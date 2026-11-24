//! VM snapshot pool: save and restore pre-booted Linux guest VMs via Apple's
//! `Virtualization.framework` (`saveMachineStateToURL` / `restoreMachineStateFromURL`).
//!
//! Because `Virtualization.framework` requires a restored VM to keep the exact
//! `VZGenericMachineIdentifier`, NIC MAC addresses, CPU count, memory size, and
//! device topology that were saved, we maintain a small slot pool per VM shape
//! `(cpus, memory_mib, rosetta, guest_fingerprint)`. Each slot `0..MAX_SLOTS`
//! owns a unique machine identifier and deterministic unicast MACs (`eth0` NAT
//! and `eth1` switch), guarded by an RAII [`SnapshotLease`] so two concurrent
//! containers never share a slot or MAC address.
//!
//! Per-container resources — scratch `disk.img`, `console.log`, `switch_port`
//! fd, and arbitrary `-v` volumes packed inside `VZMultipleDirectoryShare`
//! (`tag = "graviton"`) — are swapped freely on restore.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use graviton_core::paths;
use graviton_proto::{AGENT_VSOCK_PORT, AgentEvent, SnapshotGroup, SnapshotInfo};
use graviton_vmm::apple::{AppleHypervisor, AppleVm};
use graviton_vmm::{NetworkMode, SharedDirectory, VmConfig, VmHandle, VmIdentity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Maximum saved slots kept per `(cpus, memory_mib, rosetta)` shape.
pub const MAX_SLOTS_PER_SHAPE: usize = 8;

/// Kernel cmdline used for saved snapshot template VMs (`graviton.snapshot=1`
/// defers DHCP and disk/volume mounting until `AgentRequest::Prepare` runs on restore).
pub const SNAPSHOT_CMDLINE: &str = "console=hvc0 panic=-1 quiet graviton.snapshot=1";

const LOCKED_REASON: &str = "macOS console session is locked (unlock screen to restore .vzsave snapshots)";
const LOCKED_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Shape {
    pub cpus: u32,
    pub memory_mib: u64,
    pub rosetta: bool,
    pub fingerprint: String,
}

impl Shape {
    pub fn dir_name(&self) -> String {
        let mode = if self.rosetta { "rosetta" } else { "native" };
        format!("{}c-{}m-{mode}-{}", self.cpus, self.memory_mib, self.fingerprint)
    }

    pub fn macs_for_slot(&self, slot: usize) -> (String, String) {
        let mut h = Sha256::new();
        h.update(self.cpus.to_le_bytes());
        h.update(self.memory_mib.to_le_bytes());
        h.update([u8::from(self.rosetta), slot as u8]);
        let digest = h.finalize();
        // Locally administered unicast OUI `02:47:72` ("Gr") for eth0 NAT and
        // `06:47:72` for eth1 switch; low nibble carries the slot index so slots
        // 0..15 of a shape are guaranteed collision-free.
        let b0 = digest[0];
        let b1 = digest[1];
        let b2 = (digest[2] & 0xf0) | ((slot as u8) & 0x0f);
        (format!("02:47:72:{b0:02x}:{b1:02x}:{b2:02x}"), format!("06:47:72:{b0:02x}:{b1:02x}:{b2:02x}"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SlotMeta {
    cpus: u32,
    memory_mib: u64,
    rosetta: bool,
    fingerprint: String,
    slot: usize,
    nat_mac: String,
    switch_mac: String,
}

#[derive(Default)]
struct PoolState {
    in_use: HashSet<(String, usize)>,
    warming: HashSet<(String, usize)>,
}

/// Exclusive lease on one snapshot slot; releases the slot when the VM stops.
pub struct SnapshotLease {
    state: Arc<Mutex<PoolState>>,
    key: (String, usize),
}

impl Drop for SnapshotLease {
    fn drop(&mut self) {
        if let Ok(mut s) = self.state.lock() {
            s.in_use.remove(&self.key);
        }
    }
}

struct WarmGuard {
    state: Arc<Mutex<PoolState>>,
    key: (String, usize),
}

impl Drop for WarmGuard {
    fn drop(&mut self) {
        if let Ok(mut s) = self.state.lock() {
            s.warming.remove(&self.key);
        }
    }
}

pub struct SnapshotPool {
    root: PathBuf,
    state: Arc<Mutex<PoolState>>,
    bg_warming: Arc<AtomicBool>,
    logged_locked: AtomicBool,
    last_locked_at: Mutex<Option<Instant>>,
}

impl SnapshotPool {
    pub fn open() -> Self {
        let root = paths::snapshots_dir();
        let pool = Self {
            root,
            state: Arc::new(Mutex::new(PoolState::default())),
            bg_warming: Arc::new(AtomicBool::new(false)),
            logged_locked: AtomicBool::new(false),
            last_locked_at: Mutex::new(None),
        };
        if let Ok(fp) = guest_fingerprint(&paths::guest_dir()) {
            pool.prune_stale_fingerprints(&fp);
        }
        pool
    }

    /// Snapshots are enabled unless explicitly disabled via `GRAVITON_NO_SNAPSHOTS=1`.
    pub fn enabled(&self) -> bool {
        std::env::var("GRAVITON_NO_SNAPSHOTS").as_deref() != Ok("1")
    }

    /// Non-empty reason when snapshots are temporarily paused (for example while the
    /// macOS console session is locked and SEP keys cannot decrypt `.vzsave` files).
    pub fn pause_reason(&self) -> Option<String> {
        if !self.enabled() {
            return None;
        }
        if !AppleHypervisor::session_unlocked()
            || self.last_locked_at.lock().unwrap().is_some_and(|t| t.elapsed() < LOCKED_BACKOFF)
        {
            return Some(LOCKED_REASON.into());
        }
        None
    }

    fn slot_dir(&self, shape: &Shape, slot: usize) -> PathBuf {
        self.root.join(shape.dir_name()).join(format!("slot-{slot}"))
    }

    /// Try to restore a VM for `config` from a free cached snapshot slot.
    ///
    /// Expects `config` to already carry the container's disk, console log,
    /// `switch_port`, `multi_share_tag = Some("graviton")`, `shares`, `rosetta`,
    /// and `balloon = true`. Returns `Ok(None)` when no idle slot is ready or
    /// when a stale `.vzsave` fails to restore (after discarding the stale file).
    pub fn try_restore(&self, config: &VmConfig) -> Result<Option<(AppleVm, SnapshotLease)>> {
        if !self.enabled() {
            return Ok(None);
        }
        if let Some(reason) = self.pause_reason() {
            if !self.logged_locked.swap(true, Ordering::Relaxed) {
                eprintln!("snapshots paused: {reason}; using cold boot until unlocked");
            }
            return Ok(None);
        }
        if self.logged_locked.swap(false, Ordering::Relaxed) {
            eprintln!("snapshots resumed: macOS console session unlocked");
        }
        let guest_dir = config.kernel.parent().unwrap_or_else(|| Path::new("."));
        let Ok(fingerprint) = guest_fingerprint(guest_dir) else {
            return Ok(None);
        };
        let shape = Shape { cpus: config.cpus, memory_mib: config.memory_mib, rosetta: config.rosetta, fingerprint };
        let shape_key = shape.dir_name();

        let reserved = {
            let mut state = self.state.lock().unwrap();
            let mut found = None;
            for slot in 0..MAX_SLOTS_PER_SHAPE {
                let key = (shape_key.clone(), slot);
                if state.in_use.contains(&key) || state.warming.contains(&key) {
                    continue;
                }
                let dir = self.slot_dir(&shape, slot);
                if dir.join("state.vzsave").is_file() && dir.join("machine_id.bin").is_file() {
                    state.in_use.insert(key.clone());
                    found = Some((slot, dir, SnapshotLease { state: Arc::clone(&self.state), key }));
                    break;
                }
            }
            found
        };

        let Some((slot, dir, lease)) = reserved else {
            return Ok(None);
        };

        let machine_id = match fs::read(dir.join("machine_id.bin")) {
            Ok(bytes) if !bytes.is_empty() => bytes,
            _ => {
                let _ = fs::remove_dir_all(&dir);
                return Ok(None);
            }
        };
        let (nat_mac, switch_mac) = shape.macs_for_slot(slot);
        let mut restore_cfg = config.clone();
        restore_cfg.cmdline = SNAPSHOT_CMDLINE.into();
        restore_cfg.identity = Some(VmIdentity {
            machine_id,
            nat_mac: Some(nat_mac),
            switch_mac: Some(switch_mac),
        });

        let state_path = dir.join("state.vzsave");
        match AppleHypervisor::new().restore_vm(&restore_cfg, &state_path) {
            Ok(vm) => Ok(Some((vm, lease))),
            Err(graviton_vmm::VmmError::SnapshotLocked(msg)) => {
                *self.last_locked_at.lock().unwrap() = Some(Instant::now());
                if !self.logged_locked.swap(true, Ordering::Relaxed) {
                    eprintln!("snapshots paused ({msg}); keeping cached .vzsave slots until console unlocks");
                }
                drop(lease);
                Ok(None)
            }
            Err(err) => {
                eprintln!("snapshot restore ({shape_key}/slot-{slot}) failed: {err}; discarding stale snapshot");
                let _ = fs::remove_file(&state_path);
                drop(lease);
                Ok(None)
            }
        }
    }

    /// Warm one new snapshot slot in the background if this shape isn't full yet
    /// and no other background warm is already in progress.
    pub fn maybe_warm_background(self: &Arc<Self>, blobs_dir: PathBuf, cpus: u32, memory_mib: u64, rosetta: bool) {
        if !self.enabled() || self.pause_reason().is_some() {
            return;
        }
        if self.bg_warming.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            return;
        }
        let pool = Arc::clone(self);
        let flag = Arc::clone(&self.bg_warming);
        let _ = std::thread::Builder::new().name("snapshot-warm".into()).spawn(move || {
            let res = pool.warm_slots(&blobs_dir, cpus, memory_mib, rosetta, 1);
            flag.store(false, Ordering::SeqCst);
            if let Err(err) = res {
                eprintln!("background snapshot warm ({cpus}c/{memory_mib}m): {err:#}");
            }
        });
    }

    /// Ensure at least `target_slots` (clamped to `1..=MAX_SLOTS_PER_SHAPE`) exist
    /// for `(cpus, memory_mib, rosetta)`, warming any missing slots synchronously.
    /// When `incremental` is handled via `warm_slots(..., 1)` from background warming,
    /// warms up to `count` *additional* empty slots.
    fn check_warm_allowed(&self) -> Result<()> {
        if !self.enabled() {
            bail!("VM snapshots are disabled (GRAVITON_NO_SNAPSHOTS=1)");
        }
        if let Some(reason) = self.pause_reason() {
            bail!("cannot warm snapshots: {reason}");
        }
        Ok(())
    }

    pub fn warm_slots(&self, blobs_dir: &Path, cpus: u32, memory_mib: u64, rosetta: bool, add_count: usize) -> Result<usize> {
        self.check_warm_allowed()?;
        if rosetta && !AppleHypervisor::rosetta_available() {
            bail!("Rosetta for Linux isn't available on this Mac (run `softwareupdate --install-rosetta`)");
        }
        let guest = paths::guest_dir();
        let fingerprint = guest_fingerprint(&guest)?;
        self.prune_stale_fingerprints(&fingerprint);
        let shape = Shape { cpus, memory_mib, rosetta, fingerprint };
        let shape_key = shape.dir_name();

        let mut warmed = 0usize;
        for _ in 0..add_count.min(MAX_SLOTS_PER_SHAPE) {
            let slot_guard = {
                let mut state = self.state.lock().unwrap();
                let mut chosen = None;
                for slot in 0..MAX_SLOTS_PER_SHAPE {
                    let key = (shape_key.clone(), slot);
                    if state.in_use.contains(&key) || state.warming.contains(&key) {
                        continue;
                    }
                    if self.slot_dir(&shape, slot).join("state.vzsave").is_file() {
                        continue;
                    }
                    state.warming.insert(key.clone());
                    chosen = Some((slot, WarmGuard { state: Arc::clone(&self.state), key }));
                    break;
                }
                chosen
            };
            let Some((slot, _guard)) = slot_guard else {
                break;
            };
            self.create_slot(&guest, blobs_dir, &shape, slot)?;
            warmed += 1;
        }
        Ok(warmed)
    }

    /// Ensure at least `desired` total slots exist for `(cpus, memory_mib, rosetta)`.
    pub fn ensure_slots(&self, blobs_dir: &Path, cpus: u32, memory_mib: u64, rosetta: bool, desired: usize) -> Result<usize> {
        self.check_warm_allowed()?;
        let guest = paths::guest_dir();
        let fingerprint = guest_fingerprint(&guest)?;
        let shape = Shape { cpus, memory_mib, rosetta, fingerprint };
        let existing = (0..MAX_SLOTS_PER_SHAPE)
            .filter(|&slot| self.slot_dir(&shape, slot).join("state.vzsave").is_file())
            .count();
        let needed = desired.clamp(1, MAX_SLOTS_PER_SHAPE).saturating_sub(existing);
        if needed == 0 {
            return Ok(0);
        }
        self.warm_slots(blobs_dir, cpus, memory_mib, rosetta, needed)
    }

    fn create_slot(&self, guest: &Path, blobs_dir: &Path, shape: &Shape, slot: usize) -> Result<()> {
        let t0 = Instant::now();
        let slot_dir = self.slot_dir(shape, slot);
        fs::create_dir_all(&slot_dir).with_context(|| format!("creating {}", slot_dir.display()))?;

        let mid_path = slot_dir.join("machine_id.bin");
        let machine_id = match fs::read(&mid_path) {
            Ok(bytes) if !bytes.is_empty() => bytes,
            _ => {
                let id = AppleHypervisor::new_machine_id();
                fs::write(&mid_path, &id)?;
                id
            }
        };
        let (nat_mac, switch_mac) = shape.macs_for_slot(slot);

        // Temporary scratch workspace for the template VM (deleted as soon as state is saved).
        let work = self.root.join(format!(".warm-{}-{}-{slot}", std::process::id(), crate::runtime::new_id()));
        fs::create_dir_all(&work)?;
        let cleanup = TempDirGuard(work.clone());

        let disk = crate::runtime::scratch_disk(&work)?;
        let (vm_sock, _host_sock) = UnixDatagram::pair().context("creating template switch socket pair")?;

        let mut config = VmConfig::new(guest.join("vmlinux"), guest.join("initrd.img"), disk);
        config.cpus = shape.cpus;
        config.memory_mib = shape.memory_mib;
        config.cmdline = SNAPSHOT_CMDLINE.into();
        config.network = NetworkMode::Nat;
        config.switch_port = Some(vm_sock.as_raw_fd());
        config.console_log = Some(work.join("console.log"));
        config.shares = vec![SharedDirectory { host_path: blobs_dir.to_path_buf(), tag: "blobs".into(), read_only: true }];
        config.multi_share_tag = Some("graviton".into());
        config.rosetta = shape.rosetta;
        config.balloon = true;
        config.image_disk = Some(self.ensure_ro_placeholder()?);
        config.identity = Some(VmIdentity {
            machine_id,
            nat_mac: Some(nat_mac.clone()),
            switch_mac: Some(switch_mac.clone()),
        });

        let vm = AppleHypervisor::new().boot_vm_saveable(&config).context("booting snapshot template VM")?;
        // Wait until graviton-agent has booted, initialized Rosetta (if attached),
        // and started listening on vsock; then close the connection so the saved
        // VM has zero open client sockets.
        {
            let mut conn = File::from(vm.connect(AGENT_VSOCK_PORT).context("connecting to template VM agent")?);
            match crate::runtime::read_event_unbuffered(&mut conn)? {
                AgentEvent::Ready { .. } => {}
                other => bail!("expected ready from template VM agent, got {other:?}"),
            }
        }
        // Let the agent's serve() thread observe EOF and return to idle before pausing.
        std::thread::sleep(Duration::from_millis(15));

        vm.pause().context("pausing template VM for snapshot")?;
        let tmp_state = slot_dir.join("state.vzsave.tmp");
        let final_state = slot_dir.join("state.vzsave");
        vm.save_state(&tmp_state).context("saving template VM state")?;
        let _ = vm.kill();
        fs::rename(&tmp_state, &final_state)?;

        let meta = SlotMeta {
            cpus: shape.cpus,
            memory_mib: shape.memory_mib,
            rosetta: shape.rosetta,
            fingerprint: shape.fingerprint.clone(),
            slot,
            nat_mac,
            switch_mac,
        };
        let _ = fs::write(slot_dir.join("meta.json"), serde_json::to_vec_pretty(&meta)?);
        drop(cleanup);
        eprintln!(
            "snapshot warmed: {}/slot-{slot} in {} ms ({:.1} MiB on disk)",
            shape.dir_name(),
            t0.elapsed().as_millis(),
            file_allocated_bytes(&final_state) as f64 / 1_048_576.0
        );
        Ok(())
    }

    /// Inspect all cached snapshot slots and their disk usage.
    pub fn status(&self) -> SnapshotInfo {
        let active_fp = guest_fingerprint(&paths::guest_dir()).ok();
        let state = self.state.lock().unwrap();
        let mut groups: BTreeMap<(u32, u64, bool), SnapshotGroup> = BTreeMap::new();
        let mut total_slots = 0u32;
        let mut in_use = 0u32;
        let mut total_bytes = 0u64;

        let paused_reason = self.pause_reason();
        let Ok(shape_dirs) = fs::read_dir(&self.root) else {
            return SnapshotInfo {
                enabled: self.enabled(),
                total_slots: 0,
                in_use: 0,
                total_bytes: 0,
                max_slots_per_shape: MAX_SLOTS_PER_SHAPE as u32,
                groups: vec![],
                paused_reason,
            };
        };

        for entry in shape_dirs.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let Ok(slots) = fs::read_dir(entry.path()) else { continue };
            for slot_entry in slots.flatten() {
                let slot_path = slot_entry.path();
                let save = slot_path.join("state.vzsave");
                if !save.is_file() {
                    continue;
                }
                let Ok(meta_bytes) = fs::read(slot_path.join("meta.json")) else { continue };
                let Ok(meta) = serde_json::from_slice::<SlotMeta>(&meta_bytes) else { continue };
                if active_fp.as_deref().is_some_and(|fp| meta.fingerprint != fp) {
                    continue;
                }
                let bytes = file_allocated_bytes(&save);
                let busy = state.in_use.contains(&(name.clone(), meta.slot));
                total_slots += 1;
                if busy {
                    in_use += 1;
                }
                total_bytes += bytes;
                let g = groups.entry((meta.cpus, meta.memory_mib, meta.rosetta)).or_insert_with(|| SnapshotGroup {
                    cpus: meta.cpus,
                    memory_mib: meta.memory_mib,
                    rosetta: meta.rosetta,
                    slots: 0,
                    in_use: 0,
                    size_bytes: 0,
                });
                g.slots += 1;
                if busy {
                    g.in_use += 1;
                }
                g.size_bytes += bytes;
            }
        }

        SnapshotInfo {
            enabled: self.enabled(),
            total_slots,
            in_use,
            total_bytes,
            max_slots_per_shape: MAX_SLOTS_PER_SHAPE as u32,
            groups: groups.into_values().collect(),
            paused_reason,
        }
    }

    /// Total cached snapshot count and actual disk bytes (for `grav df`).
    pub fn disk_usage(&self) -> (u32, u64) {
        let info = self.status();
        (info.total_slots, info.total_bytes)
    }

    /// Delete all idle (not currently leased or warming) snapshots. Returns `(removed_slots, freed_bytes)`.
    pub fn clear_idle(&self, dry_run: bool) -> (u32, u64) {
        let state = self.state.lock().unwrap();
        let Ok(shape_dirs) = fs::read_dir(&self.root) else { return (0, 0) };
        let mut removed = 0u32;
        let mut freed = 0u64;

        for entry in shape_dirs.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if name.starts_with('.') {
                continue;
            }
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let Ok(slots) = fs::read_dir(&path) else { continue };
            let mut kept_any = false;
            for slot_entry in slots.flatten() {
                let slot_name = slot_entry.file_name().to_string_lossy().into_owned();
                let slot_num = slot_name.strip_prefix("slot-").and_then(|n| n.parse::<usize>().ok());
                let key = slot_num.map(|s| (name.clone(), s));
                let busy = key.as_ref().is_some_and(|k| state.in_use.contains(k) || state.warming.contains(k));
                if busy {
                    kept_any = true;
                    continue;
                }
                let save = slot_entry.path().join("state.vzsave");
                if save.is_file() {
                    removed += 1;
                    freed += file_allocated_bytes(&save);
                }
                if !dry_run {
                    let _ = fs::remove_dir_all(slot_entry.path());
                }
            }
            if !dry_run && !kept_any {
                let _ = fs::remove_dir_all(&path);
            }
        }
        (removed, freed)
    }

    fn ensure_ro_placeholder(&self) -> Result<PathBuf> {
        let path = self.root.join(".placeholder-ro-16g.img");
        let wanted = crate::erofs::EROFS_LOGICAL_BYTES;
        if path.is_file() && fs::metadata(&path).is_ok_and(|m| m.len() == wanted) {
            return Ok(path);
        }
        fs::create_dir_all(&self.root)?;
        let tmp = self.root.join(format!(".placeholder-ro-16g.{}.tmp", std::process::id()));
        let f = File::create(&tmp)?;
        f.set_len(wanted)?;
        fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// Drop any snapshot directories built for an older guest kernel/initrd or macOS build.
    fn prune_stale_fingerprints(&self, active_fp: &str) {
        let Ok(entries) = fs::read_dir(&self.root) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if !name.ends_with(&format!("-{active_fp}")) {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
}

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Fingerprint of the active guest kernel + initrd + host macOS build version.
/// Any change automatically invalidates cached `.vzsave` files.
pub fn guest_fingerprint(guest_dir: &Path) -> Result<String> {
    let mut h = Sha256::new();
    h.update(b"v2-erofs");
    for name in ["vmlinux", "initrd.img"] {
        let meta = fs::metadata(guest_dir.join(name))
            .with_context(|| format!("reading guest image metadata ({name})"))?;
        h.update(name.as_bytes());
        h.update(meta.len().to_le_bytes());
        if let Ok(mtime) = meta.modified().and_then(|t| t.duration_since(UNIX_EPOCH).map_err(|e| std::io::Error::other(e.to_string()))) {
            h.update(mtime.as_secs().to_le_bytes());
            h.update(mtime.subsec_nanos().to_le_bytes());
        }
    }
    if let Some(os) = host_os_build() {
        h.update(os.as_bytes());
    }
    let hex: String = h.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect();
    Ok(hex)
}

fn host_os_build() -> Option<String> {
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: sysctlbyname("kern.osversion") writes a NUL-terminated ASCII build string.
    let rc = unsafe { libc::sysctlbyname(c"kern.osversion".as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
    if rc == 0 && len > 0 {
        let s = std::str::from_utf8(&buf[..len]).ok()?.trim_end_matches('\0');
        Some(s.to_string())
    } else {
        None
    }
}

fn file_allocated_bytes(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |m| {
        let alloc = m.blocks() * 512;
        if alloc > 0 { alloc } else { m.len() }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_macs_are_unique_and_locally_administered() {
        let shape = Shape { cpus: 2, memory_mib: 1024, rosetta: false, fingerprint: "abcdef012345".into() };
        let mut seen = HashSet::new();
        for slot in 0..MAX_SLOTS_PER_SHAPE {
            let (nat, sw) = shape.macs_for_slot(slot);
            assert!(nat.starts_with("02:47:72:"));
            assert!(sw.starts_with("06:47:72:"));
            assert_ne!(nat, sw);
            assert!(seen.insert(nat));
            assert!(seen.insert(sw));
        }
        // Different shape produces different MACs even for slot 0.
        let rosetta_shape = Shape { cpus: 2, memory_mib: 1024, rosetta: true, fingerprint: "abcdef012345".into() };
        assert_ne!(shape.macs_for_slot(0), rosetta_shape.macs_for_slot(0));
    }
}
