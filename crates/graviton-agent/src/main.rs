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

#[cfg(target_os = "linux")]
mod build;
#[cfg(target_os = "linux")]
mod container;
#[cfg(target_os = "linux")]
mod files;
#[cfg(target_os = "linux")]
mod net;
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
    use std::ffi::CString;
    use std::fs::{self, File};
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use graviton_proto::{AGENT_VSOCK_PORT, AgentEvent, AgentRequest, decode_line, encode_line};

    type Writer = Arc<Mutex<File>>;

    pub fn main() {
        let pid1 = std::process::id() == 1;
        if pid1 {
            if let Err(err) = setup_system() {
                fatal(&format!("mounting system filesystems: {err}"));
            }
            // SAFETY: single-threaded at this point.
            unsafe { std::env::set_var("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin") };
            // The initramfs ships one static busybox; give the guest its applets (ls, cat, ...).
            if fs::metadata("/bin/busybox").is_ok() {
                let _ = Command::new("/bin/busybox").args(["--install", "-s", "/bin"]).status();
            }
        }

        let kernel = fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "unknown".into());
        console(&AgentEvent::Ready { kernel: kernel.clone() });

        if let Err(err) = load_modules() {
            console(&AgentEvent::Error { message: format!("loading modules: {err}") });
        }
        crate::net::start();

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
                Ok(AgentRequest::Prepare { container, layers, mounts, hostname }) => {
                    let t0 = std::time::Instant::now();
                    let result = crate::container::prepare(&container, &layers).and_then(|files| {
                        let ip = crate::net::wait(std::time::Duration::from_secs(8));
                        crate::container::finish(&container, &mounts, hostname.as_deref().unwrap_or(&container), ip.as_deref())?;
                        Ok((files, ip))
                    });
                    match result {
                        Ok((files, ip)) => {
                            send(&writer, &AgentEvent::Prepared { container, files, ms: t0.elapsed().as_millis() as u64, ip })
                        }
                        Err(err) => send(&writer, &AgentEvent::Error { message: format!("preparing {container}: {err}") }),
                    }
                }
                Ok(AgentRequest::Exec { argv, env, cwd, tty, container, user, hostname, rows, cols }) => {
                    let has_term = env.iter().any(|e| e.starts_with("TERM="));
                    let cmd = match &container {
                        Some(id) => crate::container::command(id, &argv, &env, &cwd, user.as_deref(), hostname.as_deref().unwrap_or(id)),
                        None => plain_command(&argv, &env, &cwd),
                    };
                    match cmd {
                        Ok(cmd) if tty => {
                            // The connection now belongs to the terminal session.
                            let pending = reader.buffer().to_vec();
                            drop(writer);
                            return crate::tty::run(cmd, reader.into_inner(), pending, rows, cols, has_term);
                        }
                        Ok(cmd) => exec(cmd, argv.first().cloned().unwrap_or_default(), Arc::clone(&writer)),
                        Err(err) => send(&writer, &AgentEvent::Error { message: format!("exec: {err}") }),
                    }
                }
                Ok(AgentRequest::Signal { pid, signal }) => {
                    // SAFETY: kill(2) with caller-supplied values; errors are reported, not UB.
                    if unsafe { libc::kill(pid, signal) } != 0 {
                        send(&writer, &AgentEvent::Error { message: format!("kill {pid}: {}", io::Error::last_os_error()) });
                    }
                }
                Ok(AgentRequest::Stdin { .. }) => {
                    send(&writer, &AgentEvent::Error { message: "stdin is only forwarded for TTY sessions".into() });
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
                Ok(AgentRequest::Copy { container, sources, dest, cwd, from_container, chown, extract, ignore }) => {
                    let req = crate::build::CopyRequest {
                        container: &container,
                        sources: &sources,
                        dest: &dest,
                        cwd: &cwd,
                        from_container: from_container.as_deref(),
                        chown: chown.as_deref(),
                        extract,
                        ignore: &ignore,
                    };
                    match crate::build::copy(&req) {
                        Ok(files) => send(&writer, &AgentEvent::Copied { files }),
                        Err(err) => send(&writer, &AgentEvent::Error { message: err.to_string() }),
                    }
                }
                Ok(AgentRequest::Shutdown) => power_off(),
                Err(err) => send(&writer, &AgentEvent::Error { message: format!("bad request: {err}") }),
            }
        }
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

    fn exec(mut cmd: Command, program: String, writer: Writer) {
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                send(&writer, &AgentEvent::Error { message: format!("exec {program}: {err}") });
                return;
            }
        };
        let pid = child.id() as i32;
        send(&writer, &AgentEvent::Started { pid });

        let out = child.stdout.take().map(|s| pump(s, pid, false, Arc::clone(&writer)));
        let err = child.stderr.take().map(|s| pump(s, pid, true, Arc::clone(&writer)));
        thread::spawn(move || {
            let status = child.wait();
            // Drain output before announcing the exit so clients see it in order.
            for t in [out, err].into_iter().flatten() {
                let _ = t.join();
            }
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
