//! Core library of `rom` (Runtime Orchestrator for Microservices).
//!
//! - `config`: parses `rom.yaml`.
//! - `registry`: validated view of the services; the source of truth at runtime.
//! - `process`: service lifecycle (spawn, readiness, stop).
//! - `gateway`: TCP proxy from public ports to running processes.
//! - `ipc` and `server`: JSON lines over a Windows named pipe, one daemon per project.

#[cfg(not(windows))]
compile_error!("rom currently targets Windows only");

pub mod config;
pub mod error;
#[cfg(windows)]
pub mod detach;
pub mod gateway;
pub mod ipc;
pub mod process;
pub mod registry;
pub mod server;

#[cfg(windows)]
mod job;

pub use error::RomError;
