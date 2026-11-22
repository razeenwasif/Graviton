//! A small OCI distribution client: anonymous and authenticated (Bearer / Basic)
//! pulls, manifest negotiation (OCI and Docker v2, single and multi-platform),
//! and verified blob downloads.

use std::collections::HashSet;
use std::fmt;
use std::fs::File;
use std::path::Path;
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

/// Options controlling registry authentication and target platform selection.
#[derive(Debug, Clone, Default)]
pub struct PullOptions<'a> {
    pub auth: Option<&'a Credentials>,
    /// Explicit platform (e.g. `linux/amd64`, `linux/arm64`, `amd64`, `arm64`).
    pub platform: Option<&'a str>,
    /// When `platform` is `None` and the native guest arch (`arm64`) isn't available,
    /// fall back to `linux/amd64` (for running under Apple Rosetta for Linux).
    pub allow_amd64_fallback: bool,
}

/// Normalize a user-supplied platform string into its OCI architecture name
/// (`"arm64"` or `"amd64"`), validating that the OS (if given) is `linux`.
pub fn normalize_platform(raw: &str) -> Result<String> {
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return Err(ImageError::Unsupported("empty platform".into()));
    }
    let parts: Vec<&str> = trimmed.split('/').collect();
    let arch = match parts.as_slice() {
        [a] => *a,
        [os, a] | [os, a, _] => {
            if *os != "linux" {
                return Err(ImageError::Unsupported(format!(
                    "platform `{raw}` (Graviton runs Linux images: linux/arm64, linux/amd64)"
                )));
            }
            *a
        }
        _ => {
            return Err(ImageError::Unsupported(format!(
                "platform `{raw}` (use linux/arm64 or linux/amd64)"
            )));
        }
    };
    match arch {
        "arm64" | "aarch64" => Ok("arm64".into()),
        "amd64" | "x86_64" | "x86-64" => Ok("amd64".into()),
        _ => Err(ImageError::Unsupported(format!(
            "architecture `{arch}` in platform `{raw}` (use linux/arm64 or linux/amd64)"
        ))),
    }
}

fn normalize_arch_label(arch: &str) -> String {
    match arch.trim().to_ascii_lowercase().as_str() {
        "" => guest_arch().into(),
        "aarch64" => "arm64".into(),
        "x86_64" | "x86-64" => "amd64".into(),
        other => other.into(),
    }
}

/// Pull progress, reported as layers download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    Resolved { manifest: String, layers: usize, total_bytes: u64 },
    Layer { index: usize, digest: String, done: u64, total: u64, cached: bool },
}

/// Push progress, reported as blobs and the manifest upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushProgress {
    Started { blobs: usize, total_bytes: u64 },
    Blob { index: usize, digest: String, done: u64, total: u64, skipped: bool },
    Manifest { digest: String, size: u64 },
}

/// Summary of a completed OCI registry push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushResult {
    pub reference: String,
    pub digest: String,
    pub size_bytes: u64,
    pub blobs_pushed: u32,
    pub blobs_skipped: u32,
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
        self.pull_with_options(reference, blobs, PullOptions::default(), progress)
    }

    /// Pull `reference` for the guest's architecture into `blobs`, using `auth`
    /// when the registry asks for credentials.
    pub fn pull_with_auth(
        &self,
        reference: &ImageReference,
        blobs: &BlobStore,
        auth: Option<&Credentials>,
        progress: impl FnMut(Progress),
    ) -> Result<LocalImage> {
        self.pull_with_options(reference, blobs, PullOptions { auth, ..PullOptions::default() }, progress)
    }

    /// Pull `reference` into `blobs` with explicit authentication and platform/Rosetta options.
    pub fn pull_with_options(
        &self,
        reference: &ImageReference,
        blobs: &BlobStore,
        opts: PullOptions<'_>,
        mut progress: impl FnMut(Progress),
    ) -> Result<LocalImage> {
        let requested_arch = opts.platform.map(normalize_platform).transpose()?;
        let canonical = normalize_registry(&reference.registry);
        let host = registry_host(&canonical);
        let repo = &reference.repository;
        let mut session = Session::new(&self.agent, canonical, host, repo.clone(), opts.auth, "pull");

        let tag_or_digest = reference.digest.clone().or_else(|| reference.tag.clone()).unwrap_or_else(|| "latest".into());
        let (mut bytes, mut media_type, mut digest) = session.manifest(&tag_or_digest)?;
        let mut manifest: Manifest = json(&bytes, "manifest")?;
        let media = manifest.media_type.clone().unwrap_or(media_type.clone());

        if INDEX_TYPES.contains(&media.as_str()) || manifest.manifests.is_some() {
            let entries = manifest.manifests.take().unwrap_or_default();
            let chosen = match requested_arch.as_deref() {
                Some(want) => pick_platform(&entries, want).ok_or_else(|| ImageError::NoPlatform {
                    reference: reference.display_name(),
                    arch: want.into(),
                    available: describe_platforms(&entries),
                })?,
                None => pick_platform(&entries, guest_arch())
                    .or_else(|| {
                        (opts.allow_amd64_fallback && guest_arch() != "amd64")
                            .then(|| pick_platform(&entries, "amd64"))
                            .flatten()
                    })
                    .ok_or_else(|| ImageError::NoPlatform {
                        reference: reference.display_name(),
                        arch: guest_arch().into(),
                        available: describe_platforms(&entries),
                    })?,
            };
            let chosen_digest = chosen.digest.clone();
            (bytes, media_type, digest) = session.manifest(&chosen_digest)?;
            manifest = json(&bytes, "manifest")?;
            let _ = media_type;
        }

        let config = manifest.config.ok_or_else(|| ImageError::Unsupported("manifest without a config".into()))?;
        let layers = manifest.layers.unwrap_or_default();
        let manifest_digest = Digest::parse(&digest)?;
        blobs.write_bytes(&manifest_digest, &bytes)?;

        let config_digest = Digest::parse(&config.digest)?;
        if !blobs.has(&config_digest) {
            session.blob(&config_digest, blobs, |_| {})?;
        }
        let image_config: ImageConfig =
            serde_json::from_slice(&blobs.read(&config_digest)?).map_err(|source| ImageError::Json { what: "image config".into(), source })?;
        let actual_arch = normalize_arch_label(&image_config.architecture);
        if let Some(want) = requested_arch.as_deref() {
            if actual_arch != want {
                return Err(ImageError::NoPlatform {
                    reference: reference.display_name(),
                    arch: want.into(),
                    available: format!("linux/{actual_arch}"),
                });
            }
        } else if actual_arch != guest_arch() && !(actual_arch == "amd64" && opts.allow_amd64_fallback) {
            return Err(ImageError::NoPlatform {
                reference: reference.display_name(),
                arch: guest_arch().into(),
                available: format!("linux/{actual_arch}"),
            });
        }

        let total_bytes = layers.iter().map(|l| l.size).sum();
        progress(Progress::Resolved { manifest: manifest_digest.to_string(), layers: layers.len(), total_bytes });

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
            architecture: actual_arch,
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
        let mut session = Session::new(&self.agent, canonical.clone(), host, String::new(), Some(creds), "pull");
        let response = session.get_path("/v2/", None)?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(ImageError::Registry(format!("{canonical} returned HTTP {status} for /v2/")));
        }
        Ok(canonical)
    }

    /// Push a local image (`image`) to `target` using OCI Distribution v1.1 endpoints.
    pub fn push_with_auth(
        &self,
        image: &LocalImage,
        target: &ImageReference,
        blobs: &BlobStore,
        auth: Option<&Credentials>,
        mut progress: impl FnMut(PushProgress),
    ) -> Result<PushResult> {
        if target.tag.is_none() && target.digest.is_some() {
            return Err(ImageError::Unsupported(
                "pushing by digest is not supported; include a tag (e.g. repo:tag)".into(),
            ));
        }
        let tag = target.tag.as_deref().unwrap_or("latest");
        let canonical = normalize_registry(&target.registry);
        let host = registry_host(&canonical);
        let repo = &target.repository;

        let manifest_digest = Digest::parse(&image.manifest_digest)?;
        let manifest_bytes = if blobs.has(&manifest_digest) {
            blobs.read(&manifest_digest)?
        } else {
            let config_digest = Digest::parse(&image.config_digest)?;
            let config_size = blobs.size(&config_digest).ok_or_else(|| {
                ImageError::Unsupported(format!("local image {} is missing config blob {}", image.reference, config_digest))
            })?;
            let layers_json: Vec<serde_json::Value> = image
                .layers
                .iter()
                .map(|l| serde_json::json!({ "mediaType": l.media_type, "digest": l.digest, "size": l.size }))
                .collect();
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {
                    "mediaType": "application/vnd.oci.image.config.v1+json",
                    "digest": image.config_digest,
                    "size": config_size,
                },
                "layers": layers_json,
            }))
            .map_err(|source| ImageError::Json { what: "manifest".into(), source })?
        };
        let manifest_parsed: Manifest = json(&manifest_bytes, "manifest")?;
        let manifest_media_type = manifest_parsed
            .media_type
            .unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".into());

        // Deduplicate blob digests while preserving layer-then-config upload order.
        let mut seen = HashSet::new();
        let mut blob_list: Vec<(Digest, u64)> = Vec::new();
        for raw_digest in image.layers.iter().map(|l| l.digest.as_str()).chain(std::iter::once(image.config_digest.as_str())) {
            let d = Digest::parse(raw_digest)?;
            if seen.insert(d.clone()) {
                let size = blobs.size(&d).ok_or_else(|| {
                    ImageError::Unsupported(format!("local image {} is missing blob {}", image.reference, d))
                })?;
                blob_list.push((d, size));
            }
        }

        let total_blob_bytes: u64 = blob_list.iter().map(|(_, s)| *s).sum();
        progress(PushProgress::Started {
            blobs: blob_list.len(),
            total_bytes: total_blob_bytes,
        });

        let mut session = Session::new(&self.agent, canonical, host, repo.clone(), auth, "pull,push");
        let mut blobs_pushed = 0u32;
        let mut blobs_skipped = 0u32;

        for (index, (digest, size)) in blob_list.iter().enumerate() {
            let size = *size;
            let digest_str = digest.to_string();
            let head_path = format!("/v2/{repo}/blobs/{digest}");
            let mut head_resp = session.head_path(&head_path)?;
            let head_status = head_resp.status().as_u16();
            if head_status == 200 {
                blobs_skipped += 1;
                progress(PushProgress::Blob {
                    index,
                    digest: digest_str,
                    done: size,
                    total: size,
                    skipped: true,
                });
                continue;
            }
            if head_status != 404 {
                let why = error_snippet(&mut head_resp);
                return Err(ImageError::Registry(format!(
                    "returned HTTP {head_status} checking blob {}{why}",
                    digest.short()
                )));
            }

            let upload_init_path = format!("/v2/{repo}/blobs/uploads/");
            let mut init_resp = session.post_empty_path(&upload_init_path)?;
            let init_status = init_resp.status().as_u16();
            match init_status {
                201 => {}
                202 => {
                    let location = raw_header(&init_resp, "location").ok_or_else(|| {
                        ImageError::Registry("upload initiation 202 response had no Location header".into())
                    })?;
                    let base_url = resolve_upload_url(session.scheme, &session.host, &location);
                    let upload_url = append_digest_query(&base_url, digest);
                    let blob_path = blobs.path(digest);
                    let mut on_chunk = |done: u64| {
                        progress(PushProgress::Blob {
                            index,
                            digest: digest_str.clone(),
                            done,
                            total: size,
                            skipped: false,
                        });
                    };
                    let mut put_resp = session.put_file_url(&upload_url, &blob_path, size, &mut on_chunk)?;
                    let put_status = put_resp.status().as_u16();
                    if put_status != 200 && put_status != 201 {
                        let why = error_snippet(&mut put_resp);
                        return Err(ImageError::Registry(format!(
                            "returned HTTP {put_status} uploading blob {}{why}",
                            digest.short()
                        )));
                    }
                }
                _ => {
                    let why = error_snippet(&mut init_resp);
                    return Err(ImageError::Registry(format!(
                        "returned HTTP {init_status} starting blob upload for {}{why}",
                        digest.short()
                    )));
                }
            }

            blobs_pushed += 1;
            progress(PushProgress::Blob {
                index,
                digest: digest_str,
                done: size,
                total: size,
                skipped: false,
            });
        }

        let manifest_path = format!("/v2/{repo}/manifests/{tag}");
        let mut man_resp = session.put_bytes_path(&manifest_path, &manifest_media_type, &manifest_bytes)?;
        let man_status = man_resp.status().as_u16();
        if man_status != 200 && man_status != 201 {
            let why = error_snippet(&mut man_resp);
            return Err(ImageError::Registry(format!(
                "returned HTTP {man_status} pushing manifest {tag}{why}"
            )));
        }
        let header_digest = raw_header(&man_resp, "docker-content-digest")
            .and_then(|d| Digest::parse(&d).ok())
            .map(|d| d.to_string());
        let final_digest = header_digest.unwrap_or_else(|| Digest::of(&manifest_bytes).to_string());
        let manifest_size = manifest_bytes.len() as u64;
        progress(PushProgress::Manifest {
            digest: final_digest.clone(),
            size: manifest_size,
        });

        Ok(PushResult {
            reference: target.to_string(),
            digest: final_digest,
            size_bytes: total_blob_bytes + manifest_size,
            blobs_pushed,
            blobs_skipped,
        })
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
    default_scope_action: &'static str,
}

impl<'a> Session<'a> {
    fn new(
        agent: &'a Agent,
        registry: String,
        host: String,
        repo: String,
        auth: Option<&'a Credentials>,
        default_scope_action: &'static str,
    ) -> Self {
        Self { agent, registry, host, scheme: "https", repo, auth, authorization: None, default_scope_action }
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

    fn call_once(
        &mut self,
        mut make: impl FnMut(&Agent, &str, &str, Option<&str>) -> std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<ureq::http::Response<ureq::Body>> {
        match make(self.agent, self.scheme, &self.host, self.authorization.as_deref()) {
            Ok(resp) => Ok(resp),
            // Standard Docker/OCI convenience: allow plain HTTP only for loopback registries (e.g. localhost:5000).
            Err(err) if self.scheme == "https" && is_loopback_host(&self.host) => {
                match make(self.agent, "http", &self.host, self.authorization.as_deref()) {
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

    /// Execute an HTTP request against this registry, answering a 401 Bearer or Basic challenge on the first attempt.
    fn call_with_auth(
        &mut self,
        mut make: impl FnMut(&Agent, &str, &str, Option<&str>) -> std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<ureq::http::Response<ureq::Body>> {
        for attempt in 0..2 {
            let response = self.call_once(&mut make)?;
            let status = response.status().as_u16();
            if status == 401 && attempt == 0 {
                self.handle_401(&response)?;
                continue;
            }
            if status == 401 || status == 403 {
                return Err(self.denied_error());
            }
            return Ok(response);
        }
        unreachable!("loop returns on the second attempt")
    }

    fn handle_401(&mut self, response: &ureq::http::Response<ureq::Body>) -> Result<()> {
        let challenge = raw_header(response, "www-authenticate")
            .ok_or_else(|| ImageError::Registry("asked for auth without a WWW-Authenticate challenge".into()))?;
        match parse_auth_challenge(&challenge) {
            Some(Challenge::Bearer(params)) => {
                let token = self.fetch_token(&params)?;
                self.authorization = Some(format!("Bearer {token}"));
                Ok(())
            }
            Some(Challenge::Basic) => {
                let creds = self.auth.ok_or_else(|| {
                    ImageError::Registry(format!("{} requires login (run `grav login {}`)", self.registry, self.registry))
                })?;
                self.authorization = Some(basic_header(creds));
                Ok(())
            }
            None => Err(ImageError::Unsupported(format!("auth challenge `{challenge}`"))),
        }
    }

    fn get_path(&mut self, path: &str, accept: Option<&str>) -> Result<ureq::http::Response<ureq::Body>> {
        self.call_with_auth(|agent, scheme, host, auth_val| {
            let url = format!("{scheme}://{host}{path}");
            let mut req = agent.get(&url);
            if let Some(accept) = accept {
                req = req.header("Accept", accept);
            }
            if let Some(auth_val) = auth_val {
                req = req.header("Authorization", auth_val);
            }
            req.call()
        })
    }

    fn head_path(&mut self, path: &str) -> Result<ureq::http::Response<ureq::Body>> {
        self.call_with_auth(|agent, scheme, host, auth_val| {
            let url = format!("{scheme}://{host}{path}");
            let mut req = agent.head(&url);
            if let Some(auth_val) = auth_val {
                req = req.header("Authorization", auth_val);
            }
            req.call()
        })
    }

    fn post_empty_path(&mut self, path: &str) -> Result<ureq::http::Response<ureq::Body>> {
        self.call_with_auth(|agent, scheme, host, auth_val| {
            let url = format!("{scheme}://{host}{path}");
            let mut req = agent.post(&url);
            if let Some(auth_val) = auth_val {
                req = req.header("Authorization", auth_val);
            }
            req.send_empty()
        })
    }

    fn put_bytes_path(&mut self, path: &str, content_type: &str, bytes: &[u8]) -> Result<ureq::http::Response<ureq::Body>> {
        self.call_with_auth(|agent, scheme, host, auth_val| {
            let url = format!("{scheme}://{host}{path}");
            let mut req = agent.put(&url).header("Content-Type", content_type);
            if let Some(auth_val) = auth_val {
                req = req.header("Authorization", auth_val);
            }
            req.send(bytes)
        })
    }

    fn put_file_url(
        &mut self,
        url: &str,
        file_path: &Path,
        size: u64,
        on_bytes: &mut impl FnMut(u64),
    ) -> Result<ureq::http::Response<ureq::Body>> {
        let same_host = url_same_host(url, &self.host);
        let size_str = size.to_string();
        for attempt in 0..2 {
            if attempt > 0 {
                on_bytes(0);
            }
            let file = File::open(file_path)?;
            let mut reader = ProgressReader::new(file, &mut *on_bytes);
            let mut req = self
                .agent
                .put(url)
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", &size_str);
            if same_host {
                if let Some(auth_val) = self.authorization.as_deref() {
                    req = req.header("Authorization", auth_val);
                }
            }
            let response = req.send(ureq::SendBody::from_reader(&mut reader)).map_err(http_err)?;
            let status = response.status().as_u16();
            if status == 401 && same_host && attempt == 0 {
                self.handle_401(&response)?;
                continue;
            }
            if status == 401 || status == 403 {
                return Err(self.denied_error());
            }
            return Ok(response);
        }
        unreachable!("loop returns on the second attempt")
    }

    fn denied_error(&self) -> ImageError {
        let is_push = self.default_scope_action.contains("push");
        if self.repo.is_empty() {
            match self.auth {
                Some(_) => ImageError::Registry(format!("authentication failed for {} (invalid username or password)", self.registry)),
                None => ImageError::Registry(format!("{} requires login (run `grav login {}`)", self.registry, self.registry)),
            }
        } else if is_push {
            match self.auth {
                Some(creds) => ImageError::Registry(format!(
                    "denied push to {} on {} as {} (check permissions or run `grav login {}`)",
                    self.repo, self.registry, creds.username, self.registry
                )),
                None => ImageError::Registry(format!(
                    "push to {} on {} requires login (run `grav login {}`)",
                    self.repo, self.registry, self.registry
                )),
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
            req = req.query("scope", format!("repository:{}:{}", self.repo, self.default_scope_action));
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

/// Download an HTTP/HTTPS URL directly to `dest`, returning `(bytes, sha256_digest)`.
pub fn fetch_url(url: &str, dest: &std::path::Path) -> Result<(u64, Digest)> {
    use std::io::{Read, Write};
    use sha2::{Digest as _, Sha256};

    let agent: Agent = Agent::config_builder()
        .http_status_as_error(false)
        .user_agent(concat!("graviton/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let response = agent.get(url).call().map_err(http_err)?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(ImageError::Registry(format!("GET {url} returned HTTP {status}")));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut reader = response.into_body().into_with_config().limit(u64::MAX).reader();
    let mut file = std::fs::File::create(dest)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    file.flush()?;
    let hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok((total, Digest::parse(&format!("sha256:{hex}"))?))
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

struct ProgressReader<R, F> {
    inner: R,
    sent: u64,
    on_bytes: F,
}

impl<R, F> ProgressReader<R, F> {
    fn new(inner: R, on_bytes: F) -> Self {
        Self { inner, sent: 0, on_bytes }
    }
}

impl<R: std::io::Read, F: FnMut(u64)> std::io::Read for ProgressReader<R, F> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.sent += n as u64;
            (self.on_bytes)(self.sent);
        }
        Ok(n)
    }
}

fn resolve_upload_url(scheme: &str, host: &str, location: &str) -> String {
    let trimmed = location.trim();
    if trimmed.len() >= 8 && (trimmed[..8].eq_ignore_ascii_case("https://") || trimmed[..7].eq_ignore_ascii_case("http://")) {
        trimmed.to_string()
    } else if trimmed.starts_with('/') {
        format!("{scheme}://{host}{trimmed}")
    } else {
        format!("{scheme}://{host}/{trimmed}")
    }
}

fn append_digest_query(url: &str, digest: &Digest) -> String {
    let sep = if url.contains('?') { '&' } else { '?' };
    let encoded = digest.as_str().replace(':', "%3A");
    format!("{url}{sep}digest={encoded}")
}

fn url_same_host(url: &str, expected_host: &str) -> bool {
    let after_scheme = if let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) {
        rest
    } else if url.starts_with('/') {
        return true;
    } else {
        url
    };
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    authority.eq_ignore_ascii_case(expected_host)
}

fn error_snippet(resp: &mut ureq::http::Response<ureq::Body>) -> String {
    match resp.body_mut().with_config().limit(4096).read_to_vec() {
        Ok(bytes) if !bytes.is_empty() => {
            let text = String::from_utf8_lossy(&bytes);
            let compact: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let trimmed: String = compact.chars().take(240).collect();
            if trimmed.is_empty() { String::new() } else { format!(": {trimmed}") }
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
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

    #[test]
    fn normalizes_platforms() {
        assert_eq!(normalize_platform("linux/amd64").unwrap(), "amd64");
        assert_eq!(normalize_platform("amd64").unwrap(), "amd64");
        assert_eq!(normalize_platform("x86_64").unwrap(), "amd64");
        assert_eq!(normalize_platform("linux/arm64/v8").unwrap(), "arm64");
        assert_eq!(normalize_platform("aarch64").unwrap(), "arm64");
        assert!(normalize_platform("windows/amd64").is_err());
        assert!(normalize_platform("linux/riscv64").is_err());
        assert!(normalize_platform("").is_err());
    }

    #[test]
    fn pulls_explicit_amd64_and_falls_back_when_allowed() {
        let amd_cfg = br#"{"architecture":"amd64","os":"linux","config":{"Cmd":["/bin/true"]}}"#.to_vec();
        let amd_cfg_d = Digest::of(&amd_cfg);
        let layer = b"amd64-layer".to_vec();
        let layer_d = Digest::of(&layer);
        let amd_man = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{amd_cfg_d}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{layer_d}","size":{}}}]}}"#,
            amd_cfg.len(),
            layer.len()
        )
        .into_bytes();
        let amd_man_d = Digest::of(&amd_man);
        let index = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{amd_man_d}","size":{},"platform":{{"architecture":"amd64","os":"linux"}}}}]}}"#,
            amd_man.len()
        )
        .into_bytes();
        let index_d = Digest::of(&index);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let amd_man_d_s = amd_man_d.to_string();
        let amd_cfg_d_s = amd_cfg_d.to_string();
        let layer_d_s = layer_d.to_string();
        let index_d_s = index_d.to_string();
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut req_line = String::new();
                if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
                    continue;
                }
                let path = req_line.split_whitespace().nth(1).unwrap_or("").to_string();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" || line == "\n" {
                        break;
                    }
                }
                let reply = |s: &mut std::net::TcpStream, ct: &str, d: &str, body: &[u8]| {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nDocker-Content-Digest: {d}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = s.write_all(head.as_bytes());
                    let _ = s.write_all(body);
                };
                if path == "/v2/only/amd64/manifests/latest" {
                    reply(&mut stream, "application/vnd.oci.image.index.v1+json", &index_d_s, &index);
                } else if path == format!("/v2/only/amd64/manifests/{amd_man_d_s}") {
                    reply(&mut stream, "application/vnd.oci.image.manifest.v1+json", &amd_man_d_s, &amd_man);
                } else if path == format!("/v2/only/amd64/blobs/{amd_cfg_d_s}") {
                    reply(&mut stream, "application/octet-stream", &amd_cfg_d_s, &amd_cfg);
                } else if path == format!("/v2/only/amd64/blobs/{layer_d_s}") {
                    reply(&mut stream, "application/octet-stream", &layer_d_s, &layer);
                } else {
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                }
            }
        });

        let tmp = std::env::temp_dir().join(format!("graviton-reg-amd64-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let blobs = BlobStore::new(tmp.clone());
        let reg = Registry::new();
        let img_ref = ImageReference::parse(&format!("{addr}/only/amd64:latest")).unwrap();

        if guest_arch() == "arm64" {
            // Without fallback, amd64-only image fails cleanly.
            assert!(matches!(
                reg.pull_with_options(&img_ref, &blobs, PullOptions::default(), |_| {}),
                Err(ImageError::NoPlatform { .. })
            ));
            // With Rosetta fallback enabled, amd64-only image pulls automatically.
            let img = reg
                .pull_with_options(&img_ref, &blobs, PullOptions { allow_amd64_fallback: true, ..PullOptions::default() }, |_| {})
                .unwrap();
            assert_eq!(img.architecture, "amd64");
        }
        // Explicit --platform linux/amd64 works unconditionally.
        let img = reg
            .pull_with_options(&img_ref, &blobs, PullOptions { platform: Some("linux/amd64"), ..PullOptions::default() }, |_| {})
            .unwrap();
        assert_eq!(img.architecture, "amd64");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolves_upload_urls_and_same_host_auth() {
        assert_eq!(
            resolve_upload_url("https", "ghcr.io", "/v2/u/app/blobs/uploads/123?_state=abc"),
            "https://ghcr.io/v2/u/app/blobs/uploads/123?_state=abc"
        );
        assert_eq!(
            resolve_upload_url("http", "127.0.0.1:5000", "http://127.0.0.1:5000/v2/app/blobs/uploads/1"),
            "http://127.0.0.1:5000/v2/app/blobs/uploads/1"
        );
        let d = Digest::of(b"blob");
        assert_eq!(
            append_digest_query("https://ghcr.io/v2/app/blobs/uploads/1?_state=abc", &d),
            format!("https://ghcr.io/v2/app/blobs/uploads/1?_state=abc&digest=sha256%3A{}", d.hex())
        );
        assert_eq!(
            append_digest_query("http://127.0.0.1:5000/v2/app/blobs/uploads/1", &d),
            format!("http://127.0.0.1:5000/v2/app/blobs/uploads/1?digest=sha256%3A{}", d.hex())
        );
        assert!(url_same_host("https://ghcr.io/v2/x", "ghcr.io"));
        assert!(url_same_host("/v2/x", "ghcr.io"));
        assert!(!url_same_host("https://s3.amazonaws.com/bucket/blob", "ghcr.io"));
    }

    fn spawn_stateful_push_registry(
        use_bearer: bool,
        expected_user: &'static str,
        expected_pass: &'static str,
    ) -> String {
        use std::collections::HashMap;
        use std::sync::Mutex;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let expected_basic = basic_header(&Credentials {
            username: expected_user.into(),
            password: expected_pass.into(),
        });
        let realm = format!("http://{addr}/token");
        type StoredBlobs = Arc<Mutex<HashMap<String, Vec<u8>>>>;
        type StoredManifests = Arc<Mutex<HashMap<String, (Vec<u8>, String)>>>;
        let blobs_store: StoredBlobs = Arc::new(Mutex::new(HashMap::new()));
        let manifests_store: StoredManifests = Arc::new(Mutex::new(HashMap::new()));

        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut req_line = String::new();
                if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
                    continue;
                }
                let mut parts = req_line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let raw_target = parts.next().unwrap_or("").to_string();
                let (path, query) = raw_target.split_once('?').unwrap_or((raw_target.as_str(), ""));

                let mut auth_header = None;
                let mut content_len = 0usize;
                let mut content_type = String::from("application/vnd.oci.image.manifest.v1+json");
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" || line == "\n" {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        let k = k.trim();
                        let v = v.trim();
                        if k.eq_ignore_ascii_case("authorization") {
                            auth_header = Some(v.to_string());
                        } else if k.eq_ignore_ascii_case("content-length") {
                            content_len = v.parse().unwrap_or(0);
                        } else if k.eq_ignore_ascii_case("content-type") {
                            content_type = v.to_string();
                        }
                    }
                }
                let mut req_body = vec![0u8; content_len];
                if content_len > 0 {
                    let _ = reader.read_exact(&mut req_body);
                }

                let write_resp = |s: &mut std::net::TcpStream, status: &str, headers: &[(&str, &str)], body: &[u8]| {
                    let mut head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
                    for (k, v) in headers {
                        head.push_str(&format!("{k}: {v}\r\n"));
                    }
                    head.push_str("\r\n");
                    let _ = s.write_all(head.as_bytes());
                    if !body.is_empty() {
                        let _ = s.write_all(body);
                    }
                };

                if path == "/token" {
                    if auth_header.as_deref() == Some(expected_basic.as_str()) {
                        let tok = if query.contains("push") { "tok-push" } else { "tok-pull" };
                        let json_body = format!(r#"{{"token":"{tok}"}}"#);
                        write_resp(&mut stream, "200 OK", &[("Content-Type", "application/json")], json_body.as_bytes());
                    } else {
                        write_resp(&mut stream, "401 Unauthorized", &[], b"bad credentials");
                    }
                    continue;
                }

                let needs_push = matches!(method.as_str(), "POST" | "PUT");
                let authorized = if use_bearer {
                    match auth_header.as_deref() {
                        Some("Bearer tok-push") => true,
                        Some("Bearer tok-pull") => !needs_push,
                        _ => false,
                    }
                } else {
                    auth_header.as_deref() == Some(expected_basic.as_str())
                };
                if !authorized {
                    let scope = if needs_push { "repository:team/app:pull,push" } else { "repository:team/app:pull" };
                    let challenge = if use_bearer {
                        format!(r#"Bearer realm="{realm}",service="mock",scope="{scope}""#)
                    } else {
                        r#"Basic realm="mock""#.into()
                    };
                    write_resp(&mut stream, "401 Unauthorized", &[("WWW-Authenticate", &challenge)], b"auth required");
                    continue;
                }

                if path == "/v2/" {
                    write_resp(&mut stream, "200 OK", &[], b"{}");
                } else if let Some(d) = path.strip_prefix("/v2/team/app/blobs/") {
                    if d == "uploads/" && method == "POST" {
                        write_resp(
                            &mut stream,
                            "202 Accepted",
                            &[("Location", "/v2/team/app/blobs/uploads/u1?_state=xyz")],
                            b"",
                        );
                    } else if d == "uploads/u1" && method == "PUT" {
                        let expected_d = query
                            .split('&')
                            .find_map(|p| p.strip_prefix("digest="))
                            .map(|v| v.replace("%3A", ":").replace("%3a", ":"))
                            .unwrap_or_default();
                        let actual = Digest::of(&req_body).to_string();
                        assert_eq!(actual, expected_d);
                        blobs_store.lock().unwrap().insert(actual.clone(), req_body);
                        write_resp(&mut stream, "201 Created", &[("Docker-Content-Digest", &actual)], b"");
                    } else if method == "HEAD" {
                        if blobs_store.lock().unwrap().contains_key(d) {
                            write_resp(&mut stream, "200 OK", &[], b"");
                        } else {
                            write_resp(&mut stream, "404 Not Found", &[], b"");
                        }
                    } else if method == "GET" {
                        if let Some(data) = blobs_store.lock().unwrap().get(d).cloned() {
                            write_resp(&mut stream, "200 OK", &[], &data);
                        } else {
                            write_resp(&mut stream, "404 Not Found", &[], b"missing");
                        }
                    } else {
                        write_resp(&mut stream, "404 Not Found", &[], b"bad blob route");
                    }
                } else if let Some(mref) = path.strip_prefix("/v2/team/app/manifests/") {
                    if method == "PUT" {
                        let md = Digest::of(&req_body).to_string();
                        manifests_store.lock().unwrap().insert(mref.to_string(), (req_body, content_type));
                        write_resp(&mut stream, "201 Created", &[("Docker-Content-Digest", &md)], b"");
                    } else if method == "GET" {
                        if let Some((data, ct)) = manifests_store.lock().unwrap().get(mref).cloned() {
                            let md = Digest::of(&data).to_string();
                            write_resp(
                                &mut stream,
                                "200 OK",
                                &[("Content-Type", &ct), ("Docker-Content-Digest", &md)],
                                &data,
                            );
                        } else {
                            write_resp(&mut stream, "404 Not Found", &[], b"missing manifest");
                        }
                    }
                } else {
                    write_resp(&mut stream, "404 Not Found", &[], b"not found");
                }
            }
        });
        addr
    }

    fn make_local_test_image(blobs: &BlobStore, reference: &str, layer_bytes: &[u8]) -> (LocalImage, Vec<u8>) {
        let cfg_bytes = format!(r#"{{"architecture":"{}","os":"linux","config":{{"Cmd":["/bin/echo","ok"]}}}}"#, guest_arch()).into_bytes();
        let cfg_digest = Digest::of(&cfg_bytes);
        blobs.write_bytes(&cfg_digest, &cfg_bytes).unwrap();
        let layer_digest = Digest::of(layer_bytes);
        blobs.write_bytes(&layer_digest, layer_bytes).unwrap();
        let manifest_bytes = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{cfg_digest}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{layer_digest}","size":{}}}]}}"#,
            cfg_bytes.len(),
            layer_bytes.len()
        )
        .into_bytes();
        let man_digest = Digest::of(&manifest_bytes);
        blobs.write_bytes(&man_digest, &manifest_bytes).unwrap();
        let img = LocalImage {
            reference: reference.into(),
            manifest_digest: man_digest.to_string(),
            config_digest: cfg_digest.to_string(),
            layers: vec![Layer {
                digest: layer_digest.to_string(),
                size: layer_bytes.len() as u64,
                media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
            }],
            architecture: guest_arch().into(),
            pulled_at: 1,
        };
        (img, cfg_bytes)
    }

    #[test]
    fn bearer_auth_push_skips_existing_and_roundtrips_pull() {
        let addr = spawn_stateful_push_registry(true, "alice", "token_123");
        let reg = Registry::new();
        let creds = Credentials { username: "alice".into(), password: "token_123".into() };

        let base = std::env::temp_dir().join(format!("graviton-push-bearer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src_blobs = BlobStore::new(base.join("src"));
        let dst_blobs = BlobStore::new(base.join("dst"));

        let target = ImageReference::parse(&format!("{addr}/team/app:1.0")).unwrap();
        let layer_data = b"gzipped-layer-payload-hello".to_vec();
        let (local, cfg_bytes) = make_local_test_image(&src_blobs, &target.to_string(), &layer_data);

        // Anonymous push fails cleanly with grav login hint.
        let err = reg.push_with_auth(&local, &target, &src_blobs, None, |_| {}).unwrap_err();
        assert!(err.to_string().contains("grav login"), "got: {err}");

        // First push upgrades pull->push scope cleanly and uploads both layer + config.
        let mut events1 = Vec::new();
        let res1 = reg
            .push_with_auth(&local, &target, &src_blobs, Some(&creds), |p| events1.push(p))
            .unwrap();
        assert_eq!(res1.blobs_pushed, 2);
        assert_eq!(res1.blobs_skipped, 0);
        assert!(events1.iter().any(|p| matches!(p, PushProgress::Started { blobs: 2, .. })));
        assert!(events1.iter().any(|p| matches!(p, PushProgress::Blob { skipped: false, .. })));
        assert!(events1.iter().any(|p| matches!(p, PushProgress::Manifest { .. })));

        // Second push detects existing blobs via HEAD and skips them all.
        let mut events2 = Vec::new();
        let res2 = reg
            .push_with_auth(&local, &target, &src_blobs, Some(&creds), |p| events2.push(p))
            .unwrap();
        assert_eq!(res2.blobs_pushed, 0);
        assert_eq!(res2.blobs_skipped, 2);
        assert_eq!(res2.digest, res1.digest);
        assert!(events2.iter().any(|p| matches!(p, PushProgress::Blob { skipped: true, .. })));

        // Pulling into a clean BlobStore recovers identical config & layer bytes.
        let pulled = reg.pull_with_auth(&target, &dst_blobs, Some(&creds), |_| {}).unwrap();
        assert_eq!(pulled.config_digest, local.config_digest);
        assert_eq!(dst_blobs.read(&Digest::parse(&pulled.config_digest).unwrap()).unwrap(), cfg_bytes);
        assert_eq!(dst_blobs.read(&Digest::parse(&pulled.layers[0].digest).unwrap()).unwrap(), layer_data);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn basic_auth_push_and_rejects_digest_only_target() {
        let addr = spawn_stateful_push_registry(false, "bob", "hunter2");
        let reg = Registry::new();
        let creds = Credentials { username: "bob".into(), password: "hunter2".into() };

        let base = std::env::temp_dir().join(format!("graviton-push-basic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let blobs = BlobStore::new(&base);

        let target = ImageReference::parse(&format!("{addr}/team/app:v2")).unwrap();
        let (local, _) = make_local_test_image(&blobs, &target.to_string(), b"basic-layer");

        let res = reg.push_with_auth(&local, &target, &blobs, Some(&creds), |_| {}).unwrap();
        assert_eq!(res.blobs_pushed, 2);

        let digest_target = ImageReference::parse(&format!("{addr}/team/app@{}", local.manifest_digest)).unwrap();
        assert!(matches!(
            reg.push_with_auth(&local, &digest_target, &blobs, Some(&creds), |_| {}),
            Err(ImageError::Unsupported(_))
        ));

        let _ = std::fs::remove_dir_all(&base);
    }
}
