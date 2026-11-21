//! In-memory engine state.
//!
//! The daemon owns one [`Store`]. Until the VMM is wired up, `Store::demo()`
//! seeds it with sample containers so the CLI and the SwiftUI app have
//! something real to talk to.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use graviton_proto::{
    ContainerStatus, ContainerSummary, EngineInfo, ImageSummary, LogLevel, LogLine, LogStream, Mount,
    PortMapping, PortProtocol, PROTOCOL_VERSION,
};
use thiserror::Error;

use crate::ImageReference;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StoreError {
    #[error("no such container: {0}")]
    NotFound(String),
    #[error("`{0}` matches more than one container; use more of the id")]
    Ambiguous(String),
    #[error("container {name} is already {state}")]
    InvalidState { name: String, state: &'static str },
    #[error("container {0} is running; stop it first")]
    Running(String),
    #[error("the name {0} is already in use")]
    NameTaken(String),
}

/// Lines kept per container.
pub const LOG_LIMIT: usize = 10_000;

#[derive(Debug, Default)]
pub struct Store {
    containers: Vec<ContainerSummary>,
    images: Vec<ImageSummary>,
    logs: HashMap<String, Vec<LogLine>>,
    engine: Option<EngineInfo>,
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn engine_info(&self) -> EngineInfo {
        self.engine.clone().unwrap_or_else(|| EngineInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: PROTOCOL_VERSION,
            kernel: "not booted".into(),
            arch: std::env::consts::ARCH.into(),
            cpus: 0,
            memory_bytes: 0,
            memory_used_bytes: 0,
            disk_bytes: 0,
            disk_used_bytes: 0,
        })
    }

    pub fn containers(&self) -> &[ContainerSummary] {
        &self.containers
    }

    pub fn images(&self) -> &[ImageSummary] {
        &self.images
    }

    /// Resolve a name, full id or unique id prefix.
    pub fn resolve(&self, key: &str) -> Result<usize, StoreError> {
        if let Some(i) = self.containers.iter().position(|c| c.name == key || c.id == key) {
            return Ok(i);
        }
        let mut hits = self.containers.iter().enumerate().filter(|(_, c)| c.id.starts_with(key));
        match (hits.next(), hits.next()) {
            (Some((i, _)), None) if !key.is_empty() => Ok(i),
            (Some(_), Some(_)) => Err(StoreError::Ambiguous(key.to_string())),
            _ => Err(StoreError::NotFound(key.to_string())),
        }
    }

    pub fn container(&self, key: &str) -> Result<&ContainerSummary, StoreError> {
        Ok(&self.containers[self.resolve(key)?])
    }

    pub fn logs(&self, key: &str, tail: Option<usize>) -> Result<Vec<LogLine>, StoreError> {
        let c = self.container(key)?;
        let lines = self.logs.get(&c.id).map(Vec::as_slice).unwrap_or_default();
        let start = tail.map_or(0, |n| lines.len().saturating_sub(n));
        Ok(lines[start..].to_vec())
    }

    pub fn start(&mut self, key: &str) -> Result<ContainerSummary, StoreError> {
        let i = self.resolve(key)?;
        let c = &mut self.containers[i];
        if c.status == ContainerStatus::Running {
            return Err(StoreError::InvalidState { name: c.name.clone(), state: "running" });
        }
        c.status = ContainerStatus::Running;
        c.exit_code = None;
        c.started_at = Some(now_secs());
        c.cpu_percent = Some(0.0);
        c.memory_bytes = Some(0);
        Ok(c.clone())
    }

    pub fn stop(&mut self, key: &str) -> Result<ContainerSummary, StoreError> {
        let i = self.resolve(key)?;
        let c = &mut self.containers[i];
        if c.status != ContainerStatus::Running && c.status != ContainerStatus::Paused {
            return Err(StoreError::InvalidState { name: c.name.clone(), state: "stopped" });
        }
        c.status = ContainerStatus::Exited;
        c.exit_code = Some(0);
        c.started_at = None;
        c.cpu_percent = None;
        c.memory_bytes = None;
        Ok(c.clone())
    }

    pub fn restart(&mut self, key: &str) -> Result<ContainerSummary, StoreError> {
        let i = self.resolve(key)?;
        if self.containers[i].status == ContainerStatus::Running {
            self.stop(key)?;
        }
        self.start(key)
    }

    pub fn name_taken(&self, name: &str) -> bool {
        self.containers.iter().any(|c| c.name == name)
    }

    pub fn insert(&mut self, container: ContainerSummary) -> Result<(), StoreError> {
        if self.name_taken(&container.name) {
            return Err(StoreError::NameTaken(container.name));
        }
        self.containers.push(container);
        Ok(())
    }

    /// Apply `f` to the container with this exact id.
    pub fn update(&mut self, id: &str, f: impl FnOnce(&mut ContainerSummary)) -> Option<ContainerSummary> {
        let c = self.containers.iter_mut().find(|c| c.id == id)?;
        f(c);
        Some(c.clone())
    }

    /// Append a line to a container's log and return it (to persist it).
    pub fn push_log(&mut self, id: &str, level: LogLevel, stream: LogStream, message: impl Into<String>) -> LogLine {
        let lines = self.logs.entry(id.to_string()).or_default();
        let seq = lines.last().map_or(1, |l| l.seq + 1);
        let timestamp_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let line = LogLine { seq, timestamp_ms, level, message: message.into(), stream };
        lines.push(line.clone());
        if lines.len() > LOG_LIMIT {
            lines.drain(..lines.len() - LOG_LIMIT);
        }
        line
    }

    /// Replace a container's log (when restoring saved state), keeping the newest lines.
    pub fn set_logs(&mut self, id: &str, mut lines: Vec<LogLine>) {
        if lines.len() > LOG_LIMIT {
            lines.drain(..lines.len() - LOG_LIMIT);
        }
        self.logs.insert(id.to_string(), lines);
    }

    /// Remove a container that isn't running.
    pub fn remove(&mut self, key: &str) -> Result<ContainerSummary, StoreError> {
        let i = self.resolve(key)?;
        if matches!(self.containers[i].status, ContainerStatus::Running | ContainerStatus::Paused) {
            return Err(StoreError::Running(self.containers[i].name.clone()));
        }
        let c = self.containers.remove(i);
        self.logs.remove(&c.id);
        Ok(c)
    }

    pub fn set_images(&mut self, images: Vec<ImageSummary>) {
        self.images = images;
    }

    pub fn set_engine(&mut self, engine: EngineInfo) {
        self.engine = Some(engine);
    }

    /// Sample data matching the design mockups.
    pub fn demo() -> Self {
        let now = now_secs();
        let mib = 1024 * 1024;
        let tcp = |host_port, container_port| PortMapping { host_ip: None, host_port, container_port, protocol: PortProtocol::Tcp };
        let mut store = Store::new();

        let mut add = |id: &str, name: &str, image: &str, running: Option<(u64, f64, u64)>, exit: Option<i32>, ports: Vec<PortMapping>, mounts: Vec<Mount>, ip: Option<&str>| {
            store.containers.push(ContainerSummary {
                id: id.into(),
                name: name.into(),
                image: image.into(),
                status: if running.is_some() { ContainerStatus::Running } else { ContainerStatus::Exited },
                exit_code: exit,
                ports,
                mounts,
                ip_address: ip.map(Into::into),
                cpu_percent: running.map(|r| r.1),
                memory_bytes: running.map(|r| r.2 * mib),
                memory_limit_bytes: Some(1024 * mib),
                created_at: now - 4 * 3600,
                started_at: running.map(|r| now - r.0),
            });
        };
        add("a3f9c2e81b07", "api-gateway", "storefront/api:0.4.2", Some((11_520, 12.4, 412)), None, vec![tcp(8080, 80)],
            vec![Mount { source: "~/code/storefront".into(), target: "/app".into(), read_only: false }], Some("192.168.64.4"));
        add("5d10e7b3c4aa", "postgres", "postgres:17-alpine", Some((11_520, 3.1, 286)), None, vec![tcp(5432, 5432)],
            vec![Mount { source: "pgdata".into(), target: "/var/lib/postgresql/data".into(), read_only: false }], Some("192.168.64.2"));
        add("9be2041f6d33", "redis-cache", "redis:7.4", Some((11_520, 0.6, 24)), None, vec![tcp(6379, 6379)], vec![], Some("192.168.64.3"));
        add("c71a88e09f12", "web", "node:22-slim", Some((2_460, 7.8, 538)), None, vec![tcp(3000, 3000)],
            vec![Mount { source: "~/code/storefront/web".into(), target: "/srv".into(), read_only: false }], Some("192.168.64.5"));
        add("e4c5d6f7a8b9", "worker", "storefront/worker:0.4.2", None, Some(0), vec![], vec![], None);
        add("1f2e3d4c5b6a", "minio", "minio/minio:latest", None, Some(137), vec![tcp(9000, 9000), tcp(9001, 9001)], vec![], None);

        let image = |r: &str, size_mb: u64, arch: &str, used_by: u32, digest: &str| {
            let parsed = ImageReference::parse(r).expect("demo references are valid");
            let display = parsed.display_name();
            let (repository, tag) = display.rsplit_once(':').unwrap_or((&display, "latest"));
            ImageSummary {
                reference: parsed.to_string(),
                repository: repository.to_string(),
                tag: tag.to_string(),
                digest: format!("sha256:{digest}"),
                size_bytes: size_mb * mib,
                architecture: arch.into(),
                used_by,
            }
        };
        store.images = vec![
            image("storefront/api:0.4.2", 84, "arm64", 1, "6e7a1c0d"),
            image("storefront/worker:0.4.2", 79, "arm64", 1, "a91b22f4"),
            image("postgres:17-alpine", 274, "arm64", 1, "0c4d93e1"),
            image("redis:7.4", 117, "arm64", 1, "f2d8a7b6"),
            image("node:22-slim", 221, "arm64", 1, "77e0c1aa"),
            image("minio/minio:latest", 162, "arm64", 1, "3b5f0e92"),
            image("python:3.13-slim", 146, "arm64", 0, "d4a6c8e0"),
            image("ubuntu:24.04", 101, "amd64", 0, "8f1e2d3c"),
            image("ghcr.io/astral-sh/uv:0.9", 38, "arm64", 0, "5a6b7c8d"),
        ];

        let t0 = (now - 30) * 1000;
        let api_logs = [
            (LogLevel::Info, "graviton-agent: process started (pid 1 -> 27)"),
            (LogLevel::Info, "loading config from /app/config.toml"),
            (LogLevel::Info, "connected to postgres://postgres:5432/storefront"),
            (LogLevel::Info, "connected to redis://redis-cache:6379"),
            (LogLevel::Info, "listening on 0.0.0.0:80"),
            (LogLevel::Info, "GET  /v1/health 200 0.4ms"),
            (LogLevel::Info, "GET  /v1/products?page=1 200 4.1ms"),
            (LogLevel::Info, "GET  /v1/products/8812 200 2.7ms"),
            (LogLevel::Info, "POST /v1/cart 201 9.3ms"),
            (LogLevel::Info, "GET  /v1/cart 200 7.0ms"),
            (LogLevel::Warn, "slow query: SELECT * FROM inventory ... (182ms)"),
            (LogLevel::Info, "GET  /v1/products?page=2 200 3.8ms"),
            (LogLevel::Error, "POST /v1/checkout 502 upstream payments timed out after 5000ms"),
            (LogLevel::Info, "retrying payments request (1/3)"),
            (LogLevel::Info, "POST /v1/checkout 200 548ms"),
            (LogLevel::Info, "GET  /v1/orders/30117 200 3.2ms"),
            (LogLevel::Info, "GET  /v1/health 200 0.3ms"),
            (LogLevel::Info, "GET  /v1/products?q=lamp 200 5.6ms"),
        ];
        store.logs.insert(
            "a3f9c2e81b07".into(),
            api_logs
                .iter()
                .enumerate()
                .map(|(i, (level, msg))| LogLine {
                    seq: 1267 + i as u64,
                    timestamp_ms: t0 + i as u64 * 1_350,
                    level: *level,
                    message: (*msg).into(),
                    stream: LogStream::Stdout,
                })
                .collect(),
        );

        store.engine = Some(EngineInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: PROTOCOL_VERSION,
            kernel: "Linux 6.12 (demo)".into(),
            arch: "arm64".into(),
            cpus: 4,
            memory_bytes: 8 * 1024 * mib,
            memory_used_bytes: 3174 * mib,
            disk_bytes: 64 * 1024 * mib,
            disk_used_bytes: 6963 * mib,
        });
        store
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_by_name_and_prefix() {
        let s = Store::demo();
        assert_eq!(s.container("postgres").unwrap().id, "5d10e7b3c4aa");
        assert_eq!(s.container("a3f9").unwrap().name, "api-gateway");
        assert_eq!(s.container("nope"), Err(StoreError::NotFound("nope".into())));
        assert_eq!(s.resolve(""), Err(StoreError::Ambiguous("".into())));
    }

    #[test]
    fn lifecycle_transitions() {
        let mut s = Store::demo();
        assert!(matches!(s.start("api-gateway"), Err(StoreError::InvalidState { .. })));
        let stopped = s.stop("api-gateway").unwrap();
        assert_eq!(stopped.status, ContainerStatus::Exited);
        assert!(s.stop("api-gateway").is_err());
        let started = s.start("worker").unwrap();
        assert_eq!(started.status, ContainerStatus::Running);
        assert!(started.started_at.is_some());
        assert_eq!(s.restart("worker").unwrap().status, ContainerStatus::Running);
    }

    #[test]
    fn insert_log_remove() {
        let mut s = Store::new();
        let mut c = Store::demo().containers()[4].clone(); // exited "worker"
        s.insert(c.clone()).unwrap();
        assert_eq!(s.insert(c.clone()), Err(StoreError::NameTaken("worker".into())));
        s.push_log(&c.id, LogLevel::Info, LogStream::Stdout, "hello");
        s.push_log(&c.id, LogLevel::Info, LogStream::Stderr, "oops");
        let logs = s.logs("worker", None).unwrap();
        assert_eq!((logs[0].seq, logs[1].seq, logs[1].stream), (1, 2, LogStream::Stderr));
        s.update(&c.id, |c| c.status = ContainerStatus::Running);
        assert_eq!(s.remove("worker"), Err(StoreError::Running("worker".into())));
        s.update(&c.id, |c| c.status = ContainerStatus::Exited);
        c = s.remove("worker").unwrap();
        assert!(s.logs(&c.id, None).is_err());
    }

    #[test]
    fn log_tail() {
        let s = Store::demo();
        let all = s.logs("api-gateway", None).unwrap();
        let tail = s.logs("api-gateway", Some(3)).unwrap();
        assert_eq!(tail.len(), 3);
        assert_eq!(tail.last(), all.last());
        assert!(s.logs("redis-cache", Some(5)).unwrap().is_empty());
    }
}
