//! Remote drive management against the local switchboard.
//!
//! The user-facing unit is a drive URL (`https://<switchboard>/d/<slug>`).
//! Adding one: fetch the drive info (the URL is a REST endpoint returning
//! `{id, graphqlEndpoint}`), check the referenced packages against the
//! package registry, ask the local switchboard to connect (MCP
//! `addRemoteDrive`, idempotent), and poll until the drive materializes.
//! Removing one: MCP `deleteDrive`. Pausing one: `deleteDrive` with the
//! config flag set (resuming re-adds it; the sync resumes from the
//! remote's persisted cursor).
//!
//! A failed registration is classified into [`AddOutcome`]: remotes whose
//! sync endpoint rejects anonymous connections (e.g. a vetra-hosted
//! switchboard whose auth projection gates the sync channel) surface as
//! `RequiresAuth` — the published switchboard cannot present credentials
//! on its sync channels yet (upstream gap), so such drives stay
//! unregistered until a build with that wiring is installed.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use std::time::Duration;

use crate::config::DriveConfig;
use crate::mcp::Mcp;

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client builds")
});

/// What a drive URL returns when fetched (the switchboard's `/d/<slug>`
/// endpoint).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DriveInfo {
    pub id: String,
    /// GraphQL endpoint the local sync engine connects to.
    #[serde(alias = "graphqlEndpoint")]
    pub graphql_endpoint: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// The result of an attempt to register a remote drive with the local
/// switchboard. The daemon records one per configured drive and feeds it
/// into [`status_view`], which is how the user sees why a drive is not
/// syncing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddOutcome {
    /// The drive is registered; materialization may still be in flight.
    Added,
    /// The remote rejected the registration: its sync endpoint requires
    /// authentication.
    RequiresAuth(String),
    /// The registration failed for another reason (message included).
    Failed(String),
}

/// Classifies an add error: permission denials from the remote's sync
/// path become [`AddOutcome::RequiresAuth`], everything else a plain
/// [`AddOutcome::Failed`].
pub fn classify_add_error(err: &anyhow::Error) -> AddOutcome {
    let mut text = String::new();
    for cause in err.chain() {
        text.push_str(cause.to_string().as_str());
        text.push('\n');
    }
    let lowered = text.to_ascii_lowercase();
    const PERMISSION_MARKERS: [&str; 6] = [
        "forbidden",
        "insufficient permissions",
        "permission denied",
        "unauthorized",
        "access denied",
        "403",
    ];
    if PERMISSION_MARKERS.iter().any(|m| lowered.contains(m)) {
        return AddOutcome::RequiresAuth(err.to_string());
    }
    AddOutcome::Failed(err.to_string())
}

/// Live status of a configured drive (polled on demand).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveStatus {
    /// Drive document present locally and the remote is reachable.
    Synced,
    /// Drive is being materialized (added but not local yet).
    Connecting,
    /// Locally paused (drive deleted from the local reactor on purpose).
    Paused,
    /// Drive is local but the remote is unreachable.
    Offline,
    /// Registration was rejected by the remote: its sync endpoint
    /// requires authentication (see [`AddOutcome::RequiresAuth`]).
    RequiresAuth,
    /// Last probe reported an error.
    Error,
}

impl DriveStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DriveStatus::Synced => "synced",
            DriveStatus::Connecting => "connecting",
            DriveStatus::Paused => "paused",
            DriveStatus::Offline => "offline",
            DriveStatus::RequiresAuth => "requires-auth",
            DriveStatus::Error => "error",
        }
    }
}

/// A configured drive with its live status and a human detail line.
#[derive(Debug, Clone)]
pub struct DriveView {
    pub config: DriveConfig,
    pub status: DriveStatus,
    pub detail: String,
}

/// Parses a drive URL (`<base>/d/<slug>`) into its parts, mirroring the
/// reactor's `parseDriveUrl`: the GraphQL endpoint is the same base with
/// the `/d/<slug>` suffix replaced by `/graphql/r`.
pub fn parse_drive_url(raw: &str) -> Result<(String, String, String)> {
    let u = url::Url::parse(raw).with_context(|| format!("invalid drive URL: {raw}"))?;
    let mut segments = u.path().split('/').filter(|s| !s.is_empty());
    let mut d_pos: Option<usize> = None;
    for (i, seg) in segments.by_ref().enumerate() {
        if seg == "d" {
            d_pos = Some(i);
        }
    }
    let Some(d_pos) = d_pos else {
        bail!("drive URL must contain a /d/<slug> segment: {raw}");
    };
    let segments: Vec<&str> = u.path().split('/').filter(|s| !s.is_empty()).collect();
    let slug = segments
        .get(d_pos + 1)
        .context("drive URL is missing the <slug> after /d/")?;
    if slug.is_empty() {
        bail!("drive URL is missing the <slug> after /d/: {raw}");
    }
    let mut gql = u.clone();
    let base_path: String = segments
        .iter()
        .take(d_pos)
        .map(|s| format!("/{s}"))
        .collect();
    gql.set_path(&format!("{base_path}/graphql/r"));
    gql.set_query(None);
    Ok((raw.to_string(), slug.to_string(), gql.to_string()))
}

/// Fetches the drive info from the drive URL itself (optionally with a
/// bearer token, e.g. from the drive's `tokenEnv`).
pub async fn fetch_drive_info(url: &str, token: Option<&str>) -> Result<DriveInfo> {
    let mut req = HTTP.get(url);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("drive {url} answered {status}");
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .with_context(|| format!("drive {url} did not return JSON"))?;
    let info: DriveInfo = serde_json::from_value(v)
        .with_context(|| format!("drive {url} info is missing id/graphqlEndpoint"))?;
    Ok(info)
}

fn mcp(config: &crate::config::ReactorConfig, token: Option<String>) -> Mcp {
    Mcp::new(config.switchboard.port, token)
}

/// Registers a remote drive with the local switchboard (idempotent)
/// and returns the drive id, without waiting for materialization.
pub async fn add_quiet(
    config: &crate::config::ReactorConfig,
    drive: &DriveConfig,
    token: Option<String>,
) -> Result<String> {
    let (url, _slug, _expected_gql) = parse_drive_url(&drive.url)?;
    let info = fetch_drive_info(&url, token.as_deref()).await?;
    if !info.graphql_endpoint.is_empty() && !info.graphql_endpoint.starts_with("http") {
        bail!(
            "drive info returned a non-absolute graphqlEndpoint: {}",
            info.graphql_endpoint
        );
    }
    let gql_host = url::Url::parse(&info.graphql_endpoint)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string));
    let drive_host = url::Url::parse(&url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string));
    if !info.graphql_endpoint.is_empty() && gql_host != drive_host && gql_host.is_some() {
        tracing::warn!(
            "drive graphqlEndpoint host {:?} differs from drive URL host {:?}",
            gql_host,
            drive_host
        );
    }

    let m = mcp(config, token);
    m.session().await.context("MCP handshake")?;
    m.add_remote_drive(&drive.url, drive.available_offline)
        .await
        .with_context(|| format!("adding drive {}", drive.name))
}

/// Adds a remote drive to the local switchboard and waits for it to
/// materialize. Returns the drive id.
pub async fn add(
    config: &crate::config::ReactorConfig,
    drive: &DriveConfig,
    token: Option<String>,
) -> Result<String> {
    let drive_id = add_quiet(config, drive, token.clone()).await?;

    // Materialization poll: the drive document appears once the remote
    // connection has been established (bounded wait, then success anyway —
    // the sync keeps running in the switchboard).
    let m = mcp(config, token);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut last_err = String::from("waiting for the remote connection");
    loop {
        if tokio::time::Instant::now() > deadline {
            tracing::info!(
                "drive {drive_id} not yet materialized after 60s ({last_err}); continuing"
            );
            return Ok(drive_id);
        }
        match m
            .call_tool("getDrive", serde_json::json!({ "driveId": drive_id }))
            .await
        {
            Ok(r) if !r.is_error => return Ok(drive_id),
            Ok(r) => last_err = r.text.unwrap_or_default(),
            Err(e) => last_err = e.to_string(),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Finds the local drive id for a configured drive (by its URL's slug)
/// and deletes it. `true` when a local drive was removed.
pub async fn remove(
    config: &crate::config::ReactorConfig,
    drive: &DriveConfig,
    token: Option<String>,
) -> Result<bool> {
    let (_url, slug, _gql) = parse_drive_url(&drive.url)?;
    let m = mcp(config, token);
    m.session().await.context("MCP handshake")?;
    let list = m
        .call_tool("getDrives", serde_json::json!({}))
        .await
        .context("getDrives")?;
    let ids: Vec<String> = list
        .structured
        .as_ref()
        .and_then(|v| v.get("driveIds"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    for id in &ids {
        let doc = match m
            .call_tool("getDrive", serde_json::json!({ "driveId": id }))
            .await
        {
            Ok(r) if !r.is_error => r,
            _ => continue,
        };
        let doc_value = match doc.structured.clone().or_else(|| {
            doc.text
                .as_deref()
                .and_then(|t| serde_json::from_str(t).ok())
        }) {
            Some(v) => v,
            None => continue,
        };
        let doc_drive = match doc_value.get("drive") {
            Some(d) => d,
            None => continue,
        };
        let doc_slug = doc_drive
            .get("state")
            .and_then(|s| s.get("slug"))
            .or_else(|| doc_drive.get("slug"))
            .and_then(|v| v.as_str());
        if doc_slug == Some(slug.as_str()) {
            let res = m
                .call_tool("deleteDrive", serde_json::json!({ "driveId": id }))
                .await?;
            if res.is_error {
                bail!(
                    "deleteDrive({id}): {}",
                    res.text.unwrap_or_else(|| "no detail".into())
                );
            }
            return Ok(true);
        }
    }
    Ok(false)
}

/// Live view of a configured drive: paused flag, local materialization,
/// remote reachability. `outcome` is the daemon's last registration
/// attempt for this drive (see [`AddOutcome`]); for a drive that never
/// materialized locally it decides between `connecting`, `requires-auth`
/// and `error`.
pub async fn status_view(
    config: &crate::config::ReactorConfig,
    drive: &DriveConfig,
    token: Option<String>,
    outcome: Option<&AddOutcome>,
) -> DriveView {
    if drive.paused {
        return DriveView {
            config: drive.clone(),
            status: DriveStatus::Paused,
            detail: "paused; resume to re-sync".into(),
        };
    }
    let (_url, slug, _) = match parse_drive_url(&drive.url) {
        Ok(t) => t,
        Err(e) => {
            return DriveView {
                config: drive.clone(),
                status: DriveStatus::Error,
                detail: e.to_string(),
            }
        }
    };
    let m = mcp(config, token.clone());
    if m.session().await.is_err() {
        return DriveView {
            config: drive.clone(),
            status: DriveStatus::Error,
            detail: "switchboard MCP unreachable".into(),
        };
    };
    let list = match m.call_tool("getDrives", serde_json::json!({})).await {
        Ok(list) => list,
        Err(e) => {
            return DriveView {
                config: drive.clone(),
                status: DriveStatus::Error,
                detail: e.to_string(),
            };
        }
    };
    let ids: Vec<String> = list
        .structured
        .as_ref()
        .and_then(|v| v.get("driveIds"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut local = false;
    for id in &ids {
        match m
            .call_tool("getDrive", serde_json::json!({ "driveId": id }))
            .await
        {
            Ok(r) if !r.is_error => {
                let doc = r
                    .structured
                    .clone()
                    .or_else(|| r.text.as_deref().and_then(|t| serde_json::from_str(t).ok()));
                let doc_slug = doc.and_then(|v| v.get("drive").cloned()).and_then(|d| {
                    d.get("header")
                        .and_then(|h| h.get("slug"))
                        .or_else(|| d.get("state").and_then(|s| s.get("slug")))
                        .or_else(|| d.get("slug"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                });
                if doc_slug == Some(slug.to_string()) {
                    local = true;
                    break;
                }
            }
            _ => continue,
        }
    }
    if !local {
        let (status, detail) = match outcome {
            Some(AddOutcome::RequiresAuth(msg)) => (
                DriveStatus::RequiresAuth,
                format!(
                    "the remote rejected the sync registration (requires authentication): {msg}"
                ),
            ),
            Some(AddOutcome::Failed(msg)) => (DriveStatus::Error, msg.clone()),
            _ => (
                DriveStatus::Connecting,
                "added; waiting for the remote connection".into(),
            ),
        };
        return DriveView {
            config: drive.clone(),
            status,
            detail,
        };
    }
    match fetch_drive_info(&drive.url, token.as_deref()).await {
        Ok(info) => DriveView {
            config: drive.clone(),
            status: DriveStatus::Synced,
            detail: format!(
                "{} via {}",
                info.name.unwrap_or_else(|| info.id.clone()),
                info.graphql_endpoint
            ),
        },
        Err(_) => DriveView {
            config: drive.clone(),
            status: DriveStatus::Offline,
            detail: "remote unreachable".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ReactorConfig;

    #[test]
    fn drive_url_parsing() {
        let (u, slug, gql) = parse_drive_url("http://localhost:4001/d/abc123").unwrap();
        assert_eq!(u, "http://localhost:4001/d/abc123");
        assert_eq!(slug, "abc123");
        assert_eq!(gql, "http://localhost:4001/graphql/r");

        let (_, slug, gql) = parse_drive_url("https://example.com/api/reactor/d/my-drive").unwrap();
        assert_eq!(slug, "my-drive");
        assert_eq!(gql, "https://example.com/api/reactor/graphql/r");

        let (_, slug, gql) = parse_drive_url(
            "https://light-colt-c497cfbd-switchboard.vetra.io/d/powerhouse-knowledge",
        )
        .unwrap();
        assert_eq!(slug, "powerhouse-knowledge");
        assert_eq!(
            gql,
            "https://light-colt-c497cfbd-switchboard.vetra.io/graphql/r"
        );

        assert!(parse_drive_url("https://example.com/no-drive-segment").is_err());
        assert!(parse_drive_url("not a url").is_err());
        assert!(parse_drive_url("https://example.com/d/").is_err());
    }

    #[tokio::test]
    async fn status_view_offline_remote() {
        let mut config = ReactorConfig::default();
        // Closed port: the switchboard MCP is deterministically
        // unreachable, regardless of what else runs on this machine.
        config.switchboard.port = 1;
        let drive = DriveConfig {
            name: "demo".into(),
            url: "http://127.0.0.1:1/d/demo".into(), // closed port
            token_env: None,
            available_offline: false,
            paused: false,
        };
        let view = status_view(&config, &drive, None, None).await;
        assert_eq!(view.status, DriveStatus::Error);
        assert!(view.detail.contains("MCP unreachable"));
    }

    #[test]
    fn classify_add_error_separates_auth_from_other_failures() {
        // The vetra sync-channel rejection (observed): a GraphQL
        // "Forbidden - insufficient permissions..." surfaced through the
        // MCP tool error.
        let err = anyhow::Error::msg(
            "adding drive vault: MCP tool addRemoteDrive failed: \
             [GraphQL] { errors: [ { message: \"Forbidden - insufficient \
             permissions to read this document\", extensions: { code: 403 } } ] }"
                .to_string(),
        );
        assert!(matches!(
            classify_add_error(&err),
            AddOutcome::RequiresAuth(msg) if msg.contains("Forbidden")
        ));

        let err = anyhow::anyhow!("addRemoteDrive failed: 403 Forbidden");
        assert!(matches!(
            classify_add_error(&err),
            AddOutcome::RequiresAuth(_)
        ));

        let err = anyhow::anyhow!("addRemoteDrive failed: connection refused");
        assert!(matches!(classify_add_error(&err), AddOutcome::Failed(_)));
    }

    #[test]
    fn drive_status_as_str_covers_requires_auth() {
        assert_eq!(DriveStatus::RequiresAuth.as_str(), "requires-auth");
    }
}
