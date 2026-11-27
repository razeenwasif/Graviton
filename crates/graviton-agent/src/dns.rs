//! Embedded DNS resolver inside the guest VM (`127.0.0.11:53`, UDP + TCP).
//!
//! Container names (and `<name>.graviton`, container IDs, and reverse PTR
//! lookups for container IPs) are answered directly from the live table pushed
//! by `gravitond` via [`AgentRequest::SetHosts`]. Every other query is
//! forwarded raw to the upstream nameservers that `udhcpc` wrote to the
//! guest's `/etc/resolv.conf` (Virtualization.framework's NAT DNS on `eth0`),
//! preserving EDNS0, DNSSEC, VPN/Tailscale split-DNS, and TCP fallback.

use std::fs;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use graviton_proto::dns::{HostTable, answer, build_ip_reply, parse_question, parse_resolv_conf, refused, servfail};
use graviton_proto::{HostEntry, XRayEvent, XRayEventKind};

/// Loopback address the guest DNS resolver binds (matches Docker's convention
/// so it doesn't collide with anything a container binds on `127.0.0.1:53`).
pub const LISTEN_IP: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 11);

const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(2);
const TCP_IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_UDP_PACKET: usize = 4096;
const MAX_INFLIGHT: usize = 64;

static TABLE: OnceLock<RwLock<(u64, HostTable)>> = OnceLock::new();
static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
static INFLIGHT: AtomicUsize = AtomicUsize::new(0);

fn table() -> &'static RwLock<(u64, HostTable)> {
    TABLE.get_or_init(|| RwLock::new((0, HostTable::default())))
}

/// Replace the container name table if `generation` is newer than the last one applied.
pub fn set_hosts(generation: u64, hosts: &[HostEntry]) {
    let next = HostTable::from_entries(hosts);
    let mut guard = table().write().unwrap();
    if generation > guard.0 {
        *guard = (generation, next);
    }
}

/// Start the UDP + TCP resolver on `127.0.0.11:53` (idempotent per VM boot).
pub fn start() -> io::Result<()> {
    STARTED
        .get_or_init(|| start_inner().map_err(|e| e.to_string()))
        .clone()
        .map_err(io::Error::other)
}

fn start_inner() -> io::Result<()> {
    let resolv = fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    let upstreams = Arc::new(parse_resolv_conf(&resolv, Some(IpAddr::V4(LISTEN_IP))));

    let addr = SocketAddr::new(IpAddr::V4(LISTEN_IP), 53);
    let udp = Arc::new(UdpSocket::bind(addr)?);
    let tcp = TcpListener::bind(addr)?;

    let udp_up = Arc::clone(&upstreams);
    let udp_sock = Arc::clone(&udp);
    thread::Builder::new().name("dns-udp".into()).spawn(move || serve_udp(udp_sock, udp_up))?;

    let tcp_up = Arc::clone(&upstreams);
    thread::Builder::new().name("dns-tcp".into()).spawn(move || serve_tcp(tcp, tcp_up))?;

    Ok(())
}

struct InflightGuard;

impl InflightGuard {
    fn try_acquire() -> Option<Self> {
        let prev = INFLIGHT.fetch_add(1, Ordering::Relaxed);
        if prev >= MAX_INFLIGHT {
            INFLIGHT.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        Some(Self)
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        INFLIGHT.fetch_sub(1, Ordering::Relaxed);
    }
}

fn answer_local(pkt: &[u8]) -> Option<Vec<u8>> {
    let guard = table().read().unwrap();
    answer(pkt, &guard.1)
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn record_dns_xray(target: &str, detail: &str, allowed: bool, latency_us: u64) {
    crate::xray::emit(XRayEvent {
        seq: 0,
        timestamp_ms: now_ms(),
        kind: XRayEventKind::Dns,
        action: "resolve".into(),
        target: target.to_string(),
        detail: Some(detail.to_string()),
        allowed,
        latency_us: Some(latency_us),
        bytes: None,
        pid: None,
    });
}

fn resolve_external_udp(query: &[u8], upstreams: &[SocketAddr]) -> Option<Vec<u8>> {
    let t0 = Instant::now();
    let q = parse_question(query);
    if crate::egress_proxy::is_active() {
        if let Some((ref name, qtype)) = q {
            match crate::egress_proxy::dns_lookup(name, qtype) {
                Ok(Some(ips)) => {
                    record_dns_xray(name, &ips.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "), true, t0.elapsed().as_micros() as u64);
                    return build_ip_reply(query, &ips);
                }
                Ok(None) => {
                    record_dns_xray(name, "blocked by egress policy", false, t0.elapsed().as_micros() as u64);
                    return refused(query);
                }
                Err(_) => return servfail(query),
            }
        }
    }
    let reply = forward_udp(query, upstreams);
    if let Some((ref name, _)) = q {
        record_dns_xray(name, if reply.is_some() { "upstream" } else { "timeout" }, true, t0.elapsed().as_micros() as u64);
    }
    reply.or_else(|| servfail(query))
}

fn resolve_external_tcp(query: &[u8], upstreams: &[SocketAddr]) -> Option<Vec<u8>> {
    let t0 = Instant::now();
    let q = parse_question(query);
    if crate::egress_proxy::is_active() {
        if let Some((ref name, qtype)) = q {
            match crate::egress_proxy::dns_lookup(name, qtype) {
                Ok(Some(ips)) => {
                    record_dns_xray(name, &ips.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "), true, t0.elapsed().as_micros() as u64);
                    return build_ip_reply(query, &ips);
                }
                Ok(None) => {
                    record_dns_xray(name, "blocked by egress policy", false, t0.elapsed().as_micros() as u64);
                    return refused(query);
                }
                Err(_) => return servfail(query),
            }
        }
    }
    let reply = forward_tcp(query, upstreams);
    if let Some((ref name, _)) = q {
        record_dns_xray(name, if reply.is_some() { "upstream" } else { "timeout" }, true, t0.elapsed().as_micros() as u64);
    }
    reply.or_else(|| servfail(query))
}

fn serve_udp(sock: Arc<UdpSocket>, upstreams: Arc<Vec<SocketAddr>>) {
    let mut buf = [0u8; MAX_UDP_PACKET];
    loop {
        let (n, src) = match sock.recv_from(&mut buf) {
            Ok(v) => v,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
        };
        let pkt = &buf[..n];
        if let Some(reply) = answer_local(pkt) {
            if let Some((name, _)) = parse_question(pkt) {
                record_dns_xray(&name, "local container table", true, 0);
            }
            let _ = sock.send_to(&reply, src);
            continue;
        }
        let Some(guard) = InflightGuard::try_acquire() else {
            if let Some(err) = servfail(pkt) {
                let _ = sock.send_to(&err, src);
            }
            continue;
        };
        let query = pkt.to_vec();
        let reply_sock = Arc::clone(&sock);
        let upstreams = Arc::clone(&upstreams);
        if thread::Builder::new()
            .spawn(move || {
                let _guard = guard;
                if let Some(reply) = resolve_external_udp(&query, &upstreams) {
                    let _ = reply_sock.send_to(&reply, src);
                }
            })
            .is_err()
        {
            if let Some(err) = servfail(pkt) {
                let _ = sock.send_to(&err, src);
            }
        }
    }
}

fn forward_udp(query: &[u8], upstreams: &[SocketAddr]) -> Option<Vec<u8>> {
    let mut buf = [0u8; MAX_UDP_PACKET];
    for &upstream in upstreams {
        let bind = if upstream.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
        let Ok(sock) = UdpSocket::bind(bind) else { continue };
        let _ = sock.set_read_timeout(Some(UPSTREAM_TIMEOUT));
        let _ = sock.set_write_timeout(Some(UPSTREAM_TIMEOUT));
        if sock.connect(upstream).is_err() {
            continue;
        }
        if sock.send(query).is_err() {
            continue;
        }
        if let Ok(n) = sock.recv(&mut buf) {
            if n >= 12 && query.len() >= 2 && buf[0..2] == query[0..2] {
                return Some(buf[..n].to_vec());
            }
        }
    }
    None
}

fn serve_tcp(listener: TcpListener, upstreams: Arc<Vec<SocketAddr>>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let Some(guard) = InflightGuard::try_acquire() else {
            drop(stream);
            continue;
        };
        let upstreams = Arc::clone(&upstreams);
        let _ = thread::Builder::new().spawn(move || {
            let _guard = guard;
            handle_tcp(stream, &upstreams);
        });
    }
}

fn handle_tcp(mut client: TcpStream, upstreams: &[SocketAddr]) {
    let _ = client.set_read_timeout(Some(TCP_IO_TIMEOUT));
    let _ = client.set_write_timeout(Some(TCP_IO_TIMEOUT));
    loop {
        let Some(query) = read_tcp_msg(&mut client) else { return };
        let reply = if let Some(local) = answer_local(&query) {
            if let Some((name, _)) = parse_question(&query) {
                record_dns_xray(&name, "local container table", true, 0);
            }
            Some(local)
        } else {
            resolve_external_tcp(&query, upstreams)
        };
        let Some(reply) = reply else { return };
        if write_tcp_msg(&mut client, &reply).is_err() {
            return;
        }
    }
}

fn forward_tcp(query: &[u8], upstreams: &[SocketAddr]) -> Option<Vec<u8>> {
    for &upstream in upstreams {
        let Ok(mut conn) = TcpStream::connect_timeout(&upstream, UPSTREAM_TIMEOUT) else { continue };
        let _ = conn.set_read_timeout(Some(TCP_IO_TIMEOUT));
        let _ = conn.set_write_timeout(Some(TCP_IO_TIMEOUT));
        if write_tcp_msg(&mut conn, query).is_err() {
            continue;
        }
        if let Some(reply) = read_tcp_msg(&mut conn) {
            if reply.len() >= 12 && query.len() >= 2 && reply[0..2] == query[0..2] {
                return Some(reply);
            }
        }
    }
    None
}

fn read_tcp_msg(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf).ok()?;
    let len = u16::from_be_bytes(len_buf) as usize;
    if len == 0 {
        return None;
    }
    let mut msg = vec![0u8; len];
    stream.read_exact(&mut msg).ok()?;
    Some(msg)
}

fn write_tcp_msg(stream: &mut TcpStream, msg: &[u8]) -> io::Result<()> {
    let len = u16::try_from(msg.len()).map_err(|_| io::Error::other("DNS reply too large"))?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(msg)
}
