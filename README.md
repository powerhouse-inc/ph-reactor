# ph-reactor

A single-process Rust daemon that runs a local Powerhouse **reactor** in
the background on Linux: an event-sourced document vault with
**libp2p-based drive sync**, a status-bar **tray icon**, and a loopback **console** for configuring the remote drives to
keep in sync (e.g. the `powerhouse-knowledge` vault on another machine).

No Node, no npm, no child processes: the daemon *is* the reactor.

- **Store**: event-sourced docs (per-field last-writer-wins with vector
  clocks, ed25519-signed ops), durable as per-doc WAL + snapshots under
  `<state>/docs/`.
- **Sync**: libp2p (TCP + Noise + Yamux) with two behaviours — gossipsub
  for live op fan-out and a request/response protocol
  (`/ph-reactor/sync/1.0.0`) for the hello handshake, per-doc catch-up,
  and periodic summary reconciliation. A "drive" is a remote **peer**
  addressed by multiaddr, not a URL.
- **Tray**: an `org.kde.StatusNotifierItem` over D-Bus (zbus) with a
  `DBusMenu` — no GTK dependency; headless-safe (no session bus → the
  daemon runs without a tray).
- **Console**: a client-side-routed control panel (Overview / Documents / Types / Folders / Groups / Settings) with a JSON API — loopback by default, bindable to Tailscale or other hosts.

## Install

**Snap** (primary): see [Build the snap](#build-the-snap) below — the
store channel is published from the GitHub release assets.

**One-liner** (brew-style; builds from a source checkout and installs for the
current user — binary to `~/.local/bin`, tray `.desktop` + autostart entry):

```sh
curl -fsSL https://<host>/ph-reactor/install.sh | bash
# or from a local checkout:
PH_REACTOR_SRC=$PWD ./scripts/install.sh
```

**Manual**:

```sh
cargo build --release --locked
install -Dm755 target/release/ph-reactor ~/.local/bin/ph-reactor
```

```sh
ph-reactor run --daemonize      # start in the background (tray + settings)
ph-reactor status               # daemon, reactor, drives
ph-reactor drive add /ip4/10.0.0.2/tcp/4201/p2p/12D3Koo… --name "Vault"
ph-reactor drive list
ph-reactor doc add note --field "body=hello" --field "n=42"
ph-reactor doc list
ph-reactor stop                 # clean shutdown
```

`ph-reactor` with no subcommand (or bare `run`) runs the daemon in the
foreground; `run --daemonize` forks it into the background (pidfile in
the state dir, stdio detached, logs in the state dir).

On first start the daemon creates its identity key (one ed25519 keypair
for the whole instance) under `~/.ph/reactor/` and starts listening on
`/ip4/0.0.0.0/tcp/4201`. Peers on the LAN can also be found via mDNS
(`p2p.mdns`); everything else is explicit multiaddr.

### Syncing a knowledge vault

The native reactor is self-contained (no Node, no switchboard process). A
"knowledge vault" is therefore a **group of reactors** that share a set of
drives, not a switchboard URL. To join one (e.g. the `powerhouse-knowledge`
vault):

```sh
# on the vault host, once:
ph-reactor invite                 # prints a one-shot invite string

# on your machine:
ph-reactor join <invite-string>   # pins the inviter (TOFU) + adds the shared drives
```

For a single peer instead of a group, `ph-reactor drive add <multiaddr>`
works too. The vault's document model packages are fetched from the registry
on first sync and installed into the local store automatically.

## CLI

```
ph-reactor [--state-dir <dir>] <command>

  run [--daemonize]        start the daemon (foreground; --daemonize forks to background)
  stop                     stop the running daemon (SIGTERM)
  status [--json]          show daemon, reactor, and drive state
  drive add <multiaddr> [--name N] [--token-env E] [--offline]
  drive remove <name-or-index>
  drive list
  drive pause <name-or-index>     stop syncing; docs stay local
  drive resume <name-or-index>    re-dial and re-sync
  drive resync <name-or-index>    force a fresh catch-up
  doc list                             local docs
  doc get <name>                       a doc's fields as JSON
  doc add <name> [--field K=V …]      create a local doc (daemon must be running)
  query <model> [--filter K=V]        query the read models (omit model = all; V is JSON)
  doctor                     diagnostics (state dir, identity, store, listen, settings, session bus)
  config show | config set <dotted.key> <value>
  logs [--follow]

Global: --state-dir <dir> (default $PH_REACTOR_STATE_DIR or ~/.ph/reactor)
```

`drive` and `doc add` talk to the running daemon over its loopback
settings API (the daemon is the single writer); `doc list`/`doc get`,
`config`, and `doctor` work standalone.

### Stable CLI contract (1.0.0)

For shell integrations (see the companion Omarchy 4 plugin
`powerhouse-inc/ph-reactor-omarchy`), the following is stable:

- `ph-reactor --version` prints `ph-reactor <semver>`.
- `status --json` exits 0 whenever the CLI runs and prints a
  `StatusSnapshot`: live from the daemon's `/api/status` when the daemon is
  up, or a degraded shape built from `config.json` when it is not
  (`reactor.running` is `false` and `last_event` says why):

```json
{
  "version": "1.0.0",
  "reactor": { "running": true, "healthy": true,
               "peer_id": "12D3Koo…", "listen": "/ip4/0.0.0.0/tcp/4201",
               "docs": 12, "last_event": "…" },
  "drives": [ { "name": "…", "addr": "/ip4/…/tcp/4201/p2p/12D3Koo…",
                "paused": false,
                "status": "synced|connecting|paused|offline|requires-auth|error",
                "detail": "…" } ],
  "settings": { "url": "http://127.0.0.1:4002" },
  "updated_at": "2026-09-12T00:00:00Z"
}
```

- `run` (daemonized), `stop`, and the `drive`/`doc` subcommands are the
  other commands integrations use. Drive tokens are referenced by
  environment variable *name* only (`--token-env NAME`); the value is
  resolved by the daemon and never passed or stored by callers.

## Sync model

Every doc mutation is an **op**: `{ doc_id, field?, value?, ts, clock,
origin, signature }` where `clock` is a per-document vector clock and
`origin` signs it with the instance's identity key. Ops apply in any
order: unknown work is accepted (clock not covered), same-field
conflicts resolve by last-writer-wins on `(ts, origin)`, and unverified
signatures are quarantined, never applied.

Three paths move ops between peers:

1. **Gossip** (gossipsub topic `ph-reactor/docs/1.0.0`): the tick loop
   publishes each newly applied local op; peers apply and the message
   fans out.
2. **Catch-up** (request/response): on a gap (a peer's clock covers
   work we lack), the requester asks for the exact ops it is missing;
   responses are capped and resumable.
3. **Summary reconciliation**: every 30 s an authenticated drive
   exchanges per-doc clocks; anything missed by gossip (loss, a
   reconnection, a doc created before the link existed) is closed by
   catch-up.

`doc add` publishes through the mesh as the doc is created, so a new
vault mirror picks it up on the next reconcile without waiting for a
reconnect.

## Tray

The tray icon is a classic `org.kde.StatusNotifierItem` (D-Bus session
bus) served by the daemon itself — it shows in KDE out of the box and in
GNOME with any SNI indicator extension. Icon: themed `network-server`
(attention variant when something needs care). Menu:

```
Powerhouse Reactor v<ver>          (disabled)
Reactor: <state>                   (disabled)
─────────────
<drive>  — <status>                (disabled)
    Pause sync / Resume sync
    Resync now
    Remove drive
─────────────
Add drive…                         (opens the settings page)
Open settings
Show logs                          ($XDG_OPEN / xdg-open)
Quit
```

Left-click (activation) opens the settings page. If no session bus is
available (headless/SSH) the daemon logs one warning and runs without a
tray; everything else works.

## Console

`http://127.0.0.1:4002/` (loopback by default — set `settings.host` to
`0.0.0.0` or your Tailscale IP to reach it over the network; there is no
auth, so the bind address is the security boundary) serves the **reactor
console** — a client-side-routed control panel (hash routing, no framework,
no build step; the HTML/JS/CSS is embedded in the binary). The original
console is still available at `/console`. The v2 views:

- **Overview** — reactor health, peer and document counts, a live
  **activity feed** of processor fires, and a one-click **reference
  processor** (an `invoice` whose `status → accepted` runs a command).
- **Documents** — browse every document (filter by model and
  `field=value`), open one to read and edit its fields (a plain field `set`
  or a model action), and create a new document under a model.
- **Types** — the registered document models. **Draft a new one from a
  text description via the LLM**, review the generated JSON definition,
  register it as an interpreter, and create documents under it.
- **Folders** — named membership containers over peers (create, add
  members).
- **Groups** — signed membership documents enforcing the **two-person
  quorum** (a change needs two distinct members; one node cannot satisfy it
  by itself). Create groups, manage members and managers, and read each
  group's signed activity; quorum failures surface as friendly errors.
- **Processors** — user-configurable **subscriptions on document changes**:
  a `ProcessorSpec` picks the models and an optional `field=value`, and a
  reaction (`run` / `log` / `emit` / `create-doc`). List, edit, remove, and
  watch each processor's **fire history**.
- **Settings** — the full config grouped by concern, including the LLM
  (OpenAI-compatible) endpoint with a **Test connection** button, and a
  pause/resume toggle for the whole sync engine.

A theme switch (system / light / dark) persists to `localStorage`. The
console talks to the JSON API on the same server. The routes:

```
  GET     /api/status                      reactor, drives, settings snapshot
  GET     /api/overview                    peer count, live document count
  GET     /api/config                      the config document
  POST    /api/config                      set / replace / pause / resume
  GET     /api/docs[?model=&field=&value=] list documents
  GET     /api/docs/<name>                 one document's fields
  POST    /api/docs                        create a document
  POST    /api/docs/action                 a field set or a model action
  GET     /api/query[?model=&filter=]      the read-model query API
  GET     /api/models                      registered document models
  POST    /api/models|/api/models/register register a model definition
  POST    /api/llm/draft-type              LLM: text -> model definition
  POST    /api/llm/test                    LLM: connectivity check
  GET     /api/folders                     list folders
  POST    /api/folders                     create a folder
  POST    /api/folders/<name>/action       add / remove a folder member
  GET     /api/groups                      list groups
  POST    /api/groups                      create a group
  POST    /api/groups/<name>/action        a signed group action (quorum-checked)
  GET     /api/groups/<name>/activity      a group's signed actions
  GET     /api/processors                  list processors
  POST    /api/processors                  create a processor
  PUT     /api/processors/<name>           update a processor
  DELETE  /api/processors/<name>           remove a processor
  GET     /api/processors/<name>/fires     a processor's fire history
  POST    /api/drives                      add a drive
  DELETE  /api/drives/<name>               remove a drive
  POST    /api/drives/<name>/pause        pause a drive's sync
  POST    /api/drives/<name>/resume       resume a drive's sync
  POST    /api/drives/<name>/resync       force a fresh catch-up
  POST    /api/invite | /api/join         invite / join a vault
  POST    /api/ban  | /api/unban          ban / unban a peer
  POST    /api/quit                        shut the daemon down
```

## Configuration

`~/.ph/reactor/config.json` (override the location with
`PH_REACTOR_STATE_DIR`):

```json
{
  "schemaVersion": 2,
  "instance": { "name": "reactor", "listen": "/ip4/0.0.0.0/tcp/4201" },
  "p2p": { "mdns": true, "tokenEnv": null },
  "drives": [
    {
      "name": "Vault",
      "addr": "/ip4/10.0.0.2/tcp/4201/p2p/12D3Koo…",
      "tokenEnv": "PH_REACTOR_DRIVE_TOKEN",
      "availableOffline": true,
      "paused": false
    }
  ],
  "settings": { "host": "127.0.0.1", "port": 4002 },
  "logLevel": "info"
}
```

- `instance.listen` — the p2p listen multiaddr (the remote side of a
  `drive add` is the *peer's* listen address, optionally with its
  `/p2p/<peer-id>`).
- `p2p.mdns` — advertise/discover peers on the local segment.
- `p2p.tokenEnv` — env var name of a global token gate: inbound hellos
  from peers without a drive entry are rejected unless their token
  matches.
- `drives[].tokenEnv` — per-drive shared token (env var *name* only; the
  value is never stored in the file). A drive reported as
  `requires-auth` starts syncing once the token matches on both sides.
- `paused`: paused drives stop syncing but keep their local mirror;
  resume re-dials and re-catches-up.
- `settings.host` / `settings.port` — the console bind address and port
  (default `127.0.0.1:4002`). Set `host` to `0.0.0.0` or your Tailscale IP
  to open the console to the network — there is no auth, so treat the bind
  address as the boundary.
- Edit via `ph-reactor config set <key> <json-value>` or the settings
  page. The daemon adopts config changes on its next poll; process-level
  settings (listen port) apply on restart.

## State layout

```
~/.ph/reactor/
  config.json              configuration (0600, schemaVersion 2)
  key                      ed25519 identity (0600) — the instance's whole identity
  docs/                    <doc-id>.log (WAL), <doc-id>.snap, index.json
  logs/                    reactor.log — size-rotated
  run/                     daemon.pid, lock (single instance), ready
```

Upgrading from 0.x: the 0.x directories (`node/`, `switchboard/`,
`data/`) are left in place, untouched and unused; delete them once you
are happy with the new instance.

## Build the snap

The package definition is [`snap/snapcraft.yaml`](snap/snapcraft.yaml).
It is a **classic** snap (it needs the session D-Bus for the tray icon and
`~/.ph/reactor` for its state). From an Ubuntu host with `snapcraft` and a
Rust toolchain:

```sh
snapcraft --use-lxd          # compiles with cargo build --release --locked
```

produces `ph-reactor_1.0.0_amd64.snap`. The `dump` part's `override-build`
runs `cargo build --release --locked` and installs the binary to
`$SNAP/bin/ph-reactor` plus the tray `.desktop` file to
`$SNAP/share/applications/`. The app runs `ph-reactor run` on session
autostart (the `.desktop` file carries `X-GNOME-Autostart-Enabled=true`).

## Troubleshooting

- `ph-reactor doctor` — checks the state dir, the identity key, the doc
  store, the listen port, the settings server, and the session bus.
- `ph-reactor logs --follow` — daemon log.
- A drive stuck at `connecting`: check the peer is listening (its
  `ph-reactor status` shows `reactor.healthy` and its listen address),
  the multiaddr is reachable (firewall), and — if the drive requires a
  token — that `tokenEnv` names a var whose value both sides share
  (`requires-auth` is the resulting status).
- A drive flapping: every 30 s the peers exchange per-doc summaries; a
  one-way link (NAT/firewall that only allows one direction) still
  converges, just slower.
- `error` with a version mismatch: the two ends run incompatible
  protocol versions; upgrade both to the same release.

## Security notes

- Every p2p connection is Noise-encrypted; peers are identified by
  key-derived peer ids (no username/password model).
- Every op carries an ed25519 signature from its origin; the store
  applies an op only after verifying the signature against a key
  registered through a completed hello on an authenticated channel.
  Unverified ops go to quarantine and are never applied.
- Tokens (global gate and per-drive) live only in environment
  variables; config stores names, never values.
- The settings server binds `127.0.0.1` only. The p2p listener binds
  the configured address (all interfaces by default) — that is what
  makes remote sync possible; use the token gates for private drives.
- No downloads, no package installation, no child processes.

## Development

### Repo layout

```
ph-reactor/
  src/               the daemon — one binary plus a lib
    main.rs          CLI entry point
    cli.rs           the clap command tree
    daemon.rs        the daemon: swarm, tick loop, subsystems, settings server
    store.rs         the event-sourced store (ops, vector clocks, WAL/snapshots)
    config.rs        config load / validate / hot-reload
    processor.rs     processors: user subscriptions on doc changes + fire feed
    doc.rs           document model and op application
    action.rs        actions (field sets, model actions)
    p2p/             the libp2p swarm (mod.rs: behaviours; codec.rs; invite.rs)
    settings/        the axum settings server + the embedded consoles
      mod.rs         the HTTP/JSON API routes
      console.html   the original console (served at /console)
    model/           document models (l1, open, group, realistic seeds)
    tray/            the StatusNotifierItem tray (D-Bus) + its menu
    status.rs        the StatusSnapshot (the `status --json` contract)
    query.rs         the read-model query API
    paths.rs         the state-dir layout
    logrotate.rs     size-rotated logging
  console/v2.html    the v2 console (embedded; served at /)
  views/             a small lib crate: read-model / document-view / processor types
  tests/             integration tests (two-engine sync, DHT discovery, invite/join)
  docs/superpowers/  the spec -> plan -> SDD -> evidence trail (see below)
  snap/              the snapcraft package definition
  scripts/install.sh the one-liner installer
```

### Build, run, test

```sh
cargo build --release --locked     # the daemon (target/release/ph-reactor)
cargo run                          # foreground daemon against the real state dir
cargo test                         # unit + in-process two-engine sync (loopback), offline
cargo clippy --all-targets -- -D warnings
```

Run a throwaway instance against a scratch state dir (console on
`127.0.0.1:4002`): `PH_REACTOR_STATE_DIR=/tmp/ph-dev cargo run`.

### Design and the superpowers workflow

Decisions are recorded under `docs/superpowers/` as a **spec -> plan ->
SDD (per-task brief/report) -> evidence** trail; new features follow the
same shape. The daemon's design and plan:

- Design: `docs/superpowers/specs/2026-09-12-native-reactor-design.md`
- Plan:   `docs/superpowers/plans/2026-09-12-native-reactor.md`
- The console redesign: `docs/superpowers/specs/2026-09-13-console-v2-design.md`

### Contributing

Work on a `fix/…` or `feat/…` branch, never directly on `main`, and make
small, self-contained commits. Keep the console framework-free (the
embedded HTML/JS/CSS loads into the binary as-is) and keep the
`status --json` contract stable — it is what the companion Omarchy plugin
(`powerhouse-inc/ph-reactor-omarchy`) parses.
