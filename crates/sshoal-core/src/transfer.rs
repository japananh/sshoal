//! Export/import of the config as a portable blob — the "reuse on another
//! machine" feature.
//!
//! Plaintext by default (it's just the YAML, easy to copy around). With a
//! passphrase the blob is encrypted with [`age`] (scrypt passphrase recipient),
//! for when the config carries real production hosts. Private keys are never in
//! here regardless — sshoal only ever stores tunnel topology.

use std::io::{Read, Write};
use std::iter;

use age::secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, SshConfig, Tunnel};

/// age files begin with this header, which lets `import` auto-detect whether a
/// blob is encrypted.
const AGE_MAGIC: &[u8] = b"age-encryption.org";

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("serializing config: {0}")]
    Serialize(#[from] serde_yaml::Error),
    #[error("encrypting: {0}")]
    Encrypt(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("this file is encrypted; a passphrase is required")]
    PassphraseRequired,
    #[error("decryption failed (wrong passphrase or corrupt file)")]
    Decrypt,
    #[error("parsing config: {0}")]
    Parse(#[from] serde_yaml::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// A self-contained, portable subset of a config: tunnels plus only the
/// [`SshConfig`]s they reference. `settings` are deliberately excluded — they're
/// machine-specific (`collapsed_folders`, `skipped_version`, `auto_update_enabled`)
/// and `AppConfig::merge` ignores incoming settings on import anyway.
///
/// A `PortableConfig` YAML is a strict subset of an `AppConfig` YAML (same
/// top-level `ssh_configs` / `tunnels` keys), so [`import`] parses it straight
/// into an `AppConfig` with default settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableConfig {
    #[serde(default)]
    pub ssh_configs: Vec<SshConfig>,
    #[serde(default)]
    pub tunnels: Vec<Tunnel>,
}

impl PortableConfig {
    /// Build a self-contained subset of `config`: the tunnels selected by
    /// `prefix` (see [`AppConfig::select_tunnels`]) plus the ssh configs they
    /// reference. With `strip_identity`, `identity_file` paths are dropped (they
    /// can be machine-specific); no key material is ever included regardless.
    pub fn build(config: &AppConfig, prefix: Option<&str>, strip_identity: bool) -> Self {
        let tunnels = config.select_tunnels(prefix);
        let mut ssh_configs = config.referenced_ssh_configs(&tunnels);
        if strip_identity {
            for c in &mut ssh_configs {
                c.identity_file = None;
            }
        }
        Self {
            ssh_configs,
            tunnels,
        }
    }
}

/// Serialize the whole `config` to a portable blob, encrypting if a passphrase
/// is given. (For a self-contained, settings-free subset, prefer
/// [`export_portable`].)
pub fn export(config: &AppConfig, passphrase: Option<&str>) -> Result<Vec<u8>, ExportError> {
    seal(serde_yaml::to_string(config)?.into_bytes(), passphrase)
}

/// Serialize a [`PortableConfig`] to a portable blob, encrypting if a passphrase
/// is given.
pub fn export_portable(
    portable: &PortableConfig,
    passphrase: Option<&str>,
) -> Result<Vec<u8>, ExportError> {
    seal(serde_yaml::to_string(portable)?.into_bytes(), passphrase)
}

/// Pass YAML bytes through unchanged (plaintext) or wrap them with age scrypt
/// passphrase encryption.
fn seal(yaml: Vec<u8>, passphrase: Option<&str>) -> Result<Vec<u8>, ExportError> {
    let Some(pass) = passphrase else {
        return Ok(yaml);
    };
    let encryptor = age::Encryptor::with_user_passphrase(SecretString::from(pass.to_owned()));
    let mut out = Vec::new();
    let mut writer = encryptor
        .wrap_output(&mut out)
        .map_err(|e| ExportError::Encrypt(e.to_string()))?;
    writer.write_all(&yaml)?;
    writer
        .finish()
        .map_err(|e| ExportError::Encrypt(e.to_string()))?;
    Ok(out)
}

/// Parse a blob produced by [`export`], decrypting first if it is encrypted.
pub fn import(bytes: &[u8], passphrase: Option<&str>) -> Result<AppConfig, ImportError> {
    let plaintext = if bytes.starts_with(AGE_MAGIC) {
        let pass = passphrase.ok_or(ImportError::PassphraseRequired)?;
        decrypt(bytes, pass)?
    } else {
        bytes.to_vec()
    };
    let text = String::from_utf8(plaintext).map_err(|_| ImportError::Decrypt)?;
    Ok(serde_yaml::from_str(&text)?)
}

fn decrypt(bytes: &[u8], passphrase: &str) -> Result<Vec<u8>, ImportError> {
    let decryptor = age::Decryptor::new(bytes).map_err(|_| ImportError::Decrypt)?;
    let identity = age::scrypt::Identity::new(SecretString::from(passphrase.to_owned()));
    let mut reader = decryptor
        .decrypt(iter::once(&identity as &dyn age::Identity))
        .map_err(|_| ImportError::Decrypt)?;
    let mut out = Vec::new();
    reader.read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Settings, Tunnel};

    fn sample() -> AppConfig {
        AppConfig {
            ssh_configs: vec![],
            tunnels: vec![Tunnel {
                path: "gc/prod/db/app-api".into(),
                ssh: "gemx-pro".into(),
                local_port: 5432,
                remote_host: "127.0.0.1".into(),
                remote_port: 5432,
            }],
            settings: Default::default(),
        }
    }

    fn tunnel(path: &str, ssh: &str, local: u16) -> Tunnel {
        Tunnel {
            path: path.into(),
            ssh: ssh.into(),
            local_port: local,
            remote_host: "db.internal".into(),
            remote_port: 5432,
        }
    }

    fn ssh(name: &str) -> SshConfig {
        SshConfig {
            name: name.into(),
            host: format!("{name}.example.com"),
            port: 22,
            user: Some("deploy".into()),
            identity_file: Some(format!("~/.ssh/{name}.pem")),
        }
    }

    /// Two ssh configs (one unused) and tunnels under two subtrees.
    fn full() -> AppConfig {
        AppConfig {
            ssh_configs: vec![ssh("dev"), ssh("prod"), ssh("unused")],
            tunnels: vec![
                tunnel("gc/dev/db", "dev", 1),
                tunnel("gc/dev/redis", "dev", 2),
                tunnel("gc/prod/db", "prod", 3),
            ],
            settings: Settings {
                skipped_version: Some("v9.9.9".into()),
                collapsed_folders: vec!["gc/prod".into()],
                ..Settings::default()
            },
        }
    }

    #[test]
    fn portable_export_is_self_contained_and_roundtrips_into_empty() {
        let cfg = full();
        let portable = PortableConfig::build(&cfg, None, false); // --all
        // Self-contained: includes the referenced configs (dev, prod), not "unused".
        let names: Vec<_> = portable
            .ssh_configs
            .iter()
            .map(|c| c.name.clone())
            .collect();
        assert_eq!(names, vec!["dev", "prod"]);

        // Import into an empty config recreates identical tunnels + referenced configs.
        let blob = export_portable(&portable, None).unwrap();
        let mut empty = AppConfig::default();
        empty.merge(import(&blob, None).unwrap(), true);
        assert_eq!(empty.tunnels, cfg.tunnels);
        assert_eq!(empty.ssh_configs, vec![ssh("dev"), ssh("prod")]);
        // Settings never travel (the importer's stay default).
        assert_eq!(empty.settings, Settings::default());
    }

    #[test]
    fn portable_export_excludes_settings_from_the_file() {
        let blob = export_portable(&PortableConfig::build(&full(), None, false), None).unwrap();
        let text = String::from_utf8(blob).unwrap();
        assert!(
            !text.contains("settings"),
            "portable file must not carry settings"
        );
        assert!(!text.contains("v9.9.9") && !text.contains("collapsed_folders"));
    }

    #[test]
    fn portable_export_by_prefix_selects_subtree_and_its_configs() {
        let portable = PortableConfig::build(&full(), Some("gc/dev"), false);
        let paths: Vec<_> = portable.tunnels.iter().map(|t| t.path.clone()).collect();
        assert_eq!(paths, vec!["gc/dev/db", "gc/dev/redis"]);
        // Only the config those tunnels use (dev), not prod/unused.
        let names: Vec<_> = portable
            .ssh_configs
            .iter()
            .map(|c| c.name.clone())
            .collect();
        assert_eq!(names, vec!["dev"]);
    }

    #[test]
    fn strip_identity_drops_key_paths() {
        let portable = PortableConfig::build(&full(), None, true);
        assert!(
            portable
                .ssh_configs
                .iter()
                .all(|c| c.identity_file.is_none())
        );
        let text = String::from_utf8(export_portable(&portable, None).unwrap()).unwrap();
        assert!(!text.contains("identity_file"));
    }

    #[test]
    fn reimport_is_idempotent() {
        let blob = export_portable(&PortableConfig::build(&full(), None, false), None).unwrap();
        let mut cfg = AppConfig::default();
        cfg.merge(import(&blob, None).unwrap(), false); // skip
        cfg.merge(import(&blob, None).unwrap(), false); // re-import, skip
        assert_eq!(cfg.tunnels.len(), 3); // no duplicates — merge is keyed
        assert_eq!(cfg.ssh_configs.len(), 2);
    }

    #[test]
    fn plaintext_roundtrips() {
        let cfg = sample();
        let blob = export(&cfg, None).expect("export");
        assert!(!blob.starts_with(AGE_MAGIC));
        let back = import(&blob, None).expect("import");
        assert_eq!(cfg, back);
    }

    #[test]
    fn encrypted_roundtrips() {
        let cfg = sample();
        let blob = export(&cfg, Some("correct horse")).expect("export");
        assert!(blob.starts_with(AGE_MAGIC), "blob should be age-encrypted");
        let back = import(&blob, Some("correct horse")).expect("import");
        assert_eq!(cfg, back);
    }

    #[test]
    fn wrong_passphrase_fails() {
        let blob = export(&sample(), Some("right")).expect("export");
        let err = import(&blob, Some("wrong")).unwrap_err();
        assert!(matches!(err, ImportError::Decrypt));
    }

    #[test]
    fn encrypted_blob_needs_a_passphrase() {
        let blob = export(&sample(), Some("pw")).expect("export");
        let err = import(&blob, None).unwrap_err();
        assert!(matches!(err, ImportError::PassphraseRequired));
    }
}
