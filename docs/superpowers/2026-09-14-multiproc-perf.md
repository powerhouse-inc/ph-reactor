# 2026-09-14 — Multi-process architecture & performance

Branch: `feat/multiproc` (worktree `~/.worktrees/ph-reactor-multiproc`).

Ticket: move toward a multi-process architecture and find the
highest-impact, well-isolated performance win.

## What the daemon's run model actually is

One tokio runtime (multi-thread) hosting four concurrent actors:

1. **The p2p engine task** (`SyncEngine::run`, `src/p2p/mod.rs`) — the
   single consumer of the libp2p swarm event stream (gossipsub +
   request-response handshake/catch-up), plus a 5-second tick that
   drains the store's outbound action queue and runs the proactive
   catch-up/summary exchange.
2. **The settings HTTP server** (axum, 127.0.0.1:4002,
   `src/settings/mod.rs`) — handlers mostly forward to the daemon's
   command channel via oneshot round-trips; they do not touch the
   store except for reads (`/api/query`).
3. **The daemon command/event loop** (`src/daemon.rs`) — executes user
   commands (store writes, engine commands), folds engine events into
   the status snapshot, re-polls `config.json` every 5 s.
4. **The processor runner** (`src/processor.rs`) — a background task
   consuming the store's doc-change feed.

There is **no switchboard (Node) subprocess in this tree** — the
`switchboard.log` reference in `src/logrotate.rs` is a leftover name.
So the only process already involved is the daemon itself; every other
concern above is a task.

## Where time actually goes (profiling)

Instrumented with `tests/multiproc_bench.rs` (release profile, this
machine):

| Path | Measurement (baseline, before the change) |
|---|---|
| Store write path: sign → verify → reduce → per-field merge → WAL append → change feed (1 subscriber) | **11,984 ops/s** (~83 µs/op) |
| Two-node convergence: `create_doc` on A → doc present in B's store | **5,005 ms / 4,992 ms / 5,014 ms** (avg 5.01 s, 3 rounds) |

Findings:

- **The store is not the bottleneck.** ~83 µs per fully-signed action
  (one ed25519 sign, one verify, JSON encode/decode, one WAL file
  open+write, one change-feed fan-out) is far above any realistic
  write rate. Note there is no `fsync` in the WAL at all — durability
  is page-cache level — so fsync batching was never the candidate.
- **The 5-second engine tick is the bottleneck.** The baseline
  convergence number is *exactly* `TICK_INTERVAL` (5 s), round after
  round: local actions sat in the store's `outbound` queue until the
  engine's next tick drained and gossiped them. The network + verify +
  apply path on loopback is a few ms. Every locally-authored doc paid
  a 0–5 s quantization (avg 2.5 s) on its first hop, before gossipsub
  could fan it out.
- **The engine event loop is the only serialization point worth
  caring about.** All swarm events (including every received action's
  verify + reduce + WAL append) are handled in one sequential task.
  Per received action that is tens of µs to ~1 ms of work; a 50-peer
  burst costs at most ~50 ms of event-loop time — real, but two
  orders of magnitude below the 5 s tick. A catch-up burst (64 actions
  per paginated response) is the one bursty case, and it is a
  rare rejoin event.
- The settings server, tray, and daemon loop do no heavy work on the
  hot path (command execution is a channel hop + the store call).
  No `block_on`/`spawn_blocking` misuse found.

## What was changed

1. **Immediate publish of local actions** (the win).
   - `src/store.rs:533` — `Store::connect_outbound()`: new unbounded
     feed. In `apply_action` (`src/store.rs:1005`), every applied
     local-origin action is sent to the feed the moment it is applied
     (under the store lock, so delivery order matches the log).
   - `src/p2p/mod.rs:425` — `SyncEngine::run()` connects the feed and
     adds a select arm: each feed action is published to the gossipsub
     topic immediately (`publish_action`, `src/p2p/mod.rs:459`).
   - The 5 s tick keeps draining `drain_outbound()` as a **backstop**
     (actions applied before the feed was connected); a bounded FIFO
     dedupe set of action content hashes (`PUBLISHED_CAP = 8192`,
     `src/p2p/mod.rs:63`) makes the overlap lossless.
2. **Fix: remote actions were being re-gossiped.** The old
   `outbound` queue received *every* applied action — local and
   remote — despite the doc comment saying local-only. Every peer
   re-published everything it received (bandwidth amplification, only
   masked by gossipsub's 15 s message-dedupe cache; a rejoining peer
   would re-gossip its whole catch-up). The queue/feed now carry
   local-origin actions only (`src/store.rs:1005`, test
   `outbound_feed_delivers_local_actions_immediately_not_remote`).
3. **Tick micro-cleanup**: `tick()` hoists `store.summary()` out of
   the per-drive loops (`src/p2p/mod.rs:763`) — was one full
   clock-map clone per drive, twice per tick (100 clones/tick at
   50 drives).

Deliberately **not** changed: the 30 s proactive catch-up cadence
(repair path only — it never limited measured convergence), the
per-`doc` WAL layout, the single-writer store mutex.

## Before / after

`tests/multiproc_bench.rs` (release profile, this machine; the
convergence figure includes a 20 ms poll in the waiter, so the true
network+apply latency is slightly lower):

| Metric | Before | After |
|---|---|---|
| Two-node convergence (create → peer's store), avg of 3 rounds | **5,005 ms** | **21.6 ms** |
| Store write throughput (500–1000 actions, 1 change-feed subscriber) | 11,984 ops/s | 12,248 ops/s (unchanged — the store path is untouched) |
| Full integration suite wall time | ~20 s | ~5 s |

Per-round after: 21.6 / 22.1 / 21.2 ms. That is a **~231× reduction**
in convergence latency; the remaining ~20 ms is loopback network +
gossip delivery + apply + the waiter's poll granularity.

Verification: `cargo build`, `cargo clippy --workspace --all-targets
-- -D warnings` clean; `cargo test --workspace` all green (96 lib
tests + dht/invite/two-engine sync + views e2e + the new bench).

## Honest assessment: is a multi-process split worth it?

**No — a true OS-process split of the p2p engine or the store is not
warranted for this daemon, and the profile says so.**

- The daemon already runs as a separate OS process per vault, with its
  own identity key and state dir — that is the isolation boundary that
  matters (a wedged vault cannot take down another vault or the host
  app).
- The work inside the process is either async I/O (network, file
  appends — no `fsync`) or short CPU bursts (ed25519, JSON) in the
  tens-of-µs-to-ms range. Nothing is slow enough to starve its
  siblings: the store does ~12k ops/s alone; the engine's event-loop
  stall under a 50-action burst is ~10–50 ms. A second process would
  add an IPC layer (serialization, backpressure, crash recovery) in
  front of a work pool that is already parallelized across tokio's
  worker threads — the store mutex is a deliberate single-writer
  design, so a separate process would not even add write parallelism.
- The actual bottleneck was **time quantization, not process
  boundaries**: a 5-second polling interval deciding when work happens.
  Event-driven delivery (the outbound feed) fixed it without any
  topology change. If the mesh ever grows to the point where one
  peer's catch-up burst measurably stalls handshakes (watch the
  50-peer harness: if event-loop stalls during catch-up show up in
  convergence tail latency, the follow-up is a dedicated store-apply
  worker *task* fed by a channel — a task, not a process).

One follow-up worth a future ticket (not done here, out of scope for
this fix): the WAL has no `fsync`, so "durable" means "in the page
cache." That is a durability design decision, independent of the
latency work above.
