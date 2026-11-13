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

use crate::error::RomError;
use crate::job::JobObject;
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
    pub port: u16,
    pub pid: Option<u32>,
    pub uptime_secs: Option<u64>,
    pub restart_count: u32,
    pub last_error: Option<String>,
}

struct Instance {
    pid: u32,
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
}

impl Slot {
    fn sleeping() -> Self {
        Self {
            state: ServiceState::Sleeping,
            last_error: None,
            restart_count: 0,
            instance: None,
        }
    }
}

/// Owns the child processes of one project. Every public operation that changes
/// the set of running services is serialized through `lifecycle`.
pub struct ProcessManager {
    registry: Registry,
    log_dir: PathBuf,
    compile_cache_dir: PathBuf,
    slots: Arc<Mutex<BTreeMap<String, Slot>>>,
    lifecycle: AsyncMutex<()>,
}

impl ProcessManager {
    pub fn new(registry: Registry, state_dir: &Path) -> io::Result<Self> {
        let log_dir = state_dir.join("logs");
        fs::create_dir_all(&log_dir)?;
        let slots = registry
            .services()
            .map(|def| (def.name.clone(), Slot::sleeping()))
            .collect();
        Ok(Self {
            registry,
            log_dir,
            compile_cache_dir: state_dir.join("compile-cache"),
            slots: Arc::new(Mutex::new(slots)),
            lifecycle: AsyncMutex::new(()),
        })
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn log_path(&self, name: &str) -> PathBuf {
        self.log_dir.join(format!("{name}.log"))
    }

    /// Starts the given services and whatever they depend on. Returns the services that were started.
    pub async fn start(&self, targets: &[String]) -> Result<Vec<String>, RomError> {
        let _guard = self.lifecycle.lock().await;
        self.start_locked(targets).await
    }

    /// Stops the given services. Fails if running services depend on them, unless `force` is set,
    /// in which case those dependents are stopped first.
    pub async fn stop(&self, targets: &[String], force: bool) -> Result<Vec<String>, RomError> {
        let _guard = self.lifecycle.lock().await;
        self.stop_locked(targets, force).await
    }

    /// Restarts one service. Its dependencies keep running; running dependents are not touched.
    pub async fn restart(&self, name: &str) -> Result<(), RomError> {
        let _guard = self.lifecycle.lock().await;
        self.registry.get(name)?;
        self.stop_one(name).await?;
        self.update(name, |slot| slot.restart_count += 1);
        self.start_locked(&[name.to_owned()]).await?;
        Ok(())
    }

    /// Stops every service, dependents first.
    pub async fn shutdown(&self) -> Result<(), RomError> {
        let all = self.registry.names();
        self.stop(&all, true).await.map(drop)
    }

    pub fn status(&self) -> Vec<ServiceStatus> {
        let slots = lock(&self.slots);
        self.registry
            .services()
            .filter_map(|def| {
                let slot = slots.get(&def.name)?;
                Some(ServiceStatus {
                    name: def.name.clone(),
                    state: slot.state,
                    port: def.port,
                    pid: slot.instance.as_ref().map(|i| i.pid),
                    uptime_secs: slot
                        .instance
                        .as_ref()
                        .map(|i| i.started_at.elapsed().as_secs()),
                    restart_count: slot.restart_count,
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

        if let Err(error) = ensure_port_free(&def) {
            return Err(self.fail(name, error));
        }

        let instance = match self.spawn(&def) {
            Ok(instance) => instance,
            Err(error) => return Err(self.fail(name, error)),
        };
        let exited = instance.exited.clone();
        self.update(name, move |slot| {
            slot.instance = Some(instance);
            slot.state = ServiceState::HealthChecking;
        });

        match self.wait_ready(&def, exited).await {
            Ok(()) => {
                self.update(name, |slot| {
                    if slot.state == ServiceState::HealthChecking {
                        slot.state = ServiceState::Running;
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
            match &slot.instance {
                None => {
                    slot.state = ServiceState::Sleeping;
                    None
                }
                Some(instance) => {
                    slot.state = ServiceState::Stopping;
                    Some((Arc::clone(&instance.job), instance.exited.clone()))
                }
            }
        };
        let Some((job, exited)) = target else {
            return Ok(());
        };
        job.terminate()?;
        self.await_exit(name, exited).await
    }

    async fn await_exit(&self, name: &str, mut exited: watch::Receiver<bool>) -> Result<(), RomError> {
        let finished = timeout(
            self.registry.shutdown_timeout,
            exited.wait_for(|done| *done),
        )
        .await
        .is_ok();
        if finished {
            Ok(())
        } else {
            Err(RomError::StopTimeout(name.to_owned()))
        }
    }

    /// Polls until the TCP port accepts connections (and `/health` answers 2xx, when configured).
    /// Returns the reason on failure.
    async fn wait_ready(
        &self,
        def: &ServiceDef,
        mut exited: watch::Receiver<bool>,
    ) -> Result<(), String> {
        let deadline = Instant::now() + self.registry.startup_timeout;
        loop {
            if *exited.borrow() {
                return Err("process exited before becoming ready".to_owned());
            }
            if probe(def).await {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "port {} did not become ready within {}s",
                    def.port,
                    self.registry.startup_timeout.as_secs()
                ));
            }
            tokio::select! {
                () = sleep(POLL_INTERVAL) => {}
                _ = exited.changed() => {}
            }
        }
    }

    fn spawn(&self, def: &ServiceDef) -> Result<Instance, RomError> {
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path(&def.name))?;
        writeln!(log, "--- rom: starting `{}` at unix {} ---", def.name, unix_now())?;
        let stderr = log.try_clone()?;

        let mut command = Command::new(&def.command);
        command
            .args(&def.args)
            .current_dir(&def.dir)
            .envs(&def.env)
            .env("PORT", def.port.to_string())
            .env("ROM_SERVICE", &def.name)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
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

fn is_node_command(command: &str) -> bool {
    Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("node") || name.eq_ignore_ascii_case("node.exe"))
}

fn lock(slots: &Mutex<BTreeMap<String, Slot>>) -> MutexGuard<'_, BTreeMap<String, Slot>> {
    slots.lock().unwrap_or_else(PoisonError::into_inner)
}

fn ensure_port_free(def: &ServiceDef) -> Result<(), RomError> {
    TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, def.port)))
        .map(drop)
        .map_err(|error| RomError::PortInUse {
            service: def.name.clone(),
            port: def.port,
            reason: error.to_string(),
        })
}

async fn probe(def: &ServiceDef) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, def.port));
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
