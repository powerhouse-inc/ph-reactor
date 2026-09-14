//! The tray's menu model: a tree of [`MenuItem`], built from a status
//! snapshot and serialized by [`super::dbusmenu`].

use crate::status::StatusSnapshot;

/// A menu node. `id` is the DBusMenu item id (assigned on build, stable
/// within a layout generation).
#[derive(Debug, Clone)]
pub struct MenuItem {
    pub id: u32,
    /// "text" (action) or "separator".
    pub kind: &'static str,
    pub label: String,
    pub enabled: bool,
    pub visible: bool,
    pub icon: Option<String>,
    /// What clicking this item does. `None` for headers and separators.
    ///
    /// Carried here rather than recovered by parsing the label: a label is a
    /// human-facing string that will be reworded, and a parser keyed on its
    /// prefix breaks silently when it is.
    pub action: Option<Action>,
    pub children: Vec<MenuItem>,
}

/// Actions the tray can dispatch to the daemon's command channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    TogglePause { name: String, to_paused: bool },
    RemoveDrive { name: String },
    OpenConsole,
    Quit,
}

/// Assigns ids in build order. 0 is the root, so items start at 1.
struct Ids(u32);

impl Ids {
    fn next(&mut self) -> u32 {
        self.0 += 1;
        self.0
    }
}

fn text(id: u32, label: impl Into<String>) -> MenuItem {
    MenuItem {
        id,
        kind: "text",
        label: label.into(),
        enabled: true,
        visible: true,
        icon: None,
        action: None,
        children: Vec::new(),
    }
}

fn header(id: u32, label: impl Into<String>) -> MenuItem {
    MenuItem {
        enabled: false,
        ..text(id, label)
    }
}

fn separator(id: u32) -> MenuItem {
    MenuItem {
        kind: "separator",
        enabled: false,
        ..text(id, "")
    }
}

/// Builds the menu:
///
/// ```text
/// ph-reactor <version> — <state>     (disabled)
/// <peer id>                          (disabled)
/// ───
/// Open console…
/// ───
/// Teams ▸   <name> — N members, M messages
/// Drives ▸  <name> — <status> ▸ Pause / Remove
/// ───
/// Quit ph-reactor
/// ```
///
/// Sections with nothing in them are omitted rather than shown empty: a
/// "Teams" submenu containing "(none)" is noise on a fresh install.
pub fn build_menu(snap: &StatusSnapshot) -> MenuItem {
    let ids = &mut Ids(0);
    let mut items = Vec::new();

    items.push(header(
        ids.next(),
        format!("ph-reactor {} — {}", snap.version, state_line(snap)),
    ));
    if let Some(peer) = &snap.reactor.peer_id {
        items.push(header(ids.next(), short_peer(peer)));
    }
    items.push(separator(ids.next()));

    let mut console = text(ids.next(), "Open console…");
    console.icon = Some("applications-internet".into());
    console.action = Some(Action::OpenConsole);
    items.push(console);

    items.push(separator(ids.next()));

    if !snap.groups.is_empty() {
        let id = ids.next();
        let children = snap
            .groups
            .iter()
            .map(|g| {
                let mut it = header(ids.next(), format!("{} — {}", g.name, group_line(g)));
                it.icon = Some("system-users".into());
                it
            })
            .collect();
        items.push(MenuItem {
            children,
            ..header(id, "Teams")
        });
    }

    let drives_id = ids.next();
    let drive_children = snap
        .drives
        .iter()
        .map(|d| {
            let id = ids.next();
            let pause = {
                let mut it = text(
                    ids.next(),
                    if d.paused { "Resume" } else { "Pause" }.to_string(),
                );
                it.action = Some(Action::TogglePause {
                    name: d.name.clone(),
                    to_paused: !d.paused,
                });
                it
            };
            let remove = {
                let mut it = text(ids.next(), "Remove");
                it.action = Some(Action::RemoveDrive {
                    name: d.name.clone(),
                });
                it
            };
            MenuItem {
                icon: Some(drive_icon(&d.status).into()),
                children: vec![pause, remove],
                ..header(id, format!("{} — {}", d.name, d.status))
            }
        })
        .collect::<Vec<_>>();
    if !drive_children.is_empty() {
        items.push(MenuItem {
            children: drive_children,
            ..header(drives_id, "Drives")
        });
        items.push(separator(ids.next()));
    }

    let mut quit = text(ids.next(), "Quit ph-reactor");
    quit.icon = Some("application-exit".into());
    quit.action = Some(Action::Quit);
    items.push(quit);

    MenuItem {
        children: items,
        ..text(0, "ph-reactor")
    }
}

/// A peer id is 52 characters; a tray menu has room for neither that nor a
/// truncation so aggressive it stops identifying the node.
fn short_peer(peer: &str) -> String {
    if peer.len() <= 20 {
        return peer.to_string();
    }
    format!("{}…{}", &peer[..12], &peer[peer.len() - 6..])
}

/// The one-line description of what the reactor is doing, shared by the menu
/// header and the tooltip title.
pub fn state_line_for(snap: &StatusSnapshot) -> String {
    state_line(snap)
}

fn state_line(snap: &StatusSnapshot) -> String {
    let r = &snap.reactor;
    if !r.running {
        return "stopped".into();
    }
    if !r.healthy {
        return "starting…".into();
    }
    if snap.drives.is_empty() {
        return "no drives".into();
    }
    if snap.drives.iter().all(|d| d.paused) {
        return "paused".into();
    }
    if snap.drives.iter().any(|d| d.status == "connecting") {
        return "connecting…".into();
    }
    if snap.drives.iter().any(|d| d.status == "synced") {
        return "synced".into();
    }
    "offline".into()
}

fn group_line(g: &crate::status::GroupSummary) -> String {
    let members = plural(g.members, "member", "members");
    let msgs = plural(g.messages, "message", "messages");
    format!("{members}, {msgs}")
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn drive_icon(status: &str) -> &'static str {
    match status {
        "synced" => "network-transmit-receive",
        "connecting" => "network-transmit",
        "paused" => "media-playback-pause",
        "offline" => "network-offline",
        _ => "dialog-error",
    }
}

/// The action carried by the item with `id`, if any.
pub fn action_for_id(root: &MenuItem, id: u32) -> Option<Action> {
    super::dbusmenu::find(root, id as i32).and_then(
        |i| {
            if i.enabled {
                i.action.clone()
            } else {
                None
            }
        },
    )
}

/// Finds an item by its exact label. Test and diagnostic helper — dispatch
/// goes by id.
pub fn find_by_label<'a>(root: &'a MenuItem, label: &str) -> Option<&'a MenuItem> {
    if root.label == label {
        return Some(root);
    }
    root.children.iter().find_map(|c| find_by_label(c, label))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{DriveStatusEntry, GroupSummary};

    fn snap() -> StatusSnapshot {
        let mut s = StatusSnapshot::empty(
            "1.0.0".into(),
            "/ip4/0.0.0.0/tcp/25422".into(),
            "http://127.0.0.1:4002".into(),
        );
        // empty() models a daemon that is not up; these fixtures describe a
        // running one, which is the state the menu is actually drawn in.
        s.reactor.running = true;
        s.reactor.healthy = true;
        s.reactor.peer_id = Some("12D3KooWBEVh11BAbHuJ5fEUZGKzZTjeYoNutC4uCq2UeGMxSWod".into());
        s.drives.push(DriveStatusEntry {
            name: "vault".into(),
            addr: "/ip4/10.0.0.2/tcp/25422/p2p/12D3KooWg8111".into(),
            paused: false,
            status: "synced".into(),
            detail: "ok".into(),
        });
        s.drives.push(DriveStatusEntry {
            name: "cold".into(),
            addr: "/ip4/10.0.0.3/tcp/25422/p2p/12D3KooWg8222".into(),
            paused: true,
            status: "paused".into(),
            detail: "paused".into(),
        });
        s
    }

    fn collect_ids(item: &MenuItem, out: &mut Vec<u32>) {
        out.push(item.id);
        for c in &item.children {
            collect_ids(c, out);
        }
    }

    #[test]
    fn ids_are_unique_across_the_whole_tree() {
        let root = build_menu(&snap());
        let mut ids = Vec::new();
        collect_ids(&root, &mut ids);
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate ids break host dispatch");
        assert_eq!(root.id, 0, "the root must be id 0");
    }

    #[test]
    fn actions_dispatch_by_id_not_by_label() {
        let mut root = build_menu(&snap());
        let quit = find_by_label(&root, "Quit ph-reactor").expect("quit").id;
        assert_eq!(action_for_id(&root, quit), Some(Action::Quit));

        // Reword every label: dispatch must be unaffected. Under the previous
        // label-parsing scheme this silently returned None.
        fn reword(i: &mut MenuItem) {
            i.label = format!("renamed {}", i.id);
            for c in &mut i.children {
                reword(c);
            }
        }
        reword(&mut root);
        assert_eq!(action_for_id(&root, quit), Some(Action::Quit));
    }

    #[test]
    fn pause_and_resume_reflect_the_drive_state() {
        let root = build_menu(&snap());
        let vault = find_by_label(&root, "vault — synced").expect("vault");
        let pause = vault.children.iter().find(|c| c.label == "Pause").unwrap();
        assert_eq!(
            action_for_id(&root, pause.id),
            Some(Action::TogglePause {
                name: "vault".into(),
                to_paused: true
            })
        );

        // "cold" is already paused, so its item offers Resume.
        let cold = find_by_label(&root, "cold — paused").expect("cold");
        let resume = cold.children.iter().find(|c| c.label == "Resume").unwrap();
        assert_eq!(
            action_for_id(&root, resume.id),
            Some(Action::TogglePause {
                name: "cold".into(),
                to_paused: false
            })
        );
    }

    #[test]
    fn disabled_items_carry_no_action() {
        let root = build_menu(&snap());
        let headers: Vec<_> = root.children.iter().filter(|c| !c.enabled).collect();
        assert!(!headers.is_empty());
        for h in headers {
            assert_eq!(action_for_id(&root, h.id), None);
        }
    }

    #[test]
    fn teams_appear_when_present_and_are_omitted_when_not() {
        let root = build_menu(&snap());
        assert!(
            find_by_label(&root, "Teams").is_none(),
            "no groups means no Teams section"
        );

        let mut s = snap();
        s.groups.push(GroupSummary {
            name: "powerhouse".into(),
            members: 2,
            messages: 1,
        });
        let root = build_menu(&s);
        assert!(find_by_label(&root, "Teams").is_some());
        assert!(find_by_label(&root, "powerhouse — 2 members, 1 message").is_some());
    }

    #[test]
    fn singular_and_plural_counts_read_correctly() {
        let mut s = snap();
        s.groups.push(GroupSummary {
            name: "solo".into(),
            members: 1,
            messages: 0,
        });
        let root = build_menu(&s);
        assert!(find_by_label(&root, "solo — 1 member, 0 messages").is_some());
    }

    #[test]
    fn peer_id_is_shortened_but_still_identifies_the_node() {
        let root = build_menu(&snap());
        let peer = "12D3KooWBEVh11BAbHuJ5fEUZGKzZTjeYoNutC4uCq2UeGMxSWod";
        let short = short_peer(peer);
        assert!(find_by_label(&root, &short).is_some());
        assert!(short.len() < peer.len());
        assert!(short.starts_with("12D3KooWBEVh"));
        assert!(short.ends_with("MxSWod"));
    }

    #[test]
    fn state_line_describes_what_the_reactor_is_doing() {
        let mut s = snap();
        assert_eq!(state_line(&s), "synced");

        s.drives.iter_mut().for_each(|d| d.paused = true);
        assert_eq!(state_line(&s), "paused");

        let mut s2 = snap();
        s2.drives[0].status = "connecting".into();
        assert_eq!(state_line(&s2), "connecting…");

        let mut s3 = snap();
        s3.drives.clear();
        assert_eq!(state_line(&s3), "no drives");

        let mut s4 = snap();
        s4.reactor.healthy = false;
        assert_eq!(state_line(&s4), "starting…");
    }

    #[test]
    fn console_item_opens_the_console() {
        let root = build_menu(&snap());
        let c = find_by_label(&root, "Open console…").expect("console item");
        assert_eq!(action_for_id(&root, c.id), Some(Action::OpenConsole));
    }
}
