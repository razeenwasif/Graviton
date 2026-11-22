//! Docker Engine REST API compatibility shim (`docker.sock`).
//!
//! Exposes a subset of the Docker Engine REST API v1.47 over a Unix domain
//! socket so standard Docker tools (`docker` CLI, Docker Compose, VS Code /
//! Proton Dev Containers, Testcontainers, SDKs) drive Graviton VMs transparently.

pub mod containers;
pub mod http;
pub mod images;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use graviton_proto::EngineEvent;
use serde::Serialize;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, oneshot, Notify};

use crate::Backend;
use crate::runtime::{Engine, AttachedRun, new_id, now_secs};
use http::{RequestHead, drain_body, read_request_head, write_error};

#[derive(Default)]
pub struct DockerContainerMeta {
    pub tty: bool,
    pub open_stdin: bool,
    pub auto_remove: bool,
    pub console_rows: u16,
    pub console_cols: u16,
    pub started_once: bool,
    pub active_attaches: usize,
    pub networks: Vec<String>,
    pub last_exit_code: Option<i32>,
    pub tty_resize_tx: Option<mpsc::UnboundedSender<(u16, u16)>>,
    pub pending_tty_attach: Option<oneshot::Sender<AttachedRun>>,
    pub exit_notify: Option<Arc<Notify>>,
    pub removed_notify: Option<Arc<Notify>>,
    pub was_removed: bool,
}

#[derive(Clone)]
pub struct ExecRecord {
    pub id: String,
    pub container_id: String,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
    pub tty: bool,
    pub attach_stdin: bool,
    pub attach_stdout: bool,
    pub attach_stderr: bool,
    pub rows: u16,
    pub cols: u16,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub pid: Option<i32>,
    pub resize_tx: Option<mpsc::UnboundedSender<(u16, u16)>>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct LogicalNetwork {
    pub name: String,
    pub id: String,
    pub created: String,
    pub scope: String,
    pub driver: String,
    #[serde(rename = "EnableIPv6")]
    pub enable_ipv6: bool,
    #[serde(rename = "IPAM")]
    pub ipam: serde_json::Value,
    pub internal: bool,
    pub attachable: bool,
    pub ingress: bool,
    pub options: HashMap<String, String>,
    pub labels: HashMap<String, String>,
}

pub struct DockerState {
    pub backend: Backend,
    pub execs: Mutex<HashMap<String, ExecRecord>>,
    pub container_meta: Mutex<HashMap<String, DockerContainerMeta>>,
    pub networks: Mutex<HashMap<String, LogicalNetwork>>,
    pub events_tx: broadcast::Sender<EngineEvent>,
}

impl DockerState {
    pub fn new(backend: Backend) -> Arc<Self> {
        let (events_tx, _) = broadcast::channel(256);
        if let Backend::Engine(engine) = &backend {
            let tx = events_tx.clone();
            engine.events.subscribe(move |ev| {
                let _ = tx.send(ev.clone());
            });
        }
        Arc::new(Self {
            backend,
            execs: Mutex::new(HashMap::new()),
            container_meta: Mutex::new(HashMap::new()),
            networks: Mutex::new(HashMap::new()),
            events_tx,
        })
    }

    pub fn engine(&self) -> Option<&Arc<Engine>> {
        match &self.backend {
            Backend::Engine(e) => Some(e),
            Backend::Demo(_) => None,
        }
    }

    pub fn ensure_exit_notify(&self, id: &str) -> Arc<Notify> {
        let mut map = self.container_meta.lock().unwrap();
        let meta = map.entry(id.to_string()).or_default();
        Arc::clone(meta.exit_notify.get_or_insert_with(|| Arc::new(Notify::new())))
    }

    pub fn ensure_removed_notify(&self, id: &str) -> Arc<Notify> {
        let mut map = self.container_meta.lock().unwrap();
        let meta = map.entry(id.to_string()).or_default();
        Arc::clone(meta.removed_notify.get_or_insert_with(|| Arc::new(Notify::new())))
    }

    /// Spawn a background watcher that waits for container `id` to exit,
    /// stores its final exit code, wakes any `/wait` callers, and deletes the
    /// container if `HostConfig.AutoRemove` was requested.
    pub fn spawn_exit_watcher(self: &Arc<Self>, engine: &Arc<Engine>, id: &str) {
        let state = Arc::clone(self);
        let engine = Arc::clone(engine);
        let id = id.to_string();
        let exit_notify = self.ensure_exit_notify(&id);
        let removed_notify = self.ensure_removed_notify(&id);
        tokio::spawn(async move {
            let mut saw_exit = false;
            loop {
                let check = {
                    let store = engine.store.lock().unwrap();
                    store
                        .container(&id)
                        .ok()
                        .map(|c| (c.status, c.exit_code))
                };
                match check {
                    Some((graviton_proto::ContainerStatus::Exited, code)) => {
                        let exit_code = code.unwrap_or(0);
                        let mut meta = state.container_meta.lock().unwrap();
                        if let Some(m) = meta.get_mut(&id) {
                            m.last_exit_code = Some(exit_code);
                        }
                        saw_exit = true;
                        break;
                    }
                    None => {
                        // Already removed externally or by Engine's own auto_remove.
                        let mut meta = state.container_meta.lock().unwrap();
                        if let Some(m) = meta.get_mut(&id) {
                            m.was_removed = true;
                        }
                        break;
                    }
                    Some(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
                }
            }
            exit_notify.notify_waiters();

            if saw_exit {
                let auto_remove = state
                    .container_meta
                    .lock()
                    .unwrap()
                    .get(&id)
                    .is_some_and(|m| m.auto_remove);
                if auto_remove {
                    // Let attached /logs or /attach readers drain buffered output first.
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
                    loop {
                        let active = state
                            .container_meta
                            .lock()
                            .unwrap()
                            .get(&id)
                            .map_or(0, |m| m.active_attaches);
                        if active == 0 || tokio::time::Instant::now() >= deadline {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
                    }
                    let e = Arc::clone(&engine);
                    let cid = id.clone();
                    let _ = tokio::task::spawn_blocking(move || e.remove(&cid)).await;
                    if let Some(m) = state.container_meta.lock().unwrap().get_mut(&id) {
                        m.was_removed = true;
                    }
                }
            }
            removed_notify.notify_waiters();
        });
    }
}

pub fn make_network_id() -> String {
    format!("{}{}", new_id(), new_id())
}

pub fn now_rfc3339() -> String {
    crate::build::rfc3339(now_secs())
}

pub async fn serve_connection(stream: UnixStream, state: Arc<DockerState>) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    loop {
        let Some(head) = read_request_head(&mut reader).await? else {
            return Ok(());
        };
        let close_after = head.close_after;
        let outcome = dispatch(&state, head, &mut reader, &mut write).await?;
        match outcome {
            RouteOutcome::Continue => {
                if close_after {
                    return Ok(());
                }
            }
            RouteOutcome::Done => return Ok(()),
        }
    }
}

pub enum RouteOutcome {
    Continue,
    Done,
}

async fn dispatch(
    state: &Arc<DockerState>,
    head: RequestHead,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    write: &mut tokio::net::unix::OwnedWriteHalf,
) -> Result<RouteOutcome> {
    let method = head.method.as_str();
    let path = head.path.as_str();

    // System & ping
    match (method, path) {
        ("GET" | "HEAD", "/_ping") => return images::ping(&head, reader, write).await,
        ("GET", "/version") => return images::version(state, &head, reader, write).await,
        ("GET", "/info") => return images::info(state, &head, reader, write).await,
        ("GET", "/system/df") => return images::system_df(state, &head, reader, write).await,
        ("GET", "/events") => return images::events(state, &head, reader, write).await,

        // Images & Build
        ("GET", "/images/json") => return images::list_images(state, &head, reader, write).await,
        ("POST", "/images/create") => return images::pull_image(state, &head, reader, write).await,
        ("POST", "/images/prune") => return images::prune_images(state, &head, reader, write).await,
        ("POST", "/build") => return images::build_image(state, &head, reader, write).await,

        // Volumes
        ("GET", "/volumes") => return images::list_volumes(state, &head, reader, write).await,
        ("POST", "/volumes/create") => return images::create_volume(state, &head, reader, write).await,
        ("POST", "/volumes/prune") => return images::prune_volumes(state, &head, reader, write).await,

        // Networks
        ("GET", "/networks") => return images::list_networks(state, &head, reader, write).await,
        ("POST", "/networks/create") => return images::create_network(state, &head, reader, write).await,
        ("POST", "/networks/prune") => return images::prune_networks(state, &head, reader, write).await,

        // Containers collection
        ("GET", "/containers/json") => return containers::list_containers(state, &head, reader, write).await,
        ("POST", "/containers/create") => return containers::create_container(state, &head, reader, write).await,
        ("POST", "/containers/prune") => return containers::prune_containers(state, &head, reader, write).await,
        _ => {}
    }

    // Parameterized `/images/{name}/...` (image names contain `/`!)
    if let Some(rest) = path.strip_prefix("/images/") {
        if let Some(name) = rest.strip_suffix("/json").filter(|n| !n.is_empty()) {
            if method == "GET" {
                return images::inspect_image(state, name, &head, reader, write).await;
            }
        } else if let Some(name) = rest.strip_suffix("/tag").filter(|n| !n.is_empty()) {
            if method == "POST" {
                return images::tag_image(state, name, &head, reader, write).await;
            }
        } else if let Some(name) = rest.strip_suffix("/push").filter(|n| !n.is_empty()) {
            if method == "POST" {
                return images::push_image(state, name, &head, reader, write).await;
            }
        } else if !rest.is_empty() && !rest.ends_with('/') && method == "DELETE" {
            return images::remove_image(state, rest, &head, reader, write).await;
        }
    }

    // Parameterized `/volumes/{name}`
    if let Some(name) = path.strip_prefix("/volumes/").filter(|n| !n.is_empty() && !n.contains('/')) {
        match method {
            "GET" => return images::inspect_volume(state, name, &head, reader, write).await,
            "DELETE" => return images::remove_volume(state, name, &head, reader, write).await,
            _ => {}
        }
    }

    // Parameterized `/networks/{id}[/connect|/disconnect]`
    if let Some(rest) = path.strip_prefix("/networks/") {
        let segs: Vec<&str> = rest.split('/').collect();
        match (method, segs.as_slice()) {
            ("GET", [id]) if !id.is_empty() => return images::inspect_network(state, id, &head, reader, write).await,
            ("DELETE", [id]) if !id.is_empty() => return images::remove_network(state, id, &head, reader, write).await,
            ("POST", [id, "connect"]) if !id.is_empty() => {
                return images::connect_network(state, id, true, &head, reader, write).await;
            }
            ("POST", [id, "disconnect"]) if !id.is_empty() => {
                return images::connect_network(state, id, false, &head, reader, write).await;
            }
            _ => {}
        }
    }

    // Parameterized `/exec/{id}/...`
    if let Some(rest) = path.strip_prefix("/exec/") {
        let segs: Vec<&str> = rest.split('/').collect();
        match (method, segs.as_slice()) {
            ("POST", [id, "start"]) if !id.is_empty() => {
                return containers::exec_start(state, id, &head, reader, write).await;
            }
            ("POST", [id, "resize"]) if !id.is_empty() => {
                return containers::exec_resize(state, id, &head, reader, write).await;
            }
            ("GET", [id, "json"]) if !id.is_empty() => {
                return containers::exec_inspect(state, id, &head, reader, write).await;
            }
            _ => {}
        }
    }

    // Parameterized `/containers/{id}[/...]`
    if let Some(rest) = path.strip_prefix("/containers/") {
        let segs: Vec<&str> = rest.split('/').collect();
        match (method, segs.as_slice()) {
            ("DELETE", [id]) if !id.is_empty() => {
                return containers::remove_container(state, id, &head, reader, write).await;
            }
            ("GET", [id, "json"]) if !id.is_empty() => {
                return containers::inspect_container(state, id, &head, reader, write).await;
            }
            ("POST", [id, "start"]) if !id.is_empty() => {
                return containers::start_container(state, id, &head, reader, write).await;
            }
            ("POST", [id, "stop"]) if !id.is_empty() => {
                return containers::stop_container(state, id, &head, reader, write).await;
            }
            ("POST", [id, "kill"]) if !id.is_empty() => {
                return containers::kill_container(state, id, &head, reader, write).await;
            }
            ("POST", [id, "restart"]) if !id.is_empty() => {
                return containers::restart_container(state, id, &head, reader, write).await;
            }
            ("POST", [id, "wait"]) if !id.is_empty() => {
                return containers::wait_container(state, id, &head, reader, write).await;
            }
            ("GET", [id, "logs"]) if !id.is_empty() => {
                return containers::container_logs(state, id, &head, reader, write).await;
            }
            ("POST", [id, "attach"]) if !id.is_empty() => {
                return containers::attach_container(state, id, &head, reader, write).await;
            }
            ("POST", [id, "resize"]) if !id.is_empty() => {
                return containers::resize_container(state, id, &head, reader, write).await;
            }
            ("GET", [id, "top"]) if !id.is_empty() => {
                return containers::container_top(state, id, &head, reader, write).await;
            }
            ("GET", [id, "stats"]) if !id.is_empty() => {
                return containers::container_stats(state, id, &head, reader, write).await;
            }
            ("HEAD", [id, "archive"]) if !id.is_empty() => {
                return containers::head_archive(state, id, &head, reader, write).await;
            }
            ("GET", [id, "archive"]) if !id.is_empty() => {
                return containers::get_archive(state, id, &head, reader, write).await;
            }
            ("PUT", [id, "archive"]) if !id.is_empty() => {
                return containers::put_archive(state, id, &head, reader, write).await;
            }
            ("POST", [id, "exec"]) if !id.is_empty() => {
                return containers::create_exec(state, id, &head, reader, write).await;
            }
            _ => {}
        }
    }

    drain_body(reader, write, &head).await?;
    write_error(write, 404, "page not found", head.close_after).await?;
    Ok(RouteOutcome::Continue)
}

#[cfg(test)]
mod tests;
