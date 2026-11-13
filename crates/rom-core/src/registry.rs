use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{Config, Mode};
use crate::error::RomError;

#[derive(Debug, Clone)]
pub struct HealthCheck {
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct ServiceDef {
    pub name: String,
    pub dir: PathBuf,
    pub command: String,
    pub args: Vec<String>,
    pub port: u16,
    pub mode: Mode,
    pub env: BTreeMap<String, String>,
    pub health: Option<HealthCheck>,
    pub dependencies: Vec<String>,
}

/// Validated, read-only view of `rom.yaml`. Services are kept in start order
/// (dependencies before dependents).
#[derive(Debug, Clone)]
pub struct Registry {
    pub project: String,
    pub startup_timeout: Duration,
    pub shutdown_timeout: Duration,
    services: BTreeMap<String, ServiceDef>,
    profiles: BTreeMap<String, Vec<String>>,
    order: Vec<String>,
}

impl Registry {
    pub fn from_config(config: Config, config_dir: &Path) -> Result<Self, RomError> {
        let mut errors = Vec::new();
        if config.version != 1 {
            errors.push(format!("unsupported version {} (expected 1)", config.version));
        }

        let root = config_dir.join(&config.project.root);
        let mut services = BTreeMap::new();
        let mut ports: BTreeMap<u16, &str> = BTreeMap::new();

        for (name, svc) in &config.services {
            if !is_valid_name(name) {
                errors.push(format!(
                    "service name `{name}` must use lowercase letters, digits and dashes"
                ));
            }

            let dir = root.join(&svc.path);
            if !dir.is_dir() {
                errors.push(format!(
                    "`{name}`: path `{}` is not an existing directory",
                    dir.display()
                ));
            }

            if svc.command.trim().is_empty() {
                errors.push(format!("`{name}`: command is empty"));
            }

            if svc.port == 0 {
                errors.push(format!("`{name}`: port must be between 1 and 65535"));
            } else if let Some(other) = ports.insert(svc.port, name.as_str()) {
                errors.push(format!(
                    "port {} is declared by both `{other}` and `{name}`",
                    svc.port
                ));
            }

            for dep in &svc.dependencies {
                if dep == name {
                    errors.push(format!("`{name}` cannot depend on itself"));
                } else if !config.services.contains_key(dep) {
                    errors.push(format!("`{name}`: dependency `{dep}` is not defined"));
                }
            }

            services.insert(
                name.clone(),
                ServiceDef {
                    name: name.clone(),
                    dir,
                    command: svc.command.clone(),
                    args: svc.args.clone(),
                    port: svc.port,
                    mode: svc.mode,
                    env: svc.env.clone(),
                    health: svc.health.as_ref().map(|h| HealthCheck {
                        path: h.path.clone(),
                    }),
                    dependencies: svc.dependencies.clone(),
                },
            );
        }

        let mut profiles = BTreeMap::new();
        for (profile_name, profile) in &config.profiles {
            for svc in &profile.services {
                if !config.services.contains_key(svc) {
                    errors.push(format!(
                        "profile `{profile_name}` references unknown service `{svc}`"
                    ));
                }
            }
            profiles.insert(profile_name.clone(), profile.services.clone());
        }

        if !errors.is_empty() {
            return Err(RomError::InvalidConfig(errors));
        }

        let order = topological_order(&services)
            .map_err(|cycle| RomError::InvalidConfig(vec![format!("dependency cycle: {cycle}")]))?;

        Ok(Self {
            project: config.project.name,
            startup_timeout: config.runtime.startup_timeout,
            shutdown_timeout: config.runtime.shutdown_timeout,
            services,
            profiles,
            order,
        })
    }

    pub fn get(&self, name: &str) -> Result<&ServiceDef, RomError> {
        self.services
            .get(name)
            .ok_or_else(|| RomError::UnknownService(name.to_owned()))
    }

    /// All service names in start order.
    pub fn names(&self) -> Vec<String> {
        self.order.clone()
    }

    /// All service definitions in start order.
    pub fn services(&self) -> impl Iterator<Item = &ServiceDef> + '_ {
        self.order.iter().map(|name| &self.services[name])
    }

    pub fn profile(&self, name: &str) -> Result<&[String], RomError> {
        self.profiles
            .get(name)
            .map(Vec::as_slice)
            .ok_or_else(|| RomError::UnknownProfile(name.to_owned()))
    }

    /// The requested services plus all their transitive dependencies, in start order.
    pub fn resolve_start_set(&self, targets: &[String]) -> Result<Vec<String>, RomError> {
        let mut wanted = BTreeSet::new();
        let mut pending: Vec<&str> = targets.iter().map(String::as_str).collect();
        while let Some(name) = pending.pop() {
            let def = self.get(name)?;
            if wanted.insert(def.name.clone()) {
                pending.extend(def.dependencies.iter().map(String::as_str));
            }
        }
        Ok(self.sort_for_start(&wanted))
    }

    /// Every service that depends, directly or not, on `name`.
    pub fn transitive_dependents(&self, name: &str) -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        for svc in self.services() {
            if svc.dependencies.iter().any(|dep| dep == name || found.contains(dep)) {
                found.insert(svc.name.clone());
            }
        }
        found
    }

    pub fn sort_for_start(&self, set: &BTreeSet<String>) -> Vec<String> {
        self.order
            .iter()
            .filter(|name| set.contains(*name))
            .cloned()
            .collect()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    Visiting,
    Done,
}

fn topological_order(services: &BTreeMap<String, ServiceDef>) -> Result<Vec<String>, String> {
    let mut marks = BTreeMap::new();
    let mut stack = Vec::new();
    let mut order = Vec::new();
    for name in services.keys() {
        visit(name, services, &mut marks, &mut stack, &mut order)?;
    }
    Ok(order)
}

fn visit(
    name: &str,
    services: &BTreeMap<String, ServiceDef>,
    marks: &mut BTreeMap<String, Mark>,
    stack: &mut Vec<String>,
    order: &mut Vec<String>,
) -> Result<(), String> {
    match marks.get(name) {
        Some(Mark::Done) => return Ok(()),
        Some(Mark::Visiting) => {
            let start = stack.iter().position(|s| s == name).unwrap_or(0);
            let mut cycle = stack[start..].to_vec();
            cycle.push(name.to_owned());
            return Err(cycle.join(" -> "));
        }
        None => {}
    }

    marks.insert(name.to_owned(), Mark::Visiting);
    stack.push(name.to_owned());
    for dep in &services[name].dependencies {
        visit(dep, services, marks, stack, order)?;
    }
    stack.pop();
    marks.insert(name.to_owned(), Mark::Done);
    order.push(name.to_owned());
    Ok(())
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_yaml(services: &[(&str, u16, &str)], extra: &str) -> String {
        let mut yaml = String::from("version: 1\nproject:\n  name: test\nservices:\n");
        for (name, port, deps) in services {
            yaml += &format!(
                "  {name}:\n    path: .\n    command: node\n    port: {port}\n    dependencies: [{deps}]\n"
            );
        }
        yaml + extra
    }

    fn build(yaml: &str) -> Result<Registry, RomError> {
        let config: Config = serde_yaml_ng::from_str(yaml).expect("test yaml parses");
        Registry::from_config(config, Path::new(env!("CARGO_MANIFEST_DIR")))
    }

    fn invalid_messages(result: Result<Registry, RomError>) -> Vec<String> {
        match result {
            Err(RomError::InvalidConfig(messages)) => messages,
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
    }

    #[test]
    fn orders_dependencies_first() {
        let registry = build(&config_yaml(
            &[("app", 3001, "db"), ("db", 3002, "")],
            "",
        ))
        .unwrap();
        assert_eq!(registry.names(), vec!["db", "app"]);
        let set = registry.resolve_start_set(&["app".to_owned()]).unwrap();
        assert_eq!(set, vec!["db", "app"]);
    }

    #[test]
    fn reports_duplicate_ports() {
        let messages = invalid_messages(build(&config_yaml(
            &[("a", 3001, ""), ("b", 3001, "")],
            "",
        )));
        assert!(messages.iter().any(|m| m.contains("port 3001")));
    }

    #[test]
    fn reports_dependency_cycles() {
        let messages = invalid_messages(build(&config_yaml(
            &[("a", 3001, "b"), ("b", 3002, "a")],
            "",
        )));
        assert!(messages.iter().any(|m| m.contains("cycle")));
    }

    #[test]
    fn reports_unknown_dependency_and_profile_service() {
        let messages = invalid_messages(build(&config_yaml(
            &[("a", 3001, "ghost")],
            "profiles:\n  p:\n    services: [nobody]\n",
        )));
        assert!(messages.iter().any(|m| m.contains("`ghost`")));
        assert!(messages.iter().any(|m| m.contains("`nobody`")));
    }

    #[test]
    fn reports_missing_directory_and_invalid_names() {
        let yaml = "version: 1\nproject:\n  name: test\nservices:\n  Bad_Name:\n    path: ./does-not-exist\n    command: node\n    port: 3001\n";
        let messages = invalid_messages(build(yaml));
        assert!(messages.iter().any(|m| m.contains("Bad_Name")));
        assert!(messages.iter().any(|m| m.contains("does-not-exist")));
    }

    #[test]
    fn transitive_dependents_follow_the_chain() {
        let registry = build(&config_yaml(
            &[("db", 3001, ""), ("user", 3002, "db"), ("fin", 3003, "user")],
            "",
        ))
        .unwrap();
        let dependents = registry.transitive_dependents("db");
        assert_eq!(dependents.into_iter().collect::<Vec<_>>(), vec!["fin", "user"]);
    }
}
