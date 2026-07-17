//! The persisted configuration model.
//!
//! Two lists: **ssh configs** (named connection targets — host/user/port/key,
//! GoLand-style) and **tunnels** (each placed in a slash tree `path` and
//! pointing at an ssh config by name). Keeping the connection details in our own
//! config (rather than only `~/.ssh/config`) makes an exported config
//! self-contained. Private keys themselves are never stored — only a path to
//! the key file.

use std::collections::HashSet;
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

fn is_false(b: &bool) -> bool {
    !*b
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
    /// Register sshoal to launch at login, so tunnels reconnect after a reboot.
    /// Default off; kept in sync with the real OS login item on launch.
    #[serde(default, skip_serializing_if = "is_false")]
    pub open_at_login: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_update_enabled: true,
            skipped_version: None,
            collapsed_folders: Vec::new(),
            open_at_login: false,
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

    /// Tunnels selected for export: everything when `prefix` is `None`, otherwise
    /// the subtree at or under `prefix`. Matching is **segment-aware** — `gc/dev`
    /// matches `gc/dev` and `gc/dev/...` but not `gc/development`.
    pub fn select_tunnels(&self, prefix: Option<&str>) -> Vec<Tunnel> {
        match prefix {
            None => self.tunnels.clone(),
            Some(p) => {
                let child = format!("{p}/");
                self.tunnels
                    .iter()
                    .filter(|t| t.path == p || t.path.starts_with(&child))
                    .cloned()
                    .collect()
            }
        }
    }

    /// The ssh configs referenced (by `Tunnel.ssh` name) by `tunnels`, in this
    /// config's own order. A tunnel whose ssh name has no matching config is
    /// simply skipped here — on import it falls back to an alias, mirroring
    /// runtime [`resolve_ssh`]. This is what makes an export self-contained.
    pub fn referenced_ssh_configs(&self, tunnels: &[Tunnel]) -> Vec<SshConfig> {
        let needed: HashSet<&str> = tunnels.iter().map(|t| t.ssh.as_str()).collect();
        self.ssh_configs
            .iter()
            .filter(|c| needed.contains(c.name.as_str()))
            .cloned()
            .collect()
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
    fn open_at_login_roundtrips_and_defaults_off() {
        // A config predating the field still loads, defaulting to off.
        let old = "tunnels: []\nsettings:\n  auto_update_enabled: true\n";
        let cfg = AppConfig::from_yaml(old).unwrap();
        assert!(!cfg.settings.open_at_login);

        // Off is omitted from the YAML (skip_serializing_if); on roundtrips.
        let mut cfg = sample();
        assert!(!cfg.to_yaml().unwrap().contains("open_at_login"));
        cfg.settings.open_at_login = true;
        let yaml = cfg.to_yaml().unwrap();
        assert!(yaml.contains("open_at_login: true"));
        assert!(AppConfig::from_yaml(&yaml).unwrap().settings.open_at_login);
    }

    #[test]
    fn load_missing_file_yields_empty_config() {
        let cfg = AppConfig::load("/nonexistent/sshoal/servers.yaml").unwrap();
        assert_eq!(cfg, AppConfig::default());
    }

    #[test]
    fn select_tunnels_prefix_is_segment_aware() {
        let cfg = AppConfig {
            ssh_configs: vec![],
            tunnels: vec![
                Tunnel {
                    path: "gc/dev/db".into(),
                    ssh: "a".into(),
                    local_port: 1,
                    remote_host: "h".into(),
                    remote_port: 2,
                },
                Tunnel {
                    path: "gc/development/db".into(), // must NOT match "gc/dev"
                    ssh: "a".into(),
                    local_port: 3,
                    remote_host: "h".into(),
                    remote_port: 4,
                },
                Tunnel {
                    path: "gc/prod/db".into(),
                    ssh: "a".into(),
                    local_port: 5,
                    remote_host: "h".into(),
                    remote_port: 6,
                },
            ],
            settings: Settings::default(),
        };
        let under: Vec<_> = cfg
            .select_tunnels(Some("gc/dev"))
            .into_iter()
            .map(|t| t.path)
            .collect();
        assert_eq!(under, vec!["gc/dev/db"]);
        assert_eq!(cfg.select_tunnels(None).len(), 3); // None = all
    }

    #[test]
    fn select_tunnels_matches_on_segment_boundaries() {
        // A tree with siblings that share a textual prefix but *not* a path
        // segment, so a naive `starts_with(prefix)` would over-select.
        let paths = [
            "gc",         // exact leaf sitting at the prefix
            "gc/prod",    // child segment
            "gc/staging", // child segment
            "gcloud/db",  // shares the text "gc" but a different first segment
            "gcp-thing",  // ditto — no segment boundary after "gc"
            "other/gc",   // "gc" appears deeper, not at the root
        ];
        let cfg = AppConfig {
            ssh_configs: vec![],
            tunnels: paths
                .iter()
                .enumerate()
                .map(|(i, p)| Tunnel {
                    path: (*p).into(),
                    ssh: "a".into(),
                    local_port: i as u16,
                    remote_host: "h".into(),
                    remote_port: 0,
                })
                .collect(),
            settings: Settings::default(),
        };

        // Selected paths, sorted for a stable comparison.
        let selected = |prefix: Option<&str>| -> Vec<String> {
            let mut v: Vec<String> = cfg
                .select_tunnels(prefix)
                .into_iter()
                .map(|t| t.path)
                .collect();
            v.sort();
            v
        };

        // (prefix, expected selected paths).
        let cases: &[(Option<&str>, &[&str])] = &[
            // exact segment match + child segments; no false positives.
            (Some("gc"), &["gc", "gc/prod", "gc/staging"]),
            // a deeper exact segment selects just itself here.
            (Some("gc/prod"), &["gc/prod"]),
            // false-positive guard: "gcloud"/"gcp-thing" never match "gc".
            (Some("gcloud"), &["gcloud/db"]),
            // `None` selects everything.
            (
                None,
                &[
                    "gc",
                    "gc/prod",
                    "gc/staging",
                    "gcloud/db",
                    "gcp-thing",
                    "other/gc",
                ],
            ),
            // a prefix that matches nothing.
            (Some("nope"), &[]),
            // empty prefix `Some("")` — documents current behavior: it matches
            // nothing (the child glob becomes "/", and no ordinary path equals
            // "" or starts with "/"). Use `None`, not `Some("")`, for "all".
            (Some(""), &[]),
        ];

        for (prefix, expected) in cases {
            let mut want: Vec<String> = expected.iter().map(|s| (*s).to_string()).collect();
            want.sort();
            assert_eq!(
                selected(*prefix),
                want,
                "prefix {prefix:?} selected the wrong tunnels"
            );
        }
    }

    #[test]
    fn referenced_ssh_configs_gathers_only_used_and_skips_missing() {
        let cfg = AppConfig {
            ssh_configs: vec![SshConfig::alias("used"), SshConfig::alias("unused")],
            tunnels: vec![
                Tunnel {
                    path: "a".into(),
                    ssh: "used".into(),
                    local_port: 1,
                    remote_host: "h".into(),
                    remote_port: 2,
                },
                Tunnel {
                    path: "b".into(),
                    ssh: "alias-only".into(), // no matching SshConfig → skipped
                    local_port: 3,
                    remote_host: "h".into(),
                    remote_port: 4,
                },
            ],
            settings: Settings::default(),
        };
        let refs: Vec<_> = cfg
            .referenced_ssh_configs(&cfg.tunnels)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(refs, vec!["used"]); // only "used"; "unused" dropped, "alias-only" skipped
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

    #[test]
    fn defaults_fill_omitted_fields() {
        // `port` omitted on the ssh config, and `settings` an empty map so
        // `auto_update_enabled` is absent — exercises the serde default helpers.
        let yaml = "ssh_configs:\n  - name: h\n    host: example.com\ntunnels: []\nsettings: {}\n";
        let cfg = AppConfig::from_yaml(yaml).unwrap();
        assert_eq!(cfg.ssh_configs[0].port, 22);
        assert!(cfg.settings.auto_update_enabled);
    }

    #[test]
    fn save_creates_dirs_and_load_roundtrips() {
        let dir = std::env::temp_dir().join(format!("sshoal-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested/servers.yaml"); // nested → exercises create_dir_all
        let cfg = sample();
        cfg.save(&path).unwrap();
        assert_eq!(AppConfig::load(&path).unwrap(), cfg);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
