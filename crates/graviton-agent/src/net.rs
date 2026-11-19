//! Guest networking: when the VM has a network device, bring up `eth0` with
//! DHCP from Virtualization.framework's NAT, in the background so it overlaps
//! with unpacking the image. `/etc/udhcpc.script` (in the initramfs) writes the
//! address, the default route, `/etc/resolv.conf` and `/run/graviton-ip`.
//!
//! The NAT keeps guests from reaching each other, so container VMs also get
//! `eth1`, wired to gravitond's switch. Once DHCP is done, the subnet is
//! routed over `eth1` (as its two halves, which beat the /24 on `eth0`), with
//! the gateway kept on `eth0`. Other containers then answer ARP on `eth1` for
//! the same addresses they have on `eth0`.

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
    let _ = crate::reap::status(Command::new(bb).args(["ip", "link", "set", "lo", "up"]));
    let status = crate::reap::status(
        Command::new(bb)
            .args(["udhcpc", "-i", "eth0", "-f", "-q", "-n", "-t", "8", "-T", "1", "-A", "1", "-s", "/etc/udhcpc.script"])
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )
    .ok()?;
    if !status.success() {
        return None;
    }
    let ip = fs::read_to_string("/run/graviton-ip").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())?;
    if Path::new("/sys/class/net/eth1").exists() {
        if let Err(err) = container_network(&ip) {
            eprintln!("graviton-agent: container network: {err}");
        }
    }
    Some(ip)
}

fn ip(args: &[&str]) -> Result<String, String> {
    let out = crate::reap::output(Command::new("/bin/busybox").arg("ip").args(args)).map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("ip {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Route the NAT subnet (but not its gateway) over eth1.
fn container_network(address: &str) -> Result<(), String> {
    let addr_line = ip(&["-o", "-4", "addr", "show", "dev", "eth0"])?;
    let cidr = addr_line.split_whitespace().skip_while(|w| *w != "inet").nth(1).ok_or("eth0 has no address")?;
    let prefix: u32 = cidr.split_once('/').and_then(|(_, p)| p.parse().ok()).ok_or("eth0's address has no prefix")?;
    let gateway = ip(&["route", "show", "default"])?
        .split_whitespace()
        .skip_while(|w| *w != "via")
        .nth(1)
        .map(String::from)
        .ok_or("no default route")?;
    let halves = split(address, prefix).ok_or("subnet too small to split")?;
    ip(&["link", "set", "eth1", "up"])?;
    // The gateway (DNS, the internet, the Mac) stays on eth0.
    ip(&["route", "replace", &format!("{gateway}/32"), "dev", "eth0", "src", address])?;
    for half in halves {
        ip(&["route", "replace", &half, "dev", "eth1", "src", address])?;
    }
    Ok(())
}

/// The two halves of `address`'s subnet: `192.168.64.60`, 24 → `192.168.64.0/25`, `192.168.64.128/25`.
fn split(address: &str, prefix: u32) -> Option<[String; 2]> {
    if !(1..=30).contains(&prefix) {
        return None;
    }
    let ip: std::net::Ipv4Addr = address.parse().ok()?;
    let mask = u32::MAX << (32 - prefix);
    let net = u32::from(ip) & mask;
    let half = 1u32 << (31 - prefix);
    let a = std::net::Ipv4Addr::from(net);
    let b = std::net::Ipv4Addr::from(net + half);
    Some([format!("{a}/{}", prefix + 1), format!("{b}/{}", prefix + 1)])
}

#[cfg(test)]
mod tests {
    #[test]
    fn splits_subnets() {
        assert_eq!(super::split("192.168.64.60", 24).unwrap(), ["192.168.64.0/25", "192.168.64.128/25"]);
        assert_eq!(super::split("10.1.2.3", 16).unwrap(), ["10.1.0.0/17", "10.1.128.0/17"]);
        assert!(super::split("10.0.0.1", 31).is_none());
    }
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
