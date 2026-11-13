//! Integration tests: real Node.js processes managed through `ProcessManager`.
#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rom_core::RomError;
use rom_core::config::Config;
use rom_core::process::{ProcessManager, ServiceState};
use rom_core::registry::Registry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const HTTP_SERVER: &str = "const http = require('http');
http.createServer((req, res) => res.end('ok')).listen(Number(process.env.PORT), '127.0.0.1');
";

/// Starts an HTTP server and a grandchild process, then records the grandchild's PID.
const HTTP_SERVER_WITH_CHILD: &str = "const http = require('http');
const fs = require('fs');
const { spawn } = require('child_process');
const child = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { stdio: 'ignore' });
fs.writeFileSync('child.pid', String(child.pid));
http.createServer((req, res) => res.end('ok')).listen(Number(process.env.PORT), '127.0.0.1');
";

const CRASHING_SERVER: &str = "process.exit(3);\n";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rom-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Services are `(name, port, dependencies)`. Every service runs `node server.js` in `dir`.
fn config_yaml(services: &[(&str, u16, &str)]) -> String {
    let mut yaml = String::from("version: 1\nproject:\n  name: test\nservices:\n");
    for (name, port, deps) in services {
        yaml += &format!(
            "  {name}:\n    path: .\n    command: node\n    args: [server.js]\n    port: {port}\n    dependencies: [{deps}]\n"
        );
    }
    yaml
}

fn manager(dir: &Path, yaml: &str) -> ProcessManager {
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

async fn http_get_ok(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)).await else {
        return false;
    };
    let request = b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    if stream.write_all(request).await.is_err() {
        return false;
    }
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response).await;
    response.starts_with("HTTP/1.1 200") && response.ends_with("ok")
}

async fn port_is_closed(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port)).await.is_err()
}

fn process_alive(pid: u32) -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .expect("tasklist runs");
    String::from_utf8_lossy(&output.stdout).contains(&format!(" {pid} "))
}

#[tokio::test]
async fn starts_serves_and_stops_a_service() {
    let dir = scratch("lifecycle");
    fs::write(dir.join("server.js"), HTTP_SERVER).unwrap();
    let pm = manager(&dir, &config_yaml(&[("api", 39101, "")]));

    let started = pm.start(&["api".to_owned()]).await.expect("start succeeds");
    assert_eq!(started, vec!["api"]);
    assert_eq!(state(&pm, "api"), ServiceState::Running);
    assert!(http_get_ok(39101).await, "service answers HTTP");

    pm.stop(&["api".to_owned()], false).await.expect("stop succeeds");
    assert_eq!(state(&pm, "api"), ServiceState::Sleeping);
    assert!(port_is_closed(39101).await, "port is released after stop");
}

#[tokio::test]
async fn stopping_kills_the_whole_process_tree() {
    let dir = scratch("tree");
    fs::write(dir.join("server.js"), HTTP_SERVER_WITH_CHILD).unwrap();
    let pm = manager(&dir, &config_yaml(&[("tree", 39102, "")]));

    pm.start(&["tree".to_owned()]).await.expect("start succeeds");
    let pid: u32 = fs::read_to_string(dir.join("child.pid"))
        .expect("child pid written")
        .trim()
        .parse()
        .unwrap();
    assert!(process_alive(pid), "grandchild is running before stop");

    pm.stop(&["tree".to_owned()], false).await.expect("stop succeeds");
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_alive(pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!process_alive(pid), "grandchild was terminated with its parent");
}

#[tokio::test]
async fn a_service_that_exits_during_startup_fails() {
    let dir = scratch("crash");
    fs::write(dir.join("server.js"), CRASHING_SERVER).unwrap();
    let pm = manager(&dir, &config_yaml(&[("broken", 39103, "")]));

    let error = pm
        .start(&["broken".to_owned()])
        .await
        .expect_err("start must fail");
    assert!(
        matches!(error, RomError::StartFailed { .. }),
        "unexpected error: {error}"
    );
    assert_eq!(state(&pm, "broken"), ServiceState::Failed);
}

#[tokio::test]
async fn dependencies_start_first_and_block_unforced_stop() {
    let dir = scratch("deps");
    fs::write(dir.join("server.js"), HTTP_SERVER).unwrap();
    let pm = manager(
        &dir,
        &config_yaml(&[("user", 39104, ""), ("finance", 39105, "user")]),
    );

    pm.start(&["finance".to_owned()]).await.expect("start succeeds");
    assert_eq!(state(&pm, "user"), ServiceState::Running);
    assert_eq!(state(&pm, "finance"), ServiceState::Running);

    let error = pm
        .stop(&["user".to_owned()], false)
        .await
        .expect_err("stopping a required dependency must be refused");
    assert!(matches!(error, RomError::StillRequired { .. }), "unexpected error: {error}");
    assert_eq!(state(&pm, "user"), ServiceState::Running);

    pm.stop(&["user".to_owned()], true).await.expect("forced stop succeeds");
    assert_eq!(state(&pm, "user"), ServiceState::Sleeping);
    assert_eq!(state(&pm, "finance"), ServiceState::Sleeping);

    pm.shutdown().await.expect("shutdown succeeds");
}

#[tokio::test]
async fn stopping_a_whole_chain_in_one_request_needs_no_force() {
    let dir = scratch("chain-stop");
    fs::write(dir.join("server.js"), HTTP_SERVER).unwrap();
    let pm = manager(
        &dir,
        &config_yaml(&[("user", 39106, ""), ("finance", 39107, "user")]),
    );

    pm.start(&["finance".to_owned()]).await.expect("start succeeds");
    let names = ["user".to_owned(), "finance".to_owned()];
    pm.stop(&names, false).await.expect("stopping the whole chain succeeds");
    assert_eq!(state(&pm, "user"), ServiceState::Sleeping);
    assert_eq!(state(&pm, "finance"), ServiceState::Sleeping);
}
