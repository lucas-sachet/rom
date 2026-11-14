//! Stage 4: file watcher restarts running services and ignores sleeping ones.
#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rom_core::config::Config;
use rom_core::process::{ProcessManager, ServiceState};
use rom_core::registry::Registry;
use rom_core::watcher;
use tokio::sync::watch;
use tokio::time::{sleep, timeout};

const HTTP_SERVER: &str = "const http = require('http');
http.createServer((req, res) => res.end(process.env.ROM_SERVICE || 'ok'))
  .listen(Number(process.env.PORT), '127.0.0.1');
";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rom-watch-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn yaml(port: u16) -> String {
    format!(
        "version: 1\nproject:\n  name: test\n  root: .\nwatch:\n  debounce: 50ms\n  settle: 80ms\nruntime:\n  idle_timeout: 30s\nservices:\n  api:\n    path: ./api\n    command: node\n    args: [server.js]\n    port: {port}\n"
    )
}

fn manager(dir: &Path, raw: &str) -> Arc<ProcessManager> {
    let config: Config = serde_yaml_ng::from_str(raw).expect("yaml parses");
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

fn restart_count(pm: &ProcessManager, name: &str) -> u32 {
    pm.status()
        .into_iter()
        .find(|s| s.name == name)
        .expect("service is registered")
        .restart_count
}

async fn wait_restarts(pm: &ProcessManager, name: &str, min: u32) {
    timeout(Duration::from_secs(8), async {
        loop {
            if restart_count(pm, name) >= min {
                return;
            }
            sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {name} restarts >= {min}"));
}

async fn wait_state(pm: &ProcessManager, name: &str, want: ServiceState) {
    timeout(Duration::from_secs(8), async {
        loop {
            if state(pm, name) == want {
                return;
            }
            sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {name} to become {want}"));
}

#[tokio::test]
async fn running_service_restarts_on_file_change() {
    let dir = scratch("restart");
    let api = dir.join("api");
    fs::create_dir_all(&api).unwrap();
    fs::write(api.join("server.js"), HTTP_SERVER).unwrap();
    let pm = manager(&dir, &yaml(39140));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let watcher_pm = Arc::clone(&pm);
    let watcher_task = tokio::spawn(async move { watcher::run(watcher_pm, shutdown_rx).await });
    sleep(Duration::from_millis(100)).await;

    pm.start(&["api".to_owned()]).await.expect("start");
    assert_eq!(state(&pm, "api"), ServiceState::Running);
    assert_eq!(restart_count(&pm, "api"), 0);

    fs::write(api.join("server.js"), format!("{HTTP_SERVER}\n// touch\n")).unwrap();
    wait_restarts(&pm, "api", 1).await;
    wait_state(&pm, "api", ServiceState::Running).await;

    let _ = shutdown_tx.send(true);
    watcher_task.await.expect("join").expect("watcher");
    pm.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn sleeping_service_is_not_started_by_file_change() {
    let dir = scratch("sleep");
    let api = dir.join("api");
    fs::create_dir_all(&api).unwrap();
    fs::write(api.join("server.js"), HTTP_SERVER).unwrap();
    let pm = manager(&dir, &yaml(39141));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let watcher_pm = Arc::clone(&pm);
    let watcher_task = tokio::spawn(async move { watcher::run(watcher_pm, shutdown_rx).await });
    sleep(Duration::from_millis(100)).await;

    assert_eq!(state(&pm, "api"), ServiceState::Sleeping);
    fs::write(api.join("server.js"), format!("{HTTP_SERVER}\n// sleep-touch\n")).unwrap();
    sleep(Duration::from_millis(500)).await;
    assert_eq!(state(&pm, "api"), ServiceState::Sleeping);
    assert_eq!(restart_count(&pm, "api"), 0);

    let _ = shutdown_tx.send(true);
    watcher_task.await.expect("join").expect("watcher");
}

#[tokio::test]
async fn git_index_lock_pauses_reload() {
    let dir = scratch("gitlock");
    let api = dir.join("api");
    fs::create_dir_all(&api).unwrap();
    fs::create_dir_all(dir.join(".git")).unwrap();
    fs::write(api.join("server.js"), HTTP_SERVER).unwrap();
    let pm = manager(&dir, &yaml(39142));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let watcher_pm = Arc::clone(&pm);
    let watcher_task = tokio::spawn(async move { watcher::run(watcher_pm, shutdown_rx).await });
    sleep(Duration::from_millis(100)).await;

    pm.start(&["api".to_owned()]).await.expect("start");
    fs::write(dir.join(".git").join("index.lock"), "locked").unwrap();
    fs::write(api.join("server.js"), format!("{HTTP_SERVER}\n// locked\n")).unwrap();
    sleep(Duration::from_millis(500)).await;
    assert_eq!(restart_count(&pm, "api"), 0);

    fs::remove_file(dir.join(".git").join("index.lock")).unwrap();
    wait_restarts(&pm, "api", 1).await;

    let _ = shutdown_tx.send(true);
    watcher_task.await.expect("join").expect("watcher");
    pm.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn changes_under_node_modules_are_ignored() {
    let dir = scratch("nm");
    let api = dir.join("api");
    let nm = api.join("node_modules").join("pkg");
    fs::create_dir_all(&nm).unwrap();
    fs::write(api.join("server.js"), HTTP_SERVER).unwrap();
    let pm = manager(&dir, &yaml(39143));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let watcher_pm = Arc::clone(&pm);
    let watcher_task = tokio::spawn(async move { watcher::run(watcher_pm, shutdown_rx).await });
    sleep(Duration::from_millis(100)).await;

    pm.start(&["api".to_owned()]).await.expect("start");
    fs::write(nm.join("index.js"), "module.exports = 1;\n").unwrap();
    sleep(Duration::from_millis(500)).await;
    assert_eq!(restart_count(&pm, "api"), 0);

    let _ = shutdown_tx.send(true);
    watcher_task.await.expect("join").expect("watcher");
    pm.shutdown().await.expect("shutdown");
}
