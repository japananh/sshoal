//! `sshoal export` / `sshoal import` — the portable-config commands.
//!
//! Run before the GUI starts. If the first argument is a known subcommand we do
//! the work and exit; otherwise we return and the app launches normally.

use std::io::Write;
use std::path::Path;

use sshoal_core::{
    AppConfig, IdentityKey, ImportError, PortableConfig, export_portable, import_portable,
    parse_ssh_config, parse_tunnel_file, ssh_configs_for,
};

use crate::config_path;

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| ".".to_string())
}

/// Minimum passphrase length for `--encrypt`. The KDF (Argon2id) only buys time
/// against an offline brute-force of the passphrase — a short one defeats it —
/// so we refuse obviously weak ones. A few random words easily clears this.
pub(crate) const MIN_PASSPHRASE_LEN: usize = 12;

/// If invoked as a CLI subcommand, run it and exit the process. Otherwise return.
pub fn maybe_run() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("export") => std::process::exit(run_export(&args[2..])),
        Some("import") => std::process::exit(run_import(&args[2..])),
        Some("import-ssh") => std::process::exit(run_import_ssh(&args[2..])),
        Some("help" | "-h" | "--help") => {
            print_usage();
            std::process::exit(0);
        }
        _ => {}
    }
}

fn print_usage() {
    eprintln!(
        "sshoal — SSH tunnel manager\n\n\
         Usage:\n  \
         sshoal                      launch the tray app\n  \
         sshoal export [--out FILE] [--all | --path PREFIX]\n  \
         \\              [--plaintext] [--strip-identity | --include-keys]\n  \
         \\                                  export tunnels + the ssh configs they use to FILE\n  \
         \\                                  or stdout; self-contained, no settings. ENCRYPTED\n  \
         \\                                  by default (Argon2id); --plaintext to opt out.\n  \
         \\                                  --include-keys embeds private keys in the file\n  \
         sshoal import FILE [--overwrite | --skip]   merge tunnels from FILE\n  \
         \\                                  (default --skip: keep current on conflict)\n  \
         sshoal import-ssh TUNNELFILE...    import opentunnels.sh tunnel files\n  \
         \\                                  ([--prefix gc] [--dry-run] [--no-overwrite])\n\n\
         Passphrase is read from $SSHOAL_PASSPHRASE, else prompted."
    );
}

fn run_import_ssh(args: &[String]) -> i32 {
    let mut dry_run = false;
    let mut overwrite = true;
    let mut prefix: Option<String> = None;
    let mut files: Vec<String> = Vec::new();

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--no-overwrite" => overwrite = false,
            "--prefix" => prefix = it.next().cloned(),
            other if other.starts_with("--") => eprintln!("note: ignoring unknown flag {other}"),
            other => files.push(other.to_string()),
        }
    }
    if files.is_empty() {
        return fail("import-ssh needs at least one tunnel file".to_string());
    }

    let mut imported = AppConfig::default();
    for file in &files {
        let text = match std::fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("skip {file}: {e}");
                continue;
            }
        };
        let stem = Path::new(file)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("imported")
            .to_string();
        imported
            .tunnels
            .extend(parse_tunnel_file(&stem, &text, prefix.as_deref()));
    }

    // Build ssh configs for the referenced aliases, resolving host/user/port/key
    // from ~/.ssh/config so the imported config is self-contained.
    let ssh_config_path =
        std::env::var("SSHOAL_SSH_CONFIG").unwrap_or_else(|_| format!("{}/.ssh/config", home()));
    let hosts = std::fs::read_to_string(&ssh_config_path)
        .map(|t| parse_ssh_config(&t))
        .unwrap_or_default();
    imported.ssh_configs = ssh_configs_for(&imported.tunnels, &hosts);

    for c in &imported.ssh_configs {
        let user = c.user.as_deref().unwrap_or("-");
        let key = c.identity_file.as_deref().unwrap_or("(default key)");
        eprintln!("  [ssh] {} → {user}@{}:{}  {key}", c.name, c.host, c.port);
    }
    for t in &imported.tunnels {
        eprintln!(
            "  {}  (via {} → {}:{})",
            t.path, t.ssh, t.remote_host, t.remote_port
        );
    }
    let count = imported.tunnels.len();
    if dry_run {
        eprintln!("(dry-run: {count} tunnel(s) parsed, nothing saved)");
        return 0;
    }

    let mut current = match AppConfig::load(config_path()) {
        Ok(c) => c,
        Err(e) => return fail(format!("loading config: {e}")),
    };
    current.merge(imported, overwrite);
    if let Err(e) = current.save(config_path()) {
        return fail(format!("saving config: {e}"));
    }
    eprintln!(
        "imported {count} tunnel(s) into {} (now {} total)",
        config_path().display(),
        current.tunnels.len()
    );
    0
}

fn run_export(args: &[String]) -> i32 {
    // Secure by default: the file carries hostnames + usernames (real infra
    // recon), so it is encrypted unless the user explicitly opts out with
    // `--plaintext`. `--encrypt` is still accepted (now a no-op) for muscle memory.
    let mut encrypt = true;
    let mut strip_identity = false;
    let mut include_keys = false;
    let mut all = false;
    let mut prefix: Option<String> = None;
    let mut out: Option<String> = None;
    let mut positional: Option<String> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--plaintext" => encrypt = false,
            "--encrypt" => encrypt = true,
            "--strip-identity" => strip_identity = true,
            "--include-keys" => include_keys = true,
            "--all" => all = true,
            "--path" => prefix = it.next().cloned(),
            "--out" => out = it.next().cloned(),
            other if other.starts_with("--") => eprintln!("note: ignoring unknown flag {other}"),
            other => positional = Some(other.to_string()),
        }
    }
    // `--all` exports everything (the default when no `--path` is given); an
    // explicit `--path` selects a subtree. `--all` wins if both are passed.
    let selection = if all { None } else { prefix.as_deref() };
    // Destination: `--out FILE`, else a positional FILE, else stdout.
    let file = out.or(positional);

    // Embedding private keys (--include-keys) contradicts dropping them.
    if include_keys && strip_identity {
        return fail("--include-keys and --strip-identity are mutually exclusive".to_string());
    }

    let config = match AppConfig::load(config_path()) {
        Ok(c) => c,
        Err(e) => return fail(format!("loading config: {e}")),
    };

    let mut portable = PortableConfig::build(&config, selection, strip_identity);
    if portable.tunnels.is_empty() {
        let where_ = prefix.as_deref().unwrap_or("the config");
        eprintln!("warning: no tunnels matched {where_} — exporting an empty file");
    }
    // Embed the contents of each referenced config's identity_file, so the export
    // reconstructs a working setup on another machine. Unreadable keys are skipped
    // (the path still travels).
    if include_keys {
        embed_keys(&mut portable);
    }

    let passphrase = if encrypt {
        let pass = passphrase(true);
        // The file is only as safe as the passphrase — reject obviously weak ones.
        if pass.chars().count() < MIN_PASSPHRASE_LEN {
            return fail(format!(
                "passphrase too short (min {MIN_PASSPHRASE_LEN} chars) — use a longer one, \
                 e.g. a few random words"
            ));
        }
        Some(pass)
    } else {
        // Plaintext export. Warn loudly — and louder still if it carries keys,
        // which is a foot-gun the user explicitly opted into.
        if include_keys {
            eprintln!(
                "warning: --plaintext WITH --include-keys — writing UNENCRYPTED PRIVATE KEYS. \
                 Anyone who reads this file gets your keys; protect it accordingly."
            );
        } else {
            eprintln!(
                "warning: --plaintext — writing an UNENCRYPTED file with hostnames and usernames. \
                 Drop --plaintext to encrypt it, and don't commit the plaintext to a shared repo."
            );
        }
        None
    };
    let blob = match export_portable(&portable, passphrase.as_deref()) {
        Ok(b) => b,
        Err(e) => return fail(format!("export: {e}")),
    };

    match file {
        Some(path) => {
            if let Err(e) = std::fs::write(&path, &blob) {
                return fail(format!("writing {path}: {e}"));
            }
            let n_keys: usize = portable
                .ssh_configs
                .iter()
                .map(|c| c.identity_files.len())
                .sum();
            let keys = if n_keys == 0 {
                String::new()
            } else {
                format!(" + {n_keys} key(s)")
            };
            eprintln!(
                "exported {} tunnel(s) + {} ssh config(s){keys} to {path}{}",
                portable.tunnels.len(),
                portable.ssh_configs.len(),
                if encrypt { " (encrypted)" } else { "" }
            );
        }
        None => {
            if std::io::stdout().write_all(&blob).is_err() {
                return 1;
            }
        }
    }
    0
}

fn run_import(args: &[String]) -> i32 {
    // Default keeps the current entry on a `path`/`name` conflict (skip);
    // `--overwrite` replaces it. `--skip` and the older `--no-overwrite` are
    // explicit aliases for the default.
    let overwrite = args.iter().any(|a| a == "--overwrite");
    let Some(path) = args.iter().find(|a| !a.starts_with("--")) else {
        return fail("import needs a FILE argument".to_string());
    };

    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return fail(format!("reading {path}: {e}")),
    };

    // Try plaintext first; if the blob is encrypted, ask for a passphrase.
    let incoming = match import_portable(&bytes, None) {
        Ok(c) => c,
        Err(ImportError::PassphraseRequired) => {
            match import_portable(&bytes, Some(&passphrase(false))) {
                Ok(c) => c,
                Err(e) => return fail(format!("import: {e}")),
            }
        }
        Err(e) => return fail(format!("import: {e}")),
    };

    // Materialize any embedded private keys (to their original path when safe,
    // else the managed keys dir), repointing identity_file on fallback.
    let mut incoming = incoming;
    let written = materialize_keys(&mut incoming, overwrite);

    let mut current = match AppConfig::load(config_path()) {
        Ok(c) => c,
        Err(e) => return fail(format!("loading config: {e}")),
    };
    let added = incoming.tunnels.len();
    // Only tunnels + ssh configs (paths) reach the config — `ssh_configs_plain`
    // drops every embedded key, so contents can never land in servers.yaml.
    current.merge(
        AppConfig {
            ssh_configs: incoming.ssh_configs_plain(),
            tunnels: incoming.tunnels,
            settings: Default::default(),
        },
        overwrite,
    );
    if let Err(e) = current.save(config_path()) {
        return fail(format!("saving config: {e}"));
    }
    let keys = if written == 0 {
        String::new()
    } else {
        format!(", wrote {written} key file(s)")
    };
    eprintln!(
        "imported {added} tunnel(s) into {} (now {} total){keys}",
        config_path().display(),
        current.tunnels.len()
    );
    0
}

/// Embed each referenced config's private key (the contents of its
/// `identity_file`) into that config's `identity_files`. An unreadable / absent
/// key is skipped with a warning (the path still travels). The public key isn't
/// stored — it's always derivable from the private key.
pub(crate) fn embed_keys(portable: &mut PortableConfig) {
    for c in &mut portable.ssh_configs {
        let Some(path) = c.identity_file.clone() else {
            continue;
        };
        match std::fs::read_to_string(expand_tilde(&path)) {
            Ok(content) => c.identity_files.push(IdentityKey {
                location: path,
                content,
            }),
            Err(e) => eprintln!("warning: skipping key for {}: {path}: {e}", c.name),
        }
    }
}

/// Write each config's embedded keys back to their original `location` when that
/// path is safe (see [`safe_key_dest`]), so a self-export restores keys to
/// `~/.ssh/…` as-is. Unsafe locations fall back to sshoal's own keys dir
/// (`~/.config/sshoal/keys/<config>`) with `identity_file` repointed there.
/// Private keys get chmod 600. Skips an existing file unless `overwrite`.
/// Returns how many key files were written.
///
/// Security: we never blindly honour the location from an imported file — a
/// hostile export could otherwise target `~/.ssh/authorized_keys`, `~/.ssh/config`,
/// `~/.config/autostart/…`, a `..` traversal, or an absolute system path. Only
/// non-control files under `~/.ssh/` are restored in place.
pub(crate) fn materialize_keys(incoming: &mut PortableConfig, overwrite: bool) -> usize {
    let mut written = 0;
    for c in &mut incoming.ssh_configs {
        let mut repoint: Option<String> = None;
        for key in &c.identity_files {
            // Restore to the key's original location if safe; else our keys dir.
            let (dest, fallback) = if safe_key_dest(&expand_tilde(&key.location)) {
                (expand_tilde(&key.location), None)
            } else {
                let rel = format!("~/.config/sshoal/keys/{}", sanitize_key_name(&c.name));
                eprintln!(
                    "note: key path {} isn't safe to restore in place; writing to {rel}",
                    key.location
                );
                (expand_tilde(&rel), Some(rel))
            };

            if Path::new(&dest).exists() && !overwrite {
                eprintln!("skip key {dest} (already exists; use --overwrite to replace)");
            } else {
                if let Some(parent) = Path::new(&dest).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(e) = std::fs::write(&dest, &key.content) {
                    eprintln!("warning: writing key {dest}: {e}");
                    continue;
                }
                chmod_600(&dest);
                written += 1;
            }
            if let Some(rel) = fallback {
                repoint = Some(rel);
            }
        }
        // Repoint identity_file only when a key fell back to the managed dir.
        if let Some(rel) = repoint {
            c.identity_file = Some(rel);
        }
    }
    written
}

/// Whether `path` (already tilde-expanded to absolute) is safe to materialize a
/// key into: directly or nested under `~/.ssh/`, no `..` traversal, and not one
/// of ssh's own control files. Everything else is rejected so a hostile export
/// can't write outside `~/.ssh` or clobber `authorized_keys` / `config` / etc.
fn safe_key_dest(path: &str) -> bool {
    safe_key_dest_in(path, &home())
}

/// Pure core of [`safe_key_dest`], with `home` injected so it's unit-testable.
fn safe_key_dest_in(path: &str, home: &str) -> bool {
    let prefix = format!("{home}/.ssh/");
    // Must be a real file *under* ~/.ssh/ — reject the dir itself and any
    // directory path (trailing slash).
    let Some(rest) = path.strip_prefix(&prefix) else {
        return false;
    };
    if rest.is_empty() || rest.ends_with('/') {
        return false;
    }
    let p = Path::new(path);
    if p.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return false;
    }
    const RESERVED: [&str; 7] = [
        "authorized_keys",
        "authorized_keys2",
        "config",
        "known_hosts",
        "known_hosts2",
        "environment",
        "rc",
    ];
    // Case-insensitive: macOS/APFS is case-insensitive, so `Authorized_keys`
    // would otherwise slip through and resolve to `authorized_keys` on disk.
    let base = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
    !base.is_empty() && !RESERVED.iter().any(|r| base.eq_ignore_ascii_case(r))
}

/// A safe filename for a managed key: only `[A-Za-z0-9_-]`, so it can never be
/// `/`, `.`, `..` or otherwise escape the keys dir.
fn sanitize_key_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() { "key".to_string() } else { s }
}

/// Restrict a key file to owner read/write (ssh refuses world-readable keys).
fn chmod_600(path: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Expand a leading `~/` to the home directory.
fn expand_tilde(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", home()),
        None => path.to_string(),
    }
}

/// Passphrase from `$SSHOAL_PASSPHRASE`, else an interactive hidden prompt.
fn passphrase(confirm: bool) -> String {
    if let Ok(p) = std::env::var("SSHOAL_PASSPHRASE")
        && !p.is_empty()
    {
        return p;
    }
    let p = rpassword::prompt_password("Passphrase: ").unwrap_or_default();
    if confirm {
        let again = rpassword::prompt_password("Confirm passphrase: ").unwrap_or_default();
        if p != again {
            eprintln!("error: passphrases do not match");
            std::process::exit(1);
        }
    }
    p
}

fn fail(message: String) -> i32 {
    eprintln!("error: {message}");
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/u";

    #[test]
    fn safe_key_dest_allows_normal_keys_under_ssh() {
        assert!(safe_key_dest_in("/home/u/.ssh/es-admin.pem", HOME));
        assert!(safe_key_dest_in("/home/u/.ssh/id_ed25519", HOME));
        assert!(safe_key_dest_in("/home/u/.ssh/keys/work.pem", HOME)); // nested ok
    }

    #[test]
    fn safe_key_dest_blocks_ssh_control_files_any_case() {
        for name in [
            "authorized_keys",
            "Authorized_keys", // macOS case-insensitive bypass attempt
            "AUTHORIZED_KEYS",
            "config",
            "Config",
            "known_hosts",
            "environment",
            "rc",
        ] {
            assert!(
                !safe_key_dest_in(&format!("/home/u/.ssh/{name}"), HOME),
                "{name} must be blocked"
            );
        }
    }

    #[test]
    fn safe_key_dest_blocks_outside_ssh_and_traversal() {
        assert!(!safe_key_dest_in(
            "/home/u/.config/autostart/x.desktop",
            HOME
        )); // outside ~/.ssh
        assert!(!safe_key_dest_in("/home/u/.bashrc", HOME));
        assert!(!safe_key_dest_in("/etc/cron.d/x", HOME)); // absolute, outside home
        assert!(!safe_key_dest_in("/home/u/.ssh/../.bashrc", HOME)); // traversal
        assert!(!safe_key_dest_in("/home/u/.sshEVIL/k", HOME)); // sibling-prefix trick
        assert!(!safe_key_dest_in("/home/u/.ssh/", HOME)); // the dir itself / empty base
        // A macOS-absolute path on a Linux home falls outside → rejected (falls back).
        assert!(!safe_key_dest_in("/Users/nana/.ssh/k.pem", HOME));
    }

    #[test]
    fn sanitize_key_name_strips_unsafe_chars() {
        assert_eq!(sanitize_key_name("gemx-dev"), "gemx-dev");
        assert_eq!(sanitize_key_name("es-admin.pem"), "es-admin_pem"); // '.' -> '_'
        assert_eq!(sanitize_key_name("../../etc/passwd"), "______etc_passwd"); // no '/' or '..'
        assert_eq!(sanitize_key_name("a/b"), "a_b");
        assert_eq!(sanitize_key_name(""), "key"); // never empty
        // Result can never contain a path separator or be a traversal token.
        for n in ["..", "/", "a/../b", ""] {
            let s = sanitize_key_name(n);
            assert!(!s.contains('/') && s != ".." && s != ".");
        }
    }
}
