//! Stage 5: CTRL_BREAK graceful stop, with Job Object fallback.
#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rom_core::config::Config;
use rom_core::process::{ProcessManager, ServiceState};
use rom_core::registry::Registry;
use tokio::time::sleep;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rom-grace-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn yaml(port: u16, shutdown: &str) -> String {
    format!(
        "version: 1\nproject:\n  name: test\nruntime:\n  shutdown_timeout: {shutdown}\nservices:\n  api:\n    path: .\n    command: node\n    args: [server.js]\n    port: {port}\n"
    )
}

fn manager(dir: &Path, raw: &str) -> std::sync::Arc<ProcessManager> {
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

fn graceful_server(marker: &Path) -> String {
    let marker = marker.to_string_lossy().replace('\\', "\\\\");
    format!(
        r#"const http = require('http');
const fs = require('fs');
function onStop(kind) {{
  try {{ fs.writeFileSync("{marker}", kind); }} catch (_) {{}}
  process.exit(0);
}}
process.on('SIGBREAK', () => onStop('break'));
process.on('SIGINT', () => onStop('int'));
http.createServer((req, res) => res.end('ok')).listen(Number(process.env.PORT), '127.0.0.1');
"#
    )
}

fn stubborn_server() -> String {
    r#"const http = require('http');
process.on('SIGBREAK', () => {});
process.on('SIGINT', () => {});
http.createServer((req, res) => res.end('ok')).listen(Number(process.env.PORT), '127.0.0.1');
"#
    .to_owned()
}

#[tokio::test]
async fn ctrl_break_lets_node_exit_gracefully() {
    let dir = scratch("ok");
    let marker = dir.join("graceful.exit");
    fs::write(dir.join("server.js"), graceful_server(&marker)).unwrap();
    let pm = manager(&dir, &yaml(39150, "5s"));

    pm.start(&["api".to_owned()]).await.expect("start");
    assert_eq!(state(&pm, "api"), ServiceState::Running);
    sleep(Duration::from_millis(200)).await;

    pm.stop(&["api".to_owned()], false).await.expect("stop");
    assert_eq!(state(&pm, "api"), ServiceState::Sleeping);
    let body = fs::read_to_string(&marker).unwrap_or_else(|error| {
        let log = fs::read_to_string(dir.join(".rom").join("logs").join("api.log"))
            .unwrap_or_default();
        panic!("graceful marker missing: {error}; log:\n{log}");
    });
    assert!(
        body.trim() == "break" || body.trim() == "int",
        "unexpected marker {body:?}"
    );
}

#[tokio::test]
async fn stubborn_process_is_killed_after_shutdown_timeout() {
    let dir = scratch("stubborn");
    fs::write(dir.join("server.js"), stubborn_server()).unwrap();
    let pm = manager(&dir, &yaml(39151, "500ms"));

    pm.start(&["api".to_owned()]).await.expect("start");
    assert_eq!(state(&pm, "api"), ServiceState::Running);
    sleep(Duration::from_millis(200)).await;

    let started = std::time::Instant::now();
    pm.stop(&["api".to_owned()], false).await.expect("stop");
    let elapsed = started.elapsed();
    assert_eq!(state(&pm, "api"), ServiceState::Sleeping);
    assert!(
        elapsed >= Duration::from_millis(400),
        "expected to wait for shutdown_timeout, elapsed={elapsed:?}"
    );
}
