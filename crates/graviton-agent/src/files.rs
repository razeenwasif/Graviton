//! Browse and copy a container's files (Files tab, `grav ls`, `grav cat`, `grav cp`).
//!
//! Paths are resolved relative to the container's root (`openat2(RESOLVE_IN_ROOT)`
//! for reads/listings, `build::resolve_in_root` for archive streaming), so `..`
//! and absolute symlinks inside the image resolve the way they would for a
//! process in the container. Volumes are mounted under the root, so they show up too.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use graviton_proto::{AgentEvent, DiffEntry, DiffKind, DirectoryListing, FileContents, FileEntry, FileKind, encode_line};

const MAX_ENTRIES: usize = 5000;
const MAX_DIFF_ENTRIES: usize = 4096;

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_IN_ROOT: u64 = 0x10;
const SYS_OPENAT2: libc::c_long = 437;

pub fn list(container: &str, path: &str) -> io::Result<DirectoryListing> {
    let dir = open_in_root(container, path, libc::O_DIRECTORY)?;
    let base = format!("/proc/self/fd/{}", dir.as_raw_fd());
    let mut entries = Vec::new();
    for entry in fs::read_dir(&base)? {
        let entry = entry?;
        let Ok(meta) = entry.metadata() else { continue }; // lstat: doesn't follow links
        let ft = meta.file_type();
        let kind = if ft.is_dir() {
            FileKind::Directory
        } else if ft.is_symlink() {
            FileKind::Symlink
        } else if ft.is_file() {
            FileKind::File
        } else {
            FileKind::Other
        };
        entries.push(FileEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            kind,
            size: meta.len(),
            mode: meta.mode() & 0o7777,
            uid: meta.uid(),
            gid: meta.gid(),
            modified: meta.mtime(),
            link_target: (kind == FileKind::Symlink)
                .then(|| fs::read_link(entry.path()).ok().map(|t| t.to_string_lossy().into_owned()))
                .flatten(),
        });
        if entries.len() >= MAX_ENTRIES {
            break;
        }
    }
    entries.sort_by(|a, b| {
        (a.kind != FileKind::Directory).cmp(&(b.kind != FileKind::Directory)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(DirectoryListing { path: normalise(path), entries })
}

pub fn read(container: &str, path: &str, limit: u64) -> io::Result<FileContents> {
    let fd = open_in_root(container, path, 0)?;
    let mut file = File::from(fd);
    let meta = file.metadata()?;
    if meta.is_dir() {
        return Err(io::Error::other(format!("{path} is a folder")));
    }
    if !meta.is_file() {
        return Err(io::Error::other(format!("{path} isn't a regular file")));
    }
    let mut data = Vec::with_capacity(limit.min(meta.len()) as usize);
    (&mut file).take(limit).read_to_end(&mut data)?;
    Ok(FileContents { path: normalise(path), size: meta.len(), truncated: (data.len() as u64) < meta.len(), data })
}

/// Stream a file or folder out of `container` as a raw tar archive on `conn`.
/// Sends one `AgentEvent::ArchiveReady` (or `Error`) line first, then raw tar bytes.
pub fn get_archive(container: &str, path: &str, mut conn: File) -> io::Result<()> {
    let root = crate::container::rootfs(container)?;
    let norm = normalise(path);
    let resolved = match crate::build::resolve_in_root(&root, &norm, false) {
        Ok(p) => p,
        Err(err) => {
            let _ = conn.write_all(&encode_line(&AgentEvent::Error { message: format!("{norm}: {err}") }));
            return Ok(());
        }
    };
    let meta = match fs::symlink_metadata(&resolved) {
        Ok(m) => m,
        Err(err) => {
            let _ = conn.write_all(&encode_line(&AgentEvent::Error { message: format!("{norm}: {err}") }));
            return Ok(());
        }
    };
    let is_dir = meta.is_dir();
    let name = if norm == "/" {
        "root".to_string()
    } else {
        norm.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("file").to_string()
    };
    conn.write_all(&encode_line(&AgentEvent::ArchiveReady { is_dir, name: name.clone() }))?;

    let mut tar = tar::Builder::new(&mut conn);
    tar.follow_symlinks(false);
    tar.mode(tar::HeaderMode::Complete);
    let mut seen_inodes: HashMap<(u64, u64), PathBuf> = HashMap::new();

    if !is_dir {
        append_entry(&mut tar, &resolved, Path::new(&name), &meta, &mut seen_inodes)?;
    } else {
        let is_root = resolved == root;
        let root_dev = meta.dev();
        let prefix = if is_root { PathBuf::new() } else { PathBuf::from(&name) };
        if !prefix.as_os_str().is_empty() {
            tar.append_path_with_name(&resolved, &prefix)?;
        }
        let mut stack = vec![PathBuf::new()];
        while let Some(rel) = stack.pop() {
            let dir = if rel.as_os_str().is_empty() { resolved.clone() } else { resolved.join(&rel) };
            let mut children: Vec<PathBuf> = match fs::read_dir(&dir) {
                Ok(rd) => rd.flatten().map(|e| rel.join(e.file_name())).collect(),
                Err(_) => continue,
            };
            children.sort();
            for child in children {
                if is_root && matches!(child.to_str(), Some("proc" | "sys" | "dev")) {
                    continue;
                }
                let abs = resolved.join(&child);
                let Ok(m) = fs::symlink_metadata(&abs) else { continue };
                let ft = m.file_type();
                if ft.is_char_device() || ft.is_block_device() || ft.is_socket() {
                    continue;
                }
                let archive_path = if prefix.as_os_str().is_empty() { child.clone() } else { prefix.join(&child) };
                append_entry(&mut tar, &abs, &archive_path, &m, &mut seen_inodes)?;
                if m.is_dir() && !(is_root && m.dev() != root_dev) {
                    stack.push(child);
                }
            }
        }
    }
    tar.finish()?;
    drop(tar);
    conn.flush()?;
    Ok(())
}

fn append_entry<W: Write>(
    tar: &mut tar::Builder<W>,
    abs: &Path,
    archive_path: &Path,
    meta: &fs::Metadata,
    seen_inodes: &mut HashMap<(u64, u64), PathBuf>,
) -> io::Result<()> {
    if meta.is_file() && meta.nlink() > 1 {
        let key = (meta.dev(), meta.ino());
        if let Some(target) = seen_inodes.get(&key) {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Link);
            h.set_size(0);
            h.set_mode(meta.mode() & 0o7777);
            h.set_uid(u64::from(meta.uid()));
            h.set_gid(u64::from(meta.gid()));
            h.set_mtime(meta.mtime().max(0) as u64);
            return tar.append_link(&mut h, archive_path, target);
        }
        seen_inodes.insert(key, archive_path.to_path_buf());
    }
    tar.append_path_with_name(abs, archive_path)
}

/// Read `size` bytes of tar (plain, gzip or zstd) from `reader` and unpack into
/// directory `path` inside `container`. Always drains `size` bytes so framing stays intact.
pub fn put_archive(container: &str, path: &str, size: u64, reader: &mut impl BufRead) -> io::Result<u64> {
    let mut limited = reader.take(size);
    let result = put_archive_inner(container, path, &mut limited);
    // Drain any unread tar padding or remaining bytes on early error.
    let _ = io::copy(&mut limited, &mut io::sink());
    result
}

fn put_archive_inner(container: &str, path: &str, limited: &mut impl Read) -> io::Result<u64> {
    let root = crate::container::rootfs(container)?;
    let norm = normalise(path);
    let dest = crate::build::resolve_in_root(&root, &norm, false)?;
    if !dest.is_dir() {
        return Err(io::Error::other(format!("{norm} isn't an existing folder")));
    }

    let mut buf = BufReader::with_capacity(64 * 1024, limited);
    let peek = buf.fill_buf()?.to_vec();
    let src: Box<dyn Read> = if peek.len() >= 2 && peek[..2] == [0x1f, 0x8b] {
        Box::new(GzDecoder::new(buf))
    } else if crate::zstd_stream::is_zstd_magic(&peek) {
        Box::new(crate::zstd_stream::ZstdReader::new(buf))
    } else {
        Box::new(buf)
    };

    let mut archive = tar::Archive::new(src);
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(false);
    archive.set_preserve_mtime(true);
    archive.set_overwrite(true);
    archive.set_unpack_xattrs(false);

    let mut count = 0u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let Some(rel) = clean_path(&entry.path()?) else { continue };
        let kind = entry.header().entry_type();
        if kind.is_character_special() || kind.is_block_special() {
            continue;
        }
        let target = dest.join(&rel);
        if let Ok(meta) = fs::symlink_metadata(&target) {
            if meta.is_dir() && !kind.is_dir() {
                fs::remove_dir_all(&target)?;
            } else if !meta.is_dir() && (kind.is_dir() || kind.is_symlink() || kind.is_hard_link()) {
                fs::remove_file(&target)?;
            }
        }
        if entry.unpack_in(&dest)? {
            count += 1;
        }
    }
    Ok(count)
}

fn clean_path(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

fn open_in_root(container: &str, path: &str, flags: libc::c_int) -> io::Result<OwnedFd> {
    let root = File::open(crate::container::rootfs(container)?).map_err(|e| io::Error::other(format!("the container's files aren't ready ({e})")))?;
    let rel = normalise(path);
    let rel = rel.trim_start_matches('/');
    let c = std::ffi::CString::new(if rel.is_empty() { "." } else { rel }).map_err(|_| io::Error::other("path contains NUL"))?;
    let how = OpenHow { flags: (flags | libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOCTTY | libc::O_NONBLOCK) as u64, mode: 0, resolve: RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS };
    // SAFETY: openat2(2) with a directory fd we own, a NUL-terminated path and a correctly sized open_how.
    let fd = unsafe { libc::syscall(SYS_OPENAT2, root.as_raw_fd(), c.as_ptr(), &how as *const OpenHow, std::mem::size_of::<OpenHow>()) };
    if fd < 0 {
        let err = io::Error::last_os_error();
        return Err(io::Error::new(err.kind(), format!("{}: {err}", normalise(path))));
    }
    // SAFETY: openat2 returned a fresh descriptor we now own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

/// `/a/./b//c/` → `/a/b/c`; `..` is left for the kernel to resolve within the root.
fn normalise(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    format!("/{}", parts.join("/"))
}

/// Return added, modified, and deleted entries in an EROFS+overlayfs container's upper layer.
pub fn diff(container: &str) -> io::Result<Vec<DiffEntry>> {
    let root = crate::container::rootfs(container)?;
    let base = root.parent().ok_or_else(|| io::Error::other("missing container base dir"))?;
    let upper = base.join("upper");
    let lower = base.join("lower");
    if !upper.is_dir() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let dir = if rel.as_os_str().is_empty() { upper.clone() } else { upper.join(&rel) };
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        let mut children: Vec<PathBuf> = rd.flatten().map(|e| rel.join(e.file_name())).collect();
        children.sort();
        for child_rel in children {
            if entries.len() >= MAX_DIFF_ENTRIES {
                break;
            }
            let abs_upper = upper.join(&child_rel);
            let Ok(meta) = fs::symlink_metadata(&abs_upper) else { continue };
            let ft = meta.file_type();
            let path = format!("/{}", child_rel.to_string_lossy());
            if ft.is_char_device() && meta.rdev() == 0 {
                entries.push(DiffEntry { kind: DiffKind::Deleted, path, size_bytes: None });
                continue;
            }
            let in_lower = fs::symlink_metadata(lower.join(&child_rel)).is_ok();
            if ft.is_dir() {
                if !in_lower {
                    entries.push(DiffEntry { kind: DiffKind::Added, path, size_bytes: None });
                }
                stack.push(child_rel);
            } else {
                entries.push(DiffEntry {
                    kind: if in_lower { DiffKind::Modified } else { DiffKind::Added },
                    path,
                    size_bytes: ft.is_file().then_some(meta.len()),
                });
            }
        }
        if entries.len() >= MAX_DIFF_ENTRIES {
            break;
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}
