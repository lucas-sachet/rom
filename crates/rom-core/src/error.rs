use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RomError {
    #[error("no rom.yaml found in {0} or any parent directory (run `rom init`)")]
    ConfigNotFound(String),

    #[error("could not read {path}: {source}")]
    ReadConfig { path: String, source: io::Error },

    #[error("invalid YAML in rom.yaml: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),

    #[error("invalid configuration:\n  - {}", .0.join("\n  - "))]
    InvalidConfig(Vec<String>),

    #[error("unknown service `{0}`")]
    UnknownService(String),

    #[error("unknown profile `{0}`")]
    UnknownProfile(String),

    #[error(
        "`{service}` is still required by running services: {dependents} (stop them first or use --force)"
    )]
    StillRequired { service: String, dependents: String },

    #[error(
        "cannot run `{command}` for `{service}`: {source}. Note: `.cmd` shims such as npm or ts-node cannot be spawned directly; use `node` and pass the script in `args`"
    )]
    Spawn {
        service: String,
        command: String,
        source: io::Error,
    },

    #[error("port {port} needed by `{service}` is not available: {reason}")]
    PortInUse {
        service: String,
        port: u16,
        reason: String,
    },

    #[error("`{service}` failed to start: {reason}")]
    StartFailed { service: String, reason: String },

    #[error("`{0}` did not exit within the shutdown timeout")]
    StopTimeout(String),

    #[error("rom daemon is not running for this project (start it with `rom daemon start`)")]
    DaemonNotRunning,

    #[error("daemon sent an invalid response")]
    Protocol,

    #[error(transparent)]
    Io(#[from] io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
