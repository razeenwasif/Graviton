//! `grav`: command-line client for gravitond.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use graviton_core::{ImageReference, paths};
use graviton_proto::{ContainerStatus, ContainerSummary, Request, Response, decode_line, encode_line};

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
                println!("Memory:    {} / {}", bytes(e.memory_used_bytes), bytes(e.memory_bytes));
                println!("Disk:      {} / {}", bytes(e.disk_used_bytes), bytes(e.disk_bytes));
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
