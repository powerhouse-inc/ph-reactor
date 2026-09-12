//! Switchboard bootstrap: npm-install the `@powerhousedao/switchboard`
//! package into `<state>/switchboard`, write its `powerhouse.config.json`,
//! generate the boot wrapper entry, and provide the spawn environment +
//! health probe.
//!
//! The daemon never runs the package's own entry (`dist/index.mjs`): it
//! generates a small wrapper (`<switchboard_dir>/.ph-reactor/entry.mjs`)
//! that boots the installed `@powerhousedao/switchboard/server` with
//! ph-reactor's choices — the active `"connect"` channel scheme (remote
//! drive sync), the package registry and boot packages (document model
//! packages load from the registry at boot), and a per-drive JWT handler
//! that reads bearer tokens from named environment variables. The
//! wrapper is regenerated whenever the daemon configuration changes, so
//! it always matches the installed package's public API.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::node::NodeRuntime;
use crate::config::{atomic_write, ReactorConfig};
use crate::paths::StatePaths;

const NPM_INSTALL_TIMEOUT: Duration = Duration::from_secs(600);

/// Our own marker for the installed switchboard (kept separate from npm's
/// `package.json` so npm never sees daemon bookkeeping).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchboardMeta {
    /// The spec that was installed (e.g. `@powerhousedao/switchboard@latest`).
    pub spec: String,
    /// The resolved version from the installed `package.json`.
    pub version: String,
    /// RFC 3339 timestamp of the install.
    pub installed_at: String,
}

fn meta_path(state: &StatePaths) -> PathBuf {
    state.switchboard_dir.join(".ph-reactor-meta.json")
}

/// The installed switchboard's entry point.
pub fn entry(state: &StatePaths) -> PathBuf {
    state
        .switchboard_dir
        .join("node_modules/@powerhousedao/switchboard/dist/index.mjs")
}

/// The `powerhouse.config.json` in the switchboard's working directory.
pub fn config_path(state: &StatePaths) -> PathBuf {
    state.switchboard_dir.join("powerhouse.config.json")
}

/// The generated boot wrapper the supervisor actually spawns.
pub fn wrapper_entry(state: &StatePaths) -> PathBuf {
    state.switchboard_dir.join(".ph-reactor").join("entry.mjs")
}

/// The published switchboard boots its reactor with the passive
/// `"switchboard"` channel scheme no matter what its entry receives
/// (its `initServer` never forwards `options.channelScheme` nor
/// `options.jwtHandler` to the reactor builder — verified in the
/// published dist), so an installed switchboard can serve as a sync
/// *server* but can never be a sync *client*. The daemon's remote drive
/// sync needs the active `"connect"` scheme (and, for auth-gated
/// remotes, a per-drive JWT handler on the channels), so the daemon
/// applies a minimal, anchored, idempotent patch to the installed
/// server chunk after every npm install:
///
/// - inserts `channelScheme: options.channelScheme` into the
///   `applySwitchboardReactorDefaults(...)` call, so the scheme the
///   boot wrapper passes actually reaches the channel factory;
/// - adds `reactorBuilder.withJwtHandler(options.jwtHandler)` after
///   that call, so the wrapper's per-drive token handler reaches the
///   channel factory as well.
///
/// The pristine chunk is backed up under `.ph-reactor/` before the
/// first patch; on builds where the anchors are absent (layout change)
/// the patch is skipped with a warning and the switchboard runs
/// passive. On builds that wire the options natively the markers are
/// already present and the patch is a no-op.
pub fn patch_sync_client(state: &StatePaths) -> Result<String> {
    let dist = state
        .switchboard_dir
        .join("node_modules/@powerhousedao/switchboard/dist");
    let mut outcome = String::from("no switchboard server chunk found to patch");
    let entries = match std::fs::read_dir(&dist) {
        Ok(e) => e,
        Err(_) => return Ok(outcome),
    };
    let marker_scheme = "channelScheme: options.channelScheme";
    let marker_jwt = "if (options.jwtHandler) reactorBuilder.withJwtHandler(options.jwtHandler);";
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("mjs") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let call = "applySwitchboardReactorDefaults(reactorBuilder, clientBuilder, {";
        let Some(at) = content.find(call) else {
            continue;
        };
        let mut patched = content.clone();
        let mut changes = Vec::new();
        if !patched.contains(marker_scheme) {
            let insert_at = at + call.len();
            patched.insert_str(insert_at, &format!(" {marker_scheme},"));
            changes.push("channelScheme wiring");
        }
        let jwt_wired = patched.contains("withJwtHandler(options.jwtHandler)");
        if !jwt_wired {
            let needle = "getRenownSignerConfig(renown, options.identity?.requireSignatures)";
            if let Some(ni) = patched.find(needle) {
                if let Some(rel) = patched[ni..].find("});") {
                    let close = ni + rel + "});".len();
                    // Match the indentation of the `});` line.
                    let line_start = patched[..close].rfind('\n').map(|i| i + 1).unwrap_or(0);
                    let indent: String = patched[line_start..close]
                        .chars()
                        .take_while(|c| *c == '\t' || *c == ' ')
                        .collect();
                    patched.insert_str(close, &format!("\n{indent}{marker_jwt}"));
                    changes.push("jwtHandler wiring");
                }
            }
        }
        if changes.is_empty() {
            outcome = format!(
                "switchboard {} already wires the sync options natively (or is patched)",
                path.file_name().unwrap().to_string_lossy()
            );
            continue;
        }
        // Backup the pristine file before mutating it (first patch only).
        let backup_dir = state.switchboard_dir.join(".ph-reactor/dist-patch");
        let backup = backup_dir.join(path.file_name().unwrap().to_string_lossy().as_ref());
        let pristine = if backup.exists() {
            std::fs::read(&backup)?
        } else {
            std::fs::create_dir_all(&backup_dir)?;
            let raw = std::fs::read(&path)?;
            std::fs::write(&backup, &raw)?;
            raw
        };
        if let Err(err) = atomic_write(&path, &patched) {
            // Restore the pristine content on failure.
            let _ = std::fs::write(&path, &pristine);
            return Err(err).with_context(|| {
                format!("patching {} failed; pristine file restored", path.display())
            });
        }
        // Verify the markers landed.
        let check = std::fs::read_to_string(&path)?;
        if !check.contains(marker_scheme)
            || !(check.contains(marker_jwt) || check.contains("withJwtHandler(options.jwtHandler)"))
        {
            let _ = std::fs::write(&path, &pristine);
            bail!(
                "post-patch verification failed for {}; pristine file restored",
                path.display()
            );
        }
        outcome = format!(
            "patched switchboard {} ({}; pristine backup at {})",
            path.file_name().unwrap().to_string_lossy(),
            changes.join(", "),
            backup.display()
        );
    }
    Ok(outcome)
}

/// Writes the boot wrapper entry (atomically). The wrapper boots the
/// installed switchboard's public `./server` API with ph-reactor's
/// choices:
///
/// - `channelScheme: "connect"`: remote drives use active request
///   channels (the installed default, `"switchboard"`, is passive-only
///   and never syncs content). Note that the published builds ignore
///   this option in their boot path — the daemon's [`patch_sync_client`]
///   closes that gap in the installed dist (a no-op on builds that wire
///   the option natively).
/// - `registryUrl` + `packages`: document model packages referenced by
///   drives are loaded from the package registry at boot (this is the
///   missing-package auto-install for a running switchboard).
/// - `jwtHandler`: the sync/attachment requests for a remote drive carry
///   a bearer token read from a named environment variable
///   (`PH_DRIVE_TOKEN_<i>`, see [`spawn_env`]) — the token value is
///   never written to disk. The published builds also do not wire this
///   handler into the sync channels; [`patch_sync_client`] closes that
///   gap, which is what makes authenticated remotes (e.g. a
///   vetra-hosted switchboard) syncable at all.
pub fn write_entry(state: &StatePaths, config: &ReactorConfig) -> Result<()> {
    state.ensure_dirs()?;
    let mut drive_tokens: Vec<(String, String)> = Vec::new();
    for (i, drive) in config.drives.iter().enumerate() {
        if drive.paused {
            continue;
        }
        if drive.token_env.is_none() {
            continue;
        }
        let Ok(u) = url::Url::parse(&drive.url) else {
            continue;
        };
        let origin = u.origin().ascii_serialization();
        drive_tokens.push((origin, format!("PH_DRIVE_TOKEN_{i}")));
    }
    let tokens_js = drive_tokens
        .iter()
        .map(|(origin, env)| -> Result<String> {
            Ok(format!(
                "  {{ match: {}, env: {} }},\n",
                serde_json::to_string(origin)?,
                serde_json::to_string(env)?
            ))
        })
        .collect::<Result<String>>()?;
    let packages_js = config
        .packages
        .iter()
        .map(|p| -> Result<String> { Ok(serde_json::to_string(p)?) })
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    let text = format!(
        "// Generated by ph-reactor - do not edit; it is regenerated\n\
// whenever the daemon configuration or the installed switchboard\n\
// changes.\n\
//\n\
// Boot wrapper for the installed @powerhousedao/switchboard package.\n\
import {{ startSwitchboard }} from \"@powerhousedao/switchboard/server\";\n\
\n\
// Per-drive auth: match the remote's origin against the name of an\n\
// environment variable holding its bearer token (never written to\n\
// disk).\n\
const driveTokens = [\n\
{tokens_js}];\n\
\n\
async function jwtHandler(url) {{\n\
  if (typeof url !== \"string\") return undefined;\n\
  for (const d of driveTokens) {{\n\
    if (url.startsWith(d.match)) {{\n\
      const token = process.env[d.env];\n\
      if (token) return `Bearer ${{token}}`;\n\
    }}\n\
  }}\n\
  return undefined;\n\
}}\n\
\n\
startSwitchboard({{\n\
  port: {},\n\
  database: {{ url: \"dev.db\" }},\n\
  mcp: true,\n\
  channelScheme: \"connect\",\n\
  registryUrl: {},\n\
  packages: [{packages_js}],\n\
  drive: {{\n\
    id: \"powerhouse\",\n\
    slug: \"powerhouse\",\n\
    global: {{\n\
      name: \"Powerhouse\",\n\
      icon: \"https://ipfs.io/ipfs/QmcaTDBYn8X2psGaXe7iQ6qd8q6oqHLgxvMX9yXf7f9uP7\",\n\
    }},\n\
    local: {{\n\
      availableOffline: true,\n\
      listeners: [],\n\
      sharingType: \"public\",\n\
      triggers: [],\n\
    }},\n\
  }},\n\
  jwtHandler,\n\
}}).catch((error) => {{\n\
  console.error(error);\n\
  process.exit(1);\n\
}});\n",
        config.switchboard.port,
        serde_json::to_string(&config.registry)?,
    );
    let dir = state.switchboard_dir.join(".ph-reactor");
    std::fs::create_dir_all(&dir)?;
    atomic_write(&wrapper_entry(state), &text)?;
    Ok(())
}

/// Writes the switchboard config file and the boot wrapper together.
/// They must always change hands as a pair: the wrapper's generated
/// shape and the config's keys are consumed by the same child process.
pub fn write_runtime_files(state: &StatePaths, config: &ReactorConfig) -> Result<()> {
    write_config(state, config)?;
    write_entry(state, config)
}

/// `true` when the installed switchboard matches the configured spec and
/// its entry point is present.
pub fn is_current(state: &StatePaths, spec: &str) -> bool {
    let Ok(raw) = std::fs::read(meta_path(state)) else {
        return false;
    };
    let Ok(meta) = serde_json::from_slice::<SwitchboardMeta>(&raw) else {
        return false;
    };
    meta.spec == spec && entry(state).exists()
}

/// Installs the switchboard with the npm that ships alongside the resolved
/// Node runtime: `npm install <spec> --registry <npm_registry>` in
/// `<state>/switchboard`.
pub async fn install(
    state: &StatePaths,
    node: &NodeRuntime,
    spec: &str,
    npm_registry: &str,
) -> Result<SwitchboardMeta> {
    state.ensure_dirs()?;
    let dir = &state.switchboard_dir;
    // Stub manifest so npm treats this directory as the install root.
    let manifest = dir.join("package.json");
    if !manifest.exists() {
        tokio::fs::write(
            &manifest,
            r#"{"name":"ph-reactor-switchboard","private":true,"version":"0.0.0"}
"#,
        )
        .await?;
    }

    tracing::info!(
        "installing {spec} (registry {npm_registry}) into {}",
        dir.display()
    );
    let npm = npm_path(node);
    let mut cmd = tokio::process::Command::new(&npm);
    cmd.current_dir(dir)
        .arg("install")
        .arg(spec)
        .arg("--registry")
        .arg(npm_registry)
        .arg("--no-audit")
        .arg("--no-fund")
        .arg("--loglevel=warn")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = tokio::time::timeout(NPM_INSTALL_TIMEOUT, cmd.output())
        .await
        .context("npm install timed out")?
        .with_context(|| format!("failed to run npm ({})", npm.display()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let err = if err.is_empty() {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            err
        };
        bail!("npm install {spec} failed: {err}");
    }

    let version = installed_version(dir).await?;
    let meta = SwitchboardMeta {
        spec: spec.into(),
        version,
        installed_at: rfc3339_now(),
    };
    let tmp = dir.join(".ph-reactor-meta.json.tmp");
    tokio::fs::write(&tmp, serde_json::to_string_pretty(&meta)?).await?;
    tokio::fs::rename(&tmp, meta_path(state)).await?;
    tracing::info!("switchboard {} installed", meta.version);
    Ok(meta)
}

/// `true` when an installed switchboard satisfies `spec` (or, if the meta
/// is missing, when the entry point exists — a fresh install follows).
/// Returns the meta when current.
pub fn current_meta(state: &StatePaths, spec: &str) -> Option<SwitchboardMeta> {
    if !is_current(state, spec) {
        return None;
    }
    let raw = std::fs::read(meta_path(state)).ok()?;
    serde_json::from_slice(&raw).ok()
}

async fn installed_version(dir: &std::path::Path) -> Result<String> {
    let raw = tokio::fs::read(dir.join("node_modules/@powerhousedao/switchboard/package.json"))
        .await
        .context("installed switchboard package.json missing")?;
    let v: serde_json::Value =
        serde_json::from_slice(&raw).context("installed package.json is not JSON")?;
    Ok(v["version"].as_str().unwrap_or("unknown").to_string())
}

/// The npm executable next to the resolved node binary (both the private
/// distribution and typical system installs keep `npm` in the same `bin/`);
/// falls back to `npm` on `PATH`.
pub fn npm_path(node: &NodeRuntime) -> PathBuf {
    let sibling = node.path.parent().map(|p| p.join("npm"));
    match sibling {
        Some(p) if p.exists() => p,
        _ => PathBuf::from("npm"),
    }
}

/// Writes the switchboard's `powerhouse.config.json` (atomically) from the
/// daemon configuration. The switchboard shallow-merges this over its
/// built-in defaults, so only the keys we control are written.
///
/// Remote drives are deliberately not written here: the daemon owns
/// their registration (MCP `addRemoteDrive` on the running switchboard),
/// and the installed entry does not read a `remoteDrives` key anyway —
/// its boot path would re-create channels for remotes the user paused
/// or removed.
pub fn write_config(state: &StatePaths, config: &ReactorConfig) -> Result<()> {
    state.ensure_dirs()?;
    let ph = serde_json::json!({
        "logLevel": config.log_level,
        "switchboard": {
            "port": config.switchboard.port,
            "database": { "url": "dev.db" },
        },
        "packages": config
            .packages
            .iter()
            .map(|name| serde_json::json!({ "packageName": name }))
            .collect::<Vec<_>>(),
        "packageRegistryUrl": config.registry,
    });
    let mut text = serde_json::to_string_pretty(&ph)?;
    text.push('\n');
    atomic_write(&config_path(state), &text)?;
    Ok(())
}

/// Environment for the spawned switchboard process: the switchboard's
/// own settings plus, for every configured drive that names a `tokenEnv`,
/// `PH_DRIVE_TOKEN_<i>` (index = position in the drive list) holding the
/// value of that variable. The generated boot wrapper reads these at
/// request time (never from the config file) and attaches them as bearer
/// tokens for the drive's origin. The names are part of the process
/// fingerprint, so a changed `tokenEnv` triggers a respawn; the values
/// are not (the daemon re-reads them on every spawn).
pub fn spawn_env(config: &ReactorConfig) -> Vec<(String, String)> {
    let mut env = vec![
        ("PORT".into(), config.switchboard.port.to_string()),
        (
            "PH_SWITCHBOARD_PORT".into(),
            config.switchboard.port.to_string(),
        ),
        ("LOG_LEVEL".into(), config.log_level.clone()),
        ("NODE_ENV".into(), "production".into()),
    ];
    for (i, drive) in config.drives.iter().enumerate() {
        if drive.paused {
            continue;
        }
        let Some(var) = &drive.token_env else {
            continue;
        };
        match std::env::var(var) {
            Ok(value) if !value.is_empty() => {
                env.push((format!("PH_DRIVE_TOKEN_{i}"), value));
            }
            _ => {
                tracing::debug!(
                    "drive '{}' tokenEnv {var} is unset; the wrapper will send no token for it",
                    drive.name
                );
            }
        }
    }
    env
}

/// `GET <base>/health` (the reactor-api gateway returns `200 OK`).
pub async fn health(base_url: &str) -> Result<bool> {
    use reqwest::Client;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .build()?;
    let url = format!("{base_url}/health");
    match client.get(&url).send().await {
        Ok(resp) => Ok(resp.status().is_success()),
        Err(_) => Ok(false),
    }
}

/// RFC 3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) without a time dep:
/// Howard Hinnant's days-to-civil algorithm over the Unix-day count.
pub fn rfc3339_now() -> String {
    use std::time::SystemTime;
    let secs = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = (if mo <= 2 { y + 1 } else { y }) as u64;
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::DriveConfig;
    use crate::paths::StatePaths;
    use std::fs;

    fn temp_paths() -> (tempfile::TempDir, StatePaths) {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::for_root(dir.path());
        (dir, paths)
    }

    #[test]
    fn write_config_emits_switchboard_shape() {
        let (_dir, state) = temp_paths();
        let mut config = ReactorConfig::default();
        config.switchboard.port = 4123;
        config.registry = "https://registry.vetra.io".into();
        config.packages = vec!["@powerhousedao/knowledge-note".into()];
        config.drives.push(DriveConfig {
            name: "remote".into(),
            url: "https://light-colt-c497cfbd-switchboard.vetra.io/d/powerhouse-knowledge".into(),
            token_env: Some("PH_TEST_VETRA_TOKEN".into()),
            available_offline: false,
            paused: false,
        });
        write_config(&state, &config).unwrap();

        let raw = fs::read_to_string(config_path(&state)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["logLevel"], "info");
        assert_eq!(v["switchboard"]["port"], 4123);
        assert_eq!(v["switchboard"]["database"]["url"], "dev.db");
        assert_eq!(
            v["packages"][0]["packageName"],
            "@powerhousedao/knowledge-note"
        );
        assert_eq!(v["packageRegistryUrl"], "https://registry.vetra.io");
        // Remote drives belong to the daemon's MCP registration, not to
        // the switchboard's config file.
        assert!(v.get("remoteDrives").is_none());
    }

    #[test]
    fn write_entry_generates_boot_wrapper() {
        let (_dir, state) = temp_paths();
        let mut config = ReactorConfig::default();
        config.switchboard.port = 4123;
        config.registry = "https://registry.dev.vetra.io".into();
        config.packages = vec!["@powerhousedao/knowledge-note".into()];
        config.drives.push(DriveConfig {
            name: "vault".into(),
            url: "https://light-colt-c497cfbd-switchboard.vetra.io/d/powerhouse-knowledge".into(),
            token_env: Some("PH_TEST_VETRA_TOKEN".into()),
            available_offline: false,
            paused: false,
        });
        // A paused drive must not appear in the token map.
        config.drives.push(DriveConfig {
            name: "paused-remote".into(),
            url: "https://other.example.com/d/zzz".into(),
            token_env: Some("PH_TEST_OTHER_TOKEN".into()),
            available_offline: false,
            paused: true,
        });
        write_entry(&state, &config).unwrap();

        let raw = fs::read_to_string(wrapper_entry(&state)).unwrap();
        assert!(raw.contains("from \"@powerhousedao/switchboard/server\""));
        assert!(raw.contains("channelScheme: \"connect\""));
        assert!(raw.contains("mcp: true"));
        assert!(raw.contains("port: 4123"));
        assert!(raw.contains(
            "match: \"https://light-colt-c497cfbd-switchboard.vetra.io\", env: \"PH_DRIVE_TOKEN_0\""
        ));
        // The paused drive (index 1) is absent from the token map.
        assert!(!raw.contains("other.example.com"));
        assert!(!raw.contains("PH_DRIVE_TOKEN_1"));
        assert!(raw.contains("\"@powerhousedao/knowledge-note\""));
        assert!(raw.contains("registryUrl: \"https://registry.dev.vetra.io\""));
    }

    #[test]
    fn write_entry_without_drives_is_valid() {
        let (_dir, state) = temp_paths();
        let config = ReactorConfig::default();
        write_entry(&state, &config).unwrap();
        let raw = fs::read_to_string(wrapper_entry(&state)).unwrap();
        assert!(raw.contains("const driveTokens = [\n];"));
        // The template literal must interpolate the token (JS `${...}`,
        // not a bare `{token}`).
        assert!(raw.contains("return `Bearer ${token}`;"));
    }

    #[test]
    fn spawn_env_carries_resolved_drive_tokens() {
        let mut config = ReactorConfig::default();
        config.drives.push(DriveConfig {
            name: "peer".into(),
            url: "http://peer.example/graphql/r".into(),
            token_env: Some("PH_REACTOR_TEST_TOKEN_ABC".into()),
            available_offline: false,
            paused: false,
        });
        // A drive whose variable is unset contributes no env entry.
        config.drives.push(DriveConfig {
            name: "unset".into(),
            url: "https://unset.example.com/d/knowledge".into(),
            token_env: Some("PH_REACTOR_TEST_TOKEN_UNSET_XYZ".into()),
            available_offline: false,
            paused: false,
        });
        std::env::set_var("PH_REACTOR_TEST_TOKEN_ABC", "s3cret");
        std::env::remove_var("PH_REACTOR_TEST_TOKEN_UNSET_XYZ");
        let env = spawn_env(&config);
        std::env::remove_var("PH_REACTOR_TEST_TOKEN_ABC");

        let token = env
            .iter()
            .find(|(k, _)| k == "PH_DRIVE_TOKEN_0")
            .map(|(_, v)| v.clone());
        assert_eq!(token.as_deref(), Some("s3cret"));
        assert!(!env.iter().any(|(k, _)| k == "PH_DRIVE_TOKEN_1"));
        assert!(env.iter().any(|(k, v)| k == "PORT" && v == "4001"));
    }

    fn fake_dist_chunk() -> String {
        String::from(
            "function applySwitchboardReactorDefaults(reactorBuilder, clientBuilder, options = {}) {\n",
        )
        + "  return options;\n"
        + "}\n"
        + "async function initServer(options) {\n"
        + "\t\tconst reactorBuilder = {};\n"
        + "\t\tconst clientBuilder = {};\n"
        + "\t\tapplySwitchboardReactorDefaults(reactorBuilder, clientBuilder, {\n"
        + "\t\t\tdocumentModels: [],\n"
        + "\t\t\tlogger: {},\n"
        + "\t\t\tsigner: renown ? getRenownSignerConfig(renown, options.identity?.requireSignatures) : void 0 \n"
        + "\t\t});\n"
        + "\t\tif (workerPool) {\n"
        + "\t\t\treturn;\n"
        + "\t\t}\n"
        + "}\n"
    }

    fn paths_with_chunk(t: &tempfile::TempDir, chunk_name: &str) -> StatePaths {
        let paths = StatePaths::for_root(t.path());
        let dist = paths
            .switchboard_dir
            .join("node_modules/@powerhousedao/switchboard/dist");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(dist.join(chunk_name), fake_dist_chunk()).unwrap();
        paths
    }

    #[test]
    fn patch_adds_both_wirings_and_backs_up() {
        let t = tempfile::TempDir::new().unwrap();
        let paths = paths_with_chunk(&t, "server-ABC123.mjs");
        let outcome = patch_sync_client(&paths).unwrap();
        assert!(
            outcome.contains("patched switchboard server-ABC123.mjs"),
            "outcome: {outcome}"
        );
        let chunk = paths
            .switchboard_dir
            .join("node_modules/@powerhousedao/switchboard/dist/server-ABC123.mjs");
        let content = std::fs::read_to_string(&chunk).unwrap();
        assert!(
            content.contains("channelScheme: options.channelScheme,")
                && content
                    .find("channelScheme: options.channelScheme")
                    .is_some_and(|i| i < content.find("documentModels").unwrap()),
            "scheme wiring missing or misplaced:\n{content}"
        );
        assert!(
            content.contains("\n\t\tif (options.jwtHandler) reactorBuilder.withJwtHandler(options.jwtHandler);\n\t\tif (workerPool) {"),
            "jwt wiring missing or misplaced:\n{content}"
        );
        let backup = paths
            .switchboard_dir
            .join(".ph-reactor/dist-patch/server-ABC123.mjs");
        assert!(backup.exists(), "pristine backup missing");
        let pristine = std::fs::read_to_string(&backup).unwrap();
        assert!(
            !pristine.contains("channelScheme: options.channelScheme"),
            "backup is not pristine"
        );
    }

    #[test]
    fn patch_is_idempotent() {
        let t = tempfile::TempDir::new().unwrap();
        let paths = paths_with_chunk(&t, "server-ABC123.mjs");
        let chunk = paths
            .switchboard_dir
            .join("node_modules/@powerhousedao/switchboard/dist/server-ABC123.mjs");
        patch_sync_client(&paths).unwrap();
        let first = std::fs::read_to_string(&chunk).unwrap();
        let outcome = patch_sync_client(&paths).unwrap();
        assert!(
            outcome.contains("already wires"),
            "second run should be a no-op, got: {outcome}"
        );
        assert_eq!(first, std::fs::read_to_string(&chunk).unwrap());
    }

    #[test]
    fn patch_is_noop_on_natively_wired_chunk() {
        let t = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::for_root(t.path());
        let dist = paths
            .switchboard_dir
            .join("node_modules/@powerhousedao/switchboard/dist");
        std::fs::create_dir_all(&dist).unwrap();
        let chunk = dist.join("server-NATIVE.mjs");
        std::fs::write(
            &chunk,
            String::from("function initServer(options) {\n")
                + "\t\tapplySwitchboardReactorDefaults(reactorBuilder, clientBuilder, { \n"
                + "\t\t\tchannelScheme: options.channelScheme,\n"
                + "\t\t\tdocumentModels: [],\n"
                + "\t\t});\n"
                + "\t\treactorBuilder.withJwtHandler(options.jwtHandler);\n"
                + "}\n",
        )
        .unwrap();
        let before = std::fs::read_to_string(&chunk).unwrap();
        let outcome = patch_sync_client(&paths).unwrap();
        assert!(outcome.contains("natively"), "outcome: {outcome}");
        assert_eq!(before, std::fs::read_to_string(&chunk).unwrap());
    }

    #[test]
    fn patch_reports_when_no_chunk() {
        let t = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::for_root(t.path());
        let outcome = patch_sync_client(&paths).unwrap();
        assert!(
            outcome.contains("no switchboard server chunk"),
            "outcome: {outcome}"
        );
    }

    #[test]
    fn write_runtime_files_writes_both() {
        let (_dir, state) = temp_paths();
        let config = ReactorConfig::default();
        write_runtime_files(&state, &config).unwrap();
        assert!(config_path(&state).exists());
        assert!(wrapper_entry(&state).exists());
    }

    #[test]
    fn write_config_is_atomic() {
        let (_dir, state) = temp_paths();
        let config = ReactorConfig::default();
        write_config(&state, &config).unwrap();
        assert!(!state
            .switchboard_dir
            .join("powerhouse.config.json.tmp")
            .exists());
        assert!(config_path(&state).exists());
    }

    #[test]
    fn is_current_requires_meta_and_entry() {
        let (_dir, state) = temp_paths();
        let spec = "@powerhousedao/switchboard@latest";
        assert!(!is_current(&state, spec));
        // meta present but no entry
        let meta = SwitchboardMeta {
            spec: spec.into(),
            version: "6.2.3".into(),
            installed_at: "2026-09-11T00:00:00Z".into(),
        };
        fs::create_dir_all(&state.switchboard_dir).unwrap();
        fs::write(meta_path(&state), serde_json::to_string(&meta).unwrap()).unwrap();
        assert!(!is_current(&state, spec));
        // entry present -> current
        let entry_dir = state
            .switchboard_dir
            .join("node_modules/@powerhousedao/switchboard/dist");
        fs::create_dir_all(&entry_dir).unwrap();
        fs::write(entry_dir.join("index.mjs"), "").unwrap();
        assert!(is_current(&state, spec));
        assert_eq!(current_meta(&state, spec).unwrap().version, "6.2.3");
        // a different spec is not current
        assert!(!is_current(&state, "@powerhousedao/switchboard@6.0.0"));
    }

    #[test]
    fn npm_path_prefers_sibling() {
        let node = NodeRuntime {
            path: PathBuf::from("/nonexistent-node-xyz/bin/node"),
            version: "v24.11.1".into(),
        };
        // no sibling -> PATH fallback
        assert_eq!(npm_path(&node), PathBuf::from("npm"));
    }

    #[test]
    fn rfc3339_now_is_well_formed() {
        let ts = rfc3339_now();
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        // year sanity
        let year: u32 = ts[0..4].parse().unwrap();
        assert!((2024..=2032).contains(&year));
    }

    /// Live: installs the real switchboard from the registry. Gated behind
    /// `PH_REACTOR_LIVE=1`; authentication for private registries comes
    /// from the ambient environment (`NPM_TOKEN` / `~/.npmrc`).
    #[tokio::test]
    async fn live_switchboard_install() {
        if std::env::var("PH_REACTOR_LIVE").is_err() {
            eprintln!("skipping live switchboard install (set PH_REACTOR_LIVE=1)");
            return;
        }
        let node = NodeRuntime {
            path: crate::bootstrap::node::probe_system()
                .await
                .map(|r| r.path)
                .unwrap_or(PathBuf::from("node")),
            version: "v24".into(),
        };
        let registry = std::env::var("PH_LIVE_REGISTRY")
            .unwrap_or_else(|_| "https://registry.dev.vetra.io".into());
        let dir = std::env::temp_dir().join(format!(
            "ph-reactor-switchboard-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let state = StatePaths::for_root(&dir);
        let meta = install(
            &state,
            &node,
            "@powerhousedao/switchboard@latest",
            &registry,
        )
        .await
        .expect("switchboard installs from the registry");
        assert!(
            meta.version
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit()),
            "installed version is not semver-like: {}",
            meta.version
        );
        assert!(wrapper_entry(&state).exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
