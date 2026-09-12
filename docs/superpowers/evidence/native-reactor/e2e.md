# Native reactor — verification evidence (2026-09-12)

Branch `feat/native-reactor`, binary `ph-reactor 1.0.0`
(`cargo build`, debug, musl not required for the loopback runs).
Machine: headless Ubuntu, session bus without a StatusNotifierWatcher
(tray falls back to the well-known `org.kde.StatusNotifierItem-1000-1`).

## Two-binary end-to-end (headline)

Two daemonized instances (started with `ph-reactor run --daemonize` —
the fork/ready-file path; the parent prints `ph-reactor started (pid
N)` and exits in ~0.3 s), separate state dirs, real generated
identities:

- A: `PH_REACTOR_STATE_DIR=/tmp/ph-rx-a` — settings 127.0.0.1:4002,
  p2p listen `/ip4/0.0.0.0/tcp/4201`
- B: `PH_REACTOR_STATE_DIR=/tmp/ph-rx-b` — settings 127.0.0.1:4003,
  p2p listen `/ip4/127.0.0.1/tcp/4203`

Both drives are pre-configured (A ↔ B cross-linked via
`ph-reactor drive add /ip4/127.0.0.1/tcp/<port>/p2p/<peer>`, matching
each side's listen address), so the daemons re-dial their partners on
every restart.

Sequence and observed output:

```
$ PH_REACTOR_STATE_DIR=/tmp/ph-rx-b ./ph-reactor doc add note \
    --field "body=hello from beta" --field "n=42"
created doc 'note'

$ PH_REACTOR_STATE_DIR=/tmp/ph-rx-a ./ph-reactor doc get note   # after the next reconcile
{
  "body": "hello from beta",
  "n": 42
}

$ PH_REACTOR_STATE_DIR=/tmp/ph-rx-a ./ph-reactor doc add from-alpha \
    --field "body=hello from alpha"
created doc 'from-alpha'

$ PH_REACTOR_STATE_DIR=/tmp/ph-rx-b ./ph-reactor doc list        # after the next reconcile
from-alpha   1730526f-57dd-4346-9315-5dbacfd86837 (1 fields)
note         583cf118-9577-4fe3-8d0f-acf8a1ed89ea (2 fields)

$ PH_REACTOR_STATE_DIR=/tmp/ph-rx-b ./ph-reactor doc get from-alpha
{
  "body": "hello from alpha"
}
```

`status --json` on both sides after convergence:

```
A: [('b', 'synced', '')]
B: [('a', 'synced', '')]
```

Propagation latency observed: a doc created on one side appeared on the
other within the next 30 s summary/catch-up window (gossip fan-out
covers the steady state; the E2E window was dominated by the
reconciliation tick, as designed).

## In-process two-engine test

`cargo test --test two_engine_sync` (10 s): two full engines (store +
swarm) over loopback TCP with ephemeral ports — handshake both
directions, `Synced` on both drives, A creates a doc → B has it, B
updates it → A has the update, clean shutdown of both.

## Bugs found and fixed by the verification

1. **Codec default capped messages at 0 bytes.** `SyncCodec`
   `#[derive(Default)]` zeroed `max_len`; the request-response behaviour
   constructs codecs via `TCodec::default()`, so every inbound frame
   failed with `message too large: 202 > 0` (a 202-byte hello). The
   explicit `Default` now delegates to `new()` (`MAX_MSG_BYTES`).
2. **Unresolved listen address (port 0).** The `Identity` event was
   emitted right after `listen_on`, before the first swarm poll, when
   `listeners()` is still empty — the configured (unresolved) address
   was announced, and dials of it failed with
   `MultiaddrNotSupported(/ip4/127.0.0.1/tcp/0/…)`. The identity is now
   announced from the `NewListenAddr` swarm event (the resolved
   address).
3. **Peer keys registered under the drive name.** The hello responder
   registered the dialer's key under the drive's *name*; ops are
   verified by *origin* (the writer's peer id), so every gossiped op
   was quarantined (`unknown origin … (no key registered)`). Registration
   now uses the verified connection peer id.
4. **Dial-side registration missing.** Only the hello responder
   registered a key, so an asymmetric link (A dials B; B never dials A)
   could not verify B's ops on A. `HelloAck` now carries the
   responder's pubkey (serde-default, backward-decodable) and the
   dialer registers it under the verified peer id.
5. **Local ops were never published to the mesh.** The store kept a
   local-only outbound queue but nothing drained it. The engine's tick
   now drains it and publishes each op to `ph-reactor/docs/1.0.0`
   (5 s cadence; 30 s per-drive summary/catch-up reconciliation
   unchanged).

## Suites

- `cargo test`: 44 unit + 1 integration — all pass.
- `cargo clippy --all-targets -- -D warnings` — clean; `cargo fmt --check` — clean.

## Not verified here (deferred)

- Omarchy plugin cross-check against the new `reactor` status block
  (its fixture still expects the 0.x `switchboard` shape; update
  belongs to the plugin's repo).
- Snap build with the new binary size (musl + libp2p) — build step is
  unchanged in `snap/snapcraft.yaml`; size delta to be recorded at the
  release.
- QUIC feature (off by default; not exercised).
