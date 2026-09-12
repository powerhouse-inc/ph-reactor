//! Daemon configuration (`<state>/config.json`).
//!
//! One file, schema version 1, camelCase on the wire. First run writes the
//! defaults; a corrupt file is moved aside (`config.json.corrupt-<ts>`) and
//! replaced by fresh defaults; unknown fields are preserved verbatim via a
//! flattened map so future versions never lose operator input.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::paths::StatePaths;

pub const CONFIG_VERSION: u32 = 1;

pub const DEFAULT_REGISTRY_URL: &str = "https://registry.dev.vetra.io";
pub const DEFAULT_NPM_REGISTRY: &str = "https://registry.npmjs.org";
pub const DEFAULT_SWITCHBOARD_SPEC: &str = "@powerhousedao/switchboard@latest";
pub const DEFAULT_BOOT_PACKAGES: &[&str] = &["@powerhousedao/knowledge-note"];

pub const LOG_LEVELS: &[&str] = &["verbose", "debug", "info", "warn", "error", "silent"];

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io: {0}")]
    Io(#[source] std::io::Error),
    #[error("invalid config JSON: {0}")]
    Parse(String),
    #[error("invalid value for {key}: {why}")]
    InvalidValue { key: String, why: String },
}

impl From<std::io::Error> for ConfigError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct NodeConfig {
    /// Minimum Node major version (inclusive) the daemon will accept from
    /// the system; below this it bootstraps a private runtime.
    #[serde(rename = "minimumVersion")]
    pub minimum_version: String,
    #[serde(rename = "preferSystem")]
    pub prefer_system: bool,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            minimum_version: "24".into(),
            prefer_system: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SwitchboardConfig {
    pub port: u16,
    /// npm spec for `@powerhousedao/switchboard` (name, name@tag, name@version).
    #[serde(rename = "packageSpec")]
    pub package_spec: String,
    /// Registry the switchboard package itself is installed from.
    #[serde(rename = "npmRegistry")]
    pub npm_registry: String,
    pub node: NodeConfig,
}

impl Default for SwitchboardConfig {
    fn default() -> Self {
        Self {
            port: 4001,
            package_spec: DEFAULT_SWITCHBOARD_SPEC.into(),
            npm_registry: DEFAULT_NPM_REGISTRY.into(),
            node: NodeConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DriveConfig {
    pub name: String,
    /// Drive REST URL, e.g. `https://<switchboard>/d/<slug>`.
    pub url: String,
    /// Name of the env var holding a Renown bearer token for this drive's
    /// switchboard (the token itself is never stored here).
    #[serde(rename = "tokenEnv", skip_serializing_if = "Option::is_none", default)]
    pub token_env: Option<String>,
    #[serde(rename = "availableOffline")]
    pub available_offline: bool,
    pub paused: bool,
}

impl Default for DriveConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            url: String::new(),
            token_env: None,
            available_offline: true,
            paused: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SettingsConfig {
    pub host: String,
    pub port: u16,
}

impl Default for SettingsConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 4002,
        }
    }
}

/// The top-level configuration document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ReactorConfig {
    pub version: u32,
    pub switchboard: SwitchboardConfig,
    /// Powerhouse package registry (document models), e.g.
    /// `https://registry.dev.vetra.io`.
    pub registry: String,
    /// Boot packages installed from the registry when the switchboard starts.
    pub packages: Vec<String>,
    pub drives: Vec<DriveConfig>,
    pub settings: SettingsConfig,
    #[serde(rename = "logLevel")]
    pub log_level: String,
    /// Unknown fields are preserved verbatim (forward compatibility).
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for ReactorConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            switchboard: SwitchboardConfig::default(),
            registry: DEFAULT_REGISTRY_URL.into(),
            packages: DEFAULT_BOOT_PACKAGES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            drives: Vec::new(),
            settings: SettingsConfig::default(),
            log_level: "info".into(),
            extra: BTreeMap::new(),
        }
    }
}

impl ReactorConfig {
    /// The subset of configuration that changes how the switchboard
    /// process is spawned. When it changes, the daemon respawns the
    /// switchboard; when only `drives` change, the running switchboard
    /// is updated in place (no restart). The drives' `tokenEnv` names
    /// are included because the generated boot wrapper bakes in the
    /// matching `PH_DRIVE_TOKEN_<i>` environment variable names (the
    /// values are resolved at spawn time and are deliberately not part
    /// of the fingerprint).
    pub fn process_fingerprint(&self) -> Value {
        serde_json::json!({
            "port": self.switchboard.port,
            "packageSpec": self.switchboard.package_spec,
            "npmRegistry": self.switchboard.npm_registry,
            "node": self.switchboard.node,
            "registry": self.registry,
            "packages": self.packages,
            "logLevel": self.log_level,
            "settings": self.settings,
            "driveTokenEnv": self
                .drives
                .iter()
                .map(|d| d.token_env.clone())
                .collect::<Vec<_>>(),
        })
    }
}

// ---------------------------------------------------------------------------
// Load / save
// ---------------------------------------------------------------------------

/// Loads the config. A missing file is created with defaults; a corrupt
/// file is preserved as `<file>.corrupt-<unix-ts>` and replaced. The bool
/// is true when the file was quarantined or freshly defaulted, so
/// adopting callers can refuse to clobber a good in-memory config with
/// replacement defaults.
pub fn load_quiet(paths: &StatePaths) -> Result<(ReactorConfig, bool), ConfigError> {
    match fs::read_to_string(&paths.config_file) {
        Ok(text) => match serde_json::from_str::<ReactorConfig>(&text) {
            Ok(mut config) => {
                config.version = config.version.max(1);
                Ok((config, false))
            }
            Err(err) => {
                quarantine(&paths.config_file, &err)?;
                let fresh = ReactorConfig::default();
                save(paths, &fresh)?;
                tracing::warn!(
                    "corrupt config at {} ({}); replaced with defaults",
                    paths.config_file.display(),
                    err
                );
                Ok((fresh, true))
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let fresh = ReactorConfig::default();
            save(paths, &fresh)?;
            tracing::info!(
                "no config at {}; wrote defaults",
                paths.config_file.display()
            );
            Ok((fresh, false))
        }
        Err(err) => Err(err.into()),
    }
}

pub fn load(paths: &StatePaths) -> Result<ReactorConfig, ConfigError> {
    let (config, _) = load_quiet(paths)?;
    Ok(config)
}

/// Atomic write: tmp file in the same directory, `0600`, rename over.
pub fn save(paths: &StatePaths, config: &ReactorConfig) -> Result<(), ConfigError> {
    let text =
        serde_json::to_string_pretty(config).map_err(|err| ConfigError::Parse(err.to_string()))?;
    let mut text = text;
    text.push('\n');
    atomic_write(&paths.config_file, &text)?;
    Ok(())
}

/// Shared atomic-write helper: tmp file in the same directory, `0600`,
/// rename over the target. Used by `save` and by the switchboard's
/// `powerhouse.config.json` writer.
pub(crate) fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!("{name}.tmp"));
    fs::write(&tmp, content)?;
    let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    fs::rename(&tmp, path)?;
    Ok(())
}

fn quarantine(file: &Path, err: &serde_json::Error) -> Result<(), ConfigError> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let bad = file.with_file_name(format!(
        "{}.corrupt-{}",
        file.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config"),
        ts
    ));
    if let Err(mv_err) = fs::rename(file, &bad) {
        tracing::warn!("could not quarantine corrupt config: {mv_err}");
    }
    let _ = err;
    Ok(())
}

// ---------------------------------------------------------------------------
// Dotted-key updates (CLI `config set`)
// ---------------------------------------------------------------------------

/// Sets a dotted key (`switchboard.port`, `registry`, `logLevel`, …) from a
/// JSON value; validates the value before committing.
pub fn set(config: &mut ReactorConfig, key: &str, value: &Value) -> Result<(), ConfigError> {
    match key {
        "switchboard.port" => set_port("switchboard.port", &mut config.switchboard.port, value)?,
        "switchboard.packageSpec" => {
            set_string(key, &mut config.switchboard.package_spec, value, |s| {
                !s.trim().is_empty()
            })?
        }
        "switchboard.npmRegistry" => set_url(key, &mut config.switchboard.npm_registry, value)?,
        "switchboard.node.minimumVersion" => set_string(
            key,
            &mut config.switchboard.node.minimum_version,
            value,
            |s| {
                s.split('.')
                    .next()
                    .and_then(|maj| maj.parse::<u32>().ok())
                    .is_some()
            },
        )?,
        "switchboard.node.preferSystem" => {
            config.switchboard.node.prefer_system = bool_value(key, value)?
        }
        "registry" => set_url(key, &mut config.registry, value)?,
        "packages" => {
            let list = value
                .as_array()
                .ok_or_else(|| invalid(key, "must be a JSON array of package name strings"))?;
            let mut out = Vec::with_capacity(list.len());
            for entry in list {
                let s = entry
                    .as_str()
                    .ok_or_else(|| invalid(key, "entries must be strings"))?;
                out.push(s.to_string());
            }
            config.packages = out;
        }
        "drives" => set_drives(key, &mut config.drives, value)?,
        "settings.host" => set_string(key, &mut config.settings.host, value, |_| true)?,
        "settings.port" => set_port("settings.port", &mut config.settings.port, value)?,
        "logLevel" => set_string(key, &mut config.log_level, value, |s| {
            LOG_LEVELS.contains(&s)
        })?,
        "version" => return Err(invalid(key, "cannot be changed")),
        _ => {
            // Forward-compat: unknown keys land in the preserved map.
            config.extra.insert(key.to_string(), value.clone());
        }
    }
    Ok(())
}

fn invalid(key: &str, why: &str) -> ConfigError {
    ConfigError::InvalidValue {
        key: key.to_string(),
        why: why.to_string(),
    }
}

fn set_string(
    key: &str,
    target: &mut String,
    value: &Value,
    valid: impl Fn(&str) -> bool,
) -> Result<(), ConfigError> {
    let s = value
        .as_str()
        .ok_or_else(|| invalid(key, "must be a string"))?;
    if !valid(s) {
        return Err(invalid(key, "value rejected by validator"));
    }
    *target = s.to_string();
    Ok(())
}

fn set_url(key: &str, target: &mut String, value: &Value) -> Result<(), ConfigError> {
    let s = value
        .as_str()
        .ok_or_else(|| invalid(key, "must be a string"))?;
    if url::Url::parse(s).is_err() || !s.starts_with("http") {
        return Err(invalid(key, "must be an http(s) URL"));
    }
    *target = s.trim_end_matches('/').to_string();
    Ok(())
}

fn set_port(key: &str, target: &mut u16, value: &Value) -> Result<(), ConfigError> {
    let n = value
        .as_u64()
        .ok_or_else(|| invalid(key, "must be an integer"))?;
    if !(1..=65535).contains(&n) {
        return Err(invalid(key, "must be 1..=65535"));
    }
    *target = n as u16;
    Ok(())
}

fn bool_value(key: &str, value: &Value) -> Result<bool, ConfigError> {
    value
        .as_bool()
        .ok_or_else(|| invalid(key, "must be a boolean"))
}

fn set_drives(key: &str, target: &mut Vec<DriveConfig>, value: &Value) -> Result<(), ConfigError> {
    let list = value
        .as_array()
        .ok_or_else(|| invalid(key, "must be a JSON array"))?;
    let mut out = Vec::with_capacity(list.len());
    for entry in list {
        let drive: DriveConfig = serde_json::from_value(entry.clone())
            .map_err(|err| invalid(key, &format!("drive entry: {err}")))?;
        if drive.url.is_empty() {
            return Err(invalid(key, "drive entries require a non-empty url"));
        }
        out.push(drive);
    }
    *target = out;
    Ok(())
}

/// Renders the config as it would be written (pretty JSON, trailing newline).
pub fn render(config: &ReactorConfig) -> String {
    let mut text = serde_json::to_string_pretty(config).expect("config serializes");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::StatePaths;

    fn temp_paths() -> (tempfile::TempDir, StatePaths) {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::for_root(dir.path());
        (dir, paths)
    }

    #[test]
    fn missing_file_creates_defaults() {
        let (_tmp, paths) = temp_paths();
        let config = load(&paths).unwrap();
        assert_eq!(config, ReactorConfig::default());
        assert!(paths.config_file.exists());
        // reload is stable
        assert_eq!(load(&paths).unwrap(), config);
    }

    #[test]
    fn corrupt_file_is_quarantined_and_replaced() {
        let (_tmp, paths) = temp_paths();
        fs::write(&paths.config_file, "{ not json").unwrap();
        let config = load(&paths).unwrap();
        assert_eq!(config, ReactorConfig::default());
        let quarantined: Vec<_> = fs::read_dir(&paths.root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("config.json.corrupt-"))
            .collect();
        assert_eq!(quarantined.len(), 1);
    }

    #[test]
    fn save_is_atomic_and_private() {
        let (_tmp, paths) = temp_paths();
        let config = ReactorConfig {
            log_level: "debug".into(),
            ..ReactorConfig::default()
        };
        save(&paths, &config).unwrap();
        let meta = fs::metadata(&paths.config_file).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(load(&paths).unwrap(), config);
    }

    #[test]
    fn unknown_fields_survive_round_trip() {
        let (_tmp, paths) = temp_paths();
        let raw = r#"{
            "version": 1,
            "logLevel": "warn",
            "futureField": {"a": 1}
        }"#;
        fs::write(&paths.config_file, raw).unwrap();
        let config = load(&paths).unwrap();
        assert_eq!(config.log_level, "warn");
        assert_eq!(
            config.extra.get("futureField"),
            Some(&Value::Object(serde_json::Map::from_iter(
                [("a".into(), Value::from(1))].into_iter()
            )))
        );
        save(&paths, &config).unwrap();
        let reloaded = load(&paths).unwrap();
        assert_eq!(reloaded.extra, config.extra);
    }

    #[test]
    fn set_validates_and_updates() {
        let mut config = ReactorConfig::default();
        set(&mut config, "switchboard.port", &Value::from(4321)).unwrap();
        assert_eq!(config.switchboard.port, 4321);
        set(&mut config, "switchboard.port", &Value::from(0)).unwrap_err();
        set(&mut config, "logLevel", &Value::from("verbose")).unwrap();
        assert_eq!(config.log_level, "verbose");
        set(&mut config, "logLevel", &Value::from("nope")).unwrap_err();
        set(
            &mut config,
            "registry",
            &Value::from("https://registry.dev.vetra.io/"),
        )
        .unwrap();
        assert_eq!(config.registry, "https://registry.dev.vetra.io");
        set(&mut config, "registry", &Value::from("not a url")).unwrap_err();
        set(
            &mut config,
            "packages",
            &serde_json::json!(["@powerhousedao/a", "@powerhousedao/b"]),
        )
        .unwrap();
        assert_eq!(config.packages.len(), 2);
        set(&mut config, "packages", &Value::from("nope")).unwrap_err();
        set(&mut config, "version", &Value::from(2)).unwrap_err();
    }

    #[test]
    fn set_drives_validates_url() {
        let mut config = ReactorConfig::default();
        set(
            &mut config,
            "drives",
            &serde_json::json!([
                {
                    "name": "Vault",
                    "url": "https://light-colt-c497cfbd-switchboard.vetra.io/d/powerhouse-knowledge"
                }
            ]),
        )
        .unwrap();
        assert_eq!(config.drives.len(), 1);
        assert!(config.drives[0].available_offline);
        assert!(!config.drives[0].paused);
        set(&mut config, "drives", &serde_json::json!([{"name": "x"}])).unwrap_err();
    }

    #[test]
    fn default_shape_matches_spec() {
        let config = ReactorConfig::default();
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["switchboard"]["port"], 4001);
        assert_eq!(
            json["switchboard"]["packageSpec"],
            "@powerhousedao/switchboard@latest"
        );
        assert_eq!(
            json["switchboard"]["npmRegistry"],
            "https://registry.npmjs.org"
        );
        assert_eq!(json["switchboard"]["node"]["minimumVersion"], "24");
        assert_eq!(json["registry"], "https://registry.dev.vetra.io");
        assert_eq!(json["packages"][0], "@powerhousedao/knowledge-note");
        assert_eq!(json["settings"]["host"], "127.0.0.1");
        assert_eq!(json["settings"]["port"], 4002);
        assert_eq!(json["logLevel"], "info");
    }
}
