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
use std::net::{TcpStream, ToSocketAddrs};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    pub port: u16,
    /// SHA-256 of its certificate, lowercase hex.
    pub fingerprint: String,
    pub token: String,
    pub device_id: String,
    /// The name the Mac gave itself.
    pub host_name: String,
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

    pub fn describe(&self) -> String {
        match self {
            Target::Local(_) => "this Mac".into(),
            Target::Remote(h) => format!("{} ({}:{})", h.name, h.address, h.port),
        }
    }

    /// A connected stream to its gravitond, ready for requests.
    pub fn connect(&self) -> Result<UnixStream> {
        match self {
            Target::Local(path) => UnixStream::connect(path)
                .with_context(|| format!("cannot reach gravitond at {} (is it running? try `grav service install`)", path.display())),
            Target::Remote(host) => {
                let (stream, _) = open_tls(&host.address, host.port, Some(&host.fingerprint))
                    .with_context(|| format!("cannot reach {} at {}:{}", host.name, host.address, host.port))?;
                let mut stream = stream;
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
pub fn pair(address: &str, port: u16, code: &str, pin: Option<&str>, name: Option<&str>) -> Result<Host> {
    let code: String = code.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_uppercase();
    if code.len() != 8 {
        bail!("the code from `grav pair` has 8 letters and digits, like ABCD-EFGH");
    }
    let (mut stream, fingerprint) = open_tls(address, port, pin)?;
    let device = format!("{} (grav)", local_name());
    let (token, device_id, host_name) = match exchange(&mut stream, &Request::Pair { code, device })? {
        Response::Paired { token, device_id, host } => (token, device_id, host),
        Response::Error { message } => bail!(message),
        other => bail!("unexpected answer {other:?}"),
    };
    let name = name.map(String::from).unwrap_or_else(|| short_name(&host_name));
    Ok(Host { name, address: address.to_string(), port, fingerprint, token, device_id, host_name })
}

/// `grav hosts add` accepts `ADDRESS[:PORT]` or the `graviton://pair?...` link.
pub struct PairTarget {
    pub address: String,
    pub port: u16,
    pub code: Option<String>,
    pub fingerprint: Option<String>,
    pub name: Option<String>,
}

impl PairTarget {
    pub fn parse(s: &str) -> Result<Self> {
        if let Some(query) = s.strip_prefix("graviton://pair?") {
            let mut t = PairTarget { address: String::new(), port: REMOTE_PORT, code: None, fingerprint: None, name: None };
            for pair in query.split('&') {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                let v = v.replace("%20", " ");
                match k {
                    "host" => t.address = v,
                    "port" => t.port = v.parse().context("bad port in the link")?,
                    "code" => t.code = Some(v),
                    "fp" => t.fingerprint = Some(v.to_lowercase()),
                    "name" => t.name = Some(v),
                    _ => {}
                }
            }
            if t.address.is_empty() {
                bail!("the link has no host address");
            }
            return Ok(t);
        }
        let (address, port) = match s.rsplit_once(':') {
            Some((a, p)) if !a.contains(':') => (a.to_string(), p.parse().context("bad port")?),
            _ => (s.to_string(), REMOTE_PORT),
        };
        Ok(PairTarget { address, port, code: None, fingerprint: None, name: None })
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
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(Pinned { pin: pin.map(str::to_lowercase), seen: Mutex::new(None), provider: Arc::clone(&provider) });
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth();
    let addr = (address, port).to_socket_addrs()?.next().ok_or_else(|| anyhow!("can't resolve {address}"))?;
    let mut tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    tcp.set_nodelay(true)?;
    let mut conn = ClientConnection::new(Arc::new(config), ServerName::try_from("graviton.local")?.to_owned())?;
    // The relay reads from grav only once earlier bytes have gone out, so this can't grow without bound.
    conn.set_buffer_limit(None);
    tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
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
        let t = PairTarget::parse(&format!("graviton://pair?host=192.168.1.20&port=7843&fp={fp}&code=Z5VRBPZ3&name=Razeen%20Mac%20mini")).unwrap();
        assert_eq!((t.address.as_str(), t.port, t.code.as_deref(), t.name.as_deref()), ("192.168.1.20", 7843, Some("Z5VRBPZ3"), Some("Razeen Mac mini")));
        assert_eq!(t.fingerprint.as_deref(), Some(fp.as_str()));
        let t = PairTarget::parse("mini.local").unwrap();
        assert_eq!((t.address.as_str(), t.port), ("mini.local", REMOTE_PORT));
        assert_eq!(PairTarget::parse("10.0.0.5:9000").unwrap().port, 9000);
        assert_eq!(short_name("My Mac mini"), "my-mac-mini");
        assert_eq!(fingerprint_short(&"f00418f9".repeat(8)), "F004 18F9 F004 18F9");
    }
}
