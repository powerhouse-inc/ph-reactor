//! CLI argument shapes (clap), shared by `main` and the daemon.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate as ph;

#[derive(Parser)]
#[command(
    name = "ph-reactor",
    version = ph::VERSION,
    about = "Run a local Powerhouse switchboard in the background with a tray icon and remote-drive sync"
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
    /// Show daemon, switchboard, and drive status.
    Status {
        /// Machine-readable output: the same `StatusSnapshot` JSON the
        /// daemon serves at `/api/status` (degraded shape from config when
        /// the daemon is down). Stable contract for shell plugins since
        /// 0.2.0.
        #[arg(long)]
        json: bool,
    },
    /// Manage synced remote drives.
    #[command(subcommand)]
    Drive(DriveCommand),
    /// Diagnose the local setup (node, npm, registry, switchboard, MCP).
    Doctor,
    /// Inspect or update the daemon configuration.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Tail daemon or switchboard logs.
    Logs {
        /// Follow the log file.
        #[arg(long)]
        follow: bool,
        /// Show the switchboard's log instead of the daemon's.
        #[arg(long)]
        switchboard: bool,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum DriveCommand {
    /// Add a remote drive by its REST URL (e.g.
    /// https://<switchboard>/d/<slug>).
    Add {
        /// Drive REST URL.
        url: String,
        /// Friendly name (defaults to the drive's own name).
        #[arg(long)]
        name: Option<String>,
        /// Name of the env var holding a bearer token for the drive's
        /// switchboard (the token itself is never stored).
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
    /// Pause a drive (by name or index).
    Pause { target: String },
    /// Resume a paused drive.
    Resume { target: String },
    /// Force a re-sync of a drive (idempotent re-registration + pull).
    Resync { target: String },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ConfigCommand {
    /// Print the effective configuration (pretty JSON).
    Show,
    /// Set a dotted key (`switchboard.port`, `registry`, `logLevel`, …).
    Set {
        /// Dotted config key.
        key: String,
        /// JSON value (quoted string, number, boolean, or array/object).
        value: String,
    },
}
