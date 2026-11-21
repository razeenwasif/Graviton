//! The remote endpoint: how the iPhone app (or another Mac) reaches gravitond.
//!
//! Off unless gravitond runs with `--remote`. It listens on TCP
//! `graviton_proto::REMOTE_PORT` with TLS, using a self-signed certificate made once and kept
//! in `~/Library/Application Support/Graviton/remote/`. Phones pin its
//! SHA-256 fingerprint. The connection speaks the same JSON lines as the Unix
//! socket, but its first message must be one of:
//!
//! * `pair` with a one-time code from `grav pair` (valid five minutes); the
//!   answer carries a long-lived token for that device.
//! * `auth` with such a token.
//!
//! Tokens are stored only as SHA-256 hashes in `devices.json`; `grav devices
//! revoke` forgets one. After [`MAX_FAILURES`] wrong codes every pending code
//! is cancelled. The service is advertised over Bonjour as `_graviton._tcp`.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use graviton_core::paths;
use graviton_proto::{PairedDevice, PairingInfo, Request, Response, decode_line, encode_line};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const CODE_TTL: Duration = Duration::from_secs(300);
const MAX_FAILURES: u32 = 5;
/// No 0/O, 1/I/L: easy to read off a screen and type on a phone.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

#[derive(Clone, Serialize, Deserialize)]
struct Device {
    id: String,
    name: String,
    token_sha256: String,
    paired_at: u64,
    last_seen: Option<u64>,
    /// Its APNs device token (hex), once it has registered for alerts.
    #[serde(default)]
    push_token: Option<String>,
    /// Registered from a debug build: alerts go through APNs's sandbox.
    #[serde(default)]
    push_sandbox: bool,
}

/// Where to send a device's push alerts.
pub struct PushTarget {
    pub name: String,
    pub token: String,
    pub sandbox: bool,
}

struct PendingCode {
    code: String,
    expires: Instant,
}

#[derive(Default)]
struct State {
    devices: Vec<Device>,
    codes: Vec<PendingCode>,
    failures: u32,
}

pub struct Remote {
    /// Push alerts, set once at start-up.
    pub push: std::sync::OnceLock<Arc<crate::push::Push>>,
    state: Mutex<State>,
    devices_path: PathBuf,
    acceptor: tokio_rustls::TlsAcceptor,
    pub fingerprint: String,
    pub port: u16,
    pub host: String,
    rng: SystemRandom,
    _bonjour: Option<Bonjour>,
}

impl Remote {
    pub fn open(port: u16) -> Result<Arc<Self>> {
        let dir = paths::support_dir().join("remote");
        fs::create_dir_all(&dir)?;
        let (cert, key) = identity(&dir)?;
        let fingerprint = hex(ring::digest::digest(&ring::digest::SHA256, &cert).as_ref());

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(
                vec![rustls::pki_types::CertificateDer::from(cert)],
                rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(key)),
            )?;

        let devices_path = dir.join("devices.json");
        let devices = match fs::read(&devices_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("reading devices.json")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let host = computer_name();
        Ok(Arc::new(Self {
            push: std::sync::OnceLock::new(),
            state: Mutex::new(State { devices, ..Default::default() }),
            devices_path,
            acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(config)),
            fingerprint,
            port,
            _bonjour: Bonjour::register(&host, port),
            host,
            rng: SystemRandom::new(),
        }))
    }

    /// `grav pair`: a fresh one-time code.
    pub fn begin_pairing(&self) -> Result<PairingInfo> {
        let mut bytes = [0u8; 8];
        self.rng.fill(&mut bytes).map_err(|_| anyhow::anyhow!("no randomness"))?;
        let code: String = bytes.iter().map(|b| CODE_ALPHABET[*b as usize % CODE_ALPHABET.len()] as char).collect();
        let mut state = self.state.lock().unwrap();
        state.codes.retain(|c| c.expires > Instant::now());
        state.codes.push(PendingCode { code: code.clone(), expires: Instant::now() + CODE_TTL });
        state.failures = 0;
        Ok(PairingInfo {
            code,
            expires_in_secs: CODE_TTL.as_secs(),
            host: self.host.clone(),
            port: self.port,
            fingerprint: self.fingerprint.clone(),
            addresses: lan_addresses(),
        })
    }

    pub fn devices(&self) -> Vec<PairedDevice> {
        self.state
            .lock()
            .unwrap()
            .devices
            .iter()
            .map(|d| PairedDevice { id: d.id.clone(), name: d.name.clone(), paired_at: d.paired_at, last_seen: d.last_seen })
            .collect()
    }

    pub fn revoke(&self, id: &str) -> Result<String> {
        let mut state = self.state.lock().unwrap();
        let before = state.devices.len();
        state.devices.retain(|d| d.id != id && !d.id.starts_with(id));
        if state.devices.len() == before {
            bail!("no paired device {id}");
        }
        self.save(&state.devices);
        Ok(id.to_string())
    }

    fn pair(&self, code: &str, device: &str) -> Result<Response> {
        let mut state = self.state.lock().unwrap();
        state.codes.retain(|c| c.expires > Instant::now());
        let typed = normalize_code(code);
        let Some(pos) = state.codes.iter().position(|c| constant_time_eq(c.code.as_bytes(), typed.as_bytes())) else {
            state.failures += 1;
            if state.failures >= MAX_FAILURES {
                state.codes.clear();
                bail!("too many wrong codes; run `grav pair` again on the Mac");
            }
            bail!("that pairing code isn't valid (codes last five minutes and work once)");
        };
        state.codes.remove(pos);

        let mut raw = [0u8; 32];
        self.rng.fill(&mut raw).map_err(|_| anyhow::anyhow!("no randomness"))?;
        let token = hex(&raw);
        let mut id_bytes = [0u8; 6];
        self.rng.fill(&mut id_bytes).map_err(|_| anyhow::anyhow!("no randomness"))?;
        let name: String = device.chars().filter(|c| !c.is_control()).take(64).collect();
        let entry = Device {
            id: hex(&id_bytes),
            name: if name.trim().is_empty() { "Unnamed device".into() } else { name },
            token_sha256: sha256_hex(token.as_bytes()),
            paired_at: now_secs(),
            last_seen: Some(now_secs()),
            push_token: None,
            push_sandbox: false,
        };
        let device_id = entry.id.clone();
        state.devices.push(entry);
        self.save(&state.devices);
        eprintln!("paired {device_id}");
        Ok(Response::Paired { token, device_id, host: self.host.clone() })
    }

    /// The device a token belongs to.
    fn authenticate(&self, token: &str) -> Option<String> {
        let hash = sha256_hex(token.as_bytes());
        let mut state = self.state.lock().unwrap();
        let mut found = None;
        for d in state.devices.iter_mut() {
            if constant_time_eq(d.token_sha256.as_bytes(), hash.as_bytes()) {
                d.last_seen = Some(now_secs());
                found = Some(d.id.clone());
            }
        }
        if found.is_some() {
            let devices = state.devices.clone();
            drop(state);
            self.save(&devices);
        }
        found
    }

    /// Remember where to send a device's alerts.
    pub fn register_push(&self, device_id: &str, token: &str, sandbox: bool) -> Result<()> {
        let token = token.trim().to_ascii_lowercase();
        if !(32..=200).contains(&token.len()) || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            anyhow::bail!("that isn't an APNs device token");
        }
        let mut state = self.state.lock().unwrap();
        let device = state.devices.iter_mut().find(|d| d.id == device_id).ok_or_else(|| anyhow::anyhow!("unknown device"))?;
        device.push_token = Some(token);
        device.push_sandbox = sandbox;
        let devices = state.devices.clone();
        drop(state);
        self.save(&devices);
        Ok(())
    }

    /// APNs said this token is no longer valid (the app was removed).
    pub fn forget_push(&self, token: &str) {
        let mut state = self.state.lock().unwrap();
        for d in state.devices.iter_mut().filter(|d| d.push_token.as_deref() == Some(token)) {
            d.push_token = None;
        }
        let devices = state.devices.clone();
        drop(state);
        self.save(&devices);
    }

    pub fn push_targets(&self) -> Vec<PushTarget> {
        self.state
            .lock()
            .unwrap()
            .devices
            .iter()
            .filter_map(|d| d.push_token.clone().map(|token| PushTarget { name: d.name.clone(), token, sandbox: d.push_sandbox }))
            .collect()
    }

    pub fn host_name(&self) -> &str {
        &self.host
    }

    fn save(&self, devices: &[Device]) {
        let tmp = self.devices_path.with_extension("json.tmp");
        let ok = fs::write(&tmp, serde_json::to_vec_pretty(devices).expect("devices serialise")).and_then(|_| {
            set_private(&tmp)?;
            fs::rename(&tmp, &self.devices_path)
        });
        if let Err(err) = ok {
            eprintln!("couldn't save paired devices: {err}");
        }
    }

    /// Accept remote connections forever.
    pub async fn serve(self: Arc<Self>, backend: crate::Backend) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", self.port))
            .await
            .with_context(|| format!("listening on TCP port {}", self.port))?;
        eprintln!("remote endpoint on port {} (fingerprint {})", self.port, short_fingerprint(&self.fingerprint));
        loop {
            let (tcp, peer) = listener.accept().await?;
            let remote = Arc::clone(&self);
            let backend = backend.clone();
            tokio::spawn(async move {
                if let Err(err) = remote.session(tcp, backend).await {
                    eprintln!("remote {peer}: {err:#}");
                }
            });
        }
    }

    async fn session(&self, tcp: tokio::net::TcpStream, backend: crate::Backend) -> Result<()> {
        let tls = tokio::time::timeout(Duration::from_secs(10), self.acceptor.accept(tcp)).await.context("TLS handshake timed out")??;
        let (read, mut write) = tokio::io::split(tls);
        let mut lines = BufReader::new(read).lines();

        // The first message decides who this is.
        let Some(first) = tokio::time::timeout(Duration::from_secs(15), lines.next_line()).await.context("no hello")?? else {
            return Ok(());
        };
        let mut device_id: Option<String> = None;
        let reply = match decode_line::<Request>(first.as_bytes()) {
            Ok(Request::Pair { code, device }) => match self.pair(&code, &device) {
                Ok(reply) => {
                    if let Response::Paired { device_id: id, .. } = &reply {
                        device_id = Some(id.clone());
                    }
                    reply
                }
                Err(e) => Response::Error { message: e.to_string() },
            },
            Ok(Request::Auth { token }) => match self.authenticate(&token) {
                Some(id) => {
                    device_id = Some(id);
                    Response::Authenticated { host: self.host.clone() }
                }
                None => Response::Error { message: "this device isn't paired with this Mac any more".into() },
            },
            _ => Response::Error { message: "pair or authenticate first".into() },
        };
        let ok = matches!(reply, Response::Paired { .. } | Response::Authenticated { .. });
        write.write_all(&encode_line(&reply)).await?;
        if !ok {
            return Ok(());
        }

        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let request = match decode_line::<Request>(line.as_bytes()) {
                Ok(r) => r,
                Err(err) => {
                    write.write_all(&encode_line(&Response::Error { message: format!("bad request: {err}") })).await?;
                    continue;
                }
            };
            match (&request, &backend) {
                (Request::RegisterPush { token, sandbox }, _) => {
                    let reply = match device_id.as_deref().map(|id| self.register_push(id, token, *sandbox)) {
                        Some(Ok(())) => match self.push.get() {
                            Some(push) => Response::Push(push.info()),
                            None => Response::Error { message: "push alerts aren't available on this Mac".into() },
                        },
                        Some(Err(e)) => Response::Error { message: e.to_string() },
                        None => Response::Error { message: "authenticate first".into() },
                    };
                    write.write_all(&encode_line(&reply)).await?;
                }
                (Request::PairBegin | Request::ListDevices | Request::RevokeDevice { .. } | Request::Pair { .. } | Request::Auth { .. }
                | Request::PushSetup { .. } | Request::PushStatus | Request::PushTest, _) => {
                    write.write_all(&encode_line(&Response::Error { message: "only available on the Mac itself".into() })).await?;
                }
                (Request::Pull { reference, progress: true }, crate::Backend::Engine(engine)) => {
                    crate::pull_stream(Arc::clone(engine), reference.clone(), &mut write).await?;
                }
                (Request::Build(spec), crate::Backend::Engine(engine)) => {
                    if spec.upload.is_none() {
                        let message = "from another Mac, send the build folder with the request (grav build --host uploads it)";
                        write.write_all(&encode_line(&Response::Error { message: message.into() })).await?;
                        continue;
                    }
                    crate::build_stream(Arc::clone(engine), spec.clone(), lines.get_mut(), &mut write).await?;
                }
                (Request::Exec(spec), crate::Backend::Engine(engine)) => {
                    if spec.tty {
                        // The connection becomes a terminal session.
                        return crate::exec_tty(Arc::clone(engine), spec.clone(), lines.into_inner(), write).await;
                    }
                    crate::exec_stream(Arc::clone(engine), spec.clone(), &mut write).await?;
                }
                _ => {
                    let response = crate::handle(request, &backend).await;
                    write.write_all(&encode_line(&response)).await?;
                }
            }
        }
        Ok(())
    }
}

/// The endpoint's certificate and PKCS#8 key (DER), made on first use.
fn identity(dir: &std::path::Path) -> Result<(Vec<u8>, Vec<u8>)> {
    let (cert_path, key_path) = (dir.join("cert.der"), dir.join("key.der"));
    if let (Ok(cert), Ok(key)) = (fs::read(&cert_path), fs::read(&key_path)) {
        return Ok((cert, key));
    }
    let names = vec!["graviton.local".to_string(), format!("{}.local", computer_name().replace(' ', "-"))];
    let certified = rcgen::generate_simple_self_signed(names).context("making the remote certificate")?;
    let cert = certified.cert.der().to_vec();
    let key = certified.signing_key.serialize_der();
    fs::write(&key_path, &key)?;
    set_private(&key_path)?;
    fs::write(&cert_path, &cert)?;
    Ok((cert, key))
}

fn set_private(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

fn computer_name() -> String {
    std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Mac".into())
}

/// IPv4 addresses on this Mac's real interfaces (not loopback or the VMs' bridge).
fn lan_addresses() -> Vec<String> {
    let mut out = Vec::new();
    // SAFETY: getifaddrs allocates a list we walk and then free.
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut cur = list;
        while !cur.is_null() {
            let ifa = &*cur;
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy();
            if !ifa.ifa_addr.is_null()
                && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET
                && !name.starts_with("lo")
                && !name.starts_with("bridge")
                && !name.starts_with("utun")
            {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                out.push(ip.to_string());
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    out
}

/// Bonjour registration through the system's DNS-SD API, kept for the daemon's life.
struct Bonjour(*mut std::ffi::c_void);
// SAFETY: the reference is only deallocated, once, on drop.
unsafe impl Send for Bonjour {}
unsafe impl Sync for Bonjour {}

#[link(name = "System")]
unsafe extern "C" {
    fn DNSServiceRegister(
        sd_ref: *mut *mut std::ffi::c_void,
        flags: u32,
        interface_index: u32,
        name: *const std::ffi::c_char,
        regtype: *const std::ffi::c_char,
        domain: *const std::ffi::c_char,
        host: *const std::ffi::c_char,
        port_network_order: u16,
        txt_len: u16,
        txt_record: *const std::ffi::c_void,
        callback: *const std::ffi::c_void,
        context: *mut std::ffi::c_void,
    ) -> i32;
    fn DNSServiceRefDeallocate(sd_ref: *mut std::ffi::c_void);
}

impl Bonjour {
    fn register(name: &str, port: u16) -> Option<Self> {
        let name = std::ffi::CString::new(name).ok()?;
        let regtype = std::ffi::CString::new(graviton_proto::BONJOUR_SERVICE).ok()?;
        let mut sd_ref = std::ptr::null_mut();
        // SAFETY: valid C strings; a null callback is allowed (we don't need the result).
        let err = unsafe {
            DNSServiceRegister(&mut sd_ref, 0, 0, name.as_ptr(), regtype.as_ptr(), std::ptr::null(), std::ptr::null(), port.to_be(), 0, std::ptr::null(), std::ptr::null(), std::ptr::null_mut())
        };
        if err != 0 {
            eprintln!("Bonjour registration failed ({err}); phones can still connect by address");
            return None;
        }
        Some(Self(sd_ref))
    }
}

impl Drop for Bonjour {
    fn drop(&mut self) {
        // SAFETY: allocated by DNSServiceRegister and released once.
        unsafe { DNSServiceRefDeallocate(self.0) };
    }
}

pub fn short_fingerprint(fp: &str) -> String {
    fp.as_bytes().chunks(4).take(4).map(|c| String::from_utf8_lossy(c).to_uppercase()).collect::<Vec<_>>().join(" ")
}

fn normalize_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn sha256_hex(data: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, data).as_ref())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_comparisons() {
        assert_eq!(normalize_code("abcd-ef23"), "ABCDEF23");
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert_eq!(short_fingerprint("3fa2c91b00d4e5f6aa"), "3FA2 C91B 00D4 E5F6");
        assert_eq!(sha256_hex(b"hello").len(), 64);
    }
}
