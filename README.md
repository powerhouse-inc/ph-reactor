# ph-reactor

A small Rust daemon that runs a local Powerhouse **switchboard** in the
background on Linux, with a status-bar **tray icon** and a loopback
**settings page** for configuring the remote drives to keep in sync
(e.g. the `powerhouse-knowledge` vault drive).

It is the "reactor" of the Powerhouse stack in the desktop sense: the
Node-based switchboard inside is the document engine and the sync engine
(the same GqlChannel sync the browser Connect uses). The Rust daemon is
the operator surface — bootstrap, supervision, tray, drive configuration:

- **Bootstrap**: finds a system Node ≥ 24 or downloads a private,
  sha256-verified Node runtime into the state directory; installs
  `@powerhousedao/switchboard` (pinned npm spec) once; generates the
  switchboard's `powerhouse.config.json` (port, registry, boot packages,
  auth off).
- **Supervision**: spawns the switchboard, polls its `/health` endpoint,
  restarts with exponential backoff on crash or unhealthiness, rotates
  logs, stops cleanly (no orphan processes).
- **Tray**: a `StatusNotifierItem` over D-Bus (zbus) with a `DBusMenu` —
  no GTK dependency; headless-safe (runs without a session bus, tray is
  optional).
- **Drive sync**: adds/removes/pauses/resumes remote drives by driving
  the local switchboard's own sync manager through its built-in MCP
  endpoint (`addRemoteDrive` / `deleteDrive`).
- **Missing packages**: document model packages are auto-installed from
  `https://registry.dev.vetra.io` — the same `HttpPackageLoader` path
  Connect uses (boot packages via the generated config, on-demand models
  via dynamic model loading).
- **Settings**: a single-page UI + JSON API on `127.0.0.1:4002`
  (drives, switchboard, registry/packages, quit).

## Install

**Snap** (primary): see [Build the snap](#build-the-snap) below — the
store channel is published from the GitHub release assets.

**Binary** (brew-style, manual):

```sh
# from a release tag, or from source:
cargo build --release --target x86_64-unknown-linux-musl
install -Dm755 target/x86_64-unknown-linux-musl/release/ph-reactor ~/.local/bin/ph-reactor
```

The binary is statically linked (musl); no runtime dependencies. A Homebrew
tap is a follow-up (the formula would install the release tarball).

## Quick start

```sh
ph-reactor run --daemonize        # start in the background (tray + settings)
ph-reactor status                 # daemon, switchboard, drives
ph-reactor drive add https://light-colt-c497cfbd-switchboard.vetra.io/d/powerhouse-knowledge \
    --name "Powerhouse Knowledge"
ph-reactor drive list
ph-reactor stop                   # clean shutdown
```

`ph-reactor` with no subcommand runs the daemon in the foreground.

On first start the daemon bootstraps Node + the switchboard package into
`~/.ph/reactor/` (a few minutes; watch `ph-reactor logs`).

## CLI

```
ph-reactor [state-dir options] <command>

  run [--daemonize]   start the daemon (default command)
  stop                stop the running daemon (SIGTERM)
  status              show daemon, switchboard, and drive state
  drive add <url> [--name N] [--token-env E] [--offline]
  drive remove <name-or-index>
  drive list
  drive pause <name-or-index>     stop syncing; local mirror deleted
  drive resume <name-or-index>    re-add and re-sync the drive
  drive resync <name-or-index>    delete + re-add (forced re-sync)
  doctor                diagnostics (node, npm, registry, switchboard, MCP)
  config show | config set <dotted.key> <value>
  logs [--follow] [--switchboard]

Global: --state-dir <dir> (default $PH_REACTOR_STATE_DIR or ~/.ph/reactor)
```

`drive` subcommands talk to the running daemon over its loopback settings
API; `config` and `doctor` work standalone.

## Tray

The tray icon is a classic `org.kde.StatusNotifierItem` (D-Bus session bus)
served by the daemon itself — it shows in KDE out of the box and in GNOME
with any SNI indicator extension. Icon: themed `network-server`
(attention variant when something needs care). Menu:

```
Powerhouse Reactor v<ver>          (disabled)
Switchboard: <state>               (disabled)
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

## Settings page

`http://127.0.0.1:4002/` (loopback only, no auth by design): switchboard
card, drives table with pause/resume/resync/remove, add-drive form,
registry/packages section. JSON API: `GET /api/status`, `POST
/api/drives`, `POST /api/drives/<name>/pause|resume|resync`, `DELETE
/api/drives/<name>`, `POST /api/config` (key/value), `POST /api/quit`.

## Configuration

`~/.ph/reactor/config.json` (override the location with
`PH_REACTOR_STATE_DIR`):

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

- `tokenEnv` names an environment variable holding a bearer token for a
  private drive's switchboard (the value is never stored in the file).
  A drive reported as `requires-auth` is synced once it is (re)added
  with `--token-env NAME` and `NAME` holds a token the remote accepts —
  the daemon's post-install patch makes the installed switchboard
  present that token on the sync channels.
- `paused`: paused drives are removed from the local reactor (sync stops)
  but kept in config; resume re-adds them.
- Edit via `ph-reactor config set <key> <json-value>` or the settings
  page. Settings that change the running process (port, package spec,
  registry, packages, log level) make the daemon respawn the switchboard
  automatically; drive changes are applied to the live switchboard without
  a restart.

## State layout

```
~/.ph/reactor/
  config.json              daemon configuration
  node/                    private Node runtime (only when bootstrapped)
  switchboard/             npm install tree; powerhouse.config.json, .ph/ (PGlite store),
                           .ph-reactor/ (generated boot wrapper + pristine backup of the
                           patched switchboard server chunk)
  logs/                    reactor.log — daemon + switchboard output ([o]/[e]), rotating 10 MB × 3
  run/                     daemon.pid, lock (single instance), switchboard.pid, ready
```

## Build the snap

From the repository root, on an Ubuntu host with `snapcraft` (LXD):

```sh
cargo build --release --target x86_64-unknown-linux-musl
mkdir -p dist && cp target/x86_64-unknown-linux-musl/release/ph-reactor dist/
snapcraft --use-lxd        # produces ph-reactor_0.1.0_amd64.snap
```

The snap is `strict`-confined: state under `$SNAP_USER_DATA/ph-reactor`,
plugs `home`/`network`/`network-bind`/`dbus` (session bus for the tray),
and starts on session autostart (`run --daemonize`).

## Troubleshooting

- `ph-reactor doctor` — checks node availability, the npm registry, the
  package registry, the installed switchboard, and the MCP endpoint.
- `ph-reactor logs --follow` — daemon log; `--switchboard` — the
  switchboard's output lines within it (`[o]`/`[e]` markers).
- Port 4001 in use: the switchboard falls forward to the next free port
  (it logs which); the daemon follows it.
- A drive stuck at `connecting`: the first sync of a large vault takes
  time; `requires-auth` means the remote needs a token (add it with
  `--token-env`); `error` means the remote refused or is unreachable
  (check `offline` for air-gapped mirrors).
- The daemon applies a small, idempotent post-install patch to the
  installed switchboard's server chunk so that its boot path accepts
  the `channelScheme` and `jwtHandler` options the daemon passes
  (published builds drop them; without the patch a local switchboard
  can only serve as a sync target and never pull from a remote). The
  pristine chunk is backed up under
  `switchboard/.ph-reactor/dist-patch/`; the patch is a no-op on a
  build that wires those options natively.

## Security notes

- Both servers bind `127.0.0.1` only. Local switchboard auth is disabled
  by default (the MCP endpoint is therefore loopback-only); enable it in
  the generated config if you expose the port.
- Drive tokens live only in environment variables.
- The downloaded Node runtime is verified against `SHASUMS256.txt` from
  nodejs.org; the switchboard is installed by npm with its own integrity
  checks.

## Development

```sh
cargo test          # 42 tests, offline (mocked HTTP, temp state dirs, fake node)
cargo clippy --all-targets
cargo run -- run    # foreground, against the real state dir
```

Design: `docs/superpowers/specs/2026-09-11-rust-reactor-design.md`;
plan: `docs/superpowers/plans/2026-09-11-rust-reactor.md`.
