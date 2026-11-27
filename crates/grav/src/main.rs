//! `grav`: command-line client for gravitond, on this Mac or a paired one.

mod compose;
mod remote;
mod service;
mod update;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, IsTerminal, Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};
use graviton_core::{ImageReference, paths};
use std::sync::{Arc, Mutex};

use graviton_proto::frame::{Decoder, Frame};
use remote::Target;
use graviton_proto::{
    BuildSecret, BuildSpec, CommitSpec, ContainerStatus, ContainerSummary, ExecSpec, FileKind, ImagePushProgress, LogLine, LogStream, PullProgress,
    Request, Response, RestartPolicy, RunSpec, decode_line, encode_line,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TailArg {
    All,
    Lines(usize),
}

impl TailArg {
    pub(crate) fn into_option(self) -> Option<usize> {
        match self {
            Self::All => None,
            Self::Lines(n) => Some(n),
        }
    }
}

impl FromStr for TailArg {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("all") {
            Ok(Self::All)
        } else {
            s.parse::<usize>()
                .map(Self::Lines)
                .map_err(|_| format!("invalid --tail value `{s}` (expected non-negative integer or `all`)"))
        }
    }
}

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
#[allow(clippy::large_enum_variant)]
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
        /// Number of lines from the end (or `all`).
        #[arg(short = 'n', long)]
        tail: Option<TailArg>,
        /// Keep streaming new log output until the container exits.
        #[arg(short = 'f', long)]
        follow: bool,
        /// Show RFC 3339 timestamps.
        #[arg(short = 't', long)]
        timestamps: bool,
        /// Show logs since timestamp (RFC 3339, Unix seconds, or e.g. `10m`, `1h30m`).
        #[arg(long)]
        since: Option<String>,
        /// Show logs before a timestamp or relative duration (e.g. `5m`).
        #[arg(long)]
        until: Option<String>,
    },
    /// Start a stopped container.
    Start { container: String },
    /// Stop a running container.
    Stop { container: String },
    /// Pause a running container's VM.
    Pause { container: String },
    /// Unpause a paused container's VM.
    Unpause { container: String },
    /// Fork a container's disk (and flushed live state if running) into one or more new containers.
    Fork {
        /// Source container ID, prefix, or name.
        container: String,
        /// Optional name for the fork (when creating one fork).
        name: Option<String>,
        /// Number of forks to create.
        #[arg(long, default_value_t = 1)]
        count: u32,
        /// Create the fork(s) stopped instead of booting immediately.
        #[arg(long = "no-start")]
        no_start: bool,
        /// Automatically remove forked containers when they stop.
        #[arg(long)]
        rm: bool,
        /// Publish a port (`[HOST_IP:]HOST:CONTAINER[/tcp|udp]`); repeatable (single fork only).
        #[arg(short, long, value_name = "[HOST_IP:]HOST:CONTAINER[/PROTO]")]
        publish: Vec<String>,
        /// Override/set an environment variable (`KEY=value`); repeatable.
        #[arg(short, long)]
        env: Vec<String>,
        /// Override command inside the fork(s).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Inspect overlay filesystem changes inside a running container.
    Diff { container: String },
    /// Restart a container.
    Restart { container: String },
    /// Pull an image from its registry.
    Pull {
        /// Target platform (e.g. linux/amd64, linux/arm64).
        #[arg(long)]
        platform: Option<String>,
        reference: String,
    },
    /// Give a local image an additional name or tag (e.g. `grav tag app ghcr.io/me/app:1.0`).
    Tag {
        /// Existing local image reference or digest prefix.
        source: String,
        /// New repository[:tag] reference.
        target: String,
    },
    /// Save one or more images to a tar archive (stdout or `-o FILE`).
    Save {
        /// Write to a file instead of stdout.
        #[arg(short = 'o', long)]
        output: Option<PathBuf>,
        /// Image reference(s) or digest prefix(es) to export.
        #[arg(required = true)]
        images: Vec<String>,
    },
    /// Load image(s) from a tar or tar.gz archive (stdin or `-i FILE`).
    Load {
        /// Read from a tar archive instead of stdin.
        #[arg(short = 'i', long)]
        input: Option<PathBuf>,
    },
    /// Create a new image from a container's changes.
    Commit {
        /// Author (e.g. "Jane Doe <jane@example.com>").
        #[arg(short = 'a', long)]
        author: Option<String>,
        /// Commit message.
        #[arg(short = 'm', long)]
        message: Option<String>,
        /// Apply Dockerfile instruction (ENV, CMD, ENTRYPOINT, WORKDIR, USER, EXPOSE, LABEL, VOLUME, STOPSIGNAL, HEALTHCHECK).
        #[arg(short = 'c', long = "change")]
        changes: Vec<String>,
        /// Container id, prefix, or name.
        container: String,
        /// Target image reference (`NAME[:TAG]`).
        image: String,
    },
    /// Log in to an OCI registry (Docker Hub, GHCR, etc.); credentials stay in the Mac's Keychain.
    Login {
        /// Registry hostname (default: docker.io).
        registry: Option<String>,
        /// Username (prompted if omitted).
        #[arg(short, long)]
        username: Option<String>,
        /// Read the password or token from stdin (for scripts and CI).
        #[arg(long)]
        password_stdin: bool,
        /// List registries you are currently logged in to.
        #[arg(long)]
        list: bool,
    },
    /// Log out of an OCI registry and remove its saved credentials.
    Logout {
        /// Registry hostname (default: docker.io).
        registry: Option<String>,
    },
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
        /// Publish a port: [IP:]HOST:CONTAINER[/tcp|udp] or PORT (default IP: 127.0.0.1); repeatable.
        #[arg(short, long = "publish")]
        publish: Vec<String>,
        /// Mount a folder or named volume: /host/dir:/path[:ro] or name:/path; repeatable.
        #[arg(short, long = "volume")]
        volume: Vec<String>,
        /// Restart policy: no, on-failure, or always.
        #[arg(long, value_name = "POLICY")]
        restart: Option<RestartPolicy>,
        /// Start it and return instead of streaming its output.
        #[arg(short, long)]
        detach: bool,
        /// Keep stdin open (use with -t for an interactive shell).
        #[arg(short, long)]
        interactive: bool,
        /// Allocate a pseudo-terminal and attach; the container stops when you exit.
        #[arg(short, long)]
        tty: bool,
        /// Automatically remove the container when it exits.
        #[arg(long)]
        rm: bool,
        /// Target platform when pulling the image (e.g. linux/amd64, linux/arm64).
        #[arg(long)]
        platform: Option<String>,
        /// Override the image's entrypoint.
        #[arg(long)]
        entrypoint: Option<String>,
        /// Working directory inside the container.
        #[arg(short = 'w', long = "workdir")]
        workdir: Option<String>,
        /// User (`uid[:gid]` or `name[:group]`).
        #[arg(short = 'u', long = "user")]
        user: Option<String>,
        /// Container hostname.
        #[arg(long)]
        hostname: Option<String>,
        /// Set container label (`KEY=value`); repeatable.
        #[arg(short = 'l', long = "label")]
        label: Vec<String>,
        /// Command to run to check health (shell form).
        #[arg(long = "health-cmd", value_name = "CMD")]
        health_cmd: Option<String>,
        /// Time between running the check (e.g. `5s`, `1m`).
        #[arg(long = "health-interval", value_name = "DURATION")]
        health_interval: Option<String>,
        /// Maximum time to allow one check to run (`5s`, `30s`).
        #[arg(long = "health-timeout", value_name = "DURATION")]
        health_timeout: Option<String>,
        /// Start period before failures count toward retries (`10s`).
        #[arg(long = "health-start-period", value_name = "DURATION")]
        health_start_period: Option<String>,
        /// Consecutive failures needed to report unhealthy.
        #[arg(long = "health-retries", value_name = "N")]
        health_retries: Option<u32>,
        /// Disable any container-specified `HEALTHCHECK`.
        #[arg(long = "no-healthcheck")]
        no_healthcheck: bool,
        /// Mount the container's root filesystem as read-only.
        #[arg(long = "read-only")]
        read_only: bool,
        /// Add Linux capabilities (`CAP_NET_BIND_SERVICE`, `ALL`, etc.); repeatable.
        #[arg(long = "cap-add", value_name = "CAP")]
        cap_add: Vec<String>,
        /// Drop Linux capabilities (`ALL`, `NET_RAW`, etc.); repeatable.
        #[arg(long = "cap-drop", value_name = "CAP")]
        cap_drop: Vec<String>,
        /// Mount a temporary in-memory filesystem (`/path` or `/path:opts`); repeatable.
        #[arg(long = "tmpfs", value_name = "PATH[:OPTS]")]
        tmpfs: Vec<String>,
        /// Run `docker-init` as container PID 1 for signal forwarding and zombie reaping.
        #[arg(long)]
        init: bool,
        /// Connect the container to one or more named networks (defaults to `graviton`).
        #[arg(long = "network", visible_alias = "net", value_name = "NETWORK")]
        network: Vec<String>,
        /// Size of `/dev/shm` (`64m`, `128mb`, `1g`, etc.).
        #[arg(long = "shm-size", value_name = "BYTES")]
        shm_size: Option<String>,
        /// Set POSIX resource limits (`NAME=SOFT[:HARD]`, e.g. `nofile=1024:4096`); repeatable.
        #[arg(long = "ulimit", value_name = "NAME=SOFT[:HARD]")]
        ulimit: Vec<String>,
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
        /// Expose a secret to `RUN --mount=type=secret`: id=ID,src=FILE or id=ID,env=VAR; repeatable.
        #[arg(long = "secret")]
        secret: Vec<String>,
        /// Build up to this stage.
        #[arg(long)]
        target: Option<String>,
        /// Rebuild every step.
        #[arg(long)]
        no_cache: bool,
        /// Target platform(s), comma-separated (e.g. linux/amd64, linux/arm64,linux/amd64).
        #[arg(long)]
        platform: Option<String>,
        /// The build context.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Remove a stopped container.
    Rm { container: String },
    /// Where the disk space goes: images, container disks, the build cache.
    Df,
    /// Remove an image no container uses.
    Rmi { image: String },
    /// Free space: the build cache and layers nothing refers to, and more if asked.
    Prune {
        /// Also remove images no container uses.
        #[arg(long)]
        images: bool,
        /// Also remove stopped containers, with their files.
        #[arg(long)]
        containers: bool,
        /// Images and stopped containers too (--images --containers).
        #[arg(short, long)]
        all: bool,
        /// Show what would go, without removing anything.
        #[arg(long)]
        dry_run: bool,
    },
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
        /// Working directory inside the container.
        #[arg(short = 'w', long = "workdir")]
        workdir: Option<String>,
        /// User (`uid[:gid]` or `name[:group]`).
        #[arg(short = 'u', long = "user")]
        user: Option<String>,
        container: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Show a live stream of container resource usage statistics.
    Stats {
        /// Container names or IDs (default: all running containers).
        containers: Vec<String>,
        /// Include stopped containers.
        #[arg(short, long)]
        all: bool,
        /// Disable streaming stats and only print the initial snapshot.
        #[arg(long = "no-stream")]
        no_stream: bool,
        /// Output format: `table` (default) or `json` (NDJSON lines).
        #[arg(long, default_value = "table", value_parser = ["table", "json"])]
        format: String,
    },
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
    /// Copy files or folders between this Mac and a running container
    /// (`CONTAINER:PATH LOCAL` or `LOCAL CONTAINER:PATH`).
    Cp {
        /// Source (`CONTAINER:PATH` or local path).
        src: String,
        /// Destination (local path, `-` for tar on stdout, or `CONTAINER:PATH`).
        dst: String,
    },
    /// Show how a container is configured, as JSON.
    Inspect { container: String },
    /// Named volumes on this Mac (`ls`, `create`, `rm`).
    #[command(visible_alias = "volume")]
    Volumes {
        #[command(subcommand)]
        action: Option<VolumeAction>,
    },
    /// Container networks (`ls`, `create`, `rm`, `connect`, `disconnect`).
    #[command(visible_alias = "networks")]
    Network {
        #[command(subcommand)]
        action: Option<NetworkAction>,
    },
    /// Manage cached VM memory snapshots for fast container starts (`status`, `warm`, `clear`).
    #[command(visible_alias = "snapshots")]
    Snapshot {
        #[command(subcommand)]
        action: Option<SnapshotAction>,
    },
    /// Multi-container stacks from `compose.yaml` / `docker-compose.yml` (`up`, `down`, `ps`, `logs`, `ls`).
    Compose(compose::ComposeArgs),
    /// What happened recently: containers started, stopped and failed, pulls and builds.
    Events {
        /// How many (default 30).
        #[arg(short = 'n', long, default_value_t = 30)]
        tail: usize,
        /// Keep watching for new ones.
        #[arg(short, long)]
        follow: bool,
    },
    /// Push a local image to its registry, or manage phone push alerts (`setup`, `status`, `test`).
    #[command(args_conflicts_with_subcommands = true)]
    Push {
        #[command(subcommand)]
        action: Option<PushAction>,
        /// Image reference to push (`NAME[:TAG]` or `REGISTRY/REPO[:TAG]`).
        image: Option<String>,
    },
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
    /// Print shell commands to point Docker CLI, Compose, and Dev Containers at Graviton.
    DockerEnv {
        /// Print commands to unset DOCKER_HOST instead.
        #[arg(long)]
        unset: bool,
    },
    /// Generate shell completion scripts for `grav`.
    Completions {
        /// Target shell (`zsh`, `bash`, or `fish`).
        #[arg(value_enum)]
        shell: CompletionShell,
    },
    /// Check GitHub Releases for a newer version of Graviton (no daemon needed).
    Update {
        /// Exit 1 when an update is available and 2 if the check cannot complete.
        #[arg(long)]
        check: bool,
        /// Output machine-readable JSON instead of human-friendly text.
        #[arg(long)]
        json: bool,
    },
    /// Parse and normalise an image reference (no daemon needed).
    Resolve { reference: String },
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionShell {
    Zsh,
    Bash,
    Fish,
}

impl CompletionShell {
    fn to_clap_shell(self) -> Shell {
        match self {
            Self::Zsh => Shell::Zsh,
            Self::Bash => Shell::Bash,
            Self::Fish => Shell::Fish,
        }
    }
}

fn write_completions<W: Write>(shell: CompletionShell, out: &mut W) -> std::io::Result<()> {
    let mut cmd = Cli::command();
    let mut buf = Vec::new();
    generate(shell.to_clap_shell(), &mut cmd, "grav", &mut buf);
    match out.write_all(&buf) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(err) => Err(err),
    }
}

#[derive(Subcommand, Debug)]
enum VolumeAction {
    /// List named volumes.
    Ls,
    /// Create an empty named volume.
    Create { name: String },
    /// Remove a named volume no container uses.
    Rm { name: String },
}

#[derive(Subcommand, Debug)]
enum NetworkAction {
    /// Show switch/DNS status, configured networks, and connected endpoints.
    Ls,
    /// Create a user-defined isolated container network.
    Create {
        name: String,
        /// Restrict external/gateway routing semantics (`--internal`).
        #[arg(long)]
        internal: bool,
    },
    /// Remove an unused user-defined network.
    Rm { name: String },
    /// Connect a container to a network (live without restarting running containers).
    Connect { network: String, container: String },
    /// Disconnect a container from a network (live without restarting running containers).
    Disconnect { network: String, container: String },
}

#[derive(Subcommand, Debug)]
enum SnapshotAction {
    /// Show cached VM snapshots and disk usage.
    Status,
    /// Pre-warm one or more VM snapshots for fast container restore.
    Warm {
        /// vCPUs for the template VM (default: 2).
        #[arg(long)]
        cpus: Option<u32>,
        /// Memory for the template VM in MiB (default: 1024).
        #[arg(long)]
        memory: Option<u64>,
        /// Warm a Rosetta-enabled template VM for linux/amd64 images.
        #[arg(long)]
        rosetta: bool,
        /// Number of slots to keep ready for concurrent containers (1–8, default: 1).
        #[arg(long)]
        slots: Option<u32>,
    },
    /// Delete idle cached VM snapshots from disk.
    Clear,
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
        /// Extra fallback address(es) to try if the primary address isn't reachable.
        #[arg(long = "fallback")]
        fallback: Vec<String>,
    },
    /// Update saved address(es), port, or Bonjour service name for a paired host.
    Update {
        /// Paired host name (see `grav hosts`).
        name: String,
        /// New address(es), highest priority first (optionally `HOST:PORT`).
        addresses: Vec<String>,
        /// Override TLS port.
        #[arg(long)]
        port: Option<u16>,
        /// Append address(es) to existing fallbacks instead of replacing them.
        #[arg(long)]
        add: bool,
        /// Remove a stale address from the host's address list.
        #[arg(long)]
        remove: Vec<String>,
        /// Set or update the Bonjour `_graviton._tcp` service name.
        #[arg(long)]
        service: Option<String>,
        /// Immediately connect and authenticate over TLS to verify reachability.
        #[arg(long)]
        verify: bool,
    },
    /// Forget a host (on that Mac, `grav devices revoke` stops its token too).
    Rm { name: String },
    /// Send commands to this host when no --host is given (`local` for this Mac).
    Use { name: String },
}

#[derive(Subcommand, Debug)]
enum PushAction {
    /// Give gravitond the APNs key (.p8); it's kept in this Mac's login Keychain.
    Setup {
        /// The key's ten-character id (from its file name: AuthKey_<ID>.p8).
        #[arg(long)]
        key_id: String,
        /// Your team id.
        #[arg(long)]
        team_id: String,
        /// The .p8 file.
        file: PathBuf,
    },
    /// Whether alerts are set up, and how many phones get them.
    Status,
    /// Send a test alert to every phone that registered.
    Test,
}

#[derive(Subcommand, Debug)]
enum ServiceAction {
    /// Start gravitond now and at every login.
    Install {
        /// Also accept paired Macs and phones (TLS on port 7843).
        #[arg(long)]
        remote: bool,
        /// IP address for --remote to bind to (e.g. a Tailscale IP or 127.0.0.1).
        #[arg(long)]
        remote_bind: Option<std::net::IpAddr>,
        /// Don't advertise the --remote endpoint over Bonjour (_graviton._tcp).
        #[arg(long)]
        no_bonjour: bool,
        /// Override the host name shown to paired devices.
        #[arg(long)]
        remote_name: Option<String>,
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
    if let Command::Compose(compose::ComposeArgs { file, project_name, env_file, action: compose::ComposeAction::Config { json }, .. }) = &cli.command {
        let cwd = std::env::current_dir()?;
        let proj = graviton_core::compose::ComposeProject::load(
            &cwd,
            &graviton_core::compose::LoadOptions {
                files: file.clone(),
                project_name: project_name.clone(),
                env_file: env_file.clone(),
            },
        )?;
        if *json {
            println!("{}", serde_json::to_string_pretty(&proj)?);
        } else {
            println!("Project: {}", proj.name);
            println!("Files:   {}", proj.files.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "));
            if !proj.volumes.is_empty() {
                let vols: Vec<String> = proj
                    .volumes
                    .iter()
                    .map(|(k, v)| if v.external { format!("{k} -> {} (external)", v.volume_name) } else { format!("{k} -> {}", v.volume_name) })
                    .collect();
                println!("Volumes: {}", vols.join(", "));
            }
            println!();
            println!("{:<18} {:<24} {:<28} DEPENDS ON", "SERVICE", "CONTAINER", "IMAGE");
            for svc in proj.services.values() {
                let deps = if svc.depends_on.is_empty() { "—".into() } else { svc.depends_on.keys().cloned().collect::<Vec<_>>().join(", ") };
                let img = if svc.build.is_some() { format!("{} (build)", svc.image) } else { svc.image.clone() };
                println!("{:<18} {:<24} {:<28} {deps}", svc.name, svc.container_name, img);
            }
        }
        return Ok(());
    }

    match &cli.command {
        Command::Hosts { action } => return hosts(action.as_ref()),
        Command::Service {
            action: ServiceAction::Install { remote, remote_bind, no_bonjour, remote_name },
        } => return service::install(*remote, *remote_bind, *no_bonjour, remote_name.as_deref()),
        Command::Service { action: ServiceAction::Uninstall } => return service::uninstall(),
        Command::Service { action: ServiceAction::Status } => return service::status(),
        Command::DockerEnv { unset } => {
            if *unset {
                println!("unset DOCKER_HOST DOCKER_BUILDKIT");
            } else {
                let sock = paths::docker_socket_path();
                let quoted = sock.display().to_string().replace('\'', "'\\''");
                println!("export DOCKER_HOST='unix://{quoted}'");
                println!("export DOCKER_BUILDKIT=0");
                println!("# Run this command to configure your shell:");
                println!("# eval \"$(grav docker-env)\"");
            }
            return Ok(());
        }
        Command::Completions { shell } => {
            let mut stdout = std::io::stdout().lock();
            write_completions(*shell, &mut stdout)?;
            return Ok(());
        }
        Command::Update { check, json } => {
            let code = update::run_update(*check, *json);
            if code != 0 {
                std::process::exit(code);
            }
            return Ok(());
        }
        _ => {}
    }

    let socket = cli.socket.clone().unwrap_or_else(paths::socket_path);
    let target = Target::resolve(cli.host.as_deref(), socket)?;
    if target.is_remote() && matches!(cli.command, Command::Pair | Command::Devices { .. } | Command::Push { action: Some(_), .. }) {
        bail!("pairing and push alerts are managed on the Mac itself: run this there (or with --host local)");
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
                println!(
                    "Rosetta:   {}",
                    if e.rosetta { "available (linux/amd64)" } else { "not installed" }
                );
                if let Some(host_used) = e.host_memory_used_bytes {
                    let total_used = host_used.max(e.memory_used_bytes).min(e.memory_bytes);
                    println!(
                        "Memory:    {} / {} ({} in VMs)",
                        bytes(total_used),
                        bytes(e.memory_bytes),
                        bytes(e.memory_used_bytes),
                    );
                } else {
                    println!("Memory:    {} in VMs of {} on this Mac", bytes(e.memory_used_bytes), bytes(e.memory_bytes));
                }
                if e.disk_bytes > 0 {
                    println!("Disk:      {} / {}", bytes(e.disk_used_bytes), bytes(e.disk_bytes));
                } else {
                    println!("Images:    {}", bytes(e.disk_used_bytes));
                }
                if !target.is_remote() {
                    let docker_sock = paths::docker_socket_path();
                    if docker_sock.exists() {
                        println!("Docker:    unix://{}", docker_sock.display());
                    }
                }
            }
            other => unexpected(other)?,
        },
        Command::Ps { all } => match client.call(&Request::ListContainers)? {
            Response::Containers(list) => print_containers(list.iter().filter(|c| all || matches!(c.status, ContainerStatus::Running | ContainerStatus::Paused))),
            other => unexpected(other)?,
        },
        Command::Images => match client.call(&Request::ListImages)? {
            Response::Images(list) => {
                println!("{:<28} {:<16} {:>9}  {:<11}  USED BY", "REPOSITORY", "TAG", "SIZE", "ARCH");
                for i in list {
                    println!("{:<28} {:<16} {:>9}  {:<11}  {}", i.repository, i.tag, bytes(i.size_bytes), i.architecture, i.used_by);
                }
            }
            other => unexpected(other)?,
        },
        Command::Logs { container, tail, follow, timestamps, since, until } => {
            let since_ms = since.as_deref().map(parse_time_arg).transpose()?;
            let until_ms = until.as_deref().map(parse_time_arg).transpose()?;
            stream_logs(
                &mut client,
                &container,
                tail.and_then(TailArg::into_option),
                follow,
                timestamps,
                since_ms,
                until_ms,
            )?;
        }
        Command::Start { container } => lifecycle(&mut client, Request::Start { id: container }, "started")?,
        Command::Stop { container } => lifecycle(&mut client, Request::Stop { id: container }, "stopped")?,
        Command::Pause { container } => lifecycle(&mut client, Request::Pause { id: container }, "paused")?,
        Command::Unpause { container } => lifecycle(&mut client, Request::Unpause { id: container }, "unpaused")?,
        Command::Fork { container, name, count, no_start, rm, publish, env, command } => {
            let ports = publish
                .into_iter()
                .map(|p| graviton_proto::PortMapping::parse(&p).map_err(anyhow::Error::msg))
                .collect::<Result<Vec<_>>>()?;
            let spec = graviton_proto::ForkSpec {
                source: container,
                names: name.into_iter().collect(),
                count,
                start: !no_start,
                auto_remove: rm,
                ports,
                env,
                command: (!command.is_empty()).then_some(command),
            };
            match client.call(&Request::Fork(Box::new(spec)))? {
                Response::Forked { containers } => {
                    for c in containers {
                        let short = &c.id[..c.id.len().min(12)];
                        println!("{} ({short}) forked", c.name);
                    }
                }
                other => unexpected(other)?,
            }
        }
        Command::Diff { container } => match client.call(&Request::Diff { id: container })? {
            Response::Diff { entries } => {
                for e in entries {
                    let marker = match e.kind {
                        graviton_proto::DiffKind::Added => 'A',
                        graviton_proto::DiffKind::Modified => 'C',
                        graviton_proto::DiffKind::Deleted => 'D',
                    };
                    println!("{marker} {}", e.path);
                }
            }
            other => unexpected(other)?,
        },
        Command::Restart { container } => lifecycle(&mut client, Request::Restart { id: container }, "restarted")?,
        Command::Pull { platform, reference } => pull_image(&mut client, &reference, platform.as_deref())?,
        Command::Tag { source, target } => match client.call(&Request::TagImage { source: source.clone(), target: target.clone() })? {
            Response::Image(i) => {
                let disp = ImageReference::parse(&i.reference).map(|r| r.display_name()).unwrap_or(i.reference);
                let short = &i.digest[..i.digest.len().min(19)];
                println!("Tagged {source} -> {disp} ({short})");
            }
            other => unexpected(other)?,
        },
        Command::Save { output, images } => save_images(&target, output, images)?,
        Command::Load { input } => load_images(&mut client, input)?,
        Command::Commit { author, message, changes, container, image } => {
            let spec = CommitSpec { container: container.clone(), reference: image, author, message, changes };
            match client.call(&Request::Commit(spec))? {
                Response::Image(i) => {
                    let disp = ImageReference::parse(&i.reference).map(|r| r.display_name()).unwrap_or(i.reference);
                    let short = &i.digest[..i.digest.len().min(19)];
                    println!("Committed {container} -> {disp} ({short}, {})", bytes(i.size_bytes));
                }
                other => unexpected(other)?,
            }
        }
        Command::Login { registry, username, password_stdin, list } => {
            if list {
                match client.call(&Request::ListLogins)? {
                    Response::Logins(entries) if entries.is_empty() => println!("Not logged in to any registries."),
                    Response::Logins(entries) => {
                        println!("{:<28} USERNAME", "REGISTRY");
                        for entry in entries {
                            println!("{:<28} {}", entry.registry, entry.username);
                        }
                    }
                    other => unexpected(other)?,
                }
            } else {
                let registry = registry.unwrap_or_else(|| "docker.io".into());
                let username = match username {
                    Some(u) => u,
                    None if password_stdin => bail!("pass --username when using --password-stdin"),
                    None => prompt_line(&format!("Username ({registry}): "))?,
                };
                let password = if password_stdin {
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf)?;
                    buf.trim_end_matches(['\r', '\n']).to_string()
                } else {
                    prompt_password("Password (or token): ")?
                };
                match client.call(&Request::RegistryLogin { registry, username, password })? {
                    Response::LoggedIn { registry, username } => println!("Logged in to {registry} as {username}."),
                    other => unexpected(other)?,
                }
            }
        }
        Command::Logout { registry } => {
            let registry = registry.unwrap_or_else(|| "docker.io".into());
            match client.call(&Request::RegistryLogout { registry })? {
                Response::LoggedOut { registry } => println!("Logged out of {registry}."),
                other => unexpected(other)?,
            }
        }
        Command::Run {
            name,
            env,
            cpus,
            memory,
            publish,
            volume,
            restart,
            detach,
            interactive,
            tty,
            rm,
            platform,
            entrypoint,
            workdir,
            user,
            hostname,
            label,
            health_cmd,
            health_interval,
            health_timeout,
            health_start_period,
            health_retries,
            no_healthcheck,
            read_only,
            cap_add,
            cap_drop,
            tmpfs,
            init,
            network,
            shm_size,
            ulimit,
            image,
            command,
        } => {
            if detach && (tty || interactive) {
                bail!("can't combine -d/--detach with -i or -t");
            }
            if detach && rm {
                bail!("can't combine -d/--detach with --rm");
            }
            let has_health_override = health_cmd.is_some()
                || health_interval.is_some()
                || health_timeout.is_some()
                || health_start_period.is_some()
                || health_retries.is_some();
            if no_healthcheck && has_health_override {
                bail!("can't combine --no-healthcheck with --health-* flags");
            }
            let healthcheck = if no_healthcheck {
                Some(graviton_proto::Healthcheck::disabled())
            } else if has_health_override {
                let interval_secs = match health_interval {
                    Some(s) => {
                        let v = graviton_proto::parse_duration_secs(&s)
                            .map_err(|e| anyhow::anyhow!("--health-interval: {e}"))?;
                        if v == 0 {
                            bail!("--health-interval must be greater than 0s");
                        }
                        v
                    }
                    None => 0,
                };
                let timeout_secs = match health_timeout {
                    Some(s) => {
                        let v = graviton_proto::parse_duration_secs(&s)
                            .map_err(|e| anyhow::anyhow!("--health-timeout: {e}"))?;
                        if v == 0 {
                            bail!("--health-timeout must be greater than 0s");
                        }
                        v
                    }
                    None => 0,
                };
                let start_period_secs = match health_start_period {
                    Some(s) => graviton_proto::parse_duration_secs(&s)
                        .map_err(|e| anyhow::anyhow!("--health-start-period: {e}"))?,
                    None => 0,
                };
                let retries = match health_retries {
                    Some(0) => bail!("--health-retries must be at least 1"),
                    Some(n) => n,
                    None => 0,
                };
                let test = match health_cmd {
                    Some(cmd) => {
                        if cmd.trim().is_empty() {
                            bail!("--health-cmd cannot be empty");
                        }
                        vec!["CMD-SHELL".into(), cmd]
                    }
                    None => Vec::new(),
                };
                Some(graviton_proto::Healthcheck {
                    test,
                    interval_secs,
                    timeout_secs,
                    start_period_secs,
                    retries,
                })
            } else {
                None
            };
            let mut labels = BTreeMap::new();
            for pair in label {
                let (k, v) = pair.split_once('=').unwrap_or((&pair, ""));
                labels.insert(k.to_string(), v.to_string());
            }
            let cap_add = cap_add
                .iter()
                .map(|c| graviton_proto::caps::normalize_cap(c).map_err(|e| anyhow::anyhow!(e)))
                .collect::<Result<Vec<_>>>()?;
            let cap_drop = cap_drop
                .iter()
                .map(|c| graviton_proto::caps::normalize_cap(c).map_err(|e| anyhow::anyhow!(e)))
                .collect::<Result<Vec<_>>>()?;
            let tmpfs = tmpfs
                .iter()
                .map(|s| graviton_proto::parse_tmpfs_spec(s).map_err(|e| anyhow::anyhow!(e)))
                .collect::<Result<Vec<_>>>()?;
            let shm_size_bytes = shm_size
                .as_deref()
                .map(|s| graviton_proto::parse_byte_size(s).map_err(|e| anyhow::anyhow!(e)))
                .transpose()?;
            let ulimits = ulimit
                .iter()
                .map(|s| graviton_proto::parse_ulimit_spec(s).map_err(|e| anyhow::anyhow!(e)))
                .collect::<Result<Vec<_>>>()?;
            let spec = RunSpec {
                image,
                name,
                command: (!command.is_empty()).then_some(command),
                env,
                cpus,
                memory_mib: memory,
                ports: publish,
                volumes: volume.iter().map(|v| absolute_volume(v)).collect::<Result<_>>()?,
                restart: restart.filter(|p| *p != RestartPolicy::No),
                tty,
                rows: 0,
                cols: 0,
                auto_remove: rm,
                platform,
                entrypoint: entrypoint.map(|e| graviton_core::compose::split_shell_words(&e)),
                working_dir: workdir,
                user,
                hostname,
                project: None,
                service: None,
                labels,
                dns_aliases: vec![],
                healthcheck,
                read_only,
                cap_add,
                cap_drop,
                tmpfs,
                init,
                networks: network,
                shm_size_bytes,
                ulimits,
            };
            if tty {
                let code = run_tty(&target, spec, interactive)?;
                std::process::exit(code);
            }
            let container = match client.call(&Request::Run(Box::new(spec)))? {
                Response::Container(c) => c,
                other => return unexpected(other),
            };
            if detach {
                println!("{}", container.id);
            } else {
                let code = follow(&mut client, &container.id)?;
                if rm {
                    let _ = client.call(&Request::Remove { id: container.id });
                }
                std::process::exit(code);
            }
        }
        Command::Exec { interactive, tty, env, workdir, user, container, command } => {
            let spec = ExecSpec {
                id: container,
                command,
                tty,
                rows: 0,
                cols: 0,
                env,
                working_dir: workdir,
                user,
            };
            let code = if tty {
                exec_tty(&target, spec, interactive)?
            } else {
                exec_stream(&mut client, spec)?
            };
            std::process::exit(code);
        }
        Command::Stats { containers, all, no_stream, format } => {
            cmd_stats(&mut client, containers, all, no_stream, &format)?;
        }
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
        Command::Cp { src, dst } => copy_files(&mut client, &target, &src, &dst)?,
        Command::Inspect { container } => match client.call(&Request::Inspect { id: container })? {
            Response::Details(d) => println!("{}", serde_json::to_string_pretty(&d)?),
            other => unexpected(other)?,
        },
        Command::Volumes { action } => match action.unwrap_or(VolumeAction::Ls) {
            VolumeAction::Ls => match client.call(&Request::ListVolumes)? {
                Response::Volumes(vols) => {
                    println!("{:<24} {:>10} {:>8}  USED BY", "NAME", "SIZE", "FILES");
                    for v in vols {
                        let used = if v.used_by.is_empty() {
                            "—".into()
                        } else {
                            v.used_by
                                .iter()
                                .map(|u| format!("{}:{}", u.container_name, u.target))
                                .collect::<Vec<_>>()
                                .join(", ")
                        };
                        println!("{:<24} {:>10} {:>8}  {used}", v.name, bytes(v.size_bytes), v.file_count);
                    }
                }
                other => unexpected(other)?,
            },
            VolumeAction::Create { name } => match client.call(&Request::CreateVolume { name })? {
                Response::Volume(v) => println!("{}", v.name),
                other => unexpected(other)?,
            },
            VolumeAction::Rm { name } => match client.call(&Request::RemoveVolume { name })? {
                Response::VolumeRemoved { name, freed_bytes } => {
                    println!("Removed volume {name} ({} freed).", bytes(freed_bytes));
                }
                other => unexpected(other)?,
            },
        },
        Command::Network { action } => match action.unwrap_or(NetworkAction::Ls) {
            NetworkAction::Ls => match client.call(&Request::NetworkInfo)? {
                Response::Network(net) => {
                    println!("Network   {} ({})", net.name, net.driver);
                    println!("Subnet    {}  gateway {}", net.subnet, net.gateway);
                    println!("DNS       {}  (*.{})", net.dns_server, net.dns_domain);
                    println!(
                        "Switch    {} connected  {} MACs  {} frames ({})",
                        net.connected_ports,
                        net.learned_macs,
                        net.frames_forwarded,
                        bytes(net.bytes_forwarded)
                    );
                    if !net.networks.is_empty() {
                        println!();
                        println!("{:<18} {:<16} {:<10} CONTAINERS", "NETWORK", "DRIVER", "SCOPE");
                        for n in &net.networks {
                            let scope = if n.builtin { "built-in" } else if n.internal { "internal" } else { "local" };
                            let members = if n.containers.is_empty() { "—".into() } else { n.containers.join(", ") };
                            println!("{:<18} {:<16} {:<10} {members}", n.name, n.driver, scope);
                        }
                    }
                    if !net.endpoints.is_empty() {
                        println!();
                        println!("{:<18} {:<10} {:<16} {:<20} {:<18} PORTS", "CONTAINER", "STATUS", "IP", "DNS", "NETWORKS");
                        for ep in net.endpoints {
                            let st = match ep.status {
                                ContainerStatus::Running => "running",
                                ContainerStatus::Exited => "exited",
                                ContainerStatus::Created => "created",
                                ContainerStatus::Paused => "paused",
                            };
                            let ip = ep.ip_address.as_deref().unwrap_or("—");
                            let dns = ep.dns_names.get(1).or_else(|| ep.dns_names.first()).map(String::as_str).unwrap_or("—");
                            let nets = if ep.networks.is_empty() { "graviton".into() } else { ep.networks.join(", ") };
                            let ports = ep
                                .ports
                                .iter()
                                .map(|p| {
                                    let proto = match p.protocol {
                                        graviton_proto::PortProtocol::Tcp => "tcp",
                                        graviton_proto::PortProtocol::Udp => "udp",
                                    };
                                    match &p.host_ip {
                                        Some(host_ip) => format!("{host_ip}:{}->{}/{proto}", p.host_port, p.container_port),
                                        None => format!("{}->{}/{proto}", p.host_port, p.container_port),
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            println!("{:<18} {:<10} {:<16} {:<20} {:<18} {ports}", ep.container_name, st, ip, dns, nets);
                        }
                    }
                }
                other => unexpected(other)?,
            },
            NetworkAction::Create { name, internal } => {
                match client.call(&Request::CreateNetwork { name, internal, labels: BTreeMap::new() })? {
                    Response::NetworkCreated(net) => println!("{}", net.name),
                    other => unexpected(other)?,
                }
            }
            NetworkAction::Rm { name } => match client.call(&Request::RemoveNetwork { name })? {
                Response::NetworkRemoved { name } => println!("Removed network {name}."),
                other => unexpected(other)?,
            },
            NetworkAction::Connect { network, container } => {
                match client.call(&Request::ConnectNetwork { network, container })? {
                    Response::NetworkUpdated { network, container, .. } => {
                        println!("Connected {container} to {network}.");
                    }
                    other => unexpected(other)?,
                }
            }
            NetworkAction::Disconnect { network, container } => {
                match client.call(&Request::DisconnectNetwork { network, container })? {
                    Response::NetworkUpdated { network, container, .. } => {
                        println!("Disconnected {container} from {network}.");
                    }
                    other => unexpected(other)?,
                }
            }
        },
        Command::Snapshot { action } => {
            let req = match action.unwrap_or(SnapshotAction::Status) {
                SnapshotAction::Status => Request::SnapshotStatus,
                SnapshotAction::Warm { cpus, memory, rosetta, slots } => {
                    eprintln!("Warming VM snapshot…");
                    Request::SnapshotWarm { cpus, memory_mib: memory, rosetta, slots }
                }
                SnapshotAction::Clear => Request::SnapshotClear,
            };
            match client.call(&req)? {
                Response::Snapshots(info) => {
                    let state = if !info.enabled {
                        "disabled".to_string()
                    } else if let Some(reason) = &info.paused_reason {
                        format!("paused — {reason}")
                    } else {
                        "enabled".to_string()
                    };
                    println!(
                        "Snapshots: {state} ({} slot{}, {} in use, {} on disk; max {}/shape)",
                        info.total_slots,
                        if info.total_slots == 1 { "" } else { "s" },
                        info.in_use,
                        bytes(info.total_bytes),
                        info.max_slots_per_shape
                    );
                    if !info.groups.is_empty() {
                        println!();
                        println!("{:<10} {:<10} {:<10} {:>6} {:>8} {:>10}", "CPUS", "MEMORY", "MODE", "SLOTS", "IN USE", "SIZE");
                        for g in info.groups {
                            let mode = if g.rosetta { "rosetta" } else { "native" };
                            println!(
                                "{:<10} {:<10} {:<10} {:>6} {:>8} {:>10}",
                                format!("{} vCPU", g.cpus),
                                format!("{} MiB", g.memory_mib),
                                mode,
                                g.slots,
                                g.in_use,
                                bytes(g.size_bytes)
                            );
                        }
                    }
                }
                other => unexpected(other)?,
            }
        }
        Command::Compose(args) => compose::run(args, &mut client, &target)?,
        Command::Build { tag, file, build_arg, secret, target: build_target, no_cache, platform, path } => {
            let context = std::fs::canonicalize(&path).with_context(|| format!("{} doesn't exist", path.display()))?;
            let dockerfile = match file {
                // A path given on the command line is relative to where you are.
                Some(f) => Some(std::fs::canonicalize(&f).with_context(|| format!("{f} doesn't exist"))?.display().to_string()),
                None => None,
            };
            let secrets = secret.iter().map(|s| parse_build_secret(s)).collect::<Result<Vec<_>>>()?;
            let spec = BuildSpec {
                context: context.display().to_string(),
                dockerfile,
                tag,
                build_args: build_arg,
                target: build_target,
                no_cache,
                upload: None,
                secrets,
                platform,
                labels: BTreeMap::new(),
            };
            run_build_spec(&mut client, &target, &context, spec)?;
        }
        Command::Events { tail, follow } => {
            let mut last = None;
            loop {
                let limit = if last.is_none() { Some(tail) } else { None };
                match client.call(&Request::Events { since: last, limit })? {
                    Response::Events(list) => {
                        for e in &list {
                            let marker = match e.kind {
                                graviton_proto::EventKind::Failed
                                | graviton_proto::EventKind::Unhealthy
                                | graviton_proto::EventKind::BuildFailed => "✗",
                                graviton_proto::EventKind::Paused => "⏸",
                                graviton_proto::EventKind::Unpaused => "▶",
                                graviton_proto::EventKind::Started
                                | graviton_proto::EventKind::Healthy
                                | graviton_proto::EventKind::Built
                                | graviton_proto::EventKind::Pulled
                                | graviton_proto::EventKind::Pushed
                                | graviton_proto::EventKind::Forked => "✓",
                                _ => "·",
                            };
                            println!("{}  {marker} {}", clock(e.timestamp_ms), e.message);
                        }
                        if let Some(e) = list.last() {
                            last = Some(e.seq);
                        } else if last.is_none() {
                            last = Some(0);
                        }
                    }
                    other => return unexpected(other),
                }
                if !follow {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        Command::Df => match client.call(&Request::DiskUsage)? {
            Response::DiskUsage(u) => {
                println!("{:<18} {:>8} {:>11} {:>14}", "", "COUNT", "SIZE", "RECLAIMABLE");
                println!("{:<18} {:>8} {:>11} {:>14}", "Images", u.images, bytes(u.blobs_bytes), format!("{} ({} unused)", bytes(u.unused_images_bytes), u.unused_images));
                println!("{:<18} {:>8} {:>11} {:>14}", "Container disks", u.containers, bytes(u.container_disks_bytes), format!("{} ({} stopped)", bytes(u.stopped_disks_bytes), u.stopped_containers));
                println!("{:<18} {:>8} {:>11} {:>14}", "Build cache", u.build_cache_steps, bytes(u.build_cache_bytes), bytes(u.build_cache_bytes));
                println!("{:<18} {:>8} {:>11} {:>14}", "VM snapshots", u.snapshots, bytes(u.snapshots_bytes), bytes(u.snapshots_bytes));
                println!("{:<18} {:>8} {:>11} {:>14}", "Unreferenced", "", bytes(u.unreferenced_bytes), bytes(u.unreferenced_bytes));
                println!();
                println!("grav prune frees the build cache, idle VM snapshots and unreferenced layers ({});", bytes(u.build_cache_bytes + u.snapshots_bytes + u.unreferenced_bytes));
                println!("--images and --containers add unused images and stopped containers.");
            }
            other => unexpected(other)?,
        },
        Command::Rmi { image } => match client.call(&Request::RemoveImage { reference: image })? {
            Response::ImageRemoved { reference, freed_bytes, kept_bytes } => {
                println!("Removed {} (freed {})", ImageReference::parse(&reference).map(|r| r.display_name()).unwrap_or(reference), bytes(freed_bytes));
                if kept_bytes > 0 {
                    println!("{} of its layers stay for the build cache; grav prune frees them.", bytes(kept_bytes));
                }
            }
            other => unexpected(other)?,
        },
        Command::Prune { images, containers, all, dry_run } => {
            let spec = graviton_proto::PruneSpec { images: images || all, containers: containers || all, dry_run };
            match client.call(&Request::Prune(spec))? {
                Response::Pruned(r) => {
                    let verb = if r.dry_run { "Would remove" } else { "Removed" };
                    if !r.containers.is_empty() {
                        println!("{verb} {} stopped container{}: {}", r.containers.len(), if r.containers.len() == 1 { "" } else { "s" }, r.containers.join(", "));
                    }
                    for i in &r.images {
                        println!("{verb} image {}", ImageReference::parse(i).map(|x| x.display_name()).unwrap_or(i.clone()));
                    }
                    println!(
                        "{verb} {} build-cache step{}, {} VM snapshot{} and {} unreferenced layer file{}",
                        r.build_cache_steps,
                        if r.build_cache_steps == 1 { "" } else { "s" },
                        r.snapshots,
                        if r.snapshots == 1 { "" } else { "s" },
                        r.blobs,
                        if r.blobs == 1 { "" } else { "s" }
                    );
                    println!("{} {}", if r.dry_run { "Would free" } else { "Freed" }, bytes(r.freed_bytes));
                }
                other => unexpected(other)?,
            }
        }
        Command::Rm { container } => match client.call(&Request::Remove { id: container })? {
            Response::Removed { id } => println!("{id}"),
            other => unexpected(other)?,
        },
        Command::Push { action, image } => match (action, image) {
            (Some(action), None) => {
                let request = match action {
                    PushAction::Setup { key_id, team_id, file } => {
                        let p8 = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
                        Request::PushSetup { key_id, team_id, p8 }
                    }
                    PushAction::Status => Request::PushStatus,
                    PushAction::Test => Request::PushTest,
                };
                match client.call(&request)? {
                    Response::Push(info) => {
                        match (&info.key_id, &info.team_id) {
                            (Some(k), Some(t)) => println!("Push alerts are on: key {k}, team {t}, kept in the login Keychain."),
                            _ => println!("Push alerts aren't set up: grav push setup --key-id ID --team-id TEAM AuthKey_ID.p8"),
                        }
                        println!("{} phone{} registered for alerts.", info.devices, if info.devices == 1 { "" } else { "s" });
                        if let Some(note) = info.note {
                            println!("{note}");
                        }
                    }
                    other => unexpected(other)?,
                }
            }
            (None, Some(reference)) => push_image(&mut client, &reference)?,
            (None, None) => bail!("pass an image reference (`grav push IMAGE`) or alert subcommand (`grav push setup|status|test`)"),
            (Some(_), Some(_)) => bail!("pass either an image reference or an alert subcommand, not both"),
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
        Command::Resolve { .. }
        | Command::DockerEnv { .. }
        | Command::Completions { .. }
        | Command::Update { .. }
        | Command::Hosts { .. }
        | Command::Service { .. } => unreachable!(),
    }
    Ok(())
}

pub(crate) struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    pub(crate) fn connect(target: &Target) -> Result<Self> {
        let stream = target.connect()?;
        Ok(Self { reader: BufReader::new(stream.try_clone()?), writer: stream })
    }

    pub(crate) fn send(&mut self, request: &Request) -> Result<()> {
        self.writer.write_all(&encode_line(request))?;
        self.writer.flush()?;
        Ok(())
    }

    pub(crate) fn recv(&mut self) -> Result<Option<Response>> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        match decode_line::<Response>(line.as_bytes())? {
            Response::Error { message } => bail!(message),
            response => Ok(Some(response)),
        }
    }

    pub(crate) fn call(&mut self, request: &Request) -> Result<Response> {
        self.send(request)?;
        self.recv()?.ok_or_else(|| anyhow::anyhow!("gravitond closed the connection"))
    }

    pub(crate) fn shutdown_handle(&self) -> Result<UnixStream> {
        Ok(self.writer.try_clone()?)
    }
}

pub(crate) fn run_build_spec(client: &mut Client, target: &Target, context: &std::path::Path, mut spec: BuildSpec) -> Result<()> {
    if let Target::Remote(host) = target {
        let archive = pack_context(context, &mut spec)?;
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
                return Ok(());
            }
            Response::Error { message } => bail!(message),
            other => return unexpected(other),
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
    let encoded_name = p.host.replace(' ', "%20");
    let extra = if p.addresses.len() > 1 {
        format!("&addrs={}", p.addresses.join(","))
    } else {
        String::new()
    };
    let link = format!(
        "graviton://pair?host={address}{extra}&service={encoded_name}&port={}&fp={}&code={}&name={encoded_name}",
        p.port,
        p.fingerprint,
        p.code,
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
            println!("{:<3}{:<18} {:<30} {:<20} MAC", "", "NAME", "ADDRESSES", "FINGERPRINT");
            println!("{:<3}{:<18} {:<30} {:<20}", if default.is_none() { "*" } else { "" }, "local", "this Mac", "");
            for h in &hosts.hosts {
                let star = if default.as_deref().is_some_and(|d| d.eq_ignore_ascii_case(&h.name)) { "*" } else { "" };
                let addrs = h.candidate_addresses();
                let addr_col = if addrs.is_empty() {
                    h.service_name
                        .as_deref()
                        .map(|s| format!("bonjour:{s}:{}", h.port))
                        .unwrap_or_else(|| format!(":{}", h.port))
                } else if addrs.len() == 1 {
                    format!("{}:{}", addrs[0], h.port)
                } else {
                    format!("{}:{} (+{} {})", addrs[0], h.port, addrs.len() - 1, addrs[1..].join(", "))
                };
                println!("{star:<3}{:<18} {:<30} {:<20} {}", h.name, addr_col, remote::fingerprint_short(&h.fingerprint), h.host_name);
            }
            println!("\n* = where commands go without --host (change with `grav hosts use NAME`)");
        }
        Some(HostsAction::Add { target, code, name, fallback }) => {
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
            let mut extra = t.addresses.clone();
            for item in fallback {
                let (addr, _) = remote::parse_host_port_opt(item)?;
                if !extra.contains(&addr) {
                    extra.push(addr);
                }
            }
            let host = remote::pair(&t.address, &extra, t.service_name.as_deref(), t.port, &code, t.fingerprint.as_deref(), name.as_deref())?;
            if t.fingerprint.is_none() {
                // Paired by address: check it's really that Mac.
                eprintln!("{} says its fingerprint is {}.", host.host_name, remote::fingerprint_short(&host.fingerprint));
                if !remote::confirm("Does that match what `grav pair` showed on it?") {
                    bail!("not saved. On that Mac, `grav devices revoke {}` removes the pairing", host.device_id);
                }
            }
            hosts.hosts.retain(|h| !h.name.eq_ignore_ascii_case(&host.name) && h.fingerprint != host.fingerprint);
            let saved_addrs = host.candidate_addresses();
            let extra_desc = if saved_addrs.len() > 1 {
                format!(" + fallbacks [{}]", saved_addrs[1..].join(", "))
            } else {
                String::new()
            };
            println!("Paired with {} as `{}` ({}:{}{}, {}).", host.host_name, host.name, host.address, host.port, extra_desc, remote::fingerprint_short(&host.fingerprint));
            println!("Use it with `grav --host {} ps`, or make it the default: `grav hosts use {}`.", host.name, host.name);
            hosts.hosts.push(host);
            hosts.save()?;
        }
        Some(HostsAction::Update { name, addresses, port, add, remove, service, verify }) => {
            let idx = hosts
                .hosts
                .iter()
                .position(|h| h.name.eq_ignore_ascii_case(name))
                .ok_or_else(|| anyhow::anyhow!("no host called {name}: see `grav hosts`"))?;
            let has_addr_args = !addresses.is_empty();
            if !has_addr_args && port.is_none() && remove.is_empty() && service.is_none() && !verify {
                bail!("nothing to update; pass new address(es), --port, --remove, --service, or --verify");
            }
            let mut h = hosts.hosts[idx].clone();
            let mut updated_port = *port;
            let mut list = if *add || !has_addr_args { h.candidate_addresses() } else { Vec::new() };

            for raw in addresses {
                let (addr, parsed_port) = remote::parse_host_port_opt(raw)?;
                if let Some(p) = parsed_port {
                    if updated_port.is_none() {
                        updated_port = Some(p);
                    }
                }
                if !list.contains(&addr) {
                    list.push(addr);
                }
            }
            if !remove.is_empty() {
                let mut to_remove = Vec::with_capacity(remove.len());
                for raw in remove {
                    let (addr, _) = remote::parse_host_port_opt(raw)?;
                    to_remove.push(addr);
                }
                list.retain(|a| !to_remove.iter().any(|r| r == a));
            }
            if let Some(p) = updated_port {
                h.port = p;
            }
            if let Some(svc) = service {
                let trimmed = svc.trim();
                h.service_name = (!trimmed.is_empty()).then(|| trimmed.to_string());
            }
            if list.is_empty() && h.service_name.is_none() {
                bail!("cannot remove the last address unless `--service` is set for Bonjour routing");
            }
            h.set_addresses(list);
            if *verify {
                remote::verify_host(&h)?;
            }
            let summary_addrs = h.candidate_addresses();
            let summary = if summary_addrs.is_empty() {
                format!("bonjour:{}", h.service_name.as_deref().unwrap_or("_graviton._tcp"))
            } else {
                summary_addrs.join(", ")
            };
            hosts.hosts[idx] = h;
            hosts.save()?;
            println!("Updated `{name}` -> [{summary}] (port {}{}).", hosts.hosts[idx].port, if *verify { ", verified TLS" } else { "" });
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

/// `grav run -it`: boot a container attached to this terminal; stops when you exit.
fn run_tty(target: &Target, mut spec: RunSpec, interactive: bool) -> Result<i32> {
    if let Some((rows, cols)) = window_size() {
        (spec.rows, spec.cols) = (rows, cols);
    }
    let mut stream = target.connect()?;
    stream.write_all(&encode_line(&Request::Run(Box::new(spec))))?;
    attach_tty(stream, interactive)
}

/// `grav exec -t`: put this terminal in raw mode and connect it to the
/// container's; returns the program's exit code.
fn exec_tty(target: &Target, mut spec: ExecSpec, interactive: bool) -> Result<i32> {
    if let Some((rows, cols)) = window_size() {
        (spec.rows, spec.cols) = (rows, cols);
    }
    let mut stream = target.connect()?;
    stream.write_all(&encode_line(&Request::Exec(spec)))?;
    attach_tty(stream, interactive)
}

fn attach_tty(mut stream: UnixStream, interactive: bool) -> Result<i32> {
    let is_tty = |fd| unsafe { libc::isatty(fd) } == 1;
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

#[derive(Debug, PartialEq, Eq)]
enum CpEndpoint<'a> {
    Local(&'a str),
    Container(&'a str, &'a str),
}

fn parse_cp_endpoint(arg: &str) -> CpEndpoint<'_> {
    if let Some((head, tail)) = arg.split_once(':') {
        // Local paths containing `:` start with `/`, `./`, `../`, or contain `/` before `:`.
        if !head.is_empty() && !head.contains('/') && !tail.is_empty() {
            return CpEndpoint::Container(head, tail);
        }
    }
    CpEndpoint::Local(arg)
}

fn copy_files(client: &mut Client, target: &Target, src: &str, dst: &str) -> Result<()> {
    match (parse_cp_endpoint(src), parse_cp_endpoint(dst)) {
        (CpEndpoint::Container(id, remote), CpEndpoint::Local(local)) => cp_from_container(target, id, remote, local),
        (CpEndpoint::Local(local), CpEndpoint::Container(id, remote)) => cp_to_container(client, local, id, remote),
        _ => bail!("expected `grav cp CONTAINER:PATH LOCAL` or `grav cp LOCAL CONTAINER:PATH`"),
    }
}

fn read_raw_response_line(stream: &mut dyn Read) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while stream.read(&mut byte)? == 1 && byte[0] != b'\n' {
        line.push(byte[0]);
    }
    Ok(line)
}

fn save_images(target: &Target, output: Option<PathBuf>, images: Vec<String>) -> Result<()> {
    if output.is_none() && std::io::stdout().is_terminal() {
        bail!("cowardly refusing to save to a terminal; pass -o FILE or redirect stdout");
    }
    let mut stream = target.connect()?;
    stream.write_all(&encode_line(&Request::SaveImage { references: images }))?;
    stream.flush()?;
    let line = read_raw_response_line(&mut stream)?;
    match decode_line::<Response>(&line)? {
        Response::ImageArchive => {}
        Response::Error { message } => bail!(message),
        other => unexpected(other)?,
    }
    if let Some(path) = output {
        let mut file = std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        std::io::copy(&mut stream, &mut file)?;
        file.flush()?;
    } else {
        let mut out = std::io::stdout().lock();
        std::io::copy(&mut stream, &mut out)?;
        out.flush()?;
    }
    Ok(())
}

fn load_images(client: &mut Client, input: Option<PathBuf>) -> Result<()> {
    struct TempFileGuard(PathBuf);
    impl Drop for TempFileGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    let _guard;
    let mut file = if let Some(path) = input {
        std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?
    } else {
        let tmp = std::env::temp_dir().join(format!("grav-load-{}.tar", std::process::id()));
        let mut f = std::fs::OpenOptions::new().create(true).truncate(true).read(true).write(true).open(&tmp)?;
        _guard = TempFileGuard(tmp);
        std::io::copy(&mut std::io::stdin().lock(), &mut f)?;
        f.flush()?;
        f.seek(SeekFrom::Start(0))?;
        f
    };

    let size = file.metadata()?.len();
    client.writer.write_all(&encode_line(&Request::LoadImage { size }))?;
    std::io::copy(&mut file, &mut client.writer)?;
    client.writer.flush()?;

    let mut line = String::new();
    if client.reader.read_line(&mut line)? == 0 {
        bail!("gravitond closed the connection");
    }
    match decode_line::<Response>(line.as_bytes())? {
        Response::Images(list) => {
            for img in list {
                let disp = ImageReference::parse(&img.reference).map(|r| r.display_name()).unwrap_or(img.reference);
                println!("Loaded image: {disp} ({}, {})", bytes(img.size_bytes), img.architecture);
            }
            Ok(())
        }
        Response::Error { message } => bail!(message),
        other => unexpected(other),
    }
}

fn cp_from_container(target: &Target, container: &str, remote_path: &str, local_dst: &str) -> Result<()> {
    let mut stream = target.connect()?;
    stream.write_all(&encode_line(&Request::GetArchive { id: container.into(), path: remote_path.into() }))?;
    let line = read_raw_response_line(&mut stream)?;
    let (is_dir, name) = match decode_line::<Response>(&line)? {
        Response::Archive { is_dir, name } => (is_dir, name),
        Response::Error { message } => bail!(message),
        other => return unexpected(other),
    };

    if local_dst == "-" {
        std::io::copy(&mut stream, &mut std::io::stdout())?;
        return Ok(());
    }

    let dst_path = std::path::Path::new(local_dst);
    let existing = std::fs::symlink_metadata(dst_path).ok();
    let src_dot = remote_path.ends_with("/.") || remote_path == "/.";

    let mut archive = tar::Archive::new(stream);
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);
    archive.set_overwrite(true);
    archive.set_unpack_xattrs(false);

    if !is_dir {
        let into_dir = existing.as_ref().is_some_and(|m| m.is_dir()) || local_dst.ends_with('/');
        if into_dir {
            std::fs::create_dir_all(dst_path)?;
            for entry in archive.entries()? {
                let mut entry = entry?;
                entry.unpack_in(dst_path)?;
            }
        } else {
            if let Some(parent) = dst_path.parent().filter(|p| !p.as_os_str().is_empty()) {
                if !parent.is_dir() {
                    bail!("destination folder {} doesn't exist", parent.display());
                }
            }
            if let Some(entry) = archive.entries()?.next() {
                let mut entry = entry?;
                if existing.is_some() {
                    let _ = std::fs::remove_file(dst_path);
                }
                entry.unpack(dst_path)?;
            }
        }
        return Ok(());
    }

    if existing.as_ref().is_some_and(|m| !m.is_dir()) {
        bail!("cannot copy folder {container}:{remote_path} onto existing file {}", dst_path.display());
    }

    let strip_prefix = existing.is_none() || src_dot;
    std::fs::create_dir_all(dst_path)?;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw_path = entry.path()?.into_owned();
        let rel = if strip_prefix {
            match raw_path.strip_prefix(&name) {
                Ok(stripped) if !stripped.as_os_str().is_empty() => stripped.to_path_buf(),
                Ok(_) => continue, // top-level dir itself
                Err(_) => raw_path,
            }
        } else {
            raw_path
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let Some(safe) = clean_local_rel(&rel) else { continue };
        let out_path = dst_path.join(&safe);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let kind = entry.header().entry_type();
        if let Ok(m) = std::fs::symlink_metadata(&out_path) {
            if m.is_dir() && !kind.is_dir() {
                std::fs::remove_dir_all(&out_path)?;
            } else if !m.is_dir() && (kind.is_dir() || kind.is_symlink() || kind.is_hard_link()) {
                std::fs::remove_file(&out_path)?;
            }
        }
        if kind.is_hard_link() && strip_prefix {
            if let Ok(Some(link)) = entry.link_name() {
                let link_owned = link.into_owned();
                let target_rel = link_owned.strip_prefix(&name).unwrap_or(&link_owned);
                if let Some(safe_target) = clean_local_rel(target_rel) {
                    std::fs::hard_link(dst_path.join(safe_target), &out_path)?;
                    continue;
                }
            }
        }
        entry.unpack(&out_path)?;
    }
    Ok(())
}

fn clean_local_rel(path: &std::path::Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

fn cp_to_container(client: &mut Client, local_src: &str, container: &str, remote_dst: &str) -> Result<()> {
    let src_dot = local_src.ends_with("/.") || local_src == ".";
    let src_path = std::path::Path::new(local_src);
    let meta = std::fs::symlink_metadata(src_path).with_context(|| format!("{} doesn't exist", src_path.display()))?;

    let remote_norm = {
        let parts: Vec<&str> = remote_dst.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
        format!("/{}", parts.join("/"))
    };
    let dst_is_dir = remote_dst.ends_with('/')
        || remote_norm == "/"
        || matches!(client.call(&Request::ListFiles { id: container.into(), path: remote_norm.clone() }), Ok(Response::Files(_)));

    let (target_dir, archive_prefix): (String, Option<String>) = if !meta.is_dir() {
        let src_name = src_path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("bad source path `{local_src}`"))?
            .to_string_lossy()
            .into_owned();
        if dst_is_dir {
            (remote_norm, Some(src_name))
        } else {
            let parent = remote_norm.rsplit_once('/').map(|(p, _)| if p.is_empty() { "/" } else { p }).unwrap_or("/");
            let name = remote_norm.rsplit('/').next().unwrap_or(&src_name).to_string();
            (parent.into(), Some(name))
        }
    } else if src_dot {
        (remote_norm, None)
    } else if dst_is_dir {
        let src_name = std::fs::canonicalize(src_path)?
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("bad folder `{local_src}`"))?
            .to_string_lossy()
            .into_owned();
        (remote_norm, Some(src_name))
    } else {
        let parent = remote_norm.rsplit_once('/').map(|(p, _)| if p.is_empty() { "/" } else { p }).unwrap_or("/");
        let name = remote_norm.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("dir").to_string();
        (parent.into(), Some(name))
    };

    let tmp_path = std::env::temp_dir().join(format!(
        "graviton-cp-{}-{}.tar",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos())
    ));
    let tmp = TempFile(tmp_path);
    {
        let file = std::fs::File::create(tmp.path())?;
        let mut tar = tar::Builder::new(file);
        tar.follow_symlinks(false);
        if !meta.is_dir() {
            let entry_name = archive_prefix.as_deref().unwrap_or("file");
            tar.append_path_with_name(src_path, entry_name)?;
        } else {
            let base = archive_prefix.map(PathBuf::from).unwrap_or_default();
            if !base.as_os_str().is_empty() {
                tar.append_dir(&base, src_path)?;
            }
            let mut stack = vec![PathBuf::new()];
            while let Some(rel) = stack.pop() {
                let dir = if rel.as_os_str().is_empty() { src_path.to_path_buf() } else { src_path.join(&rel) };
                let mut children: Vec<PathBuf> = std::fs::read_dir(&dir)?
                    .flatten()
                    .map(|e| rel.join(e.file_name()))
                    .collect();
                children.sort();
                for child in children {
                    let abs = src_path.join(&child);
                    let m = std::fs::symlink_metadata(&abs)?;
                    let archive_name = if base.as_os_str().is_empty() { child.clone() } else { base.join(&child) };
                    if m.is_dir() {
                        tar.append_dir(&archive_name, &abs)?;
                        stack.push(child);
                    } else if m.is_file() || m.file_type().is_symlink() {
                        tar.append_path_with_name(&abs, &archive_name)?;
                    }
                }
            }
        }
        tar.finish()?;
    }

    let size = std::fs::metadata(tmp.path())?.len();
    client.writer.write_all(&encode_line(&Request::PutArchive { id: container.into(), path: target_dir, size }))?;
    std::io::copy(&mut std::fs::File::open(tmp.path())?, &mut client.writer)?;
    client.writer.flush()?;

    let mut line = String::new();
    if client.reader.read_line(&mut line)? == 0 {
        bail!("gravitond closed the connection");
    }
    match decode_line::<Response>(line.as_bytes())? {
        Response::Copied { .. } => Ok(()),
        Response::Error { message } => bail!(message),
        other => unexpected(other),
    }
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

/// Temporarily disables terminal echo while reading a password.
struct NoEcho(libc::termios);

impl NoEcho {
    fn enable() -> Option<Self> {
        // SAFETY: tcgetattr/tcsetattr on stdin with a termios we own.
        unsafe {
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut original) != 0 {
                return None;
            }
            let mut hidden = original;
            hidden.c_lflag &= !libc::ECHO;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &hidden);
            Some(Self(original))
        }
    }
}

impl Drop for NoEcho {
    fn drop(&mut self) {
        // SAFETY: restores the attributes saved in `enable`.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
    }
}

fn prompt_line(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn prompt_password(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let guard = NoEcho::enable();
    let mut line = String::new();
    let res = std::io::stdin().read_line(&mut line);
    drop(guard);
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
        eprintln!();
    }
    res?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

#[derive(Clone, Default)]
struct ProgressRow {
    digest: String,
    done: u64,
    total: u64,
    skipped: bool,
    started: bool,
}

fn render_progress_rows(rows: &[ProgressRow], drawn: &mut usize, item_noun: &str, skip_label: &str) {
    let mut err = std::io::stderr().lock();
    if *drawn > 0 {
        let _ = write!(err, "\x1b[{}A", *drawn);
    }
    for (i, r) in rows.iter().enumerate() {
        let short = r.digest.strip_prefix("sha256:").map(|h| &h[..h.len().min(12)]).unwrap_or(&r.digest);
        let status = if !r.started {
            "waiting".to_string()
        } else if r.skipped {
            format!("{skip_label} ({})", bytes(r.total))
        } else if r.done >= r.total {
            format!("done   ({})", bytes(r.total))
        } else {
            let pct = (r.done * 100).checked_div(r.total).unwrap_or(0).min(100);
            let filled = (pct as usize * 20) / 100;
            let bar: String = "█".repeat(filled) + &"░".repeat(20 - filled);
            format!("[{bar}] {pct:>3}%  {} / {}", bytes(r.done), bytes(r.total))
        };
        let label = if short.is_empty() { format!("{item_noun} {}", i + 1) } else { short.to_string() };
        let _ = writeln!(err, "\r\x1b[K  {label:<12}  {status}");
    }
    let _ = err.flush();
    *drawn = rows.len();
}

struct ProgressTracker {
    rows: Vec<ProgressRow>,
    logged_done: Vec<bool>,
    drawn: usize,
    is_tty: bool,
    item_noun: &'static str,
    skip_label: &'static str,
}

struct ProgressItem {
    index: usize,
    total_items: usize,
    digest: String,
    done: u64,
    total: u64,
    skipped: bool,
}

impl ProgressTracker {
    fn new(item_noun: &'static str, skip_label: &'static str) -> Self {
        Self {
            rows: Vec::new(),
            logged_done: Vec::new(),
            drawn: 0,
            is_tty: unsafe { libc::isatty(libc::STDERR_FILENO) } == 1,
            item_noun,
            skip_label,
        }
    }

    fn start(&mut self, count: usize, total_bytes: u64) {
        self.rows = vec![ProgressRow::default(); count];
        self.logged_done = vec![false; count];
        if !self.is_tty || count > 24 {
            let plural = if count == 1 { "" } else { "s" };
            eprintln!("  {count} {}{plural}, {} total", self.item_noun, bytes(total_bytes));
        } else if !self.rows.is_empty() {
            render_progress_rows(&self.rows, &mut self.drawn, self.item_noun, self.skip_label);
        }
    }

    fn update(&mut self, item: ProgressItem) {
        let len = item.total_items.max(item.index + 1);
        if self.rows.len() < len {
            self.rows.resize(len, ProgressRow::default());
            self.logged_done.resize(len, false);
        }
        self.rows[item.index] = ProgressRow {
            digest: item.digest.clone(),
            done: item.done,
            total: item.total,
            skipped: item.skipped,
            started: true,
        };
        if self.is_tty && self.rows.len() <= 24 {
            render_progress_rows(&self.rows, &mut self.drawn, self.item_noun, self.skip_label);
        } else {
            let is_done = item.skipped || item.done >= item.total;
            if is_done && !self.logged_done[item.index] {
                self.logged_done[item.index] = true;
                let short = item
                    .digest
                    .strip_prefix("sha256:")
                    .map(|h| &h[..h.len().min(12)])
                    .unwrap_or(&item.digest);
                let tag = if item.skipped { self.skip_label } else { "done" };
                eprintln!(
                    "  {} {}/{} {short}  {tag} ({})",
                    self.item_noun,
                    item.index + 1,
                    self.rows.len(),
                    bytes(item.total)
                );
            }
        }
    }
}

fn pull_image(client: &mut Client, reference: &str, platform: Option<&str>) -> Result<()> {
    match platform {
        Some(p) => eprintln!("Pulling {reference} ({p})…"),
        None => eprintln!("Pulling {reference}…"),
    }
    client.writer.write_all(&encode_line(&Request::Pull {
        reference: reference.to_string(),
        progress: true,
        platform: platform.map(String::from),
    }))?;
    client.writer.flush()?;

    let mut tracker = ProgressTracker::new("layer", "cached");
    let mut line = String::new();

    loop {
        line.clear();
        if client.reader.read_line(&mut line)? == 0 {
            bail!("gravitond closed the connection");
        }
        match decode_line::<Response>(line.as_bytes())? {
            Response::PullProgress(PullProgress::Resolved { layers, total_bytes }) => {
                tracker.start(layers, total_bytes);
            }
            Response::PullProgress(PullProgress::Layer { index, total_layers, digest, done, total, cached }) => {
                tracker.update(ProgressItem {
                    index,
                    total_items: total_layers,
                    digest,
                    done,
                    total,
                    skipped: cached,
                });
            }
            Response::Image(i) => {
                println!(
                    "Pulled {}:{} ({}, {}, {})",
                    i.repository,
                    i.tag,
                    bytes(i.size_bytes),
                    i.architecture,
                    &i.digest[..i.digest.len().min(19)]
                );
                return Ok(());
            }
            Response::Error { message } => bail!(message),
            other => return unexpected(other),
        }
    }
}

fn push_image(client: &mut Client, reference: &str) -> Result<()> {
    eprintln!("Pushing {reference}…");
    client.writer.write_all(&encode_line(&Request::PushImage {
        reference: reference.to_string(),
        progress: true,
    }))?;
    client.writer.flush()?;

    let mut tracker = ProgressTracker::new("blob", "exists");
    let mut line = String::new();

    loop {
        line.clear();
        if client.reader.read_line(&mut line)? == 0 {
            bail!("gravitond closed the connection");
        }
        match decode_line::<Response>(line.as_bytes())? {
            Response::ImagePushProgress(ImagePushProgress::Started { blobs, total_bytes }) => {
                tracker.start(blobs, total_bytes);
            }
            Response::ImagePushProgress(ImagePushProgress::Blob {
                index,
                total_blobs,
                digest,
                done,
                total,
                skipped,
            }) => {
                tracker.update(ProgressItem {
                    index,
                    total_items: total_blobs,
                    digest,
                    done,
                    total,
                    skipped,
                });
            }
            Response::ImagePushProgress(ImagePushProgress::Manifest { digest, size }) => {
                let short = &digest[..digest.len().min(19)];
                eprintln!("  manifest {short} ({})", bytes(size));
            }
            Response::ImagePushed(res) => {
                let disp = ImageReference::parse(&res.reference)
                    .map(|r| r.display_name())
                    .unwrap_or_else(|_| res.reference.clone());
                let short = &res.digest[..res.digest.len().min(19)];
                println!(
                    "Pushed {disp} ({short}, {} uploaded, {} existed)",
                    res.blobs_pushed, res.blobs_skipped
                );
                return Ok(());
            }
            Response::Error { message } => bail!(message),
            other => return unexpected(other),
        }
    }
}

pub(crate) fn parse_time_arg(raw: &str) -> Result<u64> {
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    let now_secs = (now_ms / 1000) as libc::time_t;
    // SAFETY: `localtime_r` initializes `tm` for a valid stack `time_t`.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let offset_secs = if unsafe { libc::localtime_r(&now_secs, &mut tm) }.is_null() {
        0
    } else {
        tm.tm_gmtoff
    };
    graviton_core::time::parse_timestamp_ms_with_offset(raw, now_ms, offset_secs).map_err(anyhow::Error::msg)
}

fn print_log_line(l: &LogLine, timestamps: bool, flush_stdout: bool) -> std::io::Result<()> {
    let prefix = timestamps.then(|| graviton_core::time::format_rfc3339_ms(l.timestamp_ms));
    match l.stream {
        LogStream::Stdout => {
            let mut out = std::io::stdout().lock();
            if let Some(ts) = prefix {
                writeln!(out, "{ts} {}", l.message)?;
            } else {
                writeln!(out, "{}", l.message)?;
            }
            if flush_stdout {
                out.flush()?;
            }
        }
        LogStream::Stderr => {
            let mut err = std::io::stderr().lock();
            if let Some(ts) = prefix {
                writeln!(err, "{ts} {}", l.message)?;
            } else {
                writeln!(err, "{}", l.message)?;
            }
        }
        LogStream::System => {
            let mut err = std::io::stderr().lock();
            let text = if let Some(ts) = prefix {
                format!("{ts} graviton: {}", l.message)
            } else {
                format!("graviton: {}", l.message)
            };
            if err.is_terminal() {
                writeln!(err, "\x1b[2m{text}\x1b[0m")?;
            } else {
                writeln!(err, "{text}")?;
            }
        }
    }
    Ok(())
}

fn stream_logs(
    client: &mut Client,
    id: &str,
    tail: Option<usize>,
    follow: bool,
    timestamps: bool,
    since_ms: Option<u64>,
    until_ms: Option<u64>,
) -> Result<Option<i32>> {
    let req = Request::Logs { id: id.to_string(), tail, follow, since_ms, until_ms };
    if !follow {
        return match client.call(&req)? {
            Response::Logs(lines) => {
                for l in &lines {
                    match print_log_line(l, timestamps, false) {
                        Ok(()) => {}
                        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => return Ok(None),
                        Err(err) => return Err(err.into()),
                    }
                }
                Ok(None)
            }
            other => unexpected(other).map(|()| None),
        };
    }

    client.send(&req)?;
    while let Some(resp) = client.recv()? {
        match resp {
            Response::Log(l) => match print_log_line(&l, timestamps, true) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => return Ok(None),
                Err(err) => return Err(err.into()),
            },
            Response::LogEnd { exit_code } => return Ok(exit_code),
            other => return unexpected(other).map(|()| None),
        }
    }
    Ok(None)
}

/// Stream a container's output until it exits; returns its exit code.
fn follow(client: &mut Client, id: &str) -> Result<i32> {
    if let Some(code) = stream_logs(client, id, None, true, false, None, None)? {
        return Ok(code);
    }
    match client.call(&Request::ListContainers)? {
        Response::Containers(list) => Ok(list.into_iter().find(|c| c.id == id).and_then(|c| c.exit_code).unwrap_or(0)),
        other => unexpected(other).map(|()| 1),
    }
}

pub(crate) fn unexpected(response: Response) -> Result<()> {
    bail!("unexpected response from gravitond: {response:?}")
}

fn print_containers<'a>(list: impl Iterator<Item = &'a ContainerSummary>) {
    println!("{:<13} {:<14} {:<26} {:<24} {:>6} {:>9}  PORTS", "ID", "NAME", "IMAGE", "STATUS", "CPU", "MEM");
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    for c in list {
        let status = match (c.status, c.started_at, c.exit_code) {
            (ContainerStatus::Running, Some(t), _) => {
                let up = format!("Up {}", uptime(now.saturating_sub(t)));
                match c.health {
                    Some(graviton_proto::HealthStatus::Starting) => format!("{up} (health: starting)"),
                    Some(graviton_proto::HealthStatus::Healthy) => format!("{up} (healthy)"),
                    Some(graviton_proto::HealthStatus::Unhealthy) => format!("{up} (unhealthy)"),
                    None => up,
                }
            }
            (ContainerStatus::Paused, Some(t), _) => format!("Up {} (Paused)", uptime(now.saturating_sub(t))),
            (ContainerStatus::Paused, None, _) => "Paused".to_string(),
            (ContainerStatus::Exited, _, Some(code)) => format!("Exited ({code})"),
            (s, _, _) => format!("{s:?}"),
        };
        let ports = c
            .ports
            .iter()
            .map(|p| {
                let prefix = match p.host_ip.as_deref() {
                    Some(ip) if ip.contains(':') => format!("[{ip}]:"),
                    Some(ip) => format!("{ip}:"),
                    None => String::new(),
                };
                let proto = match p.protocol {
                    graviton_proto::PortProtocol::Tcp => "",
                    graviton_proto::PortProtocol::Udp => "/udp",
                };
                format!("{prefix}{}->{}{proto}", p.host_port, p.container_port)
            })
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "{:<13} {:<14} {:<26} {:<24} {:>6} {:>9}  {}",
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

pub(crate) fn uptime(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

/// HH:MM:SS.mmm in UTC (good enough until we pull in a time crate).
/// Unix milliseconds as local wall-clock time, `hh:mm:ss.mmm`.
fn clock(ms: u64) -> String {
    let secs = (ms / 1000) as libc::time_t;
    // SAFETY: localtime_r fills a tm we own from a time_t we own.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        let s = ms / 1000;
        return format!("{:02}:{:02}:{:02}.{:03}", (s / 3600) % 24, (s / 60) % 60, s % 60, ms % 1000);
    }
    format!("{:02}:{:02}:{:02}.{:03}", tm.tm_hour, tm.tm_min, tm.tm_sec, ms % 1000)
}

/// Parse `--secret id=ID,src=FILE` or `--secret id=ID,env=VAR` (or `--secret id=ID`
/// reading from the host env var named `ID`).
fn parse_build_secret(spec: &str) -> Result<BuildSecret> {
    let mut id: Option<String> = None;
    let mut src: Option<String> = None;
    let mut env_name: Option<String> = None;
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (k, v) = part.split_once('=').ok_or_else(|| anyhow::anyhow!("bad --secret `{spec}`: expected id=ID,src=FILE or id=ID,env=VAR"))?;
        match k.trim() {
            "id" => id = Some(v.trim().to_string()),
            "src" | "source" => src = Some(v.trim().to_string()),
            "env" => env_name = Some(v.trim().to_string()),
            "type" if v.trim() == "file" || v.trim() == "env" => {}
            other => bail!("unknown option `{other}` in --secret `{spec}`"),
        }
    }
    let id = id.filter(|s| !s.is_empty()).ok_or_else(|| anyhow::anyhow!("--secret `{spec}` needs id=..."))?;
    let data = match (src, env_name) {
        (Some(_), Some(_)) => bail!("--secret `{spec}` can't set both src= and env="),
        (Some(path), None) => std::fs::read(&path).with_context(|| format!("reading secret `{id}` from {path}"))?,
        (None, Some(var)) => std::env::var(&var)
            .with_context(|| format!("reading secret `{id}` from environment variable {var}"))?
            .into_bytes(),
        (None, None) => std::env::var(&id)
            .with_context(|| format!("--secret id={id}: pass src=FILE or env=VAR, or set ${id}"))?
            .into_bytes(),
    };
    Ok(BuildSecret { id, data })
}

fn cmd_stats(
    client: &mut Client,
    ids: Vec<String>,
    all: bool,
    no_stream: bool,
    format: &str,
) -> Result<()> {
    client.send(&Request::StatsStream { ids, all, follow: !no_stream })?;
    while let Some(resp) = client.recv()? {
        match resp {
            Response::StatsSnapshot(rows) => {
                if let Err(err) = print_stats_snapshot(&rows, format, !no_stream) {
                    if err.kind() == std::io::ErrorKind::BrokenPipe {
                        return Ok(());
                    }
                    return Err(err.into());
                }
                if no_stream {
                    break;
                }
            }
            other => return unexpected(other),
        }
    }
    Ok(())
}

fn print_stats_snapshot(
    rows: &[graviton_proto::ContainerStatRow],
    format: &str,
    clear_tty: bool,
) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    if format == "json" {
        for r in rows {
            let line = serde_json::to_vec(r).map_err(std::io::Error::other)?;
            out.write_all(&line)?;
            out.write_all(b"\n")?;
        }
        return out.flush();
    }

    if clear_tty && out.is_terminal() {
        out.write_all(b"\x1b[2J\x1b[H")?;
    }
    writeln!(
        out,
        "{:<13} {:<18} {:>8}  {:<23} {:>7}  {:<21} {:>5}",
        "CONTAINER", "NAME", "CPU %", "MEM USAGE / LIMIT", "MEM %", "NET I/O", "PIDS"
    )?;
    for r in rows {
        let short_id: String = r.id.chars().take(12).collect();
        let active = matches!(r.status, ContainerStatus::Running | ContainerStatus::Paused);
        if active {
            let cpu = format!("{:.2}%", r.cpu_percent);
            let mem = format!("{} / {}", bytes(r.memory_bytes), bytes(r.memory_limit_bytes));
            let mem_pct = if r.memory_limit_bytes > 0 {
                format!("{:.2}%", r.memory_bytes as f64 / r.memory_limit_bytes as f64 * 100.0)
            } else {
                "0.00%".into()
            };
            let net = format!("{} / {}", bytes(r.net_rx_bytes), bytes(r.net_tx_bytes));
            let pids = r.pids.to_string();
            writeln!(out, "{short_id:<13} {:<18} {cpu:>8}  {mem:<23} {mem_pct:>7}  {net:<21} {pids:>5}", r.name)?;
        } else {
            writeln!(
                out,
                "{short_id:<13} {:<18} {:>8}  {:<23} {:>7}  {:<21} {:>5}",
                r.name, "--", "-- / --", "--", "-- / --", "--"
            )?;
        }
    }
    out.flush()
}

#[cfg(test)]
fn network_delta_totals(samples: &[graviton_proto::StatSample]) -> (u64, u64) {
    let mut rx = 0u64;
    let mut tx = 0u64;
    for pair in samples.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        rx += if b.net_rx_bytes >= a.net_rx_bytes { b.net_rx_bytes - a.net_rx_bytes } else { b.net_rx_bytes };
        tx += if b.net_tx_bytes >= a.net_tx_bytes { b.net_tx_bytes - a.net_tx_bytes } else { b.net_tx_bytes };
    }
    (rx, tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_build_secrets() {
        let tmp = std::env::temp_dir().join(format!("grav-secret-test-{}", std::process::id()));
        std::fs::write(&tmp, b"top-secret-key\n").unwrap();
        let s = parse_build_secret(&format!("id=mykey,src={}", tmp.display())).unwrap();
        assert_eq!(s.id, "mykey");
        assert_eq!(s.data, b"top-secret-key\n");
        let _ = std::fs::remove_file(&tmp);

        assert!(parse_build_secret("src=/tmp/x").is_err());
        assert!(parse_build_secret("id=a,src=/x,env=Y").is_err());
    }

    #[test]
    fn parses_cp_endpoints() {
        assert_eq!(parse_cp_endpoint("web:/etc/hosts"), CpEndpoint::Container("web", "/etc/hosts"));
        assert_eq!(parse_cp_endpoint("./foo:bar"), CpEndpoint::Local("./foo:bar"));
        assert_eq!(parse_cp_endpoint("/tmp/a:b"), CpEndpoint::Local("/tmp/a:b"));
        assert_eq!(parse_cp_endpoint("-"), CpEndpoint::Local("-"));
    }

    #[test]
    fn formats_sizes_and_durations() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(412 * 1024 * 1024), "412 MB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024 + 100 * 1024 * 1024), "3.1 GB");
        assert_eq!(uptime(45), "45s");
        assert_eq!(uptime(11_520), "3h 12m");
        // Local time: only the seconds and milliseconds are the same everywhere.
        assert!(clock(3_723_004).ends_with(":03.004"));
    }

    #[test]
    fn parses_tag_and_push_commands() {
        let tagged = Cli::try_parse_from(["grav", "tag", "app:1", "ghcr.io/me/app:1.0"]).unwrap();
        assert!(matches!(
            tagged.command,
            Command::Tag { ref source, ref target } if source == "app:1" && target == "ghcr.io/me/app:1.0"
        ));

        for img_ref in ["ghcr.io/me/app:1.0", "localhost:5000/app:v1", "test:1.0"] {
            let parsed = Cli::try_parse_from(["grav", "push", img_ref]).unwrap();
            assert!(
                matches!(parsed.command, Command::Push { action: None, image: Some(ref r) } if r == img_ref),
                "unexpected parse for `{img_ref}`: {:?}",
                parsed.command
            );
        }

        let st = Cli::try_parse_from(["grav", "push", "status"]).unwrap();
        assert!(matches!(st.command, Command::Push { action: Some(PushAction::Status), image: None }));

        let t = Cli::try_parse_from(["grav", "push", "test"]).unwrap();
        assert!(matches!(t.command, Command::Push { action: Some(PushAction::Test), image: None }));

        let setup = Cli::try_parse_from([
            "grav",
            "push",
            "setup",
            "--key-id",
            "KEY1234567",
            "--team-id",
            "TEAM123456",
            "AuthKey.p8",
        ])
        .unwrap();
        assert!(matches!(
            setup.command,
            Command::Push { action: Some(PushAction::Setup { .. }), image: None }
        ));
    }

    #[test]
    fn computes_reboot_safe_network_deltas() {
        let mk = |ts, rx, tx| graviton_proto::StatSample {
            timestamp_ms: ts,
            cpu_percent: 1.0,
            memory_bytes: 100,
            memory_limit_bytes: 1000,
            net_rx_bytes: rx,
            net_tx_bytes: tx,
        };
        let samples = vec![
            mk(1_000, 1_000, 500),
            mk(3_000, 1_600, 800),
            // VM restart resets NIC counters to 0 -> 200/100 in next tick.
            mk(5_000, 200, 100),
            mk(7_000, 350, 180),
        ];
        // Delta = (600 + 200 + 150, 300 + 100 + 80) = (950, 480)
        assert_eq!(network_delta_totals(&samples), (950, 480));
        assert_eq!(network_delta_totals(&[]), (0, 0));
    }

    #[test]
    fn parses_pause_and_multi_arch_build_commands() {
        let p = Cli::try_parse_from(["grav", "pause", "api"]).unwrap();
        assert!(matches!(p.command, Command::Pause { container } if container == "api"));

        let u = Cli::try_parse_from(["grav", "unpause", "api"]).unwrap();
        assert!(matches!(u.command, Command::Unpause { container } if container == "api"));

        let b = Cli::try_parse_from(["grav", "build", "--platform", "linux/arm64,linux/amd64", "-t", "app:v1", "."]).unwrap();
        assert!(matches!(
            b.command,
            Command::Build { platform: Some(plat), .. } if plat == "linux/arm64,linux/amd64"
        ));
    }

    #[test]
    fn parses_save_load_commit_commands() {
        let s = Cli::try_parse_from(["grav", "save", "-o", "/tmp/out.tar", "alpine:3.22", "app:1"]).unwrap();
        assert!(matches!(
            s.command,
            Command::Save { output: Some(ref p), ref images } if p == std::path::Path::new("/tmp/out.tar") && images == &["alpine:3.22", "app:1"]
        ));

        let l = Cli::try_parse_from(["grav", "load", "-i", "/tmp/in.tar"]).unwrap();
        assert!(matches!(
            l.command,
            Command::Load { input: Some(ref p) } if p == std::path::Path::new("/tmp/in.tar")
        ));

        let l_stdin = Cli::try_parse_from(["grav", "load"]).unwrap();
        assert!(matches!(l_stdin.command, Command::Load { input: None }));

        let c = Cli::try_parse_from([
            "grav",
            "commit",
            "-a",
            "Dev",
            "-m",
            "snapshot",
            "-c",
            "ENV FOO=bar",
            "-c",
            "WORKDIR /app",
            "web",
            "web:snap",
        ])
        .unwrap();
        assert!(matches!(
            c.command,
            Command::Commit {
                author: Some(ref a),
                message: Some(ref m),
                ref changes,
                ref container,
                ref image,
            } if a == "Dev" && m == "snapshot" && changes == &["ENV FOO=bar", "WORKDIR /app"] && container == "web" && image == "web:snap"
        ));
    }

    #[test]
    fn parses_logs_and_compose_logs_flags() {
        let l = Cli::try_parse_from([
            "grav", "logs", "-f", "-t", "-n", "all", "--since", "10m", "--until", "5m", "web",
        ])
        .unwrap();
        assert!(matches!(
            l.command,
            Command::Logs {
                ref container,
                tail: Some(TailArg::All),
                follow: true,
                timestamps: true,
                since: Some(ref s),
                until: Some(ref u),
            } if container == "web" && s == "10m" && u == "5m"
        ));

        let l2 = Cli::try_parse_from(["grav", "logs", "--tail", "20", "web"]).unwrap();
        assert!(matches!(l2.command, Command::Logs { tail: Some(TailArg::Lines(20)), follow: false, .. }));

        let cl = Cli::try_parse_from([
            "grav", "compose", "logs", "-f", "--timestamps", "--since", "5m", "--until", "1m", "-n", "15", "api", "db",
        ])
        .unwrap();
        assert!(matches!(
            cl.command,
            Command::Compose(compose::ComposeArgs {
                action: compose::ComposeAction::Logs {
                    follow: true,
                    timestamps: true,
                    since: Some(ref s),
                    until: Some(ref u),
                    tail: Some(TailArg::Lines(15)),
                    ref services,
                },
                ..
            }) if s == "5m" && u == "1m" && services == &["api", "db"]
        ));
    }

    #[test]
    fn parses_security_run_and_stats_stream_flags() {
        let r = Cli::try_parse_from([
            "grav",
            "run",
            "--read-only",
            "--init",
            "--cap-drop",
            "ALL",
            "--cap-add",
            "CAP_NET_BIND_SERVICE",
            "--tmpfs",
            "/tmp:rw,noexec,size=64m",
            "--shm-size",
            "128m",
            "--ulimit",
            "nofile=2048:4096",
            "alpine",
            "id",
        ])
        .unwrap();
        assert!(matches!(
            r.command,
            Command::Run {
                read_only: true,
                init: true,
                ref cap_drop,
                ref cap_add,
                ref tmpfs,
                ref shm_size,
                ref ulimit,
                ..
            } if cap_drop == &["ALL"]
                && cap_add == &["CAP_NET_BIND_SERVICE"]
                && tmpfs == &["/tmp:rw,noexec,size=64m"]
                && shm_size.as_deref() == Some("128m")
                && ulimit == &["nofile=2048:4096"]
        ));

        let st_default = Cli::try_parse_from(["grav", "stats"]).unwrap();
        assert!(matches!(
            st_default.command,
            Command::Stats { ref containers, all: false, no_stream: false, ref format }
                if containers.is_empty() && format == "table"
        ));

        let st_opts = Cli::try_parse_from([
            "grav",
            "stats",
            "--no-stream",
            "-a",
            "--format",
            "json",
            "web",
            "db",
        ])
        .unwrap();
        assert!(matches!(
            st_opts.command,
            Command::Stats { ref containers, all: true, no_stream: true, ref format }
                if containers == &["web", "db"] && format == "json"
        ));
    }

    #[test]
    fn parses_completions_command() {
        for (arg, expected) in [
            ("zsh", CompletionShell::Zsh),
            ("bash", CompletionShell::Bash),
            ("fish", CompletionShell::Fish),
        ] {
            let parsed = Cli::try_parse_from(["grav", "completions", arg]).unwrap();
            assert!(matches!(parsed.command, Command::Completions { shell } if shell == expected));
        }
        assert!(Cli::try_parse_from(["grav", "completions", "cmd"]).is_err());

        let up_plain = Cli::try_parse_from(["grav", "update"]).unwrap();
        assert!(matches!(up_plain.command, Command::Update { check: false, json: false }));
        let up_flags = Cli::try_parse_from(["grav", "update", "--check", "--json"]).unwrap();
        assert!(matches!(up_flags.command, Command::Update { check: true, json: true }));
    }

    #[test]
    fn generates_non_empty_completions_for_all_shells() {
        for (shell, marker) in [
            (CompletionShell::Zsh, "#compdef grav"),
            (CompletionShell::Bash, "_grav"),
            (CompletionShell::Fish, "complete -c grav"),
        ] {
            let mut buf = Vec::new();
            write_completions(shell, &mut buf).expect("write to Vec");
            let script = String::from_utf8(buf).expect("valid utf-8 completion script");
            assert!(script.contains(marker), "{shell:?} missing marker {marker}");
            for needle in ["compose", "stats", "hosts", "completions", "docker-env"] {
                assert!(script.contains(needle), "{shell:?} missing `{needle}`");
            }
            let prefix = if shell == CompletionShell::Fish { "-l " } else { "--" };
            for flag in ["read-only", "cap-drop"] {
                let needle = format!("{prefix}{flag}");
                assert!(script.contains(&needle), "{shell:?} missing `{needle}`");
            }
        }
    }

    #[test]
    fn parses_network_flags_and_subcommands() {
        let r = Cli::try_parse_from([
            "grav", "run", "--network", "frontend", "--net", "backend", "alpine", "sh",
        ])
        .unwrap();
        assert!(matches!(r.command, Command::Run { ref network, .. } if network == &["frontend", "backend"]));

        let ls = Cli::try_parse_from(["grav", "network"]).unwrap();
        assert!(matches!(ls.command, Command::Network { action: None }));

        let create = Cli::try_parse_from(["grav", "network", "create", "--internal", "backend"]).unwrap();
        assert!(matches!(
            create.command,
            Command::Network { action: Some(NetworkAction::Create { ref name, internal: true }) } if name == "backend"
        ));

        let rm = Cli::try_parse_from(["grav", "network", "rm", "backend"]).unwrap();
        assert!(matches!(
            rm.command,
            Command::Network { action: Some(NetworkAction::Rm { ref name }) } if name == "backend"
        ));

        let conn = Cli::try_parse_from(["grav", "network", "connect", "backend", "api"]).unwrap();
        assert!(matches!(
            conn.command,
            Command::Network { action: Some(NetworkAction::Connect { ref network, ref container }) }
                if network == "backend" && container == "api"
        ));

        let disc = Cli::try_parse_from(["grav", "network", "disconnect", "backend", "api"]).unwrap();
        assert!(matches!(
            disc.command,
            Command::Network { action: Some(NetworkAction::Disconnect { ref network, ref container }) }
                if network == "backend" && container == "api"
        ));
    }

    #[test]
    fn parses_fork_and_diff_commands() {
        let f = Cli::try_parse_from([
            "grav", "fork", "pg", "pg-test", "--rm", "--no-start", "-p", "5433:5432", "-e", "MODE=test", "--", "sh", "-c", "id",
        ])
        .unwrap();
        assert!(matches!(
            f.command,
            Command::Fork {
                ref container,
                ref name,
                count: 1,
                no_start: true,
                rm: true,
                ref publish,
                ref env,
                ref command,
            } if container == "pg"
                && name.as_deref() == Some("pg-test")
                && publish == &["5433:5432"]
                && env == &["MODE=test"]
                && command == &["sh", "-c", "id"]
        ));

        let multi = Cli::try_parse_from(["grav", "fork", "pg", "--count", "3"]).unwrap();
        assert!(matches!(multi.command, Command::Fork { ref container, name: None, count: 3, .. } if container == "pg"));

        let d = Cli::try_parse_from(["grav", "diff", "pg-test"]).unwrap();
        assert!(matches!(d.command, Command::Diff { ref container } if container == "pg-test"));
    }
}
