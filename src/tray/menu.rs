//! The tray's DBusMenu model: menu items and the XML layout served over
//! `com.canonical.dbusmenu`.

use crate::status::StatusSnapshot;

/// A menu node. `id` is the DBusMenu item id (assigned on build, stable
/// within a layout generation).
#[derive(Debug, Clone)]
pub struct MenuItem {
    pub id: u32,
    /// "text" (action), "checkbox", or "separator".
    pub kind: &'static str,
    pub label: String,
    pub enabled: bool,
    pub visible: bool,
    pub checked: bool,
    pub icon: Option<String>,
    pub children: Vec<MenuItem>,
}

/// Builds the menu from a status snapshot:
///
/// ```text
/// <drives root>
///   <drive name>  [icon by status]
///     Pause / Resume
///     Remove
///   (or "Add a drive…" when none)
/// Settings…
/// Quit
/// ```
///
/// Item ids are sequential from 1 (0 is reserved for the root). The
/// action ids encode the command so the tray can dispatch them without a
/// lookup table.
pub fn build_menu(snap: &StatusSnapshot) -> MenuItem {
    let mut items = Vec::new();
    let mut next_id = 1u32;

    items.push(MenuItem {
        id: next_id,
        kind: "text",
        label: format!("ph-reactor {} — {}", snap.version, reactor_line(snap)),
        enabled: false,
        visible: true,
        checked: false,
        icon: None,
        children: Vec::new(),
    });
    next_id += 1;
    items.push(separator(&mut next_id));

    // Drives section.
    let drives_header = next_id;
    next_id += 1;
    let mut drive_children = Vec::new();
    if snap.drives.is_empty() {
        drive_children.push(MenuItem {
            id: next_id,
            kind: "text",
            label: "(no drives configured — see the settings page)".into(),
            enabled: false,
            visible: true,
            checked: false,
            icon: None,
            children: Vec::new(),
        });
        next_id += 1;
    } else {
        for d in &snap.drives {
            let drive_id = next_id;
            next_id += 1;
            let pause_id = next_id;
            next_id += 1;
            let remove_id = next_id;
            next_id += 1;
            let icon = match d.status.as_str() {
                "synced" => Some("network-transmit".to_string()),
                "connecting" => Some("network-idle".to_string()),
                "paused" => Some("media-playlist-stop".to_string()),
                "offline" => Some("network-offline".to_string()),
                _ => Some("dialog-error".to_string()),
            };
            drive_children.push(MenuItem {
                id: drive_id,
                kind: "text",
                label: format!("{} — [{}]", d.name, d.status),
                enabled: false,
                visible: true,
                checked: false,
                icon,
                children: vec![
                    MenuItem {
                        id: pause_id,
                        kind: "text",
                        label: if d.paused {
                            format!("resume {}", d.name)
                        } else {
                            format!("pause {}", d.name)
                        },
                        enabled: true,
                        visible: true,
                        checked: false,
                        icon: None,
                        children: Vec::new(),
                    },
                    MenuItem {
                        id: remove_id,
                        kind: "text",
                        label: format!("remove {}", d.name),
                        enabled: true,
                        visible: true,
                        checked: false,
                        icon: None,
                        children: Vec::new(),
                    },
                ],
            });
        }
    }
    items.push(MenuItem {
        id: drives_header,
        kind: "text",
        label: "Drives".into(),
        enabled: false,
        visible: true,
        checked: false,
        icon: None,
        children: drive_children,
    });

    let settings_id = next_id;
    next_id += 1;
    let quit_id = next_id;
    let _ = next_id;
    items.push(MenuItem {
        id: settings_id,
        kind: "text",
        label: "Open settings…".into(),
        enabled: true,
        visible: true,
        checked: false,
        icon: Some("preferences-system".to_string()),
        children: Vec::new(),
    });
    items.push(MenuItem {
        id: quit_id,
        kind: "text",
        label: "Quit ph-reactor".into(),
        enabled: true,
        visible: true,
        checked: false,
        icon: Some("application-exit".to_string()),
        children: Vec::new(),
    });

    // The DBusMenu Layout has a single root (id 0).
    MenuItem {
        id: 0,
        kind: "text",
        label: "ph-reactor".into(),
        enabled: true,
        visible: true,
        checked: false,
        icon: None,
        children: items,
    }
}

fn reactor_line(snap: &StatusSnapshot) -> String {
    let r = &snap.reactor;
    if !r.running {
        "stopped".into()
    } else if r.healthy {
        "running".into()
    } else {
        "starting…".into()
    }
}

fn separator(next_id: &mut u32) -> MenuItem {
    let it = MenuItem {
        id: *next_id,
        kind: "separator",
        label: String::new(),
        enabled: false,
        visible: true,
        checked: false,
        icon: None,
        children: Vec::new(),
    };
    *next_id += 1;
    it
}

/// Maps a menu item id to the action it carries, given the layout
/// generation the id came from. The tray keeps the last built menu and
/// resolves ids against it.
pub fn action_for_id(root: &MenuItem, id: u32) -> Option<Action> {
    if root.id == id {
        return action_of(root);
    }
    for child in &root.children {
        if let Some(a) = action_for_id(child, id) {
            return Some(a);
        }
    }
    None
}

fn action_of(item: &MenuItem) -> Option<Action> {
    if !item.enabled || item.kind == "separator" {
        return None;
    }
    let label = item.label.as_str();
    if let Some(name) = label.strip_prefix("pause ") {
        Some(Action::TogglePause {
            name: name.to_string(),
            to_paused: true,
        })
    } else if let Some(name) = label.strip_prefix("resume ") {
        Some(Action::TogglePause {
            name: name.to_string(),
            to_paused: false,
        })
    } else if let Some(name) = label.strip_prefix("remove ") {
        Some(Action::RemoveDrive {
            name: name.to_string(),
        })
    } else if label.starts_with("Open settings") {
        Some(Action::OpenSettings)
    } else if label.starts_with("Quit") {
        Some(Action::Quit)
    } else {
        None
    }
}

/// Actions the tray can dispatch to the daemon's command channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    TogglePause { name: String, to_paused: bool },
    RemoveDrive { name: String },
    OpenSettings,
    Quit,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::DriveStatusEntry;

    fn snap() -> StatusSnapshot {
        let mut s = StatusSnapshot::empty(
            "0.1.0".into(),
            "/ip4/0.0.0.0/tcp/4201".into(),
            "http://127.0.0.1:4002".into(),
        );
        s.drives.push(DriveStatusEntry {
            name: "vault".into(),
            addr: "/ip4/10.0.0.2/tcp/4201/p2p/12D3KooWg8111".into(),
            paused: false,
            status: "synced".into(),
            detail: "ok".into(),
        });
        s.drives.push(DriveStatusEntry {
            name: "cold".into(),
            addr: "/ip4/10.0.0.3/tcp/4201/p2p/12D3KooWg8222".into(),
            paused: true,
            status: "paused".into(),
            detail: "paused".into(),
        });
        s
    }

    #[test]
    fn menu_shape_and_actions() {
        let root = build_menu(&snap());
        assert_eq!(root.id, 0);
        assert!(root.children.len() >= 4);

        // ids are unique
        let mut ids = Vec::new();
        fn collect(item: &MenuItem, ids: &mut Vec<u32>) {
            ids.push(item.id);
            for c in &item.children {
                collect(c, ids);
            }
        }
        collect(&root, &mut ids);
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate item ids");

        let xml = root.xml();
        assert!(xml.contains("<dbustree"));
        assert!(xml.contains("Drives"));
        assert!(xml.contains("vault — [synced]"));
        assert!(xml.contains("pause vault"));
        // "cold" is paused, so its action reads "resume cold"
        assert!(xml.contains("resume cold"));
        assert!(!xml.contains("pause cold"));

        // action resolution
        let quit = root
            .children
            .iter()
            .find(|i| i.label.starts_with("Quit"))
            .unwrap();
        assert_eq!(action_for_id(&root, quit.id), Some(Action::Quit));
        let pause_vault = root
            .children
            .iter()
            .find(|i| i.label == "Drives")
            .and_then(|d| {
                d.children
                    .iter()
                    .find(|c| c.label.starts_with("vault"))
                    .and_then(|v| v.children.iter().find(|c| c.label.starts_with("pause")))
            })
            .expect("pause item for vault");
        assert_eq!(
            action_for_id(&root, pause_vault.id),
            Some(Action::TogglePause {
                name: "vault".into(),
                to_paused: true
            })
        );
    }
}
