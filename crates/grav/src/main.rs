//! `grav`: command-line client for gravitond.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use graviton_core::{ImageReference, paths};
use graviton_proto::{ContainerStatus, ContainerSummary, ExecSpec, LogStream, Request, Response, RunSpec, decode_line, encode_line};

#[derive(Parser, Debug)]
#[command(version, about = "Native containers for macOS")]
struct Cli {
    /// Control socket path (default: $GRAVITON_SOCKET or ~/Library/Application Support/Graviton/gravitond.sock)
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

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
    /// Pair a phone with this Mac (gravitond must run with --remote).
    Pair,
    /// Devices paired with this Mac.
    Devices {
        #[command(subcommand)]
        action: Option<DeviceAction>,
    },
    /// Parse and normalise an image reference (no daemon needed).
    Resolve { reference: String },
}

#[derive(Subcommand, Debug)]
enum DeviceAction {
    /// Forget a device; it has to pair again.
    Revoke { id: String },
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

    let socket = cli.socket.unwrap_or_else(paths::socket_path);
    let mut client = Client::connect(&socket)?;

    match cli.command {
        Command::Ping => match client.call(&Request::Ping)? {
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
                exec_tty(&socket, ExecSpec { id: container, command, tty: true, rows: 0, cols: 0, env }, interactive)?
            } else {
                exec_stream(&mut client, ExecSpec { id: container, command, tty: false, rows: 0, cols: 0, env })?
            };
            std::process::exit(code);
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
        Command::Resolve { .. } => unreachable!(),
    }
    Ok(())
}

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(path: &std::path::Path) -> Result<Self> {
        let stream = UnixStream::connect(path).with_context(|| {
            format!("cannot reach gravitond at {} (is it running? try `gravitond`)", path.display())
        })?;
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

/// `grav exec -t`: put this terminal in raw mode and splice it with the container's.
fn exec_tty(socket: &std::path::Path, mut spec: ExecSpec, interactive: bool) -> Result<i32> {
    let is_tty = |fd| unsafe { libc::isatty(fd) } == 1;
    // SAFETY: TIOCGWINSZ fills a winsize; a failure leaves the zeroed default.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0 {
        spec.rows = size.ws_row;
        spec.cols = size.ws_col;
    }
    let mut stream = UnixStream::connect(socket).with_context(|| format!("cannot reach gravitond at {}", socket.display()))?;
    stream.write_all(&encode_line(&Request::Exec(spec)))?;
    // Read the answer byte by byte: what follows it is terminal output.
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
    if interactive {
        let mut to_guest = stream.try_clone()?;
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut std::io::stdin().lock(), &mut to_guest);
            let _ = to_guest.shutdown(std::net::Shutdown::Write);
        });
    }
    let mut out = std::io::stdout();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                out.write_all(&buf[..n])?;
                out.flush()?;
            }
        }
    }
    drop(raw);
    Ok(0)
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
