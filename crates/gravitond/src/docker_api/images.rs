//! Docker Engine REST API endpoints for system, images, build, volumes,
//! networks, and events.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use graviton_core::{ImageReference, paths};
use graviton_image::Credentials;
use graviton_proto::{BuildSpec, ContainerStatus, EventKind, ImagePushProgress, PruneSpec, PullProgress};
use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

use crate::{Backend, build, cleanup, upload};
use crate::runtime::{detect_guest_kernel, host_memory, is_valid_volume_name, new_id, now_secs};

use super::containers::parse_filters;
use super::http::{
    API_VERSION, MAX_JSON_BODY, MIN_API_VERSION, RequestHead, drain_body, finish_chunked, read_body_bytes,
    read_body_to_file, write_chunk_json, write_chunked_head, write_empty, write_error, write_json, write_text,
};
use super::{DockerState, LogicalNetwork, RouteOutcome, make_network_id, now_rfc3339};

struct TempDirGuard(PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct TempFileGuard(PathBuf);
impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ---------------------------------------------------------------------------
// System: /_ping, /version, /info, /system/df, /events
// ---------------------------------------------------------------------------

pub async fn ping(
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    write_text(write, 200, "OK", head.method == "HEAD", head.close_after).await?;
    Ok(RouteOutcome::Continue)
}

pub async fn version(
    state: &Arc<DockerState>,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let info = match &state.backend {
        Backend::Engine(e) => e.engine_info(),
        Backend::Demo(s) => s.lock().unwrap().engine_info(),
    };
    let payload = json!({
        "Platform": { "Name": "Graviton" },
        "Components": [{
            "Name": "Engine",
            "Version": info.version,
            "Details": {
                "ApiVersion": API_VERSION,
                "Arch": info.arch,
                "Experimental": "false",
                "GitCommit": "graviton",
                "GoVersion": "graviton",
                "KernelVersion": info.kernel,
                "MinAPIVersion": MIN_API_VERSION,
                "Os": "linux"
            }
        }],
        "Version": info.version,
        "ApiVersion": API_VERSION,
        "MinAPIVersion": MIN_API_VERSION,
        "GitCommit": "graviton",
        "GoVersion": "graviton",
        "Os": "linux",
        "Arch": info.arch,
        "KernelVersion": info.kernel,
    });
    write_json(write, 200, &payload, head.close_after).await?;
    Ok(RouteOutcome::Continue)
}

pub async fn info(
    state: &Arc<DockerState>,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let (info, containers, images_count) = match &state.backend {
        Backend::Engine(e) => {
            let ei = e.engine_info();
            let c = e.store.lock().unwrap().containers().to_vec();
            let imgs = e.index.lock().unwrap().images().count();
            (ei, c, imgs)
        }
        Backend::Demo(s) => {
            let guard = s.lock().unwrap();
            (guard.engine_info(), guard.containers().to_vec(), guard.images().len())
        }
    };

    let running = containers.iter().filter(|c| c.status == ContainerStatus::Running).count();
    let paused = containers.iter().filter(|c| c.status == ContainerStatus::Paused).count();
    let stopped = containers.iter().filter(|c| c.status == ContainerStatus::Exited).count();

    let payload = json!({
        "ID": "GRAVITON:ENGINE:01",
        "Containers": containers.len(),
        "ContainersRunning": running,
        "ContainersPaused": paused,
        "ContainersStopped": stopped,
        "Images": images_count,
        "Driver": "graviton",
        "MemoryLimit": true,
        "SwapLimit": false,
        "KernelMemoryTCP": false,
        "CpuCfsPeriod": true,
        "CpuCfsQuota": true,
        "CPUShares": true,
        "CPUSet": true,
        "OomKillDisable": false,
        "IPv4Forwarding": true,
        "Debug": false,
        "NFd": 0,
        "NGoroutines": 0,
        "SystemTime": now_rfc3339(),
        "LoggingDriver": "json-file",
        "CgroupDriver": "cgroupfs",
        "CgroupVersion": "2",
        "NEventsListener": state.events_tx.receiver_count(),
        "KernelVersion": if info.kernel.is_empty() { detect_guest_kernel() } else { info.kernel },
        "OperatingSystem": "Graviton (macOS Virtualization.framework)",
        "OSVersion": "6.12",
        "OSType": "linux",
        "Architecture": if info.arch == "arm64" { "aarch64" } else { "x86_64" },
        "NCPU": info.cpus.max(1),
        "MemTotal": if info.memory_bytes > 0 { info.memory_bytes } else { host_memory() },
        "Name": "graviton",
        "ServerVersion": info.version,
        "DockerRootDir": paths::support_dir().display().to_string(),
        "ExperimentalBuild": false,
        "Plugins": {
            "Volume": ["local"],
            "Network": ["bridge", "host", "none"],
            "Log": ["json-file"]
        }
    });
    write_json(write, 200, &payload, head.close_after).await?;
    Ok(RouteOutcome::Continue)
}

pub async fn system_df(
    state: &Arc<DockerState>,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let Some(engine) = state.engine().cloned() else {
        write_json(write, 200, &json!({ "LayersSize": 0, "Images": [], "Containers": [], "Volumes": [] }), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };
    let e = Arc::clone(&engine);
    let df = tokio::task::spawn_blocking(move || cleanup::disk_usage(&e)).await?;
    let payload = json!({
        "LayersSize": df.blobs_bytes,
        "Images": [],
        "Containers": [],
        "Volumes": [],
        "BuildCache": [],
    });
    write_json(write, 200, &payload, head.close_after).await?;
    Ok(RouteOutcome::Continue)
}

pub async fn events(
    state: &Arc<DockerState>,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let until = head.query_param("until").and_then(|s| s.parse::<u64>().ok());
    let mut rx = state.events_tx.subscribe();
    write_chunked_head(write, 200, "application/json", &[]).await?;

    loop {
        if let Some(u) = until {
            if now_secs() >= u {
                break;
            }
        }
        match tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
            Ok(Ok(ev)) => {
                let (ev_type, action, id, name) = match ev.kind {
                    EventKind::Started => ("container", "start", ev.container_id.as_deref().unwrap_or(""), ev.container.as_deref().unwrap_or("")),
                    EventKind::Stopped | EventKind::Failed => ("container", "die", ev.container_id.as_deref().unwrap_or(""), ev.container.as_deref().unwrap_or("")),
                    EventKind::Paused => ("container", "pause", ev.container_id.as_deref().unwrap_or(""), ev.container.as_deref().unwrap_or("")),
                    EventKind::Unpaused => ("container", "unpause", ev.container_id.as_deref().unwrap_or(""), ev.container.as_deref().unwrap_or("")),
                    EventKind::Healthy => ("container", "health_status: healthy", ev.container_id.as_deref().unwrap_or(""), ev.container.as_deref().unwrap_or("")),
                    EventKind::Unhealthy => ("container", "health_status: unhealthy", ev.container_id.as_deref().unwrap_or(""), ev.container.as_deref().unwrap_or("")),
                    EventKind::Removed => ("container", "destroy", ev.container_id.as_deref().unwrap_or(""), ev.container.as_deref().unwrap_or("")),
                    EventKind::Pulled => ("image", "pull", ev.image.as_deref().unwrap_or(""), ev.image.as_deref().unwrap_or("")),
                    EventKind::Pushed => ("image", "push", ev.image.as_deref().unwrap_or(""), ev.image.as_deref().unwrap_or("")),
                    EventKind::Built | EventKind::BuildFailed => ("image", "build", ev.image.as_deref().unwrap_or(""), ev.image.as_deref().unwrap_or("")),
                };
                let secs = ev.timestamp_ms / 1000;
                let nanos = ev.timestamp_ms * 1_000_000;
                let msg = json!({
                    "Type": ev_type,
                    "Action": action,
                    "status": action,
                    "id": id,
                    "from": ev.image.as_deref().unwrap_or(""),
                    "Actor": {
                        "ID": id,
                        "Attributes": {
                            "name": name,
                            "image": ev.image.as_deref().unwrap_or(""),
                        }
                    },
                    "scope": "local",
                    "time": secs,
                    "timeNano": nanos
                });
                if write_chunk_json(write, &msg).await.is_err() {
                    return Ok(RouteOutcome::Done);
                }
            }
            Ok(Err(_)) => break,
            Err(_) => continue,
        }
    }
    let _ = finish_chunked(write).await;
    Ok(RouteOutcome::Continue)
}

// ---------------------------------------------------------------------------
// Images: /images/json, /images/{name}/json, /images/create, /tag, DELETE, prune
// ---------------------------------------------------------------------------

pub async fn list_images(
    state: &Arc<DockerState>,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let filters = head.query_param("filters").map(parse_filters).transpose()?;
    let ref_filters = filters.as_ref().and_then(|f| f.get("reference")).cloned();

    match &state.backend {
        Backend::Demo(store) => {
            let imgs = store.lock().unwrap().images().to_vec();
            let list: Vec<Value> = imgs
                .into_iter()
                .filter(|i| match &ref_filters {
                    None => true,
                    Some(wants) => {
                        let tag_name = format!("{}:{}", i.repository, i.tag);
                        wants.iter().any(|w| w == &tag_name || w == &i.repository || w == &i.reference)
                    }
                })
                .map(|i| {
                    let tag_name = format!("{}:{}", i.repository, i.tag);
                    json!({
                        "Id": i.digest,
                        "ParentId": "",
                        "RepoTags": [tag_name],
                        "RepoDigests": [format!("{}@{}", i.repository, i.digest)],
                        "Created": now_secs(),
                        "Size": i.size_bytes,
                        "VirtualSize": i.size_bytes,
                        "SharedSize": -1,
                        "Labels": {},
                        "Containers": i.used_by,
                    })
                })
                .collect();
            write_json(write, 200, &list, head.close_after).await?;
        }
        Backend::Engine(engine) => {
            struct Group {
                id: String,
                tags: Vec<String>,
                digests: Vec<String>,
                created: u64,
                size: u64,
                containers: u32,
                labels: BTreeMap<String, String>,
            }
            let local_imgs: Vec<graviton_image::LocalImage> = engine.index.lock().unwrap().images().cloned().collect();
            let mut by_cfg: HashMap<String, Group> = HashMap::new();
            let mut order: Vec<String> = Vec::new();
            for img in local_imgs {
                let display = ImageReference::parse(&img.reference)
                    .map(|r| r.display_name())
                    .unwrap_or_else(|_| img.reference.clone());
                let repo = ImageReference::parse(&img.reference)
                    .map(|r| {
                        if r.registry == "docker.io" {
                            r.repository.strip_prefix("library/").unwrap_or(&r.repository).to_string()
                        } else {
                            format!("{}/{}", r.registry, r.repository)
                        }
                    })
                    .unwrap_or_else(|_| display.clone());
                if let Some(wants) = &ref_filters {
                    let matches = wants
                        .iter()
                        .any(|w| w == &display || w == &repo || w == &img.reference || display.starts_with(&format!("{w}:")));
                    if !matches {
                        continue;
                    }
                }
                let used = engine.used_by(&img.reference);
                let labels = img
                    .config(&engine.blobs)
                    .ok()
                    .and_then(|c| c.config.labels)
                    .unwrap_or_default();
                let key = img.config_digest.clone();
                if let Some(g) = by_cfg.get_mut(&key) {
                    if !g.tags.contains(&display) {
                        g.tags.push(display);
                    }
                    let rd = format!("{repo}@{}", img.manifest_digest);
                    if !g.digests.contains(&rd) {
                        g.digests.push(rd);
                    }
                    g.containers = g.containers.saturating_add(used);
                } else {
                    order.push(key.clone());
                    by_cfg.insert(
                        key,
                        Group {
                            id: img.config_digest.clone(),
                            tags: vec![display],
                            digests: vec![format!("{repo}@{}", img.manifest_digest)],
                            created: img.pulled_at,
                            size: img.size(),
                            containers: used,
                            labels,
                        },
                    );
                }
            }
            let out: Vec<Value> = order
                .into_iter()
                .filter_map(|k| by_cfg.remove(&k))
                .map(|g| {
                    json!({
                        "Id": g.id,
                        "ParentId": "",
                        "RepoTags": g.tags,
                        "RepoDigests": g.digests,
                        "Created": g.created,
                        "Size": g.size,
                        "VirtualSize": g.size,
                        "SharedSize": -1,
                        "Labels": g.labels,
                        "Containers": g.containers,
                    })
                })
                .collect();
            write_json(write, 200, &out, head.close_after).await?;
        }
    }
    Ok(RouteOutcome::Continue)
}

pub async fn inspect_image(
    state: &Arc<DockerState>,
    raw_name: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let Some(engine) = state.engine().cloned() else {
        write_error(write, 404, &format!("No such image: {raw_name}"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };
    let name = raw_name.to_string();
    let e = Arc::clone(&engine);
    let res = tokio::task::spawn_blocking(move || e.inspect_image(&name)).await?;
    let (img, raw_cfg) = match res {
        Ok(res) => res,
        Err(err) => {
            write_error(write, 404, &format!("{err:#}"), head.close_after).await?;
            return Ok(RouteOutcome::Continue);
        }
    };

    let mut repo_tags = Vec::new();
    let mut repo_digests = Vec::new();
    for item in engine.index.lock().unwrap().images() {
        if item.config_digest == img.config_digest {
            let disp = ImageReference::parse(&item.reference)
                .map(|r| r.display_name())
                .unwrap_or_else(|_| item.reference.clone());
            if !repo_tags.contains(&disp) {
                repo_tags.push(disp);
            }
            let repo_only = ImageReference::parse(&item.reference)
                .map(|r| {
                    if r.registry == "docker.io" {
                        r.repository.strip_prefix("library/").unwrap_or(&r.repository).to_string()
                    } else {
                        format!("{}/{}", r.registry, r.repository)
                    }
                })
                .unwrap_or_else(|_| item.reference.clone());
            let rd = format!("{repo_only}@{}", item.manifest_digest);
            if !repo_digests.contains(&rd) {
                repo_digests.push(rd);
            }
        }
    }

    let c = &raw_cfg["config"];
    let diff_ids = raw_cfg["rootfs"]["diff_ids"].clone();
    let created = raw_cfg["created"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| build::rfc3339(img.pulled_at));
    let size = img.size();

    let payload = json!({
        "Id": img.config_digest,
        "RepoTags": repo_tags,
        "RepoDigests": repo_digests,
        "Parent": "",
        "Comment": "",
        "Created": created,
        "DockerVersion": "24.0.0",
        "Author": raw_cfg["author"].as_str().unwrap_or(""),
        "Architecture": if img.architecture.is_empty() { "arm64" } else { &img.architecture },
        "Os": raw_cfg["os"].as_str().unwrap_or("linux"),
        "Size": size,
        "VirtualSize": size,
        "Config": {
            "Hostname": c["Hostname"].as_str().unwrap_or(""),
            "Domainname": c["Domainname"].as_str().unwrap_or(""),
            "User": c["User"].as_str().unwrap_or(""),
            "AttachStdin": false,
            "AttachStdout": false,
            "AttachStderr": false,
            "ExposedPorts": c.get("ExposedPorts").cloned().unwrap_or(Value::Null),
            "Tty": false,
            "OpenStdin": false,
            "StdinOnce": false,
            "Env": c.get("Env").cloned().unwrap_or_else(|| json!([])),
            "Cmd": c.get("Cmd").cloned().unwrap_or(Value::Null),
            "ArgsEscaped": false,
            "Image": img.config_digest,
            "Volumes": c.get("Volumes").cloned().unwrap_or(Value::Null),
            "WorkingDir": c["WorkingDir"].as_str().unwrap_or(""),
            "Entrypoint": c.get("Entrypoint").cloned().unwrap_or(Value::Null),
            "Labels": c.get("Labels").cloned().unwrap_or_else(|| json!({})),
        },
        "RootFS": {
            "Type": "layers",
            "Layers": diff_ids
        },
        "GraphDriver": {
            "Data": Value::Null,
            "Name": "graviton"
        },
        "Metadata": {
            "LastTagTime": created
        }
    });
    write_json(write, 200, &payload, head.close_after).await?;
    Ok(RouteOutcome::Continue)
}

fn join_image_tag(name: &str, tag: &str) -> String {
    if tag.starts_with("sha256:") {
        format!("{name}@{tag}")
    } else if !tag.is_empty() && !name.contains('@') {
        let last = name.rsplit('/').next().unwrap_or("");
        if last.contains(':') {
            name.to_string()
        } else {
            format!("{name}:{tag}")
        }
    } else {
        name.to_string()
    }
}

fn parse_registry_auth(head: &RequestHead) -> Option<Credentials> {
    let raw = head.header("x-registry-auth")?.trim();
    if raw.is_empty() {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(raw)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(raw))
        .ok()?;
    let val: Value = serde_json::from_slice(&decoded).ok()?;
    let obj = val.as_object()?;
    let username = obj.get("username").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let password = obj
        .get("password")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| obj.get("identitytoken").and_then(|v| v.as_str()).filter(|s| !s.is_empty()))
        .unwrap_or("")
        .to_string();
    if username.is_empty() || password.is_empty() {
        return None;
    }
    Some(Credentials { username, password })
}

pub async fn pull_image(
    state: &Arc<DockerState>,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let from_image = head.query_param("fromImage").unwrap_or("").trim().to_string();
    let tag = head.query_param("tag").unwrap_or("").trim().to_string();
    let platform = head
        .query_param("platform")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let Some(engine) = state.engine().cloned() else {
        write_error(write, 501, "pull requires a live engine", head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };
    if from_image.is_empty() {
        write_error(write, 400, "fromImage query parameter is required", head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }

    let reference = join_image_tag(&from_image, &tag);

    write_chunked_head(write, 200, "application/json", &[]).await?;
    write_chunk_json(write, &json!({ "status": format!("Pulling from {from_image}"), "id": if tag.is_empty() { "latest" } else { &tag } })).await?;

    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let r = reference.clone();
    let p = platform.clone();
    let task = tokio::task::spawn_blocking(move || {
        let updates = tx.clone();
        let res = engine.pull_with_progress(&r, p.as_deref(), |prog| {
            let msg = match prog {
                PullProgress::Resolved { layers, .. } => {
                    json!({ "status": format!("Pulling {layers} layer{}", if layers == 1 { "" } else { "s" }) })
                }
                PullProgress::Layer { digest, done, total, cached, .. } => {
                    let short = digest.strip_prefix("sha256:").map(|h| &h[..h.len().min(12)]).unwrap_or(&digest);
                    if cached {
                        json!({ "status": "Already exists", "id": short })
                    } else {
                        json!({
                            "status": "Downloading",
                            "id": short,
                            "progressDetail": { "current": done, "total": total }
                        })
                    }
                }
            };
            let _ = updates.send(msg);
        });
        match res {
            Ok(img) => {
                let _ = tx.send(json!({ "status": format!("Digest: {}", img.digest) }));
                let _ = tx.send(json!({ "status": format!("Status: Downloaded newer image for {r}") }));
            }
            Err(err) => {
                let msg = format!("{err:#}");
                let _ = tx.send(json!({ "errorDetail": { "message": msg }, "error": msg }));
            }
        }
    });

    while let Some(msg) = rx.recv().await {
        if write_chunk_json(write, &msg).await.is_err() {
            return Ok(RouteOutcome::Done);
        }
    }
    let _ = task.await;
    finish_chunked(write).await?;
    Ok(RouteOutcome::Continue)
}

pub async fn push_image(
    state: &Arc<DockerState>,
    raw_name: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let name = raw_name.trim();
    let tag = head.query_param("tag").unwrap_or("").trim().to_string();
    let Some(engine) = state.engine().cloned() else {
        write_error(write, 501, "push requires a live engine", head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };
    if name.is_empty() {
        write_error(write, 400, "image name is required", head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }

    let reference = join_image_tag(name, &tag);
    let auth = parse_registry_auth(head);
    write_chunked_head(write, 200, "application/json", &[]).await?;
    write_chunk_json(write, &json!({ "status": format!("The push refers to repository [{name}]") })).await?;

    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let ref_clone = reference.clone();
    let tag_clone = tag.clone();
    let task = tokio::task::spawn_blocking(move || {
        let updates = tx.clone();
        let res = engine.push_with_progress(&ref_clone, auth, |prog| {
            let msg = match prog {
                ImagePushProgress::Started { .. } => Some(json!({ "status": "Preparing" })),
                ImagePushProgress::Blob { digest, done, total, skipped, .. } => {
                    let short = digest.strip_prefix("sha256:").map(|h| &h[..h.len().min(12)]).unwrap_or(&digest);
                    if skipped {
                        Some(json!({ "status": "Layer already exists", "id": short }))
                    } else if total == 0 || done >= total {
                        Some(json!({ "status": "Pushed", "id": short }))
                    } else {
                        Some(json!({
                            "status": "Pushing",
                            "id": short,
                            "progressDetail": { "current": done, "total": total }
                        }))
                    }
                }
                ImagePushProgress::Manifest { .. } => None,
            };
            if let Some(m) = msg {
                let _ = updates.send(m);
            }
        });
        match res {
            Ok(pushed) => {
                let tag_disp = if tag_clone.is_empty() {
                    ImageReference::parse(&ref_clone)
                        .ok()
                        .and_then(|r| r.tag)
                        .unwrap_or_else(|| "latest".into())
                } else {
                    tag_clone
                };
                let _ = tx.send(json!({
                    "status": format!("{tag_disp}: digest: {} size: {}", pushed.digest, pushed.size_bytes)
                }));
                let _ = tx.send(json!({
                    "aux": { "Tag": tag_disp, "Digest": pushed.digest, "Size": pushed.size_bytes }
                }));
            }
            Err(err) => {
                let msg = format!("{err:#}");
                let _ = tx.send(json!({ "errorDetail": { "message": msg }, "error": msg }));
            }
        }
    });

    while let Some(msg) = rx.recv().await {
        if write_chunk_json(write, &msg).await.is_err() {
            return Ok(RouteOutcome::Done);
        }
    }
    let _ = task.await;
    finish_chunked(write).await?;
    Ok(RouteOutcome::Continue)
}

pub async fn tag_image(
    state: &Arc<DockerState>,
    raw_name: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let Some(engine) = state.engine().cloned() else {
        write_error(write, 501, "tag requires a live engine", head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };
    let repo = head.query_param("repo").unwrap_or("").trim();
    if repo.is_empty() {
        write_error(write, 400, "repo query parameter is required", head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }
    let tag = head.query_param("tag").unwrap_or("").trim();
    let target = if tag.is_empty() {
        repo.to_string()
    } else {
        format!("{repo}:{tag}")
    };
    let src = raw_name.to_string();
    let res = tokio::task::spawn_blocking(move || engine.tag_image(&src, &target)).await?;
    match res {
        Ok(_) => write_empty(write, 201, &[], head.close_after).await?,
        Err(err) => {
            let msg = format!("{err:#}");
            let status = if msg.starts_with("No such image:") { 404 } else { 400 };
            write_error(write, status, &msg, head.close_after).await?;
        }
    }
    Ok(RouteOutcome::Continue)
}

pub async fn remove_image(
    state: &Arc<DockerState>,
    raw_name: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let Some(engine) = state.engine().cloned() else {
        write_error(write, 501, "rmi requires a live engine", head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };
    let name = raw_name.to_string();
    let res = tokio::task::spawn_blocking(move || cleanup::remove_image(&engine, &name)).await?;
    match res {
        Ok((reference, _, _)) => {
            let disp = ImageReference::parse(&reference)
                .map(|r| r.display_name())
                .unwrap_or_else(|_| reference.clone());
            write_json(write, 200, &json!([{ "Untagged": disp }]), head.close_after).await?;
        }
        Err(err) => {
            let msg = format!("{err:#}");
            let status = if msg.contains("is used by") { 409 } else { 404 };
            write_error(write, status, &msg, head.close_after).await?;
        }
    }
    Ok(RouteOutcome::Continue)
}

pub async fn prune_images(
    state: &Arc<DockerState>,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let Some(engine) = state.engine().cloned() else {
        write_json(write, 200, &json!({ "ImagesDeleted": [], "SpaceReclaimed": 0 }), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };
    let report = tokio::task::spawn_blocking(move || {
        cleanup::prune(&engine, &PruneSpec { containers: false, images: true, dry_run: false })
    })
    .await??;
    let deleted: Vec<Value> = report.images.into_iter().map(|r| json!({ "Untagged": r })).collect();
    write_json(
        write,
        200,
        &json!({
            "ImagesDeleted": deleted,
            "SpaceReclaimed": report.freed_bytes
        }),
        head.close_after,
    )
    .await?;
    Ok(RouteOutcome::Continue)
}

// ---------------------------------------------------------------------------
// POST /build
// ---------------------------------------------------------------------------

pub async fn build_image(
    state: &Arc<DockerState>,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    let Some(engine) = state.engine().cloned() else {
        drain_body(reader, write, head).await?;
        write_error(write, 501, "build requires a live engine", head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };

    let tags: Vec<String> = head
        .query_params("t")
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let dockerfile = head
        .query_param("dockerfile")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let target = head
        .query_param("target")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let no_cache = head.query_bool("nocache", false);
    let platform = head
        .query_param("platform")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);

    let mut build_args = Vec::new();
    if let Some(raw_args) = head.query_param("buildargs") {
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(raw_args) {
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    build_args.push(format!("{k}={s}"));
                } else if !v.is_null() {
                    build_args.push(format!("{k}={v}"));
                }
            }
        }
    }

    let mut labels = BTreeMap::new();
    if let Some(raw_labels) = head.query_param("labels") {
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(raw_labels) {
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    labels.insert(k, s.to_string());
                }
            }
        }
    }

    let id = new_id();
    let root = paths::support_dir().join("builds");
    tokio::fs::create_dir_all(&root).await?;
    let archive_path = root.join(format!("docker-build-{id}.tar"));
    let ctx_dir = root.join(format!("docker-ctx-{id}"));
    let _tar_guard = TempFileGuard(archive_path.clone());
    let _dir_guard = TempDirGuard(ctx_dir.clone());

    if let Err(err) = read_body_to_file(reader, write, head, &archive_path, upload::MAX_UPLOAD).await {
        write_error(write, 400, &format!("receiving build context: {err:#}"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }
    let a = archive_path.clone();
    let d = ctx_dir.clone();
    if let Err(err) = tokio::task::spawn_blocking(move || upload::unpack_archive(&a, &d)).await? {
        write_error(write, 400, &format!("unpacking build context: {err:#}"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }

    let primary_tag = tags.first().cloned().or_else(|| Some(format!("graviton-build:{id}")));
    let spec = BuildSpec {
        context: ctx_dir.display().to_string(),
        dockerfile,
        tag: primary_tag.clone(),
        build_args,
        target,
        no_cache,
        upload: None,
        secrets: vec![],
        platform,
        labels,
    };

    write_chunked_head(write, 200, "application/json", &[]).await?;
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let extra_tags: Vec<String> = tags.iter().skip(1).cloned().collect();
    let all_tags = tags.clone();

    let task = tokio::task::spawn_blocking(move || {
        let log_tx = tx.clone();
        let res = build::build(&engine, &spec, &mut |line| {
            let _ = log_tx.send(json!({ "stream": format!("{line}\n") }));
        });
        match res {
            Ok(img) => {
                let name = format!("{}:{}", img.repository, img.tag);
                engine.events.record(EventKind::Built, None, Some(&name), format!("Built {name}"));
                for extra in &extra_tags {
                    let _ = engine.tag_image(&img.reference, extra);
                }
                let cfg_id = engine
                    .resolve_local_image(&img.reference)
                    .map(|l| l.config_digest)
                    .unwrap_or_else(|| img.digest.clone());
                let short = cfg_id
                    .strip_prefix("sha256:")
                    .map(|s| &s[..s.len().min(12)])
                    .unwrap_or(&cfg_id);
                let _ = tx.send(json!({ "aux": { "ID": cfg_id } }));
                let _ = tx.send(json!({ "stream": format!("Successfully built {short}\n") }));
                for t in &all_tags {
                    let _ = tx.send(json!({ "stream": format!("Successfully tagged {t}\n") }));
                }
            }
            Err(err) => {
                let why = format!("{err:#}");
                let tag_label = spec.tag.clone().unwrap_or_else(|| "image".into());
                let short = why.lines().next().unwrap_or("").chars().take(160).collect::<String>();
                engine.events.record(
                    EventKind::BuildFailed,
                    None,
                    Some(&tag_label),
                    format!("Build of {tag_label} failed: {short}"),
                );
                let _ = tx.send(json!({ "errorDetail": { "message": why }, "error": why }));
            }
        }
    });

    while let Some(msg) = rx.recv().await {
        if write_chunk_json(write, &msg).await.is_err() {
            return Ok(RouteOutcome::Done);
        }
    }
    let _ = task.await;
    finish_chunked(write).await?;
    Ok(RouteOutcome::Continue)
}

// ---------------------------------------------------------------------------
// Volumes: /volumes, /volumes/{name}, /volumes/create, DELETE, prune
// ---------------------------------------------------------------------------

fn volume_to_json(v: &graviton_proto::VolumeSummary) -> Value {
    json!({
        "CreatedAt": build::rfc3339(v.created_at),
        "Driver": "local",
        "Labels": {},
        "Mountpoint": v.path,
        "Name": v.name,
        "Options": {},
        "Scope": "local",
        "UsageData": {
            "Size": v.size_bytes,
            "RefCount": v.used_by.len(),
        }
    })
}

pub async fn list_volumes(
    state: &Arc<DockerState>,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let Some(engine) = state.engine().cloned() else {
        write_json(write, 200, &json!({ "Volumes": [], "Warnings": [] }), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };
    let vols = tokio::task::spawn_blocking(move || engine.list_volumes()).await?;
    let items: Vec<Value> = vols.iter().map(volume_to_json).collect();
    write_json(write, 200, &json!({ "Volumes": items, "Warnings": [] }), head.close_after).await?;
    Ok(RouteOutcome::Continue)
}

pub async fn inspect_volume(
    state: &Arc<DockerState>,
    name: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let Some(engine) = state.engine().cloned() else {
        write_error(write, 404, &format!("no such volume: {name}"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };
    let want = name.to_string();
    let vol = tokio::task::spawn_blocking(move || engine.list_volumes().into_iter().find(|v| v.name == want)).await?;
    match vol {
        Some(v) => write_json(write, 200, &volume_to_json(&v), head.close_after).await?,
        None => write_error(write, 404, &format!("no such volume: {name}"), head.close_after).await?,
    }
    Ok(RouteOutcome::Continue)
}

pub async fn create_volume(
    state: &Arc<DockerState>,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    let Some(engine) = state.engine().cloned() else {
        drain_body(reader, write, head).await?;
        write_error(write, 501, "create volume requires a live engine", head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };
    let body = read_body_bytes(reader, write, head, MAX_JSON_BODY).await?;
    let payload: Value = if body.is_empty() { json!({}) } else { serde_json::from_slice(&body)? };
    let name = payload["Name"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| format!("vol-{}", new_id()));
    if !is_valid_volume_name(&name) {
        write_error(write, 400, &format!("`{name}` isn't a valid volume name"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }

    let n = name.clone();
    let vol = tokio::task::spawn_blocking(move || {
        match engine.create_volume(&n) {
            Ok(v) => Ok(v),
            Err(_) => engine
                .list_volumes()
                .into_iter()
                .find(|v| v.name == n)
                .context("creating or inspecting volume"),
        }
    })
    .await?;
    match vol {
        Ok(v) => write_json(write, 201, &volume_to_json(&v), head.close_after).await?,
        Err(err) => write_error(write, 500, &format!("{err:#}"), head.close_after).await?,
    }
    Ok(RouteOutcome::Continue)
}

pub async fn remove_volume(
    state: &Arc<DockerState>,
    name: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let Some(engine) = state.engine().cloned() else {
        write_error(write, 501, "remove volume requires a live engine", head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };
    let n = name.to_string();
    let res = tokio::task::spawn_blocking(move || engine.remove_volume(&n)).await?;
    match res {
        Ok(_) => write_empty(write, 204, &[], head.close_after).await?,
        Err(err) => {
            let msg = format!("{err:#}");
            let status = if msg.contains("is used by") { 409 } else { 404 };
            write_error(write, status, &msg, head.close_after).await?;
        }
    }
    Ok(RouteOutcome::Continue)
}

pub async fn prune_volumes(
    state: &Arc<DockerState>,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let Some(engine) = state.engine().cloned() else {
        write_json(write, 200, &json!({ "VolumesDeleted": [], "SpaceReclaimed": 0 }), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };
    let (deleted, reclaimed) = tokio::task::spawn_blocking(move || {
        let mut deleted = Vec::new();
        let mut reclaimed = 0u64;
        for v in engine.list_volumes() {
            if v.used_by.is_empty() {
                if let Ok((name, freed)) = engine.remove_volume(&v.name) {
                    deleted.push(name);
                    reclaimed += freed;
                }
            }
        }
        (deleted, reclaimed)
    })
    .await?;
    write_json(
        write,
        200,
        &json!({ "VolumesDeleted": deleted, "SpaceReclaimed": reclaimed }),
        head.close_after,
    )
    .await?;
    Ok(RouteOutcome::Continue)
}

// ---------------------------------------------------------------------------
// Networks: /networks, /networks/{id}, create, delete, connect/disconnect
// ---------------------------------------------------------------------------

fn builtin_networks(state: &DockerState) -> Vec<Value> {
    let (subnet, gateway, endpoints) = match &state.backend {
        Backend::Engine(e) => {
            let ni = e.network_info();
            (ni.subnet, ni.gateway, ni.endpoints)
        }
        Backend::Demo(_) => ("192.168.64.0/24".into(), "192.168.64.1".into(), vec![]),
    };
    let mut bridge_containers = serde_json::Map::new();
    for ep in endpoints {
        if let Some(ip) = ep.ip_address {
            bridge_containers.insert(
                ep.container_id.clone(),
                json!({
                    "Name": ep.container_name,
                    "EndpointID": ep.container_id,
                    "MacAddress": "",
                    "IPv4Address": format!("{ip}/24"),
                    "IPv6Address": ""
                }),
            );
        }
    }
    vec![
        json!({
            "Name": "bridge",
            "Id": "graviton-bridge",
            "Created": "2026-01-01T00:00:00Z",
            "Scope": "local",
            "Driver": "bridge",
            "EnableIPv6": false,
            "IPAM": {
                "Driver": "default",
                "Config": [{ "Subnet": subnet, "Gateway": gateway }]
            },
            "Internal": false,
            "Attachable": true,
            "Ingress": false,
            "Containers": Value::Object(bridge_containers),
            "Options": {},
            "Labels": {}
        }),
        json!({
            "Name": "host",
            "Id": "graviton-host",
            "Created": "2026-01-01T00:00:00Z",
            "Scope": "local",
            "Driver": "host",
            "EnableIPv6": false,
            "IPAM": { "Driver": "default", "Config": [] },
            "Internal": false,
            "Attachable": false,
            "Ingress": false,
            "Containers": {},
            "Options": {},
            "Labels": {}
        }),
        json!({
            "Name": "none",
            "Id": "graviton-none",
            "Created": "2026-01-01T00:00:00Z",
            "Scope": "local",
            "Driver": "null",
            "EnableIPv6": false,
            "IPAM": { "Driver": "default", "Config": [] },
            "Internal": false,
            "Attachable": false,
            "Ingress": false,
            "Containers": {},
            "Options": {},
            "Labels": {}
        }),
    ]
}

fn all_networks_json(state: &DockerState) -> Vec<Value> {
    let mut list = builtin_networks(state);
    for net in state.networks.lock().unwrap().values() {
        list.push(json!({
            "Name": net.name,
            "Id": net.id,
            "Created": net.created,
            "Scope": net.scope,
            "Driver": net.driver,
            "EnableIPv6": net.enable_ipv6,
            "IPAM": net.ipam,
            "Internal": net.internal,
            "Attachable": net.attachable,
            "Ingress": net.ingress,
            "Containers": {},
            "Options": net.options,
            "Labels": net.labels,
        }));
    }
    list
}

pub async fn list_networks(
    state: &Arc<DockerState>,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let filters = head.query_param("filters").map(parse_filters).transpose()?;
    let mut list = all_networks_json(state);
    if let Some(f) = &filters {
        list.retain(|item| {
            if let Some(names) = f.get("name") {
                let n = item["Name"].as_str().unwrap_or("");
                if !names.iter().any(|w| n == w || n.contains(w)) {
                    return false;
                }
            }
            if let Some(ids) = f.get("id") {
                let id = item["Id"].as_str().unwrap_or("");
                if !ids.iter().any(|w| id.starts_with(w)) {
                    return false;
                }
            }
            if let Some(drivers) = f.get("driver") {
                let d = item["Driver"].as_str().unwrap_or("");
                if !drivers.iter().any(|w| d == w) {
                    return false;
                }
            }
            true
        });
    }
    write_json(write, 200, &list, head.close_after).await?;
    Ok(RouteOutcome::Continue)
}

pub async fn inspect_network(
    state: &Arc<DockerState>,
    id_or_name: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let found = all_networks_json(state).into_iter().find(|n| {
        n["Name"].as_str() == Some(id_or_name)
            || n["Id"].as_str().is_some_and(|id| id == id_or_name || id.starts_with(id_or_name))
    });
    match found {
        Some(n) => write_json(write, 200, &n, head.close_after).await?,
        None => write_error(write, 404, &format!("network {id_or_name} not found"), head.close_after).await?,
    }
    Ok(RouteOutcome::Continue)
}

pub async fn create_network(
    state: &Arc<DockerState>,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    let body = read_body_bytes(reader, write, head, MAX_JSON_BODY).await?;
    let payload: Value = serde_json::from_slice(&body).context("parsing network create JSON")?;
    let name = payload["Name"].as_str().unwrap_or("").trim().to_string();
    if name.is_empty() {
        write_error(write, 400, "network Name is required", head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }
    if matches!(name.as_str(), "bridge" | "host" | "none") {
        write_error(write, 409, &format!("network with name {name} already exists"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }

    let id = make_network_id();
    let driver = payload["Driver"].as_str().unwrap_or("bridge").to_string();
    let internal = payload["Internal"].as_bool().unwrap_or(false);
    let attachable = payload["Attachable"].as_bool().unwrap_or(true);
    let enable_ipv6 = payload["EnableIPv6"].as_bool().unwrap_or(false);
    let ipam = payload
        .get("IPAM")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| json!({ "Driver": "default", "Config": [{ "Subnet": "192.168.64.0/24", "Gateway": "192.168.64.1" }] }));
    let mut labels = HashMap::new();
    if let Some(obj) = payload["Labels"].as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                labels.insert(k.clone(), s.to_string());
            }
        }
    }
    let mut options = HashMap::new();
    if let Some(obj) = payload["Options"].as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                options.insert(k.clone(), s.to_string());
            }
        }
    }

    let dup = {
        let mut nets = state.networks.lock().unwrap();
        if nets.values().any(|n| n.name == name) {
            true
        } else {
            nets.insert(
                id.clone(),
                LogicalNetwork {
                    name: name.clone(),
                    id: id.clone(),
                    created: now_rfc3339(),
                    scope: "local".into(),
                    driver,
                    enable_ipv6,
                    ipam,
                    internal,
                    attachable,
                    ingress: false,
                    options,
                    labels,
                },
            );
            false
        }
    };
    if dup {
        write_error(write, 409, &format!("network with name {name} already exists"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }
    write_json(write, 201, &json!({ "Id": id, "Warning": "" }), head.close_after).await?;
    Ok(RouteOutcome::Continue)
}

pub async fn remove_network(
    state: &Arc<DockerState>,
    id_or_name: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    if matches!(id_or_name, "bridge" | "host" | "none" | "graviton-bridge" | "graviton-host" | "graviton-none") {
        write_error(write, 403, &format!("{id_or_name} is a pre-defined network and cannot be removed"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }
    let found = {
        let mut nets = state.networks.lock().unwrap();
        let key = nets
            .iter()
            .find(|(id, n)| n.name == id_or_name || *id == id_or_name || id.starts_with(id_or_name))
            .map(|(id, _)| id.clone());
        key.and_then(|k| nets.remove(&k)).is_some()
    };
    if found {
        write_empty(write, 204, &[], head.close_after).await?;
    } else {
        write_error(write, 404, &format!("network {id_or_name} not found"), head.close_after).await?;
    }
    Ok(RouteOutcome::Continue)
}

pub async fn connect_network(
    state: &Arc<DockerState>,
    id_or_name: &str,
    connect: bool,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    let body = read_body_bytes(reader, write, head, MAX_JSON_BODY).await?;
    let payload: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let container_key = payload["Container"].as_str().unwrap_or("").trim();

    let Some(net_name) = all_networks_json(state).into_iter().find_map(|n| {
        let name = n["Name"].as_str()?.to_string();
        let id = n["Id"].as_str().unwrap_or("");
        (name == id_or_name || id == id_or_name || id.starts_with(id_or_name)).then_some(name)
    }) else {
        write_error(write, 404, &format!("network {id_or_name} not found"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };

    let found = {
        let store = state.backend.store().lock().unwrap();
        store.container(container_key).map(|c| c.id.clone()).map_err(|e| e.to_string())
    };
    let cid = match found {
        Ok(id) => id,
        Err(msg) => {
            write_error(write, 404, &msg, head.close_after).await?;
            return Ok(RouteOutcome::Continue);
        }
    };

    {
        let mut meta = state.container_meta.lock().unwrap();
        let entry = meta.entry(cid).or_default();
        if connect {
            if !entry.networks.contains(&net_name) {
                entry.networks.push(net_name);
            }
        } else {
            entry.networks.retain(|n| n != &net_name);
        }
    }

    write_empty(write, 200, &[], head.close_after).await?;
    Ok(RouteOutcome::Continue)
}

pub async fn prune_networks(
    state: &Arc<DockerState>,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let deleted: Vec<String> = state.networks.lock().unwrap().drain().map(|(_, n)| n.name).collect();
    write_json(write, 200, &json!({ "NetworksDeleted": deleted }), head.close_after).await?;
    Ok(RouteOutcome::Continue)
}
