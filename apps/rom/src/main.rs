use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write, stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rom_core::config::{self, find_config};
use rom_core::detach::spawn_detached;
use rom_core::gateway;
use rom_core::ipc::{self, Request, Response, pipe_name};
use rom_core::process::{ProcessManager, ServiceState, ServiceStatus, read_tail};
use rom_core::registry::Registry;
use rom_core::{RomError, server};
use tokio::sync::watch;
use tokio::time::sleep;
use tracing_subscriber::EnvFilter;

const CONFIG_TEMPLATE: &str = r#"version: 1

project:
  name: {{PROJECT}}
  root: .

runtime:
  startup_timeout: 30s
  shutdown_timeout: 30s
  idle_timeout: 30s
  connection_idle: 5s

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

#[derive(Parser)]
#[command(name = "rom", version, about = "Runtime Orchestrator for Microservices")]
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
    /// Validate rom.yaml without starting anything.
    Check,
    /// Start services; their dependencies start first.
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
                find_config(&cwd).ok_or_else(|| RomError::ConfigNotFound(cwd.display().to_string()))?
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
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("ROM_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Cmd::Init => init(),
        Cmd::Daemon { action } => {
            let project = Project::locate(cli.config)?;
            daemon(&project, action).await
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
    let name = path
        .parent()
        .and_then(Path::file_name)
        .map_or_else(|| "my-project".to_owned(), |n| n.to_string_lossy().into_owned());
    fs::write(&path, CONFIG_TEMPLATE.replace("{{PROJECT}}", &name))?;
    println!("created {}", path.display());
    Ok(())
}

async fn run(project: &Project, command: Cmd) -> Result<()> {
    match command {
        Cmd::Init | Cmd::Daemon { .. } => unreachable!("handled in main"),
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
            let stopped = send(project, Request::Stop { services: names, force }).await?;
            if stopped.is_empty() {
                println!("nothing was running");
            } else {
                println!("stopped: {}", stopped.join(", "));
            }
        }
        Cmd::Restart { service } => {
            project.registry()?.get(&service)?;
            ensure_daemon(project).await?;
            send(project, Request::Restart { service: service.clone() }).await?;
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
    let registry = project.registry()?;
    let manager = ProcessManager::new(registry, &project.state_dir)?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

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
    let gateway = tokio::spawn(async move { bound.serve(gateway_manager, shutdown_rx).await });

    tracing::info!(pipe = %project.pipe, "daemon listening");
    let served = server::serve(&project.pipe, Arc::clone(&manager), shutdown_tx).await;
    tracing::info!("stopping services");
    if let Err(error) = manager.shutdown().await {
        tracing::error!(%error, "failed to stop every service cleanly");
    }
    served.with_context(|| format!("could not listen on {} (is another daemon running?)", project.pipe))?;
    match gateway.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error).context("gateway failed"),
        Err(error) if error.is_cancelled() => {}
        Err(error) => return Err(error).context("gateway task failed"),
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
    spawn_detached(&exe, &["--config", &config, "daemon", "run"], &project.config_dir, &log)
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
        println!("daemon: running");
        match ipc::request(&project.pipe, &Request::Status).await? {
            Response::Status { services } => services,
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
                last_error: None,
            })
            .collect()
    };
    print_table(&services);
    Ok(())
}

fn print_table(services: &[ServiceStatus]) {
    println!(
        "{:<22} {:<14} {:>6} {:>7} {:>9} {:>8}  LAST ERROR",
        "SERVICE", "STATE", "PORT", "PID", "UPTIME", "RESTARTS"
    );
    for service in services {
        println!(
            "{:<22} {:<14} {:>6} {:>7} {:>9} {:>8}  {}",
            service.name,
            service.state.to_string(),
            service.port,
            service.pid.map_or_else(|| "-".to_owned(), |pid| pid.to_string()),
            service
                .uptime_secs
                .map_or_else(|| "-".to_owned(), format_uptime),
            service.restart_count,
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
    let path = project.state_dir.join("logs").join(format!("{service}.log"));
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

fn select(registry: &Registry, mut services: Vec<String>, profiles: &[String], all: bool) -> Result<Vec<String>> {
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
