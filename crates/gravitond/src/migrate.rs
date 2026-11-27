//! Cross-Mac Live VM Teleportation (`grav migrate`), sparse disk extent coding,
//! localhost port tunneling, and battery-aware burst offloading policy.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::net::TcpListener as StdTcpListener;
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use graviton_core::paths;
use graviton_proto::{
    ActiveTunnel, ChaosProfile, EgressPolicy, Healthcheck, OffloadPolicy, PortMapping, PowerStatus, RestartPolicy,
    TmpfsMount, UlimitSpec,
};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};

pub const SPARSE_MAGIC: &[u8; 8] = b"GRVSP01\0";
pub const SPARSE_CHUNK_SIZE: usize = 64 * 1024;
pub const SPARSE_EOF_SENTINEL: u64 = u64::MAX;
pub const MAX_MIGRATION_ARCHIVE: u64 = 8 * 1024 * 1024 * 1024; // 8 GiB
const MAX_NAMED_VOLUMES_BYTES: u64 = 512 * 1024 * 1024; // 512 MiB cap for portable named volumes
const MAX_UNPACKED_EXTENTS_BYTES: u64 = 64 * 1024 * 1024 * 1024; // 64 GiB sanity bound

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct PortableVolume {
    pub(crate) name: String,
    pub(crate) target: String,
    pub(crate) read_only: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct MigrationManifest {
    pub(crate) version: u32,
    pub(crate) source_id: String,
    pub(crate) name: String,
    pub(crate) image_ref: String,
    pub(crate) image_digest: String,
    pub(crate) architecture: String,
    pub(crate) was_running: bool,
    pub(crate) has_vm_state: bool,
    #[serde(default)]
    pub(crate) argv: Vec<String>,
    #[serde(default)]
    pub(crate) env: Vec<String>,
    #[serde(default)]
    pub(crate) cwd: String,
    #[serde(default)]
    pub(crate) user: Option<String>,
    pub(crate) cpus: u32,
    pub(crate) memory_mib: u64,
    #[serde(default)]
    pub(crate) ports: Vec<PortMapping>,
    #[serde(default)]
    pub(crate) named_volumes: Vec<PortableVolume>,
    #[serde(default)]
    pub(crate) restart: RestartPolicy,
    #[serde(default)]
    pub(crate) hostname: Option<String>,
    #[serde(default)]
    pub(crate) project: Option<String>,
    #[serde(default)]
    pub(crate) service: Option<String>,
    #[serde(default)]
    pub(crate) labels: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) dns_aliases: Vec<String>,
    #[serde(default)]
    pub(crate) healthcheck: Option<Healthcheck>,
    #[serde(default)]
    pub(crate) read_only: bool,
    #[serde(default)]
    pub(crate) cap_add: Vec<String>,
    #[serde(default)]
    pub(crate) cap_drop: Vec<String>,
    #[serde(default)]
    pub(crate) tmpfs: Vec<TmpfsMount>,
    #[serde(default)]
    pub(crate) init: bool,
    #[serde(default)]
    pub(crate) networks: Vec<String>,
    #[serde(default)]
    pub(crate) shm_size_bytes: Option<u64>,
    #[serde(default)]
    pub(crate) ulimits: Vec<UlimitSpec>,
    #[serde(default)]
    pub(crate) egress: EgressPolicy,
    #[serde(default)]
    pub(crate) chaos: ChaosProfile,
}

/// Encode `src` (typically a multi-GiB sparse `disk.img`) as a compact stream of
/// non-zero 64 KiB extents, using `SEEK_DATA`/`SEEK_HOLE` on APFS/Unix to skip
/// unallocated multi-GiB holes in O(allocated extents) time. Returns `(logical_bytes, extent_payload_bytes)`.
pub fn encode_sparse_extents(src: &Path, dst: &mut impl Write) -> Result<(u64, u64)> {
    let mut file = File::open(src).with_context(|| format!("opening sparse disk {}", src.display()))?;
    let logical_size = file.metadata()?.len();

    dst.write_all(SPARSE_MAGIC)?;
    dst.write_all(&logical_size.to_le_bytes())?;
    dst.write_all(&(SPARSE_CHUNK_SIZE as u32).to_le_bytes())?;

    let mut buf = vec![0u8; SPARSE_CHUNK_SIZE];
    let mut extent_bytes = 0u64;
    let fd = file.as_raw_fd();
    let mut pos = 0u64;
    let mut fallback_sequential = false;

    while pos < logical_size && !fallback_sequential {
        // SAFETY: valid open file descriptor `fd` with Darwin `SEEK_DATA`.
        let data_off = unsafe { libc::lseek(fd, pos as libc::off_t, libc::SEEK_DATA) };
        if data_off < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENXIO) {
                break; // only sparse holes remain until EOF
            }
            fallback_sequential = true;
            break;
        }
        let data_start = (data_off as u64).min(logical_size);
        if data_start >= logical_size {
            break;
        }
        // SAFETY: valid open file descriptor `fd` with Darwin `SEEK_HOLE`.
        let hole_off = unsafe { libc::lseek(fd, data_start as libc::off_t, libc::SEEK_HOLE) };
        let region_end = if hole_off < 0 {
            logical_size
        } else {
            (hole_off as u64).min(logical_size)
        };

        file.seek(SeekFrom::Start(data_start))?;
        let mut cur = data_start;
        while cur < region_end {
            let want = (region_end - cur).min(SPARSE_CHUNK_SIZE as u64) as usize;
            let mut filled = 0usize;
            while filled < want {
                let n = file.read(&mut buf[filled..want])?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            let chunk = &buf[..filled];
            if chunk.iter().any(|&b| b != 0) {
                dst.write_all(&cur.to_le_bytes())?;
                dst.write_all(&(filled as u32).to_le_bytes())?;
                dst.write_all(chunk)?;
                extent_bytes = extent_bytes.saturating_add(filled as u64);
            }
            cur = cur.saturating_add(filled as u64);
        }
        pos = region_end.max(cur.saturating_add(1));
    }

    if fallback_sequential {
        file.seek(SeekFrom::Start(pos))?;
        let mut reader = BufReader::new(file);
        let mut offset = pos;
        loop {
            let mut filled = 0usize;
            while filled < SPARSE_CHUNK_SIZE {
                let n = reader.read(&mut buf[filled..])?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            let chunk = &buf[..filled];
            if chunk.iter().any(|&b| b != 0) {
                dst.write_all(&offset.to_le_bytes())?;
                dst.write_all(&(filled as u32).to_le_bytes())?;
                dst.write_all(chunk)?;
                extent_bytes = extent_bytes.saturating_add(filled as u64);
            }
            offset = offset.saturating_add(filled as u64);
        }
    }

    dst.write_all(&SPARSE_EOF_SENTINEL.to_le_bytes())?;
    dst.write_all(&0u32.to_le_bytes())?;
    dst.flush()?;
    Ok((logical_size, extent_bytes))
}

/// Decode a sparse extent stream back into an APFS sparse file at `dst`.
pub fn decode_sparse_extents(src: &mut impl Read, dst: &Path) -> Result<u64> {
    let mut magic = [0u8; 8];
    src.read_exact(&mut magic).context("reading sparse extent header")?;
    if &magic != SPARSE_MAGIC {
        bail!("invalid sparse extent magic header");
    }
    let mut u64_buf = [0u8; 8];
    let mut u32_buf = [0u8; 4];
    src.read_exact(&mut u64_buf)?;
    let logical_size = u64::from_le_bytes(u64_buf);
    src.read_exact(&mut u32_buf)?;
    let chunk_size = u32::from_le_bytes(u32_buf) as usize;
    if logical_size > MAX_UNPACKED_EXTENTS_BYTES {
        bail!("sparse disk logical size {} exceeds safety bound", logical_size);
    }
    if chunk_size == 0 || chunk_size > 16 * 1024 * 1024 {
        bail!("invalid sparse extent chunk size {chunk_size}");
    }

    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = File::create(dst).with_context(|| format!("creating sparse disk {}", dst.display()))?;
    out.set_len(logical_size)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dst, fs::Permissions::from_mode(0o600));
    }

    let mut buf = vec![0u8; chunk_size];
    loop {
        src.read_exact(&mut u64_buf)?;
        let offset = u64::from_le_bytes(u64_buf);
        src.read_exact(&mut u32_buf)?;
        let len = u32::from_le_bytes(u32_buf) as usize;
        if offset == SPARSE_EOF_SENTINEL && len == 0 {
            break;
        }
        if len > chunk_size {
            bail!("sparse extent length {len} exceeds declared chunk size {chunk_size}");
        }
        if offset.checked_add(len as u64).is_none_or(|end| end > logical_size) {
            bail!("sparse extent out of bounds (offset={offset}, len={len}, logical={logical_size})");
        }
        src.read_exact(&mut buf[..len])?;
        out.seek(SeekFrom::Start(offset))?;
        out.write_all(&buf[..len])?;
    }
    out.flush()?;
    Ok(logical_size)
}

pub(crate) struct BundledPaths<'a> {
    pub(crate) manifest: &'a MigrationManifest,
    pub(crate) sparse_extents: Option<&'a Path>,
    pub(crate) vm_state: Option<&'a Path>,
    pub(crate) volume_dirs: &'a [(String, PathBuf)],
}

/// Package migration components into a compressed `.tar.gz` bundle at `dest_tar_gz`.
pub(crate) fn pack_migration_tar_gz(dest_tar_gz: &Path, input: &BundledPaths<'_>) -> Result<()> {
    let tmp = dest_tar_gz.with_extension("tar.gz.tmp");
    let file = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    let encoder = GzEncoder::new(BufWriter::new(file), Compression::fast());
    let mut tar = tar::Builder::new(encoder);
    tar.follow_symlinks(false);

    let manifest_bytes = serde_json::to_vec_pretty(input.manifest)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_bytes.len() as u64);
    header.set_mode(0o600);
    header.set_cksum();
    tar.append_data(&mut header, "manifest.json", manifest_bytes.as_slice())?;

    if let Some(ext_path) = input.sparse_extents {
        if ext_path.is_file() {
            tar.append_path_with_name(ext_path, "disk.extents")?;
        }
    }
    if let Some(vz_path) = input.vm_state {
        if vz_path.is_file() {
            tar.append_path_with_name(vz_path, "state.vzsave")?;
        }
    }

    let mut packed_vol_bytes = 0u64;
    for (name, dir) in input.volume_dirs {
        if !dir.is_dir() {
            continue;
        }
        let mut stack = vec![PathBuf::new()];
        while let Some(rel) = stack.pop() {
            let abs_dir = if rel.as_os_str().is_empty() { dir.clone() } else { dir.join(&rel) };
            let mut entries: Vec<PathBuf> = fs::read_dir(&abs_dir)?
                .flatten()
                .map(|e| rel.join(e.file_name()))
                .collect();
            entries.sort();
            for item_rel in entries {
                let abs = dir.join(&item_rel);
                let meta = fs::symlink_metadata(&abs)?;
                let tar_path = Path::new("volumes").join(name).join(&item_rel);
                if meta.is_dir() {
                    tar.append_dir(&tar_path, &abs)?;
                    stack.push(item_rel);
                } else if meta.is_file() || meta.file_type().is_symlink() {
                    packed_vol_bytes = packed_vol_bytes.saturating_add(meta.len());
                    if packed_vol_bytes > MAX_NAMED_VOLUMES_BYTES {
                        let _ = fs::remove_file(&tmp);
                        bail!(
                            "named volumes exceed {} MiB migration limit (use shared object storage or exclude large datasets)",
                            MAX_NAMED_VOLUMES_BYTES >> 20
                        );
                    }
                    tar.append_path_with_name(&abs, &tar_path)?;
                }
            }
        }
    }

    let encoder = tar.into_inner()?;
    encoder.finish()?;
    fs::rename(&tmp, dest_tar_gz)?;
    Ok(())
}

/// Sanitize a relative tar path, rejecting any `..`, root `/`, or Windows prefixes.
pub fn clean_safe_rel(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Normal(seg) => out.push(seg),
            Component::CurDir => {}
            Component::RootDir | Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// Extract a migration `.tar.gz` bundle into `work_dir` safely (rejecting path traversal & escaping symlinks).
pub fn unpack_migration_tar_gz(archive_path: &Path, work_dir: &Path) -> Result<()> {
    fs::create_dir_all(work_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(work_dir, fs::Permissions::from_mode(0o700));
    }
    let file = File::open(archive_path).with_context(|| format!("opening {}", archive_path.display()))?;
    let mut archive = tar::Archive::new(GzDecoder::new(BufReader::new(file)));
    archive.set_preserve_permissions(true);
    archive.set_overwrite(true);
    archive.set_unpack_xattrs(false);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw_rel = entry.path()?.into_owned();
        let Some(safe_rel) = clean_safe_rel(&raw_rel) else {
            bail!("unsafe path inside migration archive: {}", raw_rel.display());
        };
        let kind = entry.header().entry_type();
        if kind.is_hard_link() {
            bail!("hard links are not permitted in migration archives ({})", safe_rel.display());
        }
        if kind.is_symlink() {
            if let Ok(Some(target)) = entry.link_name() {
                if target.is_absolute()
                    || target.components().any(|c| matches!(c, Component::ParentDir))
                {
                    bail!("unsafe symlink target inside migration archive: {}", target.display());
                }
            }
        }
        let dst = work_dir.join(&safe_rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        entry.unpack(&dst).with_context(|| format!("unpacking {}", safe_rel.display()))?;
    }
    Ok(())
}

/// Parse `pmset -g batt` output into [`PowerStatus`].
pub fn parse_pmset_batt(output: &str) -> PowerStatus {
    let mut source = "AC Power".to_string();
    let mut on_battery = false;
    let mut battery_percent = None;

    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Now drawing from '") {
            if let Some(end) = rest.find('\'') {
                source = rest[..end].to_string();
                on_battery = source.to_ascii_lowercase().contains("battery");
            }
        }
        if battery_percent.is_none() {
            if let Some(pct_idx) = trimmed.find('%') {
                let prefix = &trimmed[..pct_idx];
                let digits: String = prefix.chars().rev().take_while(|c| c.is_ascii_digit()).collect::<String>().chars().rev().collect();
                if let Ok(pct) = digits.parse::<u8>() {
                    battery_percent = Some(pct.min(100));
                }
            }
        }
    }

    PowerStatus { on_battery, battery_percent, source }
}

/// Query macOS host power source and battery level (`pmset -g batt`).
pub fn detect_power_status() -> PowerStatus {
    match Command::new("/usr/bin/pmset").args(["-g", "batt"]).output() {
        Ok(out) if out.status.success() => parse_pmset_batt(&String::from_utf8_lossy(&out.stdout)),
        _ => PowerStatus {
            on_battery: false,
            battery_percent: None,
            source: "AC Power".into(),
        },
    }
}

pub fn offload_policy_path() -> PathBuf {
    paths::support_dir().join("offload.json")
}

pub fn load_offload_policy(path: &Path) -> OffloadPolicy {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => OffloadPolicy::default(),
    }
}

pub fn save_offload_policy(path: &Path, policy: &OffloadPolicy) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(policy)?;
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, path).with_context(|| format!("updating {}", path.display()))?;
    Ok(())
}

struct TunnelEntry {
    info: ActiveTunnel,
    task: tokio::task::JoinHandle<()>,
}

/// Manages localhost TCP port forwarders pointing at containers migrated to paired Macs.
#[derive(Default)]
pub struct TunnelManager {
    next_id: AtomicU64,
    entries: Mutex<HashMap<String, TunnelEntry>>,
}

impl TunnelManager {
    pub fn start(
        &self,
        rt: &tokio::runtime::Handle,
        container_name: String,
        remote_host: String,
        remote_addr: String,
        local_port: u16,
        remote_port: u16,
    ) -> Result<ActiveTunnel> {
        let addr_trim = remote_addr.trim().to_string();
        if addr_trim.is_empty() {
            bail!("tunnel remote address cannot be empty");
        }
        if remote_port == 0 {
            bail!("tunnel remote port must be non-zero");
        }

        let listener_std = StdTcpListener::bind(("127.0.0.1", local_port))
            .with_context(|| format!("binding local tunnel port 127.0.0.1:{local_port}"))?;
        listener_std.set_nonblocking(true)?;
        let bound_port = listener_std.local_addr()?.port();
        let _guard = rt.enter();
        let listener = TcpListener::from_std(listener_std)?;

        let id = format!("tun-{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1);
        let target_endpoint = if addr_trim.contains(':') && !addr_trim.starts_with('[') {
            format!("[{addr_trim}]:{remote_port}")
        } else {
            format!("{addr_trim}:{remote_port}")
        };

        let task = rt.spawn(async move {
            loop {
                let Ok((mut inbound, _)) = listener.accept().await else {
                    break;
                };
                let dst = target_endpoint.clone();
                tokio::spawn(async move {
                    if let Ok(mut outbound) = TcpStream::connect(&dst).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
            }
        });

        let info = ActiveTunnel {
            id: id.clone(),
            container_name,
            remote_host,
            remote_addr: addr_trim,
            local_port: bound_port,
            remote_port,
        };
        self.entries
            .lock()
            .unwrap()
            .insert(id, TunnelEntry { info: info.clone(), task });
        Ok(info)
    }

    pub fn list(&self) -> Vec<ActiveTunnel> {
        let mut out: Vec<ActiveTunnel> = self.entries.lock().unwrap().values().map(|e| e.info.clone()).collect();
        out.sort_by(|a, b| a.local_port.cmp(&b.local_port).then_with(|| a.id.cmp(&b.id)));
        out
    }

    pub fn stop(&self, id: &str) -> Result<String> {
        let mut entries = self.entries.lock().unwrap();
        let Some(entry) = entries.remove(id) else {
            bail!("no active port tunnel `{id}`");
        };
        entry.task.abort();
        Ok(id.to_string())
    }
}

impl Drop for TunnelManager {
    fn drop(&mut self) {
        if let Ok(mut entries) = self.entries.lock() {
            for (_, entry) in entries.drain() {
                entry.task.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn sparse_extents_round_trip_preserves_holes_and_data() {
        let dir = std::env::temp_dir().join(format!("grav-sparse-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let src = dir.join("disk.img");
        let dst = dir.join("restored.img");
        let total_size = 2 * 1024 * 1024 + 777; // spans multiple 64 KiB chunks + partial tail

        {
            let mut f = File::create(&src).unwrap();
            f.set_len(total_size).unwrap();
            // Write across a 64 KiB boundary in the middle and right at the tail, leaving holes everywhere else.
            f.seek(SeekFrom::Start(65_500)).unwrap();
            f.write_all(b"GRAVITON_SPARSE_BOUNDARY_PAYLOAD_0123456789ABCDEF").unwrap();
            f.seek(SeekFrom::Start(1_500_000)).unwrap();
            f.write_all(&[0xAB; 4096]).unwrap();
            f.seek(SeekFrom::Start(total_size - 16)).unwrap();
            f.write_all(b"TAIL_MARKER_1234").unwrap();
            f.flush().unwrap();
        }

        let mut encoded = Vec::new();
        let (logical, extent_bytes) = encode_sparse_extents(&src, &mut encoded).unwrap();
        assert_eq!(logical, total_size);
        // Only a small fraction of the 2 MiB sparse file had non-zero chunks.
        assert!(extent_bytes <= 5 * SPARSE_CHUNK_SIZE as u64, "unexpected extent bytes: {extent_bytes}");

        let mut cursor = std::io::Cursor::new(&encoded);
        let restored_len = decode_sparse_extents(&mut cursor, &dst).unwrap();
        assert_eq!(restored_len, total_size);

        let orig_bytes = fs::read(&src).unwrap();
        let rest_bytes = fs::read(&dst).unwrap();
        assert_eq!(orig_bytes, rest_bytes);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pmset_batt_parser_handles_ac_and_battery() {
        let batt = "\
Now drawing from 'Battery Power'
 -InternalBattery-0 (id=12345)	38%; discharging; 2:14 remaining present: true
";
        let st = parse_pmset_batt(batt);
        assert!(st.on_battery);
        assert_eq!(st.battery_percent, Some(38));
        assert_eq!(st.source, "Battery Power");

        let ac = "\
Now drawing from 'AC Power'
 -InternalBattery-0 (id=12345)	100%; charged; 0:00 remaining present: true
";
        let st_ac = parse_pmset_batt(ac);
        assert!(!st_ac.on_battery);
        assert_eq!(st_ac.battery_percent, Some(100));
        assert_eq!(st_ac.source, "AC Power");
    }

    #[test]
    fn offload_policy_round_trip_persistence() {
        let dir = std::env::temp_dir().join(format!("grav-offload-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("offload.json");

        assert_eq!(load_offload_policy(&path), OffloadPolicy::default());

        let custom = OffloadPolicy {
            enabled: true,
            preferred_host: Some("mac-studio".into()),
            battery_threshold: 45,
            offload_multi_arch: true,
            offload_builds: true,
            offload_compose: false,
        };
        save_offload_policy(&path, &custom).unwrap();
        assert_eq!(load_offload_policy(&path), custom);

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tunnel_manager_forwards_tcp_and_releases_port() {
        let echo_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo_listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            while let Ok((mut sock, _)) = echo_listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 64];
                    if let Ok(n) = sock.read(&mut buf).await {
                        let _ = sock.write_all(&buf[..n]).await;
                    }
                });
            }
        });

        let mgr = TunnelManager::default();
        let tun = mgr
            .start(
                &tokio::runtime::Handle::current(),
                "web".into(),
                "dst-mac".into(),
                "127.0.0.1".into(),
                0,
                echo_addr.port(),
            )
            .unwrap();
        assert_eq!(mgr.list().len(), 1);

        let mut client = TcpStream::connect(("127.0.0.1", tun.local_port)).await.unwrap();
        client.write_all(b"ping-teleport").await.unwrap();
        let mut reply = [0u8; 32];
        let n = client.read(&mut reply).await.unwrap();
        assert_eq!(&reply[..n], b"ping-teleport");
        drop(client);

        assert_eq!(mgr.stop(&tun.id).unwrap(), tun.id);
        assert!(mgr.list().is_empty());
        server_task.abort();
    }
}
