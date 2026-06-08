//! Core logic for **sshoal** — the configuration model, the SSH transport
//! abstraction, and the per-tunnel supervisor.
//!
//! This crate is deliberately free of any UI or tray code so that the parts a
//! user actually depends on for reliability — reconnect/backoff, state
//! transitions, config parsing — can be unit-tested headlessly. The supervisor
//! tests use an in-memory [`Transport`] fake and Tokio's paused clock, so a full
//! "drop → reconnect" or "retry with backoff" scenario verifies in microseconds
//! without touching the network.

pub mod config;
pub mod import;
pub mod supervisor;
pub mod transfer;
pub mod transport;

pub use config::{AppConfig, Tunnel};
pub use import::{SshHost, parse_ssh_config, parse_tunnel_file};
pub use supervisor::{Backoff, TunnelState, TunnelSupervisor};
pub use transfer::{ExportError, ImportError, export, import};
pub use transport::{OpenSshTransport, Transport, TunnelHandle, build_ssh_args};
