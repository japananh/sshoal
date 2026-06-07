//! The SSH transport abstraction.
//!
//! Everything above this layer (the supervisor, the UI) only ever talks to a
//! [`Transport`]: "open this tunnel, tell me when it drops, tear it down". The
//! v1 implementation, [`OpenSshTransport`], shells out to the system `ssh` so we
//! inherit `~/.ssh/config`, ProxyJump, agent and known_hosts for free. A future
//! pure-Rust (`russh`) implementation — needed on mobile, where spawning `ssh`
//! is impossible — can slot in behind the same trait without the supervisor
//! noticing.

use async_trait::async_trait;

use crate::config::{ServerConfig, TunnelSpec};

/// Opens tunnels. One `Transport` can open many tunnels.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Establish a single tunnel. Resolves once the tunnel is up, yielding a
    /// handle that reports when it later drops.
    async fn connect(
        &self,
        server: &ServerConfig,
        tunnel: &TunnelSpec,
    ) -> anyhow::Result<Box<dyn TunnelHandle>>;
}

/// A live tunnel.
#[async_trait]
pub trait TunnelHandle: Send {
    /// Resolves when the tunnel drops on its own (process exit, network loss).
    async fn closed(&mut self);
    /// Proactively tear the tunnel down (used when the user toggles it off or
    /// the app shuts down).
    async fn shutdown(&mut self);
}

/// Builds the `ssh` argument vector for one tunnel. Pure and side-effect-free
/// so it can be unit-tested without spawning anything.
pub fn build_ssh_args(server: &ServerConfig, tunnel: &TunnelSpec) -> Vec<String> {
    let mut args = vec![
        // -N: no remote command; -T: no pty — we only want the forward.
        "-N".to_string(),
        "-T".to_string(),
        // Detect a dead peer reasonably fast and fail loudly if the forward
        // can't be set up, so the supervisor sees a clean drop and reconnects.
        "-o".to_string(),
        "ServerAliveInterval=15".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=3".to_string(),
        "-o".to_string(),
        "ExitOnForwardFailure=yes".to_string(),
        "-L".to_string(),
        format!(
            "{}:{}:{}",
            tunnel.local_port, tunnel.remote_host, tunnel.remote_port
        ),
        "-p".to_string(),
        server.port.to_string(),
    ];

    let target = match &server.user {
        Some(user) => format!("{user}@{}", server.host),
        None => server.host.clone(),
    };
    args.push(target);
    args
}

/// v1 transport: supervises a child `ssh` process per tunnel.
pub struct OpenSshTransport;

#[async_trait]
impl Transport for OpenSshTransport {
    async fn connect(
        &self,
        server: &ServerConfig,
        tunnel: &TunnelSpec,
    ) -> anyhow::Result<Box<dyn TunnelHandle>> {
        let args = build_ssh_args(server, tunnel);
        let child = tokio::process::Command::new("ssh")
            .args(&args)
            .kill_on_drop(true)
            .spawn()?;
        Ok(Box::new(OpenSshHandle { child: Some(child) }))
    }
}

struct OpenSshHandle {
    child: Option<tokio::process::Child>,
}

#[async_trait]
impl TunnelHandle for OpenSshHandle {
    async fn closed(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.wait().await;
        }
    }

    async fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_include_user_port_and_forward() {
        let server = ServerConfig {
            name: "db".into(),
            host: "example.com".into(),
            port: 2222,
            user: Some("deploy".into()),
            group: None,
            tunnels: vec![],
        };
        let tunnel = TunnelSpec {
            local_port: 5432,
            remote_host: "127.0.0.1".into(),
            remote_port: 5432,
        };

        let args = build_ssh_args(&server, &tunnel);

        assert!(args.contains(&"-N".to_string()));
        assert!(args.contains(&"5432:127.0.0.1:5432".to_string()));
        // -p and its value are adjacent.
        let p = args.iter().position(|a| a == "-p").expect("-p present");
        assert_eq!(args[p + 1], "2222");
        // Target is the last arg and carries the user.
        assert_eq!(args.last().unwrap(), "deploy@example.com");
    }

    #[test]
    fn args_omit_user_when_absent() {
        let server = ServerConfig {
            name: "web".into(),
            host: "web.example.com".into(),
            port: 22,
            user: None,
            group: None,
            tunnels: vec![],
        };
        let tunnel = TunnelSpec {
            local_port: 8080,
            remote_host: "localhost".into(),
            remote_port: 80,
        };

        let args = build_ssh_args(&server, &tunnel);

        assert_eq!(args.last().unwrap(), "web.example.com");
    }
}
