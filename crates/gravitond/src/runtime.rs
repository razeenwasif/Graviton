//! Real containers: every container runs in its own guest VM.
//!
//! `Engine` owns the image store and the containers' VMs. Its methods block
//! (registry downloads, VM boots), so the async server calls them through
//! `spawn_blocking`. Each started container gets a supervisor thread that:
//!
//! 1. boots a VM with the blob store shared read-only (virtiofs tag `blobs`),
//! 2. asks the agent to unpack the image's layers into a rootfs,
//! 3. runs the image's entrypoint inside it,
//! 4. streams stdout/stderr into the container's logs,
//! 5. records the exit code and powers the VM off.

use std::collections::HashMap;
use std::fs::File;
use std::hash::{BuildHasher, RandomState};
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use graviton_core::{ImageReference, Store, paths};
use graviton_image::{BlobStore, ImageIndex, LocalImage, Progress, Registry};
use graviton_proto::{
    AGENT_VSOCK_PORT, AgentEvent, AgentRequest, ContainerStatus, ContainerSummary, EngineInfo, ImageSummary, LogLevel,
    LogStream, PROTOCOL_VERSION, RunSpec, decode_line, encode_line,
};
use graviton_vmm::apple::{AppleHypervisor, AppleVm};
use graviton_vmm::{NetworkMode, SharedDirectory, VmConfig, VmHandle};

const DEFAULT_CPUS: u32 = 2;
const DEFAULT_MEMORY_MIB: u64 = 1024;
const STOP_GRACE: Duration = Duration::from_secs(10);

/// How to start a container again (`start`, `restart`).
#[derive(Clone)]
struct ContainerSpec {
    image: LocalImage,
    argv: Vec<String>,
    env: Vec<String>,
    cwd: String,
    user: Option<String>,
    cpus: u32,
    memory_mib: u64,
}

struct Running {
    vm: Arc<AppleVm>,
    agent: Arc<Mutex<File>>,
    pid: Option<i32>,
}

pub struct Engine {
    pub store: Arc<Mutex<Store>>,
    blobs: BlobStore,
    index: Mutex<ImageIndex>,
    specs: Mutex<HashMap<String, ContainerSpec>>,
    running: Mutex<HashMap<String, Running>>,
}

impl Engine {
    pub fn open() -> Result<Arc<Self>> {
        let blobs = BlobStore::new(paths::blobs_dir());
        std::fs::create_dir_all(blobs.root().join("sha256")).context("creating the blob store")?;
        let index = ImageIndex::load(paths::support_dir().join("images.json"))?;
        let engine = Arc::new(Self {
            store: Arc::new(Mutex::new(Store::new())),
            blobs,
            index: Mutex::new(index),
            specs: Mutex::new(HashMap::new()),
            running: Mutex::new(HashMap::new()),
        });
        engine.refresh_images();
        Ok(engine)
    }

    pub fn engine_info(&self) -> EngineInfo {
        let running = self.running.lock().unwrap();
        let specs = self.specs.lock().unwrap();
        let used: u64 = running.keys().filter_map(|id| specs.get(id)).map(|s| s.memory_mib << 20).sum();
        let disk_used: u64 = self.index.lock().unwrap().images().map(LocalImage::size).sum();
        EngineInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            protocol: PROTOCOL_VERSION,
            kernel: "Linux 6.12 (Alpine linux-virt)".into(),
            arch: graviton_image::guest_arch().into(),
            cpus: std::thread::available_parallelism().map_or(0, |n| n.get() as u32),
            memory_bytes: host_memory(),
            memory_used_bytes: used,
            disk_bytes: 0,
            disk_used_bytes: disk_used,
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
                    eprintln!("  layer {} {} {:>3}%", index + 1, &digest[7..19], if total > 0 { done * 100 / total } else { 100 });
                }
            }
        })?;
        eprintln!("  done in {:.1} s", t0.elapsed().as_secs_f64());
        self.index.lock().unwrap().insert(image.clone())?;
        self.refresh_images();
        Ok(image.summary(self.used_by(&image.reference)))
    }

    fn image_for(&self, reference: &str) -> Result<LocalImage> {
        if let Some(img) = self.index.lock().unwrap().get(reference) {
            return Ok(img.clone());
        }
        self.pull(reference)?;
        self.index.lock().unwrap().get(reference).cloned().ok_or_else(|| anyhow!("pulled {reference} but can't find it"))
    }

    fn used_by(&self, reference: &str) -> u32 {
        let specs = self.specs.lock().unwrap();
        specs.values().filter(|s| s.image.reference == reference).count() as u32
    }

    fn refresh_images(&self) {
        let summaries: Vec<ImageSummary> = {
            let index = self.index.lock().unwrap();
            index.images().map(|i| i.summary(self.used_by(&i.reference))).collect()
        };
        self.store.lock().unwrap().set_images(summaries);
    }

    // -- Containers -------------------------------------------------------

    pub fn run(self: &Arc<Self>, spec: RunSpec) -> Result<ContainerSummary> {
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
            ports: vec![],
            mounts: vec![],
            ip_address: None,
            cpu_percent: None,
            memory_bytes: None,
            memory_limit_bytes: Some(cs.memory_mib << 20),
            created_at: now_secs(),
            started_at: None,
        };
        self.store.lock().unwrap().insert(summary)?;
        self.specs.lock().unwrap().insert(id.clone(), cs);
        self.refresh_images();
        self.launch(&id)
    }

    pub fn start(self: &Arc<Self>, key: &str) -> Result<ContainerSummary> {
        let id = self.store.lock().unwrap().container(key)?.id.clone();
        if self.running.lock().unwrap().contains_key(&id) {
            bail!("container {key} is already running");
        }
        if !self.specs.lock().unwrap().contains_key(&id) {
            bail!("container {key} can't be started again (it was created by another gravitond)");
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
        if let Some(pid) = pid {
            let _ = agent.lock().unwrap().write_all(&encode_line(&AgentRequest::Signal { pid, signal: libc_sigterm() }));
        }
        let deadline = Instant::now() + STOP_GRACE;
        while self.running.lock().unwrap().contains_key(&id) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        if self.running.lock().unwrap().contains_key(&id) {
            // Didn't exit on SIGTERM: power the VM off; the supervisor records it.
            let _ = vm.kill();
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.running.lock().unwrap().contains_key(&id) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
            }
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
        self.specs.lock().unwrap().remove(&removed.id);
        let _ = std::fs::remove_dir_all(paths::vms_dir().join(&removed.id));
        self.refresh_images();
        Ok(removed.id)
    }

    /// Start the supervisor and wait until the process is running (or failed).
    fn launch(self: &Arc<Self>, id: &str) -> Result<ContainerSummary> {
        self.store.lock().unwrap().update(id, |c| {
            c.status = ContainerStatus::Created;
            c.exit_code = None;
        });
        let engine = Arc::clone(self);
        let owned = id.to_string();
        std::thread::Builder::new().name(format!("container-{id}")).spawn(move || engine.supervise(&owned))?;

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

    fn supervise(&self, id: &str) {
        let result = self.run_in_vm(id);
        let code = match result {
            Ok(code) => code,
            Err(err) => {
                self.log(id, LogLevel::Error, LogStream::System, format!("{err:#}"));
                125
            }
        };
        if let Some(r) = self.running.lock().unwrap().remove(id) {
            let _ = r.agent.lock().unwrap().write_all(&encode_line(&AgentRequest::Shutdown));
            if r.vm.wait_stopped(Duration::from_secs(5)).is_err() {
                let _ = r.vm.kill();
            }
        }
        self.store.lock().unwrap().update(id, |c| {
            c.status = ContainerStatus::Exited;
            c.exit_code = Some(code);
            c.started_at = None;
        });
    }

    fn run_in_vm(&self, id: &str) -> Result<i32> {
        let spec = self.specs.lock().unwrap().get(id).cloned().ok_or_else(|| anyhow!("unknown container {id}"))?;
        let name = self.store.lock().unwrap().container(id)?.name.clone();
        let t0 = Instant::now();

        let guest = paths::guest_dir();
        let vm_dir = paths::vms_dir().join(id);
        std::fs::create_dir_all(&vm_dir)?;
        let mut config = VmConfig::new(guest.join("vmlinux"), guest.join("initrd.img"), "");
        config.cpus = spec.cpus;
        config.memory_mib = spec.memory_mib;
        config.network = NetworkMode::None;
        config.console_log = Some(vm_dir.join("console.log"));
        config.shares = vec![SharedDirectory { host_path: self.blobs.root().to_path_buf(), tag: "blobs".into(), read_only: true }];

        let vm = Arc::new(AppleHypervisor::new().boot_vm(&config).context("booting the container's VM")?);
        let fd = vm.connect(AGENT_VSOCK_PORT).context("connecting to the guest agent")?;
        let agent = Arc::new(Mutex::new(File::from(fd)));
        let mut events = Events::new(BufReader::new(agent.lock().unwrap().try_clone()?));
        match events.next()? {
            AgentEvent::Ready { .. } => {}
            other => bail!("expected ready from the agent, got {other:?}"),
        }
        self.running.lock().unwrap().insert(id.to_string(), Running { vm: Arc::clone(&vm), agent: Arc::clone(&agent), pid: None });
        let booted = t0.elapsed();

        send(&agent, &AgentRequest::Prepare { container: id.into(), layers: spec.image.layers.iter().map(|l| l.digest.clone()).collect() })?;
        let (files, unpack_ms) = match events.next()? {
            AgentEvent::Prepared { files, ms, .. } => (files, ms),
            AgentEvent::Error { message } => bail!("{message}"),
            other => bail!("expected prepared from the agent, got {other:?}"),
        };
        self.log(
            id,
            LogLevel::Info,
            LogStream::System,
            format!("VM booted in {} ms; unpacked {files} files in {unpack_ms} ms", booted.as_millis()),
        );

        send(&agent, &AgentRequest::Exec {
            argv: spec.argv.clone(),
            env: spec.env.clone(),
            cwd: spec.cwd.clone(),
            tty: false,
            container: Some(id.into()),
            user: spec.user.clone(),
            hostname: Some(name),
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
                    });
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
                AgentEvent::Ready { .. } | AgentEvent::Prepared { .. } => {}
            }
        }
    }

    fn log(&self, id: &str, level: LogLevel, stream: LogStream, message: String) {
        self.store.lock().unwrap().push_log(id, level, stream, message);
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

struct Events {
    reader: BufReader<File>,
    line: String,
}

impl Events {
    fn new(reader: BufReader<File>) -> Self {
        Self { reader, line: String::new() }
    }

    fn next(&mut self) -> Result<AgentEvent> {
        self.line.clear();
        if self.reader.read_line(&mut self.line)? == 0 {
            bail!("the guest agent closed the connection (the VM may have stopped)");
        }
        Ok(decode_line(self.line.as_bytes())?)
    }
}

/// Splits a byte stream into lines for the log.
#[derive(Default)]
struct LineBuffer(Vec<u8>);

impl LineBuffer {
    fn push(&mut self, data: &[u8]) -> Vec<String> {
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

    fn finish(&mut self) -> Option<String> {
        (!self.0.is_empty()).then(|| String::from_utf8_lossy(&std::mem::take(&mut self.0)).trim_end_matches('\r').to_string())
    }
}

fn send(agent: &Arc<Mutex<File>>, request: &AgentRequest) -> Result<()> {
    agent.lock().unwrap().write_all(&encode_line(request)).context("writing to the guest agent")
}

fn new_id() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let a = RandomState::new().hash_one(nanos);
    let b = RandomState::new().hash_one(a);
    format!("{:016x}", a ^ b.rotate_left(17))[..12].to_string()
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

const fn libc_sigterm() -> i32 {
    15
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
    fn ids_are_12_hex() {
        let a = new_id();
        assert_eq!(a.len(), 12);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, new_id());
    }
}
