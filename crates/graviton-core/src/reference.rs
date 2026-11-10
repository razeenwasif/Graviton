//! OCI image references (`nginx`, `ghcr.io/org/app:1.2`, `redis@sha256:…`).
//!
//! Normalisation follows Docker's rules so references behave the way users
//! expect: a bare name lives on Docker Hub under `library/`, and the tag
//! defaults to `latest` when there is neither a tag nor a digest.

use std::fmt;
use std::str::FromStr;

use thiserror::Error;

pub const DEFAULT_REGISTRY: &str = "docker.io";
pub const DEFAULT_TAG: &str = "latest";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImageReference {
    pub registry: String,
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReferenceError {
    #[error("image reference is empty")]
    Empty,
    #[error("invalid repository name `{0}`: use lowercase letters, digits, and . _ - separators")]
    InvalidRepository(String),
    #[error("invalid tag `{0}`")]
    InvalidTag(String),
    #[error("invalid digest `{0}`: expected <algorithm>:<hex>")]
    InvalidDigest(String),
}

impl ImageReference {
    pub fn parse(input: &str) -> Result<Self, ReferenceError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(ReferenceError::Empty);
        }

        let (name_and_tag, digest) = match input.split_once('@') {
            Some((n, d)) => {
                validate_digest(d)?;
                (n, Some(d.to_string()))
            }
            None => (input, None),
        };

        // A tag is a `:` after the last `/` (a `:` before it is a registry port).
        let last_slash = name_and_tag.rfind('/').map_or(0, |i| i + 1);
        let (name, tag) = match name_and_tag[last_slash..].rfind(':') {
            Some(i) => {
                let split = last_slash + i;
                let tag = &name_and_tag[split + 1..];
                validate_tag(tag)?;
                (&name_and_tag[..split], Some(tag.to_string()))
            }
            None => (name_and_tag, None),
        };

        let (registry, mut repository) = match name.split_once('/') {
            Some((first, rest))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first.to_string(), rest.to_string())
            }
            _ => (DEFAULT_REGISTRY.to_string(), name.to_string()),
        };

        if registry == DEFAULT_REGISTRY && !repository.contains('/') {
            repository = format!("library/{repository}");
        }
        validate_repository(&repository)?;

        let tag = match (&tag, &digest) {
            (None, None) => Some(DEFAULT_TAG.to_string()),
            _ => tag,
        };

        Ok(Self { registry, repository, tag, digest })
    }

    /// Short form for display: `postgres:17-alpine`, `ghcr.io/org/app:1.2`.
    pub fn display_name(&self) -> String {
        let repo = if self.registry == DEFAULT_REGISTRY {
            self.repository.strip_prefix("library/").unwrap_or(&self.repository).to_string()
        } else {
            format!("{}/{}", self.registry, self.repository)
        };
        let mut s = repo;
        if let Some(tag) = &self.tag {
            s.push(':');
            s.push_str(tag);
        }
        if let Some(digest) = &self.digest {
            s.push('@');
            s.push_str(digest);
        }
        s
    }
}

impl fmt::Display for ImageReference {
    /// Fully qualified form: `docker.io/library/nginx:latest`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.registry, self.repository)?;
        if let Some(tag) = &self.tag {
            write!(f, ":{tag}")?;
        }
        if let Some(digest) = &self.digest {
            write!(f, "@{digest}")?;
        }
        Ok(())
    }
}

impl FromStr for ImageReference {
    type Err = ReferenceError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

fn validate_repository(repo: &str) -> Result<(), ReferenceError> {
    let ok = !repo.is_empty()
        && repo.split('/').all(|part| {
            !part.is_empty()
                && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
                && part.as_bytes()[0].is_ascii_alphanumeric()
        });
    if ok { Ok(()) } else { Err(ReferenceError::InvalidRepository(repo.to_string())) }
}

fn validate_tag(tag: &str) -> Result<(), ReferenceError> {
    let ok = !tag.is_empty()
        && tag.len() <= 128
        && (tag.as_bytes()[0].is_ascii_alphanumeric() || tag.starts_with('_'))
        && tag.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    if ok { Ok(()) } else { Err(ReferenceError::InvalidTag(tag.to_string())) }
}

fn validate_digest(digest: &str) -> Result<(), ReferenceError> {
    let ok = match digest.split_once(':') {
        Some((alg, hex)) => {
            !alg.is_empty()
                && alg.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                && hex.len() >= 32
                && hex.bytes().all(|b| b.is_ascii_hexdigit())
        }
        None => false,
    };
    if ok { Ok(()) } else { Err(ReferenceError::InvalidDigest(digest.to_string())) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> ImageReference {
        ImageReference::parse(s).unwrap()
    }

    #[test]
    fn bare_name_defaults_to_hub_library_latest() {
        let r = p("nginx");
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "library/nginx");
        assert_eq!(r.tag.as_deref(), Some("latest"));
        assert_eq!(r.to_string(), "docker.io/library/nginx:latest");
        assert_eq!(r.display_name(), "nginx:latest");
    }

    #[test]
    fn tag_and_namespace() {
        let r = p("minio/minio:RELEASE.2025-01-01");
        assert_eq!(r.repository, "minio/minio");
        assert_eq!(r.tag.as_deref(), Some("RELEASE.2025-01-01"));
    }

    #[test]
    fn custom_registry_with_port() {
        let r = p("localhost:5000/team/api:0.4.2");
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "team/api");
        assert_eq!(r.tag.as_deref(), Some("0.4.2"));

        let r = p("ghcr.io/astral-sh/uv");
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.display_name(), "ghcr.io/astral-sh/uv:latest");
    }

    #[test]
    fn digest_without_tag_has_no_default_tag() {
        let d = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let r = p(&format!("redis@{d}"));
        assert_eq!(r.tag, None);
        assert_eq!(r.digest.as_deref(), Some(d));
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(ImageReference::parse("  "), Err(ReferenceError::Empty));
        assert!(matches!(ImageReference::parse("Nginx"), Err(ReferenceError::InvalidRepository(_))));
        assert!(matches!(ImageReference::parse("nginx:"), Err(ReferenceError::InvalidTag(_))));
        assert!(matches!(ImageReference::parse("nginx@sha256:xyz"), Err(ReferenceError::InvalidDigest(_))));
    }
}
