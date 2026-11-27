//! Guest-side egress proxy (`127.0.0.1:18080`, HTTP + CONNECT + SOCKS5) and
//! host DNS bridge over `vsock`, active when the VM runs in hardware-isolated
//! (`NetworkMode::None`) `Offline` or `Allowlist` mode.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
use std::thread;
use std::time::Duration;

use graviton_proto::{EgressBridgeReply, EgressBridgeRequest};

pub const PROXY_PORT: u16 = 18080;

type PendingStreamMap = Mutex<HashMap<u64, mpsc::Sender<(File, Vec<u8>)>>>;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static BRIDGE_WRITER: Mutex<Option<File>> = Mutex::new(None);
static PENDING_BRIDGE: OnceLock<Mutex<HashMap<u64, mpsc::Sender<EgressBridgeReply>>>> = OnceLock::new();
static PENDING_STREAMS: OnceLock<PendingStreamMap> = OnceLock::new();
static LISTENER_STARTED: OnceLock<io::Result<()>> = OnceLock::new();

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

fn pending_bridge() -> &'static Mutex<HashMap<u64, mpsc::Sender<EgressBridgeReply>>> {
    PENDING_BRIDGE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn pending_streams() -> &'static PendingStreamMap {
    PENDING_STREAMS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn ensure_proxy_listener() -> io::Result<()> {
    match LISTENER_STARTED.get_or_init(|| {
        let listener = TcpListener::bind(("127.0.0.1", PROXY_PORT))?;
        thread::Builder::new().name("egress-proxy".into()).spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = thread::Builder::new().spawn(move || {
                    let _ = handle_client(stream);
                });
            }
        })?;
        Ok(())
    }) {
        Ok(()) => Ok(()),
        Err(e) => Err(io::Error::other(e.to_string())),
    }
}

/// Take ownership of the `AgentRequest::EgressBridge` vsock stream from host `EgressGateway`.
pub fn serve_bridge(stream: File) -> io::Result<()> {
    ensure_proxy_listener()?;
    let read_side = stream.try_clone()?;
    *BRIDGE_WRITER.lock().unwrap() = Some(stream);
    ACTIVE.store(true, Ordering::SeqCst);

    let reader = BufReader::new(read_side);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Ok(reply) = serde_json::from_str::<EgressBridgeReply>(&line) else { continue };
        let id = match &reply {
            EgressBridgeReply::DnsResult { id, .. } | EgressBridgeReply::DialResult { id, .. } => *id,
        };
        if let Some(tx) = pending_bridge().lock().unwrap().remove(&id) {
            let _ = tx.send(reply);
        }
    }

    ACTIVE.store(false, Ordering::SeqCst);
    *BRIDGE_WRITER.lock().unwrap() = None;
    Ok(())
}

/// Pair an incoming `AgentRequest::EgressStream { id }` vsock stream with a waiting proxy connection.
pub fn attach_stream(id: u64, stream: File, pending: Vec<u8>) {
    if let Some(tx) = pending_streams().lock().unwrap().remove(&id) {
        let _ = tx.send((stream, pending));
    }
}

/// Resolve `domain` (`qtype`) on the macOS host via `EgressBridge`.
/// Returns `Ok(None)` when denied by host `EgressPolicy`.
pub fn dns_lookup(name: &str, qtype: u16) -> io::Result<Option<Vec<IpAddr>>> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    match bridge_rpc(id, &EgressBridgeRequest::Dns { id, name: name.to_string(), qtype })? {
        EgressBridgeReply::DnsResult { allowed: false, .. } => Ok(None),
        EgressBridgeReply::DnsResult { allowed: true, ips, .. } => {
            Ok(Some(ips.into_iter().filter_map(|s| s.parse().ok()).collect()))
        }
        _ => Err(io::Error::other("unexpected bridge reply")),
    }
}

fn bridge_rpc(id: u64, req: &EgressBridgeRequest) -> io::Result<EgressBridgeReply> {
    let (tx, rx) = mpsc::channel();
    pending_bridge().lock().unwrap().insert(id, tx);

    let mut line = serde_json::to_vec(req).map_err(io::Error::other)?;
    line.push(b'\n');

    let write_res = {
        let mut guard = BRIDGE_WRITER.lock().unwrap();
        match guard.as_mut() {
            Some(writer) => writer.write_all(&line).and_then(|()| writer.flush()),
            None => Err(io::Error::other("egress bridge disconnected")),
        }
    };
    if let Err(err) = write_res {
        pending_bridge().lock().unwrap().remove(&id);
        return Err(err);
    }

    rx.recv_timeout(Duration::from_secs(15)).map_err(|_| {
        pending_bridge().lock().unwrap().remove(&id);
        io::Error::other("egress bridge timed out")
    })
}

fn dial_via_host(host: &str, port: u16) -> Result<(File, Vec<u8>), (bool, String)> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (stream_tx, stream_rx) = mpsc::channel::<(File, Vec<u8>)>();
    pending_streams().lock().unwrap().insert(id, stream_tx);

    match bridge_rpc(id, &EgressBridgeRequest::Dial { id, host: host.to_string(), port }) {
        Ok(EgressBridgeReply::DialResult { allowed: true, error: None, .. }) => {
            stream_rx.recv_timeout(Duration::from_secs(10)).map_err(|_| {
                pending_streams().lock().unwrap().remove(&id);
                (true, "host egress stream did not arrive".to_string())
            })
        }
        Ok(EgressBridgeReply::DialResult { allowed, error, .. }) => {
            pending_streams().lock().unwrap().remove(&id);
            Err((allowed, error.unwrap_or_else(|| "blocked by Graviton egress policy".into())))
        }
        Ok(_) => {
            pending_streams().lock().unwrap().remove(&id);
            Err((true, "unexpected egress bridge reply".into()))
        }
        Err(err) => {
            pending_streams().lock().unwrap().remove(&id);
            Err((true, err.to_string()))
        }
    }
}

fn handle_client(client: TcpStream) -> io::Result<()> {
    let _ = client.set_nodelay(true);
    let mut peek = [0u8; 1];
    if client.peek(&mut peek)? == 0 {
        return Ok(());
    }
    if peek[0] == 0x05 {
        return handle_socks5(client);
    }
    handle_http(client)
}

fn handle_http(mut client: TcpStream) -> io::Result<()> {
    let mut raw = Vec::with_capacity(2048);
    let mut buf = [0u8; 2048];
    let header_end = loop {
        let n = client.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if raw.len() > 64 * 1024 {
            let _ = client.write_all(b"HTTP/1.1 431 Request Header Fields Too Large\r\nConnection: close\r\n\r\n");
            return Ok(());
        }
    };

    let head = String::from_utf8_lossy(&raw[..header_end]);
    let mut lines = head.split("\r\n");
    let req_line = lines.next().unwrap_or("");
    let mut parts = req_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("HTTP/1.1");
    let extra = &raw[header_end..];

    if method.eq_ignore_ascii_case("CONNECT") {
        let Some((host, port)) = parse_host_port(target, 443) else {
            let _ = client.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n");
            return Ok(());
        };
        match dial_via_host(&host, port) {
            Ok((mut upstream, pending_down)) => {
                client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
                if !pending_down.is_empty() {
                    client.write_all(&pending_down)?;
                }
                client.flush()?;
                if !extra.is_empty() {
                    upstream.write_all(extra)?;
                }
                splice(client, upstream)
            }
            Err((allowed, msg)) => send_http_error(&mut client, allowed, &msg),
        }
    } else {
        let (host, port, path) = parse_absolute_http_target(target, &head);
        let Some((host, port)) = host.map(|h| (h, port)) else {
            let _ = client.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n");
            return Ok(());
        };
        match dial_via_host(&host, port) {
            Ok((mut upstream, pending_down)) => {
                let mut rewritten = format!("{method} {path} {version}\r\n");
                for hline in head.split("\r\n").skip(1) {
                    if hline.is_empty() || hline.to_ascii_lowercase().starts_with("proxy-connection:") {
                        continue;
                    }
                    rewritten.push_str(hline);
                    rewritten.push_str("\r\n");
                }
                rewritten.push_str("\r\n");
                upstream.write_all(rewritten.as_bytes())?;
                if !extra.is_empty() {
                    upstream.write_all(extra)?;
                }
                if !pending_down.is_empty() {
                    client.write_all(&pending_down)?;
                }
                splice(client, upstream)
            }
            Err((allowed, msg)) => send_http_error(&mut client, allowed, &msg),
        }
    }
}

fn handle_socks5(mut client: TcpStream) -> io::Result<()> {
    let mut hdr = [0u8; 2];
    client.read_exact(&mut hdr)?;
    let nmethods = hdr[1] as usize;
    let mut methods = vec![0u8; nmethods];
    client.read_exact(&mut methods)?;
    client.write_all(&[0x05, 0x00])?; // no authentication

    let mut req = [0u8; 4];
    client.read_exact(&mut req)?;
    if req[0] != 0x05 || req[1] != 0x01 {
        let _ = client.write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
        return Ok(());
    }
    let host = match req[3] {
        0x01 => {
            let mut ip = [0u8; 4];
            client.read_exact(&mut ip)?;
            std::net::Ipv4Addr::from(ip).to_string()
        }
        0x03 => {
            let mut len = [0u8; 1];
            client.read_exact(&mut len)?;
            let mut name = vec![0u8; len[0] as usize];
            client.read_exact(&mut name)?;
            String::from_utf8_lossy(&name).into_owned()
        }
        0x04 => {
            let mut ip = [0u8; 16];
            client.read_exact(&mut ip)?;
            std::net::Ipv6Addr::from(ip).to_string()
        }
        _ => {
            let _ = client.write_all(&[0x05, 0x08, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
            return Ok(());
        }
    };
    let mut port_buf = [0u8; 2];
    client.read_exact(&mut port_buf)?;
    let port = u16::from_be_bytes(port_buf);

    match dial_via_host(&host, port) {
        Ok((upstream, pending_down)) => {
            client.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])?;
            if !pending_down.is_empty() {
                client.write_all(&pending_down)?;
            }
            splice(client, upstream)
        }
        Err((allowed, _)) => {
            let code = if allowed { 0x04 } else { 0x02 };
            let _ = client.write_all(&[0x05, code, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
            Ok(())
        }
    }
}

fn send_http_error(client: &mut TcpStream, allowed: bool, msg: &str) -> io::Result<()> {
    let status = if allowed { "502 Bad Gateway" } else { "403 Forbidden" };
    let body = format!("{msg}\n");
    let resp = format!("HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    client.write_all(resp.as_bytes())
}

fn splice(mut tcp: TcpStream, mut vsock: File) -> io::Result<()> {
    let mut tcp_w = tcp.try_clone()?;
    let mut vsock_r = vsock.try_clone()?;
    let t = thread::spawn(move || {
        let _ = io::copy(&mut vsock_r, &mut tcp_w);
        let _ = tcp_w.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut tcp, &mut vsock);
    let _ = vsock.flush();
    use std::os::fd::AsRawFd;
    // SAFETY: shutdown write half on valid vsock fd.
    unsafe {
        libc::shutdown(vsock.as_raw_fd(), libc::SHUT_WR);
    }
    let _ = t.join();
    Ok(())
}

fn parse_host_port(authority: &str, default_port: u16) -> Option<(String, u16)> {
    let a = authority.trim();
    if a.is_empty() {
        return None;
    }
    if let Some(rest) = a.strip_prefix('[') {
        let (ip, tail) = rest.split_once(']')?;
        let port = tail.strip_prefix(':').and_then(|p| p.parse().ok()).unwrap_or(default_port);
        return Some((ip.to_string(), port));
    }
    match a.rsplit_once(':') {
        Some((h, p)) => p.parse().ok().map(|port| (h.to_string(), port)),
        None => Some((a.to_string(), default_port)),
    }
}

fn parse_absolute_http_target(target: &str, head: &str) -> (Option<String>, u16, String) {
    if let Some(rest) = target.strip_prefix("http://").or_else(|| target.strip_prefix("HTTP://")) {
        let (auth, path) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, "/"),
        };
        if let Some((h, p)) = parse_host_port(auth, 80) {
            return (Some(h), p, path.to_string());
        }
    }
    let host_hdr = head.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case("host").then(|| v.trim().to_string())
    });
    let parsed = host_hdr.and_then(|h| parse_host_port(&h, 80));
    match parsed {
        Some((h, p)) => (Some(h), p, target.to_string()),
        None => (None, 80, target.to_string()),
    }
}
