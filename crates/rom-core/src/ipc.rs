//! Wire protocol between the `rom` CLI and the daemon: one JSON object per line.
//!
//! Transport is a Windows named pipe or a Unix domain socket under `$XDG_RUNTIME_DIR`.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::error::RomError;
use crate::process::{DaemonStatus, ServiceStatus};

#[cfg(windows)]
const ERROR_PIPE_BUSY: i32 = 231;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    Start { services: Vec<String> },
    Stop { services: Vec<String>, force: bool },
    Restart { service: String },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Ok {
        changed: Vec<String>,
    },
    Status {
        daemon: DaemonStatus,
        services: Vec<ServiceStatus>,
    },
    Error {
        message: String,
    },
}

/// IPC endpoint unique to the project directory (FNV-1a over the normalized path).
pub fn pipe_name(config_dir: &Path) -> String {
    let normalized = config_dir.to_string_lossy().to_lowercase();
    let hash = normalized
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\rom-{hash:016x}")
    }
    #[cfg(unix)]
    {
        use std::path::PathBuf;
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        runtime
            .join(format!("rom-{hash:016x}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

pub async fn request(pipe: &str, request: &Request) -> Result<Response, RomError> {
    let client = connect(pipe).await?;
    let (reader, mut writer) = tokio::io::split(client);

    let mut payload = serde_json::to_string(request)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;
    writer.flush().await?;

    let mut lines = BufReader::new(reader).lines();
    let line = lines.next_line().await?.ok_or(RomError::Protocol)?;
    Ok(serde_json::from_str(&line)?)
}

#[cfg(windows)]
async fn connect(pipe: &str) -> Result<tokio::net::windows::named_pipe::NamedPipeClient, RomError> {
    use std::time::Duration;

    use tokio::net::windows::named_pipe::ClientOptions;
    use tokio::time::sleep;

    for _ in 0..40 {
        match ClientOptions::new().open(pipe) {
            Ok(client) => return Ok(client),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                sleep(Duration::from_millis(25)).await;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(RomError::DaemonNotRunning);
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "daemon is busy").into())
}

#[cfg(unix)]
async fn connect(pipe: &str) -> Result<tokio::net::UnixStream, RomError> {
    match tokio::net::UnixStream::connect(pipe).await {
        Ok(stream) => Ok(stream),
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.kind() == io::ErrorKind::ConnectionRefused =>
        {
            Err(RomError::DaemonNotRunning)
        }
        Err(error) => Err(error.into()),
    }
}
