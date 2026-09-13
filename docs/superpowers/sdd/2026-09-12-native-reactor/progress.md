# Native Rust Reactor — SDD Progress

Spec: `../../specs/2026-09-12-native-reactor-design.md`. Plan:
`../../plans/2026-09-12-native-reactor.md`.

Worktree `~/.worktrees/ph-reactor-native`, branch `feat/native-reactor`.
Three working sessions: 09-11/09-12 AM (model, store, engine core),
09-12 (identity/handshake correctness, gossip fan-out, `doc` CLI,
verification), and 09-12 PM (live E2E with late drive adds; daemonize
failure reporting; hello-race fix). Machine: rustc 1.98.1, libp2p 0.56,
headless Linux (session bus present but no SNI watcher — tray falls
back to the `org.kde.StatusNotifierItem-1000-1` name).

| Date | Task | Status | Notes |
|---|---|---|---|
| 09-12 | 1 Document model (`doc.rs`) | done | commit a345b3c: `DocId/Field/Doc/Op/VecClock`, unknown-work rule, per-field LWW `(ts, origin)`, ed25519 sign/verify; convergence + tamper tests |
| 09-12 | 2 Durable store (`store.rs`) | done | commit cb8c125: per-doc WAL (fsync JSONL) + snapshot at 1024 ops + `index.json`, replay, quarantine counter, name registry, local-only outbound queue; 20+ unit tests |
| 09-12 | 3 p2p core (`p2p/`) | done | libp2p swarm (TCP+Noise+Yamux), mDNS toggle, gossipsub `ph-reactor/docs/1.0.0`, request/response `/ph-reactor/sync/1.0.0` (Hello/HelloAck/HelloErr, CatchUp/CatchUpAck capped+resumable, Summary/SummaryAck), length-prefixed JSON codec; codec round-trip + size-cap tests |
| 09-12 | 4 Drive manager | done | per-drive runtime (status, remote clocks, in-flight set, handshake flag), dial/hello/summary/catch-up, reconnect on events, status mapping to the 6-value vocabulary; token gate (`requires-auth`) |
| 09-12 | 5 Daemon, config, CLI, contracts | done | config v2 (`schemaVersion: 2`: instance/p2p/drives/settings/logLevel, `extra` preserved) with v1→v2 migration (multiaddr `url`→`addr`, npm keys dropped); `bootstrap/`, `mcp.rs`, `registry.rs`, `supervisor.rs` deleted; daemon = store + engine task + settings + tray + poll loop; `status --json` contract (`reactor` block); settings 9 routes kept; version 1.0.0 |
| 09-12 | 6 Sync integration + E2E | done | in-process two-engine test (handshake, bidirectional doc propagation over loopback TCP) + two-binary E2E (see `../../evidence/native-reactor/`) |
| 09-12 | 7 Packaging, docs, cross-checks | done (partial) | README rewritten for the native architecture; snap build unchanged in shape (strict, musl); **Omarchy plugin cross-check not run here** — its reducer expects the 0.x `switchboard` block; the new snapshot's `reactor` block needs the plugin's fixture updated in its own repo (contract section of this README documents the new shape) |
| 09-12 | 8 Evidence + SDD | done | this file, the evidence dir, plan marked complete |
| 09-12 PM | 9 Robustness: daemonize failures + handshake race | done | live E2E exposed two bugs: daemonized child failures invisible (swallowed error + zombie answers `kill 0` — a settings-port collision behind a 30 s "hang") and the hello handshake never completing when the dialer's connection lands before the drive exists (late `AddDrive` on an already-connected peer). Fixes: child stderr → `logs/stderr.log`, non-blocking `waitpid` reaping + log tails in the failure message, settings-port pre-check, and a `guests` record + `open_handshake` helper in the engine (regression test: `drive_added_after_peer_already_connected_completes_handshake`). See evidence file, bugs 6–7 |
| 09-12 | 10 open@1 model + action-log store | done | commits 4f8fdeb/9f64817: the `Action` envelope (model/kind/payload/ts/clock/origin/cosig/sig/prev_hash; ed25519 + co-signatures; content-hash chain); `open@1` (`set`/`delete`) reducing 1:1 to v1 field-writes; the per-doc WAL becomes the action log with a single apply pipeline (verify → model → validate → precondition → reduce → merge); the P2P wire carries actions |
| 09-12 | 11 L1 interpreter + group model + quorum | done | commit d7f3aac: `model/l1.rs` (a JSON model definition interpreted by one fixed engine: typed fields, write templates over `$actor`/`$ts`/`$payload.*`, a precondition DSL, and a quorum spec) and `model/group.rs` (members/managers; add/remove-member; add-manager quorum-gated for the two-person rule); quorum = N distinct valid co-signers in the group; quorum pass/fail, distinctness, and tamper tests |
| 09-12 | 12 doc verify + doc action + verify engine | done | commits 8842529/a99dc11: `Store::verify` (a read-only audit that replays and re-reduces the surviving log, re-verifying every origin + co-signature, each precondition, the prev_hash chain, and that the re-folded field map equals the stored doc; a per-action `VerifyReport` + `render()`); `doc verify` (read-only, VERIFIED/FAILED, non-zero exit) and `doc action` (daemon path: `/api/docs/action` → `Command::CreateAction` → `apply_local_action`). Also fixed `create_doc`, which built its whole batch against an empty log (every prev_hash None) — it now applies one at a time; `apply_action` has no chain check, so verify is the independent chain audit. `--cosign` is deferred to the full-P2P phase (collecting peer co-signatures needs their keys over the wire) |
| 09-13 | 13 P2P transport (DHT + relay increment) | done (partial) | commits a7dbc10/6f0ee44: Kademlia DHT (peer routing + provider records + a `DhtBootstrap` command; identify→kad hookup + a `PeerConnected` event the daemon logs) with an in-process 2-peer provider-discovery test (`tests/dht.rs`); circuit relay wired — config-gated server (`p2p.relay`, default off) + always-on client via `SwarmBuilder::with_relay_client` (additive to direct TCP). **Not done / follow-up:** the in-process relay-mediated-hop test (the relay v2 reservation is not reliable under loopback; needs a realistic network), TOFU key pinning, group-scoped name records, and the `2.0.0` action-aware wire — these ride with task 14 (invites), where the group docs make them meaningful. A banned peer is still refused via `rejects_peer` (tasks 6/12 ban machinery). |

## Deviations from the spec

- **`HelloAck` carries the responder's pubkey** (`pubkey: Option<String>`,
  serde-default so pre-pubkey peers still decode). The dialer registers
  the responder's key under the *verified* connection identity, so ops
  verify even when the responder never dials back. The hello responder
  also registers the dialer's key under the verified peer id — ops are
  verified by origin (the writer's peer id), never by drive name.
- **Identity is announced on the resolved listen address**: the engine
  emits its `Identity` event from the swarm's `NewListenAddr` (the first
  poll), not from the pre-poll `listeners()` view — required for port-0
  (ephemeral) listens; fixed-port listens resolve to the same address.
- **The engine's tick is 5 s; the per-drive reconciliation cadence is
  30 s** (`TICK_INTERVAL` vs `CATCH_UP_TICK`). The 5 s tick drains the
  store's local-only outbound queue into the gossipsub mesh (new op →
  published ≤ 5 s after creation); summary/catch-up per drive stay at
  the spec's 15 s → 30 s reconciliation window (the spec's 15 s was
  doubled to halve chatter on idle drives; convergence latency for
  missed gossip is therefore ≤ 30 s, verified in the E2E).
- **`doc` CLI added** (`list|get|add`) per the spec's CLI line, with
  `add` synchronous through the settings API (`POST /api/docs`, one-shot
  reply) — this is also what makes the two-binary E2E possible. `doc
  list/get` open the store read-only (safe with or without a daemon).
- **Status block renamed `switchboard` → `reactor`** (the 0.x name
  referred to the Node child that no longer exists; the spec's "shape
  unchanged" was interpreted as "the Omarchy-consumed fields and drive
  vocabulary survive", which they do — the plugin fixture must be
  updated in its repo, task 7 above).
- **No QUIC feature in the shipped build** (spec: optional, off by
  default; the snap stays static-musl without it).
- **Hello handshakes survive a late `AddDrive`.** A peer whose valid
  hello arrived before any drive existed is remembered (`guests`); a
  later `AddDrive` for that peer completes the handshake from the
  record, and `AddDrive`/`ConnectionEstablished` share one
  `open_handshake` helper so an already-connected peer also gets the
  hello. Without this, the late side sat in `connecting (dialing)`
  forever (docs still flowed over the other side's connection) —
  reproduced in the live E2E, now a regression test.

## Verification
- `cargo test`: 67 unit + 3 integration (two-engine sync; late-
  drive-add handshake race; DHT 2-peer provider discovery) — all green.
- `cargo clippy --all-targets -- -D warnings`: clean. `cargo fmt --check`:
  clean.
- Two-binary E2E (both daemons daemonized, separate state dirs, real
  identities): B `doc add note` → A's `doc get note` shows B's fields;
  A `doc add from-alpha` → B lists both docs; both `status --json`
  report `synced`. Second E2E (fresh dirs, live `drive add` against
  running daemons, pre-link doc with a JSON-object field, post-link
  docs both ways, restart, identical doc sets) — see
  `../../evidence/native-reactor/e2e.md`.
