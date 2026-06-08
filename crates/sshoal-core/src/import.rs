//! Import existing tunnels into sshoal.
//!
//! Many people already keep their tunnels in the line-based format used by the
//! popular `opentunnels.sh` script:
//!
//! ```text
//! 54321:db.internal:5432 gemx-dev   # All DB
//! ```
//!
//! i.e. `localport:remotehost:remoteport  <ssh-alias>  # label`, where the alias
//! is an `~/.ssh/config` Host that supplies the real hostname/user/port. We parse
//! those files plus `~/.ssh/config`, resolve the alias, and turn each file into
//! one sshoal server (a toggleable bundle of tunnels).

use std::collections::HashMap;

use crate::config::{ServerConfig, TunnelSpec};

/// The connection details resolved for an `~/.ssh/config` Host alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHost {
    pub host: String,
    pub user: Option<String>,
    pub port: u16,
}

/// Parse `~/.ssh/config` into a map of concrete alias -> connection details.
/// Wildcard `Host` patterns (`*`, `?`, `!`) are skipped.
pub fn parse_ssh_config(text: &str) -> HashMap<String, SshHost> {
    let mut map = HashMap::new();
    let mut patterns: Vec<String> = Vec::new();
    let mut host: Option<String> = None;
    let mut user: Option<String> = None;
    let mut port: u16 = 22;

    let mut flush = |patterns: &mut Vec<String>,
                     host: &mut Option<String>,
                     user: &mut Option<String>,
                     port: &mut u16| {
        for pattern in patterns.drain(..) {
            map.insert(
                pattern.clone(),
                SshHost {
                    host: host.clone().unwrap_or(pattern),
                    user: user.clone(),
                    port: *port,
                },
            );
        }
        *host = None;
        *user = None;
        *port = 22;
    };

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = split_kv(line);
        match key.to_ascii_lowercase().as_str() {
            "host" => {
                flush(&mut patterns, &mut host, &mut user, &mut port);
                patterns = value
                    .split_whitespace()
                    .filter(|p| !is_glob(p))
                    .map(str::to_string)
                    .collect();
            }
            "hostname" => host = Some(value.to_string()),
            "user" => user = Some(value.to_string()),
            "port" => {
                if let Ok(p) = value.trim().parse() {
                    port = p;
                }
            }
            _ => {}
        }
    }
    flush(&mut patterns, &mut host, &mut user, &mut port);
    map
}

/// Parse one `opentunnels.sh`-style file into servers, resolving ssh aliases
/// against `hosts`. `file_stem` (e.g. `devdb`) names the resulting server(s).
pub fn parse_tunnel_file(
    file_stem: &str,
    text: &str,
    hosts: &HashMap<String, SshHost>,
) -> Vec<ServerConfig> {
    // Preserve first-seen alias order.
    let mut order: Vec<String> = Vec::new();
    let mut by_alias: HashMap<String, Vec<TunnelSpec>> = HashMap::new();

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (addr, label) = match line.split_once('#') {
            Some((a, c)) => (a.trim(), Some(c.trim().to_string())),
            None => (line, None),
        };
        let mut tokens = addr.split_whitespace();
        let Some(spec) = tokens.next() else { continue };
        let Some(alias) = tokens.next() else { continue };
        let Some(tunnel) = parse_forward_spec(spec, label) else {
            continue;
        };
        by_alias
            .entry(alias.to_string())
            .or_insert_with(|| {
                order.push(alias.to_string());
                Vec::new()
            })
            .push(tunnel);
    }

    let multiple = order.len() > 1;
    let group = derive_group(file_stem);

    order
        .into_iter()
        .map(|alias| {
            let tunnels = by_alias.remove(&alias).unwrap_or_default();
            let resolved = hosts.get(&alias).cloned().unwrap_or(SshHost {
                host: alias.clone(),
                user: None,
                port: 22,
            });
            let name = if multiple {
                format!("{file_stem} ({alias})")
            } else {
                file_stem.to_string()
            };
            ServerConfig {
                name,
                host: resolved.host,
                port: resolved.port,
                user: resolved.user,
                group: group.clone(),
                tunnels,
            }
        })
        .collect()
}

/// `54321:host:5432` or `bind:54321:host:5432` -> a [`TunnelSpec`].
fn parse_forward_spec(spec: &str, label: Option<String>) -> Option<TunnelSpec> {
    let parts: Vec<&str> = spec.split(':').collect();
    let (local, remote_host, remote) = match parts.as_slice() {
        [local, host, remote] => (*local, *host, *remote),
        [_bind, local, host, remote] => (*local, *host, *remote),
        _ => return None,
    };
    Some(TunnelSpec {
        local_port: local.parse().ok()?,
        remote_host: remote_host.to_string(),
        remote_port: remote.parse().ok()?,
        label,
    })
}

/// `devdb` -> `dev`, `stgservers` -> `stg`, `proddb` -> `prod`, for grouping.
fn derive_group(file_stem: &str) -> Option<String> {
    for suffix in ["servers", "server", "db", "redis", "cache"] {
        if let Some(prefix) = file_stem.strip_suffix(suffix)
            && !prefix.is_empty()
        {
            return Some(prefix.to_string());
        }
    }
    None
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
  User violettran
  Port 2222
Host *
  ServerAliveInterval 30
";
        let hosts = parse_ssh_config(cfg);
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts["gemx-dev"].host, "18.141.105.247");
        assert_eq!(hosts["gemx-dev"].user.as_deref(), Some("tunneluser"));
        assert_eq!(hosts["gemx-dev"].port, 22);
        assert_eq!(hosts["gemx-pro"].port, 2222);
        assert!(!hosts.contains_key("*"));
    }

    #[test]
    fn tunnel_file_becomes_one_server_with_labelled_tunnels() {
        let hosts = parse_ssh_config("Host gemx-dev\n  HostName 1.2.3.4\n  User deploy\n");
        let file = "\
54321:gem-dev.rds.amazonaws.com:5432 gemx-dev # All DB
54399:gpv6-dev.rds.amazonaws.com:5432 gemx-dev # DB v6
";
        let servers = parse_tunnel_file("devdb", file, &hosts);
        assert_eq!(servers.len(), 1);
        let s = &servers[0];
        assert_eq!(s.name, "devdb");
        assert_eq!(s.host, "1.2.3.4");
        assert_eq!(s.user.as_deref(), Some("deploy"));
        assert_eq!(s.group.as_deref(), Some("dev"));
        assert_eq!(s.tunnels.len(), 2);
        assert_eq!(s.tunnels[0].local_port, 54321);
        assert_eq!(s.tunnels[0].remote_host, "gem-dev.rds.amazonaws.com");
        assert_eq!(s.tunnels[0].remote_port, 5432);
        assert_eq!(s.tunnels[0].label.as_deref(), Some("All DB"));
    }

    #[test]
    fn unknown_alias_falls_back_to_using_it_as_host() {
        let servers = parse_tunnel_file("x", "9000:svc:9000 myhost\n", &HashMap::new());
        assert_eq!(servers[0].host, "myhost");
        assert_eq!(servers[0].user, None);
    }

    #[test]
    fn multiple_aliases_in_one_file_split_into_named_servers() {
        let servers = parse_tunnel_file("mixed", "1:a:1 alpha\n2:b:2 beta\n", &HashMap::new());
        assert_eq!(servers.len(), 2);
        assert!(servers.iter().any(|s| s.name == "mixed (alpha)"));
        assert!(servers.iter().any(|s| s.name == "mixed (beta)"));
    }
}
