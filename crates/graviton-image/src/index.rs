use std::collections::BTreeMap;
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
}

impl LocalImage {
    pub fn size(&self) -> u64 {
        self.layers.iter().map(|l| l.size).sum()
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
        ImageSummary {
            reference: self.reference.clone(),
            repository,
            tag,
            digest: self.manifest_digest.clone(),
            size_bytes: self.size(),
            architecture: self.architecture.clone(),
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
            })
            .unwrap();
        let reloaded = ImageIndex::load(&path).unwrap();
        let img = reloaded.get("alpine").expect("found by short name");
        assert_eq!(img.size(), 3_600_000);
        let s = img.summary(2);
        assert_eq!((s.repository.as_str(), s.tag.as_str(), s.used_by), ("alpine", "latest", 2));
        let _ = fs::remove_dir_all(dir);
    }
}
