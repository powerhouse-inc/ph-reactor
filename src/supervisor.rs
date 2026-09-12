//! Switchboard process supervision: spawn the generated boot wrapper with
//! the bootstrap environment, track health, restart with exponential
//! backoff on crash, and shut down gracefully on request (SIGTERM →
//! grace → SIGKILL). Child output is sunk into the rotated daemon log.

use std::collections::VecDeque;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Result;
use tokio::process::Command;
use tokio::sync::{mpsc, watch};

use crate::bootstrap::{node, switchboard};
use crate::config::ReactorConfig;
use crate::logrotate;
use crate::paths::StatePaths;

const HEALTH_INTERVAL: Duration = Duration::from_secs(2);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(120);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A run this long (without crashing) resets the backoff counter.
const STABLE_RUN: Duration = Duration::from_secs(60);

/// Cooperative shutdown: a watch channel the daemon flips when the
/// daemon asks the supervisor to stop (SIGTERM to the child, then exit).
#[derive(Clone)]
pub struct ShutdownSignal {
    tx: std::sync::Arc<watch::Sender<bool>>,
    rx: watch::Receiver<bool>,
}

impl ShutdownSignal {
    pub fn new() -> (Self, Self) {
        let (tx, rx) = watch::channel(false);
        let a = Self {
            tx: std::sync::Arc::new(tx),
            rx: rx.clone(),
        };
        let b = Self {
            tx: a.tx.clone(),
            rx,
        };
        (a, b)
    }

    /// Asks the supervisor to stop (used on daemon shutdown).
    pub fn trigger(&self) {
        self.tx.send_modify(|f| *f = true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// Resolves once the signal fires (poll-safe inside `select!`).
    pub async fn cancelled(&self) {
        let mut rx = self.rx.clone();
        let _ = rx.changed().await;
    }
}

/// Coarse supervisor events, consumed by the tray menu and settings page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusEvent {
    /// A new child process was spawned.
    Starting,
    /// The switchboard answered `/health`.
    Healthy { port: u16 },
    /// The child exited and will be restarted after a delay.
    Restarting { attempt: u32, delay: Duration },
    /// The child exited with a non-zero status.
    Crashed { code: Option<i32> },
    /// The configuration changed; the child is respawned with it.
    ConfigUpdate,
    /// The supervisor stopped (daemon shutdown).
    Stopped,
}

impl StatusEvent {
    /// Short human-readable form for the status display.
    pub fn summary(&self) -> String {
        match self {
            StatusEvent::Starting => "starting".into(),
            StatusEvent::Healthy { port } => format!("healthy (port {port})"),
            StatusEvent::Restarting { attempt, delay } => {
                format!("restarting (attempt {attempt} in {}s)", delay.as_secs())
            }
            StatusEvent::Crashed { code } => format!("crashed (code {code:?})"),
            StatusEvent::ConfigUpdate => "config updated, restarting".into(),
            StatusEvent::Stopped => "stopped".into(),
        }
    }
}

/// Snapshot of the supervision loop's state for status display.
#[derive(Debug, Clone, Default)]
pub struct SupervisorState {
    pub running: bool,
    pub healthy: bool,
    pub restarts: u32,
    pub last_event: Option<StatusEvent>,
    /// First lines of the most recent child output (for `ph status`).
    pub recent_output: VecDeque<String>,
}

pub struct Supervisor {
    config: ReactorConfig,
    node: node::NodeRuntime,
    state: StatePaths,
    events: mpsc::UnboundedSender<StatusEvent>,
    /// The daemon's config channel: a change here respawns the child with
    /// the new configuration (no backoff).
    config_rx: watch::Receiver<ReactorConfig>,
    /// Shared with the daemon's status poller.
    pub status: std::sync::Arc<tokio::sync::Mutex<SupervisorState>>,
}

impl Supervisor {
    pub fn new(
        config: ReactorConfig,
        node: node::NodeRuntime,
        state: StatePaths,
        events: mpsc::UnboundedSender<StatusEvent>,
        config_rx: watch::Receiver<ReactorConfig>,
    ) -> Self {
        Self {
            config,
            node,
            state,
            events,
            config_rx,
            status: std::sync::Arc::new(tokio::sync::Mutex::default()),
        }
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.config.switchboard.port)
    }

    /// Runs the supervision loop until the shutdown token fires; then
    /// stops the child gracefully and returns.
    pub async fn run(self, shutdown: &ShutdownSignal) -> Result<()> {
        let mut attempt: u32 = 0;
        let mut self_ = self;
        loop {
            if shutdown.is_cancelled() {
                break;
            }
            // Pick up config updates and respawn with them.
            self_.config = self_.config_rx.borrow_and_update().clone();
            let base = self_.base_url();

            // Spawn
            let mut child = self_.spawn()?;
            let started = std::time::Instant::now();
            self_.emit(StatusEvent::Starting).await;
            self_.set_status(|s| s.running = true).await;
            tracing::info!("switchboard started (pid {})", child.id().unwrap_or(0));

            // Sink child output into the rotated log.
            let _log_path = self_.state.reactor_log();
            let (out_handle, err_handle) =
                self_.attach_stdio(child.stdout.take(), child.stderr.take());

            // Wait for health, exit, shutdown, or a config update.
            let healthy = match Self::wait_for_health(
                &base,
                &mut child,
                shutdown,
                &mut self_.config_rx,
            )
            .await
            {
                HealthOutcome::Healthy => true,
                HealthOutcome::Shutdown => {
                    self_.graceful_stop(&mut child).await;
                    drop(out_handle);
                    drop(err_handle);
                    self_.emit(StatusEvent::Stopped).await;
                    self_
                        .set_status(|s| {
                            s.running = false;
                            s.healthy = false;
                        })
                        .await;
                    return Ok(());
                }
                HealthOutcome::ConfigUpdate => {
                    self_.graceful_stop(&mut child).await;
                    drop(out_handle);
                    drop(err_handle);
                    self_.emit(StatusEvent::ConfigUpdate).await;
                    self_
                        .set_status(|s| {
                            s.running = false;
                            s.healthy = false;
                        })
                        .await;
                    tracing::info!("config changed; restarting switchboard");
                    attempt = 0;
                    continue;
                }
                _ => false,
            };
            if healthy {
                attempt = 0;
                self_
                    .emit(StatusEvent::Healthy {
                        port: self_.config.switchboard.port,
                    })
                    .await;
                self_.set_status(|s| s.healthy = true).await;
            }

            // Wait for the child to exit (or shutdown / config update).
            let exit = match Self::wait_for_exit(&mut child, shutdown, &mut self_.config_rx).await {
                ExitOutcome::Exited(status) => status,
                ExitOutcome::Shutdown => {
                    self_.graceful_stop(&mut child).await;
                    drop(out_handle);
                    drop(err_handle);
                    self_.emit(StatusEvent::Stopped).await;
                    self_
                        .set_status(|s| {
                            s.running = false;
                            s.healthy = false;
                        })
                        .await;
                    return Ok(());
                }
                ExitOutcome::ConfigUpdate => {
                    self_.graceful_stop(&mut child).await;
                    drop(out_handle);
                    drop(err_handle);
                    self_.emit(StatusEvent::ConfigUpdate).await;
                    self_
                        .set_status(|s| {
                            s.running = false;
                            s.healthy = false;
                        })
                        .await;
                    tracing::info!("config changed; restarting switchboard");
                    attempt = 0;
                    continue;
                }
            };

            // Crash path.
            if started.elapsed() >= STABLE_RUN {
                attempt = 0;
            }
            attempt += 1;
            let delay = backoff(attempt);
            self_
                .emit(StatusEvent::Crashed {
                    code: exit.and_then(|s| s.code()),
                })
                .await;
            self_.emit(StatusEvent::Restarting { attempt, delay }).await;
            self_
                .set_status(|s| {
                    s.running = false;
                    s.healthy = false;
                    s.restarts = s.restarts.saturating_add(1);
                })
                .await;
            tracing::warn!("switchboard exited; restart #{attempt} in {:?}", delay);
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = shutdown.cancelled() => break,
                _ = self_.config_rx.changed() => {}
            }
        }
        Ok(())
    }

    fn spawn(&self) -> Result<tokio::process::Child> {
        let entry = switchboard::wrapper_entry(&self.state);
        if !entry.exists() {
            anyhow::bail!("generated boot wrapper missing: {}", entry.display());
        }
        let mut cmd = Command::new(&self.node.path);
        // Inherit the user environment (HOME, PATH, NPM_TOKEN, proxy…) and
        // overlay the switchboard-specific variables.
        cmd.arg(&entry)
            .current_dir(&self.state.switchboard_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in switchboard::spawn_env(&self.config) {
            cmd.env(k, v);
        }
        cmd.spawn().map_err(|e| {
            anyhow::anyhow!("failed to spawn node ({}): {e}", self.node.path.display())
        })
    }

    /// Attaches piped stdout/stderr to line-buffered log sinks; returns the
    /// sink handles (dropping them stops the sinks).
    fn attach_stdio(
        &self,
        stdout: Option<tokio::process::ChildStdout>,
        stderr: Option<tokio::process::ChildStderr>,
    ) -> (
        Option<tokio::task::JoinHandle<()>>,
        Option<tokio::task::JoinHandle<()>>,
    ) {
        let mut out_h = None;
        let mut err_h = None;
        if let Some(out) = stdout {
            let log = self.state.reactor_log();
            out_h = Some(tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(out).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = logrotate::append_line(&log, &format!("[o] {line}"));
                }
            }));
        }
        if let Some(err) = stderr {
            let log = self.state.reactor_log();
            err_h = Some(tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(err).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = logrotate::append_line(&log, &format!("[e] {line}"));
                }
            }));
        }
        (out_h, err_h)
    }

    /// Polls `/health` until it answers, the shutdown fires, the config
    /// changes, or the timeout elapses (a start that never becomes healthy
    /// is treated as a crash).
    async fn wait_for_health(
        base: &str,
        child: &mut tokio::process::Child,
        shutdown: &ShutdownSignal,
        config_rx: &mut watch::Receiver<ReactorConfig>,
    ) -> HealthOutcome {
        let deadline = tokio::time::Instant::now() + HEALTH_TIMEOUT;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return HealthOutcome::Shutdown,
                _ = config_rx.changed() => return HealthOutcome::ConfigUpdate,
                _ = tokio::time::sleep(HEALTH_INTERVAL) => {}
            }
            if tokio::time::Instant::now() > deadline {
                return HealthOutcome::Timeout;
            }
            // A dead child never becomes healthy: stop polling early.
            if child.try_wait().ok().flatten().is_some() {
                return HealthOutcome::DeadChild;
            }
            if let Ok(true) = switchboard::health(base).await {
                return HealthOutcome::Healthy;
            }
        }
    }

    /// Waits for the child to exit, for the shutdown token, or for a
    /// config update.
    async fn wait_for_exit(
        child: &mut tokio::process::Child,
        shutdown: &ShutdownSignal,
        config_rx: &mut watch::Receiver<ReactorConfig>,
    ) -> ExitOutcome {
        tokio::select! {
            res = child.wait() => ExitOutcome::Exited(res.ok()),
            _ = shutdown.cancelled() => ExitOutcome::Shutdown,
            _ = config_rx.changed() => ExitOutcome::ConfigUpdate,
        }
    }

    /// SIGTERM, wait up to the grace period, then SIGKILL.
    async fn graceful_stop(&self, child: &mut tokio::process::Child) {
        if let Some(pid) = child.id() {
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            let _ = tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await;
        }
        let _ = child.kill().await;
    }

    async fn emit(&self, event: StatusEvent) {
        self.set_status(|s| s.last_event = Some(event.clone()))
            .await;
        let _ = self.events.send(event);
    }

    async fn set_status(&self, f: impl FnOnce(&mut SupervisorState)) {
        let mut s = self.status.lock().await;
        f(&mut s);
    }
}

/// The outcomes of the pre-health window.
enum HealthOutcome {
    Healthy,
    Timeout,
    DeadChild,
    Shutdown,
    ConfigUpdate,
}

/// The outcomes of the post-health (or never-healthy) wait for exit.
enum ExitOutcome {
    Exited(Option<std::process::ExitStatus>),
    Shutdown,
    ConfigUpdate,
}

/// Exponential backoff: 1s, 2s, 4s, … capped at [`BACKOFF_MAX`].
fn backoff(attempt: u32) -> Duration {
    let exp = attempt.saturating_sub(1).min(5);
    (BACKOFF_BASE * 2u32.pow(exp)).min(BACKOFF_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_caps_at_max() {
        assert_eq!(backoff(1), Duration::from_secs(1));
        assert_eq!(backoff(2), Duration::from_secs(2));
        assert_eq!(backoff(3), Duration::from_secs(4));
        assert_eq!(backoff(6), Duration::from_secs(30));
        assert_eq!(backoff(50), Duration::from_secs(30));
    }

    /// Live: spawns a trivial node script as the "switchboard" and verifies
    /// the supervisor detects its exit and restarts it. Gated behind
    /// `PH_REACTOR_LIVE=1` (needs a node binary).
    #[tokio::test]
    async fn live_restart_loop() {
        if std::env::var("PH_REACTOR_LIVE").is_err() {
            eprintln!("skipping live supervisor test (set PH_REACTOR_LIVE=1)");
            return;
        }
        let node = node::probe_system()
            .await
            .or_else(|| {
                Some(node::NodeRuntime {
                    path: std::path::PathBuf::from("node"),
                    version: "system".into(),
                })
            })
            .expect("a node binary is available");
        let dir = std::env::temp_dir().join(format!(
            "ph-reactor-supervisor-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let state = StatePaths::for_root(&dir);
        state.ensure_dirs().unwrap();
        // A fake switchboard entry that lives ~2s then exits with code 3.
        let entry_dir = state
            .switchboard_dir
            .join("node_modules/@powerhousedao/switchboard/dist");
        std::fs::create_dir_all(&entry_dir).unwrap();
        std::fs::write(
            entry_dir.join("index.mjs"),
            "setTimeout(() => process.exit(3), 2000);",
        )
        .unwrap();
        let config = ReactorConfig::default();
        let (tx, mut rx) = mpsc::unbounded_channel::<StatusEvent>();
        let (shutdown, _daemon_side) = ShutdownSignal::new();
        let (cfg_tx, cfg_rx) = watch::channel(config.clone());
        let _ = cfg_tx;
        let supervisor = Supervisor::new(config, node, state, tx, cfg_rx);

        let signal = shutdown.clone();
        let run = tokio::spawn(async move {
            let _ = supervisor.run(&signal).await;
        });
        // Expect: Starting, then (no health) Crashed, then Restarting.
        let mut seen_starting = false;
        let mut seen_restarting = false;
        for _ in 0..20 {
            match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
                Ok(Some(e)) => {
                    if matches!(e, StatusEvent::Starting) {
                        seen_starting = true;
                    }
                    if matches!(e, StatusEvent::Restarting { .. }) {
                        seen_restarting = true;
                    }
                    if seen_starting && seen_restarting {
                        break;
                    }
                }
                _ => break,
            }
        }
        shutdown.trigger();
        let _ = run.await;
        assert!(seen_starting, "supervisor never reported Starting");
        assert!(seen_restarting, "supervisor never restarted after crash");
        std::fs::remove_dir_all(&dir).ok();
    }
}
