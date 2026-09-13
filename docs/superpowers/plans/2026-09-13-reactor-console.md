# Reactor Console — Implementation Plan

Spec: `../specs/2026-09-13-reactor-console-design.md`.
Worktree: `~/.worktrees/ph-reactor-ui`, branch `feat/ui` (from `main` =
07d2000). Each task is one commit; tasks build in order.
Machine: rustc (see `rust-toolchain.toml`), axum 0.7, headless Linux.

## Task 1 — LLM config section (`src/config.rs`)

Add an `LlmConfig { baseUrl, api_key_env: Option<String>, model }` to the v2
config (`#[serde(default)]`, camelCase `llm`): `baseUrl` defaults to
`https://api.openai.com/v1`, `apiKeyEnv` defaults to `LLM_API_KEY`, `model`
defaults to `gpt-4o-mini`. Dotted-key validation in `config::set` for
`llm.baseUrl` (a URL), `llm.apiKeyEnv` (an env-var name, or empty/null to
clear), and `llm.model` (a non-empty model id). Acceptance: unit tests —
defaults, `set` on each key, validation rejects a bad URL / empty model, the
section round-trips through `render`/`load` and is absent from a v2 config
that never set it.

## Task 2 — Snapshot additions + daemon plumbing

`status.rs`: add `#[serde(default)]` fields to `ReactorStatus` —
`active_drives: usize` and `last_doc: Option<String>` (active drives = drives
in an active state; last doc = the most recently applied document name).
`daemon.rs`: compute them in `refresh_status` (from `drive_views` and a new
`last_doc` the daemon tracks on apply) and pass the `StatePaths` into
`Settings::new` so its handlers can read the config and the log file.
Acceptance: the existing `status --json` contract tests still pass (the new
fields are `#[serde(default)]`); a new test asserts the two fields appear and
are correct for a daemon with a synced drive.

## Task 3 — Read API (`src/settings/mod.rs`)

Add the read routes, each backed by the store/config/snapshot the `Settings`
state already holds (or now receives):

- `GET /api/config` — the current `ReactorConfig` (camelCase).
- `GET /api/processors` — the five subsystem cards (sync engine, doc store,
  settings server, log rotation, status poller) built from the latest
  snapshot + a fresh config read + the log file's size; plus a
  `recent` activity feed (last N applied docs).
- `GET /api/groups` — `query_docs(store, "group", None)` shaped to
  `{ name, members, managers, memberCount, managerCount }`.
- `GET /api/docs` — `query_docs(store, model?, filter?)` (the existing
  projection, now surfaced for the console).
- `GET /api/docs/:name` — one document's full state (fields, model, stamps)
  from the store.
- `POST /api/llm/test` — read the key from `env(apiKeyEnv)`, issue
  `GET {baseUrl}/models` with a Bearer header (falling back to
  `{baseUrl}/v1/models`), and report `{ ok, model, baseUrl, models?: [..],
  error?: .. }`. A missing key or unreachable endpoint is a structured
  error, never a panic.

## Task 4 — Group + doc mutations (`src/settings/mod.rs`)

- `POST /api/groups` — build a `group` `init` action (name, members,
  managers) and apply it through the same one-shot path `add_doc` uses.
- `POST /api/groups/:name/action` — apply a group reducer action
  (`add-member` / `remove-member` / `add-manager`) through the `action_doc`
  one-shot path; a quorum failure returns a friendly error.
- `GET /api/groups/:name/activity` — the group document's recent actions
  (kind, actor, time) from the store's action log.
Acceptance: a group created via the API appears in `GET /api/groups`; an
`add-member` action changes its members; a failed `add-manager` (quorum)
returns the model's rejection.

## Task 5 — The console SPA (`src/settings/mod.rs`)

Replace the `PAGE` constant with the console: the shadcn-style token
stylesheet (light + dark, Inter), the sidebar shell (header with instance /
peer / version / health; nav Overview / Drives / Groups / Processors /
Documents / Settings; footer theme switch + settings URL), hash routing, and
the six views (Overview, Drives, Groups, Processors, Documents, Settings).
The SPA polls `/api/status` + the active view's read endpoints, renders the
views, and wires the mutations (drive add/pause/resume/resync/remove, ban/
unban, group create/add-member/remove-member/add-manager, doc create, config
set, LLM test, theme switch). Dependency-free vanilla JS; a shared `get` /
`post` / `toast` / `chip` / `esc` helper set (kept from the current page).

## Task 6 — Evidence + SDD + README

`cargo build` + `cargo clippy --all-targets -- -D warnings` + `cargo fmt
--check` clean; full `cargo test -p ph-reactor -p ph-reactor-views` green.
Live smoke: daemon on a throwaway state dir + a local stub LLM server; open
the console and drive every view (screenshot evidence). SDD reports per
task; evidence under `docs/superpowers/evidence/reactor-console/`; README
console section.
