//! Wire protocol for Graviton.
//!
//! Two channels share these types:
//!
//! * **Control API**: clients (the `grav` CLI and the SwiftUI app) talk to
//!   `gravitond` over a Unix socket. Each message is one JSON object on one
//!   line ([`Request`] in, [`Response`] out).
//! * **Agent channel**: `gravitond` talks to `graviton-agent` inside each guest
//!   VM over vsock, again as newline-delimited JSON ([`AgentRequest`] /
//!   [`AgentEvent`]).

use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Bumped whenever a breaking change is made to the control API.
pub const PROTOCOL_VERSION: u32 = 1;

/// vsock port the guest agent listens on.
pub const AGENT_VSOCK_PORT: u32 = 1024;

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerStatus {
    Created,
    Running,
    Paused,
    Exited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortProtocol {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortMapping {
    pub host_port: u16,
    pub container_port: u16,
    pub protocol: PortProtocol,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContainerSummary {
    pub id: String,
    pub name: String,
    pub image: String,
    pub status: ContainerStatus,
    pub exit_code: Option<i32>,
    pub ports: Vec<PortMapping>,
    pub mounts: Vec<Mount>,
    pub ip_address: Option<String>,
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
    pub memory_limit_bytes: Option<u64>,
    /// Unix seconds.
    pub created_at: u64,
    /// Unix seconds; set while running.
    pub started_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSummary {
    pub reference: String,
    pub repository: String,
    pub tag: String,
    pub digest: String,
    pub size_bytes: u64,
    pub architecture: String,
    pub used_by: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    #[default]
    Stdout,
    Stderr,
    /// Messages from Graviton itself (pulling, booting, errors).
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLine {
    pub seq: u64,
    /// Unix milliseconds.
    pub timestamp_ms: u64,
    pub level: LogLevel,
    pub message: String,
    #[serde(default)]
    pub stream: LogStream,
}

/// What `grav run` asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSpec {
    pub image: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Replaces the image's Cmd (the entrypoint is kept).
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// `KEY=value` pairs added to the image's environment.
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub cpus: Option<u32>,
    #[serde(default)]
    pub memory_mib: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineInfo {
    pub version: String,
    pub protocol: u32,
    pub kernel: String,
    pub arch: String,
    pub cpus: u32,
    pub memory_bytes: u64,
    pub memory_used_bytes: u64,
    pub disk_bytes: u64,
    pub disk_used_bytes: u64,
}

// ---------------------------------------------------------------------------
// Control API
// ---------------------------------------------------------------------------

/// A client request. Serialises as `{"method": "...", "params": {...}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    Ping,
    EngineInfo,
    ListContainers,
    ListImages,
    /// `id` may be a container id, id prefix or name.
    Logs { id: String, tail: Option<usize> },
    Start { id: String },
    Stop { id: String },
    Restart { id: String },
    /// Pull an image from its registry; answers `Image` when done.
    Pull { reference: String },
    /// Create a container and start it in its own VM; answers `Container` once started.
    Run(RunSpec),
    /// Remove a stopped container.
    Remove { id: String },
}

/// A daemon response. Serialises as `{"type": "...", "data": ...}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Response {
    Pong { protocol: u32 },
    Engine(EngineInfo),
    Containers(Vec<ContainerSummary>),
    Images(Vec<ImageSummary>),
    Logs(Vec<LogLine>),
    Container(ContainerSummary),
    Image(ImageSummary),
    Removed { id: String },
    Error { message: String },
}

// ---------------------------------------------------------------------------
// Agent channel (host <-> guest)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AgentRequest {
    /// Unpack image layers (digests in the `blobs` virtiofs share, lowest
    /// first) into a fresh root filesystem for `container`.
    Prepare { container: String, layers: Vec<String> },
    /// Run a process. With `container`, it runs chrooted into that container's
    /// root filesystem in new mount, UTS and IPC namespaces.
    Exec {
        argv: Vec<String>,
        env: Vec<String>,
        cwd: String,
        tty: bool,
        #[serde(default)]
        container: Option<String>,
        /// `user[:group]`, names or numbers, resolved in the container's /etc.
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        hostname: Option<String>,
    },
    Signal { pid: i32, signal: i32 },
    Stdin { pid: i32, data: Vec<u8> },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentEvent {
    Ready { kernel: String },
    Prepared { container: String, files: u64, ms: u64 },
    Started { pid: i32 },
    Stdout { pid: i32, data: Vec<u8> },
    Stderr { pid: i32, data: Vec<u8> },
    Exited { pid: i32, code: i32 },
    Error { message: String },
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// Encode a message as one JSON line (with trailing `\n`).
pub fn encode_line<T: Serialize>(msg: &T) -> Vec<u8> {
    let mut buf = serde_json::to_vec(msg).expect("protocol types always serialise");
    buf.push(b'\n');
    buf
}

/// Decode one JSON line (trailing whitespace is ignored).
pub fn decode_line<T: DeserializeOwned>(line: &[u8]) -> Result<T, serde_json::Error> {
    serde_json::from_slice(line.trim_ascii_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_wire_shape() {
        let json = String::from_utf8(encode_line(&Request::Logs {
            id: "web".into(),
            tail: Some(10),
        }))
        .unwrap();
        assert_eq!(json, "{\"method\":\"logs\",\"params\":{\"id\":\"web\",\"tail\":10}}\n");

        let ping: Request = decode_line(b"{\"method\":\"ping\"}\n").unwrap();
        assert_eq!(ping, Request::Ping);
    }

    #[test]
    fn response_round_trip() {
        let resp = Response::Pong { protocol: PROTOCOL_VERSION };
        let line = encode_line(&resp);
        assert_eq!(decode_line::<Response>(&line).unwrap(), resp);
        assert_eq!(
            String::from_utf8(line).unwrap(),
            "{\"type\":\"pong\",\"data\":{\"protocol\":1}}\n"
        );
    }

    #[test]
    fn agent_event_shape() {
        let line = encode_line(&AgentEvent::Exited { pid: 7, code: 0 });
        assert_eq!(
            String::from_utf8(line).unwrap(),
            "{\"event\":\"exited\",\"pid\":7,\"code\":0}\n"
        );
    }
}
