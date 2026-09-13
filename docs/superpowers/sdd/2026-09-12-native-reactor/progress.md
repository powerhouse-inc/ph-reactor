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
| 09-13 | 13 P2P transport (DHT + relay + TOFU) | done (partial) | commits a7dbc10/6f0ee44/0019f2e: Kademlia DHT (peer routing + provider records + a `DhtBootstrap` command; identify→kad hookup + a `PeerConnected` event) with a 2-peer provider-discovery test; circuit relay (config-gated server `p2p.relay`, default off, + always-on client); **TOFU key pinning** (the store keeps a pinned ed25519 key per peer id; a peer presenting a *changed* key is refused — handshake not completed, drive not added). **Still open:** the relay-mediated-hop test (loopback relay v2 reservation is unreliable; needs a realistic network), group-scoped name records, and the `2.0.0` action-aware wire. |
| 09-13 | 14 Invites — one-shot handshake + CLI | done (core) | commits 0f4d168/ac27832/8c68b59: `p2p/invite.rs` — `InviteToken` (inviter's instance name + peer id + ed25519 pubkey + resolved listen address + 16-byte challenge nonce + granted groups, ed25519-signed; shareable base64 string) and `InviteAccept` (a join-proof echoing the nonce, signed by the joiner). The joiner verifies the token, pins the inviter (TOFU), and adds a drive addressed `/ip4/…/tcp/…/p2p/<inviter>`; the engine attaches the proof to the first hello, the inviter verifies it, pins the joiner, and adds a drive back. **CLI** `ph-reactor invite [--group …]` (prints the token) and `ph-reactor join <token>`; both go through the settings API (`/api/invite`, `/api/join`, one-shot replies). The joiner's drive persists via the join command; the inviter's via a new `DriveJoined` engine event (both sides durable across restarts). Proven bidirectionally with two real daemons (`doc add` on each side reaches the other; `drive list` shows the peer on both). **Not done / follow-up:** the signed `group` membership doc + per-topic gossip membership, and local ban lists (`ph-reactor ban`) — these are the remainder of task 14 / feed task 15. |
| 09-13 | 15 Ban lists | done (core) | a local ban list that refuses a peer's handshakes so it cannot sync with the vault. Engine: a `banned: HashSet<PeerId>` (constructor param, `EngineCommand::Ban`/`Unban` mutate it); the handshake is refused at BOTH gates — the dialer (`open_handshake` marks the drive `error "banned by user"` instead of dialing) and the responder (`serve_request` answers an incoming hello from a banned peer with `HelloErr::Banned`, a new `codec` variant with a unit test). Persisted to `bans.json` in the state dir (a `StatePaths::bans_file`; daemon loads it into the engine at startup and re-saves on each Ban/Unban). CLI `ph-reactor ban/unban <peer>` (settings API `/api/ban`, `/api/unban`, one-shot replies) for the future ban UI; a bad peer id is rejected. Regression test `banned_peer_is_refused` (a banned peer that dials the vault gets `HelloErr::Banned`, the drive ends `error`, and no doc crosses the link). The full ban UI and the auto-ban-on-auth-errors part of the user request remain open. |
| 09-13 | 16 Auto-ban on repeated auth failures | done | the engine counts failed auth (wrong-token) attempts per peer (`auth_failures: (count, last-attempt)`); a peer that hits `AUTH_BAN_AFTER` (3) within `AUTH_BAN_WINDOW` (10 min) is auto-banned — added to the live `banned` set, its drive marked `error: auto-banned …`, and a new `EngineEvent::PeerAutoBanned { peer }` emitted, which the daemon handles by persisting the peer to `bans.json` (so it survives restart like a manual ban). The rejected hello then answers `HelloErr::Banned` instead of `BadToken`. Unit-tested (`auto_ban_after_auth_threshold`: a peer crosses the threshold and is banned; a distinct peer is independent). This is the "auto-ban the offending peer after N failed auth attempts" part of the original request. **Still open:** the ban UI in the settings page. |
| 09-13 | 17 Ban UI in the settings page | done | the settings page gains a "Banned peers" section: a table of the current ban list with a per-peer **unban** button (POST `/api/unban`) and a "ban a peer" input (POST `/api/ban`), plus a hint that auto-ban fires after 3 failed auth attempts in 10 min. The ban list rides the shared `StatusSnapshot` (a new `bans: Vec<String>` field, `#[serde(default)]` so the `status --json` contract stays backward-compatible) — the daemon already refreshes it every 5 s and the page already polls `/api/status`, so no new polling path. End-to-end verified: ban → the peer appears in `/api/status.bans` and the page section; unban → it clears. **This completes the full ban-lists scope** (manual ban, auto-ban, and the UI). |
| 09-13 | 18 Read-model layer (`views/`) | done | a new `ph-reactor-views` crate: `DocumentView` (per-model read-model projection), `RelationshipIndex` (the relationship graph), a `ReadModelCoordinator` (primes from the store, then drives the read models off the store's doc-change feed), a `QueryService`, and a `ProcessorManager` (a job queue over the same feed). **This is a consumer crate** (it depends on `ph-reactor` to reach the store), so the daemon cannot depend on it — the daemon's own query surface is a direct projection over the store (task 20), not this crate. Unit tests for the projection, index, coordinator, and query service. |
| 09-13 | 19 Ten-client E2E + full-mesh finding | done | `views/tests/e2e.rs`: ten reactors in one process (90 drives, full mesh, real ed25519 keys, distinct ports). Two peers create a realistic project-management + finance doc set (7 + 2 docs, 4 L1 models with field types, preconditions, and a reverse-index); asserts all ten converge on the same docs and read models and that every read model answers the same queries (2 projects, 2 accounts, 3 tasks, 2 transactions). **Finding:** the initial convergence (the connect-time catch-up) is reliable for all ten, but *ongoing* changes to an already-converged set are lossy in a ten-peer full mesh — a gossip-missing peer can wait a full 30 s reconciliation tick, and non-bootstrap peers (discovered via the DHT, not dialed directly) rely on that lossy path. The two-peer engine test covers the reliable update path. Tightening live convergence for large meshes is the follow-up. |
| 09-13 | 20 Query CLI + `/api/query` | done | `src/query.rs`: a direct read-model projection over the store (`query_docs`: select by model + a field-equality filter, JSON-number-aware, sorted by name) plus `parse_filter` (`K=V`, V parsed as JSON when possible). Exposed two ways: the `query <model> [--filter K=V]` CLI (opens the store read-only, so it works with or without a running daemon, like `doc list`) and `GET /api/query?model=&field=&value=` on the settings server (the daemon holds the store in the `Settings` state). It lives in the core crate because the daemon cannot depend on `ph-reactor-views` (circular). Unit tests (model selection, field filter, numeric-vs-string matching, filter parsing) + a live CLI/HTTP smoke test. |

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
- `cargo test -p ph-reactor -p ph-reactor-views`: all green (the lib unit
  suite + the `ph-reactor-views` read-model/projection/indexer unit suites
  + the integration suites: two-engine sync, the late-drive-add handshake
  race, DHT 2-peer provider discovery, invite/join sync, banned-peer
  refusal, auto-ban-on-auth-failures, and the ten-client E2E).
- **Ten-client E2E** (`views/tests/e2e.rs`): ten reactors in one process
  (90 drives, full mesh, real ed25519 keys, distinct ports) converge on a
  realistic project-management + finance doc set created across two peers;
  every client's read model answers the same queries. Passes in ~13 s.
- `cargo clippy --all-targets -- -D warnings`: clean. `cargo fmt --check`:
  clean.
- Release binary smoke test: `ph-reactor --help`, `status`, and `doctor`
  against a fresh state dir all pass; `doctor` confirms the session bus is
  reachable (the StatusNotifierItem tray dependency), the identity key, the
  doc store, and the listen address.
- Two-binary E2E (both daemons daemonized, separate state dirs, real
  identities): B `doc add note` → A's `doc get note` shows B's fields;
  A `doc add from-alpha` → B lists both docs; both `status --json`
  report `synced`. Second E2E (fresh dirs, live `drive add` against
  running daemons, pre-link doc with a JSON-object field, post-link
  docs both ways, restart, identical doc sets) — see
  `../../evidence/native-reactor/e2e.md`.
