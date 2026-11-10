//! Interactive sessions: run a command on a pseudo-terminal and splice it with
//! the vsock connection. After the `started` line the connection carries raw
//! bytes: what the client types goes to the terminal, and everything the
//! program draws comes back. It closes when the program exits.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;

use graviton_proto::{AgentEvent, encode_line};

pub fn run(cmd: Command, mut conn: File, pending: Vec<u8>, rows: u16, cols: u16, has_term: bool) -> io::Result<()> {
    let result = session(cmd, &mut conn, pending, rows, cols, has_term);
    if let Err(err) = &result {
        // Before `started` the client is still reading JSON lines; tell it what failed.
        let _ = conn.write_all(&encode_line(&AgentEvent::Error { message: format!("terminal: {err}") }));
    }
    result
}

fn session(mut cmd: Command, conn: &mut File, pending: Vec<u8>, rows: u16, cols: u16, has_term: bool) -> io::Result<()> {
    let size = libc::winsize { ws_row: if rows == 0 { 24 } else { rows }, ws_col: if cols == 0 { 80 } else { cols }, ws_xpixel: 0, ws_ypixel: 0 };
    let (mut master_fd, mut slave_fd) = (-1, -1);
    // SAFETY: openpty fills two descriptors; the name and termios pointers may be null.
    if unsafe { libc::openpty(&mut master_fd, &mut slave_fd, std::ptr::null_mut(), std::ptr::null(), &size) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty returned two fresh descriptors that we now own.
    let (master, slave) = unsafe { (File::from_raw_fd(master_fd), OwnedFd::from_raw_fd(slave_fd)) };
    // SAFETY: fcntl on a descriptor we own; the child mustn't inherit the master.
    unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };

    cmd.stdin(Stdio::from(slave.try_clone()?)).stdout(Stdio::from(slave.try_clone()?)).stderr(Stdio::from(slave));
    if !has_term {
        cmd.env("TERM", "xterm-256color");
    }
    // SAFETY: runs in the child after its stdio is the terminal: become a
    // session leader with the terminal as its controlling tty.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            let _ = conn.write_all(&encode_line(&AgentEvent::Error { message: format!("exec: {err}") }));
            return Ok(());
        }
    };
    drop(cmd); // closes our copies of the terminal's slave side
    let pid = child.id() as i32;
    conn.write_all(&encode_line(&AgentEvent::Started { pid }))?;

    // Keystrokes → terminal. When the client's input ends (a pipe, ^D), send
    // the terminal an end-of-file so the program can finish what it was given.
    let mut keys_in = conn.try_clone()?;
    let mut keys_out = master.try_clone()?;
    thread::spawn(move || {
        let _ = keys_out.write_all(&pending);
        let _ = io::copy(&mut keys_in, &mut keys_out);
        let _ = keys_out.write_all(&[0x04]);
    });

    // Terminal → client, until the program (and everything on the terminal) exits.
    let conn = &mut *conn;
    let mut screen = master;
    let mut buf = [0u8; 16 * 1024];
    loop {
        match screen.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if conn.write_all(&buf[..n]).is_err() {
                    // The client is gone: hang up the session's processes.
                    // SAFETY: signals the session's process group; harmless if it's gone.
                    unsafe { libc::kill(-pid, libc::SIGHUP) };
                    break;
                }
            }
        }
    }
    let _ = child.wait();
    // SAFETY: shutting down a socket we own wakes the keystroke thread.
    unsafe { libc::shutdown(conn.as_raw_fd(), libc::SHUT_RDWR) };
    Ok(())
}
