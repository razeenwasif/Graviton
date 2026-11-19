//! Interactive sessions: run a command on a pseudo-terminal and connect it to
//! the vsock connection. After the `started` line the connection carries
//! terminal frames (see `graviton_proto::frame`): keystrokes, window sizes and
//! end-of-input from the client; screen output and finally the exit code from
//! here. If the client goes away, the session's processes get SIGHUP.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::thread;

use graviton_proto::frame::{self, Decoder, Frame};
use graviton_proto::{AgentEvent, encode_line};

pub fn run(cmd: Command, mut conn: File, pending: Vec<u8>, rows: u16, cols: u16, has_term: bool) -> io::Result<()> {
    let result = session(cmd, &mut conn, pending, rows, cols, has_term);
    if let Err(err) = &result {
        // Before `started` the client is still reading JSON lines; tell it what failed.
        let _ = conn.write_all(&encode_line(&AgentEvent::Error { message: format!("terminal: {err}") }));
    }
    result
}

fn winsize(rows: u16, cols: u16) -> libc::winsize {
    libc::winsize { ws_row: if rows == 0 { 24 } else { rows }, ws_col: if cols == 0 { 80 } else { cols }, ws_xpixel: 0, ws_ypixel: 0 }
}

fn session(mut cmd: Command, conn: &mut File, pending: Vec<u8>, rows: u16, cols: u16, has_term: bool) -> io::Result<()> {
    let size = winsize(rows, cols);
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
    let mut child = match crate::reap::spawn(&mut cmd) {
        Ok(child) => child,
        Err(err) => {
            let _ = conn.write_all(&encode_line(&AgentEvent::Error { message: format!("exec: {err}") }));
            return Ok(());
        }
    };
    drop(cmd); // closes our copies of the terminal's slave side
    let pid = child.id() as i32;
    conn.write_all(&encode_line(&AgentEvent::Started { pid }))?;

    // Client frames → terminal.
    let mut keys_in = conn.try_clone()?;
    let mut keys_out = master.try_clone()?;
    thread::spawn(move || {
        let mut decoder = Decoder::default();
        decoder.push(&pending);
        let mut buf = [0u8; 16 * 1024];
        'conn: loop {
            loop {
                match decoder.next_frame() {
                    Ok(Some(Frame::Data(bytes))) => {
                        if keys_out.write_all(&bytes).is_err() {
                            break 'conn;
                        }
                    }
                    Ok(Some(Frame::Resize { rows, cols })) => {
                        let size = winsize(rows, cols);
                        // SAFETY: TIOCSWINSZ on the master we own; the kernel signals the foreground group.
                        unsafe { libc::ioctl(keys_out.as_raw_fd(), libc::TIOCSWINSZ, &size) };
                    }
                    // Input ended (a pipe): ^D lets the program finish what it was given.
                    Ok(Some(Frame::Eof)) => {
                        let _ = keys_out.write_all(&[0x04]);
                    }
                    Ok(Some(Frame::Exit(_))) | Ok(None) => break,
                    Err(_) => break 'conn,
                }
            }
            match keys_in.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => decoder.push(&buf[..n]),
            }
        }
        // The client hung up: so does the terminal.
        // SAFETY: signals the session's process group; harmless if it's gone.
        unsafe { libc::kill(-pid, libc::SIGHUP) };
    });

    // Terminal → client, until the program (and everything on the terminal) exits.
    let mut screen = master;
    let mut buf = [0u8; 16 * 1024];
    let mut client_gone = false;
    loop {
        match screen.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if conn.write_all(&frame::data_frame(frame::DATA, &buf[..n])).is_err() {
                    client_gone = true;
                    // SAFETY: as above.
                    unsafe { libc::kill(-pid, libc::SIGHUP) };
                    break;
                }
            }
        }
    }
    let waited = child.wait();
    crate::reap::release(pid);
    let code = match waited {
        Ok(s) => s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0)),
        Err(_) => -1,
    };
    if !client_gone {
        let _ = conn.write_all(&Frame::Exit(code).encode());
    }
    // SAFETY: shutting down a socket we own wakes the keystroke thread.
    unsafe { libc::shutdown(conn.as_raw_fd(), libc::SHUT_RDWR) };
    Ok(())
}
