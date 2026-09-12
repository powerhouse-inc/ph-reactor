//! State directory layout for `ph-reactor`.
//!
//! Everything the daemon owns lives under one state directory so that
//! snaps, brew installs, and raw binary installs all behave the same:
//!
//! ```text
//! <state>/
//!   config.json      daemon configuration
//!   node/            private Node runtime (only when bootstrapped)
//!   switchboard/     npm install tree + generated powerhouse.config.json
//!   data/            PGlite store backing the local reactor
//!   logs/            reactor.log, switchboard.log (rotating)
//!   run/             pidfiles and the single-instance lock
//! ```
//!
//! The root resolves to `$PH_REACTOR_STATE_DIR` when set (the snap sets it
//! to `$SNAP_USER_DATA/ph-reactor`), else `$HOME/.ph/reactor`.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub const STATE_DIR_ENV: &str = "PH_REACTOR_STATE_DIR";

/// The default state root: `$PH_REACTOR_STATE_DIR` or `$HOME/.ph/reactor`,
/// read once per process. The daemon and every CLI subcommand resolve
/// through here, so there is a single source of truth.
static DEFAULT_ROOT: LazyLock<PathBuf> = LazyLock::new(|| {
    if let Some(dir) = std::env::var_os(STATE_DIR_ENV) {
        return PathBuf::from(dir);
    }
    match dirs::home_dir() {
        Some(home) => home.join(".ph").join("reactor"),
        None => PathBuf::from(".ph").join("reactor"),
    }
});

pub fn default_root() -> &'static Path {
    &DEFAULT_ROOT
}

/// Explicit state dir (CLI `--state-dir`), or [`default_root`].
pub fn root(state_dir: Option<&Path>) -> PathBuf {
    match state_dir {
        Some(p) => p.to_path_buf(),
        None => DEFAULT_ROOT.clone(),
    }
}

/// All paths under the state root, derived once.
#[derive(Debug, Clone)]
pub struct StatePaths {
    pub root: PathBuf,
    pub node_dir: PathBuf,
    pub switchboard_dir: PathBuf,
    pub data_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub run_dir: PathBuf,
    pub config_file: PathBuf,
}

impl StatePaths {
    pub fn for_root(root: &Path) -> Self {
        let root = root.to_path_buf();
        Self {
            config_file: root.join("config.json"),
            node_dir: root.join("node"),
            switchboard_dir: root.join("switchboard"),
            data_dir: root.join("data"),
            logs_dir: root.join("logs"),
            run_dir: root.join("run"),
            root,
        }
    }

    pub fn resolve(state_dir: Option<&Path>) -> Self {
        Self::for_root(&root(state_dir))
    }

    /// Creates every directory (root `0700`, the rest `0755`). Idempotent.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir_all(&self.root)?;
        let _ = fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700));
        for dir in [
            &self.node_dir,
            &self.switchboard_dir,
            &self.data_dir,
            &self.logs_dir,
            &self.run_dir,
        ] {
            fs::create_dir_all(dir)?;
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
        }
        Ok(())
    }

    pub fn reactor_log(&self) -> PathBuf {
        self.logs_dir.join("reactor.log")
    }

    pub fn switchboard_log(&self) -> PathBuf {
        self.logs_dir.join("switchboard.log")
    }

    pub fn daemon_pidfile(&self) -> PathBuf {
        self.run_dir.join("ph-reactor.pid")
    }

    pub fn switchboard_pidfile(&self) -> PathBuf {
        self.run_dir.join("switchboard.pid")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.run_dir.join("lock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_under_root() {
        let paths = StatePaths::for_root(Path::new("/srv/state"));
        assert_eq!(paths.config_file, PathBuf::from("/srv/state/config.json"));
        assert_eq!(paths.node_dir, PathBuf::from("/srv/state/node"));
        assert_eq!(
            paths.switchboard_dir,
            PathBuf::from("/srv/state/switchboard")
        );
        assert_eq!(paths.data_dir, PathBuf::from("/srv/state/data"));
        assert_eq!(paths.logs_dir, PathBuf::from("/srv/state/logs"));
        assert_eq!(paths.run_dir, PathBuf::from("/srv/state/run"));
        assert_eq!(
            paths.reactor_log(),
            PathBuf::from("/srv/state/logs/reactor.log")
        );
        assert_eq!(
            paths.switchboard_log(),
            PathBuf::from("/srv/state/logs/switchboard.log")
        );
    }

    #[test]
    fn ensure_dirs_creates_tree() {
        let dir = std::env::temp_dir().join(format!(
            "ph-reactor-test-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::remove_dir_all(&dir).ok();
        let paths = StatePaths::for_root(&dir);
        paths.ensure_dirs().unwrap();
        for sub in ["node", "switchboard", "data", "logs", "run"] {
            assert!(
                paths.root.join(sub).is_dir(),
                "{sub} missing from {}",
                paths.root.display()
            );
        }
        // second call is a no-op
        paths.ensure_dirs().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }
}
