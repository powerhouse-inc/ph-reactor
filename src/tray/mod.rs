//! Status-bar tray via `org.kde.StatusNotifierItem` + `com.canonical.dbusmenu`
//! over the session bus (zbus, no GTK).
//!
//! Registration: when a StatusNotifierWatcher is present (KDE/GNOME
//! shell indicators) the item registers with it; otherwise the item owns
//! the well-known fallback name `org.kde.StatusNotifierItem-<uid>-<n>`,
//! which indicator implementations scan for. In headless environments
//! (no session bus) the daemon runs without a tray.

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};
use zbus::connection::Connection;
use zbus::names::WellKnownName;
use zbus::object_server::SignalContext;
use zbus::zvariant::ObjectPath;

use crate::commands::Command;

pub mod menu;
use crate::status::StatusSnapshot;
use menu::{Action, MenuItem};

// The spec's conventional paths. These are not arbitrary: a host that is
// handed a BUS NAME (which is how we register) looks the item up at
// `/StatusNotifierItem`. A custom path is only reachable when registering by
// path, which not every host implements.
const SNI_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/StatusNotifierItem/Menu";

/// The result of starting the tray.
pub enum Tray {
    /// Running; [`Tray::stop`] halts it.
    Running {
        stop: watch::Sender<bool>,
        task: tokio::task::JoinHandle<()>,
    },
    /// No session bus (headless) — the daemon continues without a tray.
    Headless(String),
}

impl Tray {
    pub fn stop(&self) {
        if let Tray::Running { stop, .. } = self {
            let _ = stop.send(true);
        }
    }

    pub fn is_headless(&self) -> bool {
        matches!(self, Tray::Headless(_))
    }
}

struct TrayState {
    snapshot: StatusSnapshot,
    menu: MenuItem,
    commands: mpsc::UnboundedSender<Command>,
}

/// SNI object.
#[derive(Clone)]
struct Sni {
    state: Arc<Mutex<TrayState>>,
}

/// DBusMenu object.
#[derive(Clone)]
struct DbusMenu {
    state: Arc<Mutex<TrayState>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem", spawn = false)]
impl Sni {
    #[zbus(property, name = "Id")]
    fn id(&self) -> &str {
        "ph-reactor"
    }

    #[zbus(property)]
    fn title(&self) -> &str {
        "ph-reactor"
    }

    #[zbus(property)]
    fn category(&self) -> &str {
        "Communication"
    }

    /// Active while the reactor is healthy; Attention otherwise.
    #[zbus(property)]
    fn status(&self) -> &str {
        let s = self.state.lock();
        if s.snapshot.reactor.healthy {
            "Active"
        } else {
            "Attention"
        }
    }

    /// Themed icon, resolved by the desktop theme.
    #[zbus(property)]
    fn icon_name(&self) -> &str {
        let s = self.state.lock();
        if s.snapshot.reactor.healthy {
            "network-server"
        } else {
            "dialog-warning"
        }
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> &str {
        ""
    }

    /// The menu's object path.
    ///
    /// This is `o` (an object path) in the SNI spec, NOT `s`. Returning a
    /// string makes the property unreadable to a host expecting `o`, so the
    /// icon appears but right-clicking it produces nothing. Every working
    /// tray item on a KDE session returns `o` here.
    ///
    /// `MENU_PATH` is a compile-time constant proven valid by
    /// `menu_path_is_a_valid_object_path`; the fallback exists only so a
    /// malformed constant could never panic a running daemon.
    #[zbus(property)]
    fn menu(&self) -> ObjectPath<'_> {
        ObjectPath::try_from(MENU_PATH).unwrap_or_default()
    }

    #[zbus(property, name = "ItemActivationRequested")]
    fn item_activation_requested(&self) -> u32 {
        0
    }

    #[zbus(property)]
    fn tool_tip(&self) -> String {
        let s = self.state.lock();
        let r = &s.snapshot.reactor;
        format!(
            "<b>ph-reactor</b> — {} ({} docs)",
            if r.healthy { "syncing" } else { "not ready" },
            r.docs
        )
    }

    #[zbus(property)]
    fn attention_icon_name(&self) -> &str {
        ""
    }

    fn activate(&self, _x: i32, _y: i32) {}
    fn secondary_activate(&self, _x: i32, _y: i32) {
        let url = self.state.lock().snapshot.settings.url.clone();
        if url.is_empty() {
            return;
        }
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
    }
    fn context_menu(&self, _x: i32, _y: i32) {}
    fn scroll(&self, _amount: i32, _direction: &str) {}
    fn set_show_menu(&self, _show: bool) {}

    #[zbus(signal)]
    async fn updated(signal_ctxt: &SignalContext<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_status(signal_ctxt: &SignalContext<'_>, status: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_icon_name(signal_ctxt: &SignalContext<'_>, icon_name: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_title(signal_ctxt: &SignalContext<'_>, title: &str) -> zbus::Result<()>;
}

// com.canonical.dbusmenu, not org.kde.DBusMenu: the canonical name is what
// hosts import. KDE's own tray items expose com.canonical.dbusmenu, and a
// host that cannot find that interface simply shows no menu.
#[zbus::interface(name = "com.canonical.dbusmenu", spawn = false)]
impl DbusMenu {
    #[zbus(property)]
    fn version(&self) -> u32 {
        2
    }

    #[zbus(property)]
    fn layout(&self) -> String {
        self.state.lock().menu.xml()
    }

    /// The shell calls this before showing the menu: refresh the layout
    /// from the latest snapshot.
    async fn about_to_show(&self, _parent_id: u32) -> zbus::fdo::Result<()> {
        let mut st = self.state.lock();
        st.menu = menu::build_menu(&st.snapshot);
        Ok(())
    }

    /// `event_id` 2 = itemActivated: dispatch the item's action.
    async fn event(
        &self,
        id: u32,
        event_id: u32,
        _data: &str,
        _uuid: u32,
    ) -> zbus::fdo::Result<()> {
        if event_id == 2 {
            let action = {
                let st = self.state.lock();
                menu::action_for_id(&st.menu, id)
            };
            if let Some(action) = action {
                dispatch_action(&self.state, action).await;
            }
        }
        Ok(())
    }

    fn event_removed(&self, _id: u32, _event_id: u32, _data: &str, _uuid: u32) {}

    #[zbus(signal)]
    async fn item_activated(
        signal_ctxt: &SignalContext<'_>,
        id: u32,
        event_id: u32,
        data: &str,
        uuid: u32,
    ) -> zbus::Result<()>;
}

/// Executes a menu action: sends the command and/or opens the settings.
async fn dispatch_action(state: &Arc<Mutex<TrayState>>, action: Action) {
    let send = |cmd: Command| {
        let st = state.lock();
        let _ = st.commands.send(cmd);
    };
    match action {
        Action::TogglePause { name, to_paused } => {
            if to_paused {
                send(Command::PauseDrive { name });
            } else {
                send(Command::ResumeDrive { name });
            }
        }
        Action::RemoveDrive { name } => send(Command::RemoveDrive { name }),
        Action::OpenSettings => {
            let url = state.lock().snapshot.settings.url.clone();
            if !url.is_empty() {
                let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
            }
        }
        Action::Quit => send(Command::Quit),
    }
}

impl MenuItem {
    /// The DBusMenu `Layout` XML for this subtree (root id 0 implied).
    pub fn xml(&self) -> String {
        let mut out = String::from("<dbustree version=\"1.0\"><layout>");
        out.push_str(&format!(r#"<id version="1">{}"#, self.id));
        write_item(&mut out, self);
        out.push_str("</layout></layout></dbustree>");
        out
    }
}

fn write_item(out: &mut String, item: &menu::MenuItem) {
    if item.kind == "separator" {
        out.push_str(r#"</id><type>separator</type>"#);
        return;
    }
    out.push_str(&format!("</id><type>{}</type>", item.kind));
    if !item.enabled {
        out.push_str(r#"<enable>false</enable>"#);
    }
    if !item.visible {
        out.push_str(r#"<visible>false</visible>"#);
    }
    out.push_str(&format!("<label>{}</label>", xml_escape(&item.label)));
    if let Some(icon) = &item.icon {
        out.push_str(&format!(
            r#"<attribs><icon-name>{}</icon-name></attribs>"#,
            icon
        ));
    }
    if !item.children.is_empty() {
        out.push_str("<layout>");
        for child in &item.children {
            out.push_str(&format!(r#"<id version="1">{}"#, child.id));
            write_item(out, child);
        }
        out.push_str("</layout>");
    }
    out.push_str("</layout>");
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

async fn run(
    conn: Arc<Connection>,
    mut snapshot: watch::Receiver<StatusSnapshot>,
    stop: watch::Receiver<bool>,
    commands: mpsc::UnboundedSender<Command>,
) {
    let snap = snapshot.borrow_and_update().clone();
    let state = Arc::new(Mutex::new(TrayState {
        snapshot: snap.clone(),
        menu: menu::build_menu(&snap),
        commands,
    }));
    let sni = Sni {
        state: state.clone(),
    };
    let mnu = DbusMenu {
        state: state.clone(),
    };

    let server = conn.object_server();
    if !server.at(SNI_PATH, sni.clone()).await.unwrap_or(false) {
        tracing::warn!("SNI object already at {SNI_PATH}; not registered");
    }
    if !server.at(MENU_PATH, mnu.clone()).await.unwrap_or(false) {
        tracing::warn!("DBusMenu object already at {MENU_PATH}; not registered");
    }

    // Own the well-known name FIRST, then hand that name to the watcher.
    //
    // Order matters: the watcher resolves the name we give it and queries the
    // item on it, so the name has to exist before we register. Registering by
    // name (rather than by object path) is also what the GNOME AppIndicator
    // extension expects, so this works on both desktops.
    let uid = nix_uid();
    let name = format!("org.kde.StatusNotifierItem-{uid}-1");
    match WellKnownName::try_from(name.clone()) {
        Ok(n) => match conn.request_name(n).await {
            Ok(_) => tracing::debug!("took tray name {name}"),
            Err(err) => {
                tracing::warn!("cannot take tray name {name}: {err:#}");
                return;
            }
        },
        Err(err) => {
            tracing::warn!("bad tray name {name}: {err}");
            return;
        }
    }

    let watcher = "org.kde.StatusNotifierWatcher";
    match raw_register(conn.as_ref(), watcher, &name).await {
        Ok(()) => tracing::info!("registered {name} with the status notifier watcher"),
        // Not fatal: with no watcher running, some hosts still discover the
        // item by scanning for the well-known name we already own.
        Err(err) => tracing::warn!(
            "watcher registration failed ({err:#}); \
             the tray name is held, so a host that scans for it can still find us"
        ),
    }

    // Initial property advertisement.
    let ctxt = match SignalContext::new(conn.as_ref(), SNI_PATH) {
        Ok(c) => Some(c),
        Err(err) => {
            tracing::warn!("cannot build signal context: {err:#}");
            None
        }
    };
    if let Some(ctxt) = &ctxt {
        let _ = Sni::new_status(ctxt, "Active").await;
        let _ = Sni::new_icon_name(ctxt, "network-server").await;
        let _ = Sni::new_title(ctxt, "ph-reactor").await;
    }

    let mut rx_snap = snapshot;
    let mut rx_stop = stop;
    loop {
        tokio::select! {
            changed = rx_stop.changed() => {
                if changed.is_ok() && *rx_stop.borrow() {
                    break;
                }
            }
            changed = rx_snap.changed() => {
                if changed.is_err() {
                    break;
                }
                let snap = rx_snap.borrow_and_update().clone();
                let healthy = snap.reactor.healthy;
                {
                    let mut st = state.lock();
                    st.snapshot = snap.clone();
                    st.menu = menu::build_menu(&snap);
                }
                if let Some(ctxt) = &ctxt {
                    let status = if healthy { "Active" } else { "Attention" };
                    let icon = if healthy {
                        "network-server"
                    } else {
                        "dialog-warning"
                    };
                    let _ = Sni::new_status(ctxt, status).await;
                    let _ = Sni::new_icon_name(ctxt, icon).await;
                    let _ = Sni::updated(ctxt).await;
                }
            }
        }
    }

    let _ = server.remove::<Sni, _>(SNI_PATH).await;
    let _ = server.remove::<DbusMenu, _>(MENU_PATH).await;
    if let Ok(name) = WellKnownName::try_from(format!("org.kde.StatusNotifierItem-{uid}-1")) {
        let _ = conn.release_name(name).await;
    }
}

/// `RegisterStatusNotifierItem` as a raw method call (avoids a proxy
/// derive for a one-shot call).
///
/// The argument is a STRING, not an object path. Passing an `ObjectPath`
/// builds the call with signature `o` and every real implementation rejects
/// it -- KDE answers `UnknownMethod: No such method 'RegisterStatusNotifierItem'
/// ... (signature 'o')`, which reads like a missing method rather than the
/// type error it is.
///
/// The method also returns nothing, so there is no reply body to deserialize.
async fn raw_register(conn: &Connection, watcher: &str, service: &str) -> anyhow::Result<()> {
    conn.call_method(
        Some(watcher),
        "/StatusNotifierWatcher",
        Some(watcher),
        "RegisterStatusNotifierItem",
        &(service,),
    )
    .await?;
    Ok(())
}

fn nix_uid() -> u32 {
    unsafe { libc::getuid() }
}

/// Starts the tray. Never fails: headless environments yield
/// [`Tray::Headless`] and the daemon keeps running.
pub async fn start(
    snapshot: watch::Receiver<StatusSnapshot>,
    commands: mpsc::UnboundedSender<Command>,
) -> Tray {
    let conn = match Connection::session().await {
        Ok(c) => Arc::new(c),
        Err(err) => return Tray::Headless(format!("no session bus: {err:#}")),
    };
    let (stop_tx, stop_rx) = watch::channel(false);
    let handle = tokio::spawn({
        let conn = conn.clone();
        async move {
            run(conn, snapshot, stop_rx, commands).await;
        }
    });
    Tray::Running {
        stop: stop_tx,
        task: handle,
    }
}

#[cfg(test)]
mod path_tests {
    use super::{MENU_PATH, SNI_PATH};
    use zbus::zvariant::ObjectPath;

    /// The Menu property returns `MENU_PATH` as an object path with a safe
    /// fallback. That fallback must never be reachable in practice: if the
    /// constant were malformed, hosts would silently show no menu.
    #[test]
    fn menu_path_is_a_valid_object_path() {
        let p = ObjectPath::try_from(MENU_PATH).expect("MENU_PATH must be a valid object path");
        assert_eq!(p.as_str(), MENU_PATH);
    }

    #[test]
    fn sni_path_is_a_valid_object_path() {
        let p = ObjectPath::try_from(SNI_PATH).expect("SNI_PATH must be a valid object path");
        assert_eq!(p.as_str(), SNI_PATH);
    }

    /// Hosts handed a bus name look the item up at the conventional path.
    #[test]
    fn paths_follow_the_spec_convention() {
        assert_eq!(SNI_PATH, "/StatusNotifierItem");
        assert!(MENU_PATH.starts_with(SNI_PATH));
    }
}
