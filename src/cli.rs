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
    /// Query the maintained read models: every live doc of a model, with an
    /// optional field-equality filter. Reads the store directly (works with
    /// or without a running daemon).
    Query {
        /// The model to query (omit to query every model).
        #[arg(default_value = "")]
        model: String,
        /// A field-equality filter (K=V); V is JSON when it parses.
        #[arg(long)]
        filter: Option<String>,
    },
    /// Generate a one-shot invite string for a peer to join this vault
    /// (`join` it on their machine). Requires a running daemon.
    Invite {
        /// Groups the joiner is granted (repeatable; default: the local group).
        #[arg(long = "group")]
        groups: Vec<String>,
    },
    /// Propose a quorum-gated group action (e.g. promoting a manager).
    ///
    /// Prints a proposal token. Other members co-sign it with `cosign`; once
    /// enough have, `submit` applies it. The proposal is bound to the
    /// document's current state, so submit it before the group changes.
    Propose {
        /// The group the action applies to.
        group: String,
        /// The reducer to run, e.g. `add-manager`.
        kind: String,
        /// The reducer payload as JSON, e.g. '{"member":"12D3Koo..."}'.
        #[arg(long, default_value = "{}")]
        payload: String,
    },
    /// Add this node's co-signature to a proposal token.
    Cosign {
        /// The token from `ph-reactor propose` (or a previous `cosign`).
        token: String,
    },
    /// Apply a proposal that has gathered enough co-signatures.
    Submit {
        /// The fully co-signed token.
        token: String,
    },
    /// Consume an invite string: pin the inviter (TOFU) and add a drive that
    /// syncs with it. Requires a running daemon.
    Join {
        /// The invite string (from the inviter's `ph-reactor invite`).
        invite: String,
    },
    /// Ban a peer: its future handshakes are refused (it cannot sync with
    /// this vault). Requires a running daemon.
    Ban {
        /// The peer to ban (base58 peer id, as shown by `ph-reactor status`).
        peer: String,
    },
    /// Unban a previously banned peer.
    Unban {
        /// The peer to unban (base58 peer id).
        peer: String,
    },
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
    /// Verify a document's action log: replay and re-check every signature,
    /// co-signature, the hash chain, and the reduced field map. Read-only.
    Verify { name: String },
    /// Build, sign, and apply a model action to a local document. The daemon
    /// must be running: the action is published to the sync mesh.
    Action {
        name: String,
        /// The reducer kind to invoke (e.g. `add-manager`, `add-member`).
        kind: String,
        /// The action payload as a JSON object.
        #[arg(long)]
        payload: String,
        /// The governing model (name@version). Defaults to `open@1`.
        #[arg(long, default_value = "open@1")]
        model: String,
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
