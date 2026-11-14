//! Scan a monorepo and suggest a `rom.yaml`.
//!
//! Heuristics are intentionally conservative: each directory with a runnable
//! Node entrypoint and a detectable port becomes one service. Dependencies are
//! left empty — the developer confirms the draft. Similar-looking code is never
//! merged into one process.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::RomError;

const IGNORED_DIR_NAMES: &[&str] = &[
    "node_modules",
    ".git",
    "dist",
    "build",
    "coverage",
    "logs",
    ".cache",
    ".rom",
    "target",
    ".next",
    ".turbo",
    ".yarn",
    ".pnpm-store",
];

const ENTRY_CANDIDATES: &[&str] = &[
    "server.js",
    "server.ts",
    "src/server.js",
    "src/server.ts",
    "index.js",
    "index.ts",
    "src/index.js",
    "src/index.ts",
    "app.js",
    "app.ts",
    "src/app.js",
    "src/app.ts",
];

/// One suggested service from a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestedService {
    pub name: String,
    pub path: String,
    pub command: String,
    pub args: Vec<String>,
    pub port: u16,
    pub notes: Vec<String>,
}

/// Result of scanning a project root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub project_name: String,
    pub root: PathBuf,
    pub services: Vec<SuggestedService>,
    pub skipped: Vec<String>,
}

/// Walk `root` looking for Node services. Does not read or write `rom.yaml`.
pub fn scan(root: &Path) -> Result<Discovery, RomError> {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let project_name = root
        .file_name()
        .map(|name| slugify(&name.to_string_lossy()))
        .filter(|name| is_valid_service_name(name))
        .unwrap_or_else(|| "my-project".to_owned());

    let mut package_dirs = Vec::new();
    collect_package_dirs(&root, &root, 0, &mut package_dirs)?;
    // Single-package repos: the root itself may be the only service.
    if package_dirs.is_empty() && root.join("package.json").is_file() {
        package_dirs.push(root.clone());
    }

    let mut used_ports = BTreeSet::new();
    let mut used_names = BTreeSet::new();
    let mut services = Vec::new();
    let mut skipped = Vec::new();

    for dir in package_dirs {
        match inspect_package(&root, &dir) {
            Ok(Some(mut suggested)) => {
                if !used_names.insert(suggested.name.clone()) {
                    skipped.push(format!(
                        "{}: duplicate service name `{}`",
                        display_rel(&root, &dir),
                        suggested.name
                    ));
                    continue;
                }
                if !used_ports.insert(suggested.port) {
                    suggested.notes.push(format!(
                        "port {} also used by another discovered service — confirm",
                        suggested.port
                    ));
                }
                services.push(suggested);
            }
            Ok(None) => {}
            Err(message) => skipped.push(format!("{}: {message}", display_rel(&root, &dir))),
        }
    }

    services.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Discovery {
        project_name,
        root,
        services,
        skipped,
    })
}

/// Render a discovery result as `rom.yaml` text.
pub fn render_yaml(discovery: &Discovery) -> String {
    let mut out = String::new();
    out.push_str("# Suggested by `rom discover`. Review before use.\n");
    out.push_str("# Dependencies were left empty on purpose — fill them in yourself.\n");
    out.push_str("version: 1\n\n");
    out.push_str("project:\n");
    out.push_str(&format!("  name: {}\n", discovery.project_name));
    out.push_str("  root: .\n\n");
    out.push_str("runtime:\n");
    out.push_str("  startup_timeout: 30s\n");
    out.push_str("  shutdown_timeout: 30s\n");
    out.push_str("  idle_timeout: 30s\n");
    out.push_str("  connection_idle: 5s\n\n");
    out.push_str("watch:\n");
    out.push_str("  debounce: 300ms\n");
    out.push_str("  settle: 500ms\n\n");
    out.push_str("services:\n");
    if discovery.services.is_empty() {
        out.push_str("  {}\n");
    } else {
        for service in &discovery.services {
            for note in &service.notes {
                out.push_str(&format!("  # note: {note}\n"));
            }
            out.push_str(&format!("  {}:\n", service.name));
            out.push_str(&format!("    path: {}\n", service.path));
            out.push_str(&format!("    command: {}\n", service.command));
            out.push_str(&format!("    args: [{}]\n", yaml_string_list(&service.args)));
            out.push_str(&format!("    port: {}\n", service.port));
            out.push_str("    dependencies: []\n");
        }
    }
    out.push_str("\nprofiles: {}\n");
    if !discovery.skipped.is_empty() {
        out.push('\n');
        out.push_str("# Skipped during discovery:\n");
        for item in &discovery.skipped {
            out.push_str(&format!("# - {item}\n"));
        }
    }
    out
}

fn collect_package_dirs(
    root: &Path,
    dir: &Path,
    depth: usize,
    out: &mut Vec<PathBuf>,
) -> Result<(), RomError> {
    if depth > 6 {
        return Ok(());
    }
    if dir.join("package.json").is_file() && dir != root {
        out.push(dir.to_path_buf());
    }
    let entries = fs::read_dir(dir).map_err(RomError::Io)?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if is_ignored_name(&entry.file_name()) {
            continue;
        }
        collect_package_dirs(root, &path, depth + 1, out)?;
    }
    Ok(())
}

fn inspect_package(root: &Path, dir: &Path) -> Result<Option<SuggestedService>, String> {
    let package_text = fs::read_to_string(dir.join("package.json"))
        .map_err(|error| format!("could not read package.json: {error}"))?;
    let package: Value = serde_json::from_str(&package_text)
        .map_err(|error| format!("invalid package.json: {error}"))?;

    // Workspace roots are containers, not services.
    if package.get("workspaces").is_some() {
        return Ok(None);
    }

    let rel = display_rel(root, dir);
    let name = service_name_for(dir, &package)?;
    let (entrypoint, mut notes) = find_entrypoint(dir, &package)?;
    let Some(port) = find_port(dir, &entrypoint) else {
        return Err(format!(
            "no default PORT found in {} (set one in rom.yaml manually)",
            entrypoint.display()
        ));
    };

    let (command, args) = command_for_entrypoint(dir, &entrypoint);
    if entrypoint.extension().is_some_and(|ext| ext == "ts") {
        notes.push("TypeScript entrypoint — uses node -r ts-node/register".to_owned());
    }

    Ok(Some(SuggestedService {
        name,
        path: if rel == "." {
            ".".to_owned()
        } else {
            format!("./{rel}")
        },
        command,
        args,
        port,
        notes,
    }))
}

fn service_name_for(dir: &Path, package: &Value) -> Result<String, String> {
    let from_dir = dir
        .file_name()
        .map(|name| slugify(&name.to_string_lossy()))
        .unwrap_or_default();
    if is_valid_service_name(&from_dir) {
        return Ok(from_dir);
    }
    if let Some(name) = package.get("name").and_then(Value::as_str) {
        let slug = slugify(name.rsplit('/').next().unwrap_or(name));
        if is_valid_service_name(&slug) {
            return Ok(slug);
        }
    }
    Err(format!(
        "directory name `{}` is not a valid rom service name",
        dir.file_name().map_or_else(|| "?".into(), |n| n.to_string_lossy().into_owned())
    ))
}

fn find_entrypoint(dir: &Path, package: &Value) -> Result<(PathBuf, Vec<String>), String> {
    let mut notes = Vec::new();

    if let Some(start) = package
        .pointer("/scripts/start")
        .and_then(Value::as_str)
        .map(str::trim)
    {
        if let Some(path) = entrypoint_from_start_script(dir, start) {
            notes.push(format!("from package.json scripts.start: {start}"));
            return Ok((path, notes));
        }
    }

    for candidate in ENTRY_CANDIDATES {
        let path = dir.join(candidate);
        if path.is_file() {
            return Ok((path, notes));
        }
    }

    if let Some(main) = package.get("main").and_then(Value::as_str) {
        let path = dir.join(main);
        if path.is_file() {
            notes.push(format!("from package.json main: {main}"));
            return Ok((path, notes));
        }
    }

    Err("no server entrypoint found (server.js/ts, index.js/ts, or scripts.start)".to_owned())
}

fn entrypoint_from_start_script(dir: &Path, start: &str) -> Option<PathBuf> {
    // Examples: `node server.js`, `node -r ts-node/register server.ts`, `tsx src/server.ts`
    let tokens: Vec<&str> = start.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    // Skip package-manager wrappers; rom cannot spawn .cmd shims directly.
    if matches!(tokens[0], "npm" | "npx" | "yarn" | "pnpm" | "nodemon") {
        return None;
    }
    let file = tokens.iter().rev().find(|token| {
        token.ends_with(".js")
            || token.ends_with(".ts")
            || token.ends_with(".mjs")
            || token.ends_with(".cjs")
    })?;
    let path = dir.join(file);
    path.is_file().then_some(path)
}

fn command_for_entrypoint(service_dir: &Path, entrypoint: &Path) -> (String, Vec<String>) {
    let rel = entrypoint
        .strip_prefix(service_dir)
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| {
            entrypoint
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "server.js".to_owned())
        });

    if entrypoint.extension().is_some_and(|ext| ext == "ts") {
        (
            "node".to_owned(),
            vec!["-r".to_owned(), "ts-node/register".to_owned(), rel],
        )
    } else {
        ("node".to_owned(), vec![rel])
    }
}

fn find_port(dir: &Path, entrypoint: &Path) -> Option<u16> {
    let mut files = vec![entrypoint.to_path_buf()];
    for name in ["config.js", "config.ts", "src/config.js", "src/config.ts", ".env", "env.local"] {
        let path = dir.join(name);
        if path.is_file() {
            files.push(path);
        }
    }

    for path in files {
        if let Ok(text) = fs::read_to_string(&path) {
            if let Some(port) = extract_port(&text) {
                return Some(port);
            }
        }
    }
    None
}

fn extract_port(text: &str) -> Option<u16> {
    // process.env.PORT || 3001  /  process.env.PORT ?? 3001
    for pattern in [
        "process.env.PORT || ",
        "process.env.PORT||",
        "process.env.PORT ?? ",
        "process.env.PORT??",
        "PORT=",
        "PORT: ",
        "port: ",
    ] {
        if let Some(idx) = text.find(pattern) {
            let rest = &text[idx + pattern.len()..];
            if let Some(port) = parse_leading_port(rest) {
                return Some(port);
            }
        }
    }
    // listen(3001) / listen(Number(process.env.PORT) || 3001) already covered;
    // also bare listen(3xxx) as a last resort when it's a literal.
    if let Some(idx) = text.find(".listen(") {
        let rest = &text[idx + ".listen(".len()..];
        if rest.starts_with("Number(") || rest.starts_with("process.env") || rest.starts_with('+') {
            // fall through
        } else if let Some(port) = parse_leading_port(rest) {
            return Some(port);
        }
    }
    None
}

fn parse_leading_port(input: &str) -> Option<u16> {
    let trimmed = input.trim_start();
    let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let port: u16 = digits.parse().ok()?;
    (port > 0).then_some(port)
}

fn slugify(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
        } else if matches!(lower, '-' | '_' | ' ' | '.' | '/' | '@')
            && !out.ends_with('-')
            && !out.is_empty()
        {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

fn is_valid_service_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_ignored_name(name: &std::ffi::OsStr) -> bool {
    IGNORED_DIR_NAMES
        .iter()
        .any(|ignored| name.eq_ignore_ascii_case(ignored))
}

fn display_rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.display().to_string())
}

fn yaml_string_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("\"{item}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_ports_from_common_patterns() {
        assert_eq!(extract_port("const port = process.env.PORT || 3001;"), Some(3001));
        assert_eq!(extract_port("PORT=4010\n"), Some(4010));
        assert_eq!(extract_port("app.listen(3088);"), Some(3088));
        assert_eq!(extract_port("app.listen(Number(process.env.PORT));"), None);
    }

    #[test]
    fn slugifies_scoped_package_names() {
        assert_eq!(slugify("@tex/user-service"), "tex-user-service");
        assert_eq!(slugify("User_Service"), "user-service");
    }

    #[test]
    fn renders_empty_dependencies() {
        let discovery = Discovery {
            project_name: "demo".to_owned(),
            root: PathBuf::from("."),
            services: vec![SuggestedService {
                name: "api".to_owned(),
                path: "./api".to_owned(),
                command: "node".to_owned(),
                args: vec!["server.js".to_owned()],
                port: 3001,
                notes: vec![],
            }],
            skipped: vec![],
        };
        let yaml = render_yaml(&discovery);
        assert!(yaml.contains("api:"));
        assert!(yaml.contains("dependencies: []"));
        assert!(yaml.contains("port: 3001"));
    }
}
