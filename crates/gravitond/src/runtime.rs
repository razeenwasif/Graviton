//! Real containers: every container runs in its own guest VM.
//!
//! `Engine` owns the image store, the containers' VMs and their saved state.
//! Its methods block (registry downloads, VM boots), so the async server calls
//! them through `spawn_blocking`. Each started container gets a supervisor
//! thread that:
//!
//! 1. boots a VM on Virtualization.framework's NAT network, with the blob
//!    store (virtiofs `blobs`, read-only) and the container's volumes shared,
//! 2. asks the agent to unpack the image, mount the volumes and write the
//!    network files (DHCP runs meanwhile),
//! 3. publishes the container's ports on 127.0.0.1, forwarding to the guest,
//! 4. runs the image's entrypoint and streams its output into the logs,
//!    while a monitor thread samples its CPU, memory and network use,
//! 5. records the exit code and powers the VM off.
//!
//! Containers, their run settings and their logs are saved under
//! `~/Library/Application Support/Graviton`, so they survive a restart.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::hash::{BuildHasher, RandomState};
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, UdpSocket};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use graviton_core::{ImageReference, Store, paths};
use graviton_image::{BlobStore, Credentials, HealthcheckConfig, ImageError, ImageIndex, LocalImage, Progress, PullOptions, PushProgress, Registry};
use graviton_proto::{
    AGENT_VSOCK_PORT, AgentEvent, AgentRequest, ChaosProfile, ContainerDetails, ContainerNetworkPolicy,
    ContainerStatRow, ContainerStats, ContainerStatus, ContainerSummary, DEFAULT_NETWORK_NAME, DiffEntry,
    DirectoryListing, EgressPolicy, EngineInfo, ExecSpec, FileContents, ForkSpec, FrozenBundleInfo, FrozenRestoreInfo, GuestMount, GuestStats, HealthLog,
    HealthState, HealthStatus, Healthcheck, HostEntry, ImageSummary, LogLevel, LogLine, LogStream, Mount,
    NetworkEndpoint, NetworkInfo, PROTOCOL_VERSION, PortMapping, PortProtocol, ProcessInfo, Response, RestartPolicy,
    RunSpec, SnapshotInfo, StatSample, TmpfsMount, UlimitSpec, UserNetwork, VolumeMountRef, VolumeSummary,
    XRayEvent, caps, decode_line, encode_line, parse_tmpfs_spec, validate_ulimit,
};
use graviton_vmm::apple::{AppleHypervisor, AppleVm};
use graviton_vmm::{NetworkMode, SharedDirectory, VmConfig, VmHandle};
use serde::{Deserialize, Serialize};

pub(crate) const DEFAULT_CPUS: u32 = 2;
pub(crate) const DEFAULT_MEMORY_MIB: u64 = 1024;
const STOP_GRACE: Duration = Duration::from_secs(10);
/// Exponential backoff for automatic restarts: 1s, 2s, 4s, ..., capped at 60s.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Staying up at least this long resets the restart backoff back to 1s.
const HEALTHY_UPTIME: Duration = Duration::from_secs(10);
const KERNEL_FALLBACK: &str = "Linux 6.12";
/// How often running containers are sampled, and how many samples are kept (5 minutes).
const STATS_INTERVAL: Duration = Duration::from_secs(2);
const STATS_KEPT: usize = 150;
/// The Files tab previews this much of a file.
const READ_LIMIT: u64 = 256 * 1024;
/// UDP port-forwarding limits (max datagram payload, idle NAT session TTL, max clients per port).
const UDP_BUF_SIZE: usize = 65_535;
const UDP_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const UDP_MAX_SESSIONS: usize = 256;

/// A host socket bound before the VM boots, ready to forward to the guest once DHCP completes.
enum BoundPort {
    Tcp { listener: TcpListener, container_port: u16 },
    Udp { socket: UdpSocket, container_port: u16 },
}

/// How to start a container (again).
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ContainerSpec {
    pub(crate) image: LocalImage,
    pub(crate) argv: Vec<String>,
    pub(crate) env: Vec<String>,
    pub(crate) cwd: String,
    pub(crate) user: Option<String>,
    pub(crate) cpus: u32,
    pub(crate) memory_mib: u64,
    #[serde(default)]
    pub(crate) ports: Vec<PortMapping>,
    #[serde(default)]
    pub(crate) volumes: Vec<Volume>,
    #[serde(default)]
    pub(crate) restart: RestartPolicy,
    /// True while a container with a restart policy should stay up (including across daemon restarts).
    #[serde(default)]
    pub(crate) want_running: bool,
    /// Remove the container automatically after it stops (`grav run --rm -it`).
    #[serde(default)]
    pub(crate) auto_remove: bool,
    /// Optional explicit hostname (defaults to the container's name).
    #[serde(default)]
    pub(crate) hostname: Option<String>,
    /// Compose stack/project name, if created by `grav compose`.
    #[serde(default)]
    pub(crate) project: Option<String>,
    /// Compose service name within `project`.
    #[serde(default)]
    pub(crate) service: Option<String>,
    /// Arbitrary key-value labels (`-l KEY=VALUE`, Compose `labels:`).
    #[serde(default)]
    pub(crate) labels: BTreeMap<String, String>,
    /// Additional project-scoped DNS aliases.
    #[serde(default)]
    pub(crate) dns_aliases: Vec<String>,
    /// Container `HEALTHCHECK` configuration (defaults/image merged at creation time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) healthcheck: Option<Healthcheck>,
    #[serde(default)]
    pub(crate) read_only: bool,
    #[serde(default)]
    pub(crate) cap_add: Vec<String>,
    #[serde(default)]
    pub(crate) cap_drop: Vec<String>,
    #[serde(default)]
    pub(crate) tmpfs: Vec<TmpfsMount>,
    #[serde(default)]
    pub(crate) init: bool,
    #[serde(default)]
    pub(crate) networks: Vec<String>,
    #[serde(default)]
    pub(crate) shm_size_bytes: Option<u64>,
    #[serde(default)]
    pub(crate) ulimits: Vec<UlimitSpec>,
    #[serde(default)]
    pub(crate) forked_from: Option<String>,
    /// Container folder name inside `/run/containers/` on a cloned ext4 disk before first boot renames it.
    #[serde(default)]
    pub(crate) disk_source_id: Option<String>,
    /// Host-enforced egress firewall policy.
    #[serde(default)]
    pub(crate) egress: EgressPolicy,
    /// Host-enforced network chaos profile.
    #[serde(default)]
    pub(crate) chaos: ChaosProfile,
}

/// Interactive `grav run -it` handle returned to the control connection.
pub struct AttachedRun {
    pub guest_fd: OwnedFd,
    pub exit_tx: std::sync::mpsc::SyncSender<i32>,
    pub stopped_rx: tokio::sync::oneshot::Receiver<()>,
}

struct TtyAttach {
    rows: u16,
    cols: u16,
    ready_tx: std::sync::mpsc::SyncSender<Result<OwnedFd>>,
    exit_rx: std::sync::mpsc::Receiver<i32>,
    stopped_tx: tokio::sync::oneshot::Sender<()>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Volume {
    /// Host directory shared into the VM.
    pub(crate) source: PathBuf,
    /// Absolute path inside the container.
    pub(crate) target: String,
    pub(crate) read_only: bool,
    /// Set for named volumes (`name:/path`).
    pub(crate) name: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct SavedNetwork {
    id: String,
    name: String,
    #[serde(default)]
    internal: bool,
    #[serde(default)]
    created_at: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    labels: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Default)]
struct SavedState {
    containers: Vec<ContainerSummary>,
    specs: HashMap<String, ContainerSpec>,
    #[serde(default)]
    networks: Vec<SavedNetwork>,
}

struct Running {
    name: String,
    project: Option<String>,
    service: Option<String>,
    dns_aliases: Vec<String>,
    networks: Vec<String>,
    vm: Arc<AppleVm>,
    agent: Arc<Mutex<File>>,
    paused: Arc<AtomicBool>,
    pid: Option<i32>,
    /// IPv4 address on the container network once DHCP has finished.
    ip: Option<String>,
    /// Whether the guest runs the embedded DNS resolver (`SetHosts`).
    dns: bool,
    forwards: Vec<tokio::task::JoinHandle<()>>,
    healthcheck: Option<tokio::task::JoinHandle<()>>,
    /// The VM's port on the container network; unplugged when this is dropped (after the VM stops).
    port: Option<crate::switch::Port>,
    /// Exclusive lease on the snapshot slot this VM restored from; released after the VM stops.
    _snapshot: Option<crate::snapshot::SnapshotLease>,
    /// Live firewall & chaos policy shared with host `EgressGateway`.
    network_policy: Arc<RwLock<ContainerNetworkPolicy>>,
    /// Guard so at most one `WatchXRay` vsock consumer runs per VM boot.
    xray_watcher_started: Arc<AtomicBool>,
    /// Guard so at most one `EgressBridge` host gateway runs per VM boot.
    egress_bridge_started: Arc<AtomicBool>,
}

const MAX_XRAY_EVENTS: usize = 2048;

pub type XRaySubscription = (String, Vec<XRayEvent>, tokio::sync::broadcast::Receiver<(String, XRayEvent)>);

#[derive(Default)]
struct ContainerXRay {
    enabled: bool,
    next_seq: u64,
    events: VecDeque<XRayEvent>,
}

#[derive(Default)]
struct StatsHistory {
    cpus: u32,
    samples: VecDeque<StatSample>,
    processes: Vec<ProcessInfo>,
}

/// Real-time log notification broadcast to streaming `grav logs -f` and Docker `/logs` clients.
#[derive(Debug, Clone)]
pub enum LogEvent {
    Line { container_id: String, line: LogLine },
    Ended { container_id: String, exit_code: Option<i32> },
}

/// Real-time stats update broadcast to streaming `grav stats` and Docker `/stats` clients.
#[derive(Debug, Clone)]
pub enum StatsEvent {
    Sample { container_id: String },
    Stopped { container_id: String },
}

pub struct Engine {
    pub store: Arc<Mutex<Store>>,
    pub(crate) blobs: BlobStore,
    pub(crate) index: Mutex<ImageIndex>,
    pub(crate) specs: Mutex<HashMap<String, ContainerSpec>>,
    pub(crate) snapshots: Arc<crate::snapshot::SnapshotPool>,
    running: Mutex<HashMap<String, Running>>,
    stats: Mutex<HashMap<String, StatsHistory>>,
    health: Mutex<HashMap<String, HealthState>>,
    networks: Mutex<BTreeMap<String, SavedNetwork>>,
    log_io: Mutex<()>,
    log_tx: tokio::sync::broadcast::Sender<LogEvent>,
    stats_tx: tokio::sync::broadcast::Sender<StatsEvent>,
    xray: Mutex<HashMap<String, ContainerXRay>>,
    xray_tx: tokio::sync::broadcast::Sender<(String, XRayEvent)>,
    switch: crate::switch::Switch,
    /// Serialises guest DNS table broadcasts so concurrent starts/stops never reorder.
    hosts_sync: Mutex<()>,
    hosts_gen: AtomicU64,
    /// What happened, for the Activity feed and push alerts.
    pub events: Arc<crate::events::EventLog>,
    /// Saved OCI registry credentials (usernames on disk, passwords in login Keychain).
    pub auth: crate::auth::RegistryAuth,
    /// Containers being stopped on request (so their exit isn't a failure).
    stopping: Mutex<HashSet<String>>,
    /// Scheduled backoff restart tasks, keyed by container id (aborted on stop/start/remove/shutdown).
    pending_restarts: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// Set during daemon shutdown so running containers keep `want_running` and don't emit false failure alerts.
    shutting_down: AtomicBool,
    /// One pull per image+platform at a time: a second waits for the first, then finds it.
    pulling: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    rt: tokio::runtime::Handle,
    state_path: PathBuf,
}

impl Engine {
    /// Open the engine, restoring saved containers. Call from inside the tokio runtime.
    pub fn open() -> Result<Arc<Self>> {
        let blobs = BlobStore::new(paths::blobs_dir());
        fs::create_dir_all(blobs.root().join("sha256")).context("creating the blob store")?;
        let index = ImageIndex::load(paths::support_dir().join("images.json"))?;
        let (log_tx, _) = tokio::sync::broadcast::channel(1024);
        let (stats_tx, _) = tokio::sync::broadcast::channel(256);
        let (xray_tx, _) = tokio::sync::broadcast::channel(512);
        let engine = Arc::new(Self {
            store: Arc::new(Mutex::new(Store::new())),
            blobs,
            index: Mutex::new(index),
            specs: Mutex::new(HashMap::new()),
            snapshots: Arc::new(crate::snapshot::SnapshotPool::open()),
            running: Mutex::new(HashMap::new()),
            stats: Mutex::new(HashMap::new()),
            health: Mutex::new(HashMap::new()),
            networks: Mutex::new(BTreeMap::new()),
            log_io: Mutex::new(()),
            log_tx,
            stats_tx,
            xray: Mutex::new(HashMap::new()),
            xray_tx,
            switch: crate::switch::Switch::new(),
            hosts_sync: Mutex::new(()),
            hosts_gen: AtomicU64::new(0),
            events: Arc::new(crate::events::EventLog::open(paths::support_dir().join("events.jsonl"))),
            auth: crate::auth::RegistryAuth::open(),
            stopping: Mutex::new(HashSet::new()),
            pending_restarts: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
            pulling: Mutex::new(HashMap::new()),
            rt: tokio::runtime::Handle::current(),
            state_path: paths::support_dir().join("containers.json"),
        });
        let autostart = engine.restore()?;
        let known: HashSet<String> = engine.store.lock().unwrap().containers().iter().map(|c| c.id.clone()).collect();
        remove_orphan_disks(&known);
        engine.refresh_images();
        for id in autostart {
            let e = Arc::clone(&engine);
            engine.rt.spawn(async move {
                let _ = tokio::task::spawn_blocking(move || e.launch_auto(&id, 0)).await;
            });
        }
        Ok(engine)
    }

    /// Cleanly flush and power off running VMs when `gravitond` stops, without
    /// altering saved `want_running` state so restart policies bring them back.
    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        for (_, handle) in self.pending_restarts.lock().unwrap().drain() {
            handle.abort();
        }
        let running: Vec<Running> = self.running.lock().unwrap().drain().map(|(_, r)| r).collect();
        for r in &running {
            for f in &r.forwards {
                f.abort();
            }
            if let Some(h) = &r.healthcheck {
                h.abort();
            }
            if r.paused.load(Ordering::Acquire) {
                let _ = r.vm.resume();
                r.paused.store(false, Ordering::Release);
            }
            let _ = r.agent.lock().unwrap().write_all(&encode_line(&AgentRequest::Shutdown));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        for r in &running {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || r.vm.wait_stopped(remaining).is_err() {
                let _ = r.vm.kill();
            }
        }
    }

    pub fn engine_info(&self) -> EngineInfo {
        let running_ids: Vec<String> = self.running.lock().unwrap().keys().cloned().collect();
        let store = self.store.lock().unwrap();
        let used: u64 = running_ids
            .iter()
            .filter_map(|id| store.container(id).ok())
            .map(|c| c.memory_bytes.unwrap_or(0))
            .sum();
        let container_disks: u64 = store.containers().iter().map(|c| disk_usage(&paths::vms_dir().join(&c.id))).sum();
        drop(store);
        // Images (OCI blobs + materialized EROFS disks), plus sparse container disks.
        let images: u64 = self.index.lock().unwrap().images().map(LocalImage::size).sum::<u64>()
            + crate::erofs::images_allocated_bytes();
        let disk_used = images + container_disks;
        EngineInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            protocol: PROTOCOL_VERSION,
            kernel: detect_guest_kernel(),
            arch: graviton_image::guest_arch().into(),
            cpus: std::thread::available_parallelism().map_or(0, |n| n.get() as u32),
            memory_bytes: host_memory(),
            memory_used_bytes: used,
            disk_bytes: 0,
            disk_used_bytes: disk_used,
            rosetta: AppleHypervisor::rosetta_available(),
            host_memory_used_bytes: host_memory_used(),
        }
    }

    pub fn snapshot_status(&self) -> SnapshotInfo {
        self.snapshots.status()
    }

    pub fn snapshot_warm(&self, cpus: Option<u32>, memory_mib: Option<u64>, rosetta: bool, slots: Option<u32>) -> Result<SnapshotInfo> {
        let cpus = cpus.unwrap_or(DEFAULT_CPUS);
        let memory_mib = memory_mib.unwrap_or(DEFAULT_MEMORY_MIB);
        let desired = slots.unwrap_or(1) as usize;
        self.snapshots.ensure_slots(self.blobs.root(), cpus, memory_mib, rosetta, desired)?;
        Ok(self.snapshots.status())
    }

    pub fn snapshot_clear(&self) -> SnapshotInfo {
        let _ = self.snapshots.clear_idle(false);
        self.snapshots.status()
    }

    // -- Saved state --------------------------------------------------------

    fn restore(&self) -> Result<Vec<String>> {
        let saved: SavedState = match fs::read(&self.state_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("reading {}", self.state_path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut interrupted = HashSet::new();
        {
            let mut store = self.store.lock().unwrap();
            for mut c in saved.containers {
                if matches!(c.status, ContainerStatus::Running | ContainerStatus::Created | ContainerStatus::Paused) {
                    // Its VM ended with the previous gravitond.
                    c.status = ContainerStatus::Exited;
                    c.started_at = None;
                    interrupted.insert(c.id.clone());
                }
                c.ip_address = None;
                c.cpu_percent = None;
                c.memory_bytes = None;
                c.health = None;
                let id = c.id.clone();
                if store.insert(c).is_ok() {
                    store.set_logs(&id, crate::persistence::read_logs_tail(&log_path(&id), graviton_core::store::LOG_LIMIT));
                }
            }
        }
        let mut specs = saved.specs;
        for spec in specs.values_mut() {
            if spec.networks.is_empty() {
                spec.networks.push(DEFAULT_NETWORK_NAME.to_string());
            }
        }
        {
            let mut nets = self.networks.lock().unwrap();
            for n in saved.networks {
                nets.insert(n.name.clone(), n);
            }
        }
        let mut autostart = Vec::new();
        let mut stopped_logs = Vec::new();
        for (id, spec) in &mut specs {
            let was_running = interrupted.contains(id);
            if spec.restart != RestartPolicy::No && (spec.want_running || was_running) {
                spec.want_running = true;
                autostart.push((id.clone(), spec.restart));
            } else if was_running {
                stopped_logs.push(id.clone());
            }
        }
        let mut restored_stats = HashMap::new();
        for (id, spec) in &specs {
            let samples = crate::persistence::read_stats_tail(&stats_path(id), STATS_KEPT);
            if !samples.is_empty() {
                restored_stats.insert(
                    id.clone(),
                    StatsHistory { cpus: spec.cpus, samples, processes: Vec::new() },
                );
            }
        }
        *self.stats.lock().unwrap() = restored_stats;
        *self.specs.lock().unwrap() = specs;
        for id in stopped_logs {
            self.log(&id, LogLevel::Warn, LogStream::System, "gravitond restarted while this container was running; its VM stopped".into());
        }
        for (id, policy) in &autostart {
            self.log(id, LogLevel::Info, LogStream::System, format!("gravitond started; restarting container automatically (policy: {policy})"));
        }
        Ok(autostart.into_iter().map(|(id, _)| id).collect())
    }

    fn save(&self) {
        let state = SavedState {
            containers: self.store.lock().unwrap().containers().to_vec(),
            specs: self.specs.lock().unwrap().clone(),
            networks: self.networks.lock().unwrap().values().cloned().collect(),
        };
        // Saves from different threads each get their own temporary file: with
        // one shared, a rename could find it already moved away.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = self.state_path.with_extension(format!("json.{n}.tmp"));
        let result = fs::write(&tmp, serde_json::to_vec_pretty(&state).expect("state serialises")).and_then(|_| fs::rename(&tmp, &self.state_path));
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        if let Err(err) = result {
            eprintln!("couldn't save container state: {err}");
        }
    }

    // -- Images -----------------------------------------------------------

    pub fn pull(&self, reference: &str, platform: Option<&str>) -> Result<ImageSummary> {
        self.pull_with_progress(reference, platform, |_| {})
    }

    pub fn pull_with_progress(
        &self,
        reference: &str,
        platform: Option<&str>,
        mut on_progress: impl FnMut(graviton_proto::PullProgress),
    ) -> Result<ImageSummary> {
        let parsed = ImageReference::parse(reference)?;
        let creds = self.auth.credentials_for(&parsed.registry)?;
        let rosetta_available = AppleHypervisor::rosetta_available();
        if let Some(p) = platform {
            let want = graviton_image::normalize_platform(p)?;
            if want == "amd64" && graviton_image::guest_arch() != "amd64" && !rosetta_available {
                bail!("linux/amd64 images need Apple Rosetta for Linux (run `softwareupdate --install-rosetta`)");
            }
        }
        eprintln!(
            "pull {}{}",
            parsed.display_name(),
            platform.map(|p| format!(" ({p})")).unwrap_or_default()
        );
        let t0 = Instant::now();
        let mut last_log = Instant::now();
        let mut last_emit = Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(Instant::now);
        let mut last_layer_idx: Option<usize> = None;
        let mut total_layers = 0usize;
        let opts = PullOptions {
            auth: creds.as_ref(),
            platform,
            allow_amd64_fallback: rosetta_available,
        };
        let image = Registry::new()
            .pull_with_options(&parsed, &self.blobs, opts, |p| match p {
                Progress::Resolved { layers, total_bytes, .. } => {
                    total_layers = layers;
                    eprintln!("  {layers} layers, {:.1} MiB", total_bytes as f64 / 1048576.0);
                    on_progress(graviton_proto::PullProgress::Resolved { layers, total_bytes });
                }
                Progress::Layer { index, digest, done, total, cached } => {
                    let short = digest.strip_prefix("sha256:").map(|h| &h[..h.len().min(12)]).unwrap_or(&digest);
                    if cached {
                        eprintln!("  layer {} {short} (cached)", index + 1);
                    } else if done == total || last_log.elapsed() > Duration::from_secs(1) {
                        last_log = Instant::now();
                        eprintln!(
                            "  layer {} {short} {:>3}%",
                            index + 1,
                            (done * 100).checked_div(total).unwrap_or(100)
                        );
                    }
                    let is_new_layer = last_layer_idx != Some(index);
                    let is_terminal = cached || (total > 0 && done >= total);
                    if is_new_layer || is_terminal || last_emit.elapsed() >= Duration::from_millis(100) {
                        last_layer_idx = Some(index);
                        last_emit = Instant::now();
                        on_progress(graviton_proto::PullProgress::Layer {
                            index,
                            total_layers,
                            digest,
                            done,
                            total,
                            cached,
                        });
                    }
                }
            })
            .map_err(|err| match err {
                ImageError::NoPlatform { reference, arch, available }
                    if platform.is_none() && !rosetta_available && available.contains("linux/amd64") =>
                {
                    anyhow!(
                        "no linux/{arch} image for {reference} (available: {available}); install Rosetta with `softwareupdate --install-rosetta` to run linux/amd64 images"
                    )
                }
                other => other.into(),
            })?;
        eprintln!("  done in {:.1} s ({})", t0.elapsed().as_secs_f64(), image.architecture);
        self.index.lock().unwrap().insert(image.clone())?;
        self.refresh_images();
        let summary = image.summary(self.used_by(&image.reference));
        let name = format!("{}:{}", summary.repository, summary.tag);
        let arch_note = if summary.architecture == "amd64" && graviton_image::guest_arch() != "amd64" {
            ", amd64 via Rosetta"
        } else {
            ""
        };
        self.events.record(
            graviton_proto::EventKind::Pulled,
            None,
            Some(&name),
            format!("Pulled {name} ({:.1} MB{arch_note})", summary.size_bytes as f64 / 1048576.0),
        );
        Ok(summary)
    }

    pub(crate) fn image_for_platform(&self, reference: &str, platform: Option<&str>) -> Result<LocalImage> {
        let want_arch = platform.map(graviton_image::normalize_platform).transpose()?;
        let pick = |img: &LocalImage| match want_arch.as_deref() {
            Some(a) => img.for_arch(a),
            None => Some(img.for_arch(graviton_image::guest_arch()).unwrap_or_else(|| img.clone())),
        };
        if let Some(img) = self.index.lock().unwrap().get(reference).and_then(&pick) {
            return Ok(img);
        }
        // Two containers of a new image+platform at once: one pull, and the other waits for it.
        let key = format!("{}|{}", reference, want_arch.as_deref().unwrap_or(""));
        let gate = Arc::clone(self.pulling.lock().unwrap().entry(key).or_default());
        let _turn = gate.lock().unwrap();
        if let Some(img) = self.index.lock().unwrap().get(reference).and_then(&pick) {
            return Ok(img);
        }
        self.pull(reference, platform)?;
        self.index
            .lock()
            .unwrap()
            .get(reference)
            .and_then(pick)
            .ok_or_else(|| anyhow!("pulled {reference} but can't find it"))
    }

    pub(crate) fn used_by(&self, reference: &str) -> u32 {
        let specs = self.specs.lock().unwrap();
        specs.values().filter(|s| s.image.reference == reference).count() as u32
    }

    pub(crate) fn refresh_images(&self) {
        let summaries: Vec<ImageSummary> = {
            let index = self.index.lock().unwrap();
            index.images().map(|i| i.summary(self.used_by(&i.reference))).collect()
        };
        self.store.lock().unwrap().set_images(summaries);
    }

    /// Resolve a local image by name/reference (`alpine`, `alpine:3.22`, `ghcr.io/...`)
    /// or by full/prefix digest (`sha256:...` or hex prefix of config/manifest digest).
    pub fn resolve_local_image(&self, key: &str) -> Option<LocalImage> {
        let key = key.trim();
        let index = self.index.lock().unwrap();
        if let Some(img) = index.get(key) {
            return Some(img.clone());
        }
        let hex = key.strip_prefix("sha256:").unwrap_or(key).to_ascii_lowercase();
        if hex.len() >= 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            let mut hits = index.images().filter(|img| {
                let c = img.config_digest.strip_prefix("sha256:").unwrap_or(&img.config_digest);
                let m = img.manifest_digest.strip_prefix("sha256:").unwrap_or(&img.manifest_digest);
                c.starts_with(&hex)
                    || m.starts_with(&hex)
                    || img.variants.iter().any(|v| {
                        let vc = v.config_digest.strip_prefix("sha256:").unwrap_or(&v.config_digest);
                        let vm = v.manifest_digest.strip_prefix("sha256:").unwrap_or(&v.manifest_digest);
                        vc.starts_with(&hex) || vm.starts_with(&hex)
                    })
            });
            if let (Some(first), None) = (hits.next(), hits.next()) {
                return Some(first.clone());
            }
        }
        None
    }

    /// Inspect a local image, returning its metadata and raw OCI config JSON.
    pub fn inspect_image(&self, key: &str) -> Result<(LocalImage, serde_json::Value)> {
        let img = self
            .resolve_local_image(key)
            .ok_or_else(|| anyhow!("No such image: {key}"))?;
        let raw = self.blobs.read(&graviton_image::Digest::parse(&img.config_digest)?)?;
        let json: serde_json::Value = serde_json::from_slice(&raw).context("parsing image config")?;
        Ok((img, json))
    }

    /// Tag an existing local image under a new reference without copying layers.
    pub fn tag_image(&self, source: &str, target: &str) -> Result<ImageSummary> {
        let src = self
            .resolve_local_image(source)
            .ok_or_else(|| anyhow!("No such image: {source}"))?;
        let parsed = ImageReference::parse(target)?;
        if parsed.digest.is_some() {
            bail!("cannot tag an image with a digest reference; use NAME[:TAG]");
        }
        let mut tagged = src;
        tagged.reference = parsed.to_string();
        tagged.pulled_at = now_secs();
        self.index.lock().unwrap().insert(tagged.clone())?;
        self.refresh_images();
        Ok(tagged.summary(self.used_by(&tagged.reference)))
    }

    /// Push a local image to its registry using saved credentials (`grav login`).
    pub fn push(&self, reference: &str) -> Result<graviton_proto::ImagePushResult> {
        self.push_with_progress(reference, None, |_| {})
    }

    /// Push a local image to its registry, streaming progress updates.
    pub fn push_with_progress(
        &self,
        reference: &str,
        auth_override: Option<Credentials>,
        mut on_progress: impl FnMut(graviton_proto::ImagePushProgress),
    ) -> Result<graviton_proto::ImagePushResult> {
        let target = ImageReference::parse(reference)?;
        let local = self
            .resolve_local_image(reference)
            .or_else(|| self.resolve_local_image(&target.to_string()))
            .ok_or_else(|| {
                anyhow!(
                    "image {reference} not found locally (build it or tag an image with `grav tag SRC {reference}`)"
                )
            })?;
        let creds = match auth_override {
            Some(c) => Some(c),
            None => self.auth.credentials_for(&target.registry)?,
        };
        let display = target.display_name();
        eprintln!("push {display}");
        let t0 = Instant::now();
        let mut last_log = Instant::now();
        let mut last_emit = Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(Instant::now);
        let mut last_blob_idx: Option<usize> = None;
        let mut total_blobs = 0usize;

        let res = Registry::new().push_with_auth(&local, &target, &self.blobs, creds.as_ref(), |p| match p {
            PushProgress::Started { blobs, total_bytes } => {
                total_blobs = blobs;
                eprintln!("  {blobs} blobs, {:.1} MiB", total_bytes as f64 / 1048576.0);
                on_progress(graviton_proto::ImagePushProgress::Started { blobs, total_bytes });
            }
            PushProgress::Blob { index, digest, done, total, skipped } => {
                let short = digest.strip_prefix("sha256:").map(|h| &h[..h.len().min(12)]).unwrap_or(&digest);
                if skipped {
                    eprintln!("  blob {} {short} (already exists)", index + 1);
                } else if done == total || last_log.elapsed() > Duration::from_secs(1) {
                    last_log = Instant::now();
                    eprintln!(
                        "  blob {} {short} {:>3}%",
                        index + 1,
                        (done * 100).checked_div(total).unwrap_or(100)
                    );
                }
                let is_new_blob = last_blob_idx != Some(index);
                let is_terminal = skipped || total == 0 || done >= total;
                if is_new_blob || is_terminal || last_emit.elapsed() >= Duration::from_millis(100) {
                    last_blob_idx = Some(index);
                    last_emit = Instant::now();
                    on_progress(graviton_proto::ImagePushProgress::Blob {
                        index,
                        total_blobs,
                        digest,
                        done,
                        total,
                        skipped,
                    });
                }
            }
            PushProgress::Manifest { digest, size } => {
                eprintln!("  manifest {} ({} B)", &digest[..digest.len().min(19)], size);
                on_progress(graviton_proto::ImagePushProgress::Manifest { digest, size });
            }
        })?;

        eprintln!("  pushed in {:.1} s", t0.elapsed().as_secs_f64());
        self.events.record(
            graviton_proto::EventKind::Pushed,
            None,
            Some(&display),
            format!("Pushed {display} ({:.1} MB)", res.size_bytes as f64 / 1048576.0),
        );
        Ok(graviton_proto::ImagePushResult {
            reference: res.reference,
            digest: res.digest,
            size_bytes: res.size_bytes,
            blobs_pushed: res.blobs_pushed,
            blobs_skipped: res.blobs_skipped,
        })
    }

    // -- Containers -------------------------------------------------------

    fn create_container(&self, spec: RunSpec) -> Result<String> {
        let image = self.image_for_platform(&spec.image, spec.platform.as_deref())?;
        self.create_with_image(spec, image)
    }

    /// Create a container using only locally available images (does not auto-pull).
    /// Used by the Docker REST API where `POST /containers/create` must return 404
    /// on missing images so the Docker client performs `POST /images/create` first.
    pub fn create_container_local(&self, spec: RunSpec) -> Result<String> {
        let want_arch = spec.platform.as_deref().map(graviton_image::normalize_platform).transpose()?;
        let image = self
            .resolve_local_image(&spec.image)
            .and_then(|img| match want_arch.as_deref() {
                Some(a) => img.for_arch(a),
                None => Some(img.for_arch(graviton_image::guest_arch()).unwrap_or(img)),
            })
            .ok_or_else(|| anyhow!("No such image: {}", spec.image))?;
        self.create_with_image(spec, image)
    }

    fn create_with_image(&self, spec: RunSpec, image: LocalImage) -> Result<String> {
        let ports = spec.ports.iter().map(|p| parse_port(p)).collect::<Result<Vec<_>>>()?;
        let volumes = spec.volumes.iter().map(|v| parse_volume(v)).collect::<Result<Vec<_>>>()?;
        let cap_add = spec
            .cap_add
            .iter()
            .map(|c| caps::normalize_cap(c).map_err(|e| anyhow!(e)))
            .collect::<Result<Vec<_>>>()?;
        let cap_drop = spec
            .cap_drop
            .iter()
            .map(|c| caps::normalize_cap(c).map_err(|e| anyhow!(e)))
            .collect::<Result<Vec<_>>>()?;
        let _ = caps::resolve_cap_mask(&cap_add, &cap_drop).map_err(|e| anyhow!(e))?;
        let tmpfs = spec
            .tmpfs
            .iter()
            .map(|t| {
                let combined = if t.options.is_empty() { t.target.clone() } else { format!("{}:{}", t.target, t.options) };
                parse_tmpfs_spec(&combined).map_err(|e| anyhow!(e))
            })
            .collect::<Result<Vec<_>>>()?;
        if spec.shm_size_bytes == Some(0) {
            bail!("--shm-size must be greater than 0");
        }
        let ulimits = spec
            .ulimits
            .iter()
            .map(|u| validate_ulimit(u).map_err(|e| anyhow!(e)))
            .collect::<Result<Vec<_>>>()?;
        if let Some(name) = &spec.name {
            if self.store.lock().unwrap().name_taken(name) {
                bail!("the name {name} is already in use");
            }
        }
        let networks = normalize_network_list(&spec.networks);
        {
            let nets = self.networks.lock().unwrap();
            for net in &networks {
                if net != DEFAULT_NETWORK_NAME && !nets.contains_key(net) {
                    bail!("network `{net}` not found (create it with `grav network create {net}`)");
                }
            }
        }
        if image.architecture == "amd64" && graviton_image::guest_arch() != "amd64" && !AppleHypervisor::rosetta_available() {
            bail!(
                "{} is a linux/amd64 image, which needs Apple Rosetta for Linux (run `softwareupdate --install-rosetta`)",
                spec.image
            );
        }
        let config = image.config(&self.blobs)?.config;
        let argv = match &spec.entrypoint {
            Some(ep) => ep
                .iter()
                .cloned()
                .chain(spec.command.clone().unwrap_or_default())
                .collect(),
            None => config.argv(spec.command.as_deref()),
        };
        if argv.is_empty() {
            bail!("{} has no entrypoint or command; pass one after the image name", spec.image);
        }
        let display = ImageReference::parse(&image.reference).map(|r| r.display_name()).unwrap_or(image.reference.clone());
        let restart = spec.restart.unwrap_or_default();
        let cwd = spec
            .working_dir
            .clone()
            .filter(|w| !w.is_empty())
            .unwrap_or_else(|| config.working_dir());
        let user = spec
            .user
            .clone()
            .filter(|u| !u.is_empty())
            .or_else(|| config.user.clone().filter(|u| !u.is_empty()));
        let healthcheck = resolve_healthcheck(config.healthcheck.as_ref(), spec.healthcheck);
        let cs = ContainerSpec {
            argv,
            env: config.env(&spec.env),
            cwd,
            user,
            cpus: spec.cpus.unwrap_or(DEFAULT_CPUS),
            memory_mib: spec.memory_mib.unwrap_or(DEFAULT_MEMORY_MIB),
            image,
            ports,
            volumes,
            restart,
            want_running: !spec.tty && restart != RestartPolicy::No,
            auto_remove: spec.auto_remove && spec.tty,
            hostname: spec.hostname.filter(|h| !h.is_empty()),
            project: spec.project.filter(|p| !p.is_empty()),
            service: spec.service.filter(|s| !s.is_empty()),
            labels: spec.labels,
            dns_aliases: spec.dns_aliases,
            healthcheck,
            read_only: spec.read_only,
            cap_add,
            cap_drop,
            tmpfs,
            init: spec.init,
            networks,
            shm_size_bytes: spec.shm_size_bytes,
            ulimits,
            forked_from: None,
            disk_source_id: None,
            egress: spec.egress.unwrap_or_default(),
            chaos: spec.chaos.unwrap_or_default(),
        };

        let id = new_id();
        let name = match spec.name {
            Some(n) => n,
            None => self.generated_name(&display),
        };
        let summary = ContainerSummary {
            id: id.clone(),
            name,
            image: display,
            status: ContainerStatus::Created,
            exit_code: None,
            ports: cs.ports.clone(),
            mounts: cs
                .volumes
                .iter()
                .map(|v| Mount { source: v.name.clone().unwrap_or_else(|| v.source.display().to_string()), target: v.target.clone(), read_only: v.read_only })
                .collect(),
            ip_address: None,
            cpu_percent: None,
            memory_bytes: None,
            memory_limit_bytes: Some(cs.memory_mib << 20),
            created_at: now_secs(),
            started_at: None,
            project: cs.project.clone(),
            service: cs.service.clone(),
            health: None,
            forked_from: None,
        };
        self.store.lock().unwrap().insert(summary)?;
        self.specs.lock().unwrap().insert(id.clone(), cs);
        self.save();
        self.refresh_images();
        Ok(id)
    }

    pub fn run(self: &Arc<Self>, spec: RunSpec) -> Result<ContainerSummary> {
        let id = self.create_container(spec)?;
        self.launch(&id)
    }

    /// Create and boot a container with its main process attached to a PTY (`grav run -it`).
    /// Returns the terminal vsock fd once the process is running; when the caller sends
    /// the exit code on `exit_tx`, the VM stops and signals `stopped_rx`.
    pub fn run_tty(self: &Arc<Self>, spec: RunSpec) -> Result<AttachedRun> {
        let rows = spec.rows;
        let cols = spec.cols;
        let id = self.create_container(spec)?;
        self.launch_tty(&id, rows, cols)
    }

    /// Boot an already-created container attached to a PTY (used by `docker.sock` attach + start).
    pub fn start_tty(self: &Arc<Self>, key: &str, rows: u16, cols: u16) -> Result<AttachedRun> {
        let id = self.store.lock().unwrap().container(key)?.id.clone();
        if self.running.lock().unwrap().contains_key(&id) {
            bail!("container {key} is already running");
        }
        {
            let mut specs = self.specs.lock().unwrap();
            let Some(spec) = specs.get_mut(&id) else {
                bail!("container {key} has no saved settings, so it can't be started again");
            };
            spec.want_running = false;
        }
        self.cancel_restart(&id);
        self.save();
        self.launch_tty(&id, rows, cols)
    }

    fn launch_tty(self: &Arc<Self>, id: &str, rows: u16, cols: u16) -> Result<AttachedRun> {
        let bound = self.bind_ports(id, true)?;
        self.store.lock().unwrap().update(id, |c| {
            c.status = ContainerStatus::Created;
            c.exit_code = None;
        });
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (exit_tx, exit_rx) = std::sync::mpsc::sync_channel(1);
        let (stopped_tx, stopped_rx) = tokio::sync::oneshot::channel();
        let attach = TtyAttach { rows, cols, ready_tx, exit_rx, stopped_tx };
        let engine = Arc::clone(self);
        let owned = id.to_string();
        std::thread::Builder::new()
            .name(format!("container-{id}"))
            .spawn(move || engine.supervise(&owned, bound, 0, Some(attach)))?;

        match ready_rx.recv() {
            Ok(Ok(guest_fd)) => Ok(AttachedRun { guest_fd, exit_tx, stopped_rx }),
            Ok(Err(err)) => Err(err),
            Err(_) => bail!("container {id} exited before its terminal attached"),
        }
    }

    pub fn start(self: &Arc<Self>, key: &str) -> Result<ContainerSummary> {
        let id = self.store.lock().unwrap().container(key)?.id.clone();
        if let Some(r) = self.running.lock().unwrap().get(&id) {
            if r.paused.load(Ordering::Acquire) {
                bail!("container {key} is paused (use `grav unpause`)");
            }
            bail!("container {key} is already running");
        }
        {
            let mut specs = self.specs.lock().unwrap();
            let Some(spec) = specs.get_mut(&id) else {
                bail!("container {key} has no saved settings, so it can't be started again");
            };
            spec.want_running = spec.restart != RestartPolicy::No;
        }
        self.cancel_restart(&id);
        self.save();
        self.launch(&id)
    }

    pub fn stop(&self, key: &str) -> Result<ContainerSummary> {
        let id = self.store.lock().unwrap().container(key)?.id.clone();
        // Clear `want_running` before cancelling any pending backoff timer
        // (same lock order as `schedule_restart`, preventing any race).
        let cancelled = {
            let mut specs = self.specs.lock().unwrap();
            if let Some(spec) = specs.get_mut(&id) {
                spec.want_running = false;
            }
            self.cancel_restart(&id)
        };
        let running_entry = {
            let running = self.running.lock().unwrap();
            running.get(&id).map(|r| (Arc::clone(&r.agent), r.pid, Arc::clone(&r.vm), Arc::clone(&r.paused)))
        };
        let Some((agent, pid, vm, paused)) = running_entry else {
            if cancelled {
                self.save();
                self.log(&id, LogLevel::Info, LogStream::System, "auto-restart cancelled".into());
                return Ok(self.store.lock().unwrap().container(&id)?.clone());
            }
            bail!("container {key} isn't running");
        };
        if paused.load(Ordering::Acquire) {
            // A paused VM cannot process SIGTERM or Shutdown until its vCPUs resume.
            let _ = vm.resume();
            paused.store(false, Ordering::Release);
        }
        self.stopping.lock().unwrap().insert(id.clone());
        if let Some(pid) = pid {
            let _ = agent.lock().unwrap().write_all(&encode_line(&AgentRequest::Signal { pid, signal: 15 }));
        }
        let deadline = Instant::now() + STOP_GRACE;
        while self.running.lock().unwrap().contains_key(&id) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        // Didn't exit on SIGTERM: the agent flushes the container's disk and
        // powers the VM off; if it can't, pull the plug. The supervisor records it.
        let wait = |secs| {
            let deadline = Instant::now() + Duration::from_secs(secs);
            while self.running.lock().unwrap().contains_key(&id) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
            }
        };
        if self.running.lock().unwrap().contains_key(&id) {
            let _ = agent.lock().unwrap().write_all(&encode_line(&AgentRequest::Shutdown));
            wait(5);
        }
        if self.running.lock().unwrap().contains_key(&id) {
            let _ = vm.kill();
            wait(5);
        }
        self.save();
        Ok(self.store.lock().unwrap().container(&id)?.clone())
    }

    pub fn pause(&self, key: &str) -> Result<ContainerSummary> {
        let (id, name, image) = {
            let store = self.store.lock().unwrap();
            let c = store.container(key)?;
            (c.id.clone(), c.name.clone(), c.image.clone())
        };
        let (vm, paused) = {
            let running = self.running.lock().unwrap();
            let Some(r) = running.get(&id) else {
                bail!("container {key} isn't running");
            };
            if r.paused.load(Ordering::Acquire) {
                bail!("container {key} is already paused");
            }
            (Arc::clone(&r.vm), Arc::clone(&r.paused))
        };
        paused.store(true, Ordering::Release);
        if let Err(err) = vm.pause() {
            paused.store(false, Ordering::Release);
            return Err(err).context("pausing container VM");
        }
        let updated = self
            .store
            .lock()
            .unwrap()
            .update(&id, |c| {
                c.status = ContainerStatus::Paused;
                c.cpu_percent = Some(0.0);
            })
            .ok_or_else(|| anyhow!("container {key} disappeared"))?;
        self.save();
        self.log(&id, LogLevel::Info, LogStream::System, "container paused".into());
        self.events.record(graviton_proto::EventKind::Paused, Some((&id, &name)), Some(&image), format!("{name} paused"));
        Ok(updated)
    }

    pub fn unpause(&self, key: &str) -> Result<ContainerSummary> {
        let (id, name, image) = {
            let store = self.store.lock().unwrap();
            let c = store.container(key)?;
            (c.id.clone(), c.name.clone(), c.image.clone())
        };
        let (vm, paused) = {
            let running = self.running.lock().unwrap();
            let Some(r) = running.get(&id) else {
                bail!("container {key} isn't running");
            };
            if !r.paused.load(Ordering::Acquire) {
                bail!("container {key} isn't paused");
            }
            (Arc::clone(&r.vm), Arc::clone(&r.paused))
        };
        vm.resume().context("resuming container VM")?;
        paused.store(false, Ordering::Release);
        // Best-effort wall-clock resync so guest UTC matches host after a long pause.
        if let Ok(fd) = vm.connect(AGENT_VSOCK_PORT) {
            let mut conn = File::from(fd);
            if matches!(read_event_unbuffered(&mut conn), Ok(AgentEvent::Ready { .. })) {
                let _ = conn.write_all(&encode_line(&AgentRequest::SetClock { now_secs: now_secs() }));
                let _ = read_event_unbuffered(&mut conn);
            }
        }
        let updated = self
            .store
            .lock()
            .unwrap()
            .update(&id, |c| {
                c.status = ContainerStatus::Running;
            })
            .ok_or_else(|| anyhow!("container {key} disappeared"))?;
        self.save();
        self.sync_hosts();
        self.log(&id, LogLevel::Info, LogStream::System, "container unpaused".into());
        self.events.record(graviton_proto::EventKind::Unpaused, Some((&id, &name)), Some(&image), format!("{name} unpaused"));
        Ok(updated)
    }

    pub fn restart(self: &Arc<Self>, key: &str) -> Result<ContainerSummary> {
        let id = self.store.lock().unwrap().container(key)?.id.clone();
        self.cancel_restart(&id);
        if self.running.lock().unwrap().contains_key(&id) {
            self.stop(&id)?;
        }
        if let Some(spec) = self.specs.lock().unwrap().get_mut(&id) {
            spec.want_running = spec.restart != RestartPolicy::No;
        }
        self.save();
        self.launch(&id)
    }

    /// Fork a container's overlay state and spec into one or more isolated containers
    /// using APFS copy-on-write cloning (`clonefile`) after quiescing a running VM.
    pub fn fork(self: &Arc<Self>, spec: ForkSpec) -> Result<Vec<ContainerSummary>> {
        let (src, source_spec, target_names) = {
            let store = self.store.lock().unwrap();
            let src = store.container(&spec.source)?.clone();
            let names = resolve_fork_names(&src.name, &spec.names, spec.count)?;
            for name in &names {
                if store.name_taken(name) {
                    bail!("container name `{name}` is already in use");
                }
            }
            drop(store);
            let source_spec = self
                .specs
                .lock()
                .unwrap()
                .get(&src.id)
                .cloned()
                .ok_or_else(|| anyhow!("missing spec for container `{}`", src.name))?;
            (src, source_spec, names)
        };

        if !spec.ports.is_empty() && target_names.len() > 1 {
            bail!("cannot override published ports when creating multiple forks");
        }

        let merged_env = merge_env_vars(&source_spec.env, &spec.env)?;
        let merged_argv = spec.command.clone().unwrap_or_else(|| source_spec.argv.clone());

        struct PauseGuard<'a> {
            engine: &'a Engine,
            id: String,
            active: bool,
        }
        impl Drop for PauseGuard<'_> {
            fn drop(&mut self) {
                if self.active {
                    let _ = self.engine.unpause(&self.id);
                }
            }
        }

        let was_running = src.status == ContainerStatus::Running;
        let mut resume_guard = PauseGuard { engine: self, id: src.id.clone(), active: false };
        if was_running {
            match self.agent_call(&src.id, |_| AgentRequest::SyncFs)? {
                AgentEvent::Synced => {}
                AgentEvent::Error { message } => bail!("syncing `{}` before fork: {message}", src.name),
                other => bail!("unexpected sync response from `{}`: {other:?}", src.name),
            }
            self.pause(&src.id)?;
            resume_guard.active = true;
        }

        let src_vm_dir = paths::vms_dir().join(&src.id);
        let src_disk_path = src_vm_dir.join(SCRATCH_DISK);
        let has_populated_disk = src_disk_path.is_file()
            && (source_spec.disk_source_id.is_some()
                || src.status != ContainerStatus::Created
                || src.started_at.is_some()
                || src.exit_code.is_some());
        let inherited_disk_source = if has_populated_disk {
            Some(source_spec.disk_source_id.clone().unwrap_or_else(|| src.id.clone()))
        } else {
            None
        };

        let mut prepared = Vec::with_capacity(target_names.len());
        let mut created_dirs = Vec::with_capacity(target_names.len());
        for name in target_names {
            let fork_id = new_id();
            let dst_vm_dir = paths::vms_dir().join(&fork_id);
            if let Err(err) = fs::create_dir_all(&dst_vm_dir) {
                for dir in &created_dirs {
                    let _ = fs::remove_dir_all(dir);
                }
                return Err(err.into());
            }
            created_dirs.push(dst_vm_dir.clone());
            if has_populated_disk {
                if let Err(err) = clone_sparse_file(&src_disk_path, &dst_vm_dir.join(SCRATCH_DISK)) {
                    for dir in &created_dirs {
                        let _ = fs::remove_dir_all(dir);
                    }
                    return Err(err);
                }
            }
            let mut cs = source_spec.clone();
            cs.argv = merged_argv.clone();
            cs.env = merged_env.clone();
            cs.ports = if !spec.ports.is_empty() { spec.ports.clone() } else { Vec::new() };
            cs.auto_remove = spec.auto_remove;
            cs.want_running = spec.start;
            cs.hostname = Some(name.clone());
            cs.forked_from = Some(src.id.clone());
            cs.disk_source_id = inherited_disk_source.clone();

            let summary = ContainerSummary {
                id: fork_id.clone(),
                name,
                image: src.image.clone(),
                status: ContainerStatus::Created,
                exit_code: None,
                ports: cs.ports.clone(),
                mounts: cs
                    .volumes
                    .iter()
                    .map(|v| Mount {
                        source: v.name.clone().unwrap_or_else(|| v.source.display().to_string()),
                        target: v.target.clone(),
                        read_only: v.read_only,
                    })
                    .collect(),
                ip_address: None,
                cpu_percent: None,
                memory_bytes: None,
                memory_limit_bytes: Some(cs.memory_mib << 20),
                created_at: now_secs(),
                started_at: None,
                project: cs.project.clone(),
                service: cs.service.clone(),
                health: None,
                forked_from: Some(src.id.clone()),
            };
            prepared.push((fork_id, cs, summary));
        }

        if was_running {
            resume_guard.active = false;
            self.unpause(&src.id)?;
        }

        let mut inserted_ids: Vec<String> = Vec::with_capacity(prepared.len());
        {
            let mut store = self.store.lock().unwrap();
            let mut specs = self.specs.lock().unwrap();
            for (fork_id, cs, summary) in &prepared {
                if let Err(err) = store.insert(summary.clone()) {
                    for prev_id in &inserted_ids {
                        let _ = store.remove(prev_id);
                        specs.remove(prev_id);
                    }
                    for dir in &created_dirs {
                        let _ = fs::remove_dir_all(dir);
                    }
                    return Err(err.into());
                }
                specs.insert(fork_id.clone(), cs.clone());
                inserted_ids.push(fork_id.clone());
            }
        }
        self.save();
        self.refresh_images();

        let mut summaries = Vec::with_capacity(prepared.len());
        for (fork_id, _, summary) in prepared {
            self.events.record(
                graviton_proto::EventKind::Forked,
                Some((&summary.id, &summary.name)),
                Some(&summary.image),
                format!("{} forked from {}", summary.name, src.name),
            );
            if spec.start {
                summaries.push(self.launch(&fork_id)?);
            } else {
                summaries.push(summary);
            }
        }
        Ok(summaries)
    }

    /// Inspect overlayfs upperdir filesystem changes inside a running container.
    pub fn diff(&self, key: &str) -> Result<Vec<DiffEntry>> {
        let c = self.store.lock().unwrap().container(key)?.clone();
        if c.status != ContainerStatus::Running {
            bail!("container `{}` is not running (start it first to inspect overlay diff)", c.name);
        }
        match self.agent_call(&c.id, |id| AgentRequest::Diff { container: id })? {
            AgentEvent::Diff { entries } => Ok(entries),
            AgentEvent::Error { message } => bail!("diffing `{}`: {message}", c.name),
            other => bail!("unexpected diff response from `{}`: {other:?}", c.name),
        }
    }

    /// Quiesce a container, clone its root overlay (`disk.img`), optional `.vzsave` RAM state,
    /// and `/workspace` bind mount into a self-contained frozen failure bundle.
    pub fn freeze_and_export(
        self: &Arc<Self>,
        key: &str,
        dest_bundle_dir: PathBuf,
        keep_paused: bool,
        exit_code: Option<i32>,
        failed_step: Option<String>,
    ) -> Result<FrozenBundleInfo> {
        validate_bundle_dir(&dest_bundle_dir)?;
        let (src, source_spec) = {
            let store = self.store.lock().unwrap();
            let src = store.container(key)?.clone();
            let spec = self
                .specs
                .lock()
                .unwrap()
                .get(&src.id)
                .cloned()
                .ok_or_else(|| anyhow!("missing spec for container `{}`", src.name))?;
            (src, spec)
        };

        struct PauseGuard<'a> {
            engine: &'a Engine,
            id: String,
            active: bool,
        }
        impl Drop for PauseGuard<'_> {
            fn drop(&mut self) {
                if self.active {
                    let _ = self.engine.unpause(&self.id);
                }
            }
        }

        let was_running = src.status == ContainerStatus::Running;
        let mut resume_guard = PauseGuard { engine: self, id: src.id.clone(), active: false };
        if was_running {
            match self.agent_call(&src.id, |_| AgentRequest::SyncFs)? {
                AgentEvent::Synced => {}
                AgentEvent::Error { message } => bail!("syncing `{}` before freeze: {message}", src.name),
                other => bail!("unexpected sync response from `{}`: {other:?}", src.name),
            }
            self.pause(&src.id)?;
            resume_guard.active = !keep_paused;
        }

        fs::create_dir_all(&dest_bundle_dir).with_context(|| format!("creating frozen bundle {}", dest_bundle_dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dest_bundle_dir, fs::Permissions::from_mode(0o700));
        }

        let src_vm_dir = paths::vms_dir().join(&src.id);
        let src_disk_path = src_vm_dir.join(SCRATCH_DISK);
        let has_populated_disk = src_disk_path.is_file()
            && (source_spec.disk_source_id.is_some()
                || src.status != ContainerStatus::Created
                || src.started_at.is_some()
                || src.exit_code.is_some());
        if has_populated_disk {
            clone_sparse_file(&src_disk_path, &dest_bundle_dir.join(SCRATCH_DISK))
                .with_context(|| format!("cloning root disk into {}", dest_bundle_dir.display()))?;
        }

        let workspace_vol = source_spec
            .volumes
            .iter()
            .find(|v| v.name.is_none() && v.target == "/workspace" && v.source.is_dir())
            .or_else(|| source_spec.volumes.iter().find(|v| v.name.is_none() && !v.read_only && v.source.is_dir()));
        let mut workspace_rel = None;
        let mut workspace_target = None;
        if let Some(vol) = workspace_vol {
            let ws_dst = dest_bundle_dir.join("workspace");
            clone_dir_cow(&vol.source, &ws_dst)?;
            workspace_rel = Some("workspace".to_string());
            workspace_target = Some(vol.target.clone());
        }

        let mut has_vm_state = false;
        if was_running && AppleHypervisor::session_unlocked() {
            let vm_opt = {
                let running = self.running.lock().unwrap();
                running.get(&src.id).filter(|r| r._snapshot.is_some()).map(|r| Arc::clone(&r.vm))
            };
            if let Some(vm) = vm_opt {
                let vz_path = dest_bundle_dir.join("state.vzsave");
                match vm.save_state(&vz_path) {
                    Ok(()) => has_vm_state = true,
                    Err(e) => {
                        let _ = fs::remove_file(&vz_path);
                        self.log(&src.id, LogLevel::Warn, LogStream::System, format!("non-fatal: couldn't capture .vzsave memory state: {e}"));
                    }
                }
            }
        }

        let mut sanitized_spec = source_spec.clone();
        sanitized_spec.want_running = false;
        sanitized_spec.ports.clear();
        sanitized_spec.restart = RestartPolicy::No;
        sanitized_spec.env = sanitize_frozen_env(&source_spec.env);
        sanitized_spec.disk_source_id = if has_populated_disk {
            Some(source_spec.disk_source_id.clone().unwrap_or_else(|| src.id.clone()))
        } else {
            None
        };

        let workdir = workspace_target
            .clone()
            .unwrap_or_else(|| if sanitized_spec.cwd.is_empty() { "/".into() } else { sanitized_spec.cwd.clone() });
        let created_at = now_secs() as i64;
        let manifest = FrozenBundleManifest {
            version: 1,
            source_container_id: src.id.clone(),
            source_container_name: src.name.clone(),
            image_ref: src.image.clone(),
            spec: sanitized_spec,
            created_at,
            exit_code,
            failed_step: failed_step.clone(),
            has_vm_state,
            workspace_rel: workspace_rel.clone(),
            workspace_target,
        };
        let tmp_manifest = dest_bundle_dir.join("manifest.json.tmp");
        let final_manifest = dest_bundle_dir.join("manifest.json");
        let bytes = serde_json::to_vec_pretty(&manifest).context("serializing frozen bundle manifest")?;
        fs::write(&tmp_manifest, &bytes).with_context(|| format!("writing {}", tmp_manifest.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp_manifest, fs::Permissions::from_mode(0o600));
        }
        fs::rename(&tmp_manifest, &final_manifest)?;

        if was_running && !keep_paused {
            resume_guard.active = false;
            self.unpause(&src.id)?;
        }

        let size_bytes = dir_size_bytes(&dest_bundle_dir);
        self.events.record(
            graviton_proto::EventKind::Forked,
            Some((&src.id, &src.name)),
            Some(&src.image),
            format!("{} frozen to {} ({} KB)", src.name, dest_bundle_dir.display(), size_bytes.div_ceil(1024)),
        );

        Ok(FrozenBundleInfo {
            bundle_dir: dest_bundle_dir,
            source_container_id: src.id,
            source_container_name: src.name,
            image: src.image,
            size_bytes,
            has_vm_state,
            has_workspace: workspace_rel.is_some(),
            workdir,
            exit_code,
            failed_step,
            created_at,
        })
    }

    /// Restore an exported frozen bundle into a live, non-destructive debuggable container.
    pub fn restore_frozen(self: &Arc<Self>, bundle_dir: PathBuf, name: Option<String>, ephemeral: bool) -> Result<FrozenRestoreInfo> {
        validate_bundle_dir(&bundle_dir)?;
        let manifest_path = bundle_dir.join("manifest.json");
        if !manifest_path.is_file() {
            bail!("no frozen snapshot manifest at {}", manifest_path.display());
        }
        let raw = fs::read(&manifest_path).with_context(|| format!("reading {}", manifest_path.display()))?;
        let manifest: FrozenBundleManifest = serde_json::from_slice(&raw).with_context(|| format!("parsing {}", manifest_path.display()))?;

        let restore_id = new_id();
        let container_name = match name {
            Some(n) => {
                let n = n.trim().to_string();
                if n.is_empty() || n.contains('/') || n.contains('\0') {
                    bail!("invalid container name `{n}`");
                }
                n
            }
            None => format!("{}-frozen-{}", sanitize_frozen_name(&manifest.source_container_name), &restore_id[..6.min(restore_id.len())]),
        };

        {
            let store = self.store.lock().unwrap();
            if store.name_taken(&container_name) {
                bail!("container name `{container_name}` is already in use");
            }
        }

        let dst_vm_dir = paths::vms_dir().join(&restore_id);
        fs::create_dir_all(&dst_vm_dir).with_context(|| format!("creating {}", dst_vm_dir.display()))?;

        let cleanup_on_err = || {
            let _ = fs::remove_dir_all(&dst_vm_dir);
        };

        let bundle_disk = bundle_dir.join(SCRATCH_DISK);
        if bundle_disk.is_file() {
            if let Err(err) = clone_sparse_file(&bundle_disk, &dst_vm_dir.join(SCRATCH_DISK)) {
                cleanup_on_err();
                return Err(err);
            }
        }

        let mut cs = manifest.spec.clone();
        cs.argv = vec!["sh".into(), "-c".into(), "while :; do sleep 3600; done".into()];
        cs.ports.clear();
        cs.auto_remove = ephemeral;
        cs.want_running = true;
        cs.restart = RestartPolicy::No;
        cs.hostname = Some(container_name.clone());
        cs.forked_from = Some(manifest.source_container_id.clone());
        cs.disk_source_id = manifest
            .spec
            .disk_source_id
            .clone()
            .or_else(|| bundle_disk.is_file().then(|| manifest.source_container_id.clone()));

        let ws_target = manifest.workspace_target.clone().unwrap_or_else(|| "/workspace".into());
        let mut rewritten_vols: Vec<Volume> = cs
            .volumes
            .into_iter()
            .filter(|v| v.name.is_some() || (v.target != ws_target && v.source.is_dir()))
            .collect();
        if let Some(rel) = &manifest.workspace_rel {
            let ws_src = bundle_dir.join(rel);
            if ws_src.is_dir() {
                let ws_dst = dst_vm_dir.join("workspace");
                if let Err(err) = clone_dir_cow(&ws_src, &ws_dst) {
                    cleanup_on_err();
                    return Err(err);
                }
                rewritten_vols.push(Volume {
                    source: ws_dst,
                    target: ws_target.clone(),
                    read_only: false,
                    name: None,
                });
            }
        }
        cs.volumes = rewritten_vols;

        let summary = ContainerSummary {
            id: restore_id.clone(),
            name: container_name.clone(),
            image: manifest.image_ref.clone(),
            status: ContainerStatus::Created,
            exit_code: None,
            ports: vec![],
            mounts: cs
                .volumes
                .iter()
                .map(|v| Mount {
                    source: v.name.clone().unwrap_or_else(|| v.source.display().to_string()),
                    target: v.target.clone(),
                    read_only: v.read_only,
                })
                .collect(),
            ip_address: None,
            cpu_percent: None,
            memory_bytes: None,
            memory_limit_bytes: Some(cs.memory_mib << 20),
            created_at: now_secs(),
            started_at: None,
            project: cs.project.clone(),
            service: cs.service.clone(),
            health: None,
            forked_from: Some(manifest.source_container_id.clone()),
        };

        {
            let mut store = self.store.lock().unwrap();
            if let Err(err) = store.insert(summary.clone()) {
                cleanup_on_err();
                return Err(err.into());
            }
            self.specs.lock().unwrap().insert(restore_id.clone(), cs);
        }
        self.save();
        self.refresh_images();
        self.events.record(
            graviton_proto::EventKind::Forked,
            Some((&summary.id, &summary.name)),
            Some(&summary.image),
            format!("{} restored from frozen snapshot", summary.name),
        );

        let launched = self.launch(&restore_id)?;
        let workdir = if manifest.workspace_rel.is_some() {
            ws_target
        } else if manifest.spec.cwd.is_empty() {
            "/".into()
        } else {
            manifest.spec.cwd
        };
        Ok(FrozenRestoreInfo {
            container: launched,
            workdir,
            exit_code: manifest.exit_code,
            failed_step: manifest.failed_step,
            source_container_name: manifest.source_container_name,
            has_vm_state: manifest.has_vm_state,
        })
    }

    /// Send a POSIX signal to a running container's main process.
    pub fn signal_container(&self, key: &str, signal: i32) -> Result<()> {
        let id = self.store.lock().unwrap().container(key)?.id.clone();
        let (agent, pid, vm, paused) = {
            let running = self.running.lock().unwrap();
            let Some(r) = running.get(&id) else {
                bail!("container {key} isn't running");
            };
            (Arc::clone(&r.agent), r.pid, Arc::clone(&r.vm), Arc::clone(&r.paused))
        };
        if paused.load(Ordering::Acquire) {
            if signal == 9 {
                vm.resume().context("resuming VM for SIGKILL")?;
                paused.store(false, Ordering::Release);
            } else {
                bail!("container {key} is paused");
            }
        }
        if matches!(signal, 9 | 15) {
            if let Some(spec) = self.specs.lock().unwrap().get_mut(&id) {
                spec.want_running = false;
            }
            self.cancel_restart(&id);
            self.stopping.lock().unwrap().insert(id.clone());
        }
        if let Some(pid) = pid {
            agent
                .lock()
                .unwrap()
                .write_all(&encode_line(&AgentRequest::Signal { pid, signal }))
                .context("sending signal to guest agent")?;
        }
        Ok(())
    }

    /// Remove a stopped container.
    pub fn remove(&self, key: &str) -> Result<String> {
        let removed = self.store.lock().unwrap().remove(key)?;
        let _ = self.log_tx.send(LogEvent::Ended { container_id: removed.id.clone(), exit_code: removed.exit_code });
        self.events.record(graviton_proto::EventKind::Removed, Some((&removed.id, &removed.name)), Some(&removed.image), format!("{} removed", removed.name));
        {
            self.specs.lock().unwrap().remove(&removed.id);
            self.cancel_restart(&removed.id);
        }
        self.stats.lock().unwrap().remove(&removed.id);
        self.health.lock().unwrap().remove(&removed.id);
        let _ = fs::remove_dir_all(paths::vms_dir().join(&removed.id));
        self.save();
        self.refresh_images();
        Ok(removed.id)
    }

    /// Abort any queued backoff restart for `id`; returns whether one was waiting.
    fn cancel_restart(&self, id: &str) -> bool {
        if let Some(handle) = self.pending_restarts.lock().unwrap().remove(id) {
            handle.abort();
            true
        } else {
            false
        }
    }

    /// Start the supervisor and wait until the process is running (or failed).
    fn launch(self: &Arc<Self>, id: &str) -> Result<ContainerSummary> {
        self.launch_inner(id, 0, true)?.ok_or_else(|| anyhow!("container {id} disappeared"))
    }

    /// Automatic start (daemon boot or restart-policy retry): doesn't block
    /// waiting for startup, and reschedules with backoff if port binding fails.
    fn launch_auto(self: &Arc<Self>, id: &str, attempt: u32) {
        if self.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        if self.running.lock().unwrap().contains_key(id) {
            return;
        }
        let policy = match self.specs.lock().unwrap().get(id) {
            Some(s) if s.want_running && s.restart != RestartPolicy::No => s.restart,
            _ => return,
        };
        if let Err(err) = self.launch_inner(id, attempt, false) {
            eprintln!("auto-start {id} failed: {err:#}");
            self.schedule_restart(id, policy, attempt);
        }
    }

    fn bind_ports(&self, id: &str, clear_want_on_err: bool) -> Result<Vec<BoundPort>> {
        let ports = self.specs.lock().unwrap().get(id).map(|s| s.ports.clone()).unwrap_or_default();
        // Bind the published ports first, so a port in use or invalid interface fails before a VM boots.
        let mut bound = Vec::new();
        for p in &ports {
            let bind_ip = p.bind_ip();
            let res = match p.protocol {
                PortProtocol::Tcp => TcpListener::bind((bind_ip, p.host_port))
                    .map(|listener| BoundPort::Tcp { listener, container_port: p.container_port }),
                PortProtocol::Udp => UdpSocket::bind((bind_ip, p.host_port))
                    .map(|socket| BoundPort::Udp { socket, container_port: p.container_port }),
            };
            match res {
                Ok(port) => bound.push(port),
                Err(err) => {
                    let proto = match p.protocol {
                        PortProtocol::Tcp => "tcp",
                        PortProtocol::Udp => "udp",
                    };
                    let host = if bind_ip.contains(':') { format!("[{bind_ip}]:{}", p.host_port) } else { format!("{bind_ip}:{}", p.host_port) };
                    let message = format!("couldn't bind {host}/{proto} on this Mac ({err})");
                    self.log(id, LogLevel::Error, LogStream::System, message.clone());
                    self.store.lock().unwrap().update(id, |c| {
                        c.status = ContainerStatus::Exited;
                        c.exit_code = Some(125);
                    });
                    let _ = self.log_tx.send(LogEvent::Ended { container_id: id.to_string(), exit_code: Some(125) });
                    if clear_want_on_err {
                        if let Some(spec) = self.specs.lock().unwrap().get_mut(id) {
                            spec.want_running = false;
                        }
                    }
                    self.save();
                    bail!(message);
                }
            }
        }
        Ok(bound)
    }

    fn launch_inner(self: &Arc<Self>, id: &str, attempt: u32, wait_for_start: bool) -> Result<Option<ContainerSummary>> {
        let bound = self.bind_ports(id, wait_for_start)?;

        self.store.lock().unwrap().update(id, |c| {
            c.status = ContainerStatus::Created;
            c.exit_code = None;
        });
        let engine = Arc::clone(self);
        let owned = id.to_string();
        std::thread::Builder::new().name(format!("container-{id}")).spawn(move || engine.supervise(&owned, bound, attempt, None))?;

        if !wait_for_start {
            return Ok(None);
        }

        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let c = self.store.lock().unwrap().container(id)?.clone();
            if c.status != ContainerStatus::Created {
                return Ok(Some(c));
            }
            if Instant::now() > deadline {
                bail!("container {} took too long to start", c.name);
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    /// Queue a backoff restart if `want_running` is still set and the daemon isn't stopping.
    fn schedule_restart(self: &Arc<Self>, id: &str, policy: RestartPolicy, attempt: u32) {
        let delay = restart_delay(attempt);
        let next_attempt = attempt.saturating_add(1);
        let engine = Arc::clone(self);
        let owned = id.to_string();
        // Hold `specs` before `pending_restarts` (same lock order as `stop`/`remove`).
        let specs = self.specs.lock().unwrap();
        if self.shutting_down.load(Ordering::SeqCst) || !specs.get(id).is_some_and(|s| s.want_running && s.restart != RestartPolicy::No) {
            return;
        }
        let mut pending = self.pending_restarts.lock().unwrap();
        if self.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        let handle = self.rt.spawn(async move {
            tokio::time::sleep(delay).await;
            engine.pending_restarts.lock().unwrap().remove(&owned);
            let e = Arc::clone(&engine);
            let cid = owned.clone();
            let _ = tokio::task::spawn_blocking(move || e.launch_auto(&cid, next_attempt)).await;
        });
        if let Some(prev) = pending.insert(id.to_string(), handle) {
            prev.abort();
        }
        drop(pending);
        drop(specs);
        self.log(
            id,
            LogLevel::Info,
            LogStream::System,
            format!("restarting in {}s (policy: {policy}, attempt {next_attempt})", delay.as_secs()),
        );
    }

    fn supervise(self: &Arc<Self>, id: &str, bound: Vec<BoundPort>, attempt: u32, tty: Option<TtyAttach>) {
        let mut started_at: Option<Instant> = None;
        let (tty_ready, tty_exit, stopped_tx) = match tty {
            Some(t) => (Some((t.rows, t.cols, t.ready_tx)), Some(t.exit_rx), Some(t.stopped_tx)),
            None => (None, None, None),
        };
        let mut ready_tx = tty_ready.as_ref().map(|(_, _, tx)| tx.clone());
        let result = self.run_in_vm(id, bound, &mut started_at, tty_ready, tty_exit);
        if let (Err(err), Some(tx)) = (&result, ready_tx.take()) {
            let _ = tx.send(Err(anyhow!("{err:#}")));
        }
        if self.shutting_down.load(Ordering::SeqCst) {
            // `Engine::shutdown` is tearing VMs down cleanly; preserve saved state.
            return;
        }
        let failure = result.as_ref().err().map(|e| format!("{e:#}"));
        let code = match result {
            Ok(code) => code,
            Err(err) => {
                self.log(id, LogLevel::Error, LogStream::System, format!("{err:#}"));
                125
            }
        };
        let requested = self.stopping.lock().unwrap().remove(id);
        let had_ip = {
            let mut running = self.running.lock().unwrap();
            if let Some(r) = running.get_mut(id) {
                let had = r.ip.take().is_some();
                r.dns = false;
                had
            } else {
                false
            }
        };
        if had_ip {
            self.sync_hosts();
        }
        if let Some(r) = self.running.lock().unwrap().remove(id) {
            for f in &r.forwards {
                f.abort();
            }
            if let Some(h) = &r.healthcheck {
                h.abort();
            }
            let _ = r.agent.lock().unwrap().write_all(&encode_line(&AgentRequest::Shutdown));
            if r.vm.wait_stopped(Duration::from_secs(5)).is_err() {
                let _ = r.vm.kill();
            }
        }
        self.health.lock().unwrap().remove(id);
        let (restart_policy, auto_remove) = {
            let mut specs = self.specs.lock().unwrap();
            if let Some(spec) = specs.get_mut(id) {
                let will_restart = spec.want_running && should_restart(spec.restart, requested, code, failure.is_some());
                if !will_restart {
                    spec.want_running = false;
                }
                (will_restart.then_some(spec.restart), spec.auto_remove)
            } else {
                (None, false)
            }
        };
        self.store.lock().unwrap().update(id, |c| {
            c.status = ContainerStatus::Exited;
            c.exit_code = Some(code);
            c.started_at = None;
            c.ip_address = None;
            c.cpu_percent = None;
            c.memory_bytes = None;
            c.health = None;
        });
        let _ = self.log_tx.send(LogEvent::Ended { container_id: id.to_string(), exit_code: Some(code) });
        self.save();
        let (name, image) = self.store.lock().unwrap().container(id).map(|c| (c.name.clone(), c.image.clone())).unwrap_or_default();
        let (kind, message) = match (&failure, requested, code) {
            (Some(why), _, _) => (graviton_proto::EventKind::Failed, format!("{name} couldn't start: {why}")),
            (None, true, _) => (graviton_proto::EventKind::Stopped, format!("{name} stopped")),
            (None, false, 0) => (graviton_proto::EventKind::Stopped, format!("{name} exited")),
            (None, false, code) => (graviton_proto::EventKind::Failed, format!("{name} exited with code {code}")),
        };
        self.events.record(kind, Some((id, &name)), Some(&image), message);
        if auto_remove && restart_policy.is_none() {
            let _ = self.remove(id);
        }
        if let Some(tx) = stopped_tx {
            let _ = tx.send(());
        }
        if let Some(policy) = restart_policy {
            let next_attempt = if started_at.is_some_and(|t| t.elapsed() >= HEALTHY_UPTIME) { 0 } else { attempt };
            self.schedule_restart(id, policy, next_attempt);
        }
    }

    fn run_in_vm(
        self: &Arc<Self>,
        id: &str,
        bound: Vec<BoundPort>,
        started_at: &mut Option<Instant>,
        tty_ready: Option<(u16, u16, std::sync::mpsc::SyncSender<Result<OwnedFd>>)>,
        tty_exit: Option<std::sync::mpsc::Receiver<i32>>,
    ) -> Result<i32> {
        // The container's disk stays for its next start; `grav rm` deletes it.
        self.run_in_vm_inner(id, bound, started_at, tty_ready, tty_exit)
    }

    fn run_in_vm_inner(
        self: &Arc<Self>,
        id: &str,
        bound: Vec<BoundPort>,
        started_at: &mut Option<Instant>,
        tty_ready: Option<(u16, u16, std::sync::mpsc::SyncSender<Result<OwnedFd>>)>,
        tty_exit: Option<std::sync::mpsc::Receiver<i32>>,
    ) -> Result<i32> {
        let spec = self.specs.lock().unwrap().get(id).cloned().ok_or_else(|| anyhow!("unknown container {id}"))?;
        let name = self.store.lock().unwrap().container(id)?.name.clone();
        let t0 = Instant::now();

        let guest = paths::guest_dir();
        let vm_dir = paths::vms_dir().join(id);
        fs::create_dir_all(&vm_dir)?;
        let rosetta = spec.image.architecture == "amd64" && graviton_image::guest_arch() != "amd64";
        if rosetta && !AppleHypervisor::rosetta_available() {
            bail!("linux/amd64 images need Apple Rosetta for Linux (run `softwareupdate --install-rosetta`)");
        }
        let erofs_disk = crate::erofs::ensure_image_disk(&self.blobs, &spec.image.layers)
            .context("preparing read-only image disk")?;
        let mut config = VmConfig::new(guest.join("vmlinux"), guest.join("initrd.img"), container_disk(&vm_dir)?);
        config.image_disk = Some(erofs_disk);
        config.cpus = spec.cpus;
        config.memory_mib = spec.memory_mib;
        let hardware_isolated = spec.egress.is_restricted();
        config.network = if hardware_isolated { NetworkMode::None } else { NetworkMode::Nat };
        config.console_log = Some(vm_dir.join("console.log"));
        config.rosetta = rosetta;
        config.balloon = true;
        config.multi_share_tag = Some("graviton".into());
        config.shares = vec![SharedDirectory { host_path: self.blobs.root().to_path_buf(), tag: "blobs".into(), read_only: true }];
        let mut mounts = Vec::new();
        for (i, v) in spec.volumes.iter().enumerate() {
            if v.name.is_some() {
                fs::create_dir_all(&v.source).with_context(|| format!("creating volume {}", v.source.display()))?;
            } else if !v.source.is_dir() {
                bail!("{} doesn't exist (or isn't a folder)", v.source.display());
            }
            let tag = format!("v{i}");
            config.shares.push(SharedDirectory { host_path: v.source.clone(), tag: tag.clone(), read_only: v.read_only });
            mounts.push(GuestMount { tag, target: v.target.clone(), read_only: v.read_only });
        }

        // A second network device on the container network, so containers reach each other.
        let port = self.switch.attach_to(spec.networks.iter().cloned()).context("joining the container network")?;
        if spec.chaos.is_active() {
            self.switch.set_port_chaos(port.id(), spec.chaos);
        }
        config.switch_port = Some(port.vm_fd());
        let (vm, snapshot_lease, from_snapshot) = match self.snapshots.try_restore(&config)? {
            Some((restored, lease)) => (Arc::new(restored), Some(lease), true),
            None => (Arc::new(AppleHypervisor::new().boot_vm(&config).context("booting the container's VM")?), None, false),
        };
        let fd = vm.connect(AGENT_VSOCK_PORT).context("connecting to the guest agent")?;
        let agent = Arc::new(Mutex::new(File::from(fd)));
        let mut events = Events::new(BufReader::new(agent.lock().unwrap().try_clone()?));
        match events.next()? {
            AgentEvent::Ready { .. } => {}
            other => bail!("expected ready from the agent, got {other:?}"),
        }
        let paused = Arc::new(AtomicBool::new(false));
        let policy = Arc::new(RwLock::new(ContainerNetworkPolicy {
            egress: spec.egress.clone(),
            chaos: spec.chaos,
            hardware_isolated,
        }));
        let xray_watcher_started = Arc::new(AtomicBool::new(false));
        let egress_bridge_started = Arc::new(AtomicBool::new(false));
        self.running.lock().unwrap().insert(
            id.to_string(),
            Running {
                name: name.clone(),
                project: spec.project.clone(),
                service: spec.service.clone(),
                dns_aliases: spec.dns_aliases.clone(),
                networks: spec.networks.clone(),
                vm: Arc::clone(&vm),
                agent: Arc::clone(&agent),
                paused: Arc::clone(&paused),
                pid: None,
                ip: None,
                dns: false,
                forwards: vec![],
                healthcheck: None,
                port: Some(port),
                _snapshot: snapshot_lease,
                network_policy: Arc::clone(&policy),
                xray_watcher_started: Arc::clone(&xray_watcher_started),
                egress_bridge_started: Arc::clone(&egress_bridge_started),
            },
        );
        if hardware_isolated {
            self.start_egress_bridge(id, Arc::clone(&vm), Arc::clone(&policy), &egress_bridge_started)?;
        }
        if self.is_xray_enabled(id) {
            self.start_xray_watcher(id, Arc::clone(&vm), &xray_watcher_started);
        }
        let booted = t0.elapsed();
        let host_name = spec.hostname.clone().unwrap_or_else(|| name.clone());

        let fork_from = spec.disk_source_id.clone();
        send(&agent, &AgentRequest::Prepare {
            container: id.into(),
            layers: spec.image.layers.iter().map(|l| l.digest.clone()).collect(),
            mounts,
            tmpfs: spec.tmpfs.clone(),
            hostname: Some(host_name.clone()),
            now_secs: from_snapshot.then(now_secs),
            fork_from: fork_from.clone(),
        })?;
        let (files, unpack_ms, ip, reused, dns) = match events.next()? {
            AgentEvent::Prepared { files, ms, ip, reused, dns, .. } => (files, ms, ip, reused, dns),
            AgentEvent::Error { message } => bail!("{message}"),
            other => bail!("expected prepared from the agent, got {other:?}"),
        };
        if fork_from.is_some() {
            if let Some(saved) = self.specs.lock().unwrap().get_mut(id) {
                saved.disk_source_id = None;
            }
            self.save();
        }
        if ip.is_some() {
            if let Some(r) = self.running.lock().unwrap().get_mut(id) {
                r.ip = ip.clone();
                r.dns = dns;
            }
            self.sync_hosts();
        }
        let address: Option<IpAddr> = ip.as_deref().and_then(|s| s.parse().ok());
        self.log(
            id,
            LogLevel::Info,
            LogStream::System,
            format!(
                "{} in {} ms{}; {}; {}",
                if from_snapshot { "VM restored from snapshot" } else { "VM booted" },
                booted.as_millis(),
                if rosetta { " (amd64 via Rosetta)" } else { "" },
                if reused { format!("kept its files from the last run ({unpack_ms} ms)") } else { format!("unpacked {files} files in {unpack_ms} ms") },
                match &ip {
                    Some(ip) => format!("network {ip}"),
                    None => "no network".into(),
                }
            ),
        );
        if !from_snapshot {
            self.snapshots.maybe_warm_background(self.blobs.root().to_path_buf(), spec.cpus, spec.memory_mib, rosetta);
        }

        // Publish ports: TCP uses vsock DialTcp (bypassing host Local Network Privacy),
        // falling back to guest IP when talking to an older agent; UDP still uses NAT IP.
        if !bound.is_empty() {
            let mut handles = Vec::new();
            let mut dropped_udp = false;
            for port in bound {
                match port {
                    BoundPort::Tcp { listener, container_port } => {
                        let fallback = address.map(|ip| SocketAddr::new(ip, container_port));
                        handles.push(self.forward_tcp(listener, Arc::clone(&vm), Arc::clone(&paused), container_port, fallback)?);
                    }
                    BoundPort::Udp { socket, container_port } => {
                        if let Some(ip) = address {
                            handles.push(self.forward_udp(socket, SocketAddr::new(ip, container_port))?);
                        } else {
                            dropped_udp = true;
                        }
                    }
                }
            }
            if dropped_udp {
                self.log(id, LogLevel::Warn, LogStream::System, "the VM got no network address, so its UDP ports aren't published".into());
            }
            if let Some(r) = self.running.lock().unwrap().get_mut(id) {
                r.forwards = handles;
            }
        }

        let exec_env = inject_proxy_env(&spec.env, hardware_isolated);
        if let (Some((rows, cols, ready_tx)), Some(exit_rx)) = (tty_ready, tty_exit) {
            // Interactive `grav run -it`: launch the main process on a dedicated PTY
            // vsock connection so `agent` stays free for SetHosts, Stats, Signal, and Shutdown.
            let mut tty_conn = File::from(vm.connect(AGENT_VSOCK_PORT).context("opening terminal connection")?);
            match read_event_unbuffered(&mut tty_conn)? {
                AgentEvent::Ready { .. } => {}
                other => bail!("expected ready from the agent, got {other:?}"),
            }
            tty_conn.write_all(&encode_line(&AgentRequest::Exec {
                argv: spec.argv.clone(),
                env: exec_env.clone(),
                cwd: spec.cwd.clone(),
                tty: true,
                container: Some(id.into()),
                user: spec.user.clone(),
                hostname: Some(host_name),
                rows,
                cols,
                stdin: None,
                stdin_pipe: false,
                mounts: vec![],
                read_only: spec.read_only,
                cap_add: spec.cap_add.clone(),
                cap_drop: spec.cap_drop.clone(),
                init: spec.init,
                shm_size_bytes: spec.shm_size_bytes,
                ulimits: spec.ulimits.clone(),
            }))?;
            let pid = match read_event_unbuffered(&mut tty_conn)? {
                AgentEvent::Started { pid } => pid,
                AgentEvent::Error { message } => bail!("{message}"),
                other => bail!("expected started from the agent, got {other:?}"),
            };
            *started_at = Some(Instant::now());
            if let Some(r) = self.running.lock().unwrap().get_mut(id) {
                r.pid = Some(pid);
            }
            self.store.lock().unwrap().update(id, |c| {
                c.status = ContainerStatus::Running;
                c.started_at = Some(now_secs());
                c.ip_address = ip.clone();
            });
            self.save();
            self.start_monitor(id, Arc::clone(&vm), Arc::clone(&paused));
            if let Some(hc) = spec.healthcheck.clone() {
                self.start_healthcheck(id, Arc::clone(&vm), Arc::clone(&paused), hc);
            }
            let (cname, image) = self.store.lock().unwrap().container(id).map(|c| (c.name.clone(), c.image.clone())).unwrap_or_default();
            self.events.record(graviton_proto::EventKind::Started, Some((id, &cname)), Some(&image), format!("{cname} started"));
            let _ = ready_tx.send(Ok(OwnedFd::from(tty_conn)));
            return Ok(exit_rx.recv().unwrap_or(129));
        }

        send(&agent, &AgentRequest::Exec {
            argv: spec.argv.clone(),
            env: exec_env,
            cwd: spec.cwd.clone(),
            tty: false,
            container: Some(id.into()),
            user: spec.user.clone(),
            hostname: Some(host_name),
            rows: 0,
            cols: 0,
            stdin: None,
            stdin_pipe: false,
            mounts: vec![],
            read_only: spec.read_only,
            cap_add: spec.cap_add.clone(),
            cap_drop: spec.cap_drop.clone(),
            init: spec.init,
            shm_size_bytes: spec.shm_size_bytes,
            ulimits: spec.ulimits.clone(),
        })?;

        let mut out = LineBuffer::default();
        let mut err = LineBuffer::default();
        loop {
            match events.next()? {
                AgentEvent::Started { pid } => {
                    *started_at = Some(Instant::now());
                    if let Some(r) = self.running.lock().unwrap().get_mut(id) {
                        r.pid = Some(pid);
                    }
                    self.store.lock().unwrap().update(id, |c| {
                        c.status = ContainerStatus::Running;
                        c.started_at = Some(now_secs());
                        c.ip_address = ip.clone();
                    });
                    self.save();
                    self.start_monitor(id, Arc::clone(&vm), Arc::clone(&paused));
                    if let Some(hc) = spec.healthcheck.clone() {
                        self.start_healthcheck(id, Arc::clone(&vm), Arc::clone(&paused), hc);
                    }
                    let (cname, image) = self.store.lock().unwrap().container(id).map(|c| (c.name.clone(), c.image.clone())).unwrap_or_default();
                    self.events.record(graviton_proto::EventKind::Started, Some((id, &cname)), Some(&image), format!("{cname} started"));
                }
                AgentEvent::Stdout { data, .. } => {
                    for line in out.push(&data) {
                        self.log(id, LogLevel::Info, LogStream::Stdout, line);
                    }
                }
                AgentEvent::Stderr { data, .. } => {
                    for line in err.push(&data) {
                        self.log(id, LogLevel::Info, LogStream::Stderr, line);
                    }
                }
                AgentEvent::Exited { code, .. } => {
                    if let Some(line) = out.finish() {
                        self.log(id, LogLevel::Info, LogStream::Stdout, line);
                    }
                    if let Some(line) = err.finish() {
                        self.log(id, LogLevel::Info, LogStream::Stderr, line);
                    }
                    return Ok(code);
                }
                AgentEvent::Error { message } => bail!("{message}"),
                _ => {}
            }
        }
    }

    /// Push the current container name → IP table to every running guest with DNS enabled.
    /// Containers in the same Compose project also resolve each other by service name
    /// and any configured `dns_aliases`, without colliding across projects.
    fn sync_hosts(&self) {
        struct LivePeer {
            id: String,
            name: String,
            ip: String,
            project: Option<String>,
            service: Option<String>,
            dns_aliases: Vec<String>,
            networks: HashSet<String>,
        }
        struct SyncTarget {
            id: String,
            project: Option<String>,
            networks: HashSet<String>,
            agent: Arc<Mutex<File>>,
        }
        let _turn = self.hosts_sync.lock().unwrap();
        let (peers, targets): (Vec<LivePeer>, Vec<SyncTarget>) = {
            let running = self.running.lock().unwrap();
            let peers = running
                .iter()
                .filter_map(|(id, r)| {
                    r.ip.as_ref().map(|ip| LivePeer {
                        id: id.clone(),
                        name: r.name.clone(),
                        ip: ip.clone(),
                        project: r.project.clone(),
                        service: r.service.clone(),
                        dns_aliases: r.dns_aliases.clone(),
                        networks: r.networks.iter().cloned().collect(),
                    })
                })
                .collect();
            let targets = running
                .iter()
                .filter(|(_, r)| r.dns && r.ip.is_some() && !r.paused.load(Ordering::Acquire))
                .map(|(id, r)| SyncTarget {
                    id: id.clone(),
                    project: r.project.clone(),
                    networks: r.networks.iter().cloned().collect(),
                    agent: Arc::clone(&r.agent),
                })
                .collect();
            (peers, targets)
        };
        if targets.is_empty() {
            return;
        }
        let generation = self.hosts_gen.fetch_add(1, Ordering::Relaxed) + 1;
        for SyncTarget { id: target_id, project: target_project, networks: target_nets, agent } in targets {
            let visible: Vec<&LivePeer> = peers
                .iter()
                .filter(|p| p.id == target_id || !target_nets.is_disjoint(&p.networks))
                .collect();
            // Canonical entries first so reverse PTR keeps the container's real name.
            let mut hosts: Vec<HostEntry> = visible
                .iter()
                .map(|p| HostEntry { name: p.name.clone(), id: p.id.clone(), ip: p.ip.clone() })
                .collect();
            if let Some(proj) = target_project.as_deref() {
                for p in visible {
                    if p.project.as_deref() != Some(proj) {
                        continue;
                    }
                    if let Some(svc) = p.service.as_deref() {
                        if !svc.eq_ignore_ascii_case(&p.name) {
                            hosts.push(HostEntry { name: svc.to_string(), id: String::new(), ip: p.ip.clone() });
                        }
                    }
                    for alias in &p.dns_aliases {
                        if !alias.eq_ignore_ascii_case(&p.name) {
                            hosts.push(HostEntry { name: alias.clone(), id: String::new(), ip: p.ip.clone() });
                        }
                    }
                }
            }
            let msg = encode_line(&AgentRequest::SetHosts { generation, hosts });
            let _ = agent.lock().unwrap().write_all(&msg);
        }
    }

    /// Forward TCP connections on `listener` to `127.0.0.1:<container_port>` inside the guest over vsock,
    /// falling back to `fallback` (guest NAT IP) only when talking to an older guest agent without `DialTcp`.
    fn forward_tcp(
        &self,
        listener: TcpListener,
        vm: Arc<AppleVm>,
        paused: Arc<AtomicBool>,
        container_port: u16,
        fallback: Option<SocketAddr>,
    ) -> Result<tokio::task::JoinHandle<()>> {
        listener.set_nonblocking(true)?;
        let _guard = self.rt.enter();
        let listener = tokio::net::TcpListener::from_std(listener)?;
        Ok(self.rt.spawn(async move {
            loop {
                let Ok((mut inbound, _)) = listener.accept().await else { continue };
                if paused.load(Ordering::Acquire) {
                    continue;
                }
                let vm = Arc::clone(&vm);
                tokio::spawn(async move {
                    let _ = inbound.set_nodelay(true);
                    match tokio::task::spawn_blocking(move || dial_guest_tcp(&vm, container_port)).await {
                        Ok(Ok(fd)) => {
                            let std_stream = std::os::unix::net::UnixStream::from(fd);
                            if std_stream.set_nonblocking(true).is_ok() {
                                if let Ok(mut guest) = tokio::net::UnixStream::from_std(std_stream) {
                                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut guest).await;
                                }
                            }
                        }
                        Ok(Err(err)) => {
                            let msg = format!("{err:#}");
                            if msg.contains("bad request") || msg.contains("unknown variant") {
                                if let Some(target) = fallback {
                                    match tokio::net::TcpStream::connect(target).await {
                                        Ok(mut outbound) => {
                                            let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                                        }
                                        Err(fb_err) => {
                                            eprintln!("port forward :{container_port} fallback {target}: {fb_err}");
                                        }
                                    }
                                    return;
                                }
                            }
                            if !msg.contains("Connection refused") {
                                eprintln!("port forward :{container_port}: {msg}");
                            }
                        }
                        Err(err) => {
                            eprintln!("port forward :{container_port} task error: {err}");
                        }
                    }
                });
            }
        }))
    }

    /// Forward UDP datagrams on `socket` to `target` in the guest, giving each
    /// external client its own ephemeral guest-facing socket so replies route
    /// back cleanly and stateful protocols see distinct client endpoints.
    fn forward_udp(&self, socket: UdpSocket, target: SocketAddr) -> Result<tokio::task::JoinHandle<()>> {
        spawn_udp_forward(&self.rt, socket, target)
    }

    // -- Stats ------------------------------------------------------------

    /// Sample the container every [`STATS_INTERVAL`] on its own agent
    /// connection, until its VM stops.
    fn start_monitor(self: &Arc<Self>, id: &str, vm: Arc<AppleVm>, paused: Arc<AtomicBool>) {
        let engine = Arc::clone(self);
        let id = id.to_string();
        let (spec_cpus, limit) = self
            .specs
            .lock()
            .unwrap()
            .get(&id)
            .map_or((DEFAULT_CPUS, 0), |s| (s.cpus, s.memory_mib << 20));
        let _ = std::thread::Builder::new().name(format!("stats-{id}")).spawn(move || {
            let Ok(fd) = vm.connect(AGENT_VSOCK_PORT) else { return };
            let mut conn = File::from(fd);
            if !matches!(read_event_unbuffered(&mut conn), Ok(AgentEvent::Ready { .. })) {
                return;
            }
            let mut events = Events::new(BufReader::new(match conn.try_clone() {
                Ok(c) => c,
                Err(_) => return,
            }));
            {
                let mut stats = engine.stats.lock().unwrap();
                let history = stats.entry(id.clone()).or_default();
                if history.cpus == 0 {
                    history.cpus = spec_cpus;
                }
                history.processes.clear();
            }
            let mut previous: Option<GuestStats> = None;
            let min_allowed = AppleHypervisor::minimum_allowed_memory_bytes();
            let mut balloon = BalloonController::new(limit, min_allowed);
            loop {
                let still_running = engine.running.lock().unwrap().get(&id).is_some_and(|r| Arc::ptr_eq(&r.vm, &vm));
                if !still_running {
                    break;
                }
                if paused.load(Ordering::Acquire) {
                    previous = None;
                    std::thread::sleep(Duration::from_millis(250));
                    continue;
                }
                if conn.write_all(&encode_line(&AgentRequest::Stats { processes: true })).is_err() {
                    break;
                }
                let Ok(AgentEvent::Stats(now)) = events.next() else { break };
                if let Some(target) = balloon.update(&now) {
                    vm.set_balloon_target_bytes(target);
                }
                let sample = match &previous {
                    Some(prev) => sample_between(prev, &now, limit),
                    None => initial_sample(&now, limit),
                };
                let cpus = now.cpus;
                engine.store.lock().unwrap().update(&id, |c| {
                    c.cpu_percent = Some(sample.cpu_percent);
                    c.memory_bytes = Some(sample.memory_bytes);
                });
                {
                    let mut stats = engine.stats.lock().unwrap();
                    let history = stats.entry(id.clone()).or_default();
                    history.cpus = cpus;
                    history.samples.push_back(sample.clone());
                    while history.samples.len() > STATS_KEPT {
                        history.samples.pop_front();
                    }
                    history.processes = now.processes.clone();
                }
                crate::persistence::append_stat_sample(&stats_path(&id), &sample, STATS_KEPT);
                let _ = engine.stats_tx.send(StatsEvent::Sample { container_id: id.clone() });
                previous = Some(now);
                std::thread::sleep(STATS_INTERVAL);
            }
            let _ = engine.stats_tx.send(StatsEvent::Stopped { container_id: id });
        });
    }

    fn start_healthcheck(self: &Arc<Self>, id: &str, vm: Arc<AppleVm>, paused: Arc<AtomicBool>, hc: Healthcheck) {
        let Some(argv) = hc.exec_command() else { return };
        let Some(cs) = self.specs.lock().unwrap().get(id).cloned() else { return };
        let engine = Arc::clone(self);
        let id_owned = id.to_string();
        {
            self.health.lock().unwrap().insert(id_owned.clone(), HealthState::default());
            self.store.lock().unwrap().update(&id_owned, |c| {
                if c.status == ContainerStatus::Running {
                    c.health = Some(HealthStatus::Starting);
                }
            });
        }
        let started_wall = Instant::now();
        let interval = Duration::from_secs(hc.interval_secs.max(1));
        let timeout_dur = Duration::from_secs(hc.timeout_secs.max(1));
        let start_period = Duration::from_secs(hc.start_period_secs);
        let vm_for_task = Arc::clone(&vm);
        let handle = self.rt.spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let (agent, cname, image, hostname) = {
                    let running = engine.running.lock().unwrap();
                    let Some(r) = running.get(&id_owned).filter(|r| Arc::ptr_eq(&r.vm, &vm_for_task)) else {
                        break;
                    };
                    if r.paused.load(Ordering::Acquire) {
                        continue;
                    }
                    let (cname, image) = engine
                        .store
                        .lock()
                        .unwrap()
                        .container(&id_owned)
                        .map(|c| (c.name.clone(), c.image.clone()))
                        .unwrap_or_default();
                    let hostname = cs.hostname.clone().unwrap_or_else(|| r.name.clone());
                    (Arc::clone(&r.agent), cname, image, hostname)
                };

                let probe_pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
                let pid_ref = Arc::clone(&probe_pid);
                let vm_for_probe = Arc::clone(&vm_for_task);
                let id_for_probe = id_owned.clone();
                let cs_for_probe = cs.clone();
                let argv_for_probe = argv.clone();
                let start = now_secs();

                let fut = tokio::task::spawn_blocking(move || -> Result<(i32, String)> {
                    let (pid, conn) = exec_probe_on_vm(&vm_for_probe, &id_for_probe, &cs_for_probe, &hostname, &argv_for_probe)?;
                    pid_ref.store(pid, Ordering::Release);
                    let mut events = Events::new(BufReader::new(conn));
                    let mut output = Vec::new();
                    loop {
                        match events.next()? {
                            AgentEvent::Stdout { data, .. } | AgentEvent::Stderr { data, .. } => {
                                if output.len() < 4096 {
                                    let take = data.len().min(4096 - output.len());
                                    output.extend_from_slice(&data[..take]);
                                }
                            }
                            AgentEvent::Exited { code, .. } => {
                                return Ok((code, truncate_health_output(&output)));
                            }
                            AgentEvent::Error { message } => bail!("{message}"),
                            _ => {}
                        }
                    }
                });

                let (exit_code, output) = match tokio::time::timeout(timeout_dur, fut).await {
                    Ok(Ok(Ok(res))) => res,
                    Ok(Ok(Err(err))) => {
                        let still_running = engine
                            .running
                            .lock()
                            .unwrap()
                            .get(&id_owned)
                            .is_some_and(|r| Arc::ptr_eq(&r.vm, &vm_for_task));
                        if !still_running {
                            break;
                        }
                        if paused.load(Ordering::Acquire) {
                            continue;
                        }
                        (-1, format!("healthcheck error: {err:#}"))
                    }
                    Ok(Err(err)) => {
                        let still_running = engine
                            .running
                            .lock()
                            .unwrap()
                            .get(&id_owned)
                            .is_some_and(|r| Arc::ptr_eq(&r.vm, &vm_for_task));
                        if !still_running {
                            break;
                        }
                        if paused.load(Ordering::Acquire) {
                            continue;
                        }
                        (-1, format!("healthcheck task panicked: {err}"))
                    }
                    Err(_elapsed) => {
                        if paused.load(Ordering::Acquire) {
                            continue;
                        }
                        let pid = probe_pid.load(Ordering::Acquire);
                        if pid > 0 {
                            let _ = agent.lock().unwrap().write_all(&encode_line(&AgentRequest::Signal {
                                pid,
                                signal: libc::SIGKILL,
                            }));
                        }
                        (-1, format!("healthcheck timed out after {}s", hc.timeout_secs.max(1)))
                    }
                };

                if paused.load(Ordering::Acquire) {
                    continue;
                }
                let end = now_secs().max(start);
                let still_running = engine
                    .running
                    .lock()
                    .unwrap()
                    .get(&id_owned)
                    .is_some_and(|r| Arc::ptr_eq(&r.vm, &vm_for_task));
                if !still_running {
                    break;
                }

                let in_start_period = !start_period.is_zero() && started_wall.elapsed() < start_period;
                let entry = HealthLog { start, end, exit_code, output };
                let (transition, new_status, streak) = {
                    let mut map = engine.health.lock().unwrap();
                    let state = map.entry(id_owned.clone()).or_default();
                    let transition = apply_health_probe(state, &hc, in_start_period, entry);
                    (transition, state.status, state.failing_streak)
                };
                engine.store.lock().unwrap().update(&id_owned, |c| {
                    if matches!(c.status, ContainerStatus::Running | ContainerStatus::Paused) {
                        c.health = Some(new_status);
                    }
                });
                match transition {
                    Some(HealthStatus::Healthy) => {
                        engine.log(&id_owned, LogLevel::Info, LogStream::System, "container is healthy".into());
                        engine.events.record(
                            graviton_proto::EventKind::Healthy,
                            Some((&id_owned, &cname)),
                            Some(&image),
                            format!("{cname} is healthy"),
                        );
                    }
                    Some(HealthStatus::Unhealthy) => {
                        engine.log(
                            &id_owned,
                            LogLevel::Warn,
                            LogStream::System,
                            format!("container is unhealthy ({streak} consecutive failures)"),
                        );
                        engine.events.record(
                            graviton_proto::EventKind::Unhealthy,
                            Some((&id_owned, &cname)),
                            Some(&image),
                            format!("{cname} is unhealthy"),
                        );
                    }
                    _ => {}
                }
            }
        });
        if let Some(r) = self.running.lock().unwrap().get_mut(id) {
            if Arc::ptr_eq(&r.vm, &vm) {
                r.healthcheck = Some(handle);
                return;
            }
        }
        handle.abort();
    }

    pub fn stats(&self, key: &str) -> Result<ContainerStats> {
        let id = self.store.lock().unwrap().container(key)?.id.clone();
        let cpus = self.specs.lock().unwrap().get(&id).map_or(0, |s| s.cpus);
        let running = self.running.lock().unwrap().contains_key(&id);
        let stats = self.stats.lock().unwrap();
        let history = stats.get(&id);
        Ok(ContainerStats {
            id,
            cpus: history.and_then(|h| (h.cpus > 0).then_some(h.cpus)).unwrap_or(cpus).max(1),
            interval_secs: STATS_INTERVAL.as_secs() as u32,
            samples: history.map(|h| h.samples.iter().cloned().collect()).unwrap_or_default(),
            processes: if running { history.map(|h| h.processes.clone()).unwrap_or_default() } else { vec![] },
        })
    }

    pub fn stats_rows(&self, ids: &[String], all: bool) -> Result<Vec<ContainerStatRow>> {
        let store = self.store.lock().unwrap();
        let selected: Vec<ContainerSummary> = if !ids.is_empty() {
            ids.iter()
                .map(|key| store.container(key).cloned().map_err(anyhow::Error::from))
                .collect::<Result<Vec<_>>>()?
        } else {
            store
                .containers()
                .iter()
                .filter(|c| all || matches!(c.status, ContainerStatus::Running | ContainerStatus::Paused))
                .cloned()
                .collect()
        };
        drop(store);

        let running = self.running.lock().unwrap();
        let stats = self.stats.lock().unwrap();
        let specs = self.specs.lock().unwrap();
        let now = now_ms();
        Ok(selected
            .into_iter()
            .map(|c| {
                let active = running.contains_key(&c.id) && matches!(c.status, ContainerStatus::Running | ContainerStatus::Paused);
                let spec = specs.get(&c.id);
                let history = stats.get(&c.id);
                let spec_cpus = spec.map_or(DEFAULT_CPUS, |s| s.cpus);
                let cpus = history.and_then(|h| (h.cpus > 0).then_some(h.cpus)).unwrap_or(spec_cpus).max(1);
                let spec_limit = spec.map_or(0, |s| s.memory_mib << 20);
                let last = history.and_then(|h| h.samples.back());
                if active {
                    ContainerStatRow {
                        id: c.id,
                        name: c.name,
                        status: c.status,
                        cpus,
                        cpu_percent: last.map_or(c.cpu_percent.unwrap_or(0.0), |s| s.cpu_percent),
                        memory_bytes: last.map_or(c.memory_bytes.unwrap_or(0), |s| s.memory_bytes),
                        memory_limit_bytes: last
                            .map(|s| s.memory_limit_bytes)
                            .filter(|&l| l > 0)
                            .or(c.memory_limit_bytes)
                            .unwrap_or(spec_limit),
                        net_rx_bytes: last.map_or(0, |s| s.net_rx_bytes),
                        net_tx_bytes: last.map_or(0, |s| s.net_tx_bytes),
                        pids: history.map_or(0, |h| h.processes.len() as u32),
                        timestamp_ms: last.map_or(now, |s| s.timestamp_ms),
                    }
                } else {
                    ContainerStatRow {
                        id: c.id,
                        name: c.name,
                        status: c.status,
                        cpus,
                        cpu_percent: 0.0,
                        memory_bytes: 0,
                        memory_limit_bytes: c.memory_limit_bytes.unwrap_or(spec_limit),
                        net_rx_bytes: 0,
                        net_tx_bytes: 0,
                        pids: 0,
                        timestamp_ms: now,
                    }
                }
            })
            .collect())
    }

    // -- Files and details --------------------------------------------------

    /// Return a cloned VM handle only when container `id` is live and unpaused.
    fn active_vm(&self, id: &str, key: &str) -> Result<Arc<AppleVm>> {
        let running = self.running.lock().unwrap();
        let Some(r) = running.get(id) else {
            bail!("container {key} isn't running");
        };
        if r.paused.load(Ordering::Acquire) {
            bail!("container {key} is paused");
        }
        Ok(Arc::clone(&r.vm))
    }

    /// Send one request on a fresh agent connection and return its answer.
    fn agent_call(&self, key: &str, request: impl FnOnce(String) -> AgentRequest) -> Result<AgentEvent> {
        let id = self.store.lock().unwrap().container(key)?.id.clone();
        let vm = self.active_vm(&id, key)?;
        let mut conn = File::from(vm.connect(AGENT_VSOCK_PORT)?);
        match read_event_unbuffered(&mut conn)? {
            AgentEvent::Ready { .. } => {}
            other => bail!("expected ready from the agent, got {other:?}"),
        }
        conn.write_all(&encode_line(&request(id)))?;
        match Events::new(BufReader::new(conn)).next()? {
            AgentEvent::Error { message } => bail!("{message}"),
            event => Ok(event),
        }
    }

    pub fn list_files(&self, key: &str, path: &str) -> Result<DirectoryListing> {
        match self.agent_call(key, |container| AgentRequest::ListDir { container, path: path.to_string() })? {
            AgentEvent::Dir(listing) => Ok(listing),
            other => bail!("expected a folder listing from the agent, got {other:?}"),
        }
    }

    pub fn read_file(&self, key: &str, path: &str) -> Result<FileContents> {
        match self.agent_call(key, |container| AgentRequest::ReadFile { container, path: path.to_string(), limit: READ_LIMIT })? {
            AgentEvent::File(contents) => Ok(contents),
            other => bail!("expected a file from the agent, got {other:?}"),
        }
    }

    /// Open a dedicated agent connection streaming a tar archive of `path` out of a running container.
    /// Returns `(is_dir, name, fd)` positioned immediately after `ArchiveReady\n`.
    pub fn get_archive_open(&self, key: &str, path: &str) -> Result<(bool, String, OwnedFd)> {
        let id = self.store.lock().unwrap().container(key)?.id.clone();
        let vm = self.active_vm(&id, key)?;
        let mut conn = File::from(vm.connect(AGENT_VSOCK_PORT)?);
        match read_event_unbuffered(&mut conn)? {
            AgentEvent::Ready { .. } => {}
            other => bail!("expected ready from the agent, got {other:?}"),
        }
        conn.write_all(&encode_line(&AgentRequest::GetArchive { container: id, path: path.to_string() }))?;
        match read_event_unbuffered(&mut conn)? {
            AgentEvent::ArchiveReady { is_dir, name } => Ok((is_dir, name, OwnedFd::from(conn))),
            AgentEvent::Error { message } => bail!("{message}"),
            other => bail!("expected archive header from the agent, got {other:?}"),
        }
    }

    /// Open a dedicated agent connection ready to receive `size` bytes of tar to unpack into `path`.
    pub fn put_archive_open(&self, key: &str, path: &str, size: u64) -> Result<OwnedFd> {
        let id = self.store.lock().unwrap().container(key)?.id.clone();
        let vm = self.active_vm(&id, key)?;
        let mut conn = File::from(vm.connect(AGENT_VSOCK_PORT)?);
        match read_event_unbuffered(&mut conn)? {
            AgentEvent::Ready { .. } => {}
            other => bail!("expected ready from the agent, got {other:?}"),
        }
        conn.write_all(&encode_line(&AgentRequest::PutArchive { container: id, path: path.to_string(), size }))?;
        Ok(OwnedFd::from(conn))
    }

    pub fn inspect(&self, key: &str) -> Result<ContainerDetails> {
        let container = self.store.lock().unwrap().container(key)?.clone();
        let spec = self.specs.lock().unwrap().get(&container.id).cloned().ok_or_else(|| anyhow!("container {key} has no saved settings"))?;
        let health = self.health.lock().unwrap().get(&container.id).cloned();
        let network_policy = self
            .running
            .lock()
            .unwrap()
            .get(&container.id)
            .map(|r| r.network_policy.read().unwrap().clone())
            .unwrap_or_else(|| ContainerNetworkPolicy {
                egress: spec.egress.clone(),
                chaos: spec.chaos,
                hardware_isolated: false,
            });
        let xray_enabled = self.is_xray_enabled(&container.id);
        Ok(ContainerDetails {
            image_reference: spec.image.reference.clone(),
            image_digest: spec.image.manifest_digest.clone(),
            command: spec.argv,
            env: spec.env,
            working_dir: spec.cwd,
            user: spec.user,
            hostname: spec.hostname.unwrap_or_else(|| container.name.clone()),
            cpus: spec.cpus,
            memory_mib: spec.memory_mib,
            kernel: detect_guest_kernel(),
            vm_dir: paths::vms_dir().join(&container.id).display().to_string(),
            restart: spec.restart,
            architecture: Some(spec.image.architecture.clone()),
            rosetta: spec.image.architecture == "amd64" && graviton_image::guest_arch() != "amd64",
            project: spec.project,
            service: spec.service,
            labels: spec.labels,
            healthcheck: spec.healthcheck,
            health,
            read_only: spec.read_only,
            cap_add: spec.cap_add,
            cap_drop: spec.cap_drop,
            tmpfs: spec.tmpfs,
            init: spec.init,
            networks: spec.networks,
            shm_size_bytes: spec.shm_size_bytes,
            ulimits: spec.ulimits,
            network_policy: Some(network_policy),
            xray_enabled,
            container,
        })
    }

    // -- Hypervisor X-Ray, Egress Firewall & Chaos --------------------------

    fn is_xray_enabled(&self, id: &str) -> bool {
        self.xray.lock().unwrap().get(id).is_some_and(|x| x.enabled)
    }

    pub(crate) fn push_xray_event(&self, id: &str, mut ev: XRayEvent) {
        let mut map = self.xray.lock().unwrap();
        let entry = map.entry(id.to_string()).or_default();
        if !entry.enabled && ev.allowed {
            return;
        }
        entry.next_seq += 1;
        ev.seq = entry.next_seq;
        if entry.events.len() >= MAX_XRAY_EVENTS {
            entry.events.pop_front();
        }
        entry.events.push_back(ev.clone());
        drop(map);
        let _ = self.xray_tx.send((id.to_string(), ev));
    }

    fn start_egress_bridge(
        self: &Arc<Self>,
        id: &str,
        vm: Arc<AppleVm>,
        policy: Arc<RwLock<ContainerNetworkPolicy>>,
        started: &AtomicBool,
    ) -> Result<()> {
        if started.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let mut bridge_conn = File::from(vm.connect(AGENT_VSOCK_PORT).context("connecting egress bridge")?);
        match read_event_unbuffered(&mut bridge_conn)? {
            AgentEvent::Ready { .. } => {}
            other => bail!("expected ready on egress bridge, got {other:?}"),
        }
        bridge_conn.write_all(&encode_line(&AgentRequest::EgressBridge))?;
        bridge_conn.flush()?;
        let engine = Arc::clone(self);
        let cid = id.to_string();
        let emit: Arc<dyn Fn(XRayEvent) + Send + Sync> = Arc::new(move |ev| engine.push_xray_event(&cid, ev));
        crate::egress::spawn_gateway(vm, OwnedFd::from(bridge_conn), policy, emit);
        Ok(())
    }

    fn start_xray_watcher(self: &Arc<Self>, id: &str, vm: Arc<AppleVm>, started: &AtomicBool) {
        if started.swap(true, Ordering::SeqCst) {
            return;
        }
        let engine = Arc::clone(self);
        let cid = id.to_string();
        let _ = std::thread::Builder::new().name(format!("xray-watch-{id}")).spawn(move || {
            let Ok(fd) = vm.connect(AGENT_VSOCK_PORT) else { return };
            let mut conn = File::from(fd);
            if !matches!(read_event_unbuffered(&mut conn), Ok(AgentEvent::Ready { .. })) {
                return;
            }
            if conn.write_all(&encode_line(&AgentRequest::WatchXRay { container: cid.clone() })).is_err()
                || conn.flush().is_err()
            {
                return;
            }
            let mut events = Events::new(BufReader::new(conn));
            while let Ok(ev) = events.next() {
                match ev {
                    AgentEvent::XRayBatch { events } => {
                        for item in events {
                            engine.push_xray_event(&cid, item);
                        }
                    }
                    AgentEvent::Error { .. } => break,
                    _ => {}
                }
            }
        });
    }

    pub fn get_network_policy(&self, key: &str) -> Result<ContainerNetworkPolicy> {
        let c = self.store.lock().unwrap().container(key)?.clone();
        if let Some(r) = self.running.lock().unwrap().get(&c.id) {
            return Ok(r.network_policy.read().unwrap().clone());
        }
        let spec = self.specs.lock().unwrap().get(&c.id).cloned().ok_or_else(|| anyhow!("missing spec for {}", c.name))?;
        Ok(ContainerNetworkPolicy { egress: spec.egress, chaos: spec.chaos, hardware_isolated: false })
    }

    pub fn set_network_policy(
        self: &Arc<Self>,
        key: &str,
        egress: Option<EgressPolicy>,
        chaos: Option<ChaosProfile>,
    ) -> Result<ContainerNetworkPolicy> {
        let id = self.store.lock().unwrap().container(key)?.id.clone();
        let (new_egress, new_chaos) = {
            let mut specs = self.specs.lock().unwrap();
            let spec = specs.get_mut(&id).ok_or_else(|| anyhow!("missing spec for {key}"))?;
            if let Some(e) = egress {
                spec.egress = e;
            }
            if let Some(ch) = chaos {
                spec.chaos = ch;
            }
            (spec.egress.clone(), spec.chaos)
        };
        self.save();

        let running_entry = {
            let running = self.running.lock().unwrap();
            running.get(&id).map(|r| {
                (
                    Arc::clone(&r.vm),
                    Arc::clone(&r.network_policy),
                    r.port.as_ref().map(|p| p.id()),
                    Arc::clone(&r.egress_bridge_started),
                    r.paused.load(Ordering::Acquire),
                )
            })
        };
        if let Some((vm, pol_lock, port_id, bridge_started, paused)) = running_entry {
            let was_isolated = pol_lock.read().unwrap().hardware_isolated;
            {
                let mut g = pol_lock.write().unwrap();
                g.egress = new_egress.clone();
                g.chaos = new_chaos;
            }
            if let Some(pid) = port_id {
                self.switch.set_port_chaos(pid, new_chaos);
            }
            if new_egress.is_restricted() && !paused {
                let _ = self.start_egress_bridge(&id, vm, Arc::clone(&pol_lock), &bridge_started);
                if !was_isolated {
                    let _ = self.agent_call(&id, |_| AgentRequest::DropDefaultRoute);
                }
            }
            return Ok(pol_lock.read().unwrap().clone());
        }
        Ok(ContainerNetworkPolicy { egress: new_egress, chaos: new_chaos, hardware_isolated: false })
    }

    pub fn set_xray(self: &Arc<Self>, key: &str, enabled: bool) -> Result<bool> {
        let id = self.store.lock().unwrap().container(key)?.id.clone();
        {
            let mut map = self.xray.lock().unwrap();
            map.entry(id.clone()).or_default().enabled = enabled;
        }
        let running = self.running.lock().unwrap().get(&id).map(|r| {
            (Arc::clone(&r.vm), Arc::clone(&r.xray_watcher_started), r.paused.load(Ordering::Acquire))
        });
        if let Some((vm, started, paused)) = running {
            if !paused {
                if enabled {
                    self.start_xray_watcher(&id, vm, &started);
                }
                let _ = self.agent_call(&id, |cid| AgentRequest::SetXRay { container: cid, enabled });
            }
        }
        Ok(enabled)
    }

    pub fn xray_events(&self, key: &str, limit: Option<usize>) -> Result<Vec<XRayEvent>> {
        let id = self.store.lock().unwrap().container(key)?.id.clone();
        let map = self.xray.lock().unwrap();
        let all: Vec<XRayEvent> = map.get(&id).map(|x| x.events.iter().cloned().collect()).unwrap_or_default();
        let take = limit.unwrap_or(200);
        let start = all.len().saturating_sub(take);
        Ok(all[start..].to_vec())
    }

    pub fn subscribe_xray(
        &self,
        key: &str,
        limit: Option<usize>,
    ) -> Result<XRaySubscription> {
        let id = self.store.lock().unwrap().container(key)?.id.clone();
        let rx = self.xray_tx.subscribe();
        let history = self.xray_events(&id, limit)?;
        Ok((id, history, rx))
    }

    // -- volumes & network ------------------------------------------------

    pub fn list_volumes(&self) -> Vec<VolumeSummary> {
        let mut users: HashMap<String, Vec<VolumeMountRef>> = HashMap::new();
        {
            let specs = self.specs.lock().unwrap();
            let store = self.store.lock().unwrap();
            for (id, spec) in specs.iter() {
                let Ok(container) = store.container(id) else { continue };
                let running = matches!(container.status, ContainerStatus::Running | ContainerStatus::Paused);
                for v in &spec.volumes {
                    let Some(name) = &v.name else { continue };
                    users.entry(name.clone()).or_default().push(VolumeMountRef {
                        container_id: container.id.clone(),
                        container_name: container.name.clone(),
                        target: v.target.clone(),
                        read_only: v.read_only,
                        running,
                    });
                }
            }
        }
        let root = paths::volumes_dir();
        let mut names: HashSet<String> = users.keys().cloned().collect();
        if let Ok(entries) = fs::read_dir(&root) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if is_valid_volume_name(&name) && entry.file_type().is_ok_and(|t| t.is_dir()) {
                    names.insert(name);
                }
            }
        }
        let mut sorted: Vec<String> = names.into_iter().collect();
        sorted.sort();
        sorted
            .into_iter()
            .map(|name| {
                let dir = root.join(&name);
                let (size_bytes, file_count, created_at) = volume_usage(&dir);
                let mut used_by = users.remove(&name).unwrap_or_default();
                used_by.sort_by(|a, b| b.running.cmp(&a.running).then_with(|| a.container_name.cmp(&b.container_name)));
                VolumeSummary { name, path: dir.display().to_string(), size_bytes, file_count, created_at, used_by }
            })
            .collect()
    }

    pub fn create_volume(&self, raw_name: &str) -> Result<VolumeSummary> {
        let name = raw_name.trim();
        if !is_valid_volume_name(name) {
            bail!("`{name}` isn't a valid volume name (start with a letter or digit, then letters, digits, _ . -)");
        }
        let root = paths::volumes_dir();
        fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        let dir = root.join(name);
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => bail!("volume {name} already exists"),
            Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
        }
        let (size_bytes, file_count, created_at) = volume_usage(&dir);
        Ok(VolumeSummary { name: name.to_string(), path: dir.display().to_string(), size_bytes, file_count, created_at, used_by: vec![] })
    }

    pub fn remove_volume(&self, raw_name: &str) -> Result<(String, u64)> {
        let name = raw_name.trim();
        if !is_valid_volume_name(name) {
            bail!("`{name}` isn't a valid volume name");
        }
        let users: Vec<String> = {
            let specs = self.specs.lock().unwrap();
            let store = self.store.lock().unwrap();
            let mut list: Vec<String> = specs
                .iter()
                .filter(|(_, s)| s.volumes.iter().any(|v| v.name.as_deref() == Some(name)))
                .filter_map(|(id, _)| store.container(id).ok().map(|c| c.name.clone()))
                .collect();
            list.sort();
            list.dedup();
            list
        };
        if !users.is_empty() {
            bail!(
                "volume {name} is used by {}: remove {} first (grav rm)",
                users.join(", "),
                if users.len() == 1 { "it" } else { "them" }
            );
        }
        let dir = paths::volumes_dir().join(name);
        if !dir.is_dir() {
            bail!("no volume {name} here");
        }
        let (freed_bytes, _, _) = volume_usage(&dir);
        fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        self.events.record(graviton_proto::EventKind::Removed, None, None, format!("volume {name} removed"));
        Ok((name.to_string(), freed_bytes))
    }

    pub fn list_networks(&self) -> Vec<UserNetwork> {
        let mut attached: HashMap<String, Vec<String>> = HashMap::new();
        {
            let store = self.store.lock().unwrap();
            let specs = self.specs.lock().unwrap();
            for c in store.containers() {
                let nets = specs
                    .get(&c.id)
                    .map(|s| s.networks.as_slice())
                    .filter(|n| !n.is_empty())
                    .unwrap_or(&[]);
                if nets.is_empty() {
                    attached.entry(DEFAULT_NETWORK_NAME.into()).or_default().push(c.name.clone());
                } else {
                    for net in nets {
                        attached.entry(net.clone()).or_default().push(c.name.clone());
                    }
                }
            }
        }
        for names in attached.values_mut() {
            names.sort();
        }
        let mut out = vec![UserNetwork {
            id: "graviton-bridge".into(),
            name: DEFAULT_NETWORK_NAME.into(),
            driver: "L2 switch + NAT".into(),
            internal: false,
            builtin: true,
            created_at: 0,
            labels: BTreeMap::new(),
            containers: attached.remove(DEFAULT_NETWORK_NAME).unwrap_or_default(),
        }];
        let nets = self.networks.lock().unwrap();
        for net in nets.values() {
            out.push(UserNetwork {
                id: net.id.clone(),
                name: net.name.clone(),
                driver: "bridge".into(),
                internal: net.internal,
                builtin: false,
                created_at: net.created_at,
                labels: net.labels.clone(),
                containers: attached.remove(&net.name).unwrap_or_default(),
            });
        }
        out
    }

    pub fn create_network(&self, raw_name: &str, internal: bool, labels: BTreeMap<String, String>) -> Result<UserNetwork> {
        let name = validate_user_network_name(raw_name)?;
        let saved = {
            let mut nets = self.networks.lock().unwrap();
            if nets.contains_key(&name) {
                bail!("network with name {name} already exists");
            }
            let entry = SavedNetwork {
                id: new_id(),
                name: name.clone(),
                internal,
                created_at: now_secs(),
                labels,
            };
            nets.insert(name, entry.clone());
            entry
        };
        self.save();
        Ok(UserNetwork {
            id: saved.id,
            name: saved.name,
            driver: "bridge".into(),
            internal: saved.internal,
            builtin: false,
            created_at: saved.created_at,
            labels: saved.labels,
            containers: Vec::new(),
        })
    }

    pub fn remove_network(&self, key: &str) -> Result<String> {
        let key = key.trim();
        if matches!(key, "graviton" | "bridge" | "host" | "none" | "graviton-bridge" | "graviton-host" | "graviton-none") {
            bail!("{key} is a pre-defined network and cannot be removed");
        }
        let target_name = {
            let nets = self.networks.lock().unwrap();
            if nets.contains_key(key) {
                key.to_string()
            } else {
                nets.values()
                    .find(|n| n.id == key || n.id.starts_with(key))
                    .map(|n| n.name.clone())
                    .ok_or_else(|| anyhow!("network `{key}` not found"))?
            }
        };
        {
            let store = self.store.lock().unwrap();
            let specs = self.specs.lock().unwrap();
            for c in store.containers() {
                if specs.get(&c.id).is_some_and(|s| s.networks.iter().any(|n| n == &target_name)) {
                    bail!("network `{target_name}` has active endpoint `{}`", c.name);
                }
            }
        }
        self.networks.lock().unwrap().remove(&target_name);
        self.save();
        Ok(target_name)
    }

    pub fn connect_network(&self, network: &str, container_key: &str) -> Result<(String, String)> {
        let net_name = self.resolve_network_name(network)?;
        let (cid, cname) = {
            let store = self.store.lock().unwrap();
            let c = store.container(container_key)?;
            (c.id.clone(), c.name.clone())
        };
        let updated_nets = {
            let mut specs = self.specs.lock().unwrap();
            let spec = specs.get_mut(&cid).ok_or_else(|| anyhow!("container {container_key} has no saved settings"))?;
            if !spec.networks.contains(&net_name) {
                spec.networks.push(net_name.clone());
            }
            spec.networks.clone()
        };
        if let Some(r) = self.running.lock().unwrap().get_mut(&cid) {
            r.networks = updated_nets.clone();
            if let Some(port) = &r.port {
                self.switch.set_port_networks(port.id(), updated_nets);
            }
        }
        self.sync_hosts();
        self.save();
        Ok((net_name, cname))
    }

    pub fn disconnect_network(&self, network: &str, container_key: &str) -> Result<(String, String)> {
        let net_name = self.resolve_network_name(network)?;
        let (cid, cname) = {
            let store = self.store.lock().unwrap();
            let c = store.container(container_key)?;
            (c.id.clone(), c.name.clone())
        };
        let updated_nets = {
            let mut specs = self.specs.lock().unwrap();
            let spec = specs.get_mut(&cid).ok_or_else(|| anyhow!("container {container_key} has no saved settings"))?;
            let before = spec.networks.len();
            spec.networks.retain(|n| n != &net_name);
            if spec.networks.len() == before {
                bail!("container `{cname}` is not connected to network `{net_name}`");
            }
            spec.networks.clone()
        };
        if let Some(r) = self.running.lock().unwrap().get_mut(&cid) {
            r.networks = updated_nets.clone();
            if let Some(port) = &r.port {
                self.switch.set_port_networks(port.id(), updated_nets);
            }
        }
        self.sync_hosts();
        self.save();
        Ok((net_name, cname))
    }

    pub fn prune_unused_networks(&self) -> Result<Vec<String>> {
        let in_use: HashSet<String> = {
            let store = self.store.lock().unwrap();
            let specs = self.specs.lock().unwrap();
            store
                .containers()
                .iter()
                .filter_map(|c| specs.get(&c.id))
                .flat_map(|s| s.networks.iter().cloned())
                .collect()
        };
        let removed: Vec<String> = {
            let mut nets = self.networks.lock().unwrap();
            let unused: Vec<String> = nets.keys().filter(|name| !in_use.contains(*name)).cloned().collect();
            for name in &unused {
                nets.remove(name);
            }
            unused
        };
        if !removed.is_empty() {
            self.save();
        }
        Ok(removed)
    }

    fn resolve_network_name(&self, key: &str) -> Result<String> {
        let norm = normalize_network_name(key);
        if norm == DEFAULT_NETWORK_NAME {
            return Ok(norm);
        }
        let nets = self.networks.lock().unwrap();
        if nets.contains_key(&norm) {
            return Ok(norm);
        }
        nets.values()
            .find(|n| n.id == norm || n.id.starts_with(&norm))
            .map(|n| n.name.clone())
            .ok_or_else(|| anyhow!("network `{key}` not found"))
    }

    pub fn network_info(&self) -> NetworkInfo {
        let sw = self.switch.stats();
        let running: HashMap<String, (Option<String>, bool, Vec<String>)> = self
            .running
            .lock()
            .unwrap()
            .iter()
            .map(|(id, r)| {
                let mut aliases = Vec::new();
                if let Some(svc) = &r.service {
                    if !svc.eq_ignore_ascii_case(&r.name) {
                        aliases.push(svc.clone());
                    }
                }
                for a in &r.dns_aliases {
                    if !a.eq_ignore_ascii_case(&r.name) && !aliases.contains(a) {
                        aliases.push(a.clone());
                    }
                }
                (id.clone(), (r.ip.clone(), r.dns, aliases))
            })
            .collect();
        let spec_networks: HashMap<String, Vec<String>> = {
            let specs = self.specs.lock().unwrap();
            specs
                .iter()
                .map(|(id, s)| (id.clone(), if s.networks.is_empty() { vec![DEFAULT_NETWORK_NAME.into()] } else { s.networks.clone() }))
                .collect()
        };
        let containers = self.store.lock().unwrap().containers().to_vec();
        let (subnet, gateway) = containers
            .iter()
            .filter(|c| matches!(c.status, ContainerStatus::Running | ContainerStatus::Paused))
            .filter_map(|c| c.ip_address.as_deref())
            .find_map(ipv4_subnet_and_gateway)
            .unwrap_or_else(|| ("192.168.64.0/24".into(), "192.168.64.1".into()));
        let mut endpoints: Vec<NetworkEndpoint> = containers
            .into_iter()
            .map(|c| {
                let is_running = matches!(c.status, ContainerStatus::Running | ContainerStatus::Paused);
                let (live_ip, dns_enabled, aliases) = running.get(&c.id).cloned().unwrap_or((None, false, vec![]));
                let ip = if is_running { live_ip.or(c.ip_address) } else { None };
                let mut dns_names = if is_running && ip.is_some() {
                    vec![c.name.clone(), format!("{}.graviton", c.name), c.id.clone()]
                } else {
                    vec![]
                };
                if is_running && ip.is_some() {
                    for alias in aliases {
                        dns_names.push(alias);
                    }
                }
                let nets = spec_networks.get(&c.id).cloned().unwrap_or_else(|| vec![DEFAULT_NETWORK_NAME.into()]);
                NetworkEndpoint {
                    container_id: c.id,
                    container_name: c.name,
                    image: c.image,
                    status: c.status,
                    ip_address: ip,
                    dns_names,
                    dns_enabled,
                    ports: c.ports,
                    networks: nets,
                }
            })
            .collect();
        endpoints.sort_by(|a, b| {
            matches!(b.status, ContainerStatus::Running | ContainerStatus::Paused)
                .cmp(&matches!(a.status, ContainerStatus::Running | ContainerStatus::Paused))
                .then_with(|| a.container_name.cmp(&b.container_name))
        });
        NetworkInfo {
            name: "graviton".into(),
            driver: "L2 switch + NAT".into(),
            subnet,
            gateway,
            dns_server: "127.0.0.11:53".into(),
            dns_domain: "graviton".into(),
            connected_ports: sw.ports,
            learned_macs: sw.learned_macs,
            frames_forwarded: sw.frames_forwarded,
            bytes_forwarded: sw.bytes_forwarded,
            endpoints,
            networks: self.list_networks(),
        }
    }

    // -- exec -------------------------------------------------------------

    /// Open a fresh agent connection and start `spec` in the container.
    /// Returns `(pid, conn)` positioned just after the agent's `started` line.
    fn exec_start_inner(&self, spec: &ExecSpec, stdin_pipe: bool) -> Result<(i32, File)> {
        let (id, name) = {
            let store = self.store.lock().unwrap();
            let c = store.container(&spec.id)?;
            (c.id.clone(), c.name.clone())
        };
        let vm = self.active_vm(&id, &spec.id)?;
        let cs = self.specs.lock().unwrap().get(&id).cloned().ok_or_else(|| anyhow!("unknown container {}", spec.id))?;
        if spec.command.is_empty() {
            bail!("give a command to run");
        }
        let hardware_isolated = self
            .running
            .lock()
            .unwrap()
            .get(&id)
            .is_some_and(|r| r.network_policy.read().unwrap().hardware_isolated);
        let mut env = inject_proxy_env(&cs.env, hardware_isolated);
        env.extend(spec.env.iter().cloned());
        let cwd = spec
            .working_dir
            .clone()
            .filter(|w| !w.is_empty())
            .unwrap_or_else(|| cs.cwd.clone());
        let user = spec
            .user
            .clone()
            .filter(|u| !u.is_empty())
            .or_else(|| cs.user.clone());

        let fd: OwnedFd = vm.connect(AGENT_VSOCK_PORT)?;
        let mut conn = File::from(fd);
        match read_event_unbuffered(&mut conn)? {
            AgentEvent::Ready { .. } => {}
            other => bail!("expected ready from the agent, got {other:?}"),
        }
        conn.write_all(&encode_line(&AgentRequest::Exec {
            argv: spec.command.clone(),
            env,
            cwd,
            tty: spec.tty,
            container: Some(id),
            user,
            hostname: Some(name),
            rows: spec.rows,
            cols: spec.cols,
            stdin: None,
            stdin_pipe,
            mounts: vec![],
            read_only: false,
            cap_add: cs.cap_add,
            cap_drop: cs.cap_drop,
            init: false,
            shm_size_bytes: None,
            ulimits: cs.ulimits,
        }))?;
        match read_event_unbuffered(&mut conn)? {
            AgentEvent::Started { pid } => Ok((pid, conn)),
            AgentEvent::Error { message } => bail!("{message}"),
            other => bail!("expected started from the agent, got {other:?}"),
        }
    }

    /// Run a command without a TTY, sending its output and exit code to `out`.
    pub fn exec_stream(&self, spec: &ExecSpec, out: tokio::sync::mpsc::Sender<Response>) -> Result<()> {
        let (_, conn) = self.exec_start_inner(spec, false)?;
        let mut events = Events::new(BufReader::new(conn));
        loop {
            let response = match events.next()? {
                AgentEvent::Stdout { data, .. } => Response::ExecOutput { stream: LogStream::Stdout, data },
                AgentEvent::Stderr { data, .. } => Response::ExecOutput { stream: LogStream::Stderr, data },
                AgentEvent::Exited { code, .. } => {
                    let _ = out.blocking_send(Response::ExecExited { code });
                    return Ok(());
                }
                AgentEvent::Error { message } => bail!("{message}"),
                _ => continue,
            };
            if out.blocking_send(response).is_err() {
                return Ok(()); // the client went away
            }
        }
    }

    /// Start a command on a TTY; the returned stream carries terminal frames.
    pub fn exec_tty(&self, spec: &ExecSpec) -> Result<OwnedFd> {
        Ok(OwnedFd::from(self.exec_start_inner(spec, false)?.1))
    }

    /// Start an interactive non-TTY command, returning `(pid, agent_vsock_fd)`.
    pub fn exec_interactive(&self, spec: &ExecSpec, stdin_pipe: bool) -> Result<(i32, OwnedFd)> {
        let (pid, conn) = self.exec_start_inner(spec, stdin_pipe)?;
        Ok((pid, OwnedFd::from(conn)))
    }

    pub fn subscribe_logs(&self) -> tokio::sync::broadcast::Receiver<LogEvent> {
        self.log_tx.subscribe()
    }

    pub fn subscribe_stats(&self) -> tokio::sync::broadcast::Receiver<StatsEvent> {
        self.stats_tx.subscribe()
    }

    fn log(&self, id: &str, level: LogLevel, stream: LogStream, message: String) {
        let line = self.store.lock().unwrap().push_log(id, level, stream, message);
        {
            let _guard = self.log_io.lock().unwrap();
            crate::persistence::append_log(&log_path(id), &line);
        }
        let _ = self.log_tx.send(LogEvent::Line { container_id: id.to_string(), line });
    }

    fn generated_name(&self, image: &str) -> String {
        let base: String = image
            .rsplit('/')
            .next()
            .unwrap_or("container")
            .split([':', '@'])
            .next()
            .unwrap_or("container")
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let store = self.store.lock().unwrap();
        loop {
            let candidate = format!("{base}-{}", &new_id()[..4]);
            if !store.name_taken(&candidate) {
                return candidate;
            }
        }
    }
}

/// Exponential backoff for automatic restarts: 1s, 2s, 4s, ..., capped at 60s.
fn restart_delay(attempt: u32) -> Duration {
    let secs = INITIAL_BACKOFF.as_secs().saturating_mul(1u64 << attempt.min(6));
    Duration::from_secs(secs.min(MAX_BACKOFF.as_secs()))
}

fn should_restart(policy: RestartPolicy, requested: bool, exit_code: i32, failed_to_start: bool) -> bool {
    if requested {
        return false;
    }
    match policy {
        RestartPolicy::No => false,
        RestartPolicy::OnFailure => failed_to_start || exit_code != 0,
        RestartPolicy::Always => true,
    }
}

struct UdpSession {
    back: Arc<tokio::net::UdpSocket>,
    last_active_ms: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

/// Owns every per-client reverse-pump task for one published UDP port so
/// aborting the parent forwarder immediately aborts every child task too.
#[derive(Default)]
struct UdpSessions(Arc<Mutex<HashMap<SocketAddr, UdpSession>>>);

impl Drop for UdpSessions {
    fn drop(&mut self) {
        if let Ok(mut map) = self.0.lock() {
            for (_, session) in map.drain() {
                session.task.abort();
            }
        }
    }
}

fn spawn_udp_forward(rt: &tokio::runtime::Handle, socket: UdpSocket, target: SocketAddr) -> Result<tokio::task::JoinHandle<()>> {
    socket.set_nonblocking(true)?;
    let _guard = rt.enter();
    let front = Arc::new(tokio::net::UdpSocket::from_std(socket)?);
    let back_bind: &str = if target.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    Ok(rt.spawn(async move {
        let sessions = UdpSessions::default();
        let epoch = Instant::now();
        let mut buf = vec![0u8; UDP_BUF_SIZE];
        loop {
            let Ok((n, client)) = front.recv_from(&mut buf).await else { continue };
            let now_ms = epoch.elapsed().as_millis() as u64;
            let existing = {
                let map = sessions.0.lock().unwrap();
                map.get(&client).map(|s| {
                    s.last_active_ms.store(now_ms, Ordering::Relaxed);
                    Arc::clone(&s.back)
                })
            };
            let back = match existing {
                Some(b) => b,
                None => {
                    let Ok(sock) = tokio::net::UdpSocket::bind(back_bind).await else { continue };
                    if sock.connect(target).await.is_err() {
                        continue;
                    }
                    let back = Arc::new(sock);
                    let last_active = Arc::new(AtomicU64::new(now_ms));
                    let front_reply = Arc::clone(&front);
                    let back_recv = Arc::clone(&back);
                    let active_reply = Arc::clone(&last_active);
                    let map_ref = Arc::clone(&sessions.0);
                    let task = tokio::spawn(async move {
                        let mut rbuf = vec![0u8; UDP_BUF_SIZE];
                        loop {
                            match tokio::time::timeout(UDP_IDLE_TIMEOUT, back_recv.recv(&mut rbuf)).await {
                                Ok(Ok(m)) => {
                                    active_reply.store(epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
                                    let _ = front_reply.send_to(&rbuf[..m], client).await;
                                }
                                Ok(Err(_)) => break,
                                Err(_) => {
                                    let idle = epoch.elapsed().as_millis() as u64 - active_reply.load(Ordering::Relaxed);
                                    if idle >= UDP_IDLE_TIMEOUT.as_millis() as u64 {
                                        break;
                                    }
                                }
                            }
                        }
                        if let Ok(mut map) = map_ref.lock() {
                            if map.get(&client).is_some_and(|s| Arc::ptr_eq(&s.back, &back_recv)) {
                                map.remove(&client);
                            }
                        }
                    });
                    let mut map = sessions.0.lock().unwrap();
                    if map.len() >= UDP_MAX_SESSIONS {
                        if let Some(oldest) = map.iter().min_by_key(|(_, s)| s.last_active_ms.load(Ordering::Relaxed)).map(|(k, _)| *k) {
                            if let Some(evicted) = map.remove(&oldest) {
                                evicted.task.abort();
                            }
                        }
                    }
                    map.insert(client, UdpSession { back: Arc::clone(&back), last_active_ms: last_active, task });
                    back
                }
            };
            let _ = back.send(&buf[..n]).await;
        }
    }))
}

/// `[IP:]HOST:CONTAINER[/tcp|udp]` or `PORT[/tcp|udp]` (default bind IP: `127.0.0.1`).
const MAX_FORKS_PER_CALL: usize = 16;

fn is_valid_fork_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\0')
        && !name.chars().any(char::is_whitespace)
}

fn resolve_fork_names(source_name: &str, names: &[String], count: u32) -> Result<Vec<String>> {
    if count == 0 {
        bail!("fork count must be at least 1");
    }
    let resolved = if !names.is_empty() {
        if count > 1 && names.len() != count as usize {
            bail!("--count ({count}) doesn't match supplied fork names ({})", names.len());
        }
        if names.len() > MAX_FORKS_PER_CALL {
            bail!("at most {MAX_FORKS_PER_CALL} forks per call");
        }
        names.to_vec()
    } else {
        let count = count as usize;
        if count > MAX_FORKS_PER_CALL {
            bail!("at most {MAX_FORKS_PER_CALL} forks per call");
        }
        (1..=count).map(|i| format!("{source_name}-fork-{i}")).collect()
    };

    let mut seen = std::collections::HashSet::with_capacity(resolved.len());
    for name in &resolved {
        if !is_valid_fork_name(name) {
            bail!("invalid fork container name `{name}`");
        }
        if !seen.insert(name.as_str()) {
            bail!("duplicate fork container name `{name}`");
        }
    }
    Ok(resolved)
}

fn merge_env_vars(base: &[String], overrides: &[String]) -> Result<Vec<String>> {
    let mut merged = base.to_vec();
    for item in overrides {
        let key = match item.split_once('=') {
            Some((k, _)) => k,
            None => item.as_str(),
        };
        if key.is_empty() || key.contains('\0') {
            bail!("invalid environment variable `{item}`");
        }
        merged.retain(|existing| {
            let existing_key = existing.split_once('=').map_or(existing.as_str(), |(k, _)| k);
            existing_key != key
        });
        merged.push(item.clone());
    }
    Ok(merged)
}

fn inject_proxy_env(base: &[String], isolated: bool) -> Vec<String> {
    if !isolated {
        return base.to_vec();
    }
    let mut env = base.to_vec();
    let defaults = [
        ("HTTP_PROXY", "http://127.0.0.1:18080"),
        ("HTTPS_PROXY", "http://127.0.0.1:18080"),
        ("ALL_PROXY", "socks5h://127.0.0.1:18080"),
        ("NO_PROXY", "localhost,127.0.0.1,::1,.graviton"),
        ("http_proxy", "http://127.0.0.1:18080"),
        ("https_proxy", "http://127.0.0.1:18080"),
        ("all_proxy", "socks5h://127.0.0.1:18080"),
        ("no_proxy", "localhost,127.0.0.1,::1,.graviton"),
    ];
    for (k, v) in defaults {
        if !env.iter().any(|e| e.split_once('=').is_some_and(|(ek, _)| ek == k)) {
            env.push(format!("{k}={v}"));
        }
    }
    env
}

/// IPv6 bind addresses use brackets, e.g. `[::]:8080:80` or `[::1]:5353:53/udp`.
fn parse_port(spec: &str) -> Result<PortMapping> {
    let (ports, proto_raw) = spec.rsplit_once('/').unwrap_or((spec, "tcp"));
    let protocol = match proto_raw.trim().to_ascii_lowercase().as_str() {
        "tcp" => PortProtocol::Tcp,
        "udp" => PortProtocol::Udp,
        other => bail!("`{spec}`: unknown protocol `{other}` (use tcp or udp)"),
    };
    let bad = || anyhow!("`{spec}` isn't a valid port mapping (try 8080:80, 0.0.0.0:8080:80 or 5353:53/udp)");
    let parse_num = |s: &str| s.trim().parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(bad);

    let ports = ports.trim();
    let (host_ip, host_port, container_port) = if let Some(rest) = ports.strip_prefix('[') {
        let (ip_str, tail) = rest.split_once("]:").ok_or_else(bad)?;
        let ip: Ipv6Addr = ip_str.parse().map_err(|_| anyhow!("`{spec}`: `{ip_str}` isn't a valid IPv6 address"))?;
        let (host, container) = tail.split_once(':').ok_or_else(bad)?;
        (Some(ip.to_string()), parse_num(host)?, parse_num(container)?)
    } else {
        match ports.split(':').collect::<Vec<_>>()[..] {
            [one] => (None, parse_num(one)?, parse_num(one)?),
            [host, container] => (None, parse_num(host)?, parse_num(container)?),
            [ip_str, host, container] => {
                let ip: Ipv4Addr = ip_str.trim().parse().map_err(|_| anyhow!("`{spec}`: `{ip_str}` isn't a valid IPv4 address"))?;
                let host_ip = (ip != Ipv4Addr::LOCALHOST).then(|| ip.to_string());
                (host_ip, parse_num(host)?, parse_num(container)?)
            }
            _ => return Err(bad()),
        }
    };
    Ok(PortMapping { host_ip, host_port, container_port, protocol })
}

/// `/host/dir:/path[:ro]`, `~/dir:/path` or `name:/path[:ro]`.
fn parse_volume(spec: &str) -> Result<Volume> {
    let (rest, read_only) = match spec.rsplit_once(':') {
        Some((rest, "ro")) => (rest, true),
        Some((rest, "rw")) => (rest, false),
        _ => (spec, false),
    };
    let (source, target) = rest.split_once(':').ok_or_else(|| anyhow!("`{spec}` should be SOURCE:/path/in/container"))?;
    if !target.starts_with('/') {
        bail!("`{spec}`: the path in the container must be absolute");
    }
    let target = target.trim_end_matches('/').to_string();
    if target.is_empty() {
        bail!("`{spec}`: can't mount over the container's root");
    }
    if let Some(home_rel) = source.strip_prefix("~/") {
        let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| anyhow!("HOME isn't set"))?;
        return Ok(Volume { source: home.join(home_rel), target, read_only, name: None });
    }
    if source.starts_with('/') {
        return Ok(Volume { source: PathBuf::from(source), target, read_only, name: None });
    }
    if !is_valid_volume_name(source) {
        bail!("`{source}` isn't an absolute path or a volume name (letters, digits, _ . -)");
    }
    Ok(Volume { source: paths::volumes_dir().join(source), target, read_only, name: Some(source.to_string()) })
}

pub(crate) fn is_valid_volume_name(name: &str) -> bool {
    !name.is_empty()
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

/// Walk `dir` without following symlinks, returning `(size_bytes, file_count, created_at_secs)`.
fn volume_usage(dir: &Path) -> (u64, u64, u64) {
    let Ok(root_meta) = fs::symlink_metadata(dir) else { return (0, 0, 0) };
    let created_at = root_meta
        .created()
        .or_else(|_| root_meta.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let mut size = 0u64;
    let mut files = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(cur) = stack.pop() {
        let Ok(rd) = fs::read_dir(&cur) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else { continue };
            let ft = meta.file_type();
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                size = size.saturating_add(meta.len());
                files = files.saturating_add(1);
            } else if ft.is_symlink() {
                files = files.saturating_add(1);
            }
        }
    }
    (size, files, created_at)
}

fn ipv4_subnet_and_gateway(ip: &str) -> Option<(String, String)> {
    let addr: Ipv4Addr = ip.parse().ok()?;
    let [a, b, c, _] = addr.octets();
    Some((format!("{a}.{b}.{c}.0/24"), format!("{a}.{b}.{c}.1")))
}

fn log_path(id: &str) -> PathBuf {
    paths::vms_dir().join(id).join("log.jsonl")
}

fn stats_path(id: &str) -> PathBuf {
    paths::vms_dir().join(id).join("stats.jsonl")
}

/// Read one JSON line byte by byte, so nothing after it is consumed.
pub(crate) fn read_event_unbuffered(conn: &mut File) -> Result<AgentEvent> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if conn.read(&mut byte)? == 0 {
            bail!("the guest agent closed the connection");
        }
        if byte[0] == b'\n' {
            return Ok(decode_line(&line)?);
        }
        line.push(byte[0]);
        if line.len() > 1 << 20 {
            bail!("the guest agent sent an overlong line");
        }
    }
}

pub(crate) struct Events {
    reader: BufReader<File>,
    line: String,
}

impl Events {
    pub(crate) fn new(reader: BufReader<File>) -> Self {
        Self { reader, line: String::new() }
    }

    pub(crate) fn next(&mut self) -> Result<AgentEvent> {
        self.line.clear();
        if self.reader.read_line(&mut self.line)? == 0 {
            bail!("the guest agent closed the connection (the VM may have stopped)");
        }
        Ok(decode_line(self.line.as_bytes())?)
    }
}

/// Splits a byte stream into lines for the log.
#[derive(Default)]
pub(crate) struct LineBuffer(Vec<u8>);

impl LineBuffer {
    pub(crate) fn push(&mut self, data: &[u8]) -> Vec<String> {
        self.0.extend_from_slice(data);
        let mut lines = Vec::new();
        while let Some(pos) = self.0.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.0.drain(..=pos).collect();
            lines.push(String::from_utf8_lossy(&line[..line.len() - 1]).trim_end_matches('\r').to_string());
        }
        // Don't hold an unbounded partial line.
        if self.0.len() > 64 * 1024 {
            lines.push(String::from_utf8_lossy(&std::mem::take(&mut self.0)).into_owned());
        }
        lines
    }

    pub(crate) fn finish(&mut self) -> Option<String> {
        (!self.0.is_empty()).then(|| String::from_utf8_lossy(&std::mem::take(&mut self.0)).trim_end_matches('\r').to_string())
    }
}

/// Turn the very first reading into an initial sample (0.0% CPU until the next tick, with real memory & net totals).
fn initial_sample(now: &GuestStats, limit: u64) -> StatSample {
    let memory_limit_bytes = if limit > 0 { limit } else { now.memory_total_bytes };
    StatSample {
        timestamp_ms: now_ms(),
        cpu_percent: 0.0,
        memory_bytes: now.memory_total_bytes.saturating_sub(now.memory_available_bytes),
        memory_limit_bytes,
        net_rx_bytes: now.net_rx_bytes,
        net_tx_bytes: now.net_tx_bytes,
    }
}

/// Turn two readings of the guest's counters into one sample.
fn sample_between(prev: &GuestStats, now: &GuestStats, limit: u64) -> StatSample {
    let total = now.cpu_total.saturating_sub(prev.cpu_total);
    let busy = now.cpu_busy.saturating_sub(prev.cpu_busy);
    let cpus = now.cpus.max(1) as f64;
    let cpu_percent = if total == 0 { 0.0 } else { busy as f64 / total as f64 * cpus * 100.0 };
    let memory_limit_bytes = if limit > 0 { limit } else { now.memory_total_bytes };
    StatSample {
        timestamp_ms: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64),
        cpu_percent: (cpu_percent * 10.0).round() / 10.0,
        memory_bytes: now.memory_total_bytes.saturating_sub(now.memory_available_bytes),
        memory_limit_bytes,
        net_rx_bytes: now.net_rx_bytes,
        net_tx_bytes: now.net_tx_bytes,
    }
}

/// Adaptive `virtio-balloon` target controller driven by live guest `/proc/meminfo` stats.
struct BalloonController {
    min_target: u64,
    max_target: u64,
    current_target: u64,
    shrink_streak: u32,
    first_sample: bool,
}

impl BalloonController {
    fn new(limit: u64, min_allowed_bytes: u64) -> Self {
        let min_target = (64 << 20).max(min_allowed_bytes).min(limit);
        Self { min_target, max_target: limit, current_target: limit, shrink_streak: 0, first_sample: true }
    }

    fn update(&mut self, stats: &GuestStats) -> Option<u64> {
        if stats.memory_total_bytes == 0 || self.max_target <= self.min_target {
            self.first_sample = false;
            return None;
        }
        let avail = stats.memory_available_bytes;
        let working_set = stats.memory_total_bytes.saturating_sub(avail);

        // Grow immediately on low free RAM to prevent guest OOM kills.
        if avail < (32 << 20) {
            self.shrink_streak = 0;
            self.first_sample = false;
            let target = align_up_16m(working_set.saturating_add(128 << 20))
                .max(self.current_target.saturating_add(64 << 20))
                .min(self.max_target);
            if target > self.current_target {
                self.current_target = target;
                return Some(target);
            }
            return None;
        }

        // Inflate the balloon (reclaim idle guest pages) after sustained headroom, or right after boot.
        if avail > (96 << 20) {
            self.shrink_streak = self.shrink_streak.saturating_add(1);
            let ready = self.first_sample || self.shrink_streak >= 2;
            self.first_sample = false;
            if ready {
                let desired = align_up_16m(working_set.saturating_add(80 << 20)).clamp(self.min_target, self.max_target);
                if self.current_target.saturating_sub(desired) >= (16 << 20) {
                    self.current_target = desired;
                    self.shrink_streak = 0;
                    return Some(desired);
                }
            }
        } else {
            self.shrink_streak = 0;
            self.first_sample = false;
        }
        None
    }
}

fn align_up_16m(bytes: u64) -> u64 {
    const STEP: u64 = 16 << 20;
    bytes.saturating_add(STEP - 1) / STEP * STEP
}

fn dial_guest_tcp(vm: &AppleVm, port: u16) -> Result<OwnedFd> {
    let mut conn = File::from(vm.connect(AGENT_VSOCK_PORT)?);
    match read_event_unbuffered(&mut conn)? {
        AgentEvent::Ready { .. } => {}
        other => bail!("expected ready from the agent, got {other:?}"),
    }
    conn.write_all(&encode_line(&AgentRequest::DialTcp { port }))?;
    match read_event_unbuffered(&mut conn)? {
        AgentEvent::Connected => Ok(OwnedFd::from(conn)),
        AgentEvent::Error { message } => bail!("{message}"),
        other => bail!("expected connected from the agent, got {other:?}"),
    }
}

fn send(agent: &Arc<Mutex<File>>, request: &AgentRequest) -> Result<()> {
    agent.lock().unwrap().write_all(&encode_line(request)).context("writing to the guest agent")
}

pub(crate) fn new_id() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let a = RandomState::new().hash_one(nanos);
    let b = RandomState::new().hash_one(a);
    format!("{:016x}", a ^ b.rotate_left(17))[..12].to_string()
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

pub(crate) fn host_memory() -> u64 {
    static TOTAL: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *TOTAL.get_or_init(|| {
        std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    })
}

/// Physical host memory currently in use on macOS (Activity Monitor definition:
/// anonymous non-purgeable app memory + wired kernel memory + compressed memory).
#[cfg(target_os = "macos")]
#[allow(deprecated)] // libc marks mach_host_self deprecated in favor of mach2, which we don't depend on.
fn host_memory_used() -> Option<u64> {
    static HOST: std::sync::OnceLock<libc::mach_port_t> = std::sync::OnceLock::new();
    static PAGE_SIZE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

    let host = *HOST.get_or_init(|| unsafe { libc::mach_host_self() });
    let page_size = *PAGE_SIZE.get_or_init(|| {
        let p = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if p > 0 { p as u64 } else { 16384 }
    });

    let mut stats = std::mem::MaybeUninit::<libc::vm_statistics64>::zeroed();
    let mut count = libc::HOST_VM_INFO64_COUNT;
    let rc = unsafe {
        libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            stats.as_mut_ptr().cast(),
            &mut count,
        )
    };
    if rc != 0 {
        return None;
    }
    let stats = unsafe { stats.assume_init() };
    let internal = u64::from(stats.internal_page_count);
    let purgeable = u64::from(stats.purgeable_count);
    let wired = u64::from(stats.wire_count);
    let compressed = u64::from(stats.compressor_page_count);
    let used = internal
        .saturating_sub(purgeable)
        .saturating_add(wired)
        .saturating_add(compressed)
        .saturating_mul(page_size);
    let total = host_memory();
    Some(if total > 0 { used.min(total) } else { used })
}

#[cfg(not(target_os = "macos"))]
fn host_memory_used() -> Option<u64> {
    None
}

/// Read the active guest kernel's banner (`Linux version X.Y.Z-...`) from
/// `vmlinux`, cached by `(path, len, mtime)` so `grav info` / `inspect` stay instant.
pub(crate) fn detect_guest_kernel() -> String {
    type CacheKey = (PathBuf, u64, u64);
    static CACHE: std::sync::OnceLock<Mutex<Option<(CacheKey, String)>>> = std::sync::OnceLock::new();
    let path = paths::guest_dir().join("vmlinux");
    let Ok(meta) = fs::metadata(&path) else { return KERNEL_FALLBACK.into() };
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    let key = (path.clone(), meta.len(), mtime);
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Some((k, label)) = cache.lock().unwrap().as_ref() {
        if *k == key {
            return label.clone();
        }
    }
    let label = fs::read(&path)
        .ok()
        .and_then(|bytes| extract_linux_version(&bytes))
        .map(|ver| {
            if ver.ends_with("-virt") {
                format!("Linux {ver} (Alpine)")
            } else {
                format!("Linux {ver}")
            }
        })
        .unwrap_or_else(|| KERNEL_FALLBACK.into());
    *cache.lock().unwrap() = Some((key, label.clone()));
    label
}

fn extract_linux_version(bytes: &[u8]) -> Option<String> {
    const NEEDLE: &[u8] = b"Linux version ";
    let pos = bytes.windows(NEEDLE.len()).position(|w| w == NEEDLE)?;
    let rest = &bytes[pos + NEEDLE.len()..];
    let end = rest.iter().position(|&b| b == b' ' || b == 0 || b == b'\n')?;
    let ver = std::str::from_utf8(&rest[..end]).ok()?.trim();
    (!ver.is_empty()).then(|| ver.to_string())
}

/// The file a VM's scratch disk lives in, beside its console log.
pub(crate) const SCRATCH_DISK: &str = "disk.img";

const GVSP_MAGIC: &[u8; 8] = b"GVSP\x01\x00\x00\x00";

fn extract_gvsp_gz(src: &Path, dst: &Path) -> Result<()> {
    let file = File::open(src).with_context(|| format!("opening {}", src.display()))?;
    let mut gz = flate2::read::GzDecoder::new(BufReader::new(file));
    let mut header = [0u8; 16];
    gz.read_exact(&mut header).context("reading GVSP header")?;
    if &header[..8] != GVSP_MAGIC {
        bail!("invalid GVSP magic in {}", src.display());
    }
    let total_size = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let mut out = File::create(dst).with_context(|| format!("creating {}", dst.display()))?;
    out.set_len(total_size).with_context(|| format!("sizing {}", dst.display()))?;
    let mut rec = [0u8; 12];
    let mut buf = Vec::new();
    loop {
        gz.read_exact(&mut rec).context("reading GVSP chunk header")?;
        let offset = u64::from_le_bytes(rec[0..8].try_into().unwrap());
        let len = u32::from_le_bytes(rec[8..12].try_into().unwrap());
        if offset == 0 && len == 0 {
            break;
        }
        if len > (32 << 20) || offset.checked_add(u64::from(len)).is_none_or(|end| end > total_size) {
            bail!("corrupt GVSP chunk at {offset} (len {len}, total {total_size})");
        }
        buf.resize(len as usize, 0);
        gz.read_exact(&mut buf).context("reading GVSP chunk data")?;
        out.seek(std::io::SeekFrom::Start(offset))?;
        out.write_all(&buf)?;
    }
    out.flush()?;
    Ok(())
}

fn ensure_ext4_template() -> Result<Option<PathBuf>> {
    use std::sync::OnceLock;
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();

    let gvsp = paths::guest_dir().join("empty-disk.gvsp.gz");
    let Ok(meta) = fs::metadata(&gvsp) else {
        return Ok(None);
    };
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
    let wanted = format!("{}:{mtime}", meta.len());

    let dir = paths::vms_dir();
    fs::create_dir_all(&dir)?;
    let tpl = dir.join(".template-64g.ext4");
    let stamp = dir.join(".template-64g.stamp");
    if tpl.is_file() && fs::read_to_string(&stamp).is_ok_and(|s| s == wanted) {
        return Ok(Some(tpl));
    }

    let tmp = dir.join(format!(".template-64g.{}.tmp", std::process::id()));
    extract_gvsp_gz(&gvsp, &tmp)?;
    fs::rename(&tmp, &tpl)?;
    let _ = fs::write(&stamp, wanted);
    Ok(Some(tpl))
}

#[derive(Clone, Serialize, Deserialize)]
struct FrozenBundleManifest {
    version: u32,
    source_container_id: String,
    source_container_name: String,
    image_ref: String,
    spec: ContainerSpec,
    created_at: i64,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    failed_step: Option<String>,
    #[serde(default)]
    has_vm_state: bool,
    #[serde(default)]
    workspace_rel: Option<String>,
    #[serde(default)]
    workspace_target: Option<String>,
}

fn validate_bundle_dir(path: &Path) -> Result<()> {
    let raw = path.to_string_lossy();
    if raw.trim().is_empty() || raw.contains('\0') {
        bail!("invalid frozen bundle path");
    }
    for c in path.components() {
        if matches!(c, std::path::Component::ParentDir) {
            bail!("frozen bundle path cannot contain '..': {}", path.display());
        }
    }
    if matches!(
        path.to_str(),
        Some("/" | "/System" | "/usr" | "/bin" | "/sbin" | "/etc" | "/var" | "/private")
    ) {
        bail!("refusing system path as frozen bundle directory: {}", path.display());
    }
    Ok(())
}

fn sanitize_frozen_name(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let trimmed = clean.trim_matches('-');
    if trimmed.is_empty() { "ci".into() } else { trimmed.to_string() }
}

fn sanitize_frozen_env(env: &[String]) -> Vec<String> {
    env.iter()
        .filter(|entry| {
            let key = entry.split('=').next().unwrap_or(entry).to_ascii_uppercase();
            !["SECRET", "TOKEN", "PASSWORD", "PASSWD", "PRIVATE_KEY", "API_KEY", "AWS_SECRET", "CREDENTIAL"]
                .iter()
                .any(|needle| key.contains(needle))
        })
        .cloned()
        .collect()
}

fn clone_dir_cow(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        let _ = fs::remove_dir_all(dst);
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("/bin/cp").args(["-c", "-R"]).arg(src).arg(dst).output() {
            if out.status.success() {
                return Ok(());
            }
        }
    }
    let out = std::process::Command::new("/bin/cp")
        .arg("-R")
        .arg(src)
        .arg(dst)
        .output()
        .context("running cp -R")?;
    if !out.status.success() {
        bail!("cloning {} failed: {}", src.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn dir_size_bytes(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else { return 0 };
    if meta.is_file() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let blocks_bytes = meta.blocks().saturating_mul(512);
            return blocks_bytes.min(meta.len()).max(meta.len().min(4096));
        }
        #[cfg(not(unix))]
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    let Ok(entries) = fs::read_dir(path) else { return 0 };
    entries.flatten().map(|e| dir_size_bytes(&e.path())).sum()
}

fn clone_sparse_file(src: &Path, dst: &Path) -> Result<()> {
    let _ = fs::remove_file(dst);
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        unsafe extern "C" {
            fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32) -> libc::c_int;
        }
        if let (Ok(cs), Ok(cd)) = (CString::new(src.as_os_str().as_bytes()), CString::new(dst.as_os_str().as_bytes())) {
            // SAFETY: valid NUL-terminated paths passed to Darwin `clonefile(2)`.
            if unsafe { clonefile(cs.as_ptr(), cd.as_ptr(), 0) } == 0 {
                return Ok(());
            }
        }
    }
    let mut reader = BufReader::new(File::open(src)?);
    let total = reader.get_ref().metadata()?.len();
    let mut out = File::create(dst)?;
    out.set_len(total)?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut off = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if buf[..n].iter().any(|&b| b != 0) {
            out.seek(std::io::SeekFrom::Start(off))?;
            out.write_all(&buf[..n])?;
        }
        off += n as u64;
    }
    out.flush()?;
    Ok(())
}

/// A fresh, empty, sparse disk image in `dir` for a VM to keep containers'
/// files on (copied from the pre-formatted sparse template when `64 GiB`, else
/// zeroed sparse for the guest's `format` fallback).
pub(crate) fn scratch_disk(dir: &Path) -> Result<PathBuf> {
    let gib: u64 = std::env::var("GRAVITON_DISK_GIB").ok().and_then(|v| v.parse().ok()).filter(|g| (1..=2048).contains(g)).unwrap_or(64);
    let path = dir.join(SCRATCH_DISK);
    if gib == 64 {
        if let Some(tpl) = ensure_ext4_template()? {
            clone_sparse_file(&tpl, &path).with_context(|| format!("cloning {}", path.display()))?;
            return Ok(path);
        }
    }
    let _ = fs::remove_file(&path);
    let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    file.set_len(gib << 30).with_context(|| format!("sizing {}", path.display()))?;
    Ok(path)
}

/// A container's disk: kept across its runs (it holds the files the container
/// changed), created on its first start. `grav rm` deletes it with the
/// container's folder.
pub(crate) fn container_disk(dir: &Path) -> Result<PathBuf> {
    let path = dir.join(SCRATCH_DISK);
    if path.is_file() {
        return Ok(path);
    }
    scratch_disk(dir)
}

/// What a container's disk takes on the Mac's disk (allocated blocks, not its size).
fn disk_usage(dir: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(dir.join(SCRATCH_DISK)).map_or(0, |m| m.blocks() * 512)
}

/// Disks of containers that no longer exist (removed while gravitond wasn't running, say).
fn remove_orphan_disks(known: &HashSet<String>) {
    let Ok(entries) = fs::read_dir(paths::vms_dir()) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !known.contains(&name) {
            let _ = fs::remove_file(e.path().join(SCRATCH_DISK));
        }
    }
}

fn ns_to_secs(ns: i64) -> u64 {
    if ns <= 0 { 0 } else { (ns as u64).div_ceil(1_000_000_000) }
}

fn resolve_healthcheck(
    image_hc: Option<&HealthcheckConfig>,
    override_hc: Option<Healthcheck>,
) -> Option<Healthcheck> {
    let from_image = image_hc.and_then(|cfg| {
        if cfg.test.is_empty() || cfg.test.first().is_some_and(|s| s.eq_ignore_ascii_case("NONE")) {
            None
        } else {
            Some(
                Healthcheck {
                    test: cfg.test.clone(),
                    interval_secs: ns_to_secs(cfg.interval),
                    timeout_secs: ns_to_secs(cfg.timeout),
                    start_period_secs: ns_to_secs(cfg.start_period),
                    retries: cfg.retries,
                }
                .normalized(),
            )
        }
    });
    let resolved = match override_hc {
        None => from_image,
        Some(ov) if ov.is_disabled() => None,
        Some(ov) if !ov.test.is_empty() => Some(ov.normalized()),
        Some(ov) => from_image.map(|mut base| {
            if ov.interval_secs > 0 {
                base.interval_secs = ov.interval_secs;
            }
            if ov.timeout_secs > 0 {
                base.timeout_secs = ov.timeout_secs;
            }
            if ov.start_period_secs > 0 {
                base.start_period_secs = ov.start_period_secs;
            }
            if ov.retries > 0 {
                base.retries = ov.retries;
            }
            base.normalized()
        }),
    };
    resolved.filter(|hc| hc.exec_command().is_some())
}

fn apply_health_probe(
    state: &mut HealthState,
    hc: &Healthcheck,
    in_start_period: bool,
    log: HealthLog,
) -> Option<HealthStatus> {
    state.log.push(log);
    if state.log.len() > 5 {
        let excess = state.log.len() - 5;
        state.log.drain(..excess);
    }
    let prev = state.status;
    let code = state.log.last().map_or(-1, |l| l.exit_code);
    if code == 0 {
        state.failing_streak = 0;
        state.status = HealthStatus::Healthy;
    } else if in_start_period && state.status == HealthStatus::Starting {
        // Startup grace period: probe failures do not increment `failing_streak`.
    } else {
        state.failing_streak = state.failing_streak.saturating_add(1);
        if state.failing_streak >= hc.retries.max(1) {
            state.status = HealthStatus::Unhealthy;
        }
    }
    if state.status != prev && matches!(state.status, HealthStatus::Healthy | HealthStatus::Unhealthy) {
        Some(state.status)
    } else {
        None
    }
}

fn truncate_health_output(buf: &[u8]) -> String {
    let slice = &buf[..buf.len().min(4096)];
    String::from_utf8_lossy(slice).into_owned()
}

fn exec_probe_on_vm(
    vm: &AppleVm,
    id: &str,
    cs: &ContainerSpec,
    hostname: &str,
    argv: &[String],
) -> Result<(i32, File)> {
    let fd: OwnedFd = vm.connect(AGENT_VSOCK_PORT)?;
    let mut conn = File::from(fd);
    match read_event_unbuffered(&mut conn)? {
        AgentEvent::Ready { .. } => {}
        other => bail!("expected ready from the agent, got {other:?}"),
    }
    conn.write_all(&encode_line(&AgentRequest::Exec {
        argv: argv.to_vec(),
        env: inject_proxy_env(&cs.env, cs.egress.is_restricted()),
        cwd: cs.cwd.clone(),
        tty: false,
        container: Some(id.to_string()),
        user: cs.user.clone(),
        hostname: Some(hostname.to_string()),
        rows: 0,
        cols: 0,
        stdin: None,
        stdin_pipe: false,
        mounts: vec![],
        read_only: false,
        cap_add: cs.cap_add.clone(),
        cap_drop: cs.cap_drop.clone(),
        init: false,
        shm_size_bytes: None,
        ulimits: cs.ulimits.clone(),
    }))?;
    match read_event_unbuffered(&mut conn)? {
        AgentEvent::Started { pid } => Ok((pid, conn)),
        AgentEvent::Error { message } => bail!("{message}"),
        other => bail!("expected started from the agent, got {other:?}"),
    }
}

fn normalize_network_name(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "bridge" || trimmed == "default" {
        DEFAULT_NETWORK_NAME.to_string()
    } else {
        trimmed.to_string()
    }
}

fn normalize_network_list(raw: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for item in raw {
        let name = normalize_network_name(item);
        if !out.contains(&name) {
            out.push(name);
        }
    }
    if out.is_empty() {
        out.push(DEFAULT_NETWORK_NAME.to_string());
    }
    out
}

pub(crate) fn validate_user_network_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name.len() > 63 {
        bail!("network name must be 1–63 characters");
    }
    if matches!(name, "graviton" | "bridge" | "host" | "none" | "default") {
        bail!("network name `{name}` is reserved");
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else { bail!("network name is empty") };
    if !first.is_ascii_alphanumeric() {
        bail!("network name `{name}` must start with an ASCII letter or digit");
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')) {
        bail!("network name `{name}` may contain only letters, digits, `.`, `_`, or `-`");
    }
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_output_into_lines() {
        let mut b = LineBuffer::default();
        assert_eq!(b.push(b"hel"), Vec::<String>::new());
        assert_eq!(b.push(b"lo\r\nwor"), ["hello"]);
        assert_eq!(b.push(b"ld\n\n"), ["world", ""]);
        assert_eq!(b.push(b"tail"), Vec::<String>::new());
        assert_eq!(b.finish().as_deref(), Some("tail"));
        assert_eq!(b.finish(), None);
    }

    #[test]
    fn samples_are_rates() {
        let at = |busy, total, avail| GuestStats {
            cpus: 2,
            cpu_busy: busy,
            cpu_total: total,
            memory_total_bytes: 1000,
            memory_available_bytes: avail,
            net_rx_bytes: 5,
            net_tx_bytes: 7,
            processes: vec![],
        };
        // Half of all CPU time busy on 2 CPUs is one CPU's worth: 100%.
        let s = sample_between(&at(100, 1000, 900), &at(300, 1400, 600), 4096);
        assert_eq!((s.cpu_percent, s.memory_bytes, s.memory_limit_bytes), (100.0, 400, 4096));
        assert_eq!(sample_between(&at(1, 1, 0), &at(1, 1, 0), 0).cpu_percent, 0.0);
    }

    #[test]
    fn ids_are_12_hex() {
        let a = new_id();
        assert_eq!(a.len(), 12);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, new_id());
    }

    #[test]
    fn parses_ports() {
        assert_eq!(
            parse_port("8080:80").unwrap(),
            PortMapping { host_ip: None, host_port: 8080, container_port: 80, protocol: PortProtocol::Tcp }
        );
        assert_eq!(parse_port("127.0.0.1:8080:80").unwrap(), parse_port("8080:80").unwrap());
        assert_eq!(parse_port("5432").unwrap().container_port, 5432);
        assert_eq!(parse_port("3000:3000/tcp").unwrap().host_port, 3000);
        assert_eq!(
            parse_port("5353:53/udp").unwrap(),
            PortMapping { host_ip: None, host_port: 5353, container_port: 53, protocol: PortProtocol::Udp }
        );
        assert_eq!(
            parse_port("5353/UDP").unwrap(),
            PortMapping { host_ip: None, host_port: 5353, container_port: 5353, protocol: PortProtocol::Udp }
        );
        assert_eq!(
            parse_port("0.0.0.0:8080:80").unwrap(),
            PortMapping { host_ip: Some("0.0.0.0".into()), host_port: 8080, container_port: 80, protocol: PortProtocol::Tcp }
        );
        assert_eq!(
            parse_port("192.0.2.62:5353:53/udp").unwrap(),
            PortMapping { host_ip: Some("192.0.2.62".into()), host_port: 5353, container_port: 53, protocol: PortProtocol::Udp }
        );
        assert_eq!(
            parse_port("[::]:8080:80").unwrap(),
            PortMapping { host_ip: Some("::".into()), host_port: 8080, container_port: 80, protocol: PortProtocol::Tcp }
        );
        assert_eq!(
            parse_port("[::1]:5353:53/udp").unwrap(),
            PortMapping { host_ip: Some("::1".into()), host_port: 5353, container_port: 53, protocol: PortProtocol::Udp }
        );
        assert!(parse_port("53:53/sctp").is_err());
        assert!(parse_port("0:80").is_err());
        assert!(parse_port("a:b:c").is_err());
        assert!(parse_port("[::]:80").is_err());
    }

    #[tokio::test]
    async fn udp_forward_isolates_clients_and_cleans_up() {
        // Mock guest UDP echo server returning "<peer_port>:<payload>".
        let backend = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            while let Ok((n, peer)) = backend.recv_from(&mut buf).await {
                let reply = format!("{}:{}", peer.port(), String::from_utf8_lossy(&buf[..n]));
                let _ = backend.send_to(reply.as_bytes(), peer).await;
            }
        });

        let front_std = UdpSocket::bind("127.0.0.1:0").unwrap();
        let front_addr = front_std.local_addr().unwrap();
        let fwd = spawn_udp_forward(&tokio::runtime::Handle::current(), front_std, backend_addr).unwrap();

        let c1 = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let c2 = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        c1.send_to(b"one", front_addr).await.unwrap();
        c2.send_to(b"two", front_addr).await.unwrap();

        let mut b1 = [0u8; 64];
        let mut b2 = [0u8; 64];
        let n1 = tokio::time::timeout(Duration::from_secs(2), c1.recv(&mut b1)).await.unwrap().unwrap();
        let n2 = tokio::time::timeout(Duration::from_secs(2), c2.recv(&mut b2)).await.unwrap().unwrap();
        let r1 = String::from_utf8(b1[..n1].to_vec()).unwrap();
        let r2 = String::from_utf8(b2[..n2].to_vec()).unwrap();
        assert!(r1.ends_with(":one"), "unexpected r1: {r1}");
        assert!(r2.ends_with(":two"), "unexpected r2: {r2}");
        // Each client got its own guest-facing source port.
        assert_ne!(r1.split(':').next(), r2.split(':').next());

        // Second packet from c1 reuses its existing session (same source port).
        c1.send_to(b"again", front_addr).await.unwrap();
        let n3 = tokio::time::timeout(Duration::from_secs(2), c1.recv(&mut b1)).await.unwrap().unwrap();
        let r3 = String::from_utf8(b1[..n3].to_vec()).unwrap();
        assert!(r3.ends_with(":again"));
        assert_eq!(r1.split(':').next(), r3.split(':').next());

        // Aborting the forwarder frees the bound port immediately.
        fwd.abort();
        let _ = fwd.await;
        server.abort();
        assert!(UdpSocket::bind(front_addr).is_ok());
    }

    #[test]
    fn parses_volumes() {
        let v = parse_volume("/Users/me/code:/app:ro").unwrap();
        assert_eq!((v.source, v.target.as_str(), v.read_only, v.name), (PathBuf::from("/Users/me/code"), "/app", true, None));
        let named = parse_volume("pgdata:/var/lib/postgresql/data").unwrap();
        assert_eq!(named.name.as_deref(), Some("pgdata"));
        assert!(named.source.ends_with("volumes/pgdata"));
        assert!(!named.read_only);
        assert!(parse_volume("/tmp:relative").is_err());
        assert!(parse_volume("/tmp:/").is_err());
        assert!(parse_volume("../x:/app").is_err());
        assert!(parse_volume("nocolon").is_err());
    }

    #[test]
    fn restart_policy_decisions_and_backoff() {
        assert!(!should_restart(RestartPolicy::No, false, 1, false));
        assert!(!should_restart(RestartPolicy::No, false, 0, false));
        assert!(!should_restart(RestartPolicy::OnFailure, false, 0, false));
        assert!(should_restart(RestartPolicy::OnFailure, false, 1, false));
        assert!(should_restart(RestartPolicy::OnFailure, false, 0, true));
        assert!(!should_restart(RestartPolicy::OnFailure, true, 143, false));
        assert!(should_restart(RestartPolicy::Always, false, 0, false));
        assert!(should_restart(RestartPolicy::Always, false, 1, false));
        assert!(!should_restart(RestartPolicy::Always, true, 0, false));

        let delays: Vec<u64> = (0..8).map(|n| restart_delay(n).as_secs()).collect();
        assert_eq!(delays, vec![1, 2, 4, 8, 16, 32, 60, 60]);
    }

    #[test]
    fn volume_helpers_validate_and_measure() {
        assert!(is_valid_volume_name("pgdata"));
        assert!(is_valid_volume_name("app_cache-1.2"));
        assert!(!is_valid_volume_name(""));
        assert!(!is_valid_volume_name(".hidden"));
        assert!(!is_valid_volume_name("../escape"));
        assert!(!is_valid_volume_name("a/b"));

        let tmp = std::env::temp_dir().join(format!("graviton-vol-test-{}", new_id()));
        fs::create_dir_all(tmp.join("sub")).unwrap();
        fs::write(tmp.join("a.txt"), b"hello").unwrap();
        fs::write(tmp.join("sub/b.bin"), b"0123456789").unwrap();
        let (size, files, created) = volume_usage(&tmp);
        assert_eq!(size, 15);
        assert_eq!(files, 2);
        assert!(created > 0);
        let _ = fs::remove_dir_all(&tmp);

        assert_eq!(ipv4_subnet_and_gateway("192.168.64.9"), Some(("192.168.64.0/24".into(), "192.168.64.1".into())));
        assert_eq!(ipv4_subnet_and_gateway("not-an-ip"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn host_memory_stats_are_sane() {
        let total = host_memory();
        let used = host_memory_used().expect("macOS host_statistics64 should succeed");
        assert!(total > 0);
        assert!(used > 0 && used <= total, "used={used}, total={total}");
    }

    #[test]
    fn resolves_healthcheck_merges_and_disables() {
        let img = HealthcheckConfig {
            test: vec!["CMD-SHELL".into(), "true".into()],
            interval: 10_000_000_000,
            timeout: 2_000_000_000,
            start_period: 0,
            retries: 0,
        };
        let inherited = resolve_healthcheck(Some(&img), None).unwrap();
        assert_eq!((inherited.interval_secs, inherited.timeout_secs, inherited.retries), (10, 2, 3));

        // Disabled via RunSpec override.
        assert_eq!(resolve_healthcheck(Some(&img), Some(Healthcheck::disabled())), None);
        // Image NONE returns None.
        let img_none = HealthcheckConfig { test: vec!["NONE".into()], ..Default::default() };
        assert_eq!(resolve_healthcheck(Some(&img_none), None), None);

        // Timing-only override applies onto image test.
        let timing_only = Healthcheck { test: vec![], interval_secs: 5, timeout_secs: 0, start_period_secs: 7, retries: 0 };
        let merged = resolve_healthcheck(Some(&img), Some(timing_only)).unwrap();
        assert_eq!((merged.interval_secs, merged.timeout_secs, merged.start_period_secs, merged.retries), (5, 2, 7, 3));
        assert_eq!(merged.test, vec!["CMD-SHELL", "true"]);
    }

    #[test]
    fn healthcheck_state_machine_and_log_cap() {
        let hc = Healthcheck { test: vec!["CMD".into(), "true".into()], interval_secs: 1, timeout_secs: 1, start_period_secs: 5, retries: 2 };
        let mut st = HealthState::default();
        let entry = |code| HealthLog { start: 1, end: 2, exit_code: code, output: format!("code={code}") };

        // Failures during start_period do not count toward streak.
        assert_eq!(apply_health_probe(&mut st, &hc, true, entry(1)), None);
        assert_eq!((st.status, st.failing_streak), (HealthStatus::Starting, 0));

        // After start_period: 1 failure increments streak but stays Starting (< retries=2).
        assert_eq!(apply_health_probe(&mut st, &hc, false, entry(1)), None);
        assert_eq!((st.status, st.failing_streak), (HealthStatus::Starting, 1));

        // Second failure crosses retries=2 -> transitions to Unhealthy.
        assert_eq!(apply_health_probe(&mut st, &hc, false, entry(1)), Some(HealthStatus::Unhealthy));
        assert_eq!((st.status, st.failing_streak), (HealthStatus::Unhealthy, 2));
        // Third failure stays Unhealthy without duplicate transition.
        assert_eq!(apply_health_probe(&mut st, &hc, false, entry(1)), None);

        // First success resets streak and recovers immediately to Healthy.
        assert_eq!(apply_health_probe(&mut st, &hc, false, entry(0)), Some(HealthStatus::Healthy));
        assert_eq!((st.status, st.failing_streak), (HealthStatus::Healthy, 0));

        // 5-entry log cap preserved even across more probes.
        assert_eq!(apply_health_probe(&mut st, &hc, false, entry(0)), None);
        assert_eq!(st.log.len(), 5);
    }

    fn normalize_network_helpers_and_validation_work() {
        assert_eq!(normalize_network_list(&[]), vec!["graviton"]);
        assert_eq!(normalize_network_list(&["bridge".into(), "app".into(), "graviton".into()]), vec!["graviton", "app"]);
        assert!(validate_user_network_name("app-net.1").is_ok());
        assert!(validate_user_network_name("bridge").is_err());
        assert!(validate_user_network_name("graviton").is_err());
        assert!(validate_user_network_name("-bad").is_err());
    }

    #[test]
    fn user_networks_crud_connect_disconnect_and_persistence() {
        normalize_network_helpers_and_validation_work();
        let dir = PathBuf::from(format!("/tmp/graviton-net-test-{}", new_id()));
        fs::create_dir_all(&dir).unwrap();
        let state_path = dir.join("containers.json");

        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let engine = Engine {
            store: Arc::new(Mutex::new(Store::new())),
            blobs: BlobStore::new(dir.join("blobs")),
            index: Mutex::new(ImageIndex::load(dir.join("images.json")).unwrap()),
            specs: Mutex::new(HashMap::new()),
            snapshots: Arc::new(crate::snapshot::SnapshotPool::open()),
            running: Mutex::new(HashMap::new()),
            stats: Mutex::new(HashMap::new()),
            health: Mutex::new(HashMap::new()),
            networks: Mutex::new(BTreeMap::new()),
            log_io: Mutex::new(()),
            log_tx: tokio::sync::broadcast::channel(8).0,
            stats_tx: tokio::sync::broadcast::channel(8).0,
            xray: Mutex::new(HashMap::new()),
            xray_tx: tokio::sync::broadcast::channel(8).0,
            switch: crate::switch::Switch::new(),
            hosts_sync: Mutex::new(()),
            hosts_gen: AtomicU64::new(0),
            events: Arc::new(crate::events::EventLog::open(dir.join("events.jsonl"))),
            auth: crate::auth::RegistryAuth::open(),
            stopping: Mutex::new(HashSet::new()),
            pending_restarts: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
            pulling: Mutex::new(HashMap::new()),
            rt: rt.handle().clone(),
            state_path: state_path.clone(),
        };

        let created = engine.create_network("frontend", false, BTreeMap::from([("tier".into(), "web".into())])).unwrap();
        assert_eq!(created.name, "frontend");
        assert!(engine.create_network("frontend", false, BTreeMap::new()).is_err());
        assert!(engine.create_network("bridge", false, BTreeMap::new()).is_err());
        assert!(engine.remove_network("graviton").is_err());

        // Seed a container & spec and attach/detach live metadata.
        let cid = new_id();
        engine.store.lock().unwrap().insert(ContainerSummary {
            id: cid.clone(),
            name: "web".into(),
            image: "alpine:3.22".into(),
            status: ContainerStatus::Created,
            exit_code: None,
            ports: vec![],
            mounts: vec![],
            ip_address: None,
            cpu_percent: None,
            memory_bytes: None,
            memory_limit_bytes: Some(512 << 20),
            created_at: 1,
            started_at: None,
            project: None,
            service: None,
            health: None,
            forked_from: None,
        }).unwrap();
        engine.specs.lock().unwrap().insert(cid.clone(), ContainerSpec {
            argv: vec!["sh".into()],
            env: vec![],
            cwd: "/".into(),
            user: None,
            cpus: 2,
            memory_mib: 512,
            image: LocalImage {
                reference: "alpine:3.22".into(),
                manifest_digest: "sha256:aa".into(),
                config_digest: "sha256:bb".into(),
                layers: vec![],
                architecture: "arm64".into(),
                pulled_at: 1,
                variants: vec![],
            },
            ports: vec![],
            volumes: vec![],
            restart: RestartPolicy::No,
            want_running: false,
            auto_remove: false,
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
            networks: vec![DEFAULT_NETWORK_NAME.into()],
            shm_size_bytes: None,
            ulimits: vec![],
            forked_from: None,
            disk_source_id: None,
            egress: EgressPolicy::default(),
            chaos: ChaosProfile::default(),
        });

        engine.connect_network("frontend", "web").unwrap();
        assert!(engine.remove_network("frontend").is_err()); // busy
        let ni = engine.network_info();
        assert_eq!(ni.networks.iter().find(|n| n.name == "frontend").unwrap().containers, vec!["web"]);

        // Persist and reload SavedState -> network survives restart.
        engine.save();
        let saved: SavedState = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(saved.networks.len(), 1);
        assert_eq!(saved.networks[0].name, "frontend");

        engine.disconnect_network("frontend", "web").unwrap();
        assert_eq!(engine.remove_network("frontend").unwrap(), "frontend");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fork_helpers_validate_names_and_merge_env() {
        assert_eq!(resolve_fork_names("pg", &[], 2).unwrap(), vec!["pg-fork-1", "pg-fork-2"]);
        assert_eq!(
            resolve_fork_names("pg", &["pg-pr-42".into()], 1).unwrap(),
            vec!["pg-pr-42"]
        );
        assert!(resolve_fork_names("pg", &[], 0).is_err());
        assert!(resolve_fork_names("pg", &[], (MAX_FORKS_PER_CALL + 1) as u32).is_err());
        assert!(resolve_fork_names("pg", &["dup".into(), "dup".into()], 2).is_err());
        assert!(resolve_fork_names("pg", &["bad/name".into()], 1).is_err());
        assert!(resolve_fork_names("pg", &["a".into()], 2).is_err());

        let merged = merge_env_vars(
            &["POSTGRES_USER=app".into(), "POSTGRES_DB=prod".into(), "KEEP_ONLY".into()],
            &["POSTGRES_DB=staging".into(), "NEW_VAR=1".into()],
        )
        .unwrap();
        assert_eq!(
            merged,
            vec!["POSTGRES_USER=app", "KEEP_ONLY", "POSTGRES_DB=staging", "NEW_VAR=1"]
        );
        assert!(merge_env_vars(&[], &["=bad".into()]).is_err());
    }

    #[test]
    fn balloon_controller_reclaims_grows_and_suppresses_jitter() {
        let mib = 1024 * 1024;
        let gstats = |total_mib: u64, avail_mib: u64| GuestStats {
            cpus: 2,
            cpu_busy: 0,
            cpu_total: 0,
            memory_total_bytes: total_mib * mib,
            memory_available_bytes: avail_mib * mib,
            net_rx_bytes: 0,
            net_tx_bytes: 0,
            processes: vec![],
        };

        let mut c = BalloonController::new(1024 * mib, 4 * mib);
        // First sample with lots of idle RAM reclaims immediately down to working_set + 80 MiB aligned to 16 MiB.
        assert_eq!(c.update(&gstats(1024, 960)), Some(144 * mib));
        // Headroom in the 32..96 MiB deadband keeps streak reset and triggers no update.
        assert_eq!(c.update(&gstats(144, 60)), None);
        // 1st sample > 96 MiB builds hysteresis streak without shrinking yet.
        assert_eq!(c.update(&gstats(256, 236)), None);
        // 2nd consecutive sample > 96 MiB shrinks down to working_set(20) + 80 = 112 MiB (aligned).
        assert_eq!(c.update(&gstats(256, 236)), Some(112 * mib));
        // Shrinks down to 80 MiB when working set drops to 0.
        assert_eq!(c.update(&gstats(256, 256)), None);
        assert_eq!(c.update(&gstats(256, 256)), Some(80 * mib));

        // Low available memory (< 32 MiB) grows immediately, clamped at max_target.
        let mut grow = BalloonController::new(512 * mib, 4 * mib);
        assert_eq!(grow.update(&gstats(512, 480)), Some(112 * mib));
        assert_eq!(grow.update(&gstats(112, 16)), Some(224 * mib));
        assert_eq!(grow.update(&gstats(224, 10)), Some(352 * mib));
        assert_eq!(grow.update(&gstats(512, 20)), Some(512 * mib));
        assert_eq!(grow.update(&gstats(512, 20)), None);
    }

    #[test]
    fn gvsp_extract_round_trip() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Read;

        let dir = std::env::temp_dir().join(format!("graviton-gvsp-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("tpl.gvsp.gz");
        let dst = dir.join("disk.img");

        let mut raw = Vec::new();
        raw.extend_from_slice(GVSP_MAGIC);
        raw.extend_from_slice(&(128 * 1024u64).to_le_bytes());
        raw.extend_from_slice(&1080u64.to_le_bytes());
        raw.extend_from_slice(&2u32.to_le_bytes());
        raw.extend_from_slice(&[0x53, 0xef]);
        raw.extend_from_slice(&(65536u64).to_le_bytes());
        raw.extend_from_slice(&4u32.to_le_bytes());
        raw.extend_from_slice(b"EXT4");
        raw.extend_from_slice(&0u64.to_le_bytes());
        raw.extend_from_slice(&0u32.to_le_bytes());

        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&raw).unwrap();
        fs::write(&src, enc.finish().unwrap()).unwrap();

        extract_gvsp_gz(&src, &dst).unwrap();
        let mut f = File::open(&dst).unwrap();
        assert_eq!(f.metadata().unwrap().len(), 128 * 1024);
        let mut buf = vec![0u8; 128 * 1024];
        f.read_exact(&mut buf).unwrap();
        assert_eq!(&buf[1080..1082], &[0x53, 0xef]);
        assert_eq!(&buf[65536..65540], b"EXT4");
        assert!(buf[..1080].iter().all(|&b| b == 0));
        assert!(buf[1082..65536].iter().all(|&b| b == 0));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn frozen_bundle_helpers_sanitize_and_validate() {
        assert!(validate_bundle_dir(Path::new("/tmp/ci-snapshots/run-1")).is_ok());
        assert!(validate_bundle_dir(Path::new("")).is_err());
        assert!(validate_bundle_dir(Path::new("/tmp/../etc")).is_err());
        assert!(validate_bundle_dir(Path::new("/System")).is_err());

        let filtered = sanitize_frozen_env(&[
            "CI=true".into(),
            "PATH=/usr/bin".into(),
            "DEPLOY_TOKEN=supersecret".into(),
            "GITHUB_API_KEY=abc".into(),
            "RUST_LOG=info".into(),
            "DB_PASSWORD=pw".into(),
        ]);
        assert_eq!(filtered, vec!["CI=true", "PATH=/usr/bin", "RUST_LOG=info"]);
        assert_eq!(sanitize_frozen_name("ntr/12 test!"), "ntr-12-test");
    }
}
