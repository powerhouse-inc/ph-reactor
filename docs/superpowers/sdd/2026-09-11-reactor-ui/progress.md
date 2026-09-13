# Progress

Implementation log for the reactor console (see `2026-09-11-reactor-console-design.md`
in `docs/superpowers/specs/` and the plan in `docs/superpowers/plans/`).

| Task | Name | Status |
|------|------|--------|
| 1 | DocChange carries the action delta | done |
| 2 | Processors as subscriptions | done |
| 3 | The processor API | done |
| 4 | LLM type drafting + model registry + invoice | done |
| 5 | Folders and group documents (quorum) | done |
| 6 | The console v2 UI (flashy redesign) | done |
| 7 | Verify: build, clippy, tests, live smoke | done |
| 8 | Merge to main | pending |

## Task notes

### Task 1 - DocChange carries the action delta (done)
- `doc.rs`: `DocChange` gained `action_kind` / `action_field` / `action_value`
  (the action that produced the change).
- `store.rs`: `emit_change` populates the delta from the applied action. An
  explicit `field`/`value` payload (the `open` model) is captured as-is; a
  single-key payload (an L1 field-set reducer like `set-status`) is captured as
  that field + its value.
- The in-memory change feed (`ArcSwap<DocChange>`) already existed; no new file.

### Task 2 - Processors as subscriptions (done)
- New `processor.rs` (module): `ProcessorSpec` (a serde'd subscription: name,
  models, optional action_kind/field/value filters, an enabled flag), `Reaction`
  (a tagged enum: `run` / `log` / `emit` / `create-doc`), `ProcessorEngine`
  (subscribes to the store's change feed on its own mpsc channel - no new feed
  channel - and fires matched reactions), `ProcessorRunner` (an in-memory
  `Mutex<Vec<ProcessorSpec>>` + the engine handle + a per-processor fire history
  + `add`/`remove`/`update`/`set_spec`/`fires`), and a `ProcessorHandle` for the
  console.
- The engine preserves a spec's `created` time on an in-place update so ordering
  is stable.

### Task 3 - The processor API (done)
- `processor.rs`: a `processors_file()` (the persisted specs, under the state
  dir, mirroring `config.json`) and `processor_handle()`.
- `paths.rs`: `processors_file()` path.
- `daemon.rs`: the daemon builds a `ProcessorRunner` (subscribed in its
  constructor, spawned as a background task) and passes its handle to the
  settings server.
- `settings/mod.rs`: `GET/POST/PUT/DELETE /api/processors[/name]` +
  `GET /api/processors/:name/fires`. The old live-subsystems endpoint moved to
  `/api/overview` so `/api/processors` is free for the user's subscriptions.

### Task 4 - LLM type drafting + model registry + invoice (done)
- `settings/mod.rs`: `GET /api/models` (the registered types),
  `POST /api/llm/draft-type` (asks the configured OpenAI-compatible LLM to draft
  a model definition from a description; the key is read from the environment,
  never stored; retries up to 3 times with a "complete JSON" nudge), and
  `POST /api/models/register` (+ `POST /api/models`) (validates and registers an
  `L1` interpreter). `extract_json_object` pulls a definition out of a response
  that may be wrapped in prose or markdown fences.
- `model/realistic.rs`: the `invoice@1` model (a status lifecycle) - the target
  of the reference processor example.

### Task 5 - Folders and group documents (quorum) (done)
- `model/realistic.rs`: the `folder@1` model (a membership container: init,
  add-member, remove-member, set-description).
- `settings/mod.rs`: `GET/POST /api/folders` + `POST /api/folders/:name/action`.
- `daemon.rs`: the daemon seeds its store with the realistic models (project,
  task, account, transaction, invoice, folder) at both store-open sites so the
  console can create docs under them.
- The group@1 two-person-rule quorum (multi-sig add-manager) is exercised by
  the store's existing `group_add_manager_two_person_rule` test and, live, by
  the settings API (a single signer cannot add a manager).

### Task 6 - The console v2 UI (done)
- A single self-contained `console/v2.html` (dark, animated, no build step),
  served at `/` and `/console/v2`; the legacy console stays at `/console`.
- Tabs: Overview (peers + live fire feed + the one-click reference processor),
  Documents (create under any type, filter by model/field, inspect + update a
  field), Types (LLM-draft / manual / register / use), Folders, Groups (with the
  two-person rule and signed activity), Settings (drive, identity, LLM with a
  live test button).
- Doc creation goes through `POST /api/docs` (extended to create under a
  specific model's `init` reducer when `model` is supplied; bare names resolve
  to the registered version). `POST /api/docs/action` also resolves bare model
  names.

### Task 7 - Verify (done)
- `cargo build --workspace` + `cargo clippy --workspace` warning-free;
  `cargo test --workspace` passes (84 lib + views + integration).
- Live smoke (fresh state dir, free loopback ports, mdns/DHT/relay disabled,
  vLLM on `127.0.0.1:8002` as the LLM provider): the v2 console is the landing
  page; the reference processor fires and runs its command; folders create and
  hold members; the LLM drafts a `book` type which is registered and used to
  create a doc; a single signer cannot add a group manager (the two-person
  rule). Full detail in `evidence/e2e.md`.

### Task 8 - Merge to main (pending)
