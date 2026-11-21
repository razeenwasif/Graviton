//! Image builds inside the builder VM.
//!
//! gravitond prepares a container per Dockerfile stage (the usual `prepare`),
//! then for each step that changes files: `snapshot`, the step (`exec` for
//! RUN, `copy` for COPY/ADD, `mkdir` for WORKDIR), and `commit`, which walks
//! the root filesystem, compares it with the snapshot and writes what
//! changed as a gzipped layer tar (with OCI whiteouts for deletions and
//! hard-link tar entries for shared inodes) into the `out` share.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use flate2::Compression;
use flate2::write::GzEncoder;
use graviton_proto::build::{Ignore, expand_source, walk};
use graviton_proto::{ExecMount, ExecMountKind, InlineFile, LayerInfo, StagedFile};
use sha2::{Digest, Sha256};

use crate::container::{PreparedMount, rootfs};

const CONTEXT: &str = "/run/context";
const OUT: &str = "/run/out";
const CACHE: &str = "/run/cache";
const SECRETS: &str = "/run/secrets";
const STAGED: &str = "/run/staged";

#[derive(Clone, PartialEq, Eq)]
struct Entry {
    mode: u32,
    uid: u32,
    gid: u32,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    ino: u64,
    nlink: u64,
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
                nlink: m.nlink(),
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

/// Write what changed since the snapshot as `<OUT>/<name>`, keeping hard links
/// as tar `EntryType::Link` entries rather than duplicating file contents.
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

    // Track (dev, ino) -> first archived path for regular files with nlink > 1.
    let mut seen_inodes: HashMap<(u64, u64), PathBuf> = HashMap::new();
    for p in &changed {
        let abs = root.join(p);
        let meta = fs::symlink_metadata(&abs)?;
        let ft = meta.file_type();
        use std::os::unix::fs::FileTypeExt;
        if ft.is_char_device() || ft.is_block_device() || ft.is_socket() {
            continue;
        }
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
                tar.append_link(&mut h, p, target)?;
                entries += 1;
                continue;
            }
            seen_inodes.insert(key, p.clone());
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

/// Removes placeholder files/dirs created solely as ephemeral mountpoints
/// (e.g. `RUN --mount=type=secret`) once the child process and its mount
/// namespace have exited, so secrets never appear in a committed layer.
pub struct MountCleanup {
    files: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
}

impl Drop for MountCleanup {
    fn drop(&mut self) {
        for f in self.files.iter().rev() {
            let _ = fs::remove_file(f);
        }
        for d in self.dirs.iter().rev() {
            let _ = fs::remove_dir(d);
        }
    }
}

fn ensure_dir_tracked(root: &Path, path: &str, track: bool, created_dirs: &mut Vec<PathBuf>) -> io::Result<PathBuf> {
    let parts = graviton_proto::build::components(path);
    let mut cur = root.to_path_buf();
    for part in parts {
        let next = cur.join(&part);
        match fs::symlink_metadata(&next) {
            Ok(m) if m.file_type().is_symlink() => {
                cur = resolve_in_root(root, &format!("/{}", next.strip_prefix(root).unwrap_or(&next).to_string_lossy()), true)?;
            }
            Ok(_) => cur = next,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&next)?;
                if track {
                    created_dirs.push(next.clone());
                }
                cur = next;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(cur)
}

fn safe_rel(rel: &str) -> io::Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => return Err(io::Error::other(format!("bad relative mount source `{rel}`"))),
        }
    }
    Ok(out)
}

/// Prepare virtiofs shares and mountpoints for one `Exec`'s ephemeral mounts.
pub fn prepare_exec_mounts(container: &str, mounts: &[ExecMount]) -> io::Result<(Vec<PreparedMount>, MountCleanup)> {
    let root = rootfs(container)?;
    let mut prepared = Vec::with_capacity(mounts.len());
    let mut cleanup = MountCleanup { files: Vec::new(), dirs: Vec::new() };
    let to_c = |p: &Path| CString::new(p.as_os_str().as_bytes()).map_err(io::Error::other);

    for m in mounts {
        let target = if m.target.starts_with('/') { m.target.clone() } else { format!("/{}", m.target) };
        match m.kind {
            ExecMountKind::Cache => {
                mount_share("cache", CACHE, false)?;
                let rel = safe_rel(&m.source)?;
                if rel.as_os_str().is_empty() {
                    return Err(io::Error::other("cache mount needs a non-empty source key"));
                }
                let src = Path::new(CACHE).join(rel);
                fs::create_dir_all(&src)?;
                let dst = ensure_dir_tracked(&root, &target, false, &mut cleanup.dirs)?;
                prepared.push(PreparedMount { src: to_c(&src)?, dst: to_c(&dst)?, is_tmpfs: false, read_only: m.read_only });
            }
            ExecMountKind::Secret => {
                mount_share("secrets", SECRETS, true)?;
                let rel = safe_rel(&m.source)?;
                let src = Path::new(SECRETS).join(rel);
                if !src.is_file() {
                    return Err(io::Error::other(format!("secret file `{}` not found", m.source)));
                }
                let target_path = Path::new(&target);
                let parent = target_path.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into());
                let file_name = target_path
                    .file_name()
                    .ok_or_else(|| io::Error::other(format!("bad secret target `{target}`")))?;
                let parent_dir = ensure_dir_tracked(&root, &parent, true, &mut cleanup.dirs)?;
                let dst = parent_dir.join(file_name);
                if fs::symlink_metadata(&dst).is_err() {
                    File::create(&dst)?;
                    cleanup.files.push(dst.clone());
                }
                prepared.push(PreparedMount { src: to_c(&src)?, dst: to_c(&dst)?, is_tmpfs: false, read_only: true });
            }
            ExecMountKind::Bind => {
                let src = match &m.from_container {
                    Some(other) => resolve_in_root(&rootfs(other)?, &m.source, false)?,
                    None => {
                        mount_share("context", CONTEXT, true)?;
                        let rel = safe_rel(m.source.trim_start_matches('/'))?;
                        Path::new(CONTEXT).join(rel)
                    }
                };
                let src_meta = fs::metadata(&src).map_err(|e| io::Error::other(format!("bind mount source `{}`: {e}", m.source)))?;
                let dst = if src_meta.is_dir() {
                    ensure_dir_tracked(&root, &target, true, &mut cleanup.dirs)?
                } else {
                    let target_path = Path::new(&target);
                    let parent = target_path.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into());
                    let file_name = target_path
                        .file_name()
                        .ok_or_else(|| io::Error::other(format!("bad bind target `{target}`")))?;
                    let parent_dir = ensure_dir_tracked(&root, &parent, true, &mut cleanup.dirs)?;
                    let p = parent_dir.join(file_name);
                    if fs::symlink_metadata(&p).is_err() {
                        File::create(&p)?;
                        cleanup.files.push(p.clone());
                    }
                    p
                };
                prepared.push(PreparedMount { src: to_c(&src)?, dst: to_c(&dst)?, is_tmpfs: false, read_only: m.read_only });
            }
            ExecMountKind::Tmpfs => {
                let dst = ensure_dir_tracked(&root, &target, true, &mut cleanup.dirs)?;
                prepared.push(PreparedMount {
                    src: CString::new("tmpfs").unwrap(),
                    dst: to_c(&dst)?,
                    is_tmpfs: true,
                    read_only: m.read_only,
                });
            }
        }
    }
    Ok((prepared, cleanup))
}

pub struct CopyRequest<'a> {
    pub container: &'a str,
    pub sources: &'a [String],
    pub dest: &'a str,
    pub cwd: &'a str,
    pub from_container: Option<&'a str>,
    pub chown: Option<&'a str>,
    pub chmod: Option<u32>,
    pub extract: bool,
    pub ignore: &'a [String],
    pub inline: &'a [InlineFile],
    pub staged: &'a [StagedFile],
}

pub fn copy(req: &CopyRequest) -> io::Result<u64> {
    let root = rootfs(req.container)?;
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

    let (base, ignore, matched) = if !req.sources.is_empty() {
        let (base, ignore) = match req.from_container {
            Some(other) => (rootfs(other)?, Ignore::default()),
            None => {
                mount_share("context", CONTEXT, true)?;
                (PathBuf::from(CONTEXT), Ignore::new(req.ignore))
            }
        };
        let mut matched = Vec::new();
        for s in req.sources {
            let rel_src = s.trim_start_matches('/');
            matched.extend(expand_source(&base, if rel_src.is_empty() { "." } else { rel_src }, &ignore)?);
        }
        (Some(base), ignore, matched)
    } else {
        (None, Ignore::default(), Vec::new())
    };

    let total_sources = matched.len() + req.inline.len() + req.staged.len();
    let many = total_sources > 1 || req.sources.iter().any(|s| s.contains(['*', '?', '[']));
    let dest_is_dir = dest.ends_with('/') || many || resolve_in_root(&root, &dest, false).map(|p| p.is_dir()).unwrap_or(false);

    let mut files = 0;
    let mut inodes: HashMap<(u64, u64), PathBuf> = HashMap::new();

    if let Some(base) = base {
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
                    files += place(&base.join(&item), &target.join(sub), uid, gid, req.chmod, &mut inodes)?;
                }
            } else if req.extract && is_archive(&src) {
                let target = resolve_in_root(&root, &dest, true)?;
                files += crate::container::unpack_into(&src, &target)?;
            } else {
                let name = rel.file_name().map(|n| n.to_os_string()).unwrap_or_default();
                let target = file_target(&root, &dest, dest_is_dir, &name)?;
                files += place(&src, &target, uid, gid, req.chmod, &mut inodes)?;
            }
        }
    }

    for inline in req.inline {
        let target = file_target(&root, &dest, dest_is_dir, std::ffi::OsStr::new(&inline.name))?;
        remove_existing_for_file(&target)?;
        fs::write(&target, &inline.content)?;
        lchown(&target, uid, gid)?;
        let mode = req.chmod.unwrap_or(0o644) & 0o7777;
        fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
        files += 1;
    }

    if !req.staged.is_empty() {
        mount_share("staged", STAGED, true)?;
        for staged in req.staged {
            let rel = safe_rel(&staged.staged_path)?;
            let src = Path::new(STAGED).join(rel);
            let target = file_target(&root, &dest, dest_is_dir, std::ffi::OsStr::new(&staged.name))?;
            remove_existing_for_file(&target)?;
            fs::copy(&src, &target)?;
            lchown(&target, uid, gid)?;
            let mode = req.chmod.unwrap_or(staged.mode) & 0o7777;
            fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
            files += 1;
        }
    }

    Ok(files)
}

fn file_target(root: &Path, dest: &str, dest_is_dir: bool, default_name: &std::ffi::OsStr) -> io::Result<PathBuf> {
    if dest_is_dir {
        Ok(resolve_in_root(root, dest, true)?.join(default_name))
    } else {
        let parent = Path::new(dest).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into());
        let file_name = Path::new(dest).file_name().unwrap_or(default_name);
        Ok(resolve_in_root(root, &parent, true)?.join(file_name))
    }
}

fn remove_existing_for_file(dst: &Path) -> io::Result<()> {
    if let Ok(existing) = fs::symlink_metadata(dst) {
        if existing.is_dir() {
            fs::remove_dir_all(dst)?;
        } else {
            fs::remove_file(dst)?;
        }
    }
    Ok(())
}

/// Copy one file, folder (not its contents) or symlink, owned by uid:gid,
/// preserving hard links within the copy and applying optional `--chmod`.
fn place(
    src: &Path,
    dst: &Path,
    uid: u32,
    gid: u32,
    chmod: Option<u32>,
    inodes: &mut HashMap<(u64, u64), PathBuf>,
) -> io::Result<u64> {
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
        lchown(dst, uid, gid)?;
        let mode = match chmod {
            Some(m) => (m | ((m & 0o444) >> 2)) & 0o7777,
            None => meta.mode() & 0o7777,
        };
        fs::set_permissions(dst, fs::Permissions::from_mode(mode))?;
    } else if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
        lchown(dst, uid, gid)?;
    } else if meta.is_file() {
        if meta.nlink() > 1 {
            let key = (meta.dev(), meta.ino());
            if let Some(first) = inodes.get(&key) {
                fs::hard_link(first, dst)?;
                return Ok(1);
            }
            fs::copy(src, dst)?;
            inodes.insert(key, dst.to_path_buf());
        } else {
            fs::copy(src, dst)?;
        }
        // lchown BEFORE set_permissions so setuid/setgid bits survive.
        lchown(dst, uid, gid)?;
        let mode = chmod.unwrap_or(meta.mode()) & 0o7777;
        fs::set_permissions(dst, fs::Permissions::from_mode(mode))?;
    } else {
        return Ok(0);
    }
    Ok(1)
}

fn lchown(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let c = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: lchown(2) with a valid NUL-terminated path.
    if unsafe { libc::lchown(c.as_ptr(), uid, gid) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

fn is_archive(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    [".tar", ".tar.gz", ".tgz", ".tar.zst", ".tzst"].iter().any(|ext| name.ends_with(ext))
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
