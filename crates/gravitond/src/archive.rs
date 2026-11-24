//! Image export (`grav save`), import (`grav load`), and container commit (`grav commit`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use graviton_core::{ImageReference, paths};
use graviton_image::dockerfile::{Dockerfile, Kind};
use graviton_image::{BlobStore, Digest, ImageIndex, ImageVariant, Layer, LocalImage, guest_arch, normalize_platform};
use graviton_proto::{
    AGENT_VSOCK_PORT, AgentEvent, AgentRequest, CommitSpec, ContainerStatus, EventKind, ImageSummary, encode_line,
};
use graviton_vmm::apple::AppleHypervisor;
use graviton_vmm::{NetworkMode, SharedDirectory, VmConfig, VmHandle};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::build::{self, BuiltLayer, GZIP_LAYER};
use crate::runtime::{Engine, now_secs, read_event_unbuffered};

pub const MAX_IMAGE_LOAD: u64 = 16 * 1024 * 1024 * 1024;

const OCI_MANIFEST_MEDIA: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_INDEX_MEDIA: &str = "application/vnd.oci.image.index.v1+json";
const ZSTD_LAYER_MEDIA: &str = "application/vnd.oci.image.layer.v1.tar+zstd";

pub fn resolve_save_images(engine: &Engine, references: &[String]) -> Result<Vec<LocalImage>> {
    if references.is_empty() {
        bail!("at least one image name is required");
    }
    let mut images = Vec::with_capacity(references.len());
    for reference in references {
        let img = engine
            .resolve_local_image(reference)
            .ok_or_else(|| anyhow!("No such image: {reference}"))?;
        images.push(img);
    }
    Ok(images)
}

pub fn save_images_from_store(blobs: &BlobStore, images: &[LocalImage], writer: impl Write) -> Result<()> {
    let mut tar = tar::Builder::new(writer);
    let mut seen_blobs = HashSet::new();
    let mut index_manifests = Vec::with_capacity(images.len());
    let mut docker_manifest = Vec::with_capacity(images.len());

    append_tar_bytes(&mut tar, "oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#)?;

    for img in images {
        let display_name = ImageReference::parse(&img.reference)
            .map(|r| r.display_name())
            .unwrap_or_else(|_| img.reference.clone());

        for digest in img.all_blob_digests() {
            if let Ok(d) = Digest::parse(&digest) {
                if blobs.has(&d) {
                    append_blob_file(&mut tar, blobs, &d, &mut seen_blobs)?;
                }
            }
        }

        let cfg_digest = Digest::parse(&img.config_digest)?;
        let cfg_size = blobs
            .size(&cfg_digest)
            .ok_or_else(|| anyhow!("missing config blob {} for {}", cfg_digest, img.reference))?;
        append_blob_file(&mut tar, blobs, &cfg_digest, &mut seen_blobs)?;

        let (top_digest, top_size, top_media) = if img.is_multi_arch() {
            let idx_d = Digest::parse(&img.manifest_digest)?;
            let idx_size = blobs
                .size(&idx_d)
                .ok_or_else(|| anyhow!("missing index manifest blob {} for {}", idx_d, img.reference))?;
            append_blob_file(&mut tar, blobs, &idx_d, &mut seen_blobs)?;
            (idx_d, idx_size, OCI_INDEX_MEDIA)
        } else {
            let mut m_d = Digest::parse(&img.manifest_digest)?;
            if !blobs.has(&m_d) {
                let synth = serde_json::to_vec(&json!({
                    "schemaVersion": 2,
                    "mediaType": OCI_MANIFEST_MEDIA,
                    "config": {
                        "mediaType": "application/vnd.oci.image.config.v1+json",
                        "digest": cfg_digest.as_str(),
                        "size": cfg_size,
                    },
                    "layers": img.layers.iter().map(|l| json!({
                        "mediaType": l.media_type,
                        "digest": l.digest,
                        "size": l.size,
                    })).collect::<Vec<_>>(),
                }))?;
                m_d = Digest::of(&synth);
                if seen_blobs.insert(m_d.as_str().to_string()) {
                    append_tar_bytes(&mut tar, &format!("blobs/sha256/{}", m_d.hex()), &synth)?;
                }
            }
            let size = blobs.size(&m_d).unwrap_or(0);
            (m_d, size, OCI_MANIFEST_MEDIA)
        };

        index_manifests.push(json!({
            "mediaType": top_media,
            "digest": top_digest.as_str(),
            "size": top_size,
            "annotations": {
                "org.opencontainers.image.ref.name": display_name,
                "io.containerd.image.name": img.reference,
            },
        }));

        let layer_paths: Vec<String> = img
            .layers
            .iter()
            .filter_map(|l| Digest::parse(&l.digest).ok().map(|d| format!("blobs/sha256/{}", d.hex())))
            .collect();
        docker_manifest.push(json!({
            "Config": format!("blobs/sha256/{}", cfg_digest.hex()),
            "RepoTags": [display_name],
            "Layers": layer_paths,
        }));
    }

    let index_bytes = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": OCI_INDEX_MEDIA,
        "manifests": index_manifests,
    }))?;
    append_tar_bytes(&mut tar, "index.json", &index_bytes)?;

    let docker_bytes = serde_json::to_vec(&docker_manifest)?;
    append_tar_bytes(&mut tar, "manifest.json", &docker_bytes)?;

    tar.finish()?;
    Ok(())
}

pub fn load_images_from_path(engine: &Engine, tar_path: &Path) -> Result<Vec<ImageSummary>> {
    let loaded = {
        let mut index = engine.index.lock().unwrap();
        load_images_into_store(&engine.blobs, &mut index, tar_path)?
    };
    engine.refresh_images();
    let mut summaries = Vec::with_capacity(loaded.len());
    for img in &loaded {
        let disp = ImageReference::parse(&img.reference)
            .map(|r| r.display_name())
            .unwrap_or_else(|_| img.reference.clone());
        let summary = img.summary(engine.used_by(&img.reference));
        engine.events.record(
            EventKind::Pulled,
            None,
            Some(&disp),
            format!("Loaded {disp} ({})", build::human(summary.size_bytes)),
        );
        summaries.push(summary);
    }
    Ok(summaries)
}

pub fn load_images_into_store(blobs: &BlobStore, index: &mut ImageIndex, tar_path: &Path) -> Result<Vec<LocalImage>> {
    let stage = TempDirGuard::new("graviton-load")?;
    let mut file = File::open(tar_path).with_context(|| format!("opening {}", tar_path.display()))?;
    let mut magic = [0u8; 2];
    let n = file.read(&mut magic)?;
    file.seek(SeekFrom::Start(0))?;

    if n == 2 && magic == [0x1f, 0x8b] {
        unpack_archive_stream(blobs, &stage.path, GzDecoder::new(BufReader::new(file)))?;
    } else {
        unpack_archive_stream(blobs, &stage.path, BufReader::new(file))?;
    }

    let images = if stage.path.join("index.json").is_file() {
        load_from_oci_layout(blobs, &stage.path)?
    } else if stage.path.join("manifest.json").is_file() {
        load_from_docker_manifest(blobs, &stage.path)?
    } else {
        bail!("archive has neither index.json nor manifest.json");
    };

    if images.is_empty() {
        bail!("archive contains no loadable images");
    }
    for img in &images {
        index.insert(img.clone())?;
    }
    Ok(images)
}

fn unpack_archive_stream<R: Read>(blobs: &BlobStore, stage: &Path, reader: R) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw_path = entry.path()?.into_owned();
        let Some(rel) = clean_rel_path(&raw_path) else {
            continue;
        };
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            continue;
        }
        if entry_type.is_hard_link() {
            if let Some(target_raw) = entry.link_name()? {
                if let Some(target_rel) = clean_rel_path(&target_raw) {
                    let dst = stage.join(&rel);
                    let src = stage.join(&target_rel);
                    if src.is_file() {
                        if let Some(p) = dst.parent() {
                            fs::create_dir_all(p)?;
                        }
                        let _ = fs::hard_link(&src, &dst).or_else(|_| fs::copy(&src, &dst).map(|_| ()));
                    }
                }
            }
            continue;
        }
        if !entry_type.is_file() {
            continue;
        }

        if let Some(digest) = blob_digest_from_rel(&rel) {
            if !blobs.has(&digest) {
                blobs
                    .write_verified(&digest, &mut entry, |_| {})
                    .with_context(|| format!("verifying blob {digest}"))?;
            }
            continue;
        }

        let dst = stage.join(&rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&dst)?;
        io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}

fn load_from_oci_layout(blobs: &BlobStore, stage: &Path) -> Result<Vec<LocalImage>> {
    let raw_index = fs::read(stage.join("index.json")).context("reading index.json")?;
    let index_doc: Value = serde_json::from_slice(&raw_index).context("parsing index.json")?;
    let manifest_tags = read_docker_manifest_tags(stage);
    let manifests = index_doc["manifests"]
        .as_array()
        .ok_or_else(|| anyhow!("index.json has no manifests array"))?;

    let now = now_secs();
    let mut loaded = Vec::new();
    for desc in manifests {
        let Some(digest_str) = desc["digest"].as_str() else { continue };
        let top_digest = Digest::parse(digest_str)?;
        if !blobs.has(&top_digest) {
            bail!("archive is missing manifest blob {top_digest}");
        }
        let raw_top = blobs.read(&top_digest)?;
        let top_doc: Value = serde_json::from_slice(&raw_top).with_context(|| format!("parsing manifest {top_digest}"))?;

        let annotations = &desc["annotations"];
        let ctd_name = annotations["io.containerd.image.name"].as_str().map(str::trim).filter(|s| !s.is_empty());
        let ref_name = annotations["org.opencontainers.image.ref.name"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        if let Some(child_descs) = top_doc["manifests"].as_array() {
            let mut variants = Vec::new();
            for child in child_descs {
                if child["platform"]["architecture"].as_str() == Some("unknown") {
                    continue;
                }
                let Some(cd_str) = child["digest"].as_str() else { continue };
                let cd = Digest::parse(cd_str)?;
                if !blobs.has(&cd) {
                    continue;
                }
                let raw_child = blobs.read(&cd)?;
                let child_doc: Value = serde_json::from_slice(&raw_child)?;
                let Some(cfg_str) = child_doc["config"]["digest"].as_str() else { continue };
                let Some(layers_arr) = child_doc["layers"].as_array() else { continue };
                if layers_arr.is_empty() && child_doc["config"]["mediaType"].as_str() != Some("application/vnd.oci.image.config.v1+json") {
                    continue;
                }
                let cfg_d = Digest::parse(cfg_str)?;
                if !blobs.has(&cfg_d) {
                    bail!("archive is missing config blob {cfg_d}");
                }
                let cfg_json: Value = serde_json::from_slice(&blobs.read(&cfg_d)?)?;
                let layers = parse_verified_layers(blobs, layers_arr)?;
                let arch = arch_from_config(&cfg_json).or_else(|| child["platform"]["architecture"].as_str().and_then(norm_arch));
                variants.push(ImageVariant {
                    architecture: arch.unwrap_or_else(|| guest_arch().to_string()),
                    manifest_digest: cd.to_string(),
                    config_digest: cfg_d.to_string(),
                    layers,
                });
            }
            if variants.is_empty() {
                continue;
            }
            let primary_idx = variants.iter().position(|v| v.architecture == guest_arch()).unwrap_or(0);
            let primary = &variants[primary_idx];
            let tags = manifest_tags.get(&primary.config_digest);
            let refs = resolve_oci_references(ctd_name, ref_name, tags, &top_digest)?;
            for reference in refs {
                loaded.push(LocalImage {
                    reference,
                    manifest_digest: top_digest.to_string(),
                    config_digest: primary.config_digest.clone(),
                    layers: primary.layers.clone(),
                    architecture: primary.architecture.clone(),
                    pulled_at: now,
                    variants: if variants.len() > 1 { variants.clone() } else { Vec::new() },
                });
            }
        } else if let (Some(cfg_str), Some(layers_arr)) = (top_doc["config"]["digest"].as_str(), top_doc["layers"].as_array()) {
            let cfg_d = Digest::parse(cfg_str)?;
            if !blobs.has(&cfg_d) {
                bail!("archive is missing config blob {cfg_d}");
            }
            let cfg_json: Value = serde_json::from_slice(&blobs.read(&cfg_d)?)?;
            let layers = parse_verified_layers(blobs, layers_arr)?;
            let arch = arch_from_config(&cfg_json).unwrap_or_else(|| guest_arch().to_string());
            let tags = manifest_tags.get(cfg_d.as_str());
            let refs = resolve_oci_references(ctd_name, ref_name, tags, &top_digest)?;
            for reference in refs {
                loaded.push(LocalImage {
                    reference,
                    manifest_digest: top_digest.to_string(),
                    config_digest: cfg_d.to_string(),
                    layers: layers.clone(),
                    architecture: arch.clone(),
                    pulled_at: now,
                    variants: Vec::new(),
                });
            }
        }
    }
    Ok(loaded)
}

fn load_from_docker_manifest(blobs: &BlobStore, stage: &Path) -> Result<Vec<LocalImage>> {
    let raw = fs::read(stage.join("manifest.json")).context("reading manifest.json")?;
    let items: Vec<Value> = serde_json::from_slice(&raw).context("parsing manifest.json")?;
    let now = now_secs();
    let mut loaded = Vec::new();

    for item in items {
        let cfg_path_str = item["Config"]
            .as_str()
            .ok_or_else(|| anyhow!("manifest.json entry missing Config"))?;
        let (cfg_digest, cfg_bytes) = if let Some(d) = clean_rel_path(Path::new(cfg_path_str)).and_then(|r| blob_digest_from_rel(&r)) {
            if !blobs.has(&d) {
                bail!("missing config blob {d}");
            }
            (d.clone(), blobs.read(&d)?)
        } else {
            let rel = clean_rel_path(Path::new(cfg_path_str)).ok_or_else(|| anyhow!("bad config path {cfg_path_str}"))?;
            let bytes = fs::read(stage.join(rel)).with_context(|| format!("reading {cfg_path_str}"))?;
            let d = Digest::of(&bytes);
            blobs.write_bytes(&d, &bytes)?;
            (d, bytes)
        };
        let cfg_json: Value = serde_json::from_slice(&cfg_bytes).context("parsing image config")?;
        let arch = arch_from_config(&cfg_json).unwrap_or_else(|| guest_arch().to_string());

        let layer_entries = item["Layers"]
            .as_array()
            .ok_or_else(|| anyhow!("manifest.json entry missing Layers"))?;
        let mut layers = Vec::with_capacity(layer_entries.len());
        for l_val in layer_entries {
            let l_str = l_val.as_str().ok_or_else(|| anyhow!("bad layer entry in manifest.json"))?;
            let rel = clean_rel_path(Path::new(l_str)).ok_or_else(|| anyhow!("bad layer path {l_str}"))?;
            if let Some(d) = blob_digest_from_rel(&rel) {
                let size = blobs.size(&d).ok_or_else(|| anyhow!("missing layer blob {d}"))?;
                let media_type = detect_blob_layer_media(&blobs.path(&d))?;
                layers.push(Layer { digest: d.to_string(), size, media_type });
            } else {
                let layer_file = stage.join(rel);
                let (d, size, media_type) = store_staged_layer(blobs, &layer_file)?;
                layers.push(Layer { digest: d.to_string(), size, media_type });
            }
        }

        let manifest_bytes = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST_MEDIA,
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": cfg_digest.as_str(),
                "size": cfg_bytes.len(),
            },
            "layers": layers.iter().map(|l| json!({
                "mediaType": l.media_type,
                "digest": l.digest,
                "size": l.size,
            })).collect::<Vec<_>>(),
        }))?;
        let m_digest = Digest::of(&manifest_bytes);
        blobs.write_bytes(&m_digest, &manifest_bytes)?;

        let mut refs: Vec<String> = item["RepoTags"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|t| *t != "<none>:<none>" && !t.is_empty())
                    .filter_map(|t| ImageReference::parse(t).ok().map(|r| r.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        if refs.is_empty() {
            refs.push(ImageReference::parse(&format!("loaded:{}", &cfg_digest.hex()[..12]))?.to_string());
        }
        for reference in refs {
            loaded.push(LocalImage {
                reference,
                manifest_digest: m_digest.to_string(),
                config_digest: cfg_digest.to_string(),
                layers: layers.clone(),
                architecture: arch.clone(),
                pulled_at: now,
                variants: Vec::new(),
            });
        }
    }
    Ok(loaded)
}

pub fn commit_container(engine: &Engine, spec: &CommitSpec) -> Result<ImageSummary> {
    let (id, name, status) = {
        let store = engine.store.lock().unwrap();
        let c = store.container(&spec.container)?;
        (c.id.clone(), c.name.clone(), c.status)
    };
    if status == ContainerStatus::Paused {
        bail!("container {name} is paused; unpause or stop it before committing");
    }

    let target_raw = if spec.reference.trim().is_empty() {
        format!("committed:{}", &id[..id.len().min(12)])
    } else {
        spec.reference.trim().to_string()
    };
    let parsed_ref = ImageReference::parse(&target_raw).with_context(|| format!("`{target_raw}` isn't a valid image name"))?;
    if parsed_ref.digest.is_some() {
        bail!("cannot commit to a digest reference; use NAME[:TAG]");
    }
    let canonical_ref = parsed_ref.to_string();

    let cspec = engine
        .specs
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .ok_or_else(|| anyhow!("container {name} has no saved configuration"))?;
    let base_img = cspec.image;
    let raw_cfg = engine.blobs.read(&Digest::parse(&base_img.config_digest)?)?;
    let base_cfg: Value = serde_json::from_slice(&raw_cfg).context("reading container base image config")?;
    let mut state = build::state_from_base(&base_img, base_cfg)?;
    let now = now_secs();
    state.created = Some(now);
    if let Some(author) = spec.author.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        state.author = Some(author.to_string());
    }

    if !spec.changes.is_empty() {
        apply_commit_changes(&mut state, &spec.changes)?;
    }

    let disk_path = paths::vms_dir().join(&id).join("disk.img");
    let captured_layer = if status == ContainerStatus::Running {
        let (_, _, fd) = engine.get_archive_open(&id, "/")?;
        let mut reader = File::from(fd);
        Some(compress_tar_stream_to_blob(&engine.blobs, &mut reader)?)
    } else if disk_path.is_file() {
        Some(commit_stopped_container(&engine.blobs, &id, &base_img, &disk_path)?)
    } else {
        None
    };

    if let Some(layer) = captured_layer {
        state.layers = vec![layer];
    }
    let mut hist = json!({
        "created": build::rfc3339(now),
        "created_by": "grav commit",
    });
    if let Some(author) = spec.author.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        hist["author"] = json!(author);
    }
    if let Some(comment) = spec.message.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        hist["comment"] = json!(comment);
    }
    state.history.push(hist);

    let committed = build::write_image_with_creator(engine, &canonical_ref, &state, "grav commit")?;
    engine.index.lock().unwrap().insert(committed.clone())?;
    engine.refresh_images();
    let disp = ImageReference::parse(&committed.reference)
        .map(|r| r.display_name())
        .unwrap_or_else(|_| committed.reference.clone());
    engine.events.record(
        EventKind::Built,
        Some((id.as_str(), name.as_str())),
        Some(&disp),
        format!("Committed {name} -> {disp}"),
    );
    Ok(committed.summary(engine.used_by(&committed.reference)))
}

pub(crate) fn apply_commit_changes(state: &mut build::State, changes: &[String]) -> Result<()> {
    let mut src = String::from("FROM scratch\n");
    for line in changes {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            src.push_str(trimmed);
            src.push('\n');
        }
    }
    let df = Dockerfile::parse(&src).map_err(|e| anyhow!("invalid commit change: {e}"))?;
    let empty_args = BTreeMap::new();
    for ins in &df.stages[0].instructions {
        let vars = state.vars();
        match &ins.kind {
            Kind::Run { .. } | Kind::Copy { .. } => {
                bail!("commit change `{}` is not a config instruction", ins.text);
            }
            Kind::Workdir(dir) => {
                state.workdir = build::absolute(dir, &state.workdir);
            }
            _ => build::apply_config(state, &ins.kind, &vars, &empty_args)?,
        }
    }
    Ok(())
}

fn commit_stopped_container(blobs: &BlobStore, id: &str, base_img: &LocalImage, disk_path: &Path) -> Result<BuiltLayer> {
    let guest = paths::guest_dir();
    let work = TempDirGuard::new("graviton-commit")?;
    let mut cfg = VmConfig::new(guest.join("vmlinux"), guest.join("initrd.img"), disk_path.to_path_buf());
    cfg.image_disk = Some(crate::erofs::ensure_image_disk(blobs, &base_img.layers)?);
    cfg.cpus = 2;
    cfg.memory_mib = 512;
    cfg.network = NetworkMode::None;
    cfg.balloon = false;
    cfg.console_log = Some(work.path.join("console.log"));
    cfg.shares = vec![SharedDirectory {
        host_path: blobs.root().to_path_buf(),
        tag: "blobs".into(),
        read_only: true,
    }];
    cfg.multi_share_tag = Some("graviton".into());

    let vm = AppleHypervisor::new().boot_vm(&cfg).context("booting helper VM for commit")?;
    let result = (|| -> Result<BuiltLayer> {
        let mut conn = File::from(vm.connect(AGENT_VSOCK_PORT).context("connecting to helper VM agent")?);
        match read_event_unbuffered(&mut conn)? {
            AgentEvent::Ready { .. } => {}
            other => bail!("expected ready from helper VM agent, got {other:?}"),
        }
        conn.write_all(&encode_line(&AgentRequest::Prepare {
            container: id.to_string(),
            layers: base_img.layers.iter().map(|l| l.digest.clone()).collect(),
            mounts: vec![],
            tmpfs: vec![],
            hostname: Some("commit".into()),
            now_secs: None,
        }))?;
        match read_event_unbuffered(&mut conn)? {
            AgentEvent::Prepared { .. } => {}
            AgentEvent::Error { message } => bail!("preparing container disk for commit: {message}"),
            other => bail!("expected prepared from helper VM agent, got {other:?}"),
        }
        conn.write_all(&encode_line(&AgentRequest::GetArchive {
            container: id.to_string(),
            path: "/".into(),
        }))?;
        match read_event_unbuffered(&mut conn)? {
            AgentEvent::ArchiveReady { .. } => compress_tar_stream_to_blob(blobs, &mut conn),
            AgentEvent::Error { message } => bail!("reading container rootfs for commit: {message}"),
            other => bail!("expected archive header from helper VM agent, got {other:?}"),
        }
    })();
    let _ = vm.kill();
    result
}

struct HashingWriter<W> {
    inner: W,
    hash: Sha256,
    bytes: u64,
}

impl<W: Write> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner, hash: Sha256::new(), bytes: 0 }
    }
}

impl<W: Write> Write for HashingWriter<W> {
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn partial_blob_path(blobs: &BlobStore) -> Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = blobs.root().join("sha256");
    fs::create_dir_all(&dir)?;
    Ok(dir.join(format!(".commit-{}-{}.partial", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))))
}

fn compress_tar_stream_to_blob(blobs: &BlobStore, reader: &mut impl Read) -> Result<BuiltLayer> {
    let tmp = partial_blob_path(blobs)?;
    let out = File::create(&tmp)?;
    let gz = GzEncoder::new(HashingWriter::new(out), Compression::fast());
    let mut raw = HashingWriter::new(gz);
    if let Err(err) = io::copy(reader, &mut raw) {
        let _ = fs::remove_file(&tmp);
        return Err(err.into());
    }
    let diff_id = format!("sha256:{}", hex(&raw.hash.finalize()));
    let gz = raw.inner;
    let compressed = match gz.finish() {
        Ok(c) => c,
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            return Err(err.into());
        }
    };
    compressed.inner.sync_all()?;
    let size = compressed.bytes;
    let digest = Digest::parse(&format!("sha256:{}", hex(&compressed.hash.finalize())))?;
    drop(compressed.inner);
    fs::rename(&tmp, blobs.path(&digest))?;
    Ok(BuiltLayer {
        digest: digest.to_string(),
        diff_id,
        size,
        media_type: GZIP_LAYER.into(),
    })
}

fn store_staged_layer(blobs: &BlobStore, layer_path: &Path) -> Result<(Digest, u64, String)> {
    let mut f = File::open(layer_path).with_context(|| format!("opening layer {}", layer_path.display()))?;
    let mut magic = [0u8; 4];
    let n = f.read(&mut magic)?;
    f.seek(SeekFrom::Start(0))?;

    if n >= 2 && magic[0] == 0x1f && magic[1] == 0x8b {
        let (digest, size) = copy_to_blob(blobs, &mut f)?;
        return Ok((digest, size, GZIP_LAYER.into()));
    }
    if n >= 4 && magic == [0x28, 0xb5, 0x2f, 0xfd] {
        let (digest, size) = copy_to_blob(blobs, &mut f)?;
        return Ok((digest, size, ZSTD_LAYER_MEDIA.into()));
    }
    let layer = compress_tar_stream_to_blob(blobs, &mut f)?;
    Ok((Digest::parse(&layer.digest)?, layer.size, layer.media_type))
}

fn copy_to_blob(blobs: &BlobStore, reader: &mut impl Read) -> Result<(Digest, u64)> {
    let tmp = partial_blob_path(blobs)?;
    let out = File::create(&tmp)?;
    let mut hw = HashingWriter::new(out);
    if let Err(err) = io::copy(reader, &mut hw) {
        let _ = fs::remove_file(&tmp);
        return Err(err.into());
    }
    hw.inner.sync_all()?;
    let size = hw.bytes;
    let digest = Digest::parse(&format!("sha256:{}", hex(&hw.hash.finalize())))?;
    drop(hw.inner);
    fs::rename(&tmp, blobs.path(&digest))?;
    Ok((digest, size))
}

fn detect_blob_layer_media(path: &Path) -> Result<String> {
    let mut f = File::open(path)?;
    let mut magic = [0u8; 4];
    let n = f.read(&mut magic)?;
    if n >= 4 && magic == [0x28, 0xb5, 0x2f, 0xfd] {
        Ok(ZSTD_LAYER_MEDIA.into())
    } else {
        Ok(GZIP_LAYER.into())
    }
}

fn parse_verified_layers(blobs: &BlobStore, layers_arr: &[Value]) -> Result<Vec<Layer>> {
    let mut out = Vec::with_capacity(layers_arr.len());
    for item in layers_arr {
        let d_str = item["digest"]
            .as_str()
            .ok_or_else(|| anyhow!("manifest layer missing digest"))?;
        let d = Digest::parse(d_str)?;
        let actual_size = blobs
            .size(&d)
            .ok_or_else(|| anyhow!("archive is missing layer blob {d}"))?;
        let size = item["size"].as_u64().unwrap_or(actual_size);
        let media_type = item["mediaType"].as_str().unwrap_or(GZIP_LAYER).to_string();
        out.push(Layer { digest: d.to_string(), size, media_type });
    }
    Ok(out)
}

fn read_docker_manifest_tags(stage: &Path) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    let Ok(bytes) = fs::read(stage.join("manifest.json")) else { return map };
    let Ok(items) = serde_json::from_slice::<Vec<Value>>(&bytes) else { return map };
    for item in items {
        let Some(cfg) = item["Config"].as_str() else { continue };
        let hex_opt = cfg
            .strip_prefix("blobs/sha256/")
            .or_else(|| cfg.strip_suffix(".json"))
            .unwrap_or(cfg);
        let Ok(cfg_d) = Digest::parse(&format!("sha256:{hex_opt}")) else { continue };
        let Some(tags) = item["RepoTags"].as_array() else { continue };
        let entry = map.entry(cfg_d.to_string()).or_default();
        for t in tags {
            if let Some(s) = t.as_str() {
                if s != "<none>:<none>" && !s.is_empty() && !entry.contains(&s.to_string()) {
                    entry.push(s.to_string());
                }
            }
        }
    }
    map
}

fn resolve_oci_references(
    ctd_name: Option<&str>,
    ref_name: Option<&str>,
    manifest_tags: Option<&Vec<String>>,
    top_digest: &Digest,
) -> Result<Vec<String>> {
    if let Some(name) = ctd_name {
        if let Ok(parsed) = ImageReference::parse(name) {
            return Ok(vec![parsed.to_string()]);
        }
    }
    if let Some(tags) = manifest_tags {
        let parsed: Vec<String> = tags
            .iter()
            .filter_map(|t| ImageReference::parse(t).ok().map(|r| r.to_string()))
            .collect();
        if !parsed.is_empty() {
            return Ok(parsed);
        }
    }
    if let Some(name) = ref_name {
        if let Ok(parsed) = ImageReference::parse(name) {
            return Ok(vec![parsed.to_string()]);
        }
    }
    Ok(vec![ImageReference::parse(&format!("loaded:{}", &top_digest.hex()[..12]))?.to_string()])
}

fn arch_from_config(cfg: &Value) -> Option<String> {
    cfg["architecture"].as_str().and_then(norm_arch)
}

fn norm_arch(raw: &str) -> Option<String> {
    normalize_platform(raw).ok().or_else(|| match raw.trim().to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" => Some("amd64".into()),
        "aarch64" | "arm64" => Some("arm64".into()),
        _ => None,
    })
}

fn append_tar_bytes(tar: &mut tar::Builder<impl Write>, path: &str, data: &[u8]) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o644);
    header.set_size(data.len() as u64);
    header.set_mtime(0);
    tar.append_data(&mut header, path, data)?;
    Ok(())
}

fn append_blob_file(tar: &mut tar::Builder<impl Write>, blobs: &BlobStore, digest: &Digest, seen: &mut HashSet<String>) -> Result<()> {
    if !seen.insert(digest.as_str().to_string()) {
        return Ok(());
    }
    let path = blobs.path(digest);
    let mut f = File::open(&path).with_context(|| format!("opening blob {digest}"))?;
    let size = f.metadata()?.len();
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o644);
    header.set_size(size);
    header.set_mtime(0);
    tar.append_data(&mut header, format!("blobs/sha256/{}", digest.hex()), &mut f)?;
    Ok(())
}

fn clean_rel_path(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

fn blob_digest_from_rel(rel: &Path) -> Option<Digest> {
    let mut comps = rel.components();
    match (comps.next(), comps.next(), comps.next(), comps.next()) {
        (Some(Component::Normal(a)), Some(Component::Normal(b)), Some(Component::Normal(hex)), None)
            if a == "blobs" && b == "sha256" =>
        {
            Digest::parse(&format!("sha256:{}", hex.to_string_lossy())).ok()
        }
        _ => None,
    }
}

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(prefix: &str) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_tar_layer(blobs: &BlobStore, path_in_tar: &str, contents: &[u8]) -> (Digest, Digest, u64) {
        let mut raw = Vec::new();
        {
            let mut tar = tar::Builder::new(&mut raw);
            append_tar_bytes(&mut tar, path_in_tar, contents).unwrap();
            tar.finish().unwrap();
        }
        let diff_id = Digest::of(&raw);
        let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(&raw).unwrap();
        let gz = enc.finish().unwrap();
        let d = Digest::of(&gz);
        blobs.write_bytes(&d, &gz).unwrap();
        (d, diff_id, gz.len() as u64)
    }

    fn mk_image(blobs: &BlobStore, arch: &str, file_path: &str, data: &[u8]) -> (Digest, Digest, Layer) {
        let (layer_d, diff_id, size) = mk_tar_layer(blobs, file_path, data);
        let layer = Layer { digest: layer_d.to_string(), size, media_type: GZIP_LAYER.into() };
        let cfg = json!({
            "architecture": arch,
            "os": "linux",
            "config": { "Env": ["PATH=/bin"], "Cmd": ["sh"] },
            "rootfs": { "type": "layers", "diff_ids": [diff_id.as_str()] },
        });
        let cfg_bytes = serde_json::to_vec(&cfg).unwrap();
        let cfg_d = Digest::of(&cfg_bytes);
        blobs.write_bytes(&cfg_d, &cfg_bytes).unwrap();
        let man = json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST_MEDIA,
            "config": { "mediaType": "application/vnd.oci.image.config.v1+json", "digest": cfg_d.as_str(), "size": cfg_bytes.len() },
            "layers": [{ "mediaType": GZIP_LAYER, "digest": layer.digest, "size": layer.size }],
        });
        let man_bytes = serde_json::to_vec(&man).unwrap();
        let man_d = Digest::of(&man_bytes);
        blobs.write_bytes(&man_d, &man_bytes).unwrap();
        (man_d, cfg_d, layer)
    }

    #[test]
    fn save_and_load_single_and_multi_arch_oci_round_trip() {
        let src_dir = TempDirGuard::new("graviton-archive-src").unwrap();
        let dst_dir = TempDirGuard::new("graviton-archive-dst").unwrap();
        let src_blobs = BlobStore::new(src_dir.path.join("blobs"));
        let dst_blobs = BlobStore::new(dst_dir.path.join("blobs"));
        let mut dst_index = ImageIndex::load(dst_dir.path.join("images.json")).unwrap();

        // Single-arch image
        let (single_m, single_c, single_l) = mk_image(&src_blobs, "arm64", "hello.txt", b"single");
        let single = LocalImage {
            reference: "docker.io/library/single:v1".into(),
            manifest_digest: single_m.to_string(),
            config_digest: single_c.to_string(),
            layers: vec![single_l],
            architecture: "arm64".into(),
            pulled_at: 1,
            variants: Vec::new(),
        };

        // Multi-arch image
        let (arm_m, arm_c, arm_l) = mk_image(&src_blobs, "arm64", "arch.txt", b"arm64");
        let (amd_m, amd_c, amd_l) = mk_image(&src_blobs, "amd64", "arch.txt", b"amd64");
        let idx_doc = json!({
            "schemaVersion": 2,
            "mediaType": OCI_INDEX_MEDIA,
            "manifests": [
                {
                    "mediaType": OCI_MANIFEST_MEDIA,
                    "digest": arm_m.as_str(),
                    "size": src_blobs.size(&arm_m).unwrap(),
                    "platform": { "os": "linux", "architecture": "arm64" }
                },
                {
                    "mediaType": OCI_MANIFEST_MEDIA,
                    "digest": amd_m.as_str(),
                    "size": src_blobs.size(&amd_m).unwrap(),
                    "platform": { "os": "linux", "architecture": "amd64" }
                }
            ]
        });
        let idx_bytes = serde_json::to_vec(&idx_doc).unwrap();
        let idx_d = Digest::of(&idx_bytes);
        src_blobs.write_bytes(&idx_d, &idx_bytes).unwrap();
        let multi = LocalImage {
            reference: "docker.io/library/multi:1".into(),
            manifest_digest: idx_d.to_string(),
            config_digest: arm_c.to_string(),
            layers: vec![arm_l.clone()],
            architecture: "arm64".into(),
            pulled_at: 2,
            variants: vec![
                ImageVariant {
                    architecture: "arm64".into(),
                    manifest_digest: arm_m.to_string(),
                    config_digest: arm_c.to_string(),
                    layers: vec![arm_l],
                },
                ImageVariant {
                    architecture: "amd64".into(),
                    manifest_digest: amd_m.to_string(),
                    config_digest: amd_c.to_string(),
                    layers: vec![amd_l],
                },
            ],
        };

        let archive_path = dst_dir.path.join("saved.tar");
        save_images_from_store(&src_blobs, &[single, multi], File::create(&archive_path).unwrap()).unwrap();

        let loaded = load_images_into_store(&dst_blobs, &mut dst_index, &archive_path).unwrap();
        assert_eq!(loaded.len(), 2);
        let reloaded_single = dst_index.get("single:v1").unwrap();
        assert_eq!(reloaded_single.architecture, "arm64");
        assert!(!reloaded_single.is_multi_arch());

        let reloaded_multi = dst_index.get("multi:1").unwrap();
        assert!(reloaded_multi.is_multi_arch());
        assert_eq!(reloaded_multi.architectures(), vec!["arm64", "amd64"]);
        assert!(reloaded_multi.for_arch("amd64").is_some());
    }

    #[test]
    fn load_legacy_docker_save_uncompressed_and_gzip() {
        let dir = TempDirGuard::new("graviton-legacy-save").unwrap();
        let blobs = BlobStore::new(dir.path.join("blobs"));
        let mut index = ImageIndex::load(dir.path.join("images.json")).unwrap();

        let mut layer_tar = Vec::new();
        {
            let mut tar = tar::Builder::new(&mut layer_tar);
            append_tar_bytes(&mut tar, "etc/greeting", b"hi").unwrap();
            tar.finish().unwrap();
        }
        let diff_id = Digest::of(&layer_tar);
        let cfg = json!({
            "architecture": "amd64",
            "os": "linux",
            "config": { "Cmd": ["/bin/sh"] },
            "rootfs": { "type": "layers", "diff_ids": [diff_id.as_str()] },
        });
        let cfg_bytes = serde_json::to_vec(&cfg).unwrap();
        let cfg_hex = Digest::of(&cfg_bytes).hex().to_string();

        let manifest = json!([{
            "Config": format!("{cfg_hex}.json"),
            "RepoTags": ["legacy/app:v2"],
            "Layers": ["layer0/layer.tar"],
        }]);
        let tar_gz_path = dir.path.join("legacy.tar.gz");
        {
            let gz = GzEncoder::new(File::create(&tar_gz_path).unwrap(), Compression::fast());
            let mut tar = tar::Builder::new(gz);
            append_tar_bytes(&mut tar, &format!("{cfg_hex}.json"), &cfg_bytes).unwrap();
            append_tar_bytes(&mut tar, "layer0/layer.tar", &layer_tar).unwrap();
            append_tar_bytes(&mut tar, "manifest.json", &serde_json::to_vec(&manifest).unwrap()).unwrap();
            tar.finish().unwrap();
        }

        let loaded = load_images_into_store(&blobs, &mut index, &tar_gz_path).unwrap();
        assert_eq!(loaded.len(), 1);
        let img = index.get("legacy/app:v2").unwrap();
        assert_eq!(img.architecture, "amd64");
        assert_eq!(img.layers.len(), 1);
        assert!(blobs.has(&Digest::parse(&img.layers[0].digest).unwrap()));
    }

    #[test]
    fn apply_commit_changes_parses_config_and_rejects_run() {
        let mut state = build::State {
            architecture: "arm64".into(),
            layers: vec![],
            base_config: json!({}),
            env: vec!["PATH=/bin".into()],
            entrypoint: None,
            cmd: Some(vec!["sh".into()]),
            workdir: "/".into(),
            user: None,
            shell: vec!["/bin/sh".into(), "-c".into()],
            exposed: Default::default(),
            labels: Default::default(),
            volumes: Default::default(),
            stop_signal: None,
            healthcheck: None,
            onbuild: vec![],
            history: vec![],
            key: String::new(),
            args: Default::default(),
            created: None,
            author: None,
        };
        apply_commit_changes(
            &mut state,
            &[
                "ENV COMMIT_VER=42".into(),
                "WORKDIR app".into(),
                "LABEL role=snap".into(),
                "CMD [\"/bin/true\"]".into(),
            ],
        )
        .unwrap();
        assert!(state.env.contains(&"COMMIT_VER=42".to_string()));
        assert_eq!(state.workdir, "/app");
        assert_eq!(state.labels.get("role").map(String::as_str), Some("snap"));
        assert_eq!(state.cmd, Some(vec!["/bin/true".to_string()]));

        let err = apply_commit_changes(&mut state, &["RUN echo nope".into()]).unwrap_err();
        assert!(err.to_string().contains("not a config instruction"));
    }
}
