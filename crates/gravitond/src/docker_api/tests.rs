use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use graviton_core::Store;
use graviton_image::{BlobStore, ImageIndex, LocalImage};
use graviton_proto::{ContainerStatus, ContainerSummary, Mount, PortMapping, PortProtocol};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use crate::Backend;
use crate::runtime::ContainerSpec;

use super::containers::{
    docker_status_human, exec_error_status, matches_container_filters, normalize_docker_bind,
    normalize_docker_mount, parse_filters,
};
use super::http::{encode_mux_frame, read_request_head, strip_api_version, write_empty};
use super::{DockerState, serve_connection};

#[test]
fn maps_exec_start_errors_to_http_statuses() {
    assert_eq!(exec_error_status("container web isn't running"), 409);
    assert_eq!(exec_error_status("container web is not running"), 409);
    assert_eq!(exec_error_status("container web is paused"), 409);
    assert_eq!(exec_error_status("container web is already paused"), 409);
    assert_eq!(exec_error_status("no such container abc"), 404);
    assert_eq!(exec_error_status("exec: No such file or directory (os error 2)"), 400);
    assert_eq!(exec_error_status("give a command to run"), 400);
    assert_eq!(exec_error_status("permission denied"), 400);
    assert_eq!(exec_error_status("unexpected vsock failure"), 500);
}

#[test]
fn strips_version_prefixes_safely() {
    assert_eq!(strip_api_version("/v1.47/containers/json"), "/containers/json");
    assert_eq!(strip_api_version("/v1.24/_ping"), "/_ping");
    assert_eq!(strip_api_version("/v1.47"), "/");
    assert_eq!(strip_api_version("/volumes/create"), "/volumes/create");
    assert_eq!(strip_api_version("/version"), "/version");
    assert_eq!(strip_api_version("/_ping"), "/_ping");
}

#[test]
fn encodes_multiplexed_frames() {
    let frame = encode_mux_frame(1, b"hello");
    assert_eq!(frame, [1, 0, 0, 0, 0, 0, 0, 5, b'h', b'e', b'l', b'l', b'o']);
    let err = encode_mux_frame(2, b"boom");
    assert_eq!(err, [2, 0, 0, 0, 0, 0, 0, 4, b'b', b'o', b'o', b'm']);
}

#[test]
fn filters_containers_with_label_and_semantics() {
    let c = ContainerSummary {
        id: "a1b2c3d4e5f6".into(),
        name: "dev-box".into(),
        image: "ubuntu:24.04".into(),
        status: ContainerStatus::Running,
        exit_code: None,
        ports: vec![PortMapping { host_ip: None, host_port: 8080, container_port: 80, protocol: PortProtocol::Tcp }],
        mounts: vec![Mount { source: "/Users/me/proj".into(), target: "/workspace".into(), read_only: false }],
        ip_address: Some("192.168.64.5".into()),
        cpu_percent: None,
        memory_bytes: None,
        memory_limit_bytes: Some(1024 << 20),
        created_at: 100,
        started_at: Some(101),
        project: None,
        service: None,
        health: None,
    };
    let spec = ContainerSpec {
        image: LocalImage {
            reference: "docker.io/library/ubuntu:24.04".into(),
            manifest_digest: "sha256:aa".into(),
            config_digest: "sha256:bb".into(),
            layers: vec![],
            architecture: "arm64".into(),
            pulled_at: 1,
            variants: Vec::new(),
        },
        argv: vec!["sleep".into(), "infinity".into()],
        env: vec![],
        cwd: "/workspace".into(),
        user: None,
        cpus: 2,
        memory_mib: 1024,
        ports: vec![],
        volumes: vec![],
        restart: Default::default(),
        want_running: false,
        auto_remove: false,
        hostname: None,
        project: None,
        service: None,
        labels: BTreeMap::from([
            ("devcontainer.local_folder".into(), "/Users/me/proj".into()),
            ("devcontainer.config_file".into(), "/Users/me/proj/.devcontainer/devcontainer.json".into()),
        ]),
        dns_aliases: vec![],
        healthcheck: None,
        read_only: false,
        cap_add: vec![],
        cap_drop: vec![],
        tmpfs: vec![],
        init: false,
        networks: vec![],
    };

    // Go map filter style (AND across label keys)
    let f = parse_filters(
        r#"{"label":{"devcontainer.local_folder=/Users/me/proj":true,"devcontainer.config_file=/Users/me/proj/.devcontainer/devcontainer.json":true},"status":{"running":true}}"#,
    )
    .unwrap();
    assert!(matches_container_filters(&c, Some(&spec), &f));

    // One mismatched label fails the whole match
    let bad = parse_filters(
        r#"{"label":["devcontainer.local_folder=/Users/me/proj","devcontainer.config_file=/other"]}"#,
    )
    .unwrap();
    assert!(!matches_container_filters(&c, Some(&spec), &bad));

    // Name, id prefix, ancestor, volume filters
    let f2 = parse_filters(r#"{"name":["dev-"],"id":["a1b2"],"ancestor":["ubuntu"],"volume":["/workspace"]}"#).unwrap();
    assert!(matches_container_filters(&c, Some(&spec), &f2));

    // Health filters (`none`, `starting`, `healthy`, `unhealthy`) and status suffix
    let f_none = parse_filters(r#"{"health":["none"]}"#).unwrap();
    assert!(matches_container_filters(&c, Some(&spec), &f_none));
    assert!(!docker_status_human(&c, 120).contains("health"));

    let mut healthy = c.clone();
    healthy.health = Some(graviton_proto::HealthStatus::Healthy);
    let f_healthy = parse_filters(r#"{"health":["healthy"]}"#).unwrap();
    assert!(matches_container_filters(&healthy, Some(&spec), &f_healthy));
    assert!(!matches_container_filters(&healthy, Some(&spec), &f_none));
    assert!(docker_status_human(&healthy, 120).ends_with(" (healthy)"));

    healthy.health = Some(graviton_proto::HealthStatus::Starting);
    assert!(docker_status_human(&healthy, 120).ends_with(" (health: starting)"));

    healthy.health = Some(graviton_proto::HealthStatus::Unhealthy);
    assert!(docker_status_human(&healthy, 120).ends_with(" (unhealthy)"));

    // Ensure unused imports BlobStore/ImageIndex aren't flagged even if future tests use them
    let _ = (std::mem::size_of::<BlobStore>(), std::mem::size_of::<ImageIndex>());
}

#[test]
fn normalizes_docker_binds_and_mounts() {
    assert_eq!(normalize_docker_bind("/Users/me/proj:/workspace:rw,cached").as_deref(), Some("/Users/me/proj:/workspace"));
    assert_eq!(normalize_docker_bind("/Users/me/proj:/workspace:delegated,ro").as_deref(), Some("/Users/me/proj:/workspace:ro"));
    assert_eq!(normalize_docker_bind("pgdata:/var/lib/pg:Z").as_deref(), Some("pgdata:/var/lib/pg"));

    assert_eq!(
        normalize_docker_mount(&json!({"Type":"bind","Source":"/tmp/a","Target":"/mnt/a","ReadOnly":true})).as_deref(),
        Some("/tmp/a:/mnt/a:ro")
    );
    let anon = normalize_docker_mount(&json!({"Type":"volume","Target":"/cache"})).unwrap();
    assert!(anon.starts_with("vol-") && anon.ends_with(":/cache"));
    assert_eq!(normalize_docker_mount(&json!({"Type":"tmpfs","Target":"/dev/shm"})), None);
}

#[tokio::test]
async fn unix_socket_keepalive_and_endpoints() {
    let dir = std::env::temp_dir().join(format!("graviton-docker-api-test-{}", crate::runtime::new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("docker.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let state = DockerState::new(Backend::Demo(Arc::new(Mutex::new(Store::demo()))));

    let srv_state = Arc::clone(&state);
    let server = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let _ = serve_connection(stream, srv_state).await;
        }
    });

    let mut client = UnixStream::connect(&sock).await.unwrap();

    // Send 1st request: GET /_ping
    client.write_all(b"GET /_ping HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    let (status, headers, body) = read_http_response(&mut client).await;
    assert_eq!(status, 200);
    assert_eq!(body, b"OK");
    assert_eq!(headers.get("builder-version").map(String::as_str), Some("1"));
    assert_eq!(headers.get("api-version").map(String::as_str), Some("1.47"));

    // Send 2nd request on same keep-alive socket: GET /v1.47/containers/json?all=1
    client.write_all(b"GET /v1.47/containers/json?all=1 HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    let (status, _, body) = read_http_response(&mut client).await;
    assert_eq!(status, 200);
    let arr: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(arr.len(), 6);

    // Send 3rd request: POST /v1.47/networks/create
    let req_body = r#"{"Name":"proton-dev"}"#;
    let raw = format!(
        "POST /v1.47/networks/create HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        req_body.len(),
        req_body
    );
    client.write_all(raw.as_bytes()).await.unwrap();
    let (status, _, body) = read_http_response(&mut client).await;
    assert_eq!(status, 201);
    let created: Value = serde_json::from_slice(&body).unwrap();
    assert!(created["Id"].as_str().is_some_and(|s| !s.is_empty()));

    // Inspect created network, start stopped demo container, hit 404 on unknown route
    client.write_all(b"GET /v1.47/networks/proton-dev HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    let (status, _, body) = read_http_response(&mut client).await;
    assert_eq!(status, 200);
    let net: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(net["Name"], "proton-dev");

    // List, connect, disconnect, and remove user network
    client.write_all(b"GET /v1.47/networks HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    let (status, _, body) = read_http_response(&mut client).await;
    assert_eq!(status, 200);
    let nets: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert!(nets.iter().any(|n| n["Name"] == "proton-dev"));

    let conn_body = r#"{"Container":"worker"}"#;
    let conn_req = format!(
        "POST /v1.47/networks/proton-dev/connect HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        conn_body.len(),
        conn_body
    );
    client.write_all(conn_req.as_bytes()).await.unwrap();
    let (status, _, _) = read_http_response(&mut client).await;
    assert_eq!(status, 200);

    let disc_req = format!(
        "POST /v1.47/networks/proton-dev/disconnect HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        conn_body.len(),
        conn_body
    );
    client.write_all(disc_req.as_bytes()).await.unwrap();
    let (status, _, _) = read_http_response(&mut client).await;
    assert_eq!(status, 200);

    client.write_all(b"DELETE /v1.47/networks/proton-dev HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    let (status, _, _) = read_http_response(&mut client).await;
    assert_eq!(status, 204);

    client.write_all(b"POST /v1.47/containers/worker/start HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    let (status, _, _) = read_http_response(&mut client).await;
    assert_eq!(status, 204);

    client.write_all(b"POST /v1.47/containers/worker/start HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    let (status, _, _) = read_http_response(&mut client).await;
    assert_eq!(status, 304);

    // Pause / unpause workflow over Docker REST API.
    client.write_all(b"POST /v1.47/containers/worker/pause HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    let (status, _, _) = read_http_response(&mut client).await;
    assert_eq!(status, 204);

    client.write_all(b"POST /v1.47/containers/worker/pause HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    let (status, _, _) = read_http_response(&mut client).await;
    assert_eq!(status, 409);

    client.write_all(b"GET /v1.47/containers/json HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    let (status, _, body) = read_http_response(&mut client).await;
    assert_eq!(status, 200);
    let running_and_paused: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert!(running_and_paused.iter().any(|c| c["Names"][0] == "/worker" && c["State"] == "paused"));

    client.write_all(b"POST /v1.47/containers/worker/unpause HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    let (status, _, _) = read_http_response(&mut client).await;
    assert_eq!(status, 204);

    client.write_all(b"POST /v1.47/containers/ghost/pause HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    let (status, _, _) = read_http_response(&mut client).await;
    assert_eq!(status, 404);

    client.write_all(b"GET /v1.47/images/get?names=alpine:3.22 HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    let (status, _, _) = read_http_response(&mut client).await;
    assert_eq!(status, 501);

    client.write_all(b"GET /v1.47/images/alpine:3.22/get HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    let (status, _, _) = read_http_response(&mut client).await;
    assert_eq!(status, 501);

    client
        .write_all(b"POST /v1.47/images/load HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n")
        .await
        .unwrap();
    let (status, _, _) = read_http_response(&mut client).await;
    assert_eq!(status, 501);

    client
        .write_all(b"POST /v1.47/commit?container=worker&repo=snap&tag=1 HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n")
        .await
        .unwrap();
    let (status, _, _) = read_http_response(&mut client).await;
    assert_eq!(status, 501);

    client
        .write_all(b"HEAD /v1.47/containers/worker/archive?path=/tmp HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let (status, headers, body) = read_http_response(&mut client).await;
    assert_eq!(status, 501);
    assert_eq!(headers.get("content-length").map(String::as_str), Some("0"));
    assert!(body.is_empty());

    // Filtered GET /containers/{id}/logs with timestamps, since, until, and tail.
    let api_logs = state.backend.store().lock().unwrap().logs("api-gateway", None).unwrap();
    let first_non_sys = api_logs
        .iter()
        .find(|l| !matches!(l.stream, graviton_proto::LogStream::System))
        .unwrap()
        .timestamp_ms;
    let last_non_sys = api_logs
        .iter()
        .rev()
        .find(|l| !matches!(l.stream, graviton_proto::LogStream::System))
        .unwrap()
        .timestamp_ms;
    let req = format!(
        "GET /v1.47/containers/api-gateway/logs?stdout=1&stderr=1&timestamps=1&since={}&until={}&tail=1 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        first_non_sys / 1000,
        last_non_sys.div_ceil(1000),
    );
    client.write_all(req.as_bytes()).await.unwrap();
    let (status, _, body) = read_http_response(&mut client).await;
    assert_eq!(status, 200);
    assert!(body.len() >= 8);
    let mux_len = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
    assert_eq!(body.len(), 8 + mux_len);
    let line_text = std::str::from_utf8(&body[8..]).unwrap();
    assert!(line_text.contains("Z "), "expected RFC3339 timestamp prefix in {line_text:?}");

    client
        .write_all(b"GET /v1.47/does-not-exist HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let (status, _, _) = read_http_response(&mut client).await;
    assert_eq!(status, 404);

    let _ = server.await;
    let _ = std::fs::remove_dir_all(&dir);
    let _ = read_request_head::<tokio::io::Empty>;
}

#[tokio::test]
async fn write_empty_sets_content_length_for_200_and_201_not_204_or_304() {
    for status in [200u16, 201] {
        let mut buf = Vec::new();
        write_empty(&mut buf, status, &[], false).await.unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Content-Length: 0\r\n"), "status {status}: {text:?}");
    }

    let mut dup_buf = Vec::new();
    write_empty(&mut dup_buf, 404, &[("Content-Length", "0")], false)
        .await
        .unwrap();
    let dup_text = String::from_utf8(dup_buf).unwrap();
    assert_eq!(dup_text.to_ascii_lowercase().matches("content-length:").count(), 1);

    for status in [101u16, 204, 304] {
        let mut buf = Vec::new();
        write_empty(&mut buf, status, &[], false).await.unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            !text.to_ascii_lowercase().contains("content-length"),
            "status {status}: {text:?}"
        );
    }
}

#[test]
fn docker_stats_json_includes_pids_and_sequential_precpu() {
    use graviton_proto::{ContainerStats, ProcessInfo, StatSample};

    let st = ContainerStats {
        id: "abc123def456".into(),
        cpus: 2,
        interval_secs: 2,
        samples: vec![StatSample {
            timestamp_ms: 1_700_000_000_000,
            cpu_percent: 25.0,
            memory_bytes: 32 << 20,
            memory_limit_bytes: 1024 << 20,
            net_rx_bytes: 100,
            net_tx_bytes: 200,
        }],
        processes: vec![
            ProcessInfo { pid: 1, ppid: 0, uid: 0, state: "S".into(), command: "init".into(), rss_bytes: 1024, cpu_ms: 10 },
            ProcessInfo { pid: 2, ppid: 1, uid: 0, state: "R".into(), command: "sleep".into(), rss_bytes: 2048, cpu_ms: 20 },
        ],
    };

    let mut prev = None;
    let first = super::containers::build_docker_stats_json(&st, "web", true, &mut prev);
    assert_eq!(first["pids_stats"]["current"], 2);
    assert_eq!(first["cpu_stats"]["cpu_usage"]["total_usage"], 250_000_000);
    assert_eq!(first["cpu_stats"]["system_cpu_usage"], 4_000_000_000u64);
    assert_eq!(first["precpu_stats"]["system_cpu_usage"], 2_000_000_000u64);

    let second = super::containers::build_docker_stats_json(&st, "web", true, &mut prev);
    assert_eq!(second["precpu_stats"]["cpu_usage"]["total_usage"], 250_000_000);
    assert_eq!(second["cpu_stats"]["cpu_usage"]["total_usage"], 500_000_000);
    assert_eq!(second["precpu_stats"]["system_cpu_usage"], 4_000_000_000u64);
    assert_eq!(second["cpu_stats"]["system_cpu_usage"], 6_000_000_000u64);

    let stopped = super::containers::build_docker_stats_json(&st, "web", false, &mut prev);
    assert_eq!(stopped["pids_stats"]["current"], 0);
    assert_eq!(stopped["memory_stats"]["usage"], 0);

    let tmp = serde_json::json!({
        "Type": "tmpfs",
        "Target": "/cache",
        "ReadOnly": false,
        "TmpfsOptions": { "SizeBytes": 67108864u64, "Mode": 0o750u64 }
    });
    let parsed = super::containers::normalize_docker_tmpfs_mount(&tmp).unwrap().unwrap();
    assert_eq!(parsed.target, "/cache");
    assert_eq!(parsed.options, "size=67108864,mode=750");
}

async fn read_http_response(stream: &mut UnixStream) -> (u16, std::collections::HashMap<String, String>, Vec<u8>) {
    let mut raw = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut tmp).await.unwrap();
        assert!(n > 0, "unexpected EOF waiting for response headers");
        raw.extend_from_slice(&tmp[..n]);
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos;
            break;
        }
    }
    let header_text = std::str::from_utf8(&raw[..header_end]).unwrap();
    let mut lines = header_text.split("\r\n");
    let status_line = lines.next().unwrap();
    let status: u16 = status_line.split_whitespace().nth(1).unwrap().parse().unwrap();
    let mut headers = std::collections::HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let content_len: usize = headers
        .get("content-length")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body_start = header_end + 4;
    while raw.len() < body_start + content_len {
        let n = stream.read(&mut tmp).await.unwrap();
        assert!(n > 0, "unexpected EOF waiting for body");
        raw.extend_from_slice(&tmp[..n]);
    }
    let body = raw[body_start..body_start + content_len].to_vec();
    (status, headers, body)
}
