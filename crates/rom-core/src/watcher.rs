//! Project file watcher: restarts running services when their files change.
//!
//! Watches are registered directory-by-directory so ignored trees such as
//! `node_modules` are never handed to the OS. Sleeping services are not started
//! by file changes. A git `index.lock` pauses reloads until it is gone.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, watch};
use tokio::time::sleep;

use crate::process::ProcessManager;

const IGNORED_DIR_NAMES: &[&str] = &[
    "node_modules",
    ".git",
    "dist",
    "build",
    "coverage",
    "logs",
    ".cache",
    ".rom",
];

/// Runs until `shutdown` becomes `true`.
pub async fn run(
    manager: Arc<ProcessManager>,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let roots = service_watch_roots(&manager);
    if roots.is_empty() {
        tracing::info!("watcher: no service directories to watch");
        let _ = shutdown.wait_for(|stop| *stop).await;
        return Ok(());
    }

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<PathBuf>();
    let mut watcher = RecommendedWatcher::new(
        move |result: Result<Event, notify::Error>| {
            let Ok(event) = result else {
                return;
            };
            if !is_interesting(&event.kind) {
                return;
            }
            for path in event.paths {
                let _ = event_tx.send(path);
            }
        },
        notify::Config::default(),
    )
    .map_err(io::Error::other)?;

    let mut watched = BTreeSet::new();
    for root in &roots {
        watch_tree(&mut watcher, root, &mut watched)?;
        tracing::info!(path = %root.display(), "watcher listening");
    }

    let debounce = manager.registry().watch.debounce;
    let settle = manager.registry().watch.settle;
    let project_root = manager.registry().root.clone();
    let mut dirty: BTreeSet<String> = BTreeSet::new();
    let mut batch_started: Option<Instant> = None;
    let mut last_event = Instant::now();

    loop {
        let wait = next_wait(
            batch_started,
            last_event,
            debounce,
            settle,
            !dirty.is_empty(),
        );
        tokio::select! {
            () = wait_until_true(&mut shutdown) => return Ok(()),
            path = event_rx.recv() => {
                let Some(path) = path else {
                    return Ok(());
                };
                if path_is_ignored(&path) {
                    continue;
                }
                if let Some(dir) = new_dir_to_watch(&path) {
                    let _ = watch_tree(&mut watcher, &dir, &mut watched);
                }
                if let Some(service) = service_for_path(&manager, &path) {
                    if dirty.is_empty() {
                        batch_started = Some(Instant::now());
                    }
                    dirty.insert(service);
                    last_event = Instant::now();
                }
            }
            () = sleep(wait), if !dirty.is_empty() => {
                if !ready_to_restart(batch_started, last_event, debounce, settle) {
                    continue;
                }
                if git_index_locked(&project_root) {
                    tracing::debug!("watcher: git index.lock present; reload paused");
                    continue;
                }
                let services = std::mem::take(&mut dirty);
                batch_started = None;
                for name in services {
                    if !manager.is_running(&name) {
                        tracing::debug!(service = %name, "watcher: skip sleeping service");
                        continue;
                    }
                    tracing::info!(service = %name, "watcher: restarting after file change");
                    if let Err(error) = manager.restart(&name).await {
                        tracing::warn!(service = %name, %error, "watcher: restart failed");
                    }
                }
            }
        }
    }
}

fn service_watch_roots(manager: &ProcessManager) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = manager
        .registry()
        .services()
        .map(|def| def.dir.clone())
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

fn service_for_path(manager: &ProcessManager, path: &Path) -> Option<String> {
    let path = normalize(path);
    let mut best: Option<(usize, String)> = None;
    for def in manager.registry().services() {
        let dir = normalize(&def.dir);
        if path.starts_with(&dir) {
            let len = dir.as_os_str().len();
            if best.as_ref().is_none_or(|(best_len, _)| len > *best_len) {
                best = Some((len, def.name.clone()));
            }
        }
    }
    best.map(|(_, name)| name)
}

fn normalize(path: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    strip_extended_prefix(canonical)
}

fn strip_extended_prefix(path: PathBuf) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(rest) = raw.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path
    }
}

fn is_interesting(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
    )
}

fn is_ignored_name(name: &std::ffi::OsStr) -> bool {
    IGNORED_DIR_NAMES
        .iter()
        .any(|ignored| name.eq_ignore_ascii_case(ignored))
}

fn path_is_ignored(path: &Path) -> bool {
    path.components().any(|component| match component {
        std::path::Component::Normal(name) => is_ignored_name(name),
        _ => false,
    })
}

fn new_dir_to_watch(path: &Path) -> Option<PathBuf> {
    if path.is_dir() && !path_is_ignored(path) {
        Some(path.to_path_buf())
    } else {
        None
    }
}

fn watch_tree(
    watcher: &mut RecommendedWatcher,
    root: &Path,
    watched: &mut BTreeSet<PathBuf>,
) -> io::Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let canonical = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if !watched.insert(canonical.clone()) {
            continue;
        }
        if let Err(error) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
            tracing::warn!(path = %dir.display(), %error, "watcher: could not watch directory");
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(path = %dir.display(), %error, "watcher: could not read directory");
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if is_ignored_name(&entry.file_name()) {
                continue;
            }
            stack.push(path);
        }
    }
    Ok(())
}

fn git_index_locked(project_root: &Path) -> bool {
    project_root.join(".git").join("index.lock").is_file()
}

async fn wait_until_true(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

fn next_wait(
    batch_started: Option<Instant>,
    last_event: Instant,
    debounce: Duration,
    settle: Duration,
    has_dirty: bool,
) -> Duration {
    if !has_dirty {
        return Duration::from_secs(60);
    }
    let since_last = last_event.elapsed();
    let settle_left = settle.saturating_sub(since_last);
    let debounce_left = batch_started
        .map(|started| debounce.saturating_sub(started.elapsed()))
        .unwrap_or(Duration::ZERO);
    let wait = settle_left.max(debounce_left);
    if wait.is_zero() {
        Duration::from_millis(10)
    } else {
        wait
    }
}

fn ready_to_restart(
    batch_started: Option<Instant>,
    last_event: Instant,
    debounce: Duration,
    settle: Duration,
) -> bool {
    last_event.elapsed() >= settle
        && batch_started.is_some_and(|started| started.elapsed() >= debounce)
}

/// Test helper: ignored directory names used by the watcher.
pub fn ignored_dir_names() -> &'static [&'static str] {
    IGNORED_DIR_NAMES
}

/// Test helper: whether a path is under an ignored directory segment.
pub fn is_ignored_path(path: &Path) -> bool {
    path_is_ignored(path)
}

/// Test helper: map a path to a service name using the same longest-prefix rule.
pub fn map_path_to_service(services: &BTreeMap<String, PathBuf>, path: &Path) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for (name, dir) in services {
        if path.starts_with(dir) {
            let len = dir.as_os_str().len();
            if best.as_ref().is_none_or(|(best_len, _)| len > *best_len) {
                best = Some((len, name.clone()));
            }
        }
    }
    best.map(|(_, name)| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_node_modules_segments() {
        assert!(is_ignored_path(Path::new(
            r"L:\proj\api\node_modules\x\index.js"
        )));
        assert!(is_ignored_path(Path::new(r"L:\proj\.git\HEAD")));
        assert!(!is_ignored_path(Path::new(r"L:\proj\api\src\index.js")));
    }

    #[test]
    fn picks_longest_service_prefix() {
        let mut services = BTreeMap::new();
        services.insert("api".to_owned(), PathBuf::from(r"L:\proj\api"));
        services.insert(
            "api-worker".to_owned(),
            PathBuf::from(r"L:\proj\api\worker"),
        );
        assert_eq!(
            map_path_to_service(&services, Path::new(r"L:\proj\api\worker\main.js")).as_deref(),
            Some("api-worker")
        );
        assert_eq!(
            map_path_to_service(&services, Path::new(r"L:\proj\api\src\main.js")).as_deref(),
            Some("api")
        );
    }
}
