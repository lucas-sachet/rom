//! Stage 6: monorepo discovery suggests a rom.yaml without inventing deps.
use std::fs;
use std::path::PathBuf;

use rom_core::discover::{self, scan};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rom-discover-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

#[test]
fn discovers_node_services_with_ports_and_leaves_dependencies_empty() {
    let root = scratch("mono");
    fs::write(
        root.join("package.json"),
        r#"{"name":"mono","private":true,"workspaces":["services/*"]}"#,
    )
    .unwrap();

    let user = root.join("services").join("user-service");
    fs::create_dir_all(&user).unwrap();
    fs::write(
        user.join("package.json"),
        r#"{"name":"user-service","scripts":{"start":"node server.js"}}"#,
    )
    .unwrap();
    fs::write(
        user.join("server.js"),
        "const http = require('http');\nhttp.createServer().listen(process.env.PORT || 3001);\n",
    )
    .unwrap();

    let finance = root.join("services").join("finance-service");
    fs::create_dir_all(&finance).unwrap();
    fs::write(
        finance.join("package.json"),
        r#"{"name":"finance-service"}"#,
    )
    .unwrap();
    fs::write(
        finance.join("server.ts"),
        "const port = process.env.PORT ?? 3002;\n",
    )
    .unwrap();

    // Ignored tree.
    let nm = user.join("node_modules").join("leftpad");
    fs::create_dir_all(&nm).unwrap();
    fs::write(nm.join("package.json"), r#"{"name":"leftpad"}"#).unwrap();
    fs::write(nm.join("server.js"), "listen(9999)\n").unwrap();

    // No port → skipped.
    let orphan = root.join("services").join("orphan-service");
    fs::create_dir_all(&orphan).unwrap();
    fs::write(orphan.join("package.json"), r#"{"name":"orphan-service"}"#).unwrap();
    fs::write(orphan.join("server.js"), "console.log('no port')\n").unwrap();

    let discovery = scan(&root).expect("scan succeeds");
    assert_eq!(discovery.services.len(), 2);
    assert!(
        discovery
            .skipped
            .iter()
            .any(|s| s.contains("orphan-service"))
    );

    let user_svc = discovery
        .services
        .iter()
        .find(|s| s.name == "user-service")
        .expect("user-service");
    assert_eq!(user_svc.port, 3001);
    assert_eq!(user_svc.command, "node");
    assert_eq!(user_svc.args, vec!["server.js"]);

    let finance_svc = discovery
        .services
        .iter()
        .find(|s| s.name == "finance-service")
        .expect("finance-service");
    assert_eq!(finance_svc.port, 3002);
    assert_eq!(finance_svc.args[0], "-r");
    assert_eq!(finance_svc.args[1], "ts-node/register");
    assert_eq!(finance_svc.args[2], "server.ts");

    let yaml = discover::render_yaml(&discovery);
    assert!(yaml.contains("dependencies: []"));
    assert!(!yaml.contains("leftpad"));
    assert!(yaml.contains("user-service:"));
    assert!(yaml.contains("finance-service:"));
}

#[test]
fn does_not_merge_similar_packages_into_one_service() {
    let root = scratch("similar");
    for (name, port) in [("api-a", 3101), ("api-b", 3102)] {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("package.json"), format!(r#"{{"name":"{name}"}}"#)).unwrap();
        fs::write(
            dir.join("server.js"),
            format!("listen(process.env.PORT || {port})\n"),
        )
        .unwrap();
    }

    let discovery = scan(&root).expect("scan succeeds");
    assert_eq!(discovery.services.len(), 2);
    let names: Vec<_> = discovery.services.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["api-a", "api-b"]);
}
