//! State directory layout for `ph-reactor`.
//!
//! Everything the daemon owns lives under one state directory so that
//! snaps, brew installs, and raw binary installs all behave the same:
//!
//! ```text
//! <state>/
//!   config.json      daemon configuration
//!   key              the daemon's ed25519 identity (0600)
//!   docs/            the native doc store (snapshots + live logs)
//!   logs/            reactor.log (rotating)
//!   run/             pidfile, single-instance lock, ready marker
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
    /// The doc store (snapshots + live op logs + index).
    pub docs_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub run_dir: PathBuf,
    pub config_file: PathBuf,
}

impl StatePaths {
    pub fn for_root(root: &Path) -> Self {
        let root = root.to_path_buf();
        Self {
            config_file: root.join("config.json"),
            docs_dir: root.join("docs"),
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
        for dir in [&self.docs_dir, &self.logs_dir, &self.run_dir] {
            fs::create_dir_all(dir)?;
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
        }
        Ok(())
    }

    pub fn reactor_log(&self) -> PathBuf {
        self.logs_dir.join("reactor.log")
    }

    /// The daemon's ed25519 identity key (32 bytes, 0600).
    pub fn key_file(&self) -> PathBuf {
        self.root.join("key")
    }
    /// The local ban list: a JSON array of refused peer ids (base58).
    pub fn bans_file(&self) -> PathBuf {
        self.root.join("bans.json")
    }

    /// The user-configurable processor specs (the invoice -> payment engine).
    pub fn processors_file(&self) -> PathBuf {
        self.root.join("processors.json")
    }

    /// Packages installed on this node (see package::install).
    pub fn packages_file(&self) -> PathBuf {
        self.root.join("packages.json")
    }

    /// Publishers this node accepts packages from (see package::trust).
    pub fn publishers_file(&self) -> PathBuf {
        self.root.join("publishers.json")
    }

    /// Content-addressed chunk store for package bundles.
    pub fn blobs_dir(&self) -> PathBuf {
        self.root.join("blobs")
    }

    /// Model definitions registered at runtime.
    ///
    /// These MUST be loaded before the store replays its action logs. A
    /// document's read model is rebuilt by reducing its actions through the
    /// model that wrote them; without the definition the actions cannot be
    /// reduced and the document comes back empty — no name, no fields. Only
    /// built-in models survived a restart before this file existed, because
    /// `Store::open` seeds those itself.
    pub fn models_file(&self) -> PathBuf {
        self.root.join("models.json")
    }

    pub fn daemon_pidfile(&self) -> PathBuf {
        self.run_dir.join("ph-reactor.pid")
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
        assert_eq!(paths.docs_dir, PathBuf::from("/srv/state/docs"));
        assert_eq!(paths.logs_dir, PathBuf::from("/srv/state/logs"));
        assert_eq!(paths.run_dir, PathBuf::from("/srv/state/run"));
        assert_eq!(
            paths.reactor_log(),
            PathBuf::from("/srv/state/logs/reactor.log")
        );
        assert_eq!(paths.key_file(), PathBuf::from("/srv/state/key"));
    }

    #[test]
    fn ensure_dirs_creates_tree() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir =
            std::env::temp_dir().join(format!("ph-reactor-test-{}-{}", std::process::id(), nanos));
        std::fs::remove_dir_all(&dir).ok();
        let paths = StatePaths::for_root(&dir);
        paths.ensure_dirs().unwrap();
        for sub in ["docs", "logs", "run"] {
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
