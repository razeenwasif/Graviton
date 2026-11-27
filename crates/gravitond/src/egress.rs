//! Host-enforced zero-bypass egress firewall, DNS proxy, chaos fault injection,
//! and HTTP/TLS SNI sniffer for restricted containers (`NetworkMode::None` VMs).

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, TcpStream, ToSocketAddrs};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use graviton_proto::{
    AGENT_VSOCK_PORT, AgentRequest, ChaosProfile, ContainerNetworkPolicy, EgressBridgeReply, EgressBridgeRequest,
    EgressPolicy, XRayEvent, XRayEventKind, encode_line,
};
use graviton_vmm::VmHandle;
use graviton_vmm::apple::AppleVm;

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Evaluate whether an outbound host/IP target is permitted by `egress`.
pub fn evaluate_egress(egress: &EgressPolicy, host: &str) -> bool {
    match egress {
        EgressPolicy::Unrestricted => true,
        EgressPolicy::Offline => false,
        EgressPolicy::Allowlist { .. } => {
            if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
                egress.allows_ip(ip)
            } else {
                egress.allows_host(host)
            }
        }
    }
}

fn is_disallowed_resolved_ip(ip: IpAddr, egress: &EgressPolicy) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return !egress.allows_ip(ip);
    }
    if let IpAddr::V4(v4) = ip {
        if v4.is_link_local() && !egress.allows_ip(ip) {
            return true;
        }
    }
    false
}

/// Inject `ChaosProfile` latency/jitter and loss check before dialing or replying.
fn apply_chaos(chaos: &ChaosProfile, rng: &AtomicU64) -> bool {
    if chaos.loss_percent >= 100 {
        return false;
    }
    if chaos.loss_percent > 0 {
        let r = rng.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed) % 100;
        if r < u64::from(chaos.loss_percent) {
            return false;
        }
    }
    let mut delay_ms = u64::from(chaos.latency_ms);
    if chaos.jitter_ms > 0 {
        let r = rng.fetch_add(0x517C_C1B7_2722_0A95, Ordering::Relaxed);
        let span = u64::from(chaos.jitter_ms);
        delay_ms = delay_ms.saturating_add(r % (span * 2 + 1)).saturating_sub(span);
    }
    if delay_ms > 0 {
        thread::sleep(Duration::from_millis(delay_ms));
    }
    true
}

/// Serve `AgentRequest::EgressBridge` on the macOS host until the VM stops.
pub fn spawn_gateway(
    vm: Arc<AppleVm>,
    bridge_fd: OwnedFd,
    policy: Arc<RwLock<ContainerNetworkPolicy>>,
    emit_xray: Arc<dyn Fn(XRayEvent) + Send + Sync>,
) {
    let writer = Arc::new(Mutex::new(File::from(bridge_fd)));
    let read_file = match writer.lock().unwrap().try_clone() {
        Ok(f) => f,
        Err(_) => return,
    };
    let rng = Arc::new(AtomicU64::new(0x1337_C0DE));

    let _ = thread::Builder::new().name("egress-gw".into()).spawn(move || {
        let reader = BufReader::new(read_file);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let Ok(req) = serde_json::from_str::<EgressBridgeRequest>(&line) else { continue };
            let vm = Arc::clone(&vm);
            let policy = Arc::clone(&policy);
            let writer = Arc::clone(&writer);
            let emit_xray = Arc::clone(&emit_xray);
            let rng = Arc::clone(&rng);
            let _ = thread::Builder::new().spawn(move || match req {
                EgressBridgeRequest::Dns { id, name, qtype } => {
                    let reply = handle_dns(id, &name, qtype, &policy, &emit_xray, &rng);
                    send_reply(&writer, &reply);
                }
                EgressBridgeRequest::Dial { id, host, port } => {
                    handle_dial(id, host, port, vm, policy, writer, emit_xray, rng);
                }
            });
        }
    });
}

fn send_reply(writer: &Mutex<File>, reply: &EgressBridgeReply) {
    if let Ok(mut buf) = serde_json::to_vec(reply) {
        buf.push(b'\n');
        let mut g = writer.lock().unwrap();
        let _ = g.write_all(&buf);
        let _ = g.flush();
    }
}

fn handle_dns(
    id: u64,
    name: &str,
    qtype: u16,
    policy: &RwLock<ContainerNetworkPolicy>,
    emit_xray: &Arc<dyn Fn(XRayEvent) + Send + Sync>,
    rng: &AtomicU64,
) -> EgressBridgeReply {
    let t0 = Instant::now();
    let pol = policy.read().unwrap().clone();
    if !evaluate_egress(&pol.egress, name) {
        emit_xray(XRayEvent {
            seq: 0,
            timestamp_ms: now_ms(),
            kind: XRayEventKind::Dns,
            action: "resolve".into(),
            target: name.to_string(),
            detail: Some("blocked by egress policy".into()),
            allowed: false,
            latency_us: Some(t0.elapsed().as_micros() as u64),
            bytes: None,
            pid: None,
        });
        return EgressBridgeReply::DnsResult { id, allowed: false, ips: vec![] };
    }
    if !apply_chaos(&pol.chaos, rng) {
        emit_xray(XRayEvent {
            seq: 0,
            timestamp_ms: now_ms(),
            kind: XRayEventKind::Dns,
            action: "resolve".into(),
            target: name.to_string(),
            detail: Some("dropped by chaos loss".into()),
            allowed: true,
            latency_us: Some(t0.elapsed().as_micros() as u64),
            bytes: None,
            pid: None,
        });
        return EgressBridgeReply::DnsResult { id, allowed: true, ips: vec![] };
    }

    let ips: Vec<String> = (name, 0u16)
        .to_socket_addrs()
        .map(|iter| {
            iter.map(|sa| sa.ip())
                .filter(|ip| !is_disallowed_resolved_ip(*ip, &pol.egress))
                .filter(|ip| match qtype {
                    1 => ip.is_ipv4(),
                    28 => ip.is_ipv6(),
                    _ => true,
                })
                .map(|ip| ip.to_string())
                .take(8)
                .collect()
        })
        .unwrap_or_default();

    emit_xray(XRayEvent {
        seq: 0,
        timestamp_ms: now_ms(),
        kind: XRayEventKind::Dns,
        action: "resolve".into(),
        target: name.to_string(),
        detail: Some(if ips.is_empty() { "NXDOMAIN".into() } else { ips.join(", ") }),
        allowed: true,
        latency_us: Some(t0.elapsed().as_micros() as u64),
        bytes: None,
        pid: None,
    });
    EgressBridgeReply::DnsResult { id, allowed: true, ips }
}

#[allow(clippy::too_many_arguments)]
fn handle_dial(
    id: u64,
    host: String,
    port: u16,
    vm: Arc<AppleVm>,
    policy: Arc<RwLock<ContainerNetworkPolicy>>,
    writer: Arc<Mutex<File>>,
    emit_xray: Arc<dyn Fn(XRayEvent) + Send + Sync>,
    rng: Arc<AtomicU64>,
) {
    let t0 = Instant::now();
    let pol = policy.read().unwrap().clone();
    if !evaluate_egress(&pol.egress, &host) {
        emit_xray(XRayEvent {
            seq: 0,
            timestamp_ms: now_ms(),
            kind: XRayEventKind::Net,
            action: "dial".into(),
            target: format!("{host}:{port}"),
            detail: Some("blocked by egress policy".into()),
            allowed: false,
            latency_us: Some(t0.elapsed().as_micros() as u64),
            bytes: None,
            pid: None,
        });
        send_reply(
            &writer,
            &EgressBridgeReply::DialResult {
                id,
                allowed: false,
                error: Some(format!("{host}:{port} blocked by Graviton egress policy")),
            },
        );
        return;
    }

    if !apply_chaos(&pol.chaos, &rng) {
        emit_xray(XRayEvent {
            seq: 0,
            timestamp_ms: now_ms(),
            kind: XRayEventKind::Net,
            action: "dial".into(),
            target: format!("{host}:{port}"),
            detail: Some("dropped by chaos loss".into()),
            allowed: false,
            latency_us: Some(t0.elapsed().as_micros() as u64),
            bytes: None,
            pid: None,
        });
        send_reply(
            &writer,
            &EgressBridgeReply::DialResult { id, allowed: true, error: Some("packet dropped by chaos loss".into()) },
        );
        return;
    }

    let addrs: Vec<_> = match (host.as_str(), port).to_socket_addrs() {
        Ok(it) => it.filter(|sa| !is_disallowed_resolved_ip(sa.ip(), &pol.egress)).collect(),
        Err(err) => {
            send_reply(&writer, &EgressBridgeReply::DialResult { id, allowed: true, error: Some(err.to_string()) });
            return;
        }
    };
    if addrs.is_empty() {
        send_reply(
            &writer,
            &EgressBridgeReply::DialResult { id, allowed: false, error: Some("no permitted destination IP".into()) },
        );
        return;
    }

    let mut tcp = None;
    let mut last_err = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(10)) {
            Ok(s) => {
                tcp = Some(s);
                break;
            }
            Err(e) => last_err = Some(e.to_string()),
        }
    }
    let Some(mut upstream) = tcp else {
        send_reply(
            &writer,
            &EgressBridgeReply::DialResult { id, allowed: true, error: last_err.or_else(|| Some("connect failed".into())) },
        );
        return;
    };
    let _ = upstream.set_nodelay(true);

    let vsock_fd = match vm.connect(AGENT_VSOCK_PORT) {
        Ok(fd) => fd,
        Err(err) => {
            send_reply(&writer, &EgressBridgeReply::DialResult { id, allowed: true, error: Some(err.to_string()) });
            return;
        }
    };
    let mut vsock = File::from(vsock_fd);
    let mut br = BufReader::new(match vsock.try_clone() {
        Ok(f) => f,
        Err(e) => {
            send_reply(&writer, &EgressBridgeReply::DialResult { id, allowed: true, error: Some(e.to_string()) });
            return;
        }
    });
    let mut ready_line = String::new();
    let _ = br.read_line(&mut ready_line);
    if vsock.write_all(&encode_line(&AgentRequest::EgressStream { id })).is_err() || vsock.flush().is_err() {
        send_reply(&writer, &EgressBridgeReply::DialResult { id, allowed: true, error: Some("vsock stream setup failed".into()) });
        return;
    }

    send_reply(&writer, &EgressBridgeReply::DialResult { id, allowed: true, error: None });

    let bw_kbps = pol.chaos.bandwidth_kbps;
    let total_bytes = Arc::new(AtomicU64::new(0));
    let meta = Arc::new(Mutex::new(FlowMeta::default()));

    let mut up_r = match upstream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut vsock_w = match vsock.try_clone() {
        Ok(f) => f,
        Err(_) => return,
    };

    let bytes_in = Arc::clone(&total_bytes);
    let meta_in = Arc::clone(&meta);
    let down = thread::spawn(move || {
        let mut buf = [0u8; 16384];
        let mut first = true;
        while let Ok(n) = up_r.read(&mut buf) {
            if n == 0 {
                break;
            }
            if first {
                first = false;
                meta_in.lock().unwrap().http_status = extract_http_status(&buf[..n]);
            }
            bytes_in.fetch_add(n as u64, Ordering::Relaxed);
            if let Some(kbps) = bw_kbps {
                let us = (n as u64 * 8_000) / u64::from(kbps.max(1));
                if us > 0 {
                    thread::sleep(Duration::from_micros(us.min(100_000)));
                }
            }
            if vsock_w.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        let _ = vsock_w.flush();
    });

    let mut buf = [0u8; 16384];
    let mut first_out = true;
    while let Ok(n) = br.read(&mut buf) {
        if n == 0 {
            break;
        }
        if first_out {
            first_out = false;
            let mut m = meta.lock().unwrap();
            if let Some((method, path)) = extract_http_method_path(&buf[..n]) {
                m.method = Some(method);
                m.path = Some(path);
            } else if let Some(sni) = extract_tls_sni(&buf[..n]) {
                m.sni = Some(sni);
            }
        }
        total_bytes.fetch_add(n as u64, Ordering::Relaxed);
        if upstream.write_all(&buf[..n]).is_err() {
            break;
        }
    }
    let _ = upstream.shutdown(Shutdown::Write);
    let _ = down.join();

    let m = meta.lock().unwrap().clone();
    let (action, detail) = match (m.method, m.path, m.http_status, m.sni) {
        (Some(method), Some(path), status, _) => {
            let d = status.map_or_else(|| path.clone(), |st| format!("{path} -> {st}"));
            (method, Some(d))
        }
        (_, _, _, Some(sni)) => ("TLS".into(), Some(format!("sni={sni}"))),
        _ => ("TCP".into(), None),
    };
    emit_xray(XRayEvent {
        seq: 0,
        timestamp_ms: now_ms(),
        kind: XRayEventKind::Net,
        action,
        target: format!("{host}:{port}"),
        detail,
        allowed: true,
        latency_us: Some(t0.elapsed().as_micros() as u64),
        bytes: Some(total_bytes.load(Ordering::Relaxed)),
        pid: None,
    });
}

#[derive(Default, Clone)]
struct FlowMeta {
    method: Option<String>,
    path: Option<String>,
    http_status: Option<u16>,
    sni: Option<String>,
}

pub fn extract_http_method_path(buf: &[u8]) -> Option<(String, String)> {
    let line_end = buf.windows(2).position(|w| w == b"\r\n").unwrap_or(buf.len().min(256));
    let line = std::str::from_utf8(&buf[..line_end]).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    let path = parts.next()?;
    let proto = parts.next()?;
    if proto.starts_with("HTTP/1.")
        && matches!(method, "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS")
    {
        Some((method.to_string(), path.to_string()))
    } else {
        None
    }
}

pub fn extract_http_status(buf: &[u8]) -> Option<u16> {
    let line_end = buf.windows(2).position(|w| w == b"\r\n").unwrap_or(buf.len().min(64));
    let line = std::str::from_utf8(&buf[..line_end]).ok()?;
    let mut parts = line.split_whitespace();
    let proto = parts.next()?;
    if !proto.starts_with("HTTP/1.") {
        return None;
    }
    parts.next()?.parse().ok()
}

pub fn extract_tls_sni(buf: &[u8]) -> Option<String> {
    if buf.len() < 43 || buf[0] != 0x16 || buf[5] != 0x01 {
        return None;
    }
    let rec_len = u16::from_be_bytes([buf[3], buf[4]]) as usize;
    let end = (5 + rec_len).min(buf.len());
    let mut pos = 5 + 4 + 2 + 32; // record hdr + handshake hdr + version + random
    let sid_len = *buf.get(pos)? as usize;
    pos += 1 + sid_len;
    let cs_len = u16::from_be_bytes([*buf.get(pos)?, *buf.get(pos + 1)?]) as usize;
    pos += 2 + cs_len;
    let comp_len = *buf.get(pos)? as usize;
    pos += 1 + comp_len;
    if pos + 2 > end {
        return None;
    }
    let ext_total = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
    pos += 2;
    let ext_end = (pos + ext_total).min(end);
    while pos + 4 <= ext_end {
        let etype = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        let elen = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]) as usize;
        pos += 4;
        if pos + elen > ext_end {
            break;
        }
        if etype == 0x0000 && elen >= 5 {
            let sni_data = &buf[pos..pos + elen];
            if sni_data[2] == 0 {
                let host_len = u16::from_be_bytes([sni_data[3], sni_data[4]]) as usize;
                if 5 + host_len <= sni_data.len() {
                    return std::str::from_utf8(&sni_data[5..5 + host_len]).ok().map(str::to_string);
                }
            }
        }
        pos += elen;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn egress_evaluation_and_sniffers_work() {
        let offline = EgressPolicy::Offline;
        assert!(!evaluate_egress(&offline, "example.com"));
        assert!(!evaluate_egress(&offline, "8.8.8.8"));

        let allow = EgressPolicy::parse("allow:api.stripe.com,*.pypi.org,10.0.0.0/8").unwrap();
        assert!(evaluate_egress(&allow, "api.stripe.com"));
        assert!(evaluate_egress(&allow, "files.pypi.org"));
        assert!(!evaluate_egress(&allow, "neverssl.com"));
        assert!(evaluate_egress(&allow, "10.20.30.40"));
        assert!(!evaluate_egress(&allow, "1.1.1.1"));

        assert_eq!(
            extract_http_method_path(b"GET /v1/health HTTP/1.1\r\nHost: api.example.com\r\n\r\n"),
            Some(("GET".into(), "/v1/health".into()))
        );
        assert_eq!(extract_http_status(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n"), Some(200));
    }
}
