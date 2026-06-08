//! The SSH transport abstraction.
//!
//! Everything above this layer (the supervisor, the UI) only ever talks to a
//! [`Transport`]: "open this tunnel through this ssh config, tell me when it
//! drops, tear it down". The v1 implementation, [`OpenSshTransport`], shells out
//! to the system `ssh`. A future pure-Rust (`russh`) implementation — needed on
//! mobile, where spawning `ssh` is impossible — can slot in behind the same
//! trait without the supervisor noticing.

use std::time::Duration;

use async_trait::async_trait;
use tokio::net::TcpStream;
use tokio::process::Child;
use tokio::time::Instant;

use crate::config::{SshConfig, Tunnel};

/// How long to wait for the local forward to start accepting connections before
/// giving up on a connect attempt.
const FORWARD_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// Opens tunnels. One `Transport` can open many tunnels.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Establish a single tunnel through `ssh`. Resolves once the tunnel is up,
    /// yielding a handle that reports when it later drops.
    async fn connect(
        &self,
        tunnel: &Tunnel,
        ssh: &SshConfig,
    ) -> anyhow::Result<Box<dyn TunnelHandle>>;
}

/// A live tunnel.
#[async_trait]
pub trait TunnelHandle: Send {
    /// Resolves when the tunnel drops on its own (process exit, network loss).
    async fn closed(&mut self);
    /// Proactively tear the tunnel down.
    async fn shutdown(&mut self);
}

/// Builds the `ssh` argument vector for one tunnel. Pure and side-effect-free
/// so it can be unit-tested without spawning anything.
pub fn build_ssh_args(tunnel: &Tunnel, ssh: &SshConfig) -> Vec<String> {
    let mut args = vec![
        // -N: no remote command; -T: no pty — we only want the forward.
        "-N".to_string(),
        "-T".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=15".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=3".to_string(),
        "-o".to_string(),
        "ExitOnForwardFailure=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
    ];

    if let Some(identity) = &ssh.identity_file {
        args.push("-i".to_string());
        args.push(expand_tilde(identity));
        args.push("-o".to_string());
        args.push("IdentitiesOnly=yes".to_string());
    }

    args.push("-L".to_string());
    args.push(format!(
        "{}:{}:{}",
        tunnel.local_port, tunnel.remote_host, tunnel.remote_port
    ));

    if ssh.port != 22 {
        args.push("-p".to_string());
        args.push(ssh.port.to_string());
    }

    let target = match &ssh.user {
        Some(user) => format!("{user}@{}", ssh.host),
        None => ssh.host.clone(),
    };
    args.push(target);
    args
}

/// Expand a leading `~/` to the home directory (ssh receives an absolute path
/// since we spawn it without a shell).
fn expand_tilde(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) => format!("{home}/{rest}"),
            Err(_) => path.to_string(),
        },
        None => path.to_string(),
    }
}

/// v1 transport: supervises a child `ssh` process per tunnel.
pub struct OpenSshTransport;

#[async_trait]
impl Transport for OpenSshTransport {
    async fn connect(
        &self,
        tunnel: &Tunnel,
        ssh: &SshConfig,
    ) -> anyhow::Result<Box<dyn TunnelHandle>> {
        let args = build_ssh_args(tunnel, ssh);
        let mut child = tokio::process::Command::new("ssh")
            .args(&args)
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        // Don't report success until the local forward actually accepts
        // connections. On failure, surface ssh's own stderr as the error.
        if let Err(err) = wait_forward_ready(&mut child, tunnel.local_port).await {
            let detail = read_stderr_tail(&mut child).await;
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(detail.map_or(err, |d| anyhow::anyhow!(d)));
        }

        Ok(Box::new(OpenSshHandle { child: Some(child) }))
    }
}

/// Read the child's stderr and return its last non-empty line (ssh's error
/// message), trimmed of the leading `ssh: ` prefix.
async fn read_stderr_tail(child: &mut Child) -> Option<String> {
    use tokio::io::AsyncReadExt;
    let mut stderr = child.stderr.take()?;
    let mut buf = String::new();
    let _ = stderr.read_to_string(&mut buf).await;
    let line = buf.lines().rev().find(|l| !l.trim().is_empty())?;
    Some(line.trim().trim_start_matches("ssh: ").to_string())
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
            local_port: 54321,
            remote_host: "db.internal".into(),
            remote_port: 5432,
        }
    }

    #[test]
    fn args_carry_forward_identity_and_target() {
        let ssh = SshConfig {
            name: "gemx-dev".into(),
            host: "example.com".into(),
            port: 2222,
            user: Some("deploy".into()),
            identity_file: Some("/keys/dev.pem".into()),
        };
        let args = build_ssh_args(&tunnel(), &ssh);

        assert!(args.contains(&"54321:db.internal:5432".to_string()));
        let i = args.iter().position(|a| a == "-i").expect("-i present");
        assert_eq!(args[i + 1], "/keys/dev.pem");
        let p = args.iter().position(|a| a == "-p").expect("-p present");
        assert_eq!(args[p + 1], "2222");
        assert_eq!(args.last().unwrap(), "deploy@example.com");
    }

    #[test]
    fn args_minimal_for_alias_without_user_port_key() {
        let ssh = SshConfig::alias("gemx-dev");
        let args = build_ssh_args(&tunnel(), &ssh);
        assert!(!args.contains(&"-i".to_string()));
        assert!(!args.contains(&"-p".to_string())); // port 22 omitted
        assert_eq!(args.last().unwrap(), "gemx-dev");
    }

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
        assert!(wait_forward_ready(&mut child, port).await.is_err());
    }
}
