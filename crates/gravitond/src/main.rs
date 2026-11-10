//! `gravitond`: the Graviton engine daemon.
//!
//! Listens on a Unix socket and serves the newline-delimited JSON control API
//! defined in `graviton-proto`. Clients are the `grav` CLI and Graviton.app.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use graviton_core::{Store, paths};
use graviton_proto::{PROTOCOL_VERSION, Request, Response, decode_line, encode_line};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

#[derive(Parser, Debug)]
#[command(version, about = "The Graviton engine daemon")]
struct Args {
    /// Control socket path (default: $GRAVITON_SOCKET or ~/Library/Application Support/Graviton/gravitond.sock)
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Serve sample containers instead of an empty engine (until the VMM lands).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    demo: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
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

    let listener = UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    restrict_permissions(&socket)?;

    let store = Arc::new(Mutex::new(if args.demo { Store::demo() } else { Store::new() }));
    eprintln!("gravitond {} listening on {}", env!("CARGO_PKG_VERSION"), socket.display());

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let store = Arc::clone(&store);
                tokio::spawn(async move {
                    if let Err(err) = serve(stream, store).await {
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

async fn serve(stream: UnixStream, store: Arc<Mutex<Store>>) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match decode_line::<Request>(line.as_bytes()) {
            Ok(request) => handle(request, &mut *store.lock().await),
            Err(err) => Response::Error { message: format!("bad request: {err}") },
        };
        write.write_all(&encode_line(&response)).await?;
    }
    Ok(())
}

fn handle(request: Request, store: &mut Store) -> Response {
    let result = match request {
        Request::Ping => return Response::Pong { protocol: PROTOCOL_VERSION },
        Request::EngineInfo => return Response::Engine(store.engine_info()),
        Request::ListContainers => return Response::Containers(store.containers().to_vec()),
        Request::ListImages => return Response::Images(store.images().to_vec()),
        Request::Logs { id, tail } => store.logs(&id, tail).map(Response::Logs),
        Request::Start { id } => store.start(&id).map(Response::Container),
        Request::Stop { id } => store.stop(&id).map(Response::Container),
        Request::Restart { id } => store.restart(&id).map(Response::Container),
    };
    result.unwrap_or_else(|err| Response::Error { message: err.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_requests() {
        let mut store = Store::demo();
        assert_eq!(handle(Request::Ping, &mut store), Response::Pong { protocol: PROTOCOL_VERSION });
        match handle(Request::Stop { id: "web".into() }, &mut store) {
            Response::Container(c) => assert_eq!(c.name, "web"),
            other => panic!("unexpected {other:?}"),
        }
        match handle(Request::Stop { id: "ghost".into() }, &mut store) {
            Response::Error { message } => assert!(message.contains("no such container")),
            other => panic!("unexpected {other:?}"),
        }
    }
}
