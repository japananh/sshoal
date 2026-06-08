//! Per-tunnel supervisor: the piece that makes a tunnel feel "always on".
//!
//! It drives one tunnel through a small state machine — connect, stay up, and
//! on a drop reconnect — until told to stop. State is published over a [`watch`]
//! channel so the UI can render a live status dot without polling.
//!
//! A subtlety with the shell-out transport: `connect` resolves as soon as the
//! `ssh` process is spawned, before the forward is truly established. So a tunnel
//! that "drops" almost immediately never really came up — we treat that as a
//! failed attempt and apply exponential backoff, which is what stops a
//! refused/dead host from hot-looping. A tunnel that stayed up past
//! [`Backoff::stable_after`] reconnects promptly with the backoff reset.
//!
//! Because it is generic over [`Transport`], all of this is tested against an
//! in-memory fake on Tokio's paused clock — no network, microsecond-fast.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::config::{ServerConfig, TunnelSpec};
use crate::transport::Transport;

/// What a single tunnel is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelState {
    /// Not running.
    Idle,
    /// First connection attempt in progress.
    Connecting,
    /// Established and forwarding.
    Up,
    /// Dropped or failed; waiting/backoff before trying again.
    Reconnecting,
    /// The most recent attempt errored or dropped instantly (a retry follows).
    Failed,
}

/// Reconnect policy: exponential backoff plus how long a connection must stay up
/// before we consider it healthy (and reset the backoff).
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub base: Duration,
    pub max: Duration,
    /// A connection that drops before being up this long is treated as a failed
    /// attempt (so dead hosts back off instead of hot-looping).
    pub stable_after: Duration,
}

impl Backoff {
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            stable_after: Duration::from_secs(3),
        }
    }

    pub fn with_stable_after(mut self, stable_after: Duration) -> Self {
        self.stable_after = stable_after;
        self
    }

    /// Delay before retry number `attempt` (1-based): `base * 2^(attempt-1)`,
    /// capped at `max`.
    pub fn delay(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return Duration::ZERO;
        }
        let shift = attempt.saturating_sub(1).min(31);
        let mult = 1u64 << shift;
        let ms = (self.base.as_millis() as u64).saturating_mul(mult);
        Duration::from_millis(ms).min(self.max)
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            base: Duration::from_millis(500),
            max: Duration::from_secs(30),
            stable_after: Duration::from_secs(5),
        }
    }
}

/// Owns the background task that keeps one tunnel alive.
pub struct TunnelSupervisor {
    state_rx: watch::Receiver<TunnelState>,
    cancel_tx: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl TunnelSupervisor {
    /// Start supervising `tunnel` on `server`. Must be called from within a
    /// Tokio runtime.
    pub fn spawn(
        transport: Arc<dyn Transport>,
        server: ServerConfig,
        tunnel: TunnelSpec,
        backoff: Backoff,
    ) -> Self {
        let (state_tx, state_rx) = watch::channel(TunnelState::Idle);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let task = tokio::spawn(run(transport, server, tunnel, backoff, state_tx, cancel_rx));
        Self {
            state_rx,
            cancel_tx,
            task,
        }
    }

    /// Current state (cheap, non-blocking).
    pub fn state(&self) -> TunnelState {
        *self.state_rx.borrow()
    }

    /// A receiver to await state transitions on (e.g. to drive the UI).
    pub fn subscribe(&self) -> watch::Receiver<TunnelState> {
        self.state_rx.clone()
    }

    /// Signal the tunnel to tear down without waiting. The task observes the
    /// cancel, shuts the tunnel down, and exits on its own — handy from
    /// synchronous UI code where we can't `await`.
    pub fn cancel(&self) {
        let _ = self.cancel_tx.send(true);
    }

    /// Signal the tunnel to tear down and wait for the task to finish.
    pub async fn stop(self) {
        let _ = self.cancel_tx.send(true);
        let _ = self.task.await;
    }
}

async fn run(
    transport: Arc<dyn Transport>,
    server: ServerConfig,
    tunnel: TunnelSpec,
    backoff: Backoff,
    state_tx: watch::Sender<TunnelState>,
    mut cancel_rx: watch::Receiver<bool>,
) {
    let mut attempt: u32 = 0;
    let mut ever_up = false;

    loop {
        if *cancel_rx.borrow() {
            break;
        }

        let _ = state_tx.send(if ever_up {
            TunnelState::Reconnecting
        } else {
            TunnelState::Connecting
        });

        // Race the connect against cancellation so a stop during a slow connect
        // is honored promptly.
        let connected = tokio::select! {
            biased;
            _ = cancel_rx.changed() => {
                if *cancel_rx.borrow() { break; } else { continue; }
            }
            result = transport.connect(&server, &tunnel) => result,
        };

        match connected {
            Ok(mut handle) => {
                ever_up = true;
                let _ = state_tx.send(TunnelState::Up);
                let started = Instant::now();

                let cancelled = tokio::select! {
                    biased;
                    _ = cancel_rx.changed() => *cancel_rx.borrow(),
                    _ = handle.closed() => false,
                };

                if cancelled {
                    handle.shutdown().await;
                    break;
                }

                let uptime = started.elapsed();
                if uptime < backoff.stable_after {
                    // Never really came up — back off before retrying.
                    attempt += 1;
                    let delay = backoff.delay(attempt);
                    let _ = state_tx.send(TunnelState::Failed);
                    tracing::warn!(
                        server = %server.name,
                        attempt,
                        uptime_ms = uptime.as_millis() as u64,
                        delay_ms = delay.as_millis() as u64,
                        "tunnel dropped immediately; backing off",
                    );
                    tokio::select! {
                        biased;
                        _ = cancel_rx.changed() => { if *cancel_rx.borrow() { break; } }
                        _ = tokio::time::sleep(delay) => {}
                    }
                } else {
                    // Was healthy; reconnect promptly with backoff reset.
                    attempt = 0;
                    tracing::info!(server = %server.name, "tunnel dropped; reconnecting");
                }
            }
            Err(err) => {
                attempt += 1;
                let delay = backoff.delay(attempt);
                let _ = state_tx.send(TunnelState::Failed);
                tracing::warn!(
                    server = %server.name,
                    attempt,
                    error = %err,
                    delay_ms = delay.as_millis() as u64,
                    "tunnel connect failed; backing off",
                );
                tokio::select! {
                    biased;
                    _ = cancel_rx.changed() => { if *cancel_rx.borrow() { break; } }
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }

    let _ = state_tx.send(TunnelState::Idle);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use crate::transport::TunnelHandle;

    /// In-memory transport. `fail_first` initial connects error. If
    /// `drop_immediately`, every successful connection drops at once (simulating
    /// a refused/dead host); otherwise it stays up until [`trigger_drop`].
    struct FakeTransport {
        connect_calls: AtomicUsize,
        fail_first: usize,
        drop_immediately: bool,
        drop_tx: watch::Sender<u64>,
    }

    impl FakeTransport {
        fn new(fail_first: usize) -> Arc<Self> {
            Self::build(fail_first, false)
        }

        fn always_dropping() -> Arc<Self> {
            Self::build(0, true)
        }

        fn build(fail_first: usize, drop_immediately: bool) -> Arc<Self> {
            let (drop_tx, _) = watch::channel(0u64);
            Arc::new(Self {
                connect_calls: AtomicUsize::new(0),
                fail_first,
                drop_immediately,
                drop_tx,
            })
        }

        fn connect_count(&self) -> usize {
            self.connect_calls.load(Ordering::SeqCst)
        }

        fn trigger_drop(&self) {
            self.drop_tx.send_modify(|v| *v += 1);
        }
    }

    struct FakeHandle {
        drop_immediately: bool,
        drop_rx: watch::Receiver<u64>,
    }

    #[async_trait]
    impl TunnelHandle for FakeHandle {
        async fn closed(&mut self) {
            if self.drop_immediately {
                return;
            }
            // `subscribe()` marks the current value seen, so this resolves on the
            // next `trigger_drop()` after the handle was created.
            let _ = self.drop_rx.changed().await;
        }
        async fn shutdown(&mut self) {}
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn connect(
            &self,
            _server: &ServerConfig,
            _tunnel: &TunnelSpec,
        ) -> anyhow::Result<Box<dyn TunnelHandle>> {
            let n = self.connect_calls.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_first {
                anyhow::bail!("simulated connect failure #{n}");
            }
            Ok(Box::new(FakeHandle {
                drop_immediately: self.drop_immediately,
                drop_rx: self.drop_tx.subscribe(),
            }))
        }
    }

    fn server() -> ServerConfig {
        ServerConfig {
            name: "test".into(),
            host: "h".into(),
            port: 22,
            user: None,
            group: None,
            tunnels: vec![],
        }
    }

    fn tunnel() -> TunnelSpec {
        TunnelSpec {
            local_port: 1,
            remote_host: "127.0.0.1".into(),
            remote_port: 2,
        }
    }

    async fn wait_state(rx: &mut watch::Receiver<TunnelState>, want: TunnelState) {
        loop {
            if *rx.borrow() == want {
                return;
            }
            rx.changed().await.expect("supervisor still running");
        }
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        let b = Backoff::new(Duration::from_millis(100), Duration::from_millis(500));
        assert_eq!(b.delay(0), Duration::ZERO);
        assert_eq!(b.delay(1), Duration::from_millis(100));
        assert_eq!(b.delay(2), Duration::from_millis(200));
        assert_eq!(b.delay(3), Duration::from_millis(400));
        assert_eq!(b.delay(4), Duration::from_millis(500)); // capped
        assert_eq!(b.delay(50), Duration::from_millis(500)); // no overflow
    }

    #[tokio::test(start_paused = true)]
    async fn stable_connection_reconnects_after_a_drop() {
        let transport = FakeTransport::new(0);
        let sup = TunnelSupervisor::spawn(
            transport.clone(),
            server(),
            tunnel(),
            Backoff::new(Duration::from_millis(10), Duration::from_millis(100))
                .with_stable_after(Duration::from_millis(20)),
        );
        let mut states = sup.subscribe();

        wait_state(&mut states, TunnelState::Up).await;
        assert_eq!(transport.connect_count(), 1);

        // Stay up beyond `stable_after`, then drop: should reconnect promptly.
        tokio::time::sleep(Duration::from_millis(50)).await;
        transport.trigger_drop();

        while transport.connect_count() < 2 {
            states.changed().await.unwrap();
        }
        sup.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn immediate_drops_back_off_instead_of_hot_looping() {
        let transport = FakeTransport::always_dropping();
        let sup = TunnelSupervisor::spawn(
            transport.clone(),
            server(),
            tunnel(),
            Backoff::new(Duration::from_millis(10), Duration::from_millis(1000))
                .with_stable_after(Duration::from_secs(1)),
        );
        let mut states = sup.subscribe();

        let start = Instant::now();
        while transport.connect_count() < 3 {
            states.changed().await.unwrap();
        }
        // Backoff between attempts (10ms + 20ms) must have elapsed — proof we are
        // pacing retries, not spinning.
        assert!(
            start.elapsed() >= Duration::from_millis(25),
            "expected backoff between immediate drops, elapsed = {:?}",
            start.elapsed()
        );

        sup.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn retries_with_backoff_until_connect_succeeds() {
        // First two connects fail; the third succeeds.
        let transport = FakeTransport::new(2);
        let sup = TunnelSupervisor::spawn(
            transport.clone(),
            server(),
            tunnel(),
            Backoff::new(Duration::from_millis(10), Duration::from_millis(100)),
        );
        let mut states = sup.subscribe();

        wait_state(&mut states, TunnelState::Up).await;
        assert_eq!(transport.connect_count(), 3);

        sup.stop().await;
    }
}
