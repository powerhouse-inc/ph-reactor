//! The daemon: assembles the p2p sync engine, the local doc store, the
//! settings server, the tray, and the status poller; and implements the
//! CLI subcommands that talk to (or work around) a running daemon.
//!
//! Lifecycle:
//! `run` → identity key → doc store → p2p engine task + settings
//! server + tray → event/command/poll loop → on SIGTERM/SIGINT: shut
//! down the engine, stop the tray and the settings server, release the
//! lock, remove the pidfile.
//!
//! The engine runs on its own tokio task; the daemon's loop consumes
//! its [`EngineEvent`] stream (drive status changes, doc changes, the
//! identity announcement) and feeds the shared status snapshot.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use libp2p::{Multiaddr, PeerId};
use tokio::sync::{mpsc, watch};
use tokio::time::interval;

use crate::commands::{Command, DriveOp};
use crate::config::{self, DriveConfig, ReactorConfig};
use crate::drives::{Drive, DriveStatus};
use crate::p2p::{self, EngineCommand, EngineEvent, SyncEngine};
use crate::paths::StatePaths;
use crate::query::{parse_filter, query_docs};
use crate::settings::Settings;
use crate::status::{self, DriveStatusEntry, ReactorStatus, StatusSnapshot};
use crate::store::Store;
use crate::tray;

/// How often the daemon refreshes the status snapshot.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How long `run --daemonize` waits for the child to become ready.
const DAEMONIZE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `stop` waits for a graceful exit before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(15);

/// Load the local ban list (base58 peer ids) from `bans.json`; empty when
/// the file is missing or unparseable.
fn load_bans(paths: &StatePaths) -> HashSet<String> {
    let Ok(data) = std::fs::read_to_string(paths.bans_file()) else {
        return HashSet::new();
    };
    serde_json::from_str(&data).unwrap_or_default()
}

/// Persist the local ban list to `bans.json`.
fn save_bans(paths: &StatePaths, bans: &HashSet<String>) -> Result<()> {
    let mut v: Vec<String> = bans.iter().cloned().collect();
    v.sort();
    let data = serde_json::to_string_pretty(&v)?;
    std::fs::write(paths.bans_file(), data)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

/// Runs the daemon in the foreground (used when `--daemonize` is not
/// set).
pub async fn run(state_dir: Option<&Path>, daemonize: bool) -> Result<()> {
    assert!(
        !daemonize,
        "daemonized start is handled before the runtime begins"
    );
    // Foreground: also emit to stdout. This is what makes `kubectl logs`
    // (and a plain terminal run) show anything at all.
    run_inner(state_dir, true).await
}

/// The daemonized start. The parent's CLI tokio runtime is still alive
/// when `fork()` runs; the child inherits its memory (copy-on-write) and
/// its unused fds but must never touch that runtime — the child builds
/// and runs an entirely fresh one. The parent waits synchronously for
/// the child's ready file and exits; the child runs the daemon.
pub fn run_daemonized(state_dir: Option<&Path>) -> Result<i32> {
    let paths = StatePaths::resolve(state_dir);
    paths
        .ensure_dirs()
        .with_context(|| format!("creating {}", paths.root.display()))?;

    let pid = unsafe { libc::fork() };
    match pid {
        0 => {
            // Child: detach stdio. The daemon logs to its rotated file;
            // stderr (panics, fatal errors) goes to logs/stderr.log so a
            // forked daemon cannot fail silently into /dev/null.
            detach_stdio(&paths.logs_dir.join("stderr.log"));
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("starting the daemon runtime")?;
            match rt.block_on(run_inner(state_dir, false)) {
                Ok(()) => Ok(0),
                Err(err) => {
                    // A daemonized child has no terminal: the fatal
                    // error goes to stderr.log (and reactor.log when
                    // the subscriber is up) or it is lost.
                    eprintln!("ph-reactor: {err:#}");
                    if let Some(cause) = err.source() {
                        eprintln!("  caused by: {cause}");
                    }
                    tracing::error!("daemon failed to start: {err:#}");
                    Ok(1)
                }
            }
        }
        child if child > 0 => {
            // Parent: wait for the ready file. The child is reaped with
            // a non-blocking waitpid — kill(pid, 0) would report a dead
            // child (a zombie) as alive and hide startup failures.
            let ready = paths.run_dir.join("ready");
            let _ = std::fs::remove_file(&ready); // stale
            let deadline = Instant::now() + DAEMONIZE_TIMEOUT;
            while Instant::now() < deadline {
                if let Ok(content) = std::fs::read_to_string(&ready) {
                    let _ = reap_child(child);
                    println!("ph-reactor started (pid {})", content.trim());
                    return Ok(0);
                }
                if let Some(status) = reap_child(child) {
                    print_startup_failure(
                        &paths,
                        &format!("ph-reactor exited during startup ({status})"),
                    );
                    return Ok(1);
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            print_startup_failure(
                &paths,
                &format!("daemon did not become ready within {DAEMONIZE_TIMEOUT:?}"),
            );
            bail!("daemon did not become ready within {DAEMONIZE_TIMEOUT:?}");
        }
        _ => bail!("fork failed"),
    }
}

/// The daemon proper: store, p2p engine, settings server, tray, and the
/// status/command loop. Runs on a fresh runtime in both modes.
/// Set across the handover exec so a staged binary that somehow points back at
/// an older one cannot bounce between the two forever.
const HANDOVER_GUARD: &str = "PH_REACTOR_HANDED_OVER";

async fn run_inner(state_dir: Option<&Path>, to_stdout: bool) -> Result<()> {
    let paths = StatePaths::resolve(state_dir);
    paths
        .ensure_dirs()
        .with_context(|| format!("creating {}", paths.root.display()))?;
    let config = config::load(&paths)?;
    init_logging(&paths, &config, to_stdout)?;

    // Hand over to a staged binary, if one supersedes this process.
    //
    // This exists for the container case. There the daemon cannot replace the
    // binary it was started from -- the root filesystem is read-only -- so an
    // update lands in the state directory instead, and something has to make
    // the next start use it. Without this handover the supervisor relaunches
    // the image binary, which finds the same release still newer than itself
    // and applies it again: a restart loop wearing the costume of an upgrade.
    //
    // Before the lock is taken and before anything is opened, so the replacement
    // inherits a clean process rather than one holding a lock it does not know
    // about. Every failure falls through to running this binary, because a node
    // that starts is better than one that is certain it should not.
    if let Some((path, version)) = crate::update::apply::staged_replacement(&paths.root, crate::VERSION) {
        if std::env::var_os(HANDOVER_GUARD).is_some() {
            // Already handed over once and still here: the staged binary
            // re-execed back to us, or the marker is lying about its version.
            // Either way, stop bouncing and run what we have.
            tracing::warn!(
                "ignoring staged {version} at {}: already handed over once this start",
                path.display()
            );
        } else {
            tracing::warn!(
                "handing over to staged {version} at {} (this binary is {running})",
                path.display(),
                running = crate::VERSION,
            );
            let err = std::os::unix::process::CommandExt::exec(
                std::process::Command::new(&path)
                    .args(std::env::args_os().skip(1))
                    .env(HANDOVER_GUARD, "1"),
            );
            // exec only returns on failure.
            tracing::error!(
                "could not exec staged {}: {err}; continuing as {running}",
                path.display(),
                running = crate::VERSION,
            );
        }
    }

    let lock = acquire_lock(&paths)?;
    let pid = std::process::id();
    std::fs::write(paths.daemon_pidfile(), pid.to_string())
        .with_context(|| format!("writing {}", paths.daemon_pidfile().display()))?;

    // The identity: one ed25519 key per state dir (0600). The Keypair
    // is the libp2p/gossipsub identity; the dalek SigningKey (same
    // bytes) signs doc ops.
    let kp = p2p::load_or_create_identity(&paths.key_file())
        .map_err(anyhow::Error::msg)
        .with_context(|| {
            format!(
                "loading the identity key from {}",
                paths.key_file().display()
            )
        })?;
    let peer_id = p2p::peer_id_of(&kp);
    let signing = p2p::signing_key(&kp).map_err(anyhow::Error::msg)?;
    let _pubkey = p2p::public_key_bytes(&kp).map_err(anyhow::Error::msg)?;

    let listen = Multiaddr::from_str(&config.instance.listen)
        .with_context(|| format!("config instance.listen ({})", config.instance.listen))?;
    check_listen_port(&listen)?;
    check_settings_port(&config.settings.host, config.settings.port)?;

    // The doc store (snapshots + live logs under <state>/docs), seeded with
    // the realistic models so the console can create docs under them. The
    // built-in `open@1` + `group@1` come from `Store::open`.
    let store = {
        // Everything that is not built in must be registered BEFORE the
        // replay: the store rebuilds each document by reducing its action log
        // through the model that wrote it, so a model registered afterwards
        // leaves its documents as empty shells. That applies to the realistic
        // set and to anything registered at runtime.
        let mut seed: Vec<Arc<dyn crate::model::Model>> = Vec::new();
        for m in crate::model::realistic::realistic_models() {
            seed.push(Arc::new(m));
        }
        let mut restored = 0usize;
        for def in crate::model::persist::load_all(&paths.models_file()) {
            match crate::model::l1::L1::from_def(def.clone()) {
                Ok(m) => {
                    seed.push(Arc::new(m));
                    restored += 1;
                }
                Err(e) => tracing::error!(
                    "a persisted model definition is invalid ({e}); its documents \
                     will replay empty until it is re-registered"
                ),
            }
        }
        if restored > 0 {
            tracing::info!("restored {restored} runtime-registered model(s)");
        }
        Store::open_with_models(&paths.docs_dir, &signing, &peer_id.to_base58(), seed)
            .map_err(anyhow::Error::msg)?
    };
    tracing::info!(
        "store ready ({} live docs, peer {})",
        store.live_doc_count(),
        peer_id
    );

    // The p2p engine (its own task; talks to this loop through two
    // unbounded channels).
    let (eng_cmd_tx, eng_cmd_rx) = mpsc::unbounded_channel::<EngineCommand>();
    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel::<EngineEvent>();
    let token = config
        .p2p
        .token_env
        .as_deref()
        .and_then(|n| std::env::var(n).ok())
        .filter(|s| !s.is_empty());
    let bootstraps: Vec<(PeerId, Multiaddr)> = config
        .p2p
        .bootstraps
        .iter()
        .filter_map(|s| crate::p2p::parse_bootstrap(s))
        .collect();
    // Peers on the local ban list are refused: the engine will not complete
    // a handshake with them (even if their key is valid).
    let banned: HashSet<PeerId> = load_bans(&paths)
        .iter()
        .filter_map(|s| PeerId::from_str(s).ok())
        .collect();
    // Optional WebSocket listener. A bad multiaddr is a config error worth
    // shouting about, but it must not stop the TCP listener from coming up.
    let listen_ws = match config.instance.listen_ws.as_deref() {
        Some(raw) => match Multiaddr::from_str(raw) {
            Ok(addr) => Some(addr),
            Err(err) => {
                tracing::warn!("ignoring invalid instance.listenWs {raw:?}: {err}");
                None
            }
        },
        None => None,
    };
    // Addresses to announce. Same tolerance: a typo in one entry must not
    // take the node down.
    let external: Vec<Multiaddr> = config
        .instance
        .external
        .iter()
        .filter_map(|raw| match Multiaddr::from_str(raw) {
            Ok(addr) => Some(addr),
            Err(err) => {
                tracing::warn!("ignoring invalid instance.external entry {raw:?}: {err}");
                None
            }
        })
        .collect();

    // Chunk store for package bundles, shared with the engine so this node can
    // serve blobs it holds to peers that lack them.
    let blobs =
        Arc::new(crate::blob::BlobStore::open(&paths.blobs_dir()).map_err(anyhow::Error::msg)?);
    let engine = SyncEngine::new(
        &kp,
        store.clone(),
        &config.instance.name,
        listen,
        listen_ws,
        external,
        blobs.clone(),
        config.p2p.mdns,
        config.p2p.dht,
        config.p2p.relay,
        token,
        banned,
        eng_cmd_rx,
        evt_tx,
    )
    .await
    .context("building the p2p engine")?;
    let mut engine_task = tokio::spawn(async move {
        engine.run().await;
    });
    // Seed the DHT with the configured bootstrap peers once the engine is up.
    if !bootstraps.is_empty() {
        let _ = eng_cmd_tx.send(EngineCommand::DhtBootstrap { peers: bootstraps });
    }

    // Channels: status fan-out and the single-writer command channel
    // (tray + settings + CLI all send here).
    let settings_url = format!("http://{}:{}", config.settings.host, config.settings.port);
    let (snap_tx, snap_rx) = watch::channel(StatusSnapshot::empty(
        crate::VERSION.to_string(),
        config.instance.listen.clone(),
        settings_url.clone(),
    ));
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();

    // The user-configurable processor engine (subscriptions on doc changes):
    // it subscribes in its constructor and runs as a background task.
    let proc_file = paths.processors_file();
    // First run: seed the reference subscription so the console has a working
    // example of the doc-change engine (the README promises this). A persisted
    // file means the user has configured subscriptions; never overwrite it.
    if !proc_file.exists() {
        crate::processor::save_specs(&proc_file, &[crate::processor::reference_spec()]);
    }
    let processor_runner = crate::processor::ProcessorRunner::new(store.clone(), proc_file);
    let processor_handle = processor_runner.handle();
    processor_runner.spawn();

    // The loopback settings server.
    let settings = Settings::new(
        cmd_tx.clone(),
        snap_rx.clone(),
        store.clone(),
        paths.clone(),
        processor_handle,
        blobs.clone(),
        config.update.auto,
    )
    .start(&config.settings.host, config.settings.port)
    .await
    .with_context(|| {
        format!(
            "starting the settings server on {}:{}",
            config.settings.host, config.settings.port
        )
    })?;

    // The plugin asset origin: a SECOND listener, serving editor bundles and
    // nothing else. A different port is a different browser origin, which is
    // what keeps a plugin's UI away from the unauthenticated console API.
    // Failing to bind it is not fatal -- the reactor is fully usable without
    // plugin UI, and refusing to start over it would be a poor trade.
    let assets_port = config.settings.port.saturating_add(1);
    if let Err(e) = crate::settings::assets::start(
        &config.settings.host,
        assets_port,
        crate::settings::assets::AssetState {
            paths: paths.clone(),
            blobs: blobs.clone(),
        },
    )
    .await
    {
        tracing::warn!("plugin assets unavailable: {e}");
    }

    // The status-bar tray (headless environments continue without it).
    // Cloned before cmd_tx and store move into the tray and the context: the
    // update task outlives both and needs its own handles.
    let update_cmd_tx = cmd_tx.clone();
    let update_store = store.clone();
    let tray = tray::start(snap_rx, cmd_tx).await;
    match &tray {
        tray::Tray::Headless(reason) => tracing::warn!("no tray: {reason}"),
        tray::Tray::Running { .. } => tracing::info!("tray started"),
    }

    let mut ctx = Ctx {
        paths: paths.clone(),
        config,
        peer_id,
        listen: None,
        store,
        bans: load_bans(&paths),
        eng_cmd_tx,
        last_save_mtime: std::fs::metadata(&paths.config_file)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH),
        engine_drives: Vec::new(),
        drive_views: HashMap::new(),
        reactor_healthy: false,
        last_event: None,
        last_doc: None,
        pending_invites: HashMap::new(),
        stopping: false,
    };

    // The daemonized parent is polling for this file.
    let _ = std::fs::write(paths.run_dir.join("ready"), pid.to_string());
    tracing::info!(
        "ph-reactor {v} ready (settings {url}, peer {peer})",
        v = crate::VERSION,
        url = settings_url,
        peer = peer_id,
    );

    // Self-update.
    //
    // Deliberately NOT before the daemon reports ready. An update that runs on
    // the way up turns a bad release into a node that never starts and cannot
    // say why -- no console, no tray, no log line anyone will look for. Coming
    // up first means a failed upgrade leaves a working daemon to be told about.
    //
    // The first check is delayed because a node that has just started has not
    // synced yet: asking immediately would reliably find nothing and report
    // "up to date" on the strength of having looked too early.
    if ctx.config.update.check {
        let cmd_tx = update_cmd_tx;
        let store = update_store;
        let blobs = blobs.clone();
        let paths_for_update = paths.clone();
        let auto = ctx.config.update.auto;
        tokio::spawn(async move {
            let mut first = true;
            loop {
                tokio::time::sleep(if first {
                    std::time::Duration::from_secs(60)
                } else {
                    std::time::Duration::from_secs(6 * 60 * 60)
                })
                .await;
                first = false;
                crate::daemon::check_for_update(&store, &blobs, &paths_for_update, auto, &cmd_tx)
                    .await;
            }
        });
    }

    // Seed the engine with the drives already in the config (a restart
    // must not lose the user's drive list).
    for d in &ctx.config.drives {
        let addr = match Multiaddr::from_str(&d.addr) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("configured drive '{}' has a bad address: {e}", d.name);
                continue;
            }
        };
        ctx.engine_drives.push(d.name.clone());
        let _ = ctx.eng_cmd_tx.send(EngineCommand::AddDrive(Drive {
            name: d.name.clone(),
            addr,
            token_env: d.token_env.clone(),
            paused: d.paused,
            available_offline: d.available_offline,
        }));
    }
    let mut tick = interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing the SIGTERM handler")?;

    while !ctx.stopping {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("SIGINT received; shutting down");
                ctx.stopping = true;
                break;
            }
            _ = term.recv() => {
                tracing::info!("SIGTERM received; shutting down");
                ctx.stopping = true;
                break;
            }
            event = evt_rx.recv() => {
                match event {
                    Some(ev) => on_engine_event(&mut ctx, ev),
                    None => break,
                }
            }
            command = cmd_rx.recv() => {
                match command {
                    Some(cmd) => {
                        let label = format!("{cmd:?}");
                        match execute_command(&mut ctx, cmd) {
                            Ok(quit) => {
                                if quit {
                                    ctx.stopping = true;
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
        let snap = refresh_status(&ctx, &settings_url);
        let _ = snap_tx.send(snap);
    }

    // Graceful shutdown: stop the engine, then the services.
    let _ = ctx.eng_cmd_tx.send(EngineCommand::Shutdown);
    tray.stop();
    match tokio::time::timeout(Duration::from_secs(30), &mut engine_task).await {
        Ok(Ok(())) => tracing::info!("p2p engine stopped"),
        Ok(Err(err)) => tracing::warn!("engine task: {err}"),
        Err(_) => tracing::warn!("engine did not stop in time"),
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
    peer_id: PeerId,
    /// The engine's resolved listen address (set by its Identity event).
    listen: Option<Multiaddr>,
    store: Arc<Store>,
    /// The local ban list (base58 peer ids), mirrored from `bans.json`.
    bans: HashSet<String>,
    /// The engine's command channel.
    eng_cmd_tx: mpsc::UnboundedSender<EngineCommand>,
    /// Mtime of `config.json` as of the daemon's last write (and at
    /// startup). Anything newer is an external edit.
    last_save_mtime: SystemTime,
    /// Drives currently known to the engine (by name).
    engine_drives: Vec<String>,
    /// Last reported status per drive name (from the engine).
    drive_views: HashMap<String, (DriveStatus, Option<String>)>,
    /// The engine announced a working identity/listen.
    reactor_healthy: bool,
    /// Most recent one-line event (status page + tray).
    last_event: Option<String>,
    /// The most recently applied or changed document name.
    last_doc: Option<String>,
    /// Pending invite grants: the invite's nonce -> the groups a joiner is
    /// granted. Recorded when an invite is minted, applied (then removed)
    /// when its join-proof is accepted.
    pending_invites: HashMap<Vec<u8>, Vec<String>>,
    /// Set on shutdown signals / Quit; ends the loop.
    stopping: bool,
}

// ---------------------------------------------------------------------------
// Engine events
// ---------------------------------------------------------------------------

/// Folds one engine event into the daemon's view of the world.
fn on_engine_event(ctx: &mut Ctx, ev: EngineEvent) {
    match ev {
        EngineEvent::ChunkStored { hash } => {
            tracing::debug!("chunk {hash} stored");
        }
        EngineEvent::Identity { peer_id, listen } => {
            ctx.reactor_healthy = true;
            ctx.listen = Some(listen.clone());
            ctx.last_event = Some(format!("p2p listening on {listen}"));
            tracing::info!("engine: listening ({peer_id})");
        }
        EngineEvent::DriveStatus {
            name,
            status,
            detail,
        } => {
            ctx.drive_views
                .insert(name.clone(), (status, detail.clone()));
            ctx.last_event = Some(format!(
                "drive {name}: {}{}",
                status.as_str(),
                detail
                    .as_deref()
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default()
            ));
            tracing::debug!("engine: drive {name} -> {}", status.as_str());
        }
        EngineEvent::DocChanged { name } => {
            ctx.last_doc = name.clone();
            ctx.last_event = Some(match &name {
                Some(n) => format!("doc updated: {n}"),
                None => "remote doc change".into(),
            });
        }
        EngineEvent::DhtBootstrap(ok) => {
            ctx.last_event = Some(if ok {
                "dht: bootstrap complete".into()
            } else {
                "dht: bootstrap failed".into()
            });
            tracing::info!(ok, "engine: dht bootstrap");
        }
        EngineEvent::DhtProvider { key, peer } => {
            let _ = &key; // the key is the doc id; the daemon dials `peer` to fetch it
            ctx.last_event = Some(format!("dht: {peer} provides a doc"));
            tracing::info!(%peer, "engine: dht provider discovered");
        }
        EngineEvent::PeerConnected { peer } => {
            tracing::debug!(%peer, "engine: peer connected");
        }
        EngineEvent::DriveJoined { name, addr } => {
            if !ctx.config.drives.iter().any(|d| d.name == name) {
                ctx.config.drives.push(DriveConfig {
                    name: name.clone(),
                    addr: addr.to_string(),
                    token_env: None,
                    paused: false,
                    available_offline: true,
                });
                ctx.engine_drives.push(name.clone());
                if let Err(e) = persist_config(ctx) {
                    tracing::warn!("persisting joined drive '{name}' failed: {e:#}");
                } else {
                    ctx.last_event = Some(format!("peer {name} joined"));
                    tracing::info!("persisted a joined drive: {name}");
                }
            }
        }
        EngineEvent::InviteAccepted { peer, nonce } => {
            let joiner = peer.to_base58();
            let Some(groups) = ctx.pending_invites.remove(&nonce) else {
                ctx.last_event = Some(format!(
                    "a peer joined ({joiner}) but no invite grant was recorded for its nonce"
                ));
                tracing::info!(%peer, "invite accepted; no pending grant for its nonce");
                return;
            };
            let model = match crate::doc::ModelRef::parse("group@1") {
                Ok(m) => m,
                Err(e) => {
                    ctx.last_event = Some(format!("bad group model ref: {e}"));
                    return;
                }
            };
            let mut applied = 0usize;
            for g in &groups {
                match ctx.store.apply_local_action(
                    g,
                    &model,
                    "add-member",
                    &serde_json::json!({ "member": joiner.clone() }),
                ) {
                    Ok(_) => {
                        applied += 1;
                        tracing::info!(%peer, group = %g, "grant: added joiner to the group");
                    }
                    Err(e) => {
                        tracing::warn!(%peer, group = %g, "grant: could not add joiner to group: {e}");
                    }
                }
            }
            ctx.last_event = Some(format!(
                "granted {applied} group(s) to {joiner} on invite join"
            ));
        }
        EngineEvent::PeerAutoBanned { peer } => {
            if ctx.bans.insert(peer.clone()) {
                if let Err(e) = save_bans(&ctx.paths, &ctx.bans) {
                    tracing::warn!("persisting auto-ban for {peer} failed: {e:#}");
                } else {
                    ctx.last_event =
                        Some(format!("auto-banned peer {peer} (repeated auth failures)"));
                    tracing::info!("auto-banned {peer} and persisted it to the ban list");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Executes one user command. Returns `Ok(true)` when the command asks
/// the daemon to stop itself.
fn execute_command(ctx: &mut Ctx, cmd: Command) -> Result<bool> {
    let pause_resume = matches!(&cmd, Command::PauseDrive { .. });
    match cmd {
        Command::AddDrive {
            name,
            addr,
            token_env,
            offline,
        } => {
            let addr = addr.trim().to_string();
            let addr_ma = Multiaddr::from_str(&addr)
                .with_context(|| format!("'{}' is not a valid multiaddr", addr))?;
            let name = if name.trim().is_empty() {
                default_drive_name(&addr_ma)
            } else {
                name.trim().to_string()
            };
            if ctx
                .config
                .drives
                .iter()
                .any(|d| d.name.eq_ignore_ascii_case(&name))
            {
                bail!("a drive named '{name}' is already configured");
            }
            let drive = Drive {
                name: name.clone(),
                addr: addr_ma.clone(),
                token_env,
                paused: false,
                available_offline: offline,
            };
            ctx.config.drives.push(DriveConfig {
                name: name.clone(),
                addr: addr_ma.to_string(),
                token_env: drive.token_env.clone(),
                paused: false,
                available_offline: offline,
            });
            ctx.engine_drives.push(name.clone());
            persist_config(ctx)?;
            let _ = ctx.eng_cmd_tx.send(EngineCommand::AddDrive(drive));
            ctx.last_event = Some(format!("added drive '{name}'"));
            tracing::info!("added drive '{name}' ({addr})");
        }
        Command::RemoveDrive { name } => {
            let Some(idx) = ctx
                .config
                .drives
                .iter()
                .position(|d| d.name.eq_ignore_ascii_case(&name))
            else {
                bail!("drive '{name}' is not configured");
            };
            ctx.config.drives.remove(idx);
            let gone = ctx.engine_drives.clone();
            ctx.engine_drives.retain(|n| !n.eq_ignore_ascii_case(&name));
            for n in gone.iter().filter(|n| n.eq_ignore_ascii_case(&name)) {
                let _ = ctx
                    .eng_cmd_tx
                    .send(EngineCommand::RemoveDrive { name: n.clone() });
            }
            ctx.drive_views.remove(&name);
            persist_config(ctx)?;
            ctx.last_event = Some(format!("removed drive '{name}'"));
            tracing::info!("removed drive '{name}'");
        }
        Command::PauseDrive { name } | Command::ResumeDrive { name } => {
            let paused = pause_resume;
            let Some(d) = ctx
                .config
                .drives
                .iter_mut()
                .find(|d| d.name.eq_ignore_ascii_case(&name))
            else {
                bail!("drive '{name}' is not configured");
            };
            d.paused = paused;
            persist_config(ctx)?;
            let _ = ctx.eng_cmd_tx.send(EngineCommand::SetPaused {
                name: name.clone(),
                paused,
            });
            ctx.last_event = Some(format!(
                "{} drive '{name}'",
                if paused { "paused" } else { "resumed" }
            ));
        }
        Command::ResyncDrive { name } => {
            if !ctx
                .config
                .drives
                .iter()
                .any(|d| d.name.eq_ignore_ascii_case(&name))
            {
                bail!("drive '{name}' is not configured");
            }
            let _ = ctx
                .eng_cmd_tx
                .send(EngineCommand::Resync { name: name.clone() });
            ctx.last_event = Some(format!("re-syncing drive '{name}'"));
        }
        Command::FetchBlob { blob } => {
            let _ = ctx
                .eng_cmd_tx
                .send(crate::p2p::EngineCommand::FetchBlob { blob });
        }
        Command::SetConfig { key, value } => {
            let before = ctx.config.process_fingerprint();
            config::set(&mut ctx.config, &key, &value)?;
            persist_config(ctx)?;
            if ctx.config.process_fingerprint() != before {
                ctx.last_event = Some(format!("config: set {key} (restart the daemon to apply)"));
                tracing::info!("config: set {key} — a restart is needed to apply it");
            } else {
                ctx.last_event = Some(format!("config: set {key}"));
            }
        }
        Command::CreateDoc {
            name,
            fields,
            reply,
        } => match ctx.store.create_doc(&name, fields) {
            Ok(id) => {
                ctx.last_doc = Some(name.clone());
                tracing::info!("created doc '{name}' ({id})");
                ctx.last_event = Some(format!("created doc '{name}'"));
                let _ = reply.send(Ok(()));
            }
            Err(e) => {
                tracing::warn!("doc create '{name}' failed: {e}");
                let _ = reply.send(Err(e));
            }
        },
        Command::CreateAction {
            name,
            model,
            kind,
            payload,
            reply,
        } => match crate::doc::ModelRef::parse(&model)
            .and_then(|mr| ctx.store.apply_local_action(&name, &mr, &kind, &payload))
        {
            Ok(action) => {
                ctx.last_doc = Some(name.clone());
                tracing::info!("applied {kind} action to '{name}'");
                ctx.last_event = Some(format!("applied {kind} to '{name}'"));
                let _ = reply.send(Ok(action));
            }
            Err(e) => {
                tracing::warn!("model action '{kind}' on '{name}' failed: {e}");
                let _ = reply.send(Err(e));
            }
        },
        Command::Propose {
            name,
            model,
            kind,
            payload,
            reply,
        } => {
            let out = crate::doc::ModelRef::parse(&model)
                .and_then(|mr| ctx.store.build_local_action(&name, &mr, &kind, &payload))
                .and_then(|action| crate::p2p::cosign::encode(&action));
            match &out {
                Ok(_) => tracing::info!("proposed {kind} on '{name}' (awaiting co-signatures)"),
                Err(e) => tracing::warn!("propose {kind} on '{name}' failed: {e}"),
            }
            let _ = reply.send(out);
        }
        Command::CoSign { token, reply } => {
            let store = ctx.store.clone();
            let key_of = |o: &str| {
                store
                    .peer_key(o)
                    .and_then(|b| ed25519_dalek::VerifyingKey::from_bytes(&b).ok())
            };
            let out = crate::p2p::cosign::decode(&token, key_of).and_then(|mut action| {
                let me = ctx.peer_id.to_base58();
                crate::p2p::cosign::add_cosig(&mut action, &me, &ctx.store.key())?;
                crate::p2p::cosign::encode(&action)
            });
            match &out {
                Ok(_) => tracing::info!("co-signed a proposal"),
                Err(e) => tracing::warn!("co-sign failed: {e}"),
            }
            let _ = reply.send(out);
        }
        Command::Submit { token, reply } => {
            let store = ctx.store.clone();
            let key_of = |o: &str| {
                store
                    .peer_key(o)
                    .and_then(|b| ed25519_dalek::VerifyingKey::from_bytes(&b).ok())
            };
            let out = crate::p2p::cosign::decode(&token, key_of).and_then(|action| {
                let kind = action.kind.clone();
                ctx.store.apply_signed_action(&action)?;
                Ok(kind)
            });
            match &out {
                Ok(kind) => {
                    tracing::info!("submitted co-signed {kind} action");
                    ctx.last_event = Some(format!("applied co-signed {kind}"));
                }
                Err(e) => tracing::warn!("submit failed: {e}"),
            }
            let _ = reply.send(out.map(|k| format!("applied co-signed {k}")));
        }
        Command::CreateDocModel {
            name,
            model,
            payload,
            space,
            reply,
        } => match crate::doc::ModelRef::parse(&model).and_then(|mr| {
            let space_id = match &space {
                Some(s) => match ctx.store.get(s) {
                    Some(d) => Some(d.id),
                    None => return Err(format!("no space named '{s}'")),
                },
                None => None,
            };
            ctx.store
                .create_doc_in_space(&name, &mr, &payload, space_id)
                .map(|_| ())
        }) {
            Ok(()) => {
                ctx.last_doc = Some(name.clone());
                ctx.last_event = Some(format!("created {name} ({model})"));
                tracing::info!("created {name} under {model}");
                let _ = reply.send(Ok(()));
            }
            Err(e) => {
                tracing::warn!("model doc create '{name}' failed: {e}");
                let _ = reply.send(Err(e));
            }
        },
        Command::CreateSpace {
            name,
            visibility,
            members,
            managers,
            reply,
        } => {
            let payload = serde_json::json!({
                "name": name,
                "visibility": visibility,
                "members": members,
                "managers": managers,
            });
            match ctx.store.create_space(&name, &payload) {
                Ok(_) => {
                    ctx.last_event = Some(format!("created space {name} ({visibility})"));
                    tracing::info!("created space {name} ({visibility})");
                    let _ = reply.send(Ok(()));
                }
                Err(e) => {
                    tracing::warn!("space create '{name}' failed: {e}");
                    let _ = reply.send(Err(e));
                }
            }
        }
        Command::Invite { groups, reply } => {
            // What the JOINER will dial. A configured external address wins
            // over the bind address: behind a load balancer or reverse proxy
            // the swarm reports local addresses (including loopback), and an
            // invite carrying those tells the joiner to dial its own machine.
            let listen_str = ctx.listen.as_ref().map(|l| l.to_string());
            let listen = match crate::p2p::invite::pick_invite_addr(
                &ctx.config.instance.external,
                listen_str.as_deref(),
            ) {
                Some(a) => a,
                None => {
                    let _ = reply.send(Err(
                        "the reactor has not announced its listen address yet; retry".to_string(),
                    ));
                    return Ok(false);
                }
            };
            if crate::p2p::invite::is_undialable_by_peers(&listen) {
                // Legitimate when both reactors share a machine, so this
                // warns rather than refusing -- but it is the likeliest
                // explanation for an invite that "just doesn't connect".
                tracing::warn!(
                    "invite carries {listen}, which a remote peer cannot dial;                      set instance.external if this node is behind a proxy or load balancer"
                );
            }
            let groups = if groups.is_empty() {
                vec![ctx.config.instance.name.clone()]
            } else {
                groups
            };
            let grant_groups = groups.clone();
            let signing = ctx.store.key();
            match crate::p2p::invite::InviteToken::make(
                &ctx.config.instance.name,
                &ctx.peer_id.to_base58(),
                &signing,
                &listen,
                groups,
            ) {
                Ok(token) => match token.encode() {
                    Ok(s) => {
                        if ctx.pending_invites.len() > 256 {
                            ctx.pending_invites.clear();
                        }
                        ctx.pending_invites
                            .insert(token.nonce.clone(), grant_groups);
                        ctx.last_event = Some("generated an invite".into());
                        let _ = reply.send(Ok(s));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                },
                Err(e) => {
                    let _ = reply.send(Err(e));
                }
            }
        }
        Command::Join { invite, reply } => {
            let token = match crate::p2p::invite::InviteToken::decode(&invite) {
                Ok(t) => t,
                Err(e) => {
                    let _ = reply.send(Err(e));
                    return Ok(false);
                }
            };
            let mut key_arr = [0u8; 32];
            key_arr.copy_from_slice(&token.pubkey);
            if let Err(e) = ctx.store.register_peer_key(&token.peer_id, key_arr) {
                let _ = reply.send(Err(e));
                return Ok(false);
            }
            let signing = ctx.store.key();
            let accept = crate::p2p::invite::InviteAccept::make(
                &token.nonce,
                &ctx.config.instance.name,
                &ctx.peer_id.to_base58(),
                &signing,
            );
            let peer = match PeerId::from_str(&token.peer_id) {
                Ok(p) => p,
                Err(e) => {
                    let _ = reply.send(Err(format!("bad inviter peer id: {e}")));
                    return Ok(false);
                }
            };
            let addr = match Multiaddr::from_str(&token.addr) {
                Ok(a) => a.with(libp2p::multiaddr::Protocol::P2p(peer)),
                Err(e) => {
                    let _ = reply.send(Err(e.to_string()));
                    return Ok(false);
                }
            };
            let name = token.name;
            if ctx
                .config
                .drives
                .iter()
                .any(|d| d.name.eq_ignore_ascii_case(&name))
            {
                let _ = reply.send(Err(format!("a drive named '{name}' is already configured")));
                return Ok(false);
            }
            ctx.config.drives.push(DriveConfig {
                name: name.clone(),
                addr: addr.to_string(),
                token_env: None,
                paused: false,
                available_offline: true,
            });
            ctx.engine_drives.push(name.clone());
            if let Err(e) = persist_config(ctx) {
                let _ = reply.send(Err(e.to_string()));
                return Ok(false);
            }
            let drive = Drive {
                name,
                addr,
                token_env: None,
                paused: false,
                available_offline: true,
            };
            match ctx.eng_cmd_tx.send(EngineCommand::Join { drive, accept }) {
                Ok(()) => {
                    ctx.last_event = Some("joined a vault via invite".into());
                    tracing::info!("joined a vault via invite");
                    let _ = reply.send(Ok(()));
                }
                Err(e) => {
                    let _ = reply.send(Err(e.to_string()));
                }
            }
        }
        Command::Ban { peer, reply } => {
            let pid = match PeerId::from_str(&peer) {
                Ok(p) => p,
                Err(e) => {
                    let _ = reply.send(Err(format!("'{peer}' is not a valid peer id: {e}")));
                    return Ok(false);
                }
            };
            ctx.bans.insert(peer);
            if let Err(e) = save_bans(&ctx.paths, &ctx.bans) {
                let _ = reply.send(Err(format!("saving the ban list failed: {e}")));
                return Ok(false);
            }
            if let Err(e) = ctx.eng_cmd_tx.send(EngineCommand::Ban { peer: pid }) {
                let _ = reply.send(Err(e.to_string()));
                return Ok(false);
            }
            ctx.last_event = Some("banned a peer".into());
            let _ = reply.send(Ok(()));
        }
        Command::Unban { peer, reply } => {
            let pid = match PeerId::from_str(&peer) {
                Ok(p) => p,
                Err(e) => {
                    let _ = reply.send(Err(format!("'{peer}' is not a valid peer id: {e}")));
                    return Ok(false);
                }
            };
            ctx.bans.remove(&peer);
            if let Err(e) = save_bans(&ctx.paths, &ctx.bans) {
                let _ = reply.send(Err(format!("saving the ban list failed: {e}")));
                return Ok(false);
            }
            if let Err(e) = ctx.eng_cmd_tx.send(EngineCommand::Unban { peer: pid }) {
                let _ = reply.send(Err(e.to_string()));
                return Ok(false);
            }
            let _ = reply.send(Ok(()));
        }
        Command::Quit => {
            tracing::info!("quit requested");
            ctx.stopping = true;
            return Ok(true);
        }
    }
    Ok(false)
}

/// A default name for an unnamed drive: the peer id when the multiaddr
/// carries one, else the port.
fn default_drive_name(addr: &Multiaddr) -> String {
    for p in addr.iter() {
        if let libp2p::multiaddr::Protocol::P2p(pid) = p {
            let b58 = pid.to_base58();
            return format!("peer-{}", &b58[..8.min(b58.len())]);
        }
    }
    for p in addr.iter() {
        if let libp2p::multiaddr::Protocol::Tcp(port) = p {
            return format!("drive-{port}");
        }
    }
    "drive".into()
}

/// Saves the daemon config. External edits are adopted first so they
/// are not clobbered.
fn persist_config(ctx: &mut Ctx) -> Result<()> {
    adopt_external(ctx, true);
    config::save(&ctx.paths, &ctx.config)?;
    ctx.last_save_mtime = std::fs::metadata(&ctx.paths.config_file)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    Ok(())
}

// ---------------------------------------------------------------------------
// External config edits
// ---------------------------------------------------------------------------

/// Picks up out-of-band edits to `config.json` (e.g. a separate
/// `ph-reactor config set` from another terminal): adopts the file and
/// syncs the engine's drive set to the new config.
///
/// With `preserve_drives` the daemon keeps its in-memory drive list
/// (used right before persisting: the running command owns the drives,
/// and only the other fields are adopted, so a concurrent edit is not
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
    let restart_needed = reloaded.process_fingerprint() != ctx.config.process_fingerprint();
    if preserve_drives {
        reloaded.drives = std::mem::take(&mut ctx.config.drives);
    }
    ctx.config = reloaded;
    ctx.last_save_mtime = mtime;
    if restart_needed {
        tracing::info!("config changed; restart ph-reactor to apply it");
        ctx.last_event = Some("config changed (restart to apply)".into());
    } else {
        tracing::debug!("config reloaded");
    }
    // Sync the engine's drive set with the new config.
    let wanted: Vec<Drive> = ctx
        .config
        .drives
        .iter()
        .filter_map(|d| {
            let addr = Multiaddr::from_str(&d.addr).ok()?;
            Some(Drive {
                name: d.name.clone(),
                addr,
                token_env: d.token_env.clone(),
                paused: d.paused,
                available_offline: d.available_offline,
            })
        })
        .collect();
    let wanted_names: Vec<String> = wanted.iter().map(|d| d.name.clone()).collect();
    for d in wanted {
        if !ctx.engine_drives.contains(&d.name) {
            ctx.engine_drives.push(d.name.clone());
            let _ = ctx.eng_cmd_tx.send(EngineCommand::AddDrive(d));
        }
    }
    let stale = ctx.engine_drives.clone();
    ctx.engine_drives
        .retain(|n| wanted_names.iter().any(|w| w == n));
    for name in stale
        .into_iter()
        .filter(|n| !wanted_names.iter().any(|w| w == n))
    {
        ctx.drive_views.remove(&name);
        let _ = ctx
            .eng_cmd_tx
            .send(EngineCommand::RemoveDrive { name: name.clone() });
    }
    // paused flag drift
    for d in &ctx.config.drives {
        let _ = ctx.eng_cmd_tx.send(EngineCommand::SetPaused {
            name: d.name.clone(),
            paused: d.paused,
        });
    }
}

// ---------------------------------------------------------------------------
// Status refresh
// ---------------------------------------------------------------------------

/// Builds the next snapshot: engine view + one entry per configured
/// drive.
fn refresh_status(ctx: &Ctx, settings_url: &str) -> StatusSnapshot {
    let mut drives = Vec::with_capacity(ctx.config.drives.len());
    for d in &ctx.config.drives {
        let (status, detail) = ctx
            .drive_views
            .get(&d.name)
            .cloned()
            .unwrap_or((DriveStatus::Connecting, None));
        let status = if d.paused && status != DriveStatus::Paused {
            DriveStatus::Paused
        } else {
            status
        };
        drives.push(DriveStatusEntry {
            name: d.name.clone(),
            addr: d.addr.clone(),
            paused: d.paused,
            status: status.as_str().to_string(),
            detail: detail.unwrap_or_default(),
        });
    }
    let active_drives = drives
        .iter()
        .filter(|d| d.status == "synced" || d.status == "connecting")
        .count();
    let reactor = ReactorStatus {
        running: true,
        healthy: ctx.reactor_healthy,
        peer_id: Some(ctx.peer_id.to_base58()),
        listen: ctx.config.instance.listen.clone(),
        docs: ctx.store.live_doc_count() as u64,
        active_drives,
        last_doc: ctx.last_doc.clone(),
        last_event: ctx.last_event.clone(),
    };
    StatusSnapshot {
        version: crate::VERSION.to_string(),
        reactor,
        drives,
        bans: ctx.bans.iter().cloned().collect(),
        settings: status::SettingsStatus {
            url: settings_url.into(),
        },
        groups: group_summaries(&ctx.store),
        updated_at: status::rfc3339_now(),
    }
}

/// Summarises the group documents for the tray.
///
/// Reuses the same query the console's `/api/groups` runs, against the
/// in-memory store, so the tray never has to call the settings server.
fn group_summaries(store: &Store) -> Vec<status::GroupSummary> {
    crate::query::query_docs(store, "group", None)
        .into_iter()
        .map(|d| {
            let fields = d.get("fields");
            let len = |key: &str| {
                fields
                    .and_then(|f| f.get(key))
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0)
            };
            status::GroupSummary {
                name: d
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                members: len("members"),
                // msg_text is the append-only message column; its length is
                // the message count.
                messages: len("msg_text"),
            }
        })
        .collect()
}

/// A snapshot built from the config file when the daemon is not
/// running.
fn degraded_snapshot(config: &ReactorConfig, settings_url: &str) -> StatusSnapshot {
    let drives = config
        .drives
        .iter()
        .map(|d| DriveStatusEntry {
            name: d.name.clone(),
            addr: d.addr.clone(),
            paused: d.paused,
            status: if d.paused { "paused" } else { "offline" }.into(),
            detail: "daemon not running".into(),
        })
        .collect();
    let mut snap = StatusSnapshot::empty(
        crate::VERSION.to_string(),
        config.instance.listen.clone(),
        settings_url.into(),
    );
    snap.reactor.running = false;
    snap.reactor.last_event = Some("daemon not running".into());
    snap.drives = drives;
    snap
}

// ---------------------------------------------------------------------------
// Daemonization + single-instance lock
// ---------------------------------------------------------------------------

/// Redirects the daemon child's stdio: stdin/stdout to /dev/null, stderr
/// to `stderr_path` (appended), so panics and fatal errors stay readable.
fn detach_stdio(stderr_path: &Path) {
    let null_path = b"/dev/null\0";
    let devnull = unsafe { libc::open(null_path.as_ptr() as *const i8, libc::O_RDWR) };
    if devnull < 0 {
        return;
    }
    let mut null_terminated = stderr_path.to_string_lossy().into_owned().into_bytes();
    null_terminated.push(0);
    let stderr = unsafe {
        libc::open(
            null_terminated.as_ptr() as *const i8,
            libc::O_RDWR | libc::O_CREAT | libc::O_APPEND,
            0o644,
        )
    };
    if stderr >= 0 {
        unsafe {
            libc::dup2(stderr, 2);
            libc::close(stderr);
        }
    }
    for fd in [0, 1] {
        unsafe {
            libc::dup2(devnull, fd);
        }
    }
    unsafe {
        libc::close(devnull);
    }
}

/// Prints the tail of the reactor log and the child's stderr log when a
/// daemonized start fails, so the failure is not lost in the fork.
fn print_startup_failure(paths: &StatePaths, message: &str) {
    eprintln!("{message}");
    if let Ok(tail) = crate::logrotate::tail(&paths.reactor_log(), Some(2048)) {
        eprintln!("-- last reactor log:");
        print!("{tail}");
    }
    let stderr = paths.logs_dir.join("stderr.log");
    if stderr.is_file() {
        if let Ok(tail) = crate::logrotate::tail(&stderr, Some(2048)) {
            eprintln!("-- last child stderr:");
            print!("{tail}");
        }
    }
}

fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Non-blocking `waitpid` for a child we forked: `Some(status)` with a
/// human-readable exit status once the child is reaped, `None` while it
/// still runs. Unlike kill(pid, 0), this distinguishes a dead child from
/// a live one (a zombie still answers kill 0).
fn reap_child(pid: i32) -> Option<String> {
    let mut status: libc::c_int = 0;
    if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } != pid {
        return None;
    }
    if libc::WIFEXITED(status) {
        Some(format!("exit code {}", libc::WEXITSTATUS(status)))
    } else if libc::WIFSIGNALED(status) {
        Some(format!("killed by signal {}", libc::WTERMSIG(status)))
    } else {
        Some("unknown".to_string())
    }
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

/// Fails fast when the configured TCP listen port is taken by another
/// process (a better error than a bind failure deep in the stack).
fn check_listen_port(addr: &Multiaddr) -> Result<()> {
    let mut ip: Option<std::net::IpAddr> = None;
    let mut port: Option<u16> = None;
    for p in addr.iter() {
        match p {
            libp2p::multiaddr::Protocol::Ip4(i) => ip = Some(i.into()),
            libp2p::multiaddr::Protocol::Ip6(i) => ip = Some(i.into()),
            libp2p::multiaddr::Protocol::Tcp(n) => port = Some(n),
            _ => {}
        }
    }
    let (Some(ip), Some(port)) = (ip, port) else {
        return Ok(()); // non-TCP address: the engine reports bind errors
    };
    match std::net::TcpListener::bind((ip, port)) {
        Ok(_) => Ok(()),
        Err(e) => {
            bail!(
                "listen port {port} is already in use: {e} (change instance.listen in the config)"
            )
        }
    }
}

/// Fails fast when the configured settings port is already taken by
/// another process (the common case: two daemons defaulting to 4002).
fn check_settings_port(host: &str, port: u16) -> Result<()> {
    match std::net::TcpListener::bind((host, port)) {
        Ok(_) => Ok(()),
        Err(e) => {
            bail!(
                "settings port {port} on {host} is already in use: {e} (change settings.port in the config)"
            )
        }
    }
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
    let snap = live.unwrap_or_else(|| {
        degraded_snapshot(
            &config,
            &format!("http://{}:{}", config.settings.host, config.settings.port),
        )
    });
    if json {
        println!("{}", serde_json::to_string(&snap)?);
    } else {
        print!("{}", status::render_text(&snap));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// drive subcommand
// ---------------------------------------------------------------------------

/// The `drive` CLI: goes through the running daemon (single writer)
/// when one is up, or edits the config directly (the daemon picks it
/// up on its next poll).
pub async fn drive_command(state_dir: Option<&Path>, cmd: crate::cli::DriveCommand) -> Result<()> {
    use crate::cli::DriveCommand;
    let paths = StatePaths::resolve(state_dir);
    paths.ensure_dirs()?;
    let mut config = config::load(&paths)?;

    match cmd {
        DriveCommand::List => {
            let settings_url = format!("http://{}:{}", config.settings.host, config.settings.port);
            let snap = live_drive_views(&config, &settings_url).await;
            if snap.is_empty() {
                println!("no drives configured (add one: ph-reactor drive add <multiaddr>)");
                return Ok(());
            }
            for (i, d) in snap.iter().enumerate() {
                println!("{:>2}  {:<24} [{:<14}] {}", i + 1, d.name, d.status, d.addr);
                if !d.detail.is_empty() {
                    println!("    {}", d.detail);
                }
            }
            Ok(())
        }
        DriveCommand::Add {
            addr,
            name,
            token_env,
            offline,
        } => {
            let op = DriveOp::Add {
                name: name.unwrap_or_default(),
                addr,
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
    let drive = config
        .drives
        .iter()
        .find(|d| d.name.eq_ignore_ascii_case(target))
        .with_context(|| {
            format!("drive '{target}' not configured (see `ph-reactor drive list`)")
        })?;
    Ok(drive.name.clone())
}

/// Routes the operation through the daemon's settings API when the
/// daemon is running (single writer); otherwise applies it to the
/// config file directly (the daemon adopts it on its next poll).
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
                eprintln!("daemon is not reachable; updating the config directly ({err})");
                direct_drive_op(paths, config, op)
            }
        }
    } else {
        direct_drive_op(paths, config, op)
    }
}

/// The `doc` CLI. `list`/`get` read the store directly (safe with or
/// without a running daemon — reads never write); `add` must go
/// through the running daemon (the single writer) so the new doc is
/// published to the sync mesh as it is created.
pub async fn doc_command(state_dir: Option<&Path>, cmd: crate::cli::DocCommand) -> Result<()> {
    use crate::cli::DocCommand;
    let paths = StatePaths::resolve(state_dir);
    paths.ensure_dirs()?;
    match cmd {
        DocCommand::List => {
            let store = open_local_store(&paths)?;
            let docs = store.list();
            if docs.is_empty() {
                println!("no local docs (add one: ph-reactor doc add <name> [--field k=v])");
                return Ok(());
            }
            for d in docs {
                let live = d.fields.values().filter(|f| !f.deleted).count();
                println!("{:<32} {} ({} fields)", d.name, d.id, live);
            }
            Ok(())
        }
        DocCommand::Get { name } => {
            let store = open_local_store(&paths)?;
            let Some(doc) = store.get(&name) else {
                bail!("no such doc: '{name}'");
            };
            let mut out = serde_json::Map::new();
            for (k, f) in &doc.fields {
                out.insert(
                    k.clone(),
                    if f.deleted {
                        serde_json::Value::Null
                    } else {
                        f.value.clone()
                    },
                );
            }
            println!("{}", serde_json::to_string_pretty(&out)?);
            Ok(())
        }
        DocCommand::Add { name, fields } => {
            if !daemon_is_running(&paths.daemon_pidfile()) {
                bail!("the daemon is not running — start it (ph-reactor) and retry: docs are created through the daemon so they reach the sync mesh");
            }
            let config = config::load(&paths)?;
            let mut map = BTreeMap::new();
            for kv in &fields {
                let (k, v) = kv
                    .split_once('=')
                    .with_context(|| format!("--field expects KEY=VALUE (got '{kv}')"))?;
                let k = k.trim();
                let v = v.trim();
                if k.is_empty() {
                    bail!("--field name may not be empty");
                }
                let value: serde_json::Value = match serde_json::from_str(v) {
                    Ok(val) => val,
                    Err(_) => serde_json::Value::String(v.to_string()),
                };
                map.insert(k.to_string(), value);
            }
            let url = format!(
                "http://{}:{}/api/docs",
                config.settings.host, config.settings.port
            );
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()?;
            let body = serde_json::json!({ "name": name, "fields": map });
            let r = client.post(&url).json(&body).send().await?;
            let status = r.status();
            if status.is_success() {
                println!("created doc '{name}'");
                Ok(())
            } else {
                let text = r.text().await.unwrap_or_default();
                bail!("daemon rejected the doc: {status} {text}")
            }
        }
        DocCommand::Verify { name } => {
            let store = open_local_store(&paths)?;
            let report = store.verify(&name).map_err(anyhow::Error::msg)?;
            print!("{}", report.render());
            if report.ok {
                Ok(())
            } else {
                bail!("verification failed for '{name}'")
            }
        }
        DocCommand::Action {
            name,
            kind,
            payload,
            model,
        } => {
            if !daemon_is_running(&paths.daemon_pidfile()) {
                bail!("the daemon is not running — start it (ph-reactor) and retry: model actions are applied through the daemon so they reach the sync mesh");
            }
            let payload_val: serde_json::Value =
                serde_json::from_str(&payload).with_context(|| "the --payload must be JSON")?;
            let config = config::load(&paths)?;
            let url = format!(
                "http://{}:{}/api/docs/action",
                config.settings.host, config.settings.port
            );
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()?;
            let body = serde_json::json!({
                "name": name, "kind": kind, "payload": payload_val, "model": model
            });
            let r = client.post(&url).json(&body).send().await?;
            let status = r.status();
            if status.is_success() {
                println!("applied {kind} to '{name}'");
                Ok(())
            } else {
                let text = r.text().await.unwrap_or_default();
                bail!("daemon rejected the action: {status} {text}")
            }
        }
    }
}

/// The `invite` CLI: asks the running daemon to generate a signed invite
/// (its live identity + a challenge) for a peer to join this vault, and
/// prints the string.
pub async fn invite_command(state_dir: Option<&Path>, groups: Vec<String>) -> Result<()> {
    let paths = StatePaths::resolve(state_dir);
    paths.ensure_dirs()?;
    if !daemon_is_running(&paths.daemon_pidfile()) {
        bail!("the daemon is not running — start it (ph-reactor) and retry: invites are generated by the daemon from its live identity");
    }
    let config = config::load(&paths)?;
    let url = format!(
        "http://{}:{}/api/invite",
        config.settings.host, config.settings.port
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let body = serde_json::json!({ "groups": groups });
    let r = client.post(&url).json(&body).send().await?;
    let status = r.status();
    if !status.is_success() {
        let text = r.text().await.unwrap_or_default();
        bail!("invite failed: {status} {text}");
    }
    let v: serde_json::Value = r.json().await?;
    let invite = v
        .get("invite")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();
    println!("{invite}");
    Ok(())
}

/// The `join` CLI: consumes an invite string through the running daemon —
/// it pins the inviter (TOFU) and adds a drive with a signed join-proof.
/// Posts to one of the co-signing endpoints and prints the returned token.
async fn cosign_endpoint(
    state_dir: Option<&Path>,
    path: &str,
    body: serde_json::Value,
    what: &str,
) -> Result<()> {
    let paths = StatePaths::resolve(state_dir);
    paths.ensure_dirs()?;
    if !daemon_is_running(&paths.daemon_pidfile()) {
        bail!("the daemon is not running — start it (ph-reactor) and retry: {what} needs the daemon's live identity and store");
    }
    let config = config::load(&paths)?;
    let url = format!(
        "http://{}:{}{path}",
        config.settings.host, config.settings.port
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let r = client.post(&url).json(&body).send().await?;
    let status = r.status();
    if !status.is_success() {
        let text = r.text().await.unwrap_or_default();
        bail!("{what} failed: {text}");
    }
    let v: serde_json::Value = r.json().await?;
    println!(
        "{}",
        v.get("token").and_then(|s| s.as_str()).unwrap_or_default()
    );
    Ok(())
}

/// `ph-reactor propose <group> <kind> --payload <json>`
pub async fn propose_command(
    state_dir: Option<&Path>,
    group: String,
    kind: String,
    payload: String,
) -> Result<()> {
    let payload: serde_json::Value = serde_json::from_str(&payload)
        .map_err(|e| anyhow::anyhow!("--payload is not valid JSON: {e}"))?;
    cosign_endpoint(
        state_dir,
        "/api/propose",
        serde_json::json!({ "group": group, "kind": kind, "payload": payload }),
        "propose",
    )
    .await
}

/// `ph-reactor cosign <token>`
pub async fn cosign_command(state_dir: Option<&Path>, token: String) -> Result<()> {
    cosign_endpoint(
        state_dir,
        "/api/cosign",
        serde_json::json!({ "token": token }),
        "cosign",
    )
    .await
}

/// `ph-reactor submit <token>`
pub async fn submit_command(state_dir: Option<&Path>, token: String) -> Result<()> {
    cosign_endpoint(
        state_dir,
        "/api/submit",
        serde_json::json!({ "token": token }),
        "submit",
    )
    .await
}

pub async fn join_command(state_dir: Option<&Path>, invite: String) -> Result<()> {
    let paths = StatePaths::resolve(state_dir);
    paths.ensure_dirs()?;
    if !daemon_is_running(&paths.daemon_pidfile()) {
        bail!("the daemon is not running — start it (ph-reactor) and retry: joins add a drive through the daemon so the engine dials the inviter");
    }
    let config = config::load(&paths)?;
    let url = format!(
        "http://{}:{}/api/join",
        config.settings.host, config.settings.port
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let body = serde_json::json!({ "invite": invite });
    let r = client.post(&url).json(&body).send().await?;
    let status = r.status();
    if !status.is_success() {
        let text = r.text().await.unwrap_or_default();
        bail!("join failed: {status} {text}");
    }
    println!(
        "joined: the inviter is pinned and a drive was added (it syncs once the connection is up)"
    );
    Ok(())
}

/// The `ban` / `unban` CLI: asks the running daemon to add to (or remove
/// from) the local ban list. A banned peer's handshakes are refused, so it
/// can no longer sync with this vault.
pub async fn ban_command(state_dir: Option<&Path>, peer: String, unban: bool) -> Result<()> {
    let paths = StatePaths::resolve(state_dir);
    paths.ensure_dirs()?;
    if !daemon_is_running(&paths.daemon_pidfile()) {
        bail!("the daemon is not running — start it (ph-reactor) and retry: ban lists are enforced by the daemon's engine");
    }
    let config = config::load(&paths)?;
    let endpoint = if unban { "api/unban" } else { "api/ban" };
    let url = format!(
        "http://{}:{}/{}",
        config.settings.host, config.settings.port, endpoint
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let body = serde_json::json!({ "peer": peer });
    let r = client.post(&url).json(&body).send().await?;
    let status = r.status();
    if !status.is_success() {
        let text = r.text().await.unwrap_or_default();
        bail!(
            "{} failed: {status} {text}",
            if unban { "unban" } else { "ban" }
        );
    }
    if unban {
        println!("unbanned {peer}");
    } else {
        println!("banned {peer} (its handshakes will be refused)");
    }
    Ok(())
}

/// The `query` CLI: a read-model projection over the store's documents.
/// Works with or without a running daemon — it opens the store read-only
/// (the store is the source of truth for every doc the vault has synced).
pub async fn query_command(
    state_dir: Option<&Path>,
    model: &str,
    filter: Option<&str>,
) -> Result<()> {
    let paths = StatePaths::resolve(state_dir);
    paths.ensure_dirs()?;
    let store = open_local_store(&paths)?;
    let filter = filter.and_then(parse_filter);
    let docs = query_docs(&store, model, filter.as_ref());
    let rendered = serde_json::to_string_pretty(&serde_json::Value::Array(docs))
        .unwrap_or_else(|_| "[]".into());
    println!("{rendered}");
    Ok(())
}
/// Opens the local store from disk for read-only CLI use. The daemon
/// may be running concurrently; this instance only reads, so the two
/// views never contend on the files.
fn open_local_store(paths: &StatePaths) -> Result<Arc<Store>> {
    let kp = p2p::load_or_create_identity(&paths.key_file())
        .map_err(anyhow::Error::msg)
        .with_context(|| {
            format!(
                "loading the identity key from {}",
                paths.key_file().display()
            )
        })?;
    let signing = p2p::signing_key(&kp).map_err(anyhow::Error::msg)?;
    let origin = p2p::peer_id_of(&kp).to_base58();
    let mut seed: Vec<Arc<dyn crate::model::Model>> = Vec::new();
    for m in crate::model::realistic::realistic_models() {
        seed.push(Arc::new(m));
    }
    for def in crate::model::persist::load_all(&paths.models_file()) {
        if let Ok(m) = crate::model::l1::L1::from_def(def) {
            seed.push(Arc::new(m));
        }
    }
    let store = Store::open_with_models(&paths.docs_dir, &signing, &origin, seed)
        .map_err(anyhow::Error::msg)?;
    Ok(store)
}

/// Applies the operation to the config file (no daemon running; the
/// daemon will pick it up on start or on its next config poll).
fn direct_drive_op(paths: &StatePaths, config: &mut ReactorConfig, op: DriveOp) -> Result<()> {
    let pause_resume = matches!(&op, DriveOp::Pause { .. });
    match op {
        DriveOp::Add {
            name,
            addr,
            token_env,
            offline,
        } => {
            let addr = addr.trim().to_string();
            let addr_ma = Multiaddr::from_str(&addr)
                .with_context(|| format!("'{addr}' is not a valid multiaddr"))?;
            let name = if name.trim().is_empty() {
                default_drive_name(&addr_ma)
            } else {
                name.trim().to_string()
            };
            if config
                .drives
                .iter()
                .any(|d| d.name.eq_ignore_ascii_case(&name))
            {
                bail!("a drive named '{name}' is already configured");
            }
            config.drives.push(DriveConfig {
                name: name.clone(),
                addr: addr_ma.to_string(),
                token_env,
                paused: false,
                available_offline: offline,
            });
            config::save(paths, config)?;
            println!("added drive '{name}' ({addr})");
            Ok(())
        }
        DriveOp::Remove { name } => {
            let before = config.drives.len();
            config
                .drives
                .retain(|d| !d.name.eq_ignore_ascii_case(&name));
            if config.drives.len() == before {
                bail!("drive '{name}' is not configured");
            }
            config::save(paths, config)?;
            println!("removed drive '{name}'");
            Ok(())
        }
        DriveOp::Pause { name } | DriveOp::Resume { name } => {
            let paused = pause_resume;
            let Some(d) = config
                .drives
                .iter_mut()
                .find(|d| d.name.eq_ignore_ascii_case(&name))
            else {
                bail!("drive '{name}' is not configured");
            };
            d.paused = paused;
            config::save(paths, config)?;
            println!(
                "{} drive '{name}'",
                if paused { "paused" } else { "resumed" }
            );
            Ok(())
        }
        DriveOp::Resync { name } => {
            if !config
                .drives
                .iter()
                .any(|d| d.name.eq_ignore_ascii_case(&name))
            {
                bail!("drive '{name}' is not configured");
            }
            println!(
                "drive '{name}' will re-sync on the next daemon start (it is not running now)"
            );
            Ok(())
        }
    }
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
            addr,
            token_env,
            offline,
        } => {
            let body = serde_json::json!({
                "name": name,
                "addr": addr,
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

/// Percent-encodes a drive name for use as a URL path segment (names
/// may contain spaces).
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

/// One live view per configured drive (for `drive list`): from the
/// running daemon's `/api/status` when it is up, from the config file
/// otherwise.
async fn live_drive_views(
    config: &ReactorConfig,
    settings_url: &str,
) -> Vec<status::DriveStatusEntry> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .ok();
    if let Some(client) = &client {
        if let Some(resp) = client
            .get(format!("{settings_url}/api/status"))
            .send()
            .await
            .ok()
            .filter(|r| r.status().is_success())
        {
            if let Ok(snap) = resp.json::<status::StatusSnapshot>().await {
                return snap.drives;
            }
        }
    }
    config
        .drives
        .iter()
        .map(|d| status::DriveStatusEntry {
            name: d.name.clone(),
            addr: d.addr.clone(),
            paused: d.paused,
            status: if d.paused { "paused" } else { "offline" }.into(),
            detail: "daemon not running".into(),
        })
        .collect()
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

    // Identity key.
    match p2p::load_or_create_identity(&paths.key_file()) {
        Ok(kp) => {
            let peer = p2p::peer_id_of(&kp);
            let b58 = peer.to_base58();
            check(
                "identity",
                true,
                &format!("peer {}", &b58[..16.min(b58.len())]),
            );
        }
        Err(err) => check("identity", false, &err),
    }

    // Doc store (opens read-write but performs no writes).
    let store_check: Result<String> = (|| {
        let kp =
            p2p::load_or_create_identity(&paths.key_file()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let signing = p2p::signing_key(&kp).map_err(|e| anyhow::anyhow!("{e}"))?;
        let peer = p2p::peer_id_of(&kp);
        let store = Store::open(&paths.docs_dir, &signing, &peer.to_base58())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(format!("{} live docs", store.live_doc_count()))
    })();
    match store_check {
        Ok(detail) => check("store", true, &detail),
        Err(err) => check("store", false, &err.to_string()),
    }

    // Listen address (valid multiaddr + free TCP port).
    let listen = config.instance.listen.clone();
    match Multiaddr::from_str(&listen) {
        Ok(ma) => match check_listen_port(&ma) {
            Ok(()) => check("listen", true, &listen),
            Err(err) => check("listen", false, &err.to_string()),
        },
        Err(err) => check("listen", false, &err.to_string()),
    }

    // Settings server (only meaningful when the daemon is up).
    let settings_url = format!(
        "http://{}:{}/api/status",
        config.settings.host, config.settings.port
    );
    if daemon_is_running(&paths.daemon_pidfile()) {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .ok();
        let ok = if let Some(c) = &client {
            c.get(&settings_url)
                .send()
                .await
                .ok()
                .is_some_and(|r| r.status().is_success())
        } else {
            false
        };
        check("settings", ok, &settings_url);
    } else {
        check("settings", true, "daemon not running (skipped)");
    }

    // Session bus (needed for the tray).
    match std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        Ok(addr) if !addr.is_empty() => check("session bus", true, &addr),
        _ => check(
            "session bus",
            false,
            "no session bus (the tray will not appear; headless is fine)",
        ),
    }

    if failed > 0 {
        bail!("{failed} check(s) failed");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// logs
// ---------------------------------------------------------------------------

/// Prints the last lines of the daemon log; follows the file on growth
/// when `follow`.
pub async fn logs(state_dir: Option<&Path>, follow: bool) -> Result<()> {
    let paths = StatePaths::resolve(state_dir);
    let path = paths.reactor_log();
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
    path: std::path::PathBuf,
}

struct LogAppend {
    path: std::path::PathBuf,
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

/// File appender (rotated) plus, when running in the foreground, stdout.
///
/// The stdout layer is what makes the daemon observable under a process
/// supervisor that captures stdio -- `kubectl logs`, systemd, or a plain
/// terminal. Without it the only record is `logs/reactor.log` inside the
/// state dir, which in a container means no log shipping at all.
///
/// stdout rather than stderr on purpose: log collectors record which stream
/// a line arrived on, and emitting routine info lines on stderr makes them
/// read as errors downstream.
fn init_logging(paths: &StatePaths, config: &ReactorConfig, to_stdout: bool) -> Result<()> {
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
    if to_stdout {
        tracing_subscriber::registry()
            .with(filter)
            .with(file_layer)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(io::stdout)
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

/// Looks for a newer release and either says so or applies it.
///
/// The conditions are all checks that applying would make anyway, evaluated
/// here so that "an update is available" never means "an update that would be
/// refused is available".
pub async fn check_for_update(
    store: &Arc<crate::store::Store>,
    blobs: &Arc<crate::blob::BlobStore>,
    paths: &crate::paths::StatePaths,
    auto: bool,
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<Command>,
) {
    use crate::package::trust::TrustStore;
    use crate::update::{self, Release};

    let trust = TrustStore::load(&paths.publishers_file());
    let current = crate::VERSION;
    let platform = update::current_platform();

    let mut best: Option<Release> = None;
    for doc in crate::query::query_docs(store, "release", None) {
        if doc["fields"]["status"].as_str() == Some("withdrawn") {
            continue;
        }
        let Some(raw) = doc["fields"]["manifest"].as_str() else {
            continue;
        };
        let Ok(r) = serde_json::from_str::<Release>(raw) else {
            continue;
        };
        // Untrusted publisher, bad signature, wrong platform or not newer: all
        // reasons this node would refuse, so none of them is "available".
        if !trust.is_trusted(&r.publisher_key)
            || r.verify().is_err()
            || !r.runs_on(platform)
            || !r.is_newer_than(current)
        {
            continue;
        }
        let better = match &best {
            None => true,
            Some(b) => matches!(
                update::compare(&r.version, &b.version),
                Some(std::cmp::Ordering::Greater)
            ),
        };
        if better {
            best = Some(r);
        }
    }

    let Some(release) = best else {
        tracing::debug!("no newer release for {platform} (running {current})");
        return;
    };

    let missing = blobs.missing(&release.binary).len();
    if missing > 0 {
        tracing::info!(
            "release {} is available; fetching {missing} remaining chunks",
            release.version
        );
        let _ = cmd_tx.send(Command::FetchBlob {
            blob: release.binary.clone(),
        });
        return;
    }

    if !auto {
        // The notification IS the feature when auto is off. Logged at info so
        // it survives the default filter, and surfaced by /api/updates.
        tracing::info!(
            "release {} is available (running {current}). Apply it from the console, \
             or set update.auto to apply signed releases automatically.",
            release.version
        );
        return;
    }

    let Ok(exe) = std::env::current_exe() else {
        tracing::warn!("update.auto is on but the running binary cannot be located");
        return;
    };
    match crate::update::apply::apply(&release, blobs, &exe, &paths.root, current) {
        Ok(target) => {
            if target != exe {
                // The container case: the image binary is on a read-only root,
                // so the update lives in the state directory. Say so loudly --
                // `kubectl` will keep reporting the image tag, and a silent
                // divergence between the two is worse than a noisy one.
                tracing::warn!(
                    "applied release {} to {} -- this is NOT the binary this process \
                     started from ({}), so the image tag no longer describes what runs",
                    release.version,
                    target.display(),
                    exe.display()
                );
            } else {
                tracing::warn!("applied release {} to {}", release.version, target.display());
            }
            tracing::warn!("stopping so the supervisor starts the new binary");
            let _ = cmd_tx.send(Command::Quit);
        }
        Err(e) => tracing::warn!("could not apply release {}: {e}", release.version),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn test_config() -> ReactorConfig {
        let mut c = ReactorConfig::default();
        c.instance.listen = "/ip4/127.0.0.1/tcp/4101".into();
        c.drives.push(DriveConfig {
            name: "vault".into(),
            addr: "/ip4/10.0.0.2/tcp/4201/p2p/12D3KooWg8111".into(),
            token_env: None,
            paused: false,
            available_offline: true,
        });
        c
    }

    #[test]
    fn status_json_flag_parses() {
        let args = crate::cli::CliArgs::parse_from(["ph-reactor", "status", "--json"]);
        match args.command {
            Some(crate::cli::Command::Status { json: true }) => {}
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    /// The exact JSON shape the shell plugin consumes from
    /// `status --json` while the daemon is not running.
    #[test]
    fn degraded_snapshot_matches_the_shell_contract() {
        let settings_url = "http://127.0.0.1:4002".to_string();
        let snap = degraded_snapshot(&test_config(), &settings_url);
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&snap).unwrap()).unwrap();

        for key in ["version", "reactor", "drives", "settings", "updated_at"] {
            assert!(v.get(key).is_some(), "missing top-level key {key}");
        }

        let r = &v["reactor"];
        for key in [
            "running",
            "healthy",
            "peer_id",
            "listen",
            "docs",
            "last_event",
        ] {
            assert!(r.get(key).is_some(), "missing reactor key {key}");
        }
        assert_eq!(r["running"], false);
        assert_eq!(r["healthy"], false);
        assert_eq!(r["listen"], "/ip4/127.0.0.1/tcp/4101");
        assert_eq!(r["last_event"], "daemon not running");

        let d = &v["drives"][0];
        for key in ["name", "addr", "paused", "status", "detail"] {
            assert!(d.get(key).is_some(), "missing drive key {key}");
        }
        assert_eq!(d["name"], "vault");
        assert_eq!(d["addr"], "/ip4/10.0.0.2/tcp/4201/p2p/12D3KooWg8111");
        assert_eq!(d["paused"], false);
        assert_eq!(d["status"], "offline");
        assert_eq!(d["detail"], "daemon not running");

        assert_eq!(v["settings"]["url"], settings_url);
        assert_eq!(v["version"], crate::VERSION);

        let ts = v["updated_at"].as_str().unwrap();
        assert!(
            ts.chars().next().is_some_and(|c| c.is_ascii_digit()) && ts.contains('T'),
            "updated_at is not RFC 3339-shaped: {ts}"
        );
    }

    /// The daemon's `/api/status` serves the same struct; the live shape
    /// (healthy engine, synced drive) must carry the same keys and the
    /// documented drive-status vocabulary.
    #[test]
    fn live_snapshot_shape_is_the_same_contract() {
        let snap = StatusSnapshot {
            version: crate::VERSION.to_string(),
            reactor: ReactorStatus {
                running: true,
                healthy: true,
                peer_id: Some("12D3KooWg8111".into()),
                listen: "/ip4/0.0.0.0/tcp/4201".into(),
                docs: 4,
                active_drives: 0,
                last_doc: None,
                last_event: Some("drive vault: synced".into()),
            },
            drives: vec![DriveStatusEntry {
                name: "vault".into(),
                addr: "/ip4/10.0.0.2/tcp/4201/p2p/12D3KooWg8111".into(),
                paused: true,
                status: "paused".into(),
                detail: String::new(),
            }],
            bans: Vec::new(),
            settings: status::SettingsStatus {
                url: "http://127.0.0.1:4002".into(),
            },
            groups: Vec::new(),
            updated_at: "2026-09-12T00:00:00Z".into(),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&snap).unwrap()).unwrap();
        assert_eq!(v["reactor"]["running"], true);
        assert_eq!(v["reactor"]["healthy"], true);
        assert_eq!(v["reactor"]["peer_id"], "12D3KooWg8111");
        assert_eq!(v["drives"][0]["paused"], true);
        assert_eq!(v["drives"][0]["status"], "paused");
    }

    #[test]
    fn default_drive_name_prefers_peer_id() {
        let ma: Multiaddr = format!("/ip4/10.0.0.2/tcp/4201/p2p/{}", crate::p2p::test_peer_id())
            .parse()
            .unwrap();
        let name = default_drive_name(&ma);
        assert!(name.starts_with("peer-"), "{name}");
        let no_peer: Multiaddr = "/ip4/10.0.0.2/tcp/4242".parse().unwrap();
        assert_eq!(default_drive_name(&no_peer), "drive-4242");
    }
}
