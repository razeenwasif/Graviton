//! `grav`: command-line client for gravitond, on this Mac or a paired one.

mod remote;
mod service;

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use graviton_core::{ImageReference, paths};
use std::sync::{Arc, Mutex};

use graviton_proto::frame::{Decoder, Frame};
use remote::Target;
use graviton_proto::{BuildSpec, ContainerStatus, ContainerSummary, ExecSpec, FileKind, LogStream, Request, Response, RunSpec, decode_line, encode_line};

#[derive(Parser, Debug)]
#[command(version, about = "Native containers for macOS")]
struct Cli {
    /// Control socket path (default: $GRAVITON_SOCKET or ~/Library/Application Support/Graviton/gravitond.sock)
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    /// Run on a paired Mac (see `grav hosts`), or `local` for this one.
    #[arg(long, global = true, env = "GRAVITON_HOST")]
    host: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Check that the daemon is reachable.
    Ping,
    /// Show engine details.
    Info,
    /// List containers.
    Ps {
        /// Include stopped containers.
        #[arg(short, long)]
        all: bool,
    },
    /// List images.
    Images,
    /// Show a container's logs.
    Logs {
        container: String,
        /// Number of lines from the end.
        #[arg(short = 'n', long)]
        tail: Option<usize>,
    },
    /// Start a stopped container.
    Start { container: String },
    /// Stop a running container.
    Stop { container: String },
    /// Restart a container.
    Restart { container: String },
    /// Pull an image from its registry.
    Pull { reference: String },
    /// Run a command in a new container (each container gets its own VM).
    Run {
        /// Name the container (default: image name plus a short suffix).
        #[arg(long)]
        name: Option<String>,
        /// Set an environment variable (KEY=value); repeatable.
        #[arg(short, long = "env")]
        env: Vec<String>,
        /// vCPUs for the container's VM.
        #[arg(long)]
        cpus: Option<u32>,
        /// Memory for the container's VM, in MiB.
        #[arg(long)]
        memory: Option<u64>,
        /// Publish a port on 127.0.0.1: HOST:CONTAINER (or one port for both); repeatable.
        #[arg(short, long = "publish")]
        publish: Vec<String>,
        /// Mount a folder or named volume: /host/dir:/path[:ro] or name:/path; repeatable.
        #[arg(short, long = "volume")]
        volume: Vec<String>,
        /// Start it and return instead of streaming its output.
        #[arg(short, long)]
        detach: bool,
        image: String,
        /// Command (replaces the image's default command).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Build an image from a Dockerfile (in its own VM; no Docker needed).
    Build {
        /// Name the image (NAME or NAME:TAG); default: the folder's name.
        #[arg(short, long)]
        tag: Option<String>,
        /// The Dockerfile, relative to the folder (default: Dockerfile).
        #[arg(short, long)]
        file: Option<String>,
        /// Set an ARG (KEY=value); repeatable.
        #[arg(long = "build-arg")]
        build_arg: Vec<String>,
        /// Build up to this stage.
        #[arg(long)]
        target: Option<String>,
        /// Rebuild every step.
        #[arg(long)]
        no_cache: bool,
        /// The build context.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Remove a stopped container.
    Rm { container: String },
    /// Run a command in a running container.
    Exec {
        /// Keep stdin open (use with -t for a shell).
        #[arg(short, long)]
        interactive: bool,
        /// Run in a terminal.
        #[arg(short, long)]
        tty: bool,
        /// Set an environment variable (KEY=value); repeatable.
        #[arg(short, long = "env")]
        env: Vec<String>,
        container: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Show a running container's CPU, memory and network use.
    Stats { container: String },
    /// List the processes in a running container.
    Top { container: String },
    /// List a folder inside a running container.
    Ls {
        container: String,
        #[arg(default_value = "/")]
        path: String,
    },
    /// Print a file from inside a running container (up to 256 KiB).
    Cat { container: String, path: String },
    /// Show how a container is configured, as JSON.
    Inspect { container: String },
    /// Pair a phone with this Mac (gravitond must run with --remote).
    Pair,
    /// Devices paired with this Mac.
    Devices {
        #[command(subcommand)]
        action: Option<DeviceAction>,
    },
    /// Other Macs this one can run containers on.
    Hosts {
        #[command(subcommand)]
        action: Option<HostsAction>,
    },
    /// Run this Mac's gravitond (the one inside Graviton.app) at login.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Parse and normalise an image reference (no daemon needed).
    Resolve { reference: String },
}

#[derive(Subcommand, Debug)]
enum DeviceAction {
    /// Forget a device; it has to pair again.
    Revoke { id: String },
}

#[derive(Subcommand, Debug)]
enum HostsAction {
    /// Pair with a Mac running `gravitond --remote`: give its address (or the
    /// graviton://pair link) and the code `grav pair` shows on it.
    Add {
        /// ADDRESS[:PORT], e.g. mini.local, or the graviton://pair?... link.
        target: String,
        /// The code from `grav pair` (asked for if not given).
        code: Option<String>,
        /// What to call it in --host (default: from its name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Forget a host (on that Mac, `grav devices revoke` stops its token too).
    Rm { name: String },
    /// Send commands to this host when no --host is given (`local` for this Mac).
    Use { name: String },
}

#[derive(Subcommand, Debug)]
enum ServiceAction {
    /// Start gravitond now and at every login.
    Install {
        /// Also accept paired Macs and phones (TLS on port 7843).
        #[arg(long)]
        remote: bool,
    },
    /// Stop it and don't start it at login.
    Uninstall,
    /// Whether it's installed and running.
    Status,
}

fn main() {
    if let Err(err) = run(Cli::parse()) {
        eprintln!("grav: {err:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    if let Command::Resolve { reference } = &cli.command {
        let r = ImageReference::parse(reference)?;
        println!("{r}");
        return Ok(());
    }

    match &cli.command {
        Command::Hosts { action } => return hosts(action.as_ref()),
        Command::Service { action: ServiceAction::Install { remote } } => return service::install(*remote),
        Command::Service { action: ServiceAction::Uninstall } => return service::uninstall(),
        Command::Service { action: ServiceAction::Status } => return service::status(),
        _ => {}
    }

    let socket = cli.socket.clone().unwrap_or_else(paths::socket_path);
    let target = Target::resolve(cli.host.as_deref(), socket)?;
    if target.is_remote() && matches!(cli.command, Command::Pair | Command::Devices { .. }) {
        bail!("pairing is managed on the Mac itself: run `grav pair` there");
    }
    let mut client = Client::connect(&target)?;

    match cli.command {
        Command::Ping => match client.call(&Request::Ping)? {
            Response::Pong { protocol } if target.is_remote() => println!("gravitond on {} is up (protocol v{protocol})", target.describe()),
            Response::Pong { protocol } => println!("gravitond is up (protocol v{protocol})"),
            other => unexpected(other)?,
        },
        Command::Info => match client.call(&Request::EngineInfo)? {
            Response::Engine(e) => {
                println!("Version:   {}", e.version);
                println!("Kernel:    {}", e.kernel);
                println!("Arch:      {} · {} vCPU", e.arch, e.cpus);
                println!("Memory:    {} in VMs of {} on this Mac", bytes(e.memory_used_bytes), bytes(e.memory_bytes));
                if e.disk_bytes > 0 {
                    println!("Disk:      {} / {}", bytes(e.disk_used_bytes), bytes(e.disk_bytes));
                } else {
                    println!("Images:    {}", bytes(e.disk_used_bytes));
                }
            }
            other => unexpected(other)?,
        },
        Command::Ps { all } => match client.call(&Request::ListContainers)? {
            Response::Containers(list) => print_containers(list.iter().filter(|c| all || c.status == ContainerStatus::Running)),
            other => unexpected(other)?,
        },
        Command::Images => match client.call(&Request::ListImages)? {
            Response::Images(list) => {
                println!("{:<28} {:<16} {:>9}  {:<6}  USED BY", "REPOSITORY", "TAG", "SIZE", "ARCH");
                for i in list {
                    println!("{:<28} {:<16} {:>9}  {:<6}  {}", i.repository, i.tag, bytes(i.size_bytes), i.architecture, i.used_by);
                }
            }
            other => unexpected(other)?,
        },
        Command::Logs { container, tail } => match client.call(&Request::Logs { id: container, tail })? {
            Response::Logs(lines) => {
                for l in lines {
                    println!("{} {:<5} {}", clock(l.timestamp_ms), format!("{:?}", l.level).to_uppercase(), l.message);
                }
            }
            other => unexpected(other)?,
        },
        Command::Start { container } => lifecycle(&mut client, Request::Start { id: container }, "started")?,
        Command::Stop { container } => lifecycle(&mut client, Request::Stop { id: container }, "stopped")?,
        Command::Restart { container } => lifecycle(&mut client, Request::Restart { id: container }, "restarted")?,
        Command::Pull { reference } => {
            eprintln!("Pulling {reference}…");
            match client.call(&Request::Pull { reference })? {
                Response::Image(i) => println!(
                    "Pulled {}:{} ({}, {}, {})",
                    i.repository,
                    i.tag,
                    bytes(i.size_bytes),
                    i.architecture,
                    &i.digest[..i.digest.len().min(19)]
                ),
                other => unexpected(other)?,
            }
        }
        Command::Run { name, env, cpus, memory, publish, volume, detach, image, command } => {
            let spec = RunSpec {
                image,
                name,
                command: (!command.is_empty()).then_some(command),
                env,
                cpus,
                memory_mib: memory,
                ports: publish,
                volumes: volume.iter().map(|v| absolute_volume(v)).collect::<Result<_>>()?,
            };
            let container = match client.call(&Request::Run(spec))? {
                Response::Container(c) => c,
                other => return unexpected(other),
            };
            if detach {
                println!("{}", container.id);
            } else {
                let code = follow(&mut client, &container.id)?;
                std::process::exit(code);
            }
        }
        Command::Exec { interactive, tty, env, container, command } => {
            let code = if tty {
                exec_tty(&target, ExecSpec { id: container, command, tty: true, rows: 0, cols: 0, env }, interactive)?
            } else {
                exec_stream(&mut client, ExecSpec { id: container, command, tty: false, rows: 0, cols: 0, env })?
            };
            std::process::exit(code);
        }
        Command::Stats { container } => match client.call(&Request::Stats { id: container })? {
            Response::Stats(st) => match st.samples.last() {
                None => println!("No samples yet (they start once the container runs, every {} s).", st.interval_secs),
                Some(s) => {
                    let first = &st.samples[0];
                    let secs = ((s.timestamp_ms - first.timestamp_ms) / 1000).max(1);
                    println!("CPU       {:.1}%  ({} vCPU; 100% is one busy CPU)", s.cpu_percent, st.cpus);
                    println!("Memory    {} / {}", bytes(s.memory_bytes), bytes(s.memory_limit_bytes));
                    println!("Network   ↓ {}  ↑ {}  in total", bytes(s.net_rx_bytes), bytes(s.net_tx_bytes));
                    if st.samples.len() > 1 {
                        let rx = (s.net_rx_bytes - first.net_rx_bytes) / secs;
                        let tx = (s.net_tx_bytes - first.net_tx_bytes) / secs;
                        println!("          ↓ {}/s  ↑ {}/s  over the last {}", bytes(rx), bytes(tx), uptime(secs));
                    }
                    println!("Processes {}", st.processes.len());
                }
            },
            other => unexpected(other)?,
        },
        Command::Top { container } => match client.call(&Request::Stats { id: container })? {
            Response::Stats(st) => {
                println!("{:>6} {:>6} {:>5} {:<2} {:>9} {:>9}  COMMAND", "PID", "PPID", "UID", "S", "RSS", "CPU TIME");
                for p in st.processes {
                    println!("{:>6} {:>6} {:>5} {:<2} {:>9} {:>9}  {}", p.pid, p.ppid, p.uid, p.state, bytes(p.rss_bytes), format!("{:.1}s", p.cpu_ms as f64 / 1000.0), p.command);
                }
            }
            other => unexpected(other)?,
        },
        Command::Ls { container, path } => match client.call(&Request::ListFiles { id: container, path })? {
            Response::Files(listing) => {
                for e in listing.entries {
                    let kind = match e.kind {
                        FileKind::Directory => 'd',
                        FileKind::Symlink => 'l',
                        FileKind::File => '-',
                        FileKind::Other => '?',
                    };
                    let link = e.link_target.map(|t| format!(" -> {t}")).unwrap_or_default();
                    println!("{kind}{:04o} {:>5}:{:<5} {:>9}  {}{link}", e.mode, e.uid, e.gid, bytes(e.size), e.name);
                }
            }
            other => unexpected(other)?,
        },
        Command::Cat { container, path } => match client.call(&Request::ReadFile { id: container, path })? {
            Response::File(f) => {
                std::io::stdout().write_all(&f.data)?;
                if f.truncated {
                    eprintln!("\ngrav: showed the first {} of {}", bytes(f.data.len() as u64), bytes(f.size));
                }
            }
            other => unexpected(other)?,
        },
        Command::Inspect { container } => match client.call(&Request::Inspect { id: container })? {
            Response::Details(d) => println!("{}", serde_json::to_string_pretty(&d)?),
            other => unexpected(other)?,
        },
        Command::Build { tag, file, build_arg, target: build_target, no_cache, path } => {
            let context = std::fs::canonicalize(&path).with_context(|| format!("{} doesn't exist", path.display()))?;
            let dockerfile = match file {
                // A path given on the command line is relative to where you are.
                Some(f) => Some(std::fs::canonicalize(&f).with_context(|| format!("{f} doesn't exist"))?.display().to_string()),
                None => None,
            };
            let mut spec = BuildSpec { context: context.display().to_string(), dockerfile, tag, build_args: build_arg, target: build_target, no_cache, upload: None };
            if let Target::Remote(host) = &target {
                // That Mac can't see this folder: send it along.
                let archive = pack_context(&context, &mut spec)?;
                let size = archive.path().metadata()?.len();
                eprintln!("Sending {} to {} ({:.1} MB)…", context.display(), host.name, size as f64 / 1048576.0);
                spec.upload = Some(size);
                client.writer.write_all(&encode_line(&Request::Build(spec)))?;
                std::io::copy(&mut std::fs::File::open(archive.path())?, &mut client.writer)?;
            } else {
                client.writer.write_all(&encode_line(&Request::Build(spec)))?;
            }
            let mut line = String::new();
            loop {
                line.clear();
                if client.reader.read_line(&mut line)? == 0 {
                    bail!("gravitond closed the connection");
                }
                match decode_line::<Response>(line.as_bytes())? {
                    Response::BuildLog { line } => println!("{line}"),
                    Response::Image(i) => {
                        println!("{}:{}  {}", i.repository, i.tag, &i.digest[..i.digest.len().min(19)]);
                        break;
                    }
                    Response::Error { message } => bail!(message),
                    other => return unexpected(other),
                }
            }
        }
        Command::Rm { container } => match client.call(&Request::Remove { id: container })? {
            Response::Removed { id } => println!("{id}"),
            other => unexpected(other)?,
        },
        Command::Pair => match client.call(&Request::PairBegin)? {
            Response::Pairing(p) => print_pairing(&p)?,
            other => unexpected(other)?,
        },
        Command::Devices { action: None } => match client.call(&Request::ListDevices)? {
            Response::Devices(list) if list.is_empty() => println!("No devices are paired. Run `grav pair` to add one."),
            Response::Devices(list) => {
                println!("{:<14} {:<28} {:<18} LAST SEEN", "ID", "NAME", "PAIRED");
                let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
                for d in list {
                    let seen = d.last_seen.map_or("never".into(), |t| format!("{} ago", uptime(now.saturating_sub(t))));
                    println!("{:<14} {:<28} {:<18} {seen}", d.id, d.name, format!("{} ago", uptime(now.saturating_sub(d.paired_at))));
                }
            }
            other => unexpected(other)?,
        },
        Command::Devices { action: Some(DeviceAction::Revoke { id }) } => match client.call(&Request::RevokeDevice { id })? {
            Response::Revoked { id } => println!("{id} revoked"),
            other => unexpected(other)?,
        },
        Command::Resolve { .. } | Command::Hosts { .. } | Command::Service { .. } => unreachable!(),
    }
    Ok(())
}

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(target: &Target) -> Result<Self> {
        let stream = target.connect()?;
        Ok(Self { reader: BufReader::new(stream.try_clone()?), writer: stream })
    }

    fn call(&mut self, request: &Request) -> Result<Response> {
        self.writer.write_all(&encode_line(request))?;
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            bail!("gravitond closed the connection");
        }
        match decode_line::<Response>(line.as_bytes())? {
            Response::Error { message } => bail!(message),
            response => Ok(response),
        }
    }
}

fn lifecycle(client: &mut Client, request: Request, verb: &str) -> Result<()> {
    match client.call(&request)? {
        Response::Container(c) => {
            println!("{} {verb}", c.name);
            Ok(())
        }
        other => unexpected(other),
    }
}

fn print_pairing(p: &graviton_proto::PairingInfo) -> Result<()> {
    let address = p.addresses.first().cloned().unwrap_or_else(|| "this-mac.local".into());
    let link = format!(
        "graviton://pair?host={address}&port={}&fp={}&code={}&name={}",
        p.port,
        p.fingerprint,
        p.code,
        p.host.replace(' ', "%20")
    );
    let qr = qrcode::QrCode::new(link.as_bytes())?;
    println!("{}", qr.render::<qrcode::render::unicode::Dense1x2>().quiet_zone(true).build());
    println!("Pair a phone with {}", p.host);
    println!();
    println!("  Code         {}-{}", &p.code[..4], &p.code[4..]);
    println!("  Address      {}  (port {})", if p.addresses.is_empty() { "unknown".into() } else { p.addresses.join(", ") }, p.port);
    println!("  Fingerprint  {}", p.fingerprint.as_bytes().chunks(4).take(4).map(|c| String::from_utf8_lossy(c).to_uppercase()).collect::<Vec<_>>().join(" "));
    println!();
    println!("In Graviton on the phone: Hosts, then pick this Mac (or add it by address) and enter the code.");
    println!("Check the fingerprint the phone shows matches. The code works once, for {} minutes.", p.expires_in_secs / 60);
    println!();
    println!("Link: {link}");
    Ok(())
}

/// Make a host path in a volume spec absolute (the daemon has its own working directory).
fn absolute_volume(spec: &str) -> Result<String> {
    let Some((source, rest)) = spec.split_once(':') else { return Ok(spec.to_string()) };
    if source.starts_with('.') {
        let abs = std::fs::canonicalize(source).with_context(|| format!("{source} doesn't exist"))?;
        return Ok(format!("{}:{rest}", abs.display()));
    }
    Ok(spec.to_string())
}

/// `grav exec` without a terminal: print output as it arrives; returns the exit code.
fn exec_stream(client: &mut Client, spec: ExecSpec) -> Result<i32> {
    client.writer.write_all(&encode_line(&Request::Exec(spec)))?;
    let mut line = String::new();
    loop {
        line.clear();
        if client.reader.read_line(&mut line)? == 0 {
            bail!("gravitond closed the connection");
        }
        match decode_line::<Response>(line.as_bytes())? {
            Response::ExecOutput { stream: LogStream::Stderr, data } => std::io::stderr().write_all(&data)?,
            Response::ExecOutput { data, .. } => {
                let mut out = std::io::stdout().lock();
                out.write_all(&data)?;
                out.flush()?;
            }
            Response::ExecExited { code } => return Ok(code),
            Response::Error { message } => bail!(message),
            other => return unexpected(other).map(|_| 1),
        }
    }
}

/// Pack a build folder for another Mac: everything `.dockerignore` keeps,
/// plus the Dockerfile and `.dockerignore` themselves. Points `spec` at the
/// Dockerfile inside the archive and names the image after the folder.
fn pack_context(context: &std::path::Path, spec: &mut BuildSpec) -> Result<TempFile> {
    use graviton_proto::build::{Ignore, walk};
    let ignore_lines = std::fs::read_to_string(context.join(".dockerignore")).map(|t| Ignore::lines(&t)).unwrap_or_default();
    let ignore = Ignore::new(&ignore_lines);
    let archive = TempFile::new("grav-context", "tar.gz")?;
    let gz = flate2::write::GzEncoder::new(std::fs::File::create(archive.path())?, flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    tar.follow_symlinks(false);
    let mut forced: Vec<PathBuf> = vec![".dockerignore".into()];
    match spec.dockerfile.take() {
        None => forced.push("Dockerfile".into()),
        Some(file) => match std::path::Path::new(&file).strip_prefix(context) {
            Ok(rel) => {
                forced.push(rel.to_path_buf());
                spec.dockerfile = Some(rel.display().to_string());
            }
            // Outside the folder: it travels under a fixed name.
            Err(_) => tar.append_path_with_name(&file, graviton_proto::UPLOADED_DOCKERFILE)?,
        },
    }
    for rel in walk(context, std::path::Path::new(""), &ignore)? {
        if !rel.as_os_str().is_empty() {
            tar.append_path_with_name(context.join(&rel), &rel)?;
        }
    }
    for rel in forced {
        if ignore.is_ignored(&rel) && context.join(&rel).is_file() {
            tar.append_path_with_name(context.join(&rel), &rel)?;
        }
    }
    tar.into_inner()?.finish()?;
    if spec.tag.is_none() {
        let name: String = context
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '-' })
            .collect();
        let name = name.trim_matches(['-', '.', '_']).to_string();
        spec.tag = Some(if name.is_empty() { "graviton-build".into() } else { name });
    }
    Ok(archive)
}

/// A file in the temporary folder, removed when dropped.
struct TempFile(PathBuf);

impl TempFile {
    fn new(stem: &str, ext: &str) -> Result<Self> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        Ok(Self(std::env::temp_dir().join(format!("{stem}-{}-{nanos}.{ext}", std::process::id()))))
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn hosts(action: Option<&HostsAction>) -> Result<()> {
    let mut hosts = remote::Hosts::load()?;
    match action {
        None => {
            if hosts.hosts.is_empty() {
                println!("No other Macs yet. On the Mac mini: `grav service install --remote`, then `grav pair`;");
                println!("here: `grav hosts add ADDRESS CODE`.");
                return Ok(());
            }
            let default = hosts.default.clone();
            println!("{:<3}{:<18} {:<26} {:<20} MAC", "", "NAME", "ADDRESS", "FINGERPRINT");
            println!("{:<3}{:<18} {:<26} {:<20}", if default.is_none() { "*" } else { "" }, "local", "this Mac", "");
            for h in &hosts.hosts {
                let star = if default.as_deref().is_some_and(|d| d.eq_ignore_ascii_case(&h.name)) { "*" } else { "" };
                println!("{star:<3}{:<18} {:<26} {:<20} {}", h.name, format!("{}:{}", h.address, h.port), remote::fingerprint_short(&h.fingerprint), h.host_name);
            }
            println!("\n* = where commands go without --host (change with `grav hosts use NAME`)");
        }
        Some(HostsAction::Add { target, code, name }) => {
            let t = remote::PairTarget::parse(target)?;
            let code = match code.clone().or(t.code.clone()) {
                Some(c) => c,
                None => {
                    eprint!("Code from `grav pair` on that Mac: ");
                    std::io::stderr().flush()?;
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    line.trim().to_string()
                }
            };
            let host = remote::pair(&t.address, t.port, &code, t.fingerprint.as_deref(), name.as_deref())?;
            if t.fingerprint.is_none() {
                // Paired by address: check it's really that Mac.
                eprintln!("{} says its fingerprint is {}.", host.host_name, remote::fingerprint_short(&host.fingerprint));
                if !remote::confirm("Does that match what `grav pair` showed on it?") {
                    bail!("not saved. On that Mac, `grav devices revoke {}` removes the pairing", host.device_id);
                }
            }
            hosts.hosts.retain(|h| !h.name.eq_ignore_ascii_case(&host.name) && h.fingerprint != host.fingerprint);
            println!("Paired with {} as `{}` ({}:{}, {}).", host.host_name, host.name, host.address, host.port, remote::fingerprint_short(&host.fingerprint));
            println!("Use it with `grav --host {} ps`, or make it the default: `grav hosts use {}`.", host.name, host.name);
            hosts.hosts.push(host);
            hosts.save()?;
        }
        Some(HostsAction::Rm { name }) => {
            let before = hosts.hosts.len();
            hosts.hosts.retain(|h| !h.name.eq_ignore_ascii_case(name));
            if hosts.hosts.len() == before {
                bail!("no host called {name}");
            }
            if hosts.default.as_deref().is_some_and(|d| d.eq_ignore_ascii_case(name)) {
                hosts.default = None;
            }
            hosts.save()?;
            println!("Forgot {name}.");
        }
        Some(HostsAction::Use { name }) => {
            if name == "local" {
                hosts.default = None;
                println!("Commands go to this Mac unless you pass --host.");
            } else {
                let host = hosts.get(name).ok_or_else(|| anyhow::anyhow!("no host called {name}: see `grav hosts`"))?;
                println!("Commands go to {} unless you pass --host (`--host local` for this Mac).", host.name);
                hosts.default = Some(host.name.clone());
            }
            hosts.save()?;
        }
    }
    Ok(())
}

/// `grav exec -t`: put this terminal in raw mode and connect it to the
/// container's; returns the program's exit code.
fn exec_tty(target: &Target, mut spec: ExecSpec, interactive: bool) -> Result<i32> {
    let is_tty = |fd| unsafe { libc::isatty(fd) } == 1;
    if let Some((rows, cols)) = window_size() {
        (spec.rows, spec.cols) = (rows, cols);
    }
    let mut stream = target.connect()?;
    stream.write_all(&encode_line(&Request::Exec(spec)))?;
    // Read the answer byte by byte: what follows it is terminal frames.
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while stream.read(&mut byte)? == 1 && byte[0] != b'\n' {
        line.push(byte[0]);
    }
    match decode_line::<Response>(&line)? {
        Response::Attached => {}
        Response::Error { message } => bail!(message),
        other => return unexpected(other).map(|_| 1),
    }

    let raw = if interactive && is_tty(libc::STDIN_FILENO) { RawMode::enable() } else { None };
    let to_guest = Arc::new(Mutex::new(stream.try_clone()?));
    let send = |w: &Arc<Mutex<UnixStream>>, f: Frame| w.lock().map(|mut s| s.write_all(&f.encode()).is_ok()).unwrap_or(false);
    if interactive {
        let w = Arc::clone(&to_guest);
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 16 * 1024];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if !send(&w, Frame::Data(buf[..n].to_vec())) {
                            return;
                        }
                    }
                }
            }
            send(&w, Frame::Eof);
        });
    }
    if is_tty(libc::STDOUT_FILENO) {
        // Follow window resizes (polling keeps this free of signal handlers).
        let w = Arc::clone(&to_guest);
        std::thread::spawn(move || {
            let mut last = window_size();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(250));
                let now = window_size();
                if now != last {
                    last = now;
                    if let Some((rows, cols)) = now {
                        if !send(&w, Frame::Resize { rows, cols }) {
                            return;
                        }
                    }
                }
            }
        });
    }

    let mut out = std::io::stdout();
    let mut decoder = Decoder::default();
    let mut buf = [0u8; 16 * 1024];
    let mut code = None;
    'read: loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => decoder.push(&buf[..n]),
        }
        loop {
            match decoder.next_frame() {
                Ok(Some(Frame::Data(bytes))) => {
                    out.write_all(&bytes)?;
                    out.flush()?;
                }
                Ok(Some(Frame::Exit(c))) => {
                    code = Some(c);
                    break 'read;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(err) => {
                    drop(raw);
                    bail!(err);
                }
            }
        }
    }
    drop(raw);
    code.ok_or_else(|| anyhow::anyhow!("the terminal session ended without an exit code (did the container stop?)"))
}

fn window_size() -> Option<(u16, u16)> {
    // SAFETY: TIOCGWINSZ fills a winsize we own.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    (unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0 && size.ws_row > 0).then_some((size.ws_row, size.ws_col))
}

/// Raw terminal mode for the life of the value.
struct RawMode(libc::termios);

impl RawMode {
    fn enable() -> Option<Self> {
        // SAFETY: tcgetattr/tcsetattr on stdin with a termios we own.
        unsafe {
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut original) != 0 {
                return None;
            }
            let mut raw = original;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
            Some(Self(original))
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restores the attributes saved in `enable`.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
    }
}

/// Stream a container's output until it exits; returns its exit code.
fn follow(client: &mut Client, id: &str) -> Result<i32> {
    let mut seen = 0u64;
    let mut print_new = |client: &mut Client| -> Result<()> {
        if let Response::Logs(lines) = client.call(&Request::Logs { id: id.to_string(), tail: None })? {
            let mut out = std::io::stdout().lock();
            let from = seen;
            for l in lines.into_iter().filter(|l| l.seq > from) {
                seen = l.seq;
                match l.stream {
                    LogStream::Stdout => writeln!(out, "{}", l.message)?,
                    LogStream::Stderr => eprintln!("{}", l.message),
                    LogStream::System => eprintln!("\x1b[2mgraviton: {}\x1b[0m", l.message),
                }
            }
            out.flush()?;
        }
        Ok(())
    };
    loop {
        print_new(client)?;
        let status = match client.call(&Request::ListContainers)? {
            Response::Containers(list) => list.into_iter().find(|c| c.id == id),
            other => return unexpected(other).map(|_| 1),
        };
        match status {
            Some(c) if c.status == ContainerStatus::Exited => {
                print_new(client)?;
                return Ok(c.exit_code.unwrap_or(0));
            }
            Some(_) => std::thread::sleep(std::time::Duration::from_millis(80)),
            None => bail!("container {id} disappeared"),
        }
    }
}

fn unexpected(response: Response) -> Result<()> {
    bail!("unexpected response from gravitond: {response:?}")
}

fn print_containers<'a>(list: impl Iterator<Item = &'a ContainerSummary>) {
    println!("{:<13} {:<14} {:<26} {:<14} {:>6} {:>9}  PORTS", "ID", "NAME", "IMAGE", "STATUS", "CPU", "MEM");
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    for c in list {
        let status = match (c.status, c.started_at, c.exit_code) {
            (ContainerStatus::Running, Some(t), _) => format!("Up {}", uptime(now.saturating_sub(t))),
            (ContainerStatus::Exited, _, Some(code)) => format!("Exited ({code})"),
            (s, _, _) => format!("{s:?}"),
        };
        let ports = c.ports.iter().map(|p| format!("{}->{}", p.host_port, p.container_port)).collect::<Vec<_>>().join(", ");
        println!(
            "{:<13} {:<14} {:<26} {:<14} {:>6} {:>9}  {}",
            c.id,
            c.name,
            c.image,
            status,
            c.cpu_percent.map_or("-".into(), |v| format!("{v:.1}%")),
            c.memory_bytes.map_or("-".into(), bytes),
            ports
        );
    }
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{n} B") } else if v >= 100.0 { format!("{v:.0} {}", UNITS[unit]) } else { format!("{v:.1} {}", UNITS[unit]) }
}

fn uptime(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

/// HH:MM:SS.mmm in UTC (good enough until we pull in a time crate).
fn clock(ms: u64) -> String {
    let secs = ms / 1000;
    format!("{:02}:{:02}:{:02}.{:03}", (secs / 3600) % 24, (secs / 60) % 60, secs % 60, ms % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes_and_durations() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(412 * 1024 * 1024), "412 MB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024 + 100 * 1024 * 1024), "3.1 GB");
        assert_eq!(uptime(45), "45s");
        assert_eq!(uptime(11_520), "3h 12m");
        assert_eq!(clock(3_723_004), "01:02:03.004");
    }
}
