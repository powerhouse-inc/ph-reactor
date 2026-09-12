//! The shared status snapshot: what the tray menu, the settings page, and
//! `ph status` all display. The daemon's poller refreshes it (on an
//! interval and after every command) and fans it out via a watch channel.

use serde::{Deserialize, Serialize};

/// A full snapshot of the daemon's observable state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub version: String,
    pub switchboard: SwitchboardStatus,
    pub drives: Vec<DriveStatusEntry>,
    pub settings: SettingsStatus,
    /// RFC 3339 UTC of the last refresh.
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchboardStatus {
    pub running: bool,
    pub healthy: bool,
    /// The installed switchboard version (None until bootstrap completes).
    pub version: Option<String>,
    pub port: u16,
    pub restarts: u32,
    pub last_event: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveStatusEntry {
    pub name: String,
    pub url: String,
    pub paused: bool,
    /// One of: synced | connecting | paused | offline | error
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsStatus {
    pub url: String,
}

impl StatusSnapshot {
    pub fn empty(version: String, port: u16, settings_url: String) -> Self {
        Self {
            version,
            switchboard: SwitchboardStatus {
                running: false,
                healthy: false,
                version: None,
                port,
                restarts: 0,
                last_event: None,
            },
            drives: Vec::new(),
            settings: SettingsStatus { url: settings_url },
            updated_at: crate::bootstrap::switchboard::rfc3339_now(),
        }
    }
}

/// Renders the human-facing block used by `ph status` and the settings
/// page header.
pub fn render_text(snap: &StatusSnapshot) -> String {
    let mut out = String::new();
    let sb = &snap.switchboard;
    let state = if !sb.running {
        "stopped"
    } else if sb.healthy {
        "healthy"
    } else {
        "starting"
    };
    out.push_str(&format!(
        "ph-reactor {}  (settings: {})\n",
        snap.version, snap.settings.url
    ));
    out.push_str(&format!(
        "  switchboard: {state} (port {}, version {}, restarts {})\n",
        sb.port,
        sb.version.as_deref().unwrap_or("?"),
        sb.restarts
    ));
    if snap.drives.is_empty() {
        out.push_str("  drives: (none configured)\n");
    } else {
        out.push_str("  drives:\n");
        for d in &snap.drives {
            out.push_str(&format!(
                "    {}  [{}]  {}\n      {}\n      {}\n",
                d.name, d.status, d.url, d.detail, "token: via env when configured"
            ));
        }
    }
    out
}
