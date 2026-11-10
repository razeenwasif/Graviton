//! Resource counters for the guest, read from /proc. One guest runs one
//! container, so the guest's numbers are the container's.

use std::fs;

use graviton_proto::{GuestStats, ProcessInfo};

pub fn collect(processes: bool) -> GuestStats {
    let (cpu_busy, cpu_total, cpus) = cpu();
    let (memory_total_bytes, memory_available_bytes) = memory();
    let (net_rx_bytes, net_tx_bytes) = network();
    GuestStats {
        cpus,
        cpu_busy,
        cpu_total,
        memory_total_bytes,
        memory_available_bytes,
        net_rx_bytes,
        net_tx_bytes,
        processes: if processes { container_processes() } else { vec![] },
    }
}

/// Busy and total jiffies across all CPUs, and the CPU count.
fn cpu() -> (u64, u64, u32) {
    let stat = fs::read_to_string("/proc/stat").unwrap_or_default();
    let mut cpus = 0;
    let (mut busy, mut total) = (0, 0);
    for line in stat.lines() {
        if line.starts_with("cpu ") {
            // user nice system idle iowait irq softirq steal (guest time is counted in user)
            let v: Vec<u64> = line.split_whitespace().skip(1).take(8).filter_map(|n| n.parse().ok()).collect();
            total = v.iter().sum();
            let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
            busy = total.saturating_sub(idle);
        } else if line.starts_with("cpu") {
            cpus += 1;
        }
    }
    (busy, total, cpus)
}

fn memory() -> (u64, u64) {
    let info = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let field = |name: &str| {
        info.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
            .map_or(0, |kib| kib * 1024)
    };
    (field("MemTotal"), field("MemAvailable"))
}

/// Bytes received and sent on every interface but loopback.
fn network() -> (u64, u64) {
    let dev = fs::read_to_string("/proc/net/dev").unwrap_or_default();
    let (mut rx, mut tx) = (0, 0);
    for line in dev.lines().skip(2) {
        let Some((name, counters)) = line.split_once(':') else { continue };
        if name.trim() == "lo" {
            continue;
        }
        let v: Vec<u64> = counters.split_whitespace().filter_map(|n| n.parse().ok()).collect();
        if v.len() >= 9 {
            rx += v[0];
            tx += v[8];
        }
    }
    (rx, tx)
}

/// Processes in a container: those in a different mount namespace from the
/// agent's (which leaves out the agent, the DHCP client and kernel threads).
fn container_processes() -> Vec<ProcessInfo> {
    let own_ns = fs::read_link("/proc/self/ns/mnt").ok();
    // SAFETY: sysconf has no preconditions.
    let (ticks, page) = unsafe { (libc::sysconf(libc::_SC_CLK_TCK).max(1) as u64, libc::sysconf(libc::_SC_PAGESIZE).max(1) as u64) };
    let Ok(dir) = fs::read_dir("/proc") else { return vec![] };
    let mut out = Vec::new();
    for entry in dir.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        let base = format!("/proc/{pid}");
        if fs::read_link(format!("{base}/ns/mnt")).ok() == own_ns {
            continue;
        }
        let Ok(stat) = fs::read_to_string(format!("{base}/stat")) else { continue };
        // pid (comm) state ppid ... ; comm may contain spaces and parentheses.
        let Some(close) = stat.rfind(')') else { continue };
        let comm = stat[stat.find('(').map_or(0, |i| i + 1)..close].to_string();
        let f: Vec<&str> = stat[close + 1..].split_whitespace().collect();
        // After ")": state(0) ppid(1) ... utime(11) stime(12) ... rss(21)
        let num = |i: usize| f.get(i).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
        if pid == 2 || num(1) == 2 {
            continue; // kthreadd and kernel threads (kdevtmpfs has a mount namespace of its own)
        }
        let cmdline = fs::read(format!("{base}/cmdline")).unwrap_or_default();
        let command = if cmdline.is_empty() {
            format!("[{comm}]")
        } else {
            String::from_utf8_lossy(&cmdline).trim_end_matches('\0').replace('\0', " ")
        };
        let uid = fs::read_to_string(format!("{base}/status"))
            .ok()
            .and_then(|s| s.lines().find_map(|l| l.strip_prefix("Uid:")).and_then(|r| r.split_whitespace().next()?.parse().ok()))
            .unwrap_or(0);
        out.push(ProcessInfo {
            pid,
            ppid: num(1) as i32,
            uid,
            state: f.first().unwrap_or(&"?").to_string(),
            command,
            rss_bytes: num(21).max(0) as u64 * page,
            cpu_ms: (num(11) + num(12)).max(0) as u64 * 1000 / ticks,
        });
    }
    out.sort_by_key(|p| p.pid);
    out
}
