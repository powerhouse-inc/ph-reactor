# Console IA + Inbox Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the grown sidebar with three regions (Inbox / current space / this node) and add a cross-space Inbox fed by manifest-declared attention rules.

**Architecture:** The daemon gains `attention` on the signed manifest and an `/api/inbox` endpoint that evaluates those rules across every space the node is a member of. The console is restructured into three regions with a command palette; no new framework — the existing vanilla renderers and design tokens stay.

**Tech Stack:** Rust (axum, serde), vanilla JS + CSS in `console/v2.html`, `scripts/cargo.sh` for all builds (no host toolchain).

**Spec:** `docs/superpowers/specs/2026-09-15-console-ia-and-user-stories-design.md`

## Global Constraints

- All builds run through `./scripts/cargo.sh` — there is no cargo on the host.
- `attention` MUST use `#[serde(default, skip_serializing_if = "Vec::is_empty")]`. A manifest field that always serializes changes the bytes every past signature was made over and breaks every published package.
- Never add a sidebar label to `RESERVED_NAV_LABELS` without a test.
- Console changes are guarded by tests in `mod console_tests` that scan `PAGE_V2`.
- Clippy must pass with `-D warnings`.

---

### Task 1: Attention rules on the manifest

**Files:** Modify `src/package/mod.rs`
**Produces:** `pub struct Attention { model, when: Map, needs: String, label: String }`, `Attention::describe() -> String`, `Attention::validate() -> Result<(), String>`, `Manifest.attention: Vec<Attention>`

- [ ] Step 1: Write failing tests — `attention_is_skipped_when_empty_so_old_signatures_survive`, `an_attention_rule_describes_itself`, `a_rule_that_needs_nobody_is_refused`.
- [ ] Step 2: `./scripts/cargo.sh test --lib -- attention` → FAIL (no such type).
- [ ] Step 3: Add `Attention` next to `Projection`; add the field to `Manifest` with `skip_serializing_if`.
- [ ] Step 4: Tests pass; fix every `Manifest { .. }` literal the compiler names.
- [ ] Step 5: Commit.

### Task 2: `/api/inbox`

**Files:** Create `src/settings/inbox.rs`; modify `src/settings/mod.rs` (route)
**Consumes:** `Attention` from Task 1
**Produces:** `GET /api/inbox` → `[{ id, doc, space, spaceTier, app, label, model, ts }]`; `pub fn rows_for(store, installed, me) -> Vec<Row>`

- [ ] Step 1: Failing tests — a rule matches only when every `when` field matches; a row is emitted only when `me` is in the `needs` field; each row carries its space and tier; a doc with no space is skipped.
- [ ] Step 2: Run → FAIL.
- [ ] Step 3: Implement `rows_for` as a pure function over documents + installed packages, then the axum handler over it.
- [ ] Step 4: Tests pass.
- [ ] Step 5: Commit.

### Task 3: Sidebar — three regions and routing

**Files:** Modify `console/v2.html`, `src/settings/mod.rs` (console_tests)

- [ ] Step 1: Failing console tests — `the_sidebar_has_exactly_three_regions`, `old_hashes_redirect_rather_than_404`, `the_node_region_is_not_a_sibling_of_a_space_app`.
- [ ] Step 2: Run → FAIL.
- [ ] Step 3: Rebuild the sidebar markup as `#region-inbox`, `#region-space`, `#region-node`; add `REDIRECTS` map in `routeFromHash`.
- [ ] Step 4: Tests pass.
- [ ] Step 5: Commit.

### Task 4: The Inbox screen

**Files:** Modify `console/v2.html`

- [ ] Step 1: Failing console test — `an_empty_inbox_says_what_is_true` (asserts the copy is present, not a bare container).
- [ ] Step 2: Run → FAIL.
- [ ] Step 3: `renderInbox()` — rows grouped by urgency, each with a space chip carrying the tier colour; clicking a row opens the owning app scoped to that space and updates the lens.
- [ ] Step 4: Tests pass.
- [ ] Step 5: Commit.

### Task 5: Command palette and keyboard switching

**Files:** Modify `console/v2.html`

- [ ] Step 1: Failing console test — `the_palette_is_reachable_by_keyboard` (asserts a `keydown` handler binding `k` with a modifier exists).
- [ ] Step 2: Run → FAIL.
- [ ] Step 3: Palette listing spaces, current-space apps and node sections; filter as you type; Enter navigates; Escape closes; focus is trapped and restored.
- [ ] Step 4: Tests pass.
- [ ] Step 5: Commit.

### Task 6: The node region

**Files:** Modify `console/v2.html`

- [ ] Step 1: Failing console test — `the_node_region_holds_software_store_and_peers`.
- [ ] Step 2: Run → FAIL.
- [ ] Step 3: `renderNode()` with sections Overview / Software / Store / Identity, reusing the existing plugins, documents, types and settings renderers.
- [ ] Step 4: Tests pass.
- [ ] Step 5: Commit.

### Task 7: Achra declares attention, verified live

**Files:** Modify `packages/achra/powerhouse.manifest.json`; publish and install on a live daemon

- [ ] Step 1: Add `attention` for `proposal` awaiting `approvers` and `milestone` awaiting `org`.
- [ ] Step 2: Republish and install against a scratch daemon.
- [ ] Step 3: Create two spaces, a proposal in each, confirm `/api/inbox` returns rows from both, each labelled with its space.
- [ ] Step 4: Screenshot the console via the CDP driver.
- [ ] Step 5: Commit.

## Self-review

- Spec coverage: W1/W2 → Tasks 2+4; W3 → Task 4 (tier chip) and existing composer copy; W4/W5 → Task 5; W8/O6 → Task 6 space list; O1–O5 → Task 6. Removal of Groups/Documents/Types/Folders → Task 3 redirects + Task 6.
- Placeholder scan: none.
- Type consistency: `Attention` fields (`model`, `when`, `needs`, `label`) are used identically in Tasks 1, 2 and 7.
