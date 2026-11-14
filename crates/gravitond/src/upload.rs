//! Build folders sent from another Mac (`grav build --host`, the Mac app).
//!
//! The client sends the folder as a tar (gzipped or not) right after its
//! build request. gravitond saves it, unpacks it into a temporary folder
//! under `builds/`, builds from there and deletes it afterwards.

use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use graviton_core::paths;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// The most a client may send (the folder, compressed).
pub const MAX_UPLOAD: u64 = 8 << 30;

/// A received build folder, deleted when dropped.
pub struct Upload {
    pub dir: PathBuf,
    pub bytes: u64,
    pub files: u64,
}

impl Drop for Upload {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Read `size` bytes of tar from `reader` and unpack them into a fresh folder.
pub async fn receive(reader: &mut (impl AsyncRead + Unpin), size: u64) -> Result<Upload> {
    if size > MAX_UPLOAD {
        bail!("the build folder is {} GB; the limit is {} GB (check .dockerignore)", size >> 30, MAX_UPLOAD >> 30);
    }
    let id = crate::runtime::new_id();
    let root = paths::support_dir().join("builds");
    tokio::fs::create_dir_all(&root).await?;
    let archive = root.join(format!("upload-{id}.tar"));
    let mut file = tokio::fs::File::create(&archive).await?;
    let mut limited = reader.take(size);
    let copied = tokio::io::copy(&mut limited, &mut file).await.context("receiving the build folder")?;
    file.flush().await?;
    drop(file);
    if copied != size {
        let _ = tokio::fs::remove_file(&archive).await;
        bail!("the build folder upload ended early ({copied} of {size} bytes)");
    }
    let dir = root.join(format!("context-{id}"));
    let unpack_dir = dir.clone();
    let unpack_archive = archive.clone();
    let unpacked = tokio::task::spawn_blocking(move || unpack(&unpack_archive, &unpack_dir)).await?;
    let _ = fs::remove_file(&archive);
    match unpacked {
        Ok(files) => Ok(Upload { dir, bytes: size, files }),
        Err(err) => {
            let _ = fs::remove_dir_all(&dir);
            Err(err)
        }
    }
}

fn unpack(archive: &Path, dir: &Path) -> Result<u64> {
    fs::create_dir_all(dir)?;
    let mut file = File::open(archive)?;
    let mut magic = [0u8; 2];
    let n = file.read(&mut magic)?;
    file.seek(SeekFrom::Start(0))?;
    let reader: Box<dyn Read> = if n == 2 && magic == [0x1f, 0x8b] {
        Box::new(GzDecoder::new(BufReader::new(file)))
    } else {
        Box::new(BufReader::new(file))
    };
    let mut tar = tar::Archive::new(reader);
    tar.set_preserve_permissions(true);
    tar.set_preserve_mtime(true);
    tar.set_unpack_xattrs(false);
    let mut files = 0;
    for entry in tar.entries().context("reading the build folder")? {
        let mut entry = entry?;
        // unpack_in refuses paths that would land outside `dir`.
        if entry.unpack_in(dir)? {
            files += 1;
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn receives_and_unpacks() {
        let mut tar = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(5);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append_data(&mut h, "src/a.txt", &b"hello"[..]).unwrap();
        let mut evil = tar::Header::new_gnu();
        evil.set_size(1);
        evil.set_mode(0o644);
        // The tar crate won't write `..` paths, so poke the name in directly.
        evil.as_old_mut().name[..9].copy_from_slice(b"../escape");
        evil.set_cksum();
        tar.append(&evil, &b"x"[..]).unwrap();
        let bytes = tar.into_inner().unwrap();

        let mut reader = &bytes[..];
        let up = receive(&mut reader, bytes.len() as u64).await.unwrap();
        assert_eq!(fs::read_to_string(up.dir.join("src/a.txt")).unwrap(), "hello");
        assert!(!up.dir.parent().unwrap().join("escape").exists());
        let dir = up.dir.clone();
        drop(up);
        assert!(!dir.exists());

        let mut short = &bytes[..10];
        assert!(receive(&mut short, bytes.len() as u64).await.is_err());
    }
}
