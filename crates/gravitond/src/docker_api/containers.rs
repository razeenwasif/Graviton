//! Docker Engine REST API endpoints for `/containers/*` and `/exec/*`.

use std::collections::{BTreeMap, HashMap};
use std::net::{TcpListener, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use graviton_proto::{
    AgentEvent, AgentRequest, ContainerStatus, ContainerSummary, ExecSpec, FileKind, HealthStatus, Healthcheck, LogLevel,
    LogStream, PruneSpec, RestartPolicy, RunSpec, decode_line, encode_line,
    frame::{Decoder, Frame},
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};

use crate::cleanup;
use crate::runtime::{ContainerSpec, Engine, new_id, now_secs};
use crate::{Backend, upload};

use super::http::{
    MAX_JSON_BODY, RequestHead, drain_body, encode_mux_frame, finish_chunked, percent_decode, read_body_bytes,
    read_body_to_file, write_bytes, write_chunk, write_chunk_json, write_chunked_head, write_empty, write_error,
    write_json, write_stream_start,
};
use super::{DockerContainerMeta, DockerState, ExecRecord, RouteOutcome};

struct AttachGuard {
    state: Arc<DockerState>,
    id: String,
}

impl AttachGuard {
    fn acquire(state: &Arc<DockerState>, id: &str) -> Self {
        let mut meta = state.container_meta.lock().unwrap();
        let entry = meta.entry(id.to_string()).or_default();
        entry.active_attaches = entry.active_attaches.saturating_add(1);
        Self { state: Arc::clone(state), id: id.to_string() }
    }
}

impl Drop for AttachGuard {
    fn drop(&mut self) {
        if let Ok(mut meta) = self.state.container_meta.lock() {
            if let Some(entry) = meta.get_mut(&self.id) {
                entry.active_attaches = entry.active_attaches.saturating_sub(1);
            }
        }
    }
}

struct TempPathGuard(std::path::PathBuf);
impl Drop for TempPathGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ---------------------------------------------------------------------------
// GET /containers/json
// ---------------------------------------------------------------------------

pub async fn list_containers(
    state: &Arc<DockerState>,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let all = head.query_bool("all", false);
    let limit = head.query_param("limit").and_then(|s| s.parse::<usize>().ok());
    let filters = head.query_param("filters").map(parse_filters).transpose()?;

    let now = now_secs();
    let summaries = state.backend.store().lock().unwrap().containers().to_vec();
    let mut out = Vec::new();
    for c in summaries {
        if !all && !matches!(c.status, ContainerStatus::Running | ContainerStatus::Paused) {
            continue;
        }
        let spec = match &state.backend {
            Backend::Engine(e) => e.specs.lock().unwrap().get(&c.id).cloned(),
            Backend::Demo(_) => None,
        };
        if let Some(f) = &filters {
            if !matches_container_filters(&c, spec.as_ref(), f) {
                continue;
            }
        }
        let networks = state
            .container_meta
            .lock()
            .unwrap()
            .get(&c.id)
            .map(|m| m.networks.clone())
            .unwrap_or_default();
        out.push(container_list_json(&c, spec.as_ref(), &networks, now));
        if limit.is_some_and(|n| n > 0 && out.len() >= n) {
            break;
        }
    }

    write_json(write, 200, &out, head.close_after).await?;
    Ok(RouteOutcome::Continue)
}

/// Parse Docker `filters` JSON into `key -> Vec<value>`, accepting both Go map
/// syntax (`{"label":{"k=v":true}}`) and array syntax (`{"label":["k=v"]}`).
pub fn parse_filters(raw: &str) -> Result<HashMap<String, Vec<String>>> {
    let val: Value = serde_json::from_str(raw).context("invalid `filters` JSON")?;
    let Some(obj) = val.as_object() else {
        return Ok(HashMap::new());
    };
    let mut out = HashMap::new();
    for (k, v) in obj {
        let mut list = Vec::new();
        match v {
            Value::Array(arr) => {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        list.push(s.to_string());
                    }
                }
            }
            Value::Object(map) => {
                for (entry, enabled) in map {
                    if enabled.as_bool().unwrap_or(true) {
                        list.push(entry.clone());
                    }
                }
            }
            _ => {}
        }
        if !list.is_empty() {
            out.insert(k.to_ascii_lowercase(), list);
        }
    }
    Ok(out)
}

pub(crate) fn matches_container_filters(
    c: &ContainerSummary,
    spec: Option<&ContainerSpec>,
    filters: &HashMap<String, Vec<String>>,
) -> bool {
    let empty_labels = BTreeMap::new();
    let labels = spec.map(|s| &s.labels).unwrap_or(&empty_labels);

    // Docker semantics: ALL `label` filter criteria must match (AND).
    if let Some(want_labels) = filters.get("label") {
        for expr in want_labels {
            if let Some((k, v)) = expr.split_once("!=") {
                if labels.get(k).is_some_and(|actual| actual == v) {
                    return false;
                }
            } else if let Some((k, v)) = expr.split_once('=') {
                if labels.get(k).map(String::as_str) != Some(v) {
                    return false;
                }
            } else if !labels.contains_key(expr.as_str()) {
                return false;
            }
        }
    }

    if let Some(ids) = filters.get("id") {
        if !ids.iter().any(|want| c.id.starts_with(want) || c.id == *want) {
            return false;
        }
    }

    if let Some(names) = filters.get("name") {
        let cname = c.name.trim_start_matches('/');
        if !names.iter().any(|want| {
            let w = want.trim_start_matches('/');
            cname == w || cname.contains(w)
        }) {
            return false;
        }
    }

    if let Some(statuses) = filters.get("status") {
        let s = docker_state_str(c.status);
        if !statuses.iter().any(|want| want.eq_ignore_ascii_case(s)) {
            return false;
        }
    }

    if let Some(exited) = filters.get("exited") {
        if c.status != ContainerStatus::Exited
            || !exited.iter().any(|want| want.parse::<i32>().ok() == c.exit_code)
        {
            return false;
        }
    }

    if let Some(ancestors) = filters.get("ancestor") {
        let full_ref = spec.map(|s| s.image.reference.as_str()).unwrap_or("");
        let config_id = spec.map(|s| s.image.config_digest.as_str()).unwrap_or("");
        if !ancestors.iter().any(|want| {
            c.image == *want
                || c.image.split(':').next() == Some(want.as_str())
                || full_ref == want.as_str()
                || config_id == want.as_str()
        }) {
            return false;
        }
    }

    if let Some(volumes) = filters.get("volume") {
        if !volumes.iter().any(|want| c.mounts.iter().any(|m| m.source == *want || m.target == *want)) {
            return false;
        }
    }

    if let Some(want_health) = filters.get("health") {
        let actual = c.health.map_or("none", HealthStatus::as_str);
        if !want_health.iter().any(|want| want.eq_ignore_ascii_case(actual)) {
            return false;
        }
    }

    true
}

fn docker_state_str(status: ContainerStatus) -> &'static str {
    match status {
        ContainerStatus::Created => "created",
        ContainerStatus::Running => "running",
        ContainerStatus::Paused => "paused",
        ContainerStatus::Exited => "exited",
    }
}

pub(crate) fn docker_status_human(c: &ContainerSummary, now: u64) -> String {
    let format_up = |started_at: Option<u64>| {
        let secs = now.saturating_sub(started_at.unwrap_or(now));
        if secs < 2 {
            "Up Less than a second".into()
        } else if secs < 60 {
            format!("Up {secs} seconds")
        } else if secs < 3600 {
            format!("Up {} minutes", secs / 60)
        } else {
            format!("Up {} hours", secs / 3600)
        }
    };
    match c.status {
        ContainerStatus::Created => "Created".into(),
        ContainerStatus::Running => {
            let base = format_up(c.started_at);
            match c.health {
                Some(HealthStatus::Starting) => format!("{base} (health: starting)"),
                Some(HealthStatus::Healthy) => format!("{base} (healthy)"),
                Some(HealthStatus::Unhealthy) => format!("{base} (unhealthy)"),
                None => base,
            }
        }
        ContainerStatus::Paused => format!("{} (Paused)", format_up(c.started_at)),
        ContainerStatus::Exited => format!("Exited ({})", c.exit_code.unwrap_or(0)),
    }
}

fn docker_ports_list(c: &ContainerSummary) -> Vec<Value> {
    c.ports
        .iter()
        .map(|p| {
            let mut obj = json!({
                "IP": p.bind_ip(),
                "PrivatePort": p.container_port,
                "PublicPort": p.host_port,
                "Type": match p.protocol {
                    graviton_proto::PortProtocol::Tcp => "tcp",
                    graviton_proto::PortProtocol::Udp => "udp",
                }
            });
            if p.host_ip.as_deref() == Some("0.0.0.0") {
                obj["IP"] = json!("0.0.0.0");
            }
            obj
        })
        .collect()
}

fn docker_mounts_list(c: &ContainerSummary) -> Vec<Value> {
    c.mounts
        .iter()
        .map(|m| {
            let is_bind = m.source.starts_with('/') || m.source.starts_with('~');
            if is_bind {
                json!({
                    "Type": "bind",
                    "Source": m.source,
                    "Destination": m.target,
                    "Mode": if m.read_only { "ro" } else { "rw" },
                    "RW": !m.read_only,
                    "Propagation": "rprivate"
                })
            } else {
                json!({
                    "Type": "volume",
                    "Name": m.source,
                    "Source": graviton_core::paths::volumes_dir().join(&m.source).display().to_string(),
                    "Destination": m.target,
                    "Driver": "local",
                    "Mode": if m.read_only { "ro" } else { "rw" },
                    "RW": !m.read_only,
                    "Propagation": ""
                })
            }
        })
        .collect()
}

fn container_list_json(
    c: &ContainerSummary,
    spec: Option<&ContainerSpec>,
    extra_networks: &[String],
    now: u64,
) -> Value {
    let command = spec.map(|s| s.argv.join(" ")).unwrap_or_default();
    let image_id = spec.map(|s| s.image.config_digest.as_str()).unwrap_or("");
    let labels = spec.map(|s| &s.labels).cloned().unwrap_or_default();
    let networks = build_networks_map(c, extra_networks);

    json!({
        "Id": c.id,
        "Names": [format!("/{}", c.name)],
        "Image": c.image,
        "ImageID": image_id,
        "Command": command,
        "Created": c.created_at,
        "Ports": docker_ports_list(c),
        "Labels": labels,
        "State": docker_state_str(c.status),
        "Status": docker_status_human(c, now),
        "HostConfig": { "NetworkMode": "bridge" },
        "NetworkSettings": { "Networks": networks },
        "Mounts": docker_mounts_list(c),
    })
}

fn build_networks_map(c: &ContainerSummary, extra_networks: &[String]) -> Value {
    let ip = c.ip_address.as_deref().unwrap_or("");
    let gateway = if ip.is_empty() { "" } else { "192.168.64.1" };
    let mut nets = serde_json::Map::new();
    let entry = json!({
        "NetworkID": "graviton-bridge",
        "EndpointID": if ip.is_empty() { Value::String(String::new()) } else { json!(c.id) },
        "Gateway": gateway,
        "IPAddress": ip,
        "IPPrefixLen": if ip.is_empty() { 0 } else { 24 },
        "MacAddress": "",
        "Aliases": Value::Null,
    });
    if extra_networks.is_empty() {
        nets.insert("bridge".into(), entry);
    } else {
        for n in extra_networks {
            nets.insert(n.clone(), entry.clone());
        }
    }
    Value::Object(nets)
}

// ---------------------------------------------------------------------------
// POST /containers/create
// ---------------------------------------------------------------------------

pub async fn create_container(
    state: &Arc<DockerState>,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    let Some(engine) = state.engine().cloned() else {
        drain_body(reader, write, head).await?;
        write_error(write, 501, "create requires a live engine", head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };

    let body = read_body_bytes(reader, write, head, MAX_JSON_BODY).await?;
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            write_error(write, 400, &format!("invalid JSON body: {err}"), head.close_after).await?;
            return Ok(RouteOutcome::Continue);
        }
    };

    let image = payload["Image"].as_str().unwrap_or("").trim().to_string();
    if image.is_empty() {
        write_error(write, 400, "Config.Image is required", head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }

    let name = head
        .query_param("name")
        .map(|n| n.trim_start_matches('/').trim().to_string())
        .filter(|n| !n.is_empty());
    let platform = head
        .query_param("platform")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let command = parse_string_or_array(&payload["Cmd"]);
    let entrypoint = if payload["Entrypoint"].is_null() {
        None
    } else {
        parse_string_or_array(&payload["Entrypoint"])
    };
    let env: Vec<String> = payload["Env"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let working_dir = payload["WorkingDir"].as_str().map(String::from).filter(|s| !s.is_empty());
    let user = payload["User"].as_str().map(String::from).filter(|s| !s.is_empty());
    let hostname = payload["Hostname"].as_str().map(String::from).filter(|s| !s.is_empty());

    let mut labels = BTreeMap::new();
    if let Some(obj) = payload["Labels"].as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                labels.insert(k.clone(), s.to_string());
            }
        }
    }
    let project = labels.get("com.docker.compose.project").cloned();
    let service = labels.get("com.docker.compose.service").cloned();

    let tty = payload["Tty"].as_bool().unwrap_or(false);
    let open_stdin = payload["OpenStdin"].as_bool().unwrap_or(false);

    let hc = &payload["HostConfig"];
    let auto_remove = hc["AutoRemove"].as_bool().unwrap_or(false);

    let mut volumes = Vec::new();
    if let Some(binds) = hc["Binds"].as_array() {
        for b in binds {
            if let Some(raw) = b.as_str() {
                if let Some(mapped) = normalize_docker_bind(raw) {
                    volumes.push(mapped);
                }
            }
        }
    }
    if let Some(mounts) = hc["Mounts"].as_array() {
        for m in mounts {
            if let Some(mapped) = normalize_docker_mount(m) {
                volumes.push(mapped);
            }
        }
    }

    let ports = parse_port_bindings(&hc["PortBindings"]);

    let restart = match hc["RestartPolicy"]["Name"].as_str().unwrap_or("") {
        "always" | "unless-stopped" => Some(RestartPolicy::Always),
        "on-failure" => Some(RestartPolicy::OnFailure),
        _ => None,
    };

    let memory_mib = hc["Memory"]
        .as_u64()
        .filter(|&b| b > 0)
        .map(|b| (b >> 20).max(128));

    let cpus = hc["NanoCpus"]
        .as_u64()
        .filter(|&n| n > 0)
        .map(|n| (n / 1_000_000_000).max(1) as u32)
        .or_else(|| hc["CpuCount"].as_u64().filter(|&n| n > 0).map(|n| n as u32));

    let mut networks = Vec::new();
    let mut dns_aliases = Vec::new();
    if let Some(ep_map) = payload["NetworkingConfig"]["EndpointsConfig"].as_object() {
        for (net_name, cfg) in ep_map {
            networks.push(net_name.clone());
            if let Some(aliases) = cfg["Aliases"].as_array() {
                for a in aliases {
                    if let Some(s) = a.as_str().filter(|s| !s.is_empty()) {
                        dns_aliases.push(s.to_string());
                    }
                }
            }
        }
    }
    if let Some(mode) = hc["NetworkMode"].as_str().filter(|m| !m.is_empty() && *m != "default") {
        if networks.is_empty() {
            networks.push(mode.to_string());
        }
    }

    let healthcheck = payload.get("Healthcheck").and_then(|v| v.as_object()).map(|hc| {
        let test: Vec<String> = hc
            .get("Test")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let ns_to_secs = |k: &str| -> u64 {
            let ns = hc.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
            if ns <= 0 { 0 } else { (ns as u64).div_ceil(1_000_000_000) }
        };
        Healthcheck {
            test,
            interval_secs: ns_to_secs("Interval"),
            timeout_secs: ns_to_secs("Timeout"),
            start_period_secs: ns_to_secs("StartPeriod"),
            retries: hc.get("Retries").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        }
    });

    let spec = RunSpec {
        image: image.clone(),
        name,
        command,
        env,
        cpus,
        memory_mib,
        ports,
        volumes,
        restart,
        tty,
        rows: 0,
        cols: 0,
        auto_remove,
        platform,
        entrypoint,
        working_dir,
        user,
        hostname,
        project,
        service,
        labels,
        dns_aliases,
        healthcheck,
    };

    let res = tokio::task::spawn_blocking(move || engine.create_container_local(spec)).await?;
    match res {
        Ok(id) => {
            state.container_meta.lock().unwrap().insert(
                id.clone(),
                DockerContainerMeta {
                    tty,
                    open_stdin,
                    auto_remove,
                    networks,
                    ..DockerContainerMeta::default()
                },
            );
            write_json(write, 201, &json!({ "Id": id, "Warnings": [] }), head.close_after).await?;
        }
        Err(err) => {
            let msg = format!("{err:#}");
            let status = if msg.starts_with("No such image:") {
                404
            } else if msg.contains("already in use") {
                409
            } else {
                400
            };
            write_error(write, status, &msg, head.close_after).await?;
        }
    }
    Ok(RouteOutcome::Continue)
}

fn parse_string_or_array(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::Array(arr) => Some(arr.iter().filter_map(|x| x.as_str().map(String::from)).collect()),
        Value::String(s) if !s.is_empty() => Some(vec![s.clone()]),
        _ => None,
    }
}

/// Normalize a Docker bind string (`SRC:DST[:opts]`) into `SRC:DST[:ro]`,
/// stripping macOS/Linux advisory mount options like `cached`, `delegated`,
/// `consistent`, `z`, `Z`, `rprivate`, `bind`.
pub(crate) fn normalize_docker_bind(spec: &str) -> Option<String> {
    let parts: Vec<&str> = spec.split(':').collect();
    match parts.as_slice() {
        [src, dst] if !dst.is_empty() => {
            let src = if src.is_empty() { format!("vol-{}", new_id()) } else { (*src).to_string() };
            Some(format!("{src}:{dst}"))
        }
        [src, dst, opts] if !dst.is_empty() => {
            let ro = opts.split(',').any(|o| o.trim().eq_ignore_ascii_case("ro"));
            let src = if src.is_empty() { format!("vol-{}", new_id()) } else { (*src).to_string() };
            Some(if ro { format!("{src}:{dst}:ro") } else { format!("{src}:{dst}") })
        }
        _ => None,
    }
}

pub(crate) fn normalize_docker_mount(m: &Value) -> Option<String> {
    let kind = m["Type"].as_str().unwrap_or("bind");
    let target = m["Target"].as_str().filter(|t| t.starts_with('/'))?;
    let ro = m["ReadOnly"].as_bool().unwrap_or(false);
    let suffix = if ro { ":ro" } else { "" };
    match kind {
        "bind" => {
            let source = m["Source"].as_str().filter(|s| !s.is_empty())?;
            Some(format!("{source}:{target}{suffix}"))
        }
        "volume" => {
            let source = m["Source"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| format!("vol-{}", new_id()));
            Some(format!("{source}:{target}{suffix}"))
        }
        // `tmpfs` mounts aren't virtiofs shares on macOS; skip gracefully rather than failing create.
        _ => None,
    }
}

fn parse_port_bindings(v: &Value) -> Vec<String> {
    let Some(map) = v.as_object() else {
        return vec![];
    };
    let mut out = Vec::new();
    for (cport_proto, bindings) in map {
        let (cport, proto) = match cport_proto.split_once('/') {
            Some((p, pr)) => (p, pr),
            None => (cport_proto.as_str(), "tcp"),
        };
        let Some(arr) = bindings.as_array() else {
            continue;
        };
        if arr.is_empty() {
            if let Some(hp) = alloc_ephemeral_port("127.0.0.1", proto) {
                out.push(format!("{hp}:{cport}/{proto}"));
            }
            continue;
        }
        for b in arr {
            let host_ip = b["HostIp"].as_str().map(str::trim).unwrap_or("");
            let bind_ip = if host_ip.is_empty() { "127.0.0.1" } else { host_ip };
            let raw_hp = b["HostPort"].as_str().map(str::trim).unwrap_or("");
            let host_port = if raw_hp.is_empty() || raw_hp == "0" {
                match alloc_ephemeral_port(bind_ip, proto) {
                    Some(p) => p.to_string(),
                    None => continue,
                }
            } else {
                raw_hp.to_string()
            };
            if host_ip.is_empty() || host_ip == "127.0.0.1" {
                out.push(format!("{host_port}:{cport}/{proto}"));
            } else if host_ip.contains(':') && !host_ip.starts_with('[') {
                out.push(format!("[{host_ip}]:{host_port}:{cport}/{proto}"));
            } else {
                out.push(format!("{host_ip}:{host_port}:{cport}/{proto}"));
            }
        }
    }
    out
}

fn alloc_ephemeral_port(ip: &str, proto: &str) -> Option<u16> {
    match proto.to_ascii_lowercase().as_str() {
        "udp" => UdpSocket::bind((ip, 0)).ok()?.local_addr().ok().map(|a| a.port()),
        _ => TcpListener::bind((ip, 0)).ok()?.local_addr().ok().map(|a| a.port()),
    }
}

// ---------------------------------------------------------------------------
// GET /containers/{id}/json
// ---------------------------------------------------------------------------

pub async fn inspect_container(
    state: &Arc<DockerState>,
    raw_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let key = raw_id.trim_start_matches('/');
    let Some(engine) = state.engine().cloned() else {
        write_error(write, 404, &format!("No such container: {key}"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };
    let key_owned = key.to_string();
    let res = tokio::task::spawn_blocking(move || engine.inspect(&key_owned)).await?;
    let details = match res {
        Ok(d) => d,
        Err(err) => {
            write_error(write, 404, &format!("{err:#}"), head.close_after).await?;
            return Ok(RouteOutcome::Continue);
        }
    };

    let c = &details.container;
    let (tty, open_stdin, auto_remove, networks_list) = {
        let meta = state.container_meta.lock().unwrap();
        let m = meta.get(&c.id);
        (
            m.is_some_and(|x| x.tty),
            m.is_some_and(|x| x.open_stdin),
            m.is_some_and(|x| x.auto_remove),
            m.map(|x| x.networks.clone()).unwrap_or_default(),
        )
    };

    let mut port_bindings = serde_json::Map::new();
    let mut ns_ports = serde_json::Map::new();
    let mut exposed_ports = serde_json::Map::new();
    for p in &c.ports {
        let key = format!(
            "{}/{}",
            p.container_port,
            match p.protocol {
                graviton_proto::PortProtocol::Tcp => "tcp",
                graviton_proto::PortProtocol::Udp => "udp",
            }
        );
        exposed_ports.insert(key.clone(), json!({}));
        let entry = json!({
            "HostIp": p.bind_ip(),
            "HostPort": p.host_port.to_string(),
        });
        port_bindings
            .entry(key.clone())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .unwrap()
            .push(entry.clone());
        ns_ports
            .entry(key)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .unwrap()
            .push(entry);
    }

    let binds: Vec<String> = c
        .mounts
        .iter()
        .map(|mnt| {
            if mnt.read_only {
                format!("{}:{}:ro", mnt.source, mnt.target)
            } else {
                format!("{}:{}", mnt.source, mnt.target)
            }
        })
        .collect();

    let created = crate::build::rfc3339(c.created_at);
    let started_at = c
        .started_at
        .map(crate::build::rfc3339)
        .unwrap_or_else(|| "0001-01-01T00:00:00Z".into());
    let path = details.command.first().cloned().unwrap_or_default();
    let args: Vec<String> = details.command.iter().skip(1).cloned().collect();
    let running = matches!(c.status, ContainerStatus::Running | ContainerStatus::Paused);
    let networks = build_networks_map(c, &networks_list);
    let restart_name = details.restart.as_str();

    let mut inspect_json = json!({
        "Id": c.id,
        "Created": created,
        "Path": path,
        "Args": args,
        "State": {
            "Status": docker_state_str(c.status),
            "Running": running,
            "Paused": c.status == ContainerStatus::Paused,
            "Restarting": false,
            "OOMKilled": false,
            "Dead": false,
            "Pid": if running { 1 } else { 0 },
            "ExitCode": c.exit_code.unwrap_or(0),
            "Error": "",
            "StartedAt": started_at,
            "FinishedAt": "0001-01-01T00:00:00Z"
        },
        "Image": details.image_digest,
        "ResolvConfPath": "",
        "HostnamePath": "",
        "HostsPath": "",
        "LogPath": "",
        "Name": format!("/{}", c.name),
        "RestartCount": 0,
        "Driver": "graviton",
        "Platform": "linux",
        "MountLabel": "",
        "ProcessLabel": "",
        "AppArmorProfile": "",
        "ExecIDs": Value::Null,
        "HostConfig": {
            "Binds": binds,
            "ContainerIDFile": "",
            "LogConfig": { "Type": "json-file", "Config": {} },
            "NetworkMode": networks_list.first().map(String::as_str).unwrap_or("bridge"),
            "PortBindings": Value::Object(port_bindings),
            "RestartPolicy": { "Name": restart_name, "MaximumRetryCount": 0 },
            "AutoRemove": auto_remove,
            "Memory": details.memory_mib << 20,
            "NanoCpus": u64::from(details.cpus) * 1_000_000_000,
        },
        "Config": {
            "Hostname": details.hostname,
            "Domainname": "",
            "User": details.user.clone().unwrap_or_default(),
            "AttachStdin": open_stdin,
            "AttachStdout": true,
            "AttachStderr": true,
            "ExposedPorts": Value::Object(exposed_ports),
            "Tty": tty,
            "OpenStdin": open_stdin,
            "StdinOnce": false,
            "Env": details.env,
            "Cmd": details.command,
            "Image": details.image_reference,
            "Volumes": Value::Null,
            "WorkingDir": details.working_dir,
            "Entrypoint": Value::Null,
            "Labels": details.labels,
        },
        "Mounts": docker_mounts_list(c),
        "NetworkSettings": {
            "Bridge": "",
            "Gateway": c.ip_address.as_deref().map(|_| "192.168.64.1").unwrap_or(""),
            "IPAddress": c.ip_address.clone().unwrap_or_default(),
            "IPPrefixLen": if c.ip_address.is_some() { 24 } else { 0 },
            "Ports": Value::Object(ns_ports),
            "Networks": networks,
        }
    });
    if let Some(h) = &details.health {
        let log: Vec<Value> = h
            .log
            .iter()
            .map(|entry| {
                json!({
                    "Start": crate::build::rfc3339(entry.start),
                    "End": crate::build::rfc3339(entry.end),
                    "ExitCode": entry.exit_code,
                    "Output": entry.output,
                })
            })
            .collect();
        inspect_json["State"]["Health"] = json!({
            "Status": h.status.as_str(),
            "FailingStreak": h.failing_streak,
            "Log": log,
        });
    }
    if let Some(hc) = &details.healthcheck {
        inspect_json["Config"]["Healthcheck"] = json!({
            "Test": hc.test,
            "Interval": i64::try_from(hc.interval_secs.saturating_mul(1_000_000_000)).unwrap_or(i64::MAX),
            "Timeout": i64::try_from(hc.timeout_secs.saturating_mul(1_000_000_000)).unwrap_or(i64::MAX),
            "StartPeriod": i64::try_from(hc.start_period_secs.saturating_mul(1_000_000_000)).unwrap_or(i64::MAX),
            "Retries": hc.retries,
        });
    }

    write_json(write, 200, &inspect_json, head.close_after).await?;
    Ok(RouteOutcome::Continue)
}

// ---------------------------------------------------------------------------
// POST /containers/{id}/start|stop|kill|restart & DELETE /containers/{id}
// ---------------------------------------------------------------------------

pub async fn start_container(
    state: &Arc<DockerState>,
    raw_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let key = raw_id.trim_start_matches('/').to_string();

    if let Backend::Demo(store) = &state.backend {
        let res = store.lock().unwrap().start(&key);
        return match res {
            Ok(_) => {
                write_empty(write, 204, &[], head.close_after).await?;
                Ok(RouteOutcome::Continue)
            }
            Err(graviton_core::store::StoreError::InvalidState { .. }) => {
                write_empty(write, 304, &[], head.close_after).await?;
                Ok(RouteOutcome::Continue)
            }
            Err(e) => {
                write_error(write, 404, &e.to_string(), head.close_after).await?;
                Ok(RouteOutcome::Continue)
            }
        };
    }

    let engine = state.engine().cloned().unwrap();
    let resolved = {
        let store = engine.store.lock().unwrap();
        store.container(&key).map(|c| (c.id.clone(), c.status)).map_err(|e| e.to_string())
    };
    let (id, status) = match resolved {
        Ok(pair) => pair,
        Err(msg) => {
            write_error(write, 404, &msg, head.close_after).await?;
            return Ok(RouteOutcome::Continue);
        }
    };

    if status == ContainerStatus::Running {
        write_empty(write, 304, &[], head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }

    let (pending_tty, rows, cols) = {
        let mut map = state.container_meta.lock().unwrap();
        let meta = map.entry(id.clone()).or_default();
        meta.started_once = true;
        meta.last_exit_code = None;
        meta.exit_notify.get_or_insert_with(|| Arc::new(tokio::sync::Notify::new()));
        meta.removed_notify.get_or_insert_with(|| Arc::new(tokio::sync::Notify::new()));
        (
            meta.pending_tty_attach.take(),
            if meta.console_rows == 0 { 24 } else { meta.console_rows },
            if meta.console_cols == 0 { 80 } else { meta.console_cols },
        )
    };

    if let Some(tty_tx) = pending_tty {
        let e = Arc::clone(&engine);
        let cid = id.clone();
        let res = tokio::task::spawn_blocking(move || e.start_tty(&cid, rows, cols)).await?;
        match res {
            Ok(attached) => {
                let _ = tty_tx.send(attached);
                write_empty(write, 204, &[], head.close_after).await?;
            }
            Err(err) => {
                write_error(write, 500, &format!("{err:#}"), head.close_after).await?;
            }
        }
        return Ok(RouteOutcome::Continue);
    }

    let e = Arc::clone(&engine);
    let cid = id.clone();
    let res = tokio::task::spawn_blocking(move || e.start(&cid)).await?;
    match res {
        Ok(_) => {
            state.spawn_exit_watcher(&engine, &id);
            write_empty(write, 204, &[], head.close_after).await?;
        }
        Err(err) => {
            write_error(write, 500, &format!("{err:#}"), head.close_after).await?;
        }
    }
    Ok(RouteOutcome::Continue)
}

pub async fn stop_container(
    state: &Arc<DockerState>,
    raw_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let key = raw_id.trim_start_matches('/').to_string();

    if let Backend::Demo(store) = &state.backend {
        let res = store.lock().unwrap().stop(&key);
        return match res {
            Ok(_) => {
                write_empty(write, 204, &[], head.close_after).await?;
                Ok(RouteOutcome::Continue)
            }
            Err(graviton_core::store::StoreError::InvalidState { .. }) => {
                write_empty(write, 304, &[], head.close_after).await?;
                Ok(RouteOutcome::Continue)
            }
            Err(e) => {
                write_error(write, 404, &e.to_string(), head.close_after).await?;
                Ok(RouteOutcome::Continue)
            }
        };
    }

    let engine = state.engine().cloned().unwrap();
    let resolved = {
        let store = engine.store.lock().unwrap();
        store.container(&key).map(|c| c.status).map_err(|e| e.to_string())
    };
    let status = match resolved {
        Ok(s) => s,
        Err(msg) => {
            write_error(write, 404, &msg, head.close_after).await?;
            return Ok(RouteOutcome::Continue);
        }
    };
    if status != ContainerStatus::Running && status != ContainerStatus::Paused {
        write_empty(write, 304, &[], head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }
    let res = tokio::task::spawn_blocking(move || engine.stop(&key)).await?;
    match res {
        Ok(_) => write_empty(write, 204, &[], head.close_after).await?,
        Err(err) => write_error(write, 500, &format!("{err:#}"), head.close_after).await?,
    }
    Ok(RouteOutcome::Continue)
}

pub async fn kill_container(
    state: &Arc<DockerState>,
    raw_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let key = raw_id.trim_start_matches('/').to_string();
    let signal = head.query_param("signal").map(parse_signal_num).unwrap_or(9);

    let Some(engine) = state.engine().cloned() else {
        write_error(write, 404, &format!("No such container: {key}"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };
    let resolved = {
        let store = engine.store.lock().unwrap();
        store.container(&key).map(|c| c.status).map_err(|e| e.to_string())
    };
    let status = match resolved {
        Ok(s) => s,
        Err(msg) => {
            write_error(write, 404, &msg, head.close_after).await?;
            return Ok(RouteOutcome::Continue);
        }
    };
    if !matches!(status, ContainerStatus::Running | ContainerStatus::Paused) {
        write_error(write, 409, &format!("container {key} is not running"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }

    let res = tokio::task::spawn_blocking(move || engine.signal_container(&key, signal)).await?;
    match res {
        Ok(()) => write_empty(write, 204, &[], head.close_after).await?,
        Err(err) => write_error(write, 500, &format!("{err:#}"), head.close_after).await?,
    }
    Ok(RouteOutcome::Continue)
}

fn parse_signal_num(s: &str) -> i32 {
    if let Ok(n) = s.trim().parse::<i32>() {
        return n;
    }
    match s.trim().trim_start_matches("SIG").to_ascii_uppercase().as_str() {
        "HUP" => 1,
        "INT" => 2,
        "QUIT" => 3,
        "KILL" => 9,
        "USR1" => 10,
        "USR2" => 12,
        "TERM" => 15,
        _ => 15,
    }
}

pub async fn restart_container(
    state: &Arc<DockerState>,
    raw_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let key = raw_id.trim_start_matches('/').to_string();

    if let Backend::Demo(store) = &state.backend {
        let res = store.lock().unwrap().restart(&key);
        return match res {
            Ok(_) => {
                write_empty(write, 204, &[], head.close_after).await?;
                Ok(RouteOutcome::Continue)
            }
            Err(e) => {
                write_error(write, 404, &e.to_string(), head.close_after).await?;
                Ok(RouteOutcome::Continue)
            }
        };
    }

    let engine = state.engine().cloned().unwrap();
    let exists = engine.store.lock().unwrap().container(&key).is_ok();
    if !exists {
        write_error(write, 404, &format!("no such container: {key}"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }
    let e = Arc::clone(&engine);
    let cid = key.clone();
    let res = tokio::task::spawn_blocking(move || e.restart(&cid)).await?;
    match res {
        Ok(c) => {
            state.spawn_exit_watcher(&engine, &c.id);
            write_empty(write, 204, &[], head.close_after).await?;
        }
        Err(err) => write_error(write, 500, &format!("{err:#}"), head.close_after).await?,
    }
    Ok(RouteOutcome::Continue)
}

async fn write_demo_lifecycle_result<T>(
    res: std::result::Result<T, graviton_core::store::StoreError>,
    key: &str,
    head: &RequestHead,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    match res {
        Ok(_) => write_empty(write, 204, &[], head.close_after).await?,
        Err(graviton_core::store::StoreError::InvalidState { state: s, .. }) => {
            write_error(write, 409, &format!("Container {key} is {s}"), head.close_after).await?;
        }
        Err(e) => {
            write_error(write, 404, &e.to_string(), head.close_after).await?;
        }
    }
    Ok(RouteOutcome::Continue)
}

pub async fn pause_container(
    state: &Arc<DockerState>,
    raw_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let key = raw_id.trim_start_matches('/').to_string();

    if let Backend::Demo(store) = &state.backend {
        let res = store.lock().unwrap().pause(&key);
        return write_demo_lifecycle_result(res, &key, head, write).await;
    }

    let engine = state.engine().cloned().unwrap();
    let status_res = engine.store.lock().unwrap().container(&key).map(|c| c.status);
    let status = match status_res {
        Ok(s) => s,
        Err(e) => {
            write_error(write, 404, &e.to_string(), head.close_after).await?;
            return Ok(RouteOutcome::Continue);
        }
    };
    if status == ContainerStatus::Paused {
        write_error(write, 409, &format!("Container {key} is already paused"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }
    if status != ContainerStatus::Running {
        write_error(write, 409, &format!("Container {key} is not running"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }
    let res = tokio::task::spawn_blocking(move || engine.pause(&key)).await?;
    match res {
        Ok(_) => write_empty(write, 204, &[], head.close_after).await?,
        Err(err) => {
            let msg = format!("{err:#}");
            write_error(write, exec_error_status(&msg), &msg, head.close_after).await?;
        }
    }
    Ok(RouteOutcome::Continue)
}

pub async fn unpause_container(
    state: &Arc<DockerState>,
    raw_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let key = raw_id.trim_start_matches('/').to_string();

    if let Backend::Demo(store) = &state.backend {
        let res = store.lock().unwrap().unpause(&key);
        return write_demo_lifecycle_result(res, &key, head, write).await;
    }

    let engine = state.engine().cloned().unwrap();
    let status_res = engine.store.lock().unwrap().container(&key).map(|c| c.status);
    let status = match status_res {
        Ok(s) => s,
        Err(e) => {
            write_error(write, 404, &e.to_string(), head.close_after).await?;
            return Ok(RouteOutcome::Continue);
        }
    };
    if status != ContainerStatus::Paused {
        write_error(write, 409, &format!("Container {key} is not paused"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }
    let res = tokio::task::spawn_blocking(move || engine.unpause(&key)).await?;
    match res {
        Ok(_) => write_empty(write, 204, &[], head.close_after).await?,
        Err(err) => {
            let msg = format!("{err:#}");
            write_error(write, exec_error_status(&msg), &msg, head.close_after).await?;
        }
    }
    Ok(RouteOutcome::Continue)
}

pub async fn remove_container(
    state: &Arc<DockerState>,
    raw_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let key = raw_id.trim_start_matches('/').to_string();
    let force = head.query_bool("force", false);

    let Some(engine) = state.engine().cloned() else {
        write_error(write, 501, "remove requires a live engine", head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };
    let resolved = {
        let store = engine.store.lock().unwrap();
        store.container(&key).map(|c| (c.id.clone(), c.status)).map_err(|e| e.to_string())
    };
    let info = match resolved {
        Ok(pair) => pair,
        Err(msg) => {
            write_error(write, 404, &msg, head.close_after).await?;
            return Ok(RouteOutcome::Continue);
        }
    };
    if matches!(info.1, ContainerStatus::Running | ContainerStatus::Paused) && !force {
        write_error(
            write,
            409,
            &format!("container {} is running; stop it before removing or pass force=1", info.0),
            head.close_after,
        )
        .await?;
        return Ok(RouteOutcome::Continue);
    }

    let id = info.0;
    let res = tokio::task::spawn_blocking(move || {
        if force && matches!(info.1, ContainerStatus::Running | ContainerStatus::Paused) {
            let _ = engine.stop(&id);
        }
        engine.remove(&id)
    })
    .await?;

    match res {
        Ok(removed_id) => {
            if let Some(m) = state.container_meta.lock().unwrap().get_mut(&removed_id) {
                m.was_removed = true;
            }
            if let Some(n) = state
                .container_meta
                .lock()
                .unwrap()
                .get(&removed_id)
                .and_then(|m| m.removed_notify.clone())
            {
                n.notify_waiters();
            }
            write_empty(write, 204, &[], head.close_after).await?;
        }
        Err(err) => write_error(write, 500, &format!("{err:#}"), head.close_after).await?,
    }
    Ok(RouteOutcome::Continue)
}

pub async fn prune_containers(
    state: &Arc<DockerState>,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let Some(engine) = state.engine().cloned() else {
        write_json(write, 200, &json!({ "ContainersDeleted": [], "SpaceReclaimed": 0 }), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };
    let report = tokio::task::spawn_blocking(move || {
        cleanup::prune(&engine, &PruneSpec { containers: true, images: false, dry_run: false })
    })
    .await??;
    write_json(
        write,
        200,
        &json!({
            "ContainersDeleted": report.containers,
            "SpaceReclaimed": report.freed_bytes
        }),
        head.close_after,
    )
    .await?;
    Ok(RouteOutcome::Continue)
}

// ---------------------------------------------------------------------------
// POST /containers/{id}/wait
// ---------------------------------------------------------------------------

pub async fn wait_container(
    state: &Arc<DockerState>,
    raw_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let key = raw_id.trim_start_matches('/').to_string();
    let condition = head.query_param("condition").unwrap_or("not-running");

    let Some(engine) = state.engine().cloned() else {
        write_error(write, 404, &format!("No such container: {key}"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };

    let resolved = {
        let store = engine.store.lock().unwrap();
        store.container(&key).map(|c| (c.id.clone(), c.status, c.exit_code)).map_err(|e| e.to_string())
    };
    let (id, initial_status, initial_code) = match resolved {
        Ok(triple) => (triple.0, Some(triple.1), triple.2),
        Err(msg) => {
            let cached = state
                .container_meta
                .lock()
                .unwrap()
                .get(&key)
                .and_then(|m| m.last_exit_code);
            if let Some(code) = cached {
                write_json(write, 200, &json!({ "StatusCode": code, "Error": Value::Null }), head.close_after).await?;
                return Ok(RouteOutcome::Continue);
            }
            write_error(write, 404, &msg, head.close_after).await?;
            return Ok(RouteOutcome::Continue);
        }
    };

    if condition != "removed" && initial_status == Some(ContainerStatus::Exited) {
        let meta_started = state
            .container_meta
            .lock()
            .unwrap()
            .get(&id)
            .is_some_and(|m| m.started_once);
        if condition == "not-running" || meta_started {
            let code = initial_code.unwrap_or(0);
            write_json(write, 200, &json!({ "StatusCode": code, "Error": Value::Null }), head.close_after).await?;
            return Ok(RouteOutcome::Continue);
        }
    }

    // Flush 200 OK headers immediately (chunked encoding) so Go's Docker
    // client returns the response stream and proceeds to `/start` instead of
    // blocking on `wait` response headers.
    write_chunked_head(write, 200, "application/json", &[]).await?;

    let mut exit_code: i32 = initial_code.unwrap_or(0);
    let mut saw_running_or_started = false;
    loop {
        let (store_state, meta_info) = {
            let s = engine.store.lock().unwrap().container(&id).ok().map(|c| (c.status, c.exit_code));
            let m = state
                .container_meta
                .lock()
                .unwrap()
                .get(&id)
                .map(|m| (m.started_once, m.last_exit_code, m.was_removed))
                .unwrap_or((false, None, false));
            (s, m)
        };
        if meta_info.0 {
            saw_running_or_started = true;
        }
        match store_state {
            None => {
                exit_code = meta_info.1.unwrap_or(exit_code);
                break;
            }
            Some((ContainerStatus::Running, _)) => {
                saw_running_or_started = true;
            }
            Some((ContainerStatus::Exited, code)) => {
                exit_code = meta_info.1.or(code).unwrap_or(0);
                if condition != "removed" && (condition == "not-running" || saw_running_or_started) {
                    break;
                }
                if condition == "removed" && meta_info.2 {
                    break;
                }
            }
            Some(_) => {}
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    if write_chunk_json(write, &json!({ "StatusCode": exit_code, "Error": Value::Null })).await.is_ok() {
        let _ = finish_chunked(write).await;
    }
    Ok(RouteOutcome::Continue)
}

// ---------------------------------------------------------------------------
// GET /containers/{id}/logs
// ---------------------------------------------------------------------------

pub async fn container_logs(
    state: &Arc<DockerState>,
    raw_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let key = raw_id.trim_start_matches('/');
    let want_stdout = head.query_bool("stdout", false);
    let want_stderr = head.query_bool("stderr", false);
    let follow = head.query_bool("follow", false);
    let timestamps = head.query_bool("timestamps", false);
    let tail = match head.query_param("tail") {
        Some("all") | None => None,
        Some(n) => n.parse::<usize>().ok(),
    };
    let since_secs = head.query_param("since").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);

    let resolved = {
        let store = state.backend.store().lock().unwrap();
        store.container(key).map(|c| {
            let lines = store.logs(&c.id, tail).unwrap_or_default();
            (c.id.clone(), lines, c.status)
        }).map_err(|e| e.to_string())
    };
    let (id, initial_lines, initial_status) = match resolved {
        Ok(triple) => triple,
        Err(msg) => {
            write_error(write, 404, &msg, head.close_after).await?;
            return Ok(RouteOutcome::Continue);
        }
    };

    let is_tty = state
        .container_meta
        .lock()
        .unwrap()
        .get(&id)
        .is_some_and(|m| m.tty);

    let format_line = |l: &graviton_proto::LogLine| -> Option<Vec<u8>> {
        if (since_secs > 0 && (l.timestamp_ms / 1000) < since_secs) || !want_stream(l.stream, want_stdout, want_stderr) {
            return None;
        }
        let text = if timestamps {
            format!("{} {}\n", crate::build::rfc3339(l.timestamp_ms / 1000), l.message)
        } else {
            format!("{}\n", l.message)
        };
        if is_tty {
            Some(text.into_bytes())
        } else {
            let stype = match l.stream {
                LogStream::Stderr => 2,
                _ => 1,
            };
            Some(encode_mux_frame(stype, text.as_bytes()))
        }
    };

    let ctype = if is_tty {
        "application/vnd.docker.raw-stream"
    } else {
        "application/vnd.docker.multiplexed-stream"
    };

    if !follow || initial_status == ContainerStatus::Exited {
        let mut payload = Vec::new();
        for l in &initial_lines {
            if let Some(chunk) = format_line(l) {
                payload.extend_from_slice(&chunk);
            }
        }
        write_bytes(write, 200, ctype, &payload, &[], head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }

    write_chunked_head(write, 200, ctype, &[]).await?;
    let mut last_seq = 0u64;
    for l in &initial_lines {
        last_seq = last_seq.max(l.seq);
        if let Some(chunk) = format_line(l) {
            if write_chunk(write, &chunk).await.is_err() {
                return Ok(RouteOutcome::Done);
            }
        }
    }

    loop {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let (lines, still_running) = {
            let store = state.backend.store().lock().unwrap();
            let lines = store.logs(&id, None).unwrap_or_default();
            let running = store
                .container(&id)
                .is_ok_and(|c| c.status == ContainerStatus::Running || c.status == ContainerStatus::Created);
            (lines, running)
        };
        for l in lines {
            if l.seq > last_seq {
                last_seq = l.seq;
                if let Some(chunk) = format_line(&l) {
                    if write_chunk(write, &chunk).await.is_err() {
                        return Ok(RouteOutcome::Done);
                    }
                }
            }
        }
        if !still_running {
            break;
        }
    }
    let _ = finish_chunked(write).await;
    Ok(RouteOutcome::Continue)
}

fn want_stream(stream: LogStream, stdout: bool, stderr: bool) -> bool {
    match stream {
        LogStream::Stdout => stdout || !stderr,
        LogStream::Stderr => stderr,
        LogStream::System => false,
    }
}

// ---------------------------------------------------------------------------
// POST /containers/{id}/attach & /resize
// ---------------------------------------------------------------------------

pub async fn attach_container(
    state: &Arc<DockerState>,
    raw_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    let key = raw_id.trim_start_matches('/').to_string();
    let Some(engine) = state.engine().cloned() else {
        drain_body(reader, write, head).await?;
        write_error(write, 501, "attach requires a live engine", head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };

    let resolved = {
        let store = engine.store.lock().unwrap();
        store.container(&key).map(|c| {
            let tail_seq = store.logs(&c.id, None).ok().and_then(|l| l.last().map(|x| x.seq)).unwrap_or(0);
            (c.id.clone(), c.status, tail_seq)
        }).map_err(|e| e.to_string())
    };
    let (id, initial_status, existing_tail_seq) = match resolved {
        Ok(triple) => triple,
        Err(msg) => {
            drain_body(reader, write, head).await?;
            write_error(write, 404, &msg, head.close_after).await?;
            return Ok(RouteOutcome::Continue);
        }
    };

    let want_logs = head.query_bool("logs", false);
    let stream_output = head.query_bool("stream", true);
    let want_stdout = head.query_bool("stdout", true);
    let want_stderr = head.query_bool("stderr", true);
    let is_tty = state
        .container_meta
        .lock()
        .unwrap()
        .get(&id)
        .is_some_and(|m| m.tty);

    let _guard = AttachGuard::acquire(state, &id);
    let ctype = if is_tty {
        "application/vnd.docker.raw-stream"
    } else {
        "application/vnd.docker.multiplexed-stream"
    };
    write_stream_start(write, head, ctype).await?;

    if is_tty {
        let (tx, rx) = oneshot::channel();
        {
            let mut meta = state.container_meta.lock().unwrap();
            let m = meta.entry(id.clone()).or_default();
            m.pending_tty_attach = Some(tx);
        }
        let attached = match tokio::time::timeout(Duration::from_secs(60), rx).await {
            Ok(Ok(a)) => a,
            _ => return Ok(RouteOutcome::Done),
        };
        let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();
        {
            let mut meta = state.container_meta.lock().unwrap();
            if let Some(m) = meta.get_mut(&id) {
                m.tty_resize_tx = Some(resize_tx);
            }
        }

        let guest = std::os::unix::net::UnixStream::from(attached.guest_fd);
        guest.set_nonblocking(true)?;
        let guest = UnixStream::from_std(guest)?;
        let (mut guest_in, mut guest_out) = guest.into_split();

        let pending = reader.buffer().to_vec();
        let read_half = reader.get_mut();

        // Pump client stdin & resize events to guest PTY.
        let mut client_read = std::mem::replace(read_half, unsafe_dummy_read().await);
        // Note: take OwnedReadHalf cleanly by returning Done below.
        let stdin_task = tokio::spawn(async move {
            if !pending.is_empty() && guest_out.write_all(&Frame::Data(pending).encode()).await.is_err() {
                return;
            }
            let mut buf = [0u8; 4096];
            let mut eof_sent = false;
            loop {
                tokio::select! {
                    biased;
                    Some((rows, cols)) = resize_rx.recv() => {
                        if guest_out.write_all(&Frame::Resize { rows, cols }.encode()).await.is_err() {
                            break;
                        }
                    }
                    n = client_read.read(&mut buf), if !eof_sent => {
                        match n {
                            Ok(0) => {
                                eof_sent = true;
                                let _ = guest_out.write_all(&Frame::Eof.encode()).await;
                            }
                            Ok(n) => {
                                if guest_out.write_all(&Frame::Data(buf[..n].to_vec()).encode()).await.is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        });

        let mut decoder = Decoder::default();
        let mut buf = [0u8; 16 * 1024];
        let mut exit_code: Option<i32> = None;
        'tty: loop {
            match guest_in.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => decoder.push(&buf[..n]),
            }
            loop {
                match decoder.next_frame() {
                    Ok(Some(Frame::Data(bytes))) => {
                        if write.write_all(&bytes).await.is_err() {
                            break 'tty;
                        }
                        let _ = write.flush().await;
                    }
                    Ok(Some(Frame::Exit(code))) => {
                        exit_code = Some(code);
                        break 'tty;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => break 'tty,
                }
            }
        }
        stdin_task.abort();
        let final_code = exit_code.unwrap_or(129);
        {
            let mut meta = state.container_meta.lock().unwrap();
            if let Some(m) = meta.get_mut(&id) {
                m.last_exit_code = Some(final_code);
                m.tty_resize_tx = None;
            }
        }
        let _ = attached.exit_tx.send(final_code);
        let _ = attached.stopped_rx.await;
        if let Some(n) = state
            .container_meta
            .lock()
            .unwrap()
            .get(&id)
            .and_then(|m| m.exit_notify.clone())
        {
            n.notify_waiters();
        }
        let _ = write.shutdown().await;
        return Ok(RouteOutcome::Done);
    }

    // Non-TTY multiplexed stream attach.
    let mut next_seq = if want_logs || initial_status == ContainerStatus::Created {
        0
    } else {
        existing_tail_seq
    };
    let mut saw_started = initial_status == ContainerStatus::Running;
    loop {
        let (lines, status, meta_started) = {
            let store = engine.store.lock().unwrap();
            let lines = store.logs(&id, None).unwrap_or_default();
            let status = store.container(&id).ok().map(|c| c.status);
            let started = state
                .container_meta
                .lock()
                .unwrap()
                .get(&id)
                .is_some_and(|m| m.started_once);
            (lines, status, started)
        };
        if meta_started || status == Some(ContainerStatus::Running) {
            saw_started = true;
        }
        for l in lines {
            if l.seq > next_seq {
                next_seq = l.seq;
                if want_stream(l.stream, want_stdout, want_stderr) {
                    let stype = if l.stream == LogStream::Stderr { 2 } else { 1 };
                    let payload = format!("{}\n", l.message);
                    if write.write_all(&encode_mux_frame(stype, payload.as_bytes())).await.is_err() {
                        return Ok(RouteOutcome::Done);
                    }
                    let _ = write.flush().await;
                }
            }
        }
        if !stream_output {
            break;
        }
        match status {
            None => break,
            Some(ContainerStatus::Exited) if saw_started || initial_status == ContainerStatus::Exited => {
                // If boot failed (exit 125) and no stdout/stderr was produced, emit last system error to stderr.
                if next_seq == 0 {
                    let err_msg = engine
                        .store
                        .lock()
                        .unwrap()
                        .logs(&id, Some(20))
                        .unwrap_or_default()
                        .into_iter()
                        .rev()
                        .find(|l| l.stream == LogStream::System && l.level == LogLevel::Error)
                        .map(|l| format!("{}\n", l.message));
                    if let Some(msg) = err_msg {
                        let _ = write.write_all(&encode_mux_frame(2, msg.as_bytes())).await;
                    }
                }
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(15)).await,
        }
    }
    let _ = write.shutdown().await;
    Ok(RouteOutcome::Done)
}

async fn unsafe_dummy_read() -> OwnedReadHalf {
    let (a, _b) = UnixStream::pair().expect("unix stream pair");
    a.into_split().0
}

pub async fn resize_container(
    state: &Arc<DockerState>,
    raw_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let key = raw_id.trim_start_matches('/');
    let rows = head.query_param("h").and_then(|s| s.parse::<u16>().ok()).unwrap_or(24);
    let cols = head.query_param("w").and_then(|s| s.parse::<u16>().ok()).unwrap_or(80);
    let id = match state.engine().and_then(|e| e.store.lock().unwrap().container(key).ok().map(|c| c.id.clone())) {
        Some(id) => id,
        None => {
            write_error(write, 404, &format!("No such container: {key}"), head.close_after).await?;
            return Ok(RouteOutcome::Continue);
        }
    };
    if let Some(m) = state.container_meta.lock().unwrap().get_mut(&id) {
        m.console_rows = rows;
        m.console_cols = cols;
        if let Some(tx) = &m.tty_resize_tx {
            let _ = tx.send((rows, cols));
        }
    }
    write_empty(write, 200, &[], head.close_after).await?;
    Ok(RouteOutcome::Continue)
}

// ---------------------------------------------------------------------------
// GET /containers/{id}/top & /stats
// ---------------------------------------------------------------------------

pub async fn container_top(
    state: &Arc<DockerState>,
    raw_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let key = raw_id.trim_start_matches('/').to_string();
    let Some(engine) = state.engine().cloned() else {
        write_error(write, 404, &format!("No such container: {key}"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };
    let stats = match tokio::task::spawn_blocking(move || engine.stats(&key)).await? {
        Ok(s) => s,
        Err(err) => {
            write_error(write, 404, &format!("{err:#}"), head.close_after).await?;
            return Ok(RouteOutcome::Continue);
        }
    };
    let processes: Vec<Vec<String>> = stats
        .processes
        .into_iter()
        .map(|p| {
            vec![
                p.uid.to_string(),
                p.pid.to_string(),
                p.ppid.to_string(),
                p.state,
                format!("{} KB", p.rss_bytes / 1024),
                p.command,
            ]
        })
        .collect();
    write_json(
        write,
        200,
        &json!({
            "Titles": ["UID", "PID", "PPID", "STAT", "RSS", "CMD"],
            "Processes": processes
        }),
        head.close_after,
    )
    .await?;
    Ok(RouteOutcome::Continue)
}

pub async fn container_stats(
    state: &Arc<DockerState>,
    raw_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let key = raw_id.trim_start_matches('/').to_string();
    let stream = head.query_bool("stream", true);
    let oneshot = head.query_bool("one-shot", false);

    let Some(engine) = state.engine().cloned() else {
        write_error(write, 404, &format!("No such container: {key}"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };

    let build_stats_json = |st: &graviton_proto::ContainerStats, name: &str| {
        let last = st.samples.last();
        let mem = last.map_or(0, |s| s.memory_bytes);
        let limit = last.map_or(0, |s| s.memory_limit_bytes);
        let rx = last.map_or(0, |s| s.net_rx_bytes);
        let tx = last.map_or(0, |s| s.net_tx_bytes);
        let cpu_ns = last.map_or(0u64, |s| (s.cpu_percent * 10_000_000.0) as u64);
        json!({
            "read": crate::build::rfc3339(now_secs()),
            "id": st.id,
            "name": format!("/{name}"),
            "cpu_stats": {
                "cpu_usage": { "total_usage": cpu_ns },
                "system_cpu_usage": 1_000_000_000u64 * u64::from(st.cpus.max(1)),
                "online_cpus": st.cpus.max(1),
            },
            "precpu_stats": {
                "cpu_usage": { "total_usage": 0 },
                "system_cpu_usage": 0,
                "online_cpus": st.cpus.max(1),
            },
            "memory_stats": { "usage": mem, "limit": limit },
            "networks": { "eth0": { "rx_bytes": rx, "tx_bytes": tx } }
        })
    };

    let resolved = {
        let store = engine.store.lock().unwrap();
        store.container(&key).map(|c| c.name.clone()).map_err(|e| e.to_string())
    };
    let cname = match resolved {
        Ok(n) => n,
        Err(msg) => {
            write_error(write, 404, &msg, head.close_after).await?;
            return Ok(RouteOutcome::Continue);
        }
    };

    if !stream || oneshot {
        let k = key.clone();
        let e = Arc::clone(&engine);
        let st = tokio::task::spawn_blocking(move || e.stats(&k)).await??;
        write_json(write, 200, &build_stats_json(&st, &cname), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }

    write_chunked_head(write, 200, "application/json", &[]).await?;
    loop {
        let k = key.clone();
        let e = Arc::clone(&engine);
        let Ok(Ok(st)) = tokio::task::spawn_blocking(move || e.stats(&k)).await else {
            break;
        };
        let mut line = serde_json::to_vec(&build_stats_json(&st, &cname))?;
        line.push(b'\n');
        if write_chunk(write, &line).await.is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let _ = finish_chunked(write).await;
    Ok(RouteOutcome::Done)
}

// ---------------------------------------------------------------------------
// HEAD/GET/PUT /containers/{id}/archive
// ---------------------------------------------------------------------------

fn stat_container_path(engine: &Engine, key: &str, raw_path: &str) -> Result<String> {
    let cleaned = raw_path.trim();
    let trimmed = if cleaned == "/" {
        "/"
    } else {
        cleaned.trim_end_matches('/')
    };
    let stat_json = if trimmed == "/" {
        json!({
            "name": "/",
            "size": 0,
            "mode": 0x8000_0000u64 | 0o755,
            "mtime": crate::build::rfc3339(now_secs()),
            "linkTarget": "",
        })
    } else {
        let (parent, base) = match trimmed.rfind('/') {
            Some(0) => ("/", &trimmed[1..]),
            Some(pos) => (&trimmed[..pos], &trimmed[pos + 1..]),
            None => ("/", trimmed),
        };
        let listing = engine.list_files(key, parent)?;
        let entry = listing
            .entries
            .into_iter()
            .find(|e| e.name == base)
            .ok_or_else(|| anyhow!("no such file or directory: {trimmed}"))?;
        let go_mode: u64 = u64::from(entry.mode & 0o777)
            | match entry.kind {
                FileKind::Directory => 0x8000_0000,
                FileKind::Symlink => 0x0800_0000,
                FileKind::File | FileKind::Other => 0,
            };
        json!({
            "name": entry.name,
            "size": entry.size,
            "mode": go_mode,
            "mtime": crate::build::rfc3339(entry.modified.max(0) as u64),
            "linkTarget": entry.link_target.unwrap_or_default(),
        })
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&stat_json)?);
    Ok(encoded)
}

pub async fn head_archive(
    state: &Arc<DockerState>,
    raw_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let key = raw_id.trim_start_matches('/').to_string();
    let path = head.query_param("path").unwrap_or("/").to_string();
    let Some(engine) = state.engine().cloned() else {
        write_empty(write, 501, &[("Content-Length", "0")], head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };
    let stat = tokio::task::spawn_blocking(move || stat_container_path(&engine, &key, &path)).await?;
    match stat {
        Ok(b64) => {
            write_empty(write, 200, &[("X-Docker-Container-Path-Stat", &b64), ("Content-Length", "0")], head.close_after).await?;
        }
        Err(_) => {
            write_empty(write, 404, &[("Content-Length", "0")], head.close_after).await?;
        }
    }
    Ok(RouteOutcome::Continue)
}

pub async fn get_archive(
    state: &Arc<DockerState>,
    raw_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let key = raw_id.trim_start_matches('/').to_string();
    let path = head.query_param("path").unwrap_or("/").to_string();
    let Some(engine) = state.engine().cloned() else {
        write_error(write, 501, "archive requires a live engine", head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };
    let (e1, k1, p1) = (Arc::clone(&engine), key.clone(), path.clone());
    let opened = tokio::task::spawn_blocking(move || {
        let stat = stat_container_path(&e1, &k1, &p1)?;
        let (_is_dir, _name, fd) = e1.get_archive_open(&k1, &p1)?;
        Ok::<_, anyhow::Error>((stat, fd))
    })
    .await?;

    let (stat, fd) = match opened {
        Ok(res) => res,
        Err(err) => {
            write_error(write, 404, &format!("{err:#}"), head.close_after).await?;
            return Ok(RouteOutcome::Continue);
        }
    };

    write_chunked_head(
        write,
        200,
        "application/x-tar",
        &[("X-Docker-Container-Path-Stat", &stat)],
    )
    .await?;

    let guest = std::os::unix::net::UnixStream::from(fd);
    guest.set_nonblocking(true)?;
    let mut guest = UnixStream::from_std(guest)?;
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        match guest.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if write_chunk(write, &buf[..n]).await.is_err() {
                    return Ok(RouteOutcome::Done);
                }
            }
            Err(_) => break,
        }
    }
    let _ = finish_chunked(write).await;
    Ok(RouteOutcome::Continue)
}

pub async fn put_archive(
    state: &Arc<DockerState>,
    raw_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    let key = raw_id.trim_start_matches('/').to_string();
    let path = head.query_param("path").unwrap_or("/").to_string();
    let Some(engine) = state.engine().cloned() else {
        drain_body(reader, write, head).await?;
        write_error(write, 501, "archive requires a live engine", head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };

    let tmp_dir = graviton_core::paths::support_dir().join("builds");
    tokio::fs::create_dir_all(&tmp_dir).await?;
    let tmp_path = tmp_dir.join(format!("cp-{}.tar", new_id()));
    let _cleanup = TempPathGuard(tmp_path.clone());

    let size = read_body_to_file(reader, write, head, &tmp_path, upload::MAX_UPLOAD).await?;

    let p = path.clone();
    let k = key.clone();
    let e = Arc::clone(&engine);
    let fd = match tokio::task::spawn_blocking(move || e.put_archive_open(&k, &p, size)).await? {
        Ok(fd) => fd,
        Err(err) => {
            let msg = format!("{err:#}");
            let status = if msg.contains("no such container") { 404 } else { 409 };
            write_error(write, status, &msg, head.close_after).await?;
            return Ok(RouteOutcome::Continue);
        }
    };

    let guest = std::os::unix::net::UnixStream::from(fd);
    guest.set_nonblocking(true)?;
    let guest = UnixStream::from_std(guest)?;
    let (guest_in, mut guest_out) = guest.into_split();

    let mut file = tokio::fs::File::open(&tmp_path).await?;
    tokio::io::copy(&mut file, &mut guest_out).await?;
    guest_out.flush().await?;

    let mut lines = BufReader::new(guest_in).lines();
    match lines.next_line().await? {
        Some(line) => match decode_line::<AgentEvent>(line.as_bytes())? {
            AgentEvent::Copied { .. } => write_empty(write, 200, &[], head.close_after).await?,
            AgentEvent::Error { message } => write_error(write, 500, &message, head.close_after).await?,
            other => write_error(write, 500, &format!("unexpected guest answer: {other:?}"), head.close_after).await?,
        },
        None => write_error(write, 500, "guest agent closed connection during cp", head.close_after).await?,
    }
    Ok(RouteOutcome::Continue)
}

// ---------------------------------------------------------------------------
// POST /containers/{id}/exec, POST /exec/{id}/start|resize, GET /exec/{id}/json
// ---------------------------------------------------------------------------

pub async fn create_exec(
    state: &Arc<DockerState>,
    raw_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    let key = raw_id.trim_start_matches('/');
    let body = read_body_bytes(reader, write, head, MAX_JSON_BODY).await?;
    let payload: Value = serde_json::from_slice(&body).context("parsing exec JSON")?;

    let Some(engine) = state.engine().cloned() else {
        write_error(write, 501, "exec requires a live engine", head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };

    let resolved = {
        let store = engine.store.lock().unwrap();
        store.container(key).map(|c| (c.id.clone(), c.status)).map_err(|e| e.to_string())
    };
    let (container_id, status) = match resolved {
        Ok(pair) => pair,
        Err(msg) => {
            write_error(write, 404, &msg, head.close_after).await?;
            return Ok(RouteOutcome::Continue);
        }
    };
    if status != ContainerStatus::Running {
        write_error(write, 409, &format!("Container {container_id} is not running"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }

    let cmd = parse_string_or_array(&payload["Cmd"]).unwrap_or_default();
    if cmd.is_empty() {
        write_error(write, 400, "Cmd is required", head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }

    let env: Vec<String> = payload["Env"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let working_dir = payload["WorkingDir"].as_str().map(String::from).filter(|s| !s.is_empty());
    let user = payload["User"].as_str().map(String::from).filter(|s| !s.is_empty());
    let tty = payload["Tty"].as_bool().unwrap_or(false);
    let attach_stdin = payload["AttachStdin"].as_bool().unwrap_or(false);
    let attach_stdout = payload["AttachStdout"].as_bool().unwrap_or(false);
    let attach_stderr = payload["AttachStderr"].as_bool().unwrap_or(false);
    let (rows, cols) = parse_console_size(&payload["ConsoleSize"]);

    let exec_id = format!("{}{}", new_id(), new_id());
    state.execs.lock().unwrap().insert(
        exec_id.clone(),
        ExecRecord {
            id: exec_id.clone(),
            container_id,
            cmd,
            env,
            working_dir,
            user,
            tty,
            attach_stdin,
            attach_stdout,
            attach_stderr,
            rows,
            cols,
            running: false,
            exit_code: None,
            pid: None,
            resize_tx: None,
        },
    );

    write_json(write, 201, &json!({ "Id": exec_id }), head.close_after).await?;
    Ok(RouteOutcome::Continue)
}

fn parse_console_size(v: &Value) -> (u16, u16) {
    match v.as_array() {
        Some(a) if a.len() == 2 => (
            a[0].as_u64().unwrap_or(0) as u16,
            a[1].as_u64().unwrap_or(0) as u16,
        ),
        _ => (0, 0),
    }
}

pub async fn exec_start(
    state: &Arc<DockerState>,
    exec_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    let body = read_body_bytes(reader, write, head, MAX_JSON_BODY).await?;
    let payload: Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body).unwrap_or_else(|_| json!({}))
    };

    let Some(mut rec) = state.execs.lock().unwrap().get(exec_id).cloned() else {
        write_error(write, 404, &format!("No such exec instance: {exec_id}"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };
    let Some(engine) = state.engine().cloned() else {
        write_error(write, 501, "exec requires a live engine", head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };

    let detach = payload["Detach"].as_bool().unwrap_or(false);
    if let Some(tty) = payload["Tty"].as_bool() {
        rec.tty = tty;
    }
    let (r, c) = parse_console_size(&payload["ConsoleSize"]);
    if r > 0 && c > 0 {
        rec.rows = r;
        rec.cols = c;
    }

    let spec = ExecSpec {
        id: rec.container_id.clone(),
        command: rec.cmd.clone(),
        tty: rec.tty,
        rows: rec.rows,
        cols: rec.cols,
        env: rec.env.clone(),
        working_dir: rec.working_dir.clone(),
        user: rec.user.clone(),
    };

    if detach {
        let e = Arc::clone(&engine);
        let (pid, guest_fd) = match tokio::task::spawn_blocking(move || e.exec_interactive(&spec, false)).await? {
            Ok(res) => res,
            Err(err) => {
                let msg = format!("{err:#}");
                let status = exec_error_status(&msg);
                if let Some(entry) = state.execs.lock().unwrap().get_mut(exec_id) {
                    entry.running = false;
                    entry.exit_code = Some(126);
                }
                write_error(write, status, &msg, head.close_after).await?;
                return Ok(RouteOutcome::Continue);
            }
        };

        if let Some(entry) = state.execs.lock().unwrap().get_mut(exec_id) {
            entry.running = true;
            entry.exit_code = None;
            entry.pid = Some(pid);
        }

        let guest = std::os::unix::net::UnixStream::from(guest_fd);
        guest.set_nonblocking(true)?;
        let mut guest = UnixStream::from_std(guest)?;
        let st = Arc::clone(state);
        let eid = exec_id.to_string();
        let is_tty = rec.tty;
        tokio::spawn(async move {
            let mut final_code = 126i32;
            if is_tty {
                let mut decoder = Decoder::default();
                let mut buf = [0u8; 8192];
                'drain: loop {
                    match guest.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => decoder.push(&buf[..n]),
                    }
                    loop {
                        match decoder.next_frame() {
                            Ok(Some(Frame::Exit(code))) => {
                                final_code = code;
                                break 'drain;
                            }
                            Ok(Some(_)) => {}
                            Ok(None) => break,
                            Err(_) => break 'drain,
                        }
                    }
                }
            } else {
                let mut lines = BufReader::new(&mut guest).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(ev) = decode_line::<AgentEvent>(line.as_bytes()) else {
                        continue;
                    };
                    match ev {
                        AgentEvent::Exited { code, .. } => {
                            final_code = code;
                            break;
                        }
                        AgentEvent::Error { .. } => {
                            final_code = 126;
                            break;
                        }
                        _ => {}
                    }
                }
            }
            drop(guest);
            if let Some(entry) = st.execs.lock().unwrap().get_mut(&eid) {
                entry.running = false;
                entry.exit_code = Some(final_code);
            }
        });
        write_empty(write, 200, &[], head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }

    // Hijacked interactive or multiplexed stream.
    let ctype = if rec.tty {
        "application/vnd.docker.raw-stream"
    } else {
        "application/vnd.docker.multiplexed-stream"
    };
    write_stream_start(write, head, ctype).await?;

    if rec.tty {
        let e = Arc::clone(&engine);
        let (pid, tty_fd) = match tokio::task::spawn_blocking(move || e.exec_interactive(&spec, false)).await? {
            Ok(res) => res,
            Err(err) => {
                let _ = write.write_all(format!("{err:#}\r\n").as_bytes()).await;
                if let Some(entry) = state.execs.lock().unwrap().get_mut(exec_id) {
                    entry.running = false;
                    entry.exit_code = Some(126);
                }
                return Ok(RouteOutcome::Done);
            }
        };

        let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();
        if let Some(entry) = state.execs.lock().unwrap().get_mut(exec_id) {
            entry.running = true;
            entry.exit_code = None;
            entry.pid = Some(pid);
            entry.resize_tx = Some(resize_tx);
        }

        let guest = std::os::unix::net::UnixStream::from(tty_fd);
        guest.set_nonblocking(true)?;
        let guest = UnixStream::from_std(guest)?;
        let (mut guest_in, mut guest_out) = guest.into_split();

        let pending = reader.buffer().to_vec();
        let mut client_read = std::mem::replace(reader.get_mut(), unsafe_dummy_read().await);
        let attach_stdin = rec.attach_stdin;
        let stdin_task = tokio::spawn(async move {
            if attach_stdin && !pending.is_empty() && guest_out.write_all(&Frame::Data(pending).encode()).await.is_err() {
                return;
            }
            let mut buf = [0u8; 4096];
            let mut eof_sent = !attach_stdin;
            loop {
                tokio::select! {
                    biased;
                    Some((rows, cols)) = resize_rx.recv() => {
                        if guest_out.write_all(&Frame::Resize { rows, cols }.encode()).await.is_err() {
                            break;
                        }
                    }
                    n = client_read.read(&mut buf), if !eof_sent => {
                        match n {
                            Ok(0) => {
                                eof_sent = true;
                                let _ = guest_out.write_all(&Frame::Eof.encode()).await;
                            }
                            Ok(n) => {
                                if guest_out.write_all(&Frame::Data(buf[..n].to_vec()).encode()).await.is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        });

        let mut decoder = Decoder::default();
        let mut buf = [0u8; 16 * 1024];
        let mut exit_code = 0i32;
        'tty: loop {
            match guest_in.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => decoder.push(&buf[..n]),
            }
            loop {
                match decoder.next_frame() {
                    Ok(Some(Frame::Data(bytes))) => {
                        if write.write_all(&bytes).await.is_err() {
                            break 'tty;
                        }
                        let _ = write.flush().await;
                    }
                    Ok(Some(Frame::Exit(code))) => {
                        exit_code = code;
                        break 'tty;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => break 'tty,
                }
            }
        }
        stdin_task.abort();
        if let Some(entry) = state.execs.lock().unwrap().get_mut(exec_id) {
            entry.running = false;
            entry.exit_code = Some(exit_code);
            entry.resize_tx = None;
        }
        let _ = write.shutdown().await;
        return Ok(RouteOutcome::Done);
    }

    // Non-TTY exec (supports AttachStdin via AgentRequest::Stdin + EOF).
    let attach_stdin = rec.attach_stdin;
    let e = Arc::clone(&engine);
    let (pid, guest_fd) = match tokio::task::spawn_blocking(move || e.exec_interactive(&spec, attach_stdin)).await? {
        Ok(res) => res,
        Err(err) => {
            let msg = format!("{err:#}\n");
            let _ = write.write_all(&encode_mux_frame(2, msg.as_bytes())).await;
            let _ = write.flush().await;
            if let Some(entry) = state.execs.lock().unwrap().get_mut(exec_id) {
                entry.running = false;
                entry.exit_code = Some(126);
            }
            return Ok(RouteOutcome::Done);
        }
    };

    if let Some(entry) = state.execs.lock().unwrap().get_mut(exec_id) {
        entry.running = true;
        entry.exit_code = None;
        entry.pid = Some(pid);
    }

    let guest = std::os::unix::net::UnixStream::from(guest_fd);
    guest.set_nonblocking(true)?;
    let guest = UnixStream::from_std(guest)?;
    let (guest_in, mut guest_out) = guest.into_split();

    let stdin_task = if attach_stdin {
        let pending = reader.buffer().to_vec();
        let mut client_read = std::mem::replace(reader.get_mut(), unsafe_dummy_read().await);
        Some(tokio::spawn(async move {
            if !pending.is_empty() {
                let frame = encode_line(&AgentRequest::Stdin { pid, data: pending });
                if guest_out.write_all(&frame).await.is_err() {
                    return;
                }
            }
            let mut buf = [0u8; 8192];
            loop {
                match client_read.read(&mut buf).await {
                    Ok(0) => {
                        // Signal clean pipe EOF to the guest child stdin.
                        let _ = guest_out.write_all(&encode_line(&AgentRequest::Stdin { pid, data: vec![] })).await;
                        let _ = guest_out.flush().await;
                        break;
                    }
                    Ok(n) => {
                        let frame = encode_line(&AgentRequest::Stdin { pid, data: buf[..n].to_vec() });
                        if guest_out.write_all(&frame).await.is_err() || guest_out.flush().await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }))
    } else {
        None
    };

    let mut lines = BufReader::new(guest_in).lines();
    let mut exit_code = 0i32;
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(ev) = decode_line::<AgentEvent>(line.as_bytes()) else {
            continue;
        };
        match ev {
            AgentEvent::Stdout { data, .. } if rec.attach_stdout => {
                if write.write_all(&encode_mux_frame(1, &data)).await.is_err() {
                    break;
                }
                let _ = write.flush().await;
            }
            AgentEvent::Stderr { data, .. } if rec.attach_stderr => {
                if write.write_all(&encode_mux_frame(2, &data)).await.is_err() {
                    break;
                }
                let _ = write.flush().await;
            }
            AgentEvent::Exited { code, .. } => {
                exit_code = code;
                break;
            }
            AgentEvent::Error { message } => {
                if rec.attach_stderr {
                    let msg = format!("{message}\n");
                    let _ = write.write_all(&encode_mux_frame(2, msg.as_bytes())).await;
                }
                exit_code = 126;
                break;
            }
            _ => {}
        }
    }
    if let Some(t) = stdin_task {
        t.abort();
    }
    if let Some(entry) = state.execs.lock().unwrap().get_mut(exec_id) {
        entry.running = false;
        entry.exit_code = Some(exit_code);
    }
    let _ = write.shutdown().await;
    Ok(RouteOutcome::Done)
}

pub async fn exec_resize(
    state: &Arc<DockerState>,
    exec_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let rows = head.query_param("h").and_then(|s| s.parse::<u16>().ok()).unwrap_or(24);
    let cols = head.query_param("w").and_then(|s| s.parse::<u16>().ok()).unwrap_or(80);
    let found = {
        let mut map = state.execs.lock().unwrap();
        if let Some(entry) = map.get_mut(exec_id) {
            entry.rows = rows;
            entry.cols = cols;
            if let Some(tx) = &entry.resize_tx {
                let _ = tx.send((rows, cols));
            }
            true
        } else {
            false
        }
    };
    if !found {
        write_error(write, 404, &format!("No such exec instance: {exec_id}"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    }
    write_empty(write, 200, &[], head.close_after).await?;
    Ok(RouteOutcome::Continue)
}

pub async fn exec_inspect(
    state: &Arc<DockerState>,
    exec_id: &str,
    head: &RequestHead,
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
) -> Result<RouteOutcome> {
    drain_body(reader, write, head).await?;
    let rec = state.execs.lock().unwrap().get(exec_id).cloned();
    let Some(rec) = rec else {
        write_error(write, 404, &format!("No such exec instance: {exec_id}"), head.close_after).await?;
        return Ok(RouteOutcome::Continue);
    };

    let json = json!({
        "CanRemove": false,
        "DetachKeys": "",
        "ID": rec.id,
        "Running": rec.running,
        "ExitCode": rec.exit_code.unwrap_or(0),
        "ProcessConfig": {
            "privileged": false,
            "user": rec.user.as_deref().unwrap_or(""),
            "tty": rec.tty,
            "entrypoint": rec.cmd.first().map(String::as_str).unwrap_or(""),
            "arguments": rec.cmd.get(1..).unwrap_or(&[]),
        },
        "OpenStdin": rec.attach_stdin,
        "OpenStderr": rec.attach_stderr,
        "OpenStdout": rec.attach_stdout,
        "ContainerID": rec.container_id,
        "Pid": rec.pid.unwrap_or(0),
    });
    write_json(write, 200, &json, head.close_after).await?;
    Ok(RouteOutcome::Continue)
}

pub(super) fn exec_error_status(msg: &str) -> u16 {
    if msg.contains("isn't running") || msg.contains("not running") || msg.contains("is paused") || msg.contains("already paused") {
        409
    } else if msg.contains("no such container") {
        404
    } else if msg.contains("exec:") || msg.contains("give a command") || msg.contains("No such file") || msg.contains("permission denied") {
        400
    } else {
        500
    }
}

/// Percent-decode an arbitrary name segment when needed.
#[allow(dead_code)]
fn decode_segment(s: &str) -> String {
    percent_decode(s, false)
}
