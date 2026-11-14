use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio::time::{sleep, timeout};
use windows_sys::Win32::System::Threading::CREATE_NEW_CONSOLE;

use crate::config::Mode;
use crate::console::{hide_console_windows, send_ctrl_break};
use crate::error::RomError;
use crate::job::JobObject;
use crate::metrics;
use crate::registry::{Registry, ServiceDef};

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const LOG_TAIL_LINES: usize = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Sleeping,
    Starting,
    HealthChecking,
    Running,
    Stopping,
    Failed,
    Crashed,
}

impl fmt::Display for ServiceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Sleeping => "sleeping",
            Self::Starting => "starting",
            Self::HealthChecking => "health-check",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Failed => "failed",
            Self::Crashed => "crashed",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub name: String,
    pub state: ServiceState,
    /// Public port from `rom.yaml` (the gateway listens here).
    pub port: u16,
    /// Port the process is actually bound to, when running.
    pub listen_port: Option<u16>,
    pub pid: Option<u32>,
    pub uptime_secs: Option<u64>,
    pub restart_count: u32,
    pub active_connections: u32,
    /// Seconds since the last proxied connection open/close (or start).
    pub last_activity_secs_ago: Option<u64>,
    /// Process working set in bytes. Not exclusive physical RAM — do not sum.
    pub working_set_bytes: Option<u64>,
    /// Cumulative kernel + user CPU time for the process, in seconds.
    pub cpu_time_secs: Option<f64>,
    pub last_error: Option<String>,
}

/// Cost of the rom daemon process itself (separate from services).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub pid: u32,
    pub uptime_secs: u64,
    /// Daemon working set in bytes. Not exclusive physical RAM — do not sum with services.
    pub working_set_bytes: Option<u64>,
    pub cpu_time_secs: Option<f64>,
}

/// Full status snapshot returned by the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub daemon: DaemonStatus,
    pub services: Vec<ServiceStatus>,
}

struct Instance {
    pid: u32,
    listen_port: u16,
    job: Arc<JobObject>,
    started_at: Instant,
    /// Becomes `true` once the child process has been reaped.
    exited: watch::Receiver<bool>,
}

struct Slot {
    state: ServiceState,
    last_error: Option<String>,
    restart_count: u32,
    instance: Option<Instance>,
    active_connections: usize,
    last_activity: Option<Instant>,
    /// Bumped to cancel a pending idle-stop task.
    idle_generation: u64,
}

impl Slot {
    fn sleeping() -> Self {
        Self {
            state: ServiceState::Sleeping,
            last_error: None,
            restart_count: 0,
            instance: None,
            active_connections: 0,
            last_activity: None,
            idle_generation: 0,
        }
    }
}

/// Owns the child processes of one project. Every public operation that changes
/// the set of running services is serialized through `lifecycle`.
pub struct ProcessManager {
    registry: Registry,
    log_dir: PathBuf,
    compile_cache_dir: PathBuf,
    started_at: Instant,
    slots: Arc<Mutex<BTreeMap<String, Slot>>>,
    lifecycle: AsyncMutex<()>,
}

impl ProcessManager {
    pub fn new(registry: Registry, state_dir: &Path) -> io::Result<Arc<Self>> {
        let log_dir = state_dir.join("logs");
        fs::create_dir_all(&log_dir)?;
        let slots = registry
            .services()
            .map(|def| (def.name.clone(), Slot::sleeping()))
            .collect();
        Ok(Arc::new(Self {
            registry,
            log_dir,
            compile_cache_dir: state_dir.join("compile-cache"),
            started_at: Instant::now(),
            slots: Arc::new(Mutex::new(slots)),
            lifecycle: AsyncMutex::new(()),
        }))
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn log_path(&self, name: &str) -> PathBuf {
        self.log_dir.join(format!("{name}.log"))
    }

    /// Starts the given services and whatever they depend on. Returns the services that were started.
    pub async fn start(self: &Arc<Self>, targets: &[String]) -> Result<Vec<String>, RomError> {
        let _guard = self.lifecycle.lock().await;
        let started = self.start_locked(targets).await?;
        for name in &started {
            self.arm_idle_if_needed(name);
        }
        Ok(started)
    }

    /// Ensures `name` (and its dependencies) are running, then returns the process listen port.
    /// Concurrent callers share a single start: the lifecycle lock serializes them, and the
    /// second caller finds the service already active.
    pub async fn ensure_ready(self: &Arc<Self>, name: &str) -> Result<u16, RomError> {
        let _guard = self.lifecycle.lock().await;
        self.registry.get(name)?;
        if let Some(port) = self.listen_port_of(name) {
            return Ok(port);
        }
        let started = self.start_locked(&[name.to_owned()]).await?;
        for started_name in &started {
            self.arm_idle_if_needed(started_name);
        }
        self.listen_port_of(name).ok_or_else(|| RomError::StartFailed {
            service: name.to_owned(),
            reason: "service started but has no listen port".to_owned(),
        })
    }

    /// Marks a proxied connection as active (cancels any pending idle stop).
    pub fn connection_opened(self: &Arc<Self>, name: &str) {
        let mut slots = lock(&self.slots);
        if let Some(slot) = slots.get_mut(name) {
            slot.active_connections += 1;
            slot.last_activity = Some(Instant::now());
            slot.idle_generation = slot.idle_generation.wrapping_add(1);
        }
    }

    /// Marks a proxied connection as finished. Arms idle stop when none remain.
    pub fn connection_closed(self: &Arc<Self>, name: &str) {
        let should_arm = {
            let mut slots = lock(&self.slots);
            let Some(slot) = slots.get_mut(name) else {
                return;
            };
            slot.active_connections = slot.active_connections.saturating_sub(1);
            slot.last_activity = Some(Instant::now());
            slot.active_connections == 0 && slot.state == ServiceState::Running
        };
        if should_arm {
            self.arm_idle_if_needed(name);
        }
    }

    /// Stops the given services. Fails if running services depend on them, unless `force` is set,
    /// in which case those dependents are stopped first.
    pub async fn stop(self: &Arc<Self>, targets: &[String], force: bool) -> Result<Vec<String>, RomError> {
        let _guard = self.lifecycle.lock().await;
        self.stop_locked(targets, force).await
    }

    /// Restarts one service. Its dependencies keep running; running dependents are not touched.
    pub async fn restart(self: &Arc<Self>, name: &str) -> Result<(), RomError> {
        let _guard = self.lifecycle.lock().await;
        self.registry.get(name)?;
        self.stop_one(name).await?;
        self.update(name, |slot| slot.restart_count += 1);
        let started = self.start_locked(&[name.to_owned()]).await?;
        for started_name in &started {
            self.arm_idle_if_needed(started_name);
        }
        Ok(())
    }

    /// Stops every service, dependents first.
    pub async fn shutdown(self: &Arc<Self>) -> Result<(), RomError> {
        let all = self.registry.names();
        self.stop(&all, true).await.map(drop)
    }

    pub fn is_running(&self, name: &str) -> bool {
        lock(&self.slots)
            .get(name)
            .is_some_and(|slot| slot.state == ServiceState::Running)
    }

    pub fn status_snapshot(&self) -> StatusSnapshot {
        StatusSnapshot {
            daemon: self.daemon_status(),
            services: self.status(),
        }
    }

    pub fn daemon_status(&self) -> DaemonStatus {
        let metrics = metrics::current_process_metrics().ok();
        DaemonStatus {
            pid: std::process::id(),
            uptime_secs: self.started_at.elapsed().as_secs(),
            working_set_bytes: metrics.as_ref().map(|m| m.working_set_bytes),
            cpu_time_secs: metrics.as_ref().map(|m| m.cpu_time.as_secs_f64()),
        }
    }

    pub fn status(&self) -> Vec<ServiceStatus> {
        let slots = lock(&self.slots);
        self.registry
            .services()
            .filter_map(|def| {
                let slot = slots.get(&def.name)?;
                let process = slot
                    .instance
                    .as_ref()
                    .and_then(|instance| metrics::process_metrics(instance.pid).ok());
                Some(ServiceStatus {
                    name: def.name.clone(),
                    state: slot.state,
                    port: def.port,
                    listen_port: slot.instance.as_ref().map(|i| i.listen_port),
                    pid: slot.instance.as_ref().map(|i| i.pid),
                    uptime_secs: slot
                        .instance
                        .as_ref()
                        .map(|i| i.started_at.elapsed().as_secs()),
                    restart_count: slot.restart_count,
                    active_connections: slot.active_connections as u32,
                    last_activity_secs_ago: slot
                        .last_activity
                        .map(|at| at.elapsed().as_secs()),
                    working_set_bytes: process.as_ref().map(|m| m.working_set_bytes),
                    cpu_time_secs: process.as_ref().map(|m| m.cpu_time.as_secs_f64()),
                    last_error: slot.last_error.clone(),
                })
            })
            .collect()
    }

    async fn start_locked(&self, targets: &[String]) -> Result<Vec<String>, RomError> {
        let mut started = Vec::new();
        for name in self.registry.resolve_start_set(targets)? {
            if self.is_active(&name) {
                continue;
            }
            self.start_one(&name).await?;
            started.push(name);
        }
        Ok(started)
    }

    async fn stop_locked(&self, targets: &[String], force: bool) -> Result<Vec<String>, RomError> {
        let requested: BTreeSet<String> = targets.iter().cloned().collect();
        let mut set = BTreeSet::new();
        for name in &requested {
            self.registry.get(name)?;
            // Dependents that are part of this same request are stopped too, so they do not block it.
            let dependents: Vec<String> = self
                .registry
                .transitive_dependents(name)
                .into_iter()
                .filter(|dep| !requested.contains(dep) && self.is_active(dep))
                .collect();
            if !dependents.is_empty() && !force {
                return Err(RomError::StillRequired {
                    service: name.clone(),
                    dependents: dependents.join(", "),
                });
            }
            set.insert(name.clone());
            set.extend(dependents);
        }

        let mut stopped = Vec::new();
        for name in self.registry.sort_for_start(&set).into_iter().rev() {
            if self.is_active(&name) {
                stopped.push(name.clone());
            }
            self.stop_one(&name).await?;
        }
        Ok(stopped)
    }

    async fn start_one(&self, name: &str) -> Result<(), RomError> {
        let def = self.registry.get(name)?.clone();
        self.update(name, |slot| {
            slot.state = ServiceState::Starting;
            slot.last_error = None;
        });

        let listen_port = match allocate_listen_port() {
            Ok(port) => port,
            Err(error) => {
                return Err(self.fail(
                    name,
                    RomError::PortInUse {
                        service: name.to_owned(),
                        port: 0,
                        reason: error.to_string(),
                    },
                ));
            }
        };

        let instance = match self.spawn(&def, listen_port) {
            Ok(instance) => instance,
            Err(error) => return Err(self.fail(name, error)),
        };
        let exited = instance.exited.clone();
        self.update(name, move |slot| {
            slot.instance = Some(instance);
            slot.state = ServiceState::HealthChecking;
        });

        match self.wait_ready(&def, listen_port, exited).await {
            Ok(()) => {
                self.update(name, |slot| {
                    if slot.state == ServiceState::HealthChecking {
                        slot.state = ServiceState::Running;
                        slot.last_activity = Some(Instant::now());
                    }
                });
                Ok(())
            }
            Err(reason) => {
                let _ = self.stop_one(name).await;
                self.update(name, |slot| slot.instance = None);
                let tail = read_tail(&self.log_path(name), LOG_TAIL_LINES).unwrap_or_default();
                let reason = if tail.trim().is_empty() {
                    reason
                } else {
                    format!("{reason}\n--- last log lines ---\n{tail}")
                };
                Err(self.fail(
                    name,
                    RomError::StartFailed {
                        service: name.to_owned(),
                        reason,
                    },
                ))
            }
        }
    }

    async fn stop_one(&self, name: &str) -> Result<(), RomError> {
        let target = {
            let mut slots = lock(&self.slots);
            let Some(slot) = slots.get_mut(name) else {
                return Ok(());
            };
            slot.idle_generation = slot.idle_generation.wrapping_add(1);
            match &slot.instance {
                None => {
                    slot.state = ServiceState::Sleeping;
                    None
                }
                Some(instance) => {
                    slot.state = ServiceState::Stopping;
                    Some((
                        instance.pid,
                        Arc::clone(&instance.job),
                        instance.exited.clone(),
                    ))
                }
            }
        };
        let Some((pid, job, exited)) = target else {
            return Ok(());
        };

        // Graceful path: CTRL_BREAK → Node SIGBREAK. Fall back to the Job Object
        // if the process ignores the signal or has no console.
        match send_ctrl_break(pid) {
            Ok(()) => {
                if timeout(
                    self.registry.shutdown_timeout,
                    wait_for_exit(exited.clone()),
                )
                .await
                .is_ok()
                {
                    return Ok(());
                }
                tracing::warn!(
                    service = %name,
                    "CTRL_BREAK ignored; terminating job object"
                );
            }
            Err(error) => {
                tracing::debug!(
                    service = %name,
                    %error,
                    "CTRL_BREAK unavailable; terminating job object"
                );
            }
        }

        job.terminate()?;
        self.await_exit(name, exited).await
    }

    fn arm_idle_if_needed(self: &Arc<Self>, name: &str) {
        let Ok(def) = self.registry.get(name) else {
            return;
        };
        if def.mode == Mode::Persistent {
            return;
        }
        let generation = {
            let mut slots = lock(&self.slots);
            let Some(slot) = slots.get_mut(name) else {
                return;
            };
            if slot.active_connections > 0 || slot.state != ServiceState::Running {
                return;
            }
            slot.idle_generation = slot.idle_generation.wrapping_add(1);
            slot.idle_generation
        };
        let timeout = self.registry.idle_timeout;
        let this = Arc::clone(self);
        let name = name.to_owned();
        tokio::spawn(async move {
            sleep(timeout).await;
            this.run_idle_stop(&name, generation).await;
        });
    }

    async fn run_idle_stop(self: &Arc<Self>, name: &str, generation: u64) {
        let _guard = self.lifecycle.lock().await;
        let should_stop = {
            let slots = lock(&self.slots);
            let Some(slot) = slots.get(name) else {
                return;
            };
            let is_on_demand = self
                .registry
                .get(name)
                .is_ok_and(|def| def.mode == Mode::OnDemand);
            slot.idle_generation == generation
                && slot.active_connections == 0
                && slot.state == ServiceState::Running
                && is_on_demand
        };
        if !should_stop {
            return;
        }
        tracing::info!(service = %name, "stopping idle service");
        if let Err(error) = self.stop_one(name).await {
            tracing::warn!(service = %name, %error, "idle stop failed");
        }
    }

    async fn await_exit(&self, name: &str, exited: watch::Receiver<bool>) -> Result<(), RomError> {
        let finished = timeout(self.registry.shutdown_timeout, wait_for_exit(exited))
            .await
            .is_ok();
        if finished {
            Ok(())
        } else {
            Err(RomError::StopTimeout(name.to_owned()))
        }
    }

    /// Polls until the listen port accepts connections (and `/health` answers 2xx, when configured).
    /// Returns the reason on failure.
    async fn wait_ready(
        &self,
        def: &ServiceDef,
        listen_port: u16,
        mut exited: watch::Receiver<bool>,
    ) -> Result<(), String> {
        let deadline = Instant::now() + self.registry.startup_timeout;
        loop {
            if *exited.borrow() {
                return Err("process exited before becoming ready".to_owned());
            }
            if probe(def, listen_port).await {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "port {listen_port} did not become ready within {}s",
                    self.registry.startup_timeout.as_secs()
                ));
            }
            tokio::select! {
                () = sleep(POLL_INTERVAL) => {}
                _ = exited.changed() => {}
            }
        }
    }

    fn spawn(&self, def: &ServiceDef, listen_port: u16) -> Result<Instance, RomError> {
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path(&def.name))?;
        writeln!(
            log,
            "--- rom: starting `{}` on listen port {listen_port} (public {}) at unix {} ---",
            def.name,
            def.port,
            unix_now()
        )?;
        let stderr = log.try_clone()?;

        let mut command = Command::new(&def.command);
        command
            .args(&def.args)
            .current_dir(&def.dir)
            .envs(&def.env)
            .env("PORT", listen_port.to_string())
            .env("ROM_SERVICE", &def.name)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr))
            // Own hidden console so CTRL_BREAK can be delivered on stop.
            // CREATE_NEW_PROCESS_GROUP is ignored when combined with this flag.
            .creation_flags(CREATE_NEW_CONSOLE);
        // Node 22.1+ reuses V8 bytecode across starts. Harmless on older Node, which ignores it.
        if is_node_command(&def.command) {
            let cache = self.compile_cache_dir.join(&def.name);
            fs::create_dir_all(&cache)?;
            command.env("NODE_COMPILE_CACHE", &cache);
        }
        let mut child = command.spawn()
            .map_err(|source| RomError::Spawn {
                service: def.name.clone(),
                command: def.command.clone(),
                source,
            })?;

        let Some(pid) = child.id() else {
            return Err(io::Error::other("child exited before it could be tracked").into());
        };
        hide_console_windows(pid);

        // NOTE: assignment happens right after spawn, so a grandchild created in that
        // instant could escape the job. Node services do not do this during boot.
        let job = JobObject::new()?;
        let assigned = match child.raw_handle() {
            Some(handle) => job.assign(handle),
            None => Err(io::Error::other("process handle unavailable")),
        };
        if let Err(source) = assigned {
            let _ = child.start_kill();
            return Err(source.into());
        }

        let (exited_tx, exited_rx) = watch::channel(false);
        let job = Arc::new(job);
        let slots = Arc::clone(&self.slots);
        let service = def.name.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            {
                let mut guard = lock(&slots);
                if let Some(slot) = guard.get_mut(&service) {
                    let next = match slot.state {
                        ServiceState::Stopping => ServiceState::Sleeping,
                        ServiceState::Running => ServiceState::Crashed,
                        _ => ServiceState::Failed,
                    };
                    slot.last_error = (next != ServiceState::Sleeping).then(|| describe_exit(&status));
                    slot.state = next;
                    // Dropping the last Arc<JobObject> closes the job and reaps any leftover descendants.
                    slot.instance = None;
                }
            }
            let _ = exited_tx.send(true);
        });

        Ok(Instance {
            pid,
            listen_port,
            job,
            started_at: Instant::now(),
            exited: exited_rx,
        })
    }

    fn is_active(&self, name: &str) -> bool {
        lock(&self.slots).get(name).is_some_and(|slot| {
            matches!(
                slot.state,
                ServiceState::Starting | ServiceState::HealthChecking | ServiceState::Running
            )
        })
    }

    fn listen_port_of(&self, name: &str) -> Option<u16> {
        lock(&self.slots).get(name).and_then(|slot| {
            (slot.state == ServiceState::Running)
                .then(|| slot.instance.as_ref().map(|i| i.listen_port))
                .flatten()
        })
    }

    fn update(&self, name: &str, change: impl FnOnce(&mut Slot)) {
        if let Some(slot) = lock(&self.slots).get_mut(name) {
            change(slot);
        }
    }

    fn fail(&self, name: &str, error: RomError) -> RomError {
        self.update(name, |slot| {
            slot.state = ServiceState::Failed;
            slot.last_error = Some(error.to_string());
        });
        error
    }
}

async fn wait_for_exit(mut exited: watch::Receiver<bool>) {
    let _ = exited.wait_for(|done| *done).await;
}

fn is_node_command(command: &str) -> bool {
    Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("node") || name.eq_ignore_ascii_case("node.exe"))
}

fn lock(slots: &Mutex<BTreeMap<String, Slot>>) -> MutexGuard<'_, BTreeMap<String, Slot>> {
    slots.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Picks a free localhost port for a service process.
/// Avoids Windows' reserved range 50000–50059 on this machine.
fn allocate_listen_port() -> io::Result<u16> {
    for _ in 0..32 {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        let port = listener.local_addr()?.port();
        drop(listener);
        if !(50_000..=50_059).contains(&port) {
            return Ok(port);
        }
    }
    Err(io::Error::other("could not allocate a free listen port"))
}

async fn probe(def: &ServiceDef, listen_port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, listen_port));
    let Ok(Ok(mut stream)) = timeout(PROBE_TIMEOUT, TcpStream::connect(addr)).await else {
        return false;
    };
    let Some(health) = &def.health else {
        return true;
    };

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        health.path
    );
    if stream.write_all(request.as_bytes()).await.is_err() {
        return false;
    }
    let mut head = [0u8; 16];
    let read = timeout(PROBE_TIMEOUT, stream.read(&mut head)).await;
    matches!(read, Ok(Ok(n)) if n >= 10 && head.starts_with(b"HTTP/1.") && head[9] == b'2')
}

fn describe_exit(status: &io::Result<ExitStatus>) -> String {
    match status {
        Ok(status) => match status.code() {
            Some(code) => format!("exited with code {code}"),
            None => "terminated".to_owned(),
        },
        Err(error) => format!("could not observe exit: {error}"),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Returns the last `lines` lines of a log file, decoding invalid UTF-8 lossily.
pub fn read_tail(path: &Path, lines: usize) -> io::Result<String> {
    let text = String::from_utf8_lossy(&fs::read(path)?).into_owned();
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    Ok(all[start..].join("\n"))
}
