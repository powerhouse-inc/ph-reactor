//! Commands flowing from the tray menu, the settings page, and the CLI
//! to the daemon's command loop (single writer, so page actions cannot
//! race the poller).

use serde_json::Value;

/// One user-initiated mutation. The daemon executes commands
/// sequentially; each one ends with a persisted config update and a
/// status refresh.
#[derive(Debug, Clone)]
pub enum Command {
    /// Add a remote drive (name for the local config entry; url is the
    /// drive URL; token_env names the env var with a bearer token;
    /// `offline` marks the local mirror available-offline).
    AddDrive {
        name: String,
        url: String,
        token_env: Option<String>,
        offline: bool,
    },
    /// Remove the drive (local delete + config removal).
    RemoveDrive { name: String },
    /// Delete the local drive (stops syncing; config keeps it, paused).
    PauseDrive { name: String },
    /// Re-add the drive (re-syncs from the remote).
    ResumeDrive { name: String },
    /// Delete + re-add the drive (forced re-sync).
    ResyncDrive { name: String },
    /// Update a dotted config key (validated by `config::set`).
    SetConfig { key: String, value: Value },
    /// Stop the daemon.
    Quit,
}

/// A drive mutation, in the vocabulary shared by the CLI and the daemon
/// command loop (the settings page sends the equivalent JSON).
#[derive(Debug, Clone)]
pub enum DriveOp {
    Add {
        name: String,
        url: String,
        token_env: Option<String>,
        offline: bool,
    },
    Remove {
        name: String,
    },
    Pause {
        name: String,
    },
    Resume {
        name: String,
    },
    Resync {
        name: String,
    },
}
