//! Images for Graviton: pulling from OCI registries, storing blobs by digest,
//! and remembering which images are available locally.
//!
//! Layers are kept exactly as the registry serves them (usually gzip tars).
//! They are unpacked inside the guest VM by the agent, where Linux keeps file
//! ownership, modes and case-sensitive names intact.

mod blobs;
mod config;
mod index;
mod registry;

pub use blobs::{BlobStore, Digest};
pub use config::{ImageConfig, RuntimeConfig};
pub use index::{ImageIndex, LocalImage, Layer};
pub use registry::{Progress, Registry};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ImageError {
    #[error(transparent)]
    Reference(#[from] graviton_core::ReferenceError),
    #[error("registry {0}")]
    Registry(String),
    #[error("{0} not found in the registry")]
    NotFound(String),
    #[error("no linux/{arch} image for {reference} (available: {available})")]
    NoPlatform { reference: String, arch: String, available: String },
    #[error("digest mismatch for {expected}: got {actual}")]
    DigestMismatch { expected: String, actual: String },
    #[error("unsupported {0}")]
    Unsupported(String),
    #[error("image {0} isn't pulled yet (try `grav pull {0}`)")]
    NotPulled(String),
    #[error("bad JSON from {what}: {source}")]
    Json { what: String, source: serde_json::Error },
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = ImageError> = std::result::Result<T, E>;

/// The guest architecture images are pulled for.
pub fn guest_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}
