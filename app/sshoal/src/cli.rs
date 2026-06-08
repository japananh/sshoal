//! `sshoal export` / `sshoal import` — the portable-config commands.
//!
//! Run before the GUI starts. If the first argument is a known subcommand we do
//! the work and exit; otherwise we return and the app launches normally.

use std::io::Write;
use std::path::Path;

use sshoal_core::{AppConfig, ImportError, export, import, parse_ssh_config, parse_tunnel_file};

use crate::config_path;

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
         sshoal export [FILE] [--encrypt]   write config to FILE (or stdout)\n  \
         sshoal import FILE [--no-overwrite]  merge config from FILE\n  \
         sshoal import-ssh TUNNELFILE...    import opentunnels.sh-style files\n  \
         \\                                  ([--dry-run] [--no-overwrite])\n\n\
         Passphrase is read from $SSHOAL_PASSPHRASE, else prompted.\n\
         import-ssh resolves ssh aliases via ~/.ssh/config ($SSHOAL_SSH_CONFIG)."
    );
}

fn run_import_ssh(args: &[String]) -> i32 {
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let overwrite = !args.iter().any(|a| a == "--no-overwrite");
    let files: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    if files.is_empty() {
        return fail("import-ssh needs at least one tunnel file".to_string());
    }

    let ssh_config_path =
        std::env::var("SSHOAL_SSH_CONFIG").unwrap_or_else(|_| format!("{}/.ssh/config", home()));
    let hosts = match std::fs::read_to_string(&ssh_config_path) {
        Ok(text) => parse_ssh_config(&text),
        Err(_) => {
            eprintln!("note: could not read {ssh_config_path}; aliases used as hostnames");
            Default::default()
        }
    };

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
            .servers
            .extend(parse_tunnel_file(&stem, &text, &hosts));
    }

    for s in &imported.servers {
        eprintln!(
            "  {} [{}]  {}  — {} tunnel(s)",
            s.name,
            s.group.as_deref().unwrap_or("-"),
            s.host,
            s.tunnels.len()
        );
    }
    let count = imported.servers.len();
    if dry_run {
        eprintln!("(dry-run: {count} server(s) parsed, nothing saved)");
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
        "imported {count} server(s) into {} (now {} total)",
        config_path().display(),
        current.servers.len()
    );
    0
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| ".".to_string())
}

fn run_export(args: &[String]) -> i32 {
    let encrypt = args.iter().any(|a| a == "--encrypt");
    let file = args.iter().find(|a| !a.starts_with("--"));

    let config = match AppConfig::load(config_path()) {
        Ok(c) => c,
        Err(e) => return fail(format!("loading config: {e}")),
    };

    let passphrase = if encrypt {
        Some(passphrase(true))
    } else {
        None
    };
    let blob = match export(&config, passphrase.as_deref()) {
        Ok(b) => b,
        Err(e) => return fail(format!("export: {e}")),
    };

    match file {
        Some(path) => {
            if let Err(e) = std::fs::write(path, &blob) {
                return fail(format!("writing {path}: {e}"));
            }
            eprintln!(
                "exported {} server(s) to {path}{}",
                config.servers.len(),
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
    let overwrite = !args.iter().any(|a| a == "--no-overwrite");
    let Some(path) = args.iter().find(|a| !a.starts_with("--")) else {
        return fail("import needs a FILE argument".to_string());
    };

    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return fail(format!("reading {path}: {e}")),
    };

    // Try plaintext first; if the blob is encrypted, ask for a passphrase.
    let incoming = match import(&bytes, None) {
        Ok(c) => c,
        Err(ImportError::PassphraseRequired) => match import(&bytes, Some(&passphrase(false))) {
            Ok(c) => c,
            Err(e) => return fail(format!("import: {e}")),
        },
        Err(e) => return fail(format!("import: {e}")),
    };

    let mut current = match AppConfig::load(config_path()) {
        Ok(c) => c,
        Err(e) => return fail(format!("loading config: {e}")),
    };
    let added = incoming.servers.len();
    current.merge(incoming, overwrite);
    if let Err(e) = current.save(config_path()) {
        return fail(format!("saving config: {e}"));
    }
    eprintln!(
        "imported {added} server(s) into {} (now {} total)",
        config_path().display(),
        current.servers.len()
    );
    0
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
