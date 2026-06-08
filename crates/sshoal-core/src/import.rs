//! Import existing tunnels into sshoal.
//!
//! Many people already keep their tunnels in the line-based format used by the
//! popular `opentunnels.sh` script:
//!
//! ```text
//! 54321:db.internal:5432 gemx-dev   # All DB
//! ```
//!
//! i.e. `localport:remotehost:remoteport  <ssh-alias>  # label`. We turn each
//! line into a [`Tunnel`] whose `ssh` is the alias (kept as-is so it still works
//! in a plain terminal) and whose tree `path` is derived from the file name and
//! the label: `devredis` + `App-api` → `<prefix>/dev/redis/app-api`.

use std::collections::HashMap;

use crate::config::{SshConfig, Tunnel};

/// The connection details resolved for an `~/.ssh/config` Host alias. Kept for
/// callers that want to display/verify alias targets; import stores the alias
/// itself rather than resolving it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHost {
    pub host: String,
    pub user: Option<String>,
    pub port: u16,
    pub identity_file: Option<String>,
}

/// Parse `~/.ssh/config` into a map of concrete alias -> connection details.
/// Wildcard `Host` patterns (`*`, `?`, `!`) are skipped.
pub fn parse_ssh_config(text: &str) -> HashMap<String, SshHost> {
    let mut map = HashMap::new();
    let mut patterns: Vec<String> = Vec::new();
    let mut host: Option<String> = None;
    let mut user: Option<String> = None;
    let mut port: u16 = 22;
    let mut identity: Option<String> = None;

    let mut flush = |patterns: &mut Vec<String>,
                     host: &mut Option<String>,
                     user: &mut Option<String>,
                     port: &mut u16,
                     identity: &mut Option<String>| {
        for pattern in patterns.drain(..) {
            map.insert(
                pattern.clone(),
                SshHost {
                    host: host.clone().unwrap_or(pattern),
                    user: user.clone(),
                    port: *port,
                    identity_file: identity.clone(),
                },
            );
        }
        *host = None;
        *user = None;
        *port = 22;
        *identity = None;
    };

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = split_kv(line);
        match key.to_ascii_lowercase().as_str() {
            "host" => {
                flush(
                    &mut patterns,
                    &mut host,
                    &mut user,
                    &mut port,
                    &mut identity,
                );
                patterns = value
                    .split_whitespace()
                    .filter(|p| !is_glob(p))
                    .map(str::to_string)
                    .collect();
            }
            "hostname" => host = Some(value.to_string()),
            "user" => user = Some(value.to_string()),
            "identityfile" => identity = Some(value.to_string()),
            "port" => {
                if let Ok(p) = value.trim().parse() {
                    port = p;
                }
            }
            _ => {}
        }
    }
    flush(
        &mut patterns,
        &mut host,
        &mut user,
        &mut port,
        &mut identity,
    );
    map
}

/// Build [`SshConfig`]s for the ssh aliases referenced by `tunnels`, resolving
/// each against `hosts` (from `~/.ssh/config`) when possible.
pub fn ssh_configs_for(tunnels: &[Tunnel], hosts: &HashMap<String, SshHost>) -> Vec<SshConfig> {
    let mut seen = std::collections::BTreeSet::new();
    let mut configs = Vec::new();
    for tunnel in tunnels {
        if !seen.insert(tunnel.ssh.clone()) {
            continue;
        }
        let config = match hosts.get(&tunnel.ssh) {
            Some(h) => SshConfig {
                name: tunnel.ssh.clone(),
                host: h.host.clone(),
                port: h.port,
                user: h.user.clone(),
                identity_file: h.identity_file.clone(),
            },
            None => SshConfig::alias(&tunnel.ssh),
        };
        configs.push(config);
    }
    configs
}

/// Parse one `opentunnels.sh`-style file into tunnels. `file_stem` (e.g.
/// `devredis`) seeds the path's env/type segments; `prefix` (e.g. `gc`) is an
/// optional tree root.
pub fn parse_tunnel_file(file_stem: &str, text: &str, prefix: Option<&str>) -> Vec<Tunnel> {
    let (env, kind) = split_stem(file_stem);
    let mut tunnels = Vec::new();

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (addr, label) = match line.split_once('#') {
            Some((a, c)) => (a.trim(), Some(c.trim())),
            None => (line, None),
        };
        let mut tokens = addr.split_whitespace();
        let Some(spec) = tokens.next() else { continue };
        let Some(alias) = tokens.next() else { continue };
        let Some((local_port, remote_host, remote_port)) = parse_forward_spec(spec) else {
            continue;
        };

        let leaf = label
            .map(slugify)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| local_port.to_string());
        let path = join_path(&[prefix.unwrap_or(""), &env, &kind, &leaf]);

        tunnels.push(Tunnel {
            path,
            ssh: alias.to_string(),
            local_port,
            remote_host,
            remote_port,
        });
    }
    tunnels
}

/// `devredis` -> (`dev`, `redis`), `proddb` -> (`prod`, `db`), `gemx` -> (`gemx`, "").
fn split_stem(file_stem: &str) -> (String, String) {
    for suffix in ["servers", "server", "db", "redis", "cache"] {
        if let Some(prefix) = file_stem.strip_suffix(suffix)
            && !prefix.is_empty()
        {
            return (prefix.to_string(), suffix.to_string());
        }
    }
    (file_stem.to_string(), String::new())
}

/// `54321:host:5432` or `bind:54321:host:5432` -> (local, host, remote).
fn parse_forward_spec(spec: &str) -> Option<(u16, String, u16)> {
    let parts: Vec<&str> = spec.split(':').collect();
    let (local, host, remote) = match parts.as_slice() {
        [local, host, remote] => (*local, *host, *remote),
        [_bind, local, host, remote] => (*local, *host, *remote),
        _ => return None,
    };
    Some((local.parse().ok()?, host.to_string(), remote.parse().ok()?))
}

/// Lower-case, replace runs of non-alphanumeric chars with a single `-`.
fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !out.is_empty() && !dash {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

fn join_path(segments: &[&str]) -> String {
    segments
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("/")
}

fn split_kv(line: &str) -> (&str, &str) {
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let key = &line[..end];
    let rest = line[end..]
        .trim_start_matches(|c: char| c.is_whitespace() || c == '=')
        .trim();
    (key, rest)
}

fn is_glob(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?') || pattern.starts_with('!')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_config_resolves_concrete_hosts_and_skips_wildcards() {
        let cfg = "\
Host gemx-dev
  HostName 18.141.105.247
  User tunneluser
Host gemx-pro
  HostName 3.219.76.151
  Port 2222
Host *
  ServerAliveInterval 30
";
        let hosts = parse_ssh_config(cfg);
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts["gemx-dev"].host, "18.141.105.247");
        assert_eq!(hosts["gemx-dev"].user.as_deref(), Some("tunneluser"));
        assert_eq!(hosts["gemx-pro"].port, 2222);
        assert!(!hosts.contains_key("*"));
    }

    #[test]
    fn tunnel_file_builds_tree_paths_keeping_alias() {
        let file = "\
63799:redis.internal:6379 gemx-dev # App-api
6378:redis.internal:6379 gemx-dev # Auth Service
";
        let tunnels = parse_tunnel_file("devredis", file, Some("gc"));
        assert_eq!(tunnels.len(), 2);
        assert_eq!(tunnels[0].path, "gc/dev/redis/app-api");
        assert_eq!(tunnels[0].ssh, "gemx-dev");
        assert_eq!(tunnels[0].local_port, 63799);
        assert_eq!(tunnels[0].remote_host, "redis.internal");
        assert_eq!(tunnels[1].path, "gc/dev/redis/auth-service");
    }

    #[test]
    fn path_without_prefix_or_label_falls_back() {
        let tunnels = parse_tunnel_file("proddb", "54321:db:5432 gemx-pro\n", None);
        assert_eq!(tunnels[0].path, "prod/db/54321");
    }

    #[test]
    fn slugify_handles_spaces_and_punctuation() {
        assert_eq!(slugify("All DB v7"), "all-db-v7");
        assert_eq!(slugify("App-api"), "app-api");
        assert_eq!(slugify("  weird/name! "), "weird-name");
    }
}
