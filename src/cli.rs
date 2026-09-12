//! CLI argument shapes (clap), shared by `main` and the daemon.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate as ph;

#[derive(Parser)]
#[command(
    name = "ph-reactor",
    version = ph::VERSION,
    about = "Run the native Powerhouse reactor (p2p doc sync) in the background with a tray icon and remote-drive sync"
)]
pub struct CliArgs {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Override the state directory (default: $PH_REACTOR_STATE_DIR or
    /// ~/.ph/reactor).
    #[arg(long, global = true)]
    pub state_dir: Option<PathBuf>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Run the daemon (the default when no subcommand is given).
    Run {
        /// Fork into the background (pidfile in the state dir).
        #[arg(long)]
        daemonize: bool,
    },
    /// Stop the running daemon (SIGTERM, waits for a clean exit).
    Stop,
    /// Show reactor and drive status.
    Status {
        /// Machine-readable output: the same `StatusSnapshot` JSON the
        /// daemon serves at `/api/status` (degraded shape from config when
        /// the daemon is down). Stable contract for shell plugins.
        #[arg(long)]
        json: bool,
    },
    /// Manage synced remote drives.
    #[command(subcommand)]
    Drive(DriveCommand),
    /// Manage the local documents of this vault.
    #[command(subcommand)]
    Doc(DocCommand),
    /// Diagnose the local setup (state dir, identity, store, listener,
    /// settings server).
    Doctor,
    /// Inspect or update the daemon configuration.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Tail the daemon log.
    Logs {
        /// Follow the log file.
        #[arg(long)]
        follow: bool,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum DriveCommand {
    /// Add a remote drive by the peer's multiaddr (e.g.
    /// /ip4/10.0.0.2/tcp/4201/p2p/12D3Koo…).
    Add {
        /// The peer's multiaddr (the `/p2p/` component is optional).
        addr: String,
        /// Friendly name for the drive entry.
        #[arg(long)]
        name: Option<String>,
        /// Name of the env var holding the shared sync token (the token
        /// itself is never stored).
        #[arg(long = "token-env")]
        token_env: Option<String>,
        /// Mark the local mirror as available-offline.
        #[arg(long)]
        offline: bool,
    },
    /// Remove a drive (by name or 1-based index from `drive list`).
    Remove { target: String },
    /// List drives and their sync status.
    List,
    /// Pause a drive (by name or index; docs stay local).
    Pause { target: String },
    /// Resume a paused drive.
    Resume { target: String },
    /// Force a fresh catch-up for a drive.
    Resync { target: String },
}

#[derive(Subcommand, Debug, Clone)]
pub enum DocCommand {
    /// List the local documents.
    List,
    /// Print one document's fields as JSON.
    Get { name: String },
    /// Create a local document. The daemon must be running: the doc is
    /// published to the sync mesh as it is created.
    Add {
        name: String,
        /// Initial fields as KEY=VALUE (values parse as JSON when
        /// possible, e.g. 42, true, {"a":1}; otherwise plain strings).
        #[arg(long = "field", value_name = "KEY=VALUE")]
        fields: Vec<String>,
    },
}
#[derive(Subcommand, Debug, Clone)]
pub enum ConfigCommand {
    /// Print the effective configuration (pretty JSON).
    Show,
    /// Set a dotted key (`instance.listen`, `p2p.mdns`, `logLevel`, …).
    Set {
        /// Dotted config key.
        key: String,
        /// JSON value (quoted string, number, boolean, or array/object).
        value: String,
    },
}
