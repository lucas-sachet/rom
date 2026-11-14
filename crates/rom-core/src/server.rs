//! Daemon side of the IPC protocol: accepts named pipe clients and dispatches requests.

use std::io;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::watch;

use crate::error::RomError;
use crate::ipc::{Request, Response};
use crate::process::ProcessManager;

/// Serves requests on `pipe` until `shutdown` becomes `true`.
/// Fails if another daemon already owns the pipe.
pub async fn serve(
    pipe: &str,
    manager: Arc<ProcessManager>,
    shutdown_tx: watch::Sender<bool>,
) -> io::Result<()> {
    let mut shutdown = shutdown_tx.subscribe();
    let mut server = ServerOptions::new().first_pipe_instance(true).create(pipe)?;
    loop {
        let connected = tokio::select! {
            result = server.connect() => Some(result),
            changed = shutdown.wait_for(|stop| *stop) => {
                let _ = changed;
                None
            }
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
            if let Err(error) = handle(client, &manager, &shutdown_tx).await {
                tracing::warn!(%error, "IPC request failed");
            }
        });
    }
}

async fn handle(
    pipe: NamedPipeServer,
    manager: &Arc<ProcessManager>,
    shutdown_tx: &watch::Sender<bool>,
) -> Result<(), RomError> {
    let (reader, mut writer) = tokio::io::split(pipe);
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

    // Stop the gateway before killing services, so a late accept cannot start them again.
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
        Request::Status => (
            Response::Status {
                services: manager.status(),
            },
            false,
        ),
        Request::Start { services } => (to_response(manager.start(&services).await), false),
        Request::Stop { services, force } => (to_response(manager.stop(&services, force).await), false),
        Request::Restart { service } => {
            let result = manager
                .restart(&service)
                .await
                .map(|()| vec![service.clone()]);
            (to_response(result), false)
        }
        // Services are stopped after the gateway is signalled, in `handle`.
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
