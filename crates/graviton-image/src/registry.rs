//! A small OCI distribution client: anonymous and authenticated (Bearer / Basic)
//! pulls, manifest negotiation (OCI and Docker v2, single and multi-platform),
//! and verified blob downloads.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use graviton_core::ImageReference;
use serde::Deserialize;
use ureq::Agent;

use crate::{BlobStore, Digest, ImageConfig, ImageError, Layer, LocalImage, Result, guest_arch};

const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.oci.image.manifest.v1+json, \
application/vnd.docker.distribution.manifest.v2+json";

const INDEX_TYPES: [&str; 2] = [
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
];

/// Username and password (or personal access token) for an OCI registry.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"[redacted]")
            .finish()
    }
}

/// Pull progress, reported as layers download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    Resolved { manifest: String, layers: usize, total_bytes: u64 },
    Layer { index: usize, digest: String, done: u64, total: u64, cached: bool },
}

pub struct Registry {
    agent: Agent,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    #[serde(default)]
    media_type: String,
    digest: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    platform: Option<Platform>,
}

#[derive(Deserialize)]
struct Platform {
    architecture: String,
    os: String,
    #[serde(default)]
    variant: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    #[serde(default)]
    media_type: Option<String>,
    #[serde(default)]
    manifests: Option<Vec<Descriptor>>,
    #[serde(default)]
    config: Option<Descriptor>,
    #[serde(default)]
    layers: Option<Vec<Descriptor>>,
}

impl Registry {
    pub fn new() -> Self {
        let agent: Agent = Agent::config_builder()
            .http_status_as_error(false)
            .user_agent(concat!("graviton/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self { agent }
    }

    /// Pull `reference` anonymously for the guest's architecture into `blobs`.
    pub fn pull(&self, reference: &ImageReference, blobs: &BlobStore, progress: impl FnMut(Progress)) -> Result<LocalImage> {
        self.pull_with_auth(reference, blobs, None, progress)
    }

    /// Pull `reference` for the guest's architecture into `blobs`, using `auth`
    /// when the registry asks for credentials.
    pub fn pull_with_auth(
        &self,
        reference: &ImageReference,
        blobs: &BlobStore,
        auth: Option<&Credentials>,
        mut progress: impl FnMut(Progress),
    ) -> Result<LocalImage> {
        let canonical = normalize_registry(&reference.registry);
        let host = registry_host(&canonical);
        let repo = &reference.repository;
        let mut session = Session::new(&self.agent, canonical, host, repo.clone(), auth);

        let tag_or_digest = reference.digest.clone().or_else(|| reference.tag.clone()).unwrap_or_else(|| "latest".into());
        let (mut bytes, mut media_type, mut digest) = session.manifest(&tag_or_digest)?;
        let mut manifest: Manifest = json(&bytes, "manifest")?;
        let media = manifest.media_type.clone().unwrap_or(media_type.clone());

        if INDEX_TYPES.contains(&media.as_str()) || manifest.manifests.is_some() {
            let entries = manifest.manifests.take().unwrap_or_default();
            let chosen = pick_platform(&entries, guest_arch()).ok_or_else(|| ImageError::NoPlatform {
                reference: reference.display_name(),
                arch: guest_arch().into(),
                available: describe_platforms(&entries),
            })?;
            let chosen_digest = chosen.digest.clone();
            (bytes, media_type, digest) = session.manifest(&chosen_digest)?;
            manifest = json(&bytes, "manifest")?;
            let _ = media_type;
        }

        let config = manifest.config.ok_or_else(|| ImageError::Unsupported("manifest without a config".into()))?;
        let layers = manifest.layers.unwrap_or_default();
        let manifest_digest = Digest::parse(&digest)?;
        blobs.write_bytes(&manifest_digest, &bytes)?;

        let total_bytes = layers.iter().map(|l| l.size).sum();
        progress(Progress::Resolved { manifest: manifest_digest.to_string(), layers: layers.len(), total_bytes });

        let config_digest = Digest::parse(&config.digest)?;
        if !blobs.has(&config_digest) {
            session.blob(&config_digest, blobs, |_| {})?;
        }
        let image_config: ImageConfig =
            serde_json::from_slice(&blobs.read(&config_digest)?).map_err(|source| ImageError::Json { what: "image config".into(), source })?;

        for (index, layer) in layers.iter().enumerate() {
            let d = Digest::parse(&layer.digest)?;
            if blobs.has(&d) {
                progress(Progress::Layer { index, digest: d.to_string(), done: layer.size, total: layer.size, cached: true });
                continue;
            }
            let total = layer.size;
            session.blob(&d, blobs, |done| {
                progress(Progress::Layer { index, digest: d.to_string(), done, total, cached: false })
            })?;
        }

        Ok(LocalImage {
            reference: reference.to_string(),
            manifest_digest: manifest_digest.to_string(),
            config_digest: config_digest.to_string(),
            layers: layers.into_iter().map(|l| Layer { digest: l.digest, size: l.size, media_type: l.media_type }).collect(),
            architecture: if image_config.architecture.is_empty() { guest_arch().into() } else { image_config.architecture },
            pulled_at: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        })
    }

    /// Check that `creds` are accepted by `registry` (`GET /v2/`).
    /// Returns the canonical registry name (e.g. `"docker.io"`, `"ghcr.io"`).
    pub fn verify_login(&self, registry: &str, creds: &Credentials) -> Result<String> {
        let canonical = normalize_registry(registry);
        if canonical.is_empty() || canonical.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(ImageError::Registry(format!("invalid registry name `{registry}`")));
        }
        let host = registry_host(&canonical);
        let mut session = Session::new(&self.agent, canonical.clone(), host, String::new(), Some(creds));
        let response = session.get_path("/v2/", None)?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(ImageError::Registry(format!("{canonical} returned HTTP {status} for /v2/")));
        }
        Ok(canonical)
    }
}

struct Session<'a> {
    agent: &'a Agent,
    registry: String,
    host: String,
    scheme: &'static str,
    repo: String,
    auth: Option<&'a Credentials>,
    authorization: Option<String>,
}

impl<'a> Session<'a> {
    fn new(agent: &'a Agent, registry: String, host: String, repo: String, auth: Option<&'a Credentials>) -> Self {
        Self { agent, registry, host, scheme: "https", repo, auth, authorization: None }
    }

    /// Fetch a manifest; returns (bytes, media type, digest).
    fn manifest(&mut self, reference: &str) -> Result<(Vec<u8>, String, String)> {
        let path = format!("/v2/{}/manifests/{}", self.repo, reference);
        let mut response = self.get_path(&path, Some(MANIFEST_ACCEPT))?;
        let status = response.status().as_u16();
        if status == 404 {
            return Err(ImageError::NotFound(format!("{}:{}", self.repo, reference)));
        }
        if status != 200 {
            return Err(ImageError::Registry(format!("returned HTTP {status} for {}://{}{path}", self.scheme, self.host)));
        }
        let media_type = header(&response, "content-type").unwrap_or_default();
        let header_digest = header(&response, "docker-content-digest");
        let bytes = response.body_mut().with_config().limit(16 * 1024 * 1024).read_to_vec().map_err(http_err)?;
        let computed = Digest::of(&bytes);
        // When we asked by digest, the content must match it.
        if reference.starts_with("sha256:") && computed.as_str() != reference {
            return Err(ImageError::DigestMismatch { expected: reference.into(), actual: computed.to_string() });
        }
        let digest = header_digest.filter(|d| d == computed.as_str()).unwrap_or_else(|| computed.to_string());
        Ok((bytes, media_type, digest))
    }

    fn blob(&mut self, digest: &Digest, blobs: &BlobStore, on_bytes: impl FnMut(u64)) -> Result<u64> {
        let path = format!("/v2/{}/blobs/{}", self.repo, digest);
        let response = self.get_path(&path, None)?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(ImageError::Registry(format!("returned HTTP {status} for blob {}", digest.short())));
        }
        let reader = response.into_body().into_with_config().limit(u64::MAX).reader();
        blobs.write_verified(digest, reader, on_bytes)
    }

    fn send_once(&mut self, path: &str, accept: Option<&str>) -> Result<ureq::http::Response<ureq::Body>> {
        let make = |scheme: &str, host: &str, authorization: Option<&str>| {
            let url = format!("{scheme}://{host}{path}");
            let mut req = self.agent.get(&url);
            if let Some(accept) = accept {
                req = req.header("Accept", accept);
            }
            if let Some(auth_val) = authorization {
                req = req.header("Authorization", auth_val);
            }
            req.call()
        };

        match make(self.scheme, &self.host, self.authorization.as_deref()) {
            Ok(resp) => Ok(resp),
            // Standard Docker/OCI convenience: allow plain HTTP only for loopback registries (e.g. localhost:5000).
            Err(err) if self.scheme == "https" && is_loopback_host(&self.host) => {
                match make("http", &self.host, self.authorization.as_deref()) {
                    Ok(resp) => {
                        self.scheme = "http";
                        Ok(resp)
                    }
                    Err(_) => Err(http_err(err)),
                }
            }
            Err(err) => Err(http_err(err)),
        }
    }

    /// GET with Bearer or Basic auth, answering a 401 challenge on the first attempt.
    fn get_path(&mut self, path: &str, accept: Option<&str>) -> Result<ureq::http::Response<ureq::Body>> {
        for attempt in 0..2 {
            let response = self.send_once(path, accept)?;
            let status = response.status().as_u16();
            if status == 401 && attempt == 0 {
                let challenge = raw_header(&response, "www-authenticate")
                    .ok_or_else(|| ImageError::Registry("asked for auth without a WWW-Authenticate challenge".into()))?;
                match parse_auth_challenge(&challenge) {
                    Some(Challenge::Bearer(params)) => {
                        let token = self.fetch_token(&params)?;
                        self.authorization = Some(format!("Bearer {token}"));
                        continue;
                    }
                    Some(Challenge::Basic) => {
                        let creds = self.auth.ok_or_else(|| {
                            ImageError::Registry(format!("{} requires login (run `grav login {}`)", self.registry, self.registry))
                        })?;
                        self.authorization = Some(basic_header(creds));
                        continue;
                    }
                    None => return Err(ImageError::Unsupported(format!("auth challenge `{challenge}`"))),
                }
            }
            if status == 401 || status == 403 {
                return Err(self.denied_error());
            }
            return Ok(response);
        }
        unreachable!("loop returns on the second attempt")
    }

    fn denied_error(&self) -> ImageError {
        if self.repo.is_empty() {
            match self.auth {
                Some(_) => ImageError::Registry(format!("authentication failed for {} (invalid username or password)", self.registry)),
                None => ImageError::Registry(format!("{} requires login (run `grav login {}`)", self.registry, self.registry)),
            }
        } else {
            match self.auth {
                Some(creds) => ImageError::Registry(format!(
                    "denied access to {} on {} as {} (check permissions or run `grav login {}`)",
                    self.repo, self.registry, creds.username, self.registry
                )),
                None => ImageError::Registry(format!(
                    "denied access to {} on {} (private images need `grav login {}`)",
                    self.repo, self.registry, self.registry
                )),
            }
        }
    }

    fn fetch_token(&self, params: &[(String, String)]) -> Result<String> {
        let realm = params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("realm"))
            .map(|(_, v)| v.clone())
            .ok_or_else(|| ImageError::Registry("auth challenge has no realm".into()))?;
        let mut req = self.agent.get(&realm);
        let mut has_scope = false;
        let mut has_account = false;
        for (k, v) in params {
            if k.eq_ignore_ascii_case("service") || k.eq_ignore_ascii_case("scope") {
                if k.eq_ignore_ascii_case("scope") {
                    has_scope = true;
                }
                req = req.query(k, v);
            } else if k.eq_ignore_ascii_case("account") {
                has_account = true;
                req = req.query(k, v);
            }
        }
        if !has_scope && !self.repo.is_empty() {
            req = req.query("scope", format!("repository:{}:pull", self.repo));
        }
        if let Some(creds) = self.auth {
            if !has_account {
                req = req.query("account", &creds.username);
            }
            req = req.header("Authorization", &basic_header(creds));
        }
        let mut response = req.call().map_err(http_err)?;
        let status = response.status().as_u16();
        if status == 401 || status == 403 {
            return Err(self.denied_error());
        }
        if status != 200 {
            return Err(ImageError::Registry(format!("token service returned HTTP {status}")));
        }
        #[derive(Deserialize)]
        struct Token {
            token: Option<String>,
            access_token: Option<String>,
        }
        let bytes = response.body_mut().read_to_vec().map_err(http_err)?;
        let t: Token = json(&bytes, "token response")?;
        t.token.or(t.access_token).ok_or_else(|| ImageError::Registry("token response had no token".into()))
    }
}

/// Canonicalize a registry name (e.g. `https://index.docker.io/v1/` → `docker.io`).
pub fn normalize_registry(input: &str) -> String {
    let trimmed = input.trim();
    let without_scheme = if trimmed.len() >= 8 && trimmed[..8].eq_ignore_ascii_case("https://") {
        &trimmed[8..]
    } else if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("http://") {
        &trimmed[7..]
    } else {
        trimmed
    };
    let host = without_scheme.split('/').next().unwrap_or("").trim().to_ascii_lowercase();
    match host.as_str() {
        "" | "docker.io" | "index.docker.io" | "registry-1.docker.io" | "registry.hub.docker.com" => "docker.io".into(),
        _ => host,
    }
}

/// Docker Hub's API lives on a different host than its name.
fn registry_host(registry: &str) -> String {
    match registry {
        "docker.io" | "index.docker.io" => "registry-1.docker.io".into(),
        other => other.into(),
    }
}

fn is_loopback_host(host: &str) -> bool {
    let bare = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        host.split(':').next().unwrap_or(host)
    };
    matches!(bare, "localhost" | "127.0.0.1" | "::1")
}

fn basic_header(creds: &Credentials) -> String {
    let raw = format!("{}:{}", creds.username, creds.password);
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw.as_bytes()))
}

#[derive(Debug, PartialEq, Eq)]
enum Challenge {
    Bearer(Vec<(String, String)>),
    Basic,
}

fn parse_auth_challenge(challenge: &str) -> Option<Challenge> {
    let s = challenge.trim();
    let (scheme, rest) = match s.find(char::is_whitespace) {
        Some(idx) => (&s[..idx], s[idx..].trim_start()),
        None => (s, ""),
    };
    if scheme.eq_ignore_ascii_case("bearer") {
        Some(Challenge::Bearer(parse_challenge_params(rest)))
    } else if scheme.eq_ignore_ascii_case("basic") {
        Some(Challenge::Basic)
    } else {
        None
    }
}

/// `realm="…",service="…",scope="…"` → [(realm, …), (service, …), (scope, …)]
fn parse_challenge_params(rest: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut chars = rest.chars().peekable();
    loop {
        while matches!(chars.peek(), Some(' ' | ',')) {
            chars.next();
        }
        let key: String = chars.by_ref().take_while(|&c| c != '=').collect();
        if key.is_empty() {
            break;
        }
        let value: String = if chars.peek() == Some(&'"') {
            chars.next();
            chars.by_ref().take_while(|&c| c != '"').collect()
        } else {
            chars.by_ref().take_while(|&c| c != ',').collect()
        };
        out.push((key.trim().to_string(), value));
    }
    out
}

fn pick_platform<'a>(entries: &'a [Descriptor], arch: &str) -> Option<&'a Descriptor> {
    let linux = |d: &&Descriptor| d.platform.as_ref().is_some_and(|p| p.os == "linux" && p.architecture == arch);
    // Prefer arm64/v8 or no variant; skip attestation manifests (platform unknown/unknown).
    entries
        .iter()
        .filter(linux)
        .find(|d| d.platform.as_ref().is_some_and(|p| matches!(p.variant.as_deref(), None | Some("v8"))))
        .or_else(|| entries.iter().find(linux))
}

fn describe_platforms(entries: &[Descriptor]) -> String {
    let list: Vec<String> = entries
        .iter()
        .filter_map(|d| d.platform.as_ref())
        .filter(|p| p.os != "unknown")
        .map(|p| match &p.variant {
            Some(v) => format!("{}/{}/{v}", p.os, p.architecture),
            None => format!("{}/{}", p.os, p.architecture),
        })
        .collect();
    if list.is_empty() { "none".into() } else { list.join(", ") }
}

fn header(response: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
    response.headers().get(name).and_then(|v| v.to_str().ok()).map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
}

fn raw_header(response: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
    response.headers().get(name).and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string())
}

fn json<T: serde::de::DeserializeOwned>(bytes: &[u8], what: &str) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|source| ImageError::Json { what: what.into(), source })
}

fn http_err(err: ureq::Error) -> ImageError {
    ImageError::Registry(err.to_string())
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::thread;

    use super::*;

    #[test]
    fn parses_bearer_and_basic_challenges() {
        let c = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull""#;
        let Some(Challenge::Bearer(p)) = parse_auth_challenge(c) else { panic!("expected bearer") };
        assert_eq!(p[0], ("realm".into(), "https://auth.docker.io/token".into()));
        assert_eq!(p[1], ("service".into(), "registry.docker.io".into()));
        assert_eq!(p[2], ("scope".into(), "repository:library/alpine:pull".into()));
        assert_eq!(parse_auth_challenge(r#"Basic realm="Registry""#), Some(Challenge::Basic));
        assert_eq!(parse_auth_challenge("basic"), Some(Challenge::Basic));
        assert_eq!(parse_auth_challenge("Digest realm=x"), None);
    }

    #[test]
    fn normalizes_registry_names() {
        assert_eq!(normalize_registry(""), "docker.io");
        assert_eq!(normalize_registry("docker.io"), "docker.io");
        assert_eq!(normalize_registry("https://index.docker.io/v1/"), "docker.io");
        assert_eq!(normalize_registry("registry-1.docker.io"), "docker.io");
        assert_eq!(normalize_registry("https://GHCR.IO/"), "ghcr.io");
        assert_eq!(normalize_registry("http://localhost:5000/v2/"), "localhost:5000");
        assert_eq!(registry_host("docker.io"), "registry-1.docker.io");
        assert_eq!(registry_host("ghcr.io"), "ghcr.io");
        assert!(is_loopback_host("localhost:5000"));
        assert!(is_loopback_host("127.0.0.1:8080"));
        assert!(is_loopback_host("[::1]:5000"));
        assert!(!is_loopback_host("ghcr.io"));
        assert!(!is_loopback_host("192.0.2.62:5000"));
    }

    #[test]
    fn redacts_password_in_debug() {
        let c = Credentials { username: "alice".into(), password: "super-secret-value".into() };
        let dbg = format!("{c:?}");
        assert!(dbg.contains("alice"));
        assert!(!dbg.contains("super-secret-value"));
    }

    #[test]
    fn picks_linux_arm64() {
        let index: Manifest = serde_json::from_str(r#"{"manifests":[
            {"digest":"sha256:amd","platform":{"architecture":"amd64","os":"linux"}},
            {"digest":"sha256:v7","platform":{"architecture":"arm","os":"linux","variant":"v7"}},
            {"digest":"sha256:arm","platform":{"architecture":"arm64","os":"linux","variant":"v8"}},
            {"digest":"sha256:att","platform":{"architecture":"unknown","os":"unknown"}}]}"#).unwrap();
        let entries = index.manifests.unwrap();
        assert_eq!(pick_platform(&entries, "arm64").unwrap().digest, "sha256:arm");
        assert!(pick_platform(&entries, "riscv64").is_none());
        assert_eq!(describe_platforms(&entries), "linux/amd64, linux/arm/v7, linux/arm64/v8");
    }

    /// Tiny in-process mock HTTP OCI registry supporting Bearer or Basic auth + zstd layer blobs.
    fn spawn_mock_registry(mode: &'static str, expected_user: &'static str, expected_pass: &'static str) -> (String, Digest, Digest) {
        let config_bytes = format!(r#"{{"architecture":"{}","os":"linux","config":{{"Cmd":["/bin/true"]}}}}"#, guest_arch()).into_bytes();
        let config_digest = Digest::of(&config_bytes);
        // Minimal valid zstd frame carrying b"layer-data"
        let mut layer_bytes = vec![0x28, 0xb5, 0x2f, 0xfd, 0x20, 10, 0x51, 0x00, 0x00];
        layer_bytes.extend_from_slice(b"layer-data");
        let layer_digest = Digest::of(&layer_bytes);
        let manifest_bytes = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config_digest}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar+zstd","digest":"{layer_digest}","size":{}}}]}}"#,
            config_bytes.len(),
            layer_bytes.len()
        )
        .into_bytes();
        let manifest_digest = Digest::of(&manifest_bytes);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let expected_basic = basic_header(&Credentials { username: expected_user.into(), password: expected_pass.into() });
        let realm = format!("http://{addr}/token");
        let config_bytes = Arc::new(config_bytes);
        let layer_bytes = Arc::new(layer_bytes);
        let manifest_bytes = Arc::new(manifest_bytes);
        let cfg_d = config_digest.to_string();
        let lyr_d = layer_digest.to_string();
        let man_d = manifest_digest.to_string();

        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut stream = stream;
                let mut req_line = String::new();
                if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
                    continue;
                }
                let path = req_line.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut auth_header = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" || line == "\n" {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        if k.trim().eq_ignore_ascii_case("authorization") {
                            auth_header = Some(v.trim().to_string());
                        }
                    }
                }

                let write_resp = |s: &mut std::net::TcpStream, status: &str, headers: &[(&str, &str)], body: &[u8]| {
                    let mut head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
                    for (k, v) in headers {
                        head.push_str(&format!("{k}: {v}\r\n"));
                    }
                    head.push_str("\r\n");
                    let _ = s.write_all(head.as_bytes());
                    let _ = s.write_all(body);
                };

                if path.starts_with("/token") {
                    if auth_header.as_deref() == Some(expected_basic.as_str()) {
                        write_resp(&mut stream, "200 OK", &[("Content-Type", "application/json")], br#"{"token":"tok-123"}"#);
                    } else {
                        write_resp(&mut stream, "401 Unauthorized", &[], b"bad credentials");
                    }
                    continue;
                }

                let authorized = match mode {
                    "bearer" => auth_header.as_deref() == Some("Bearer tok-123"),
                    "basic" => auth_header.as_deref() == Some(expected_basic.as_str()),
                    _ => false,
                };
                if !authorized {
                    let challenge = if mode == "bearer" {
                        format!(r#"Bearer realm="{realm}",service="mock""#)
                    } else {
                        r#"Basic realm="mock""#.into()
                    };
                    write_resp(&mut stream, "401 Unauthorized", &[("WWW-Authenticate", &challenge)], b"auth required");
                    continue;
                }

                if path == "/v2/" {
                    write_resp(&mut stream, "200 OK", &[], b"{}");
                } else if path == "/v2/team/app/manifests/1.0" {
                    write_resp(
                        &mut stream,
                        "200 OK",
                        &[
                            ("Content-Type", "application/vnd.oci.image.manifest.v1+json"),
                            ("Docker-Content-Digest", &man_d),
                        ],
                        &manifest_bytes,
                    );
                } else if path == format!("/v2/team/app/blobs/{cfg_d}") {
                    write_resp(&mut stream, "200 OK", &[], &config_bytes);
                } else if path == format!("/v2/team/app/blobs/{lyr_d}") {
                    write_resp(&mut stream, "200 OK", &[], &layer_bytes);
                } else {
                    write_resp(&mut stream, "404 Not Found", &[], b"not found");
                }
            }
        });

        (addr, config_digest, layer_digest)
    }

    #[test]
    fn bearer_auth_login_and_zstd_pull() {
        let (addr, _, layer_digest) = spawn_mock_registry("bearer", "octocat", "ghp_secret");
        let reg = Registry::new();
        let good = Credentials { username: "octocat".into(), password: "ghp_secret".into() };
        let bad = Credentials { username: "octocat".into(), password: "wrong".into() };

        assert_eq!(reg.verify_login(&addr, &good).unwrap(), addr);
        assert!(reg.verify_login(&addr, &bad).is_err());

        let tmp = std::env::temp_dir().join(format!("graviton-reg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let blobs = BlobStore::new(tmp.clone());
        let img_ref = ImageReference::parse(&format!("{addr}/team/app:1.0")).unwrap();

        // Anonymous pull fails with actionable message.
        let err = reg.pull(&img_ref, &blobs, |_| {}).unwrap_err();
        assert!(err.to_string().contains("grav login"), "got: {err}");

        // Authenticated pull succeeds including the zstd layer and emits progress.
        let mut events = Vec::new();
        let img = reg.pull_with_auth(&img_ref, &blobs, Some(&good), |p| events.push(p)).unwrap();
        assert_eq!(img.layers.len(), 1);
        assert!(img.layers[0].media_type.contains("zstd"));
        assert!(blobs.has(&layer_digest));
        assert!(events.iter().any(|e| matches!(e, Progress::Resolved { layers: 1, .. })));
        assert!(events.iter().any(|e| matches!(e, Progress::Layer { index: 0, cached: false, .. })));

        // Second pull reports the layer as cached.
        let mut events2 = Vec::new();
        reg.pull_with_auth(&img_ref, &blobs, Some(&good), |p| events2.push(p)).unwrap();
        assert!(events2.iter().any(|e| matches!(e, Progress::Layer { index: 0, cached: true, .. })));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn basic_auth_login_and_pull() {
        let (addr, _, _) = spawn_mock_registry("basic", "dev", "hunter2");
        let reg = Registry::new();
        let good = Credentials { username: "dev".into(), password: "hunter2".into() };
        let bad = Credentials { username: "dev".into(), password: "nope".into() };

        assert_eq!(reg.verify_login(&addr, &good).unwrap(), addr);
        assert!(reg.verify_login(&addr, &bad).is_err());

        let tmp = std::env::temp_dir().join(format!("graviton-reg-basic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let blobs = BlobStore::new(tmp.clone());
        let img_ref = ImageReference::parse(&format!("{addr}/team/app:1.0")).unwrap();
        assert!(reg.pull(&img_ref, &blobs, |_| {}).is_err());
        let img = reg.pull_with_auth(&img_ref, &blobs, Some(&good), |_| {}).unwrap();
        assert_eq!(img.layers.len(), 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
