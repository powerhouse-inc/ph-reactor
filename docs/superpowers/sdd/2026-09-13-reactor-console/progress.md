# Reactor Console — SDD Progress

Spec: `../../specs/2026-09-13-reactor-console-design.md`. Plan:
`../../plans/2026-09-13-reactor-console.md`.

Worktree `~/.worktrees/ph-reactor-ui`, branch `feat/ui` (from `main` =
07d2000). The new user interface for the native reactor: a client-side-routed
console (Overview / Drives / Groups / Processors / Documents / Settings)
served on the daemon's loopback port, with a new LLM (OpenAI-compatible)
config section and the read/mutation API it drives.

| Date | Task | Status | Notes |
|---|---|---|---|
| 2026-09-13 | 1 — LLM config section (`config.rs`) | done | `LlmConfig { baseUrl, apiKeyEnv, model }`, `#[serde(default)]`; dotted-key validation; unit tests. |
| 2026-09-13 | 2 — Snapshot + daemon plumbing | done | `ReactorStatus` gains `active_drives` + `last_doc` (both `#[serde(default)]`); `daemon.rs` populates them in `refresh_status`; `Settings` receives the state paths. |
| 2026-09-13 | 3 — Read API (`settings/mod.rs`) | done | `/api/config`, `/api/processors`, `/api/groups`, `/api/docs`, `/api/docs/:name`, `POST /api/llm/test`. |
| 2026-09-13 | 4 — Group + doc mutations | done | `POST /api/groups` (an `init` action), `POST /api/groups/:name/action` (add/remove member, add manager), `GET /api/groups/:name/activity`. The two-person rule enforced by the model; quorum failures surfaced. CLI `group` verb added. |
| 2026-09-13 | 5 — The console SPA | done | shadcn-style light/dark stylesheet + Inter; sidebar shell; hash routing; the six views; the shared `get`/`post`/`toast`/`chip`/`esc` helpers. |
| 2026-09-13 | 6 — Evidence + SDD + README | done | `docs/superpowers/evidence/reactor-console/e2e.md`; README console section; build/clippy/fmt/tests green. |

## Deviations from the spec

- **Group create validation**: the spec's "create a group" assumed any
  membership; the model enforces the two-person rule (>= 2 managers), so
  a single-manager group is rejected at creation (surfaced as a friendly
  error in the console, a non-zero exit in the CLI). This is stricter and
  safer than the spec's neutral wording; the spec's intent (two-person
  governance) is preserved.
- **LLM test base URL**: the spec said "issue `GET {baseUrl}/models`";
  the implementation tries `{baseUrl}/models` first and falls back to
  `{baseUrl}/v1/models` when the first 404s, so a `baseUrl` that already
  ends in `/v1` (the documented default) and one that does not both work.

## Verification

See `../../evidence/reactor-console/e2e.md`:

- **Store replay fix** — a compacted (snapshot-only) store replays its
  documents after restart (regression: they were lost); locked in by
  `load_replays_snapshot_then_wal`.
- **Groups** — CLI `group create/list/add-member/activity` exercised;
  the two-person rule rejects a single-manager group.
- **Console** — driven in a real Chromium tab against a live daemon:
  Overview, Settings/LLM (Test against a local stub -> "2 models
  available"), Documents (create + open), Groups (create + membership +
  activity), Drives. Renders cleanly; no framework/build step.
- **LLM test** — reachable stub returns the model list; unreachable and
  missing-key cases return structured errors (never a 500/panic).
- **Build/clippy/fmt/tests** — all green (`80` lib tests + integration).
- **Contract** — `status --json` still satisfies the Omarchy-plugin shape
  check.

**Not done** (documented in the evidence): auto-install of missing
packages from `registry.dev.vetra.io` is connect's capability (the Rust
daemon needs no npm packages at runtime); a brew formula is a follow-up
against the published release assets (the snap is the primary channel).
