//! `graviton-agent`: PID 1 inside every Graviton guest.
//!
//! Boot sequence:
//! 1. Mount `/dev`, `/proc`, `/sys` and `/tmp`.
//! 2. Print an [`AgentEvent::Ready`] line on the console (hvc0) for the boot log.
//! 3. Load the kernel modules listed in `/lib/modules/order` (vsock).
//! 4. Listen on vsock port [`AGENT_VSOCK_PORT`]; every connection gets a
//!    `Ready` event, then newline-delimited [`AgentRequest`]s.
//!
//! `prepare` unpacks an image into a container root filesystem; `exec` with a
//! container runs the process inside it (see `container.rs`). Without one it
//! runs directly in the guest, which `gravitond boot-check` uses. `stats`
//! reports the guest's resource counters (`stats.rs`); `list_dir` and
//! `read_file` browse the container's files (`files.rs`).
//!
//! Build it for the guest with `scripts/build-guest.sh`.

mod ns;
mod zstd_stream;

#[cfg(target_os = "linux")]
mod build;
#[cfg(target_os = "linux")]
mod container;
#[cfg(target_os = "linux")]
mod dns;
#[cfg(target_os = "linux")]
mod files;
#[cfg(target_os = "linux")]
mod net;
#[cfg(target_os = "linux")]
mod reap;
#[cfg(target_os = "linux")]
mod stats;
#[cfg(target_os = "linux")]
mod tty;

#[cfg(target_os = "linux")]
fn main() {
    linux::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("graviton-agent runs inside the Linux guest VM; build it with scripts/build-guest.sh");
    std::process::exit(2);
}

#[cfg(target_os = "linux")]
pub(crate) mod linux {
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::fs::{self, File};
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::process::{ChildStdin, Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use graviton_proto::{AGENT_VSOCK_PORT, AgentEvent, AgentRequest, decode_line, encode_line};

    type Writer = Arc<Mutex<File>>;

    pub fn main() {
        if let Ok(fd_str) = std::env::var(crate::ns::LAUNCH_FD_ENV) {
            crate::ns::run_launcher(&fd_str);
        }
        let pid1 = std::process::id() == 1;
        if pid1 {
            if let Err(err) = setup_system() {
                fatal(&format!("mounting system filesystems: {err}"));
            }
            // SAFETY: single-threaded at this point.
            unsafe { std::env::set_var("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin") };
            // The initramfs ships one static busybox; give the guest its applets (ls, cat, ...).
            if fs::metadata("/bin/busybox").is_ok() {
                let _ = crate::reap::status(Command::new("/bin/busybox").args(["--install", "-s", "/bin"]));
            }
            // Orphans inside containers are re-parented to us; wait for them.
            crate::reap::start();
        }

        let kernel = fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "unknown".into());
        console(&AgentEvent::Ready { kernel: kernel.clone() });

        if let Err(err) = load_modules() {
            console(&AgentEvent::Error { message: format!("loading modules: {err}") });
        }
        setup_rosetta();
        // Template VMs saved as snapshots bring links up but defer DHCP until `Prepare` runs after restore.
        if is_snapshot_boot() {
            crate::net::bring_up_links();
        } else {
            crate::net::start();
        }

        let listener = match vsock_listen(AGENT_VSOCK_PORT) {
            Ok(fd) => fd,
            Err(err) => fatal(&format!("vsock listen on port {AGENT_VSOCK_PORT}: {err}")),
        };

        loop {
            // SAFETY: plain accept(2) on a listening socket we own.
            let fd = unsafe { libc::accept4(listener.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
            if fd < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                fatal(&format!("vsock accept: {err}"));
            }
            // SAFETY: accept returned a fresh descriptor that nothing else owns.
            let conn = unsafe { File::from_raw_fd(fd) };
            let kernel = kernel.clone();
            thread::spawn(move || {
                if let Err(err) = serve(conn, kernel) {
                    console(&AgentEvent::Error { message: format!("connection: {err}") });
                }
            });
        }
    }

    fn serve(conn: File, kernel: String) -> io::Result<()> {
        let writer: Writer = Arc::new(Mutex::new(conn.try_clone()?));
        send(&writer, &AgentEvent::Ready { kernel });

        let mut reader = BufReader::new(conn);
        let mut stdins: HashMap<i32, ChildStdin> = HashMap::new();
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            if line.trim().is_empty() {
                continue;
            }
            match decode_line::<AgentRequest>(line.as_bytes()) {
                Ok(AgentRequest::Prepare { container, layers, mounts, hostname, now_secs }) => {
                    let t0 = std::time::Instant::now();
                    if let Some(secs) = now_secs {
                        set_wall_clock(secs);
                    }
                    // Idempotent: starts DHCP now if this VM was restored from a snapshot.
                    crate::net::start();
                    let result = crate::container::prepare(&container, &layers).and_then(|(files, reused)| {
                        let ip = crate::net::wait(std::time::Duration::from_secs(8));
                        let dns = crate::container::finish(&container, &mounts, hostname.as_deref().unwrap_or(&container), ip.as_deref())?;
                        Ok((files, reused, ip, dns))
                    });
                    match result {
                        Ok((files, reused, ip, dns)) => {
                            send(&writer, &AgentEvent::Prepared { container, files, ms: t0.elapsed().as_millis() as u64, ip, reused, dns })
                        }
                        Err(err) => send(&writer, &AgentEvent::Error { message: format!("preparing {container}: {err}") }),
                    }
                }
                Ok(AgentRequest::Exec { argv, env, cwd, tty, container, user, hostname, rows, cols, stdin, stdin_pipe, mounts }) => {
                    let has_term = env.iter().any(|e| e.starts_with("TERM="));
                    let program = argv.first().cloned().unwrap_or_default();
                    if let Some(id) = &container {
                        let (pm, cleanup) = if mounts.is_empty() {
                            (Vec::new(), None)
                        } else {
                            match crate::build::prepare_exec_mounts(id, &mounts) {
                                Ok((pm, cl)) => (pm, Some(cl)),
                                Err(err) => {
                                    send(&writer, &AgentEvent::Error { message: format!("exec mounts: {err}") });
                                    continue;
                                }
                            }
                        };
                        let launch_env = if tty && !has_term {
                            let mut e = env.clone();
                            e.push("TERM=xterm-256color".into());
                            e
                        } else {
                            env.clone()
                        };
                        match crate::container::prepare_launch(
                            id,
                            &argv,
                            &launch_env,
                            &cwd,
                            user.as_deref(),
                            hostname.as_deref().unwrap_or(id),
                            tty,
                            pm,
                        ) {
                            Ok(launch) if tty => {
                                drop(cleanup);
                                let pending = reader.buffer().to_vec();
                                drop(writer);
                                return crate::tty::run_container(launch, reader.into_inner(), pending, rows, cols);
                            }
                            Ok(mut launch) => {
                                let stdin_stdio = match open_stdin_stdio(stdin, stdin_pipe) {
                                    Ok(s) => s,
                                    Err(err) => {
                                        drop(cleanup);
                                        send(&writer, &AgentEvent::Error { message: format!("preparing stdin for {program}: {err}") });
                                        continue;
                                    }
                                };
                                launch.cmd.stdin(stdin_stdio).stdout(Stdio::piped()).stderr(Stdio::piped());
                                match launch.spawn_and_wait_start() {
                                    Ok((child, guard)) => {
                                        finish_exec(child, cleanup, guard, &mut stdins, Arc::clone(&writer));
                                    }
                                    Err(err) => {
                                        drop(cleanup);
                                        send(&writer, &AgentEvent::Error { message: format!("{err}") });
                                    }
                                }
                            }
                            Err(err) => send(&writer, &AgentEvent::Error { message: format!("exec: {err}") }),
                        }
                    } else {
                        match plain_command(&argv, &env, &cwd) {
                            Ok(cmd) if tty => {
                                let pending = reader.buffer().to_vec();
                                drop(writer);
                                return crate::tty::run(cmd, reader.into_inner(), pending, rows, cols, has_term);
                            }
                            Ok(mut cmd) => {
                                let stdin_stdio = match open_stdin_stdio(stdin, stdin_pipe) {
                                    Ok(s) => s,
                                    Err(err) => {
                                        send(&writer, &AgentEvent::Error { message: format!("preparing stdin for {program}: {err}") });
                                        continue;
                                    }
                                };
                                cmd.stdin(stdin_stdio).stdout(Stdio::piped()).stderr(Stdio::piped());
                                match crate::reap::spawn(&mut cmd) {
                                    Ok(child) => finish_exec(child, None, None, &mut stdins, Arc::clone(&writer)),
                                    Err(err) => send(&writer, &AgentEvent::Error { message: format!("exec {program}: {err}") }),
                                }
                            }
                            Err(err) => send(&writer, &AgentEvent::Error { message: format!("exec: {err}") }),
                        }
                    }
                }
                Ok(AgentRequest::Signal { pid, signal }) => {
                    // SAFETY: kill(2) with caller-supplied values; errors are reported, not UB.
                    if unsafe { libc::kill(pid, signal) } != 0 {
                        send(&writer, &AgentEvent::Error { message: format!("kill {pid}: {}", io::Error::last_os_error()) });
                    }
                }
                Ok(AgentRequest::Stdin { pid, data }) => {
                    if data.is_empty() {
                        stdins.remove(&pid);
                    } else if let Some(pipe) = stdins.get_mut(&pid) {
                        if pipe.write_all(&data).is_err() || pipe.flush().is_err() {
                            stdins.remove(&pid);
                        }
                    }
                }
                Ok(AgentRequest::Stats { processes }) => send(&writer, &AgentEvent::Stats(crate::stats::collect(processes))),
                Ok(AgentRequest::ListDir { container, path }) => match crate::files::list(&container, &path) {
                    Ok(listing) => send(&writer, &AgentEvent::Dir(listing)),
                    Err(err) => send(&writer, &AgentEvent::Error { message: err.to_string() }),
                },
                Ok(AgentRequest::ReadFile { container, path, limit }) => match crate::files::read(&container, &path, limit.min(4 << 20)) {
                    Ok(contents) => send(&writer, &AgentEvent::File(contents)),
                    Err(err) => send(&writer, &AgentEvent::Error { message: err.to_string() }),
                },
                Ok(AgentRequest::GetArchive { container, path }) => {
                    // Dedicated connection: sends ArchiveReady + raw tar bytes until EOF.
                    drop(writer);
                    return crate::files::get_archive(&container, &path, reader.into_inner());
                }
                Ok(AgentRequest::PutArchive { container, path, size }) => {
                    match crate::files::put_archive(&container, &path, size, &mut reader) {
                        Ok(files) => send(&writer, &AgentEvent::Copied { files }),
                        Err(err) => send(&writer, &AgentEvent::Error { message: err.to_string() }),
                    }
                }
                Ok(AgentRequest::Snapshot { container }) => match crate::build::snapshot(&container) {
                    Ok(()) => send(&writer, &AgentEvent::Done),
                    Err(err) => send(&writer, &AgentEvent::Error { message: format!("snapshot: {err}") }),
                },
                Ok(AgentRequest::Commit { container, name }) => match crate::build::commit(&container, &name) {
                    Ok(layer) => send(&writer, &AgentEvent::Committed(layer)),
                    Err(err) => send(&writer, &AgentEvent::Error { message: format!("writing the layer: {err}") }),
                },
                Ok(AgentRequest::Mkdir { container, path }) => match crate::build::mkdir(&container, &path) {
                    Ok(()) => send(&writer, &AgentEvent::Done),
                    Err(err) => send(&writer, &AgentEvent::Error { message: err.to_string() }),
                },
                Ok(AgentRequest::Copy { container, sources, dest, cwd, from_container, chown, chmod, extract, ignore, inline, staged }) => {
                    let req = crate::build::CopyRequest {
                        container: &container,
                        sources: &sources,
                        dest: &dest,
                        cwd: &cwd,
                        from_container: from_container.as_deref(),
                        chown: chown.as_deref(),
                        chmod,
                        extract,
                        ignore: &ignore,
                        inline: &inline,
                        staged: &staged,
                    };
                    match crate::build::copy(&req) {
                        Ok(files) => send(&writer, &AgentEvent::Copied { files }),
                        Err(err) => send(&writer, &AgentEvent::Error { message: err.to_string() }),
                    }
                }
                Ok(AgentRequest::SetHosts { generation, hosts }) => {
                    crate::dns::set_hosts(generation, &hosts);
                }
                Ok(AgentRequest::Shutdown) => power_off(),
                Err(err) => send(&writer, &AgentEvent::Error { message: format!("bad request: {err}") }),
            }
        }
    }

    /// Unlinked seekable file in the guest's `/tmp` (outside any container
    /// rootfs) so `/proc/self/fd/0` works cleanly for `#!` heredoc scripts.
    fn make_stdin_file(bytes: &[u8]) -> io::Result<File> {
        use std::io::{Seek, SeekFrom};
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = format!("/tmp/.graviton-stdin-{}-{n}", std::process::id());
        let mut file = fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
        let _ = fs::remove_file(&path);
        file.write_all(bytes)?;
        file.seek(SeekFrom::Start(0))?;
        Ok(file)
    }

    fn plain_command(argv: &[String], env: &[String], cwd: &str) -> io::Result<Command> {
        let (program, args) = argv.split_first().ok_or_else(|| io::Error::other("empty argv"))?;
        let mut cmd = Command::new(program);
        cmd.args(args).current_dir(if cwd.is_empty() { "/" } else { cwd });
        for pair in env {
            if let Some((k, v)) = pair.split_once('=') {
                cmd.env(k, v);
            }
        }
        Ok(cmd)
    }

    fn open_stdin_stdio(stdin_bytes: Option<Vec<u8>>, stdin_pipe: bool) -> io::Result<Stdio> {
        if stdin_pipe {
            Ok(Stdio::piped())
        } else if let Some(bytes) = stdin_bytes {
            Ok(Stdio::from(make_stdin_file(&bytes)?))
        } else {
            Ok(Stdio::null())
        }
    }

    fn finish_exec(
        mut child: std::process::Child,
        cleanup: Option<crate::build::MountCleanup>,
        init_guard: Option<crate::ns::InitGuard>,
        stdins: &mut HashMap<i32, ChildStdin>,
        writer: Writer,
    ) {
        let pid = child.id() as i32;
        if let Some(pipe) = child.stdin.take() {
            stdins.insert(pid, pipe);
        }
        send(&writer, &AgentEvent::Started { pid });

        let out = child.stdout.take().map(|s| pump(s, pid, false, Arc::clone(&writer)));
        let err = child.stderr.take().map(|s| pump(s, pid, true, Arc::clone(&writer)));
        thread::spawn(move || {
            let status = child.wait();
            crate::reap::release(pid);
            drop(init_guard);
            // Drain output and remove ephemeral secret/bind mountpoints BEFORE
            // announcing Exited so a following Commit never sees them.
            for t in [out, err].into_iter().flatten() {
                let _ = t.join();
            }
            drop(cleanup);
            let code = match status {
                Ok(s) => s.code().unwrap_or_else(|| {
                    use std::os::unix::process::ExitStatusExt;
                    128 + s.signal().unwrap_or(0)
                }),
                Err(_) => -1,
            };
            send(&writer, &AgentEvent::Exited { pid, code });
        });
    }

    fn pump(mut src: impl Read + Send + 'static, pid: i32, stderr: bool, writer: Writer) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match src.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let data = buf[..n].to_vec();
                        let event = if stderr { AgentEvent::Stderr { pid, data } } else { AgentEvent::Stdout { pid, data } };
                        send(&writer, &event);
                    }
                }
            }
        })
    }

    fn send(writer: &Writer, event: &AgentEvent) {
        if let Ok(mut w) = writer.lock() {
            let _ = w.write_all(&encode_line(event));
        }
    }

    fn console(event: &AgentEvent) {
        let mut out = io::stdout().lock();
        let _ = out.write_all(&encode_line(event));
        let _ = out.flush();
    }

    fn fatal(message: &str) -> ! {
        console(&AgentEvent::Error { message: message.to_string() });
        if std::process::id() == 1 {
            power_off();
        }
        std::process::exit(1);
    }

    fn power_off() -> ! {
        // SAFETY: sync(2) and reboot(2) take no pointers.
        unsafe {
            libc::sync();
            libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
        }
        std::process::exit(0);
    }

    // -----------------------------------------------------------------------
    // System setup
    // -----------------------------------------------------------------------

    fn setup_system() -> io::Result<()> {
        for (source, target, fstype) in [
            ("devtmpfs", "/dev", "devtmpfs"),
            ("proc", "/proc", "proc"),
            ("sysfs", "/sys", "sysfs"),
            ("tmpfs", "/tmp", "tmpfs"),
            ("tmpfs", "/run", "tmpfs"),
        ] {
            fs::create_dir_all(target)?;
            mount(source, target, fstype)?;
        }
        // Terminals for `grav exec -t` (containers get /dev, and so this, by bind mount).
        fs::create_dir_all("/dev/pts")?;
        mount_with("devpts", "/dev/pts", "devpts", libc::MS_NOSUID | libc::MS_NOEXEC, Some("ptmxmode=0666,mode=0620"))?;
        Ok(())
    }

    fn mount(source: &str, target: &str, fstype: &str) -> io::Result<()> {
        mount_with(source, target, fstype, libc::MS_NOSUID, None)
    }

    pub(crate) fn mount_with(source: &str, target: &str, fstype: &str, flags: libc::c_ulong, data: Option<&str>) -> io::Result<()> {
        let (s, t, f) = (cstr(source)?, cstr(target)?, cstr(fstype)?);
        let d = data.map(cstr).transpose()?;
        let dp = d.as_ref().map_or(std::ptr::null(), |d| d.as_ptr().cast());
        // SAFETY: all pointers are valid NUL-terminated strings (or null) for the call's duration.
        let rc = unsafe { libc::mount(s.as_ptr(), t.as_ptr(), f.as_ptr(), flags, dp) };
        if rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EBUSY) {
            Ok(())
        } else {
            Err(io::Error::other(format!("mount {fstype} on {target}: {}", io::Error::last_os_error())))
        }
    }

    /// Load the modules named in `/lib/modules/order`, one path per line.
    fn load_modules() -> io::Result<()> {
        let order = match fs::read_to_string("/lib/modules/order") {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        for name in order.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let file = File::open(format!("/lib/modules/{name}"))?;
            let params = c"";
            // SAFETY: finit_module(2) with an open fd and an empty parameter string.
            let rc = unsafe { libc::syscall(libc::SYS_finit_module, file.as_raw_fd(), params.as_ptr(), 0) };
            if rc != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                return Err(io::Error::other(format!("{name}: {}", io::Error::last_os_error())));
            }
        }
        Ok(())
    }

    /// True when the kernel cmdline carries `graviton.snapshot=1` (template VM
    /// that will be saved before DHCP, disk access, or virtiofs volume mounts).
    fn is_snapshot_boot() -> bool {
        fs::read_to_string("/proc/cmdline")
            .map(|s| s.split_whitespace().any(|tok| tok == "graviton.snapshot=1"))
            .unwrap_or(false)
    }

    /// Sync guest `CLOCK_REALTIME` to host UTC seconds after VM snapshot restore.
    fn set_wall_clock(secs: u64) {
        let ts = libc::timespec { tv_sec: secs as _, tv_nsec: 0 };
        // SAFETY: valid timespec pointer passed to clock_settime(CLOCK_REALTIME).
        unsafe {
            let _ = libc::clock_settime(libc::CLOCK_REALTIME, &raw const ts);
        }
    }

    /// Mount Apple's `rosetta` virtiofs share (if attached) and register the
    /// x86_64 ELF handler in `binfmt_misc` with `OCF` flags so the kernel pins
    /// the open interpreter fd across container `chroot`s.
    fn setup_rosetta() {
        const ROSETTA_DIR: &str = "/run/rosetta";
        const BINFMT_DIR: &str = "/proc/sys/fs/binfmt_misc";
        const ROSETTA_BINFMT: &str =
            ":rosetta:M::\\x7fELF\\x02\\x01\\x01\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x02\\x00\\x3e\\x00:\\xff\\xff\\xff\\xff\\xff\\xfe\\xfe\\x00\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xfe\\xff\\xff\\xff:/run/rosetta/rosetta:OCF";

        if fs::create_dir_all(ROSETTA_DIR).is_err() {
            return;
        }
        // Note: intentionally NOT MS_NOEXEC — `/run/rosetta/rosetta` must be executable.
        if mount_with("rosetta", ROSETTA_DIR, "virtiofs", libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV, None).is_err() {
            return;
        }
        if !std::path::Path::new("/run/rosetta/rosetta").is_file() {
            return;
        }
        if mount_with("binfmt_misc", BINFMT_DIR, "binfmt_misc", libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, None).is_err() {
            console(&AgentEvent::Error { message: "rosetta: couldn't mount binfmt_misc".into() });
            return;
        }
        if !std::path::Path::new("/proc/sys/fs/binfmt_misc/rosetta").exists() {
            if let Err(err) = fs::write("/proc/sys/fs/binfmt_misc/register", ROSETTA_BINFMT) {
                console(&AgentEvent::Error { message: format!("rosetta: binfmt_misc register failed: {err}") });
            }
        }
    }

    fn vsock_listen(port: u32) -> io::Result<OwnedFd> {
        // SAFETY: socket(2) returns a new descriptor or -1.
        let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a fresh descriptor that we now own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: sockaddr_vm is plain old data; zeroed is a valid starting value.
        let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_port = port;
        addr.svm_cid = libc::VMADDR_CID_ANY;
        let len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
        // SAFETY: addr is a valid sockaddr_vm of `len` bytes.
        if unsafe { libc::bind(fd.as_raw_fd(), (&raw const addr).cast(), len) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: listen(2) on a bound socket.
        if unsafe { libc::listen(fd.as_raw_fd(), 16) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }

    fn cstr(s: &str) -> io::Result<CString> {
        CString::new(s).map_err(|_| io::Error::other("string contains NUL"))
    }
}
