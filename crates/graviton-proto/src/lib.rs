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

pub mod build;
pub mod dns;

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

/// When `gravitond` should restart a container automatically.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    /// Never restart automatically.
    #[default]
    No,
    /// Restart on non-zero exit (with backoff), and on daemon startup if it was running.
    OnFailure,
    /// Restart on any self-exit (with backoff), and on daemon startup unless stopped.
    Always,
}

impl RestartPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            RestartPolicy::No => "no",
            RestartPolicy::OnFailure => "on-failure",
            RestartPolicy::Always => "always",
        }
    }
}

impl std::fmt::Display for RestartPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RestartPolicy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "no" | "never" => Ok(RestartPolicy::No),
            "on-failure" | "on_failure" => Ok(RestartPolicy::OnFailure),
            "always" => Ok(RestartPolicy::Always),
            other => Err(format!("unknown restart policy `{other}` (use: no, on-failure, always)")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortMapping {
    /// Interface address on the Mac to bind (`None` means `127.0.0.1`).
    /// Use `0.0.0.0` (or `::`) for every interface, or a specific IP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_ip: Option<String>,
    pub host_port: u16,
    pub container_port: u16,
    pub protocol: PortProtocol,
}

impl PortMapping {
    /// The host IP this mapping binds on the Mac (defaults to `127.0.0.1`).
    pub fn bind_ip(&self) -> &str {
        self.host_ip.as_deref().unwrap_or("127.0.0.1")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

/// One container's mount of a named volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeMountRef {
    pub container_id: String,
    pub container_name: String,
    pub target: String,
    pub read_only: bool,
    pub running: bool,
}

/// One named volume (`-v name:/path`) stored under Graviton's `volumes/` folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeSummary {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    pub file_count: u64,
    /// Unix seconds.
    pub created_at: u64,
    pub used_by: Vec<VolumeMountRef>,
}

/// One container connected to (or known on) the container network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkEndpoint {
    pub container_id: String,
    pub container_name: String,
    pub image: String,
    pub status: ContainerStatus,
    pub ip_address: Option<String>,
    pub dns_names: Vec<String>,
    pub dns_enabled: bool,
    pub ports: Vec<PortMapping>,
}

/// Live view of Graviton's shared container network, switch, and embedded DNS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkInfo {
    pub name: String,
    pub driver: String,
    pub subnet: String,
    pub gateway: String,
    pub dns_server: String,
    pub dns_domain: String,
    pub connected_ports: u32,
    pub learned_macs: u32,
    pub frames_forwarded: u64,
    pub bytes_forwarded: u64,
    pub endpoints: Vec<NetworkEndpoint>,
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

/// Per-layer pull progress, streamed before the final `Response::Image` when
/// `Request::Pull { progress: true, .. }` is set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum PullProgress {
    /// Manifest resolved; `layers` layers totalling `total_bytes` compressed.
    Resolved { layers: usize, total_bytes: u64 },
    /// Progress on layer `index` (0-based of `total_layers`).
    Layer {
        index: usize,
        total_layers: usize,
        digest: String,
        done: u64,
        total: u64,
        cached: bool,
    },
}

/// One saved registry login (username only; passwords stay in the Mac's Keychain).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryLoginInfo {
    pub registry: String,
    pub username: String,
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
    /// `[ip:]host:container[/tcp|udp]` or `port[/tcp|udp]` (default bind IP: 127.0.0.1).
    #[serde(default)]
    pub ports: Vec<String>,
    /// `/host/dir:/container/path[:ro]` or `name:/container/path[:ro]` (a named volume).
    #[serde(default)]
    pub volumes: Vec<String>,
    /// Automatic restart policy (`no`, `on-failure`, `always`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart: Option<RestartPolicy>,
    /// Run the container's main process attached to a pseudo-terminal (`grav run -it`);
    /// the daemon answers `Attached` and the connection then carries terminal [`frame`]s.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tty: bool,
    /// Initial terminal size when `tty` is set.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub rows: u16,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub cols: u16,
    /// Automatically remove the container after it exits (`grav run --rm`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto_remove: bool,
}

fn is_zero_u16(n: &u16) -> bool {
    *n == 0
}

/// Push alert set-up on this Mac.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushInfo {
    pub configured: bool,
    pub key_id: Option<String>,
    pub team_id: Option<String>,
    /// Paired devices that registered for alerts.
    pub devices: u32,
    /// What the last send did (for `grav push test`).
    #[serde(default)]
    pub note: Option<String>,
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

/// Something that happened on the engine, for the Activity feed and alerts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineEvent {
    /// Increases by one per event; ask for events after the last one you saw.
    pub seq: u64,
    /// Unix milliseconds.
    pub timestamp_ms: u64,
    pub kind: EventKind,
    #[serde(default)]
    pub container_id: Option<String>,
    #[serde(default)]
    pub container: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    /// One line for people: "web exited with code 1".
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Started,
    /// Exited on its own with code 0, or stopped on request.
    Stopped,
    /// Exited on its own with a non-zero code, or couldn't start.
    Failed,
    Pulled,
    Built,
    BuildFailed,
    Removed,
}

impl EventKind {
    /// Worth interrupting someone for (a push alert).
    pub fn is_alert(self) -> bool {
        matches!(self, EventKind::Failed | EventKind::Built | EventKind::BuildFailed)
    }
}

/// One secret passed via `grav build --secret id=...,src=...` (kept in memory,
/// never written to the image or build cache, never printed in debug output).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildSecret {
    pub id: String,
    pub data: Vec<u8>,
}

impl std::fmt::Debug for BuildSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildSecret")
            .field("id", &self.id)
            .field("data", &"[redacted]")
            .finish()
    }
}

/// What `grav build` asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildSpec {
    /// Absolute path of the build context (a folder on the Mac).
    pub context: String,
    /// The Dockerfile: relative to the context, or absolute. Default `Dockerfile`.
    #[serde(default)]
    pub dockerfile: Option<String>,
    /// Name for the image, e.g. `myapp` or `myapp:1.0`. Default: the context folder's name.
    #[serde(default)]
    pub tag: Option<String>,
    /// `KEY=value` for the Dockerfile's ARGs.
    #[serde(default)]
    pub build_args: Vec<String>,
    /// Build only up to this stage (`FROM … AS name`).
    #[serde(default)]
    pub target: Option<String>,
    /// Rebuild every step instead of reusing cached layers.
    #[serde(default)]
    pub no_cache: bool,
    /// From another Mac: the build folder follows the request line as this
    /// many bytes of tar (gzipped or not), and `context` is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload: Option<u64>,
    /// Secrets for `RUN --mount=type=secret,id=...` (in memory only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<BuildSecret>,
}

/// Where an uploaded build folder's Dockerfile goes when it lives outside the folder.
pub const UPLOADED_DOCKERFILE: &str = ".graviton-dockerfile";

/// Where the disk space goes (`grav df`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskUsage {
    pub images: u32,
    /// Images no container uses.
    pub unused_images: u32,
    /// Every stored image layer, config and manifest, counted once.
    pub blobs_bytes: u64,
    /// What removing the unused images would free (layers no one else needs).
    pub unused_images_bytes: u64,
    pub containers: u32,
    pub stopped_containers: u32,
    /// Containers' disks as they really take up space (they're sparse).
    pub container_disks_bytes: u64,
    pub stopped_disks_bytes: u64,
    /// Steps remembered by the build cache, and the layers only it keeps.
    pub build_cache_steps: u32,
    pub build_cache_bytes: u64,
    /// Layers nothing refers to any more.
    pub unreferenced_bytes: u64,
}

/// What `grav prune` removes. The build cache and unreferenced layers always go.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneSpec {
    /// Also images no container uses.
    #[serde(default)]
    pub images: bool,
    /// Also stopped containers, with their disks.
    #[serde(default)]
    pub containers: bool,
    /// Report what would go without removing anything.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneReport {
    /// Names of the containers removed.
    pub containers: Vec<String>,
    /// References of the images removed.
    pub images: Vec<String>,
    pub build_cache_steps: u32,
    /// Blob files deleted.
    pub blobs: u32,
    pub freed_bytes: u64,
    pub dry_run: bool,
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
    /// Automatic restart policy saved with the container.
    #[serde(default)]
    pub restart: RestartPolicy,
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
    /// Named volumes on this Mac, with disk size and which containers use each.
    ListVolumes,
    /// Create an empty named volume.
    CreateVolume { name: String },
    /// Remove a named volume no container uses.
    RemoveVolume { name: String },
    /// Live state of the shared container network, switch and DNS.
    NetworkInfo,
    /// `id` may be a container id, id prefix or name.
    Logs { id: String, tail: Option<usize> },
    Start { id: String },
    Stop { id: String },
    Restart { id: String },
    /// Pull an image from its registry; answers `Image` when done.
    /// With `progress: true`, streams `PullProgress` lines before the final `Image`.
    Pull {
        reference: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        progress: bool,
    },
    /// Verify and save credentials for an OCI registry (password goes in the Mac's Keychain).
    RegistryLogin {
        registry: String,
        username: String,
        password: String,
    },
    /// Remove saved credentials for an OCI registry.
    RegistryLogout { registry: String },
    /// List registries with saved credentials.
    ListLogins,
    /// Build an image from a Dockerfile. The daemon answers with `BuildLog`
    /// lines as it goes, then `Image` (or `Error`).
    Build(BuildSpec),
    /// Create a container and start it in its own VM; answers `Container` once started.
    Run(RunSpec),
    /// Remove a stopped container.
    Remove { id: String },
    /// Where the disk space goes.
    DiskUsage,
    /// Remove an image no container uses, and the layers only it needed.
    RemoveImage { reference: String },
    /// Clear the build cache and unreferenced layers, and more if asked.
    Prune(PruneSpec),
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
    /// Stream a file or folder out of a running container as a tar archive.
    /// Answers `Archive { is_dir, name }` followed by raw tar bytes until EOF.
    GetArchive { id: String, path: String },
    /// Unpack `size` bytes of tar (following the request line) into a directory
    /// inside a running container; answers `Copied { files }`.
    PutArchive { id: String, path: String, size: u64 },
    /// How a container is configured.
    Inspect { id: String },
    /// Recent events, oldest first: those after `since` (a `seq`), at most `limit`.
    Events {
        #[serde(default)]
        since: Option<u64>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Local only: store the APNs key (a .p8's text) in the login Keychain,
    /// so failures and finished builds reach paired phones as push alerts.
    PushSetup { key_id: String, team_id: String, p8: String },
    /// Local only: whether push alerts are set up, and to how many phones.
    PushStatus,
    /// Local only: send a test alert to every registered phone.
    PushTest,
    /// Remote only: this device's APNs token (hex); `sandbox` for debug builds.
    RegisterPush { token: String, sandbox: bool },
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
    Volumes(Vec<VolumeSummary>),
    Volume(VolumeSummary),
    VolumeRemoved { name: String, freed_bytes: u64 },
    Network(NetworkInfo),
    Logs(Vec<LogLine>),
    Container(ContainerSummary),
    Image(ImageSummary),
    PullProgress(PullProgress),
    LoggedIn { registry: String, username: String },
    LoggedOut { registry: String },
    Logins(Vec<RegistryLoginInfo>),
    Removed { id: String },
    DiskUsage(DiskUsage),
    /// `kept_bytes`: its layers the build cache still holds (`grav prune` frees them).
    ImageRemoved { reference: String, freed_bytes: u64, kept_bytes: u64 },
    Pruned(PruneReport),
    BuildLog { line: String },
    ExecOutput { stream: LogStream, data: Vec<u8> },
    ExecExited { code: i32 },
    Attached,
    Stats(ContainerStats),
    Files(DirectoryListing),
    File(FileContents),
    /// Sent before streaming raw tar bytes for `Request::GetArchive`.
    Archive { is_dir: bool, name: String },
    /// Sent after `Request::PutArchive` unpacks into the container.
    Copied { files: u64 },
    Details(Box<ContainerDetails>),
    Events(Vec<EngineEvent>),
    Push(PushInfo),
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

/// A container's name, id, and IPv4 address on the container network, for guest DNS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostEntry {
    pub name: String,
    #[serde(default)]
    pub id: String,
    pub ip: String,
}

/// Inline file content for `COPY <<EOF ...` heredocs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineFile {
    pub name: String,
    pub content: Vec<u8>,
}

/// File downloaded on the host (e.g. `ADD https://...`) and exposed in the
/// builder VM's `staged` virtiofs share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedFile {
    /// Path relative to the `staged` virtiofs share root.
    pub staged_path: String,
    /// Destination file name when copying into a directory.
    pub name: String,
    /// Default permission mode (overridden by `Copy.chmod`).
    pub mode: u32,
}

/// Kind of ephemeral mount attached for a single `RUN --mount=...` execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecMountKind {
    /// Persistent directory in the `cache` virtiofs share.
    Cache,
    /// Single secret file in the `secrets` virtiofs share (read-only).
    Secret,
    /// File or folder from the build context (`context` share) or another stage's container rootfs.
    Bind,
    /// Ephemeral in-memory tmpfs.
    Tmpfs,
}

/// Ephemeral mount applied only inside an `Exec` child's private mount namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecMount {
    pub kind: ExecMountKind,
    /// Relative path in the share (`cache`, `secrets`, `context`) or container rootfs (`bind` with `from_container`).
    #[serde(default)]
    pub source: String,
    /// For `Bind` from another build stage/image container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_container: Option<String>,
    /// Absolute path inside the container where the mount appears.
    pub target: String,
    #[serde(default)]
    pub read_only: bool,
    /// Optional mode (octal bits) applied to created cache/tmpfs/secret targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
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
        /// Optional bytes written to the non-TTY child's stdin (used for `#!` heredoc scripts).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stdin: Option<Vec<u8>>,
        /// Ephemeral per-RUN mounts (cache, secret, bind, tmpfs), visible only inside this process's mount namespace.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mounts: Vec<ExecMount>,
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
    /// Stream a file or folder out of `container` as a raw tar archive on this
    /// dedicated vsock connection (preceded by `AgentEvent::ArchiveReady`).
    GetArchive { container: String, path: String },
    /// Unpack `size` raw tar (or tar.gz) bytes following this request into
    /// directory `path` inside `container`; answers `AgentEvent::Copied`.
    PutArchive { container: String, path: String, size: u64 },
    /// Remember the state of a container's files, for the next `commit`.
    Snapshot { container: String },
    /// Write what changed since the snapshot as a gzipped layer tar named
    /// `name` in the `out` share, then snapshot again.
    Commit { container: String, name: String },
    /// Copy files into a container (Dockerfile COPY/ADD). Sources are paths or
    /// patterns relative to the build context (`context` share), or to another
    /// container's root when `from_container` is set.
    Copy {
        container: String,
        sources: Vec<String>,
        dest: String,
        /// Working directory for a relative `dest`.
        #[serde(default)]
        cwd: String,
        #[serde(default)]
        from_container: Option<String>,
        /// `user[:group]`; files are owned by root otherwise.
        #[serde(default)]
        chown: Option<String>,
        /// Optional `--chmod` permission bits (e.g. `0o755`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chmod: Option<u32>,
        /// ADD: unpack local tar archives instead of copying them.
        #[serde(default)]
        extract: bool,
        /// .dockerignore patterns (context copies only).
        #[serde(default)]
        ignore: Vec<String>,
        /// Inline files from `COPY <<EOF` heredocs.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        inline: Vec<InlineFile>,
        /// Pre-downloaded files from `ADD <url>` in the `staged` share.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        staged: Vec<StagedFile>,
    },
    /// Create a directory (and its parents) in a container (WORKDIR).
    Mkdir { container: String, path: String },
    /// Update the guest DNS resolver's container name table. Sent on the
    /// supervisor connection with no reply event so it never interleaves with
    /// container output.
    SetHosts {
        generation: u64,
        hosts: Vec<HostEntry>,
    },
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
        /// The container's files were already on its disk from an earlier run.
        #[serde(default)]
        reused: bool,
        /// The guest runs the container DNS resolver and accepts `SetHosts`.
        #[serde(default)]
        dns: bool,
    },
    Started { pid: i32 },
    Stdout { pid: i32, data: Vec<u8> },
    Stderr { pid: i32, data: Vec<u8> },
    Exited { pid: i32, code: i32 },
    Stats(GuestStats),
    Dir(DirectoryListing),
    File(FileContents),
    /// Precedes raw tar bytes for `AgentRequest::GetArchive`.
    ArchiveReady { is_dir: bool, name: String },
    /// A step finished that has nothing else to report (snapshot, mkdir).
    Done,
    /// `copy` placed this many files.
    Copied { files: u64 },
    /// `commit`'s layer; `file` is `None` when nothing changed.
    Committed(LayerInfo),
    Error { message: String },
}

/// A layer the agent wrote to the `out` share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerInfo {
    /// File name in the `out` share; `None` for an empty change.
    pub file: Option<String>,
    /// sha256 of the gzipped tar.
    pub digest: String,
    /// sha256 of the uncompressed tar (the image config's diff_id).
    pub diff_id: String,
    pub size: u64,
    /// Files, folders and whiteouts in it.
    pub entries: u64,
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
        assert_eq!(spec.restart, None);
        let with_restart = br#"{"method":"run","params":{"env":[],"image":"redis","ports":[],"restart":"on-failure","volumes":[]}}"#;
        let Request::Run(spec) = decode_line::<Request>(with_restart).unwrap() else { panic!("not a run") };
        assert_eq!(spec.restart, Some(RestartPolicy::OnFailure));
        assert_eq!("always".parse::<RestartPolicy>().unwrap(), RestartPolicy::Always);
        assert_eq!("on-failure".parse::<RestartPolicy>().unwrap(), RestartPolicy::OnFailure);
        assert_eq!("no".parse::<RestartPolicy>().unwrap(), RestartPolicy::No);
        assert!("bogus".parse::<RestartPolicy>().is_err());
        let pull: Request = decode_line(br#"{"method":"pull","params":{"reference":"alpine"}}"#).unwrap();
        assert_eq!(pull, Request::Pull { reference: "alpine".into(), progress: false });
        let pull_stream: Request = decode_line(br#"{"method":"pull","params":{"progress":true,"reference":"alpine"}}"#).unwrap();
        assert_eq!(pull_stream, Request::Pull { reference: "alpine".into(), progress: true });
    }

    #[test]
    fn pull_progress_and_registry_logins() {
        let resolved = Response::PullProgress(PullProgress::Resolved { layers: 3, total_bytes: 12_345 });
        let line = String::from_utf8(encode_line(&resolved)).unwrap();
        assert_eq!(line, "{\"type\":\"pull_progress\",\"data\":{\"stage\":\"resolved\",\"layers\":3,\"total_bytes\":12345}}\n");
        assert_eq!(decode_line::<Response>(line.as_bytes()).unwrap(), resolved);

        let layer = Response::PullProgress(PullProgress::Layer {
            index: 1,
            total_layers: 3,
            digest: "sha256:abcdef0123456789".into(),
            done: 500,
            total: 1000,
            cached: false,
        });
        let line = String::from_utf8(encode_line(&layer)).unwrap();
        assert_eq!(
            line,
            "{\"type\":\"pull_progress\",\"data\":{\"stage\":\"layer\",\"index\":1,\"total_layers\":3,\"digest\":\"sha256:abcdef0123456789\",\"done\":500,\"total\":1000,\"cached\":false}}\n"
        );
        assert_eq!(decode_line::<Response>(line.as_bytes()).unwrap(), layer);

        let login = Request::RegistryLogin {
            registry: "ghcr.io".into(),
            username: "octocat".into(),
            password: "secret".into(),
        };
        let encoded = encode_line(&login);
        assert_eq!(decode_line::<Request>(&encoded).unwrap(), login);

        let logins = Response::Logins(vec![RegistryLoginInfo { registry: "ghcr.io".into(), username: "octocat".into() }]);
        let encoded = encode_line(&logins);
        assert_eq!(decode_line::<Response>(&encoded).unwrap(), logins);
    }

    #[test]
    fn build_secrets_and_mounts_round_trip() {
        let secret = BuildSecret { id: "npm".into(), data: b"super-secret".to_vec() };
        let dbg = format!("{secret:?}");
        assert!(dbg.contains("npm"));
        assert!(!dbg.contains("super-secret"));

        // Swift's BuildSpec omits `secrets`; Rust defaults it to empty.
        let from_swift = br#"{"method":"build","params":{"build_args":["PY=3.12"],"context":"/Users/me/app","no_cache":true,"tag":"app:1"}}"#;
        let Request::Build(spec) = decode_line::<Request>(from_swift).unwrap() else { panic!("not build") };
        assert!(spec.secrets.is_empty());

        let exec = AgentRequest::Exec {
            argv: vec!["/bin/sh".into()],
            env: vec![],
            cwd: "/".into(),
            tty: false,
            container: Some("b1".into()),
            user: None,
            hostname: None,
            rows: 0,
            cols: 0,
            stdin: Some(b"echo hi\n".to_vec()),
            mounts: vec![ExecMount {
                kind: ExecMountKind::Cache,
                source: "pip".into(),
                from_container: None,
                target: "/root/.cache/pip".into(),
                read_only: false,
                mode: Some(0o755),
            }],
        };
        let encoded = encode_line(&exec);
        assert_eq!(decode_line::<AgentRequest>(&encoded).unwrap(), exec);

        let get = Request::GetArchive { id: "web".into(), path: "/etc/nginx".into() };
        assert_eq!(decode_line::<Request>(&encode_line(&get)).unwrap(), get);
        let put = Request::PutArchive { id: "web".into(), path: "/app".into(), size: 1024 };
        assert_eq!(decode_line::<Request>(&encode_line(&put)).unwrap(), put);
        let arch = Response::Archive { is_dir: true, name: "nginx".into() };
        assert_eq!(decode_line::<Response>(&encode_line(&arch)).unwrap(), arch);
        let copied = Response::Copied { files: 4 };
        assert_eq!(decode_line::<Response>(&encode_line(&copied)).unwrap(), copied);
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

    #[test]
    fn set_hosts_and_prepared_dns_compat() {
        let req = AgentRequest::SetHosts {
            generation: 3,
            hosts: vec![HostEntry { name: "redis".into(), id: "a1b2c3d4e5f6".into(), ip: "192.168.64.5".into() }],
        };
        let line = encode_line(&req);
        assert_eq!(decode_line::<AgentRequest>(&line).unwrap(), req);

        // Older agents omit `dns`; it defaults to false.
        let old: AgentEvent = decode_line(br#"{"event":"prepared","container":"abc","files":10,"ms":50,"ip":"192.168.64.2"}"#).unwrap();
        assert_eq!(
            old,
            AgentEvent::Prepared { container: "abc".into(), files: 10, ms: 50, ip: Some("192.168.64.2".into()), reused: false, dns: false }
        );
    }

    #[test]
    fn volumes_and_network_round_trip() {
        assert_eq!(String::from_utf8(encode_line(&Request::ListVolumes)).unwrap(), "{\"method\":\"list_volumes\"}\n");
        assert_eq!(
            String::from_utf8(encode_line(&Request::CreateVolume { name: "pgdata".into() })).unwrap(),
            "{\"method\":\"create_volume\",\"params\":{\"name\":\"pgdata\"}}\n"
        );
        assert_eq!(
            String::from_utf8(encode_line(&Request::RemoveVolume { name: "pgdata".into() })).unwrap(),
            "{\"method\":\"remove_volume\",\"params\":{\"name\":\"pgdata\"}}\n"
        );
        assert_eq!(String::from_utf8(encode_line(&Request::NetworkInfo)).unwrap(), "{\"method\":\"network_info\"}\n");

        let vol = VolumeSummary {
            name: "pgdata".into(),
            path: "/Users/me/Library/Application Support/Graviton/volumes/pgdata".into(),
            size_bytes: 4096,
            file_count: 12,
            created_at: 1_700_000_000,
            used_by: vec![VolumeMountRef {
                container_id: "abc123def456".into(),
                container_name: "postgres".into(),
                target: "/var/lib/postgresql/data".into(),
                read_only: false,
                running: true,
            }],
        };
        assert_eq!(decode_line::<Response>(&encode_line(&Response::Volumes(vec![vol.clone()]))).unwrap(), Response::Volumes(vec![vol.clone()]));
        assert_eq!(decode_line::<Response>(&encode_line(&Response::Volume(vol))).unwrap(), decode_line::<Response>(&encode_line(&Response::Volume(VolumeSummary {
            name: "pgdata".into(),
            path: "/Users/me/Library/Application Support/Graviton/volumes/pgdata".into(),
            size_bytes: 4096,
            file_count: 12,
            created_at: 1_700_000_000,
            used_by: vec![VolumeMountRef {
                container_id: "abc123def456".into(),
                container_name: "postgres".into(),
                target: "/var/lib/postgresql/data".into(),
                read_only: false,
                running: true,
            }],
        }))).unwrap());
        assert_eq!(
            decode_line::<Response>(&encode_line(&Response::VolumeRemoved { name: "pgdata".into(), freed_bytes: 4096 })).unwrap(),
            Response::VolumeRemoved { name: "pgdata".into(), freed_bytes: 4096 }
        );
        let net = NetworkInfo {
            name: "graviton".into(),
            driver: "L2 switch + NAT".into(),
            subnet: "192.168.64.0/24".into(),
            gateway: "192.168.64.1".into(),
            dns_server: "127.0.0.11:53".into(),
            dns_domain: "graviton".into(),
            connected_ports: 2,
            learned_macs: 2,
            frames_forwarded: 128,
            bytes_forwarded: 65536,
            endpoints: vec![NetworkEndpoint {
                container_id: "abc123def456".into(),
                container_name: "postgres".into(),
                image: "postgres:17-alpine".into(),
                status: ContainerStatus::Running,
                ip_address: Some("192.168.64.2".into()),
                dns_names: vec!["postgres".into(), "postgres.graviton".into(), "abc123def456".into()],
                dns_enabled: true,
                ports: vec![PortMapping { host_ip: None, host_port: 5432, container_port: 5432, protocol: PortProtocol::Tcp }],
            }],
        };
        assert_eq!(decode_line::<Response>(&encode_line(&Response::Network(net.clone()))).unwrap(), Response::Network(net));
    }
}
