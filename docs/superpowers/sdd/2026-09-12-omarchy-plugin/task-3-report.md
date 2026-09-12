# Task 3 report — State machine + fixtures + node tests

**Done:** `js/ReactorState.js` — pure JavaScript (QML importable via
`import "js/ReactorState.js" as R`, Node-testable via `require`):
- `classifyCliProbe`: missing (spawn error / "not found" on either
  stream) vs error (other nonzero) vs unsupported (semver < 0.2.0,
  including prerelease ordering) vs ok; `parseVersion` handles clap
  lines and bare tokens.
- `normalizeStatusPayload`: maps the Rust `StatusSnapshot`
  (`switchboard`/`settings`/`drives`) to a bounded UI view; names
  truncated (80), details (160), unknown drive statuses pass through,
  nameless drives skipped.
- `reduce`: pure + idempotent fold over `{type: "probe", cli, status}`
  events; the seven phases `missing | unsupported | stopped |
  starting | ready | degraded | error` are kept distinct; last-known-good
  drives survive a failed status probe; the next good probe recovers.
- `bound()`: single-line, length-capped with ellipsis — every string
  that reaches the UI or IPC goes through it.

During this session the drive view was extended with `url` (spec: the
panel lists the switchboard URL per drive; the Rust payload already
carries it).

**Tests:** `node --test tests/js/ReactorState.test.js` — 15 cases over
committed fixtures in `demo/fixtures/` (all seven phases, prerelease
semver, URL passthrough, malformed JSON, timeout, recovery transitions,
purity/idempotence, `bound`, `parseVersion`). All green.

**Deviations:** none.
