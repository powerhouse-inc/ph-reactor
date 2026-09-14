//! `ph-reactor` — the native Powerhouse reactor: an event-sourced
//! document store with libp2p drive sync, running as a status-bar
//! background daemon.
//!
//! Modules:
//! - [`config`] — `<state>/config.json` load/save/defaults
//! - [`paths`] — state directory layout
//! - [`cli`] — argument parsing
//! - [`daemon`] — run/daemonize lifecycle, locking, shutdown, CLI ops
//! - [`store`] — the native doc store (snapshots + live logs, signed
//!   ops, vector clocks)
//! - [`doc`] — the doc/op/clock model (shared by the store and p2p)
//! - [`p2p`] — the libp2p sync engine (gossipsub + hello/catch-up +
//!   mDNS) and the daemon identity
//! - [`processor`] — user-configurable subscriptions on doc changes (the
//!   invoice → payment engine)
//! - [`drives`] — drive config + status vocabulary
//! - [`status`] — the shared status snapshot (tray, settings, CLI)
//! - [`commands`] — the daemon's command channel vocabulary
//! - [`tray`] — StatusNotifierItem + DBusMenu (session bus, no GTK)
//! - [`settings`] — loopback settings page + JSON API
//! - [`logrotate`] — size-based log rotation

pub mod action;
pub mod blob;
pub mod cli;
pub mod commands;
pub mod config;
pub mod daemon;
pub mod doc;
pub mod drives;
pub mod logrotate;
pub mod model;
pub mod p2p;
pub mod package;
pub mod paths;
pub mod processor;
pub mod projection;
pub mod query;
pub mod settings;
pub mod status;
pub mod store;
pub mod update;
pub mod tray;

pub const APP_NAME: &str = "Powerhouse Reactor";

/// The running binary's version (shared by the binary and the lib so
/// the settings page and `ph-reactor status` agree).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shared error type for library-level failures. App-level (`anyhow`)
/// errors are built on top of this at the daemon/CLI boundary.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("config: {0}")]
    Config(#[from] config::ConfigError),

    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
