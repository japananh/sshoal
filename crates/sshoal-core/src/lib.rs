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
pub mod updater;

pub use config::{AppConfig, Settings, SshConfig, Tunnel};
pub use import::{SshHost, parse_ssh_config, parse_tunnel_file, ssh_configs_for};
pub use supervisor::{Backoff, TunnelState, TunnelSupervisor};
pub use transfer::{
    EmbeddedKey, ExportError, ImportError, PortableConfig, export, export_portable, import,
    import_portable,
};
pub use transport::{
    OpenSshTransport, Transport, TunnelHandle, build_ssh_args, build_test_ssh_args,
};
pub use updater::{UpdateError, UpdateInfo, check_latest, install_latest};
