# Reactor v2 — Consolidation & Verification

Date: 2026-09-14. Branch: `feat/reactor-v2` (`main` + four workstreams, merged).

## What this is

Four independent workstreams, each developed in its own worktree, consolidated
into one branch and verified end-to-end. They map to the requested
improvements:

| # | Request | Workstream | Key doc |
|---|---------|-----------|---------|
| 1 | 50-peer sync: does group-chat work with 50 peers, any conflicts? | `sync-scale` | `2026-09-14-sync-scale.md` |
| 2 | Group = shared space with public/private channels + a folder drive | `group-ui` (model) | `2026-09-14-group-shared-space-design.md` |
| 3 | Make the reactor more efficient (multi-process) | `multiproc` | `2026-09-14-multiproc-perf.md` |
| 4,5 | Audit p2p for security; anything else to improve? | `audit` | `2026-09-14-security-improvements-audit.md` |
| 6 | Group UI: channels + folder tree + a per-model rich editor | `group-ui` (console) | `2026-09-14-group-shared-space-design.md` |
| 7 | Fix the sidebar (home/groups/settings/profile) + peers shows 1 | `group-ui` (console + store) | `2026-09-14-group-shared-space-design.md` |

## The changes

- **Group shared space (2, 6, 7).** `group@1` grows two permissive `array`
  fields: `channels` (objects `{name, visibility: public|private, members}`)
  and `drive` (objects `{name, kind: folder|doc, model, parent}` — a
  Google-Drive-like tree). The console gains a four-item sidebar
  (Home / Groups / Settings / Profile) with a new Profile view, a sub-tabbed
  group space, and a per-field editor whose widgets are derived from the model
  definition (text / number / checkbox / dropdown-from-`enums` / add-remove
  list / JSON) and which dispatches the model's real `set-*` reducers so edits
  sync over the mesh. `/api/models` now exposes each model's `enums`.
- **Private-channel authorization.** A group `post` to a `private` channel is
  gated by a **model-declared `auth` block** that the **store runs on its
  single apply path** (`model.authorize`, after the signature check, before
  reduce). This is the exact case a plain top-level `actor-in` cannot express
  (the allow-list lives inside a nested `channels` array entry). Public
  channels are not auth-gated (the group-member precondition suffices).
- **Never-zero peer count (7).** `overview.peerCount` counts the node itself
  (`known_peers().len() + 1`); `Store::known_peers()` excludes the local
  origin so a remote peer is never double-counted. A lone node shows `1`.
- **Multi-process convergence (3).** The store feeds the sync layer an
  outbound channel of **local** actions and publishes them **immediately**
  (no poll-interval wait); remote actions are not re-gossipped (origin
  gate). `connect_outbound` hands the receiver to the p2p engine. Convergence
  latency for a two-node document drops from ~5005 ms to ~21.6 ms (**~231x**)
  in the `multiproc_bench` test.
- **50-peer convergence test (1).** `fifty_concurrent_origins_converge_regardless_of_order`
  (in `src/doc.rs`) applies 50 origins' 100 concurrent ops (distinct fields +
  a shared `counter`) in 10 different orders and asserts an identical final
  doc in every order (no divergence), all distinct writes retained, and the
  same-field conflict resolving to one deterministic LWW `(ts, origin)` winner.
  `same_ts_same_field_breaks_by_origin` pins the origin tie-break. **Result:
  group-chat is conflict-safe at 50 peers** — the per-field CRDT merge is
  commutative and idempotent, so convergence is a property of the op set, not
  the delivery order; same-field writes resolve by design (last-writer-wins),
  and concurrent channel appends do not collide (each gets its own position).
- **Security fix (4, 5).** `POST /api/llm/test` accepted a per-request
  `baseUrl`; `llm_base_url_ok` now refuses link-local `169.254.0.0/16`
  (the cloud metadata range) before any request, closing an SSRF /
  credential-exfiltration path. The full audit (`...security-improvements-audit.md`)
  ranks the remaining findings: the design is sound (one ed25519 identity,
  secrets as env-var names, TOFU-pinned keys, signed gossip, declarative
  models, hash-chained docs); the open items (default `0.0.0.0`+DHT+no-token
  first-contact TOFU, non-expiring / reusable invites, the unauthenticated
  loopback settings API, and the no-fsync WAL) are documented with
  recommendations rather than changed, to avoid breaking first-run setup.

## Verification (combined branch `feat/reactor-v2`)

- `cargo clippy --workspace --all-targets -- -D warnings` → **clean**.
- `cargo test --workspace` → **all green**: 107 lib tests (incl. the 50-peer
  convergence test, the private-channel auth test, the multiproc
  outbound-feed tests, and the store TOFU/signature/quarantine/replay tests)
  plus the e2e suite (`two_engine_sync`, `invite_join`, `dht_discovery`,
  multi-client convergence) and the views/doc tests.
- `cargo test --test multiproc_bench` → convergence-latency bench **passes**.
- **Running-daemon E2E** (isolated state dir + ports 14002/14201, so the
  user's live daemon on 4002/4201 is untouched):
  - `GET /api/overview` → `peerCount = 1`, `knownPeers = []`, sync engine
    running (lone node is never zero).
  - `POST /api/groups` → 200 (creator auto-added as member+manager).
  - `POST /api/groups/core/channels` → 200 for a public channel, a private
    channel listing the local peer, and a private channel listing a stranger.
  - `POST /api/groups/core/action` (`kind: post`) → 200 to `general` and to
    the self-owned `insiders`; **400** to `sec` with *"actor … is not a
    member of the private channel 'sec'"*; the group's `msgChannel` records
    only the accepted posts.
  - `POST /api/groups/core/drive/folder` → 200; `GET /api/groups/core` shows
    the channel set and drive.
  - `GET /api/models` → model catalog with fields/`enums`/reducers.
  - `POST /api/llm/test` with `baseUrl` `http://169.254.169.254/...` →
    **400** *"refusing LLM request to a link-local (cloud metadata) address"*.

## Result

All seven requested improvements are implemented, merged, and verified
end-to-end on `feat/reactor-v2`. The daemon's core (the event-sourced
store, libp2p drive sync, status-bar tray, and loopback console) is
unchanged and intact; the new group shared space, the multi-process
convergence speedup, and the closed LLM-SSRF path are live.
Ready to merge to `main`.
