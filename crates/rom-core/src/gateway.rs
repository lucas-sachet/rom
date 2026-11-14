//! TCP gateway: listens on each service's public port and proxies to its process.
//!
//! A connection on a sleeping service starts that service (and its dependencies) once;
//! concurrent clients share the same start via `ProcessManager::ensure_ready`.
//! Quiet keep-alive connections are closed after `connection_idle` with no bytes.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::time::sleep;

use crate::process::ProcessManager;

/// Bound public-port listeners for one project.
pub struct Gateway {
    listeners: Vec<(String, TcpListener)>,
}

impl Gateway {
    /// Binds `127.0.0.1` on every service's public port. Fails if any port is unavailable.
    pub async fn bind(manager: &ProcessManager) -> io::Result<Self> {
        let mut listeners = Vec::new();
        for def in manager.registry().services() {
            let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, def.port));
            let listener = TcpListener::bind(addr).await.map_err(|source| {
                io::Error::new(
                    source.kind(),
                    format!(
                        "cannot bind public port {} for `{}`: {source}",
                        def.port, def.name
                    ),
                )
            })?;
            tracing::info!(service = %def.name, port = def.port, "gateway listening");
            listeners.push((def.name.clone(), listener));
        }
        Ok(Self { listeners })
    }

    /// Accepts connections until `shutdown` becomes `true`.
    pub async fn serve(
        self,
        manager: Arc<ProcessManager>,
        mut shutdown: watch::Receiver<bool>,
    ) -> io::Result<()> {
        let mut tasks = Vec::with_capacity(self.listeners.len());
        for (name, listener) in self.listeners {
            let manager = Arc::clone(&manager);
            let shutdown = shutdown.clone();
            tasks.push(tokio::spawn(async move {
                accept_loop(name, listener, manager, shutdown).await;
            }));
        }

        let _ = shutdown.wait_for(|stop| *stop).await;
        for task in tasks {
            task.abort();
        }
        Ok(())
    }
}

async fn accept_loop(
    name: String,
    listener: TcpListener,
    manager: Arc<ProcessManager>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let accepted = tokio::select! {
            result = listener.accept() => result,
            changed = shutdown.wait_for(|stop| *stop) => {
                let _ = changed;
                return;
            }
        };
        let (client, peer) = match accepted {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!(service = %name, %error, "gateway accept failed");
                continue;
            }
        };
        let manager = Arc::clone(&manager);
        let service = name.clone();
        tokio::spawn(async move {
            if let Err(error) = proxy(client, &manager, &service).await {
                tracing::warn!(service = %service, %peer, %error, "gateway proxy failed");
            }
        });
    }
}

async fn proxy(mut client: TcpStream, manager: &Arc<ProcessManager>, name: &str) -> io::Result<()> {
    let listen_port = manager.ensure_ready(name).await.map_err(io::Error::other)?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, listen_port));
    let mut upstream = TcpStream::connect(addr).await?;
    manager.connection_opened(name);
    let _guard = ConnectionGuard {
        manager: Arc::clone(manager),
        name: name.to_owned(),
    };
    let idle = manager.registry().connection_idle;
    copy_until_quiet(&mut client, &mut upstream, idle).await
}

struct ConnectionGuard {
    manager: Arc<ProcessManager>,
    name: String,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.manager.connection_closed(&self.name);
    }
}

/// Bidirectional copy that ends when either side closes or no bytes flow for `quiet`.
async fn copy_until_quiet(
    left: &mut TcpStream,
    right: &mut TcpStream,
    quiet: Duration,
) -> io::Result<()> {
    let mut left_buf = vec![0u8; 16 * 1024];
    let mut right_buf = vec![0u8; 16 * 1024];
    let mut last_activity = Instant::now();

    loop {
        let wait = quiet.saturating_sub(last_activity.elapsed());
        tokio::select! {
            result = left.read(&mut left_buf) => {
                match result? {
                    0 => return Ok(()),
                    n => {
                        right.write_all(&left_buf[..n]).await?;
                        last_activity = Instant::now();
                    }
                }
            }
            result = right.read(&mut right_buf) => {
                match result? {
                    0 => return Ok(()),
                    n => {
                        left.write_all(&right_buf[..n]).await?;
                        last_activity = Instant::now();
                    }
                }
            }
            () = sleep(wait) => return Ok(()),
        }
    }
}
