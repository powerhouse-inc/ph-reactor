# Task 7 report — Tray (SNI + DBusMenu)

**Done:** `tray/mod.rs` + `tray/menu.rs` over zbus (no GTK). SNI object
at `/StatusNotifier/Item/PhReactor` (valid D-Bus path — no dashes);
registers with `org.kde.StatusNotifierWatcher` when present, else owns
the well-known fallback `org.kde.StatusNotifierItem-<uid>-1` that
indicators scan for. Themed `IconName` (`network-server`; attention
variant when unhealthy). `Status` Active/Attention. DBusMenu sibling
object from a small in-memory model (AboutToShow, Event, update
signals). Menu actions send `Command`s into the daemon's single-writer
loop. No session bus -> `Tray::Headless(reason)`, one warning, daemon
continues.

**Live evidence:** on this headless box (session bus present, no KDE
watcher) the daemon took the fallback name; on desktops with a watcher
the RegisterStatusNotifierItem path is used. See evidence file.
