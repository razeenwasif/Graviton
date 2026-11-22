//! Disk space: `grav df`, `grav rmi` and `grav prune`.
//!
//! Every image layer, config and manifest is a file in the blob store,
//! shared by whatever refers to it: images in the index, containers (each
//! keeps the image it was started from, so it can start again even if that
//! image is removed later) and the build cache. A blob is deleted only when
//! none of them refers to it, and only once it's at least `FRESH` old, so a
//! pull or build that has written layers but not yet recorded them keeps them.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use graviton_core::paths;
use graviton_image::LocalImage;
use graviton_proto::{ContainerStatus, DiskUsage, PruneReport, PruneSpec};

use crate::runtime::Engine;

/// Blobs younger than this are left alone (a pull or build may be about to record them).
const FRESH: Duration = Duration::from_secs(10 * 60);

fn image_blobs(image: &LocalImage) -> impl Iterator<Item = String> + '_ {
    image.all_blob_digests().into_iter()
}

/// What refers to which blobs.
struct References {
    /// Images some container uses, and the images containers were started from.
    in_use: HashSet<String>,
    /// Images no container uses.
    unused_images: HashSet<String>,
    build_cache: HashSet<String>,
    build_cache_steps: u32,
}

impl References {
    fn gather(engine: &Engine) -> Self {
        let mut in_use = HashSet::new();
        let mut unused_images = HashSet::new();
        for spec in engine.specs.lock().unwrap().values() {
            in_use.extend(image_blobs(&spec.image));
        }
        let images: Vec<LocalImage> = engine.index.lock().unwrap().images().cloned().collect();
        for image in &images {
            if engine.used_by(&image.reference) > 0 {
                in_use.extend(image_blobs(image));
            } else {
                unused_images.extend(image_blobs(image));
            }
        }
        let (build_cache, build_cache_steps) = crate::build::cache_contents();
        Self { in_use, unused_images, build_cache, build_cache_steps }
    }
}

/// `sha256:<hex>` → (size, old enough to delete), for every complete blob.
fn blobs(engine: &Engine) -> Vec<(String, u64, bool)> {
    let dir = engine.blobs.root().join("sha256");
    let Ok(entries) = fs::read_dir(&dir) else { return vec![] };
    let now = SystemTime::now();
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name.len() != 64 {
                return None; // partial downloads
            }
            let meta = e.metadata().ok()?;
            let old = meta.modified().ok().and_then(|m| now.duration_since(m).ok()).is_some_and(|age| age >= FRESH);
            Some((format!("sha256:{name}"), meta.len(), old))
        })
        .collect()
}

fn delete_blob(engine: &Engine, digest: &str) -> bool {
    let hex = digest.trim_start_matches("sha256:");
    fs::remove_file(engine.blobs.root().join("sha256").join(hex)).is_ok()
}

fn disk_of(id: &str) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(paths::vms_dir().join(id).join(crate::runtime::SCRATCH_DISK)).map_or(0, |m| m.blocks() * 512)
}

pub fn disk_usage(engine: &Engine) -> DiskUsage {
    let refs = References::gather(engine);
    let mut usage = DiskUsage::default();
    for (digest, size, _) in blobs(engine) {
        usage.blobs_bytes += size;
        let in_use = refs.in_use.contains(&digest);
        if !in_use && refs.unused_images.contains(&digest) && !refs.build_cache.contains(&digest) {
            usage.unused_images_bytes += size;
        }
        if !in_use && !refs.unused_images.contains(&digest) && refs.build_cache.contains(&digest) {
            usage.build_cache_bytes += size;
        }
        if !in_use && !refs.unused_images.contains(&digest) && !refs.build_cache.contains(&digest) {
            usage.unreferenced_bytes += size;
        }
    }
    let index = engine.index.lock().unwrap();
    usage.images = index.images().count() as u32;
    usage.unused_images = index.images().filter(|i| engine.used_by(&i.reference) == 0).count() as u32;
    drop(index);
    for c in engine.store.lock().unwrap().containers() {
        let disk = disk_of(&c.id);
        usage.containers += 1;
        usage.container_disks_bytes += disk;
        if c.status == ContainerStatus::Exited {
            usage.stopped_containers += 1;
            usage.stopped_disks_bytes += disk;
        }
    }
    usage.build_cache_steps = refs.build_cache_steps;
    usage.build_cache_bytes += crate::build::cache_mounts_bytes();
    let (snapshots, snapshots_bytes) = engine.snapshots.disk_usage();
    usage.snapshots = snapshots;
    usage.snapshots_bytes = snapshots_bytes;
    usage
}

/// Remove an image no container uses. Returns (freed, kept for the build cache).
pub fn remove_image(engine: &Engine, reference: &str) -> Result<(String, u64, u64)> {
    let image = engine
        .index
        .lock()
        .unwrap()
        .get(reference)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no image {reference} here (see grav images)"))?;
    let users: Vec<String> = {
        let specs = engine.specs.lock().unwrap();
        let store = engine.store.lock().unwrap();
        specs
            .iter()
            .filter(|(_, s)| s.image.reference == image.reference)
            .filter_map(|(id, _)| store.container(id).ok().map(|c| c.name.clone()))
            .collect()
    };
    if !users.is_empty() {
        bail!("{reference} is used by {}: remove {} first (grav rm)", users.join(", "), if users.len() == 1 { "it" } else { "them" });
    }
    engine.index.lock().unwrap().remove(&image.reference)?;
    engine.refresh_images();
    let refs = References::gather(engine);
    let (mut freed, mut kept) = (0, 0);
    for (digest, size, _) in blobs(engine).into_iter().filter(|(d, _, _)| image_blobs(&image).any(|b| b == *d)) {
        if refs.in_use.contains(&digest) || refs.unused_images.contains(&digest) {
            continue; // another image shares it
        }
        if refs.build_cache.contains(&digest) {
            kept += size;
        } else if delete_blob(engine, &digest) {
            freed += size;
        }
    }
    Ok((image.reference, freed, kept))
}

pub fn prune(engine: &Engine, spec: &PruneSpec) -> Result<PruneReport> {
    let mut report = PruneReport { dry_run: spec.dry_run, ..PruneReport::default() };

    // 1. Stopped containers, with their disks.
    let mut gone: HashSet<String> = HashSet::new();
    if spec.containers {
        let stopped: Vec<(String, String)> = engine
            .store
            .lock()
            .unwrap()
            .containers()
            .iter()
            .filter(|c| c.status == ContainerStatus::Exited)
            .map(|c| (c.id.clone(), c.name.clone()))
            .collect();
        for (id, name) in stopped {
            report.freed_bytes += disk_of(&id);
            if !spec.dry_run {
                engine.remove(&id)?;
            }
            gone.insert(id);
            report.containers.push(name);
        }
    }

    // What the containers that remain still need: the images they use and the images they were started from.
    let (mut in_use, used_refs): (HashSet<String>, HashSet<String>) = {
        let specs = engine.specs.lock().unwrap();
        let remaining = specs.iter().filter(|(id, _)| !gone.contains(*id)).map(|(_, s)| s);
        let mut blobs = HashSet::new();
        let mut refs = HashSet::new();
        for s in remaining {
            blobs.extend(image_blobs(&s.image));
            refs.insert(s.image.reference.clone());
        }
        (blobs, refs)
    };

    // 2. Images no remaining container uses.
    let images: Vec<LocalImage> = engine.index.lock().unwrap().images().cloned().collect();
    for image in &images {
        if used_refs.contains(&image.reference) {
            in_use.extend(image_blobs(image));
        } else if spec.images {
            if !spec.dry_run {
                engine.index.lock().unwrap().remove(&image.reference)?;
            }
            report.images.push(image.reference.clone());
        } else {
            in_use.extend(image_blobs(image)); // kept
        }
    }
    if spec.images && !spec.dry_run {
        engine.refresh_images();
    }

    // 3. The build cache (step index + persistent RUN --mount=type=cache dirs).
    let (_, steps) = crate::build::cache_contents();
    report.build_cache_steps = steps;
    let cache_mounts = crate::build::cache_mounts_bytes();
    if !spec.dry_run {
        crate::build::clear_cache();
        crate::build::clear_cache_mounts();
    }
    report.freed_bytes += cache_mounts;

    // 4. Blobs nothing refers to any more (and old enough not to belong to a pull or build in progress).
    for (digest, size, old) in blobs(engine) {
        if in_use.contains(&digest) || !old {
            continue;
        }
        if spec.dry_run || delete_blob(engine, &digest) {
            report.blobs += 1;
            report.freed_bytes += size;
        }
    }

    // 5. Idle cached VM snapshots (automatically re-warmed on demand).
    let (snap_count, snap_bytes) = engine.snapshots.clear_idle(spec.dry_run);
    report.snapshots = snap_count;
    report.freed_bytes += snap_bytes;

    if !spec.dry_run {
        remove_stale_builds(&paths::support_dir().join("builds"));
    }
    Ok(report)
}

/// Build folders and uploads left by builds that crashed or were interrupted.
fn remove_stale_builds(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let now = SystemTime::now();
    for e in entries.flatten() {
        let old = e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|m| now.duration_since(m).ok()).is_some_and(|a| a >= Duration::from_secs(3600));
        if old {
            let p = e.path();
            let _ = if p.is_dir() { fs::remove_dir_all(&p) } else { fs::remove_file(&p) };
        }
    }
}
