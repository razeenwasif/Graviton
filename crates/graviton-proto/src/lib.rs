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
pub const PROTOCOL_VERSION: u32 = 2;

/// vsock port the guest agent listens on.
pub const AGENT_VSOCK_PORT: u32 = 1024;

/// TCP port of gravitond's remote (TLS) endpoint, and its Bonjour service type.
pub const REMOTE_PORT: u16 = 7843;
pub const BONJOUR_SERVICE: &str = "_graviton._tcp";

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
    /// `host:container[/tcp]` or `port`, published on 127.0.0.1.
    #[serde(default)]
    pub ports: Vec<String>,
    /// `/host/dir:/container/path[:ro]` or `name:/container/path[:ro]` (a named volume).
    #[serde(default)]
    pub volumes: Vec<String>,
}

/// What `grav pair` shows: everything a phone needs to pair with this Mac.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingInfo {
    /// One-time code, valid for `expires_in_secs`.
    pub code: String,
    pub expires_in_secs: u64,
    /// The Mac's name as shown on the phone.
    pub host: String,
    pub port: u16,
    /// SHA-256 of the endpoint's certificate, lowercase hex; the phone pins it.
    pub fingerprint: String,
    /// This Mac's LAN addresses.
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedDevice {
    pub id: String,
    pub name: String,
    /// Unix seconds.
    pub paired_at: u64,
    pub last_seen: Option<u64>,
}

/// What `grav exec` asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecSpec {
    /// Container id, id prefix or name.
    pub id: String,
    pub command: Vec<String>,
    /// Run in a pseudo-terminal; the connection then carries raw bytes both ways.
    #[serde(default)]
    pub tty: bool,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub env: Vec<String>,
}

/// One sample of a container's resource use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatSample {
    /// Unix milliseconds.
    pub timestamp_ms: u64,
    /// 100 is one CPU fully busy (so two busy vCPUs read 200).
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub memory_limit_bytes: u64,
    /// Totals since the VM booted.
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
}

/// A process inside a container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: i32,
    pub ppid: i32,
    pub uid: u32,
    /// Process state letter from /proc (R running, S sleeping, ...).
    pub state: String,
    pub command: String,
    pub rss_bytes: u64,
    /// CPU time used, in milliseconds.
    pub cpu_ms: u64,
}

/// What the Stats tab shows: recent samples (oldest first) and the processes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContainerStats {
    pub id: String,
    pub cpus: u32,
    /// Seconds between samples.
    pub interval_secs: u32,
    pub samples: Vec<StatSample>,
    pub processes: Vec<ProcessInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub kind: FileKind,
    pub size: u64,
    /// Permission bits, e.g. 0o755.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// Unix seconds.
    pub modified: i64,
    #[serde(default)]
    pub link_target: Option<String>,
}

/// A folder in a container, sorted folders first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryListing {
    pub path: String,
    pub entries: Vec<FileEntry>,
}

/// The start of a file in a container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileContents {
    pub path: String,
    pub size: u64,
    pub data: Vec<u8>,
    /// Set when `data` holds only the first part of the file.
    pub truncated: bool,
}

/// Everything about how a container was set up and started.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContainerDetails {
    pub container: ContainerSummary,
    pub image_reference: String,
    pub image_digest: String,
    /// The process the container runs (entrypoint plus command).
    pub command: Vec<String>,
    pub env: Vec<String>,
    pub working_dir: String,
    pub user: Option<String>,
    pub hostname: String,
    pub cpus: u32,
    pub memory_mib: u64,
    pub kernel: String,
    /// Where the container's VM keeps its console and logs on the Mac.
    pub vm_dir: String,
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
    /// Run a command in a running container. Without a TTY the daemon answers
    /// with `ExecOutput` lines and a final `ExecExited`; with one it answers
    /// `Attached` and the connection then carries terminal [`frame`]s.
    Exec(ExecSpec),
    /// Recent resource use and the processes of a running container.
    Stats { id: String },
    /// List a folder inside a running container.
    ListFiles { id: String, path: String },
    /// Read the start of a file inside a running container.
    ReadFile { id: String, path: String },
    /// How a container is configured.
    Inspect { id: String },
    /// Local only: create a one-time pairing code for a phone.
    PairBegin,
    /// Local only: devices paired with this Mac.
    ListDevices,
    /// Local only: forget a paired device; its token stops working.
    RevokeDevice { id: String },
    /// Remote, first message: trade a pairing code for a token.
    Pair { code: String, device: String },
    /// Remote, first message: authenticate with a token from `Pair`.
    Auth { token: String },
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
    ExecOutput { stream: LogStream, data: Vec<u8> },
    ExecExited { code: i32 },
    Attached,
    Stats(ContainerStats),
    Files(DirectoryListing),
    File(FileContents),
    Details(Box<ContainerDetails>),
    Pairing(PairingInfo),
    Devices(Vec<PairedDevice>),
    Revoked { id: String },
    Paired { token: String, device_id: String, host: String },
    Authenticated { host: String },
    Error { message: String },
}

// ---------------------------------------------------------------------------
// Agent channel (host <-> guest)
// ---------------------------------------------------------------------------

/// A virtiofs share to mount inside a container's root filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestMount {
    pub tag: String,
    /// Absolute path inside the container.
    pub target: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AgentRequest {
    /// Unpack image layers (digests in the `blobs` virtiofs share, lowest
    /// first) into a fresh root filesystem for `container`, then mount its
    /// volumes and write its network files.
    Prepare {
        container: String,
        layers: Vec<String>,
        #[serde(default)]
        mounts: Vec<GuestMount>,
        #[serde(default)]
        hostname: Option<String>,
    },
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
        /// Terminal size when `tty` is set.
        #[serde(default)]
        rows: u16,
        #[serde(default)]
        cols: u16,
    },
    Signal { pid: i32, signal: i32 },
    Stdin { pid: i32, data: Vec<u8> },
    /// Resource counters for the whole guest (one guest runs one container).
    Stats {
        #[serde(default)]
        processes: bool,
    },
    /// List a folder; `path` is resolved inside the container's root.
    ListDir { container: String, path: String },
    /// Read up to `limit` bytes from the start of a file in the container.
    ReadFile { container: String, path: String, limit: u64 },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentEvent {
    Ready { kernel: String },
    Prepared {
        container: String,
        files: u64,
        ms: u64,
        /// The guest's address on the NAT network, once DHCP has finished.
        #[serde(default)]
        ip: Option<String>,
    },
    Started { pid: i32 },
    Stdout { pid: i32, data: Vec<u8> },
    Stderr { pid: i32, data: Vec<u8> },
    Exited { pid: i32, code: i32 },
    Stats(GuestStats),
    Dir(DirectoryListing),
    File(FileContents),
    Error { message: String },
}

/// Raw counters from the guest's /proc; gravitond turns them into rates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestStats {
    pub cpus: u32,
    /// Jiffies spent busy, and in total, across all CPUs since boot.
    pub cpu_busy: u64,
    pub cpu_total: u64,
    pub memory_total_bytes: u64,
    pub memory_available_bytes: u64,
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
    #[serde(default)]
    pub processes: Vec<ProcessInfo>,
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// Terminal sessions. After `attached` (and the agent's `started`) the
/// connection carries frames in both directions: one kind byte, a big-endian
/// u32 length, then the payload. gravitond passes them through untouched.
pub mod frame {
    /// Bytes for (from the client) or from (to the client) the terminal.
    pub const DATA: u8 = 0;
    /// Client: the window changed size. Payload: rows u16, cols u16 (big-endian).
    pub const RESIZE: u8 = 1;
    /// Client: input has ended (a pipe closed); the terminal gets ^D.
    pub const EOF: u8 = 2;
    /// Agent: the program exited. Payload: code i32 (big-endian). Last frame.
    pub const EXIT: u8 = 3;

    pub const MAX_PAYLOAD: usize = 1 << 20;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Frame {
        Data(Vec<u8>),
        Resize { rows: u16, cols: u16 },
        Eof,
        Exit(i32),
    }

    impl Frame {
        pub fn encode(&self) -> Vec<u8> {
            let (kind, payload): (u8, Vec<u8>) = match self {
                Frame::Data(d) => (DATA, d.clone()),
                Frame::Resize { rows, cols } => (RESIZE, [rows.to_be_bytes(), cols.to_be_bytes()].concat()),
                Frame::Eof => (EOF, vec![]),
                Frame::Exit(code) => (EXIT, code.to_be_bytes().to_vec()),
            };
            data_frame(kind, &payload)
        }
    }

    /// Frame `payload` as `kind` without building a [`Frame`] first.
    pub fn data_frame(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(5 + payload.len());
        out.push(kind);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// Splits a byte stream into frames, however it arrives.
    #[derive(Default)]
    pub struct Decoder(Vec<u8>);

    impl Decoder {
        pub fn push(&mut self, bytes: &[u8]) {
            self.0.extend_from_slice(bytes);
        }

        /// The next complete frame, `Ok(None)` if more bytes are needed.
        pub fn next_frame(&mut self) -> Result<Option<Frame>, String> {
            if self.0.len() < 5 {
                return Ok(None);
            }
            let len = u32::from_be_bytes([self.0[1], self.0[2], self.0[3], self.0[4]]) as usize;
            if len > MAX_PAYLOAD {
                return Err(format!("terminal frame too long ({len} bytes)"));
            }
            if self.0.len() < 5 + len {
                return Ok(None);
            }
            let kind = self.0[0];
            let payload: Vec<u8> = self.0.drain(..5 + len).skip(5).collect();
            let frame = match (kind, payload.len()) {
                (DATA, _) => Frame::Data(payload),
                (RESIZE, 4) => Frame::Resize {
                    rows: u16::from_be_bytes([payload[0], payload[1]]),
                    cols: u16::from_be_bytes([payload[2], payload[3]]),
                },
                (EOF, 0) => Frame::Eof,
                (EXIT, 4) => Frame::Exit(i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])),
                _ => return Err(format!("bad terminal frame (kind {kind}, {} bytes)", payload.len())),
            };
            Ok(Some(frame))
        }
    }
}

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
            "{\"type\":\"pong\",\"data\":{\"protocol\":2}}\n"
        );
    }

    #[test]
    fn run_from_swift() {
        // Exactly what GravitonKit's `Wire.line(.run(...))` sends (see its RunTests).
        let line = br#"{"method":"run","params":{"command":["python3","-m","http.server"],"env":["A=1"],"image":"python:3.13-slim","memory_mib":2048,"name":"web","ports":["8080:8000"],"volumes":["~/code:/app:ro"]}}"#;
        let Request::Run(spec) = decode_line::<Request>(line).unwrap() else { panic!("not a run") };
        assert_eq!(spec.image, "python:3.13-slim");
        assert_eq!(spec.name.as_deref(), Some("web"));
        assert_eq!(spec.command.as_deref(), Some(&["python3".to_string(), "-m".into(), "http.server".into()][..]));
        assert_eq!((spec.cpus, spec.memory_mib), (None, Some(2048)));
        assert_eq!((spec.ports, spec.volumes), (vec!["8080:8000".to_string()], vec!["~/code:/app:ro".to_string()]));
        let pull: Request = decode_line(br#"{"method":"pull","params":{"reference":"alpine"}}"#).unwrap();
        assert_eq!(pull, Request::Pull { reference: "alpine".into() });
    }

    #[test]
    fn terminal_frames() {
        use frame::{Decoder, Frame};
        let frames = [Frame::Data(b"ls\r".to_vec()), Frame::Resize { rows: 40, cols: 120 }, Frame::Eof, Frame::Exit(-3)];
        let bytes: Vec<u8> = frames.iter().flat_map(Frame::encode).collect();
        assert_eq!(&bytes[..8], &[0, 0, 0, 0, 3, b'l', b's', b'\r']);
        // Byte by byte, as a slow connection might deliver it.
        let mut d = Decoder::default();
        let mut got = Vec::new();
        for b in bytes {
            d.push(&[b]);
            while let Some(f) = d.next_frame().unwrap() {
                got.push(f);
            }
        }
        assert_eq!(got, frames);
        let mut bad = Decoder::default();
        bad.push(&[9, 0, 0, 0, 0]);
        assert!(bad.next_frame().is_err());
    }

    #[test]
    fn new_requests_from_swift() {
        let r: Request = decode_line(br#"{"method":"list_files","params":{"id":"web","path":"/etc"}}"#).unwrap();
        assert_eq!(r, Request::ListFiles { id: "web".into(), path: "/etc".into() });
        let r: Request = decode_line(br#"{"method":"stats","params":{"id":"web"}}"#).unwrap();
        assert_eq!(r, Request::Stats { id: "web".into() });
        let line = String::from_utf8(encode_line(&AgentRequest::Stats { processes: true })).unwrap();
        assert_eq!(line, "{\"op\":\"stats\",\"processes\":true}\n");
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
