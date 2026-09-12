# Omarchy plugin (ph-reactor-omarchy) — SDD Progress

Spec: `../../specs/2026-09-12-omarchy-plugin-design.md`. Plan:
`../../plans/2026-09-12-omarchy-plugin.md`.

Machine layer in this worktree (`~/.worktrees/ph-reactor-omarchy-plugin`,
branch `feat/omarchy-plugin` from `main`); plugin repo
`~/ph-reactor-omarchy` (branch `feat/omarchy-plugin`). This machine has no
Omarchy host and no Qt 6: portable validation only; QtTest and live-shell
layers are documented, not claimed.

| Date | Task | Status | Notes |
|---|---|---|---|
| 2026-09-12 | 1 Machine layer `status --json` | done | commit 64b5082: `Status { json }`, 0.1.0 → 0.2.0, degraded + live snapshot contract tests, README contract section; 45 tests green, 0 warnings; binary verified |
| 2026-09-12 | 2 Plugin repo scaffold | done | toolkit `generate`: manifest (service + bar-widget), MIT, preview, CI, `omarchy plugin validate` support |
| 2026-09-12 | 3 State machine + fixtures + node tests | done | `js/ReactorState.js` (pure, Node-testable): 7 phases, semver, bounded normalization; 15 `node --test` cases over committed fixtures matching the Rust `StatusSnapshot` shape; drive view extended with `url` (spec: panel shows the switchboard URL) |
| 2026-09-12 | 4 Service.qml | done | singleton; `--version` then `status --json` via `PhProcess` (argv arrays only, bounded streams, 10 s watchdog, overlap rejection); 9 IPC methods; autoStart (inline setting, fires once at `stopped`); no sudo/downloads/config reads |
| 2026-09-12 | 5 BarWidget + Panel | done | `WidgetButton` glyph + tooltip (left-click panel, right-click re-probe); service lookup via the capability facade (`serviceFor`, re-queried until mounted); `Panel.qml` is a private `PopupCard` popout (current first-party media pattern, not the toolkit's `PanelWindow` template) with drives list (name, status chip, detail, URL; pause/resume/resync/two-tap remove), add-drive form, start/stop + settings URL footer |
| 2026-09-12 | 6 Reversible demo harness | done | `demo/harness.sh`: fixture `ph-reactor` (exact Rust `StatusSnapshot` JSON) + `omarchy` stub on PATH, `OMARCHY_PATH`/`OMARCHY_SHELL_IPC_TIMEOUT` exported; real-Omarchy path enables via `omarchy shell add` and removes on exit; headless path prints the CLI contract. Headless verified end-to-end (fixture → state machine → `ready` with 2 drives) |
| 2026-09-12 | 7 Validation, README, preview, CI | done | README written (install, usage, IPC contract, security, dev loop); `tests/run` green (manifest validation + 15 node tests + fixture JSON + `omarchy plugin validate` via stub); CI runs `tests/run` on push/PR; preview image is the toolkit placeholder (final artwork deferred) |
| 2026-09-12 | 8 Evidence + publication prep | done | marketplace submission issue drafted (not opened) in `../../evidence/omarchy-plugin/`; live-shell evidence explicitly deferred to the owner's Omarchy machine (no Qt 6 here) |

## Deviations from the spec

- **Panel popout**: spec originally sketched the toolkit's `PanelWindow`
  (layer-shell) pattern. As-built uses `PopupCard` — the *current*
  first-party pattern for widget-owned popouts (media plugin), which the
  widget-contract doc points to as the local pattern. Simpler, and it is
  what the host's popout coordination expects.
- **Settings-page link**: spec assumed a `qs.Commons.OpenUrl` wrapper;
  Omarchy's Commons does not ship `OpenUrl`. The panel shows the URL as a
  bounded, selectable line instead; the plugin launches no external
  process beyond the `ph-reactor` CLI.
- **Drive actions keyed by name**: the Rust CLI accepts names or indices;
  the normalized drive view carries the name, so the panel passes names.

## Deferred (as specified)

QtTest CI (needs Qt 6 + Quickshell host); Windows/macOS; live-shell
screenshot evidence (owner's machine); opening the marketplace issue
(owner approval); final preview artwork.
