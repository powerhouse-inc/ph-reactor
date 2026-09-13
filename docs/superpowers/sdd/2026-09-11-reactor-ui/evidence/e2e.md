# End-to-end evidence

The daemon was started against a fresh state dir (`/tmp/ph-smoke`) with a minimal
config (p2p listener on a free loopback port, mdns/DHT/relay disabled, settings
server on `127.0.0.1:4312`) and the local vLLM (`http://127.0.0.1:8002/v1`,
model `qwen3.8-27b-w4a16`) configured as the LLM provider via the `llm` config
section (key read from the `PH_LLM_KEY` env var, set to a placeholder — vLLM
accepts any bearer). Every request below went through the daemon's JSON API.

## The redesigned console is the landing page

`GET /` returns the new self-contained console (`console/v2.html`): title
`Reactor`, the "Live activity" terminal, and the one-click reference processor.
The previous console is preserved at `/console`.

## Model registry

`GET /api/models` lists every seeded + registered type:

```
account@1  folder@1  group@1  invoice@1  open@1  project@1  task@1  transaction@1
```

## The reference processor fires

1. `POST /api/processors` with `{ name: "on-invoice-accepted", models: ["invoice"],
   field: "status", value: "accepted", reaction: { kind: "run", command: "echo paid" } }`
   → `{"ok":true}`.
2. `POST /api/docs` with `{ name: "invoice-42", model: "invoice", fields: { …,
   "status": "draft" } }` → `{"ok":true}` (bare `model` name resolved to
   `invoice@1`).
3. `POST /api/docs/action` with `{ name: "invoice-42", model: "invoice",
   kind: "set-status", payload: { status: "accepted" } }` → the action applies.
4. `GET /api/processors/on-invoice-accepted/fires` records the fire:

```json
{ "action": "set-status", "detail": "ran: echo paid (exit exit status: 0)",
  "doc": "invoice-42", "model": "invoice", "spec": "on-invoice-accepted",
  "ts": 1789322209456 }
```

The `run` reaction executed the command and recorded its exit status — exactly
the "invoice → accepted → run payment" behaviour the design calls for.

## Folders

`POST /api/folders` (`{ name: "invoices-2026", description: "Q2" }`) →
`{"ok":true}`. `POST /api/folders/invoices-2026/action`
(`{ kind: "add-member", payload: { member: "invoice-42" } }`) appends the doc.
`GET /api/folders` → `[("invoices-2026", ["invoice-42"])]`.

## LLM type drafting → register → create (the new-type loop)

Using the configured vLLM:

1. `POST /api/llm/draft-type` (`{ description: "a book: title, author, year,
   and a numeric rating" }`) → the model returned a well-formed definition:
   `name: book`, fields `[author, rating, title, year]`, reducers
   `[init, set-author, set-rating, set-title, set-year]`.
2. `POST /api/models/register` with that definition → `{ name: "book", ok: true }`.
3. `POST /api/docs` (`{ name: "book-1", model: "book", fields: { title: "Dune",
   author: "Herbert", year: 1965, rating: 5 } }`) → `{"ok":true}`.
4. `GET /api/docs/book-1` →
   `{ model: "book", fields: { author: "Herbert", rating: 5, title: "Dune",
   year: 1965 } }`.

The draft step retries (up to 3 times, nudging for complete JSON) because a
27B-class model occasionally truncates; the retries make the loop reliable.

## Multi-sig quorum (the two-person rule)

With a `core-team` group (members `alice, bob, carol`; the local node is a
manager):

- `POST /api/groups/core-team/action` `{ kind: "add-member", payload: { member:
  "dave" } }` → **200** (a single manager can add a member).
- `POST /api/groups/core-team/action` `{ kind: "add-manager", payload: { member:
  "eve" } }` → **400**:

```
quorum not met: 0 of 0 co-signers are members of 'core-team' (need 2). The
two-person rule: 2 distinct members *other than the proposer* must co-sign this
action — a single node cannot satisfy it by itself.
```

A single signer cannot add a second manager; the group's signed activity feed
(`GET /api/groups/core-team/activity`) lists the applied `init`/`add-member`
actions. This is the multi-sig guarantee, enforced by the model's quorum
precondition and checked by the store against the group's own membership.

## Negative / degraded paths

- `POST /api/llm/draft-type` with no LLM key set → graceful
  `{"ok":false,"error":"no LLM API key set …; use the manual type editor"}`.
- The LLM occasionally returns truncated JSON; the retry loop recovers and the
  endpoint only fails after 3 attempts (returning the raw text for inspection).

## Static checks

`cargo build --workspace` and `cargo clippy --workspace` are warning-free;
`cargo test --workspace` passes (84 lib tests + the views and integration
suites).
