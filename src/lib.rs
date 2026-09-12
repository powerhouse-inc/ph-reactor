//! `ph-reactor` — run a local Powerhouse switchboard in the background.
//!
//! Modules:
//! - [`config`] — `~/.ph/reactor/config.json` load/save/defaults
//! - [`paths`] — state directory layout
//! - [`cli`] — argument parsing
//! - [`daemon`] — run/daemonize lifecycle, locking, shutdown
//! - [`bootstrap`] — Node runtime + switchboard package installation
//! - [`supervisor`] — switchboard process lifecycle (spawn, health, backoff)
//! - [`mcp`] — Streamable-HTTP MCP client (the switchboard's `/mcp` endpoint)
//! - [`drives`] — remote drive management (add/remove/pause/resume, status)
//! - [`registry`] — Powerhouse package registry checks
//! - [`tray`] — StatusNotifierItem + DBusMenu (session bus, no GTK)
//! - [`settings`] — loopback settings page + JSON API
//! - [`logrotate`] — size-based log rotation

pub mod bootstrap;
pub mod cli;
pub mod doc;
pub mod commands;
pub mod config;
pub mod daemon;
pub mod drives;
pub mod logrotate;
pub mod mcp;
pub mod paths;
pub mod registry;
pub mod settings;
pub mod status;
pub mod store;
pub mod supervisor;
pub mod tray;

pub const APP_NAME: &str = "Powerhouse Reactor";

/// The running binary's version (shared by the binary and the lib so the
/// MCP `clientInfo` and the settings page agree).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shared error type for library-level failures. App-level (`anyhow`)
/// errors are built on top of this at the daemon/CLI boundary.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("http {status} from {url}")]
    Http { status: u16, url: String },

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("config: {0}")]
    Config(#[from] config::ConfigError),

    #[error("network: {0}")]
    Network(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
