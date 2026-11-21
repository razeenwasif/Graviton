//! A small OCI distribution client: anonymous bearer-token auth, manifest
//! negotiation (OCI and Docker v2, single and multi-platform), and verified
//! blob downloads.

use std::time::{SystemTime, UNIX_EPOCH};

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

    /// Pull `reference` for the guest's architecture into `blobs`.
    pub fn pull(&self, reference: &ImageReference, blobs: &BlobStore, mut progress: impl FnMut(Progress)) -> Result<LocalImage> {
        let host = registry_host(&reference.registry);
        let repo = &reference.repository;
        let mut session = Session { agent: &self.agent, host: host.clone(), repo: repo.clone(), token: None };

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
            if layer.media_type.contains("zstd") {
                return Err(ImageError::Unsupported(format!("zstd-compressed layer {} (gzip and plain tar are supported)", layer.digest)));
            }
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
}

struct Session<'a> {
    agent: &'a Agent,
    host: String,
    repo: String,
    token: Option<String>,
}

impl Session<'_> {
    /// Fetch a manifest; returns (bytes, media type, digest).
    fn manifest(&mut self, reference: &str) -> Result<(Vec<u8>, String, String)> {
        let url = format!("https://{}/v2/{}/manifests/{}", self.host, self.repo, reference);
        let mut response = self.get(&url, Some(MANIFEST_ACCEPT))?;
        let status = response.status().as_u16();
        if status == 404 {
            return Err(ImageError::NotFound(format!("{}:{}", self.repo, reference)));
        }
        if status != 200 {
            return Err(ImageError::Registry(format!("returned HTTP {status} for {url}")));
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
        let url = format!("https://{}/v2/{}/blobs/{}", self.host, self.repo, digest);
        let response = self.get(&url, None)?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(ImageError::Registry(format!("returned HTTP {status} for blob {}", digest.short())));
        }
        let reader = response.into_body().into_with_config().limit(u64::MAX).reader();
        blobs.write_verified(digest, reader, on_bytes)
    }

    /// GET with bearer auth, fetching an anonymous token on the first 401.
    fn get(&mut self, url: &str, accept: Option<&str>) -> Result<ureq::http::Response<ureq::Body>> {
        for attempt in 0..2 {
            let mut req = self.agent.get(url);
            if let Some(accept) = accept {
                req = req.header("Accept", accept);
            }
            if let Some(token) = &self.token {
                req = req.header("Authorization", &format!("Bearer {token}"));
            }
            let response = req.call().map_err(http_err)?;
            if response.status().as_u16() == 401 && attempt == 0 {
                let challenge = header(&response, "www-authenticate")
                    .ok_or_else(|| ImageError::Registry("asked for auth without a WWW-Authenticate challenge".into()))?;
                self.token = Some(self.fetch_token(&challenge)?);
                continue;
            }
            if response.status().as_u16() == 401 {
                return Err(ImageError::Registry(format!(
                    "denied access to {} (private images need a login, which Graviton doesn't support yet)",
                    self.repo
                )));
            }
            return Ok(response);
        }
        unreachable!("loop returns on the second attempt")
    }

    fn fetch_token(&self, challenge: &str) -> Result<String> {
        let params = parse_challenge(challenge)
            .ok_or_else(|| ImageError::Unsupported(format!("auth challenge `{challenge}`")))?;
        let realm = params.iter().find(|(k, _)| k == "realm").map(|(_, v)| v.clone())
            .ok_or_else(|| ImageError::Registry("auth challenge has no realm".into()))?;
        let mut req = self.agent.get(&realm);
        for (k, v) in &params {
            if k == "service" || k == "scope" {
                req = req.query(k, v);
            }
        }
        if !params.iter().any(|(k, _)| k == "scope") {
            req = req.query("scope", format!("repository:{}:pull", self.repo));
        }
        let mut response = req.call().map_err(http_err)?;
        if response.status().as_u16() != 200 {
            return Err(ImageError::Registry(format!("token service returned HTTP {}", response.status().as_u16())));
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

/// Docker Hub's API lives on a different host than its name.
fn registry_host(registry: &str) -> String {
    match registry {
        "docker.io" | "index.docker.io" => "registry-1.docker.io".into(),
        other => other.into(),
    }
}

/// `Bearer realm="…",service="…",scope="…"` → [(realm, …), (service, …), (scope, …)]
fn parse_challenge(challenge: &str) -> Option<Vec<(String, String)>> {
    let rest = challenge.trim().strip_prefix("Bearer ").or_else(|| challenge.trim().strip_prefix("bearer "))?;
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
    Some(out)
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

fn json<T: serde::de::DeserializeOwned>(bytes: &[u8], what: &str) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|source| ImageError::Json { what: what.into(), source })
}

fn http_err(err: ureq::Error) -> ImageError {
    ImageError::Registry(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bearer_challenges() {
        let c = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull""#;
        let p = parse_challenge(c).unwrap();
        assert_eq!(p[0], ("realm".into(), "https://auth.docker.io/token".into()));
        assert_eq!(p[1], ("service".into(), "registry.docker.io".into()));
        assert_eq!(p[2], ("scope".into(), "repository:library/alpine:pull".into()));
        assert!(parse_challenge("Basic realm=x").is_none());
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

    #[test]
    fn docker_hub_host() {
        assert_eq!(registry_host("docker.io"), "registry-1.docker.io");
        assert_eq!(registry_host("ghcr.io"), "ghcr.io");
    }
}
