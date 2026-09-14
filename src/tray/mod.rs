//! Status-bar tray via `org.kde.StatusNotifierItem` + `com.canonical.dbusmenu`
//! over the session bus (zbus, no GTK).
//!
//! Registration: when a StatusNotifierWatcher is present (KDE/GNOME
//! shell indicators) the item registers with it; otherwise the item owns
//! the well-known fallback name `org.kde.StatusNotifierItem-<uid>-<n>`,
//! which indicator implementations scan for. In headless environments
//! (no session bus) the daemon runs without a tray.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};
use zbus::connection::Connection;
use zbus::names::WellKnownName;
use zbus::object_server::SignalContext;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

use crate::commands::Command;

pub mod dbusmenu;
pub mod icon;
pub mod menu;
use crate::status::StatusSnapshot;
use menu::{Action, MenuItem};

// The spec's conventional paths. These are not arbitrary: a host that is
// handed a BUS NAME (which is how we register) looks the item up at
// `/StatusNotifierItem`. A custom path is only reachable when registering by
// path, which not every host implements.
const SNI_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/StatusNotifierItem/Menu";

/// The SNI `ToolTip` property: `(icon-name, pixmaps, title, description)`,
/// D-Bus signature `(sa(iiay)ss)`.
type ToolTip = (String, Vec<(i32, i32, Vec<u8>)>, String, String);

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
    /// Bumped whenever the menu is rebuilt. Hosts cache a layout and only
    /// re-fetch when `LayoutUpdated` carries a revision newer than theirs.
    revision: u32,
    /// Where the embedded icon was installed, for `IconThemePath`.
    icon_theme_path: String,
}

impl TrayState {
    /// Rebuilds the menu from the current snapshot, returning the new
    /// revision.
    fn rebuild(&mut self) -> u32 {
        self.menu = menu::build_menu(&self.snapshot);
        self.revision = self.revision.wrapping_add(1);
        self.revision
    }
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

    /// `NeedsAttention` is the spec spelling (not "Attention"), and it makes
    /// a shell surface the icon out of a collapsed tray -- which is what a
    /// reactor that cannot sync should do rather than sit there looking fine.
    #[zbus(property)]
    fn status(&self) -> &str {
        let s = self.state.lock();
        if s.snapshot.reactor.running && s.snapshot.reactor.healthy {
            "Active"
        } else {
            "NeedsAttention"
        }
    }

    /// The Powerhouse logomark. Recoloured by the desktop, so it reads on a
    /// light or a dark panel; see `icon::LOGOMARK_SVG`.
    #[zbus(property)]
    fn icon_name(&self) -> String {
        icon::ICON_NAME.to_string()
    }

    /// Shown instead of `IconName` while `Status` is `NeedsAttention`.
    #[zbus(property)]
    fn attention_icon_name(&self) -> &str {
        "dialog-warning"
    }

    /// A small badge over the base icon. State lives here rather than in the
    /// base icon so the brand mark stays recognisable; a host that ignores
    /// overlays simply shows the mark.
    #[zbus(property)]
    fn overlay_icon_name(&self) -> &str {
        let s = self.state.lock();
        let d = &s.snapshot.drives;
        if d.is_empty() {
            ""
        } else if d.iter().all(|x| x.paused) {
            "media-playback-pause"
        } else if d.iter().any(|x| x.status == "connecting") {
            "emblem-synchronizing"
        } else {
            ""
        }
    }

    /// Where the embedded icon was installed, so a host that does not rescan
    /// the theme still finds it.
    #[zbus(property)]
    fn icon_theme_path(&self) -> String {
        self.state.lock().icon_theme_path.clone()
    }

    /// False: left-click invokes `Activate` rather than popping the menu.
    /// Without this some hosts assume the icon is menu-only and never call
    /// `Activate` at all.
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
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

    /// The hover tooltip.
    ///
    /// zbus derives the signature `(sa(iiay)ss)` from [`ToolTip`]; returning
    /// a bare string here would advertise `s` and the host would fail to read
    /// it — the same class of bug that made `Menu` unreadable.
    #[zbus(property)]
    fn tool_tip(&self) -> ToolTip {
        let s = self.state.lock();
        let snap = &s.snapshot;
        let mut lines = vec![format!("ph-reactor {}", snap.version)];
        if let Some(peer) = &snap.reactor.peer_id {
            lines.push(peer.clone());
        }
        lines.push(format!(
            "{} document{}",
            snap.reactor.docs,
            if snap.reactor.docs == 1 { "" } else { "s" }
        ));
        for d in &snap.drives {
            lines.push(format!("{} — {}", d.name, d.status));
        }
        for g in &snap.groups {
            lines.push(format!("team {} — {} members", g.name, g.members));
        }
        (
            icon::ICON_NAME.to_string(),
            Vec::new(),
            format!("ph-reactor — {}", menu::state_line_for(snap)),
            lines.join("\n"),
        )
    }

    /// Left-click: open the console. This was an empty stub, which is why
    /// clicking the icon appeared to do nothing at all.
    fn activate(&self, _x: i32, _y: i32) {
        open_console(&self.state);
    }

    /// Middle-click does the same, as it always has.
    fn secondary_activate(&self, _x: i32, _y: i32) {
        open_console(&self.state);
    }

    /// Right-click. Hosts pop the menu from the `Menu` property themselves;
    /// there is nothing to do here, and returning an error would make some
    /// hosts skip the menu entirely.
    fn context_menu(&self, _x: i32, _y: i32) {}

    fn scroll(&self, _amount: i32, _direction: &str) {}

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
        3
    }

    /// "normal" or "notice". We never demand attention through the menu.
    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }

    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        let p = self.state.lock().icon_theme_path.clone();
        if p.is_empty() {
            Vec::new()
        } else {
            vec![p]
        }
    }

    /// The layout a host renders.
    ///
    /// Every argument is signed: the spec uses `i`, and the previous
    /// implementation's `u` made the whole interface unreadable.
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, dbusmenu::LayoutItem<'static>)> {
        let st = self.state.lock();
        match dbusmenu::layout(&st.menu, parent_id, recursion_depth, &property_names) {
            Some(item) => Ok((st.revision, item)),
            None => Err(zbus::fdo::Error::InvalidArgs(format!(
                "no menu item with id {parent_id}"
            ))),
        }
    }

    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        property_names: Vec<String>,
    ) -> Vec<(i32, HashMap<String, OwnedValue>)> {
        let st = self.state.lock();
        dbusmenu::group_properties(&st.menu, &ids, &property_names)
            .into_iter()
            .map(|(id, props)| (id, owned(props)))
            .collect()
    }

    fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedValue> {
        let st = self.state.lock();
        let item = dbusmenu::find(&st.menu, id)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("no menu item with id {id}")))?;
        dbusmenu::props_of(item, std::slice::from_ref(&name))
            .remove(&name)
            .and_then(|v| OwnedValue::try_from(v).ok())
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("item {id} has no {name}")))
    }

    /// A host calls this before opening a submenu. Rebuilding here keeps the
    /// menu honest: the snapshot may have moved since it was last drawn.
    ///
    /// Returns whether the layout changed, which is what tells the host to
    /// re-fetch rather than draw its cached copy.
    async fn about_to_show(&self, _id: i32) -> bool {
        let mut st = self.state.lock();
        let before = format!("{:?}", st.menu);
        st.rebuild();
        format!("{:?}", st.menu) != before
    }

    /// `event_id` is a STRING ("clicked", "hovered", …), not the integer the
    /// previous implementation compared against.
    async fn event(
        &self,
        id: i32,
        event_id: String,
        _data: Value<'_>,
        _timestamp: u32,
    ) -> zbus::fdo::Result<()> {
        if event_id != "clicked" {
            return Ok(());
        }
        let action = {
            let st = self.state.lock();
            menu::action_for_id(&st.menu, id as u32)
        };
        if let Some(action) = action {
            dispatch_action(&self.state, action).await;
        }
        Ok(())
    }

    #[zbus(signal)]
    async fn layout_updated(
        signal_ctxt: &SignalContext<'_>,
        revision: u32,
        parent_id: i32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn item_activation_requested(
        signal_ctxt: &SignalContext<'_>,
        id: i32,
        timestamp: u32,
    ) -> zbus::Result<()>;
}

/// Borrowed property values to owned ones, for the `a{sv}` returns.
fn owned(props: HashMap<String, Value<'static>>) -> HashMap<String, OwnedValue> {
    props
        .into_iter()
        .filter_map(|(k, v)| OwnedValue::try_from(v).ok().map(|v| (k, v)))
        .collect()
}

/// `Status` for a snapshot: `NeedsAttention` is the spec spelling, and it is
/// what surfaces the icon out of a collapsed tray.
fn sni_status(snap: &StatusSnapshot) -> &'static str {
    if snap.reactor.running && snap.reactor.healthy {
        "Active"
    } else {
        "NeedsAttention"
    }
}

/// Opens the console in the user's browser.
fn open_console(state: &Arc<Mutex<TrayState>>) {
    let url = state.lock().snapshot.settings.url.clone();
    if url.is_empty() {
        tracing::warn!("no console URL in the snapshot; not opening");
        return;
    }
    match std::process::Command::new("xdg-open").arg(&url).spawn() {
        Ok(_) => tracing::debug!("opened {url}"),
        Err(err) => tracing::warn!("could not open {url}: {err}"),
    }
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
        Action::OpenConsole => open_console(state),
        Action::Quit => send(Command::Quit),
    }
}

async fn run(
    conn: Arc<Connection>,
    mut snapshot: watch::Receiver<StatusSnapshot>,
    stop: watch::Receiver<bool>,
    commands: mpsc::UnboundedSender<Command>,
) {
    let snap = snapshot.borrow_and_update().clone();
    // Write the embedded logomark into the user's icon theme. Best-effort:
    // without it the host falls back to a generic icon, which is a cosmetic
    // loss, not a reason to run without a tray.
    let icon_theme_path = icon::ensure_installed()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let state = Arc::new(Mutex::new(TrayState {
        snapshot: snap.clone(),
        menu: menu::build_menu(&snap),
        commands,
        revision: 1,
        icon_theme_path,
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
    let menu_ctxt = match SignalContext::new(conn.as_ref(), MENU_PATH) {
        Ok(c) => Some(c),
        Err(err) => {
            tracing::warn!("cannot build menu signal context: {err:#}");
            None
        }
    };
    if let Some(ctxt) = &ctxt {
        let _ = Sni::new_status(ctxt, sni_status(&snap)).await;
        let _ = Sni::new_icon_name(ctxt, icon::ICON_NAME).await;
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
                let revision = {
                    let mut st = state.lock();
                    st.snapshot = snap.clone();
                    st.rebuild()
                };
                if let Some(ctxt) = &ctxt {
                    let _ = Sni::new_status(ctxt, sni_status(&snap)).await;
                    let _ = Sni::updated(ctxt).await;
                }
                // Tell the host the menu moved. Without this an open menu
                // keeps showing whatever was true when it was first drawn.
                if let Some(ctxt) = &menu_ctxt {
                    let _ = DbusMenu::layout_updated(ctxt, revision, 0).await;
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
