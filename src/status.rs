//! The shared status snapshot: what the tray menu, the settings page,
//! and `ph-reactor status` all display. The daemon refreshes it (on an
//! interval and after every command) and fans it out via a watch
//! channel.
//!
//! The JSON shape (the `status --json` contract consumed by shell
//! integrations):
//! ```json
//! {
//!   "version": "1.0.0",
//!   "reactor": {
//!     "running": true, "healthy": true,
//!     "peer_id": "12D3Koo…", "listen": "/ip4/0.0.0.0/tcp/4201",
//!     "docs": 12, "last_event": "op applied: note-1"
//!   },
//!   "drives": [
//!     { "name": "vault", "addr": "/ip4/10.0.0.2/tcp/4201/p2p/12D3Koo…",
//!       "paused": false, "status": "synced", "detail": "" }
//!   ],
//!   "settings": { "url": "http://127.0.0.1:4002" },
//!   "updated_at": "2026-09-12T12:00:00Z"
//! }
//! ```

use serde::{Deserialize, Serialize};

/// A full snapshot of the daemon's observable state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub version: String,
    /// The native reactor core (the p2p engine + local store).
    pub reactor: ReactorStatus,
    pub drives: Vec<DriveStatusEntry>,
    pub settings: SettingsStatus,
    /// RFC 3339 UTC of the last refresh.
    pub updated_at: String,
}

/// The p2p sync core: up, its identity, its doc count.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactorStatus {
    /// The daemon process is running.
    pub running: bool,
    /// The p2p engine is listening (a bound listener or a successful
    /// identity is healthy enough for drive sync to work).
    pub healthy: bool,
    /// This instance's peer id (base58).
    pub peer_id: Option<String>,
    /// The configured listen multiaddr.
    pub listen: String,
    /// Live docs in the local store.
    pub docs: u64,
    /// Most recent noteworthy event (one line, for the tray menu and
    /// the settings page).
    pub last_event: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveStatusEntry {
    pub name: String,
    /// The drive's multiaddr.
    pub addr: String,
    pub paused: bool,
    /// One of: synced | connecting | paused | offline | requires-auth | error
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsStatus {
    pub url: String,
}

impl StatusSnapshot {
    pub fn empty(version: String, listen: String, settings_url: String) -> Self {
        Self {
            version,
            reactor: ReactorStatus {
                running: false,
                healthy: false,
                peer_id: None,
                listen,
                docs: 0,
                last_event: None,
            },
            drives: Vec::new(),
            settings: SettingsStatus { url: settings_url },
            updated_at: rfc3339_now(),
        }
    }
}

/// The current time as RFC 3339 UTC (`2026-09-12T12:00:00Z`).
pub fn rfc3339_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    rfc3339(secs)
}

/// Formats Unix epoch seconds as an RFC3339 UTC string.
pub fn rfc3339(secs: u64) -> String {
    // civil-from-days (Howard Hinnant's algorithm)
    let days = (secs / 86400) as i64;
    let rem = (secs % 86400) as u32;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Renders the human-facing block used by `ph-reactor status` and the
/// settings page header.
pub fn render_text(snap: &StatusSnapshot) -> String {
    let mut out = String::new();
    let r = &snap.reactor;
    let state = if !r.running {
        "stopped"
    } else if r.healthy {
        "healthy"
    } else {
        "starting"
    };
    out.push_str(&format!(
        "ph-reactor {}  (settings: {})\n",
        snap.version, snap.settings.url
    ));
    out.push_str(&format!(
        "  reactor: {state}  ({} docs, peer {})\n      listen: {}\n",
        r.docs,
        r.peer_id.as_deref().unwrap_or("?"),
        r.listen,
    ));
    if let Some(ev) = &r.last_event {
        out.push_str(&format!("      last event: {ev}\n"));
    }
    if snap.drives.is_empty() {
        out.push_str("  drives: (none configured)\n");
    } else {
        out.push_str("  drives:\n");
        for d in &snap.drives {
            out.push_str(&format!(
                "    {}  [{}]\n      {}\n      {}\n",
                d.name, d.status, d.addr, d.detail
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StatusSnapshot {
        StatusSnapshot {
            version: "1.0.0".into(),
            reactor: ReactorStatus {
                running: true,
                healthy: true,
                peer_id: Some("12D3KooWg8111".into()),
                listen: "/ip4/0.0.0.0/tcp/4201".into(),
                docs: 3,
                last_event: Some("op applied: note-1".into()),
            },
            drives: vec![DriveStatusEntry {
                name: "vault".into(),
                addr: "/ip4/10.0.0.2/tcp/4201/p2p/12D3KooWg8111".into(),
                paused: false,
                status: "synced".into(),
                detail: String::new(),
            }],
            settings: SettingsStatus {
                url: "http://127.0.0.1:4002".into(),
            },
            updated_at: "2026-09-12T00:00:00Z".into(),
        }
    }

    #[test]
    fn snapshot_json_shape() {
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&sample()).unwrap()).unwrap();
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
        assert_eq!(r["running"], true);
        assert_eq!(r["docs"], 3);
        let d = &v["drives"][0];
        for key in ["name", "addr", "paused", "status", "detail"] {
            assert!(d.get(key).is_some(), "missing drive key {key}");
        }
        assert_eq!(d["status"], "synced");
    }

    #[test]
    fn rfc3339_is_correct_for_known_instants() {
        // Expected values verified against `date -u -d @<secs>`.
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1609459200), "2021-01-01T00:00:00Z");
        assert_eq!(rfc3339(1789346096), "2026-09-14T00:34:56Z");
    }

    #[test]
    fn render_text_lists_drives_and_state() {
        let text = render_text(&sample());
        assert!(text.contains("healthy"));
        assert!(text.contains("vault  [synced]"));
        assert!(text.contains("/ip4/10.0.0.2/tcp/4201/p2p/12D3KooWg8111"));
    }
}
