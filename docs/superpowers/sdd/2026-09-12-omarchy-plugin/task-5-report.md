# Task 5 report — BarWidget + Panel

**Done:**
- `BarWidget.qml` (`kinds: bar-widget`): `WidgetButton` with a phase
  glyph (● ready, ◐ starting, ○ stopped, ! degraded, ✕ missing/
  unsupported/error — urgent-colored for attention states, dimmed for
  stopped/unknown) plus the `ph-reactor` label in horizontal form (glyph
  alone vertically). Tooltip = state message + settings URL. Left-click
  toggles the private popout; right-click forces a re-probe.
- Service lookup: the widget resolves its own service singleton through
  the capability facade (`bar.shell.serviceFor(moduleName)`) and
  re-queries on a 2 s timer until the service is mounted (both load
  when the plugin enables; the lookup can postdate the first binding
  evaluation). Pushes its inline `autoStart` setting to the service on
  every relevant change.
- Popout contract: the widget root exposes `opened`, `open()`,
  `close()`; the popout itself is `Panel.qml`, loaded through a
  `Loader`, with `bar`, `settings`, the anchor item, the host widget,
  and the service re-injected whenever any source changes.
- `Panel.qml`: a `PopupCard` (the *current* first-party pattern for
  widget-owned popouts — the media plugin; not the toolkit's
  `PanelWindow` template): header (CLI version, switchboard version,
  state message, settings URL as bounded selectable text), drives list
  (name + status chip `synced|connecting|paused|offline|error` + detail
  + switchboard URL; per-row pause/resume, resync, and two-tap armed
  remove), add-drive form (URL required; name and token-env var name
  optional; one-line result feedback), footer (start/stop button).

**Tests:** not directly (QML needs the Omarchy Qt runtime — absent on
this machine). The data the panel renders is the tested reducer output
(task 3); the end-to-end fixture run (task 6) exercised the exact
payload path. Live-shell verification is the owner's step on an Omarchy
machine.

**Deviations:** `PopupCard` instead of `PanelWindow` (current first-party
pattern); settings URL as selectable text instead of an `OpenUrl`
wrapper (not shipped in Omarchy's Commons). Both recorded in the spec.
