# Rust Reactor (`ph-reactor`) — Implementation Plan

> Provenance: written for the powerhouse monorepo (crate at
> `apps/ph-reactor`); moved to this repository on 2026-09-12 — paths below
> are rebased to the repository root.
> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** a single Rust binary `ph-reactor` (Linux) that runs a local
Powerhouse switchboard in the background with a status-bar tray icon and a
drives-configuration menu; bootstraps Node + the switchboard package
automatically; syncs remote drives (e.g. the
`powerhouse-knowledge` vault drive) by driving the local switchboard's own
sync manager over its MCP endpoint; auto-installs missing document model
packages from `https://registry.dev.vetra.io`; installable via snap and a
Homebrew formula.

**Architecture:** see
`docs/superpowers/specs/2026-09-11-rust-reactor-design.md`. One crate at
the repository root (crate and binary `ph-reactor`). Modules: `config`,
`paths`, `bootstrap/{node,switchboard}`, `supervisor`, `mcp`, `drives`,
`registry`, `tray/{mod,menu}`, `settings/{mod,page}`, `daemon`. All
network/FS/process side effects go through narrow functions with injected
clocks/clients so unit tests run offline (mock HTTP server for MCP and
drive-info; temp dirs for state; fake `node`/`npm` shims for bootstrap).
The daemon's core is a `tokio` task graph; the tray is the only D-Bus
consumer (zbus). No GTK.

**Conventions:** Rust 2021 edition, `anyhow` for app errors at the daemon
boundary, `thiserror` for library-level enums; `tracing` for logs (no
`println!` outside the CLI's human output); `serde` types mirror the JSON
shapes in the spec; tests are `#[cfg(test)]` modules colocated in each file
plus `tests/` integration tests; `cargo fmt` + `cargo clippy -- -D warnings`
must pass; no new dependencies beyond the manifest list in the spec's Files
table; all paths under the state dir come from `paths::StatePaths` (testable
via `PH_REACTOR_STATE_DIR`).

**Work location:** this repository (the work happened in the powerhouse
monorepo worktree `~/.worktrees/powerhouse/rust-reactor`, branch
`feat/rust-reactor`; the crate moved to this repository on 2026-09-12).
Validate with `cargo` only. Incremental commits per task.

---

## Task 1 — Crate scaffold, paths, config

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `.gitignore`
- Create: `src/main.rs`, `src/lib.rs`, `src/paths.rs`, `src/config.rs`

- [x] **Step 1:** `Cargo.toml` with deps: `tokio` (rt-multi-thread,
      macros, process, signal, time, fs, io-util), `clap` (derive),
      `serde` (derive) + `serde_json`, `reqwest` (rustls-tls, json,
      stream), `zbus` (dbus-feature for session bus), `axum`, `tracing` +
      `tracing-subscriber` (json, fmt), `dirs`, `thiserror`, `anyhow`,
      `flate2`, `sha2`, `futures-util`, `rand` (for backoff jitter).
      Package name `ph-reactor`, binary `ph-reactor`, edition 2021,
      `rust-version = "1.80"`.
- [x] **Step 2:** `paths.rs`: `StatePaths { root, node_dir, switchboard_dir,
      data_dir, logs_dir, run_dir, config_file }` resolved from
      `PH_REACTOR_STATE_DIR` env else `$HOME/.ph/reactor`; `ensure_dirs()`
      (create all, `0700` on root).
- [x] **Step 3:** `config.rs`: types per spec (`ReactorConfig` v1:
      switchboard{port, packageSpec, npmRegistry, node{minimumVersion,
      preferSystem}}, registry, packages, drives[], settings{host,port},
      logLevel); `Drive { name, url, token_env, available_offline, paused }`;
      defaults (port 4001, spec `@powerhousedao/switchboard@latest`,
      registry `https://registry.dev.vetra.io`, settings port 4002,
      logLevel info, knowledge-note in packages); `load()` (missing →
      defaults written; corrupt → move aside to `.corrupt-<ts>`, then
      defaults; unknown fields preserved via `#[serde(flatten)]
      extra: BTreeMap`); `save()` atomic (tmp file + rename, `0600`);
      `set(dotted_key, json_value)` with validation.
- [x] **Step 4:** `lib.rs` exposes modules; `main.rs` clap shell:
      subcommands `run` (default; flags `--daemonize`, `--state-dir`),
      `stop`, `status`, `drive` (add/remove/list/pause/resume/resync),
      `doctor`, `config` (show/set), `logs` (`--follow`,
      `--switchboard`), `--version`. All commands parse and dispatch to
      placeholder functions that `todo!()` — except `config show`, which
      works end-to-end (load + pretty-print JSON).
- [x] **Step 5:** tests: config defaults (fresh dir → file written with
      defaults; reload stable); corrupt config recovery; atomic save
      (content identical after kill — use temp dir, write twice);
      dotted-key set (valid + invalid key rejected); paths resolution
      (env override + home fallback).

**Acceptance:** `cargo test` green; `cargo run -- config show` prints the
default JSON; no network/Node needed.

---

## Task 2 — Node bootstrap

**Files:**
- Create: `src/bootstrap/mod.rs`, `src/bootstrap/node.rs`
- Test: `src/bootstrap/node.rs` (unit), `tests/node-bootstrap.rs`

- [x] **Step 1:** `NodeRuntime { path: PathBuf, version: String }`;
      `probe_system(minimum) -> Option<NodeRuntime>` (runs `node --version`
      with a 5 s timeout; semver comparison, prerelease-aware, e.g.
      `v24.1.0` ≥ `24`, `v22.11.0` < `24`); `resolve(minimum, preferSystem,
      state) -> Result<NodeRuntime>` — system ok → use; else
      `download_private(state)`.
- [x] **Step 2:** `download_private`: pinned
      `NODE_DIST_VERSION = "v24.11.1"`; arch detection (`x86_64` →
      `x64`, `aarch64` → `arm64`, else error "unsupported architecture");
      URL `https://nodejs.org/dist/{v}/node-{v}-linux-{arch}.tar.gz`;
      fetch `SHASUMS256.txt`, match the tarball line, sha256-verify the
      downloaded bytes (mismatch → delete, retry, max 3); stream to a `.tmp`
      file; extract with flate2+`tar`-style manual walk (use the `tar`
      crate? — add `tar` to deps if needed; prefer `flate2` + `tar`);
      rename `<state>/node/.extract-<pid>` → `<state>/node/node-{v}-linux-{arch}`;
      marker file `.node-ok` with version.
- [x] **Step 3:** idempotency: marker present + `bin/node --version`
      matches pin → skip download.
- [x] **Step 4:** tests: semver gate (cases above + `v24.0.0-rc.1` < 24);
      SHASUMS line parsing (real sample lines incl. macOS entries that must
      not match); arch mapping; corrupt-marker recovery; full download
      test gated behind `PH_REACTOR_LIVE=1` env (skipped in CI default).
- [x] **Step 5:** live check (manual, evidence): on this machine with
      `PH_REACTOR_STATE_DIR=$(mktemp -d)`, force `preferSystem=false`
      → private node downloads, hash verifies, `bin/node --version` works.

**Acceptance:** `cargo test` green; live run on this machine bootstraps a
working private Node 24 into a temp state dir in under ~60 s.

---

## Task 3 — Switchboard bootstrap (npm install + config generation)

**Files:**
- Create: `src/bootstrap/switchboard.rs`
- Modify: `src/bootstrap/mod.rs`
- Test: unit + `tests/switchboard-bootstrap.rs`

- [x] **Step 1:** `SwitchboardInstall { version, node_modules, entry }`;
      `ensure(node, state, cfg) -> Result<SwitchboardInstall>`:
      - marker `<state>/switchboard/.install-ok` holding the requested spec
        + resolved version; present & spec unchanged & entry file exists →
        return.
      - else run `<node>/bin/npm install --prefix <state>/switchboard
        <spec> --registry <npmRegistry>` (spawn, stream stdout/stderr to
        the log, 10 min timeout, 2 retries).
      - post-install: read `<state>/switchboard/node_modules/@powerhousedao/
        switchboard/package.json` → version; verify
        `dist/index.mjs` exists; write marker.
- [x] **Step 2:** `write_powerhouse_config(state, cfg)`: generates
      `<state>/switchboard/powerhouse.config.json` exactly as the spec's
      JSON (port, db url = `<state>/data`, registry url, packages array,
      auth disabled); idempotent (only rewrite when input changed).
- [x] **Step 3:** `spawn_env(state, cfg) -> BTreeMap<String,String>`:
      `PH_SWITCHBOARD_PORT`, `PH_SWITCHBOARD_DATABASE_URL`,
      `PH_REGISTRY_URL`, `PH_REGISTRY_PACKAGES`, `DYNAMIC_MODEL_LOADING=1`,
      `LOG_LEVEL`, `PH_PGLITE_IN_MEMORY=0`, `HOME` (unchanged), `NODE_ENV=production`.
- [x] **Step 4:** tests with a **fake node dir** (test writes a fake
      `bin/npm` shell script that records argv and creates the expected
      `package.json`+`dist/index.mjs`): install runs once, idempotent
      second call (no second spawn), spec change triggers reinstall,
      failed npm (non-zero fake) → Err with tail of the log;
      `powerhouse.config.json` generation (byte-exact for default cfg;
      re-run without change does not rewrite — mtime stable).
- [x] **Step 5:** live check: real `npm install` of
      `@powerhousedao/switchboard@latest` from npmjs into a temp state dir
      (evidence: `dist/index.mjs` present, version recorded).

**Acceptance:** `cargo test` green; live bootstrap produces a runnable
switchboard tree + correct `powerhouse.config.json`.

---

## Task 4 — Supervisor (spawn, health, backoff, logs, stop)

**Files:**
- Create: `src/supervisor.rs`, `src/logrotate.rs`
- Test: unit + `tests/supervisor.rs`

- [x] **Step 1:** `Supervisor { ... }`: `start(node, install, env)` spawns
      `node <entry>` with `cwd = <state>/switchboard`, stdio piped to a
      `LogSink` (`<state>/logs/switchboard.log`, rotate at 10 MB, keep 3,
      line-buf flush); records pid in `<state>/run/switchboard.pid`.
- [x] **Step 2:** health loop (tokio task): `GET
      http://127.0.0.1:{port}/health` every 5 s, 3 s timeout; 3 consecutive
      failures or process exit → `stop_child()` (SIGTERM, 5 s grace,
      SIGKILL) → restart with backoff (1 s doubling, 300 s cap, jitter
      ±10 %, reset after 5 min healthy); 5 consecutive failed boots (exit
      within 30 s of spawn, or health never passed) → `Error` state (stop
      retrying; `restart()` method re-arms).
- [x] **Step 3:** `Status` enum: `Starting, Running { port, version },
      Restarting { attempt, next_in }, Error { message }`; `version` from
      install marker; `port` from env (actual port parsing from startup log
      line `listening on` — best-effort; fall back to configured port).
- [x] **Step 4:** graceful `shutdown()`: SIGTERM to the child, await exit
      (10 s), remove pidfile; SIGTERM/SIGINT to the daemon → supervisor
      `shutdown()` then exit.
- [x] **Step 5:** tests: with a **fake node script** (a `node` shell stub
      that prints a ready line and sleeps / exits on demand via a signal
      file): (a) healthy child stays up across a health-flap window;
      (b) killed child → exactly one restart, backoff timer observed
      (injected clock); (c) crash-loop (child exits immediately 5×) →
      Error state, no further spawns; (d) SIGTERM to the stub → child
      receives SIGTERM (stub records it) and pidfile removed; (e) log
      rotation (write > 10 MB quickly via stub → 2 generations exist,
      oldest removed at 3).

**Acceptance:** `cargo test` green incl. all five scenarios; a real
switchboard (Task 6 E2E) stays up through a manual `kill -9`.

---

## Task 5 — MCP client (Streamable HTTP)

**Files:**
- Create: `src/mcp.rs`
- Test: `src/mcp.rs` unit + `tests/mcp-client.rs`

- [x] **Step 1:** `McpClient { http, url }`; `connect()`: POST
      `{url}/mcp` body
      `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"ph-reactor","version":"<VER>"}}}`
      with headers `Content-Type: application/json`,
      `Accept: application/json, text/event-stream`; capture
      `Mcp-Session-Id` response header; handle both `application/json`
      (single JSON-RPC response) and `text/event-stream` (read the first
      `data:` line) response encodings; then POST
      `notifications/initialized` (no id).
- [x] **Step 2:** `call_tool(name, arguments: Value) -> Result<Value>`:
      POST `tools/call` with a fresh id; parse
      `result.structuredContent` ?? parse `result.content[0].text` as JSON;
      `isError: true` → Err with the tool's error text; JSON-RPC `error`
      → Err(mapped); HTTP 404/410 with session header set → re-`connect()`
      once then retry the call; 401 → Err(AuthRequired).
- [x] **Step 3:** session reuse: keep the session id across calls (one
      session per switchboard incarnation; drop on reconnect); timeout 30 s
      per call (drive adds can be slow during backfill).
- [x] **Step 4:** tests against an **axum test server** (in-process,
      bound to 127.0.0.1:0) implementing the Streamable-HTTP contract:
      (a) full handshake (initialize → session id echoed → initialized
      notification received by server); (b) `tools/call` success with
      structuredContent; (c) success with text-only content (JSON in
      text); (d) tool `isError` → Err; (e) JSON-RPC error → Err; (f) SSE
      response encoding; (g) session-expired (404 on second call) →
      auto-reconnect + retry succeeds; (h) 401 → AuthRequired.

**Acceptance:** `cargo test` green; the client works against the *real*
switchboard MCP in Task 6 (no fallbacks invented — the spec's protocol
shapes are the contract).

---

## Task 6 — Drives (info, add/remove/pause/resume, status) + live E2E

**Files:**
- Create: `src/drives.rs`, `src/registry.rs`
- Test: unit + `tests/drives.rs`

- [x] **Step 1:** `DriveInfo { id, slug, name, icon?, meta,
      graphql_endpoint }`; `fetch_drive_info(url, token: Option<&str>)`
      (Bearer header only when token present; non-2xx → `DriveError::Auth
      {status}` for 401/403, `DriveError::Connection{status}` otherwise;
      body validated: `id` and `graphqlEndpoint` strings required —
      mirrors `drive-info.ts`).
- [x] **Step 2:** `DriveManager { mcp, config, http }`:
      - `add(url, name?, token_env?, available_offline)`: fetch info →
        registry pre-check (`GET {registry}/packages/<name>` for each
        configured boot package; 404 → warning, not failure) → MCP
        `addRemoteDrive { url, options: { availableOffline } }` →
        persist config entry (name default = info.name; token_env stored)
        → poll MCP `getDrive { driveId }` until Ok (every 5 s, 10 min
        hard timeout) → status `Synced`.
      - `remove(index)`: resolve entry → MCP `deleteDrive { driveId }`
        (accept `success: false` with a warning) → drop config entry.
      - `pause`/`resume`: flip `paused` in config (persist).
      - `resync(index)`: MCP `addRemoteDrive` again (idempotent) + clear
        the materialization timeout (status returns to `Bootstrapping`
        until `getDrive` answers).
      - status source of truth: config entries + live probes (switchboard
        health + `getDrive` per unpaused drive every 30 s).
- [x] **Step 3:** `registry.rs`: `check_package(registry, name) ->
      Result<PackageInfo>` (`GET {registry}/packages/{name}`; parse
      `{name, manifest:{documentModels:[...]}}`; 404 → `Err(NotFound)`).
- [x] **Step 4:** unit tests with a mock HTTP server: drive-info Bearer
      header presence/absence; status mapping (401→Auth, 500→Connection);
      malformed body (missing graphqlEndpoint) → Err; `addRemoteDrive`
      idempotency is server-side (the manager always calls it; assert the
      manager calls exactly once per `add()`); pause persists across
      `DriveManager` reconstruction (config round-trip); resync flow
      (two MCP calls observed in order).
- [x] **Step 5:** **Live E2E (the money shot)** on this machine:
      `PH_REACTOR_STATE_DIR=$(mktemp -d) ./ph-reactor run` (foreground,
      pty) → wait for `Switchboard running :4001` →
      `./ph-reactor drive add https://light-colt-c497cfbd-switchboard.vetra.io/d/powerhouse-knowledge`
      → expect: drive info fetched (no token), switchboard MCP
      `addRemoteDrive` accepted, backfill proceeds (switchboard log shows
      sync activity; `getDrive` materializes the `Powerhouse Knowledge`
      drive), `drive list` shows it `Synced`. Evidence: terminal capture +
      `ph-reactor status` output + a `getDrive` response excerpt showing
      the drive name. Then `drive remove` → gone from `drive list`.
      Then kill the daemon (SIGTERM) → restart → drive still listed
      `Synced` (persistence + idempotent re-registration).

**Acceptance:** all unit tests green; the live E2E above completes with
the real `powerhouse-knowledge` drive mirrored in the local switchboard
(verify independently: `curl -s localhost:4001/d/powerhouse-knowledge` or
the GraphQL `findDocuments` shows the drive document).

---

## Task 7 — Tray (StatusNotifierItem + DBusMenu)

**Files:**
- Create: `src/tray/mod.rs`, `src/tray/menu.rs`, `src/assets/icon-22.png`,
      `src/assets/icon-32.png`
- Test: `src/tray/menu.rs` (XML model), integration smoke in E2E

- [x] **Step 1:** `menu.rs`: `MenuItem { id, label, enabled, checked,
      submenu: Option<Vec<MenuItem>>, action: Option<TrayAction> }`;
      `TrayAction` enum (OpenSettings, AddDrive, ShowLogs, RestartSwitchboard,
      Quit, DrivePause{idx}, DriveResume{idx}, DriveResync{idx},
      DriveRemove{idx}); `build_menu(snapshot) -> Vec<MenuItem>` per the
      spec's menu layout (index-based ids `0..`, stable across rebuilds
      while the drive set is unchanged); `dbusmenu_xml(menu, path) ->
      String` (the DBusMenu XML: `<MenuBar>`, `<Menu id=...>` nesting,
      `<Separator/>`, `type="Separator"` items, `enabled`, `visible`,
      `label`, data- attributes for action mapping).
- [x] **Step 2:** `mod.rs`: `Tray::start(snapshot_rx, action_tx)`:
      - unique bus name `org.kde.StatusNotifierItem-{uid}-{pid}-0`;
      - implement `org.kde.StatusNotifierItem` (properties Id, Category,
        Status, Title, Description, IconName, IconPixmap (ARGB32 from the
        embedded PNGs — decode with a tiny pure-Rust PNG reader: use the
        `image` crate? — add `image` (png feature only) to deps if needed;
        or pre-convert assets to raw ARGB bytes at build time with
        `include_bytes!` of a `.rgba` file — prefer the latter, zero
        runtime deps), ToolTip, MenuPath, StatusNotifierItemVersion);
      - register with the watcher if present (call
        `RegisterStatusNotifierItem(path)`); if the watcher is absent →
        log once, return `Ok(None)` (headless path).
      - `org.kde.DBusMenu` interface: `GetLayout(parent, recursionDepth,
        propertyNames) -> (revision, layout xml)`, `Event(id, eventId,
        data, timestamp)` → map to `TrayAction` on the action channel;
        `AboutToShow` → `Update()` + `ItemsChanged`/`LayoutChanged` signals
        after a rebuild.
      - property updates on snapshot change: `Status`, `Title`,
        `ToolTip.Text`, `IconPixmap` (needs-attention icon variant: same
        mark with a red dot — second asset), `UpdateProperties`/
        `NewIcon`/`StatusChanged`/`ToolTipChanged` signals.
- [x] **Step 3:** menu XML unit tests: default snapshot (1 drive synced)
      produces the exact XML structure (assert element tree, ids,
      separators, disabled states); needs-attention snapshot flips status
      property value; action mapping table (id → action) round-trips.
- [x] **Step 4:** runtime smoke (this machine, if a session bus exists in
      this environment): start the daemon with a tray-capable bus; assert
      the SNI object is registered (via `dbus-send`/`gdbus` introspection
      from a test harness); if no session bus (container), the headless
      path is exercised instead (daemon healthy, one log line) — either
      outcome is recorded as evidence.

**Acceptance:** `cargo test` green; on a session bus the SNI object
registers and the menu XML serves; headless environment degrades cleanly
(daemon keeps running).

---

## Task 8 — Settings page + JSON API

**Files:**
- Create: `src/settings/mod.rs`, `src/settings/page.rs`
- Test: `tests/settings-api.rs`

- [x] **Step 1:** axum router bound to `127.0.0.1:{settings.port}`:
      `GET /` → the single page (embedded, `include_str!`), `GET
      /api/status` → `StatusSnapshot` JSON, `POST /api/drives`,
      `POST /api/drives/{idx}/{action}` (pause|resume|resync), `DELETE
      /api/drives/{idx}`, `POST /api/switchboard/restart`; JSON errors
      `{"error": msg}`; all mutating endpoints run the DriveManager/
      supervisor operations through the shared task (one writer — the
      daemon task serializes commands via a mpsc, so concurrent page
      actions cannot race).
- [x] **Step 2:** `page.rs`: one HTML document, inline CSS/JS (vanilla,
      no frameworks, no external requests): header (version, switchboard
      state chip, restart/stop buttons), drives table (name, URL, status
      chip with color, per-row buttons: pause/resume, resync, remove with
      confirm), add-drive form (url required, name + token-env optional),
      registry section (registry URL, boot packages with per-package
      check icon), auto-refresh `/api/status` every 10 s (fetch, re-render
      chips only). ~300–400 lines total. No build step, no CDN.
- [x] **Step 3:** tests (axum `oneshot` + a stubbed daemon command
      channel): status JSON shape (exact fields); add-drive validates url
      (400 on non-URL); pause/resume/resync dispatch the right command;
      delete dispatches; switchboard restart dispatches; 404 for unknown
      index; loopback-only bind asserted (socket is 127.0.0.1).

**Acceptance:** `cargo test` green; in the live E2E (Task 6 re-run) the
page renders at `http://127.0.0.1:4002/` (screenshot or `curl` capture of
the HTML + one `/api/status` JSON).

---

## Task 9 — Daemon lifecycle + CLI wiring

**Files:**
- Create: `src/daemon.rs`
- Modify: `src/main.rs` (wire real implementations into every subcommand)
- Test: `tests/cli.rs`

- [x] **Step 1:** `daemon.rs`: `run(foreground|daemonize)`: state dir
      ensure → single-instance lock (`<state>/run/lock`, O_EXCL, pid +
      start time; stale-lock detection by pid liveness) → bootstrap
      (node → switchboard) → supervisor start → tray start (best-effort) →
      settings server start → drive re-registration (config entries,
      idempotent) → status poller task (30 s) → command mpsc consumer →
      signal handling (SIGTERM/SIGINT → ordered shutdown: poller, tray,
      settings, supervisor, release lock); `daemonize`: double-fork (or
      `tokio::task::spawn` + parent exit after child readiness signal
      file), pidfile `<state>/run/ph-reactor.pid`, stdout closed.
- [x] **Step 2:** CLI wiring: `status` (no daemon required: reads state +
      probes); `stop` (SIGTERM via pidfile, wait ≤ 10 s); `drive *` and
      `config set` talk to the running daemon over the settings API
      (curl-equivalent via reqwest); `doctor` (standalone: node version,
      npm present, registry reachable + boot package check, switchboard
      install state, switchboard health, MCP initialize round-trip,
      session-bus presence — each line `ok|warn|fail: detail`); `logs`
      (tail the relevant file, `--follow` streams).
- [x] **Step 3:** tests: lock semantics (second instance refuses; stale
      lock taken over); `stop` against a fake daemon pidfile (spawn a
      sleep, SIGTERM, exit 0); doctor lines against a temp state dir with
      and without bootstrap; `status` output shape (drive table +
      switchboard line) — all with injected state, no real switchboard.
- [x] **Step 4:** manual pass: `run --daemonize` from a pty (backgrounds,
      pidfile present), `status`, `stop` (clean exit, pidfile gone,
      switchboard child gone too).

**Acceptance:** `cargo test` green; the daemonize/status/stop manual pass
works; a second concurrent `run` refuses cleanly.

---

## Task 10 — Packaging (snap, Homebrew, release workflow)

**Files:**
- Create: `snap/snapcraft.yaml`, `snap/ph-reactor.desktop`,
      `snap/autostart/ph-reactor.desktop`, `packaging/brew/ph-reactor.rb`,
      `packaging/desktop/ph-reactor.desktop`,
      `.github/workflows/rust-reactor.yml`
- Modify: `README.md` (install section)

- [x] **Step 1:** `snapcraft.yaml`: `base: core24`, `confinement: strict`,
      grade stable; dump part for the musl binary (built in CI); app
      `ph-reactor` with `command: bin/ph-reactor`, `autostart: true`,
      plugs `home, network, network-bind, dbus`; env
      `PH_REACTOR_STATE_DIR=$SNAP_USER_DATA/ph-reactor` (snap-safe state,
      survives the home-interface layout); desktop file (NoDisplay=true,
      the tray is the UI).
- [x] **Step 2:** `packaging/brew/ph-reactor.rb`: `Formula`
      `ph-reactor`, stable url to the GitHub release asset
      `ph-reactor-<ver>-linux-x86_64.tar.gz` (sha256 filled at release
      time; `version "0.1.0"` placeholder + `url` templated by the
      release script), `depends_on "curl"` (for xdg-open fallback only),
      `install`: `bin.install "ph-reactor"`; `test`: `system
      bin/"ph-reactor", "doctor"`-ish smoke (`--version`); guard
      `on_macos!` with a clear "Linux only for now" message.
- [x] **Step 3:** `.github/workflows/rust-reactor.yml`: trigger on
      `workflow_dispatch` + tag `ph-reactor-v*`; jobs: build (ubuntu-24.04,
      install `x86_64-unknown-linux-musl` via rustup, `cargo build
      --release --target ...`, `cargo test` first), package (tarballs
      `ph-reactor-<ver>-linux-{x86_64,aarch64}.tar.gz` + `SHA256SUMS`),
      release (softprops/action-gh-release with assets); separate
      `snap` job (`snapcraft` via `samuelmeuli/action-snapcraft` or
      `docker run` of the snapcraft image, manual dispatch only) pushing
      to the `edge` channel.
- [x] **Step 4:** verify locally what the environment allows:
      `snapcraft` may be unavailable in this environment — if so,
      validate the YAML parses (`python3 -c yaml.safe_load`), record the
      gap; brew: if `brew` is present run
      `brew style packaging/brew/ph-reactor.rb` (rubocop), else lint by
      eye and note it; `--version` output of the release-built binary.
- [x] **Step 5:** README: install via snap
      (`snap install ph-reactor --edge`), via brew
      (`brew tap powerhouse-inc/powerhouse && brew install ph-reactor`),
      via the raw binary tarball; first-run behavior (bootstrap time),
      state layout, tray note (KDE / GNOME-with-indicator; headless),
      token setup for private drives, troubleshooting (doctor, logs).

**Acceptance:** YAML parses; formula passes `brew style` where available;
workflow file is syntactically valid YAML and references real paths;
README renders with the right commands.

---

## Task 11 — Final verification, evidence, SDD reports, commits

- [ ] `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
      `cargo test` — all green in the worktree.
- [ ] Full live pass on this machine (evidence to
      `docs/superpowers/evidence/rust-reactor/`):
      1. `doctor` output (all ok lines).
      2. first-run bootstrap: node download + switchboard npm install
         (timings, versions).
      3. `run` → switchboard health → tray registration attempt
         (session bus present or not — whichever this environment has).
      4. `drive add https://light-colt-c497cfbd-switchboard.vetra.io/d/powerhouse-knowledge`
         → backfill log excerpt → `drive list` (Synced) → settings
         `/api/status` JSON (drive present) → independent verification:
         the drive document is queryable on the local switchboard
         (GraphQL or MCP `getDrive` from a plain curl).
      5. `drive remove` → list empty; daemon restart → persistence
         behavior per spec.
      6. `stop` → clean exit (children gone, pidfiles gone).
      7. tray: if a session bus exists, capture the SNI registration
         (gdbus introspection + GetLayout XML dump); otherwise the
         headless log line.
- [ ] SDD: write `docs/superpowers/sdd/2026-09-11-rust-reactor/` —
      `progress.md` (per-task status + notes) and `task-N-brief.md` /
      `task-N-report.md` pairs for Tasks 1–10 (briefs: the plan section +
      the actual interface decisions made; reports: what was done, test
      results, evidence links, deviations).
- [ ] Commits (one per task, message style `feat(ph-reactor): <task>` /
      `chore(release): <packaging>` / `docs(superpowers): ...`),
      `git status` clean, branch `feat/rust-reactor` pushed (no PR in this
      environment unless asked — report the branch state).

**Acceptance:** the whole flow above is reproducible from a clean state
dir in one session; evidence files exist and match the claims; the
colleague can run `ph-reactor` on a Linux desktop, see the tray, and have
the vault drive syncing.
