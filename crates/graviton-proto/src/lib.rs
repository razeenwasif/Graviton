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

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub mod build;
pub mod caps;
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

    /// Parse `[ip:]host:container[/tcp|udp]` or `port[/tcp|udp]` into a [`PortMapping`].
    /// IPv6 bind addresses use brackets, e.g. `[::]:8080:80` or `[::1]:5353:53/udp`.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (ports, proto_raw) = spec.rsplit_once('/').unwrap_or((spec, "tcp"));
        let protocol = match proto_raw.trim().to_ascii_lowercase().as_str() {
            "tcp" => PortProtocol::Tcp,
            "udp" => PortProtocol::Udp,
            other => return Err(format!("`{spec}`: unknown protocol `{other}` (use tcp or udp)")),
        };
        let bad = || format!("`{spec}` isn't a valid port mapping (try 8080:80, 0.0.0.0:8080:80 or 5353:53/udp)");
        let parse_num = |s: &str| s.trim().parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(bad);

        let ports = ports.trim();
        let (host_ip, host_port, container_port) = if let Some(rest) = ports.strip_prefix('[') {
            let (ip_str, tail) = rest.split_once("]:").ok_or_else(bad)?;
            let ip: std::net::Ipv6Addr =
                ip_str.parse().map_err(|_| format!("`{spec}`: `{ip_str}` isn't a valid IPv6 address"))?;
            let (host, container) = tail.split_once(':').ok_or_else(bad)?;
            (Some(ip.to_string()), parse_num(host)?, parse_num(container)?)
        } else {
            match ports.split(':').collect::<Vec<_>>()[..] {
                [one] => (None, parse_num(one)?, parse_num(one)?),
                [host, container] => (None, parse_num(host)?, parse_num(container)?),
                [ip_str, host, container] => {
                    let ip: std::net::Ipv4Addr =
                        ip_str.parse().map_err(|_| format!("`{spec}`: `{ip_str}` isn't a valid IPv4 address"))?;
                    let host_ip = (ip != std::net::Ipv4Addr::LOCALHOST).then(|| ip.to_string());
                    (host_ip, parse_num(host)?, parse_num(container)?)
                }
                _ => return Err(bad()),
            }
        };
        Ok(Self { host_ip, host_port, container_port, protocol })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

/// An ephemeral in-memory tmpfs mount inside a container (`--tmpfs /path[:opts]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TmpfsMount {
    pub target: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub options: String,
}

/// Parse a CLI/Compose tmpfs spec (`/path` or `/path:opt1,opt2`).
pub fn parse_tmpfs_spec(raw: &str) -> Result<TmpfsMount, String> {
    let trimmed = raw.trim();
    let (target_raw, options_raw) = match trimmed.split_once(':') {
        Some((t, o)) => (t.trim(), o.trim()),
        None => (trimmed, ""),
    };
    if target_raw.is_empty() || !target_raw.starts_with('/') {
        return Err(format!("tmpfs target `{raw}` must be an absolute path inside the container"));
    }
    if target_raw == "/" {
        return Err("tmpfs target cannot be `/`".into());
    }
    if target_raw.contains('\0') || options_raw.contains('\0') {
        return Err("tmpfs spec cannot contain NUL bytes".into());
    }
    for comp in std::path::Path::new(target_raw).components() {
        if matches!(comp, std::path::Component::ParentDir | std::path::Component::Prefix(_)) {
            return Err(format!("tmpfs target `{target_raw}` cannot contain `..`"));
        }
    }
    let cleaned = target_raw.trim_end_matches('/').to_string();
    if cleaned.is_empty() {
        return Err("tmpfs target cannot be `/`".into());
    }
    Ok(TmpfsMount { target: cleaned, options: options_raw.to_string() })
}

/// Parse Docker-style tmpfs mount options into `(linux_ms_flags, kernel_data_string)`.
///
/// Defaults match Docker: `MS_NOSUID | MS_NODEV | MS_NOEXEC`, plus `mode=1777` unless a `mode=`
/// option is explicitly given.
pub fn parse_tmpfs_options(raw: &str) -> (u64, String) {
    const MS_RDONLY: u64 = 1;
    const MS_NOSUID: u64 = 2;
    const MS_NODEV: u64 = 4;
    const MS_NOEXEC: u64 = 8;
    const MS_SYNCHRONOUS: u64 = 16;
    const MS_DIRSYNC: u64 = 128;
    const MS_NOATIME: u64 = 1024;
    const MS_RELATIME: u64 = 1 << 21;
    const MS_STRICTATIME: u64 = 1 << 24;

    let mut flags = MS_NOSUID | MS_NODEV | MS_NOEXEC;
    let mut data = Vec::new();
    let mut has_mode = false;
    for tok in raw.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        match tok {
            "ro" => flags |= MS_RDONLY,
            "rw" => flags &= !MS_RDONLY,
            "noexec" => flags |= MS_NOEXEC,
            "exec" => flags &= !MS_NOEXEC,
            "nosuid" => flags |= MS_NOSUID,
            "suid" => flags &= !MS_NOSUID,
            "nodev" => flags |= MS_NODEV,
            "dev" => flags &= !MS_NODEV,
            "noatime" => flags |= MS_NOATIME,
            "relatime" => flags |= MS_RELATIME,
            "strictatime" => flags |= MS_STRICTATIME,
            "sync" => flags |= MS_SYNCHRONOUS,
            "dirsync" => flags |= MS_DIRSYNC,
            other => {
                if other.starts_with("mode=") {
                    has_mode = true;
                }
                data.push(other.to_string());
            }
        }
    }
    if !has_mode {
        data.push("mode=1777".to_string());
    }
    (flags, data.join(","))
}

/// Default `/dev/shm` tmpfs size (`64 MiB`), matching Docker.
pub const DEFAULT_SHM_SIZE_BYTES: u64 = 64 * 1024 * 1024;

/// Sentinel value representing `RLIM_INFINITY` in serialized [`UlimitSpec`] values.
pub const ULIMIT_UNLIMITED: i64 = -1;

/// One POSIX resource limit (`--ulimit name=soft[:hard]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UlimitSpec {
    pub name: String,
    pub soft: i64,
    pub hard: i64,
}

const VALID_ULIMIT_NAMES: &[&str] = &[
    "as", "core", "cpu", "data", "fsize", "locks", "memlock", "msgqueue", "nice", "nofile", "nproc", "rss", "rtprio",
    "rttime", "sigpending", "stack",
];

/// Parse a human byte size such as `64m`, `128mb`, `1g`, or `67108864`.
pub fn parse_byte_size(raw: &str) -> Result<u64, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("byte size cannot be empty".into());
    }
    let split = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    if split == 0 {
        return Err(format!("invalid byte size `{raw}`"));
    }
    let (num_str, unit_raw) = trimmed.split_at(split);
    let n: u64 = num_str.parse().map_err(|_| format!("invalid byte size `{raw}`"))?;
    let unit = unit_raw.trim().to_ascii_lowercase();
    let mult: u64 = match unit.as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        _ => return Err(format!("unsupported size suffix `{unit_raw}` in `{raw}` (use b, k, m, or g)")),
    };
    let bytes = n.checked_mul(mult).ok_or_else(|| format!("byte size `{raw}` overflows"))?;
    if bytes == 0 {
        return Err("byte size must be greater than 0".into());
    }
    Ok(bytes)
}

fn parse_ulimit_val(raw: &str) -> Result<i64, String> {
    let t = raw.trim();
    if t.eq_ignore_ascii_case("unlimited") {
        return Ok(ULIMIT_UNLIMITED);
    }
    let v: i64 = t.parse().map_err(|_| format!("invalid ulimit value `{raw}`"))?;
    if v < ULIMIT_UNLIMITED {
        return Err(format!("ulimit value `{raw}` must be >= -1 or `unlimited`"));
    }
    Ok(v)
}

/// Normalize and validate a [`UlimitSpec`].
pub fn validate_ulimit(u: &UlimitSpec) -> Result<UlimitSpec, String> {
    let name = u.name.trim().to_ascii_lowercase();
    if !VALID_ULIMIT_NAMES.contains(&name.as_str()) {
        return Err(format!("unsupported ulimit name `{}`", u.name));
    }
    if u.soft < ULIMIT_UNLIMITED || u.hard < ULIMIT_UNLIMITED {
        return Err(format!("ulimit `{name}` values must be >= -1"));
    }
    if (u.soft == ULIMIT_UNLIMITED && u.hard != ULIMIT_UNLIMITED)
        || (u.soft != ULIMIT_UNLIMITED && u.hard != ULIMIT_UNLIMITED && u.soft > u.hard)
    {
        return Err(format!("ulimit `{name}` soft limit ({}) exceeds hard limit ({})", u.soft, u.hard));
    }
    Ok(UlimitSpec { name, soft: u.soft, hard: u.hard })
}

/// Parse a CLI `--ulimit name=soft[:hard]` string.
pub fn parse_ulimit_spec(raw: &str) -> Result<UlimitSpec, String> {
    let trimmed = raw.trim();
    let Some((name_raw, val_raw)) = trimmed.split_once('=') else {
        return Err(format!("invalid ulimit `{raw}`: expected NAME=SOFT[:HARD]"));
    };
    let (soft_raw, hard_raw) = match val_raw.split_once(':') {
        Some((s, h)) => (s, Some(h)),
        None => (val_raw, None),
    };
    let soft = parse_ulimit_val(soft_raw)?;
    let hard = match hard_raw {
        Some(h) => parse_ulimit_val(h)?,
        None => soft,
    };
    validate_ulimit(&UlimitSpec {
        name: name_raw.to_string(),
        soft,
        hard,
    })
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

/// Default shared container network name.
pub const DEFAULT_NETWORK_NAME: &str = "graviton";

/// A named container network (either the built-in default `graviton` bridge or a user-defined L2-isolated network).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserNetwork {
    pub id: String,
    pub name: String,
    pub driver: String,
    #[serde(default)]
    pub internal: bool,
    #[serde(default)]
    pub builtin: bool,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub containers: Vec<String>,
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
    #[serde(default)]
    pub networks: Vec<String>,
}

/// Live view of Graviton's container networks, switch, and embedded DNS.
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
    #[serde(default)]
    pub networks: Vec<UserNetwork>,
}

/// Default seconds between `HEALTHCHECK` probes when omitted or `0`.
pub const DEFAULT_HEALTH_INTERVAL_SECS: u64 = 30;
/// Default seconds before an individual `HEALTHCHECK` probe times out when omitted or `0`.
pub const DEFAULT_HEALTH_TIMEOUT_SECS: u64 = 30;
/// Default consecutive failed probes before becoming `unhealthy` when omitted or `0`.
pub const DEFAULT_HEALTH_RETRIES: u32 = 3;

fn default_health_interval_secs() -> u64 {
    DEFAULT_HEALTH_INTERVAL_SECS
}

fn default_health_timeout_secs() -> u64 {
    DEFAULT_HEALTH_TIMEOUT_SECS
}

fn default_health_retries() -> u32 {
    DEFAULT_HEALTH_RETRIES
}

/// Container health state reported while a `HEALTHCHECK` is active.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    #[default]
    Starting,
    Healthy,
    Unhealthy,
}

impl HealthStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            HealthStatus::Starting => "starting",
            HealthStatus::Healthy => "healthy",
            HealthStatus::Unhealthy => "unhealthy",
        }
    }
}

impl std::fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Container `HEALTHCHECK` configuration shared across Dockerfiles, Compose, CLI flags and the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Healthcheck {
    /// Probe command (`["NONE"]`, `["CMD", prog, args...]`, or `["CMD-SHELL", script]`).
    #[serde(default)]
    pub test: Vec<String>,
    /// Seconds between probes (`0` selects the 30s default).
    #[serde(default = "default_health_interval_secs")]
    pub interval_secs: u64,
    /// Seconds before an individual probe times out (`0` selects the 30s default).
    #[serde(default = "default_health_timeout_secs")]
    pub timeout_secs: u64,
    /// Startup grace period in seconds while failing probes do not count toward `retries`.
    #[serde(default)]
    pub start_period_secs: u64,
    /// Consecutive failures needed to transition to `unhealthy` (`0` selects the default 3).
    #[serde(default = "default_health_retries")]
    pub retries: u32,
}

impl Default for Healthcheck {
    fn default() -> Self {
        Self {
            test: Vec::new(),
            interval_secs: DEFAULT_HEALTH_INTERVAL_SECS,
            timeout_secs: DEFAULT_HEALTH_TIMEOUT_SECS,
            start_period_secs: 0,
            retries: DEFAULT_HEALTH_RETRIES,
        }
    }
}

impl Healthcheck {
    /// Sentinel value (`test: ["NONE"]`) that disables an image's built-in `HEALTHCHECK`.
    pub fn disabled() -> Self {
        Self {
            test: vec!["NONE".into()],
            interval_secs: 0,
            timeout_secs: 0,
            start_period_secs: 0,
            retries: 0,
        }
    }

    pub fn is_disabled(&self) -> bool {
        self.test.first().is_some_and(|s| s.eq_ignore_ascii_case("NONE"))
    }

    /// Fill zeroed timing/retry fields with standard Docker defaults (`30s / 30s / 3`).
    pub fn normalized(&self) -> Self {
        Self {
            test: self.test.clone(),
            interval_secs: if self.interval_secs == 0 { DEFAULT_HEALTH_INTERVAL_SECS } else { self.interval_secs },
            timeout_secs: if self.timeout_secs == 0 { DEFAULT_HEALTH_TIMEOUT_SECS } else { self.timeout_secs },
            start_period_secs: self.start_period_secs,
            retries: if self.retries == 0 { DEFAULT_HEALTH_RETRIES } else { self.retries },
        }
    }

    /// Resolve `test` into the container `argv` executed by the probe runner.
    pub fn exec_command(&self) -> Option<Vec<String>> {
        if self.test.is_empty() || self.is_disabled() {
            return None;
        }
        match self.test[0].as_str() {
            "CMD" => Some(self.test[1..].to_vec()).filter(|v| !v.is_empty()),
            "CMD-SHELL" => self.test.get(1).map(|cmd| vec!["/bin/sh".into(), "-c".into(), cmd.clone()]),
            _ => Some(self.test.clone()),
        }
    }
}

/// Output of one completed `HEALTHCHECK` probe invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthLog {
    /// Unix seconds when the probe started.
    pub start: u64,
    /// Unix seconds when the probe completed.
    pub end: u64,
    pub exit_code: i32,
    #[serde(default)]
    pub output: String,
}

/// Live healthcheck status and bounded history of the most recent probes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthState {
    pub status: HealthStatus,
    #[serde(default)]
    pub failing_streak: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub log: Vec<HealthLog>,
}

/// Parse a Go/Docker-style duration string (e.g. `500ms`, `10s`, `1m30s`, `2m`, `1h`, or bare
/// non-negative seconds like `15`) into integer seconds, rounding positive sub-second durations
/// up to `1s` while preserving explicit `0`.
pub fn parse_duration_secs(raw: &str) -> Result<u64, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("duration cannot be empty".into());
    }
    if s.starts_with('-') {
        return Err(format!("duration `{raw}` cannot be negative"));
    }
    if s.chars().all(|c| c.is_ascii_digit() || c == '.') {
        let secs: f64 = s.parse().map_err(|_| format!("invalid duration `{raw}`"))?;
        if !secs.is_finite() || secs < 0.0 {
            return Err(format!("invalid duration `{raw}`"));
        }
        if secs == 0.0 {
            return Ok(0);
        }
        return Ok((secs * 1000.0).ceil() as u64).map(|ms| ms.div_ceil(1000));
    }

    let bytes = s.as_bytes();
    let mut i = 0usize;
    let mut total_ms: u128 = 0;
    while i < bytes.len() {
        let num_start = i;
        let mut saw_digit = false;
        let mut saw_dot = false;
        while i < bytes.len() {
            let b = bytes[i];
            if b.is_ascii_digit() {
                saw_digit = true;
                i += 1;
            } else if b == b'.' && !saw_dot {
                saw_dot = true;
                i += 1;
            } else {
                break;
            }
        }
        if !saw_digit {
            return Err(format!("invalid duration `{raw}`"));
        }
        let num: f64 = s[num_start..i].parse().map_err(|_| format!("invalid duration `{raw}`"))?;
        let unit_start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        let unit = &s[unit_start..i];
        let mult_ms: f64 = match unit {
            "ms" => 1.0,
            "s" => 1000.0,
            "m" => 60_000.0,
            "h" => 3_600_000.0,
            "" => return Err(format!("missing duration unit in `{raw}` (use ms, s, m, or h)")),
            other => return Err(format!("unsupported duration unit `{other}` in `{raw}`")),
        };
        let ms = num * mult_ms;
        if !ms.is_finite() || ms < 0.0 {
            return Err(format!("invalid duration `{raw}`"));
        }
        total_ms = total_ms.saturating_add(ms.round() as u128);
    }
    if total_ms == 0 {
        return Ok(0);
    }
    Ok(u64::try_from(total_ms.div_ceil(1000)).unwrap_or(u64::MAX))
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
    /// Compose stack/project name when created by `grav compose`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Compose service name within `project`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Current healthcheck status, when a healthcheck is configured and the container is running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthStatus>,
    /// Container ID this container was forked from (`grav fork`), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
}

/// Parameters for `grav fork` (`Request::Fork`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkSpec {
    /// Source container ID, prefix, or name.
    pub source: String,
    /// Optional explicit names for each fork; when omitted, `<source>-fork-<n>` is generated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    /// Number of forks to create (defaults to 1 or `names.len()`).
    #[serde(default = "default_fork_count")]
    pub count: u32,
    /// Start forked containers immediately (`true` unless `--no-start` / paused requested).
    #[serde(default = "default_true")]
    pub start: bool,
    /// Automatically remove forked containers when they stop (`--rm`).
    #[serde(default)]
    pub auto_remove: bool,
    /// Override published ports (only allowed when creating 1 fork).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortMapping>,
    /// Override or append environment variables (`KEY=VAL`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    /// Override command executed inside the fork(s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
}

fn default_fork_count() -> u32 {
    1
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffEntry {
    pub kind: DiffKind,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
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

/// Progress reported while pushing an image (`grav push IMAGE`), streamed before `Response::ImagePushed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum ImagePushProgress {
    Started { blobs: usize, total_bytes: u64 },
    Blob {
        index: usize,
        total_blobs: usize,
        digest: String,
        done: u64,
        total: u64,
        skipped: bool,
    },
    Manifest { digest: String, size: u64 },
}

/// Summary of a completed `grav push IMAGE` upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePushResult {
    pub reference: String,
    pub digest: String,
    pub size_bytes: u64,
    pub blobs_pushed: u32,
    pub blobs_skipped: u32,
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
    /// Target platform when pulling the image (e.g. `linux/amd64` or `linux/arm64`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// Replaces the image's Entrypoint (and clears image Cmd unless `command` is also given).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    /// Overrides the image's WorkingDir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// Overrides the image's User (`uid[:gid]` or `name[:group]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Overrides the container hostname (defaults to the container name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Compose project/stack name (for grouping and project-scoped service DNS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Compose service name (automatically registered as a project-scoped DNS alias).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Arbitrary key-value metadata (`grav run -l K=V`, Compose `labels:`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Additional project-scoped DNS aliases (Compose `networks.<net>.aliases`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dns_aliases: Vec<String>,
    /// Overrides or disables (`test: ["NONE"]`) the image's `HEALTHCHECK`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<Healthcheck>,
    /// Mount the container's root filesystem read-only (`--read-only`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// Linux capabilities added to the container (`--cap-add`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cap_add: Vec<String>,
    /// Linux capabilities dropped from the container (`--cap-drop`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cap_drop: Vec<String>,
    /// Ephemeral tmpfs mounts (`--tmpfs`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tmpfs: Vec<TmpfsMount>,
    /// Run a dedicated `docker-init` zombie reaper as container PID 1 (`--init`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub init: bool,
    /// Named networks to attach to (empty defaults to `["graviton"]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub networks: Vec<String>,
    /// Size of `/dev/shm` tmpfs mount in bytes (`--shm-size`). `None` defaults to `DEFAULT_SHM_SIZE_BYTES`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shm_size_bytes: Option<u64>,
    /// POSIX resource limits applied inside the container (`--ulimit`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ulimits: Vec<UlimitSpec>,
    /// Host-enforced egress firewall policy (`Unrestricted` by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<EgressPolicy>,
    /// Host-enforced latency, jitter, loss, and bandwidth chaos profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chaos: Option<ChaosProfile>,
}

/// Host-enforced container egress firewall mode (zero-bypass: restricted modes omit the NAT NIC).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum EgressPolicy {
    /// Full Virtualization.framework NAT + switch access (default).
    #[default]
    Unrestricted,
    /// Air-gapped: no external NAT NIC and host denies all external connections.
    Offline,
    /// Only allow listed domains (`example.com`, `*.pypi.org`) and CIDRs (`10.0.0.0/8`).
    Allowlist {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        hosts: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        cidrs: Vec<String>,
    },
}

impl EgressPolicy {
    pub fn is_restricted(&self) -> bool {
        !matches!(self, Self::Unrestricted)
    }

    /// Parse `--egress`: `unrestricted`, `offline`, or `allow:host1,*.host2,1.2.3.0/24`.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        let lower = trimmed.to_ascii_lowercase();
        if matches!(lower.as_str(), "unrestricted" | "open" | "default") {
            return Ok(Self::Unrestricted);
        }
        if matches!(lower.as_str(), "offline" | "none" | "block" | "airgap") {
            return Ok(Self::Offline);
        }
        let targets_raw = trimmed
            .strip_prefix("allow:")
            .or_else(|| trimmed.strip_prefix("allowlist:"))
            .ok_or_else(|| format!("invalid egress policy `{raw}` (use unrestricted, offline, or allow:host1,host2)"))?;
        let mut hosts = Vec::new();
        let mut cidrs = Vec::new();
        for part in targets_raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if is_valid_cidr_or_ip(part) {
                if !cidrs.iter().any(|c| c == part) {
                    cidrs.push(part.to_string());
                }
            } else {
                let norm = part.trim_end_matches('.').to_ascii_lowercase();
                if norm.is_empty() || norm.chars().any(char::is_whitespace) || norm.contains('/') {
                    return Err(format!("invalid allowlist entry `{part}`"));
                }
                if !hosts.iter().any(|h| h == &norm) {
                    hosts.push(norm);
                }
            }
        }
        if hosts.is_empty() && cidrs.is_empty() {
            return Err("allowlist requires at least one host or CIDR target".into());
        }
        Ok(Self::Allowlist { hosts, cidrs })
    }

    pub fn allows_host(&self, host: &str) -> bool {
        match self {
            Self::Unrestricted => true,
            Self::Offline => false,
            Self::Allowlist { hosts, .. } => {
                let target = host.trim().trim_end_matches('.').to_ascii_lowercase();
                hosts.iter().any(|rule| {
                    if let Some(suffix) = rule.strip_prefix("*.") {
                        target == suffix || target.ends_with(&format!(".{suffix}"))
                    } else {
                        target == *rule
                    }
                })
            }
        }
    }

    pub fn allows_ip(&self, ip: std::net::IpAddr) -> bool {
        match self {
            Self::Unrestricted => true,
            Self::Offline => false,
            Self::Allowlist { cidrs, .. } => cidrs.iter().any(|c| cidr_contains(c, ip)),
        }
    }
}

fn is_valid_cidr_or_ip(raw: &str) -> bool {
    if raw.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let Some((addr, prefix)) = raw.split_once('/') else {
        return false;
    };
    let Ok(prefix) = prefix.parse::<u8>() else {
        return false;
    };
    match addr.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(_)) => prefix <= 32,
        Ok(std::net::IpAddr::V6(_)) => prefix <= 128,
        Err(_) => false,
    }
}

pub fn cidr_contains(cidr: &str, ip: std::net::IpAddr) -> bool {
    let (addr_str, prefix_opt) = match cidr.split_once('/') {
        Some((a, p)) => (a, p.parse::<u8>().ok()),
        None => (cidr, None),
    };
    let Ok(net_ip) = addr_str.parse::<std::net::IpAddr>() else {
        return false;
    };
    match (net_ip, ip) {
        (std::net::IpAddr::V4(net), std::net::IpAddr::V4(target)) => {
            let prefix = prefix_opt.unwrap_or(32);
            if prefix > 32 {
                return false;
            }
            let mask = if prefix == 0 { 0u32 } else { u32::MAX << (32 - prefix) };
            (u32::from(net) & mask) == (u32::from(target) & mask)
        }
        (std::net::IpAddr::V6(net), std::net::IpAddr::V6(target)) => {
            let prefix = prefix_opt.unwrap_or(128);
            if prefix > 128 {
                return false;
            }
            let mask = if prefix == 0 { 0u128 } else { u128::MAX << (128 - prefix) };
            (u128::from(net) & mask) == (u128::from(target) & mask)
        }
        _ => false,
    }
}

/// Host-enforced per-container fault injection (`latency`, `jitter`, `loss`, bandwidth cap).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChaosProfile {
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub latency_ms: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub jitter_ms: u32,
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub loss_percent: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_kbps: Option<u32>,
}

impl ChaosProfile {
    pub fn is_active(&self) -> bool {
        self.latency_ms > 0 || self.jitter_ms > 0 || self.loss_percent > 0 || self.bandwidth_kbps.is_some()
    }
}

/// Active firewall + chaos policy for a container.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerNetworkPolicy {
    #[serde(default)]
    pub egress: EgressPolicy,
    #[serde(default)]
    pub chaos: ChaosProfile,
    /// True when the VM is booted with `NetworkMode::None` (zero host NAT interface).
    #[serde(default)]
    pub hardware_isolated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum XRayEventKind {
    Dns,
    Net,
    File,
    Proc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XRayEvent {
    #[serde(default)]
    pub seq: u64,
    pub timestamp_ms: u64,
    pub kind: XRayEventKind,
    pub action: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default = "default_true")]
    pub allowed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
}

fn is_zero_u8(n: &u8) -> bool {
    *n == 0
}

fn is_zero_u16(n: &u16) -> bool {
    *n == 0
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

fn is_zero_u64(n: &u64) -> bool {
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
    Paused,
    Unpaused,
    Healthy,
    Unhealthy,
    Pulled,
    Pushed,
    Built,
    BuildFailed,
    Removed,
    Forked,
}

impl EventKind {
    /// Worth interrupting someone for (a push alert).
    pub fn is_alert(self) -> bool {
        matches!(self, EventKind::Failed | EventKind::Unhealthy | EventKind::Built | EventKind::BuildFailed)
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
    /// Target platform for base images and output config (e.g. `linux/amd64`, `linux/arm64`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// Extra labels added to the built image's config (`docker build --label`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

/// Parameters for `Request::Commit` (`grav commit CONTAINER IMAGE`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitSpec {
    pub container: String,
    /// Target image reference (`repo[:tag]`); empty defaults to `committed:<short_id>`.
    #[serde(default)]
    pub reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Dockerfile config instructions (`ENV`, `CMD`, `ENTRYPOINT`, `WORKDIR`, `USER`, `EXPOSE`, `LABEL`, `VOLUME`, `STOPSIGNAL`, `HEALTHCHECK`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<String>,
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
    /// Cached VM memory snapshots (slots) and their total disk usage.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub snapshots: u32,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub snapshots_bytes: u64,
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
    /// Cached VM snapshot slots deleted.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub snapshots: u32,
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
    /// Working directory override inside the container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// User override (`uid[:gid]` or `name[:group]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
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

/// Live summary row for one container streamed by `Request::StatsStream` (`grav stats`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContainerStatRow {
    pub id: String,
    pub name: String,
    pub status: ContainerStatus,
    pub cpus: u32,
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub memory_limit_bytes: u64,
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
    pub pids: u32,
    pub timestamp_ms: u64,
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
    /// Image architecture (e.g. `arm64`, `amd64`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
    /// True when running an x86_64 image through Rosetta for Linux.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rosetta: bool,
    /// Compose project/stack name, if part of a stack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Compose service name, if part of a stack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Container labels.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Configured container `HEALTHCHECK`, if enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<Healthcheck>,
    /// Live healthcheck status and bounded probe logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthState>,
    /// True when the container's root filesystem is mounted read-only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// Linux capabilities added to the container (`--cap-add`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cap_add: Vec<String>,
    /// Linux capabilities dropped from the container (`--cap-drop`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cap_drop: Vec<String>,
    /// Ephemeral tmpfs mounts (`--tmpfs`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tmpfs: Vec<TmpfsMount>,
    /// True when started with `--init`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub init: bool,
    /// Attached networks for this container.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub networks: Vec<String>,
    /// Size of `/dev/shm` tmpfs mount in bytes (`--shm-size`). `None` defaults to `DEFAULT_SHM_SIZE_BYTES`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shm_size_bytes: Option<u64>,
    /// POSIX resource limits applied inside the container (`--ulimit`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ulimits: Vec<UlimitSpec>,
    /// Active egress firewall & chaos policy for this container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_policy: Option<ContainerNetworkPolicy>,
    /// Whether live Hypervisor X-Ray collection is currently enabled.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub xray_enabled: bool,
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
    /// Whether Apple's Rosetta for Linux is available for running `linux/amd64` images.
    #[serde(default)]
    pub rosetta: bool,
    /// Real host memory currently in use (macOS Activity Monitor definition: app + wired + compressed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_memory_used_bytes: Option<u64>,
}

/// Cached VM memory snapshot pool for one VM shape (`grav snapshot status`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotGroup {
    pub cpus: u32,
    pub memory_mib: u64,
    pub rosetta: bool,
    pub slots: u32,
    pub in_use: u32,
    pub size_bytes: u64,
}

/// Summary of the daemon's VM snapshot pool (`grav snapshot status|warm|clear`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub enabled: bool,
    pub total_slots: u32,
    pub in_use: u32,
    pub total_bytes: u64,
    pub max_slots_per_shape: u32,
    pub groups: Vec<SnapshotGroup>,
    /// Populated when snapshot restore/warm is paused (e.g., host macOS screen locked).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_reason: Option<String>,
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
    /// List configured container networks (built-in and user-defined).
    ListNetworks,
    /// Create a user-defined container network.
    CreateNetwork {
        name: String,
        #[serde(default)]
        internal: bool,
        #[serde(default)]
        labels: BTreeMap<String, String>,
    },
    /// Remove an unused user-defined network.
    RemoveNetwork { name: String },
    /// Attach a container to a network (live on running containers).
    ConnectNetwork { network: String, container: String },
    /// Detach a container from a network (live on running containers).
    DisconnectNetwork { network: String, container: String },
    /// `id` may be a container id, id prefix or name.
    /// With `follow: true`, streams `Response::Log` lines and finishes with `Response::LogEnd`.
    Logs {
        id: String,
        tail: Option<usize>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        follow: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until_ms: Option<u64>,
    },
    Start { id: String },
    Stop { id: String },
    Restart { id: String },
    Pause { id: String },
    Unpause { id: String },
    /// Pull an image from its registry; answers `Image` when done.
    /// With `progress: true`, streams `PullProgress` lines before the final `Image`.
    Pull {
        reference: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        progress: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        platform: Option<String>,
    },
    /// Give an existing local image an additional name/tag without copying layers.
    TagImage { source: String, target: String },
    /// Push a local image to its registry; answers `ImagePushed` (preceded by
    /// `ImagePushProgress` lines when `progress: true`).
    PushImage {
        reference: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        progress: bool,
    },
    /// Export one or more local images into a tar archive stream (`grav save`).
    /// Answers `ImageArchive` followed by raw tar bytes until EOF.
    SaveImage { references: Vec<String> },
    /// Load one or more images from `size` tar/tar.gz bytes following this request line (`grav load`).
    LoadImage { size: u64 },
    /// Snapshot a container's filesystem + config changes into a local image (`grav commit`).
    Commit(CommitSpec),
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
    Run(Box<RunSpec>),
    /// Fork a container's disk (and flushed live state if running) into one or more new containers.
    Fork(Box<ForkSpec>),
    /// Inspect overlay filesystem changes (`Added`, `Modified`, `Deleted`) inside a container.
    Diff { id: String },
    /// Inspect active egress firewall & chaos policy on a container.
    GetNetworkPolicy { id: String },
    /// Update active egress firewall and/or chaos policy on a container.
    SetNetworkPolicy {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        egress: Option<EgressPolicy>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chaos: Option<ChaosProfile>,
    },
    /// Enable or disable Hypervisor X-Ray event collection on a container.
    SetXRay { id: String, enabled: bool },
    /// Fetch buffered X-Ray events, or stream live events (`follow: true`).
    XRay {
        id: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        follow: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    /// Remove a stopped container.
    Remove { id: String },
    /// Where the disk space goes.
    DiskUsage,
    /// Remove an image no container uses, and the layers only it needed.
    RemoveImage { reference: String },
    /// Clear the build cache and unreferenced layers, and more if asked.
    Prune(PruneSpec),
    /// Inspect cached VM snapshots.
    SnapshotStatus,
    /// Pre-warm one or more VM snapshots for fast container restore.
    SnapshotWarm {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cpus: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        memory_mib: Option<u64>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        rosetta: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slots: Option<u32>,
    },
    /// Remove all idle cached VM snapshots from disk.
    SnapshotClear,
    /// Run a command in a running container. Without a TTY the daemon answers
    /// with `ExecOutput` lines and a final `ExecExited`; with one it answers
    /// `Attached` and the connection then carries terminal [`frame`]s.
    Exec(ExecSpec),
    /// Recent resource use and the processes of a running container.
    Stats { id: String },
    /// Snapshot or live stream (`follow: true`) of resource stats across containers (`grav stats`).
    StatsStream {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ids: Vec<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        all: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        follow: bool,
    },
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
    Networks(Vec<UserNetwork>),
    NetworkCreated(UserNetwork),
    NetworkRemoved { name: String },
    NetworkUpdated { network: String, container: String, connected: bool },
    Logs(Vec<LogLine>),
    /// Single streamed line for `Request::Logs { follow: true, .. }`.
    Log(LogLine),
    /// Sent after `Response::Log` stream ends (container exited/removed or `--until` reached).
    LogEnd {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    Container(ContainerSummary),
    Forked { containers: Vec<ContainerSummary> },
    Diff { entries: Vec<DiffEntry> },
    NetworkPolicy(ContainerNetworkPolicy),
    XRayStatus { enabled: bool },
    XRayEvents(Vec<XRayEvent>),
    XRayEvent(XRayEvent),
    Image(ImageSummary),
    PullProgress(PullProgress),
    ImagePushProgress(ImagePushProgress),
    ImagePushed(ImagePushResult),
    LoggedIn { registry: String, username: String },
    LoggedOut { registry: String },
    Logins(Vec<RegistryLoginInfo>),
    Removed { id: String },
    DiskUsage(DiskUsage),
    /// `kept_bytes`: its layers the build cache still holds (`grav prune` frees them).
    ImageRemoved { reference: String, freed_bytes: u64, kept_bytes: u64 },
    Pruned(PruneReport),
    Snapshots(SnapshotInfo),
    BuildLog { line: String },
    ExecOutput { stream: LogStream, data: Vec<u8> },
    ExecExited { code: i32 },
    Attached,
    Stats(ContainerStats),
    StatsSnapshot(Vec<ContainerStatRow>),
    Files(DirectoryListing),
    File(FileContents),
    /// Sent before streaming raw tar bytes for `Request::GetArchive`.
    Archive { is_dir: bool, name: String },
    /// Sent before streaming raw tar bytes for `Request::SaveImage`.
    ImageArchive,
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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tmpfs: Vec<TmpfsMount>,
        #[serde(default)]
        hostname: Option<String>,
        /// Host Unix wall-clock time in seconds (used after VM snapshot restore so guest clock matches host UTC).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        now_secs: Option<u64>,
        /// When booting a forked container whose cloned ext4 still holds `/run/containers/<fork_from>`, rename it to `<container>`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fork_from: Option<String>,
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
        /// Keep non-TTY child stdin open as a pipe fed by subsequent `AgentRequest::Stdin` frames (empty data = EOF).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        stdin_pipe: bool,
        /// Ephemeral per-RUN mounts (cache, secret, bind, tmpfs), visible only inside this process's mount namespace.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mounts: Vec<ExecMount>,
        /// Mount the container root filesystem read-only after attaching volumes/tmpfs.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        read_only: bool,
        /// Linux capabilities added to the container (`--cap-add`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        cap_add: Vec<String>,
        /// Linux capabilities dropped from the container (`--cap-drop`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        cap_drop: Vec<String>,
        /// Label PID 1 as `docker-init` (`--init`).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        init: bool,
        /// Custom `/dev/shm` tmpfs size in bytes (`--shm-size`); `None` uses `DEFAULT_SHM_SIZE_BYTES` when initializing the container.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shm_size_bytes: Option<u64>,
        /// POSIX resource limits (`--ulimit`) applied in the child before privilege/capability drop.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ulimits: Vec<UlimitSpec>,
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
    /// Open a direct bidirectional TCP tunnel over vsock to `127.0.0.1:<port>` inside the guest VM.
    DialTcp { port: u16 },
    /// Sync guest wall clock (`CLOCK_REALTIME`) to host UTC seconds (e.g. after VM unpause).
    SetClock { now_secs: u64 },
    /// Flush dirty ext4 pages and journals on `/run/containers` before host APFS `clonefile`.
    SyncFs,
    /// Collect overlayfs upperdir diff entries (`Added`, `Modified`, `Deleted`) for `container`.
    Diff { container: String },
    /// Enable or disable guest X-Ray collection (file, process, net).
    SetXRay { container: String, enabled: bool },
    /// Stream batched `AgentEvent::XRayBatch` frames on this dedicated vsock connection.
    WatchXRay { container: String },
    /// Control plane connection between host `EgressGateway` and guest `egress_proxy` (`eth0`-free VMs).
    EgressBridge,
    /// Bidirectional byte tunnel paired with a waiting host dial in `egress_proxy`.
    EgressStream { id: u64 },
    /// Remove the guest's `eth0` default route on live downgrade from Unrestricted to Offline/Allowlist.
    DropDefaultRoute,
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
    /// Precedes raw TCP bytes for `AgentRequest::DialTcp`.
    Connected,
    /// A step finished that has nothing else to report (snapshot, mkdir, clock sync).
    Done,
    /// `copy` placed this many files.
    Copied { files: u64 },
    /// `commit`'s layer; `file` is `None` when nothing changed.
    Committed(LayerInfo),
    /// Reply to `AgentRequest::SyncFs`.
    Synced,
    /// Reply to `AgentRequest::Diff`.
    Diff { entries: Vec<DiffEntry> },
    /// Reply to `AgentRequest::SetXRay`.
    XRayStatus { enabled: bool },
    /// Streamed batch on `AgentRequest::WatchXRay`.
    XRayBatch { events: Vec<XRayEvent> },
    Error { message: String },
}

/// Control messages sent by guest `egress_proxy` to host `EgressGateway` over `AgentRequest::EgressBridge`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum EgressBridgeRequest {
    Dns { id: u64, name: String, qtype: u16 },
    Dial { id: u64, host: String, port: u16 },
}

/// Replies sent by host `EgressGateway` to guest `egress_proxy` over `AgentRequest::EgressBridge`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum EgressBridgeReply {
    DnsResult {
        id: u64,
        allowed: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ips: Vec<String>,
    },
    DialResult {
        id: u64,
        allowed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
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
            follow: false,
            since_ms: None,
            until_ms: None,
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
        assert_eq!(spec.project, None);
        assert!(spec.labels.is_empty());

        let compose_run = Request::Run(Box::new(RunSpec {
            image: "postgres:17-alpine".into(),
            name: Some("myapp-db-1".into()),
            command: None,
            env: vec!["POSTGRES_PASSWORD=dev".into()],
            cpus: Some(2),
            memory_mib: Some(1024),
            ports: vec!["5432:5432".into()],
            volumes: vec!["myapp_pgdata:/var/lib/postgresql/data".into()],
            restart: Some(RestartPolicy::Always),
            tty: false,
            rows: 0,
            cols: 0,
            auto_remove: false,
            platform: None,
            entrypoint: Some(vec!["docker-entrypoint.sh".into()]),
            working_dir: Some("/var/lib/postgresql".into()),
            user: Some("postgres".into()),
            hostname: Some("db".into()),
            project: Some("myapp".into()),
            service: Some("db".into()),
            labels: BTreeMap::from([("com.docker.compose.project".into(), "myapp".into())]),
            dns_aliases: vec!["database".into()],
            healthcheck: None,
            read_only: false,
            cap_add: vec![],
            cap_drop: vec![],
            tmpfs: vec![],
            init: false,
            networks: vec![],
            shm_size_bytes: None,
            ulimits: vec![],
            egress: None,
            chaos: None,
        }));
        assert_eq!(decode_line::<Request>(&encode_line(&compose_run)).unwrap(), compose_run);
        assert_eq!("always".parse::<RestartPolicy>().unwrap(), RestartPolicy::Always);
        assert_eq!("on-failure".parse::<RestartPolicy>().unwrap(), RestartPolicy::OnFailure);
        assert_eq!("no".parse::<RestartPolicy>().unwrap(), RestartPolicy::No);
        assert!("bogus".parse::<RestartPolicy>().is_err());
        let pull: Request = decode_line(br#"{"method":"pull","params":{"reference":"alpine"}}"#).unwrap();
        assert_eq!(pull, Request::Pull { reference: "alpine".into(), progress: false, platform: None });
        let pull_stream: Request = decode_line(br#"{"method":"pull","params":{"progress":true,"reference":"alpine"}}"#).unwrap();
        assert_eq!(pull_stream, Request::Pull { reference: "alpine".into(), progress: true, platform: None });
        let pull_amd64: Request = decode_line(br#"{"method":"pull","params":{"platform":"linux/amd64","reference":"alpine"}}"#).unwrap();
        assert_eq!(pull_amd64, Request::Pull { reference: "alpine".into(), progress: false, platform: Some("linux/amd64".into()) });
        let snap_req = Request::SnapshotWarm { cpus: Some(2), memory_mib: Some(1024), rosetta: true, slots: Some(2) };
        assert_eq!(decode_line::<Request>(&encode_line(&snap_req)).unwrap(), snap_req);
        let snap_resp = Response::Snapshots(SnapshotInfo {
            enabled: true,
            total_slots: 2,
            in_use: 1,
            total_bytes: 256 << 20,
            max_slots_per_shape: 8,
            groups: vec![SnapshotGroup { cpus: 2, memory_mib: 1024, rosetta: false, slots: 2, in_use: 1, size_bytes: 256 << 20 }],
            paused_reason: None,
        });
        let snap_json = encode_line(&snap_resp);
        assert!(!String::from_utf8_lossy(&snap_json).contains("paused_reason"));
        assert_eq!(decode_line::<Response>(&snap_json).unwrap(), snap_resp);

        let paused_snap_resp = Response::Snapshots(SnapshotInfo {
            enabled: true,
            total_slots: 1,
            in_use: 0,
            total_bytes: 13 << 20,
            max_slots_per_shape: 8,
            groups: vec![],
            paused_reason: Some("macOS console session is locked".into()),
        });
        assert_eq!(decode_line::<Response>(&encode_line(&paused_snap_resp)).unwrap(), paused_snap_resp);
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

        // Swift's BuildSpec omits `secrets` and `labels`; Rust defaults them to empty.
        let from_swift = br#"{"method":"build","params":{"build_args":["PY=3.12"],"context":"/Users/me/app","no_cache":true,"tag":"app:1"}}"#;
        let Request::Build(spec) = decode_line::<Request>(from_swift).unwrap() else { panic!("not build") };
        assert!(spec.secrets.is_empty());
        assert!(spec.labels.is_empty());

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
            stdin_pipe: false,
            mounts: vec![ExecMount {
                kind: ExecMountKind::Cache,
                source: "pip".into(),
                from_container: None,
                target: "/root/.cache/pip".into(),
                read_only: false,
                mode: Some(0o755),
            }],
            read_only: false,
            cap_add: vec![],
            cap_drop: vec![],
            init: false,
            shm_size_bytes: None,
            ulimits: vec![],
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
                networks: vec![DEFAULT_NETWORK_NAME.into()],
            }],
            networks: vec![UserNetwork {
                id: "graviton-bridge".into(),
                name: DEFAULT_NETWORK_NAME.into(),
                driver: "L2 switch + NAT".into(),
                internal: false,
                builtin: true,
                created_at: 1_700_000_000,
                labels: BTreeMap::new(),
                containers: vec!["postgres".into()],
            }],
        };
        assert_eq!(decode_line::<Response>(&encode_line(&Response::Network(net.clone()))).unwrap(), Response::Network(net));
    }

    #[test]
    fn engine_info_host_memory_round_trip_and_backcompat() {
        let legacy = br#"{"type":"engine","data":{"version":"0.1.0","protocol":1,"kernel":"Linux 6.12","arch":"arm64","cpus":10,"memory_bytes":17179869184,"memory_used_bytes":1073741824,"disk_bytes":0,"disk_used_bytes":1000,"rosetta":true}}"#;
        let Response::Engine(decoded) = decode_line::<Response>(legacy).unwrap() else {
            panic!("expected engine");
        };
        assert_eq!(decoded.host_memory_used_bytes, None);

        let with_host = EngineInfo {
            host_memory_used_bytes: Some(10_737_418_240),
            ..decoded
        };
        let encoded = encode_line(&Response::Engine(with_host.clone()));
        assert_eq!(decode_line::<Response>(&encoded).unwrap(), Response::Engine(with_host));
    }

    #[test]
    fn tag_and_push_image_round_trip() {
        let tag_req = Request::TagImage {
            source: "app:1.0".into(),
            target: "ghcr.io/me/app:1.0".into(),
        };
        assert_eq!(
            String::from_utf8(encode_line(&tag_req)).unwrap(),
            "{\"method\":\"tag_image\",\"params\":{\"source\":\"app:1.0\",\"target\":\"ghcr.io/me/app:1.0\"}}\n"
        );
        assert_eq!(decode_line::<Request>(&encode_line(&tag_req)).unwrap(), tag_req);

        let push_req = Request::PushImage {
            reference: "ghcr.io/me/app:1.0".into(),
            progress: true,
        };
        assert_eq!(decode_line::<Request>(&encode_line(&push_req)).unwrap(), push_req);

        // Older callers omitting `progress` default it to false.
        let minimal = br#"{"method":"push_image","params":{"reference":"ghcr.io/me/app:1.0"}}"#;
        assert_eq!(
            decode_line::<Request>(minimal).unwrap(),
            Request::PushImage { reference: "ghcr.io/me/app:1.0".into(), progress: false }
        );

        let prog = Response::ImagePushProgress(ImagePushProgress::Blob {
            index: 0,
            total_blobs: 2,
            digest: "sha256:abcd".into(),
            done: 512,
            total: 1024,
            skipped: false,
        });
        assert_eq!(decode_line::<Response>(&encode_line(&prog)).unwrap(), prog);

        let res = Response::ImagePushed(ImagePushResult {
            reference: "ghcr.io/me/app:1.0".into(),
            digest: "sha256:ef01".into(),
            size_bytes: 2048,
            blobs_pushed: 1,
            blobs_skipped: 1,
        });
        assert_eq!(decode_line::<Response>(&encode_line(&res)).unwrap(), res);
    }

    #[test]
    fn healthcheck_duration_and_event_round_trip() {
        assert_eq!(parse_duration_secs("30s").unwrap(), 30);
        assert_eq!(parse_duration_secs("1m30s").unwrap(), 90);
        assert_eq!(parse_duration_secs("1h2m3s").unwrap(), 3723);
        assert_eq!(parse_duration_secs("500ms").unwrap(), 1);
        assert_eq!(parse_duration_secs("1.5s").unwrap(), 2);
        assert_eq!(parse_duration_secs("0s").unwrap(), 0);
        assert_eq!(parse_duration_secs("0").unwrap(), 0);
        assert_eq!(parse_duration_secs("15").unwrap(), 15);
        for bad in ["", "-5s", "10x", "abc"] {
            assert!(parse_duration_secs(bad).is_err(), "expected err for `{bad}`");
        }

        let hc = Healthcheck {
            test: vec!["CMD-SHELL".into(), "curl -f http://localhost/".into()],
            interval_secs: 0,
            timeout_secs: 0,
            start_period_secs: 5,
            retries: 0,
        };
        assert!(!hc.is_disabled());
        let norm = hc.normalized();
        assert_eq!((norm.interval_secs, norm.timeout_secs, norm.start_period_secs, norm.retries), (30, 30, 5, 3));
        assert_eq!(
            norm.exec_command().as_deref(),
            Some(&["/bin/sh".to_string(), "-c".into(), "curl -f http://localhost/".into()][..])
        );

        let cmd_hc = Healthcheck {
            test: vec!["CMD".into(), "/bin/check".into(), "--fast".into()],
            ..Default::default()
        };
        assert_eq!(cmd_hc.exec_command().as_deref(), Some(&["/bin/check".to_string(), "--fast".into()][..]));

        let disabled = Healthcheck::disabled();
        assert!(disabled.is_disabled());
        assert_eq!(disabled.exec_command(), None);
        assert_eq!(Healthcheck::default().exec_command(), None);

        let state = HealthState {
            status: HealthStatus::Healthy,
            failing_streak: 0,
            log: vec![HealthLog { start: 10, end: 11, exit_code: 0, output: "ok".into() }],
        };
        let encoded_state = serde_json::to_vec(&state).unwrap();
        assert_eq!(serde_json::from_slice::<HealthState>(&encoded_state).unwrap(), state);

        assert!(!EventKind::Healthy.is_alert());
        assert!(EventKind::Unhealthy.is_alert());
        let healthy_ev = EngineEvent {
            seq: 1,
            timestamp_ms: 1000,
            kind: EventKind::Healthy,
            container_id: Some("abc".into()),
            container: Some("web".into()),
            image: Some("nginx".into()),
            message: "web is healthy".into(),
        };
        assert_eq!(
            decode_line::<Response>(&encode_line(&Response::Events(vec![healthy_ev.clone()]))).unwrap(),
            Response::Events(vec![healthy_ev])
        );

        // Legacy ContainerSummary and ContainerDetails without health fields still decode cleanly.
        let legacy_summary: ContainerSummary = serde_json::from_str(
            r#"{"id":"abc","name":"web","image":"nginx","status":"running","exit_code":null,"ports":[],"mounts":[],"ip_address":null,"cpu_percent":null,"memory_bytes":null,"memory_limit_bytes":null,"created_at":1,"started_at":2}"#,
        )
        .unwrap();
        assert_eq!(legacy_summary.health, None);

        let legacy_details: ContainerDetails = serde_json::from_str(
            r#"{"container":{"id":"abc","name":"web","image":"nginx","status":"running","exit_code":null,"ports":[],"mounts":[],"ip_address":null,"cpu_percent":null,"memory_bytes":null,"memory_limit_bytes":null,"created_at":1,"started_at":2},"image_reference":"nginx","image_digest":"sha256:aa","command":["nginx"],"env":[],"working_dir":"/","user":null,"hostname":"web","cpus":2,"memory_mib":512,"kernel":"6.12","vm_dir":"/tmp"}"#,
        )
        .unwrap();
        assert_eq!(legacy_details.healthcheck, None);
        assert_eq!(legacy_details.health, None);
    }

    #[test]
    fn pause_and_dial_tcp_round_trip() {
        let pause = Request::Pause { id: "web".into() };
        assert_eq!(String::from_utf8(encode_line(&pause)).unwrap(), "{\"method\":\"pause\",\"params\":{\"id\":\"web\"}}\n");
        assert_eq!(decode_line::<Request>(&encode_line(&pause)).unwrap(), pause);

        let unpause = Request::Unpause { id: "web".into() };
        assert_eq!(String::from_utf8(encode_line(&unpause)).unwrap(), "{\"method\":\"unpause\",\"params\":{\"id\":\"web\"}}\n");
        assert_eq!(decode_line::<Request>(&encode_line(&unpause)).unwrap(), unpause);

        assert!(!EventKind::Paused.is_alert());
        assert!(!EventKind::Unpaused.is_alert());

        let dial = AgentRequest::DialTcp { port: 18092 };
        assert_eq!(decode_line::<AgentRequest>(&encode_line(&dial)).unwrap(), dial);
        let clk = AgentRequest::SetClock { now_secs: 1_795_000_000 };
        assert_eq!(decode_line::<AgentRequest>(&encode_line(&clk)).unwrap(), clk);
        assert_eq!(decode_line::<AgentEvent>(&encode_line(&AgentEvent::Connected)).unwrap(), AgentEvent::Connected);
    }

    #[test]
    fn save_load_commit_round_trip() {
        let save = Request::SaveImage { references: vec!["alpine:3.22".into(), "app:1".into()] };
        assert_eq!(decode_line::<Request>(&encode_line(&save)).unwrap(), save);

        let load = Request::LoadImage { size: 42_000 };
        assert_eq!(decode_line::<Request>(&encode_line(&load)).unwrap(), load);

        let commit = Request::Commit(CommitSpec {
            container: "web".into(),
            reference: "myrepo/web:v2".into(),
            author: Some("Jane".into()),
            message: Some("snapshot".into()),
            changes: vec!["ENV FOO=bar".into(), "CMD [\"sh\"]".into()],
        });
        assert_eq!(decode_line::<Request>(&encode_line(&commit)).unwrap(), commit);

        let min_commit: Request = decode_line(br#"{"method":"commit","params":{"container":"c1"}}"#).unwrap();
        assert_eq!(
            min_commit,
            Request::Commit(CommitSpec { container: "c1".into(), ..Default::default() })
        );

        assert_eq!(
            decode_line::<Response>(&encode_line(&Response::ImageArchive)).unwrap(),
            Response::ImageArchive
        );
    }

    #[test]
    fn streaming_logs_round_trip() {
        let req = Request::Logs {
            id: "api".into(),
            tail: Some(25),
            follow: true,
            since_ms: Some(1_700_000_000_000),
            until_ms: Some(1_700_000_060_000),
        };
        assert_eq!(decode_line::<Request>(&encode_line(&req)).unwrap(), req);

        // Legacy clients omitting follow/since_ms/until_ms decode cleanly.
        let legacy: Request = decode_line(br#"{"method":"logs","params":{"id":"web","tail":10}}"#).unwrap();
        assert_eq!(
            legacy,
            Request::Logs { id: "web".into(), tail: Some(10), follow: false, since_ms: None, until_ms: None }
        );

        let log_resp = Response::Log(LogLine {
            seq: 7,
            timestamp_ms: 1_700_000_000_123,
            level: LogLevel::Info,
            message: "ready".into(),
            stream: LogStream::Stdout,
        });
        assert_eq!(decode_line::<Response>(&encode_line(&log_resp)).unwrap(), log_resp);

        let end_with_code = Response::LogEnd { exit_code: Some(42) };
        assert_eq!(decode_line::<Response>(&encode_line(&end_with_code)).unwrap(), end_with_code);

        let end_none = Response::LogEnd { exit_code: None };
        assert_eq!(String::from_utf8(encode_line(&end_none)).unwrap(), "{\"type\":\"log_end\",\"data\":{}}\n");
        assert_eq!(decode_line::<Response>(&encode_line(&end_none)).unwrap(), end_none);
    }

    #[test]
    fn tmpfs_security_and_stats_stream_round_trip() {
        let m = parse_tmpfs_spec("/run/lock:rw,noexec,size=32m").unwrap();
        assert_eq!(m, TmpfsMount { target: "/run/lock".into(), options: "rw,noexec,size=32m".into() });
        let m_plain = parse_tmpfs_spec("/tmp/").unwrap();
        assert_eq!(m_plain, TmpfsMount { target: "/tmp".into(), options: String::new() });
        assert!(parse_tmpfs_spec("/").is_err());
        assert!(parse_tmpfs_spec("relative").is_err());
        assert!(parse_tmpfs_spec("/a/../b").is_err());

        let (flags_default, data_default) = parse_tmpfs_options("");
        assert_eq!(flags_default, 2 | 4 | 8);
        assert_eq!(data_default, "mode=1777");

        let (flags_custom, data_custom) = parse_tmpfs_options("ro,exec,size=64m,mode=0700");
        assert_eq!(flags_custom, 1 | 2 | 4);
        assert_eq!(data_custom, "size=64m,mode=0700");

        let req = Request::StatsStream { ids: vec!["web".into()], all: true, follow: true };
        assert_eq!(decode_line::<Request>(&encode_line(&req)).unwrap(), req);

        let snap = Response::StatsSnapshot(vec![ContainerStatRow {
            id: "abc123def456".into(),
            name: "web".into(),
            status: ContainerStatus::Running,
            cpus: 2,
            cpu_percent: 12.5,
            memory_bytes: 16 << 20,
            memory_limit_bytes: 1024 << 20,
            net_rx_bytes: 100,
            net_tx_bytes: 200,
            pids: 3,
            timestamp_ms: 1_700_000_000_000,
        }]);
        assert_eq!(decode_line::<Response>(&encode_line(&snap)).unwrap(), snap);

        let min_details: ContainerDetails = serde_json::from_str(
            r#"{"container":{"id":"abc","name":"web","image":"nginx","status":"running","exit_code":null,"ports":[],"mounts":[],"ip_address":null,"cpu_percent":null,"memory_bytes":null,"memory_limit_bytes":null,"created_at":1,"started_at":2},"image_reference":"nginx","image_digest":"sha256:aa","command":["nginx"],"env":[],"working_dir":"/","user":null,"hostname":"web","cpus":2,"memory_mib":512,"kernel":"6.12","vm_dir":"/tmp"}"#,
        )
        .unwrap();
        assert!(!min_details.read_only);
        assert!(min_details.cap_add.is_empty());
        assert!(min_details.cap_drop.is_empty());
        assert!(min_details.tmpfs.is_empty());
        assert!(!min_details.init);
        assert!(min_details.networks.is_empty());
    }

    #[test]
    fn user_network_requests_and_responses_round_trip() {
        let create = Request::CreateNetwork {
            name: "app-net".into(),
            internal: true,
            labels: BTreeMap::from([("env".into(), "dev".into())]),
        };
        for req in [
            Request::ListNetworks,
            create,
            Request::RemoveNetwork { name: "app-net".into() },
            Request::ConnectNetwork { network: "app-net".into(), container: "web".into() },
            Request::DisconnectNetwork { network: "app-net".into(), container: "web".into() },
        ] {
            assert_eq!(decode_line::<Request>(&encode_line(&req)).unwrap(), req);
        }

        let net = UserNetwork {
            id: "net123456789".into(),
            name: "app-net".into(),
            driver: "bridge".into(),
            internal: true,
            builtin: false,
            created_at: 1_700_000_000,
            labels: BTreeMap::from([("env".into(), "dev".into())]),
            containers: vec!["web".into()],
        };
        for resp in [
            Response::Networks(vec![net.clone()]),
            Response::NetworkCreated(net),
            Response::NetworkRemoved { name: "app-net".into() },
            Response::NetworkUpdated { network: "app-net".into(), container: "web".into(), connected: true },
        ] {
            assert_eq!(decode_line::<Response>(&encode_line(&resp)).unwrap(), resp);
        }
    }

    #[test]
    fn shm_size_and_ulimit_parsing_and_round_trip() {
        assert_eq!(parse_byte_size("64m").unwrap(), 64 * 1024 * 1024);
        assert_eq!(parse_byte_size("128MB").unwrap(), 128 * 1024 * 1024);
        assert_eq!(parse_byte_size("2G").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_byte_size("4096").unwrap(), 4096);
        assert_eq!(parse_byte_size("512kib").unwrap(), 512 * 1024);
        assert!(parse_byte_size("0m").is_err());
        assert!(parse_byte_size("").is_err());
        assert!(parse_byte_size("10tb").is_err());

        assert_eq!(
            parse_ulimit_spec("NOFILE=1024:4096").unwrap(),
            UlimitSpec { name: "nofile".into(), soft: 1024, hard: 4096 }
        );
        assert_eq!(
            parse_ulimit_spec("core=unlimited").unwrap(),
            UlimitSpec { name: "core".into(), soft: ULIMIT_UNLIMITED, hard: ULIMIT_UNLIMITED }
        );
        assert_eq!(
            parse_ulimit_spec("nproc=512:-1").unwrap(),
            UlimitSpec { name: "nproc".into(), soft: 512, hard: ULIMIT_UNLIMITED }
        );
        assert!(parse_ulimit_spec("nofile=4096:1024").is_err());
        assert!(parse_ulimit_spec("nofile=unlimited:1024").is_err());
        assert!(parse_ulimit_spec("bogus=10").is_err());
        assert!(parse_ulimit_spec("nofile").is_err());

        let run = Request::Run(Box::new(RunSpec {
            image: "alpine:latest".into(),
            name: None,
            command: None,
            env: vec![],
            cpus: None,
            memory_mib: None,
            ports: vec![],
            volumes: vec![],
            restart: None,
            tty: false,
            rows: 0,
            cols: 0,
            auto_remove: false,
            platform: None,
            entrypoint: None,
            working_dir: None,
            user: None,
            hostname: None,
            project: None,
            service: None,
            labels: BTreeMap::new(),
            dns_aliases: vec![],
            healthcheck: None,
            read_only: false,
            cap_add: vec![],
            cap_drop: vec![],
            tmpfs: vec![],
            init: false,
            networks: vec![],
            shm_size_bytes: Some(128 * 1024 * 1024),
            ulimits: vec![UlimitSpec { name: "nofile".into(), soft: 2048, hard: 4096 }],
            egress: None,
            chaos: None,
        }));
        assert_eq!(decode_line::<Request>(&encode_line(&run)).unwrap(), run);
    }

    #[test]
    fn egress_policy_chaos_and_xray_round_trip() {
        assert_eq!(EgressPolicy::parse("offline").unwrap(), EgressPolicy::Offline);
        assert_eq!(EgressPolicy::parse("unrestricted").unwrap(), EgressPolicy::Unrestricted);
        let allow = EgressPolicy::parse("allow:api.stripe.com,*.pypi.org,10.0.0.0/8").unwrap();
        assert!(allow.allows_host("api.stripe.com"));
        assert!(allow.allows_host("files.pypi.org"));
        assert!(allow.allows_host("pypi.org"));
        assert!(!allow.allows_host("evil.com"));
        assert!(allow.allows_ip("10.1.2.3".parse().unwrap()));
        assert!(!allow.allows_ip("8.8.8.8".parse().unwrap()));
        assert!(EgressPolicy::parse("allow:").is_err());

        let policy_req = Request::SetNetworkPolicy {
            id: "web".into(),
            egress: Some(allow.clone()),
            chaos: Some(ChaosProfile {
                latency_ms: 120,
                jitter_ms: 20,
                loss_percent: 5,
                bandwidth_kbps: Some(1024),
            }),
        };
        assert_eq!(decode_line::<Request>(&encode_line(&policy_req)).unwrap(), policy_req);

        let xray_ev = Response::XRayEvent(XRayEvent {
            seq: 1,
            timestamp_ms: 1700000000123,
            kind: XRayEventKind::Net,
            action: "GET".into(),
            target: "api.stripe.com:443".into(),
            detail: Some("/v1/charges 200".into()),
            allowed: true,
            latency_us: Some(4200),
            bytes: Some(1024),
            pid: Some(42),
        });
        assert_eq!(decode_line::<Response>(&encode_line(&xray_ev)).unwrap(), xray_ev);

        for req in [
            AgentRequest::SetXRay { container: "c1".into(), enabled: true },
            AgentRequest::WatchXRay { container: "c1".into() },
            AgentRequest::EgressBridge,
            AgentRequest::EgressStream { id: 7 },
            AgentRequest::DropDefaultRoute,
        ] {
            assert_eq!(decode_line::<AgentRequest>(&encode_line(&req)).unwrap(), req);
        }
    }

    #[test]
    fn fork_and_diff_round_trip() {
        let req = Request::Fork(Box::new(ForkSpec {
            source: "pg-main".into(),
            names: vec!["pg-test".into()],
            count: 1,
            start: true,
            auto_remove: true,
            ports: vec![PortMapping {
                host_ip: None,
                host_port: 5433,
                container_port: 5432,
                protocol: PortProtocol::Tcp,
            }],
            env: vec!["FOO=bar".into()],
            command: Some(vec!["sh".into(), "-c".into(), "echo ok".into()]),
        }));
        assert_eq!(decode_line::<Request>(&encode_line(&req)).unwrap(), req);

        let diff_req = Request::Diff { id: "pg-test".into() };
        assert_eq!(decode_line::<Request>(&encode_line(&diff_req)).unwrap(), diff_req);

        let diff_resp = Response::Diff {
            entries: vec![
                DiffEntry { kind: DiffKind::Added, path: "/etc/forked".into(), size_bytes: Some(12) },
                DiffEntry { kind: DiffKind::Modified, path: "/etc/hosts".into(), size_bytes: Some(64) },
                DiffEntry { kind: DiffKind::Deleted, path: "/etc/motd".into(), size_bytes: None },
            ],
        };
        assert_eq!(decode_line::<Response>(&encode_line(&diff_resp)).unwrap(), diff_resp);

        for agent_req in [
            AgentRequest::SyncFs,
            AgentRequest::Diff { container: "c1".into() },
            AgentRequest::Prepare {
                container: "c2".into(),
                layers: vec!["sha256:aa".into()],
                mounts: vec![],
                tmpfs: vec![],
                hostname: Some("c2".into()),
                now_secs: None,
                fork_from: Some("c1".into()),
            },
        ] {
            assert_eq!(decode_line::<AgentRequest>(&encode_line(&agent_req)).unwrap(), agent_req);
        }
        assert_eq!(decode_line::<AgentEvent>(&encode_line(&AgentEvent::Synced)).unwrap(), AgentEvent::Synced);
    }
}
