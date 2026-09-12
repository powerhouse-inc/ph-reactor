# Task 8 report — Settings page + API

**Done:** `settings/mod.rs` — axum on 127.0.0.1:4002 (configurable):
`GET /` embedded single-page UI (inline CSS/JS, no CDN),
`GET /api/status` (StatusSnapshot), `POST /api/drives`
(name/url/tokenEnv/availableOffline), `POST
/api/drives/{name}/pause|resume|resync`, `DELETE /api/drives/{name}`,
`POST /api/config`, `POST /api/quit`. All mutations go through the
daemon's command loop (single writer) — page actions cannot race the
poller or the CLI.

**Fixes found during E2E:** routes standardized on axum 0.7 `{name}`
syntax (stray `:name` duplicates removed); name validation relaxed
(any chars except `/`, NUL) with percent-encoding of names in URLs;
JSON field aligned to `availableOffline`.

**Tests:** 6 (status JSON, add validation incl. names with spaces,
route round-trips, config set, quit). All green.
