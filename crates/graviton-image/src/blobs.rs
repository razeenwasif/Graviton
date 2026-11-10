use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::{ImageError, Result};

/// A `sha256:<hex>` content digest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest(String);

impl Digest {
    pub fn parse(s: &str) -> Result<Self> {
        match s.split_once(':') {
            Some(("sha256", hex)) if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) => {
                Ok(Self(s.to_string()))
            }
            _ => Err(ImageError::Unsupported(format!("digest `{s}` (only sha256 is supported)"))),
        }
    }

    pub fn of(bytes: &[u8]) -> Self {
        Self(format!("sha256:{}", hex(&Sha256::digest(bytes))))
    }

    pub fn hex(&self) -> &str {
        &self.0["sha256:".len()..]
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `sha256:abcdef012345`
    pub fn short(&self) -> &str {
        &self.0[..("sha256:".len() + 12).min(self.0.len())]
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Content-addressed blobs at `<root>/sha256/<hex>`. This directory is shared
/// read-only into guest VMs (virtiofs tag `blobs`).
#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, digest: &Digest) -> PathBuf {
        self.root.join("sha256").join(digest.hex())
    }

    pub fn has(&self, digest: &Digest) -> bool {
        self.path(digest).is_file()
    }

    pub fn read(&self, digest: &Digest) -> Result<Vec<u8>> {
        Ok(fs::read(self.path(digest))?)
    }

    pub fn size(&self, digest: &Digest) -> Option<u64> {
        fs::metadata(self.path(digest)).ok().map(|m| m.len())
    }

    /// Stream `reader` into the store, verifying it hashes to `expected`.
    /// `on_bytes` is called with the running byte count.
    pub fn write_verified(&self, expected: &Digest, mut reader: impl Read, mut on_bytes: impl FnMut(u64)) -> Result<u64> {
        let dir = self.root.join("sha256");
        fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!(".{}.partial", expected.hex()));
        let mut file = File::create(&tmp)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 256 * 1024];
        let mut total = 0u64;
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    let _ = fs::remove_file(&tmp);
                    return Err(e.into());
                }
            };
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n])?;
            total += n as u64;
            on_bytes(total);
        }
        file.sync_all()?;
        let actual = Digest(format!("sha256:{}", hex(&hasher.finalize())));
        if &actual != expected {
            let _ = fs::remove_file(&tmp);
            return Err(ImageError::DigestMismatch { expected: expected.to_string(), actual: actual.to_string() });
        }
        fs::rename(&tmp, self.path(expected))?;
        Ok(total)
    }

    /// Store bytes already in memory.
    pub fn write_bytes(&self, expected: &Digest, bytes: &[u8]) -> Result<()> {
        self.write_verified(expected, bytes, |_| {})?;
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_parse_and_store() {
        let d = Digest::of(b"hello");
        assert_eq!(d.as_str(), "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        assert_eq!(Digest::parse(d.as_str()).unwrap(), d);
        assert!(Digest::parse("md5:abc").is_err());
        assert!(Digest::parse("sha256:ABC").is_err());

        let dir = std::env::temp_dir().join(format!("graviton-blobs-test-{}", std::process::id()));
        let store = BlobStore::new(&dir);
        store.write_bytes(&d, b"hello").unwrap();
        assert!(store.has(&d));
        assert_eq!(store.read(&d).unwrap(), b"hello");

        let wrong = Digest::of(b"other");
        assert!(matches!(store.write_bytes(&wrong, b"hello"), Err(ImageError::DigestMismatch { .. })));
        assert!(!store.has(&wrong));
        let _ = fs::remove_dir_all(dir);
    }
}
