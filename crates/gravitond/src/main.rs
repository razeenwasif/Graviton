//! `gravitond`: the Graviton engine daemon.
//!
//! Listens on a Unix socket and serves the newline-delimited JSON control API
//! defined in `graviton-proto`. Clients are the `grav` CLI and Graviton.app.
//! Containers run in their own guest VMs (see `runtime.rs`); `--demo` serves
//! the sample containers from the design mockups instead.

mod auth;
mod boot_check;
mod build;
mod cleanup;
mod events;
mod push;
mod remote;
mod runtime;
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

    /// Serve the sample containers from the design mockups instead of real ones.
    #[arg(long)]
    demo: bool,

    /// Also listen for paired phones over TLS on the local network (see `grav pair`).
    #[arg(long)]
    remote: bool,

    /// TCP port for --remote.
    #[arg(long, default_value_t = graviton_proto::REMOTE_PORT)]
    remote_port: u16,

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
    let socket = args.socket.unwrap_or_else(paths::socket_path);

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

    let backend = if args.demo {
        Backend::Demo(Arc::new(Mutex::new(Store::demo())))
    } else {
        Backend::Engine(Engine::open()?)
    };

    let remote = if args.remote {
        let remote = remote::Remote::open(args.remote_port)?;
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
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    if let Backend::Engine(engine) = &backend {
        let engine = Arc::clone(engine);
        let _ = tokio::task::spawn_blocking(move || engine.shutdown()).await;
    }
    tokio::fs::remove_file(&socket).await.ok();
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
        if let (Request::Pull { reference, progress: true }, Backend::Engine(engine)) = (&request, &backend) {
            pull_stream(Arc::clone(engine), reference.clone(), &mut write).await?;
            continue;
        }
        if let (Request::Build(spec), Backend::Engine(engine)) = (&request, &backend) {
            build_stream(Arc::clone(engine), spec.clone(), lines.get_mut(), &mut write).await?;
            continue;
        }
        if let (Request::Run(spec), Backend::Engine(engine)) = (&request, &backend) {
            if spec.tty {
                let reader = lines.into_inner();
                return run_tty(Arc::clone(engine), spec.clone(), reader, write).await;
            }
        }
        if let (Request::GetArchive { id, path }, Backend::Engine(engine)) = (&request, &backend) {
            return get_archive_stream(Arc::clone(engine), id.clone(), path.clone(), write).await;
        }
        if let (Request::PutArchive { id, path, size }, Backend::Engine(engine)) = (&request, &backend) {
            put_archive_stream(Arc::clone(engine), id.clone(), path.clone(), *size, lines.get_mut(), &mut write).await?;
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
        let response = handle(request, &backend).await;
        write.write_all(&encode_line(&response)).await?;
    }
}

/// `grav pull` (and app Pull sheets): stream per-layer progress, then the image.
pub(crate) async fn pull_stream(
    engine: Arc<Engine>,
    reference: String,
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::task::spawn_blocking(move || {
        let updates = tx.clone();
        let result = engine.pull_with_progress(&reference, |progress| {
            let _ = updates.send(Response::PullProgress(progress));
        });
        let _ = tx.send(match result {
            Ok(image) => Response::Image(image),
            Err(err) => Response::Error { message: format!("{err:#}") },
        });
    });
    let mut disconnected = false;
    while let Some(response) = rx.recv().await {
        if write.write_all(&encode_line(&response)).await.is_err() || write.flush().await.is_err() {
            disconnected = true;
            break; // client closed the sheet/terminal; pull carries on in the background
        }
    }
    drop(rx);
    if !disconnected {
        let _ = task.await;
    }
    Ok(())
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

pub(crate) async fn handle(request: Request, backend: &Backend) -> Response {
    // Reads answer straight from the store.
    {
        let store = backend.store().lock().unwrap();
        match &request {
            Request::Ping => return Response::Pong { protocol: PROTOCOL_VERSION },
            Request::ListContainers => return Response::Containers(store.containers().to_vec()),
            Request::ListImages => return Response::Images(store.images().to_vec()),
            Request::Logs { id, tail } => return result(store.logs(id, *tail).map(Response::Logs)),
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
                _ => Response::Error { message: "gravitond is serving demo data; restart it without --demo to pull, run, exec and browse files".into() },
            }
        }
        Backend::Engine(engine) => {
            let engine = Arc::clone(engine);
            // Engine calls block on downloads and VM boots.
            let outcome = tokio::task::spawn_blocking(move || -> Result<Response> {
                Ok(match request {
                    Request::EngineInfo => Response::Engine(engine.engine_info()),
                    Request::Pull { reference, .. } => Response::Image(engine.pull(&reference)?),
                    Request::RegistryLogin { registry, username, password } => {
                        let (registry, username) = engine.auth.login(&registry, &username, &password)?;
                        Response::LoggedIn { registry, username }
                    }
                    Request::RegistryLogout { registry } => {
                        let registry = engine.auth.logout(&registry)?;
                        Response::LoggedOut { registry }
                    }
                    Request::ListLogins => Response::Logins(engine.auth.list()),
                    Request::Run(spec) => Response::Container(engine.run(spec)?),
                    Request::Start { id } => Response::Container(engine.start(&id)?),
                    Request::Stop { id } => Response::Container(engine.stop(&id)?),
                    Request::Restart { id } => Response::Container(engine.restart(&id)?),
                    Request::Remove { id } => Response::Removed { id: engine.remove(&id)? },
                    Request::DiskUsage => Response::DiskUsage(cleanup::disk_usage(&engine)),
                    Request::RemoveImage { reference } => {
                        let (reference, freed_bytes, kept_bytes) = cleanup::remove_image(&engine, &reference)?;
                        Response::ImageRemoved { reference, freed_bytes, kept_bytes }
                    }
                    Request::Prune(spec) => Response::Pruned(cleanup::prune(&engine, &spec)?),
                    Request::Stats { id } => Response::Stats(engine.stats(&id)?),
                    Request::ListFiles { id, path } => Response::Files(engine.list_files(&id, &path)?),
                    Request::ReadFile { id, path } => Response::File(engine.read_file(&id, &path)?),
                    Request::Inspect { id } => Response::Details(Box::new(engine.inspect(&id)?)),
                    Request::Events { since, limit } => Response::Events(engine.events.since(since, limit)),
                    Request::Exec(_) | Request::GetArchive { .. } | Request::PutArchive { .. } => {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn demo_backend_handles_requests() {
        let backend = Backend::Demo(Arc::new(Mutex::new(Store::demo())));
        assert_eq!(handle(Request::Ping, &backend).await, Response::Pong { protocol: PROTOCOL_VERSION });
        match handle(Request::Stop { id: "web".into() }, &backend).await {
            Response::Container(c) => assert_eq!(c.name, "web"),
            other => panic!("unexpected {other:?}"),
        }
        match handle(Request::Stop { id: "ghost".into() }, &backend).await {
            Response::Error { message } => assert!(message.contains("no such container")),
            other => panic!("unexpected {other:?}"),
        }
        match handle(Request::Pull { reference: "alpine".into(), progress: false }, &backend).await {
            Response::Error { message } => assert!(message.contains("demo")),
            other => panic!("unexpected {other:?}"),
        }
    }
}
