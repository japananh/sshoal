//! The persisted configuration model: a flat list of tunnels, each placed in a
//! slash-separated tree `path` (e.g. `gc/dev/db/app-api`) and pointed at an SSH
//! target. Load/save to a YAML file (the unit of export/import).
//!
//! Private keys are intentionally *not* stored here — sshoal relies on the
//! user's existing `~/.ssh` (config, keys, agent, known_hosts). The `ssh` field
//! is just an alias / `user@host` passed straight to `ssh`, so the same value
//! works in a plain terminal too.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// A single local→remote port forward, placed in the tree at `path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tunnel {
    /// Tree location, slash-separated. The last segment is the display name,
    /// e.g. `gc/dev/db/app-api` shows as `app-api` under `gc › dev › db`.
    pub path: String,
    /// SSH target passed straight to `ssh`: an `~/.ssh/config` alias (preferred)
    /// or `user@host`. Keeping it an alias means the same value works in a plain
    /// terminal and host details stay in one place (`~/.ssh/config`).
    pub ssh: String,
    /// Optional ssh port override. When `None`, ssh uses `~/.ssh/config` / 22.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_port: Option<u16>,
    /// Port opened on the local machine (the `L` side of `ssh -L`).
    pub local_port: u16,
    /// Host to forward to, as seen *from the ssh server* (e.g. `127.0.0.1` or an
    /// RDS endpoint).
    pub remote_host: String,
    /// Port on `remote_host` to forward to.
    pub remote_port: u16,
}

impl Tunnel {
    /// The leaf name (last path segment).
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    /// The non-empty path segments, e.g. `["gc", "dev", "db", "app-api"]`.
    pub fn segments(&self) -> Vec<&str> {
        self.path.split('/').filter(|s| !s.is_empty()).collect()
    }
}

/// The whole config file: just the tunnels (the tree is derived from `path`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub tunnels: Vec<Tunnel>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading config {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("writing config {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing config: {0}")]
    Parse(#[from] serde_yaml::Error),
}

impl AppConfig {
    /// Parse a config from a YAML string.
    pub fn from_yaml(yaml: &str) -> Result<Self, ConfigError> {
        Ok(serde_yaml::from_str(yaml)?)
    }

    /// Serialize to a YAML string.
    pub fn to_yaml(&self) -> Result<String, ConfigError> {
        Ok(serde_yaml::to_string(self)?)
    }

    /// Load from a file, returning an empty config if the file does not exist.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_yaml(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    /// Merge tunnels from `other` into this config, keyed by `path`. On a path
    /// collision, `overwrite` decides whether the incoming tunnel replaces the
    /// existing one (used by import: replace your local copy, or keep it).
    pub fn merge(&mut self, other: AppConfig, overwrite: bool) {
        for incoming in other.tunnels {
            match self.tunnels.iter_mut().find(|t| t.path == incoming.path) {
                Some(existing) if overwrite => *existing = incoming,
                Some(_) => {}
                None => self.tunnels.push(incoming),
            }
        }
    }

    /// Save to a file, creating parent directories as needed.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: parent.display().to_string(),
                source,
            })?;
        }
        std::fs::write(path, self.to_yaml()?).map_err(|source| ConfigError::Write {
            path: path.display().to_string(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AppConfig {
        AppConfig {
            tunnels: vec![Tunnel {
                path: "gc/dev/db/app-api".into(),
                ssh: "gemx-dev".into(),
                ssh_port: None,
                local_port: 54321,
                remote_host: "db.internal".into(),
                remote_port: 5432,
            }],
        }
    }

    #[test]
    fn tunnel_name_and_segments() {
        let t = &sample().tunnels[0];
        assert_eq!(t.name(), "app-api");
        assert_eq!(t.segments(), vec!["gc", "dev", "db", "app-api"]);
    }

    #[test]
    fn yaml_roundtrips() {
        let cfg = sample();
        let yaml = cfg.to_yaml().expect("serialize");
        let parsed = AppConfig::from_yaml(&yaml).expect("parse");
        assert_eq!(cfg, parsed);
    }

    #[test]
    fn load_missing_file_yields_empty_config() {
        let cfg = AppConfig::load("/nonexistent/sshoal/servers.yaml").expect("load");
        assert_eq!(cfg, AppConfig::default());
    }

    #[test]
    fn merge_adds_new_and_optionally_overwrites() {
        let mut base = sample(); // gc/dev/db/app-api -> 54321
        let mut incoming = sample();
        incoming.tunnels[0].local_port = 59999;
        incoming.tunnels.push(Tunnel {
            path: "gc/dev/redis/app-api".into(),
            ssh: "gemx-dev".into(),
            ssh_port: None,
            local_port: 63799,
            remote_host: "redis.internal".into(),
            remote_port: 6379,
        });

        let mut keep = base.clone();
        keep.merge(incoming.clone(), false);
        assert_eq!(keep.tunnels.len(), 2);
        assert_eq!(keep.tunnels[0].local_port, 54321); // untouched

        base.merge(incoming, true);
        assert_eq!(base.tunnels.len(), 2);
        assert_eq!(base.tunnels[0].local_port, 59999); // replaced
    }
}
