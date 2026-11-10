//! Browse a container's files for the app's Files tab.
//!
//! Paths are opened with `openat2(RESOLVE_IN_ROOT)` relative to the
//! container's root, so `..` and absolute symlinks inside the image resolve
//! the way they would for a process in the container. Volumes are mounted
//! under the root, so they show up too.

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;

use graviton_proto::{DirectoryListing, FileContents, FileEntry, FileKind};

const MAX_ENTRIES: usize = 5000;

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
