//! The daemon: assembles the switchboard supervisor, the settings
//! server, the tray, and the status poller; and implements the CLI
//! subcommands that talk to (or work around) a running daemon.
//!
//! Lifecycle:
//! `run` → bootstrap (node + switchboard) → supervisor task + settings
//! server + tray → event/command/poll loop → on SIGTERM/SIGINT: stop
//! the tray, signal the supervisor (graceful child stop), close the
//! settings server, release the lock, remove the pidfile.

use std::collections::HashMap;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use tokio::sync::{mpsc, watch};
use tokio::time::interval;

use crate::bootstrap::{node, switchboard};
use crate::commands::{Command, DriveOp};
use crate::config::{self, DriveConfig, ReactorConfig};
use crate::drives;
use crate::paths::StatePaths;
use crate::registry;
use crate::settings::Settings;
use crate::status::{self, DriveStatusEntry, StatusSnapshot, SwitchboardStatus};
use crate::supervisor::{ShutdownSignal, StatusEvent, Supervisor, SupervisorState};
use crate::tray;

/// How often the daemon refreshes the status snapshot.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How long `run --daemonize` waits for the child to become ready.
const DAEMONIZE_TIMEOUT: Duration = Duration::from_secs(300);
/// How long `stop` waits for a graceful exit before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

/// Runs the daemon in the foreground (used when `--daemonize` is not set).
pub async fn run(state_dir: Option<&Path>, daemonize: bool) -> Result<()> {
    assert!(
        !daemonize,
        "daemonized start is handled before the runtime begins"
    );
    run_inner(state_dir).await
}

/// The daemonized start: forks before any tokio runtime exists, so the
/// child never shares the parent's epoll fd (forking inside a live
/// runtime corrupts both sides' event loops). The parent waits
/// synchronously for the child's ready file and exits; the child builds
/// a fresh runtime and runs the daemon.
pub fn run_daemonized(state_dir: Option<&Path>) -> Result<i32> {
    let paths = StatePaths::resolve(state_dir);
    paths
        .ensure_dirs()
        .with_context(|| format!("creating {}", paths.root.display()))?;

    let pid = unsafe { libc::fork() };
    match pid {
        0 => {
            // Child: detach from the session and the controlling
            // terminal; stdio goes to /dev/null (the daemon logs to its
            // rotated file). The parent never started a runtime, so
            // nothing is inherited that could corrupt a fresh one.
            detach_stdio();
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("starting the daemon runtime")?;
            let code = rt
                .block_on(run_inner(state_dir))
                .err()
                .map(|_| 1)
                .unwrap_or(0);
            Ok(code)
        }
        child if child > 0 => {
            // Parent: wait for the ready file while the child is alive.
            let ready = paths.run_dir.join("ready");
            let _ = std::fs::remove_file(&ready); // stale
            let deadline = Instant::now() + DAEMONIZE_TIMEOUT;
            while Instant::now() < deadline {
                if let Ok(content) = std::fs::read_to_string(&ready) {
                    println!("ph-reactor started (pid {})", content.trim());
                    return Ok(0);
                }
                if !process_alive(child) {
                    eprintln!("ph-reactor exited during startup; last log lines:");
                    if let Ok(tail) = crate::logrotate::tail(&paths.reactor_log(), Some(2048)) {
                        print!("{tail}");
                    }
                    return Ok(1);
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            bail!("daemon did not become ready within {DAEMONIZE_TIMEOUT:?}");
        }
        _ => bail!("fork failed"),
    }
}

/// The daemon proper: bootstrap, supervisor, settings server, tray, and
/// the status/command loop. Runs on a fresh runtime in both modes.
async fn run_inner(state_dir: Option<&Path>) -> Result<()> {
    let paths = StatePaths::resolve(state_dir);
    paths
        .ensure_dirs()
        .with_context(|| format!("creating {}", paths.root.display()))?;
    let config = config::load(&paths)?;
    init_logging(&paths, &config, false)?;

    let lock = acquire_lock(&paths)?;
    let pid = std::process::id();
    std::fs::write(paths.daemon_pidfile(), pid.to_string())
        .with_context(|| format!("writing {}", paths.daemon_pidfile().display()))?;

    // Channels: status fan-out, the single-writer command channel,
    // supervisor events, shutdown, and the hot config.
    let (snap_tx, snap_rx) = watch::channel(StatusSnapshot::empty(
        crate::VERSION.to_string(),
        config.switchboard.port,
        format!("http://{}:{}", config.settings.host, config.settings.port),
    ));
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel::<StatusEvent>();
    let (shutdown, daemon_shutdown) = ShutdownSignal::new();
    let (cfg_tx, cfg_rx) = watch::channel(config.clone());

    // Bootstrap the node runtime and the switchboard package.
    let node = node::resolve(
        &config.switchboard.node.minimum_version,
        config.switchboard.node.prefer_system,
        &paths,
    )
    .await
    .with_context(|| "resolving the Node runtime")?;
    tracing::info!("using node {} at {}", node.version, node.path.display());
    bootstrap_switchboard(&paths, &node, &config)
        .await
        .context("bootstrapping the switchboard")?;

    // The supervision loop.
    let sup = Supervisor::new(config.clone(), node, paths.clone(), evt_tx, cfg_rx);
    let sup_status = sup.status.clone();
    let mut supervisor_task = tokio::spawn({
        let shutdown = daemon_shutdown;
        async move {
            if let Err(err) = sup.run(&shutdown).await {
                tracing::error!("supervisor stopped: {err:#}");
            }
        }
    });

    // The loopback settings server.
    let settings = Settings::new(cmd_tx.clone(), snap_rx.clone())
        .start(&config.settings.host, config.settings.port)
        .await
        .with_context(|| {
            format!(
                "starting the settings server on {}:{}",
                config.settings.host, config.settings.port
            )
        })?;

    // The status-bar tray (headless environments continue without it).
    let tray = tray::start(snap_rx, cmd_tx).await;
    match &tray {
        tray::Tray::Headless(reason) => tracing::warn!("no tray: {reason}"),
        tray::Tray::Running { .. } => tracing::info!("tray started"),
    }

    // The daemonized parent is polling for this file.
    let _ = std::fs::write(paths.run_dir.join("ready"), pid.to_string());

    let mut ctx = Ctx {
        paths: paths.clone(),
        config,
        cfg_tx,
        sup_status,
        last_save_mtime: std::fs::metadata(&paths.config_file)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH),
        drive_outcomes: HashMap::new(),
        drive_attempt_urls: HashMap::new(),
    };
    let mut tick = interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing the SIGTERM handler")?;
    tracing::info!(
        "ph-reactor {v} ready (settings http://{host}:{port})",
        v = crate::VERSION,
        host = ctx.config.settings.host,
        port = ctx.config.settings.port,
    );

    while !shutdown.is_cancelled() {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("SIGINT received; shutting down");
                shutdown.trigger();
                break;
            }
            _ = term.recv() => {
                tracing::info!("SIGTERM received; shutting down");
                shutdown.trigger();
                break;
            }
            event = evt_rx.recv() => {
                match event {
                    Some(e) => {
                        tracing::debug!("supervisor: {}", e.summary());
                        if matches!(e, StatusEvent::Healthy { .. }) {
                            readd_drives(&mut ctx).await;
                            let snap = refresh_status(&ctx).await;
                            let _ = snap_tx.send(snap);
                        }
                    }
                    None => break,
                }
            }
            command = cmd_rx.recv() => {
                match command {
                    Some(cmd) => {
                        let label = format!("{cmd:?}");
                        match execute_command(&mut ctx, cmd, &snap_tx).await {
                            Ok(quit) => {
                                if quit {
                                    shutdown.trigger();
                                    break;
                                }
                            }
                            Err(err) => tracing::warn!("command {label} failed: {err:#}"),
                        }
                    }
                    None => break,
                }
            }
            _ = tick.tick() => {
                adopt_external(&mut ctx, false);
            }
        }
        let snap = refresh_status(&ctx).await;
        let _ = snap_tx.send(snap);
    }

    // Graceful shutdown.
    shutdown.trigger();
    tray.stop();
    match tokio::time::timeout(Duration::from_secs(30), &mut supervisor_task).await {
        Ok(Ok(())) => tracing::info!("supervisor stopped"),
        Ok(Err(err)) => tracing::warn!("supervisor task: {err}"),
        Err(_) => tracing::warn!("supervisor did not stop in time"),
    }
    settings.stop();
    remove_ready(&paths);
    let _ = std::fs::remove_file(paths.daemon_pidfile());
    drop(lock);
    tracing::info!("ph-reactor stopped");
    Ok(())
}

/// The daemon's shared, command-mutable state.
struct Ctx {
    paths: StatePaths,
    config: ReactorConfig,
    cfg_tx: watch::Sender<ReactorConfig>,
    sup_status: Arc<tokio::sync::Mutex<SupervisorState>>,
    /// Mtime of `config.json` as of the daemon's last write (and at
    /// startup). Anything newer is an external edit.
    last_save_mtime: SystemTime,
    /// Last registration attempt per configured drive (by name); feeds
    /// the drive status views (`connecting` / `requires-auth` / `error`).
    drive_outcomes: HashMap<String, drives::AddOutcome>,
    /// The URL of the last add attempt per drive name — kept for drives
    /// whose add failed (they never enter `config.drives`) so the status
    /// view can show which remote was rejected.
    drive_attempt_urls: HashMap<String, String>,
}

/// Picks up out-of-band edits to `config.json` (e.g. a separate
/// `ph-reactor config set` from another terminal): adopts the file and,
/// when the process-relevant fields changed, tells the supervisor to
/// respawn the switchboard.
///
/// With `preserve_drives` the daemon keeps its in-memory drive list
/// (used before persisting: the running command owns the drives, and
/// only the other fields are adopted, so a concurrent edit is not
/// clobbered). Between commands the file is fully authoritative.
fn adopt_external(ctx: &mut Ctx, preserve_drives: bool) {
    let mtime = match std::fs::metadata(&ctx.paths.config_file).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return,
    };
    if mtime <= ctx.last_save_mtime {
        return;
    }
    let (mut reloaded, quarantined) = match config::load_quiet(&ctx.paths) {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!("config file changed but could not be reloaded: {err:#}");
            return;
        }
    };
    if quarantined {
        tracing::warn!("config file was replaced with defaults; keeping the in-memory config");
        ctx.last_save_mtime = mtime;
        return;
    }
    let respawn = reloaded.process_fingerprint() != ctx.config.process_fingerprint();
    if preserve_drives {
        reloaded.drives = std::mem::take(&mut ctx.config.drives);
    }
    ctx.config = reloaded;
    ctx.last_save_mtime = mtime;
    if respawn {
        tracing::info!("config changed; restarting the switchboard");
        let _ = ctx.cfg_tx.send(ctx.config.clone());
    } else {
        tracing::debug!("config reloaded");
    }
}

/// How long a drive command may wait for the switchboard to be ready
/// (covers daemon startup and config-triggered respawns).
const WAIT_READY_TIMEOUT: Duration = Duration::from_secs(180);

/// Drive commands need the local switchboard's MCP endpoint. `/health`
/// can pass before the MCP server is mounted (it registers after the
/// document model packages load), so the gate verifies the actual MCP
/// handshake. Actions that arrive while the child is starting up or
/// being respawned wait for it instead of failing.
async fn wait_switchboard_ready(ctx: &Ctx) -> Result<()> {
    let deadline = Instant::now() + WAIT_READY_TIMEOUT;
    loop {
        let healthy = ctx.sup_status.lock().await.healthy;
        // Short-circuits: the MCP probe only runs when the switchboard
        // is up (its /health answered).
        let mcp_ok = healthy
            && crate::mcp::Mcp::new(ctx.config.switchboard.port, None)
                .session()
                .await
                .is_ok();
        if mcp_ok {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("switchboard is not ready (yet); try again in a moment");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Every (re)start of the child rebuilds the in-memory sync channels;
/// the registered drives themselves live in the reactor's persisted
/// store and are restored at boot. This hook re-runs the (idempotent)
/// registration for every configured, non-paused drive once the child
/// reports healthy: a fresh install or a restored registration that
/// never completed (e.g. a remote that rejected the first attempt) is
/// completed here, and the outcome of each attempt is recorded so the
/// status view can explain what a non-materialized drive is waiting
/// for. Paused drives stay out.
async fn readd_drives(ctx: &mut Ctx) {
    if !ctx.config.drives.iter().any(|d| !d.paused) {
        return;
    }
    // `/health` can pass before the MCP endpoint is mounted; wait for
    // the handshake before registering.
    if wait_switchboard_ready(ctx).await.is_err() {
        return;
    }
    for drive in ctx.config.drives.iter().filter(|d| !d.paused) {
        let token = drive_token(&drive.token_env);
        match drives::add_quiet(&ctx.config, drive, token).await {
            Ok(id) => {
                ctx.drive_outcomes
                    .insert(drive.name.clone(), drives::AddOutcome::Added);
                tracing::info!(
                    "re-registered drive '{}' ({id}) after switchboard restart",
                    drive.name
                )
            }
            Err(err) => {
                let outcome = drives::classify_add_error(&err);
                ctx.drive_outcomes.insert(drive.name.clone(), outcome);
                tracing::warn!("re-registering drive {} failed: {err:#}", drive.name);
            }
        }
    }
}
/// Executes one user command. Returns `Ok(true)` when the command asks
/// the daemon to stop itself.
async fn execute_command(
    ctx: &mut Ctx,
    cmd: Command,
    snap_tx: &watch::Sender<StatusSnapshot>,
) -> Result<bool> {
    if matches!(
        cmd,
        Command::AddDrive { .. }
            | Command::RemoveDrive { .. }
            | Command::PauseDrive { .. }
            | Command::ResumeDrive { .. }
            | Command::ResyncDrive { .. }
    ) {
        wait_switchboard_ready(ctx).await?;
    }
    match cmd {
        Command::AddDrive {
            name,
            url,
            token_env,
            offline,
        } => {
            // Record the attempted URL before the op consumes it, so a
            // failed add (drive never enters the config) still shows in
            // the status view with the rejected remote.
            ctx.drive_attempt_urls.insert(name.clone(), url.clone());
            let op = DriveOp::Add {
                name,
                url,
                token_env,
                offline,
            };
            let detail = {
                let outcomes = &mut ctx.drive_outcomes;
                apply_drive_change(&mut ctx.config, op, outcomes).await
            }?;
            persist_config(ctx, false).await?;
            push_status(ctx, snap_tx).await;
            tracing::info!("{detail}");
        }
        Command::RemoveDrive { name } => {
            ctx.drive_attempt_urls.remove(&name);
            let op = DriveOp::Remove { name };
            let detail = {
                let outcomes = &mut ctx.drive_outcomes;
                apply_drive_change(&mut ctx.config, op, outcomes).await
            }?;
            persist_config(ctx, false).await?;
            push_status(ctx, snap_tx).await;
            tracing::info!("{detail}");
        }
        Command::PauseDrive { name } => {
            let op = DriveOp::Pause { name };
            let detail = {
                let outcomes = &mut ctx.drive_outcomes;
                apply_drive_change(&mut ctx.config, op, outcomes).await
            }?;
            persist_config(ctx, false).await?;
            push_status(ctx, snap_tx).await;
            tracing::info!("{detail}");
        }
        Command::ResumeDrive { name } => {
            // Flip the flag and persist it first (visible in status and
            // the tray immediately), then run the re-add, which may take
            // a while (bounded materialization poll).
            // A missing drive is reported by the op below.
            if let Ok(d) = find_drive_mut(&mut ctx.config, &name) {
                d.paused = false;
            }
            persist_config(ctx, false).await?;
            push_status(ctx, snap_tx).await;
            let op = DriveOp::Resume { name };
            let detail = {
                let outcomes = &mut ctx.drive_outcomes;
                apply_drive_change(&mut ctx.config, op, outcomes).await
            }?;
            persist_config(ctx, false).await?;
            push_status(ctx, snap_tx).await;
            tracing::info!("{detail}");
        }
        Command::ResyncDrive { name } => {
            let op = DriveOp::Resync { name };
            let detail = {
                let outcomes = &mut ctx.drive_outcomes;
                apply_drive_change(&mut ctx.config, op, outcomes).await
            }?;
            persist_config(ctx, false).await?;
            push_status(ctx, snap_tx).await;
            tracing::info!("{detail}");
        }
        Command::SetConfig { key, value } => {
            config::set(&mut ctx.config, &key, &value)?;
            persist_config(ctx, true).await?;
            push_status(ctx, snap_tx).await;
            tracing::info!("config: set {key}");
        }
        Command::Quit => {
            tracing::info!("quit requested");
            let mut s = ctx.sup_status.lock().await;
            s.last_event = Some(StatusEvent::Stopped);
            return Ok(true);
        }
    }
    Ok(false)
}

/// Pushes a fresh snapshot. Drive ops can be long (the bounded
/// materialization poll); the loop's own refresh only runs between
/// commands, so the status and tray would otherwise sit stale.
async fn push_status(ctx: &Ctx, snap_tx: &watch::Sender<StatusSnapshot>) {
    let snap = refresh_status(ctx).await;
    let _ = snap_tx.send(snap);
}

/// Saves the daemon config, rewrites the switchboard's config file, and
/// pushes the new config to the supervisor. Drives live in the switchboard's
/// own store, so drive mutations must not respawn the child (that would
/// abort the in-flight sync); process-affecting keys do.
async fn persist_config(ctx: &mut Ctx, respawn: bool) -> Result<()> {
    // A concurrent external edit (another `ph-reactor config set`) is
    // adopted before writing, so it is not clobbered. The daemon stays
    // authoritative over the drive list.
    adopt_external(ctx, true);
    config::save(&ctx.paths, &ctx.config)?;
    switchboard::write_runtime_files(&ctx.paths, &ctx.config)?;
    ctx.last_save_mtime = std::fs::metadata(&ctx.paths.config_file)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    if respawn {
        let _ = ctx.cfg_tx.send(ctx.config.clone());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Drive operations (shared by the daemon command loop and the CLI)
// ---------------------------------------------------------------------------

/// Resolves a drive by name (case-insensitive).
fn find_drive<'a>(config: &'a ReactorConfig, name: &str) -> Result<&'a DriveConfig> {
    config
        .drives
        .iter()
        .find(|d| d.name.eq_ignore_ascii_case(name))
        .with_context(|| format!("drive '{name}' not configured (see `ph-reactor drive list`)"))
}

fn find_drive_mut<'a>(config: &'a mut ReactorConfig, name: &str) -> Result<&'a mut DriveConfig> {
    config
        .drives
        .iter_mut()
        .find(|d| d.name.eq_ignore_ascii_case(name))
        .with_context(|| format!("drive '{name}' not configured (see `ph-reactor drive list`)"))
}

/// The bearer token for a drive: the value of its `tokenEnv`, when set
/// and non-empty. Never stored, never logged.
fn drive_token(token_env: &Option<String>) -> Option<String> {
    match token_env {
        Some(var) => std::env::var(var).ok().filter(|t| !t.is_empty()),
        None => None,
    }
}

/// Applies a drive mutation: the live MCP call against the running
/// switchboard plus the persisted config update. Returns a one-line
/// summary. `config` is updated in place (the caller persists); every
/// registration attempt is recorded in `outcomes` (keyed by drive name)
/// so the status view can explain a drive that never materialized.
pub async fn apply_drive_change(
    config: &mut ReactorConfig,
    op: DriveOp,
    outcomes: &mut HashMap<String, drives::AddOutcome>,
) -> Result<String> {
    match op {
        DriveOp::Add {
            name,
            url,
            token_env,
            offline,
        } => {
            let url = url.trim().to_string();
            let token = drive_token(&token_env);
            let info = match drives::fetch_drive_info(&url, token.as_deref()).await {
                Ok(info) => info,
                Err(err) => {
                    let outcome = drives::classify_add_error(&err);
                    outcomes.insert(name.trim().to_string(), outcome);
                    bail!(add_failure_message(&err));
                }
            };
            let name = if name.trim().is_empty() {
                info.name
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or_else(|| {
                        drives::parse_drive_url(&url)
                            .map(|(_, _, slug)| slug)
                            .unwrap_or_else(|_| "drive".into())
                    })
            } else {
                name.trim().to_string()
            };
            if find_drive(config, &name).is_ok() {
                bail!("a drive named '{name}' is already configured");
            }
            let drive = DriveConfig {
                name: name.clone(),
                url: url.clone(),
                token_env,
                available_offline: offline,
                paused: false,
            };
            match drives::add(config, &drive, token).await {
                Ok(id) => {
                    outcomes.insert(name.clone(), drives::AddOutcome::Added);
                    config.drives.push(drive);
                    Ok(format!("added drive '{name}' ({url}) as {id}"))
                }
                Err(err) => {
                    let outcome = drives::classify_add_error(&err);
                    outcomes.insert(name.clone(), outcome);
                    Err(anyhow::anyhow!(add_failure_message(&err)))
                }
            }
        }
        DriveOp::Remove { name } => {
            let drive = find_drive(config, &name)
                .cloned()
                .context("no such drive")?;
            drives::remove(config, &drive, drive_token(&drive.token_env))
                .await
                .with_context(|| format!("removing drive {name}"))?;
            outcomes.remove(&drive.name);
            config.drives.retain(|d| d.name != name);
            Ok(format!("removed drive '{name}'"))
        }
        DriveOp::Pause { name } => {
            let drive = find_drive(config, &name)
                .cloned()
                .context("no such drive")?;
            if drive.paused {
                return Ok(format!("drive '{name}' is already paused"));
            }
            drives::remove(config, &drive, drive_token(&drive.token_env))
                .await
                .with_context(|| format!("pausing drive {name}"))?;
            let d = find_drive_mut(config, &name)?;
            d.paused = true;
            outcomes.remove(&drive.name);
            Ok(format!(
                "paused drive '{name}' (local mirror deleted; resume to re-sync)"
            ))
        }
        DriveOp::Resume { name } => {
            let mut drive = find_drive(config, &name)
                .cloned()
                .context("no such drive")?;
            if drive.paused {
                drive.paused = false;
                find_drive_mut(config, &name)?.paused = false;
            }
            let token = drive_token(&drive.token_env);
            match drives::add(config, &drive, token).await {
                Ok(_id) => {
                    outcomes.insert(drive.name.clone(), drives::AddOutcome::Added);
                    Ok(format!("resumed drive '{}'", drive.name))
                }
                Err(err) => {
                    let outcome = drives::classify_add_error(&err);
                    outcomes.insert(drive.name.clone(), outcome);
                    Err(anyhow::anyhow!(add_failure_message(&err)))
                }
            }
        }
        DriveOp::Resync { name } => {
            let mut drive = find_drive(config, &name)
                .cloned()
                .context("no such drive")?;
            let token = drive_token(&drive.token_env);
            // Delete the local mirror first (ignored when absent),
            // then re-add: the re-add re-syncs from the remote.
            let _ = drives::remove(config, &drive, token.clone()).await;
            drive.paused = false;
            match drives::add(config, &drive, token).await {
                Ok(_id) => {
                    let d = find_drive_mut(config, &name)?;
                    *d = drive;
                    outcomes.insert(name.clone(), drives::AddOutcome::Added);
                    Ok(format!("re-synced drive '{name}'"))
                }
                Err(err) => {
                    let outcome = drives::classify_add_error(&err);
                    outcomes.insert(name.clone(), outcome);
                    let d = find_drive_mut(config, &name)?;
                    *d = drive;
                    Err(anyhow::anyhow!(add_failure_message(&err)))
                }
            }
        }
    }
}

/// Formats a failed registration for the user: a permission rejection
/// gets the remedy (a per-drive token); anything else passes through.
fn add_failure_message(err: &anyhow::Error) -> String {
    match drives::classify_add_error(err) {
        drives::AddOutcome::RequiresAuth(_msg) => format!(
            "{err:#}; the remote requires authentication for its sync endpoint. Add the drive again with --token-env NAME and set NAME to a token the remote accepts (the value is read from the environment and never written to disk)"
        ),
        _ => err.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Status refresh
// ---------------------------------------------------------------------------

/// Builds the next snapshot: supervisor state + one live view per drive.
async fn refresh_status(ctx: &Ctx) -> StatusSnapshot {
    let sup = ctx.sup_status.lock().await;
    let sb = SwitchboardStatus {
        running: sup.running,
        healthy: sup.healthy,
        version: switchboard::current_meta(&ctx.paths, &ctx.config.switchboard.package_spec)
            .map(|m| m.version),
        port: ctx.config.switchboard.port,
        restarts: sup.restarts,
        last_event: sup.last_event.as_ref().map(|e| e.summary()),
    };
    let mut drives = Vec::with_capacity(ctx.config.drives.len());
    for d in &ctx.config.drives {
        let entry = if d.paused {
            DriveStatusEntry {
                name: d.name.clone(),
                url: d.url.clone(),
                paused: true,
                status: "paused".into(),
                detail: "paused; resume to re-sync".into(),
            }
        } else if sb.healthy {
            let view = drives::status_view(
                &ctx.config,
                d,
                drive_token(&d.token_env),
                ctx.drive_outcomes.get(&d.name),
            )
            .await;
            DriveStatusEntry {
                name: d.name.clone(),
                url: d.url.clone(),
                paused: false,
                status: view.status.as_str().to_string(),
                detail: view.detail,
            }
        } else {
            DriveStatusEntry {
                name: d.name.clone(),
                url: d.url.clone(),
                paused: false,
                status: "offline".into(),
                detail: "switchboard not healthy".into(),
            }
        };
        drives.push(entry);
    }
    // A failed add never enters `config.drives`; surface the recorded
    // outcome so the user sees which remote was rejected and why.
    for (name, outcome) in ctx.drive_outcomes.iter() {
        if ctx.config.drives.iter().any(|d| &d.name == name) {
            continue;
        }
        let (status, detail) = match outcome {
            drives::AddOutcome::Added => continue,
            drives::AddOutcome::RequiresAuth(msg) => (
                "requires-auth",
                format!(
                    "the remote rejected the sync registration (authentication required): {msg}"
                ),
            ),
            drives::AddOutcome::Failed(msg) => ("error", msg.clone()),
        };
        drives.push(DriveStatusEntry {
            name: name.clone(),
            url: ctx
                .drive_attempt_urls
                .get(name)
                .cloned()
                .unwrap_or_default(),
            paused: false,
            status: status.into(),
            detail,
        });
    }
    let settings_url = format!(
        "http://{}:{}",
        ctx.config.settings.host, ctx.config.settings.port
    );
    StatusSnapshot {
        version: crate::VERSION.to_string(),
        switchboard: sb,
        drives,
        settings: status::SettingsStatus { url: settings_url },
        updated_at: switchboard::rfc3339_now(),
    }
}

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

/// Installs the switchboard when the spec or installation changed, and
/// (re)writes its `powerhouse.config.json`.
async fn bootstrap_switchboard(
    paths: &StatePaths,
    node: &node::NodeRuntime,
    config: &ReactorConfig,
) -> Result<()> {
    let spec = &config.switchboard.package_spec;
    if switchboard::is_current(paths, spec) {
        match switchboard::patch_sync_client(paths) {
            Ok(outcome) => tracing::info!("sync-client patch: {outcome}"),
            Err(err) => tracing::warn!("sync-client patch failed: {err:#}"),
        }
        switchboard::write_runtime_files(paths, config)?;
        return Ok(());
    }
    tracing::info!(
        "installing switchboard {spec} from {}",
        config.switchboard.npm_registry
    );
    let meta = switchboard::install(paths, node, spec, &config.switchboard.npm_registry).await?;
    tracing::info!("switchboard {} installed", meta.version);
    match switchboard::patch_sync_client(paths) {
        Ok(outcome) => tracing::info!("sync-client patch: {outcome}"),
        Err(err) => tracing::warn!("sync-client patch failed: {err:#}"),
    }
    switchboard::write_runtime_files(paths, config)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Daemonization + single-instance lock
// ---------------------------------------------------------------------------

/// Redirects stdin/stdout/stderr to /dev/null (daemon child).
fn detach_stdio() {
    let path = b"/dev/null\0";
    let devnull = unsafe { libc::open(path.as_ptr() as *const i8, libc::O_RDWR) };
    if devnull < 0 {
        return;
    }
    for fd in [0, 1, 2] {
        unsafe {
            libc::dup2(devnull, fd);
        }
    }
    unsafe {
        libc::close(devnull);
    }
}

fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Takes the single-instance lock (`flock`, exclusive, non-blocking) and
/// fails with the stale pidfile's owner when another daemon holds it.
fn acquire_lock(paths: &StatePaths) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.lock_file())
        .with_context(|| format!("opening {}", paths.lock_file().display()))?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        if let Ok(stale) = std::fs::read_to_string(paths.daemon_pidfile()) {
            let pid: i32 = stale.trim().parse().unwrap_or(-1);
            if pid > 0 && process_alive(pid) {
                bail!("another ph-reactor is already running (pid {pid})");
            }
        }
        bail!("state directory lock is held: {errno}");
    }
    Ok(file)
}

fn remove_ready(paths: &StatePaths) {
    let _ = std::fs::remove_file(paths.run_dir.join("ready"));
}

// ---------------------------------------------------------------------------
// stop
// ---------------------------------------------------------------------------

/// Sends SIGTERM to the running daemon and waits for it to exit
/// (escalating to SIGKILL past the grace period).
pub async fn stop(state_dir: Option<&Path>) -> Result<()> {
    let paths = StatePaths::resolve(state_dir);
    let pidfile = &paths.daemon_pidfile();
    let raw = match std::fs::read_to_string(pidfile) {
        Ok(r) => r,
        Err(_) => {
            println!("ph-reactor is not running");
            return Ok(());
        }
    };
    let pid: i32 = raw
        .trim()
        .parse()
        .with_context(|| format!("unreadable pidfile {}", pidfile.display()))?;
    if !process_alive(pid) {
        println!("ph-reactor is not running (stale pidfile removed)");
        let _ = std::fs::remove_file(pidfile);
        return Ok(());
    }
    println!("stopping ph-reactor (pid {pid})…");
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + STOP_GRACE;
    while process_alive(pid) {
        if Instant::now() > deadline {
            tracing::warn!("grace period exceeded; sending SIGKILL");
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!("ph-reactor stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

/// Prints the shared snapshot: from the running daemon when it is up,
/// from the config file (degraded) when it is not. With `--json` the
/// `StatusSnapshot` is emitted as JSON (the shell-plugin contract).
pub async fn status(state_dir: Option<&Path>, json: bool) -> Result<()> {
    let paths = StatePaths::resolve(state_dir);
    paths.ensure_dirs()?;
    let config = config::load(&paths)?;
    let url = format!(
        "http://{}:{}/api/status",
        config.settings.host, config.settings.port
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;
    let live = match client
        .get(&url)
        .send()
        .await
        .ok()
        .filter(|r| r.status().is_success())
    {
        Some(resp) => resp.json::<status::StatusSnapshot>().await.ok(),
        None => None,
    };
    match live {
        Some(snap) => {
            if json {
                println!("{}", serde_json::to_string(&snap)?);
            } else {
                print!("{}", status::render_text(&snap));
            }
            Ok(())
        }
        None => {
            let snap = degraded_snapshot(&paths, &config);
            if json {
                println!("{}", serde_json::to_string(&snap)?);
            } else {
                print!("{}", status::render_text(&snap));
            }
            Ok(())
        }
    }
}

/// A snapshot built from the config file and a direct health probe when
/// the daemon is not running.
fn degraded_snapshot(paths: &StatePaths, config: &ReactorConfig) -> StatusSnapshot {
    let sb = SwitchboardStatus {
        running: false,
        healthy: false,
        version: switchboard::current_meta(paths, &config.switchboard.package_spec)
            .map(|m| m.version),
        port: config.switchboard.port,
        restarts: 0,
        last_event: Some("daemon not running".into()),
    };
    let drives = config
        .drives
        .iter()
        .map(|d| DriveStatusEntry {
            name: d.name.clone(),
            url: d.url.clone(),
            paused: d.paused,
            status: if d.paused {
                "paused".into()
            } else {
                "offline".into()
            },
            detail: "daemon not running".into(),
        })
        .collect();
    let settings_url = format!("http://{}:{}", config.settings.host, config.settings.port);
    StatusSnapshot {
        version: crate::VERSION.to_string(),
        switchboard: sb,
        drives,
        settings: status::SettingsStatus { url: settings_url },
        updated_at: switchboard::rfc3339_now(),
    }
}

// ---------------------------------------------------------------------------
// drive subcommand
// ---------------------------------------------------------------------------

/// The `drive` CLI: goes through the running daemon (single writer) when
/// one is up, or works directly against a manually running switchboard.
pub async fn drive_command(state_dir: Option<&Path>, cmd: crate::cli::DriveCommand) -> Result<()> {
    use crate::cli::DriveCommand;
    let paths = StatePaths::resolve(state_dir);
    paths.ensure_dirs()?;
    let mut config = config::load(&paths)?;

    match cmd {
        DriveCommand::List => {
            let snap = live_drive_views(&config).await;
            if snap.is_empty() {
                println!("no drives configured (add one: ph-reactor drive add <url>)");
                return Ok(());
            }
            for (i, d) in snap.iter().enumerate() {
                println!("{:>2}  {:<24} [{:<10}] {}", i + 1, d.name, d.status, d.url);
                if !d.detail.is_empty() {
                    println!("    {}", d.detail);
                }
            }
            Ok(())
        }
        DriveCommand::Add {
            url,
            name,
            token_env,
            offline,
        } => {
            let op = DriveOp::Add {
                name: name.unwrap_or_default(),
                url,
                token_env,
                offline,
            };
            dispatch_drive_op(&paths, &mut config, op).await
        }
        DriveCommand::Remove { target } => {
            let name = resolve_target(&config, &target)?;
            dispatch_drive_op(&paths, &mut config, DriveOp::Remove { name }).await
        }
        DriveCommand::Pause { target } => {
            let name = resolve_target(&config, &target)?;
            dispatch_drive_op(&paths, &mut config, DriveOp::Pause { name }).await
        }
        DriveCommand::Resume { target } => {
            let name = resolve_target(&config, &target)?;
            dispatch_drive_op(&paths, &mut config, DriveOp::Resume { name }).await
        }
        DriveCommand::Resync { target } => {
            let name = resolve_target(&config, &target)?;
            dispatch_drive_op(&paths, &mut config, DriveOp::Resync { name }).await
        }
    }
}

/// A 1-based index or a drive name.
fn resolve_target(config: &ReactorConfig, target: &str) -> Result<String> {
    if let Ok(idx) = target.parse::<usize>() {
        if idx >= 1 && idx <= config.drives.len() {
            return Ok(config.drives[idx - 1].name.clone());
        }
        bail!(
            "index {idx} out of range ({} drives configured)",
            config.drives.len()
        );
    }
    let drive = find_drive(config, target)?;
    Ok(drive.name.clone())
}

/// Routes the operation through the daemon's settings API when the
/// daemon is running (single writer); otherwise executes it directly.
async fn dispatch_drive_op(
    paths: &StatePaths,
    config: &mut ReactorConfig,
    op: DriveOp,
) -> Result<()> {
    if daemon_is_running(&paths.daemon_pidfile()) {
        let (method, url, body) = api_target(config, &op)?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        let req = match method {
            "POST" => client.post(&url),
            _ => client.delete(&url),
        };
        let req = match body {
            Some(b) => req.json(&b),
            None => req,
        };
        match req.send().await {
            Ok(r) if r.status().is_success() => {
                println!("sent to the running daemon (it will apply it shortly)");
                Ok(())
            }
            Ok(r) => bail!("daemon rejected the request: {}", r.status()),
            Err(err) => {
                eprintln!("daemon is not reachable; falling back to a direct operation ({err})");
                direct_drive_op(paths, config, op).await
            }
        }
    } else {
        direct_drive_op(paths, config, op).await
    }
}

/// Runs the operation in-process (no daemon): the live MCP call plus
/// the config file update. Requires a switchboard running on the
/// configured port.
async fn direct_drive_op(
    paths: &StatePaths,
    config: &mut ReactorConfig,
    op: DriveOp,
) -> Result<()> {
    let name = match &op {
        DriveOp::Add { name, .. } if name.trim().is_empty() => String::new(),
        DriveOp::Add { name, .. } => name.clone(),
        DriveOp::Remove { name }
        | DriveOp::Pause { name }
        | DriveOp::Resume { name }
        | DriveOp::Resync { name } => name.clone(),
    };
    if let Some(existing) = config
        .drives
        .iter()
        .find(|d| !name.is_empty() && d.name.eq_ignore_ascii_case(&name))
    {
        if matches!(op, DriveOp::Add { .. }) {
            bail!("a drive named '{}' is already configured", existing.name);
        }
    }
    // One-shot process: the outcome map exists only to shape the error
    // message (there is no long-lived status view to feed).
    let mut outcomes: HashMap<String, drives::AddOutcome> = HashMap::new();
    let detail = apply_drive_change(config, op, &mut outcomes).await?;
    config::save(paths, config)?;
    switchboard::write_runtime_files(paths, config)?;
    println!("{detail}");
    Ok(())
}

fn api_target(
    config: &ReactorConfig,
    op: &DriveOp,
) -> Result<(&'static str, String, Option<serde_json::Value>)> {
    let base = format!(
        "http://{}:{}/api/drives",
        config.settings.host, config.settings.port
    );
    Ok(match op {
        DriveOp::Add {
            name,
            url,
            token_env,
            offline,
        } => {
            let body = serde_json::json!({
                "name": name,
                "url": url,
                "tokenEnv": token_env,
                "availableOffline": offline,
            });
            ("POST", base, Some(body))
        }
        DriveOp::Remove { name } => (
            "DELETE",
            format!("{base}/{}", encode_path_segment(name)),
            None,
        ),
        DriveOp::Pause { name } => (
            "POST",
            format!("{base}/{}/pause", encode_path_segment(name)),
            None,
        ),
        DriveOp::Resume { name } => (
            "POST",
            format!("{base}/{}/resume", encode_path_segment(name)),
            None,
        ),
        DriveOp::Resync { name } => (
            "POST",
            format!("{base}/{}/resync", encode_path_segment(name)),
            None,
        ),
    })
}

/// Percent-encodes a drive name for use as a URL path segment (names may
/// contain spaces).
fn encode_path_segment(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// One live view per configured drive (for `drive list`).
async fn live_drive_views(config: &ReactorConfig) -> Vec<status::DriveStatusEntry> {
    let base = format!("http://127.0.0.1:{}", config.switchboard.port);
    let sb_healthy = switchboard::health(&base).await.unwrap_or(false);
    let mut out = Vec::new();
    for d in &config.drives {
        let entry = if d.paused {
            status::DriveStatusEntry {
                name: d.name.clone(),
                url: d.url.clone(),
                paused: true,
                status: "paused".into(),
                detail: "paused; resume to re-sync".into(),
            }
        } else if sb_healthy {
            let view = drives::status_view(config, d, drive_token(&d.token_env), None).await;
            status::DriveStatusEntry {
                name: d.name.clone(),
                url: d.url.clone(),
                paused: false,
                status: view.status.as_str().to_string(),
                detail: view.detail,
            }
        } else {
            status::DriveStatusEntry {
                name: d.name.clone(),
                url: d.url.clone(),
                paused: false,
                status: "offline".into(),
                detail: "switchboard not healthy".into(),
            }
        };
        out.push(entry);
    }
    out
}

fn daemon_is_running(pidfile: &Path) -> bool {
    let raw = match std::fs::read_to_string(pidfile) {
        Ok(r) => r,
        Err(_) => return false,
    };
    match raw.trim().parse::<i32>() {
        Ok(pid) if pid > 0 => process_alive(pid),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

/// Diagnoses the local setup; non-zero exit when a check fails.
pub async fn doctor(state_dir: Option<&Path>) -> Result<()> {
    let paths = StatePaths::resolve(state_dir);
    paths.ensure_dirs()?;
    let config = config::load(&paths)?;
    let mut failed = 0;
    let mut check = |name: &str, ok: bool, detail: &str| {
        let mark = if ok { "PASS" } else { "FAIL" };
        println!("{mark}  {name:<16} {detail}");
        if !ok {
            failed += 1;
        }
    };

    // State dir.
    let dirs_ok = paths.ensure_dirs().is_ok();
    check("state dir", dirs_ok, &format!("{}", paths.root.display()));

    // Node.
    match node::resolve(
        &config.switchboard.node.minimum_version,
        config.switchboard.node.prefer_system,
        &paths,
    )
    .await
    {
        Ok(n) => check(
            "node",
            true,
            &format!("{} at {}", n.version, n.path.display()),
        ),
        Err(err) => check("node", false, &err.to_string()),
    }

    // Switchboard installation.
    let spec = &config.switchboard.package_spec;
    match switchboard::current_meta(&paths, spec) {
        Some(m) => check("switchboard", true, &format!("{} ({spec})", m.version)),
        None => check(
            "switchboard",
            false,
            &format!("not installed for spec {spec} (run `ph-reactor` to bootstrap)"),
        ),
    }

    // Registry.
    if let Some(first) = config.packages.first() {
        match registry::check_package(&config.registry, first).await {
            Ok(p) => check(
                "registry",
                true,
                &format!(
                    "{} ({} {})",
                    config.registry,
                    p.name,
                    p.latest.as_deref().unwrap_or("?")
                ),
            ),
            Err(err) => check("registry", false, &err.to_string()),
        }
    }

    // MCP (only meaningful when the switchboard is up).
    let base = format!("http://127.0.0.1:{}", config.switchboard.port);
    let healthy = switchboard::health(&base).await.unwrap_or(false);
    if healthy {
        let mcp = crate::mcp::Mcp::new(config.switchboard.port, None);
        match (mcp.session().await, mcp.tools_list().await) {
            (Ok(_), Ok(tools)) => check("mcp", true, &format!("{} tools", tools.len())),
            (Ok(_), Err(err)) | (Err(err), _) => check("mcp", false, &err.to_string()),
        }
    } else {
        check("mcp", true, "skipped (switchboard not running)");
    }

    if failed > 0 {
        bail!("{failed} check(s) failed");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// logs
// ---------------------------------------------------------------------------

/// Prints the last lines of the daemon or switchboard log; follows the
/// file on growth when `follow`.
pub async fn logs(state_dir: Option<&Path>, follow: bool, switchboard_log: bool) -> Result<()> {
    let paths = StatePaths::resolve(state_dir);
    let path = if switchboard_log {
        paths.switchboard_log()
    } else {
        paths.reactor_log()
    };
    let mut offset: u64 = 0;
    loop {
        match std::fs::read(&path) {
            Ok(bytes) => {
                if (bytes.len() as u64) < offset {
                    offset = 0; // rotated
                }
                if bytes.len() as u64 > offset {
                    let start = (offset as usize).min(bytes.len());
                    let chunk = &bytes[start..];
                    if let Ok(text) = std::str::from_utf8(chunk) {
                        print!("{text}");
                    }
                    offset = bytes.len() as u64;
                }
            }
            Err(_) => offset = 0, // not there yet
        }
        if !follow {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

struct DaemonLogWriter {
    path: PathBuf,
}

struct LogAppend {
    path: PathBuf,
}

impl Write for LogAppend {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        crate::logrotate::append(&self.path, buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for DaemonLogWriter {
    type Writer = LogAppend;
    fn make_writer(&self) -> Self::Writer {
        LogAppend {
            path: self.path.clone(),
        }
    }
}

/// File appender (rotated) plus, when running in the foreground, stderr.
fn init_logging(paths: &StatePaths, config: &ReactorConfig, to_stderr: bool) -> Result<()> {
    let level = match config.log_level.as_str() {
        "verbose" => "debug",
        "debug" => "debug",
        "info" => "info",
        "warn" => "warn",
        "error" => "error",
        "silent" => "off",
        other => other,
    };
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(DaemonLogWriter {
            path: paths.reactor_log(),
        })
        .with_ansi(false)
        .with_target(false);
    if to_stderr {
        tracing_subscriber::registry()
            .with(filter)
            .with(file_layer)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(io::stderr)
                    .with_ansi(false),
            )
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(file_layer)
            .init();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{NodeConfig, SettingsConfig, SwitchboardConfig};
    use crate::status::SettingsStatus;
    use clap::Parser;
    use std::collections::BTreeMap;

    fn test_config() -> ReactorConfig {
        ReactorConfig {
            version: 1,
            switchboard: SwitchboardConfig {
                port: 4001,
                package_spec: "@powerhousedao/switchboard".into(),
                npm_registry: "https://registry.dev.vetra.io".into(),
                node: NodeConfig::default(),
            },
            registry: "https://registry.dev.vetra.io".into(),
            packages: Vec::new(),
            drives: vec![DriveConfig {
                name: "vault".into(),
                url: "https://demo.invalid/d/vault".into(),
                token_env: None,
                available_offline: false,
                paused: false,
            }],
            settings: SettingsConfig::default(),
            log_level: "info".into(),
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn status_json_flag_parses() {
        let args = crate::cli::CliArgs::parse_from(["ph-reactor", "status", "--json"]);
        match args.command {
            Some(crate::cli::Command::Status { json: true }) => {}
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    /// The exact JSON shape the shell plugin (ph-reactor-omarchy) consumes
    /// from `status --json` while the daemon is not running.
    #[test]
    fn degraded_snapshot_matches_the_shell_contract() {
        let t = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::for_root(t.path());
        let snap = degraded_snapshot(&paths, &test_config());
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&snap).unwrap()).unwrap();

        for key in ["version", "switchboard", "drives", "settings", "updated_at"] {
            assert!(v.get(key).is_some(), "missing top-level key {key}");
        }

        let sb = &v["switchboard"];
        for key in [
            "running",
            "healthy",
            "version",
            "port",
            "restarts",
            "last_event",
        ] {
            assert!(sb.get(key).is_some(), "missing switchboard key {key}");
        }
        assert_eq!(sb["running"], false);
        assert_eq!(sb["healthy"], false);
        assert_eq!(sb["version"], serde_json::Value::Null);
        assert_eq!(sb["port"], 4001);
        assert_eq!(sb["restarts"], 0);
        assert_eq!(sb["last_event"], "daemon not running");

        let d = &v["drives"][0];
        for key in ["name", "url", "paused", "status", "detail"] {
            assert!(d.get(key).is_some(), "missing drive key {key}");
        }
        assert_eq!(d["name"], "vault");
        assert_eq!(d["url"], "https://demo.invalid/d/vault");
        assert_eq!(d["paused"], false);
        assert_eq!(d["status"], "offline");
        assert_eq!(d["detail"], "daemon not running");

        assert_eq!(v["settings"]["url"], "http://127.0.0.1:4002");
        assert_eq!(v["version"], crate::VERSION);

        let ts = v["updated_at"].as_str().unwrap();
        assert!(
            ts.chars().next().is_some_and(|c| c.is_ascii_digit()) && ts.contains('T'),
            "updated_at is not RFC 3339-shaped: {ts}"
        );
    }

    /// The daemon's `/api/status` serves the same struct; the live shape
    /// (healthy switchboard, paused drive) must carry the same keys and the
    /// documented drive-status vocabulary.
    #[test]
    fn live_snapshot_shape_is_the_same_contract() {
        let snap = StatusSnapshot {
            version: crate::VERSION.to_string(),
            switchboard: SwitchboardStatus {
                running: true,
                healthy: true,
                version: Some("6.2.2".into()),
                port: 4001,
                restarts: 1,
                last_event: Some("switchboard healthy".into()),
            },
            drives: vec![DriveStatusEntry {
                name: "vault".into(),
                url: "https://demo.invalid/d/vault".into(),
                paused: true,
                status: "paused".into(),
                detail: String::new(),
            }],
            settings: SettingsStatus {
                url: "http://127.0.0.1:4002".into(),
            },
            updated_at: "2026-09-12T00:00:00Z".into(),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&snap).unwrap()).unwrap();
        assert_eq!(v["switchboard"]["running"], true);
        assert_eq!(v["switchboard"]["healthy"], true);
        assert_eq!(v["switchboard"]["version"], "6.2.2");
        assert_eq!(v["drives"][0]["paused"], true);
        assert_eq!(v["drives"][0]["status"], "paused");
    }
}
