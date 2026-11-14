//! Daemon side of the IPC protocol: accepts clients and dispatches requests.

use std::io;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;

use crate::error::RomError;
use crate::ipc::{Request, Response};
use crate::process::ProcessManager;

/// Serves requests on `endpoint` until `shutdown` becomes `true`.
/// Fails if another daemon already owns the endpoint.
pub async fn serve(
    endpoint: &str,
    manager: Arc<ProcessManager>,
    shutdown_tx: watch::Sender<bool>,
) -> io::Result<()> {
    #[cfg(windows)]
    {
        windows_serve(endpoint, manager, shutdown_tx).await
    }
    #[cfg(unix)]
    {
        unix_serve(endpoint, manager, shutdown_tx).await
    }
}

#[cfg(windows)]
async fn windows_serve(
    pipe: &str,
    manager: Arc<ProcessManager>,
    shutdown_tx: watch::Sender<bool>,
) -> io::Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let mut shutdown = shutdown_tx.subscribe();
    let mut server = ServerOptions::new().first_pipe_instance(true).create(pipe)?;
    loop {
        let connected = tokio::select! {
            result = server.connect() => Some(result),
            () = wait_until_true(&mut shutdown) => None,
        };
        match connected {
            None => return Ok(()),
            Some(result) => result?,
        }

        let client = server;
        server = ServerOptions::new().create(pipe)?;
        let manager = Arc::clone(&manager);
        let shutdown_tx = shutdown_tx.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_split(client, &manager, &shutdown_tx).await {
                tracing::warn!(%error, "IPC request failed");
            }
        });
    }
}

#[cfg(unix)]
async fn unix_serve(
    path: &str,
    manager: Arc<ProcessManager>,
    shutdown_tx: watch::Sender<bool>,
) -> io::Result<()> {
    use std::path::Path;

    use tokio::net::UnixListener;

    let path = Path::new(path);
    if path.exists() {
        match tokio::net::UnixStream::connect(path).await {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "another rom daemon already owns this socket",
                ));
            }
            Err(_) => {
                std::fs::remove_file(path)?;
            }
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(path)?;
    let mut shutdown = shutdown_tx.subscribe();
    loop {
        let accepted = tokio::select! {
            result = listener.accept() => Some(result),
            () = wait_until_true(&mut shutdown) => None,
        };
        match accepted {
            None => {
                let _ = std::fs::remove_file(path);
                return Ok(());
            }
            Some(result) => {
                let (client, _) = result?;
                let manager = Arc::clone(&manager);
                let shutdown_tx = shutdown_tx.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_split(client, &manager, &shutdown_tx).await {
                        tracing::warn!(%error, "IPC request failed");
                    }
                });
            }
        }
    }
}

async fn handle_split<S>(
    stream: S,
    manager: &Arc<ProcessManager>,
    shutdown_tx: &watch::Sender<bool>,
) -> Result<(), RomError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };

    let (response, stop_daemon) = match serde_json::from_str::<Request>(&line) {
        Ok(request) => dispatch(request, manager).await,
        Err(error) => (
            Response::Error {
                message: format!("invalid request: {error}"),
            },
            false,
        ),
    };

    let mut payload = serde_json::to_string(&response)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;
    writer.flush().await?;

    if stop_daemon {
        let _ = shutdown_tx.send(true);
        if let Err(error) = manager.shutdown().await {
            tracing::error!(%error, "failed to stop every service during daemon shutdown");
        }
    }
    Ok(())
}

async fn dispatch(request: Request, manager: &Arc<ProcessManager>) -> (Response, bool) {
    match request {
        Request::Status => {
            let snapshot = manager.status_snapshot();
            (
                Response::Status {
                    daemon: snapshot.daemon,
                    services: snapshot.services,
                },
                false,
            )
        }
        Request::Start { services } => (to_response(manager.start(&services).await), false),
        Request::Stop { services, force } => (to_response(manager.stop(&services, force).await), false),
        Request::Restart { service } => {
            let result = manager
                .restart(&service)
                .await
                .map(|()| vec![service.clone()]);
            (to_response(result), false)
        }
        Request::Shutdown => (Response::Ok { changed: Vec::new() }, true),
    }
}

fn to_response(result: Result<Vec<String>, RomError>) -> Response {
    match result {
        Ok(changed) => Response::Ok { changed },
        Err(error) => Response::Error {
            message: error.to_string(),
        },
    }
}

async fn wait_until_true(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}
