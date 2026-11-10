//! `graviton-agent`: PID 1 inside every Graviton guest.
//!
//! Boot sequence:
//! 1. Mount `/dev`, `/proc`, `/sys` and `/tmp`.
//! 2. Print an [`AgentEvent::Ready`] line on the console (hvc0) for the boot log.
//! 3. Load the kernel modules listed in `/lib/modules/order` (vsock).
//! 4. Listen on vsock port [`AGENT_VSOCK_PORT`]; every connection gets a
//!    `Ready` event, then newline-delimited [`AgentRequest`]s.
//!
//! Milestone 1 runs processes directly in the guest. Namespaces, cgroups and
//! `pivot_root` into the image's rootfs come with milestone 2.
//!
//! Build it for the guest with `scripts/build-guest.sh`.

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
mod linux {
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

        for line in BufReader::new(conn).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match decode_line::<AgentRequest>(line.as_bytes()) {
                Ok(AgentRequest::Exec { argv, env, cwd, tty: _ }) => exec(argv, env, cwd, Arc::clone(&writer)),
                Ok(AgentRequest::Signal { pid, signal }) => {
                    // SAFETY: kill(2) with caller-supplied values; errors are reported, not UB.
                    if unsafe { libc::kill(pid, signal) } != 0 {
                        send(&writer, &AgentEvent::Error { message: format!("kill {pid}: {}", io::Error::last_os_error()) });
                    }
                }
                Ok(AgentRequest::Stdin { .. }) => {
                    send(&writer, &AgentEvent::Error { message: "stdin forwarding arrives with milestone 2".into() });
                }
                Ok(AgentRequest::Shutdown) => power_off(),
                Err(err) => send(&writer, &AgentEvent::Error { message: format!("bad request: {err}") }),
            }
        }
        Ok(())
    }

    fn exec(argv: Vec<String>, env: Vec<String>, cwd: String, writer: Writer) {
        let Some((program, args)) = argv.split_first() else {
            send(&writer, &AgentEvent::Error { message: "exec: empty argv".into() });
            return;
        };
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(if cwd.is_empty() { "/" } else { &cwd })
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for pair in &env {
            if let Some((k, v)) = pair.split_once('=') {
                cmd.env(k, v);
            }
        }
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
        Ok(())
    }

    fn mount(source: &str, target: &str, fstype: &str) -> io::Result<()> {
        let (s, t, f) = (cstr(source)?, cstr(target)?, cstr(fstype)?);
        // SAFETY: all pointers are valid NUL-terminated strings for the call's duration.
        let rc = unsafe { libc::mount(s.as_ptr(), t.as_ptr(), f.as_ptr(), libc::MS_NOSUID, std::ptr::null()) };
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
