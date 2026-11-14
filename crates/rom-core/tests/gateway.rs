//! Stage 2: TCP gateway wakes sleeping services and coalesces concurrent starts.
#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rom_core::config::Config;
use rom_core::gateway::Gateway;
use rom_core::process::{ProcessManager, ServiceState};
use rom_core::registry::Registry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;

const HTTP_SERVER: &str = "const http = require('http');
http.createServer((req, res) => {
  res.end(process.env.ROM_SERVICE);
}).listen(Number(process.env.PORT), '127.0.0.1');
";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rom-gw-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn config_yaml(services: &[(&str, u16, &str)]) -> String {
    let mut yaml = String::from("version: 1\nproject:\n  name: test\nservices:\n");
    for (name, port, deps) in services {
        yaml += &format!(
            "  {name}:\n    path: .\n    command: node\n    args: [server.js]\n    port: {port}\n    dependencies: [{deps}]\n"
        );
    }
    yaml
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

fn pid(pm: &ProcessManager, name: &str) -> u32 {
    pm.status()
        .into_iter()
        .find(|s| s.name == name)
        .expect("service is registered")
        .pid
        .expect("service has a pid")
}

async fn http_get_body(port: u16) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect to public port");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .await
        .expect("write request");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read response");
    response
        .rsplit("\r\n\r\n")
        .next()
        .expect("http body")
        .trim()
        .to_owned()
}

#[tokio::test]
async fn connection_starts_dependencies_and_serves_the_original_client() {
    let dir = scratch("wake");
    fs::write(dir.join("server.js"), HTTP_SERVER).unwrap();
    let pm = manager(
        &dir,
        &config_yaml(&[
            ("billing", 39120, ""),
            ("user", 39121, ""),
            ("finance", 39122, "user"),
        ]),
    );

    assert_eq!(state(&pm, "billing"), ServiceState::Sleeping);
    assert_eq!(state(&pm, "user"), ServiceState::Sleeping);
    assert_eq!(state(&pm, "finance"), ServiceState::Sleeping);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let gateway = Gateway::bind(&pm).await.expect("bind public ports");
    let gateway_pm = Arc::clone(&pm);
    let gateway_task = tokio::spawn(async move { gateway.serve(gateway_pm, shutdown_rx).await });

    // Give accept loops a tick to arm.
    tokio::task::yield_now().await;

    let body = http_get_body(39122).await;
    assert_eq!(body, "finance");
    assert_eq!(state(&pm, "user"), ServiceState::Running);
    assert_eq!(state(&pm, "finance"), ServiceState::Running);
    assert_eq!(state(&pm, "billing"), ServiceState::Sleeping);

    let _ = shutdown_tx.send(true);
    gateway_task.await.expect("gateway join").expect("gateway serve");
    pm.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn concurrent_connections_create_a_single_process() {
    let dir = scratch("coalesce");
    fs::write(dir.join("server.js"), HTTP_SERVER).unwrap();
    let pm = manager(&dir, &config_yaml(&[("api", 39123, "")]));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let gateway = Gateway::bind(&pm).await.expect("bind public ports");
    let gateway_pm = Arc::clone(&pm);
    let gateway_task = tokio::spawn(async move { gateway.serve(gateway_pm, shutdown_rx).await });
    tokio::task::yield_now().await;

    let ok = Arc::new(AtomicUsize::new(0));
    let mut joins = Vec::new();
    for _ in 0..10 {
        let ok = Arc::clone(&ok);
        joins.push(tokio::spawn(async move {
            let body = http_get_body(39123).await;
            assert_eq!(body, "api");
            ok.fetch_add(1, Ordering::SeqCst);
        }));
    }
    for join in joins {
        join.await.expect("client task");
    }
    assert_eq!(ok.load(Ordering::SeqCst), 10);
    assert_eq!(state(&pm, "api"), ServiceState::Running);
    let first_pid = pid(&pm, "api");

    // A later request must reuse the same process.
    assert_eq!(http_get_body(39123).await, "api");
    assert_eq!(pid(&pm, "api"), first_pid);

    let _ = shutdown_tx.send(true);
    gateway_task.await.expect("gateway join").expect("gateway serve");
    pm.shutdown().await.expect("shutdown");
}
