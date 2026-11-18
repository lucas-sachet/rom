use std::fs::{self, File};
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write, stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rom_core::config::{self, find_config};
use rom_core::console;
use rom_core::detach::spawn_detached;
use rom_core::discover;
use rom_core::gateway;
use rom_core::ipc::{self, Request, Response, pipe_name};
use rom_core::metrics;
use rom_core::process::{ProcessManager, ServiceState, ServiceStatus, read_tail};
use rom_core::registry::Registry;
use rom_core::watcher;
use rom_core::{RomError, server};
use tokio::sync::watch;
use tokio::time::sleep;
use tracing_subscriber::EnvFilter;

const CONFIG_TEMPLATE_HEAD: &str = r#"version: 1

project:
  name: {{PROJECT}}
  root: .

runtime:
  startup_timeout: 30s
  shutdown_timeout: 30s
  idle_timeout: 30s
  connection_idle: 5s
"#;

const CONFIG_TEMPLATE_TAIL: &str = r#"
watch:
  debounce: 300ms
  settle: 500ms

# Services go under `services:`. Example:
#
#  user-service:
#    path: ./user-service
#    command: node
#    args: [server.js]
#    port: 3001
#    mode: on-demand    # or `persistent` (never idle-stopped)
#    health:            # optional; without it, readiness is "port accepts connections"
#      type: http
#      path: /health
#    dependencies: []
#
services: {}

profiles: {}
"#;

fn config_template() -> String {
    let mut template = String::with_capacity(
        CONFIG_TEMPLATE_HEAD.len()
            + rom_core::config::PREFIX_COLORS_YAML_HINT.len()
            + CONFIG_TEMPLATE_TAIL.len(),
    );
    template.push_str(CONFIG_TEMPLATE_HEAD);
    template.push_str(rom_core::config::PREFIX_COLORS_YAML_HINT);
    template.push_str(CONFIG_TEMPLATE_TAIL);
    template
}

#[derive(Parser)]
#[command(
    name = "rom",
    version,
    about = "Runtime Orchestrator for Microservices"
)]
struct Cli {
    /// Path to rom.yaml. Defaults to the nearest rom.yaml in the current directory or its parents.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a rom.yaml in the current directory.
    Init,
    /// Scan the monorepo and suggest a rom.yaml (does not overwrite unless forced).
    Discover {
        /// Directory to scan. Defaults to the current directory.
        #[arg(long)]
        root: Option<PathBuf>,
        /// Write the suggestion to rom.yaml (fails if it already exists).
        #[arg(long)]
        write: bool,
        /// Allow `--write` to replace an existing rom.yaml.
        #[arg(long)]
        force: bool,
    },
    /// Validate rom.yaml without starting anything.
    Check,
    /// Run the daemon in this terminal, stream logs, and optionally start services.
    /// Ctrl+C stops everything. Prefer this for day-to-day work.
    Up {
        services: Vec<String>,
        #[arg(short, long = "profile")]
        profiles: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// Start services via a background daemon (returns immediately; logs go to `.rom/logs`).
    Start {
        services: Vec<String>,
        #[arg(short, long = "profile")]
        profiles: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// Stop services.
    Stop {
        services: Vec<String>,
        #[arg(short, long = "profile")]
        profiles: Vec<String>,
        #[arg(long)]
        all: bool,
        /// Also stop running services that depend on these.
        #[arg(long)]
        force: bool,
    },
    /// Restart one service. Its dependencies keep running.
    Restart { service: String },
    /// Show the state of every service.
    Status,
    /// Print a service's log.
    Logs {
        service: String,
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
        #[arg(short, long)]
        follow: bool,
    },
    /// Manage the background daemon for this project.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start the daemon in the background.
    Start,
    /// Stop all services and the daemon.
    Stop,
    /// Run the daemon in the foreground (useful for debugging).
    Run,
}

struct Project {
    config_path: PathBuf,
    config_dir: PathBuf,
    state_dir: PathBuf,
    pipe: String,
}

impl Project {
    fn locate(explicit: Option<PathBuf>) -> Result<Self> {
        let config_path = match explicit {
            Some(path) => std::path::absolute(path)?,
            None => {
                let cwd = std::env::current_dir()?;
                find_config(&cwd)
                    .ok_or_else(|| RomError::ConfigNotFound(cwd.display().to_string()))?
            }
        };
        let config_dir = config_path
            .parent()
            .map(Path::to_path_buf)
            .context("config path has no parent directory")?;
        Ok(Self {
            state_dir: config_dir.join(".rom"),
            pipe: pipe_name(&config_dir),
            config_dir,
            config_path,
        })
    }

    fn registry(&self) -> Result<Registry> {
        let config = config::load(&self.config_path)?;
        Ok(Registry::from_config(config, &self.config_dir)?)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if console::maybe_run_as_helper() {
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("ROM_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Cmd::Init => init(),
        Cmd::Discover { root, write, force } => discover_cmd(root, write, force),
        Cmd::Daemon { action } => {
            let project = Project::locate(cli.config)?;
            daemon(&project, action).await
        }
        Cmd::Up {
            services,
            profiles,
            all,
        } => {
            let project = Project::locate(cli.config)?;
            up(&project, services, profiles, all).await
        }
        command => {
            let project = Project::locate(cli.config)?;
            run(&project, command).await
        }
    }
}

fn init() -> Result<()> {
    let path = std::env::current_dir()?.join("rom.yaml");
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    let name = path.parent().and_then(Path::file_name).map_or_else(
        || "my-project".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    fs::write(&path, config_template().replace("{{PROJECT}}", &name))?;
    println!("created {}", path.display());
    Ok(())
}

fn discover_cmd(root: Option<PathBuf>, write: bool, force: bool) -> Result<()> {
    let root = match root {
        Some(path) => std::path::absolute(path)?,
        None => std::env::current_dir()?,
    };
    let discovery = discover::scan(&root)?;
    let yaml = discover::render_yaml(&discovery);

    for skipped in &discovery.skipped {
        eprintln!("skip: {skipped}");
    }
    eprintln!(
        "discovered {} service(s) under {}",
        discovery.services.len(),
        root.display()
    );

    if !write {
        print!("{yaml}");
        return Ok(());
    }

    let path = root.join("rom.yaml");
    if path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite, or omit --write to print the suggestion",
            path.display()
        );
    }
    fs::write(&path, yaml)?;
    println!("wrote {}", path.display());
    Ok(())
}

async fn up(
    project: &Project,
    services: Vec<String>,
    profiles: Vec<String>,
    all: bool,
) -> Result<()> {
    if is_running(project).await {
        bail!(
            "a background daemon is already running for this project; stop it with `rom daemon stop` then retry `rom up`"
        );
    }
    let registry = project.registry()?;
    let names = if services.is_empty() && profiles.is_empty() && !all {
        Vec::new()
    } else {
        select(&registry, services, &profiles, all)?
    };

    fs::create_dir_all(project.state_dir.join("logs"))?;
    println!(
        "rom up — logs stream here; Ctrl+C stops the daemon and every service ({})",
        project.config_dir.display()
    );

    let service_names = registry.names();
    let use_color = registry.prefix_colors && stdout_supports_color();
    if use_color {
        console::enable_ansi_stdout();
    }

    let log_dir = project.state_dir.join("logs");
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let tail_shutdown = shutdown_rx.clone();
    let tail = tokio::spawn(async move {
        follow_log_dir(log_dir, tail_shutdown, service_names, use_color).await
    });

    let start_names = names.clone();
    let start_pipe = project.pipe.clone();
    let starter = tokio::spawn(async move {
        // Wait until the in-process daemon accepts IPC.
        for _ in 0..100 {
            sleep(Duration::from_millis(50)).await;
            if ipc::request(&start_pipe, &Request::Status).await.is_ok() {
                break;
            }
        }
        if start_names.is_empty() {
            println!(
                "daemon ready (no services selected — connect to a public port or `rom start` from another terminal)"
            );
            return Ok::<(), anyhow::Error>(());
        }
        match ipc::request(
            &start_pipe,
            &Request::Start {
                services: start_names.clone(),
            },
        )
        .await?
        {
            Response::Ok { changed } => {
                if changed.is_empty() {
                    println!("already running");
                } else {
                    println!("started: {}", changed.join(", "));
                }
                Ok(())
            }
            Response::Error { message } => bail!(message),
            Response::Status { .. } => bail!("daemon sent an unexpected response"),
        }
    });

    let result = run_daemon_with_shutdown(project, shutdown_tx).await;
    let _ = starter.await;
    let _ = tail.await;
    result
}

/// Concurrently-style prefix colors. Bold text on a background, chosen for contrast.
/// Applied only to `[service]`; the caller prints the reset before the log text.
const PREFIX_STYLES: &[&str] = &[
    "\u{1b}[1;97;44m",  // white on blue
    "\u{1b}[1;97;45m",  // white on magenta
    "\u{1b}[1;30;46m",  // black on cyan
    "\u{1b}[1;30;42m",  // black on green
    "\u{1b}[1;30;43m",  // black on yellow
    "\u{1b}[1;97;41m",  // white on red
    "\u{1b}[1;30;47m",  // black on white
    "\u{1b}[1;97;104m", // white on bright blue
    "\u{1b}[1;30;105m", // black on bright magenta
    "\u{1b}[1;30;106m", // black on bright cyan
    "\u{1b}[1;30;102m", // black on bright green
    "\u{1b}[1;30;103m", // black on bright yellow
];
const RESET: &str = "\u{1b}[0m";

fn stdout_supports_color() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if std::env::var("TERM").ok().as_deref() == Some("dumb") {
        return false;
    }
    stdout().is_terminal()
}

fn format_service_line(name: &str, line: &str, names: &[String], use_color: bool) -> String {
    if !use_color {
        return format!("[{name}] {line}");
    }
    let style = prefix_style(name, names);
    format!("{style}[{name}]{RESET} {line}")
}

fn prefix_style(name: &str, names: &[String]) -> &'static str {
    let index = names
        .iter()
        .position(|candidate| candidate == name)
        .unwrap_or_else(|| name_slot(name));
    PREFIX_STYLES[index % PREFIX_STYLES.len()]
}

fn name_slot(name: &str) -> usize {
    name.bytes().fold(0usize, |acc, byte| {
        acc.wrapping_mul(31).wrapping_add(usize::from(byte))
    })
}

async fn follow_log_dir(
    log_dir: PathBuf,
    mut shutdown: watch::Receiver<bool>,
    names: Vec<String>,
    use_color: bool,
) {
    use std::collections::HashMap;
    use std::io::{Read, Seek, SeekFrom};

    let mut offsets: HashMap<PathBuf, u64> = HashMap::new();
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        if let Ok(entries) = fs::read_dir(&log_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("log") {
                    continue;
                }
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "service".to_owned());
                let Ok(mut file) = File::open(&path) else {
                    continue;
                };
                let Ok(len) = file.seek(SeekFrom::End(0)) else {
                    continue;
                };
                let pos = offsets.entry(path.clone()).or_insert(len);
                if len < *pos {
                    *pos = 0;
                }
                if len == *pos {
                    continue;
                }
                if file.seek(SeekFrom::Start(*pos)).is_err() {
                    continue;
                }
                let mut chunk = String::new();
                if file.read_to_string(&mut chunk).is_err() {
                    continue;
                }
                *pos = len;
                for line in chunk.lines() {
                    println!("{}", format_service_line(&name, line, &names, use_color));
                }
            }
        }
        tokio::select! {
            () = sleep(Duration::from_millis(100)) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
        }
    }
}

async fn run(project: &Project, command: Cmd) -> Result<()> {
    match command {
        Cmd::Init | Cmd::Discover { .. } | Cmd::Daemon { .. } | Cmd::Up { .. } => {
            unreachable!("handled in main")
        }
        Cmd::Check => {
            let registry = project.registry()?;
            println!(
                "ok: project `{}` with {} services",
                registry.project,
                registry.names().len()
            );
        }
        Cmd::Start {
            services,
            profiles,
            all,
        } => {
            let registry = project.registry()?;
            let names = select(&registry, services, &profiles, all)?;
            ensure_daemon(project).await?;
            let started = send(project, Request::Start { services: names }).await?;
            if started.is_empty() {
                println!("already running");
            } else {
                println!("started: {}", started.join(", "));
            }
        }
        Cmd::Stop {
            services,
            profiles,
            all,
            force,
        } => {
            let registry = project.registry()?;
            let names = select(&registry, services, &profiles, all)?;
            if !is_running(project).await {
                println!("daemon is not running; nothing to stop");
                return Ok(());
            }
            let stopped = send(
                project,
                Request::Stop {
                    services: names,
                    force,
                },
            )
            .await?;
            if stopped.is_empty() {
                println!("nothing was running");
            } else {
                println!("stopped: {}", stopped.join(", "));
            }
        }
        Cmd::Restart { service } => {
            project.registry()?.get(&service)?;
            ensure_daemon(project).await?;
            send(
                project,
                Request::Restart {
                    service: service.clone(),
                },
            )
            .await?;
            println!("restarted: {service}");
        }
        Cmd::Status => status(project).await?,
        Cmd::Logs {
            service,
            lines,
            follow,
        } => logs(project, &service, lines, follow).await?,
    }
    Ok(())
}

async fn daemon(project: &Project, action: DaemonAction) -> Result<()> {
    match action {
        DaemonAction::Start => {
            start_daemon(project).await?;
            println!("daemon started for {}", project.config_dir.display());
        }
        DaemonAction::Stop => {
            if !is_running(project).await {
                println!("daemon is not running");
                return Ok(());
            }
            send(project, Request::Shutdown).await?;
            println!("daemon stopped");
        }
        DaemonAction::Run => run_daemon(project).await?,
    }
    Ok(())
}

async fn run_daemon(project: &Project) -> Result<()> {
    let (shutdown_tx, _shutdown_rx) = watch::channel(false);
    run_daemon_with_shutdown(project, shutdown_tx).await
}

async fn run_daemon_with_shutdown(
    project: &Project,
    shutdown_tx: watch::Sender<bool>,
) -> Result<()> {
    let registry = project.registry()?;
    let manager = ProcessManager::new(registry, &project.state_dir)?;
    let shutdown_rx = shutdown_tx.subscribe();

    let ctrl_c_tx = shutdown_tx.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = ctrl_c_tx.send(true);
        }
    });

    let bound = gateway::Gateway::bind(&manager)
        .await
        .context("gateway failed to bind public ports")?;
    let gateway_manager = Arc::clone(&manager);
    let gateway_shutdown = shutdown_rx.clone();
    let gateway = tokio::spawn(async move { bound.serve(gateway_manager, gateway_shutdown).await });

    let watcher_manager = Arc::clone(&manager);
    let watcher_shutdown = shutdown_rx.clone();
    let file_watcher =
        tokio::spawn(async move { watcher::run(watcher_manager, watcher_shutdown).await });

    tracing::info!(pipe = %project.pipe, "daemon listening");
    let served = server::serve(&project.pipe, Arc::clone(&manager), shutdown_tx).await;
    tracing::info!("stopping services");
    if let Err(error) = manager.shutdown().await {
        tracing::error!(%error, "failed to stop every service cleanly");
    }
    served.with_context(|| {
        format!(
            "could not listen on {} (is another daemon running?)",
            project.pipe
        )
    })?;
    match gateway.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error).context("gateway failed"),
        Err(error) if error.is_cancelled() => {}
        Err(error) => return Err(error).context("gateway task failed"),
    }
    match file_watcher.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error).context("watcher failed"),
        Err(error) if error.is_cancelled() => {}
        Err(error) => return Err(error).context("watcher task failed"),
    }
    Ok(())
}

async fn start_daemon(project: &Project) -> Result<()> {
    if is_running(project).await {
        bail!("daemon is already running");
    }
    fs::create_dir_all(&project.state_dir)?;
    let log_path = project.state_dir.join("daemon.log");
    let log = File::create(&log_path)?;

    let exe = std::env::current_exe()?;
    let config = project.config_path.to_string_lossy().into_owned();
    spawn_detached(
        &exe,
        &["--config", &config, "daemon", "run"],
        &project.config_dir,
        &log,
    )
    .context("failed to launch the daemon")?;

    for _ in 0..100 {
        sleep(Duration::from_millis(100)).await;
        if is_running(project).await {
            return Ok(());
        }
    }
    bail!("daemon did not become ready; see {}", log_path.display())
}

async fn ensure_daemon(project: &Project) -> Result<()> {
    if is_running(project).await {
        return Ok(());
    }
    start_daemon(project).await
}

async fn is_running(project: &Project) -> bool {
    ipc::request(&project.pipe, &Request::Status).await.is_ok()
}

async fn send(project: &Project, request: Request) -> Result<Vec<String>> {
    match ipc::request(&project.pipe, &request).await? {
        Response::Ok { changed } => Ok(changed),
        Response::Error { message } => bail!(message),
        Response::Status { .. } => bail!("daemon sent an unexpected response"),
    }
}

async fn status(project: &Project) -> Result<()> {
    let registry = project.registry()?;
    let services = if is_running(project).await {
        match ipc::request(&project.pipe, &Request::Status).await? {
            Response::Status { daemon, services } => {
                println!(
                    "daemon: pid={} uptime={} working_set={} cpu={} (working set is not exclusive RAM)",
                    daemon.pid,
                    format_uptime(daemon.uptime_secs),
                    daemon
                        .working_set_bytes
                        .map_or_else(|| "-".to_owned(), metrics::format_bytes),
                    daemon
                        .cpu_time_secs
                        .map_or_else(|| "-".to_owned(), |secs| format!("{secs:.1}s")),
                );
                services
            }
            Response::Error { message } => bail!(message),
            Response::Ok { .. } => bail!("daemon sent an unexpected response"),
        }
    } else {
        println!("daemon: not running (showing configured services)");
        registry
            .services()
            .map(|def| ServiceStatus {
                name: def.name.clone(),
                state: ServiceState::Sleeping,
                port: def.port,
                listen_port: None,
                pid: None,
                uptime_secs: None,
                restart_count: 0,
                active_connections: 0,
                last_activity_secs_ago: None,
                working_set_bytes: None,
                cpu_time_secs: None,
                last_error: None,
            })
            .collect()
    };
    print_table(&services);
    Ok(())
}

fn print_table(services: &[ServiceStatus]) {
    println!(
        "{:<22} {:<14} {:>6} {:>7} {:>9} {:>5} {:>4} {:>8} {:>8} {:>8}  LAST ERROR",
        "SERVICE", "STATE", "PORT", "PID", "UPTIME", "CONNS", "RST", "WSET", "CPU", "ACTIVE"
    );
    for service in services {
        println!(
            "{:<22} {:<14} {:>6} {:>7} {:>9} {:>5} {:>4} {:>8} {:>8} {:>8}  {}",
            service.name,
            service.state.to_string(),
            service.port,
            service
                .pid
                .map_or_else(|| "-".to_owned(), |pid| pid.to_string()),
            service
                .uptime_secs
                .map_or_else(|| "-".to_owned(), format_uptime),
            service.active_connections,
            service.restart_count,
            service
                .working_set_bytes
                .map_or_else(|| "-".to_owned(), metrics::format_bytes),
            service
                .cpu_time_secs
                .map_or_else(|| "-".to_owned(), |secs| format!("{secs:.1}s")),
            service
                .last_activity_secs_ago
                .map_or_else(|| "-".to_owned(), |secs| format!("{secs}s")),
            service.last_error.as_deref().unwrap_or("")
        );
    }
}

fn format_uptime(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

async fn logs(project: &Project, service: &str, lines: usize, follow: bool) -> Result<()> {
    project.registry()?.get(service)?;
    let path = project
        .state_dir
        .join("logs")
        .join(format!("{service}.log"));
    if !path.exists() {
        bail!("no logs yet for `{service}`");
    }

    let mut out = stdout();
    let tail = read_tail(&path, lines)?;
    writeln!(out, "{tail}")?;
    if !follow {
        return Ok(());
    }

    let mut file = File::open(&path)?;
    let mut position = file.seek(SeekFrom::End(0))?;
    loop {
        let len = file.metadata()?.len();
        if len < position {
            position = 0;
        }
        if len > position {
            file.seek(SeekFrom::Start(position))?;
            let mut chunk = Vec::new();
            file.read_to_end(&mut chunk)?;
            position += chunk.len() as u64;
            out.write_all(&chunk)?;
            out.flush()?;
        }
        sleep(Duration::from_millis(250)).await;
    }
}

fn select(
    registry: &Registry,
    mut services: Vec<String>,
    profiles: &[String],
    all: bool,
) -> Result<Vec<String>> {
    if all {
        services.extend(registry.names());
    }
    for profile in profiles {
        services.extend(registry.profile(profile)?.iter().cloned());
    }
    services.sort();
    services.dedup();
    if services.is_empty() {
        bail!("no services selected: pass service names, --profile or --all");
    }
    Ok(services)
}

#[cfg(test)]
mod tests {
    use super::{PREFIX_STYLES, RESET, config_template, format_service_line};

    #[test]
    fn init_template_leaves_prefix_colors_commented() {
        let template = config_template();
        assert!(template.contains("# prefix_colors: true"));
        assert!(
            !template
                .lines()
                .any(|line| line.trim_start().starts_with("prefix_colors:"))
        );
    }

    #[test]
    fn colored_prefix_does_not_paint_the_log_text() {
        let names = vec!["notification".to_owned(), "dashboard".to_owned()];
        let line = format_service_line("notification", "\u{1b}[32mready\u{1b}[0m", &names, true);
        let (prefix, rest) = line.split_once(RESET).unwrap();
        assert_eq!(prefix, format!("{}[notification]", PREFIX_STYLES[0]));
        assert_eq!(rest, " \u{1b}[32mready\u{1b}[0m");
    }

    #[test]
    fn plain_prefix_when_colors_are_off() {
        assert_eq!(
            format_service_line("api", "ready", &[], false),
            "[api] ready"
        );
    }

    #[test]
    fn each_service_keeps_the_same_color() {
        let names = vec!["a".to_owned(), "b".to_owned()];
        let first = format_service_line("b", "one", &names, true);
        let again = format_service_line("b", "two", &names, true);
        assert!(first.starts_with(PREFIX_STYLES[1]));
        assert!(again.starts_with(PREFIX_STYLES[1]));
        let other = format_service_line("a", "x", &names, true);
        assert!(other.starts_with(PREFIX_STYLES[0]));
    }
}
