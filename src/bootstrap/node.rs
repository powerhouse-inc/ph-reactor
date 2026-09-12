//! Node.js runtime resolution.
//!
//! The switchboard requires Node >= 24. Resolution order:
//! 1. the system's `node` (when it satisfies the minimum and the config
//!    prefers the system runtime),
//! 2. a private runtime downloaded from nodejs.org into the state dir
//!    (`<state>/node/node-<ver>-linux-<arch>/`), sha256-verified against the
//!    distribution's `SHASUMS256.txt` before use.
//!
//! The private runtime is bootstrapped once and re-verified on every start
//! (marker + `node --version`), so a corrupt or partial install self-heals.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::paths::StatePaths;

/// The pinned Node distribution for private runtimes. The switchboard's
/// `engines` field requires `>=24.0.0`; this is a stable 24.x release.
pub const NODE_DIST_VERSION: &str = "v24.11.1";

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_ATTEMPTS: u32 = 3;
const SHASUMS_BASE: &str = "https://nodejs.org/dist";

/// A usable Node runtime: path to the `node` executable + its version.
#[derive(Debug, Clone)]
pub struct NodeRuntime {
    pub path: PathBuf,
    pub version: String,
}

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .expect("reqwest client builds")
});

/// `true` when `version` (e.g. `v24.1.0`, `22.11.0`, `v24.0.0-rc.1`) is at
/// least `minimum` (e.g. `24` → 24.0.0). Prereleases sort below their
/// release (semver rules), so `v24.0.0-rc.1` is *not* >= 24.0.0.
pub fn version_at_least(version: &str, minimum: &str) -> bool {
    let actual = parse_semver(version);
    let min = parse_semver(&format!("{minimum}.0.0")).or_else(|| parse_semver(minimum));
    match (actual, min) {
        (Some(a), Some(m)) => a >= m,
        _ => false,
    }
}

fn parse_semver(raw: &str) -> Option<semver::Version> {
    let trimmed = raw.trim().trim_start_matches('v').trim();
    // Tolerate `24` / `24.1` (fill missing components with zeros).
    let padded: Vec<&str> = trimmed
        .split('.')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let mut padded = padded;
    while padded.len() < 3 {
        padded.push("0");
    }
    let s = padded.join(".");
    semver::Version::parse(&s).ok()
}

/// Probes the system's Node: runs `node --version` with a short timeout.
pub async fn probe_system() -> Option<NodeRuntime> {
    for candidate in system_node_candidates() {
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::process::Command::new(&candidate)
                .arg("--version")
                .output(),
        )
        .await
        .ok()?;
        let Ok(output) = output else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if parse_semver(&version).is_some() {
            return Some(NodeRuntime {
                path: candidate,
                version,
            });
        }
    }
    None
}

fn system_node_candidates() -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from("node")];
    if let Some(home) = dirs::home_dir() {
        out.push(home.join(".local/bin/node"));
    }
    out.push(PathBuf::from("/usr/local/bin/node"));
    out.push(PathBuf::from("/usr/bin/node"));
    out
}

/// Resolves the Node runtime to use: the system's when it satisfies the
/// minimum and `prefer_system`, else the private runtime (bootstrap on
/// first use).
pub async fn resolve(
    minimum: &str,
    prefer_system: bool,
    state: &StatePaths,
) -> Result<NodeRuntime> {
    if prefer_system {
        if let Some(runtime) = probe_system().await {
            if version_at_least(&runtime.version, minimum) {
                return Ok(runtime);
            }
            tracing::info!(
                "system node {} is below {}; using the private runtime",
                runtime.version,
                minimum
            );
        } else {
            tracing::info!("no system node found; using the private runtime");
        }
    }
    bootstrap_private(state).await
}

fn private_dir(state: &StatePaths) -> PathBuf {
    state
        .node_dir
        .join(format!("node-{NODE_DIST_VERSION}-linux-{}", arch()))
}

fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => {
            // Unreachable on supported targets; fail loudly elsewhere.
            let _ = other;
            panic!("unsupported architecture: {}", std::env::consts::ARCH)
        }
    }
}

fn tarball_name() -> String {
    format!("node-{NODE_DIST_VERSION}-linux-{}.tar.gz", arch())
}

/// The private runtime, bootstrapped if needed:
/// download → sha256 verify (SHASUMS256.txt) → extract → marker.
pub async fn bootstrap_private(state: &StatePaths) -> Result<NodeRuntime> {
    state.ensure_dirs()?;
    let dir = private_dir(state);
    let marker = state.node_dir.join(".node-ok");

    // Fast path: marker + a healthy runtime.
    if marker.exists() && dir.join("bin/node").exists() && verify_runtime(&dir).await {
        let version = runtime_version(&dir).await?;
        return Ok(NodeRuntime {
            path: dir.join("bin/node"),
            version,
        });
    }

    tracing::info!(
        "bootstrapping private node {} into {}",
        NODE_DIST_VERSION,
        dir.display()
    );
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match download_and_extract(state, &dir).await {
            Ok(()) => {
                let version = runtime_version(&dir).await?;
                tokio::fs::write(&marker, format!("{NODE_DIST_VERSION}\n")).await?;
                tracing::info!("private node {} ready at {}", version, dir.display());
                return Ok(NodeRuntime {
                    path: dir.join("bin/node"),
                    version,
                });
            }
            Err(err) => {
                tracing::warn!(
                    "node bootstrap attempt {attempt}/{} failed: {err:#}",
                    MAX_ATTEMPTS
                );
                let _ = tokio::fs::remove_dir_all(&dir).await;
                last_err = Some(err);
                tokio::time::sleep(Duration::from_secs((2 * attempt) as u64)).await;
            }
        }
    }
    bail!(
        "could not bootstrap node {NODE_DIST_VERSION}: {}",
        last_err.map(|e| e.to_string()).unwrap_or_default()
    )
}

/// Runs `<dir>/bin/node --version` and checks it matches the pin.
async fn verify_runtime(dir: &Path) -> bool {
    let version = runtime_version(dir).await;
    match version {
        Ok(v) => v.starts_with(NODE_DIST_VERSION.trim_start_matches('v')),
        Err(_) => false,
    }
}

async fn runtime_version(dir: &Path) -> Result<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(dir.join("bin/node"))
            .arg("--version")
            .output(),
    )
    .await
    .context("node --version timed out")?
    .context("failed to run private node")?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Streams the tarball to a temp file while hashing it, verifies against
/// SHASUMS256.txt, then extracts into a fresh directory and renames.
async fn download_and_extract(state: &StatePaths, target: &Path) -> Result<()> {
    let name = tarball_name();
    let url = format!("{SHASUMS_BASE}/{NODE_DIST_VERSION}/{name}");
    let expected = expected_sha256(&name).await?;

    let pid = std::process::id();
    let tmp = state.node_dir.join(format!(".download-{pid}.tmp"));
    let extract_tmp = state.node_dir.join(format!(".extract-{pid}"));

    tokio::fs::create_dir_all(&extract_tmp).await?;
    let mut file = tokio::fs::File::create(&tmp).await?;
    let mut hasher = Sha256::new();
    let mut stream = HTTP
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url} status"))?
        .bytes_stream();
    use futures_util::TryStreamExt;
    loop {
        let Some(chunk) = stream.try_next().await? else {
            break;
        };
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    let actual = format!("{:x}", hasher.finalize());
    if !constant_time_eq(actual.as_bytes(), expected.as_bytes()) {
        let _ = tokio::fs::remove_file(&tmp).await;
        bail!("sha256 mismatch for {name}: expected {expected}, got {actual}");
    }

    // gunzip + untar
    let raw_bytes = tokio::fs::read(&tmp).await?;
    let decompressed: Vec<u8> = {
        use flate2::read::MultiGzDecoder;
        use std::io::Read;
        let mut decoder = MultiGzDecoder::new(raw_bytes.as_slice());
        let mut out = Vec::new();
        decoder.read_to_end(&mut out)?;
        out
    };
    let mut archive = tar::Archive::new(std::io::Cursor::new(decompressed));
    archive
        .unpack(&extract_tmp)
        .with_context(|| "extract node tarball")?;

    // The tarball's top-level dir is the runtime; flatten it into `target`.
    let mut inner: Option<PathBuf> = None;
    for entry in std::fs::read_dir(&extract_tmp)?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            inner = Some(path);
            break;
        }
    }
    let inner = inner.with_context(|| "tarball did not unpack to a directory")?;
    if target.exists() {
        tokio::fs::remove_dir_all(target).await?;
    }
    tokio::fs::rename(&inner, target).await?;
    let _ = tokio::fs::remove_dir_all(&extract_tmp).await;
    let _ = tokio::fs::remove_file(&tmp).await;
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x == y)
}

/// Fetches `SHASUMS256.txt` and returns the expected sha256 hex digest for
/// `name`.
async fn expected_sha256(name: &str) -> Result<String> {
    let url = format!("{SHASUMS_BASE}/{NODE_DIST_VERSION}/SHASUMS256.txt");
    let text = HTTP
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url} status"))?
        .text()
        .await?;
    parse_shasums_line(&text, name)
        .map(|(digest, _)| digest)
        .with_context(|| format!("no SHASUMS256.txt entry for {name}"))
}

/// Parses a `SHASUMS256.txt` line (`<hex>  <filename>`) for `name`; skips
/// lines for other platforms.
pub fn parse_shasums_line(text: &str, name: &str) -> Option<(String, String)> {
    for line in text.lines() {
        let line = line.trim();
        let Some((hash, file)) = line.split_once(' ') else {
            continue;
        };
        let hash = hash.trim();
        let file = file.trim();
        if file == name && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some((hash.to_string(), file.to_string()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_gate() {
        assert!(version_at_least("v24.1.0", "24"));
        assert!(version_at_least("v24.0.0", "24"));
        assert!(version_at_least("25.2.1", "24"));
        assert!(!version_at_least("v22.11.0", "24"));
        assert!(!version_at_least("v23.9.9", "24"));
        // prerelease sorts below its release
        assert!(!version_at_least("v24.0.0-rc.1", "24"));
        // a prerelease of 24.1.0 sorts below the release itself (semver)
        assert!(!version_at_least("v24.1.0-rc.1", "24.1.0"));
        assert!(version_at_least("v24.1.0-rc.1", "24"));
        // partial versions pad to zero
        assert!(version_at_least("24.1", "24"));
        assert!(!version_at_least("junk", "24"));
        assert!(!version_at_least("", "24"));
    }

    const SHASUMS: &str = "\
9f13f140f7e6a4c3d8f1f0f5a0e8b2c1d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8  node-v24.11.1-aix-ppc64.tar.gz
a1b2c3d4e5f60718293a4b5c6d7e8f901234567890abcdef1234567890abcdef12  node-v24.11.1-darwin-arm64.tar.gz
1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef  node-v24.11.1-linux-x64.tar.gz
fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210  node-v24.11.1-linux-arm64.tar.gz
0000000000000000000000000000000000000000000000000000000000000000  node-v24.11.1-win-x64.zip
";

    #[test]
    fn shasums_picks_the_matching_platform() {
        let (digest, file) = parse_shasums_line(SHASUMS, "node-v24.11.1-linux-x64.tar.gz").unwrap();
        assert_eq!(file, "node-v24.11.1-linux-x64.tar.gz");
        assert_eq!(
            digest,
            "1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef"
        );
        assert_eq!(parse_shasums_line(SHASUMS, "node-v99.tar.gz"), None);
        // single-space separator is tolerated (64-hex digests only)
        let digest64 = "1".repeat(64);
        assert!(parse_shasums_line(&format!("{digest64} cd"), "cd").is_some());
    }
    #[test]
    fn constant_time_eq_rejects_length_mismatch() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    /// Live bootstrap: downloads the real node distribution. Gated behind
    /// `PH_REACTOR_LIVE=1` so the default suite stays offline.
    #[tokio::test]
    async fn live_private_bootstrap() {
        if std::env::var("PH_REACTOR_LIVE").is_err() {
            eprintln!("skipping live bootstrap (set PH_REACTOR_LIVE=1)");
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "ph-reactor-node-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let state = StatePaths::for_root(&dir);
        let runtime = bootstrap_private(&state)
            .await
            .expect("private bootstrap works against nodejs.org");
        assert!(
            runtime.version.starts_with("v24"),
            "got {}",
            runtime.version
        );
        let out = tokio::process::Command::new(&runtime.path)
            .arg("-e")
            .arg("process.stdout.write(process.version)")
            .output()
            .await
            .unwrap();
        let printed = String::from_utf8_lossy(&out.stdout);
        assert!(printed.starts_with("v24"), "got {printed}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
