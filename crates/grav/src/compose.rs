//! `grav compose`: multi-container stacks from `compose.yaml` / `docker-compose.yml`.
//!
//! Translates Compose services into native Graviton builds, named volumes, and
//! VMs (each service runs in its own VM, sharing the L2 switch and project-scoped
//! service DNS so `api` reaches `db` by service name).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use graviton_core::compose::{ComposeProject, ComposeService, DependsCondition, LoadOptions};
use graviton_proto::{
    BuildSpec, ContainerStatus, ContainerSummary, LogStream, PortProtocol, Request, Response, RunSpec,
};

use crate::remote::Target;
use crate::{Client, run_build_spec, unexpected, uptime};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigint(_sig: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

fn install_sigint_handler() {
    INTERRUPTED.store(false, Ordering::SeqCst);
    // SAFETY: `handle_sigint` only performs an async-signal-safe atomic store.
    unsafe {
        libc::signal(libc::SIGINT, handle_sigint as *const () as libc::sighandler_t);
    }
}

#[derive(Args, Debug)]
pub struct ComposeArgs {
    /// Compose file(s) to load; repeatable (default: compose.yaml / docker-compose.yml).
    #[arg(short = 'f', long = "file")]
    pub file: Vec<PathBuf>,

    /// Override the stack/project name (default: top-level `name:` or folder name).
    #[arg(short = 'p', long = "project-name")]
    pub project_name: Option<String>,

    /// Alternate `.env` file for `${VAR}` interpolation.
    #[arg(long = "env-file")]
    pub env_file: Option<PathBuf>,

    /// Enable optional service profile(s); repeatable.
    #[arg(long = "profile")]
    pub profile: Vec<String>,

    #[command(subcommand)]
    pub action: ComposeAction,
}

#[derive(Subcommand, Debug)]
pub enum ComposeAction {
    /// Build (if needed), create, and start all services in dependency order.
    Up {
        /// Run in the background instead of streaming multiplexed logs.
        #[arg(short, long)]
        detach: bool,
        /// Rebuild images for services with `build:` even if already built.
        #[arg(long)]
        build: bool,
        /// Don't build images automatically even if missing.
        #[arg(long)]
        no_build: bool,
        /// Stop and recreate containers even if they already exist.
        #[arg(long)]
        force_recreate: bool,
        /// Specific services to start (plus their dependencies).
        services: Vec<String>,
    },
    /// Stop and remove all containers in this stack.
    Down {
        /// Also remove named volumes declared in the Compose file (non-external).
        #[arg(short, long)]
        volumes: bool,
    },
    /// List containers belonging to this stack.
    Ps {
        /// Include stopped containers.
        #[arg(short, long)]
        all: bool,
    },
    /// View interleaved logs from services in this stack.
    Logs {
        /// Keep streaming new log lines.
        #[arg(short, long)]
        follow: bool,
        /// Number of lines from the end per service.
        #[arg(short = 'n', long)]
        tail: Option<usize>,
        /// Specific services to show (default: all).
        services: Vec<String>,
    },
    /// Stop running services in reverse dependency order (without removing them).
    Stop { services: Vec<String> },
    /// Start existing stopped services in dependency order.
    Start { services: Vec<String> },
    /// Restart services in dependency order.
    Restart { services: Vec<String> },
    /// Build images for services that specify `build:`.
    Build {
        /// Rebuild every Dockerfile step without cache.
        #[arg(long)]
        no_cache: bool,
        /// Specific services to build (default: all with `build:`).
        services: Vec<String>,
    },
    /// Validate and print the resolved Compose stack.
    Config {
        /// Output resolved configuration as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List all Compose stacks currently on the engine (no compose.yaml needed).
    Ls,
}

fn load_project(args: &ComposeArgs) -> Result<ComposeProject> {
    let cwd = std::env::current_dir().context("determining current directory")?;
    let opts = LoadOptions {
        files: args.file.clone(),
        project_name: args.project_name.clone(),
        env_file: args.env_file.clone(),
    };
    ComposeProject::load(&cwd, &opts).map_err(Into::into)
}

pub fn run(args: ComposeArgs, client: &mut Client, target: &Target) -> Result<()> {
    match &args.action {
        ComposeAction::Ls => return list_stacks(client),
        ComposeAction::Config { json } => {
            let proj = load_project(&args)?;
            return print_config(&proj, *json);
        }
        _ => {}
    }

    match args.action {
        ComposeAction::Up { detach, build, no_build, force_recreate, ref services } => {
            let proj = load_project(&args)?;
            up(client, target, &proj, services, &args.profile, detach, build, no_build, force_recreate)
        }
        ComposeAction::Down { volumes } => down(client, &args, volumes),
        ComposeAction::Ps { all } => {
            let proj = load_project(&args)?;
            ps(client, &proj, all)
        }
        ComposeAction::Logs { follow, tail, ref services } => {
            let proj = load_project(&args)?;
            logs(client, &proj, services, tail, follow)
        }
        ComposeAction::Stop { ref services } => {
            let proj = load_project(&args)?;
            lifecycle_many(client, &proj, services, &args.profile, Action::Stop)
        }
        ComposeAction::Start { ref services } => {
            let proj = load_project(&args)?;
            lifecycle_many(client, &proj, services, &args.profile, Action::Start)
        }
        ComposeAction::Restart { ref services } => {
            let proj = load_project(&args)?;
            lifecycle_many(client, &proj, services, &args.profile, Action::Restart)
        }
        ComposeAction::Build { no_cache, ref services } => {
            let proj = load_project(&args)?;
            build_services(client, target, &proj, services, &args.profile, no_cache)
        }
        ComposeAction::Config { .. } | ComposeAction::Ls => unreachable!(),
    }
}

fn list_stacks(client: &mut Client) -> Result<()> {
    let containers = list_containers(client)?;
    let mut stacks: BTreeMap<String, Vec<ContainerSummary>> = BTreeMap::new();
    for c in containers {
        if let Some(proj) = c.project.clone() {
            stacks.entry(proj).or_default().push(c);
        }
    }
    if stacks.is_empty() {
        println!("No Compose stacks yet. Run `grav compose up` in a folder with a compose.yaml.");
        return Ok(());
    }
    println!("{:<22} {:<16} SERVICES", "STACK", "STATUS");
    for (name, members) in stacks {
        let running = members.iter().filter(|c| c.status == ContainerStatus::Running).count();
        let status = if running == members.len() {
            format!("running ({running}/{})", members.len())
        } else if running == 0 {
            format!("stopped (0/{})", members.len())
        } else {
            format!("partial ({running}/{})", members.len())
        };
        let mut svcs: Vec<String> = members
            .iter()
            .map(|c| c.service.clone().unwrap_or_else(|| c.name.clone()))
            .collect();
        svcs.sort();
        svcs.dedup();
        println!("{:<22} {:<16} {}", name, status, svcs.join(", "));
    }
    Ok(())
}

fn print_config(proj: &ComposeProject, as_json: bool) -> Result<()> {
    if as_json {
        println!("{}", serde_json::to_string_pretty(proj)?);
        return Ok(());
    }
    println!("Project: {}", proj.name);
    println!(
        "Files:   {}",
        proj.files.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
    );
    if !proj.volumes.is_empty() {
        let vols: Vec<String> = proj
            .volumes
            .iter()
            .map(|(k, v)| {
                if v.external {
                    format!("{k} -> {} (external)", v.volume_name)
                } else {
                    format!("{k} -> {}", v.volume_name)
                }
            })
            .collect();
        println!("Volumes: {}", vols.join(", "));
    }
    println!();
    println!("{:<18} {:<24} {:<28} DEPENDS ON", "SERVICE", "CONTAINER", "IMAGE");
    for svc in proj.services.values() {
        let deps = if svc.depends_on.is_empty() {
            "—".into()
        } else {
            svc.depends_on.keys().cloned().collect::<Vec<_>>().join(", ")
        };
        let img = if svc.build.is_some() { format!("{} (build)", svc.image) } else { svc.image.clone() };
        println!("{:<18} {:<24} {:<28} {deps}", svc.name, svc.container_name, img);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn up(
    client: &mut Client,
    target: &Target,
    proj: &ComposeProject,
    selected: &[String],
    profiles: &[String],
    detach: bool,
    force_build: bool,
    no_build: bool,
    force_recreate: bool,
) -> Result<()> {
    let ordered = proj.startup_order(selected, profiles)?;
    if ordered.is_empty() {
        println!("No services selected to start (check --profile or service names).");
        return Ok(());
    }

    // Services that another service waits on with `condition: service_completed_successfully`.
    let mut wait_complete: HashSet<&str> = HashSet::new();
    for svc in &ordered {
        for (dep, cond) in &svc.depends_on {
            if *cond == DependsCondition::ServiceCompletedSuccessfully {
                wait_complete.insert(dep.as_str());
            }
        }
    }

    // Ensure non-external named volumes exist before starting services.
    for vol in proj.owned_volume_names() {
        match client.call(&Request::CreateVolume { name: vol.clone() }) {
            Ok(_) => eprintln!("  + volume {vol}"),
            Err(err) if err.to_string().contains("already exists") => {}
            Err(err) => return Err(err).with_context(|| format!("creating volume {vol}")),
        }
    }

    // Build services that need building.
    let existing_images: HashSet<String> = match client.call(&Request::ListImages)? {
        Response::Images(list) => list
            .into_iter()
            .flat_map(|i| [i.reference, format!("{}:{}", i.repository, i.tag)])
            .collect(),
        other => return unexpected(other),
    };
    for svc in &ordered {
        if let Some(b) = &svc.build {
            let have_image = existing_images.contains(&svc.image)
                || graviton_core::ImageReference::parse(&svc.image)
                    .ok()
                    .is_some_and(|r| existing_images.contains(&r.to_string()));
            if force_build || (!no_build && !have_image) {
                eprintln!("[{}] Building {}…", svc.name, svc.image);
                build_one(client, target, svc, b, false)?;
            }
        }
    }

    // Start containers in topological dependency order.
    let existing: HashMap<String, ContainerSummary> = list_containers(client)?
        .into_iter()
        .map(|c| (c.name.clone(), c))
        .collect();

    let mut started_ids: Vec<(String, String)> = Vec::new(); // (service_name, container_id)
    for svc in &ordered {
        let mut reuse_existing = existing.get(&svc.container_name).cloned();
        if force_recreate {
            if let Some(c) = reuse_existing.take() {
                if c.status == ContainerStatus::Running {
                    let _ = client.call(&Request::Stop { id: c.id.clone() });
                }
                client.call(&Request::Remove { id: c.id })?;
            }
        }

        let container = match reuse_existing {
            Some(c) if c.status == ContainerStatus::Running => {
                eprintln!("  ✓ {:<16} {} (already running)", svc.name, c.name);
                c
            }
            Some(c) => {
                match client.call(&Request::Start { id: c.id.clone() })? {
                    Response::Container(started) => {
                        eprintln!("  ▶ {:<16} {} (started)", svc.name, started.name);
                        started
                    }
                    other => return unexpected(other),
                }
            }
            None => {
                let spec = service_to_run_spec(proj, svc);
                match client.call(&Request::Run(spec))? {
                    Response::Container(created) => {
                        let ip = created.ip_address.as_deref().unwrap_or("starting");
                        eprintln!("  ▶ {:<16} {} ({ip})", svc.name, created.name);
                        created
                    }
                    other => return unexpected(other),
                }
            }
        };

        started_ids.push((svc.name.clone(), container.id.clone()));

        if wait_complete.contains(svc.name.as_str()) {
            eprintln!("  … waiting for {} to complete…", svc.name);
            let code = wait_for_exit(client, &container.id)?;
            if code != 0 {
                bail!("service `{}` exited with code {code} (dependent services not started)", svc.name);
            }
            eprintln!("  ✓ {:<16} completed (exit 0)", svc.name);
        }
    }

    if detach {
        println!("Stack `{}` is up ({} service{}).", proj.name, ordered.len(), if ordered.len() == 1 { "" } else { "s" });
        return Ok(());
    }

    // Foreground mode: stream multiplexed logs until Ctrl-C (then stop stack cleanly) or all exit.
    install_sigint_handler();
    eprintln!("Attaching to {} (press Ctrl-C to stop)…", proj.name);
    stream_multiplexed_logs(client, &started_ids, None, true, true)?;

    if INTERRUPTED.load(Ordering::SeqCst) {
        eprintln!("\nGracefully stopping stack `{}`…", proj.name);
        for (svc_name, id) in started_ids.iter().rev() {
            if client.call(&Request::Stop { id: id.clone() }).is_ok() {
                eprintln!("  ■ {svc_name} stopped");
            }
        }
    }
    Ok(())
}

fn down(client: &mut Client, args: &ComposeArgs, remove_volumes: bool) -> Result<()> {
    let loaded = load_project(args).ok();
    let project_name = match (&loaded, &args.project_name) {
        (Some(p), _) => p.name.clone(),
        (None, Some(n)) => graviton_core::compose::sanitize_project_name(n),
        (None, None) => {
            // Surface the real load error if no -p was given.
            let _ = load_project(args)?;
            unreachable!()
        }
    };

    let all = list_containers(client)?;
    let expected_names: HashSet<String> = loaded
        .as_ref()
        .map(|p| p.services.values().map(|s| s.container_name.clone()).collect())
        .unwrap_or_default();

    let mut mine: Vec<ContainerSummary> = all
        .into_iter()
        .filter(|c| c.project.as_deref() == Some(project_name.as_str()) || expected_names.contains(&c.name))
        .collect();

    // Order in reverse dependency order if compose file was loaded.
    if let Some(proj) = &loaded {
        let rev_order: HashMap<String, usize> = proj
            .shutdown_order(&[], &args.profile)
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(idx, s)| (s.container_name.clone(), idx))
            .collect();
        mine.sort_by_key(|c| rev_order.get(&c.name).copied().unwrap_or(usize::MAX));
    }

    if mine.is_empty() {
        eprintln!("No containers found for stack `{project_name}`.");
    } else {
        for c in mine {
            if c.status == ContainerStatus::Running {
                client.call(&Request::Stop { id: c.id.clone() })?;
                eprintln!("  ■ stopped {}", c.name);
            }
            client.call(&Request::Remove { id: c.id })?;
            eprintln!("  ✗ removed {}", c.name);
        }
    }

    if remove_volumes {
        if let Some(proj) = &loaded {
            for vol in proj.owned_volume_names() {
                match client.call(&Request::RemoveVolume { name: vol.clone() }) {
                    Ok(Response::VolumeRemoved { name, .. }) => eprintln!("  ✗ removed volume {name}"),
                    Ok(_) => {}
                    Err(err) if err.to_string().contains("no volume") => {}
                    Err(err) => eprintln!("  ! volume {vol}: {err}"),
                }
            }
        }
    }
    Ok(())
}

fn ps(client: &mut Client, proj: &ComposeProject, include_stopped: bool) -> Result<()> {
    let containers = project_containers(client, proj)?;
    let filtered: Vec<&ContainerSummary> = containers
        .iter()
        .filter(|c| include_stopped || c.status == ContainerStatus::Running)
        .collect();

    println!("{:<16} {:<24} {:<24} {:<14} {:<16} PORTS", "SERVICE", "NAME", "IMAGE", "STATUS", "IP");
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    for c in filtered {
        let svc = c.service.as_deref().unwrap_or("—");
        let status = match (c.status, c.started_at, c.exit_code) {
            (ContainerStatus::Running, Some(t), _) => format!("Up {}", uptime(now.saturating_sub(t))),
            (ContainerStatus::Exited, _, Some(code)) => format!("Exited ({code})"),
            (s, _, _) => format!("{s:?}"),
        };
        let ip = c.ip_address.as_deref().unwrap_or("—");
        let ports = c
            .ports
            .iter()
            .map(|p| {
                let proto = match p.protocol {
                    PortProtocol::Tcp => "",
                    PortProtocol::Udp => "/udp",
                };
                match &p.host_ip {
                    Some(h) => format!("{h}:{}->{}{proto}", p.host_port, p.container_port),
                    None => format!("{}->{}{proto}", p.host_port, p.container_port),
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        println!("{:<16} {:<24} {:<24} {:<14} {:<16} {ports}", svc, c.name, c.image, status, ip);
    }
    Ok(())
}

fn logs(client: &mut Client, proj: &ComposeProject, selected: &[String], tail: Option<usize>, follow: bool) -> Result<()> {
    let filter: HashSet<&str> = selected.iter().map(String::as_str).collect();
    for s in selected {
        if !proj.services.contains_key(s) {
            bail!("unknown service `{s}` in project `{}`", proj.name);
        }
    }
    let containers = project_containers(client, proj)?;
    let targets: Vec<(String, String)> = containers
        .into_iter()
        .filter_map(|c| {
            let svc = c.service.clone().unwrap_or_else(|| c.name.clone());
            if filter.is_empty() || filter.contains(svc.as_str()) {
                Some((svc, c.id))
            } else {
                None
            }
        })
        .collect();
    if targets.is_empty() {
        println!("No containers found for stack `{}`.", proj.name);
        return Ok(());
    }
    if follow {
        install_sigint_handler();
    }
    stream_multiplexed_logs(client, &targets, tail, follow, false)
}

#[derive(Clone, Copy)]
enum Action {
    Stop,
    Start,
    Restart,
}

fn lifecycle_many(
    client: &mut Client,
    proj: &ComposeProject,
    selected: &[String],
    profiles: &[String],
    action: Action,
) -> Result<()> {
    let mut ordered = match action {
        Action::Stop => proj.shutdown_order(selected, profiles)?,
        Action::Start | Action::Restart => proj.startup_order(selected, profiles)?,
    };
    if !selected.is_empty() {
        let wanted: HashSet<&str> = selected.iter().map(String::as_str).collect();
        ordered.retain(|s| wanted.contains(s.name.as_str()));
    }
    let by_name: HashMap<String, ContainerSummary> = project_containers(client, proj)?
        .into_iter()
        .map(|c| (c.name.clone(), c))
        .collect();

    for svc in ordered {
        let Some(c) = by_name.get(&svc.container_name) else {
            continue;
        };
        match action {
            Action::Stop => {
                if c.status == ContainerStatus::Running {
                    client.call(&Request::Stop { id: c.id.clone() })?;
                    println!("{} stopped", c.name);
                }
            }
            Action::Start => {
                if c.status != ContainerStatus::Running {
                    client.call(&Request::Start { id: c.id.clone() })?;
                    println!("{} started", c.name);
                }
            }
            Action::Restart => {
                client.call(&Request::Restart { id: c.id.clone() })?;
                println!("{} restarted", c.name);
            }
        }
    }
    Ok(())
}

fn build_services(
    client: &mut Client,
    target: &Target,
    proj: &ComposeProject,
    selected: &[String],
    profiles: &[String],
    no_cache: bool,
) -> Result<()> {
    let mut ordered = proj.startup_order(selected, profiles)?;
    if !selected.is_empty() {
        let wanted: HashSet<&str> = selected.iter().map(String::as_str).collect();
        ordered.retain(|s| wanted.contains(s.name.as_str()));
    }
    let mut built = 0;
    for svc in ordered {
        if let Some(b) = &svc.build {
            eprintln!("[{}] Building {}…", svc.name, svc.image);
            build_one(client, target, svc, b, no_cache)?;
            built += 1;
        }
    }
    if built == 0 {
        println!("No services in `{}` define a `build:` section.", proj.name);
    }
    Ok(())
}

fn build_one(
    client: &mut Client,
    target: &Target,
    svc: &ComposeService,
    b: &graviton_core::compose::ComposeBuild,
    no_cache: bool,
) -> Result<()> {
    let context = std::fs::canonicalize(&b.context)
        .with_context(|| format!("service `{}` build context {} doesn't exist", svc.name, b.context.display()))?;
    let dockerfile = b.dockerfile.as_ref().map(|df| {
        let p = PathBuf::from(df);
        let full = if p.is_absolute() { p } else { context.join(p) };
        full.display().to_string()
    });
    let build_args: Vec<String> = b.args.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let spec = BuildSpec {
        context: context.display().to_string(),
        dockerfile,
        tag: Some(svc.image.clone()),
        build_args,
        target: b.target.clone(),
        no_cache,
        upload: None,
        secrets: vec![],
        platform: b.platform.clone().or_else(|| svc.platform.clone()),
    };
    run_build_spec(client, target, &context, spec)
}

fn service_to_run_spec(proj: &ComposeProject, svc: &ComposeService) -> RunSpec {
    let env: Vec<String> = svc.environment.iter().map(|(k, v)| format!("{k}={v}")).collect();
    RunSpec {
        image: svc.image.clone(),
        name: Some(svc.container_name.clone()),
        command: svc.command.clone(),
        env,
        cpus: svc.cpus,
        memory_mib: svc.memory_mib,
        ports: svc.ports.clone(),
        volumes: svc.volumes.clone(),
        restart: svc.restart,
        tty: false,
        rows: 0,
        cols: 0,
        auto_remove: false,
        platform: svc.platform.clone(),
        entrypoint: svc.entrypoint.clone(),
        working_dir: svc.working_dir.clone(),
        user: svc.user.clone(),
        hostname: svc.hostname.clone().or_else(|| Some(svc.name.clone())),
        project: Some(proj.name.clone()),
        service: Some(svc.name.clone()),
        labels: svc.labels.clone(),
        dns_aliases: svc.dns_aliases.clone(),
    }
}

fn list_containers(client: &mut Client) -> Result<Vec<ContainerSummary>> {
    match client.call(&Request::ListContainers)? {
        Response::Containers(list) => Ok(list),
        other => {
            unexpected(other)?;
            unreachable!()
        }
    }
}

fn project_containers(client: &mut Client, proj: &ComposeProject) -> Result<Vec<ContainerSummary>> {
    let expected: HashSet<&str> = proj.services.values().map(|s| s.container_name.as_str()).collect();
    let mut list: Vec<ContainerSummary> = list_containers(client)?
        .into_iter()
        .filter(|c| c.project.as_deref() == Some(proj.name.as_str()) || expected.contains(c.name.as_str()))
        .collect();
    list.sort_by(|a, b| a.service.cmp(&b.service).then_with(|| a.name.cmp(&b.name)));
    Ok(list)
}

fn wait_for_exit(client: &mut Client, id: &str) -> Result<i32> {
    loop {
        let list = list_containers(client)?;
        match list.into_iter().find(|c| c.id == id) {
            Some(c) if c.status == ContainerStatus::Exited => return Ok(c.exit_code.unwrap_or(0)),
            Some(_) => std::thread::sleep(Duration::from_millis(120)),
            None => bail!("container {id} disappeared"),
        }
    }
}

fn stream_multiplexed_logs(
    client: &mut Client,
    targets: &[(String, String)],
    tail: Option<usize>,
    follow: bool,
    stop_when_all_exited: bool,
) -> Result<()> {
    const COLORS: [&str; 6] = ["\x1b[36m", "\x1b[33m", "\x1b[35m", "\x1b[32m", "\x1b[34m", "\x1b[96m"];
    let width = targets.iter().map(|(name, _)| name.len()).max().unwrap_or(4).min(20);
    let mut seen: HashMap<String, u64> = HashMap::new();
    let mut first_pass = true;

    loop {
        if follow && INTERRUPTED.load(Ordering::SeqCst) {
            break;
        }
        let mut out = std::io::stdout().lock();
        for (idx, (svc, id)) in targets.iter().enumerate() {
            let color = COLORS[idx % COLORS.len()];
            let req_tail = if first_pass { tail } else { None };
            if let Ok(Response::Logs(lines)) = client.call(&Request::Logs { id: id.clone(), tail: req_tail }) {
                let from = *seen.get(id).unwrap_or(&0);
                for l in lines.into_iter().filter(|l| l.seq > from) {
                    seen.insert(id.clone(), l.seq);
                    match l.stream {
                        LogStream::System => {
                            let _ = writeln!(out, "{color}{svc:<width$}\x1b[0m \x1b[2m| {}\x1b[0m", l.message);
                        }
                        _ => {
                            let _ = writeln!(out, "{color}{svc:<width$}\x1b[0m | {}", l.message);
                        }
                    }
                }
            }
        }
        let _ = out.flush();
        first_pass = false;

        if !follow {
            break;
        }
        if stop_when_all_exited {
            let all = list_containers(client)?;
            let any_running = targets
                .iter()
                .any(|(_, id)| all.iter().any(|c| c.id == *id && c.status != ContainerStatus::Exited));
            if !any_running {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    Ok(())
}
