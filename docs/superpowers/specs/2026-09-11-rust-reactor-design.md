# Rust Reactor (`ph-reactor`) — Design

## Problem

Running a local Powerhouse switchboard on a Linux desktop today means:
install Node 24, install `@powerhousedao/switchboard` by hand, run it in a
terminal, and register remote drives for sync from a browser (Connect) or by
script. There is no installable package, no background presence, no status-bar
icon, and no durable way to say "keep this knowledge-vault drive synced on my
machine". A colleague who wants the `powerhouse-knowledge` drive
(`https://light-colt-c497cfbd-switchboard.vetra.io/d/powerhouse-knowledge`)
mirrored locally has no product for it.

## Goals

1. An installable Linux package — **snap** (primary) and a **Homebrew formula**
   (secondary) — providing the `ph-reactor` binary.
2. `ph-reactor` runs a local switchboard **in the background**:
   - bootstraps a private Node.js ≥ 24 runtime when the system's Node is
     missing or too old (downloaded from nodejs.org, hash-verified),
   - installs `@powerhousedao/switchboard` (npm, pinned spec) into the user's
     state directory on first run,
   - spawns, health-monitors, and restarts the switchboard process with
     backoff; writes logs to the state directory; supports graceful stop.
3. A **status-bar (tray) icon** implemented as a `StatusNotifierItem` over
   D-Bus (`zbus`) with a `DBusMenu` — no GTK dependency, headless-safe,
   renders in KDE and in GNOME with an indicator-capable extension.
4. An **options menu** (tray menu + local web settings page) to configure the
   remote drives to sync: add by drive URL, remove, pause/resume, resync,
   status per drive.
5. **Drive sync reuses the existing Reactor sync engine.** The daemon
   registers each remote drive with the *local switchboard's own* sync
   manager through the switchboard's built-in MCP endpoint
   (`POST /mcp`, tool `addRemoteDrive` — see
   `packages/reactor-mcp/src/tools/reactor.ts`). The local switchboard
   then runs the battle-tested GqlChannel (touch/poll/push envelopes,
   cursors, backfill, backoff) against the remote switchboard, exactly as
   the browser Connect does
   (`packages/reactor-browser/src/actions/drive.ts`). The Rust side is
   the transport and UX; it does not re-implement the document engine,
   operation store, or sync protocol.
   - The daemon boots the switchboard with `channelScheme: "connect"`
     (active *request* channels; the installed default `"switchboard"`
     scheme is passive — it only serves a peer's channel and never
     syncs content from a remote on its own).
  - Published switchboard builds do not forward `channelScheme` or
    `jwtHandler` to the reactor builder in their boot path (verified
    in 6.2.2 and 6.2.3-dev.3), so an installed switchboard can only
    serve as a sync *server*. The daemon closes both gaps with a
    minimal, anchored, idempotent **post-install patch** to the
    installed server chunk (see *Bootstrap details*): the daemon is a
    sync client, and per-drive bearer tokens reach the sync channels.
    The patch is a no-op on a build that wires the options natively.
6. **Missing packages install automatically from
   `https://registry.dev.vetra.io`**, the same mechanism Connect already
   uses:
   - boot packages: the generated boot wrapper passes `registryUrl` and
     `packages` directly to `startSwitchboard` (options), so the
     switchboard's `HttpPackageLoader`
     (`packages/reactor-api/src/packages/http-loader.ts`) imports them
     from the registry CDN at boot;
   - runtime: with `DYNAMIC_MODEL_LOADING=1` the reactor's
     `documentModelLoader` fetches any document model referenced by an
     incoming operation but not already loaded, on demand, from
     `<registry>/-/cdn/<spec>/node/document-models/index.mjs`;
   - the daemon pre-checks configured boot packages against
     `GET <registry>/packages/<name>` and surfaces missing packages in
     the tray/settings instead of a boot crash.
7. **Configuration** in `~/.ph/reactor/config.json` (drives, registry,
   version pins, ports, log level): atomic writes, defaults for first run,
   editable via CLI and the settings page.

## Non-goals

- Re-implementing the Reactor document engine, operation store, reducers, or
  the GraphQL sync protocol in Rust. The Node Reactor (inside switchboard)
  is the source of truth; a Rust re-implementation of that engine is a
  separate, much larger effort.
- macOS/Windows (the layout leaves room; v1 is Linux x86_64/aarch64).
- Document editing UI (that is Connect / the switchboard GUI; the tray and
  settings page are for operations).
- Postgres-backed local storage (PGlite is the default; a `postgres://`
  database URL remains a config escape hatch via the generated
  `powerhouse.config.json`).
- Live per-remote sync telemetry (outbox sizes, cursor ordinals): the local
  sync manager exposes these only in-process today; the daemon reports
  drive-level state (see *State model*). A `syncRemotes` GraphQL query in
  `reactor-api` is a follow-up, not part of this change.
- Self-update of `ph-reactor` itself (snap/brew own the update path).

## Architecture

```
ph-reactor (Rust, single static binary, tokio)
|
+-- cli/         clap: run (default), drive add|remove|list|pause|resume|resync,
|                status, doctor, config, logs, stop
+-- config/      config.json load/save (atomic, versioned), defaults
+-- paths/       state layout under ~/.ph/reactor/
+-- bootstrap/
|    node/       system probe -> private nodejs.org tarball (sha256-verified)
|    switchboard/ npm install @powerhousedao/switchboard@<spec> into state dir,
|                generate powerhouse.config.json, write spawn env
+-- supervisor/  spawn node switchboard; poll GET /health; restart with
|                exponential backoff; rotating logs; graceful SIGTERM
+-- mcp/         Streamable-HTTP MCP client (initialize / tools/call),
|                session handling, JSON-RPC error mapping
+-- drives/      DriveInfo fetch (Bearer-aware); add/remove/pause/resume via
|                MCP; state model polled every 30 s
+-- registry/    GET <registry>/packages/<name> pre-check for boot packages
+-- tray/        StatusNotifierItem (zbus) + DBusMenu (XML menu model)
+-- settings/    axum on 127.0.0.1: single-page UI + /api JSON
+-- daemon/      run/daemonize lifecycle, pidfile, single-instance lock
```

State layout (`~/.ph/reactor/`):

```
config.json          daemon configuration (drives, registry, pins, ports)
node/                private Node runtime (only when bootstrapped)
switchboard/         npm install tree; powerhouse.config.json + .ph-reactor/entry.mjs live here
data/                PGlite store for the local reactor (switchboard DB)
logs/                reactor.log, switchboard.log (rotating, keep 3)
run/                 pid + lock
```

The supervisor never runs the package's own entry (`dist/index.mjs`);
it spawns a **generated boot wrapper** (`<state>/switchboard/.ph-reactor/entry.mjs`),
rewritten atomically whenever the daemon configuration changes. The
wrapper imports the installed `@powerhousedao/switchboard/server`
(public API, same version as the installed package) and calls
`startSwitchboard` with the daemon's choices:

- `port`, `database` (the built-in PGlite defaults), `mcp: true`,
- `channelScheme: "connect"` — active request channels for remote
  drives (see goal 5),
- `registryUrl` + `packages` — boot-time document model package loading
  from the package registry (goal 6),
- the switchboard's own default `drive` (the local `powerhouse` drive),
- a `jwtHandler` that maps a request URL to a per-drive bearer token
  read from an environment variable. Both the `channelScheme` and the
  `jwtHandler` options only take effect because of the post-install
  patch below; without it the published boot path drops them.

The child is spawned with `cwd = <state>/switchboard` and env:

| Env | Value |
|---|---|
| `PORT`, `PH_SWITCHBOARD_PORT` | config `switchboard.port` (default 4001) |
| `LOG_LEVEL` | config `logLevel` |
| `NODE_ENV` | `production` |
| `PH_DRIVE_TOKEN_<i>` | for each non-paused drive with a `tokenEnv`, the *value* of that variable, resolved at spawn time (index `i` = position in the drive list) |

The wrapper's token map and the `PH_DRIVE_TOKEN_<i>` names use the same
indexing, so a changed `tokenEnv` is part of the process fingerprint
(respawn); the token values are not (re-read on every spawn).

Everything else goes into the generated `powerhouse.config.json`
(`@powerhousedao/config` shallow-merges it over the switchboard's
built-in defaults, so only the keys the daemon controls are written):

```json
{
  "logLevel": "info",
  "switchboard": { "port": 4001, "database": { "url": "dev.db" } },
  "packageRegistryUrl": "https://registry.dev.vetra.io",
  "packages": [{ "packageName": "@powerhousedao/knowledge-note" }]
}
```

Remote drives are deliberately **not** part of this file: the installed
entry does not read a `remoteDrives` key, and the daemon owns
registration (MCP on the running switchboard), which is also what keeps
paused/removed drives from silently re-syncing after a respawn.

The local store is the switchboard's built-in PGlite directory under its
working directory (`<state>/switchboard/.ph/reactor-storage`), persisted
across restarts; local auth stays disabled by the built-in default. The
registry URL and boot packages are what make missing document model
packages install automatically from `https://registry.dev.vetra.io` —
the same `HttpPackageLoader` path Connect uses (boot packages via the
generated wrapper, on-demand models via dynamic model loading).

## Daemon configuration

`~/.ph/reactor/config.json` (schema version 1; unknown fields preserved,
missing fields filled with defaults; corrupt file backed up and re-created):

```json
{
  "version": 1,
  "switchboard": {
    "port": 4001,
    "packageSpec": "@powerhousedao/switchboard@latest",
    "npmRegistry": "https://registry.npmjs.org",
    "node": { "minimumVersion": "24", "preferSystem": true }
  },
  "registry": "https://registry.dev.vetra.io",
  "packages": ["@powerhousedao/knowledge-note"],
  "drives": [
    {
      "name": "Powerhouse Knowledge",
      "url": "https://light-colt-c497cfbd-switchboard.vetra.io/d/powerhouse-knowledge",
      "tokenEnv": "PH_REACTOR_DRIVE_TOKEN",
      "availableOffline": true,
      "paused": false
    }
  ],
  "settings": { "host": "127.0.0.1", "port": 4002 },
  "logLevel": "info"
}
```

- `tokenEnv` names an environment variable holding a Renown bearer token for
  that drive's switchboard (sent as `Authorization: Bearer …` on the drive
  info fetch and as the ambient token for the sync session). Unset variable
  = anonymous, which is how the public `powerhouse-knowledge` vault works
  (verified: `GET /d/powerhouse-knowledge` returns 200 anonymously).
- `paused` is daemon-side: a paused drive's remote is not re-activated after
  a switchboard restart, and the daemon stops status polling for it. The
  local switchboard's persisted `sync_remotes` row keeps its own state;
  pause/resume is v1 a *polling* gate (see *State model*).

## Drive sync flow

Adding a drive (from tray, settings page, or CLI):

1. **Resolve**: `GET <drive url>` (with the drive's Bearer token when
   configured) → `DriveInfo { id, slug, name, icon?, meta,
   graphqlEndpoint }` (the shape returned by the switchboard's drive-info
   endpoint; the client mirrors `reactor-browser/src/actions/drive-info.ts`,
   which sends the token whenever one is available). Non-2xx → error state
   for the drive (401/403 reported as an auth problem, other statuses as
   connection problems).
2. **Pre-check registry** (only if `packages` is non-empty): each boot
   package resolves via `GET <registry>/packages/<name>`; a 404 is a drive-
   level warning (the model may still load dynamically later).
3. **Register**: MCP `tools/call addRemoteDrive { url, options:
   { availableOffline } }` against `http://127.0.0.1:<port>/mcp`. The
   switchboard's handler fetches the drive info itself, checks
   `syncManager.list()` for an existing remote with the same
   `DriveCollectionId.forDrive(id)` and skips when present (idempotent
   across daemon restarts; the local reactor persists the drive document
   + sync cursors in its own store). With the `"connect"` channel scheme
   the add also *opens the request channel*: the remote must accept the
   channel's initial `touch`, otherwise the registration is rolled back
   and the daemon records the outcome (see the state model).
4. **Materialize**: poll the local reactor (MCP `getDrive` for the
   resolved id) until the drive document is queryable locally (the
   backfill delivers it). Bounded wait, then success anyway — the sync
   keeps running in the switchboard; a still-missing document surfaces
   as `connecting` in the status.

Removing a drive: MCP `deleteDrive { driveId }` (cascade-deletes the local
drive document and its contents — `packages/reactor-mcp/src/tools/reactor.ts`)
+ drop the entry from `config.json`. The persisted remote row on the local
reactor is a follow-up cleanup (it goes idle without its drive; harmless in
v1, noted in the docs).

Pause/resume/resync (the local sync manager has no pause primitive —
removal is the clean, complete stop):
- **pause**: delete the drive from the local reactor (MCP
  `deleteDrive`) and set `paused` in config. The sync channel tears
  down and the local mirror is gone (nothing stale is served); the
  remote drive is untouched.
- **resume**: clear the flag and re-run the add flow (idempotent):
  the channel re-establishes and the mirror is re-backfilled.
- **resync**: delete + re-add, forcing a full re-backfill from the
  remote.

```rust
enum DriveStatus {
  Synced,           // local mirror exists; remote reachable
  Connecting,       // registered (or registration attempted); local document not there yet
  Paused,           // paused in config (mirror deleted)
  Offline,          // drive is local but the remote is unreachable
  RequiresAuth,     // the remote rejected the sync registration (401/403):
                    // it needs a credential — add the drive with a tokenEnv
                    // whose variable holds a token the remote accepts
  Error,            // other failure (MCP failure, bad URL, ...)
}
```
- A status poller (every 5 s) resolves each drive against the local
  switchboard: MCP `getDrives`/`getDrive` for local document presence,
  plus a Bearer-aware drive-info GET for remote reachability (only for
  drives that are local).
- For a configured drive that never materialized, the view reflects the
  daemon's last *registration attempt*: success → `connecting` (backfill
  in flight), permission rejection → `requires-auth`, other failure →
  `error`. The attempts are recorded in daemon memory (not persisted):
  a new daemon process starts with `connecting` until its boot re-add
  settles.
- The daemon does not claim per-envelope liveness (cursors and
  in-flight mailboxes live inside the local sync engine); `Synced` is
  the honest, observable claim: mirror present + remote reachable.
- A `StatusSnapshot` (daemon phase, switchboard health + port +
  version, drives, daemon version) is republished on every poll and
  after every command; the tray and the settings page both render from
  it (the tray tooltip carries a compact form).

## Tray

- Serves the SNI object at `/StatusNotifier/Item/PhReactor` (a D-Bus
  object path must be a valid name — no dashes) and, when a
  `org.kde.StatusNotifierWatcher` is present, registers with it
  (`RegisterStatusNotifierItem`); otherwise it owns the well-known
  fallback name `org.kde.StatusNotifierItem-<uid>-1`, which indicator
  implementations scan for. The menu is a sibling object
  (`/StatusNotifier/Item/PhReactor/Menu`) serving `org.kde.DBusMenu`
  from a small in-memory model (`AboutToShow`, `Event`, update
  signals).
- `IconName`: themed icon (`network-server`; the attention variant when
  the state is not healthy) — no bundled pixmaps, so the icon tracks
  the desktop's theme.
- `Status` property: `Active` (healthy) / `Attention` (switchboard
  down or any drive in `Offline`/`Error`) — the classic SNI values.
- `ActivationRequested` (left-click) → open the settings page
  (`$XDG_OPEN`, else `xdg-open`, at `http://127.0.0.1:<settings port>/`).
- Menu (rebuilt on each snapshot change):

```
Powerhouse Reactor v0.1.0          (disabled)
Switchboard: <state>               (disabled)
─────────────
<drive>  — <status>                (disabled, label)
    Pause sync / Resume sync
    Resync now
    Remove drive
<drive 2> …
─────────────
Add drive…                         (opens the settings page)
Open settings
Show logs                          (opens the daemon log)
Quit
```

Menu clicks send a `Command` into the daemon's single-writer command
loop (the same path the settings page and CLI use), so concurrent
actions cannot race.

- No session bus (headless/SSH): the daemon logs one warning and runs
  without a tray; the settings page and CLI remain fully functional.

## Settings page

Served by the daemon on `127.0.0.1:4002` (config `settings`;
loopback-only — no auth is needed because it never leaves the host,
same posture as the switchboard MCP gate on a loopback switchboard
with auth disabled):

- `GET /` — single self-contained HTML page (inline CSS/JS, no build
  step, no CDN, embedded in the binary): switchboard card (state,
  port, version), drives table (name, status chip, pause/resume,
  resync, remove actions), add-drive form (url, optional name,
  token-env, available-offline), registry/packages section (registry
  url, boot package list with per-package availability check).
- `GET /api/status` — `StatusSnapshot` JSON.
- `POST /api/drives` `{ url, name?, tokenEnv?, availableOffline? }` —
  add (runs the flow above).
- `POST /api/drives/<name>/pause|resume|resync` — state changes.
- `DELETE /api/drives/<name>` — remove.
- `POST /api/config` `{ key, value }` — dotted config key (same
  validation as `ph-reactor config set`; process-relevant keys
  respawn the switchboard).
- `POST /api/quit` — stop the daemon.

## Bootstrap details

**Node.**
- Probe the system's node: `node --version` (5 s timeout) against `PATH`,
  `~/.local/bin/node`, `/usr/local/bin/node`, `/usr/bin/node`; if the
  version satisfies the configured minimum (24; semver-aware, prereleases
  sort below their release) and `preferSystem` → use it.
- Otherwise download
  `https://nodejs.org/dist/<ver>/node-<ver>-linux-<arch>.tar.gz` (pinned
  `NODE_DIST_VERSION = "v24.11.1"`; arch from the binary: x64 / arm64),
  verify the sha256 against the corresponding line in
  `https://nodejs.org/dist/<ver>/SHASUMS256.txt`, and extract the
  tarball's single top-level directory (flattened) to
  `<state>/node/node-<ver>-linux-<arch>/`; use `<dir>/bin/node`.
- A marker file records the installed version; every start re-verifies
  (`node --version`), so a corrupt or partial runtime self-heals.
  Downloads retry with backoff (3 attempts, 5 min timeout each).

**Switchboard.**
- `npm install <packageSpec> --registry <npmRegistry> --no-audit
  --no-fund` in `<state>/switchboard` (a stub `package.json` makes the
  directory the install root), using the `npm` that ships next to the
  resolved `node` — no separate npm bootstrap. 10 min timeout; npm's
  own integrity checks verify the package.
- `.ph-reactor-meta.json` (requested spec, resolved version read from
  the installed `package.json`, install time) is the idempotency marker:
  re-runs reinstall only when the requested spec differs or the entry
  point is missing.
- `powerhouse.config.json` is rewritten (atomically) from the daemon
  config on every start.
- The registry URL and boot packages in that config are what make
  missing document model packages install automatically from
  `https://registry.dev.vetra.io` at boot and on demand — the same
  `HttpPackageLoader` path Connect uses.
- **Sync-client patch.** After the install (and re-checked on every
  start, since the step is idempotent) the daemon patches the
  installed `dist/server-*.mjs`: it inserts
  `channelScheme: options.channelScheme` into the
  `applySwitchboardReactorDefaults(...)` call and adds
  `reactorBuilder.withJwtHandler(options.jwtHandler)` after it — the
  two wiring gaps in the published build. The pristine chunk is backed
  up under `<state>/switchboard/.ph-reactor/dist-patch/` before the
  first patch; the patched file is re-read and verified after writing
  (the backup is restored on mismatch or write failure); on a build
  where the markers are already present (natively wired, or previously
  patched) the step is a no-op, and on an unrecognizable layout it is
  skipped with a warning (the switchboard then runs passive).
**Supervisor.**
- Spawn `node <state>/switchboard/.ph-reactor/entry.mjs` (the generated
  boot wrapper — see above); the wrapper resolves the installed
  `@powerhousedao/switchboard/server` from its own
  `node_modules`.
  Child exits never clear the local sync state: the persisted cursors
  are the resume point, so a restart resumes where the sync left off
  (a full re-backfill is the explicit `drive resync`).
- Health: `GET http://127.0.0.1:<port>/health` every 5 s (the
  reactor-api health route); 3 consecutive failures (or process exit)
  → SIGTERM (5 s grace → SIGKILL) → restart.
- Restart backoff 1 s → 2 → 4 → … → 300 s cap, reset after 5 min
  healthy; 5 consecutive failed boots (immediate exit or never healthy)
  → `Error` state (no further retries; `ph-reactor doctor` explains).
- Config hot-restart: the daemon watches its own `config.json`;
  changes to process-relevant keys (port, package spec, npm registry,
  node settings, registry, packages, log level) rewrite
  `powerhouse.config.json` and respawn the switchboard with no backoff.
  Drive changes are applied to the running switchboard via MCP and do
  not restart it.
- Logs: the daemon's tracing and the switchboard child's output
  (marked `[o]`/`[e]`) share `<state>/logs/reactor.log`, rotated at
  10 MB, 3 generations; the daemon also mirrors to stderr when a
  terminal is attached. `ph-reactor logs --switchboard` filters the
  child's lines.
- Shutdown: SIGTERM/SIGINT (or `stop` / `quit`) → the tray unregisters,
  the switchboard child is SIGTERM'd (10 s grace → SIGKILL), and the
  pidfile/lock/ready files are removed; no orphans.

**Daemon lifecycle.**
- `run --daemonize` forks *before* any tokio runtime exists — forking
  inside a tokio process shares the runtime's epoll and corrupts both
  sides — and the child builds a fresh one. The parent waits for the
  child's ready file (written once supervisor, tray, and settings are
  up) and exits 0.
- Single instance: `flock` on `<state>/run/lock` plus a pidfile
  (`<state>/run/daemon.pid`); a second `run` refuses with the running
  pid. `stop` reads the pidfile and SIGTERMs the daemon.
- `status` queries the running daemon's settings API; when the daemon
  is down it reports that, with the configured drives from the config
  file (degraded view).

## Packaging

**Snap** (`snap/snapcraft.yaml`, `base: core24`,
`confinement: strict`):
- ships the statically linked (musl) `ph-reactor` binary (CI builds it
  into `dist/`; the part is a plain install), session autostart runs
  `run --daemonize`.
- state under `$SNAP_USER_DATA/ph-reactor` (the daemon honors
  `PH_REACTOR_STATE_DIR`, set in both the app and the autostart
  `.desktop` entry).
- plugs: `home`, `network`, `network-bind`, `dbus` (session bus for the
  SNI tray).
- build: `snapcraft --use-lxd` on an Ubuntu host (see the README);
  publish to the snap store from the release assets.

**Homebrew** (follow-up): the formula would install the release binary
(the workflow already produces it); not part of v1 — the binary install
path in the README covers it meanwhile.

**Release pipeline** (`.github/workflows/rust-reactor.yml`):
- on tag `ph-reactor-v*`: build stable Rust with
  `x86_64-unknown-linux-musl`, upload the binary + `sha256` sidecar to
  the GitHub release. aarch64 and the snap store push are manual
  follow-ups.

## CLI

```
ph-reactor                     # = ph-reactor run (foreground daemon)
ph-reactor run [--daemonize]   # start the daemon; --daemonize forks to background (pidfile)
ph-reactor stop                # SIGTERM the running daemon (pidfile)
ph-reactor status              # human-readable status (drives, switchboard, versions)
ph-reactor drive add <url> [--name N] [--token-env E] [--offline]
ph-reactor drive remove <name-or-index>
ph-reactor drive list
ph-reactor drive pause|resume|resync <name-or-index>
ph-reactor doctor              # node/npm/registry/switchboard/MCP diagnostics
ph-reactor config show
ph-reactor config set <dotted.key> <value>
ph-reactor logs [--follow] [--switchboard]
ph-reactor --version
```

`drive` subcommands talk to a running daemon over the settings HTTP API
(loopback), so they work identically for snap/brew installs and for a
foreground run; `doctor` runs standalone (no daemon required).

## Security posture

- The daemon binds only `127.0.0.1` (switchboard port + settings port).
- Local switchboard auth is disabled by default (`auth.enabled=false`); the
  MCP endpoint is loopback-only as a result. Enabling auth (Renown) is a
  config escape hatch for operators who want it.
- Drive tokens are never stored in `config.json` — only the name of an env
  variable holding them.
- The settings page is loopback-only and serves no user data beyond the
  daemon's own status; the switchboard's own data is never proxied by the
  settings server.
- Node and switchboard binaries are hash-verified (node: SHASUMS256.txt;
  switchboard: npm's own integrity checks).

## Edge cases

- **No session bus / headless box**: the tray is not started (one log
  warning); the daemon continues; CLI/settings work.
- **Port collision** (4001 taken): the switchboard falls forward to the
  next free port (its own fallback) and logs which; the daemon polls
  the configured port, so a shifted port surfaces as an unhealthy
  switchboard in `status` (follow-up: parse the actual port from the
  switchboard's startup log).
- **System node present but < 24**: private runtime is used; the system
  node is never modified.
- **npm registry unreachable** during bootstrap: retry with backoff; the
  daemon stays in `Bootstrapping` with the reason visible in settings; no
  half-installed tree is marked complete (marker file only on success).
- **Registry unreachable at runtime**: dynamic model loading fails per
  model (the reactor logs and the document stays unrenderable for models
  that need it); the daemon's boot pre-check surfaces this in the tray.
- **Remote drive requires auth** (401/403 on the drive-info fetch, or a
  permission rejection of the sync registration — e.g. a vetra-hosted
  switchboard whose auth projection gates the channel's `touch`): drive
  status `requires-auth`, with the rejection detail; the fix is to add
  the drive with `--token-env NAME` (the token value comes from the
  environment, never from the config file) — the daemon's post-install
  patch makes the published build present that token on the sync
  channels, so an authenticated remote syncs once the token is set.
- **Slug collision with the local drive**: the local default drive and
  the remote drive share a slug (both `powerhouse`). Both reactors
  independently create the same document id; when the channels
  exchange the two `CREATE_DOCUMENT`s each side dead-letters the
  other's create (a revision-0 create against a non-zero stream) and
  that drive cannot converge. Known limitation (an upstream conflict-
  resolution question, not fixable in the daemon); the typical case is
  unaffected — a vault drive's slug (e.g. `powerhouse-knowledge`)
  differs from the local default. Recovery for a colliding pair: stop
  the daemon and wipe `<state>/switchboard/.ph` (the local store),
  then start again with only the non-colliding drives configured.
- **Daemon restart**: bootstrap is idempotent (marker files); the
  switchboard restarts with the same PGlite dir (data survives);
  registered drives persist in the store and their channels resume from
  the persisted cursors; the boot re-add is an idempotent safety net for
  registrations that never completed.
- **Two daemons**: single-instance `flock` on `<state>/run/lock` (plus
  a pidfile); a second `run` refuses with a clear message.
- **Config migration**: unknown fields are preserved verbatim; `version`
  bumps are handled by a small migration function chain.

## Files

| Path | Change |
|---|---|
| `Cargo.toml` | new — crate manifest (tokio, zbus, reqwest/rustls, clap, serde, axum, tracing, semver, sha2, flate2, tar, dirs, thiserror, anyhow, futures, parking_lot, libc, url, rand) |
| `src/main.rs` | new — CLI entry (clap) dispatching to the daemon |
| `src/lib.rs` | new — module root, `App` type |
| `src/paths.rs` | new — state dir resolution (`PH_REACTOR_STATE_DIR` > `$HOME/.ph/reactor`) |
| `src/config.rs` | new — config types, defaults, load/save (atomic, corrupt-file recovery), dotted-key `set` |
| `src/bootstrap/node.rs` | new — node probe/download/extract/sha256-verify |
| `src/bootstrap/switchboard.rs` | new — npm install, meta marker, sync-client patch of the installed server chunk, `powerhouse.config.json` generation, boot wrapper, spawn env, health probe |
| `src/supervisor.rs` | new — process spawn, health poll, backoff, config hot-restart, stop |
| `src/logrotate.rs` | new — size-rotated log sink (10 MB × 3) |
| `src/mcp.rs` | new — Streamable-HTTP MCP client (initialize, tools/call, session) |
| `src/drives.rs` | new — drive info fetch, add/remove/pause/resume/resync, 5-state status model, poller |
| `src/registry.rs` | new — boot-package availability pre-check |
| `src/status.rs` | new — `StatusSnapshot` types (JSON for API/tray) |
| `src/commands.rs` | new — the daemon's command vocabulary (shared by tray, page, CLI) |
| `src/tray/mod.rs` | new — SNI + DBusMenu over zbus (session bus, headless-safe) |
| `src/tray/menu.rs` | new — menu model → DBusMenu XML |
| `src/settings/mod.rs` | new — axum server: JSON API + embedded single-page UI (loopback) |
| `src/daemon.rs` | new — run/daemonize/stop/status/doctor/logs, pidfile, flock, signals, command loop |
| `README.md` | new — install, usage, state layout, snap build, troubleshooting |
| `snap/snapcraft.yaml` | new — strict snap (core24, static binary, autostart, SNI plugs) |
| `.github/workflows/rust-reactor.yml` | new — release build on `ph-reactor-v*` tags (musl x86_64, GitHub release assets) |
| `docs/superpowers/plans/2026-09-11-rust-reactor.md` | new — implementation plan |
| `docs/superpowers/sdd/2026-09-11-rust-reactor/progress.md` | new — SDD progress |
| `docs/superpowers/evidence/rust-reactor/` | new — verification evidence |

No changes to TypeScript packages in this design; the daemon consumes the
existing switchboard surface (MCP, GraphQL, health, drive-info) — with one
documented exception: the post-install sync-client patch applied to the
installed server chunk (a local, reversible transform, not a source change).
