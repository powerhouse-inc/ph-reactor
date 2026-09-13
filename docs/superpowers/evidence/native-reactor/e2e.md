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
6. **Daemonized child failures were invisible.** Two parts: the child's
   error was swallowed (`run_inner`'s `Err` mapped to a bare exit code),
   and the parent's liveness check used `kill(pid, 0)`, which answers
   *true for a zombie* — so a child that died early (the trigger
   here: both instances defaulting to settings port 4002; the second
   failed the bind) sat as a zombie while the parent waited out the
   full 30 s timeout with nothing to show. Now: the child writes its
   fatal error to `logs/stderr.log` (and `reactor.log`) and exits 1;
   the parent reaps with a non-blocking `waitpid` (dead children are
   detected instantly, with their exit status) and prints the log
   tails on any failure; a settings-port pre-check fails fast with an
   actionable message. The failure that took a whole session to
   diagnose now reports in 0.28 s: `settings port 4002 … already in
   use (change settings.port in the config)`.
7. **Hello handshake race on late drive adds.** A's dial reached B
   before B had a drive for A: B's `serve_request(Hello)` had no drive
   to mark, so B's half of the handshake never completed — and B's
   later `AddDrive` dial found the peer *already connected*, so no new
   `ConnectionEstablished` fired to open it. B's drive sat in
   `connecting (dialing)` indefinitely while docs still flowed over A's
   connection. Now: peers that presented a valid hello are remembered
   (`guests`), a later `AddDrive` for such a peer completes the
   handshake immediately, and `AddDrive`/`ConnectionEstablished` share
   an `open_handshake` helper that also fires on an already-connected
   peer. Regression: `drive_added_after_peer_already_connected_completes_handshake`
   (one-sided handshake first, late drive add second, then full
   bidirectional doc propagation).


## Live two-binary E2E: late drive add and pre-link docs

Fresh state dirs (`/tmp/ph-e2e-1`, `/tmp/ph-e2e-2`), fresh identities,
both daemonized. Steps, in order:

1. `doc add` on B **before any link existed** (`note-from-two`, with a
   JSON-object field `payload` mimicking a knowledge-note document
   model: nested string, number, and array values).
2. `drive add` on each side with the other's `/ip4/127.0.0.1/tcp/<port>/p2p/<peer>`
   (live, against the running daemons — not pre-configured).
3. A's drive reached `synced` and pulled `note-from-two` via
   catch-up; the JSON field arrived byte-exact (`"score": 7` still a
   number, array intact) — the receiving side had no prior notion of
   the doc's shape; there is no document-model registry to fail
   against, the field map is open.
4. Post-link `doc add` on each side: `note-from-one` (nested object
   field) and `note-two-v2` both appeared on the other side with
   identical doc UUIDs (same documents, not copies).
5. Both daemons stopped and restarted: all three docs survived
   (WAL replay) and both drives re-handshook to `synced` within the
   first reconcile window; `doc list` output identical on both sides
   (`diff` clean).

The handshake race this run exposed is bug 7 above; the daemonize
failure behind the earlier "hangs" is bug 6.

## Invite/join: join a vault with a signed string (2026-09-13)

`ph-reactor invite` (inviter) prints a signed, shareable token
(`base64(JSON)`) binding the inviter's instance name, peer id, ed25519
public key, resolved listen address, a 16-byte challenge nonce, and the
granted groups, with an ed25519 signature over all of it. `ph-reactor
join <token>` (joiner) verifies it, pins the inviter's key (TOFU), adds a
drive addressed `/ip4/…/tcp/…/p2p/<inviter-peer>`, and attaches a signed
join-proof (echoing the nonce) to the first hello; the inviter verifies
that proof, pins the joiner, and adds a drive back. No pre-shared
multiaddr is needed; a tampered or forged token fails the signature
check, and a key change on a pinned peer is refused.

Proven with two daemonized instances (no pre-configured drives):

- alpha `invite` -> printed a 540-char token (`addr=/ip4/127.0.0.1/tcp/4399`,
  `groups=["reactor"]`, signed).
- beta `join <token>` -> drive added and the inviter pinned.
- alpha `doc add shared-note` -> beta `doc get shared-note` returned it
  within ~9 s (beta reached `synced`).
- beta `doc add beta-note` -> alpha `doc get beta-note` returned it
  (bidirectional).
- After convergence, `drive list` shows the peer on BOTH sides; the
  inviter's copy is persisted to its config (survives a restart) via the
  engine's `DriveJoined` event.

Security properties are covered by `tests/invite_join.rs` (full E2E:
invite, join, both sides `Synced`, doc propagation, clean shutdown) and
the unit tests in `p2p/invite.rs` (forged proof rejected by `verify()`,
nonce binding, TOFU key mismatch).

## Suites

- `cargo test`: 73 unit + 4 integration (`dht_discovery`, `invite_join`,
  `two_engine_sync`) — all pass.
- `cargo clippy --all-targets` — clean.

## Not verified here (deferred)

- Omarchy plugin cross-check against the new `reactor` status block
  (its fixture still expects the 0.x `switchboard` shape; update
  belongs to the plugin's repo).
- Snap build with the new binary size (musl + libp2p) — build step is
  unchanged in `snap/snapcraft.yaml`; size delta to be recorded at the
  release.
- QUIC feature (off by default; not exercised).

## 2026-09-13: `query` CLI + `/api/query`

One daemonized instance (`--state-dir /tmp/phq`, release build), two
open-model docs created with `doc add`:

```
$ ph-reactor query
[
  { "fields": { "n": 42, "name": "task1", "status": "todo" }, "model": "open", "name": "task1" },
  { "fields": { "name": "task2", "status": "doing" },        "model": "open", "name": "task2" }
]

$ ph-reactor query "" --filter status=doing     # string field
[ { "fields": { "name": "task2", "status": "doing" }, "model": "open", "name": "task2" } ]

$ ph-reactor query open --filter n=42           # JSON-number field
[ { "fields": { "n": 42, "name": "task1", "status": "todo" }, "model": "open", "name": "task1" } ]

$ curl '127.0.0.1:4002/api/query?model=open&field=status&value=doing'
[{"fields":{"name":"task2","status":"doing"},"model":"open","name":"task2"}]
```

The CLI opens the store read-only (no daemon required); `/api/query` is
answered from the daemon's live store. Unit tests (in `src/query.rs`)
pin the numeric-vs-string distinction and the model selection.

## 2026-09-13: ten-client E2E

`views/tests/e2e.rs`: ten reactors in one process, 90 drives, full mesh,
real ed25519 keys, distinct ports. Two peers create a realistic
project-management + finance set (7 + 2 docs across 4 L1 models with
field types, preconditions, and a reverse-index); the test asserts all
ten converge on the same doc set and read models and that every read
model answers the same queries (2 projects, 2 accounts, 3 tasks,
2 transactions). Passes in ~13 s.

**Finding (documented, not asserted):** the connect-time catch-up
converges all ten reliably, but *ongoing* changes to an already-converged
set are lossy in a ten-peer full mesh — a gossip-missing peer can wait a
full 30 s reconciliation tick, and DHT-discovered (non-bootstrap) peers
lean on that path. The two-peer engine test covers the reliable update
path. Tightening live convergence for large meshes is the follow-up.
