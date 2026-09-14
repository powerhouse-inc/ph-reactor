# Group shared-space rebuild — Progress

Spec: `../../specs/2026-09-14-group-shared-space-design.md`
Base: `../../specs/2026-09-13-group-space-design.md` (flat drive + single channel; this rebuild adds channels, a folder drive, private-channel auth, the slimmed console, and a never-zero peer count)

| # | Task | Status | Commit |
|---|---|---|---|
| 1 | Model: `channels` (public/private) + folder `drive`; L1 `auth` block for `post`; `init` seeds `general` | ✅ done | `17e4aa0` |
| 2 | Store: run `model.authorize` on the single apply path (step 3c); add `known_peers()` | ✅ done | `de775fa` |
| 3 | API: channel add/remove; `drive/folder`, `drive/doc`, `drive` list; `GET /api/groups/:n` (+channels/drive); overview `peerCount`; `/api/models` `enums`; `task@1` status/tags | ✅ done | `1a5ea50` |
| 4 | Console: 4-item sidebar (Home/Groups/Settings/Profile), new `Profile` view, sub-tabbed group space (Overview/Channels/Drive), field-type-aware rich editor | ✅ done | `78c50b9` |
| 5 | Style: apply rustfmt to the group-space model, L1 `authorize`, settings API | ✅ done | `321f886` |
| 6 | Fix(store): `known_peers()` excludes the local origin so `peerCount` is not double-counted (1 for a lone node) | ✅ done | `5f996a3` |
| 7 | Fix(console): Profile view — drop the dead `isKnown` note, clarify the never-zero hint | ✅ done | `ffc0263` |
| 8 | Verification: `cargo build`/`clippy -- -D warnings`/`fmt --check` clean; 19 group-model tests pass; curl smoke test (channels public+private, non-member private post rejected, folder tree, note+task docs, models catalog, console markers) | ✅ done | — (evidence below) |

## Notes

- Work on `feat/group-ui` from `main`.
- **Peer count is a judgment call on scope.** The overview's `peerCount` counts
  *known* peers (authenticated via a handshake/invite) + the local node, not
  ephemeral TCP connections. A node that has never met a peer reads **1** (you),
  which is the honest "you're the only node" signal the task asked for.
- **Private-channel read is a UI affordance, not a mesh guarantee.** Group docs
  replicate to all members, so a member outside a private channel's allow-list
  can see its messages on a synced node; the hard gate is on *posting* (the
  store `auth` rule). The console hides a private channel the local peer is not
  in. See the spec's Non-goals.
- **`auth` runs only when the doc is loaded** (like preconditions); an
  unknown/`open`-model post is decided by the model's own preconditions/reduce.
  Documented at `store::apply_action` step 3c.

## Evidence (curl smoke test, temp state dir, ports 14201/14002)

- `GET /api/overview` → `peerCount: 1`, `knownPeers: []` (a lone node).
- Create group `core` → 200. Add public `announce`, private `insiders`
  (members: self), private `sec` (members: a stranger).
- Post to `general`/`announce`/`insiders` → 200. Post to `sec` → **400**:
  `precondition failed: actor … is not a member of the private channel 'sec'`.
  `GET /api/groups/core` shows `msgChannel` = `[general, announce, insiders]`
  (the rejected `sec` post never appended).
- Add folder `projects` (root) → subfolder `q3` (parent `projects`) → a `note`
  and a `task` doc in `q3`. `GET /api/groups/core/drive` returns the tree with
  `parent` links; the task doc carries `status: in-progress`, `priority: 2`,
  `tags: [ui, mesh]` (defaults filled for `assignee`/`project`).
- `GET /api/models` → `task` exposes `enums.status = [open, in-progress,
  blocked, done]` (drives the editor's dropdown); `note` is two string fields.
- `curl /` → the served console has the 4 sidebar tabs (`overview`/`groups`/
  `settings`/`profile`), the 3 sub-tabs (`overview`/`channels`/`drive`), and the
  `openEditor`/`renderGroupDrive`/`drive/doc`/`drive/folder` markers.
