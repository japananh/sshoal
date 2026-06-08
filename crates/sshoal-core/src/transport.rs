//! The SSH transport abstraction.
//!
//! Everything above this layer (the supervisor, the UI) only ever talks to a
//! [`Transport`]: "open this tunnel, tell me when it drops, tear it down". The
//! v1 implementation, [`OpenSshTransport`], shells out to the system `ssh` so we
//! inherit `~/.ssh/config`, ProxyJump, agent and known_hosts for free. A future
//! pure-Rust (`russh`) implementation — needed on mobile, where spawning `ssh`
//! is impossible — can slot in behind the same trait without the supervisor
//! noticing.

use std::time::Duration;

use async_trait::async_trait;
use tokio::net::TcpStream;
use tokio::process::Child;
use tokio::time::Instant;

use crate::config::Tunnel;

/// How long to wait for the local forward to start accepting connections before
/// giving up on a connect attempt.
const FORWARD_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// Opens tunnels. One `Transport` can open many tunnels.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Establish a single tunnel. Resolves once the tunnel is up, yielding a
    /// handle that reports when it later drops.
    async fn connect(&self, tunnel: &Tunnel) -> anyhow::Result<Box<dyn TunnelHandle>>;
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
pub fn build_ssh_args(tunnel: &Tunnel) -> Vec<String> {
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
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
        "-L".to_string(),
        format!(
            "{}:{}:{}",
            tunnel.local_port, tunnel.remote_host, tunnel.remote_port
        ),
    ];

    if let Some(port) = tunnel.ssh_port {
        args.push("-p".to_string());
        args.push(port.to_string());
    }

    // The ssh target (alias or user@host). ssh resolves the rest from config.
    args.push(tunnel.ssh.clone());
    args
}

/// v1 transport: supervises a child `ssh` process per tunnel.
pub struct OpenSshTransport;

#[async_trait]
impl Transport for OpenSshTransport {
    async fn connect(&self, tunnel: &Tunnel) -> anyhow::Result<Box<dyn TunnelHandle>> {
        let args = build_ssh_args(tunnel);
        let mut child = tokio::process::Command::new("ssh")
            .args(&args)
            .kill_on_drop(true)
            .spawn()?;

        // Don't report success until the local forward actually accepts
        // connections — otherwise the UI would show "Up" the instant ssh is
        // spawned, before the tunnel is really usable.
        if let Err(err) = wait_forward_ready(&mut child, tunnel.local_port).await {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(err);
        }

        Ok(Box::new(OpenSshHandle { child: Some(child) }))
    }
}

/// Poll `127.0.0.1:<local_port>` until the forward accepts a connection, the
/// `ssh` process exits, or we time out.
async fn wait_forward_ready(child: &mut Child, local_port: u16) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("ssh exited before the forward was ready ({status})");
        }
        if TcpStream::connect(("127.0.0.1", local_port)).await.is_ok() {
            return Ok(());
        }
        if start.elapsed() >= FORWARD_READY_TIMEOUT {
            anyhow::bail!("forward on 127.0.0.1:{local_port} not ready in time");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
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

    fn tunnel() -> Tunnel {
        Tunnel {
            path: "gc/dev/db/app-api".into(),
            ssh: "gemx-dev".into(),
            ssh_port: None,
            local_port: 54321,
            remote_host: "db.internal".into(),
            remote_port: 5432,
        }
    }

    #[test]
    fn args_carry_forward_and_ssh_target() {
        let args = build_ssh_args(&tunnel());
        assert!(args.contains(&"-N".to_string()));
        assert!(args.contains(&"54321:db.internal:5432".to_string()));
        // No explicit port -> rely on ssh config; target is last and is the alias.
        assert!(!args.contains(&"-p".to_string()));
        assert_eq!(args.last().unwrap(), "gemx-dev");
    }

    #[test]
    fn args_include_explicit_ssh_port() {
        let mut t = tunnel();
        t.ssh_port = Some(2222);
        t.ssh = "deploy@example.com".into();
        let args = build_ssh_args(&t);
        let p = args.iter().position(|a| a == "-p").expect("-p present");
        assert_eq!(args[p + 1], "2222");
        assert_eq!(args.last().unwrap(), "deploy@example.com");
    }

    /// Grab a port that nothing is listening on (bind then release).
    async fn a_closed_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    #[tokio::test]
    async fn forward_ready_succeeds_once_the_port_listens() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();

        let result = wait_forward_ready(&mut child, port).await;

        let _ = child.start_kill();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn forward_ready_fails_when_ssh_exits_early() {
        let port = a_closed_port().await;
        let mut child = tokio::process::Command::new("true").spawn().unwrap();

        let result = wait_forward_ready(&mut child, port).await;

        assert!(result.is_err());
    }
}
