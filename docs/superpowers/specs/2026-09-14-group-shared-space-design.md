# Group as a shared space (channels + folder drive) — Rebuild

Follow-on to `2026-09-13-group-space-design.md` (which turned `group@1` into a
membership + single-channel + flat-drive container and made invites grant
membership). That design deliberately left **multi-channel** and a **real
file layout** as follow-ups. This rebuild closes both, plus the console
chrome that had drifted: a 6-item sidebar and a 0-when-alone "peers" count.

## Problem

1. **One channel.** `group@1` carries a `channel` *name* per message but the
   UI renders a single `general` channel. There is no way to have several
   named conversations, nor any notion of a *private* one.
2. **Flat drive.** `drive` is a `string[]` of doc names. There are no folders
   and every entry is implicitly a `note`. A "Google-Drive-like" shared space
   needs a hierarchy and a choice of document type.
3. **Private channels can't be authorized in the L1 DSL.** The precondition
   vocabulary has `actor-in <field>`, but it only reads a *top-level*
   `string[]` field of the doc. A "must be a member of *this* channel" rule
   needs to look inside a nested object in an array — the DSL can't express
   it, so it has to be a declared model capability the store enforces.
4. **Console chrome.** The sidebar had 6 items (Home-ish "Overview" plus
   Documents/Types/Folders that now live *inside* the group space), and the
   "peers" footer read **0** on a single node — which reads as "you're not
   connected to anyone" rather than "you're the only node."

## Goals

1. **Channels**: a group has a `channels` array; each is `{ name, visibility:
   public|private, members: string[] }`. `init` seeds a public `general`.
   Any **member** can create a channel; a **manager** removes one. A *public*
   channel is readable/postable by any member; a *private* one only by its
   `members`.
2. **Folder drive**: replace the flat `drive:string[]` with a `drive` array
   of `{ name, kind: folder|doc, model, parent }` (parent `null` = root).
   Members add folders/docs; managers remove. A doc records its **type**
   (`note`, `task`, …) and its **parent** folder; the actual doc of that type
   is created in the store when the item is added.
3. **Private-channel auth as a declared model rule.** Add an L1 **`auth`**
   block (definition-driven, distributed with the def) that the store runs on
   the single apply path. For a group `post`, it checks the actor against the
   target channel's allow-list when that channel is private.
4. **Peer count that is never 0.** The overview reports
   `peerCount = knownPeers + 1`; `knownPeers` excludes the local origin, so a
   lone node reads **1** ("you") and grows as real peers are discovered.
5. **Slimmed console.** The sidebar is exactly **Home / Groups / Settings /
   Profile**. The Groups view becomes a space with sub-tabs **Overview |
   Channels | Drive**, and a drive doc opens in a **field-type-aware rich
   editor**.

## Non-goals

- Per-message edit/delete (channels stay append-only, like before).
- *Read*-side private-channel hiding at the mesh: a group doc is replicated
  to its **members**, so a member not in a private channel's allow-list can
  still *see* that channel's messages on a synced node. The hard gate is on
  **posting** (the store rejects it); the console additionally *hides* a
  private channel the local peer is not in, as the read-side affordance.
  True read-side mesh encryption of a subset's messages is out of scope.
- Cross-group or cross-drive search; per-doc ACLs beyond group membership.

## Data model

All state stays **in the group doc** (signed, hash-chained, member-replicated,
`doc verify`-able). The L1 write template is a scalar, so the message log
remains **parallel arrays** appended atomically by one `post`. The two new
structures are `array` fields whose items are **objects** (the L1 `array`/
`object` types are permissive, so this is expressible in-def with no new
engine features):

| field       | type      | meaning                                              |
|-------------|-----------|------------------------------------------------------|
| `channels`  | `array`   | `[{ name, visibility: "public"\|"private", members: string[] }]` |
| `drive`     | `array`   | `[{ name, kind: "folder"\|"doc", model: string\|null, parent: string\|null }]` |

(`members` is meaningful only for a `private` channel; `parent` `null` =
root, a doc's `model` is its type name, `model` `null` for a folder.)

New / changed reducers (all `set`/`append` of a whole array, so they converge
under per-field LWW):

- `init { name, members, managers }` — unchanged payload; now also seeds
  `channels = [{ name: "general", visibility: "public", members: [] }]` and
  `drive = []`.
- `add-channel { item }` — pre `actor-in: members`. Appends `item`.
- `remove-channel { channels }` — pre `actor-in: managers`. `set`s the
  recomputed array.
- `post { text, channel }` — pre `actor-in: members`; **plus** the `auth`
  rule below. Appends to the four `msg_*` arrays.
- `add-folder { item }` — pre `actor-in: members`. Appends to `drive`.
- `add-doc { item }` — pre `actor-in: members`. Appends to `drive` (the
  handler separately creates the actual doc of `item.model`).
- `remove-item { drive }` — pre `actor-in: managers`. `set`s the recomputed
  array (covers folders and docs).

### The `auth` block (new L1 capability)

A model may declare `auth: { <kind>: { field, name, match, visibility,
private, allow } }`. The store, on the single apply path (and only when the
doc is loaded), locates the array entry in `field` whose `name` equals
`match` (a payload template); if that entry's `visibility` equals `private`,
the actor (`action.origin`) must appear in the entry's `allow` array or the
action is rejected. `match` is how a *dynamic* target (the `post`'s channel
name) is bound to a nested entry. This is definition-driven and ships with the
def, so it is as distributed/verifiable as a precondition. The group model
uses it for `post` → `field: channels`, `name: name`, `match: $payload.channel`,
`visibility: visibility`, `private: private`, `allow: members`.

### A second doc type to exercise the editor

`task@1` (in `model/realistic.rs`) gets a `status` **enum** and a `tags`
**list** on top of the existing fields:
`{ title, status, priority: number, assignee, project, tags: string[] }`,
reducers `init` + per-field `set-*`. The model def gains a top-level **`enums`**
map (e.g. `task.status = ["open","in-progress","blocked","done"]`) and
`/api/models` exposes it, so the editor renders a `<select>` for enum string
fields. `note@1` (`title`,`body`) stays as the simple two-text-field type.

## API (loopback settings server)

Reused: `POST /api/groups` (create; auto-adds the local peer as member+
manager), `GET /api/models` (catalog, now incl. `enums`).

Added / extended:
- `GET /api/groups/:name` — now returns `channels` (and `drive`) alongside
  `members`, `managers`, and the `msg_*` arrays.
- `POST /api/groups/:name/channels` `{ name, visibility?, members? }` —
  `add-channel` (member). `visibility` defaults to `public`.
- `DELETE /api/groups/:name/channels/:chan` — `remove-channel` (manager).
- `POST /api/groups/:name/action` `{ kind, payload }` — unchanged; used for
  `post` (and membership actions).
- `GET /api/groups/:name/drive` — the drive as a flat list of items, each
  `{ name, kind, model, parent }`, with the doc's current `fields`/`title`
  inlined and a `missing` flag for a gone doc. A legacy *string* entry is
  normalized to a root `note` doc (migration, below).
- `POST /api/groups/:name/drive/folder` `{ name, parent? }` — `add-folder`.
- `POST /api/groups/:name/drive/doc` `{ name?, model, parent?, fields? }` —
  creates a doc of `model` (init with defaults + `fields`) and `add-doc`s it.
- `DELETE /api/groups/:name/drive/:item` — `remove-item` (manager).
- `GET /api/overview` — `peerCount = knownPeers + 1` where `knownPeers`
  excludes the local origin (never 0; see the store change).

## Console (v2.html)

- **Sidebar** — exactly `Home` (`data-tab=overview`, the existing overview
  view), `Groups`, `Settings`, `Profile`. `documents`/`types`/`folders` stay in
  `RENDER`/`TITLES` for deep-link back-compat but are no longer nav items.
- **Profile** (new) — this node's identity: full peer id, instance name,
  listen address, live-doc count, the `peerCount` ("includes you"), and a
  "Nodes on the mesh" card showing the local node (badged *you*) plus any
  known remote peers.
- **Groups → the space.** `showGroup(name)` renders a sub-tab bar
  **Overview | Channels | Drive** (per-group UI state kept in a `groupUi` map).
  - *Overview*: the invite link (generate/copy), members, managers (add
    member).
  - *Channels*: a channel list with `public`/`private` badges; a create form
    (name + public/private toggle + member list when private); a composer with
    a channel **selector** that posts to the selected channel; and the selected
    channel's messages (the `msg_*` arrays filtered by `msg_channel`). A
    private channel the local peer is not in shows as **locked** (no composer)
    — the read-side affordance.
  - *Drive*: a breadcrumb (root › … › current folder) and the current
    folder's items (sub-folders + docs). A **New** menu offers *New folder*
    (name + parent = current) and *New document* (type picked from
    `/api/models`, excluding `group`/`folder`, + parent = current). Clicking a
    doc opens the rich editor; delete is manager-gated.
- **Rich editor.** Reads the doc's model from the catalog and renders
  field-type-aware widgets: `string` → text, **`string` with a declared enum →
  `<select>`**, `number` → number, `boolean` → checkbox, `string[]`/list → an
  editable add/remove row list, `object`/unknown → a JSON textarea. A field is
  editable only if the model has a `set-<field>` reducer (else a generic
  `set`), otherwise it is read-only. **Save dispatches the model's real
  per-field `set-*` reducers** through the existing `POST /api/docs/action`
  path, so edits sync over the mesh.

## Edge cases

- **Migration (flat → folder drive).** `drive` changes from `string[]` to
  `array`. A legacy `string` entry still passes `check_state` (permissive
  `array`), and both `drive_list` and the console normalize a string item to
  a root `note` doc, so an existing drive keeps working. Because the def's
  content hash changes, a group doc written under the *old* hash no longer
  verifies against it; re-issuing `init` adopts the new shape.
- **Private-channel post by a non-allowlisted member.** The group-member
  precondition passes, but the `auth` rule rejects: *"actor … is not a member
  of the private channel '…'."* A public channel is not `auth`-gated (the
  member gate suffices). Enforced at `store::apply_action` step 3c, *before*
  reduce, so a failed check never mutates the entry.
- **`known_peers` includes the local origin.** `Store::open` pins the local
  key (so local actions verify), so `known_keys` contains the self id.
  `known_peers()` now filters it out, keeping the peer count honest (1 for a
  lone node).
- **Enum value not in the dropdown.** If a stored value isn't in the declared
  enum set, the editor offers it as a leading option rather than dropping it.
- **Dangling drive ref.** A `doc` item whose doc is gone is flagged `missing`
  (rendered, not fatal), as before.

## Files

- `src/model/l1.rs` — new `auth` def field + `Model::authorize` (nested-entry
  allow-list check).
- `src/model/group.rs` — `channels` + `drive` fields, the `auth` block, and
  `add-channel`/`remove-channel`/`add-folder`/`add-doc`/`remove-item`; `init`
  seeds `general`. Focused unit tests (incl. private-channel auth accept/
  reject).
- `src/model/realistic.rs` — `task@1` gains `status` (enum) + `tags` (list);
  `enums` maps exposed on the def.
- `src/store.rs` — `apply_action` runs `model.authorize` on the single path
  (step 3c); `known_peers()` excludes the local origin.
- `src/settings/mod.rs` — `GET /api/groups/:name` (+channels/drive), channel
  add/remove, `drive/folder`, `drive/doc`, `drive` list (normalized), overview
  `peerCount`; `/api/models` exposes `enums`.
- `console/v2.html` — 4-item sidebar, `Profile` view, the sub-tabbed group
  space, and the field-type-aware rich editor.
- `docs/superpowers/…` — this spec + SDD progress; the curl smoke test is the
  evidence.
