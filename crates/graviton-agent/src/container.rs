//! Container root filesystems and isolated processes inside the guest.
//!
//! * `prepare` mounts the host's blob store (virtiofs tag `blobs`, read-only),
//!   makes `/run/containers/<id>/rootfs`, and unpacks the image's layers into
//!   it in order, applying OCI whiteouts. Roots live on the VM's scratch disk
//!   (`/dev/vda`, formatted ext4 on first use each boot) so an image and what
//!   a job writes don't have to fit in the VM's memory; a VM without one (an
//!   older gravitond) gets a tmpfs as before.
//! * `command` builds a `Command` that, between fork and exec, enters new
//!   mount, UTS and IPC namespaces, moves the rootfs to `/` and chroots into
//!   it (pivot_root can't leave an initramfs), mounts `/proc` and `/sys`,
//!   sets the hostname and drops to the image's user.
//!
//! The VM boundary is the security boundary; these namespaces give the
//! process the filesystem and identity it expects.

use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use flate2::read::GzDecoder;

const BLOBS: &str = "/run/blobs";
const SHARES: &str = "/run/shares";
const CONTAINERS: &str = "/run/containers";
/// The scratch disk gravitond attaches, if it does.
const DISK: &str = "/dev/vda";

/// Whether container roots live on the scratch disk: formats and mounts it at
/// `CONTAINERS` the first time it's asked, once per boot.
fn on_disk() -> io::Result<bool> {
    static READY: std::sync::OnceLock<Result<bool, String>> = std::sync::OnceLock::new();
    READY.get_or_init(|| format_disk().map_err(|e| e.to_string())).clone().map_err(io::Error::other)
}

/// Drop any kernel block-device page cache for `dev` before inspecting or
/// mounting it: a VM restored from a snapshot probed `/dev/vda` against a blank
/// template disk before the container's real `disk.img` was attached.
fn invalidate_block_cache(dev: &str) {
    const BLKFLSBUF: libc::Ioctl = 0x1261;
    if let Ok(file) = File::open(dev) {
        // SAFETY: BLKFLSBUF flushes/invalidates cached pages on this unmounted block device.
        unsafe {
            let _ = libc::ioctl(std::os::fd::AsRawFd::as_raw_fd(&file), BLKFLSBUF, 0);
        }
    }
}

fn format_disk() -> io::Result<bool> {
    if !Path::new(DISK).exists() {
        return Ok(false);
    }
    invalidate_block_cache(DISK);
    if !is_ext4(DISK) {
        format(DISK)?;
    }
    fs::create_dir_all(CONTAINERS)?;
    crate::linux::mount_with(DISK, CONTAINERS, "ext4", libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOATIME, None)
        .map_err(|e| io::Error::other(format!("mounting {DISK}: {e}")))?;
    Ok(true)
}

/// Whether `dev` already holds an ext2/3/4 filesystem (a container's disk from an earlier run).
fn is_ext4(dev: &str) -> bool {
    let mut magic = [0u8; 2];
    File::open(dev).and_then(|mut f| {
        f.seek(SeekFrom::Start(1024 + 56))?;
        f.read_exact(&mut magic)
    })
    .is_ok()
        && u16::from_le_bytes(magic) == 0xEF53
}

/// Format a new disk. A container's disk lasts until it's removed, so it gets a
/// small journal (a VM powered off mid-write comes back consistent). No
/// checksums to pay for, nothing to discard (it's a fresh, empty sparse
/// file), and a fixed UUID and hash seed: making those up needs randomness
/// the kernel doesn't have this early in boot, and mke2fs waited seconds for it.
fn format(dev: &str) -> io::Result<()> {
    let out = crate::reap::output(Command::new("/sbin/mke2fs").args([
        "-q",
        "-F",
        "-t",
        "ext4",
        "-J",
        "size=16",
        "-O",
        "^metadata_csum",
        "-E",
        "lazy_itable_init=1,nodiscard,hash_seed=67726176-6974-6f6e-0000-000000000001",
        "-U",
        "67726176-6974-6f6e-0000-000000000002",
        "-m",
        "0",
        dev,
    ]))?;
    if !out.status.success() {
        return Err(io::Error::other(format!("formatting {dev}: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    Ok(())
}

pub fn rootfs(id: &str) -> io::Result<PathBuf> {
    let ok = !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !ok {
        return Err(io::Error::other(format!("invalid container id `{id}`")));
    }
    Ok(Path::new(CONTAINERS).join(id).join("rootfs"))
}

/// Unpack `layers` (sha256 digests, lowest first) into the container's root.
/// On its disk, files from an earlier run of the same image are kept as they
/// are (`(0, true)`); otherwise the root starts fresh. Returns the entry
/// count unpacked and whether the files were reused.
pub fn prepare(id: &str, layers: &[String]) -> io::Result<(u64, bool)> {
    mount_blobs()?;
    let disk = on_disk()?;
    let root = rootfs(id)?;
    // Written once a root is fully unpacked: which image it came from.
    let marker = root.with_file_name("image");
    let wanted = layers.join("\n");
    if disk && root.is_dir() && fs::read_to_string(&marker).is_ok_and(|m| m == wanted) {
        let path = root.to_string_lossy();
        crate::linux::mount_with(&path, &path, "none", libc::MS_BIND, None)?;
        return Ok((0, true));
    }
    let _ = fs::remove_file(&marker);
    if root.exists() {
        let _ = umount(&root);
        let _ = fs::remove_dir_all(&root);
    }
    fs::create_dir_all(&root)?;
    let path = root.to_string_lossy();
    if disk {
        // Entering the container moves its root to `/`, which needs a mount
        // point: bind the folder onto itself.
        crate::linux::mount_with(&path, &path, "none", libc::MS_BIND, None)?;
        set_mode(&root, 0o755)?;
    } else {
        crate::linux::mount_with("tmpfs", &path, "tmpfs", libc::MS_NOSUID | libc::MS_NODEV, Some("mode=0755,size=90%"))?;
    }

    let mut files = 0;
    for digest in layers {
        let hex = digest
            .strip_prefix("sha256:")
            .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| io::Error::other(format!("bad layer digest `{digest}`")))?;
        let blob = Path::new(BLOBS).join("sha256").join(hex);
        files += unpack_layer(&blob, &root).map_err(|e| io::Error::other(format!("layer {}: {e}", &digest[..19.min(digest.len())])))?;
    }

    for (dir, mode) in [("proc", 0o555), ("sys", 0o555), ("dev", 0o755), ("tmp", 0o1777), ("etc", 0o755)] {
        let p = root.join(dir);
        if !p.exists() {
            fs::create_dir_all(&p)?;
            set_mode(&p, mode)?;
        }
    }
    if disk {
        // Get the unpacked files and the marker onto the disk before the
        // container runs: ext4 holds new data in memory for up to 30 s, and
        // a VM powered off before then would come back with an empty marker
        // (and unpack again over the container's changes).
        fs::write(&marker, &wanted)?;
        let dir = File::open(CONTAINERS)?;
        // SAFETY: syncfs(2) on a descriptor we own.
        if unsafe { libc::syncfs(std::os::fd::AsRawFd::as_raw_fd(&dir)) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((files, false))
}

/// Mount the container's volumes and write its network files. Returns whether
/// the guest's container DNS resolver (`127.0.0.11:53`) is active.
pub fn finish(id: &str, mounts: &[graviton_proto::GuestMount], hostname: &str, ip: Option<&str>) -> io::Result<bool> {
    let root = rootfs(id)?;
    for m in mounts {
        let rel = clean(Path::new(&m.target)).ok_or_else(|| io::Error::other(format!("bad mount target `{}`", m.target)))?;
        let target = root.join(rel);
        fs::create_dir_all(&target)?;
        let multi_src = ensure_multi_share().then(|| Path::new(SHARES).join(&m.tag)).filter(|p| p.exists());
        if let Some(src) = multi_src {
            let dst = target.to_string_lossy();
            crate::linux::mount_with(&src.to_string_lossy(), &dst, "none", libc::MS_BIND, None)
                .map_err(|e| io::Error::other(format!("binding {} at {}: {e}", m.tag, m.target)))?;
            let mut remount = libc::MS_BIND | libc::MS_REMOUNT | libc::MS_NOSUID | libc::MS_NODEV;
            if m.read_only {
                remount |= libc::MS_RDONLY;
            }
            let _ = crate::linux::mount_with("", &dst, "none", remount, None);
        } else {
            let mut flags = libc::MS_NOSUID | libc::MS_NODEV;
            if m.read_only {
                flags |= libc::MS_RDONLY;
            }
            crate::linux::mount_with(&m.tag, &target.to_string_lossy(), "virtiofs", flags, None)
                .map_err(|e| io::Error::other(format!("mounting {} at {}: {e}", m.tag, m.target)))?;
        }
    }

    let etc = root.join("etc");
    fs::create_dir_all(&etc)?;
    let write = |name: &str, contents: &str| -> io::Result<()> {
        let path = etc.join(name);
        remove_any(&path)?; // images often ship these as symlinks
        fs::write(path, contents)
    };
    write("hostname", &format!("{hostname}\n"))?;
    let named = ip.unwrap_or("127.0.1.1");
    write("hosts", &format!("127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n{named}\t{hostname}\n"))?;

    let dns_active = if ip.is_some() && Path::new("/sys/class/net/eth1").exists() {
        match crate::dns::start() {
            Ok(()) => {
                write("resolv.conf", &format!("nameserver {}\n", crate::dns::LISTEN_IP))?;
                true
            }
            Err(err) => {
                eprintln!("graviton-agent: container dns: {err}");
                false
            }
        }
    } else {
        false
    };
    if !dns_active {
        if let Ok(resolv) = fs::read_to_string("/etc/resolv.conf") {
            write("resolv.conf", &resolv)?;
        }
    }
    Ok(dns_active)
}

/// Mount the host's `graviton` multi-directory virtiofs share at `/run/shares`,
/// if attached. Container VMs pack `blobs` plus any `-v` volumes under one
/// `VZMultipleDirectoryShare` so VM snapshots can restore with arbitrary volumes.
fn ensure_multi_share() -> bool {
    static MOUNTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *MOUNTED.get_or_init(|| {
        if fs::create_dir_all(SHARES).is_err() {
            return false;
        }
        crate::linux::mount_with("graviton", SHARES, "virtiofs", libc::MS_NOSUID | libc::MS_NODEV, None).is_ok()
    })
}

fn mount_blobs() -> io::Result<()> {
    if Path::new(BLOBS).join("sha256").is_dir() {
        return Ok(());
    }
    fs::create_dir_all(BLOBS)?;
    if ensure_multi_share() {
        let src = Path::new(SHARES).join("blobs");
        if src.is_dir() {
            crate::linux::mount_with(&src.to_string_lossy(), BLOBS, "none", libc::MS_BIND, None)
                .map_err(|e| io::Error::other(format!("binding the image store: {e}")))?;
            let _ = crate::linux::mount_with(
                "",
                BLOBS,
                "none",
                libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
                None,
            );
            return Ok(());
        }
    }
    crate::linux::mount_with("blobs", BLOBS, "virtiofs", libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV, None)
        .map_err(|e| io::Error::other(format!("mounting the image store: {e}")))
}

/// Unpack a tar (gzipped or not) into `dir` (ADD of a local archive).
pub fn unpack_into(archive: &Path, dir: &Path) -> io::Result<u64> {
    unpack_layer(archive, dir)
}

fn unpack_layer(blob: &Path, root: &Path) -> io::Result<u64> {
    let mut file = File::open(blob)?;
    let mut magic = [0u8; 4];
    let n = file.read(&mut magic)?;
    file.seek(SeekFrom::Start(0))?;
    let reader: Box<dyn Read> = if n >= 2 && magic[..2] == [0x1f, 0x8b] {
        Box::new(GzDecoder::new(BufReader::with_capacity(1 << 20, file)))
    } else if crate::zstd_stream::is_zstd_magic(&magic[..n]) {
        Box::new(crate::zstd_stream::ZstdReader::new(BufReader::with_capacity(1 << 20, file)))
    } else {
        Box::new(BufReader::with_capacity(1 << 20, file))
    };

    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(true);
    archive.set_preserve_mtime(true);
    archive.set_overwrite(true);
    archive.set_unpack_xattrs(false);

    let mut count = 0;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let Some(rel) = clean(&entry.path()?) else { continue };
        let name = rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let parent = root.join(rel.parent().unwrap_or(Path::new("")));

        if name == ".wh..wh..opq" {
            // Opaque directory: hide everything lower layers put here.
            if let Ok(children) = fs::read_dir(&parent) {
                for child in children.flatten() {
                    remove_any(&child.path())?;
                }
            }
            continue;
        }
        if let Some(hidden) = name.strip_prefix(".wh.") {
            remove_any(&parent.join(hidden))?;
            continue;
        }

        let kind = entry.header().entry_type();
        if kind.is_character_special() || kind.is_block_special() {
            continue; // /dev comes from the guest
        }
        let dest = root.join(&rel);
        if let Ok(meta) = fs::symlink_metadata(&dest) {
            if meta.is_dir() && !kind.is_dir() {
                fs::remove_dir_all(&dest)?;
            } else if !meta.is_dir() && kind.is_dir() {
                fs::remove_file(&dest)?;
            } else if !meta.is_dir() && kind.is_hard_link() {
                let same_target = entry
                    .link_name()
                    .ok()
                    .flatten()
                    .and_then(|l| clean(&l))
                    .is_some_and(|l| l == rel);
                if !same_target {
                    fs::remove_file(&dest)?;
                }
            }
        }
        entry.unpack_in(root)?;
        count += 1;
    }
    Ok(count)
}

/// Strip `./` and leading `/`; refuse anything with `..`.
fn clean(path: &Path) -> Option<PathBuf> {
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

fn remove_any(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

fn umount(path: &Path) -> io::Result<()> {
    let p = CString::new(path.as_os_str().as_encoded_bytes()).map_err(io::Error::other)?;
    // SAFETY: valid NUL-terminated path.
    if unsafe { libc::umount2(p.as_ptr(), libc::MNT_DETACH) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Resolve `user[:group]` against the container's /etc/passwd and /etc/group.
pub fn resolve_user(root: &Path, spec: Option<&str>) -> io::Result<(u32, u32, String)> {
    let spec = spec.unwrap_or("").trim();
    let (user, group) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };
    let passwd = fs::read_to_string(root.join("etc/passwd")).unwrap_or_default();
    let entries: Vec<Vec<&str>> = passwd.lines().map(|l| l.split(':').collect()).filter(|f: &Vec<&str>| f.len() >= 6).collect();

    let (uid, mut gid, home) = if user.is_empty() {
        (0, 0, "/root".to_string())
    } else if let Ok(uid) = user.parse::<u32>() {
        match entries.iter().find(|f| f[2] == user) {
            Some(f) => (uid, f[3].parse().unwrap_or(uid), f[5].to_string()),
            None => (uid, uid, "/".to_string()),
        }
    } else {
        let f = entries
            .iter()
            .find(|f| f[0] == user)
            .ok_or_else(|| io::Error::other(format!("user `{user}` not found in the image's /etc/passwd")))?;
        (f[2].parse().unwrap_or(0), f[3].parse().unwrap_or(0), f[5].to_string())
    };
    if let Some(g) = group.filter(|g| !g.is_empty()) {
        gid = match g.parse::<u32>() {
            Ok(n) => n,
            Err(_) => fs::read_to_string(root.join("etc/group"))
                .unwrap_or_default()
                .lines()
                .map(|l| l.split(':').collect::<Vec<_>>())
                .find(|f| f.len() >= 3 && f[0] == g)
                .and_then(|f| f[2].parse().ok())
                .ok_or_else(|| io::Error::other(format!("group `{g}` not found in the image's /etc/group")))?,
        };
    }
    Ok((uid, gid, home))
}

/// One pre-validated ephemeral mount to attach inside a child's private mount namespace.
pub struct PreparedMount {
    pub src: CString,
    pub dst: CString,
    pub is_tmpfs: bool,
    pub read_only: bool,
}

/// A `Command` that runs `argv` inside container `id`.
pub fn command(
    id: &str,
    argv: &[String],
    env: &[String],
    cwd: &str,
    user: Option<&str>,
    hostname: &str,
    mounts: Vec<PreparedMount>,
) -> io::Result<Command> {
    let root = rootfs(id)?;
    if !root.join("proc").is_dir() {
        return Err(io::Error::other(format!("container {id} has no root filesystem (prepare it first)")));
    }
    let (uid, gid, home) = resolve_user(&root, user)?;
    let (program, args) = argv.split_first().ok_or_else(|| io::Error::other("empty command"))?;

    let mut cmd = Command::new(program);
    cmd.args(args).env_clear();
    let mut has_home = false;
    for pair in env {
        if let Some((k, v)) = pair.split_once('=') {
            has_home |= k == "HOME";
            cmd.env(k, v);
        }
    }
    if !has_home {
        cmd.env("HOME", &home);
    }

    let c = |s: &str| CString::new(s).map_err(io::Error::other);
    let root_c = c(&root.to_string_lossy())?;
    let dev_dst = c(&root.join("dev").to_string_lossy())?;
    let cwd_c = c(if cwd.is_empty() { "/" } else { cwd })?;
    let host = hostname.as_bytes().to_vec();

    // SAFETY: the closure runs in the forked child before exec and only makes
    // raw syscalls on data prepared above (no allocation, no locks).
    unsafe {
        cmd.pre_exec(move || enter(&root_c, &dev_dst, &cwd_c, &host, uid, gid, &mounts));
    }
    Ok(cmd)
}

fn enter(root: &CString, dev_dst: &CString, cwd: &CString, hostname: &[u8], uid: u32, gid: u32, mounts: &[PreparedMount]) -> io::Result<()> {
    fn check(rc: libc::c_int) -> io::Result<()> {
        if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }
    let null = std::ptr::null::<libc::c_char>();
    // SAFETY: plain syscalls with valid NUL-terminated strings owned by the caller.
    unsafe {
        check(libc::unshare(libc::CLONE_NEWNS | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC))?;
        check(libc::mount(null, c"/".as_ptr(), null, libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()))?;
        check(libc::mount(c"/dev".as_ptr(), dev_dst.as_ptr(), null, libc::MS_BIND | libc::MS_REC, std::ptr::null()))?;
        for m in mounts {
            if m.is_tmpfs {
                let mut flags = libc::MS_NOSUID | libc::MS_NODEV;
                if m.read_only {
                    flags |= libc::MS_RDONLY;
                }
                check(libc::mount(c"tmpfs".as_ptr(), m.dst.as_ptr(), c"tmpfs".as_ptr(), flags, std::ptr::null()))?;
            } else {
                check(libc::mount(m.src.as_ptr(), m.dst.as_ptr(), null, libc::MS_BIND, std::ptr::null()))?;
                if m.read_only {
                    check(libc::mount(
                        null,
                        m.dst.as_ptr(),
                        null,
                        libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
                        std::ptr::null(),
                    ))?;
                }
            }
        }
        check(libc::chdir(root.as_ptr()))?;
        check(libc::mount(root.as_ptr(), c"/".as_ptr(), null, libc::MS_MOVE, std::ptr::null()))?;
        check(libc::chroot(c".".as_ptr()))?;
        check(libc::chdir(c"/".as_ptr()))?;
        check(libc::mount(c"proc".as_ptr(), c"/proc".as_ptr(), c"proc".as_ptr(), libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, std::ptr::null()))?;
        let _ = libc::mount(c"sysfs".as_ptr(), c"/sys".as_ptr(), c"sysfs".as_ptr(), libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, std::ptr::null());
        if !hostname.is_empty() {
            check(libc::sethostname(hostname.as_ptr().cast(), hostname.len()))?;
        }
        check(libc::setgroups(0, std::ptr::null()))?;
        check(libc::setgid(gid))?;
        check(libc::setuid(uid))?;
        check(libc::chdir(cwd.as_ptr()))?;
    }
    Ok(())
}
