# Reactor Console v2 (the group-centric experience) — Design

## Problem

Console v1 is organized around daemon subsystems: Overview / Drives / Groups /
Processors / Documents / Settings. It works, but the object a user actually
cares about — their **groups and documents** — is buried among subsystem
read-outs. Concretely it cannot:

- show a group's **members and each member's drive**, the folders inside it,
  and the documents in those folders;
- create a **typed** document (one of the real model types), or **create a new
  type** with LLM help (describe → draft → review → approve);
- let a user **define a processor** as a subscription on document changes
  (by type, by action, by field/value change) and see it fire;
- look like a product a customer would be shown — the visual design is flat.

## Goals

1. **Group-centric navigation.** The sidebar is the home object: the connected
   identity (peer + sync health) at the top, the user's **groups** listed
   below, a **New group** action, and a secondary **System** section
   (Overview / Sync drives / Documents / Settings). The subsystem views move
   under System; groups come forward.
2. **Group view → members' drives.** Opening a group shows its **members**;
   each member has a **drive** (their document space within the group). A
   drive contains **folders** and **documents**. A *drive* here is a member's
   document namespace — distinct from the network *drives* (the P2P sync
   peers), which live under System.
3. **Drive view → folders + documents.** A member's drive lists folders
   (derived from a document's optional `folder` field) and the documents in
   each.
4. **Document view.** A document shows its model, its fields (rendered), and
   its activity; it can be edited and deleted.
5. **Create a document — two paths.**
   - **Choose a type** (an existing document model) and fill its fields.
   - **New type with the LLM**: describe what you need; the daemon asks the
     LLM to draft a model definition (name + fields); you **review** the draft
     and **approve** it, which registers the type; you then create documents of
     it. With no LLM configured, a manual type editor is offered instead.
6. **Processors as subscriptions.** A processor is a named subscription: a set
   of document types (empty = any), an optional **action kind**, an optional
   **field → value** change. On a match it runs a **reaction** (log / run a
   command / emit an event / create a document). The reference example: listen
   to `invoice`, and when `status` changes to `accepted`, run a payment
   command. Create, edit, remove processors from the UI; see their fire
   history.
7. **A polished, impressive design.** A modern design system (dark + light):
   gradient accents, glass cards, Inter typography, smooth transitions,
   semantic status chips, strong empty states. It should read as a serious
   product, not an admin form.

## Non-goals

- Loopback authentication (the console stays bound to 127.0.0.1).
- A separate published frontend package (the SPA stays embedded in the binary).
- Editing a document's model after creation (a document is pinned to the
  model it was created under).
- Multi-node co-signing from the UI (the two-person rule is surfaced as a
  clear error, as today).

## Information architecture

```
Console (hash-routed SPA, loopback)
├── Sidebar
│   ├── Identity        — peer id, "connected", sync health
│   ├── Groups          — the user's group documents, + "New group"
│   └── System          — Overview / Sync drives / Documents / Settings
├── Group view          #/groups/:name
│   ├── Header          — name, member count, manager count, status
│   ├── Members         — each member → its drive (folder chips, doc count)
│   ├── Documents       — all docs across members, filterable
│   ├── Membership      — add/remove member, add manager
│   └── Activity        — the group's signed actions
├── Drive view          #/groups/:name/drives/:member
│   ├── Folders         — grouped by the doc's folder field
│   └── Documents       — rows/cards → Document view
├── Document view       #/groups/:name/docs/:doc
│   ├── Fields          — the model's fields, rendered
│   ├── Edit / Delete
│   └── Activity
├── New document        #/new (owner + folder + type or LLM-drafted type)
│   ├── Choose type     — the registered document models
│   ├── New type (LLM)  — describe → draft → review → approve → register
│   └── Fields          — create
├── Processors          #/processors
│   ├── List            — name, types, filter, reaction, last fired
│   ├── Create / edit   — types, action, field, value, reaction
│   └── Fire history
└── System
    ├── Overview        — reactor health, counts, recent activity
    ├── Sync drives     — the P2P peers (add/remove/pause, status)
    ├── Documents       — every document, filterable by model
    └── Settings        — instance, network, LLM, logging
```

## Backend changes

### The doc-change feed carries the action delta

`DocChange` gains three `#[serde(default)]`-style (in-memory) fields,
populated from the applied action's payload in the `apply_action` notify path:

- `action_kind: Option<String>` — the action's reducer kind
  (`create` / `set` / `delete` / a model reducer such as `accept`).
- `action_field: Option<String>` — the payload's `field`, if present.
- `action_value: Option<Value>` — the payload's `value`, if present.

Read models ignore them (backward compatible). This is what lets a processor
match a *transition* (`status` → `accepted`) rather than merely a state.

### Processors as subscriptions

- `ActionFilter { models, action_kind, field, value }` with
  `matches(&DocChange)`: `models` empty = any; the other constraints are ANDed
  against the change's delta (all specified must match).
- `Processor::filter(&self) -> ActionFilter` (default: the model list only, so
  existing implementations are unchanged). The dispatcher applies the filter
  before enqueuing a job.
- `ProcessorSpec` — persisted in `processors.json`:
  `{ name, models, action_kind?, field?, value?, reaction }`.
- `Reaction` — `Log { message }` | `Run { command }` | `Emit { event }` |
  `CreateDoc { model, fields }`. `Run` executes a local command with a timeout
  (the console is trusted/loopback; this is documented).
- `ConfiguredProcessor` — a `Processor` whose `filter()` returns its spec's
  filter and whose `on_change` performs the reaction, logging each fire to a
  bounded history (exposed via the API).
- The manager loads specs at startup and hot-reloads `processors.json` on
  change, so a spec created from the UI takes effect without a restart.

### LLM type drafting and the model registry

- `GET /api/models` — the registered document-model types (name, version,
  fields) for the create-document picker.
- `POST /api/llm/draft-type` `{ description }` — calls the configured
  OpenAI-compatible endpoint and returns a **draft** model definition
  (`{ name, version, fields: [...] }`) for review. With no key configured it
  returns a clear error (the UI falls back to the manual editor).
- `POST /api/models/register` `{ definition }` — validates the definition and
  registers it as an `L1` interpreter in the model registry, making the type
  usable for new documents.
- `invoice@1` is added to the realistic models (number, customer, total,
  currency, and a `status` lifecycle `draft → sent → accepted → paid`) so the
  reference processor example is real.

### Folders and group documents

- A document carries an optional **`folder`** field (a standard field the
  create flow offers). A drive's folders are the distinct `folder` values among
  its documents.
- `GET /api/groups/:name/docs` — the group's documents grouped by owner
  (member), so the group view can render each member's drive; `GET /api/docs`
  gains `?owner=` and `?folder=` filters.

## API surface (new / changed, on the loopback server)

| Method | Path | Purpose |
|---|---|---|
| `GET`  | `/api/models` | registered document types (name, version, fields) |
| `POST` | `/api/models/register` | register a new type from a definition (LLM or manual) |
| `POST` | `/api/llm/draft-type` | draft a type definition from a description |
| `GET`  | `/api/processors` | the persisted processor specs + last-fired per spec |
| `POST` | `/api/processors` | create a processor spec |
| `PUT`  | `/api/processors/:name` | edit a processor spec (hot-reloads) |
| `DELETE` | `/api/processors/:name` | remove a processor spec |
| `GET`  | `/api/processors/:name/fires` | the spec's recent fire history |
| `GET`  | `/api/groups/:name/docs` | the group's documents grouped by owner + folder |
| `GET`  | `/api/docs` | unchanged + `?owner=` / `?folder=` filters |

(`/api/status`, `/api/groups`, `/api/docs`, `/api/drives*`, `/api/llm/test`,
`/api/config`, `/api/query`, the group-action route, and the invite/join/ban
routes are unchanged.)

## The design system

A single embedded stylesheet, dark-first with a light theme, on the existing
shadcn-style token base (`--background`, `--card`, `--primary`, …). The v2
look adds:

- **Accent gradient** (violet → cyan) for primary actions, the active nav
  item, and status emphasis.
- **Glass cards** — translucent surfaces with a soft border and depth shadow,
  rounded `12px`, a subtle entrance fade/translate on view change.
- **Typography** — Inter (system fallback), a clear scale (12/13/15/18/24/32),
  generous line-height, tracked-out section labels.
- **Status chips** — pill chips with a semantic dot (synced/ok = green,
  connecting = amber, paused = grey, error = red, requires-auth = violet).
- **Micro-interactions** — hover lift on rows/cards, focus rings, a toast with
  a slide-in, a skeleton shimmer for loading states.
- **Icons** — a small inline-SVG set (group, drive/folder, document, processor,
  settings, plus) so the sidebar and views are icon-led.

## Security

Loopback-only. The LLM key is read from the environment (the name is stored,
the value never is). Processor `Run` reactions execute local commands with a
timeout and are a documented, trusted-local capability — never reachable from
the network.

## Tests / verification

- **Unit:** `DocChange` delta population from `create`/`set`/`delete`;
  `ActionFilter::matches` across the constraint combinations; `ProcessorSpec`
  round-trip + hot-reload; the LLM draft-type call against a local stub
  endpoint; model registration (a registered type creates a doc); the `invoice`
  reducers; group-docs grouping by owner.
- **Integration:** the reference processor — an `invoice` whose `status` is set
  to `accepted` triggers the `Run` reaction exactly once (and does not fire for
  other fields/actions). A LLM-drafted type (from the stub) can create
  documents.
- **Build:** `cargo build` + `cargo clippy --all-targets -- -D warnings` +
  `cargo fmt --check` clean.
- **Live (browser):** create a group → add a member → create a typed document
  (and a folder) → see it in the member's drive → draft + approve a new LLM
  type → create an invoice processor → set an invoice `status` to `accepted`
  → watch the processor fire. The console renders cleanly with no browser
  console errors. Screenshot evidence captured for the SDD.

## Impact

| Area | Change |
|---|---|
| `src/store.rs` | `DocChange` +3 fields; populated in the `apply_action` notify |
| `src/model/realistic.rs` | + `invoice@1` (status lifecycle + reducers) |
| `views/src/processor.rs` | `ActionFilter`, `Processor::filter()`, `ConfiguredProcessor`, spec persistence + hot-reload |
| `src/settings/mod.rs` | new routes (models, processors, llm/draft-type, group docs) + the LLM client |
| `src/daemon.rs` | wire the processor manager to the persisted specs; pass the paths |
| `src/settings/console.html` | full redesign: the v2 IA + the polished design system |
| `README.md` | a console-v2 section (the IA, the LLM flow, processors, the API) |
