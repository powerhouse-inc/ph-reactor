# Native Rust Reactor (libp2p) — Implementation Plan

Spec: `../specs/2026-09-12-native-reactor-design.md`.
Worktree: `~/.worktrees/ph-reactor-native`, branch `feat/native-reactor`
(from `main` = c2f9a0f). Each task is one commit; tasks build in order.
Machine: rustc 1.98.1 (libp2p 0.56 MSRV 1.83 satisfied).

Status (2026-09-12): Tasks 1–12 done — the core reactor plus the model
core (open@1 / action-log store, the L1 interpreter + group model +
quorum, and `doc verify` / `doc action` with the verify engine). Tasks
13–15 (full P2P transport, invites, N-peer E2E) pending. See
`../sdd/2026-09-12-native-reactor/progress.md` for the record and
deviations (HelloAck carries the responder's pubkey; identity is
announced on the resolved listen address; `doc` CLI added; gossip
fan-out on a 5 s tick with 30 s per-drive reconciliation; `doc action
--cosign` deferred to full P2P).

## Task 1 — Document model (`src/doc.rs`)

`DocId` (uuid v4), `Field { value, ts, origin }`, `Doc`, `Op`,
`VecClock`. Pure merge semantics: `apply_op(state, op)` with the
unknown-work rule (op clock not ⊇ state clock) and per-field LWW on
`(ts, origin)`; op deletion; ed25519 sign/verify over
`doc_id||key||value||ts||origin`.
Acceptance: unit tests — merge convergence under random permutations
(both directions of a fork), LWW determinism, signature valid/wrong-key/
tampered, clock ⊇/⊉/disjoint cases.

## Task 2 — Durable store (`src/store.rs`)

In-memory read model (docs by id + name index) over per-doc append-only
WAL (`docs/<id>.log`, fsync'd JSONL) + snapshot at 1024 ops
(`docs/<id>.snap`, log truncated); `index.json` name→id; local op queue
(`mpsc`, single writer) that persists then applies then emits a
`SyncMsg` for the p2p layer; startup replay (snapshot + log); quarantine
of unverified ops.
Acceptance: unit tests — append/replay after reopen, snapshot+truncate
boundary, name uniqueness + validation, delete removes log+snap+index,
corrupt line isolation.

## Task 3 — p2p core (`src/p2p/{mod,exchange,codec}.rs`)

Identity: ed25519 keypair file (`key`, 0600) → `libp2p::identity`.
Swarm builder: TCP + Noise + Yamux; `quic` cargo feature (off by
default); mDNS (instance name); gossipsub topic
`ph-reactor/docs/1.0.0`; custom request/response behaviour
`/ph-reactor/sync/1.0.0` with `Hello/HelloAck`, `Request/Response`
(payload-capped, resumable), `Summary/SummaryAck`; compact codec
(length-prefixed serde_json — v1 simplicity; binary codec is a
follow-up). `SwarmEvent` enum for the daemon.
Acceptance: unit tests — codec round-trip, Hello matrix (version,
token), Summary diff both directions, cap/resume; swarm unit: two
in-process swarms over loopback complete Hello.

## Task 4 — Drive manager (rewrite `src/drives.rs`)

`Drive { name, addr, peer?, token_env?, paused, available_offline }`.
Per-drive task: dial (PeerId resolved from handshake) → Hello (token
gate) → initial catch-up (per-doc `Request` with empty clocks) → live:
consume `SyncMsg` (apply, republish to gossipsub, forward to peers
connected to the same drive set) + 15 s `Summary` reconciliation while
connected; reconnect backoff 1→60 s. Status mapping to the existing
vocabulary (`connecting|synced|paused|offline|requires-auth|error`).
Acceptance: unit tests — status transitions, backoff schedule,
pause/remove cancel tasks; integration (with task 5's wiring) in task 6.

## Task 5 — Daemon, config, CLI, contracts

- `config.rs`: v2 schema (`p2p{port,listen,mdns,announce}`,
  `instanceName`, drives with `addr`), v1→v2 migration (multiaddr `url`
  → `addr`; drop `switchboard/node/registry/packages`; preserve
  `extra`), dotted-key `set()` validation.
- Delete `src/bootstrap/`, `src/mcp.rs`, `src/registry.rs`,
  `src/supervisor.rs`.
- `daemon.rs`: engine bring-up in-process (store replay → swarm listen
  → `EngineReady`), 5 s poller building `StatusSnapshot` from store +
  drives (switchboard block = engine: running/healthy, port = p2p
  port, last_event = latest sync event), command execution against the
  store (add/remove/pause/resume/resync, set-config, quit), `doctor`
  rewrite, `logrotate`/pidfile/flock/daemonize kept.
- `cli.rs`: `drive add <multiaddr …>`, new `doc` subcommand
  (`list|get|add --field k=v`), `--version`/`status --json` unchanged.
- `status.rs`: shape unchanged; docs count in `last_event` where
  useful; version → 1.0.0 (Cargo.toml; `rust-version` → 1.83; add
  libp2p/uuid deps, `quic` feature).
- `settings/mod.rs`: routes/bodies unchanged; add-drive hint text →
  multiaddr.
Acceptance: `cargo build` + `cargo clippy -- -D warnings` clean;
`status --json` with no drives = valid snapshot; `config set` v2 keys
validate; v1 config migrates.

## Task 6 — Sync integration + E2E

- In-process: two daemon engines over loopback — gossip propagation,
  offline catch-up after reconnect, 3-peer convergence, token-mismatch
  rejection.
- Two-binary E2E script (`tests/e2e.sh` or a `#[tokio::test]` spawning
  the built binary): B `doc add`; A `drive add /ip4/127.0.0.1/tcp/…/p2p/…`;
  A sees B's doc; A mutates; B sees; both `status --json` = synced.
Acceptance: both green; E2E evidence captured.

## Task 7 — Packaging, docs, cross-checks

README rewrite (one-process architecture, multiaddr drives, tokens,
security, state layout v2); snapcraft.yaml + release workflow build the
new binary (musl, `quic` off); Omarchy plugin cross-check: its
`ReactorState.js` reducer accepts the new `status --json` (run its
node tests with a v2-shape fixture; fix the fixture only if a field
semantically changed — expected: none).
Acceptance: plugin `tests/run` green against v2 output.

## Task 8 — Evidence + SDD

Full `cargo test` + clippy + fmt, release build size recorded (vs
v0.2.0: libp2p cost), E2E evidence file, SDD reports per task.

## v1.1 — document models and full P2P

Spec: the "Extension" section of
`../specs/2026-09-12-native-reactor-design.md`.

## Task 9 — Action envelope + model foundation

DONE. `doc.rs`: `Hash32`, `ModelRef` (`name@version[#hash]`); `action.rs`:
`Action` (signed envelope incl. the vector clock, model ref, `cosig`,
`prev_hash`) + `CoSig`; `model/mod.rs`: `Model` trait (`validate_payload` /
`check_precondition` / `reduce` / `check_state`) + `Reject` + `ModelRegistry`.
The v1 `Op` is retained as the field-write primitive a model reduces to.
Acceptance: sign/verify/cosig/hash round-trip tests; `cargo check` clean.

## Task 10 — `open@1` + action-log store

`model/open.rs`: `open@1` (`set`/`delete`) reduces 1:1 to the v1 field-writes —
byte-identical for pre-model docs. Rework `store.rs`: the per-doc WAL becomes
the **action log** (`Entry.log: Vec<Action>`); replay reduces each action
through its model into field-writes and feeds the (unchanged) per-field LWW;
the snapshot stores the reduced field map + clocks + the chain hash. `doc add`
builds an `open@1` action; `drain_outbound` / `catch_up` derive ops.
Acceptance: the existing 44 unit + 2 integration tests stay green; new test:
action-log replay reproduces the op-based state.

## Task 11 — L1 declarative interpreter + group model + quorum

`model/l1.rs`: a JSON model definition interpreted by one fixed engine over a
small vocabulary (typed fields; write templates over `$actor`/`$ts`/
`$payload.*`; a precondition DSL incl. `actor-in-group`, `field-is|not`, named
checks, and `quorum {group, min}`). `model/group.rs`: the built-in `group`
model (`members`/`managers`; `add-member`/`remove-member`/`add-manager`;
`add-manager` quorum-gated for the two-person rule). Quorum = N distinct valid
co-signers in the group.
Acceptance: unit tests — payload schema, precondition (quorum pass/fail,
distinctness), reduce to expected field-writes, group membership, tamper.

## Task 12 — `doc action` / `doc verify` + verify engine

`doc action <name> <kind> --payload <json> [--cosign <peer> …]` (daemon path,
like `doc add`) and `doc verify <name>` (read-only store): replay the action
log — re-reduce every action, verify every signature (origin + cosig), every
precondition, the hash chain, and that the re-folded field map equals the
stored one — and print the human-readable audit report.
Acceptance: a happy-path verify prints `VERIFIED`; a tampered action (bad sig /
broken chain / failed precondition) is flagged line-by-line.

## Task 13 — Full P2P transport

Cargo: + `libp2p/{kademlia,identify,relay}`. `p2p/mod.rs`: Kademlia DHT (peer
routing + provider records + name records), relay v2 (client **and** server),
`identify`; bootstrap list in config. Wire becomes **action-aware** (protocol
`/ph-reactor/sync/2.0.0`, topic `ph-reactor/docs/2.0.0`): `ActionMsg` carries
the action + its reduced ops (so a peer without the model still converges the
field map). TOFU key pinning (first key pinned; mismatch flagged). Group-scoped
name records (signed by the group, resolvable only by members).
Acceptance: in-process 3-peer over loopback with one relay-mediated hop; DHT
provider lookup returns a model provider; a banned peer is refused.

## Task 14 — Invites (one-shot handshake)

`ph-reactor invite` (one-shot string: peer ID + signed challenge + granted
groups) and `ph-reactor join <string>` (respond with peer ID + signed
challenge). Both sides pin each other (TOFU); the signed `group` doc records
the membership; per-topic gossip membership propagates. Local ban lists
(`ph-reactor ban <peer>`).
Acceptance: two fresh peers exchange an invite string and sync, the invite
appearing as a signed action in the group doc; a banned peer is refused.

## Task 15 — N-peer E2E + evidence

E2E: N peers (>= 5) with a mix of direct and relay-mediated links, DHT
discovery, per-process topics, and an invite-driven join; converge a model doc
+ a group doc; `doc verify` on a joining peer reproduces the report. Evidence
under `docs/superpowers/evidence/native-reactor/`; SDD reports.
