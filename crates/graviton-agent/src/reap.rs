//! Waiting for orphans, as PID 1 must.
//!
//! The agent is PID 1 in its VM, so a process orphaned inside a container (a
//! daemon whose parent exited, a test's leftover child, anything a killed
//! process started) is re-parented to it. Unless PID 1 waits for it, it stays
//! a zombie: `kill -0` still finds it, `ps` lists it, and its pid is never
//! reused. A CI job checking that what it stopped is gone would wait forever.
//!
//! The agent's own children are different: whoever spawned each one waits for
//! it, to report its exit code. So they're spawned through [`spawn`], which
//! records their pids, and a sweeper reaps every other zombie child a few times
//! a second. A child is recorded while the same lock the sweeper holds is
//! held, so the sweeper can never take one between its spawn and its record.

use std::collections::BTreeSet;
use std::io;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::Mutex;
use std::time::Duration;

/// Pids of children someone else waits for.
static OWNED: Mutex<BTreeSet<i32>> = Mutex::new(BTreeSet::new());

/// Spawn a child its caller will wait for. Call [`release`] once it has.
pub fn spawn(cmd: &mut Command) -> io::Result<Child> {
    let mut owned = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    let child = cmd.spawn()?;
    owned.insert(child.id() as i32);
    Ok(child)
}

/// The caller has waited for `pid`; it's nobody's now.
pub fn release(pid: i32) {
    OWNED.lock().unwrap_or_else(|e| e.into_inner()).remove(&pid);
}

/// `Command::status`, recorded.
pub fn status(cmd: &mut Command) -> io::Result<ExitStatus> {
    let mut child = spawn(cmd)?;
    let result = child.wait();
    release(child.id() as i32);
    result
}

/// `Command::output`, recorded.
pub fn output(cmd: &mut Command) -> io::Result<Output> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = spawn(cmd)?;
    let pid = child.id() as i32;
    let result = child.wait_with_output();
    release(pid);
    result
}

/// Start sweeping, when this is PID 1.
pub fn start() {
    if std::process::id() != 1 {
        return;
    }
    std::thread::spawn(|| loop {
        sweep();
        std::thread::sleep(Duration::from_millis(200));
    });
}

/// Reap every zombie child nobody owns. Returns how many.
fn sweep() -> usize {
    let owned = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    let me = std::process::id() as i32;
    let Ok(entries) = std::fs::read_dir("/proc") else { return 0 };
    let mut reaped = 0;
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        if owned.contains(&pid) {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else { continue };
        if let Some((state, ppid)) = parse_stat(&stat) {
            if state == 'Z' && ppid == me {
                let mut status = 0;
                // SAFETY: waitpid on one zombie child of ours, without blocking.
                if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == pid {
                    reaped += 1;
                }
            }
        }
    }
    reaped
}

/// State and parent pid from `/proc/<pid>/stat`: `pid (comm) S ppid …`. The
/// command name can hold spaces and parentheses, so read after the last `)`.
fn parse_stat(stat: &str) -> Option<(char, i32)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse().ok()?;
    Some((state, ppid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_lines() {
        assert_eq!(parse_stat("42 (sleep) Z 1 42 42 0 -1"), Some(('Z', 1)));
        assert_eq!(parse_stat("7 (my (odd) name) S 3 7 7"), Some(('S', 3)));
        assert_eq!(parse_stat("garbage"), None);
    }

    /// An orphan's zombie is reaped; a child someone owns is left for them.
    /// Needs to be the parent of both, so it runs as a normal test here: the
    /// sweep only looks at this process's children.
    #[test]
    fn reaps_only_what_nobody_waits_for() {
        let mut owned = spawn(Command::new("/bin/true").stdin(Stdio::null())).unwrap();
        let stray = Command::new("/bin/true").spawn().unwrap();
        let stray_pid = stray.id() as i32;
        std::mem::forget(stray); // nobody will wait for this one
        std::thread::sleep(Duration::from_millis(300));
        let gone = |pid: i32| !std::path::Path::new(&format!("/proc/{pid}")).exists();
        let mut tries = 0;
        while !gone(stray_pid) && tries < 20 {
            sweep();
            tries += 1;
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(gone(stray_pid), "the stray zombie was reaped");
        assert!(owned.wait().unwrap().success(), "the owned child's status was left for its owner");
        release(owned.id() as i32);
    }
}
