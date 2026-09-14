//! Daemon configuration (`<state>/config.json`).
//!
//! One file, schema version 2, camelCase on the wire. First run writes
//! the defaults; a corrupt file is moved aside (`config.json.corrupt-<ts>`)
//! and replaced by fresh defaults; unknown fields are preserved
//! verbatim via a flattened map so future versions never lose operator
//! input.
//!
//! Schema:
//! ```json
//! {
//!   "schemaVersion": 2,
//!   "instance": { "name": "my-mac", "listen": "/ip4/0.0.0.0/tcp/4201",
//!                  "listenWs": "/ip4/0.0.0.0/tcp/25423/ws",
//!                  "external": ["/dns4/ws.example/tcp/443/tls/ws"] },
//!   "p2p": { "mdns": true, "tokenEnv": null },
//!   "drives": [
//!     {
//!       "name": "vault",
//!       "addr": "/ip4/10.0.0.2/tcp/4201/p2p/12D3Koo...",
//!       "tokenEnv": "VAULT_TOKEN",
//!       "paused": false,
//!       "availableOffline": true
//!     }
//!   ],
//!   "settings": { "host": "127.0.0.1", "port": 4002 },
//!   "logLevel": "info"
//! }
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::paths::StatePaths;

pub const CONFIG_VERSION: u32 = 2;
/// Default libp2p listen address (all interfaces, TCP 4201).
pub const DEFAULT_LISTEN: &str = "/ip4/0.0.0.0/tcp/4201";
/// Default instance name (shown in handshakes and status pages).
pub const DEFAULT_INSTANCE_NAME: &str = "reactor";

pub const LOG_LEVELS: &[&str] = &["verbose", "debug", "info", "warn", "error", "silent"];

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("config: {0}")]
    Invalid(String),
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct InstanceConfig {
    /// Human name of this instance (shown to peers in handshakes).
    pub name: String,
    /// The libp2p listen multiaddr.
    pub listen: String,
    /// An optional second listen multiaddr for the WebSocket transport,
    /// e.g. `/ip4/0.0.0.0/tcp/25423/ws`.
    ///
    /// Plain `ws`, not `wss`: the expected deployment terminates TLS at a
    /// reverse proxy and forwards a plain WebSocket to this process. Peers
    /// still dial `/dns4/<host>/tcp/443/tls/ws/...` -- TLS is a transport
    /// concern handled by the proxy, so the two ends agree.
    ///
    /// Empty (the default) means the WebSocket listener is not started.
    #[serde(default, rename = "listenWs", skip_serializing_if = "Option::is_none")]
    pub listen_ws: Option<String>,
    /// Multiaddrs this node should advertise to peers, for when the address
    /// others must dial is not one this process can observe -- behind a load
    /// balancer, a reverse proxy, or NAT.
    ///
    /// Without this a node announces only what it is bound to, which for a
    /// container is a private address no peer can reach. Each entry is passed
    /// to `Swarm::add_external_address`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external: Vec<String>,
}

impl Default for InstanceConfig {
    fn default() -> Self {
        Self {
            name: DEFAULT_INSTANCE_NAME.into(),
            listen: DEFAULT_LISTEN.into(),
            listen_ws: None,
            external: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct P2pConfig {
    /// Enable mDNS for LAN peer discovery.
    pub mdns: bool,
    /// Env var name of the global token gate: inbound hellos from
    /// peers without a drive entry are rejected unless their token
    /// matches this variable's value.
    #[serde(rename = "tokenEnv")]
    pub token_env: Option<String>,
    /// Enable the Kademlia DHT: peer routing, provider records, and
    /// bootstrap discovery over the relay/loopback network.
    pub dht: bool,
    /// Enable the circuit relay (client and server) for NAT traversal.
    pub relay: bool,
    /// Bootstrap peers as full multiaddrs (`/ip4/…/tcp/…/p2p/<peer-id>`).
    /// Used to seed the DHT on a node that knows no one yet.
    pub bootstraps: Vec<String>,
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            mdns: true,
            token_env: None,
            dht: true,
            relay: false,
            bootstraps: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DriveConfig {
    pub name: String,
    /// Multiaddr of the remote peer (see `drives::Drive`).
    pub addr: String,
    /// Env var name holding the shared token (value never persisted).
    #[serde(rename = "tokenEnv")]
    pub token_env: Option<String>,
    pub paused: bool,
    #[serde(rename = "availableOffline")]
    pub available_offline: bool,
}

impl Default for DriveConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            addr: String::new(),
            token_env: None,
            paused: false,
            available_offline: true,
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

/// The OpenAI-compatible LLM endpoint (see the spec's "New config").
/// `apiKeyEnv` is the environment variable that holds the key; the value is
/// read from the environment at call time and never written to disk (the
/// same `tokenEnv` convention the drives use).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LlmConfig {
    /// The OpenAI-compatible base URL (ending in the version prefix).
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    /// The env-var name holding the API key (never the value itself).
    #[serde(rename = "apiKeyEnv")]
    pub api_key_env: String,
    /// The model id sent to the endpoint.
    pub model: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".into(),
            api_key_env: "LLM_API_KEY".into(),
            model: "gpt-4o-mini".into(),
        }
    }
}

/// Self-update behaviour.
///
/// `check` is on by default and `auto` is off, which is the asymmetry the
/// plugin system already establishes: knowing an update exists costs nothing,
/// and applying one is a decision. A binary has no sandbox and no capability
/// list, so it would be incoherent to demand consent for a plugin and not for
/// the daemon itself.
///
/// `auto` exists for nodes nobody is watching — a cluster reactor with no tray
/// and no operator at the console. Turning it on says: this key is trusted
/// enough that a build signed by it may replace the running binary unattended.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UpdateConfig {
    /// Look for newer releases on start and while running.
    pub check: bool,
    /// Apply a verified newer release from a trusted publisher without asking.
    pub auto: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            check: true,
            auto: false,
        }
    }
}

/// The top-level configuration document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ReactorConfig {
    #[serde(rename = "schemaVersion")]
    pub version: u32,
    pub instance: InstanceConfig,
    pub p2p: P2pConfig,
    pub drives: Vec<DriveConfig>,
    pub settings: SettingsConfig,
    pub llm: LlmConfig,
    #[serde(default)]
    pub update: UpdateConfig,
    #[serde(rename = "logLevel")]
    pub log_level: String,
    /// Unknown fields, preserved verbatim (forward compatibility).
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for ReactorConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            instance: InstanceConfig::default(),
            p2p: P2pConfig::default(),
            drives: Vec::new(),
            settings: SettingsConfig::default(),
            llm: LlmConfig::default(),
            update: UpdateConfig::default(),
            log_level: "info".into(),
            extra: BTreeMap::new(),
        }
    }
}

impl ReactorConfig {
    /// Fingerprint of the fields that require a daemon restart to take
    /// effect (listen address, mdns, token gate, settings endpoint, log
    /// level).
    pub fn process_fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.instance.listen.hash(&mut h);
        self.instance.name.hash(&mut h);
        self.p2p.mdns.hash(&mut h);
        self.p2p.token_env.hash(&mut h);
        self.settings.host.hash(&mut h);
        self.settings.port.hash(&mut h);
        self.log_level.hash(&mut h);
        h.finish()
    }

    /// Validate the whole document. Returns the first problem found.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.instance.name.trim().is_empty() {
            return Err(invalid("instance.name", "must be a non-empty string"));
        }
        if let Err(e) = libp2p::Multiaddr::from_str(&self.instance.listen) {
            return Err(invalid(
                "instance.listen",
                &format!("not a valid multiaddr: {e}"),
            ));
        }
        for d in &self.drives {
            if d.name.trim().is_empty() {
                return Err(invalid("drives[].name", "must be non-empty"));
            }
            if let Err(e) = libp2p::Multiaddr::from_str(&d.addr) {
                return Err(invalid(
                    &format!("drives[{}].addr", d.name),
                    &format!("not a valid multiaddr: {e}"),
                ));
            }
        }
        if !LOG_LEVELS.contains(&self.log_level.as_str()) {
            return Err(invalid(
                "logLevel",
                &format!("expected one of: {}", LOG_LEVELS.join(", ")),
            ));
        }
        if self.llm.base_url.trim().is_empty()
            || !(self.llm.base_url.starts_with("http://")
                || self.llm.base_url.starts_with("https://"))
        {
            return Err(invalid("llm.baseUrl", "must be an http(s) base URL"));
        }
        if self.llm.model.trim().is_empty() {
            return Err(invalid("llm.model", "must be a non-empty string"));
        }
        Ok(())
    }
}

use std::str::FromStr;

// ---------------------------------------------------------------------------
// Load / save
// ---------------------------------------------------------------------------

/// Loads the config. A missing file is created with defaults; a corrupt
/// file is preserved as `<file>.corrupt-<unix-ts>` and replaced. The
/// bool is true when the file was quarantined or freshly defaulted, so
/// adopting callers can refuse to clobber a good in-memory config with
/// replacement defaults.
pub fn load_quiet(paths: &StatePaths) -> Result<(ReactorConfig, bool), ConfigError> {
    let file = &paths.config_file;
    match std::fs::read_to_string(file) {
        Ok(raw) => match serde_json::from_str::<ReactorConfig>(&raw) {
            Ok(config) => {
                config.validate()?;
                Ok((config, false))
            }
            Err(err) => {
                quarantine(file, &err)?;
                let config = default_with_version();
                std::fs::write(file, render(&config))?;
                set_config_mode(file, 0o600);
                Ok((config, true))
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let config = default_with_version();
            std::fs::write(file, render(&config))?;
            set_config_mode(file, 0o600);
            Ok((config, true))
        }
        Err(e) => Err(ConfigError::Io(e)),
    }
}

pub fn load(paths: &StatePaths) -> Result<ReactorConfig, ConfigError> {
    Ok(load_quiet(paths)?.0)
}

/// The defaults a fresh state dir gets.
fn default_with_version() -> ReactorConfig {
    ReactorConfig {
        version: CONFIG_VERSION,
        ..Default::default()
    }
}

/// Atomic write: tmp file in the same directory, `0600`, rename over.
pub fn save(paths: &StatePaths, config: &ReactorConfig) -> Result<(), ConfigError> {
    atomic_write(&paths.config_file, &render(config))?;
    set_config_mode(&paths.config_file, 0o600);
    Ok(())
}

/// Shared atomic-write helper: tmp file in the same directory, `0600`,
/// rename over the target.
pub(crate) fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent"))?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|f| f.to_str()).unwrap_or("f")
    ));
    std::fs::write(&tmp, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)
}

fn set_config_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = mode;
}

fn quarantine(file: &Path, err: &serde_json::Error) -> Result<(), ConfigError> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let q = file.with_file_name(format!(
        "{}.corrupt-{ts}",
        file.file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("config")
    ));
    let _ = std::fs::rename(file, &q);
    tracing::warn!(
        "quarantined corrupt config {} -> {} ({err})",
        file.display(),
        q.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Dotted-key updates (CLI `config set`, settings API)
// ---------------------------------------------------------------------------

/// Sets a dotted key (`instance.listen`, `p2p.mdns`, `logLevel`, …) from
/// a JSON value; validates the value before committing. Drive list
/// changes go through the drive commands.
pub fn set(config: &mut ReactorConfig, key: &str, value: &Value) -> Result<(), ConfigError> {
    match key {
        "instance.name" => {
            set_string("instance.name", &mut config.instance.name, value, |s| {
                !s.trim().is_empty()
            })?;
        }
        "instance.listen" => {
            set_string("instance.listen", &mut config.instance.listen, value, |s| {
                libp2p::Multiaddr::from_str(s).is_ok()
            })?;
        }
        "p2p.mdns" => {
            config.p2p.mdns = bool_value("p2p.mdns", value)?;
        }
        "p2p.tokenEnv" => match value {
            Value::Null => config.p2p.token_env = None,
            Value::String(s) => config.p2p.token_env = Some(s.clone()),
            _ => return Err(invalid("p2p.tokenEnv", "expected a string or null")),
        },
        "settings.host" => {
            set_string("settings.host", &mut config.settings.host, value, |s| {
                !s.is_empty()
            })?;
        }
        "settings.port" => {
            set_port("settings.port", &mut config.settings.port, value)?;
        }
        "update.check" => match value {
            Value::Bool(b) => config.update.check = *b,
            _ => return Err(invalid("update.check", "expected a boolean")),
        },
        "update.auto" => match value {
            Value::Bool(b) => config.update.auto = *b,
            _ => return Err(invalid("update.auto", "expected a boolean")),
        },
        "logLevel" => {
            let s = match value {
                Value::String(s) => s.clone(),
                _ => return Err(invalid("logLevel", "expected a string")),
            };
            if !LOG_LEVELS.contains(&s.as_str()) {
                return Err(invalid(
                    "logLevel",
                    &format!("expected one of: {}", LOG_LEVELS.join(", ")),
                ));
            }
            config.log_level = s;
        }
        "llm.baseUrl" => {
            set_string("llm.baseUrl", &mut config.llm.base_url, value, |s| {
                s.starts_with("http://") || s.starts_with("https://")
            })?;
        }
        "llm.apiKeyEnv" => match value {
            Value::Null => config.llm.api_key_env = String::new(),
            Value::String(s) => config.llm.api_key_env = s.clone(),
            _ => return Err(invalid("llm.apiKeyEnv", "expected a string or null")),
        },
        "llm.model" => {
            set_string("llm.model", &mut config.llm.model, value, |s| !s.trim().is_empty())?;
        }
        _ => {
            return Err(invalid(
                key,
                "unknown key (allowed: instance.name, instance.listen, p2p.mdns, p2p.tokenEnv, llm.baseUrl, llm.apiKeyEnv, llm.model, settings.host, settings.port, update.check, update.auto, logLevel)",
            ))
        }
    }
    config.validate()
}

fn invalid(key: &str, why: &str) -> ConfigError {
    ConfigError::Invalid(format!("{key}: {why}"))
}

fn set_string(
    key: &str,
    target: &mut String,
    value: &Value,
    valid: impl Fn(&str) -> bool,
) -> Result<(), ConfigError> {
    let s = match value {
        Value::String(s) => s.clone(),
        _ => return Err(invalid(key, "expected a string")),
    };
    if !valid(&s) {
        return Err(invalid(key, "invalid value"));
    }
    *target = s;
    Ok(())
}

fn set_port(key: &str, target: &mut u16, value: &Value) -> Result<(), ConfigError> {
    let n = match value.as_u64() {
        Some(n) => n,
        None => return Err(invalid(key, "expected a number")),
    };
    if !(1..=65535).contains(&n) {
        return Err(invalid(key, "port must be 1..=65535"));
    }
    *target = n as u16;
    Ok(())
}

fn bool_value(key: &str, value: &Value) -> Result<bool, ConfigError> {
    value
        .as_bool()
        .ok_or_else(|| invalid(key, "expected a boolean"))
}

/// Renders the config as it would be written (pretty JSON, trailing
/// newline).
pub fn render(config: &ReactorConfig) -> String {
    let mut s = serde_json::to_string_pretty(config).expect("config serializes");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths() -> (tempfile::TempDir, StatePaths) {
        let t = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::for_root(t.path());
        (t, paths)
    }

    #[test]
    fn load_creates_defaults_when_missing() {
        let (_t, paths) = temp_paths();
        let (config, fresh) = load_quiet(&paths).unwrap();
        assert!(fresh);
        assert_eq!(config.version, CONFIG_VERSION);
        assert_eq!(config.instance.listen, DEFAULT_LISTEN);
        assert!(paths.config_file.exists());
    }

    #[test]
    fn load_quarantines_corrupt_file() {
        let (_t, paths) = temp_paths();
        std::fs::write(&paths.config_file, "{ not json").unwrap();
        let (config, fresh) = load_quiet(&paths).unwrap();
        assert!(fresh);
        assert_eq!(config.version, CONFIG_VERSION);
        let raw = std::fs::read_to_string(&paths.config_file).unwrap();
        assert!(raw.starts_with('{'));
        assert!(raw.contains("\"schemaVersion\": 2"));
    }

    #[test]
    fn unknown_fields_are_preserved() {
        let (_t, paths) = temp_paths();
        let mut config = load_quiet(&paths).unwrap().0;
        config
            .extra
            .insert("future.thing".into(), Value::String("x".into()));
        save(&paths, &config).unwrap();
        let re = load(&paths).unwrap();
        assert_eq!(
            re.extra.get("future.thing"),
            Some(&Value::String("x".into()))
        );
    }

    #[test]
    fn set_validates_listen_as_multiaddr() {
        let mut config = ReactorConfig::default();
        set(
            &mut config,
            "instance.listen",
            &Value::String("/ip4/0.0.0.0/tcp/4202".into()),
        )
        .unwrap();
        assert_eq!(config.instance.listen, "/ip4/0.0.0.0/tcp/4202");
        assert!(set(
            &mut config,
            "instance.listen",
            &Value::String("nonsense".into())
        )
        .is_err());
    }

    #[test]
    fn set_rejects_bad_loglevel_and_port() {
        let mut config = ReactorConfig::default();
        assert!(set(&mut config, "logLevel", &Value::String("loud".into())).is_err());
        set(&mut config, "logLevel", &Value::String("debug".into())).unwrap();
        assert!(set(&mut config, "settings.port", &Value::from(0)).is_err());
        set(&mut config, "settings.port", &Value::from(9000)).unwrap();
        assert_eq!(config.settings.port, 9000);
    }

    #[test]
    fn set_token_env_accepts_string_and_null() {
        let mut config = ReactorConfig::default();
        set(&mut config, "p2p.tokenEnv", &Value::String("GATE".into())).unwrap();
        assert_eq!(config.p2p.token_env.as_deref(), Some("GATE"));
        set(&mut config, "p2p.tokenEnv", &Value::Null).unwrap();
        assert!(config.p2p.token_env.is_none());
    }

    #[test]
    fn llm_config_defaults_set_and_round_trips() {
        // Defaults.
        let config = ReactorConfig::default();
        assert_eq!(config.llm.base_url, "https://api.openai.com/v1");
        assert_eq!(config.llm.api_key_env, "LLM_API_KEY");
        assert_eq!(config.llm.model, "gpt-4o-mini");

        // set(): valid values.
        let mut c = ReactorConfig::default();
        set(
            &mut c,
            "llm.baseUrl",
            &Value::String("https://my.llm.local/v1".into()),
        )
        .unwrap();
        assert_eq!(c.llm.base_url, "https://my.llm.local/v1");
        set(&mut c, "llm.apiKeyEnv", &Value::String("MY_KEY".into())).unwrap();
        assert_eq!(c.llm.api_key_env, "MY_KEY");
        set(&mut c, "llm.model", &Value::String("gpt-4o".into())).unwrap();
        assert_eq!(c.llm.model, "gpt-4o");

        // set(): validation rejects a non-URL base and an empty model.
        assert!(set(&mut c, "llm.baseUrl", &Value::String("not-a-url".into())).is_err());
        assert!(set(&mut c, "llm.model", &Value::String("".into())).is_err());
        // apiKeyEnv: null clears it.
        set(&mut c, "llm.apiKeyEnv", &Value::Null).unwrap();
        assert_eq!(c.llm.api_key_env, "");

        // Round-trip through save/load preserves the section.
        let (_t, paths) = temp_paths();
        let mut saved = ReactorConfig::default();
        saved.llm.model = "test-model".into();
        saved.llm.base_url = "http://127.0.0.1:9999/v1".into();
        save(&paths, &saved).unwrap();
        let re = load(&paths).unwrap();
        assert_eq!(re.llm.model, "test-model");
        assert_eq!(re.llm.base_url, "http://127.0.0.1:9999/v1");
    }

    #[test]
    fn fingerprint_changes_on_listen_and_token_env() {
        let a = ReactorConfig::default();
        let mut b = a.clone();
        assert_eq!(a.process_fingerprint(), b.process_fingerprint());
        b.instance.listen = "/ip4/0.0.0.0/tcp/4202".into();
        assert_ne!(a.process_fingerprint(), b.process_fingerprint());
        b = a.clone();
        b.p2p.token_env = Some("X".into());
        assert_ne!(a.process_fingerprint(), b.process_fingerprint());
        // drive changes do not affect the fingerprint
        b = a.clone();
        b.drives.push(DriveConfig {
            name: "v".into(),
            addr: "/ip4/10.0.0.1/tcp/4201".into(),
            ..Default::default()
        });
        assert_eq!(a.process_fingerprint(), b.process_fingerprint());
    }
}
