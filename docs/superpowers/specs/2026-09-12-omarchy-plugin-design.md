# Omarchy 4 Quattro plugin for `ph-reactor` - Design

Issue: none yet (new product surface; marketplace submission prepared but not
opened - see "Publication").

## Problem

`ph-reactor` runs a local switchboard as a background daemon whose only UI is
a tray icon (libappindicator) and a local settings page. On Wayland compositors
running **Omarchy 4** (a Quickshell-based desktop shell), there is no way to
observe or control the reactor from the shell: no status in the bar, no drive
management without a terminal, and the tray is not part of the shell.

## Goals

1. A new **Omarchy 4 Quattro shell plugin** (`io.github.powerhouse-inc.ph-reactor-omarchy`,
   kinds `service` + `bar-widget`) that drives an independently installed
   `ph-reactor` binary: bar status at a glance, a popout panel for
   start/stop and drive management (add / remove / pause / resume / resync).
2. The plugin is a **thin shell surface only**. It never supervises, stores,
   or reads daemon state or credentials; everything goes through the
   `ph-reactor` CLI with a stable bounded contract.
3. Machine-layer change in `ph-reactor`: `status --json` (the daemon already
   serves `StatusSnapshot` JSON at `/api/status`; the CLI prints it when the
   daemon is down too) and a version bump to **0.2.0**, which becomes the
   plugin's minimum CLI contract.
4. **Distinct, non-collapsed failure states**: `missing`, `unsupported`,
   `stopped`, `starting`, `ready`, `degraded`, `error` - each with the right
   user action (start, update, open settings, retry).
5. Portable, deterministic tests and a reversible live-shell demo (fictional
   fixtures only); live-shell evidence documented for the owner's Omarchy
   machine.
6. Marketplace-ready repository: dedicated public repo, root `manifest.json`,
   root README with install/removal, license, preview image, exact submission
   issue prepared (not opened).

Non-goals: no re-implementation of the daemon/supervisor in QML; no `sudo`,
package installation, or systemd provisioning from the plugin (`omarchy
plugin add` runs no hooks and grants no privilege); no tokens or config
values in QML/`shell.json` (drive tokens stay in the daemon's environment via
the existing `--token-env` pattern); no Windows/macOS in this pass; the
tray is untouched and keeps working alongside the plugin.

## Two-layer model

Per the `omarchy-plugin-migrate` map: the compiled daemon is an **independent
machine layer** (supervision, npm bootstrap, data under `~/.ph/reactor`,
credentials); the Quattro plugin is the **shell layer** - a status client and
control surface. The migration strategy applies directly: first make the
external core expose a stable bounded CLI (done: `status --json` + `0.2.0`),
then build one Quattro vertical slice with fictional fixtures.

| Existing responsibility | Quattro destination |
| --- | --- |
| Tray icon / status surface | `bar-widget` (rebuild with `BarWidget`, semantic tokens) |
| Tray menu / options menu (drives) | private popout panel from the widget (no top-level `panel` kind) |
| Long-lived polling | `service` (one singleton, many visual consumers) |
| Compiled daemon + supervision | independent machine layer (unchanged) |
| Drive tokens (env vars) | stay in the daemon's environment; plugin passes only env var *names* |
| Install (snap/brew/script) | documented prerequisite in the plugin README; no install hooks |

## Plugin identity

- **ID**: `io.github.powerhouse-inc.ph-reactor-omarchy` (permanent; outside the
  reserved `omarchy.*` namespace; maps to `github.com/powerhouse-inc/ph-reactor-omarchy`).
- **Name**: `ph-reactor`; **license**: MIT (matches ph-reactor).
- **Kinds**: `service` + `bar-widget`. The panel is a private sibling loaded
  by the widget (a bar widget may load a private panel without claiming a
  top-level `panel` kind).
- **Entry points**: `service: "Service.qml"`, `barWidget: "BarWidget.qml"`.
- **barWidget metadata**: category `Developer Tools`, `allowMultiple: false`,
  `defaultSection: right`, single setting `autoStart` (boolean, default
  `false`) on the `shell.json` entry (no nested config object).

## State machine (`js/ReactorState.js`, pure JavaScript, Node-testable)

Input: a CLI probe result (`--version`) plus the `status --json` payload (the
`StatusSnapshot` JSON) or a probe failure (exit code, bounded stderr,
timeout). Output:

```json
{
  "phase": "missing|unsupported|stopped|starting|ready|degraded|error",
  "cliVersion": "0.2.0",
  "daemon": { "running": true, "healthy": true, "switchboardVersion": "6.2.2",
              "restarts": 0, "settingsUrl": "http://127.0.0.1:4000" },
  "drives": [ { "name": "...", "url": "https://…/d/…",
                "status": "synced|connecting|paused|offline|error",
                "paused": false, "detail": "..." } ],
  "message": "one bounded human line for tooltip/status"
}
```

- `missing`: CLI not on PATH. Backoff to a 60 s probe interval.
- `unsupported`: CLI present but semver < 0.2.0 (the `status --json` contract).
- `stopped`: `switchboard.running == false` (daemon down or child down) -
  action: Start.
- `starting`: `running && !healthy` - action: wait; auto-refresh continues.
- `ready`: healthy and no drive in `error`.
- `degraded`: healthy but at least one drive `error`.
- `error`: CLI failed unexpectedly (nonzero exit, malformed JSON, timeout).
  Last known good drives are kept; bounded stderr becomes the message.
- Refresh is idempotent (a probe in flight rejects a new one), bounded
  (stdout 16 KB, stderr 4 KB, 10 s timeout with a kill watchdog), and
  recovers: the next successful probe clears `error`.
- A minimal semver comparator lives in the same module (no dependencies).

## Service (`Service.qml`, process-wide singleton)

- Hosted once while the plugin is enabled; resolves the external CLI by a
  fixed setting (`cliPath`, default `ph-reactor`) and invokes it with
  **argument arrays only** (never constructed command strings) via
  `Quickshell.Io.PhProcess`.
- `refresh()`: run `--version` (first probe; cached afterwards) then
  `status --json`; normalize through `ReactorState.js`; publish state to
  widgets (QML binding) and to the `IpcHandler`.
- Actions (each rejects overlap, triggers a refresh after completion):
  `start()` → `run --daemonize`; `stop()` → `stop`;
  `driveAdd(url, name, tokenEnv, offline)` → `drive add …`;
  `driveRemove/Pause/Resume/Resync(target)` → matching commands.
- `autoStart` (property, default `false`, set by the widget from its
  `shell.json` setting): when true, the service starts the daemon if it
  finds `stopped` at mount. The service never starts the daemon implicitly
  otherwise.
- **IPC**: `IpcHandler` target = the plugin ID. Methods: `refresh(): void`,
  `status(): string` (bounded JSON of the normalized state), `start():
  string`, `stop(): string`, `driveAdd(url, name, tokenEnv, offline):
  string`, `driveRemove(target): string`, `drivePause(target): string`,
  `driveResume(target): string`, `driveResync(target): string`. Action
  methods return a bounded one-line result (`ok` / `error: …`); no
  tokens, environment dumps, or unbounded payloads ever cross the socket.
- No `sudo`, no `pkexec`, no downloads, no shared `/tmp` state, no reading
  of `config.json` or token values - the CLI is the only door.

## Bar widget (`BarWidget.qml`)

- `WidgetButton`: horizontal form shows a short label with a phase glyph
  (`●` ready, `◐` starting, `○` stopped, `!` degraded, `✕` missing /
  unsupported / error, warning-colored for the last three); vertical form
  shows the glyph alone (not a rotated label).
- Tooltip: the state `message` plus drive summary.
- Left-click: open the private popout panel (popout contract: `opened`,
  `open()`, `close()` on the root; `bar`, `settings`, anchor, and host
  widget injected into the panel).
- Right-click: force a refresh (documented in tooltip and README).
- Resolves the singleton `service` from the shell's service map (recomputes
  on revision change) with an inert local fallback until it appears -
  multi-monitor safe.

## Panel (`Panel.qml`, private popout)

- **PopupCard** (the current first-party pattern for widget-owned popouts,
  as used by the media plugin): anchored to the widget, themed popup
  styling, click-outside dismissal through the owner.
- Header: state message, versions (CLI + switchboard), and the
  settings-page URL as a bounded, selectable line. (`qs.Commons` has no
  `OpenUrl` in Omarchy's Commons; opening the link in a browser is left
  to the user - the plugin launches no external process beyond the
  `ph-reactor` CLI itself.)
- Drives: list of name, status chip (`synced` / `connecting` / `paused` /
  `offline` / `error`) with detail, and row actions (pause / resume /
  resync / remove; remove is a two-tap "arm" confirmation).
- Add drive: URL field (required), name and token-env fields (optional),
  one-line result feedback from the service.
- Footer: Start / Stop button per phase.
- No persisted state of its own; closing it changes nothing on the machine.

## Failure states

Preserved distinctly (never collapsed into generic fallback data):
`missing` (show install pointer in panel), `unsupported` (show required
version), `stopped`, `starting`, `ready`, `degraded` (drive errors listed),
`error` (bounded CLI error text + retry). Widget text, tooltip, and panel
header all render the same normalized state.

## Enable / update / remove behavior

- **Enable**: mounts QML; nothing on the machine changes unless `autoStart`
  is true (a deliberate user setting).
- **Update**: repo fast-forward, manifest revalidation by the shell, service
  reload; the state machine is idempotent so a mid-update refresh is safe.
- **Remove**: `omarchy plugin remove` deletes only plugin files. The daemon
  keeps running; data under `~/.ph/reactor` is untouched. Documented: to
  stop the reactor use `ph-reactor stop`.

## Security review points (per the migration map)

- No download-and-execute: the only external binary is the user-installed
  `ph-reactor` on PATH; the fixture CLI is demo-only and path-scoped.
- No privileged commands; no passwordless sudoers; no shared `/tmp` PID or
  control state.
- No copied tokens or environment dumps: the token *name* is the only
  credential-adjacent value, passed as an argv to the CLI that already owns
  it.
- Removal deletes nothing outside the plugin's own cloned directory.

## Tests

1. **Portable** (`./tests/run`, no Omarchy host needed):
   - repo copy of the toolkit's `validate_manifest.py` (manifest schema,
     entry points, reserved-ID, symlink, size, and README/license checks);
   - Node built-in test runner (`node --test`) over `js/ReactorState.js`
     with committed fixtures: all seven phases, semver ordering
     (including prerelease), drive normalization (URL passthrough,
     unknown statuses pass through, nameless drives skipped), malformed
     JSON (last known good drives retained), nonzero exit with and
     without stderr, distinct timeout reporting, recovery transitions
     (error→ready, missing→unknown→ready, unsupported retains drive
     data), reducer purity/idempotence, `bound()` truncation, and
     `parseVersion()` on clap-style and bare version lines;
   - JSON validity of every fixture; `bash -n` on `demo/run`;
   - `omarchy plugin validate` when an Omarchy host is present (gated).
2. **QtTest** (service transitions with Quickshell stubs): deferred to CI
   once the repo is public (no Qt 6 / Quickshell on this machine); exact
   command recorded in the README test section.
3. **Live shell** (on the owner's Omarchy machine): `demo/harness.sh` -
   a reversible one-command harness: puts a committed fixture
   `ph-reactor` CLI (fictional drives, `demo.invalid` URLs, env-var names
   only) and an `omarchy` stub first on PATH, exports
   `OMARCHY_PATH` + `OMARCHY_SHELL_IPC_TIMEOUT`, and - when a real
   Omarchy binary is found - enables the plugin with the official
   `omarchy shell add <plugin dir>` and removes it again on exit. In a
   headless shell the same script prints the exact CLI contract commands
   instead.

## Demo fixture and screenshot state

Fictional, deterministic, committed: drive `Fictional Vault`
(`https://demo.invalid/d/fictional-vault`), token env var name
`PH_DEMO_TOKEN` (value never materialized), states cycling
stopped → starting → ready → degraded → ready. Screenshot: bar with the
ready glyph + panel open over the drives list.

## Publication

New dedicated public repo `powerhouse-inc/ph-reactor-omarchy` (the
marketplace requires one plugin with a root `manifest.json`; it cannot be a
subdirectory of `ph-reactor`). Submission issue to
`HANCORE-linux/omarchy-plugin-marketplace` is prepared with the toolkit's
`prepare_submission.py` (title `[Plugin]: ph-reactor`, category
`Developer Tools`, tags `system`, `quickshell`) and saved as evidence; it is
opened only after owner approval.

## Verification plan

- `python3 validate_plugin.py <plugin> --json --security` (toolkit copy)
  passes; `./tests/run` passes on this machine (no Omarchy);
- `demo_preflight.py <plugin>` passes (harness structure);
- Rust side: `cargo test` for the new `status --json` contract test;
  `ph-reactor status --json` exercised against a live daemon and while the
  daemon is stopped (both payload shapes);
- live-shell evidence: recorded by the owner on Omarchy 4 after this
  delivery (commands + Omarchy version + SHA per the evidence-record
  format), not claimed here.

## Deferred scope

QtTest CI job; Windows/macOS; multiple widget instances; a tray→plugin
migration guide for existing users (both surfaces coexist); opening the
marketplace issue; brew/snap packaging (separate workstream).

## Impact

| Repo | Change |
| --- | --- |
| `ph-reactor` | `status --json` flag; version 0.1.0 → 0.2.0; CLI docs in README; contract test |
| `ph-reactor-omarchy` (new) | the entire Quattro plugin (manifest, three QML entry surfaces, JS state machine, tests, demo, README, license, preview, CI) |
| marketplace | prepared submission issue (evidence), not opened |
