//! Resource counters for the guest, read from /proc. One guest runs one
//! container, so the guest's numbers are the container's.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

#[cfg(target_os = "linux")]
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::fs;

#[cfg(target_os = "linux")]
use graviton_proto::{GuestStats, ProcessInfo};

#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn memory() -> (u64, u64) {
    let info = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let vmstat = fs::read_to_string("/proc/vmstat").unwrap_or_default();
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE).max(4096) as u64 };
    parse_memory(&info, &vmstat, page)
}

fn parse_memory(meminfo: &str, vmstat: &str, page_size: u64) -> (u64, u64) {
    let mem_field = |name: &str| {
        meminfo
            .lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
            .map_or(0, |kib| kib * 1024)
    };
    let vm_field = |name: &str| {
        vmstat
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let raw_total = mem_field("MemTotal");
    let avail = mem_field("MemAvailable");
    // Linux virtio_balloon with VIRTIO_BALLOON_F_DEFLATE_ON_OOM leaves MemTotal
    // constant while inflated pages reduce MemAvailable; subtract held balloon
    // pages so (MemTotal - MemAvailable) stays equal to genuine guest working set.
    let balloon_pages = vm_field("balloon_inflate").saturating_sub(vm_field("balloon_deflate"));
    let balloon_bytes = balloon_pages.saturating_mul(page_size);
    let effective_total = raw_total.saturating_sub(balloon_bytes).max(avail);
    (effective_total, avail)
}

/// Bytes received and sent on every interface but loopback.
#[cfg(target_os = "linux")]
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

/// Processes inside container PID namespaces (excluding the agent, DHCP,
/// outer launchers, and kernel threads), mapped to their container-local PIDs.
#[cfg(target_os = "linux")]
fn container_processes() -> Vec<ProcessInfo> {
    let own_pid_ns = fs::read_link("/proc/self/ns/pid").ok();
    // SAFETY: sysconf has no preconditions.
    let (ticks, page) = unsafe { (libc::sysconf(libc::_SC_CLK_TCK).max(1) as u64, libc::sysconf(libc::_SC_PAGESIZE).max(1) as u64) };
    let Ok(dir) = fs::read_dir("/proc") else { return vec![] };
    let mut raw: Vec<(i32, i32, ProcessInfo)> = Vec::new();
    let mut outer_to_inner: HashMap<i32, i32> = HashMap::new();
    for entry in dir.flatten() {
        let Some(outer_pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        let base = format!("/proc/{outer_pid}");
        if fs::read_link(format!("{base}/ns/pid")).ok() == own_pid_ns {
            continue;
        }
        let Ok(stat) = fs::read_to_string(format!("{base}/stat")) else { continue };
        // pid (comm) state ppid ... ; comm may contain spaces and parentheses.
        let Some(close) = stat.rfind(')') else { continue };
        let comm = stat[stat.find('(').map_or(0, |i| i + 1)..close].to_string();
        let f: Vec<&str> = stat[close + 1..].split_whitespace().collect();
        // After ")": state(0) ppid(1) ... utime(11) stime(12) ... rss(21)
        let num = |i: usize| f.get(i).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
        let outer_ppid = num(1) as i32;
        if outer_pid == 2 || outer_ppid == 2 {
            continue; // kthreadd and kernel threads
        }
        let status = fs::read_to_string(format!("{base}/status")).unwrap_or_default();
        let inner_pid = crate::ns::parse_nspid(&status).unwrap_or(outer_pid);
        outer_to_inner.insert(outer_pid, inner_pid);

        let cmdline = fs::read(format!("{base}/cmdline")).unwrap_or_default();
        let command = if inner_pid == 1 && comm == "init" {
            "/init".to_string()
        } else if cmdline.is_empty() {
            format!("[{comm}]")
        } else {
            String::from_utf8_lossy(&cmdline).trim_end_matches('\0').replace('\0', " ")
        };
        let uid = status
            .lines()
            .find_map(|l| l.strip_prefix("Uid:"))
            .and_then(|r| r.split_whitespace().next()?.parse().ok())
            .unwrap_or(0);
        raw.push((
            outer_pid,
            outer_ppid,
            ProcessInfo {
                pid: inner_pid,
                ppid: 0,
                uid,
                state: f.first().unwrap_or(&"?").to_string(),
                command,
                rss_bytes: num(21).max(0) as u64 * page,
                cpu_ms: (num(11) + num(12)).max(0) as u64 * 1000 / ticks,
            },
        ));
    }
    let mut out: Vec<ProcessInfo> = raw
        .into_iter()
        .map(|(_, outer_ppid, mut info)| {
            info.ppid = if info.pid == 1 { 0 } else { outer_to_inner.get(&outer_ppid).copied().unwrap_or(0) };
            info
        })
        .collect();
    out.sort_by_key(|p| p.pid);
    out
}

#[cfg(test)]
mod tests {
    use super::parse_memory;

    #[test]
    fn parse_memory_subtracts_inflated_balloon_pages() {
        let meminfo = "MemTotal:         507612 kB\nMemFree:           21000 kB\nMemAvailable:      58152 kB\n";
        let vmstat = "nr_free_pages 5250\nballoon_inflate 90112\nballoon_deflate 0\n";
        let (total, avail) = parse_memory(meminfo, vmstat, 4096);
        assert_eq!(avail, 58152 * 1024);
        assert_eq!(total.saturating_sub(avail), 89_012 * 1024);

        // Deflated or absent balloon leaves MemTotal unchanged.
        let (t2, a2) = parse_memory(meminfo, "balloon_inflate 100\nballoon_deflate 200\n", 4096);
        assert_eq!((t2, a2), (507_612 * 1024, 58_152 * 1024));
    }
}
