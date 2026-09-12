# Native Rust Reactor (libp2p sync) — Design

Issue: none yet (architecture change; supersedes the Node-switchboard
machinery of v0.x).

## Problem

`ph-reactor` today is a thin supervisor: the Rust daemon spawns a Node.js
process (`@powerhousedao/switchboard`), which runs the TypeScript Reactor
over PGlite (WASM Postgres). All document storage, all content sync, and
all remote-drive registration happen in that Node child. The Rust side
talks to it only over loopback (`GET /health`, stateless JSON-RPC `POST
/mcp`) and to remotes via plain HTTP (`GET <drive-url>`). The result: a
heavy private Node runtime bootstrap (600 s npm installs, tarball
downloads, dist patching), an interpreter-bound document store, HTTP-based
content sync, and two processes where one would do.

## Goals

1. **One process.** The daemon *is* the reactor: a native Rust document
   store (event-sourced, durable) replaces Node + PGlite entirely. No
   child process, no Node bootstrap, no npm.
2. **libp2p for sync.** Content sync over libp2p (Noise-encrypted,
   identity-based peers) instead of HTTP. A "drive" is now a remote
   **peer** (multiaddr), not a switchboard URL.
3. **Performance:** native event log + read model, in-process calls
   between daemon features (no loopback hop for control), QUIC-grade
   transport where enabled.
4. **Stable external contract.** The `status --json` / `/api/status`
   `StatusSnapshot` shape and the settings page's 9 routes keep working
   unchanged (the Omarchy plugin and the embedded UI survive the rebuild),
   with the drive-status vocabulary
   `synced|connecting|paused|offline|requires-auth|error` preserved.

Non-goals: DHT/Kademlia discovery (v1 = explicit multiaddr + mDNS);
CRDTs beyond per-field last-writer-wins; full Powerhouse signature/
permission model (a per-op ed25519 signature is included, but no
registry-of-keys); attachment/blob storage (field values are JSON);
Windows/macOS; serving arbitrary HTTP switchboards (a remote must run
the same native protocol); DHT-based content addressing.

## Identity and discovery

- **Identity:** each daemon generates an ed25519 keypair on first start,
  stored 0600 at `<state>/key`. The libp2p `PeerId` derives from it; it
  is the stable identity for op origins and peer addressing.
- **Listening:** the swarm listens on `p2p.port` (default 4201) — TCP +
  Noise + Yamux always; QUIC behind the `quic` cargo feature (off by
  default, same port).
- **Discovery:** (a) explicit dial: `drive add <multiaddr>` — the peer
  id is learned from the Noise handshake; (b) mDNS on the local
  segment for peer-to-peer on a LAN (announces the instance name). No
  DHT in v1.
- **Auth:** optional per-drive shared token (the existing `tokenEnv`
  convention: only the env-var *name* is stored, the value is read from
  the environment). Exchanged in the sync `Hello`; a mismatch rejects
  the drive (`requires-auth` status).

## Document model and store (`doc.rs`, `store.rs`)

- **Document:** `doc_id` (UUID), `name` (unique slug, ≤ 64 chars, no
  `/`), `fields: BTreeMap<String, Field>` where
  `Field { value: serde_json::Value, ts: u64, origin: PeerId }`, plus
  created/updated stamps.
- **Operation:** `Op { doc_id, field: Option<String>, key: String,
  value: Option<Value>, ts: u64, clock: VecClock, origin: PeerId,
  sig: [u8; 64] }`. `field: None` = delete the document. `sig` signs
  `doc_id || key || value || ts || origin` with the origin's identity
  key; the store verifies on apply (invalid → quarantine, never applied).
- **Concurrency:** per-document **vector clock** (`BTreeMap<PeerId,
  u64>`). An op is applied when its clock is not a superset of the
  receiver's for that doc (unknown-work rule). Same-field conflicts
  resolve by LWW on `(ts, origin)` — deterministic and convergent
  regardless of application order.
- **Durability:** per-document append-only log
  (`<state>/docs/<doc_id>.log`, one JSON op per line, fsync'd) plus a
  snapshot (`<state>/docs/<doc_id>.snap`: current state + clock) written
  when the log exceeds 1024 ops; the log is then truncated. Startup
  replays snapshots + logs. A global `<state>/docs/index.json` maps
  name → doc_id.
- **Local writes** go through the same op path as remote sync (single
  writer, in-process `mpsc`), so local and remote histories merge with
  identical rules.
- **CLI surface** (new): `ph-reactor doc list|get <name>|add <name>
  [--field k=v …]` — a native daemon is useful standalone, and this is
  how local content enters a vault.

## Sync engine (`p2p/`)

Two libp2p behaviours:

1. **Gossip** (gossipsub, topic `ph-reactor/docs/1.0.0`): on applying a
   local or remote op, the swarm publishes a compact `OpMsg`
   (the op + sender's per-doc clock). Peers apply unknown ops and
   republish — fan-out for new work.
2. **Exchange** (custom request/response, protocol
   `/ph-reactor/sync/1.0.0`):
   - `Hello { name, version, token? }` → `HelloAck { name, version,
     doc_count }` or a typed error (`bad-version`, `auth-required`,
     `bad-token`);
   - `Request { doc_id, have: VecClock }` →
     `Response { doc: Option<DocState>, ops: [Op], more: bool }`
     (payload-capped, resumable via the updated clock) — history
     catch-up when a peer joins or reconnects;
   - `Summary { per-doc clocks }` — periodic reconciliation: the
     requester sends its clocks, the responder returns the clocks it is
     missing, the requester then `Request`s exactly those gaps. Covers
     gossipsub loss without unbounded state.

A **drive** (`Drive { name, addr: Multiaddr, peer: PeerId?, token_env?,
paused, available_offline }`) is a managed peer: the daemon dials
`addr` (resolving the PeerId), completes `Hello`, performs initial
catch-up, then tracks live ops via gossip + periodic `Summary`
reconciliation (15 s) while connected. Reconnects with backoff; state
(clocks, mirror) survives process restarts in the store, matching the
current "registrations restored at boot" semantics.

Drive status mapping (preserves the existing vocabulary): dialing /
hello in flight → `connecting`; connected + no open gaps → `synced`;
`paused` flag → `paused`; not connected, last seen > 60 s → `offline`;
hello rejected for version → `error`; token required/mismatched →
`requires-auth`; transport/protocol failure → `error`.

## Daemon, config, and contracts (`daemon.rs`, `config.rs`, `status.rs`)

- **Process model:** single instance (flock), daemonize, pidfile/ready
  handshake, signals, size-rotated logs — all kept. The **supervisor
  module and the entire `bootstrap/` tree (Node resolution, npm install,
  dist patch) are deleted**; `mcp.rs` and `registry.rs` go with them.
  What the supervisor's health window did (ready signal before the
  poller trusts the engine) becomes an in-process `EngineReady`
  future.
- **Config v2** (`config.json`, `version: 2`):
  `p2p { port: 4201, listen: "/ip4/0.0.0.0/tcp/{port}", mdns: true,
  announce: Option<String> }`, `drives[] { name, addr, peerId?,
  tokenEnv?, paused, availableOffline }`, `settings { host, port }`,
  `logLevel`, `instanceName` (advertised in `Hello`), flattened `extra`.
  One-time migration from v1: drive `url` values that parse as
  multiaddresses are moved to `addr`; the npm-only keys
  (`switchboard`, `node`, `registry`, `packages`) are dropped with a
  log line. `config set` dotted-key validation updated accordingly.
- **`StatusSnapshot` (unchanged shape):** `version` = crate version
  (bumped to **1.0.0** — the `status --json` contract is preserved, so
  the Omarchy plugin's ≥ 0.2.0 gate still passes); the `switchboard`
  block now reports the native engine (always `running`/`healthy` once
  the daemon is up; `version` = engine build, `port` = p2p listen
  port, `last_event` = latest sync event); `drives[]` as before with the
  status vocabulary above.
- **Settings page (9 routes, unchanged):** same paths, same bodies,
  same 202-fire-and-forget semantics — now backed by the in-process
  engine instead of MCP. `POST /api/drives` takes the peer's multiaddr
  as `url` (field name unchanged; the embedded page's hint text updated
  to multiaddr syntax).
- **`ph doctor`:** state dir + key, p2p stack bring-up on the listen
  port, per-drive dial/hello outcome, drive mirror statistics.

## State layout (v2)

```
<state>/
  config.json       0600 (v2)
  key               0600 ed25519 identity
  docs/             <doc_id>.log, <doc_id>.snap, index.json
  logs/reactor.log  size-rotated
  run/              pidfile, lock, ready
```
(`node/`, `switchboard/`, `data/` disappear; existing installs keep
those directories inert — removal is user-facing, documented, not done
silently.)

## Security

- Noise encryption on every connection; peers are identified by key.
- Ops carry an ed25519 signature from their origin; unverified ops are
  quarantined to `docs/quarantine/` and never applied.
- The sync listener is bound to the configured address (default all
  interfaces for p2p reachability) — a documented trade-off: token
  gates private drives; mDNS is LAN-only.
- No downloads, no package installation, no child processes, no env
  token values on disk.

## Tests

1. **Unit (core):** op application + vector-clock unknown-work rule;
   LWW convergence under permuted application order (property-style
   over random op sequences); fork/merge of two offline histories;
   signature verification (valid, wrong key, tampered); WAL
   append/snapshot/truncate/replay; name registry (uniqueness, NUL/`/`
   rejection); v1→v2 config migration (multiaddr `url` moved, npm keys
   dropped, unknown fields preserved).
2. **Unit (protocol):** Hello version/token matrices; Summary diff
   computation; response payload capping + resume.
3. **Integration (in-process swarms over loopback TCP):** two daemons —
   doc created on A appears on B via gossip; B disconnects, A adds
   three more, B rejoins → catch-up converges (Summary + Request);
   three-peer star converges to identical field maps; pause/resume;
   token mismatch → `requires-auth` and no content flows.
4. **End-to-end (two built binaries, separate state dirs):** daemon B
   has a local doc (`doc add`); daemon A does `drive add
   /ip4/127.0.0.1/tcp/<B-port>/p2p/<B-peerid>`; A's `doc get` shows B's
   doc; A mutates a field, B observes; `status --json` on both sides
   shows `synced`. This is the headline evidence.
5. **Contract:** `status --json` output on a daemon with a synced drive
   matches the `StatusSnapshot` schema (including the drive-status
   vocabulary) — the Omarchy plugin's reducer must accept it (run the
   plugin's `js/ReactorState.js` reducer over the new output as a
   cross-check).

## Verification plan

- `cargo test` (unit + integration) and `cargo clippy -- -D warnings`
  clean; release build succeeds.
- Two-binary E2E as above, on loopback; evidence recorded (commands,
  versions, SHAs) under `docs/superpowers/evidence/native-reactor/`.
- Cross-check: the ph-reactor-omarchy plugin's `tests/run` state-machine
  accepts the new `status --json` (fixture updated to the v2 shape if
  any field semantics moved — expected: none).
- Snap/CI: the existing release workflow and snapcraft.yaml keep
  working (static musl build now includes libp2p — build-time and
  binary-size impact recorded in the SDD; `quic` feature stays off in
  the snap to keep the static link clean).

## Deferred scope

DHT discovery; QUIC in the shipped snap; CRDTs; attachments; registry/
package concepts (no longer applicable to the native engine); removing
the dead `node/`, `switchboard/`, `data/` directories from existing
installs; Windows/macOS.

## Impact

| Area | Change |
|---|---|
| `Cargo.toml` | + `libp2p` (0.5x: tcp, noise, yamux, gossipsub, mdns; `quic` optional feature), + `ed25519-dalek` (or libp2p's identity), + `uuid`; − nothing forced (reqwest kept for the settings server/doctor); `rust-version` → 1.83 (libp2p MSRV); version → 1.0.0 |
| `src/` | + `doc.rs`, `store.rs`, `p2p/`; rewritten `daemon.rs`, `drives.rs`, `cli.rs`, `config.rs`, `status.rs`; deleted `bootstrap/`, `mcp.rs`, `registry.rs`, `supervisor.rs` (folded into daemon) |
| `README.md` | architecture section rewritten (one process, p2p drives, multiaddr syntax, token gates, security notes) |
| Omarchy plugin | contract unchanged; fixture/status cross-check in its test suite |

## Extension: document models and full P2P (v1.1)

This section supersedes the v1 "deferred scope" (DHT, registry-of-keys) with a
deliberate full-P2P design, and adds a model layer that turns the store from a
field-map sync engine into an auditable, model-governed document system.
Motivated by rescuing the open-source project as a scalable,
internet-connected network organization with distinct business processes.

### Design invariants

- **One mechanism.** Documents, groups, and models are all signed actions in
  replicated, hash-chained logs. There is no separate "directory": a group is
  a document, a model is a signed artifact, and membership is an action.
- **Name = sugar, key = identity, action = signed.** No capability, group
  membership, or doc state may be granted on the basis of a name. Names are
  cosmetic labels over peer IDs; security comes from ed25519 keys.
- **A model is a pure function.** `reduce(state, action) -> field-writes` has
  no I/O, clock, or RNG, so a document's log replays identically on every
  peer. State stays the v1 per-field LWW map; reducers are the audited gate in
  front of it.
- **A document is self-contained evidence.** It pins its model by
  `name@version[#hash]`; a doc plus its action log verifies anywhere, against
  exactly the model it was written under.

### The action envelope (`action.rs`, `doc.rs`)

`Action { doc_id, model: ModelRef, kind, payload, ts, clock, origin, cosig,
prev_hash, sig }`. `model` is `name@version[#hash]`. `sig` is the origin's
ed25519 signature over the canonical `message_bytes`, which — unlike the v1
`Op` — includes the vector clock, the model ref (and its pinned hash), the
payload, and the co-signers. `prev_hash` is the content hash of the previous
action in the doc's log, chaining it so any insert/delete/reorder is
detectable. `cosig` carries N-of-M co-signers for quorum-gated reducers. The
per-doc action log is the durable WAL; replay reduces each action to
field-writes and feeds them to the (unchanged) per-field LWW merge.

### Models (`model/`)

`Model` trait: `ref_`, `validate_payload`, `check_precondition`, `reduce`,
`check_state`. A `ModelRegistry` holds loaded models by `(name, version)`.

- **`open@1`** — the default and backward-compatible model: `set` / `delete`
  reduce 1:1 to the v1 field-writes, so a pre-model doc behaves
  byte-identically (a non-breaking rescue).
- **L1 (declarative)** — one JSON interpreter for all declarative models:
  `{ name, version, state: { fields }, reducers: { kind: { payload, writes,
  pre } } }`, interpreted over a fixed, auditable vocabulary (a small type set;
  write templates over `$actor`/`$ts`/`$payload.*`; a precondition DSL with
  `actor-in-group`, `field-is|not`, named checks, and `quorum`). No lambdas,
  no control flow, no I/O.
- **`group`** — a built-in L1 model: `members` / `managers` with
  `add-member` / `remove-member` / `add-manager` reducers. Membership is thus
  a signed, hash-chained, replicated log — time-versioned for free and
  `doc verify`-able like any document.

### Quorum (N-of-M)

A reducer's `pre` may include `{ "quorum": { "group": <group-ref-or-field>,
"min": N } }`: the action is valid only if N distinct valid co-signers
(`cosig`) are members of that group (and distinct from one another). This is
the primitive behind two-person rules, dual control, and N-of-M approval
committees — all data, all verified on replay.

### `doc action` / `doc verify`

- `doc action <name> <kind> --payload <json> [--cosign <peer> …]` — build,
  sign (plus co-sign), and publish an action through the normal op path.
- `doc verify <name>` — replay the action log: re-reduce every action, verify
  every signature (origin + co-signers), every precondition, the hash chain,
  and that the re-folded field map equals the stored one. Prints the
  human-readable audit report (the legal artifact).

### Full P2P (supersedes the v1 "no DHT" non-goal)

- **Identity:** the ed25519 peer ID (v1). Names are group-scoped, signed, and
  resolvable only by members — there is no global name namespace to squat.
- **Discovery:** mDNS (LAN) + a bootstrap list + Kademlia DHT (peer routing,
  provider records, name records). The bootstrap list is an address list, not
  a trust authority.
- **Connectivity:** relay v2 (any peer may run a relay, discovered via DHT
  provider records) + circuit for NAT traversal; direct hole-punching when
  NATs cooperate. Identity-only — no IP entry required.
- **TOFU pinning:** the first key seen for a peer is pinned; a later mismatch
  is flagged, never silently trusted.
- **Anti-spam:** closed topics (join only what a signed group admits you to),
  per-peer rate/relay quotas, and local ban lists (each peer is its own trust
  authority). Names carry no power, so squatting degrades only the cosmetic
  layer.

### Invite / join (one-shot handshake)

`invite` produces a one-shot string: the inviter's peer ID + a challenge
signed by their key + the group(s) being granted. `join <string>` responds
with the joiner's peer ID + a signed challenge. Both sides pin each other
(TOFU); the signed `group` doc records the membership; per-topic gossip
membership propagates. No directory; the string is single-use.

### Phasing

1. **Model core** (topology-independent; lands first): `open@1`, L1, group,
   quorum, the action-log store, `doc action` / `doc verify`.
2. **Full transport:** Kademlia + relay + bootstrap; action-aware wire;
   provider + group-scoped name records; TOFU.
3. **Invites:** one-shot handshake, per-topic membership, local ban lists.
4. **Web of trust / B2B** (horizon): orgs sign each other's group-manager keys.
