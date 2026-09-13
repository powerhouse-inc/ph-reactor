# Reactor Console — verification evidence (2026-09-13)

Worktree `~/.worktrees/ph-reactor-ui`, branch `feat/ui` (from `main` =
07d2000). Binary `ph-reactor 1.0.0` (`cargo build --release --locked`,
shared target dir). Machine: headless Ubuntu 24.04, session bus without
a StatusNotifierWatcher (tray falls back to the well-known
`org.kde.StatusNotifierItem-1000-1`).

This session added the reactor console (the user interface), the group
feature, a store replay fix, and the LLM test endpoint. Each is
verified below.

## Store replay fix (the "empty vault after restart" bug)

Regression before the fix: a daemon whose WAL was compacted (snapshot
written, WAL moved to `.previous`) lost all documents after a restart,
because the new store replayed only the empty (post-rotation) WAL and
skipped the snapshot.

Reproduction (before fix): add 8+ documents (each `apply` writes to the
doc's WAL; the first crosses the 4 KiB rotation threshold and rewrites
its snapshot), restart the daemon, `doc list` -> empty.

After the fix (`Store::load` replays the snapshot, then the WAL from
`snapshot.log`):

```
$ PH_REACTOR_STATE_DIR=/tmp/ph-rx-e ./ph-reactor doc add n1 --field "v=1"
... (8 documents added)
$ PH_REACTOR_STATE_DIR=/tmp/ph-rx-e ./ph-reactor doc list
n1  open@1  {v: 1}
... (all 8 present)
$ pkill -f 'ph-reactor run'; PH_REACTOR_STATE_DIR=/tmp/ph-rx-e ./ph-reactor run --daemonize &
$ sleep 2; PH_REACTOR_STATE_DIR=/tmp/ph-rx-e ./ph-reactor doc list
... (all 8 STILL present after restart)
```

Locked in by `store::tests::load_replays_snapshot_then_wal` (snapshot at
log 3, ops 4-5 in the WAL -> replayed state is `a=1, b=2`).

## The group feature

Groups are first-class documents, managed through the console and the
CLI:

```
$ PH_REACTOR_STATE_DIR=/tmp/ph-rx-g ./ph-reactor group create crew \
    --member /ip4/127.0.0.1/tcp/4201/p2p/12D3...A --manager ...A --manager ...B
created group 'crew'
$ PH_REACTOR_STATE_DIR=/tmp/ph-rx-g ./ph-reactor group list
crew  members: 1  managers: 2  ...A, ...B
$ PH_REACTOR_STATE_DIR=/tmp/ph-rx-g ./ph-reactor group add-member crew --peer ...C
added member ...C
$ PH_REACTOR_STATE_DIR=/tmp/ph-rx-g ./ph-reactor group activity crew
add-member  by ...A  (actor)
init        by ...A  (actor)
```

The two-person rule holds: a group needs >= 2 managers to be valid;
`group create` with a single manager is rejected with
`group 'solo' is invalid: a group needs at least two managers`. The
console's Groups view drives the same API (`/api/groups`,
`/api/groups/:name/action`, `/api/groups/:name/activity`) and surfaces a
quorum rejection as a friendly error.

## The console (live, driven via the browser tool)

Daemon on a throwaway state dir (settings 127.0.0.1:4012, p2p
127.0.0.1:4212), a local stub LLM server (127.0.0.1:4901 serving
`/v1/models` with a `Bearer` check). Opened the console at
`http://127.0.0.1:4012/` in a real Chromium tab; observed it
rendering and drove every view via `tab.evaluate` (the same DOM
manipulations a user makes):

- **Overview** — the reactor health card (running, peer id, listen,
  doc count) and the LLM card (endpoint + model + "not tested yet").
- **Settings / LLM** — set `baseUrl` to the stub, `model` to a test id,
  hit **Test connection**: `LLM endpoint OK - 2 models available`.
- **Documents** — created a `open@1` document (`newdoc`, fields
  `greeting=hi`, `answer=42`); it appeared in the list; opened it and
  read its fields.
- **Groups** — created a group (`crew` with two managers from the
  daemon's own peer id); it appeared in the list; opened it and saw its
  membership and its `init` action in the activity feed.
- **Drives** — added a drive via the console form (a valid p2p multiaddr)
  and it appeared with a `connecting` status chip and the Pause / Resync /
  Remove actions; a truncated multiaddr was rejected with a structured
  error ("invalid multihash"), confirming the form validates input. The
  peer ban list lives here.
- **Processors** — all five subsystem cards rendered (Sync engine,
  Document store, Settings server, Log rotation, Status poller) with
  their live values.

The console rendered cleanly throughout: no framework, no build step,
vanilla JS + hand-rolled CSS (shadcn-style light/dark tokens, Inter
font). Every mutation went through the daemon's single-writer command
channel; the SPA polls `/api/status` plus the active view's read
endpoints.

## LLM test endpoint (API)

`POST /api/llm/test`:
- against the stub with `LLM_API_KEY` set -> `{ ok: true, model,
  baseUrl, models: [...] }` (200).
- against an unreachable endpoint -> `{ ok: false, error: "..." }` (200,
  structured — never a 500/panic).
- with the key env var unset -> `{ ok: false, error: "api key
  environment variable 'LLM_API_KEY' is not set" }` (200).

Locked in by `settings::tests::llm_test_*` (reachable returns the model
list; unreachable returns the error, not a panic; a missing key reports
a structured error).

## Build + tests (all green)

- `cargo build --release --locked` — clean.
- `cargo clippy --all-targets -- -D warnings` — clean.
- `cargo fmt --all --check` — clean.
- `cargo test -p ph-reactor -p ph-reactor-views`: **80** lib tests +
  integration tests (e2e, sync) — all pass.
- `status --json` still satisfies the Omarchy-plugin contract (the two
  new snapshot fields are `#[serde(default)]`).

## What was NOT done (documented gaps)

- **Auto-install of missing packages from `registry.dev.vetra.io`** is
  not implemented in the daemon. That capability is the **connect**
  frontend's (`apps/connect` package manager — `package-manager.ts`,
  `useRegistryPackages.ts`, the `registry.dev.vetra.io` npm protocol);
  the Rust daemon is a self-contained compiled binary and needs no npm
  packages at runtime. The spec notes this is connect's responsibility.
- **Brew formula**: not added (the user said "snap/brew?" — the snap
  is the primary channel and exists; a brew formula can be added
  against the published GitHub release assets as a follow-up).
