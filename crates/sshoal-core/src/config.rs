//! The persisted configuration model.
//!
//! Two lists: **ssh configs** (named connection targets — host/user/port/key,
//! GoLand-style) and **tunnels** (each placed in a slash tree `path` and
//! pointing at an ssh config by name). Keeping the connection details in our own
//! config (rather than only `~/.ssh/config`) makes an exported config
//! self-contained. Private keys themselves are never stored — only a path to
//! the key file.

use std::path::Path;

use serde::{Deserialize, Serialize};

fn default_port() -> u16 {
    22
}

/// A named SSH connection target — what a tunnel connects *through*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshConfig {
    /// Unique name; tunnels reference it via `Tunnel::ssh`.
    pub name: String,
    /// Hostname / IP, or an `~/.ssh/config` alias.
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Path to the private key (`ssh -i`). `None` lets ssh use its defaults /
    /// agent / `~/.ssh/config`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
}

impl SshConfig {
    /// A bare config that just defers to `ssh`/`~/.ssh/config` for `name`
    /// (used as a fallback when a tunnel references an unknown config).
    pub fn alias(name: &str) -> Self {
        Self {
            name: name.to_string(),
            host: name.to_string(),
            port: 22,
            user: None,
            identity_file: None,
        }
    }
}

/// A single local→remote port forward, placed in the tree at `path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tunnel {
    /// Tree location, slash-separated. The last segment is the display name,
    /// e.g. `gc/dev/db/app-api` shows as `app-api` under `gc › dev › db`.
    pub path: String,
    /// Name of the [`SshConfig`] this tunnel connects through.
    pub ssh: String,
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

fn default_true() -> bool {
    true
}

/// App-wide preferences (not tunnels). Persisted in the same YAML file under a
/// `settings:` key; an older config without the key still loads, defaulting
/// every field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Check GitHub for a newer release on launch and surface it in the UI.
    /// Default on: a check is read-only (it never installs), and the user can
    /// turn it off in Preferences.
    #[serde(default = "default_true")]
    pub auto_update_enabled: bool,
    /// A release tag the user dismissed; the update banner stays hidden for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_version: Option<String>,
    /// Folder tree paths the user has collapsed; every other folder is expanded.
    /// Stored as the *collapsed* set (not expanded) so a fresh or older config —
    /// where this is empty — starts with everything expanded, as before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collapsed_folders: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_update_enabled: true,
            skipped_version: None,
            collapsed_folders: Vec::new(),
        }
    }
}

/// The whole config file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub ssh_configs: Vec<SshConfig>,
    #[serde(default)]
    pub tunnels: Vec<Tunnel>,
    #[serde(default)]
    pub settings: Settings,
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

    /// Find an ssh config by name.
    pub fn ssh_config(&self, name: &str) -> Option<&SshConfig> {
        self.ssh_configs.iter().find(|c| c.name == name)
    }

    /// Resolve the ssh config a tunnel uses, falling back to a bare alias config
    /// so a tunnel that names an `~/.ssh/config` alias still works.
    pub fn resolve_ssh(&self, tunnel: &Tunnel) -> SshConfig {
        self.ssh_config(&tunnel.ssh)
            .cloned()
            .unwrap_or_else(|| SshConfig::alias(&tunnel.ssh))
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

    /// Merge `other` into this config. Tunnels are keyed by `path`, ssh configs
    /// by `name`; on a collision, `overwrite` decides whether the incoming entry
    /// replaces the existing one.
    pub fn merge(&mut self, other: AppConfig, overwrite: bool) {
        for incoming in other.ssh_configs {
            match self
                .ssh_configs
                .iter_mut()
                .find(|c| c.name == incoming.name)
            {
                Some(existing) if overwrite => *existing = incoming,
                Some(_) => {}
                None => self.ssh_configs.push(incoming),
            }
        }
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
            ssh_configs: vec![SshConfig {
                name: "gemx-dev".into(),
                host: "1.2.3.4".into(),
                port: 22,
                user: Some("deploy".into()),
                identity_file: Some("~/.ssh/dev.pem".into()),
            }],
            tunnels: vec![Tunnel {
                path: "gc/dev/db/app-api".into(),
                ssh: "gemx-dev".into(),
                local_port: 54321,
                remote_host: "db.internal".into(),
                remote_port: 5432,
            }],
            settings: Settings::default(),
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
        let parsed = AppConfig::from_yaml(&cfg.to_yaml().unwrap()).unwrap();
        assert_eq!(cfg, parsed);
    }

    #[test]
    fn resolve_ssh_falls_back_to_alias() {
        let cfg = sample();
        // known config
        assert_eq!(cfg.resolve_ssh(&cfg.tunnels[0]).host, "1.2.3.4");
        // unknown -> alias
        let t = Tunnel {
            path: "x".into(),
            ssh: "other".into(),
            local_port: 1,
            remote_host: "h".into(),
            remote_port: 2,
        };
        let resolved = cfg.resolve_ssh(&t);
        assert_eq!(resolved.host, "other");
        assert_eq!(resolved.user, None);
    }

    #[test]
    fn collapsed_folders_roundtrip_and_default_empty() {
        // A config predating the field still loads, defaulting to all-expanded.
        let old = "tunnels: []\nsettings:\n  auto_update_enabled: true\n";
        let cfg = AppConfig::from_yaml(old).unwrap();
        assert!(cfg.settings.collapsed_folders.is_empty());

        // And the field roundtrips when set.
        let mut cfg = sample();
        cfg.settings.collapsed_folders = vec!["gc".into(), "gc/dev".into()];
        let parsed = AppConfig::from_yaml(&cfg.to_yaml().unwrap()).unwrap();
        assert_eq!(parsed.settings.collapsed_folders, vec!["gc", "gc/dev"]);
    }

    #[test]
    fn load_missing_file_yields_empty_config() {
        let cfg = AppConfig::load("/nonexistent/sshoal/servers.yaml").unwrap();
        assert_eq!(cfg, AppConfig::default());
    }

    #[test]
    fn merge_keys_tunnels_by_path_and_ssh_by_name() {
        let mut base = sample();
        let mut incoming = sample();
        incoming.tunnels[0].local_port = 59999;
        incoming.ssh_configs[0].host = "9.9.9.9".into();

        let mut keep = base.clone();
        keep.merge(incoming.clone(), false);
        assert_eq!(keep.tunnels.len(), 1);
        assert_eq!(keep.tunnels[0].local_port, 54321); // untouched
        assert_eq!(keep.ssh_configs[0].host, "1.2.3.4");

        base.merge(incoming, true);
        assert_eq!(base.tunnels[0].local_port, 59999); // replaced
        assert_eq!(base.ssh_configs[0].host, "9.9.9.9");
    }
}
