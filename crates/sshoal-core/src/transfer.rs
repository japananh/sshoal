//! Export/import of the config as a portable blob — the "reuse on another
//! machine" feature.
//!
//! Plaintext is just the YAML (easy to inspect/diff). With a passphrase the blob
//! is encrypted with **Argon2id** (key derivation) + **XChaCha20-Poly1305**
//! (authenticated encryption), both from RustCrypto; we own only the small
//! versioned envelope around them. By default the export carries only tunnel
//! topology + ssh-config metadata (paths, not keys); private-key *contents* are
//! included only when the caller opts in (the `keys` section).

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, SshConfig, Tunnel};

/// Our encrypted blobs start with these 8 bytes (`c1` = format version 1), so
/// `import` can tell an encrypted blob from plaintext YAML.
const MAGIC: &[u8; 8] = b"SSHOALc1";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24; // XChaCha20-Poly1305 nonce
const KEY_LEN: usize = 32;
/// Header layout: MAGIC(8) | m_cost(4) | t_cost(4) | p_cost(4) | salt(16) | nonce(24).
const HEADER_LEN: usize = 8 + 4 + 4 + 4 + SALT_LEN + NONCE_LEN;
/// Argon2id parameters — the OWASP 2026 baseline (m=19 MiB, t=2, p=1). Stored in
/// the header so a future bump can still decrypt old blobs.
const M_COST: u32 = 19_456;
const T_COST: u32 = 2;
const P_COST: u32 = 1;
/// Upper bounds on the *untrusted* header params we'll honour on decrypt. argon2
/// itself allows absurd values (m up to ~256 GiB), so a hostile blob could OOM
/// us before the AEAD tag is ever checked — reject out-of-range params first.
/// 1 GiB / t=16 / p=16 sits far above any sane setting but well below a crash.
const MAX_M_COST: u32 = 1_048_576; // KiB = 1 GiB
const MAX_T_COST: u32 = 16;
const MAX_P_COST: u32 = 16;

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("serializing config: {0}")]
    Serialize(#[from] serde_yaml::Error),
    #[error("encrypting: {0}")]
    Encrypt(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("this file is encrypted; a passphrase is required")]
    PassphraseRequired,
    #[error("decryption failed (wrong passphrase or corrupt file)")]
    Decrypt,
    #[error("parsing config: {0}")]
    Parse(#[from] serde_yaml::Error),
}

fn default_port() -> u16 {
    22
}

/// A self-contained, portable subset of a config: tunnels plus only the ssh
/// configs they reference. `settings` are deliberately excluded — they're
/// machine-specific (`collapsed_folders`, `skipped_version`, `auto_update_enabled`)
/// and `AppConfig::merge` ignores incoming settings on import anyway.
///
/// This is a **distinct type** from [`AppConfig`] on purpose: embedded private
/// keys live under [`PortableSsh::identity_files`], and the plain [`SshConfig`]
/// has no key field — so importing into the main config structurally can never
/// carry key material onto disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableConfig {
    #[serde(default)]
    pub ssh_configs: Vec<PortableSsh>,
    #[serde(default)]
    pub tunnels: Vec<Tunnel>,
}

/// An ssh config inside an export: the same fields as [`SshConfig`], plus
/// optional embedded key material (`identity_files`, empty unless the user opted
/// into `--include-keys`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableSsh {
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
    /// Embedded private keys for this config (export-only). Usually one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub identity_files: Vec<IdentityKey>,
}

/// One embedded private key: where it lived and its contents. The public key is
/// intentionally not stored — it's always derivable from the private key
/// (`ssh-keygen -y -f <key>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityKey {
    /// Original path the key lived at, e.g. `~/.ssh/es-admin.pem`. Drives where
    /// import restores it (in place when safe, else the managed keys dir).
    pub location: String,
    /// The private-key file's contents (PEM / OpenSSH text).
    pub content: String,
}

impl PortableSsh {
    /// The plain [`SshConfig`] (no key material) — what reaches the main config.
    pub fn to_ssh_config(&self) -> SshConfig {
        SshConfig {
            name: self.name.clone(),
            host: self.host.clone(),
            port: self.port,
            user: self.user.clone(),
            identity_file: self.identity_file.clone(),
        }
    }

    fn from_ssh_config(c: SshConfig) -> Self {
        Self {
            name: c.name,
            host: c.host,
            port: c.port,
            user: c.user,
            identity_file: c.identity_file,
            identity_files: Vec::new(),
        }
    }
}

impl PortableConfig {
    /// Build a self-contained subset of `config`: the tunnels selected by
    /// `prefix` (see [`AppConfig::select_tunnels`]) plus the ssh configs they
    /// reference. With `strip_identity`, `identity_file` paths are dropped. No key
    /// material is embedded here — callers add it via `identity_files`.
    pub fn build(config: &AppConfig, prefix: Option<&str>, strip_identity: bool) -> Self {
        let tunnels = config.select_tunnels(prefix);
        let ssh_configs = config
            .referenced_ssh_configs(&tunnels)
            .into_iter()
            .map(|mut c| {
                if strip_identity {
                    c.identity_file = None;
                }
                PortableSsh::from_ssh_config(c)
            })
            .collect();
        Self {
            ssh_configs,
            tunnels,
        }
    }

    /// The ssh configs as plain [`SshConfig`]s, with all key material dropped.
    pub fn ssh_configs_plain(&self) -> Vec<SshConfig> {
        self.ssh_configs
            .iter()
            .map(PortableSsh::to_ssh_config)
            .collect()
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

/// Pass YAML bytes through unchanged (plaintext) or wrap them with Argon2id +
/// XChaCha20-Poly1305. The header (params + salt + nonce) is bound as the AEAD's
/// associated data, so tampering with it fails decryption.
fn seal(yaml: Vec<u8>, passphrase: Option<&str>) -> Result<Vec<u8>, ExportError> {
    let Some(pass) = passphrase else {
        return Ok(yaml);
    };

    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut salt).map_err(|e| ExportError::Encrypt(e.to_string()))?;
    getrandom::getrandom(&mut nonce).map_err(|e| ExportError::Encrypt(e.to_string()))?;

    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&M_COST.to_le_bytes());
    header.extend_from_slice(&T_COST.to_le_bytes());
    header.extend_from_slice(&P_COST.to_le_bytes());
    header.extend_from_slice(&salt);
    header.extend_from_slice(&nonce);

    let key = derive_key(pass, &salt, M_COST, T_COST, P_COST).map_err(ExportError::Encrypt)?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&key));
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &yaml,
                aad: &header,
            },
        )
        .map_err(|e| ExportError::Encrypt(e.to_string()))?;

    let mut out = header;
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Parse a blob produced by [`export_portable`] / [`export`], decrypting first if
/// it is encrypted. Returns the full [`PortableConfig`] including any embedded
/// `keys` (a whole-config export's `settings` are ignored).
pub fn import_portable(
    bytes: &[u8],
    passphrase: Option<&str>,
) -> Result<PortableConfig, ImportError> {
    let plaintext = if bytes.starts_with(MAGIC) {
        let pass = passphrase.ok_or(ImportError::PassphraseRequired)?;
        open(bytes, pass)?
    } else {
        bytes.to_vec()
    };
    let text = String::from_utf8(plaintext).map_err(|_| ImportError::Decrypt)?;
    Ok(serde_yaml::from_str(&text)?)
}

/// Like [`import_portable`] but as an [`AppConfig`] (default settings, embedded
/// key material dropped) — convenient when the caller only needs tunnels + ssh
/// configs.
pub fn import(bytes: &[u8], passphrase: Option<&str>) -> Result<AppConfig, ImportError> {
    let p = import_portable(bytes, passphrase)?;
    Ok(AppConfig {
        ssh_configs: p.ssh_configs_plain(),
        tunnels: p.tunnels,
        settings: Default::default(),
    })
}

/// Derive a 32-byte key from a passphrase with Argon2id.
fn derive_key(
    passphrase: &str,
    salt: &[u8],
    m: u32,
    t: u32,
    p: u32,
) -> Result<[u8; KEY_LEN], String> {
    let params = Params::new(m, t, p, Some(KEY_LEN)).map_err(|e| e.to_string())?;
    let kdf = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; KEY_LEN];
    kdf.hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| e.to_string())?;
    Ok(key)
}

/// Decrypt a blob written by [`seal`]: re-derive the key from the header's params
/// + salt and open the AEAD (verifying the tag over ciphertext + header).
fn open(bytes: &[u8], passphrase: &str) -> Result<Vec<u8>, ImportError> {
    if bytes.len() < HEADER_LEN {
        return Err(ImportError::Decrypt);
    }
    let (header, ciphertext) = bytes.split_at(HEADER_LEN);
    let rd = |o: usize| u32::from_le_bytes(header[o..o + 4].try_into().unwrap());
    let (m, t, p) = (rd(8), rd(12), rd(16));
    let salt = &header[20..20 + SALT_LEN];
    let nonce = &header[20 + SALT_LEN..HEADER_LEN];

    // These params come from an untrusted file and drive memory allocation in
    // the KDF, so bound them before deriving — a hostile blob must not OOM us.
    if !(1..=MAX_M_COST).contains(&m)
        || !(1..=MAX_T_COST).contains(&t)
        || !(1..=MAX_P_COST).contains(&p)
    {
        return Err(ImportError::Decrypt);
    }
    let key = derive_key(passphrase, salt, m, t, p).map_err(|_| ImportError::Decrypt)?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&key));
    cipher
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad: header,
            },
        )
        .map_err(|_| ImportError::Decrypt)
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
    fn embedded_keys_roundtrip_through_import_portable() {
        let mut portable = PortableConfig::build(&full(), Some("gc/dev"), false);
        // Attach a private key to the "dev" config.
        portable
            .ssh_configs
            .iter_mut()
            .find(|c| c.name == "dev")
            .unwrap()
            .identity_files = vec![IdentityKey {
            location: "~/.ssh/dev.pem".into(),
            content:
                "-----BEGIN OPENSSH PRIVATE KEY-----\nMOCK\n-----END OPENSSH PRIVATE KEY-----\n"
                    .into(),
        }];
        let blob = export_portable(&portable, Some("a good passphrase")).unwrap();
        let back = import_portable(&blob, Some("a good passphrase")).unwrap();
        assert_eq!(back, portable); // everything, including embedded keys, round-trips
        // The plain `import` view is key-free (SshConfig has no key field at all).
        let app = import(&blob, Some("a good passphrase")).unwrap();
        assert_eq!(app.ssh_configs, portable.ssh_configs_plain());
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
        assert!(!blob.starts_with(MAGIC));
        let back = import(&blob, None).expect("import");
        assert_eq!(cfg, back);
    }

    #[test]
    fn encrypted_roundtrips() {
        let cfg = sample();
        let blob = export(&cfg, Some("correct horse battery")).expect("export");
        assert!(blob.starts_with(MAGIC), "blob should be encrypted");
        let back = import(&blob, Some("correct horse battery")).expect("import");
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
        let blob = export(&sample(), Some("a good passphrase")).expect("export");
        let err = import(&blob, None).unwrap_err();
        assert!(matches!(err, ImportError::PassphraseRequired));
    }

    #[test]
    fn absurd_argon2_params_are_rejected_before_allocating() {
        let mut blob = export(&sample(), Some("a good passphrase")).expect("export");
        // Forge m_cost (header offset 8..12) to ~1 TB of KiB — must be rejected by
        // the clamp, not handed to the KDF to allocate.
        blob[8..12].copy_from_slice(&1_000_000_000u32.to_le_bytes());
        assert!(matches!(
            import(&blob, Some("a good passphrase")).unwrap_err(),
            ImportError::Decrypt
        ));
    }

    #[test]
    fn tampering_is_detected() {
        let mut blob = export(&sample(), Some("a good passphrase")).expect("export");
        // Flip a byte in the ciphertext (after the header) — AEAD must reject it.
        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        assert!(matches!(
            import(&blob, Some("a good passphrase")).unwrap_err(),
            ImportError::Decrypt
        ));

        // Flip a byte in the header (the salt) — bound as AAD, so also rejected.
        let mut blob = export(&sample(), Some("a good passphrase")).expect("export");
        blob[24] ^= 0x01; // inside the salt region
        assert!(matches!(
            import(&blob, Some("a good passphrase")).unwrap_err(),
            ImportError::Decrypt
        ));
    }
}
