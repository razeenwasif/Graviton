//! Image builds inside the builder VM.
//!
//! gravitond prepares a container per Dockerfile stage (the usual `prepare`),
//! then for each step that changes files: `snapshot`, the step (`exec` for
//! RUN, `copy` for COPY/ADD, `mkdir` for WORKDIR), and `commit`, which walks
//! the root filesystem, compares it with the snapshot and writes what
//! changed as a gzipped layer tar (with OCI whiteouts for deletions) into the
//! `out` share. gravitond moves the layers into its blob store.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use flate2::Compression;
use flate2::write::GzEncoder;
use graviton_proto::LayerInfo;
use graviton_proto::build::{Ignore, expand_source, walk};
use sha2::{Digest, Sha256};

use crate::container::rootfs;

const CONTEXT: &str = "/run/context";
const OUT: &str = "/run/out";

#[derive(Clone, PartialEq, Eq)]
struct Entry {
    mode: u32,
    uid: u32,
    gid: u32,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    ino: u64,
}

type Snap = BTreeMap<PathBuf, Entry>;

static SNAPSHOTS: LazyLock<Mutex<HashMap<String, Snap>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn snapshot(container: &str) -> io::Result<()> {
    let snap = scan(&rootfs(container)?)?;
    SNAPSHOTS.lock().unwrap().insert(container.to_string(), snap);
    Ok(())
}

/// Every path under `root` (relative), without following symlinks. The
/// container's /proc, /sys and /dev are mount points at run time, so their
/// contents don't belong in layers.
fn scan(root: &Path) -> io::Result<Snap> {
    let mut snap = Snap::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let abs = root.join(&rel);
        let m = fs::symlink_metadata(&abs)?;
        if !rel.as_os_str().is_empty() {
            snap.insert(rel.clone(), Entry {
                mode: m.mode(),
                uid: m.uid(),
                gid: m.gid(),
                size: m.size(),
                mtime: (m.mtime(), m.mtime_nsec()),
                ctime: (m.ctime(), m.ctime_nsec()),
                ino: m.ino(),
            });
        }
        let skip = matches!(rel.to_str(), Some("proc" | "sys" | "dev"));
        if m.is_dir() && !skip {
            for e in fs::read_dir(&abs)? {
                stack.push(rel.join(e?.file_name()));
            }
        }
    }
    Ok(snap)
}

/// Hashes everything written through it.
struct Hashing<W> {
    inner: W,
    hash: Sha256,
    bytes: u64,
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Write what changed since the snapshot as `<OUT>/<name>`.
pub fn commit(container: &str, name: &str) -> io::Result<LayerInfo> {
    if name.contains('/') || name.starts_with('.') {
        return Err(io::Error::other(format!("bad layer name `{name}`")));
    }
    let root = rootfs(container)?;
    let before = SNAPSHOTS.lock().unwrap().remove(container).ok_or_else(|| io::Error::other("commit without a snapshot"))?;
    let after = scan(&root)?;

    let mut changed: BTreeSet<PathBuf> = after.iter().filter(|(p, e)| before.get(*p) != Some(*e)).map(|(p, _)| p.clone()).collect();
    // Deleted paths, leaving out those under a deleted folder.
    let deleted: Vec<PathBuf> = before
        .keys()
        .filter(|p| !after.contains_key(*p))
        .filter(|p| !p.ancestors().skip(1).any(|a| !a.as_os_str().is_empty() && before.contains_key(a) && !after.contains_key(a)))
        .cloned()
        .collect();
    let empty = changed.is_empty() && deleted.is_empty();
    // Folders above anything in the layer go in too, so it unpacks with the right owners and modes.
    for p in changed.clone().iter().chain(deleted.iter()) {
        for a in p.ancestors().skip(1) {
            if !a.as_os_str().is_empty() && after.contains_key(a) {
                changed.insert(a.to_path_buf());
            }
        }
    }
    SNAPSHOTS.lock().unwrap().insert(container.to_string(), after.clone());
    if empty {
        return Ok(LayerInfo { file: None, digest: String::new(), diff_id: String::new(), size: 0, entries: 0 });
    }

    mount_share("out", OUT, false)?;
    let path = Path::new(OUT).join(name);
    let file = File::create(&path)?;
    let compressed = Hashing { inner: file, hash: Sha256::new(), bytes: 0 };
    let gz = GzEncoder::new(compressed, Compression::fast());
    let raw = Hashing { inner: gz, hash: Sha256::new(), bytes: 0 };
    let mut tar = tar::Builder::new(raw);
    tar.follow_symlinks(false);
    tar.mode(tar::HeaderMode::Complete);

    let mut entries = 0u64;
    // Whiteouts first, then the new content (BTreeSet keeps parents before children).
    for p in &deleted {
        let wh = p.parent().unwrap_or(Path::new("")).join(format!(".wh.{}", p.file_name().unwrap_or_default().to_string_lossy()));
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(0);
        h.set_mode(0o644);
        h.set_mtime(0);
        tar.append_data(&mut h, &wh, io::empty())?;
        entries += 1;
    }
    for p in &changed {
        let abs = root.join(p);
        let meta = fs::symlink_metadata(&abs)?;
        let ft = meta.file_type();
        use std::os::unix::fs::FileTypeExt;
        if ft.is_char_device() || ft.is_block_device() || ft.is_socket() {
            continue;
        }
        tar.append_path_with_name(&abs, p)?;
        entries += 1;
    }
    let raw = tar.into_inner()?;
    let diff_id = format!("sha256:{}", hex(&raw.hash.finalize()));
    let compressed = raw.inner.finish()?;
    let digest = format!("sha256:{}", hex(&compressed.hash.finalize()));
    compressed.inner.sync_all()?;
    Ok(LayerInfo { file: Some(name.to_string()), digest, diff_id, size: compressed.bytes, entries })
}

/// Follow `path` inside `root` the way a process chrooted there would,
/// resolving symlinks (absolute ones against `root`). With `create`, missing
/// folders are made (owned by root, 0755). Returns the real path.
pub fn resolve_in_root(root: &Path, path: &str, create: bool) -> io::Result<PathBuf> {
    let mut todo: Vec<String> = graviton_proto::build::components(path).into_iter().rev().collect();
    let mut cur = root.to_path_buf();
    let mut hops = 0;
    while let Some(part) = todo.pop() {
        let next = cur.join(&part);
        match fs::symlink_metadata(&next) {
            Ok(m) if m.file_type().is_symlink() => {
                hops += 1;
                if hops > 40 {
                    return Err(io::Error::other(format!("too many symlinks in {path}")));
                }
                let target = fs::read_link(&next)?;
                if target.is_absolute() {
                    cur = root.to_path_buf();
                }
                for c in target.components().rev() {
                    match c {
                        Component::Normal(s) => todo.push(s.to_string_lossy().into_owned()),
                        Component::ParentDir => todo.push("..".into()),
                        _ => {}
                    }
                }
            }
            _ if part == ".." => {
                if cur != root {
                    cur.pop();
                }
            }
            Ok(_) => cur = next,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if create {
                    fs::create_dir(&next)?;
                }
                cur = next;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(cur)
}

pub fn mkdir(container: &str, path: &str) -> io::Result<()> {
    let dir = resolve_in_root(&rootfs(container)?, path, true)?;
    if !dir.is_dir() {
        return Err(io::Error::other(format!("WORKDIR {path} exists and isn't a folder")));
    }
    Ok(())
}

pub struct CopyRequest<'a> {
    pub container: &'a str,
    pub sources: &'a [String],
    pub dest: &'a str,
    pub cwd: &'a str,
    pub from_container: Option<&'a str>,
    pub chown: Option<&'a str>,
    pub extract: bool,
    pub ignore: &'a [String],
}

pub fn copy(req: &CopyRequest) -> io::Result<u64> {
    let root = rootfs(req.container)?;
    let (base, ignore) = match req.from_container {
        Some(other) => (rootfs(other)?, Ignore::default()),
        None => {
            mount_share("context", CONTEXT, true)?;
            (PathBuf::from(CONTEXT), Ignore::new(req.ignore))
        }
    };
    let (uid, gid) = match req.chown {
        Some(spec) => {
            let (u, g, _) = crate::container::resolve_user(&root, Some(spec))?;
            (u, g)
        }
        None => (0, 0),
    };
    let dest = if req.dest.starts_with('/') {
        req.dest.to_string()
    } else {
        format!("{}/{}", if req.cwd.is_empty() { "/" } else { req.cwd }, req.dest)
    };

    let mut matched = Vec::new();
    for s in req.sources {
        let rel_src = s.trim_start_matches('/');
        matched.extend(expand_source(&base, if rel_src.is_empty() { "." } else { rel_src }, &ignore)?);
    }
    let many = matched.len() > 1 || req.sources.iter().any(|s| s.contains(['*', '?', '[']));
    let dest_is_dir = dest.ends_with('/') || many || resolve_in_root(&root, &dest, false).map(|p| p.is_dir()).unwrap_or(false);

    let mut files = 0;
    for rel in matched {
        let src = base.join(&rel);
        let meta = fs::symlink_metadata(&src)?;
        if meta.is_dir() {
            // A folder's contents go into the destination folder.
            let target = resolve_in_root(&root, &dest, true)?;
            for item in walk(&base, &rel, &ignore)? {
                let sub = item.strip_prefix(&rel).unwrap_or(&item);
                if sub.as_os_str().is_empty() {
                    continue;
                }
                files += place(&base.join(&item), &target.join(sub), uid, gid)?;
            }
        } else if req.extract && is_archive(&src) {
            let target = resolve_in_root(&root, &dest, true)?;
            files += crate::container::unpack_into(&src, &target)?;
        } else {
            let name = rel.file_name().map(|n| n.to_os_string()).unwrap_or_default();
            let target = if dest_is_dir {
                resolve_in_root(&root, &dest, true)?.join(name)
            } else {
                let parent = Path::new(&dest).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into());
                let file_name = Path::new(&dest).file_name().map(|n| n.to_os_string()).unwrap_or(name);
                resolve_in_root(&root, &parent, true)?.join(file_name)
            };
            files += place(&src, &target, uid, gid)?;
        }
    }
    Ok(files)
}

/// Copy one file, folder (not its contents) or symlink, owned by uid:gid.
fn place(src: &Path, dst: &Path, uid: u32, gid: u32) -> io::Result<u64> {
    let meta = fs::symlink_metadata(src)?;
    if let Ok(existing) = fs::symlink_metadata(dst) {
        if !(existing.is_dir() && meta.is_dir()) {
            if existing.is_dir() { fs::remove_dir_all(dst)? } else { fs::remove_file(dst)? }
        }
    }
    if meta.is_dir() {
        if !dst.is_dir() {
            fs::create_dir(dst)?;
        }
        fs::set_permissions(dst, fs::Permissions::from_mode(meta.mode() & 0o7777))?;
    } else if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
    } else if meta.is_file() {
        fs::copy(src, dst)?;
        fs::set_permissions(dst, fs::Permissions::from_mode(meta.mode() & 0o7777))?;
    } else {
        return Ok(0);
    }
    lchown(dst, uid, gid)?;
    Ok(1)
}

fn lchown(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: lchown(2) with a valid NUL-terminated path.
    if unsafe { libc::lchown(c.as_ptr(), uid, gid) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

fn is_archive(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    [".tar", ".tar.gz", ".tgz"].iter().any(|ext| name.ends_with(ext))
}

/// Mount a virtiofs share once.
fn mount_share(tag: &str, at: &str, read_only: bool) -> io::Result<()> {
    static MOUNTED: LazyLock<Mutex<BTreeSet<String>>> = LazyLock::new(|| Mutex::new(BTreeSet::new()));
    let mut mounted = MOUNTED.lock().unwrap();
    if mounted.contains(tag) {
        return Ok(());
    }
    fs::create_dir_all(at)?;
    let mut flags = libc::MS_NOSUID | libc::MS_NODEV;
    if read_only {
        flags |= libc::MS_RDONLY;
    }
    crate::linux::mount_with(tag, at, "virtiofs", flags, None).map_err(|e| io::Error::other(format!("mounting the build's {tag} folder: {e}")))?;
    mounted.insert(tag.to_string());
    Ok(())
}
