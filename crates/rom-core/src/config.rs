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
    #[serde(default)]
    pub watch: WatchConfig,
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
    /// Stop an on-demand service after this long with no proxied connections.
    #[serde(deserialize_with = "deserialize_duration")]
    pub idle_timeout: Duration,
    /// Close a proxied connection after this long with no bytes in either direction.
    #[serde(deserialize_with = "deserialize_duration")]
    pub connection_idle: Duration,
    /// When true, `rom up` colors only the `[service]` prefix. Log text is not recolored.
    pub prefix_colors: bool,
}

/// Commented hint inserted into generated `rom.yaml` files. Kept commented so
/// prefix colors stay off until someone opts in.
pub const PREFIX_COLORS_YAML_HINT: &str = r#"  # Color only the [service] prefix in `rom up`, like concurrently.
  # Prefixes share one width: the longest name in this run, capped at 20.
  # The cap is used only when a name is longer than that. Shorter names are
  # padded; longer ones compact to `sta..end`. The log text itself is not colored.
  # Has no effect when stdout is not a terminal or when NO_COLOR is set.
  # Uncomment to enable:
  # prefix_colors: true
"#;

impl Default for Runtime {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(30),
            connection_idle: Duration::from_secs(5),
            prefix_colors: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WatchConfig {
    /// Coalesce bursts: minimum time from the first change before a restart.
    #[serde(deserialize_with = "deserialize_duration")]
    pub debounce: Duration,
    /// Quiet window after the last change before a restart runs.
    #[serde(deserialize_with = "deserialize_duration")]
    pub settle: Duration,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(300),
            settle: Duration::from_millis(500),
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
    /// `persistent` services are never stopped for idle (queues, cron).
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
    /// Service names. `service:label` (no space around `:`) shows `label` in `rom up`.
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
        _ => Err(format!(
            "invalid duration `{input}` (use e.g. 500ms, 30s, 2m)"
        )),
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

    #[test]
    fn prefix_colors_stay_off_unless_enabled() {
        let off: Config =
            serde_yaml_ng::from_str("version: 1\nproject:\n  name: t\nservices: {}\n").unwrap();
        assert!(!off.runtime.prefix_colors);

        let hinted = format!(
            "version: 1\nproject:\n  name: t\nruntime:\n  startup_timeout: 30s\n{PREFIX_COLORS_YAML_HINT}services: {{}}\n"
        );
        let from_hint: Config = serde_yaml_ng::from_str(&hinted).unwrap();
        assert!(!from_hint.runtime.prefix_colors);
        assert!(PREFIX_COLORS_YAML_HINT.contains("# prefix_colors: true"));
        assert!(
            !PREFIX_COLORS_YAML_HINT
                .lines()
                .any(|line| line.trim_start().starts_with("prefix_colors:"))
        );

        let on: Config = serde_yaml_ng::from_str(
            "version: 1\nproject:\n  name: t\nruntime:\n  prefix_colors: true\nservices: {}\n",
        )
        .unwrap();
        assert!(on.runtime.prefix_colors);
    }
}
