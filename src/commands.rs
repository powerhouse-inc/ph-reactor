//! Commands flowing from the tray menu, the settings page, and the CLI
//! to the daemon's command loop (single writer, so page actions cannot
//! race the poller).

use std::collections::BTreeMap;

use serde_json::Value;
/// One user-initiated mutation. The daemon executes commands
/// sequentially; each one ends with a persisted config update and a
/// status refresh.
#[derive(Debug)]
pub enum Command {
    /// Add a remote drive. `addr` is a multiaddr of the remote peer
    /// (optionally with a `/p2p/` component); `token_env` names the env
    /// var holding the shared token; `offline` marks the local mirror
    /// available-offline.
    AddDrive {
        name: String,
        addr: String,
        token_env: Option<String>,
        offline: bool,
    },
    /// Remove the drive (config removal; live connections drop on the
    /// idle timeout).
    RemoveDrive { name: String },
    /// Pause the drive (stops syncing; docs stay local).
    PauseDrive { name: String },
    /// Unpause the drive (re-dials and re-syncs).
    ResumeDrive { name: String },
    /// Force a fresh catch-up on the next reconcile.
    ResyncDrive { name: String },
    /// Create a local doc with initial fields. Synchronous: the sender
    /// (the CLI via the settings API) awaits the outcome on `reply`, so
    /// the command is not fire-and-forget like the drive mutations.
    CreateDoc {
        name: String,
        fields: BTreeMap<String, Value>,
        reply: tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
    },
    /// Build, sign (the daemon's own key), and apply a model action to an
    /// existing local doc, publishing it to the mesh. Synchronous, like
    /// `CreateDoc`: the sender awaits the applied action on `reply`.
    CreateAction {
        name: String,
        /// The governing model, as a `name@version` ref.
        model: String,
        kind: String,
        payload: Value,
        reply: tokio::sync::oneshot::Sender<std::result::Result<crate::action::Action, String>>,
    },
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
        addr: String,
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
