//! Guest networking: when the VM has a network device, bring up `eth0` with
//! DHCP from Virtualization.framework's NAT, in the background so it overlaps
//! with unpacking the image. `/etc/udhcpc.script` (in the initramfs) writes the
//! address, the default route, `/etc/resolv.conf` and `/run/graviton-ip`.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

static ADDRESS: OnceLock<Option<String>> = OnceLock::new();

pub fn start() {
    if !Path::new("/sys/class/net/eth0").exists() {
        let _ = ADDRESS.set(None);
        return;
    }
    thread::spawn(|| {
        let _ = ADDRESS.set(configure());
    });
}

fn configure() -> Option<String> {
    let bb = "/bin/busybox";
    let _ = Command::new(bb).args(["ip", "link", "set", "lo", "up"]).status();
    let status = Command::new(bb)
        .args(["udhcpc", "-i", "eth0", "-f", "-q", "-n", "-t", "8", "-T", "1", "-A", "1", "-s", "/etc/udhcpc.script"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    fs::read_to_string("/run/graviton-ip").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// The guest's address, waiting up to `timeout` for DHCP. `None` without a
/// network device or when DHCP failed.
pub fn wait(timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(address) = ADDRESS.get() {
            return address.clone();
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}
