//! `grav`: command-line client for gravitond.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use graviton_core::{ImageReference, paths};
use graviton_proto::{ContainerStatus, ContainerSummary, LogStream, Request, Response, RunSpec, decode_line, encode_line};

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
    /// Parse and normalise an image reference (no daemon needed).
    Resolve { reference: String },
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
        Command::Run { name, env, cpus, memory, detach, image, command } => {
            let spec = RunSpec {
                image,
                name,
                command: (!command.is_empty()).then_some(command),
                env,
                cpus,
                memory_mib: memory,
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
        Command::Rm { container } => match client.call(&Request::Remove { id: container })? {
            Response::Removed { id } => println!("{id}"),
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
