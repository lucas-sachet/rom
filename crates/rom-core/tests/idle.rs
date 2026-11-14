//! Stage 3: idle stop and quiet keep-alive handling.
#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rom_core::config::Config;
use rom_core::gateway::Gateway;
use rom_core::process::{ProcessManager, ServiceState};
use rom_core::registry::Registry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::time::{sleep, timeout};

const HTTP_SERVER: &str = "const http = require('http');
http.createServer((req, res) => {
  if (req.url === '/stream') {
    res.writeHead(200, { 'Content-Type': 'text/plain' });
    const id = setInterval(() => { try { res.write('x'); } catch (_) {} }, 40);
    req.on('close', () => clearInterval(id));
    return;
  }
  res.end(process.env.ROM_SERVICE);
}).listen(Number(process.env.PORT), '127.0.0.1');
";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rom-idle-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn yaml_on_demand(port: u16) -> String {
    format!(
        "version: 1\nproject:\n  name: test\nruntime:\n  idle_timeout: 200ms\n  connection_idle: 150ms\nservices:\n  api:\n    path: .\n    command: node\n    args: [server.js]\n    port: {port}\n"
    )
}

fn yaml_persistent(port: u16) -> String {
    format!(
        "version: 1\nproject:\n  name: test\nruntime:\n  idle_timeout: 200ms\n  connection_idle: 150ms\nservices:\n  worker:\n    path: .\n    command: node\n    args: [server.js]\n    port: {port}\n    mode: persistent\n"
    )
}

fn manager(dir: &Path, yaml: &str) -> Arc<ProcessManager> {
    let config: Config = serde_yaml_ng::from_str(yaml).expect("yaml parses");
    let registry = Registry::from_config(config, dir).expect("registry builds");
    ProcessManager::new(registry, &dir.join(".rom")).expect("manager builds")
}

fn state(pm: &ProcessManager, name: &str) -> ServiceState {
    pm.status()
        .into_iter()
        .find(|s| s.name == name)
        .expect("service is registered")
        .state
}

async fn wait_state(pm: &ProcessManager, name: &str, want: ServiceState) {
    timeout(Duration::from_secs(5), async {
        loop {
            if state(pm, name) == want {
                return;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {name} to become {want}"));
}

async fn http_get(port: u16, path: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    response
}

#[tokio::test]
async fn on_demand_service_stops_after_idle_timeout() {
    let dir = scratch("stop");
    fs::write(dir.join("server.js"), HTTP_SERVER).unwrap();
    let pm = manager(&dir, &yaml_on_demand(39130));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let gateway = Gateway::bind(&pm).await.expect("bind");
    let gateway_pm = Arc::clone(&pm);
    let gateway_task = tokio::spawn(async move { gateway.serve(gateway_pm, shutdown_rx).await });
    tokio::task::yield_now().await;

    let body = http_get(39130, "/").await;
    assert!(body.contains("api"), "unexpected body: {body}");
    assert_eq!(state(&pm, "api"), ServiceState::Running);

    wait_state(&pm, "api", ServiceState::Sleeping).await;

    let _ = shutdown_tx.send(true);
    gateway_task.await.expect("join").expect("serve");
}

#[tokio::test]
async fn persistent_service_ignores_idle_timeout() {
    let dir = scratch("persistent");
    fs::write(dir.join("server.js"), HTTP_SERVER).unwrap();
    let pm = manager(&dir, &yaml_persistent(39131));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let gateway = Gateway::bind(&pm).await.expect("bind");
    let gateway_pm = Arc::clone(&pm);
    let gateway_task = tokio::spawn(async move { gateway.serve(gateway_pm, shutdown_rx).await });
    tokio::task::yield_now().await;

    assert!(http_get(39131, "/").await.contains("worker"));
    assert_eq!(state(&pm, "worker"), ServiceState::Running);

    sleep(Duration::from_millis(600)).await;
    assert_eq!(state(&pm, "worker"), ServiceState::Running);

    pm.shutdown().await.expect("shutdown");
    let _ = shutdown_tx.send(true);
    gateway_task.await.expect("join").expect("serve");
}

#[tokio::test]
async fn active_stream_defers_idle_stop_until_closed() {
    let dir = scratch("stream");
    fs::write(dir.join("server.js"), HTTP_SERVER).unwrap();
    let pm = manager(&dir, &yaml_on_demand(39132));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let gateway = Gateway::bind(&pm).await.expect("bind");
    let gateway_pm = Arc::clone(&pm);
    let gateway_task = tokio::spawn(async move { gateway.serve(gateway_pm, shutdown_rx).await });
    tokio::task::yield_now().await;

    let mut stream = TcpStream::connect(("127.0.0.1", 39132))
        .await
        .expect("connect");
    stream
        .write_all(b"GET /stream HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .expect("write");

    let mut buf = [0u8; 64];
    let n = timeout(Duration::from_secs(2), stream.read(&mut buf))
        .await
        .expect("read timeout")
        .expect("read");
    assert!(n > 0, "expected stream bytes");

    sleep(Duration::from_millis(500)).await;
    assert_eq!(state(&pm, "api"), ServiceState::Running);

    drop(stream);
    wait_state(&pm, "api", ServiceState::Sleeping).await;

    let _ = shutdown_tx.send(true);
    gateway_task.await.expect("join").expect("serve");
}

#[tokio::test]
async fn rom_start_without_traffic_still_idles() {
    let dir = scratch("start-idle");
    fs::write(dir.join("server.js"), HTTP_SERVER).unwrap();
    let pm = manager(&dir, &yaml_on_demand(39133));

    pm.start(&["api".to_owned()]).await.expect("start");
    assert_eq!(state(&pm, "api"), ServiceState::Running);
    wait_state(&pm, "api", ServiceState::Sleeping).await;
}
