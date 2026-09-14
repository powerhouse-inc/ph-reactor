//! Applying a release: stage, verify, swap, keep the old one.
//!
//! This is the only code in the daemon that can leave a machine with no working
//! binary, so every step is ordered to fail in the safe direction:
//!
//! 1. **Write to a temporary file first.** A half-written binary must never
//!    occupy the path that is about to be executed.
//! 2. **Verify the staged bytes against the signed hash**, after writing, not
//!    before. Verifying bytes in memory and then writing something else is a
//!    check of the wrong thing.
//! 3. **Keep the running binary** as `.previous` rather than deleting it, so a
//!    node that will not start has something to go back to.
//! 4. **Rename, never copy, into place.** `rename` within a filesystem is
//!    atomic: a concurrent exec sees the old inode or the new one, never a
//!    partial file.
//!
//! The daemon does not exec the new binary itself. It exits, and whatever
//! supervises it — systemd, the autostart entry, Kubernetes — starts it again.
//! Re-execing in-process would mean the upgrade path is one the supervisor has
//! never taken, and an upgrade is a bad moment to discover that.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::blob::{BlobRef, BlobStore};

use super::Release;

#[derive(Debug)]
pub enum ApplyError {
    /// The bytes did not match the hash the publisher signed.
    Corrupt(String),
    /// Not all chunks have arrived yet.
    Incomplete(usize),
    /// Built for a different target; applying it would brick the node.
    WrongPlatform { want: String, got: String },
    /// Not actually an upgrade.
    NotNewer { current: String, offered: String },
    Io(String),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Corrupt(e) => write!(f, "the staged binary does not match its signed hash: {e}"),
            Self::Incomplete(n) => write!(f, "{n} chunks of the binary have not arrived yet"),
            Self::WrongPlatform { want, got } => {
                write!(f, "this release is for {got}, this node is {want}")
            }
            Self::NotNewer { current, offered } => {
                write!(f, "{offered} does not supersede the running {current}")
            }
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

/// Where a staged binary lives.
///
/// Beside the running binary when that directory is writable, which keeps the
/// atomic rename on one filesystem. In a container the root filesystem is
/// read-only, so it falls back to the state directory — see [`staging_dir`].
pub fn staging_dir(exe: &Path, state_dir: &Path) -> PathBuf {
    let beside = exe.parent().unwrap_or(Path::new("."));
    if is_writable(beside) {
        beside.to_path_buf()
    } else {
        state_dir.join("bin")
    }
}

fn is_writable(dir: &Path) -> bool {
    // Asked by trying, not by reading permission bits: bits do not account for
    // a read-only mount, which is precisely the container case this exists for.
    let probe = dir.join(".ph-reactor-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Stages a release's binary and puts it in place.
///
/// Returns the path that will run next. When that is not the path the daemon
/// was started from — a container with a read-only root — the caller is told so
/// it can say that out loud rather than let the two silently disagree.
pub fn apply(
    release: &Release,
    blobs: &BlobStore,
    exe: &Path,
    state_dir: &Path,
    current_version: &str,
) -> Result<PathBuf, ApplyError> {
    if !release.runs_on(super::current_platform()) {
        return Err(ApplyError::WrongPlatform {
            want: super::current_platform().to_string(),
            got: release.platform.clone(),
        });
    }
    if !release.is_newer_than(current_version) {
        return Err(ApplyError::NotNewer {
            current: current_version.to_string(),
            offered: release.version.clone(),
        });
    }
    let missing = blobs.missing(&release.binary).len();
    if missing > 0 {
        return Err(ApplyError::Incomplete(missing));
    }

    // `get` reassembles and checks the whole blob against the signed hash.
    let bytes = blobs.get(&release.binary).map_err(ApplyError::Corrupt)?;

    let dir = staging_dir(exe, state_dir);
    std::fs::create_dir_all(&dir)
        .map_err(|e| ApplyError::Io(format!("creating {}: {e}", dir.display())))?;

    let staged = dir.join(format!(".ph-reactor-{}.staged", release.version));
    std::fs::write(&staged, &bytes)
        .map_err(|e| ApplyError::Io(format!("writing {}: {e}", staged.display())))?;

    // Re-read what actually landed on disk. A short write, a full filesystem or
    // a truncation between write and rename all produce a file that is not what
    // was verified -- and the only way to know is to look at the file.
    let written = std::fs::read(&staged)
        .map_err(|e| ApplyError::Io(format!("re-reading {}: {e}", staged.display())))?;
    if crate::doc::Hash32::of(&written) != release.binary.hash {
        let _ = std::fs::remove_file(&staged);
        return Err(ApplyError::Corrupt(
            "what landed on disk is not what was verified".into(),
        ));
    }

    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| ApplyError::Io(format!("chmod {}: {e}", staged.display())))?;

    // The target is the running binary when its directory is writable, and the
    // staged copy in the state directory otherwise.
    let target = if dir == exe.parent().unwrap_or(Path::new(".")) {
        exe.to_path_buf()
    } else {
        dir.join("ph-reactor")
    };

    // Keep what is running. A node that will not start needs something to go
    // back to, and deleting it first would make that impossible exactly when it
    // matters.
    if target.exists() {
        let previous = with_suffix(&target, ".previous");
        if let Err(e) = std::fs::rename(&target, &previous) {
            // Not fatal on a fresh staging path, but never proceed if the thing
            // being replaced could not be preserved.
            if target.exists() {
                let _ = std::fs::remove_file(&staged);
                return Err(ApplyError::Io(format!(
                    "could not preserve the current binary: {e}"
                )));
            }
        }
    }

    std::fs::rename(&staged, &target)
        .map_err(|e| ApplyError::Io(format!("installing {}: {e}", target.display())))?;

    // When the update did NOT land on the path this process was started from --
    // a container, where the binary sits on a read-only root -- something has
    // to make the next start use it. Without this the supervisor re-launches
    // the image binary, which finds the same release still newer than itself
    // and applies it again: a restart loop that looks like an upgrade.
    //
    // A marker file rather than running the staged binary with `--version`:
    // reading a file is cheap, cannot hang, and does not execute a binary
    // before it has been decided that it should run.
    if target != exe {
        let marker = dir.join(STAGED_MARKER);
        let body = serde_json::json!({ "version": release.version, "path": target });
        std::fs::write(&marker, body.to_string())
            .map_err(|e| ApplyError::Io(format!("writing {}: {e}", marker.display())))?;
    }

    Ok(target)
}

/// Names the staged binary the next start should run instead of this one.
pub const STAGED_MARKER: &str = "staged.json";

/// A staged binary that supersedes the running one, if there is one.
///
/// Returns its path and version. Everything that could be wrong is treated as
/// "there isn't one": a missing file, unreadable JSON, a version that does not
/// compare, a path that no longer exists. Falling back to the binary that is
/// already running is always safe; refusing to start is not.
pub fn staged_replacement(state_dir: &Path, current_version: &str) -> Option<(PathBuf, String)> {
    let marker = state_dir.join("bin").join(STAGED_MARKER);
    let raw = std::fs::read_to_string(marker).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let version = v.get("version")?.as_str()?.to_string();
    let path = PathBuf::from(v.get("path")?.as_str()?);
    if !path.is_file() {
        return None;
    }
    match super::compare(&version, current_version) {
        Some(std::cmp::Ordering::Greater) => Some((path, version)),
        _ => None,
    }
}

/// Puts the preserved binary back. The rollback half of [`apply`].
pub fn rollback(target: &Path) -> Result<(), String> {
    let previous = with_suffix(target, ".previous");
    if !previous.exists() {
        return Err(format!("no {} to roll back to", previous.display()));
    }
    std::fs::rename(&previous, target).map_err(|e| format!("rolling back: {e}"))
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Whether a blob is fully here and matches its hash — the question the console
/// asks before offering an Update button that could only fail.
pub fn ready(blobs: &BlobStore, binary: &BlobRef) -> bool {
    blobs.is_complete(binary) && blobs.get(binary).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    /// Without a marker the container case is a restart loop: the image binary
    /// comes back, sees the same release is still newer than itself, and
    /// applies it again forever.
    #[test]
    fn staging_outside_the_running_path_records_what_to_run_next() {
        let dir = tempfile::tempdir().expect("tmp");
        let blobs = BlobStore::open(&dir.path().join("blobs")).expect("blobs");
        let ro = dir.path().join("image-bin");
        std::fs::create_dir_all(&ro).expect("mkdir");
        let exe = ro.join("ph-reactor");
        std::fs::write(&exe, b"the image binary").expect("write");
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).expect("chmod");

        let state = dir.path().join("state");
        let r = release_for(&blobs, b"the new binary", "9.9.9", super::super::current_platform());
        let applied = apply(&r, &blobs, &exe, &state, "1.0.0");
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let target = applied.expect("apply");

        let (path, version) =
            staged_replacement(&state, "1.0.0").expect("a staged binary is recorded");
        assert_eq!(path, target);
        assert_eq!(version, "9.9.9");

        // And once the staged binary IS what is running, it must not be
        // offered again -- that is the loop, one step later.
        assert!(
            staged_replacement(&state, "9.9.9").is_none(),
            "a staged binary must not supersede itself"
        );
    }

    /// Replacing the running binary in place needs no marker, and must not
    /// leave one that would redirect a later start.
    #[test]
    fn replacing_in_place_records_no_marker() {
        let dir = tempfile::tempdir().expect("tmp");
        let blobs = BlobStore::open(&dir.path().join("blobs")).expect("blobs");
        let exe = dir.path().join("ph-reactor");
        std::fs::write(&exe, b"old").expect("write");
        let r = release_for(&blobs, b"new", "9.9.9", super::super::current_platform());
        apply(&r, &blobs, &exe, dir.path(), "1.0.0").expect("apply");
        assert!(staged_replacement(dir.path(), "1.0.0").is_none());
    }

    /// Anything wrong with the marker means "run what is already running".
    #[test]
    fn a_broken_marker_is_ignored_rather_than_fatal() {
        let dir = tempfile::tempdir().expect("tmp");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        let marker = bin.join(STAGED_MARKER);

        for body in [
            "not json",
            "{}",
            r#"{"version":"9.9.9"}"#,
            r#"{"version":"9.9.9","path":"/nonexistent/ph-reactor"}"#,
            r#"{"version":"nightly","path":"/bin/sh"}"#,
        ] {
            std::fs::write(&marker, body).expect("write");
            assert!(
                staged_replacement(dir.path(), "1.0.0").is_none(),
                "must ignore marker {body:?}"
            );
        }
    }

    fn release_for(blobs: &BlobStore, bytes: &[u8], version: &str, platform: &str) -> Release {
        let binary = blobs.put(bytes).expect("store");
        let k = SigningKey::from_bytes(&[5u8; 32]);
        let mut r = Release {
            version: version.into(),
            platform: platform.into(),
            notes: String::new(),
            binary,
            publisher_key: hex::encode(k.verifying_key().to_bytes()),
            sig: String::new(),
        };
        r.sign(&k);
        r
    }

    #[test]
    fn applying_installs_the_binary_and_keeps_the_old_one() {
        let dir = tempfile::tempdir().expect("tmp");
        let blobs = BlobStore::open(&dir.path().join("blobs")).expect("blobs");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        let exe = bin.join("ph-reactor");
        std::fs::write(&exe, b"the old binary").expect("write");

        let r = release_for(&blobs, b"the new binary", "9.9.9", super::super::current_platform());
        let target = apply(&r, &blobs, &exe, dir.path(), "1.0.0").expect("apply");

        assert_eq!(target, exe, "a writable dir is replaced in place");
        assert_eq!(std::fs::read(&exe).expect("read"), b"the new binary");
        assert_eq!(
            std::fs::read(bin.join("ph-reactor.previous")).expect("read"),
            b"the old binary",
            "the running binary must be preserved for rollback"
        );
        assert_eq!(
            std::fs::metadata(&exe).expect("meta").permissions().mode() & 0o111,
            0o111,
            "the installed binary must be executable"
        );
    }

    #[test]
    fn rollback_restores_the_previous_binary() {
        let dir = tempfile::tempdir().expect("tmp");
        let blobs = BlobStore::open(&dir.path().join("blobs")).expect("blobs");
        let exe = dir.path().join("ph-reactor");
        std::fs::write(&exe, b"the old binary").expect("write");

        let r = release_for(&blobs, b"the new binary", "9.9.9", super::super::current_platform());
        apply(&r, &blobs, &exe, dir.path(), "1.0.0").expect("apply");
        rollback(&exe).expect("rollback");
        assert_eq!(std::fs::read(&exe).expect("read"), b"the old binary");
    }

    /// The whole reason the platform is in the signed bytes.
    #[test]
    fn a_release_for_another_platform_is_never_installed() {
        let dir = tempfile::tempdir().expect("tmp");
        let blobs = BlobStore::open(&dir.path().join("blobs")).expect("blobs");
        let exe = dir.path().join("ph-reactor");
        std::fs::write(&exe, b"the old binary").expect("write");

        let r = release_for(&blobs, b"a darwin binary", "9.9.9", "aarch64-apple-darwin");
        match apply(&r, &blobs, &exe, dir.path(), "1.0.0") {
            Err(ApplyError::WrongPlatform { .. }) => {}
            other => panic!("expected WrongPlatform, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(&exe).expect("read"),
            b"the old binary",
            "a refused release must not have touched the binary"
        );
    }

    #[test]
    fn an_older_release_is_refused() {
        let dir = tempfile::tempdir().expect("tmp");
        let blobs = BlobStore::open(&dir.path().join("blobs")).expect("blobs");
        let exe = dir.path().join("ph-reactor");
        std::fs::write(&exe, b"the old binary").expect("write");

        let r = release_for(&blobs, b"an older binary", "0.1.0", super::super::current_platform());
        match apply(&r, &blobs, &exe, dir.path(), "1.8.0") {
            Err(ApplyError::NotNewer { .. }) => {}
            other => panic!("expected NotNewer, got {other:?}"),
        }
        assert_eq!(std::fs::read(&exe).expect("read"), b"the old binary");
    }

    /// A release whose chunks have not all arrived must not be applied, and
    /// must say how much is missing so the caller can fetch the rest.
    #[test]
    fn an_incomplete_binary_is_refused() {
        let dir = tempfile::tempdir().expect("tmp");
        let publisher = BlobStore::open(&dir.path().join("pub")).expect("blobs");
        let receiver = BlobStore::open(&dir.path().join("recv")).expect("blobs");
        let exe = dir.path().join("ph-reactor");
        std::fs::write(&exe, b"the old binary").expect("write");

        let r = release_for(&publisher, b"the new binary", "9.9.9", super::super::current_platform());
        match apply(&r, &receiver, &exe, dir.path(), "1.0.0") {
            Err(ApplyError::Incomplete(n)) => assert!(n > 0),
            other => panic!("expected Incomplete, got {other:?}"),
        }
        assert_eq!(std::fs::read(&exe).expect("read"), b"the old binary");
    }

    /// A read-only directory containing the binary is the container case. The
    /// update must land in the state directory instead of failing.
    #[test]
    fn a_read_only_binary_directory_stages_into_the_state_dir() {
        let dir = tempfile::tempdir().expect("tmp");
        let blobs = BlobStore::open(&dir.path().join("blobs")).expect("blobs");
        let ro = dir.path().join("usr-local-bin");
        std::fs::create_dir_all(&ro).expect("mkdir");
        let exe = ro.join("ph-reactor");
        std::fs::write(&exe, b"the image binary").expect("write");
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).expect("chmod");

        let state = dir.path().join("state");
        let r = release_for(&blobs, b"the new binary", "9.9.9", super::super::current_platform());
        let target = apply(&r, &blobs, &exe, &state, "1.0.0");

        // Restore write permission before any assertion can fail, or the
        // tempdir cannot be cleaned up.
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let target = target.expect("apply into the state dir");
        assert_eq!(target, state.join("bin").join("ph-reactor"));
        assert_ne!(target, exe, "the read-only image binary is left alone");
        assert_eq!(std::fs::read(&target).expect("read"), b"the new binary");
        assert_eq!(
            std::fs::read(&exe).expect("read"),
            b"the image binary",
            "the read-only original must be untouched"
        );
    }
}
