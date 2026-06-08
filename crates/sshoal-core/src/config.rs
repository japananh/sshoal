//! The persisted configuration model: the servers a user manages, the tunnels
//! each one forwards, and load/save to a YAML file (the unit of export/import).
//!
//! Private keys are intentionally *not* stored here — sshoal relies on the
//! user's existing `~/.ssh` (config, keys, agent, known_hosts). This file holds
//! only the tunnel topology and labels, which is what makes it safe-ish to
//! export and copy between machines.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// A single local→remote port forward over an SSH connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelSpec {
    /// Port opened on the local machine (the `L` side of `ssh -L`).
    pub local_port: u16,
    /// Host the server should forward to, as seen *from the server*
    /// (e.g. `127.0.0.1` for a service on the box, or `db.internal`).
    pub remote_host: String,
    /// Port on `remote_host` to forward to.
    pub remote_port: u16,
}

/// One server the user connects to, plus the tunnels to bring up for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Human label shown in the tray/UI.
    pub name: String,
    /// Hostname or IP. May be an alias defined in `~/.ssh/config`.
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// SSH user. When `None`, ssh resolves it from `~/.ssh/config`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Optional group label for bulk "connect all in group" actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tunnels: Vec<TunnelSpec>,
}

fn default_port() -> u16 {
    22
}

/// The whole config file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
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

    /// Merge servers from `other` into this config. On a name collision,
    /// `overwrite` decides whether the incoming server replaces the existing one
    /// (used by import: replace your local copy, or keep it).
    pub fn merge(&mut self, other: AppConfig, overwrite: bool) {
        for incoming in other.servers {
            match self.servers.iter_mut().find(|s| s.name == incoming.name) {
                Some(existing) if overwrite => *existing = incoming,
                Some(_) => {}
                None => self.servers.push(incoming),
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
            servers: vec![ServerConfig {
                name: "staging db".into(),
                host: "staging.example.com".into(),
                port: 22,
                user: Some("deploy".into()),
                group: Some("staging".into()),
                tunnels: vec![TunnelSpec {
                    local_port: 5432,
                    remote_host: "127.0.0.1".into(),
                    remote_port: 5432,
                }],
            }],
        }
    }

    #[test]
    fn yaml_roundtrips() {
        let cfg = sample();
        let yaml = cfg.to_yaml().expect("serialize");
        let parsed = AppConfig::from_yaml(&yaml).expect("parse");
        assert_eq!(cfg, parsed);
    }

    #[test]
    fn port_defaults_to_22_when_omitted() {
        let yaml = r#"
servers:
  - name: web
    host: web.example.com
"#;
        let cfg = AppConfig::from_yaml(yaml).expect("parse");
        assert_eq!(cfg.servers[0].port, 22);
        assert!(cfg.servers[0].tunnels.is_empty());
    }

    #[test]
    fn load_missing_file_yields_empty_config() {
        let cfg = AppConfig::load("/nonexistent/sshoal/servers.yaml").expect("load");
        assert_eq!(cfg, AppConfig::default());
    }

    #[test]
    fn merge_adds_new_and_optionally_overwrites() {
        let mut base = sample(); // one server named "staging db"
        let mut incoming = sample();
        incoming.servers[0].host = "changed.example.com".into();
        incoming.servers.push(ServerConfig {
            name: "web".into(),
            host: "web.example.com".into(),
            port: 22,
            user: None,
            group: None,
            tunnels: vec![],
        });

        // Without overwrite: new server added, existing one untouched.
        let mut keep = base.clone();
        keep.merge(incoming.clone(), false);
        assert_eq!(keep.servers.len(), 2);
        assert_eq!(keep.servers[0].host, "staging.example.com");

        // With overwrite: existing server replaced.
        base.merge(incoming, true);
        assert_eq!(base.servers.len(), 2);
        assert_eq!(base.servers[0].host, "changed.example.com");
    }
}
