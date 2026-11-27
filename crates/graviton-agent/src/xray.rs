//! Opt-in Hypervisor X-Ray event collector (`proc`, `file`, and direct `net` fallback).

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Write;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use graviton_proto::{AgentEvent, XRayEvent, XRayEventKind, encode_line};

const MAX_QUEUE: usize = 1024;

static ENABLED: AtomicBool = AtomicBool::new(false);
static ACTIVE_CONTAINER: Mutex<Option<String>> = Mutex::new(None);
static COLLECTOR_STARTED: OnceLock<()> = OnceLock::new();
static SUBSCRIBERS: Mutex<Vec<mpsc::Sender<Vec<XRayEvent>>>> = Mutex::new(Vec::new());
static BUFFER: Mutex<VecDeque<XRayEvent>> = Mutex::new(VecDeque::new());

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn set_enabled(container: &str, enabled: bool) {
    *ACTIVE_CONTAINER.lock().unwrap() = Some(container.to_string());
    ENABLED.store(enabled, Ordering::Relaxed);
    if enabled {
        ensure_collector();
    }
}

pub fn emit(event: XRayEvent) {
    if !is_enabled() {
        return;
    }
    let mut buf = BUFFER.lock().unwrap();
    if buf.len() >= MAX_QUEUE {
        buf.pop_front();
    }
    buf.push_back(event);
}

pub fn watch_stream(container: &str, mut stream: fs::File) -> std::io::Result<()> {
    set_enabled(container, true);
    let (tx, rx) = mpsc::channel::<Vec<XRayEvent>>();
    SUBSCRIBERS.lock().unwrap().push(tx);

    while let Ok(events) = rx.recv() {
        if events.is_empty() {
            continue;
        }
        let line = encode_line(&AgentEvent::XRayBatch { events });
        if stream.write_all(&line).is_err() || stream.flush().is_err() {
            break;
        }
    }
    Ok(())
}

fn ensure_collector() {
    COLLECTOR_STARTED.get_or_init(|| {
        let _ = thread::Builder::new().name("xray-flush".into()).spawn(flush_loop);
        let _ = thread::Builder::new().name("xray-scan".into()).spawn(scan_loop);
    });
}

fn flush_loop() {
    loop {
        thread::sleep(Duration::from_millis(150));
        let batch: Vec<XRayEvent> = {
            let mut buf = BUFFER.lock().unwrap();
            buf.drain(..).collect()
        };
        if batch.is_empty() {
            continue;
        }
        let mut subs = SUBSCRIBERS.lock().unwrap();
        subs.retain(|tx| tx.send(batch.clone()).is_ok());
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

#[derive(Clone, PartialEq, Eq)]
struct ProcEntry {
    cmdline: String,
    uid_comm: String,
}

#[derive(Clone, PartialEq, Eq)]
struct FileMeta {
    len: u64,
    mtime_ns: i128,
}

fn scan_loop() {
    let own_pid_ns = fs::read_link("/proc/self/ns/pid").ok();
    let mut known_procs: HashMap<i32, ProcEntry> = HashMap::new();
    let mut known_files: HashMap<String, FileMeta> = HashMap::new();
    let mut known_socks: HashSet<(String, u16)> = HashSet::new();
    let mut seeded = false;

    loop {
        thread::sleep(Duration::from_millis(250));
        if !is_enabled() {
            seeded = false;
            known_procs.clear();
            known_files.clear();
            known_socks.clear();
            continue;
        }

        let container_id = ACTIVE_CONTAINER.lock().unwrap().clone();
        let current_procs = snapshot_procs(own_pid_ns.as_ref());
        let current_files = container_id.as_deref().map(snapshot_upper_files).unwrap_or_default();
        let current_socks = if crate::egress_proxy::is_active() {
            HashSet::new()
        } else {
            snapshot_tcp_remotes()
        };

        if !seeded {
            known_procs = current_procs;
            known_files = current_files;
            known_socks = current_socks;
            seeded = true;
            continue;
        }

        let ts = now_ms();
        for (pid, info) in &current_procs {
            if known_procs.get(pid) != Some(info) {
                emit(XRayEvent {
                    seq: 0,
                    timestamp_ms: ts,
                    kind: XRayEventKind::Proc,
                    action: "exec".into(),
                    target: info.cmdline.clone(),
                    detail: Some(info.uid_comm.clone()),
                    allowed: true,
                    latency_us: None,
                    bytes: None,
                    pid: Some(*pid),
                });
            }
        }
        for (pid, info) in &known_procs {
            if !current_procs.contains_key(pid) {
                emit(XRayEvent {
                    seq: 0,
                    timestamp_ms: ts,
                    kind: XRayEventKind::Proc,
                    action: "exit".into(),
                    target: info.cmdline.clone(),
                    detail: None,
                    allowed: true,
                    latency_us: None,
                    bytes: None,
                    pid: Some(*pid),
                });
            }
        }

        for (path, meta) in &current_files {
            match known_files.get(path) {
                None => emit(XRayEvent {
                    seq: 0,
                    timestamp_ms: ts,
                    kind: XRayEventKind::File,
                    action: "create".into(),
                    target: path.clone(),
                    detail: None,
                    allowed: true,
                    latency_us: None,
                    bytes: Some(meta.len),
                    pid: None,
                }),
                Some(prev) if prev != meta => emit(XRayEvent {
                    seq: 0,
                    timestamp_ms: ts,
                    kind: XRayEventKind::File,
                    action: "write".into(),
                    target: path.clone(),
                    detail: None,
                    allowed: true,
                    latency_us: None,
                    bytes: Some(meta.len),
                    pid: None,
                }),
                _ => {}
            }
        }
        for path in known_files.keys() {
            if !current_files.contains_key(path) {
                emit(XRayEvent {
                    seq: 0,
                    timestamp_ms: ts,
                    kind: XRayEventKind::File,
                    action: "unlink".into(),
                    target: path.clone(),
                    detail: None,
                    allowed: true,
                    latency_us: None,
                    bytes: None,
                    pid: None,
                });
            }
        }

        for (ip, port) in &current_socks {
            if !known_socks.contains(&(ip.clone(), *port)) {
                emit(XRayEvent {
                    seq: 0,
                    timestamp_ms: ts,
                    kind: XRayEventKind::Net,
                    action: "connect".into(),
                    target: format!("{ip}:{port}"),
                    detail: Some("direct TCP".into()),
                    allowed: true,
                    latency_us: None,
                    bytes: None,
                    pid: None,
                });
            }
        }

        known_procs = current_procs;
        known_files = current_files;
        known_socks = current_socks;
    }
}

fn snapshot_procs(own_pid_ns: Option<&PathBuf>) -> HashMap<i32, ProcEntry> {
    let mut out = HashMap::new();
    let Ok(rd) = fs::read_dir("/proc") else { return out };
    for entry in rd.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else { continue };
        let base = entry.path();
        if let Some(agent_ns) = own_pid_ns {
            if fs::read_link(base.join("ns/pid")).ok().as_ref() == Some(agent_ns) {
                continue;
            }
        }
        let comm = fs::read_to_string(base.join("comm")).unwrap_or_default().trim().to_string();
        if comm.is_empty() {
            continue;
        }
        let cmdline_bytes = fs::read(base.join("cmdline")).unwrap_or_default();
        let cmdline = if cmdline_bytes.is_empty() {
            comm.clone()
        } else {
            cmdline_bytes
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        };
        out.insert(pid, ProcEntry { cmdline, uid_comm: comm });
    }
    out
}

fn snapshot_upper_files(container_id: &str) -> HashMap<String, FileMeta> {
    let mut out = HashMap::new();
    let Ok(root) = crate::container::rootfs(container_id) else { return out };
    let Some(base) = root.parent() else { return out };
    let upper = base.join("upper");
    if !upper.is_dir() {
        return out;
    }
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        if out.len() >= 2048 {
            break;
        }
        let dir = if rel.as_os_str().is_empty() { upper.clone() } else { upper.join(&rel) };
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for ent in rd.flatten() {
            let child_rel = rel.join(ent.file_name());
            let Ok(meta) = fs::symlink_metadata(upper.join(&child_rel)) else { continue };
            let ft = meta.file_type();
            if ft.is_dir() {
                stack.push(child_rel);
            } else {
                let mtime_ns = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos() as i128);
                let path_str = format!("/{}", child_rel.to_string_lossy());
                out.insert(path_str, FileMeta { len: meta.len(), mtime_ns });
            }
        }
    }
    out
}

fn snapshot_tcp_remotes() -> HashSet<(String, u16)> {
    let mut out = HashSet::new();
    let Ok(proc_net) = fs::read_to_string("/proc/net/tcp") else { return out };
    for line in proc_net.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 {
            continue;
        }
        // State 01 = ESTABLISHED, 02 = SYN_SENT.
        if cols[3] != "01" && cols[3] != "02" {
            continue;
        }
        let Some((ip_hex, port_hex)) = cols[2].split_once(':') else { continue };
        let Ok(raw_ip) = u32::from_str_radix(ip_hex, 16) else { continue };
        let Ok(port) = u16::from_str_radix(port_hex, 16) else { continue };
        if port == 0 {
            continue;
        }
        let ip = Ipv4Addr::from(u32::from_be(raw_ip));
        if ip.is_loopback() || ip.is_unspecified() {
            continue;
        }
        out.insert((ip.to_string(), port));
    }
    out
}
