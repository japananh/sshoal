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

use crate::config::AppConfig;

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

/// Serialize `config` to a portable blob, encrypting with `passphrase` if given.
pub fn export(config: &AppConfig, passphrase: Option<&str>) -> Result<Vec<u8>, ExportError> {
    let yaml = serde_yaml::to_string(config)?;
    let Some(pass) = passphrase else {
        return Ok(yaml.into_bytes());
    };

    let encryptor = age::Encryptor::with_user_passphrase(SecretString::from(pass.to_owned()));
    let mut out = Vec::new();
    let mut writer = encryptor
        .wrap_output(&mut out)
        .map_err(|e| ExportError::Encrypt(e.to_string()))?;
    writer.write_all(yaml.as_bytes())?;
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
    use crate::config::Tunnel;

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
