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

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher, RandomState};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use graviton_core::{ImageReference, Store, paths};
use graviton_image::{BlobStore, ImageIndex, LocalImage, Progress, Registry};
use graviton_proto::{
    AGENT_VSOCK_PORT, AgentEvent, AgentRequest, ContainerDetails, ContainerStats, ContainerStatus, ContainerSummary,
    DirectoryListing, EngineInfo, ExecSpec, FileContents, GuestMount, GuestStats, HostEntry, ImageSummary, LogLevel, LogLine, LogStream,
    Mount, PROTOCOL_VERSION, PortMapping, PortProtocol, ProcessInfo, Response, RunSpec, StatSample, decode_line, encode_line,
};
use graviton_vmm::apple::{AppleHypervisor, AppleVm};
use graviton_vmm::{NetworkMode, SharedDirectory, VmConfig, VmHandle};
use serde::{Deserialize, Serialize};

const DEFAULT_CPUS: u32 = 2;
const DEFAULT_MEMORY_MIB: u64 = 1024;
const STOP_GRACE: Duration = Duration::from_secs(10);
const KERNEL: &str = "Linux 6.12 (Alpine linux-virt)";
/// How often running containers are sampled, and how many samples are kept (5 minutes).
const STATS_INTERVAL: Duration = Duration::from_secs(2);
const STATS_KEPT: usize = 150;
/// The Files tab previews this much of a file.
const READ_LIMIT: u64 = 256 * 1024;

/// How to start a container (again).
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ContainerSpec {
    pub(crate) image: LocalImage,
    argv: Vec<String>,
    env: Vec<String>,
    cwd: String,
    user: Option<String>,
    cpus: u32,
    memory_mib: u64,
    #[serde(default)]
    ports: Vec<PortMapping>,
    #[serde(default)]
    volumes: Vec<Volume>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Volume {
    /// Host directory shared into the VM.
    source: PathBuf,
    /// Absolute path inside the container.
    target: String,
    read_only: bool,
    /// Set for named volumes (`name:/path`).
    name: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct SavedState {
    containers: Vec<ContainerSummary>,
    specs: HashMap<String, ContainerSpec>,
}

struct Running {
    name: String,
    vm: Arc<AppleVm>,
    agent: Arc<Mutex<File>>,
    pid: Option<i32>,
    /// IPv4 address on the container network once DHCP has finished.
    ip: Option<String>,
    /// Whether the guest runs the embedded DNS resolver (`SetHosts`).
    dns: bool,
    forwards: Vec<tokio::task::JoinHandle<()>>,
    /// The VM's port on the container network; unplugged when this is dropped (after the VM stops).
    _port: Option<crate::switch::Port>,
}

#[derive(Default)]
struct StatsHistory {
    cpus: u32,
    samples: VecDeque<StatSample>,
    processes: Vec<ProcessInfo>,
}

pub struct Engine {
    pub store: Arc<Mutex<Store>>,
    pub(crate) blobs: BlobStore,
    pub(crate) index: Mutex<ImageIndex>,
    pub(crate) specs: Mutex<HashMap<String, ContainerSpec>>,
    running: Mutex<HashMap<String, Running>>,
    stats: Mutex<HashMap<String, StatsHistory>>,
    switch: crate::switch::Switch,
    /// Serialises guest DNS table broadcasts so concurrent starts/stops never reorder.
    hosts_sync: Mutex<()>,
    hosts_gen: AtomicU64,
    /// What happened, for the Activity feed and push alerts.
    pub events: Arc<crate::events::EventLog>,
    /// Containers being stopped on request (so their exit isn't a failure).
    stopping: Mutex<HashSet<String>>,
    /// One pull per image at a time: a second waits for the first, then finds it.
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
        let engine = Arc::new(Self {
            store: Arc::new(Mutex::new(Store::new())),
            blobs,
            index: Mutex::new(index),
            specs: Mutex::new(HashMap::new()),
            running: Mutex::new(HashMap::new()),
            stats: Mutex::new(HashMap::new()),
            switch: crate::switch::Switch::new(),
            hosts_sync: Mutex::new(()),
            hosts_gen: AtomicU64::new(0),
            events: Arc::new(crate::events::EventLog::open(paths::support_dir().join("events.jsonl"))),
            stopping: Mutex::new(HashSet::new()),
            pulling: Mutex::new(HashMap::new()),
            rt: tokio::runtime::Handle::current(),
            state_path: paths::support_dir().join("containers.json"),
        });
        engine.restore()?;
        let known: HashSet<String> = engine.store.lock().unwrap().containers().iter().map(|c| c.id.clone()).collect();
        remove_orphan_disks(&known);
        engine.refresh_images();
        Ok(engine)
    }

    pub fn engine_info(&self) -> EngineInfo {
        let running = self.running.lock().unwrap();
        let specs = self.specs.lock().unwrap();
        let used: u64 = running.keys().filter_map(|id| specs.get(id)).map(|s| s.memory_mib << 20).sum();
        // Images, plus what containers' disks really take (they're sparse).
        let images: u64 = self.index.lock().unwrap().images().map(LocalImage::size).sum();
        let disks: u64 = self.store.lock().unwrap().containers().iter().map(|c| disk_usage(&paths::vms_dir().join(&c.id))).sum();
        let disk_used = images + disks;
        EngineInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            protocol: PROTOCOL_VERSION,
            kernel: KERNEL.into(),
            arch: graviton_image::guest_arch().into(),
            cpus: std::thread::available_parallelism().map_or(0, |n| n.get() as u32),
            memory_bytes: host_memory(),
            memory_used_bytes: used,
            disk_bytes: 0,
            disk_used_bytes: disk_used,
        }
    }

    // -- Saved state --------------------------------------------------------

    fn restore(&self) -> Result<()> {
        let saved: SavedState = match fs::read(&self.state_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("reading {}", self.state_path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let mut interrupted = Vec::new();
        {
            let mut store = self.store.lock().unwrap();
            for mut c in saved.containers {
                if matches!(c.status, ContainerStatus::Running | ContainerStatus::Created | ContainerStatus::Paused) {
                    // Its VM ended with the previous gravitond.
                    c.status = ContainerStatus::Exited;
                    c.started_at = None;
                    interrupted.push(c.id.clone());
                }
                c.ip_address = None;
                c.cpu_percent = None;
                c.memory_bytes = None;
                let id = c.id.clone();
                if store.insert(c).is_ok() {
                    store.set_logs(&id, read_log(&log_path(&id)));
                }
            }
        }
        *self.specs.lock().unwrap() = saved.specs;
        for id in interrupted {
            self.log(&id, LogLevel::Warn, LogStream::System, "gravitond restarted while this container was running; its VM stopped".into());
        }
        Ok(())
    }

    fn save(&self) {
        let state = SavedState {
            containers: self.store.lock().unwrap().containers().to_vec(),
            specs: self.specs.lock().unwrap().clone(),
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

    pub fn pull(&self, reference: &str) -> Result<ImageSummary> {
        let parsed = ImageReference::parse(reference)?;
        eprintln!("pull {}", parsed.display_name());
        let t0 = Instant::now();
        let mut last_report = Instant::now();
        let image = Registry::new().pull(&parsed, &self.blobs, |p| match p {
            Progress::Resolved { layers, total_bytes, .. } => {
                eprintln!("  {layers} layers, {:.1} MiB", total_bytes as f64 / 1048576.0)
            }
            Progress::Layer { index, digest, done, total, cached } => {
                if cached {
                    eprintln!("  layer {} {} (cached)", index + 1, &digest[7..19]);
                } else if done == total || last_report.elapsed() > Duration::from_secs(1) {
                    last_report = Instant::now();
                    eprintln!(
                        "  layer {} {} {:>3}%",
                        index + 1,
                        &digest[7..19],
                        (done * 100).checked_div(total).unwrap_or(100)
                    );
                }
            }
        })?;
        eprintln!("  done in {:.1} s", t0.elapsed().as_secs_f64());
        self.index.lock().unwrap().insert(image.clone())?;
        self.refresh_images();
        let summary = image.summary(self.used_by(&image.reference));
        let name = format!("{}:{}", summary.repository, summary.tag);
        self.events.record(graviton_proto::EventKind::Pulled, None, Some(&name), format!("Pulled {name} ({:.1} MB)", summary.size_bytes as f64 / 1048576.0));
        Ok(summary)
    }

    pub(crate) fn image_for(&self, reference: &str) -> Result<LocalImage> {
        if let Some(img) = self.index.lock().unwrap().get(reference) {
            return Ok(img.clone());
        }
        // Two containers of a new image at once: one pull, and the other waits for it.
        let gate = Arc::clone(self.pulling.lock().unwrap().entry(reference.to_string()).or_default());
        let _turn = gate.lock().unwrap();
        if let Some(img) = self.index.lock().unwrap().get(reference) {
            return Ok(img.clone());
        }
        self.pull(reference)?;
        self.index.lock().unwrap().get(reference).cloned().ok_or_else(|| anyhow!("pulled {reference} but can't find it"))
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

    // -- Containers -------------------------------------------------------

    pub fn run(self: &Arc<Self>, spec: RunSpec) -> Result<ContainerSummary> {
        let ports = spec.ports.iter().map(|p| parse_port(p)).collect::<Result<Vec<_>>>()?;
        let volumes = spec.volumes.iter().map(|v| parse_volume(v)).collect::<Result<Vec<_>>>()?;
        if let Some(name) = &spec.name {
            if self.store.lock().unwrap().name_taken(name) {
                bail!("the name {name} is already in use");
            }
        }
        let image = self.image_for(&spec.image)?;
        let config = image.config(&self.blobs)?.config;
        let argv = config.argv(spec.command.as_deref());
        if argv.is_empty() {
            bail!("{} has no entrypoint or command; pass one after the image name", spec.image);
        }
        let display = ImageReference::parse(&image.reference).map(|r| r.display_name()).unwrap_or(image.reference.clone());
        let cs = ContainerSpec {
            argv,
            env: config.env(&spec.env),
            cwd: config.working_dir(),
            user: config.user.clone().filter(|u| !u.is_empty()),
            cpus: spec.cpus.unwrap_or(DEFAULT_CPUS),
            memory_mib: spec.memory_mib.unwrap_or(DEFAULT_MEMORY_MIB),
            image,
            ports,
            volumes,
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
        };
        self.store.lock().unwrap().insert(summary)?;
        self.specs.lock().unwrap().insert(id.clone(), cs);
        self.save();
        self.refresh_images();
        self.launch(&id)
    }

    pub fn start(self: &Arc<Self>, key: &str) -> Result<ContainerSummary> {
        let id = self.store.lock().unwrap().container(key)?.id.clone();
        if self.running.lock().unwrap().contains_key(&id) {
            bail!("container {key} is already running");
        }
        if !self.specs.lock().unwrap().contains_key(&id) {
            bail!("container {key} has no saved settings, so it can't be started again");
        }
        self.launch(&id)
    }

    pub fn stop(&self, key: &str) -> Result<ContainerSummary> {
        let id = self.store.lock().unwrap().container(key)?.id.clone();
        let (agent, pid, vm) = {
            let running = self.running.lock().unwrap();
            let r = running.get(&id).ok_or_else(|| anyhow!("container {key} isn't running"))?;
            (Arc::clone(&r.agent), r.pid, Arc::clone(&r.vm))
        };
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
        Ok(self.store.lock().unwrap().container(&id)?.clone())
    }

    pub fn restart(self: &Arc<Self>, key: &str) -> Result<ContainerSummary> {
        let id = self.store.lock().unwrap().container(key)?.id.clone();
        if self.running.lock().unwrap().contains_key(&id) {
            self.stop(&id)?;
        }
        self.launch(&id)
    }

    pub fn remove(&self, key: &str) -> Result<String> {
        let removed = self.store.lock().unwrap().remove(key)?;
        self.events.record(graviton_proto::EventKind::Removed, Some((&removed.id, &removed.name)), Some(&removed.image), format!("{} removed", removed.name));
        self.specs.lock().unwrap().remove(&removed.id);
        self.stats.lock().unwrap().remove(&removed.id);
        let _ = fs::remove_dir_all(paths::vms_dir().join(&removed.id));
        self.save();
        self.refresh_images();
        Ok(removed.id)
    }

    /// Start the supervisor and wait until the process is running (or failed).
    fn launch(self: &Arc<Self>, id: &str) -> Result<ContainerSummary> {
        let ports = self.specs.lock().unwrap().get(id).map(|s| s.ports.clone()).unwrap_or_default();
        // Bind the published ports first, so a port in use fails before a VM boots.
        let mut listeners = Vec::new();
        for p in &ports {
            match TcpListener::bind(("127.0.0.1", p.host_port)) {
                Ok(listener) => listeners.push((listener, p.container_port)),
                Err(err) => {
                    let message = format!("port {} is already in use on this Mac ({err})", p.host_port);
                    self.log(id, LogLevel::Error, LogStream::System, message.clone());
                    self.store.lock().unwrap().update(id, |c| {
                        c.status = ContainerStatus::Exited;
                        c.exit_code = Some(125);
                    });
                    self.save();
                    bail!(message);
                }
            }
        }

        self.store.lock().unwrap().update(id, |c| {
            c.status = ContainerStatus::Created;
            c.exit_code = None;
        });
        let engine = Arc::clone(self);
        let owned = id.to_string();
        std::thread::Builder::new().name(format!("container-{id}")).spawn(move || engine.supervise(&owned, listeners))?;

        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let c = self.store.lock().unwrap().container(id)?.clone();
            if c.status != ContainerStatus::Created {
                return Ok(c);
            }
            if Instant::now() > deadline {
                bail!("container {} took too long to start", c.name);
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    fn supervise(self: &Arc<Self>, id: &str, listeners: Vec<(TcpListener, u16)>) {
        let result = self.run_in_vm(id, listeners);
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
            let _ = r.agent.lock().unwrap().write_all(&encode_line(&AgentRequest::Shutdown));
            if r.vm.wait_stopped(Duration::from_secs(5)).is_err() {
                let _ = r.vm.kill();
            }
        }
        self.store.lock().unwrap().update(id, |c| {
            c.status = ContainerStatus::Exited;
            c.exit_code = Some(code);
            c.started_at = None;
            c.ip_address = None;
            c.cpu_percent = None;
            c.memory_bytes = None;
        });
        self.save();
        let (name, image) = self.store.lock().unwrap().container(id).map(|c| (c.name.clone(), c.image.clone())).unwrap_or_default();
        let (kind, message) = match (&failure, requested, code) {
            (Some(why), _, _) => (graviton_proto::EventKind::Failed, format!("{name} couldn't start: {why}")),
            (None, true, _) => (graviton_proto::EventKind::Stopped, format!("{name} stopped")),
            (None, false, 0) => (graviton_proto::EventKind::Stopped, format!("{name} exited")),
            (None, false, code) => (graviton_proto::EventKind::Failed, format!("{name} exited with code {code}")),
        };
        self.events.record(kind, Some((id, &name)), Some(&image), message);
    }

    fn run_in_vm(self: &Arc<Self>, id: &str, listeners: Vec<(TcpListener, u16)>) -> Result<i32> {
        // The container's disk stays for its next start; `grav rm` deletes it.
        self.run_in_vm_inner(id, listeners)
    }

    fn run_in_vm_inner(self: &Arc<Self>, id: &str, listeners: Vec<(TcpListener, u16)>) -> Result<i32> {
        let spec = self.specs.lock().unwrap().get(id).cloned().ok_or_else(|| anyhow!("unknown container {id}"))?;
        let name = self.store.lock().unwrap().container(id)?.name.clone();
        let t0 = Instant::now();

        let guest = paths::guest_dir();
        let vm_dir = paths::vms_dir().join(id);
        fs::create_dir_all(&vm_dir)?;
        let mut config = VmConfig::new(guest.join("vmlinux"), guest.join("initrd.img"), container_disk(&vm_dir)?);
        config.cpus = spec.cpus;
        config.memory_mib = spec.memory_mib;
        config.network = NetworkMode::Nat;
        config.console_log = Some(vm_dir.join("console.log"));
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
        let port = self.switch.attach().context("joining the container network")?;
        config.switch_port = Some(port.vm_fd());
        let vm = Arc::new(AppleHypervisor::new().boot_vm(&config).context("booting the container's VM")?);
        let fd = vm.connect(AGENT_VSOCK_PORT).context("connecting to the guest agent")?;
        let agent = Arc::new(Mutex::new(File::from(fd)));
        let mut events = Events::new(BufReader::new(agent.lock().unwrap().try_clone()?));
        match events.next()? {
            AgentEvent::Ready { .. } => {}
            other => bail!("expected ready from the agent, got {other:?}"),
        }
        self.running.lock().unwrap().insert(
            id.to_string(),
            Running {
                name: name.clone(),
                vm: Arc::clone(&vm),
                agent: Arc::clone(&agent),
                pid: None,
                ip: None,
                dns: false,
                forwards: vec![],
                _port: Some(port),
            },
        );
        let booted = t0.elapsed();

        send(&agent, &AgentRequest::Prepare {
            container: id.into(),
            layers: spec.image.layers.iter().map(|l| l.digest.clone()).collect(),
            mounts,
            hostname: Some(name.clone()),
        })?;
        let (files, unpack_ms, ip, reused, dns) = match events.next()? {
            AgentEvent::Prepared { files, ms, ip, reused, dns, .. } => (files, ms, ip, reused, dns),
            AgentEvent::Error { message } => bail!("{message}"),
            other => bail!("expected prepared from the agent, got {other:?}"),
        };
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
                "VM booted in {} ms; {}; {}",
                booted.as_millis(),
                if reused { format!("kept its files from the last run ({unpack_ms} ms)") } else { format!("unpacked {files} files in {unpack_ms} ms") },
                match &ip {
                    Some(ip) => format!("network {ip}"),
                    None => "no network".into(),
                }
            ),
        );

        // Publish ports now that the guest has an address.
        if !listeners.is_empty() {
            match address {
                Some(ip) => {
                    let mut handles = Vec::new();
                    for (listener, container_port) in listeners {
                        handles.push(self.forward(listener, SocketAddr::new(ip, container_port))?);
                    }
                    if let Some(r) = self.running.lock().unwrap().get_mut(id) {
                        r.forwards = handles;
                    }
                }
                None => self.log(id, LogLevel::Warn, LogStream::System, "the VM got no network address, so its ports aren't published".into()),
            }
        }

        send(&agent, &AgentRequest::Exec {
            argv: spec.argv.clone(),
            env: spec.env.clone(),
            cwd: spec.cwd.clone(),
            tty: false,
            container: Some(id.into()),
            user: spec.user.clone(),
            hostname: Some(name),
            rows: 0,
            cols: 0,
        })?;

        let mut out = LineBuffer::default();
        let mut err = LineBuffer::default();
        loop {
            match events.next()? {
                AgentEvent::Started { pid } => {
                    if let Some(r) = self.running.lock().unwrap().get_mut(id) {
                        r.pid = Some(pid);
                    }
                    self.store.lock().unwrap().update(id, |c| {
                        c.status = ContainerStatus::Running;
                        c.started_at = Some(now_secs());
                        c.ip_address = ip.clone();
                    });
                    self.save();
                    self.start_monitor(id, Arc::clone(&vm));
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
    fn sync_hosts(&self) {
        let _turn = self.hosts_sync.lock().unwrap();
        let (hosts, targets): (Vec<HostEntry>, Vec<Arc<Mutex<File>>>) = {
            let running = self.running.lock().unwrap();
            let hosts = running
                .iter()
                .filter_map(|(id, r)| {
                    r.ip.as_ref().map(|ip| HostEntry {
                        name: r.name.clone(),
                        id: id.clone(),
                        ip: ip.clone(),
                    })
                })
                .collect();
            let targets = running
                .values()
                .filter(|r| r.dns && r.ip.is_some())
                .map(|r| Arc::clone(&r.agent))
                .collect();
            (hosts, targets)
        };
        if targets.is_empty() {
            return;
        }
        let generation = self.hosts_gen.fetch_add(1, Ordering::Relaxed) + 1;
        let msg = encode_line(&AgentRequest::SetHosts { generation, hosts });
        for agent in targets {
            let _ = agent.lock().unwrap().write_all(&msg);
        }
    }

    /// Forward connections on `listener` (127.0.0.1) to `target` in the guest.
    fn forward(&self, listener: TcpListener, target: SocketAddr) -> Result<tokio::task::JoinHandle<()>> {
        listener.set_nonblocking(true)?;
        let _guard = self.rt.enter();
        let listener = tokio::net::TcpListener::from_std(listener)?;
        Ok(self.rt.spawn(async move {
            loop {
                let Ok((mut inbound, _)) = listener.accept().await else { continue };
                tokio::spawn(async move {
                    if let Ok(mut outbound) = tokio::net::TcpStream::connect(target).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
            }
        }))
    }

    // -- Stats ------------------------------------------------------------

    /// Sample the container every [`STATS_INTERVAL`] on its own agent
    /// connection, until its VM stops.
    fn start_monitor(self: &Arc<Self>, id: &str, vm: Arc<AppleVm>) {
        let engine = Arc::clone(self);
        let id = id.to_string();
        let limit = self.specs.lock().unwrap().get(&id).map_or(0, |s| s.memory_mib << 20);
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
            engine.stats.lock().unwrap().insert(id.clone(), StatsHistory::default());
            let mut previous: Option<GuestStats> = None;
            loop {
                let still_running = engine.running.lock().unwrap().get(&id).is_some_and(|r| Arc::ptr_eq(&r.vm, &vm));
                if !still_running || conn.write_all(&encode_line(&AgentRequest::Stats { processes: true })).is_err() {
                    break;
                }
                let Ok(AgentEvent::Stats(now)) = events.next() else { break };
                if let Some(prev) = &previous {
                    let sample = sample_between(prev, &now, limit);
                    engine.store.lock().unwrap().update(&id, |c| {
                        c.cpu_percent = Some(sample.cpu_percent);
                        c.memory_bytes = Some(sample.memory_bytes);
                    });
                    let mut stats = engine.stats.lock().unwrap();
                    let history = stats.entry(id.clone()).or_default();
                    history.cpus = now.cpus;
                    history.samples.push_back(sample);
                    while history.samples.len() > STATS_KEPT {
                        history.samples.pop_front();
                    }
                    history.processes = now.processes.clone();
                }
                previous = Some(now);
                std::thread::sleep(STATS_INTERVAL);
            }
        });
    }

    pub fn stats(&self, key: &str) -> Result<ContainerStats> {
        let id = self.store.lock().unwrap().container(key)?.id.clone();
        let cpus = self.specs.lock().unwrap().get(&id).map_or(0, |s| s.cpus);
        let running = self.running.lock().unwrap().contains_key(&id);
        let stats = self.stats.lock().unwrap();
        let history = stats.get(&id);
        Ok(ContainerStats {
            id,
            cpus: history.map_or(cpus, |h| h.cpus.max(1)),
            interval_secs: STATS_INTERVAL.as_secs() as u32,
            samples: history.map(|h| h.samples.iter().cloned().collect()).unwrap_or_default(),
            processes: if running { history.map(|h| h.processes.clone()).unwrap_or_default() } else { vec![] },
        })
    }

    // -- Files and details --------------------------------------------------

    /// Send one request on a fresh agent connection and return its answer.
    fn agent_call(&self, key: &str, request: impl FnOnce(String) -> AgentRequest) -> Result<AgentEvent> {
        let id = self.store.lock().unwrap().container(key)?.id.clone();
        let vm = self.running.lock().unwrap().get(&id).map(|r| Arc::clone(&r.vm)).ok_or_else(|| anyhow!("container {key} isn't running"))?;
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

    pub fn inspect(&self, key: &str) -> Result<ContainerDetails> {
        let container = self.store.lock().unwrap().container(key)?.clone();
        let spec = self.specs.lock().unwrap().get(&container.id).cloned().ok_or_else(|| anyhow!("container {key} has no saved settings"))?;
        Ok(ContainerDetails {
            image_reference: spec.image.reference.clone(),
            image_digest: spec.image.manifest_digest.clone(),
            command: spec.argv,
            env: spec.env,
            working_dir: spec.cwd,
            user: spec.user,
            hostname: container.name.clone(),
            cpus: spec.cpus,
            memory_mib: spec.memory_mib,
            kernel: KERNEL.into(),
            vm_dir: paths::vms_dir().join(&container.id).display().to_string(),
            container,
        })
    }

    // -- exec -------------------------------------------------------------

    /// Open a fresh agent connection and start `spec` in the container.
    /// Returns the connection, positioned just after the agent's `started` line.
    fn exec_start(&self, spec: &ExecSpec) -> Result<File> {
        let (id, name) = {
            let store = self.store.lock().unwrap();
            let c = store.container(&spec.id)?;
            (c.id.clone(), c.name.clone())
        };
        let vm = self.running.lock().unwrap().get(&id).map(|r| Arc::clone(&r.vm)).ok_or_else(|| anyhow!("container {} isn't running", spec.id))?;
        let cs = self.specs.lock().unwrap().get(&id).cloned().ok_or_else(|| anyhow!("unknown container {}", spec.id))?;
        if spec.command.is_empty() {
            bail!("give a command to run");
        }
        let mut env = cs.env.clone();
        env.extend(spec.env.iter().cloned());

        let fd: OwnedFd = vm.connect(AGENT_VSOCK_PORT)?;
        let mut conn = File::from(fd);
        match read_event_unbuffered(&mut conn)? {
            AgentEvent::Ready { .. } => {}
            other => bail!("expected ready from the agent, got {other:?}"),
        }
        conn.write_all(&encode_line(&AgentRequest::Exec {
            argv: spec.command.clone(),
            env,
            cwd: cs.cwd.clone(),
            tty: spec.tty,
            container: Some(id),
            user: cs.user.clone(),
            hostname: Some(name),
            rows: spec.rows,
            cols: spec.cols,
        }))?;
        match read_event_unbuffered(&mut conn)? {
            AgentEvent::Started { .. } => Ok(conn),
            AgentEvent::Error { message } => bail!("{message}"),
            other => bail!("expected started from the agent, got {other:?}"),
        }
    }

    /// Run a command without a TTY, sending its output and exit code to `out`.
    pub fn exec_stream(&self, spec: &ExecSpec, out: tokio::sync::mpsc::Sender<Response>) -> Result<()> {
        let conn = self.exec_start(spec)?;
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
        Ok(OwnedFd::from(self.exec_start(spec)?))
    }

    fn log(&self, id: &str, level: LogLevel, stream: LogStream, message: String) {
        let line = self.store.lock().unwrap().push_log(id, level, stream, message);
        append_log(&log_path(id), &line);
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

/// `8080:80`, `8080:80/tcp` or `80` (same port on both sides).
fn parse_port(spec: &str) -> Result<PortMapping> {
    let (ports, proto) = spec.split_once('/').unwrap_or((spec, "tcp"));
    if proto != "tcp" {
        bail!("`{spec}`: only TCP ports can be published so far");
    }
    let parse = |s: &str| s.trim().parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(|| anyhow!("`{spec}` isn't a valid port mapping (try 8080:80)"));
    let (host_port, container_port) = match ports.split(':').collect::<Vec<_>>()[..] {
        [one] => (parse(one)?, parse(one)?),
        [host, container] => (parse(host)?, parse(container)?),
        _ => bail!("`{spec}` isn't a valid port mapping (try 8080:80)"),
    };
    Ok(PortMapping { host_port, container_port, protocol: PortProtocol::Tcp })
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
    let valid = !source.is_empty()
        && source.as_bytes()[0].is_ascii_alphanumeric()
        && source.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b));
    if !valid {
        bail!("`{source}` isn't an absolute path or a volume name (letters, digits, _ . -)");
    }
    Ok(Volume { source: paths::support_dir().join("volumes").join(source), target, read_only, name: Some(source.to_string()) })
}

fn log_path(id: &str) -> PathBuf {
    paths::vms_dir().join(id).join("log.jsonl")
}

fn append_log(path: &Path, line: &LogLine) {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(&encode_line(line));
    }
}

fn read_log(path: &Path) -> Vec<LogLine> {
    let Ok(f) = File::open(path) else { return vec![] };
    BufReader::new(f).lines().map_while(Result::ok).filter_map(|l| decode_line(l.as_bytes()).ok()).collect()
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

fn host_memory() -> u64 {
    std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// The file a VM's scratch disk lives in, beside its console log.
pub(crate) const SCRATCH_DISK: &str = "disk.img";

/// A fresh, empty, sparse disk image in `dir` for a VM to keep containers'
/// files on (the guest formats it at boot). It holds up to
/// `$GRAVITON_DISK_GIB` (default 64) GiB, but takes only what's written to it
/// on the Mac's disk. Their files used to live in the VM's RAM, so an image
/// and its build's output had to fit in the memory the container was given.
pub(crate) fn scratch_disk(dir: &Path) -> Result<PathBuf> {
    let gib: u64 = std::env::var("GRAVITON_DISK_GIB").ok().and_then(|v| v.parse().ok()).filter(|g| (1..=2048).contains(g)).unwrap_or(64);
    let path = dir.join(SCRATCH_DISK);
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
        assert_eq!(parse_port("8080:80").unwrap(), PortMapping { host_port: 8080, container_port: 80, protocol: PortProtocol::Tcp });
        assert_eq!(parse_port("5432").unwrap().container_port, 5432);
        assert_eq!(parse_port("3000:3000/tcp").unwrap().host_port, 3000);
        assert!(parse_port("53:53/udp").is_err());
        assert!(parse_port("0:80").is_err());
        assert!(parse_port("a:b:c").is_err());
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
}
