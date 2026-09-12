# Omarchy Quattro plugin for `ph-reactor` — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** a thin Omarchy 4 Quattro shell plugin (new public repo
`powerhouse-inc/ph-reactor-omarchy`, ID `io.github.powerhouse-inc.ph-reactor-omarchy`,
kinds `service` + `bar-widget`) that observes and controls an independently
installed `ph-reactor` daemon: bar status glyph, popout panel with
start/stop and drive management. Plus the machine-layer contract change in
`ph-reactor`: `status --json` and version 0.2.0.

**Architecture:** see
`docs/superpowers/specs/2026-09-12-omarchy-plugin-design.md`. Two layers:
the machine layer (`ph-reactor`, unchanged supervisor/daemon) and the shell
layer (one Quattro vertical slice: `Service.qml` singleton polling the CLI,
`BarWidget.qml` + private `Panel.qml` rendering a normalized state from pure
JavaScript `js/ReactorState.js`).

**Conventions:** plugin repo generated from the
`build-omarchy-plugins` v0.2.3 toolkit (`skills/omarchy-plugin-scaffold`);
its `manifest.json` schema, template lifecycle contracts, portable validator,
and demo/reversible-harness rules are normative — deviate only where the
spec says so. QML: 2-space indent, template import set (`QtQuick`,
`Quickshell`, `Quickshell.Io`, `Quickshell.Wayland`, `qs.Commons`, `qs.Ui`),
theme tokens only (`Color.*`, `Style.*`, `Border.*`). JS: no dependencies;
`js/ReactorState.js` must run under plain Node (CommonJS `module.exports`).
Rust (T1): existing crate conventions; `cargo fmt` + `clippy -- -D warnings`
+ `cargo test` clean.

**Work location:** T1 in a new `ph-reactor` worktree
(`~/.worktrees/ph-reactor-omarchy-plugin`, branch `feat/omarchy-plugin` from
`main`). T2–T8 in the new plugin repo `~/ph-reactor-omarchy` (branch
`feat/omarchy-plugin`; `main` only receives the merged result). This machine
has no Omarchy host and no Qt 6: QtTest and live-shell layers are *documented*,
not claimed (evidence record lists them as environment limits).

---

## Task 1 — Machine layer: `status --json` (ph-reactor)

- [ ] New worktree `~/.worktrees/ph-reactor-omarchy-plugin`, branch
  `feat/omarchy-plugin` from `main`.
- [ ] `cli.rs`: `Command::Status` gains `#[arg(long)] json: bool`.
- [ ] `daemon.rs::status`: when `--json`, print `serde_json::to_string` of
  the selected snapshot (live from `/api/status` or the degraded one) and
  return; text path unchanged.
- [ ] `Cargo.toml`: version `0.1.0` → `0.2.0` (the plugin's minimum CLI
  contract is "has `status --json`").
- [ ] Contract test (colocated in `daemon.rs` or `status.rs`): the degraded
  snapshot serializes to the exact shape the plugin depends on — top-level
  keys `version`, `switchboard{running,healthy,version,port,restarts,last_event}`,
  `drives[{name,url,paused,status,detail}]`, `settings{url}`, `updated_at`;
  drive `status` in the documented set; plus a clap parse test that
  `status --json` is accepted.
- [ ] README: `status --json` row + a "Stable CLI contract for shell
  plugins" note (0.2.0+, `status --json`, `drive add … --token-env`,
  `run --daemonize`, `stop`).
- [ ] Verify: `cargo test`, `cargo fmt --check`, `cargo clippy -- -D
  warnings`; live: build, start the daemon, `status --json` (daemon up),
  stop, `status --json` (degraded shape); both parse as JSON.
- [ ] Commit (2): (a) version bump + flag + handler, (b) contract test +
  README. Push the branch.

## Task 2 — Plugin repo scaffold

- [ ] `python3 /tmp/build-omarchy-plugins/skills/omarchy-plugin-scaffold/scripts/new_plugin.py
  --id io.github.powerhouse-inc.ph-reactor-omarchy --name "ph-reactor"
  --kind service --kind bar-widget --author "powerhouse-inc"
  --description "Status and drive control for the ph-reactor local switchboard daemon"
  --version 0.1.0 --default-section right
  --output ~/ph-reactor-omarchy --no-git` — then `git init` + branch
  `feat/omarchy-plugin` (the generator's git init stays out; we own history).
- [ ] Finalize `manifest.json`: `barWidget.category: "Developer Tools"`;
  `defaults: {"autoStart": false}`; `schema: [{key "autoStart", type
  "boolean", label "Start the reactor when Omarchy starts", defaultValue
  false}]`; keep entry-point mapping, ID, license MIT.
- [ ] Toolkit validator passes: `python3
  /tmp/build-omarchy-plugins/skills/omarchy-plugin-test/scripts/validate_plugin.py
  ~/ph-reactor-omarchy` (and `--json --security`; fix every finding).
- [ ] Commit: "plugin scaffold (manifest, CI, license, validator)".

## Task 3 — State machine: `js/ReactorState.js`

- [ ] Pure module (Node CommonJS, no deps):
  - `cmpSemver(a, b)` — numeric tuple compare, prerelease < release, unknown
    → 0-safe (never throws on garbage).
  - `MIN_CLI = "0.2.0"`.
  - `classifyProbe(versionOut, ok)` → `missing | unsupported(version) |
    cli(version)`.
  - `normalizeStatus(payloadJson, cli)` → the spec's normalized state object
    (phase/daemon/drives/message); handles: well-formed snapshot, malformed
    JSON, missing fields, drive-status passthrough, message assembly
    (bounded: first 200 chars, no newlines).
  - `reduce(state, event)` — events `probe(ok, json)` / `probeFail(bounded
    stderr, timeout)`; keeps last known good drives on `error`; idempotent
    (an in-flight flag is the caller's; the reducer is pure).
- [ ] Fixtures `demo/fixtures/`: `status-ready.json` (2 drives: synced +
  paused; fictional names/urls `demo.invalid`), `status-degraded.json`
  (one `error` drive), `status-stopped.json` (degraded shape, running
  false), `status-starting.json`, `cli-old-version.txt` (`ph-reactor
  0.1.0`), `cli-not-found.txt`, `status-malformed.txt`.
- [ ] `js/ReactorState.test.js` via `node --test`: all seven phases;
  semver edges (0.1.9 < 0.2.0, prerelease, garbage); malformed/timeout
  classification; last-good retention; error→ready recovery; no crash on
  any fixture.
- [ ] `./tests/run` updated to run the node suite; all pass.
- [ ] Commit: "state machine + fixtures + node tests".

## Task 4 — `Service.qml`

- [ ] Singleton `Item` (template header: `omarchyPath`, `shell`, `manifest`
  properties).
- [ ] CLI invocation via `Quickshell.Io.PhProcess`: `args` arrays only;
  `stdoutMax`/`stderrMax` bounded (16 KB / 4 KB); 10 s kill watchdog
  (`Timer` + `PhProcess.kill`); overlap rejection (`busy` flag).
- [ ] `refresh()`: version probe (once, cached) → `status --json` →
  `ReactorState.reduce` → publish `state` (JS object property) for widget
  bindings; missing → backoff interval 60 s, else 10 s (`Timer` repeat).
- [ ] Actions `start/stop/driveAdd/driveRemove/drivePause/driveResume/
  driveResync`: each builds the exact argv (token env name passed as
  `--token-env NAME` only), runs with 15 s watchdog, returns a bounded
  one-liner, then `refresh()`.
- [ ] `autoStart` property (default false): on first `stopped` observation,
  if true → `start()`.
- [ ] `IpcHandler { target: "io.github.powerhouse-inc.ph-reactor-omarchy" }`
  with the nine spec methods; `status()` returns bounded JSON (drives
  truncated to 10 entries, detail ≤ 200 chars each).
- [ ] Commit: "service: CLI bridge, polling, IPC".

## Task 5 — `BarWidget.qml` + `Panel.qml` + `SettingsLink.qml`

- [ ] `BarWidget.qml`: per the spec (glyphs `● ◐ ○ ! ✕`; horizontal label +
  vertical glyph-only; tooltip = message + drive summary; left → open
  panel; right → service refresh; service singleton lookup with inert
  fallback; popout contract `opened/open()/close()`; `autoStart` pushed to
  the service on completed + on setting change).
- [ ] `Panel.qml`: private popout per the template window pattern
  (namespace, top layer, on-demand focus, ignore exclusion, BorderSurface);
  header (phase chip + message + versions + settings link via
  `Loader`-loaded `SettingsLink.qml` — degrades to selectable URL text);
  drives `ListView` (name, chip, detail, row actions; remove = two-tap arm);
  add-drive form (URL required; name; token env; offline checkbox; result
  line); footer Start/Stop.
- [ ] `SettingsLink.qml`: `qs.Commons.OpenUrl` wrapper (url property);
  isolated so an import gap cannot break the panel.
- [ ] Commit: "bar widget + popout panel".

## Task 6 — Reversible demo harness

- [ ] `demo/fixtures/bin/ph-reactor`: bash fixture CLI honoring
  `--version` (0.2.0), `status --json` (reads `PHR_DEMO_STATE` file →
  fixture payload), `run --daemonize` / `stop` (toggle the state file
  stopped↔ready), `drive add|remove|pause|resume|resync` (append to an
  invocation log, mutate the state file deterministically). Fictional only
  (`demo.invalid`, `PH_DEMO_TOKEN` name).
- [ ] `demo/run`: the reversible harness per the demo skill — collision-
  resistant backups of `shell.json` + plugin install (refuse on stale
  recovery artifacts), fixture bin first on the shell PATH, enable the
  plugin, wait for the invocation log to reach `version → status` (machine
  ready condition), then idle for the operator to capture the screenshot;
  `trap` restore on EXIT/INT/TERM; verify restoration; print exact paths if
  restore fails.
- [ ] `demo_preflight.py ~/ph-reactor-omarchy` passes.
- [ ] Commit: "reversible demo harness + fixture CLI".

## Task 7 — Validation, README, preview, CI

- [ ] `./tests/run` green end-to-end (validator + node tests + fixtures +
  `bash -n`); toolkit `validate_plugin.py --json --security` — record the
  report; fix or explicitly justify every advisory.
- [ ] README final: what it is; the two-layer model in one paragraph;
  install (prerequisite: `ph-reactor` ≥ 0.2.0 on PATH — snap/brew/script
  pointer to the ph-reactor README; `omarchy plugin add <repo-url>`);
  usage (bar, panel, IPC table with all nine methods and result shapes);
  removal (plugin removal keeps daemon + data; `ph-reactor stop` to stop);
  settings (`autoStart`); testing section (portable / QtTest with exact
  command / live demo) and the environment-limit record; dependencies.
- [ ] `preview.svg` (template) → `preview.png` via PIL (bar mock: glyph +
  label + open panel); marketplace size limits respected.
- [ ] CI `test.yml` (template) committed; note the deferred QtTest job.
- [ ] Commit: "docs, preview, CI".

## Task 8 — Evidence + publication prep (ph-reactor repo)

- [ ] `docs/superpowers/sdd/2026-09-12-omarchy-plugin/`: `progress.md`
  (task table), terse per-task reports, evidence files (validator JSON,
  node-test output, fixture-CLI transcript, T1 live `status --json`
  transcripts up/down, clippy/fmt results, Omarchy-host limitation note).
- [ ] `prepare_submission.py ~/ph-reactor-omarchy` → exact marketplace
  issue title + body saved as evidence (NOT opened; owner approves first).
- [ ] Plugin repo: merge `feat/omarchy-plugin` → `main`, tag `v0.1.0`.
- [ ] ph-reactor: push `feat/omarchy-plugin` (open PR if the remote
  accepts); final summary to the user (what was built, what the user must
  do on their Omarchy machine, what awaits approval).
