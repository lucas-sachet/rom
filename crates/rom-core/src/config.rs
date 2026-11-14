use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer};

use crate::error::RomError;

pub const CONFIG_FILE_NAMES: [&str; 2] = ["rom.yaml", "rom.yml"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub project: Project,
    #[serde(default)]
    pub runtime: Runtime,
    pub services: BTreeMap<String, ServiceConfig>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub name: String,
    /// Directory that service `path` values are relative to, itself relative to the config file.
    #[serde(default = "default_root")]
    pub root: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Runtime {
    #[serde(deserialize_with = "deserialize_duration")]
    pub startup_timeout: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub shutdown_timeout: Duration,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub path: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub port: u16,
    /// Accepted for forward compatibility; idle stop (stage 3) will honour `persistent`.
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub health: Option<HealthConfig>,
    #[serde(default)]
    pub dependencies: Vec<String>,
}

#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    #[default]
    OnDemand,
    Persistent,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthConfig {
    #[serde(rename = "type")]
    pub kind: HealthKind,
    pub path: String,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum HealthKind {
    Http,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub services: Vec<String>,
}

fn default_root() -> String {
    ".".to_owned()
}

/// Parses durations such as `500ms`, `30s` or `2m`.
pub fn parse_duration(input: &str) -> Result<Duration, String> {
    let trimmed = input.trim();
    let split = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (digits, unit) = trimmed.split_at(split);
    let value: u64 = digits
        .parse()
        .map_err(|_| format!("invalid duration `{input}` (use e.g. 500ms, 30s, 2m)"))?;
    match unit {
        "ms" => Ok(Duration::from_millis(value)),
        "s" => Ok(Duration::from_secs(value)),
        "m" => Ok(Duration::from_secs(value * 60)),
        _ => Err(format!("invalid duration `{input}` (use e.g. 500ms, 30s, 2m)")),
    }
}

fn deserialize_duration<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    let raw = String::deserialize(deserializer)?;
    parse_duration(&raw).map_err(D::Error::custom)
}

/// Walks up from `start` looking for `rom.yaml` (or `rom.yml`).
pub fn find_config(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .flat_map(|dir| CONFIG_FILE_NAMES.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

pub fn load(path: &Path) -> Result<Config, RomError> {
    let text = fs::read_to_string(path).map_err(|source| RomError::ReadConfig {
        path: path.display().to_string(),
        source,
    })?;
    Ok(serde_yaml_ng::from_str(&text)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_duration("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_duration("2m"), Ok(Duration::from_secs(120)));
        assert!(parse_duration("30").is_err());
        assert!(parse_duration("abc").is_err());
    }
}
