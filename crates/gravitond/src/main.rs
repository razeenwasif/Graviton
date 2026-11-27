//! `gravitond`: the Graviton engine daemon.
//!
//! Listens on a Unix socket and serves the newline-delimited JSON control API
//! defined in `graviton-proto`. Clients are the `grav` CLI and Graviton.app.
//! Containers run in their own guest VMs (see `runtime.rs`); `--demo` serves
//! the sample containers from the design mockups instead.

mod archive;
mod auth;
mod boot_check;
mod build;
mod cleanup;
mod docker_api;
mod erofs;
mod events;
mod persistence;
mod push;
mod remote;
mod runtime;
mod snapshot;
mod switch;
mod upload;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use graviton_core::{Store, paths};
use graviton_proto::{PROTOCOL_VERSION, Request, Response, decode_line, encode_line};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use runtime::Engine;

#[derive(Parser, Debug)]
#[command(version, about = "The Graviton engine daemon")]
struct Args {
    /// Control socket path (default: $GRAVITON_SOCKET or ~/Library/Application Support/Graviton/gravitond.sock)
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Docker-compatible REST API socket path (default: $GRAVITON_DOCKER_SOCKET or <support_dir>/docker.sock)
    #[arg(long)]
    docker_socket: Option<PathBuf>,

    /// Disable the Docker Engine REST API compatibility socket (`docker.sock`).
    #[arg(long)]
    no_docker_socket: bool,

    /// Serve the sample containers from the design mockups instead of real ones.
    #[arg(long)]
    demo: bool,

    /// Also listen for paired phones over TLS on the local network (see `grav pair`).
    #[arg(long)]
    remote: bool,

    /// TCP port for --remote.
    #[arg(long, default_value_t = graviton_proto::REMOTE_PORT)]
    remote_port: u16,

    /// IP address for --remote to bind to (e.g. a Tailscale IP or 127.0.0.1).
    #[arg(long, default_value = "0.0.0.0")]
    remote_bind: std::net::IpAddr,

    /// Don't advertise the --remote endpoint over Bonjour (_graviton._tcp).
    #[arg(long)]
    no_bonjour: bool,

    /// Override the host name shown to paired devices instead of the Mac's ComputerName.
    #[arg(long)]
    remote_name: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Boot a guest VM, run one command in it through the agent, and power it off.
    BootCheck(boot_check::BootCheckArgs),
}

/// What answers requests: real VMs, or the sample data.
#[derive(Clone)]
pub(crate) enum Backend {
    Engine(Arc<Engine>),
    Demo(Arc<Mutex<Store>>),
}

impl Backend {
    fn store(&self) -> &Arc<Mutex<Store>> {
        match self {
            Backend::Engine(e) => &e.store,
            Backend::Demo(s) => s,
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(Command::BootCheck(check)) = args.command {
        return boot_check::run(check);
    }
    tokio::runtime::Runtime::new()?.block_on(serve_forever(args))
}

async fn serve_forever(args: Args) -> Result<()> {
    let explicit_socket = args.socket.is_some();
    let socket = args.socket.unwrap_or_else(paths::socket_path);
    let docker_socket = if args.no_docker_socket {
        None
    } else if let Some(p) = args.docker_socket {
        Some(p)
    } else if explicit_socket {
        // Keep test/custom `--socket` daemons isolated from the user's primary `docker.sock`.
        Some(socket.parent().unwrap_or(std::path::Path::new("/tmp")).join("docker.sock"))
    } else {
        Some(paths::docker_socket_path())
    };

    if let Some(dir) = socket.parent() {
        tokio::fs::create_dir_all(dir).await.with_context(|| format!("creating {}", dir.display()))?;
    }
    // A socket left behind by a crashed daemon blocks bind(); a live daemon answers instead.
    if socket.exists() {
        if UnixStream::connect(&socket).await.is_ok() {
            anyhow::bail!("another gravitond is already listening on {}", socket.display());
        }
        tokio::fs::remove_file(&socket).await.ok();
    }
    if let Some(ds) = &docker_socket {
        if let Some(dir) = ds.parent() {
            tokio::fs::create_dir_all(dir).await.with_context(|| format!("creating {}", dir.display()))?;
        }
        if ds.exists() {
            if UnixStream::connect(ds).await.is_ok() {
                anyhow::bail!("another daemon is already listening on {}", ds.display());
            }
            tokio::fs::remove_file(ds).await.ok();
        }
    }

    let backend = if args.demo {
        Backend::Demo(Arc::new(Mutex::new(Store::demo())))
    } else {
        Backend::Engine(Engine::open()?)
    };

    let remote = if args.remote {
        let remote = remote::Remote::open_with(args.remote_port, args.remote_bind, !args.no_bonjour, args.remote_name.clone())?;
        // Push alerts for failures and builds, to phones that registered for them.
        let push = push::Push::new(Arc::clone(&remote), tokio::runtime::Handle::current());
        if let Backend::Engine(engine) = &backend {
            let p = Arc::clone(&push);
            engine.events.subscribe(move |event| p.notify(event));
        }
        let _ = remote.push.set(push);
        let serving = Arc::clone(&remote);
        let for_remote = backend.clone();
        tokio::spawn(async move {
            if let Err(err) = serving.serve(for_remote).await {
                eprintln!("remote endpoint stopped: {err:#}");
            }
        });
        Some(remote)
    } else {
        None
    };

    let listener = UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    restrict_permissions(&socket)?;
    eprintln!(
        "gravitond {} listening on {}{}",
        env!("CARGO_PKG_VERSION"),
        socket.display(),
        if args.demo { " (demo data)" } else { "" }
    );

    let docker_endpoint = if let Some(ds) = &docker_socket {
        let dl = UnixListener::bind(ds).with_context(|| format!("binding {}", ds.display()))?;
        restrict_permissions(ds)?;
        let state = docker_api::DockerState::new(backend.clone());
        eprintln!("Docker API listening on {}", ds.display());
        Some((dl, ds.clone(), state))
    } else {
        None
    };

    // launchd stops agents with SIGTERM; Ctrl-C sends SIGINT.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            _ = terminate.recv() => break,
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let backend = backend.clone();
                let remote = remote.clone();
                tokio::spawn(async move {
                    if let Err(err) = serve(stream, backend, remote).await {
                        eprintln!("connection error: {err:#}");
                    }
                });
            }
            accepted = async {
                match &docker_endpoint {
                    Some((dl, _, _)) => dl.accept().await.map(Some),
                    None => std::future::pending().await,
                }
            } => {
                if let Some((stream, _)) = accepted? {
                    if let Some((_, _, ds)) = &docker_endpoint {
                        let ds = Arc::clone(ds);
                        tokio::spawn(async move {
                            if let Err(err) = docker_api::serve_connection(stream, ds).await {
                                eprintln!("docker api connection error: {err:#}");
                            }
                        });
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    if let Backend::Engine(engine) = &backend {
        let engine = Arc::clone(engine);
        let _ = tokio::task::spawn_blocking(move || engine.shutdown()).await;
    }
    tokio::fs::remove_file(&socket).await.ok();
    if let Some((_, ds, _)) = docker_endpoint {
        tokio::fs::remove_file(&ds).await.ok();
    }
    eprintln!("gravitond stopped");
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod {}", path.display()))
}

async fn serve(stream: UnixStream, backend: Backend, remote: Option<Arc<remote::Remote>>) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    loop {
        let Some(line) = lines.next_line().await? else { return Ok(()) };
        if line.trim().is_empty() {
            continue;
        }
        let request = match decode_line::<Request>(line.as_bytes()) {
            Ok(request) => request,
            Err(err) => {
                write.write_all(&encode_line(&Response::Error { message: format!("bad request: {err}") })).await?;
                continue;
            }
        };
        // Pairing is managed from the Mac itself.
        let local = match &request {
            Request::PushSetup { .. } | Request::PushStatus | Request::PushTest => Some(match remote.as_ref().and_then(|r| r.push.get()) {
                None => Response::Error { message: "push alerts need gravitond --remote (phones pair through it)".into() },
                Some(push) => match &request {
                    Request::PushSetup { key_id, team_id, p8 } => result(push.setup(key_id, team_id, p8).map(Response::Push)),
                    Request::PushStatus => Response::Push(push.info()),
                    Request::PushTest => {
                        let results = push.send_all("Graviton", "Push alerts from this Mac are working.", None).await;
                        let mut info = push.info();
                        info.note = Some(if results.is_empty() {
                            "no phone has registered for alerts yet: open Graviton on it and allow notifications".into()
                        } else {
                            results
                                .iter()
                                .map(|(device, r)| match r {
                                    Ok(()) => format!("{device}: sent"),
                                    Err(e) => format!("{device}: {e:#}"),
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        });
                        Response::Push(info)
                    }
                    _ => unreachable!(),
                },
            }),
            Request::RegisterPush { .. } => Some(Response::Error { message: "phones register for alerts over the remote endpoint".into() }),
            Request::PairBegin | Request::ListDevices | Request::RevokeDevice { .. } => Some(match &remote {
                None => Response::Error { message: "remote access is off; start gravitond with --remote to pair a phone".into() },
                Some(r) => match &request {
                    Request::PairBegin => result(r.begin_pairing().map(Response::Pairing)),
                    Request::ListDevices => Response::Devices(r.devices()),
                    Request::RevokeDevice { id } => result(r.revoke(id).map(|id| Response::Revoked { id })),
                    _ => unreachable!(),
                },
            }),
            Request::Pair { .. } | Request::Auth { .. } => {
                Some(Response::Error { message: "pairing happens over the remote endpoint, not the local socket".into() })
            }
            _ => None,
        };
        if let Some(response) = local {
            write.write_all(&encode_line(&response)).await?;
            continue;
        }
        if let (Request::Pull { reference, progress: true, platform }, Backend::Engine(engine)) = (&request, &backend) {
            pull_stream(Arc::clone(engine), reference.clone(), platform.clone(), &mut write).await?;
            continue;
        }
        if let (Request::PushImage { reference, progress: true }, Backend::Engine(engine)) = (&request, &backend) {
            push_stream(Arc::clone(engine), reference.clone(), &mut write).await?;
            continue;
        }
        if let (Request::Build(spec), Backend::Engine(engine)) = (&request, &backend) {
            build_stream(Arc::clone(engine), spec.clone(), lines.get_mut(), &mut write).await?;
            continue;
        }
        if let (Request::Run(spec), Backend::Engine(engine)) = (&request, &backend) {
            if spec.tty {
                let reader = lines.into_inner();
                return run_tty(Arc::clone(engine), spec.as_ref().clone(), reader, write).await;
            }
        }
        if let (Request::GetArchive { id, path }, Backend::Engine(engine)) = (&request, &backend) {
            return get_archive_stream(Arc::clone(engine), id.clone(), path.clone(), write).await;
        }
        if let (Request::PutArchive { id, path, size }, Backend::Engine(engine)) = (&request, &backend) {
            put_archive_stream(Arc::clone(engine), id.clone(), path.clone(), *size, lines.get_mut(), &mut write).await?;
            continue;
        }
        if let (Request::SaveImage { references }, Backend::Engine(engine)) = (&request, &backend) {
            return save_image_stream(Arc::clone(engine), references.clone(), write).await;
        }
        if let (Request::LoadImage { size }, Backend::Engine(engine)) = (&request, &backend) {
            load_image_stream(Arc::clone(engine), *size, lines.get_mut(), &mut write).await?;
            continue;
        }
        if let (Request::Exec(spec), Backend::Engine(engine)) = (&request, &backend) {
            if spec.tty {
                // The connection becomes a raw terminal stream.
                let reader = lines.into_inner();
                return exec_tty(Arc::clone(engine), spec.clone(), reader, write).await;
            }
            exec_stream(Arc::clone(engine), spec.clone(), &mut write).await?;
            continue;
        }
        if let Request::Logs { id, tail, follow: true, since_ms, until_ms } = &request {
            if !logs_stream(&backend, id.clone(), *tail, *since_ms, *until_ms, lines.get_mut(), &mut write).await? {
                return Ok(());
            }
            continue;
        }
        if let Request::StatsStream { ids, all, follow } = &request {
            if !stats_stream(&backend, ids.clone(), *all, *follow, lines.get_mut(), &mut write).await? {
                return Ok(());
            }
            continue;
        }
        let response = handle(request, &backend).await;
        write.write_all(&encode_line(&response)).await?;
    }
}

async fn stream_blocking<F>(write: &mut (impl tokio::io::AsyncWrite + Unpin), work: F) -> Result<()>
where
    F: FnOnce(tokio::sync::mpsc::UnboundedSender<Response>) + Send + 'static,
{
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::task::spawn_blocking(move || work(tx));
    let mut disconnected = false;
    while let Some(response) = rx.recv().await {
        if write.write_all(&encode_line(&response)).await.is_err() || write.flush().await.is_err() {
            disconnected = true;
            break; // client closed the sheet/terminal; operation carries on in the background
        }
    }
    drop(rx);
    if !disconnected {
        let _ = task.await;
    }
    Ok(())
}

/// `grav pull` (and app Pull sheets): stream per-layer progress, then the image.
pub(crate) async fn pull_stream(
    engine: Arc<Engine>,
    reference: String,
    platform: Option<String>,
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<()> {
    stream_blocking(write, move |tx| {
        let updates = tx.clone();
        let result = engine.pull_with_progress(&reference, platform.as_deref(), |progress| {
            let _ = updates.send(Response::PullProgress(progress));
        });
        let _ = tx.send(match result {
            Ok(image) => Response::Image(image),
            Err(err) => Response::Error { message: format!("{err:#}") },
        });
    })
    .await
}

/// `grav push IMAGE`: stream per-blob upload progress, then `ImagePushed`.
pub(crate) async fn push_stream(
    engine: Arc<Engine>,
    reference: String,
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<()> {
    stream_blocking(write, move |tx| {
        let updates = tx.clone();
        let result = engine.push_with_progress(&reference, None, |progress| {
            let _ = updates.send(Response::ImagePushProgress(progress));
        });
        let _ = tx.send(match result {
            Ok(res) => Response::ImagePushed(res),
            Err(err) => Response::Error { message: format!("{err:#}") },
        });
    })
    .await
}

/// `grav build`: stream the build's log lines, then the image. With an
/// upload, the build folder comes next on `read`.
pub(crate) async fn build_stream(
    engine: Arc<Engine>,
    mut spec: graviton_proto::BuildSpec,
    read: &mut (impl tokio::io::AsyncRead + Unpin),
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<()> {
    let upload = match spec.upload {
        None => None,
        Some(size) => match upload::receive(read, size).await {
            Ok(up) => {
                let line = format!("Received the build folder ({} files, {:.1} MB)", up.files, up.bytes as f64 / 1048576.0);
                write.write_all(&encode_line(&Response::BuildLog { line })).await?;
                spec.context = up.dir.display().to_string();
                if spec.dockerfile.is_none() && up.dir.join(graviton_proto::UPLOADED_DOCKERFILE).is_file() {
                    spec.dockerfile = Some(graviton_proto::UPLOADED_DOCKERFILE.into());
                }
                Some(up)
            }
            Err(err) => {
                // The rest of the upload can't be told apart from requests: hang up.
                write.write_all(&encode_line(&Response::Error { message: format!("{err:#}") })).await?;
                anyhow::bail!("build upload failed: {err:#}");
            }
        },
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::task::spawn_blocking(move || {
        let lines = tx.clone();
        let result = build::build(&engine, &spec, &mut |line| {
            eprintln!("build: {line}");
            let _ = lines.send(Response::BuildLog { line });
        });
        let tag = spec.tag.clone().unwrap_or_else(|| "image".into());
        match &result {
            Ok(image) => {
                let name = format!("{}:{}", image.repository, image.tag);
                engine.events.record(graviton_proto::EventKind::Built, None, Some(&name), format!("Built {name}"));
            }
            Err(err) => {
                let why = format!("{err:#}");
                let short = why.lines().next().unwrap_or("").chars().take(160).collect::<String>();
                engine.events.record(graviton_proto::EventKind::BuildFailed, None, Some(&tag), format!("Build of {tag} failed: {short}"));
            }
        }
        let _ = tx.send(match result {
            Ok(image) => Response::Image(image),
            Err(err) => Response::Error { message: format!("{err:#}") },
        });
    });
    while let Some(response) = rx.recv().await {
        if write.write_all(&encode_line(&response)).await.is_err() {
            break; // the client went away; the build finishes on its own
        }
    }
    let _ = task.await;
    drop(upload); // deletes the received folder
    Ok(())
}

/// `grav exec` without a TTY: stream output lines, then the exit code.
pub(crate) async fn exec_stream(engine: Arc<Engine>, spec: graviton_proto::ExecSpec, write: &mut (impl tokio::io::AsyncWrite + Unpin)) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let task = tokio::task::spawn_blocking(move || engine.exec_stream(&spec, tx));
    while let Some(response) = rx.recv().await {
        write.write_all(&encode_line(&response)).await?;
    }
    if let Ok(Err(err)) = task.await {
        write.write_all(&encode_line(&Response::Error { message: format!("{err:#}") })).await?;
    }
    Ok(())
}

/// `grav exec -it` and the apps' terminals: answer `attached`, then splice the
/// client and the guest's terminal. Both ends speak terminal frames
/// (`graviton_proto::frame`), which pass through untouched.
pub(crate) async fn exec_tty<R, W>(engine: Arc<Engine>, spec: graviton_proto::ExecSpec, mut client_in: R, mut client_out: W) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let guest = match tokio::task::spawn_blocking(move || engine.exec_tty(&spec)).await? {
        Ok(fd) => fd,
        Err(err) => {
            client_out.write_all(&encode_line(&Response::Error { message: format!("{err:#}") })).await?;
            return Ok(());
        }
    };
    client_out.write_all(&encode_line(&Response::Attached)).await?;
    let guest = std::os::unix::net::UnixStream::from(guest);
    guest.set_nonblocking(true)?;
    let guest = UnixStream::from_std(guest)?;
    let (mut guest_in, mut guest_out) = guest.into_split();

    let keys = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut client_in, &mut guest_out).await;
        let _ = guest_out.shutdown().await; // tells the agent to hang up the session
    });
    let _ = tokio::io::copy(&mut guest_in, &mut client_out).await;
    keys.abort();
    let _ = client_out.shutdown().await;
    Ok(())
}

/// `grav run -it`: boot a container with its main process on a PTY, splice
/// terminal frames, and wait for the VM to stop before delivering `Frame::Exit`.
pub(crate) async fn run_tty<R, W>(engine: Arc<Engine>, spec: graviton_proto::RunSpec, mut client_in: R, mut client_out: W) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use graviton_proto::frame::{self, Decoder, Frame};
    use tokio::io::AsyncReadExt;

    let attached = match tokio::task::spawn_blocking(move || engine.run_tty(spec)).await? {
        Ok(a) => a,
        Err(err) => {
            client_out.write_all(&encode_line(&Response::Error { message: format!("{err:#}") })).await?;
            let _ = client_out.flush().await;
            return Ok(());
        }
    };
    client_out.write_all(&encode_line(&Response::Attached)).await?;
    client_out.flush().await?;

    let guest = std::os::unix::net::UnixStream::from(attached.guest_fd);
    guest.set_nonblocking(true)?;
    let guest = UnixStream::from_std(guest)?;
    let (mut guest_in, mut guest_out) = guest.into_split();

    let keys = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut client_in, &mut guest_out).await;
        let _ = guest_out.shutdown().await;
    });

    let mut decoder = Decoder::default();
    let mut buf = [0u8; 16 * 1024];
    let mut exit_code: Option<i32> = None;
    'read: loop {
        match guest_in.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => decoder.push(&buf[..n]),
        }
        loop {
            match decoder.next_frame() {
                Ok(Some(Frame::Data(bytes))) => {
                    if client_out.write_all(&frame::data_frame(frame::DATA, &bytes)).await.is_err() {
                        break 'read;
                    }
                    let _ = client_out.flush().await;
                }
                Ok(Some(Frame::Exit(code))) => {
                    exit_code = Some(code);
                    break 'read;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => break 'read,
            }
        }
    }

    keys.abort();
    let _ = attached.exit_tx.send(exit_code.unwrap_or(129));
    let _ = attached.stopped_rx.await;
    if let Some(code) = exit_code {
        let _ = client_out.write_all(&Frame::Exit(code).encode()).await;
        let _ = client_out.flush().await;
    }
    let _ = client_out.shutdown().await;
    Ok(())
}

/// `grav cp CONTAINER:PATH LOCAL` and Mac Files tab downloads: answer `Archive`
/// and then stream raw tar bytes until EOF.
pub(crate) async fn get_archive_stream<W>(engine: Arc<Engine>, id: String, path: String, mut write: W) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let (is_dir, name, fd) = match tokio::task::spawn_blocking(move || engine.get_archive_open(&id, &path)).await? {
        Ok(res) => res,
        Err(err) => {
            write.write_all(&encode_line(&Response::Error { message: format!("{err:#}") })).await?;
            let _ = write.flush().await;
            return Ok(());
        }
    };
    write.write_all(&encode_line(&Response::Archive { is_dir, name })).await?;
    write.flush().await?;
    let guest = std::os::unix::net::UnixStream::from(fd);
    guest.set_nonblocking(true)?;
    let mut guest = UnixStream::from_std(guest)?;
    let _ = tokio::io::copy(&mut guest, &mut write).await;
    let _ = write.flush().await;
    let _ = write.shutdown().await;
    Ok(())
}

/// `grav cp LOCAL CONTAINER:PATH` and Mac Files tab uploads: stream `size` raw
/// tar bytes from `read` into the running container and answer `Copied`.
pub(crate) async fn put_archive_stream(
    engine: Arc<Engine>,
    id: String,
    path: String,
    size: u64,
    read: &mut (impl tokio::io::AsyncRead + Unpin),
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<()> {
    use tokio::io::AsyncReadExt;

    if size > upload::MAX_UPLOAD {
        write
            .write_all(&encode_line(&Response::Error {
                message: format!("archive is {} GB; limit is {} GB", size >> 30, upload::MAX_UPLOAD >> 30),
            }))
            .await?;
        anyhow::bail!("archive too large ({size} bytes)");
    }
    let fd = match tokio::task::spawn_blocking(move || engine.put_archive_open(&id, &path, size)).await? {
        Ok(fd) => fd,
        Err(err) => {
            // Remaining raw archive bytes follow on `read`: hang up after telling the client.
            write.write_all(&encode_line(&Response::Error { message: format!("{err:#}") })).await?;
            anyhow::bail!("put_archive rejected: {err:#}");
        }
    };
    let guest = std::os::unix::net::UnixStream::from(fd);
    guest.set_nonblocking(true)?;
    let guest = UnixStream::from_std(guest)?;
    let (guest_in, mut guest_out) = guest.into_split();

    let mut limited = read.take(size);
    let copied = tokio::io::copy(&mut limited, &mut guest_out).await.context("streaming archive to container")?;
    guest_out.flush().await?;
    if copied != size {
        let msg = format!("upload ended early ({copied} of {size} bytes)");
        let _ = write.write_all(&encode_line(&Response::Error { message: msg.clone() })).await;
        anyhow::bail!(msg);
    }

    let mut lines = BufReader::new(guest_in).lines();
    let reply = match lines.next_line().await? {
        Some(line) => match decode_line::<graviton_proto::AgentEvent>(line.as_bytes())? {
            graviton_proto::AgentEvent::Copied { files } => Response::Copied { files },
            graviton_proto::AgentEvent::Error { message } => Response::Error { message },
            other => Response::Error { message: format!("unexpected answer from agent: {other:?}") },
        },
        None => Response::Error { message: "guest agent closed the connection while unpacking".into() },
    };
    write.write_all(&encode_line(&reply)).await?;
    write.flush().await?;
    Ok(())
}

pub(crate) struct SyncChannelWriter(pub(crate) tokio::sync::mpsc::Sender<Vec<u8>>);

impl std::io::Write for SyncChannelWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.0.blocking_send(buf.to_vec()).is_err() {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "client disconnected"));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `grav save`: verify requested images exist, send `ImageArchive`, then stream tar bytes until EOF.
pub(crate) async fn save_image_stream<W>(engine: Arc<Engine>, references: Vec<String>, mut write: W) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let check_engine = Arc::clone(&engine);
    let check_refs = references.clone();
    let images = match tokio::task::spawn_blocking(move || archive::resolve_save_images(&check_engine, &check_refs)).await? {
        Ok(imgs) => imgs,
        Err(err) => {
            write.write_all(&encode_line(&Response::Error { message: format!("{err:#}") })).await?;
            let _ = write.flush().await;
            return Ok(());
        }
    };
    write.write_all(&encode_line(&Response::ImageArchive)).await?;
    write.flush().await?;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let blobs = engine.blobs.clone();
    let writer_task = tokio::task::spawn_blocking(move || archive::save_images_from_store(&blobs, &images, SyncChannelWriter(tx)));
    while let Some(chunk) = rx.recv().await {
        if write.write_all(&chunk).await.is_err() {
            break;
        }
    }
    let _ = write.flush().await;
    let _ = write.shutdown().await;
    if let Ok(Err(err)) = writer_task.await {
        eprintln!("save_image_stream error: {err:#}");
    }
    Ok(())
}

/// `grav load`: receive `size` tar/tar.gz bytes, unpack into BlobStore + ImageIndex, and answer `Images`.
pub(crate) async fn load_image_stream(
    engine: Arc<Engine>,
    size: u64,
    read: &mut (impl tokio::io::AsyncRead + Unpin),
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<()> {
    use tokio::io::AsyncReadExt;

    if size > archive::MAX_IMAGE_LOAD {
        write
            .write_all(&encode_line(&Response::Error {
                message: format!("image archive is {} GB; limit is {} GB", size >> 30, archive::MAX_IMAGE_LOAD >> 30),
            }))
            .await?;
        anyhow::bail!("image archive too large ({size} bytes)");
    }

    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp_path = std::env::temp_dir().join(format!(
        "graviton-load-upload-{}-{}.tar",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _guard = Cleanup(tmp_path.clone());

    let mut out = tokio::fs::File::create(&tmp_path).await?;
    let mut limited = read.take(size);
    let copied = tokio::io::copy(&mut limited, &mut out).await.context("receiving image archive")?;
    out.flush().await?;
    drop(out);
    if copied != size {
        let msg = format!("upload ended early ({copied} of {size} bytes)");
        let _ = write.write_all(&encode_line(&Response::Error { message: msg.clone() })).await;
        anyhow::bail!(msg);
    }

    let load_path = tmp_path.clone();
    let reply = match tokio::task::spawn_blocking(move || archive::load_images_from_path(&engine, &load_path)).await? {
        Ok(list) => Response::Images(list),
        Err(err) => Response::Error { message: format!("{err:#}") },
    };
    write.write_all(&encode_line(&reply)).await?;
    write.flush().await?;
    Ok(())
}

fn is_log_active(status: graviton_proto::ContainerStatus) -> bool {
    use graviton_proto::ContainerStatus;
    matches!(status, ContainerStatus::Created | ContainerStatus::Running | ContainerStatus::Paused)
}

fn drain_after_and_active(
    backend: &Backend,
    id: &str,
    last_seq: u64,
    since_ms: Option<u64>,
    until_ms: Option<u64>,
) -> (Vec<graviton_proto::LogLine>, bool, Option<i32>) {
    let store = backend.store().lock().unwrap();
    let trailing = store.logs_after(id, last_seq, since_ms, until_ms);
    let (active, ec) = match store.container(id) {
        Ok(c) => (is_log_active(c.status), c.exit_code),
        Err(_) => (false, None),
    };
    (trailing, active, ec)
}

async fn emit_log_lines<W>(
    write: &mut W,
    lines: Vec<graviton_proto::LogLine>,
    last_seq: &mut u64,
) -> bool
where
    W: tokio::io::AsyncWrite + Unpin,
{
    for line in lines {
        *last_seq = (*last_seq).max(line.seq);
        if write.write_all(&encode_line(&Response::Log(line))).await.is_err() {
            return false;
        }
    }
    true
}

/// Stream historical and live container logs as `Response::Log` lines followed by `Response::LogEnd`.
/// Returns `Ok(true)` when the stream finished cleanly so further requests can reuse the connection.
pub(crate) async fn logs_stream<R, W>(
    backend: &Backend,
    id: String,
    tail: Option<usize>,
    since_ms: Option<u64>,
    until_ms: Option<u64>,
    read: &mut BufReader<R>,
    write: &mut W,
) -> Result<bool>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use crate::runtime::LogEvent;
    use tokio::io::AsyncBufReadExt;
    use tokio::sync::broadcast::error::RecvError;

    // Subscribe before inspecting Store so no log line or exit transition can fall between them.
    let rx = match backend {
        Backend::Engine(engine) => Some(engine.subscribe_logs()),
        Backend::Demo(_) => None,
    };

    let resolved = {
        let store = backend.store().lock().unwrap();
        store.container(&id).map(|c| {
            let lines = store.logs_filtered(&c.id, tail, since_ms, until_ms).unwrap_or_default();
            let last_seq = store.last_log_seq(&c.id);
            (c.id.clone(), lines, last_seq, c.status, c.exit_code)
        })
    };
    let (canonical_id, initial_lines, mut last_seq, initial_status, initial_exit_code) = match resolved {
        Ok(tuple) => tuple,
        Err(err) => {
            if write.write_all(&encode_line(&Response::Error { message: err.to_string() })).await.is_err()
                || write.flush().await.is_err()
            {
                return Ok(false);
            }
            return Ok(true);
        }
    };

    if !emit_log_lines(write, initial_lines, &mut last_seq).await || write.flush().await.is_err() {
        return Ok(false);
    }

    if !is_log_active(initial_status) || rx.is_none() || until_ms.is_some_and(|u| crate::runtime::now_ms() >= u) {
        let exit_code = (!is_log_active(initial_status)).then_some(initial_exit_code).flatten();
        if write.write_all(&encode_line(&Response::LogEnd { exit_code })).await.is_err() || write.flush().await.is_err() {
            return Ok(false);
        }
        return Ok(true);
    }

    let mut rx = rx.expect("checked Some above");
    loop {
        let until_rem = until_ms.map(|u| u.saturating_sub(crate::runtime::now_ms()));
        if until_rem == Some(0) {
            let trailing = backend.store().lock().unwrap().logs_after(&canonical_id, last_seq, since_ms, until_ms);
            if !emit_log_lines(write, trailing, &mut last_seq).await
                || write.write_all(&encode_line(&Response::LogEnd { exit_code: None })).await.is_err()
                || write.flush().await.is_err()
            {
                return Ok(false);
            }
            return Ok(true);
        }

        let check_eof = read.buffer().is_empty();
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(LogEvent::Line { container_id, line }) if container_id == canonical_id => {
                    if line.seq <= last_seq {
                        continue;
                    }
                    last_seq = line.seq;
                    if until_ms.is_some_and(|u| line.timestamp_ms > u) {
                        if write.write_all(&encode_line(&Response::LogEnd { exit_code: None })).await.is_err()
                            || write.flush().await.is_err()
                        {
                            return Ok(false);
                        }
                        return Ok(true);
                    }
                    if since_ms.is_some_and(|s| line.timestamp_ms < s) {
                        continue;
                    }
                    if write.write_all(&encode_line(&Response::Log(line))).await.is_err() || write.flush().await.is_err() {
                        return Ok(false);
                    }
                }
                Ok(LogEvent::Ended { container_id, exit_code }) if container_id == canonical_id => {
                    let (trailing, still_active, store_exit) =
                        drain_after_and_active(backend, &canonical_id, last_seq, since_ms, until_ms);
                    if !emit_log_lines(write, trailing, &mut last_seq).await {
                        return Ok(false);
                    }
                    if !still_active {
                        if write.write_all(&encode_line(&Response::LogEnd { exit_code: store_exit.or(exit_code) })).await.is_err()
                            || write.flush().await.is_err()
                        {
                            return Ok(false);
                        }
                        return Ok(true);
                    }
                    if write.flush().await.is_err() {
                        return Ok(false);
                    }
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {
                    let (trailing, still_active, store_exit) =
                        drain_after_and_active(backend, &canonical_id, last_seq, since_ms, until_ms);
                    if !emit_log_lines(write, trailing, &mut last_seq).await {
                        return Ok(false);
                    }
                    if !still_active {
                        if write.write_all(&encode_line(&Response::LogEnd { exit_code: store_exit })).await.is_err()
                            || write.flush().await.is_err()
                        {
                            return Ok(false);
                        }
                        return Ok(true);
                    }
                    if write.flush().await.is_err() {
                        return Ok(false);
                    }
                }
                Err(RecvError::Closed) => {
                    if write.write_all(&encode_line(&Response::LogEnd { exit_code: None })).await.is_err()
                        || write.flush().await.is_err()
                    {
                        return Ok(false);
                    }
                    return Ok(true);
                }
            },
            _ = tokio::time::sleep(std::time::Duration::from_millis(until_rem.unwrap_or(0))), if until_rem.is_some() => {
                let trailing = backend.store().lock().unwrap().logs_after(&canonical_id, last_seq, since_ms, until_ms);
                if !emit_log_lines(write, trailing, &mut last_seq).await
                    || write.write_all(&encode_line(&Response::LogEnd { exit_code: None })).await.is_err()
                    || write.flush().await.is_err()
                {
                    return Ok(false);
                }
                return Ok(true);
            }
            res = read.fill_buf(), if check_eof => match res {
                Ok([]) => return Ok(false),
                Err(_) => return Ok(false),
                Ok(_) => {}
            }
        }
    }
}

fn demo_stats_rows(store: &Store, ids: &[String], all: bool) -> Result<Vec<graviton_proto::ContainerStatRow>> {
    let selected: Vec<graviton_proto::ContainerSummary> = if !ids.is_empty() {
        ids.iter()
            .map(|k| store.container(k).cloned().map_err(anyhow::Error::from))
            .collect::<Result<Vec<_>>>()?
    } else {
        store
            .containers()
            .iter()
            .filter(|c| all || matches!(c.status, graviton_proto::ContainerStatus::Running | graviton_proto::ContainerStatus::Paused))
            .cloned()
            .collect()
    };
    let now = runtime::now_ms();
    Ok(selected
        .into_iter()
        .map(|c| {
            let active = matches!(c.status, graviton_proto::ContainerStatus::Running | graviton_proto::ContainerStatus::Paused);
            graviton_proto::ContainerStatRow {
                id: c.id,
                name: c.name,
                status: c.status,
                cpus: 2,
                cpu_percent: if active { c.cpu_percent.unwrap_or(0.0) } else { 0.0 },
                memory_bytes: if active { c.memory_bytes.unwrap_or(0) } else { 0 },
                memory_limit_bytes: c.memory_limit_bytes.unwrap_or(1024 << 20),
                net_rx_bytes: 0,
                net_tx_bytes: 0,
                pids: if active { 2 } else { 0 },
                timestamp_ms: now,
            }
        })
        .collect())
}

/// Stream resource stats snapshots (`Response::StatsSnapshot`) across one or more containers.
/// Returns `Ok(true)` when finished cleanly so the caller may reuse the connection.
pub(crate) async fn stats_stream<R, W>(
    backend: &Backend,
    ids: Vec<String>,
    all: bool,
    follow: bool,
    read: &mut BufReader<R>,
    write: &mut W,
) -> Result<bool>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use crate::runtime::StatsEvent;
    use tokio::io::AsyncBufReadExt;
    use tokio::sync::broadcast::error::RecvError;

    let (mut rx, initial_res) = match backend {
        Backend::Demo(store) => {
            let s = store.lock().unwrap();
            (None, demo_stats_rows(&s, &ids, all))
        }
        Backend::Engine(engine) => {
            let rx = engine.subscribe_stats();
            let res = engine.stats_rows(&ids, all);
            (Some(rx), res)
        }
    };

    let initial_rows = match initial_res {
        Ok(rows) => rows,
        Err(err) => {
            if write.write_all(&encode_line(&Response::Error { message: err.to_string() })).await.is_err()
                || write.flush().await.is_err()
            {
                return Ok(false);
            }
            return Ok(true);
        }
    };
    if write.write_all(&encode_line(&Response::StatsSnapshot(initial_rows))).await.is_err()
        || write.flush().await.is_err()
    {
        return Ok(false);
    }

    let (Backend::Engine(engine), Some(rx)) = (backend, &mut rx) else {
        return Ok(true);
    };
    if !follow {
        return Ok(true);
    }

    let tracked: Option<std::collections::HashSet<String>> = (!ids.is_empty()).then(|| {
        let store = engine.store.lock().unwrap();
        ids.iter()
            .filter_map(|k| store.container(k).ok().map(|c| c.id.clone()))
            .collect()
    });
    let matches_event = |cid: &str| tracked.as_ref().is_none_or(|set| set.contains(cid));

    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await;

    loop {
        let check_eof = read.buffer().is_empty();
        let should_emit = tokio::select! {
            res = read.fill_buf(), if check_eof => match res {
                Ok([]) | Err(_) => return Ok(false),
                Ok(_) => false,
            },
            ev = rx.recv() => match ev {
                Ok(StatsEvent::Sample { container_id, .. }) | Ok(StatsEvent::Stopped { container_id }) => {
                    let hit = matches_event(&container_id);
                    let mut extra_hit = false;
                    while let Ok(more) = rx.try_recv() {
                        match more {
                            StatsEvent::Sample { container_id, .. } | StatsEvent::Stopped { container_id } => {
                                extra_hit |= matches_event(&container_id);
                            }
                        }
                    }
                    hit || extra_hit
                }
                Err(RecvError::Lagged(_)) => true,
                Err(RecvError::Closed) => return Ok(true),
            },
            _ = ticker.tick() => true,
        };
        if should_emit {
            let Ok(rows) = engine.stats_rows(&ids, all) else {
                continue;
            };
            if write.write_all(&encode_line(&Response::StatsSnapshot(rows))).await.is_err()
                || write.flush().await.is_err()
            {
                return Ok(false);
            }
        }
    }
}

pub(crate) async fn handle(request: Request, backend: &Backend) -> Response {
    // Reads answer straight from the store.
    {
        let store = backend.store().lock().unwrap();
        match &request {
            Request::Ping => return Response::Pong { protocol: PROTOCOL_VERSION },
            Request::ListContainers => return Response::Containers(store.containers().to_vec()),
            Request::ListImages => return Response::Images(store.images().to_vec()),
            Request::Logs { id, tail, since_ms, until_ms, .. } => {
                return result(store.logs_filtered(id, *tail, *since_ms, *until_ms).map(Response::Logs));
            }
            Request::EngineInfo if matches!(backend, Backend::Demo(_)) => return Response::Engine(store.engine_info()),
            _ => {}
        }
    }

    match backend {
        Backend::Demo(store) => {
            let mut store = store.lock().unwrap();
            match request {
                Request::Start { id } => result(store.start(&id).map(Response::Container)),
                Request::Stop { id } => result(store.stop(&id).map(Response::Container)),
                Request::Restart { id } => result(store.restart(&id).map(Response::Container)),
                Request::Pause { id } => result(store.pause(&id).map(Response::Container)),
                Request::Unpause { id } => result(store.unpause(&id).map(Response::Container)),
                Request::ListVolumes => Response::Volumes(demo_volumes(&store)),
                Request::NetworkInfo => Response::Network(demo_network(&store)),
                Request::ListNetworks => Response::Networks(demo_network(&store).networks),
                Request::SnapshotStatus => Response::Snapshots(graviton_proto::SnapshotInfo::default()),
                _ => Response::Error { message: "gravitond is serving demo data; restart it without --demo to pull, run, exec and browse files".into() },
            }
        }
        Backend::Engine(engine) => {
            let engine = Arc::clone(engine);
            // Engine calls block on downloads and VM boots.
            let outcome = tokio::task::spawn_blocking(move || -> Result<Response> {
                Ok(match request {
                    Request::EngineInfo => Response::Engine(engine.engine_info()),
                    Request::ListVolumes => Response::Volumes(engine.list_volumes()),
                    Request::CreateVolume { name } => Response::Volume(engine.create_volume(&name)?),
                    Request::RemoveVolume { name } => {
                        let (name, freed_bytes) = engine.remove_volume(&name)?;
                        Response::VolumeRemoved { name, freed_bytes }
                    }
                    Request::NetworkInfo => Response::Network(engine.network_info()),
                    Request::ListNetworks => Response::Networks(engine.list_networks()),
                    Request::CreateNetwork { name, internal, labels } => {
                        Response::NetworkCreated(engine.create_network(&name, internal, labels)?)
                    }
                    Request::RemoveNetwork { name } => Response::NetworkRemoved { name: engine.remove_network(&name)? },
                    Request::ConnectNetwork { network, container } => {
                        let (network, container) = engine.connect_network(&network, &container)?;
                        Response::NetworkUpdated { network, container, connected: true }
                    }
                    Request::DisconnectNetwork { network, container } => {
                        let (network, container) = engine.disconnect_network(&network, &container)?;
                        Response::NetworkUpdated { network, container, connected: false }
                    }
                    Request::Pull { reference, platform, .. } => Response::Image(engine.pull(&reference, platform.as_deref())?),
                    Request::TagImage { source, target } => Response::Image(engine.tag_image(&source, &target)?),
                    Request::PushImage { reference, .. } => Response::ImagePushed(engine.push(&reference)?),
                    Request::Commit(spec) => Response::Image(archive::commit_container(&engine, &spec)?),
                    Request::RegistryLogin { registry, username, password } => {
                        let (registry, username) = engine.auth.login(&registry, &username, &password)?;
                        Response::LoggedIn { registry, username }
                    }
                    Request::RegistryLogout { registry } => {
                        let registry = engine.auth.logout(&registry)?;
                        Response::LoggedOut { registry }
                    }
                    Request::ListLogins => Response::Logins(engine.auth.list()),
                    Request::Run(spec) => Response::Container(engine.run(*spec)?),
                    Request::Start { id } => Response::Container(engine.start(&id)?),
                    Request::Stop { id } => Response::Container(engine.stop(&id)?),
                    Request::Restart { id } => Response::Container(engine.restart(&id)?),
                    Request::Pause { id } => Response::Container(engine.pause(&id)?),
                    Request::Unpause { id } => Response::Container(engine.unpause(&id)?),
                    Request::Remove { id } => Response::Removed { id: engine.remove(&id)? },
                    Request::DiskUsage => Response::DiskUsage(cleanup::disk_usage(&engine)),
                    Request::RemoveImage { reference } => {
                        let (reference, freed_bytes, kept_bytes) = cleanup::remove_image(&engine, &reference)?;
                        Response::ImageRemoved { reference, freed_bytes, kept_bytes }
                    }
                    Request::Prune(spec) => Response::Pruned(cleanup::prune(&engine, &spec)?),
                    Request::SnapshotStatus => Response::Snapshots(engine.snapshot_status()),
                    Request::SnapshotWarm { cpus, memory_mib, rosetta, slots } => {
                        Response::Snapshots(engine.snapshot_warm(cpus, memory_mib, rosetta, slots)?)
                    }
                    Request::SnapshotClear => Response::Snapshots(engine.snapshot_clear()),
                    Request::Stats { id } => Response::Stats(engine.stats(&id)?),
                    Request::ListFiles { id, path } => Response::Files(engine.list_files(&id, &path)?),
                    Request::ReadFile { id, path } => Response::File(engine.read_file(&id, &path)?),
                    Request::Inspect { id } => Response::Details(Box::new(engine.inspect(&id)?)),
                    Request::Events { since, limit } => Response::Events(engine.events.since(since, limit)),
                    Request::Exec(_)
                    | Request::GetArchive { .. }
                    | Request::PutArchive { .. }
                    | Request::SaveImage { .. }
                    | Request::LoadImage { .. }
                    | Request::StatsStream { .. } => {
                        unreachable!("handled by serve()")
                    }
                    Request::Build(_) => Response::Error { message: "builds run on the Mac itself: use grav build there".into() },
                    Request::PairBegin | Request::ListDevices | Request::RevokeDevice { .. } | Request::Pair { .. } | Request::Auth { .. }
                    | Request::PushSetup { .. } | Request::PushStatus | Request::PushTest | Request::RegisterPush { .. } => {
                        Response::Error { message: "not available here".into() }
                    }
                    _ => unreachable!("reads are answered above"),
                })
            })
            .await;
            match outcome {
                Ok(Ok(response)) => response,
                Ok(Err(err)) => Response::Error { message: format!("{err:#}") },
                Err(err) => Response::Error { message: format!("internal error: {err}") },
            }
        }
    }
}

fn result<E: std::fmt::Display>(r: Result<Response, E>) -> Response {
    r.unwrap_or_else(|err| Response::Error { message: err.to_string() })
}

fn demo_volumes(store: &Store) -> Vec<graviton_proto::VolumeSummary> {
    let mut out = Vec::new();
    for c in store.containers() {
        for m in &c.mounts {
            if !m.source.starts_with('/') && !m.source.starts_with('~') {
                out.push(graviton_proto::VolumeSummary {
                    name: m.source.clone(),
                    path: paths::volumes_dir().join(&m.source).display().to_string(),
                    size_bytes: 184 << 20,
                    file_count: 412,
                    created_at: c.created_at,
                    used_by: vec![graviton_proto::VolumeMountRef {
                        container_id: c.id.clone(),
                        container_name: c.name.clone(),
                        target: m.target.clone(),
                        read_only: m.read_only,
                        running: c.status == graviton_proto::ContainerStatus::Running,
                    }],
                });
            }
        }
    }
    out
}

fn demo_network(store: &Store) -> graviton_proto::NetworkInfo {
    let endpoints: Vec<graviton_proto::NetworkEndpoint> = store
        .containers()
        .iter()
        .map(|c| {
            let running = c.status == graviton_proto::ContainerStatus::Running;
            graviton_proto::NetworkEndpoint {
                container_id: c.id.clone(),
                container_name: c.name.clone(),
                image: c.image.clone(),
                status: c.status,
                ip_address: c.ip_address.clone(),
                dns_names: if running && c.ip_address.is_some() {
                    vec![c.name.clone(), format!("{}.graviton", c.name), c.id.clone()]
                } else {
                    vec![]
                },
                dns_enabled: running,
                ports: c.ports.clone(),
                networks: vec![graviton_proto::DEFAULT_NETWORK_NAME.into()],
            }
        })
        .collect();
    let connected = endpoints.iter().filter(|e| e.status == graviton_proto::ContainerStatus::Running).count() as u32;
    let names: Vec<String> = store.containers().iter().map(|c| c.name.clone()).collect();
    graviton_proto::NetworkInfo {
        name: "graviton".into(),
        driver: "L2 switch + NAT".into(),
        subnet: "192.168.64.0/24".into(),
        gateway: "192.168.64.1".into(),
        dns_server: "127.0.0.11:53".into(),
        dns_domain: "graviton".into(),
        connected_ports: connected,
        learned_macs: connected,
        frames_forwarded: 1842,
        bytes_forwarded: 4_812_900,
        endpoints,
        networks: vec![graviton_proto::UserNetwork {
            id: "graviton-bridge".into(),
            name: graviton_proto::DEFAULT_NETWORK_NAME.into(),
            driver: "L2 switch + NAT".into(),
            internal: false,
            builtin: true,
            created_at: 0,
            labels: std::collections::BTreeMap::new(),
            containers: names,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn demo_backend_handles_requests() {
        let backend = Backend::Demo(Arc::new(Mutex::new(Store::demo())));
        assert_eq!(handle(Request::Ping, &backend).await, Response::Pong { protocol: PROTOCOL_VERSION });
        match handle(Request::Pause { id: "web".into() }, &backend).await {
            Response::Container(c) => assert_eq!(c.status, graviton_proto::ContainerStatus::Paused),
            other => panic!("unexpected {other:?}"),
        }
        match handle(Request::Unpause { id: "web".into() }, &backend).await {
            Response::Container(c) => assert_eq!(c.status, graviton_proto::ContainerStatus::Running),
            other => panic!("unexpected {other:?}"),
        }
        match handle(Request::Stop { id: "web".into() }, &backend).await {
            Response::Container(c) => assert_eq!(c.name, "web"),
            other => panic!("unexpected {other:?}"),
        }
        match handle(Request::Stop { id: "ghost".into() }, &backend).await {
            Response::Error { message } => assert!(message.contains("no such container")),
            other => panic!("unexpected {other:?}"),
        }
        match handle(Request::Pull { reference: "alpine".into(), progress: false, platform: None }, &backend).await {
            Response::Error { message } => assert!(message.contains("demo")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn filtered_and_streaming_logs_on_demo_backend() {
        let backend = Backend::Demo(Arc::new(Mutex::new(Store::demo())));
        let sample = handle(
            Request::Logs { id: "api-gateway".into(), tail: Some(2), follow: false, since_ms: None, until_ms: None },
            &backend,
        )
        .await;
        let Response::Logs(lines) = sample else { panic!("expected logs, got {sample:?}") };
        assert_eq!(lines.len(), 2);

        let cutoff = lines[1].timestamp_ms;
        let windowed = handle(
            Request::Logs { id: "api-gateway".into(), tail: None, follow: false, since_ms: Some(cutoff), until_ms: Some(cutoff) },
            &backend,
        )
        .await;
        let Response::Logs(w) = windowed else { panic!("expected logs, got {windowed:?}") };
        assert!(w.iter().all(|l| l.timestamp_ms == cutoff));
        assert!(w.iter().any(|l| l.seq == lines[1].seq));

        let mut reader = BufReader::new(tokio::io::empty());
        let mut out = Vec::new();
        let reusable = logs_stream(&backend, "api-gateway".into(), Some(2), None, None, &mut reader, &mut out)
            .await
            .unwrap();
        assert!(reusable);

        let decoded: Vec<Response> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| decode_line::<Response>(l.as_bytes()).unwrap())
            .collect();
        assert_eq!(decoded.len(), 3);
        assert!(matches!(decoded[0], Response::Log(_)));
        assert!(matches!(decoded[1], Response::Log(_)));
        assert_eq!(decoded[2], Response::LogEnd { exit_code: None });
    }

    #[tokio::test]
    async fn stats_stream_on_demo_backend() {
        let backend = Backend::Demo(Arc::new(Mutex::new(Store::demo())));
        let mut reader = BufReader::new(tokio::io::empty());
        let mut out = Vec::new();
        let reusable = stats_stream(&backend, vec![], false, true, &mut reader, &mut out)
            .await
            .unwrap();
        assert!(reusable);
        let decoded: Vec<Response> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| decode_line::<Response>(l.as_bytes()).unwrap())
            .collect();
        assert_eq!(decoded.len(), 1);
        let Response::StatsSnapshot(rows) = &decoded[0] else {
            panic!("expected StatsSnapshot, got {:?}", decoded[0]);
        };
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|r| r.pids > 0));
    }
}
