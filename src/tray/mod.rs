//! Status-bar tray via `org.kde.StatusNotifierItem` + `org.kde.DBusMenu`
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

const SNI_PATH: &str = "/StatusNotifier/Item/PhReactor";
const MENU_PATH: &str = "/StatusNotifier/Item/PhReactor/Menu";

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

    /// Active while the switchboard is healthy; Attention otherwise.
    #[zbus(property)]
    fn status(&self) -> &str {
        let s = self.state.lock();
        if s.snapshot.switchboard.healthy {
            "Active"
        } else {
            "Attention"
        }
    }

    /// Themed icon, resolved by the desktop theme.
    #[zbus(property)]
    fn icon_name(&self) -> &str {
        let s = self.state.lock();
        if s.snapshot.switchboard.healthy {
            "network-server"
        } else {
            "dialog-warning"
        }
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> &str {
        ""
    }

    /// The menu's object path (SNI spec: `s` property).
    #[zbus(property)]
    fn menu(&self) -> &str {
        MENU_PATH
    }

    #[zbus(property, name = "ItemActivationRequested")]
    fn item_activation_requested(&self) -> u32 {
        0
    }

    #[zbus(property)]
    fn tool_tip(&self) -> String {
        let s = self.state.lock();
        let sb = &s.snapshot.switchboard;
        format!(
            "<b>ph-reactor</b> — switchboard {} (port {})",
            if sb.healthy { "healthy" } else { "not ready" },
            sb.port
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

#[zbus::interface(name = "org.kde.DBusMenu", spawn = false)]
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

    // Register with the watcher when one is present; else take the
    // fallback well-known name indicator implementations scan for.
    // Register with the watcher when one is present; else take the
    // fallback well-known name indicator implementations scan for.
    let uid = nix_uid();
    let watcher = "org.kde.StatusNotifierWatcher";
    let registered = match raw_register(conn.as_ref(), watcher, SNI_PATH).await {
        Ok(service) => {
            tracing::info!("registered with status notifier watcher {service}");
            true
        }
        Err(err) => {
            tracing::warn!("watcher registration failed ({err:#}); using fallback name");
            false
        }
    };
    if !registered {
        let name = format!("org.kde.StatusNotifierItem-{uid}-1");
        match WellKnownName::try_from(name.clone()) {
            Ok(n) => match conn.request_name(n).await {
                Ok(_) => tracing::info!("taking fallback name {name}"),
                Err(err) => {
                    tracing::warn!("cannot take fallback name {name}: {err:#}");
                    return;
                }
            },
            Err(err) => {
                tracing::warn!("bad name {name}: {err}");
                return;
            }
        }
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
                let healthy = snap.switchboard.healthy;
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
async fn raw_register(conn: &Connection, watcher: &str, path: &str) -> anyhow::Result<String> {
    let item = ObjectPath::try_from(path)?;
    let reply = conn
        .call_method(
            Some(watcher),
            "/StatusNotifierWatcher",
            Some(watcher),
            "RegisterStatusNotifierItem",
            &(item,),
        )
        .await?;
    let service: String = reply.body().deserialize()?;
    Ok(service)
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
