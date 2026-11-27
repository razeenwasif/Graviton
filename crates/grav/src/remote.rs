//! Other Macs running `gravitond --remote`.
//!
//! `grav hosts add` pairs with one using a code from `grav pair` on that Mac
//! and keeps it in `hosts.json` (only you can read it): its address, its
//! certificate's SHA-256 (pinned: any other certificate is refused) and the
//! token it issued. Commands with `--host NAME` then connect over TLS,
//! authenticate, and hand the rest of `grav` an ordinary local socket: a relay
//! thread carries bytes between it and the TLS connection, so requests,
//! streamed builds and terminals all work unchanged.

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use graviton_core::paths;
use graviton_proto::{REMOTE_PORT, Request, Response, decode_line, encode_line};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    /// What you call it in `--host`.
    pub name: String,
    pub address: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_name: Option<String>,
    /// SHA-256 of its certificate, lowercase hex.
    pub fingerprint: String,
    pub token: String,
    pub device_id: String,
    /// The name the Mac gave itself.
    pub host_name: String,
}

impl Host {
    /// Ordered, deduplicated addresses to try for this host (primary `address` first).
    pub fn candidate_addresses(&self) -> Vec<String> {
        let mut out = Vec::new();
        for addr in std::iter::once(&self.address).chain(self.addresses.iter()) {
            let trimmed = addr.trim();
            if !trimmed.is_empty() && !out.iter().any(|existing: &String| existing == trimmed) {
                out.push(trimmed.to_string());
            }
        }
        out
    }

    /// Replace the ordered address list while keeping `address` synced to the primary entry.
    pub fn set_addresses(&mut self, addrs: Vec<String>) {
        let mut clean = Vec::new();
        for addr in addrs {
            let trimmed = addr.trim();
            if !trimmed.is_empty() && !clean.iter().any(|existing: &String| existing == trimmed) {
                clean.push(trimmed.to_string());
            }
        }
        self.address = clean.first().cloned().unwrap_or_default();
        self.addresses = if clean.len() > 1 { clean } else { Vec::new() };
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Hosts {
    /// Used when no `--host` is given; `None` is this Mac.
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub hosts: Vec<Host>,
}

impl Hosts {
    fn path() -> PathBuf {
        paths::support_dir().join("hosts.json")
    }

    pub fn load() -> Result<Self> {
        match fs::read(Self::path()) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("reading {}", Self::path().display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = Self::path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(self)?)?;
        fs::rename(tmp, &path)?;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&Host> {
        self.hosts.iter().find(|h| h.name.eq_ignore_ascii_case(name))
    }
}

/// Where a command goes.
pub enum Target {
    Local(PathBuf),
    Remote(Host),
}

impl Target {
    /// `--host` (or `$GRAVITON_HOST`), else the default host, else this Mac.
    /// `local` always means this Mac.
    pub fn resolve(flag: Option<&str>, socket: PathBuf) -> Result<Self> {
        let hosts = Hosts::load()?;
        let name = flag.map(String::from).or(hosts.default.clone());
        match name.as_deref() {
            None | Some("local") | Some("") => Ok(Target::Local(socket)),
            Some(n) => hosts
                .get(n)
                .cloned()
                .map(Target::Remote)
                .ok_or_else(|| anyhow!("no host called {n}: see `grav hosts`, or pair with `grav hosts add`")),
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Target::Remote(_))
    }

    /// Resolve a migration destination (`unix:<path>`, `local`, or paired host name).
    pub fn resolve_migration_dest(dest: &str, default_socket: PathBuf) -> Result<Self> {
        if let Some(sock) = dest.strip_prefix("unix:") {
            return Ok(Target::Local(PathBuf::from(sock)));
        }
        Self::resolve(Some(dest), default_socket)
    }

    /// Primary reachable address for TCP port tunneling to this target (`None` for local).
    pub fn tunnel_host(&self) -> Option<String> {
        match self {
            Target::Local(_) => None,
            Target::Remote(h) => h
                .candidate_addresses()
                .into_iter()
                .next()
                .or_else(|| h.service_name.as_ref().map(|s| format!("{s}.local"))),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Target::Local(_) => "this Mac".into(),
            Target::Remote(h) => {
                let addrs = h.candidate_addresses();
                let extra = addrs.len().saturating_sub(1);
                if addrs.is_empty() {
                    if let Some(svc) = &h.service_name {
                        return format!("{} ({svc}._graviton._tcp:{})", h.name, h.port);
                    }
                    return format!("{} (bonjour:{})", h.name, h.port);
                }
                let primary = &addrs[0];
                if extra > 0 {
                    format!("{} ({primary}:{} +{extra})", h.name, h.port)
                } else {
                    format!("{} ({primary}:{})", h.name, h.port)
                }
            }
        }
    }

    /// A connected stream to its gravitond, ready for requests.
    pub fn connect(&self) -> Result<UnixStream> {
        match self {
            Target::Local(path) => UnixStream::connect(path)
                .with_context(|| format!("cannot reach gravitond at {} (is it running? try `grav service install`)", path.display())),
            Target::Remote(host) => {
                let candidates = host.candidate_addresses();
                let timeout = Duration::from_secs(3);
                let mut stream = match connect_host_fastest(&candidates, host.port, &host.fingerprint, timeout) {
                    Ok((stream, _)) => stream,
                    Err(primary_err) => {
                        if let Some((stream, _)) = connect_via_bonjour(host, timeout) {
                            stream
                        } else {
                            let tried = if candidates.is_empty() {
                                "(no addresses)".to_string()
                            } else {
                                candidates.iter().map(|a| format!("{a}:{}", host.port)).collect::<Vec<_>>().join(", ")
                            };
                            return Err(primary_err).with_context(|| {
                                format!(
                                    "cannot reach {} via [{tried}] or Bonjour _graviton._tcp (update with `grav hosts update {} <address>`)",
                                    host.name, host.name
                                )
                            });
                        }
                    }
                };
                match exchange(&mut stream, &Request::Auth { token: host.token.clone() })? {
                    Response::Authenticated { .. } => Ok(stream),
                    Response::Error { message } => bail!("{}: {message} (pair again with `grav hosts add`)", host.name),
                    other => bail!("{}: unexpected answer {other:?}", host.name),
                }
            }
        }
    }
}

/// Send one request and read one answer line, without reading past it.
fn exchange(stream: &mut UnixStream, request: &Request) -> Result<Response> {
    stream.write_all(&encode_line(request))?;
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if stream.read(&mut byte)? == 0 {
            bail!("the Mac closed the connection");
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    Ok(decode_line(&line)?)
}

/// Pair with a Mac: trade a `grav pair` code for a token and remember the host.
pub fn pair(address: &str, extra_addresses: &[String], service_hint: Option<&str>, port: u16, code: &str, pin: Option<&str>, name: Option<&str>) -> Result<Host> {
    let code: String = code.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_uppercase();
    if code.len() != 8 {
        bail!("the code from `grav pair` has 8 letters and digits, like ABCD-EFGH");
    }
    let mut ordered = Vec::new();
    for addr in std::iter::once(address).chain(extra_addresses.iter().map(String::as_str)) {
        let trimmed = addr.trim();
        if !trimmed.is_empty() && !ordered.iter().any(|existing: &String| existing == trimmed) {
            ordered.push(trimmed.to_string());
        }
    }
    let (mut stream, fingerprint) = if let Some(expected_pin) = pin.filter(|p| !p.is_empty()) {
        let (stream, _) = connect_host_fastest(&ordered, port, expected_pin, Duration::from_secs(4))?;
        (stream, expected_pin.to_ascii_lowercase())
    } else {
        open_tls(address, port, None)?
    };
    let device = format!("{} (grav)", local_name());
    let (token, device_id, host_name) = match exchange(&mut stream, &Request::Pair { code, device })? {
        Response::Paired { token, device_id, host } => (token, device_id, host),
        Response::Error { message } => bail!(message),
        other => bail!("unexpected answer {other:?}"),
    };
    let name = name.map(String::from).unwrap_or_else(|| short_name(&host_name));
    let mut host = Host {
        name,
        address: String::new(),
        addresses: Vec::new(),
        port,
        service_name: service_hint.map(String::from).or_else(|| Some(host_name.clone())),
        fingerprint,
        token,
        device_id,
        host_name,
    };
    host.set_addresses(ordered);
    Ok(host)
}

/// Verify an updated host configuration reaches `gravitond` and authenticates over TLS.
pub fn verify_host(host: &Host) -> Result<()> {
    let mut stream = Target::Remote(host.clone()).connect()?;
    match exchange(&mut stream, &Request::Ping)? {
        Response::Pong { .. } => Ok(()),
        other => bail!("unexpected ping reply: {other:?}"),
    }
}

/// `grav hosts add` accepts `ADDRESS[:PORT]` or the `graviton://pair?...` link.
pub struct PairTarget {
    pub address: String,
    pub addresses: Vec<String>,
    pub service_name: Option<String>,
    pub port: u16,
    pub code: Option<String>,
    pub fingerprint: Option<String>,
    pub name: Option<String>,
}

impl PairTarget {
    pub fn parse(s: &str) -> Result<Self> {
        if let Some(query) = s.strip_prefix("graviton://pair?") {
            let mut t = PairTarget {
                address: String::new(),
                addresses: Vec::new(),
                service_name: None,
                port: REMOTE_PORT,
                code: None,
                fingerprint: None,
                name: None,
            };
            for pair in query.split('&') {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                let v = v.replace("%20", " ");
                match k {
                    "host" => t.address = v,
                    "addrs" => {
                        for item in v.split(',') {
                            let item = item.trim();
                            if !item.is_empty() && !t.addresses.iter().any(|existing| existing == item) {
                                t.addresses.push(item.to_string());
                            }
                        }
                    }
                    "service" => {
                        let s = v.trim();
                        if !s.is_empty() {
                            t.service_name = Some(s.to_string());
                        }
                    }
                    "port" => t.port = v.parse().context("bad port in the link")?,
                    "code" => t.code = Some(v),
                    "fp" => t.fingerprint = Some(v.to_lowercase()),
                    "name" => t.name = Some(v),
                    _ => {}
                }
            }
            if t.address.is_empty() {
                if let Some(first) = t.addresses.first() {
                    t.address = first.clone();
                } else {
                    bail!("the link has no host address");
                }
            } else if !t.addresses.iter().any(|a| a == &t.address) {
                t.addresses.insert(0, t.address.clone());
            }
            return Ok(t);
        }
        let (address, port) = parse_host_port(s, REMOTE_PORT)?;
        Ok(PairTarget {
            addresses: vec![address.clone()],
            address,
            service_name: None,
            port,
            code: None,
            fingerprint: None,
            name: None,
        })
    }
}

/// Split `HOST[:PORT]` (or `[IPv6]:PORT`) with fallback `default_port`.
pub fn parse_host_port(s: &str, default_port: u16) -> Result<(String, u16)> {
    parse_host_port_inner(s).map(|(host, opt)| (host, opt.unwrap_or(default_port)))
}

pub fn parse_host_port_opt(s: &str) -> Result<(String, Option<u16>)> {
    parse_host_port_inner(s)
}

fn parse_host_port_inner(s: &str) -> Result<(String, Option<u16>)> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        bail!("host address cannot be empty");
    }
    if let Some(rest) = trimmed.strip_prefix('[') {
        let Some((host, tail)) = rest.split_once(']') else {
            bail!("unclosed `[` in IPv6 address `{trimmed}`");
        };
        if let Some(port_str) = tail.strip_prefix(':') {
            return Ok((host.to_string(), Some(port_str.parse().context("bad port")?)));
        }
        if !tail.is_empty() {
            bail!("unexpected suffix `{tail}` after `]` in `{trimmed}`");
        }
        return Ok((host.to_string(), None));
    }
    match trimmed.rsplit_once(':') {
        Some((a, p)) if !a.contains(':') => Ok((a.to_string(), Some(p.parse().context("bad port")?))),
        _ => Ok((trimmed.to_string(), None)),
    }
}

fn local_name() -> String {
    std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Mac".into())
}

/// "My Mac mini" → "my-mac-mini"
fn short_name(host: &str) -> String {
    let s: String = host.to_lowercase().chars().filter_map(|c| if c.is_ascii_alphanumeric() { Some(c) } else if c == ' ' || c == '-' { Some('-') } else { None }).collect();
    let s = s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    if s.is_empty() { "mac".into() } else { s }
}

pub fn fingerprint_short(fp: &str) -> String {
    fp.as_bytes().chunks(4).take(4).map(|c| String::from_utf8_lossy(c).to_uppercase()).collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// TLS
// ---------------------------------------------------------------------------

/// Accepts exactly one certificate (by SHA-256), or with no pin, whichever it
/// sees first (pairing by code); records what it saw either way.
#[derive(Debug)]
struct Pinned {
    pin: Option<String>,
    seen: Mutex<Option<String>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        let fp: String = Sha256::digest(end_entity.as_ref()).iter().map(|b| format!("{b:02x}")).collect();
        *self.seen.lock().unwrap() = Some(fp.clone());
        match &self.pin {
            Some(pin) if *pin != fp => Err(rustls::Error::General(format!("certificate {} isn't the one paired with ({})", fingerprint_short(&fp), fingerprint_short(pin)))),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }
    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Connect, finish the TLS handshake and start the relay. Returns the local
/// end and the certificate's fingerprint.
fn open_tls(address: &str, port: u16, pin: Option<&str>) -> Result<(UnixStream, String)> {
    open_tls_timeout(address, port, pin, Duration::from_secs(5))
}

fn connect_tcp_resolved(address: &str, port: u16, timeout: Duration) -> Result<TcpStream> {
    let mut addrs: Vec<SocketAddr> = (address, port).to_socket_addrs()?.collect();
    if addrs.is_empty() {
        bail!("can't resolve {address}");
    }
    // gravitond listens on 0.0.0.0:port (IPv4), so dial IPv4 before IPv6 link-local addresses.
    addrs.sort_by_key(|a| !a.is_ipv4());
    let mut last_err: Option<io::Error> = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(tcp) => return Ok(tcp),
            Err(err) => last_err = Some(err),
        }
    }
    Err(anyhow!(last_err.map_or_else(|| format!("can't connect to {address}:{port}"), |e| e.to_string())))
}

fn open_tls_timeout(address: &str, port: u16, pin: Option<&str>, timeout: Duration) -> Result<(UnixStream, String)> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(Pinned { pin: pin.map(str::to_lowercase), seen: Mutex::new(None), provider: Arc::clone(&provider) });
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth();
    let mut tcp = connect_tcp_resolved(address, port, timeout)?;
    tcp.set_nodelay(true)?;
    let mut conn = ClientConnection::new(Arc::new(config), ServerName::try_from("graviton.local")?.to_owned())?;
    // The relay reads from grav only once earlier bytes have gone out, so this can't grow without bound.
    conn.set_buffer_limit(None);
    tcp.set_read_timeout(Some(timeout.max(Duration::from_secs(5))))?;
    while conn.is_handshaking() {
        conn.complete_io(&mut tcp)?;
    }
    tcp.set_read_timeout(None)?;
    let fingerprint = verifier.seen.lock().unwrap().clone().ok_or_else(|| anyhow!("the Mac sent no certificate"))?;
    let (ours, theirs) = UnixStream::pair()?;
    std::thread::Builder::new().name("grav-tls".into()).spawn(move || {
        let _ = relay(conn, tcp, theirs);
    })?;
    Ok((ours, fingerprint))
}

/// Race all candidate addresses concurrently and return whichever completes TCP + pinned TLS first.
fn connect_host_fastest(candidates: &[String], port: u16, pin: &str, timeout: Duration) -> Result<(UnixStream, String)> {
    if candidates.is_empty() {
        bail!("no address configured");
    }
    if candidates.len() == 1 {
        return open_tls_timeout(&candidates[0], port, Some(pin), timeout);
    }
    let (tx, rx) = mpsc::channel();
    for addr in candidates {
        let addr = addr.clone();
        let pin = pin.to_string();
        let tx = tx.clone();
        let _ = std::thread::Builder::new().name("grav-dial".into()).spawn(move || {
            let res = open_tls_timeout(&addr, port, Some(&pin), timeout);
            let _ = tx.send(res);
        });
    }
    drop(tx);

    let mut last_err: Option<anyhow::Error> = None;
    for res in rx {
        match res {
            Ok(ok) => return Ok(ok),
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("all candidate addresses failed")))
}

/// Extract `fp=<sha256>` from an RFC 6763 DNS-SD TXT record.
fn parse_txt_fp(txt: &[u8]) -> Option<String> {
    let mut i = 0;
    while i < txt.len() {
        let len = txt[i] as usize;
        i += 1;
        if i + len > txt.len() {
            break;
        }
        if let Ok(entry) = std::str::from_utf8(&txt[i..i + len]) {
            if let Some(val) = entry.strip_prefix("fp=") {
                let fp = val.trim().to_ascii_lowercase();
                if fp.len() == 64 && fp.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Some(fp);
                }
            }
        }
        i += len;
    }
    None
}

#[cfg(target_os = "macos")]
fn connect_via_bonjour(host: &Host, timeout: Duration) -> Option<(UnixStream, String)> {
    let discovered = discover_bonjour_hosts(Duration::from_millis(450));
    if discovered.is_empty() {
        return None;
    }

    let mut matches: Vec<&BonjourEndpoint> = discovered
        .iter()
        .filter(|ep| ep.fp.as_deref().is_some_and(|fp| fp.eq_ignore_ascii_case(&host.fingerprint)))
        .collect();

    if matches.is_empty() {
        matches = discovered
            .iter()
            .filter(|ep| {
                host.service_name.as_deref().is_some_and(|s| ep.name.eq_ignore_ascii_case(s))
                    || ep.name.eq_ignore_ascii_case(&host.host_name)
                    || ep.name.eq_ignore_ascii_case(&host.name)
            })
            .collect();
    }

    for ep in matches {
        if let Ok(ok) = open_tls_timeout(&ep.target, ep.port, Some(&host.fingerprint), timeout) {
            return Some(ok);
        }
    }
    None
}

#[cfg(not(target_os = "macos"))]
fn connect_via_bonjour(_host: &Host, _timeout: Duration) -> Option<(UnixStream, String)> {
    None
}

#[cfg(target_os = "macos")]
struct BonjourEndpoint {
    name: String,
    target: String,
    port: u16,
    fp: Option<String>,
}

#[cfg(target_os = "macos")]
fn discover_bonjour_hosts(timeout: Duration) -> Vec<BonjourEndpoint> {
    use std::ffi::{CStr, CString, c_char, c_void};

    const K_MORE_COMING: u32 = 0x1;
    const K_ADD: u32 = 0x2;

    #[link(name = "System")]
    unsafe extern "C" {
        fn DNSServiceBrowse(
            sd_ref: *mut *mut c_void,
            flags: u32,
            interface_index: u32,
            regtype: *const c_char,
            domain: *const c_char,
            callback: unsafe extern "C" fn(*mut c_void, u32, u32, i32, *const c_char, *const c_char, *const c_char, *mut c_void),
            context: *mut c_void,
        ) -> i32;
        fn DNSServiceResolve(
            sd_ref: *mut *mut c_void,
            flags: u32,
            interface_index: u32,
            name: *const c_char,
            regtype: *const c_char,
            domain: *const c_char,
            callback: unsafe extern "C" fn(*mut c_void, u32, u32, i32, *const c_char, *const c_char, u16, u16, *const u8, *mut c_void),
            context: *mut c_void,
        ) -> i32;
        fn DNSServiceRefSockFD(sd_ref: *mut c_void) -> i32;
        fn DNSServiceProcessResult(sd_ref: *mut c_void) -> i32;
        fn DNSServiceRefDeallocate(sd_ref: *mut c_void);
    }

    struct SdRef(*mut c_void);
    impl Drop for SdRef {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: allocated by DNSServiceBrowse/Resolve and deallocated once outside callbacks.
                unsafe { DNSServiceRefDeallocate(self.0) };
            }
        }
    }

    #[derive(Default)]
    struct BrowseState {
        items: Vec<(String, String, String)>,
        more_coming: bool,
    }

    unsafe extern "C" fn on_browse(
        _sd_ref: *mut c_void,
        flags: u32,
        _iface: u32,
        err: i32,
        name: *const c_char,
        regtype: *const c_char,
        domain: *const c_char,
        ctx: *mut c_void,
    ) {
        if err != 0 || (flags & K_ADD) == 0 || name.is_null() || regtype.is_null() || domain.is_null() || ctx.is_null() {
            return;
        }
        // SAFETY: C pointers from mDNSResponder valid during callback; ctx points to live BrowseState.
        unsafe {
            let state = &mut *(ctx as *mut BrowseState);
            let tuple = (
                CStr::from_ptr(name).to_string_lossy().into_owned(),
                CStr::from_ptr(regtype).to_string_lossy().into_owned(),
                CStr::from_ptr(domain).to_string_lossy().into_owned(),
            );
            if !state.items.iter().any(|existing| existing == &tuple) {
                state.items.push(tuple);
            }
            state.more_coming = (flags & K_MORE_COMING) != 0;
        }
    }

    #[derive(Default)]
    struct ResolveState {
        found: Option<(String, u16, Option<String>)>,
    }

    unsafe extern "C" fn on_resolve(
        _sd_ref: *mut c_void,
        _flags: u32,
        _iface: u32,
        err: i32,
        _fullname: *const c_char,
        hosttarget: *const c_char,
        port_be: u16,
        txt_len: u16,
        txt_ptr: *const u8,
        ctx: *mut c_void,
    ) {
        if err != 0 || hosttarget.is_null() || ctx.is_null() {
            return;
        }
        // SAFETY: hosttarget and txt_ptr..txt_len valid during callback; ctx points to live ResolveState.
        unsafe {
            let state = &mut *(ctx as *mut ResolveState);
            let target = CStr::from_ptr(hosttarget).to_string_lossy().trim_end_matches('.').to_string();
            let txt = if !txt_ptr.is_null() && txt_len > 0 {
                std::slice::from_raw_parts(txt_ptr, txt_len as usize)
            } else {
                &[]
            };
            let fp = parse_txt_fp(txt);
            state.found = Some((target, u16::from_be(port_be), fp));
        }
    }

    fn pump(sd: &SdRef, until: Instant, stop: &dyn Fn() -> bool) {
        // SAFETY: valid DNSServiceRef owned by caller.
        let fd = unsafe { DNSServiceRefSockFD(sd.0) };
        if fd < 0 {
            return;
        }
        while Instant::now() < until && !stop() {
            let remain_ms = until.saturating_duration_since(Instant::now()).as_millis().min(50) as i32;
            let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            // SAFETY: polling valid socket descriptor owned by mDNSResponder.
            let pr = unsafe { libc::poll(&mut pfd, 1, remain_ms.max(1)) };
            if pr <= 0 {
                continue;
            }
            if (pfd.revents & libc::POLLIN) != 0 {
                // SAFETY: processes one ready DNS-SD message synchronously.
                if unsafe { DNSServiceProcessResult(sd.0) } != 0 {
                    break;
                }
            }
        }
    }

    let Ok(regtype) = CString::new(graviton_proto::BONJOUR_SERVICE) else {
        return Vec::new();
    };
    let mut browse_state = BrowseState::default();
    let mut raw_browse = std::ptr::null_mut();
    // SAFETY: regtype is valid CString; browse_state outlives browse_ref via drop order.
    let rc = unsafe {
        DNSServiceBrowse(&mut raw_browse, 0, 0, regtype.as_ptr(), std::ptr::null(), on_browse, (&mut browse_state as *mut BrowseState).cast())
    };
    if rc != 0 || raw_browse.is_null() {
        return Vec::new();
    }
    let browse_ref = SdRef(raw_browse);
    let mut settled_at: Option<Instant> = None;
    let browse_deadline = Instant::now() + timeout;
    // Pump browse socket briefly; stop ~60ms after MoreComing clears so multiple interfaces flush.
    let browse_fd = unsafe { DNSServiceRefSockFD(browse_ref.0) };
    if browse_fd >= 0 {
        while Instant::now() < browse_deadline {
            let now = Instant::now();
            if let Some(done_at) = settled_at {
                if now >= done_at {
                    break;
                }
            }
            let remain_ms = browse_deadline.saturating_duration_since(now).as_millis().min(40) as i32;
            let mut pfd = libc::pollfd { fd: browse_fd, events: libc::POLLIN, revents: 0 };
            let pr = unsafe { libc::poll(&mut pfd, 1, remain_ms.max(1)) };
            if pr > 0 && (pfd.revents & libc::POLLIN) != 0 {
                if unsafe { DNSServiceProcessResult(browse_ref.0) } != 0 {
                    break;
                }
                if !browse_state.items.is_empty() && !browse_state.more_coming {
                    settled_at = Some(Instant::now() + Duration::from_millis(60));
                } else if browse_state.more_coming {
                    settled_at = None;
                }
            }
        }
    }
    drop(browse_ref);

    let mut out = Vec::new();
    for (name, rtype, domain) in browse_state.items {
        let (Ok(c_name), Ok(c_type), Ok(c_dom)) = (CString::new(name.as_str()), CString::new(rtype.as_str()), CString::new(domain.as_str())) else {
            continue;
        };
        let mut rstate = ResolveState::default();
        let mut raw_res = std::ptr::null_mut();
        let rc = unsafe {
            DNSServiceResolve(&mut raw_res, 0, 0, c_name.as_ptr(), c_type.as_ptr(), c_dom.as_ptr(), on_resolve, (&mut rstate as *mut ResolveState).cast())
        };
        if rc != 0 || raw_res.is_null() {
            continue;
        }
        let res_ref = SdRef(raw_res);
        pump(&res_ref, Instant::now() + Duration::from_millis(350), &|| rstate.found.is_some());
        drop(res_ref);
        if let Some((target, port, fp)) = rstate.found {
            out.push(BonjourEndpoint { name, target, port, fp });
        }
    }
    out
}

/// Carry bytes between the local socket and the TLS connection until either closes.
fn relay(mut conn: ClientConnection, mut tcp: TcpStream, mut local: UnixStream) -> io::Result<()> {
    tcp.set_nonblocking(true)?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // Send whatever is queued.
        while conn.wants_write() {
            match conn.write_tls(&mut tcp) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        let mut fds = [
            libc::pollfd { fd: tcp.as_raw_fd(), events: libc::POLLIN | if conn.wants_write() { libc::POLLOUT } else { 0 }, revents: 0 },
            // Take more from grav only when what it sent before is on the wire.
            libc::pollfd { fd: local.as_raw_fd(), events: if conn.wants_write() { 0 } else { libc::POLLIN }, revents: 0 },
        ];
        // SAFETY: poll(2) on two descriptors we own.
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match conn.read_tls(&mut tcp) {
                Ok(0) => break,
                Ok(_) => {
                    conn.process_new_packets().map_err(io::Error::other)?;
                    loop {
                        match conn.reader().read(&mut buf) {
                            Ok(0) => return local.shutdown(std::net::Shutdown::Both),
                            Ok(n) => local.write_all(&buf[..n])?,
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(e) => return Err(e),
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        if fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let n = local.read(&mut buf)?;
            if n == 0 {
                conn.send_close_notify();
                let _ = conn.write_tls(&mut tcp);
                break;
            }
            conn.writer().write_all(&buf[..n])?;
        }
    }
    let _ = local.shutdown(std::net::Shutdown::Both);
    Ok(())
}

/// Ask on the terminal.
pub fn confirm(question: &str) -> bool {
    eprint!("{question} [y/N] ");
    let _ = io::stderr().flush();
    let mut answer = String::new();
    BufReader::new(io::stdin()).read_line(&mut answer).is_ok() && matches!(answer.trim(), "y" | "Y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_targets() {
        let fp = "ab".repeat(32);
        let t = PairTarget::parse(&format!(
            "graviton://pair?host=192.168.1.20&addrs=192.168.1.20,100.64.0.10&service=BuildHost&port=7843&fp={fp}&code=Z5VRBPZ3&name=My%20Mac%20mini"
        ))
        .unwrap();
        assert_eq!((t.address.as_str(), t.port, t.code.as_deref(), t.name.as_deref()), ("192.168.1.20", 7843, Some("Z5VRBPZ3"), Some("My Mac mini")));
        assert_eq!(t.addresses, vec!["192.168.1.20", "100.64.0.10"]);
        assert_eq!(t.service_name.as_deref(), Some("BuildHost"));
        assert_eq!(t.fingerprint.as_deref(), Some(fp.as_str()));
        let t = PairTarget::parse("mini.local").unwrap();
        assert_eq!((t.address.as_str(), t.port), ("mini.local", REMOTE_PORT));
        assert_eq!(PairTarget::parse("10.0.0.5:9000").unwrap().port, 9000);
        assert_eq!(parse_host_port_opt("[fe80::1]:7843").unwrap(), ("fe80::1".into(), Some(7843)));
        assert_eq!(short_name("My Mac mini"), "my-mac-mini");
        assert_eq!(fingerprint_short(&"f00418f9".repeat(8)), "F004 18F9 F004 18F9");
    }

    #[test]
    fn decodes_legacy_and_multi_address_hosts_json() {
        let legacy = r#"{"default":"mini","hosts":[{"name":"mini","address":"100.64.0.10","port":7843,"fingerprint":"ab","token":"tok","device_id":"d1","host_name":"BuildHost"}]}"#;
        let parsed: Hosts = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.hosts[0].candidate_addresses(), vec!["100.64.0.10"]);
        assert_eq!(parsed.hosts[0].service_name, None);

        let mut host = parsed.hosts[0].clone();
        host.set_addresses(vec!["192.0.2.61".into(), "100.64.0.10".into(), "192.0.2.61".into()]);
        assert_eq!(host.address, "192.0.2.61");
        assert_eq!(host.candidate_addresses(), vec!["192.0.2.61", "100.64.0.10"]);
    }

    #[test]
    fn parses_txt_fp_record() {
        let fp = "ab".repeat(32);
        let mut raw = Vec::new();
        raw.push(7);
        raw.extend_from_slice(b"foo=bar");
        let kv = format!("fp={}", fp.to_uppercase());
        raw.push(kv.len() as u8);
        raw.extend_from_slice(kv.as_bytes());
        assert_eq!(parse_txt_fp(&raw), Some(fp));
        assert_eq!(parse_txt_fp(&[5, b'f', b'p', b'=', b'a', b'b']), None);
        assert_eq!(parse_txt_fp(&[10, b'f', b'p']), None);
    }

    #[test]
    fn resolved_tcp_prefers_live_port() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stream = connect_tcp_resolved("127.0.0.1", port, Duration::from_secs(1)).unwrap();
        drop(stream);
        drop(listener);
    }
}
