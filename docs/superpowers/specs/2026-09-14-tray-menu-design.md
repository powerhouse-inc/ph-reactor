# A tray that actually works — Design

## Problem

The tray icon appears and does nothing. Clicking it — any button — produces
no menu, no window, no feedback.

Four independent defects, each of which degraded silently rather than failing:

1. **`Activate` is an empty stub.** `fn activate(&self, _x: i32, _y: i32) {}`.
   Left-click, the primary interaction, was never implemented. Only
   middle-click (`SecondaryActivate`) did anything.
2. **The menu speaks a protocol no host implements.** `DbusMenu` exposes a
   `Layout` *property* carrying Qt's internal `<dbustree>` XML. The real
   interface is a `GetLayout` *method* returning `(u, (ia{sv}av))`. Calling it
   on our object answers `Unknown method 'GetLayout'`.
3. **Every argument type is wrong.** `Event` is `(uusu)` where the spec says
   `(isvu)`; `AboutToShow` is `(u)` where the spec says `(i)`. Unsigned where
   the spec is signed throughout.
4. **`GetGroupProperties` and `GetProperty` do not exist**, and the spec's
   three signals (`LayoutUpdated`, `ItemsPropertiesUpdated`,
   `ItemActivationRequested`) are absent — `ItemActivationRequested` is
   declared as a *property* on the wrong interface.

Verified against a live KDE Plasma session and against KDE's own
`libdbusmenuqt/com.canonical.dbusmenu.xml`.

## Goals

1. **Left-click opens the console.** The icon is a shortcut to the app.
2. **Right-click opens a menu that works**, on KDE and on GNOME (via the
   AppIndicator extension).
3. **The icon carries information** — sync state at a glance, and it surfaces
   itself when something is wrong.
4. **The menu reflects what the reactor is actually doing**, including the
   teams it belongs to, and updates while open.
5. **No new runtime dependencies.** No clipboard helper, no GTK, no polling
   the console over HTTP.

## Non-goals

- Team or channel *management* from the tray. The menu is a read-only view
  plus the few actions that already exist (pause/resume/remove a drive, open
  the console, quit). Composing a message belongs in the console.
- Icon pixmaps (`IconPixmap` and friends). Themed icon names only; every
  target desktop resolves them.
- Restoring the XML `Layout` property for backwards compatibility. Nothing
  consumes it — it never worked.

## Decisions and their rationale

### Left-click opens the console; right-click opens the menu

`ItemIsMenu = false`, so a host routes left-click to `Activate` rather than
popping the menu. `Activate` opens `settings.url` with `xdg-open`.
`SecondaryActivate` (middle-click) keeps doing the same, which is what it
already did.

This is the shape every comparable sync daemon uses. The icon means "my
reactor"; the menu is for quick actions. Making left-click open the menu
would cost an extra click to reach the console, which is the main reason to
touch the icon at all.

### Actions travel on the menu item, not in its label

Today `action_of` recovers an action by **parsing the label**:
`label.strip_prefix("pause ")`, `label.starts_with("Quit")`. Renaming a label
silently breaks the action, and no test would catch it because the label and
the parser are edited together.

`MenuItem` gains `action: Option<Action>`, and `action_for_id` becomes an id
lookup. This is a prerequisite for the redesigned menu rather than optional
tidying: the new labels ("Open console…", team entries) would otherwise need
new prefixes invented for the parser to match.

### The icon reports sync state via an overlay, not by changing

Today it is `network-server` whenever healthy and `dialog-warning` otherwise.
Both are static, so a reactor that is connected looks the same as one that has
been retrying for an hour.

| Condition | `OverlayIconName` | `Status` |
|---|---|---|
| not running, or unhealthy | — (`AttentionIconName` = `dialog-warning`) | `NeedsAttention` |
| a drive is `connecting` | `emblem-synchronizing` | `Active` |
| every drive `synced` (at least one) | — | `Active` |
| no drives, or all paused | `media-playback-pause` | `Active` |

The base `IconName` stays the Powerhouse mark in every row; only the overlay
and status change. An overlay a host ignores costs nothing — the brand icon
still shows.

`NeedsAttention` makes Plasma surface the icon out of a collapsed tray, so a
broken reactor becomes visible instead of silently sitting there.

### The icon is the Powerhouse logomark, recoloured by the desktop

The tray icon is the square glyph from the Powerhouse lockup
(`64526a93e82c4b60e5e1a58a_ph.svg`), with the "Powerhouse" wordmark removed —
the first path of that file is the mark and spans exactly `0..114` in both
axes, so the crop is a clean square with no re-drawing.

It is shipped **monochrome and theme-aware**, following the convention Breeze
itself uses (verified by reading `/usr/share/icons/breeze/status/22/`):

```svg
<style id="current-color-scheme">.ColorScheme-Text { color: #232629; }</style>
<path class="ColorScheme-Text" fill="currentColor" .../>
```

KDE rewrites that stylesheet with the active scheme colour; Breeze light ships
`#232629` and breeze-dark `#fcfcfc`, which is exactly "white on a dark panel,
black on a light one". GNOME recolours symbolic icons by its own mechanism.

A caveat worth recording: this SVG renders **blank** in renderers that ignore
the embedded stylesheet (ImageMagick, for one) because `currentColor` then
resolves to nothing. That is not a defect — a stock Breeze icon renders
equally blank in the same renderer. Do not "fix" it by hard-coding a fill;
that would break recolouring, which is the entire point.

The icons are **embedded in the binary** and written to
`~/.local/share/icons/hicolor/{scalable,symbolic}/apps/` on tray start if
absent, with `IconThemePath` pointing there as a fallback. That keeps the
daemon self-contained — it already embeds the console HTML — so the icon works
from a snap, a `cargo build`, or a copied binary, with no installer step.

State is *not* encoded in the base icon, because the brand mark should stay
recognisable. It is carried by `Status` (`NeedsAttention` surfaces the icon
out of a collapsed tray), by `OverlayIconName` as a small badge where the host
supports it, and by the tooltip.

### Teams come from the store, not from HTTP

`StatusSnapshot` gains `groups: Vec<GroupSummary>`, filled in
`refresh_status` with the already-public `query::query_docs(&store, "group",
None)` — the same call the console's `/api/groups` uses. No new store API, and
the tray never talks to the settings server.

### Menu layout

```
ph-reactor 1.2.0 — synced          disabled
12D3KooWBEVh…SWod                  disabled, this node's peer id
─────────
Open console…
─────────
Teams  ▸
  powerhouse — 2 members, 1 message
Drives ▸
  ph-bootstrap — synced
    Pause
    Remove
─────────
Quit ph-reactor
```

The peer id is shown, not copied to the clipboard: copying needs `wl-copy` or
`xclip`, which is an external dependency that fails silently on a mismatched
session. A visible id can be read; a clipboard item that quietly does nothing
is worse than none.

"Open settings…" becomes "Open console…" — it opens the console, which is what
that URL serves. `Action::OpenSettings` keeps its name; only the label moves.

## Architecture

### `src/tray/dbusmenu.rs` (new)

Pure serialization, no D-Bus and no locks, so it is unit-testable:

- `layout(root, parent_id, depth, filter) -> Option<LayoutItem>` — finds the
  subtree rooted at `parent_id`, honouring `recursionDepth` (`-1` = unlimited,
  `0` = the item alone) and the `propertyNames` filter (empty = all).
- `props_of(item, filter) -> HashMap<String, Value>` — the spec's item
  properties: `type` (only when `separator`), `label`, `enabled`, `visible`,
  `icon-name`, `children-display` (`submenu` when the item has children).
  Defaults are omitted, as the spec requires — `enabled` and `visible` are
  only emitted when false.
- `find(root, id)` — id lookup shared with action dispatch.

`LayoutItem` carries `id: i32`, `props`, and `children: Vec<Value>`; its
derived signature is `(ia{sv}av)`, and children nest as variants wrapping the
same structure. That shape was confirmed by dumping `GetLayout` from a working
tray item on the same session.

### `src/tray/mod.rs`

The `DbusMenu` interface is replaced with the six spec methods
(`GetLayout`, `GetGroupProperties`, `GetProperty`, `Event`, `AboutToShow`,
plus `Version`/`Status` properties) and the three signals. A `revision`
counter increments whenever the menu is rebuilt; `LayoutUpdated` carries it so
an open menu refreshes.

`Sni` gains a working `Activate`, an `ItemIsMenu` property, and the icon and
tooltip derivation above. The spurious `ItemActivationRequested` property and
the non-standard `SetShowMenu` method are removed.

### Event dispatch

`Event(id, "clicked", data, timestamp)` resolves the id against the last built
menu and dispatches its `action`. The spec's event id is the **string**
`"clicked"`, not the integer `2` the current code compares against.

## Testing

| Level | What |
|---|---|
| Unit | `layout()` shape: root id 0, `children-display` on parents, separators typed, defaults omitted, `recursionDepth` 0/1/-1 honoured, property filtering |
| Unit | `find()` and action lookup by id, including that a renamed label does not change dispatch |
| Unit | Icon/status derivation for each of the four states |
| Unit | Teams section renders from a snapshot with groups, and is omitted when there are none |
| Live | `busctl call … GetLayout iias -- 0 -1 0` answers with a populated tree |
| Live | The menu opens on right-click in Plasma, entries are clickable |
| Live | Left-click opens the console in a browser |

## Risks

| Risk | Mitigation |
|---|---|
| zvariant signature mismatch is invisible until a host calls it | A unit test asserts `LayoutItem`'s signature is exactly `(ia{sv}av)`, and live `busctl` verification is part of the definition of done |
| Removing the XML `Layout` property breaks a consumer | Nothing consumes it; it never worked on any host |
| Teams plumbing slows `refresh_status` | It reuses the query the console already runs on every poll; the store is in-memory |
| GNOME behaves differently from KDE | Registration by bus name and the conventional object path are what the AppIndicator extension expects; only KDE can be verified here, and that limit is stated rather than assumed away |
