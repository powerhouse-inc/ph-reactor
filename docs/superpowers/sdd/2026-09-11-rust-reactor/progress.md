# Rust Reactor (ph-reactor) — SDD Progress

Worked in the powerhouse monorepo: branch `feat/rust-reactor`, worktree
`~/.worktrees/powerhouse/rust-reactor` (crate at `apps/ph-reactor/`); moved
to this repository on 2026-09-12.
| Date: 2026-09-11 → 2026-09-12 (completed)

| Task | Status | Notes |
|---|---|---|
| 1 Crate scaffold, paths, config | done | config v1 (drives, registry, pins), atomic save, corrupt-file recovery, dotted-key set; 10 tests |
| 2 Node bootstrap | done | system probe (4 candidates, semver gate) or pinned v24.11.1 tarball, sha256-verified vs SHASUMS256.txt, self-healing marker |
| 3 Switchboard bootstrap | done | npm install into state dir (stub manifest), `.ph-reactor-meta.json` idempotency, `powerhouse.config.json` generation (registry + boot packages), spawn env, health probe |
| 4 Supervisor | done | spawn/health (5 s × 3)/backoff (1→300 s)/failed-boot cap, size-rotated logs, graceful stop; config hot-restart for process-relevant keys |
| 5 MCP client | done | Streamable-HTTP initialize/tools/call, session id, JSON-RPC error mapping; 6 tests |
| 6 Drives + live E2E | done | drive-info fetch (Bearer-aware), add/remove/pause/resume/resync via MCP, 5-state poller; live E2E against the powerhouse-knowledge vault drive |
| 7 Tray (SNI + DBusMenu) | done | SNI at /StatusNotifier/Item/PhReactor + watcher registration / well-known fallback, themed icon, DBusMenu XML; headless-safe |
| 8 Settings page + API | done | axum loopback server: status/drives/config/quit + embedded single-page UI |
| 9 Daemon lifecycle + CLI | done | fork-before-runtime daemonize, flock + pidfile + ready file, signals, command loop (tray/page/CLI single writer), full CLI (run/stop/status/drive/doctor/config/logs) |
| 10 Packaging (snap/brew/CI) | done (snap + CI) | `apps/ph-reactor/snap/snapcraft.yaml` (core24, strict, autostart, SNI plugs) + release workflow (musl x86_64 → GitHub release). Homebrew formula: follow-up |
| 11 Final verification + evidence | done | `cargo test` 32/32, clippy 0 warnings, `cargo fmt` clean; live lifecycle E2E (start→tray→add→pause→resume→stop, no orphans) in `docs/superpowers/evidence/rust-reactor/` |

## Day 2 (2026-09-12) — live-daemon hardening

Ran the built daemon end-to-end against the live vault drive and fixed:

1. **Dead command dispatch in the settings routes**: the per-drive
   handlers returned `202 Accepted` before sending anything
   (`drive_cmd` always returned `Some(202)`), so pause/resume/remove/
   resync via the HTTP API were silent no-ops. The validation gate now
   returns `None` on a valid name; the command is then actually sent.
2. **MCP readiness gate**: switchboard `/health` can answer before the
   MCP server is mounted (it registers after the document model
   packages load), so early drive commands could hit MCP 404s. Drive
   commands now wait for a real MCP handshake (bounded, 180 s).
3. **Resume ordering + stale snapshots**: the unpaused state is
   persisted before the re-add (which includes the bounded 60 s
   materialization poll), so status and the tray flip immediately.
   A fresh snapshot is pushed after every persist, since the loop's
   own refresh only runs between commands.
4. **Out-of-band config edits**: the daemon now watches `config.json`
   (mtime). External edits (e.g. another `ph-reactor config set`) are
   adopted between commands; process-relevant changes respawn the
   switchboard, drive-only changes are picked up without a restart.
   `load_quiet` (quarantine flag) prevents a corrupt file from
   clobbering the in-memory config; the daemon stays authoritative
   over the drive list when adopting mid-command.

Known rare flake (not fixed): in one instance the tracing file layer
silently failed at startup and the daemonized child logged no INFO
lines at all (file created, daemon fully functional). A restart fixed
it. Investigate with strace if it recurs.

Open follow-ups (not v1): parse the switchboard's actual port from its
startup log (port-fallback case), aarch64 release build, snap store
publish, Homebrew formula, per-remote sync telemetry (needs a
`syncRemotes` query in `reactor-api`).

## Day 3 (2026-09-12) — sync actually syncs (CONNECT channel scheme)

Day 2's live E2E proved the daemon plumbing, but the vault drive never
synced content. Root cause (verified against the installed
`@powerhousedao/switchboard@6.2.2` dist): the switchboard boots with the
passive `"switchboard"` channel scheme — its GqlChannelFactory only
serves *inbound* channels (a peer's `POST /graphql/r`); the daemon's
`addRemoteDrive` persisted a dead registration (no `sync_cursors` row,
no backfill ever). The Connect app, by contrast, uses the active
`"connect"` scheme (`GqlRequestChannelFactory`: touch/poll/push against
the remote's `/graphql/r`).

Changes:

1. **Generated boot wrapper**: the supervisor no longer runs
   `dist/index.mjs`; it spawns
   `<switchboard>/.ph-reactor/entry.mjs`, which the daemon regenerates
   on every config write. The wrapper imports the installed
   `@powerhousedao/switchboard/server` and calls `startSwitchboard`
   with `channelScheme: "connect"`, `mcp: true`, the registry URL and
   boot packages (boot-time package auto-install via `HttpPackageLoader`),
   the default local drive, and a `jwtHandler`.
2. **Per-drive tokens without secrets on disk**: drives with a
   `tokenEnv` get `PH_DRIVE_TOKEN_<i>` in the child environment (value
   resolved at spawn time; names are part of the process fingerprint,
   values are not) and an origin→env entry in the wrapper's `jwtHandler`.
   The wrapper is forward-compatible: the published 6.2.2 does not wire
   `options.jwtHandler` into its sync channels (verified in its dist
   chunks — only the attachment service uses a JWT handler), so until a
   build with that wiring ships, auth-gated remotes cannot be
   authenticated on the sync channel.
3. **Honest status**: new `requires-auth` drive status (plus
   `AddOutcome` classification of add failures: permission markers →
   `requires-auth`, else `error`). The daemon records each drive's last
   registration attempt (boot re-add, add/resume/resync, cleared on
   remove/pause) and the status view renders it instead of an endless
   `connecting`.
4. **No more sync-state clearing**: the supervisor's `clear_sync_state`
   (a `node -e` PGlite script that deleted `sync_%` tables after every
   child exit) is removed. It destroyed the cursor resume points —
   with persisted cursors a restart now *resumes* the sync; a full
   re-backfill is the explicit `drive resync`.
5. **`powerhouse.config.json` no longer writes `remoteDrives`** — the
   installed 6.2.2 entry never read that key, and the daemon owns
   registration (MCP), which also keeps paused/removed drives from
   re-syncing after a respawn.

Verification: `cargo test` 38/38, clippy + fmt clean. Live E2E in
`docs/superpowers/evidence/rust-reactor/`:

- **Local peer** (second switchboard, own state dir + PGlite, port 4101,
  public drive with two documents): the daemon's child registered it
  over `connect` channels — the peer's drive document **materialized in
  the daemon's local store** (`getDrives` shows both drives; `getDocument`
  returns the peer's document content). This is the first content sync
  through the daemon's child.
- **Stop/start**: the daemon stopped; on restart the drive document
  persisted (no sync-state clearing) and the cursor resumed — no full
  re-backfill (no new backfill envelopes in the peer's log).
- **Vetra** (`powerhouse-knowledge`): the daemon boots cleanly; the
  anonymous touch of the vetra sync endpoint is rejected by vetra's
  auth projection (403 "insufficient permissions to read this document")
  → the drive is reported `requires-auth` with the reason, instead of
  silently pretending to sync. Verified the published switchboard cannot
  fix this alone (no `jwtHandler` wiring in 6.2.2's sync path) — filed
  as an upstream gap in the spec.

## Day 3 (continued, 2026-09-12) — the published build drops both options; the daemon patches it

The previous day's E2E showed the daemon's child receiving the peer's
touch but the peer never receiving anything back — the channel was
half-dead. Root cause (verified against both published builds, 6.2.2
and 6.2.3-dev.3): `initServer` calls
`applySwitchboardReactorDefaults(reactorBuilder, clientBuilder, {...})`
without forwarding `options.channelScheme` or `options.jwtHandler`, so
the boot wrapper's `channelScheme: "connect"` and `jwtHandler` were
silent no-ops — the child ran passive no matter what. The daemon is a
sync client by design, so the fix lives in the daemon:

1. **Post-install sync-client patch** (`bootstrap::switchboard::patch_sync_client`):
   after the npm install (and re-checked on every start; idempotent) the
   daemon patches the installed `dist/server-*.mjs`: inserts
   `channelScheme: options.channelScheme` into the
   `applySwitchboardReactorDefaults` call and appends
   `reactorBuilder.withJwtHandler(options.jwtHandler)` after it.
   Anchored on unique strings, pristine chunk backed up under
   `.ph-reactor/dist-patch/`, re-read-and-verified after writing (backup
   restored on failure), no-op when the markers are already present
   (a future build that wires the options natively needs no daemon
   change), skipped with a warning on an unrecognizable layout.
2. **Fresh-state bootstrap bug**: the foreground `run` path (used by
   supervised starts) loaded the config before the state tree existed —
   a first run on a fresh state root died with ENOENT on the config
   write. `run_inner` now ensures the directories first.
3. **Materialized drives stuck at `connecting`**: the local-mirror
   match read the drive slug from `state.slug`/`slug`, but `getDrive`
   reports it under `header.slug`.
4. **Failed adds invisible**: a rejected add (e.g. the anonymous vetra
   `touch`) never entered `config.drives` and vanished from `status`.
   The daemon records the attempted URL per drive name; the status view
   lists drives that only exist as failed outcomes (with the remote
   URL and the reason). The `requires-auth` failure message now names
   the remedy (`--token-env`).

Live E2E (fresh state root, switchboard 6.2.2 patched at install):

- **Peer → daemon**: the peer switchboard (4101, its own drive
  `peervault` with two `bai/knowledge-note` documents, package loaded
  from `registry.dev.vetra.io`) — both documents materialized in the
  daemon's local store within a minute.
- **Daemon → peer**: a note created in the daemon's local mirror
  appeared in the peer's drive ~25 s later. Bidirectional sync through
  the daemon's child is proven.
- **Status**: the peer drive reports `synced` (`peervault via
  http://127.0.0.1:4101/graphql/r`); the vetra vault drive reports
  `requires-auth` with the rejection detail and the token remedy.
- **Slug collision** (discovered, documented, not fixed in v1): a
  remote drive whose slug equals the local default drive's slug
  (`powerhouse`) makes both reactors create the same document id; the
  exchanged `CREATE_DOCUMENT`s dead-letter on both sides and the drive
  cannot converge. Typical vault drives use distinct slugs
  (`powerhouse-knowledge`), so the user's case is unaffected; recovery
  for a colliding pair is a store wipe (spec: Edge cases).

Verification: `cargo test` 42/42, clippy + fmt clean; all three code
changes committed separately (patch / bootstrap fix / status fixes).
