//! Stage 7: per-service and daemon process metrics in status.
#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rom_core::config::Config;
use rom_core::process::{ProcessManager, ServiceState};
use rom_core::registry::Registry;
use tokio::time::{sleep, timeout};

const HTTP_SERVER: &str = "const http = require('http');
http.createServer((req, res) => res.end('ok')).listen(Number(process.env.PORT), '127.0.0.1');
";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rom-metrics-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn manager(dir: &Path, port: u16) -> Arc<ProcessManager> {
    let yaml = format!(
        "version: 1\nproject:\n  name: test\nservices:\n  api:\n    path: .\n    command: node\n    args: [server.js]\n    port: {port}\n"
    );
    let config: Config = serde_yaml_ng::from_str(&yaml).expect("yaml parses");
    let registry = Registry::from_config(config, dir).expect("registry builds");
    ProcessManager::new(registry, &dir.join(".rom")).expect("manager builds")
}

fn status_of(pm: &ProcessManager, name: &str) -> rom_core::process::ServiceStatus {
    pm.status()
        .into_iter()
        .find(|s| s.name == name)
        .expect("service is registered")
}

async fn wait_running(pm: &ProcessManager, name: &str) {
    timeout(Duration::from_secs(5), async {
        loop {
            if status_of(pm, name).state == ServiceState::Running {
                return;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {name} to become running"));
}

#[tokio::test]
async fn running_service_reports_working_set_and_cpu() {
    let dir = scratch("ws");
    fs::write(dir.join("server.js"), HTTP_SERVER).unwrap();
    let pm = manager(&dir, 39201);
    pm.start(&["api".into()]).await.expect("start");
    wait_running(&pm, "api").await;

    let service = status_of(&pm, "api");
    assert!(service.pid.is_some());
    assert!(service.uptime_secs.is_some());
    let working_set = service.working_set_bytes.expect("working set");
    assert!(working_set > 0, "working set should be positive");
    let cpu = service.cpu_time_secs.expect("cpu time");
    assert!(cpu >= 0.0);

    let snapshot = pm.status_snapshot();
    assert_eq!(snapshot.daemon.pid, std::process::id());
    let daemon_ws = snapshot
        .daemon
        .working_set_bytes
        .expect("daemon working set");
    assert!(daemon_ws > 0);
    // Working set is not exclusive RAM — never treat the sum as physical usage.
    let _ = working_set.saturating_add(daemon_ws);

    pm.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn connection_activity_updates_status_fields() {
    let dir = scratch("conn");
    fs::write(dir.join("server.js"), HTTP_SERVER).unwrap();
    let pm = manager(&dir, 39202);
    pm.start(&["api".into()]).await.expect("start");
    wait_running(&pm, "api").await;

    let before = status_of(&pm, "api");
    assert_eq!(before.active_connections, 0);
    assert!(before.last_activity_secs_ago.is_some());

    pm.connection_opened("api");
    let open = status_of(&pm, "api");
    assert_eq!(open.active_connections, 1);
    assert_eq!(open.last_activity_secs_ago, Some(0));

    sleep(Duration::from_millis(50)).await;
    pm.connection_closed("api");
    let closed = status_of(&pm, "api");
    assert_eq!(closed.active_connections, 0);
    assert!(closed.last_activity_secs_ago.is_some());

    pm.shutdown().await.expect("shutdown");
}
