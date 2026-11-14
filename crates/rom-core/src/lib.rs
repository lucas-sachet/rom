//! Core library of `rom` (Runtime Orchestrator for Microservices).
//!
//! - `config`: parses `rom.yaml`.
//! - `registry`: validated view of the services; the source of truth at runtime.
//! - `process`: service lifecycle (spawn, readiness, stop).
//! - `gateway`: TCP proxy from public ports to running processes.
//! - `watcher`: restarts running services when their files change.
//! - `ipc` and `server`: JSON lines over a named pipe (Windows) or Unix domain socket.

pub mod config;
pub mod console;
pub mod detach;
pub mod discover;
pub mod error;
pub mod gateway;
pub mod ipc;
pub mod metrics;
pub mod process;
pub mod registry;
pub mod server;
pub mod watcher;

mod job;
#[cfg(windows)]
mod win_spawn;

pub use error::RomError;
