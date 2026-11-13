//! Daemon side of the IPC protocol: accepts named pipe clients and dispatches requests.

use std::io;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::Notify;

use crate::error::RomError;
use crate::ipc::{Request, Response};
use crate::process::ProcessManager;

/// Serves requests on `pipe` until `shutdown` is notified.
/// Fails if another daemon already owns the pipe.
pub async fn serve(pipe: &str, manager: Arc<ProcessManager>, shutdown: Arc<Notify>) -> io::Result<()> {
    let mut server = ServerOptions::new().first_pipe_instance(true).create(pipe)?;
    loop {
        let connected = tokio::select! {
            result = server.connect() => Some(result),
            () = shutdown.notified() => None,
        };
        match connected {
            None => return Ok(()),
            Some(result) => result?,
        }

        let client = server;
        server = ServerOptions::new().create(pipe)?;
        let manager = Arc::clone(&manager);
        let shutdown = Arc::clone(&shutdown);
        tokio::spawn(async move {
            if let Err(error) = handle(client, &manager, &shutdown).await {
                tracing::warn!(%error, "IPC request failed");
            }
        });
    }
}

async fn handle(pipe: NamedPipeServer, manager: &ProcessManager, shutdown: &Notify) -> Result<(), RomError> {
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

    // Notify only after the reply is written, so the client sees the answer.
    if stop_daemon {
        shutdown.notify_one();
    }
    Ok(())
}

async fn dispatch(request: Request, manager: &ProcessManager) -> (Response, bool) {
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
        Request::Shutdown => (to_response(manager.shutdown().await.map(|()| Vec::new())), true),
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
