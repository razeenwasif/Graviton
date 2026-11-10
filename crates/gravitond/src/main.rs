//! `gravitond`: the Graviton engine daemon.
//!
//! Listens on a Unix socket and serves the newline-delimited JSON control API
//! defined in `graviton-proto`. Clients are the `grav` CLI and Graviton.app.
//! Containers run in their own guest VMs (see `runtime.rs`); `--demo` serves
//! the sample containers from the design mockups instead.

mod boot_check;
mod runtime;

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
enum Backend {
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
                tokio::spawn(async move {
                    if let Err(err) = serve(stream, backend).await {
                        eprintln!("connection error: {err:#}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => break,
        }
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

async fn serve(stream: UnixStream, backend: Backend) -> Result<()> {
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

/// `grav exec` without a TTY: stream output lines, then the exit code.
async fn exec_stream(engine: Arc<Engine>, spec: graviton_proto::ExecSpec, write: &mut tokio::net::unix::OwnedWriteHalf) -> Result<()> {
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

/// `grav exec -it`: answer `attached`, then splice the client and the guest's terminal.
async fn exec_tty(
    engine: Arc<Engine>,
    spec: graviton_proto::ExecSpec,
    mut client_in: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut client_out: tokio::net::unix::OwnedWriteHalf,
) -> Result<()> {
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

async fn handle(request: Request, backend: &Backend) -> Response {
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
                _ => Response::Error { message: "gravitond is serving demo data; restart it without --demo to pull, run and exec".into() },
            }
        }
        Backend::Engine(engine) => {
            let engine = Arc::clone(engine);
            // Engine calls block on downloads and VM boots.
            let outcome = tokio::task::spawn_blocking(move || -> Result<Response> {
                Ok(match request {
                    Request::EngineInfo => Response::Engine(engine.engine_info()),
                    Request::Pull { reference } => Response::Image(engine.pull(&reference)?),
                    Request::Run(spec) => Response::Container(engine.run(spec)?),
                    Request::Start { id } => Response::Container(engine.start(&id)?),
                    Request::Stop { id } => Response::Container(engine.stop(&id)?),
                    Request::Restart { id } => Response::Container(engine.restart(&id)?),
                    Request::Remove { id } => Response::Removed { id: engine.remove(&id)? },
                    Request::Exec(_) => unreachable!("exec is handled by serve()"),
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
        match handle(Request::Pull { reference: "alpine".into() }, &backend).await {
            Response::Error { message } => assert!(message.contains("demo")),
            other => panic!("unexpected {other:?}"),
        }
    }
}
