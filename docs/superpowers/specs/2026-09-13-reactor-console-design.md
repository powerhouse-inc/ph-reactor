# Reactor Console (the user interface) — Design

Issue: none yet (new user interface; replaces the embedded single-page
settings page).

Design references: `@powerhousedao/connect` (`apps/connect`) and
`@powerhousedao/reactor-browser` (`packages/reactor-browser`) — the connect
settings modal's AI-endpoint tab, the shadcn-style design tokens, and the
reactor-browser inspector views (remotes / queue / processors / worker).

## Problem

The native reactor daemon (`ph-reactor`) ships a single flat loopback page:
a reactor-status banner, a drives table with an add form, and a ban list.
There is no place to:

- configure the LLM (OpenAI-compatible) endpoint the reactor uses;
- see every document the vault holds, or open one and read its fields;
- create and manage groups, or see what is happening inside them;
- see the daemon's local processing (its background subsystems) at a glance.

The page is a single scroll; it cannot scale to the number of concerns a
user now manages, and it exposes none of the reactor's richer surface (groups,
documents, the document-model layer) that the TypeScript Reactor and connect
present.

## Goals

1. **One console.** Replace the single page with a client-side-routed control
   panel — the "reactor console" — served by the daemon on its loopback port.
   A persistent **left sidebar** (Overview / Drives / Groups / Processors /
   Documents / Settings) plus a header (instance name, peer id, version,
   health) and a footer (theme switch, settings URL). A **theme switch**
   (system / light / dark) persisted in `localStorage`, defaulting to dark.
2. **Configuration.** A Settings view editing the whole config: instance
   (name, listen), network (mDNS / DHT / relay toggles, the auth token
   env-var name, bootstrap peers), logging (level), server (read-only), and a
   new **LLM (OpenAI-compatible)** section: `baseUrl` (default
   `https://api.openai.com/v1`), `apiKeyEnv` (the environment variable that
   holds the key — the value is never written to disk), `model` (default
   `gpt-4o-mini`), and a **Test connection** button that round-trips the
   endpoint (`GET {baseUrl}/models` with a Bearer key read from the
   environment) and reports the result.
3. **Drives.** Add / remove / pause / resume / resync drives; every configured
   drive shown with its live status (`synced | connecting | paused | offline |
   requires-auth | error`), address, and detail. The ban list moves here.
4. **Groups.** Create a group (name, initial members, managers); list every
   `group` document with its members and managers; open a group to see its
   current membership and its recent activity (the signed actions in the
   group's log), and add a member / remove a member / add a manager.
5. **Processors.** A live read of the daemon's local processing layer — its
   background subsystems (the p2p sync engine, the document store, the
   settings server, log rotation, the status poller) — plus a recent-activity
   feed of document changes.
6. **Documents.** Browse every document in the local store (filterable by
   model and `field=value`), open one to read its fields, and create a new
   `open@1` document.

Non-goals: a separate published frontend package (the console ships embedded
in the daemon binary; its HTML/JS/CSS is structured to be extracted later);
authentication for the loopback UI (bound to 127.0.0.1, as today); new
document models (the console drives the existing `open@1` / `group` / L1
models); a streaming AI chat (that is the reactor-browser's job, not the
daemon's); a mobile-optimized layout (desktop control panel; responsive
enough not to break on a narrow window).

## Console architecture

The daemon's existing axum settings server (loopback, 127.0.0.1) serves:

- `GET /` — the console SPA: a single self-contained HTML document
  (markup + CSS + vanilla JS — no framework, no build step, embedded in the
  binary like today's `PAGE`). Client-side routing by `location.hash`
  (`#/overview`, `#/drives`, ...); the active view is the only content
  rendered. The SPA polls `/api/status` (and the read endpoints of the active
  view) on a short interval and issues mutations to the API. Every mutation
  goes through the daemon's single-writer command channel (unchanged), so page
  actions never race the poller or the tray.

The SPA is dependency-free vanilla JS + hand-rolled CSS that mirrors the
connect/reactor-browser design system:

- **Font:** Inter, with a `system-ui` fallback stack.
- **Theme tokens:** the shadcn-style CSS variables (`--background`,
  `--foreground`, `--card`, `--card-foreground`, `--muted`,
  `--muted-foreground`, `--border`, `--ring`, `--primary`, and the semantic
  `--info / --success / --warning / --destructive`), defined for both light
  and dark; the current dark palette is preserved as the dark theme.
- **Components:** cards, chips (the existing status chip vocabulary is kept),
  tables, forms, buttons (incl. a destructive variant), and a toast
  (the existing toast is kept) — all from a small shared stylesheet.

## The views

**Overview (landing).** The reactor health card (running / healthy, peer id,
listen, doc count, last event); a drives summary (counts by status + the
drives list); the LLM endpoint status (configured? last test result?); and a
recent-activity feed (the most recent document changes). "What is happening
right now."

**Drives.** A table: name, address, status chip, detail, per-drive actions
(resync, pause/resume, remove). An "add a drive" form (multiaddr, optional
name, optional token env, offline). The ban list (banned peers + ban/unban)
lives here — it is peer hygiene.

**Groups.** A list of every `group` document: name, member count, manager
count, member/manager ids. A "create group" form (name, initial members,
managers). Open a group: current members + managers, its recent activity
(the signed actions: `init` / `add-member` / `remove-member` / `add-manager`,
with actor + time), and actions to add a member / remove a member / add a
manager. The two-person rule is enforced by the model; a failed quorum is
surfaced as a friendly error.

**Processors.** Subsystem cards, each a live read:

- **Sync engine** — up / healthy, configured drive count, currently-active
  drives (synced + connecting), last sync event.
- **Document store** — live doc count, last applied document.
- **Settings server** — the loopback URL it is bound to.
- **Log rotation** — log file path, current size, rotation count.
- **Status poller** — refresh interval, last refresh time.

A recent-activity feed (the daemon's processing of document changes) is shown
alongside.

**Documents.** A list of every document: name, model, updated time, a
one-line summary. Filterable by model (a dropdown) and by `field=value`. Open
a document: its fields (name → value) and its model. A "new document" form
(name, model, fields as `k=v`) creates an `open@1` document.

**Settings.** The full config, grouped: **Instance** (name, listen),
**Network** (mDNS / DHT / relay toggles, the auth token env-var name,
bootstrap peers), **LLM** (baseUrl, apiKeyEnv, model, Test connection),
**Server** (settings host / port — read-only; a change requires a restart),
**Logging** (level). A save per group issues the dotted-key config sets and
reports which were applied. The theme switch is in the sidebar footer.

## New config (the LLM endpoint)

`config.json` gains an `llm` section. The schema stays v2 and the section is
`#[serde(default)]`, so existing configs are unaffected (the flattened `extra`
map already preserves unknown keys):

```json
"llm": {
  "baseUrl": "https://api.openai.com/v1",
  "apiKeyEnv": "LLM_API_KEY",
  "model": "gpt-4o-mini"
}
```

`baseUrl` is the OpenAI-compatible base ending in the version prefix
(`…/v1`). `apiKeyEnv` is the *name* of the environment variable holding the
key — the value is never written to disk (the same `tokenEnv` convention the
drives already use). `model` is the model id. The endpoint is exercised by
`POST /api/llm/test`, which reads the key from the environment and issues
`GET {baseUrl}/models` with a Bearer header, reporting reachable / the model
list / the error.

## New API routes (on the existing loopback server)

| Method | Path | Purpose |
|---|---|---|
| `GET`  | `/api/config` | the current config (for the Settings form) |
| `POST` | `/api/config` | set a dotted key (existing, unchanged) |
| `POST` | `/api/llm/test` | round-trip the configured endpoint; report the result |
| `GET`  | `/api/processors` | the daemon's live subsystems + recent activity |
| `GET`  | `/api/groups` | list `group` documents (name, members, managers) |
| `POST` | `/api/groups` | create a group (an `init` action) |
| `POST` | `/api/groups/:name/action` | a group action (add/remove member, add manager) |
| `GET`  | `/api/groups/:name/activity` | the group's recent signed actions |
| `GET`  | `/api/docs` | list all documents (name, model, fields); `?model=&field=&value=` |
| `GET`  | `/api/docs/:name` | one document's full state (fields, model, stamps) |
| `POST` | `/api/docs` | create a document (existing, unchanged) |
| `GET`  | `/api/status` | the shared snapshot (existing; two `#[serde(default)]` fields added) |

(`/api/drives*`, `/api/ban`, `/api/unban`, `/api/invite`, `/api/join`,
`/api/query`, `/api/quit` are unchanged.)

## Status snapshot additions

Two `#[serde(default)]` fields are added so the console can show them without
a new poll: the sync engine's **active drive count** and the **last applied
document name** (both already tracked by the daemon's context). The
Omarchy-consumed fields and the drive-status vocabulary are untouched
(backward compatible).

## Security

The console is bound to 127.0.0.1 (loopback only) — the same trust model as
today's settings page. The LLM API key is never stored (env-var name only).
No new external exposure.

## Tests / verification

- **Unit (Rust):** the `llm` config set / parse + dotted-key validation;
  `GET /api/config` returns the config; `GET /api/groups` lists group docs;
  `POST /api/groups` creates a group and it appears; a group action changes
  membership; `GET /api/docs` lists documents; `GET /api/processors` reports
  the subsystems; `POST /api/llm/test` against a local stub server returns
  the model list; a bad endpoint returns an error, not a panic.
- **Build:** `cargo build` + `cargo clippy --all-targets -- -D warnings` +
  `cargo fmt --check` clean.
- **Live smoke (browser):** launch the daemon on a throwaway state dir, open
  the console at its loopback URL, and drive every view — the Overview shows
  the reactor; add a drive and watch its status; create a group, add a
  member, see the activity; list documents, open one; set the LLM endpoint
  and hit Test against a local stub. The console renders cleanly with no
  browser-console errors. Screenshot evidence captured.
- **Contract:** `status --json` still satisfies the Omarchy-plugin shape check
  (the new fields are `#[serde(default)]`).

## Impact

| Area | Change |
|---|---|
| `src/config.rs` | + `llm` section (`baseUrl`, `apiKeyEnv`, `model`), `#[serde(default)]`; dotted-key validation for the new keys |
| `src/status.rs` | + two `#[serde(default)]` snapshot fields (active drives, last applied doc) |
| `src/daemon.rs` | populate the new snapshot fields in `refresh_status`; pass the state paths into `Settings` |
| `src/settings/mod.rs` | the `PAGE` constant replaced by the console SPA; + the new routes (config / processors / groups / docs / llm); `Settings` gains the paths it needs to answer them |
| `README.md` | a console section (the views, the LLM endpoint, the API surface) |
