//! Per-container PID namespace isolation, built-in PID-1 reaping, and
//! namespace-entering execution (`exec` / `tty`).
//!
//! Each container starts via a tiny self-re-exec of `/init` (`_GRAVITON_LAUNCH_FD`):
//!
//! ```text
//!   graviton-agent (VM PID 1)
//!     └─ launcher (/init, VM root PID ns; receives external kills & relays to pid_init)
//!          └─ pid_init ("init", container PID 1; mounts container /proc, reaps all orphans)
//!               └─ child_pid (container PID 2; main process inside namespaces + chroot)
//! ```
//!
//! Subsequent `exec` / `tty` requests for the same container find `pid_init` in
//! [`CONTAINER_INIT`] and `setns` into its IPC, UTS, PID, and mount namespaces so
//! `ps` / `top` / `kill` share one cohesive PID namespace per container.

use serde::{Deserialize, Serialize};

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const LAUNCH_FD_ENV: &str = "_GRAVITON_LAUNCH_FD";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct MountSpec {
    pub src: String,
    pub dst: String,
    pub is_tmpfs: bool,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub enum LaunchMode {
    Init {
        root: String,
        dev_dst: String,
        hostname: String,
        mounts: Vec<MountSpec>,
        read_only: bool,
        init: bool,
        #[serde(default)]
        shm_size_bytes: Option<u64>,
    },
    Enter {
        init_pid: i32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct LaunchSpec {
    pub mode: LaunchMode,
    pub argv: Vec<String>,
    pub env: Vec<String>,
    pub cwd: String,
    pub uid: u32,
    pub gid: u32,
    pub tty: bool,
    pub cap_mask: u64,
    #[serde(default)]
    pub ulimits: Vec<graviton_proto::UlimitSpec>,
    pub status_fd: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub enum StartReport {
    Ok { init_pid: Option<i32> },
    Err { message: String },
}

/// Convert a raw `waitpid(2)` status integer into a shell-style exit code.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn decode_wait_status(status: i32) -> i32 {
    if (status & 0x7f) == 0 {
        (status >> 8) & 0xff
    } else {
        128 + (status & 0x7f)
    }
}

/// Parse `/proc/<pid>/status`'s `NSpid:` line and return the innermost
/// (container-local) PID.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_nspid(status_text: &str) -> Option<i32> {
    status_text
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .and_then(|rest| rest.split_whitespace().last())
        .and_then(|pid| pid.parse::<i32>().ok())
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::fs::{self, File, OpenOptions};
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Child, Command};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{LAUNCH_FD_ENV, LaunchMode, LaunchSpec, MountSpec, StartReport, decode_wait_status};

    static CONTAINER_INIT: Mutex<Option<HashMap<String, i32>>> = Mutex::new(None);
    static CFG_SEQ: AtomicU64 = AtomicU64::new(0);

    pub fn register_init(id: &str, pid: i32) {
        let mut guard = CONTAINER_INIT.lock().unwrap_or_else(|e| e.into_inner());
        guard.get_or_insert_with(HashMap::new).insert(id.to_string(), pid);
    }

    pub fn unregister_init(id: &str, pid: i32) {
        let mut guard = CONTAINER_INIT.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(map) = guard.as_mut() {
            if map.get(id).copied() == Some(pid) {
                map.remove(id);
            }
        }
    }

    pub fn lookup_init(id: &str) -> Option<i32> {
        let candidate = {
            let guard = CONTAINER_INIT.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().and_then(|m| m.get(id).copied())
        }?;
        if Path::new(&format!("/proc/{candidate}/ns/pid")).exists() {
            Some(candidate)
        } else {
            unregister_init(id, candidate);
            None
        }
    }

    /// Keeps `CONTAINER_INIT` registered while a container's initial PID-1 task lives.
    pub struct InitGuard {
        id: String,
        pid: i32,
    }

    impl Drop for InitGuard {
        fn drop(&mut self) {
            unregister_init(&self.id, self.pid);
        }
    }

    pub struct PreparedLaunch {
        pub cmd: Command,
        status_r: File,
        status_w: Option<File>,
        _cfg_file: File,
        container_id: String,
        register: bool,
    }

    impl PreparedLaunch {
        #[allow(clippy::too_many_arguments)]
        pub fn build(
            container_id: &str,
            mode: LaunchMode,
            argv: &[String],
            env: &[String],
            cwd: &str,
            uid: u32,
            gid: u32,
            tty: bool,
            cap_mask: u64,
            ulimits: &[graviton_proto::UlimitSpec],
            register: bool,
        ) -> io::Result<Self> {
            let mut pipe_fds = [-1i32; 2];
            // SAFETY: pipe2 fills two fresh CLOEXEC file descriptors.
            if unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fresh descriptors from pipe2.
            let status_r = unsafe { File::from_raw_fd(pipe_fds[0]) };
            // SAFETY: fresh descriptors from pipe2.
            let status_w = unsafe { File::from_raw_fd(pipe_fds[1]) };
            let status_w_fd = status_w.as_raw_fd();

            let spec = LaunchSpec {
                mode,
                argv: argv.to_vec(),
                env: env.to_vec(),
                cwd: cwd.to_string(),
                uid,
                gid,
                tty,
                cap_mask,
                ulimits: ulimits.to_vec(),
                status_fd: status_w_fd,
            };
            let json = serde_json::to_vec(&spec).map_err(io::Error::other)?;

            let seq = CFG_SEQ.fetch_add(1, Ordering::Relaxed);
            let cfg_path = format!("/tmp/.graviton-ns-{}-{seq}", std::process::id());
            let mut cfg_file = OpenOptions::new().read(true).write(true).create_new(true).open(&cfg_path)?;
            let _ = fs::remove_file(&cfg_path);
            cfg_file.write_all(&json)?;
            use std::io::Seek;
            cfg_file.seek(io::SeekFrom::Start(0))?;
            let cfg_fd = cfg_file.as_raw_fd();

            let mut cmd = Command::new("/init");
            cmd.env_clear().env(LAUNCH_FD_ENV, cfg_fd.to_string());
            // SAFETY: pre_exec runs in the forked child before exec and only clears
            // FD_CLOEXEC on our two dedicated pipe/config descriptors.
            unsafe {
                cmd.pre_exec(move || {
                    clear_cloexec(cfg_fd)?;
                    clear_cloexec(status_w_fd)?;
                    Ok(())
                });
            }

            Ok(Self {
                cmd,
                status_r,
                status_w: Some(status_w),
                _cfg_file: cfg_file,
                container_id: container_id.to_string(),
                register,
            })
        }

        pub fn spawn_and_wait_start(mut self) -> io::Result<(Child, Option<InitGuard>)> {
            let mut child = crate::reap::spawn(&mut self.cmd)?;
            let pid = child.id() as i32;
            // Drop write end in parent immediately so EOF means the child died before reporting.
            drop(self.status_w.take());

            let mut line = String::new();
            let read_res = BufReader::new(&mut self.status_r).read_line(&mut line);
            match read_res {
                Ok(0) => {
                    let _ = child.wait();
                    crate::reap::release(pid);
                    Err(io::Error::other("container launcher exited before reporting start"))
                }
                Ok(_) => match serde_json::from_str::<StartReport>(line.trim()) {
                    Ok(StartReport::Ok { init_pid }) => {
                        let guard = if self.register {
                            init_pid.map(|ipid| {
                                register_init(&self.container_id, ipid);
                                InitGuard { id: self.container_id.clone(), pid: ipid }
                            })
                        } else {
                            None
                        };
                        Ok((child, guard))
                    }
                    Ok(StartReport::Err { message }) => {
                        let _ = child.wait();
                        crate::reap::release(pid);
                        Err(io::Error::other(message))
                    }
                    Err(err) => {
                        let _ = child.wait();
                        crate::reap::release(pid);
                        Err(io::Error::other(format!("bad launcher status: {err}")))
                    }
                },
                Err(err) => {
                    let _ = child.wait();
                    crate::reap::release(pid);
                    Err(err)
                }
            }
        }
    }

    pub fn run_launcher(fd_str: &str) -> ! {
        let cfg_fd: i32 = match fd_str.parse() {
            Ok(fd) if fd >= 0 => fd,
            _ => unsafe { libc::_exit(126) },
        };
        let _ = set_cloexec(cfg_fd);
        // SAFETY: cfg_fd was inherited by this dedicated launcher process.
        let mut cfg_file = unsafe { File::from_raw_fd(cfg_fd) };
        let mut raw = Vec::new();
        if cfg_file.read_to_end(&mut raw).is_err() {
            unsafe { libc::_exit(126) };
        }
        drop(cfg_file);

        let spec: LaunchSpec = match serde_json::from_slice(&raw) {
            Ok(s) => s,
            Err(_) => unsafe { libc::_exit(126) },
        };
        let _ = set_cloexec(spec.status_fd);

        match &spec.mode {
            LaunchMode::Init { root, dev_dst, hostname, mounts, read_only, init, shm_size_bytes } => {
                run_init(&spec, root, dev_dst, hostname, mounts, *read_only, *init, *shm_size_bytes)
            }
            LaunchMode::Enter { init_pid } => run_enter(&spec, *init_pid),
        }
    }

    fn report_err(status_fd: i32, message: &str) -> ! {
        if let Ok(mut line) = serde_json::to_vec(&StartReport::Err { message: message.to_string() }) {
            line.push(b'\n');
            write_all_fd(status_fd, &line);
        }
        unsafe { libc::close(status_fd) };
        unsafe { libc::_exit(126) };
    }

    fn report_ok(status_fd: i32, init_pid: Option<i32>) {
        if let Ok(mut line) = serde_json::to_vec(&StartReport::Ok { init_pid }) {
            line.push(b'\n');
            write_all_fd(status_fd, &line);
        }
        unsafe { libc::close(status_fd) };
    }

    fn write_all_fd(fd: i32, bytes: &[u8]) {
        let mut off = 0;
        while off < bytes.len() {
            // SAFETY: fd is valid and bytes slice is bounded.
            let n = unsafe { libc::write(fd, bytes[off..].as_ptr().cast(), bytes.len() - off) };
            if n <= 0 {
                break;
            }
            off += n as usize;
        }
    }

    fn clear_cloexec(fd: i32) -> io::Result<()> {
        // SAFETY: fcntl on an open descriptor.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: clearing FD_CLOEXEC so our child launcher inherits it.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn set_cloexec(fd: i32) -> io::Result<()> {
        // SAFETY: fcntl on an open descriptor.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: setting FD_CLOEXEC so container children never inherit internal pipes.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn redirect_stdio_to_dev_null() {
        // SAFETY: open /dev/null and duplicate onto 0, 1, 2 in supervisor processes so
        // only the container's child holds user pipes or PTY slave open.
        unsafe {
            let null_fd = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
            if null_fd >= 0 {
                let _ = libc::dup2(null_fd, 0);
                let _ = libc::dup2(null_fd, 1);
                let _ = libc::dup2(null_fd, 2);
                if null_fd > 2 {
                    libc::close(null_fd);
                }
            }
        }
    }

    fn forwardable_sigset() -> libc::sigset_t {
        // SAFETY: sigset_t initialization with known signal constants.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for sig in [
                libc::SIGTERM,
                libc::SIGINT,
                libc::SIGHUP,
                libc::SIGQUIT,
                libc::SIGUSR1,
                libc::SIGUSR2,
                libc::SIGWINCH,
                libc::SIGCHLD,
            ] {
                libc::sigaddset(&mut set, sig);
            }
            set
        }
    }

    #[repr(C)]
    struct CapUserHeader {
        version: u32,
        pid: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CapUserData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    fn last_linux_cap() -> u32 {
        fs::read_to_string("/proc/sys/kernel/cap_last_cap")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .map(|c| c.min(63))
            .unwrap_or(40)
    }

    fn apply_capabilities_and_identity(cap_mask: u64, uid: u32, gid: u32) -> Result<(), String> {
        const PR_CAP_AMBIENT: libc::c_int = 47;
        const PR_CAP_AMBIENT_CLEAR_ALL: libc::c_ulong = 4;
        const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

        // 1. Drop bounding caps before touching UID/CAP_SETPCAP.
        let last_cap = last_linux_cap();
        for cap in 0..=last_cap {
            if (cap_mask & (1u64 << cap)) == 0 {
                // SAFETY: valid PR_CAPBSET_DROP capability number on Linux.
                if unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0) } != 0 {
                    let err = io::Error::last_os_error();
                    if err.raw_os_error() != Some(libc::EINVAL) {
                        return Err(format!("prctl PR_CAPBSET_DROP({cap}): {err}"));
                    }
                }
            }
        }

        // 2. Drop supplementary groups and transition to target gid/uid.
        // SAFETY: clearing supplementary groups then dropping gid/uid inside forked child.
        unsafe {
            if libc::setgroups(0, std::ptr::null()) != 0 {
                return Err(format!("setgroups: {}", io::Error::last_os_error()));
            }
            if libc::setgid(gid) != 0 {
                return Err(format!("setgid: {}", io::Error::last_os_error()));
            }
            if libc::setuid(uid) != 0 {
                return Err(format!("setuid: {}", io::Error::last_os_error()));
            }
            let _ = libc::prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0);
        }

        // 3. Restrict effective/permitted/inheritable capability sets.
        let eff_prm = if uid == 0 { cap_mask } else { 0 };
        let lo = (eff_prm & 0xffff_ffff) as u32;
        let hi = (eff_prm >> 32) as u32;
        let mut hdr = CapUserHeader { version: LINUX_CAPABILITY_VERSION_3, pid: 0 };
        let mut data = [
            CapUserData { effective: lo, permitted: lo, inheritable: 0 },
            CapUserData { effective: hi, permitted: hi, inheritable: 0 },
        ];
        // SAFETY: capset(2) with Linux v3 header and 2-word CapUserData array for calling thread.
        if unsafe { libc::syscall(libc::SYS_capset, &raw mut hdr, data.as_mut_ptr()) } != 0 {
            return Err(format!("capset: {}", io::Error::last_os_error()));
        }
        Ok(())
    }

    fn apply_ulimits(ulimits: &[graviton_proto::UlimitSpec]) -> Result<(), String> {
        for u in ulimits {
            let norm = graviton_proto::validate_ulimit(u)?;
            let resource: libc::c_int = match norm.name.as_str() {
                "as" => libc::RLIMIT_AS as _,
                "core" => libc::RLIMIT_CORE as _,
                "cpu" => libc::RLIMIT_CPU as _,
                "data" => libc::RLIMIT_DATA as _,
                "fsize" => libc::RLIMIT_FSIZE as _,
                "locks" => libc::RLIMIT_LOCKS as _,
                "memlock" => libc::RLIMIT_MEMLOCK as _,
                "msgqueue" => libc::RLIMIT_MSGQUEUE as _,
                "nice" => libc::RLIMIT_NICE as _,
                "nofile" => libc::RLIMIT_NOFILE as _,
                "nproc" => libc::RLIMIT_NPROC as _,
                "rss" => libc::RLIMIT_RSS as _,
                "rtprio" => libc::RLIMIT_RTPRIO as _,
                "rttime" => libc::RLIMIT_RTTIME as _,
                "sigpending" => libc::RLIMIT_SIGPENDING as _,
                "stack" => libc::RLIMIT_STACK as _,
                other => return Err(format!("unsupported ulimit `{other}`")),
            };
            let to_rlim = |val: i64| -> libc::rlim_t {
                if val == graviton_proto::ULIMIT_UNLIMITED {
                    libc::RLIM_INFINITY
                } else {
                    val as libc::rlim_t
                }
            };
            let rlim = libc::rlimit {
                rlim_cur: to_rlim(norm.soft),
                rlim_max: to_rlim(norm.hard),
            };
            // SAFETY: setting resource limits on the forked child before dropping capabilities/UID.
            if unsafe { libc::setrlimit(resource as _, &raw const rlim) } != 0 {
                return Err(format!("setrlimit({}): {}", norm.name, io::Error::last_os_error()));
            }
        }
        Ok(())
    }

    fn exec_child(spec: &LaunchSpec, err_w: i32) -> ! {
        let fail = |msg: &str| -> ! {
            write_all_fd(err_w, msg.as_bytes());
            unsafe { libc::_exit(127) };
        };

        // Unblock signals so the container process handles them with default/app handlers.
        unsafe {
            let mut empty: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut empty);
            libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
        }

        if spec.tty {
            // SAFETY: setsid + TIOCSCTTY gives an interactive exec its controlling terminal.
            unsafe {
                if libc::setsid() < 0 {
                    fail(&format!("setsid: {}", io::Error::last_os_error()));
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    fail(&format!("TIOCSCTTY: {}", io::Error::last_os_error()));
                }
            }
        }

        let Some((program, _)) = spec.argv.split_first() else {
            fail("empty command");
        };
        let cwd_c = match CString::new(if spec.cwd.is_empty() { "/" } else { spec.cwd.as_str() }) {
            Ok(c) => c,
            Err(_) => fail("invalid working directory"),
        };

        if let Err(msg) = apply_ulimits(&spec.ulimits) {
            fail(&msg);
        }
        if let Err(msg) = apply_capabilities_and_identity(spec.cap_mask, spec.uid, spec.gid) {
            fail(&msg);
        }

        // SAFETY: chdir inside the container root after capability and UID drop.
        unsafe {
            if libc::chdir(cwd_c.as_ptr()) != 0 {
                fail(&format!("chdir {}: {}", spec.cwd, io::Error::last_os_error()));
            }
        }

        let prog_c = match CString::new(program.as_str()) {
            Ok(c) => c,
            Err(_) => fail("program contains NUL"),
        };
        let arg_cstrings: Vec<CString> = match spec.argv.iter().map(|s| CString::new(s.as_str())).collect() {
            Ok(v) => v,
            Err(_) => fail("argument contains NUL"),
        };
        let mut argv_ptrs: Vec<*const libc::c_char> = arg_cstrings.iter().map(|s| s.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());

        let env_cstrings: Vec<CString> = match spec.env.iter().map(|s| CString::new(s.as_str())).collect() {
            Ok(v) => v,
            Err(_) => fail("env contains NUL"),
        };
        let mut env_ptrs: Vec<*const libc::c_char> = env_cstrings.iter().map(|s| s.as_ptr()).collect();
        env_ptrs.push(std::ptr::null());

        // Set PATH in environ alongside envp so execvpe resolves PATH reliably in musl.
        let path_val = spec
            .env
            .iter()
            .find_map(|p| p.strip_prefix("PATH="))
            .unwrap_or("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        // SAFETY: single-threaded child immediately before execvpe.
        unsafe {
            std::env::set_var("PATH", path_val);
            libc::execvpe(prog_c.as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr());
        }
        let err = io::Error::last_os_error();
        fail(&format!("exec {program}: {err}"));
    }

    fn make_err_pipe() -> io::Result<(i32, i32)> {
        let mut fds = [-1i32; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((fds[0], fds[1]))
    }

    fn read_err_pipe(err_r: i32) -> String {
        let mut out = String::new();
        // SAFETY: err_r is an open pipe read end we now own and close.
        let mut file = unsafe { File::from_raw_fd(err_r) };
        let _ = file.read_to_string(&mut out);
        out
    }

    fn relay_loop(target_pid: i32, mask: &libc::sigset_t, reap_all: bool) -> ! {
        loop {
            let sig = unsafe { libc::sigwaitinfo(mask, std::ptr::null_mut()) };
            if sig < 0 {
                continue;
            }
            if sig == libc::SIGCHLD {
                loop {
                    let mut st = 0i32;
                    let wait_pid = if reap_all { -1 } else { target_pid };
                    let r = unsafe { libc::waitpid(wait_pid, &mut st, libc::WNOHANG) };
                    if r <= 0 {
                        break;
                    }
                    if r == target_pid {
                        unsafe { libc::_exit(decode_wait_status(st)) };
                    }
                }
            } else {
                // SAFETY: forward stop/interrupt/hangup signals cleanly to child/init.
                unsafe { libc::kill(target_pid, sig) };
            }
        }
    }

    fn remount_root_readonly() -> Result<(), String> {
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        let mut flags = libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY;
        if unsafe { libc::statvfs(c"/".as_ptr(), &raw mut st) } == 0 {
            if (st.f_flag & libc::ST_NOSUID) != 0 {
                flags |= libc::MS_NOSUID;
            }
            if (st.f_flag & libc::ST_NODEV) != 0 {
                flags |= libc::MS_NODEV;
            }
            if (st.f_flag & libc::ST_NOEXEC) != 0 {
                flags |= libc::MS_NOEXEC;
            }
            if (st.f_flag & libc::ST_NOATIME) != 0 {
                flags |= libc::MS_NOATIME;
            }
            if (st.f_flag & libc::ST_NODIRATIME) != 0 {
                flags |= libc::MS_NODIRATIME;
            }
        }
        // SAFETY: remounting the container's private `/` mountpoint read-only while leaving submounts untouched.
        if unsafe { libc::mount(std::ptr::null(), c"/".as_ptr(), std::ptr::null(), flags, std::ptr::null()) } != 0 {
            return Err(format!("remount / read-only: {}", io::Error::last_os_error()));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn run_init(
        spec: &LaunchSpec,
        root: &str,
        dev_dst: &str,
        hostname: &str,
        mounts: &[MountSpec],
        read_only: bool,
        init: bool,
        shm_size_bytes: Option<u64>,
    ) -> ! {
        let check = |rc: libc::c_int, what: &str| -> Result<(), String> {
            if rc == 0 { Ok(()) } else { Err(format!("{what}: {}", io::Error::last_os_error())) }
        };
        let to_c = |s: &str, what: &str| -> Result<CString, String> { CString::new(s).map_err(|_| format!("{what}: NUL byte")) };

        let setup_res: Result<(), String> = (|| {
            let root_c = to_c(root, "root")?;
            let dev_c = to_c(dev_dst, "dev")?;
            let null = std::ptr::null::<libc::c_char>();

            unsafe {
                check(
                    libc::unshare(libc::CLONE_NEWPID | libc::CLONE_NEWNS | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC),
                    "unshare",
                )?;
                check(libc::mount(null, c"/".as_ptr(), null, libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()), "MS_PRIVATE")?;
                check(libc::mount(c"/dev".as_ptr(), dev_c.as_ptr(), null, libc::MS_BIND | libc::MS_REC, std::ptr::null()), "bind /dev")?;
                for m in mounts {
                    let dst_c = to_c(&m.dst, "mount dst")?;
                    if m.is_tmpfs {
                        let mut flags = libc::MS_NOSUID | libc::MS_NODEV;
                        if m.read_only {
                            flags |= libc::MS_RDONLY;
                        }
                        check(libc::mount(c"tmpfs".as_ptr(), dst_c.as_ptr(), c"tmpfs".as_ptr(), flags, std::ptr::null()), "mount tmpfs")?;
                    } else {
                        let src_c = to_c(&m.src, "mount src")?;
                        check(libc::mount(src_c.as_ptr(), dst_c.as_ptr(), null, libc::MS_BIND, std::ptr::null()), "bind mount")?;
                        if m.read_only {
                            check(
                                libc::mount(
                                    null,
                                    dst_c.as_ptr(),
                                    null,
                                    libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
                                    std::ptr::null(),
                                ),
                                "remount ro",
                            )?;
                        }
                    }
                }
                check(libc::chdir(root_c.as_ptr()), "chdir root")?;
                check(libc::mount(root_c.as_ptr(), c"/".as_ptr(), null, libc::MS_MOVE, std::ptr::null()), "move root")?;
                check(libc::chroot(c".".as_ptr()), "chroot")?;
                check(libc::chdir(c"/".as_ptr()), "chdir /")?;
                if !hostname.is_empty() {
                    check(libc::sethostname(hostname.as_ptr().cast(), hostname.len()), "sethostname")?;
                }
            }
            Ok(())
        })();

        if let Err(msg) = setup_res {
            report_err(spec.status_fd, &msg);
        }

        let mask = forwardable_sigset();
        unsafe { libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut()) };

        let (err_r, err_w) = match make_err_pipe() {
            Ok(p) => p,
            Err(e) => report_err(spec.status_fd, &format!("err pipe: {e}")),
        };

        // Fork PID 1 inside the newly created PID namespace.
        let pid_init = unsafe { libc::fork() };
        if pid_init < 0 {
            report_err(spec.status_fd, &format!("fork init: {}", io::Error::last_os_error()));
        }
        if pid_init == 0 {
            unsafe {
                libc::close(err_r);
                libc::close(spec.status_fd);
                // Die automatically if the outer launcher is SIGKILLed.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    write_all_fd(err_w, b"prctl PDEATHSIG failed");
                    libc::_exit(126);
                }
                let init_name = if init { c"docker-init" } else { c"init" };
                let _ = libc::prctl(libc::PR_SET_NAME, init_name.as_ptr());

                if libc::mount(
                    c"proc".as_ptr(),
                    c"/proc".as_ptr(),
                    c"proc".as_ptr(),
                    libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                    std::ptr::null(),
                ) != 0
                {
                    let msg = format!("mount /proc: {}", io::Error::last_os_error());
                    write_all_fd(err_w, msg.as_bytes());
                    libc::_exit(126);
                }
                let _ = libc::mount(
                    c"sysfs".as_ptr(),
                    c"/sys".as_ptr(),
                    c"sysfs".as_ptr(),
                    libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                    std::ptr::null(),
                );
                // Mount private /dev/shm inside the container's chroot + mount namespace before read-only rootfs remount.
                let _ = fs::create_dir_all("/dev/shm");
                let shm_bytes = shm_size_bytes.unwrap_or(graviton_proto::DEFAULT_SHM_SIZE_BYTES);
                if let Ok(shm_opts) = CString::new(format!("mode=1777,size={shm_bytes}")) {
                    if libc::mount(
                        c"tmpfs".as_ptr(),
                        c"/dev/shm".as_ptr(),
                        c"tmpfs".as_ptr(),
                        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                        shm_opts.as_ptr().cast(),
                    ) != 0
                    {
                        let msg = format!("mount /dev/shm: {}", io::Error::last_os_error());
                        write_all_fd(err_w, msg.as_bytes());
                        libc::_exit(126);
                    }
                }
                if read_only {
                    if let Err(msg) = remount_root_readonly() {
                        write_all_fd(err_w, msg.as_bytes());
                        libc::_exit(126);
                    }
                }
            }

            let child_pid = unsafe { libc::fork() };
            if child_pid < 0 {
                let msg = format!("fork main process: {}", io::Error::last_os_error());
                write_all_fd(err_w, msg.as_bytes());
                unsafe { libc::_exit(126) };
            }
            if child_pid == 0 {
                exec_child(spec, err_w);
            }

            unsafe { libc::close(err_w) };
            redirect_stdio_to_dev_null();
            relay_loop(child_pid, &mask, true);
        }

        // Outer launcher: wait for grandchild exec check, notify agent, relay signals + exit code.
        unsafe { libc::close(err_w) };
        redirect_stdio_to_dev_null();

        let err_msg = read_err_pipe(err_r);
        if !err_msg.is_empty() {
            let mut st = 0;
            // SAFETY: pid_init failed and exited before closing err_w; wait to avoid strays.
            unsafe { libc::waitpid(pid_init, &mut st, 0) };
            report_err(spec.status_fd, err_msg.trim());
        }
        report_ok(spec.status_fd, Some(pid_init));
        relay_loop(pid_init, &mask, false);
    }

    fn run_enter(spec: &LaunchSpec, init_pid: i32) -> ! {
        let open_ro = |path: &str, flags: libc::c_int| -> Result<OwnedFd, String> {
            let p = CString::new(path).map_err(|_| format!("{path}: NUL byte"))?;
            // SAFETY: open existing namespace/root path before setns changes mounts/proc.
            let fd = unsafe { libc::open(p.as_ptr(), flags | libc::O_CLOEXEC) };
            if fd < 0 {
                Err(format!("open {path}: {}", io::Error::last_os_error()))
            } else {
                Ok(unsafe { OwnedFd::from_raw_fd(fd) })
            }
        };

        let enter_res: Result<(), String> = (|| {
            let ipc = open_ro(&format!("/proc/{init_pid}/ns/ipc"), libc::O_RDONLY)?;
            let uts = open_ro(&format!("/proc/{init_pid}/ns/uts"), libc::O_RDONLY)?;
            let pid = open_ro(&format!("/proc/{init_pid}/ns/pid"), libc::O_RDONLY)?;
            let mnt = open_ro(&format!("/proc/{init_pid}/ns/mnt"), libc::O_RDONLY)?;
            let root = open_ro(&format!("/proc/{init_pid}/root"), libc::O_RDONLY | libc::O_DIRECTORY)?;

            let set = |fd: &OwnedFd, nstype: libc::c_int, name: &str| -> Result<(), String> {
                if unsafe { libc::setns(fd.as_raw_fd(), nstype) } != 0 {
                    Err(format!("setns {name}: {}", io::Error::last_os_error()))
                } else {
                    Ok(())
                }
            };
            set(&ipc, libc::CLONE_NEWIPC, "ipc")?;
            set(&uts, libc::CLONE_NEWUTS, "uts")?;
            set(&pid, libc::CLONE_NEWPID, "pid")?;
            set(&mnt, libc::CLONE_NEWNS, "mnt")?;
            unsafe {
                if libc::fchdir(root.as_raw_fd()) != 0 {
                    return Err(format!("fchdir container root: {}", io::Error::last_os_error()));
                }
                if libc::chroot(c".".as_ptr()) != 0 {
                    return Err(format!("chroot container root: {}", io::Error::last_os_error()));
                }
                if libc::chdir(c"/".as_ptr()) != 0 {
                    return Err(format!("chdir /: {}", io::Error::last_os_error()));
                }
            }
            Ok(())
        })();

        if let Err(msg) = enter_res {
            report_err(spec.status_fd, &msg);
        }

        let mask = forwardable_sigset();
        unsafe { libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut()) };

        let (err_r, err_w) = match make_err_pipe() {
            Ok(p) => p,
            Err(e) => report_err(spec.status_fd, &format!("err pipe: {e}")),
        };

        // Fork so the exec child enters the container's PID namespace (setns NEWPID takes effect on fork).
        let child_pid = unsafe { libc::fork() };
        if child_pid < 0 {
            report_err(spec.status_fd, &format!("fork exec: {}", io::Error::last_os_error()));
        }
        if child_pid == 0 {
            unsafe {
                libc::close(err_r);
                libc::close(spec.status_fd);
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    write_all_fd(err_w, b"prctl PDEATHSIG failed");
                    libc::_exit(126);
                }
            }
            exec_child(spec, err_w);
        }

        unsafe { libc::close(err_w) };
        redirect_stdio_to_dev_null();

        let err_msg = read_err_pipe(err_r);
        if !err_msg.is_empty() {
            let mut st = 0;
            // SAFETY: child_pid failed and exited before closing err_w; wait to avoid strays.
            unsafe { libc::waitpid(child_pid, &mut st, 0) };
            report_err(spec.status_fd, err_msg.trim());
        }
        report_ok(spec.status_fd, None);
        relay_loop(child_pid, &mask, false);
    }
}

#[cfg(target_os = "linux")]
pub use imp::{InitGuard, PreparedLaunch, lookup_init, run_launcher};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_exit_and_signal_wait_statuses() {
        assert_eq!(decode_wait_status(0), 0);
        assert_eq!(decode_wait_status(42 << 8), 42);
        assert_eq!(decode_wait_status(15), 143); // SIGTERM (15)
        assert_eq!(decode_wait_status(9 | 0x80), 137); // SIGKILL + core bit
    }

    #[test]
    fn parses_innermost_nspid_and_roundtrips_spec() {
        let status = "Name:\tsleep\nPid:\t58\nNSpid:\t58\t2\nUid:\t0\t0\t0\t0\n";
        assert_eq!(parse_nspid(status), Some(2));
        assert_eq!(parse_nspid("Name:\tinit\nNSpid:\t57\t1\n"), Some(1));
        assert_eq!(parse_nspid("Name:\tagent\nPid:\t1\n"), None);

        let spec = LaunchSpec {
            mode: LaunchMode::Enter { init_pid: 57 },
            argv: vec!["ps".into(), "-ef".into()],
            env: vec!["PATH=/bin".into()],
            cwd: "/app".into(),
            uid: 1000,
            gid: 1000,
            tty: false,
            cap_mask: graviton_proto::caps::DEFAULT_CAP_MASK,
            ulimits: vec![],
            status_fd: 5,
        };
        let encoded = serde_json::to_vec(&spec).unwrap();
        assert_eq!(serde_json::from_slice::<LaunchSpec>(&encoded).unwrap(), spec);
    }
}
