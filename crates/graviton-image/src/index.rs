use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use graviton_proto::ImageSummary;
use serde::{Deserialize, Serialize};

use crate::{BlobStore, Digest, ImageConfig, ImageError, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layer {
    pub digest: String,
    pub size: u64,
    pub media_type: String,
}

/// One platform-specific manifest inside a multi-arch OCI image index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageVariant {
    pub architecture: String,
    pub manifest_digest: String,
    pub config_digest: String,
    pub layers: Vec<Layer>,
}

/// An image available locally: its manifest, config and layers are all in the blob store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalImage {
    /// Fully qualified, e.g. `docker.io/library/alpine:latest`.
    pub reference: String,
    pub manifest_digest: String,
    pub config_digest: String,
    pub layers: Vec<Layer>,
    pub architecture: String,
    /// Unix seconds.
    pub pulled_at: u64,
    /// Non-empty when built as a multi-arch OCI image index (top-level fields hold the primary variant).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<ImageVariant>,
}

impl LocalImage {
    pub fn is_multi_arch(&self) -> bool {
        self.variants.len() > 1
    }

    pub fn architectures(&self) -> Vec<String> {
        if self.variants.is_empty() {
            vec![self.architecture.clone()]
        } else {
            self.variants.iter().map(|v| v.architecture.clone()).collect()
        }
    }

    /// Resolve a single-architecture view of this image (`None` if not present).
    pub fn for_arch(&self, arch: &str) -> Option<LocalImage> {
        if self.variants.is_empty() {
            return (self.architecture == arch).then(|| self.clone());
        }
        self.variants.iter().find(|v| v.architecture == arch).map(|v| LocalImage {
            reference: self.reference.clone(),
            manifest_digest: v.manifest_digest.clone(),
            config_digest: v.config_digest.clone(),
            layers: v.layers.clone(),
            architecture: v.architecture.clone(),
            pulled_at: self.pulled_at,
            variants: Vec::new(),
        })
    }

    /// Every unique blob digest referenced by this image (index, manifests, configs, layers).
    pub fn all_blob_digests(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let mut push = |d: &str| {
            if !d.is_empty() && seen.insert(d.to_string()) {
                out.push(d.to_string());
            }
        };
        push(&self.manifest_digest);
        push(&self.config_digest);
        for l in &self.layers {
            push(&l.digest);
        }
        for v in &self.variants {
            push(&v.manifest_digest);
            push(&v.config_digest);
            for l in &v.layers {
                push(&l.digest);
            }
        }
        out
    }

    pub fn size(&self) -> u64 {
        if self.variants.is_empty() {
            return self.layers.iter().map(|l| l.size).sum();
        }
        let mut seen = HashSet::new();
        let mut total = 0u64;
        for v in &self.variants {
            for l in &v.layers {
                if seen.insert(l.digest.as_str()) {
                    total += l.size;
                }
            }
        }
        total
    }

    pub fn config(&self, blobs: &BlobStore) -> Result<ImageConfig> {
        let bytes = blobs.read(&Digest::parse(&self.config_digest)?)?;
        serde_json::from_slice(&bytes).map_err(|source| ImageError::Json { what: "image config".into(), source })
    }

    pub fn summary(&self, used_by: u32) -> ImageSummary {
        let display = graviton_core::ImageReference::parse(&self.reference)
            .map(|r| r.display_name())
            .unwrap_or_else(|_| self.reference.clone());
        let (repository, tag) = match display.rsplit_once(':') {
            Some((repo, tag)) if !tag.contains('/') => (repo.to_string(), tag.to_string()),
            _ => (display.clone(), String::new()),
        };
        let architecture = if self.variants.len() > 1 {
            self.architectures().join(",")
        } else {
            self.architecture.clone()
        };
        ImageSummary {
            reference: self.reference.clone(),
            repository,
            tag,
            digest: self.manifest_digest.clone(),
            size_bytes: self.size(),
            architecture,
            used_by,
        }
    }
}

/// `images.json`: fully qualified reference → local image.
#[derive(Debug)]
pub struct ImageIndex {
    path: PathBuf,
    images: BTreeMap<String, LocalImage>,
}

impl ImageIndex {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let images = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|source| ImageError::Json { what: path.display().to_string(), source })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path, images })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn images(&self) -> impl Iterator<Item = &LocalImage> {
        self.images.values()
    }

    /// Look up by any spelling of the reference (`alpine`, `docker.io/library/alpine:latest`).
    pub fn get(&self, reference: &str) -> Option<&LocalImage> {
        let key = graviton_core::ImageReference::parse(reference).ok()?.to_string();
        self.images.get(&key)
    }

    pub fn insert(&mut self, image: LocalImage) -> Result<()> {
        self.images.insert(image.reference.clone(), image);
        self.save()
    }

    pub fn remove(&mut self, reference: &str) -> Result<Option<LocalImage>> {
        let key = graviton_core::ImageReference::parse(reference)?.to_string();
        let removed = self.images.remove(&key);
        self.save()?;
        Ok(removed)
    }

    fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&self.images).expect("index serialises"))?;
        fs::rename(tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_round_trip_and_lookup() {
        let dir = std::env::temp_dir().join(format!("graviton-index-test-{}", std::process::id()));
        let path = dir.join("images.json");
        let mut index = ImageIndex::load(&path).unwrap();
        index
            .insert(LocalImage {
                reference: "docker.io/library/alpine:latest".into(),
                manifest_digest: "sha256:aa".into(),
                config_digest: "sha256:bb".into(),
                layers: vec![Layer { digest: "sha256:cc".into(), size: 3_600_000, media_type: "x".into() }],
                architecture: "arm64".into(),
                pulled_at: 1,
                variants: vec![],
            })
            .unwrap();
        let reloaded = ImageIndex::load(&path).unwrap();
        let img = reloaded.get("alpine").expect("found by short name");
        assert_eq!(img.size(), 3_600_000);
        assert_eq!(img.for_arch("arm64").unwrap().config_digest, "sha256:bb");
        assert!(img.for_arch("amd64").is_none());
        let s = img.summary(2);
        assert_eq!((s.repository.as_str(), s.tag.as_str(), s.used_by), ("alpine", "latest", 2));

        // Multi-arch index entry round-trip & size deduplication.
        let shared = Layer { digest: "sha256:shared".into(), size: 1000, media_type: "x".into() };
        let multi = LocalImage {
            reference: "docker.io/library/multi:1".into(),
            manifest_digest: "sha256:idx".into(),
            config_digest: "sha256:cfg-arm".into(),
            layers: vec![shared.clone()],
            architecture: "arm64".into(),
            pulled_at: 2,
            variants: vec![
                ImageVariant {
                    architecture: "arm64".into(),
                    manifest_digest: "sha256:man-arm".into(),
                    config_digest: "sha256:cfg-arm".into(),
                    layers: vec![shared.clone()],
                },
                ImageVariant {
                    architecture: "amd64".into(),
                    manifest_digest: "sha256:man-amd".into(),
                    config_digest: "sha256:cfg-amd".into(),
                    layers: vec![shared, Layer { digest: "sha256:amd-only".into(), size: 500, media_type: "x".into() }],
                },
            ],
        };
        index.insert(multi).unwrap();
        let reloaded = ImageIndex::load(&path).unwrap();
        let m = reloaded.get("multi:1").unwrap();
        assert!(m.is_multi_arch());
        assert_eq!(m.architectures(), vec!["arm64", "amd64"]);
        assert_eq!(m.size(), 1500);
        assert_eq!(m.summary(1).architecture, "arm64,amd64");
        assert_eq!(m.for_arch("amd64").unwrap().manifest_digest, "sha256:man-amd");
        assert_eq!(m.all_blob_digests().len(), 7);
        let _ = fs::remove_dir_all(dir);
    }
}
