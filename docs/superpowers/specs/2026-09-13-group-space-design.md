# Group as a shared space (drive + channels) - Design

Extends the built-in `group@1` model from a *membership container* into a
**shared space**: a place with a **drive** (shared documents/files) and a
**channel** (a signed message log), both gated by the group's members and
replicated over the mesh. Adds the missing piece to the invite flow: an
invite that **grants group membership** to the joiner.

## Problem

Today a `group@1` doc only tracks `members`/`managers` (plus a signed
activity feed). Two gaps keep it from feeling like a shared, decentralized
space:

1. **No content.** A group has no drive (shared files) and no channel
   (conversation). The name "group" implies a room people share things in;
   there is nothing to share.
2. **Invites don't grant groups.** `InviteToken` carries a `groups` field
   ("group names the joiner is granted on acceptance"), but the join path
   (`Command::Join`) decodes the token and *never applies the grant* - the
   joiner gets a drive connection but is never added to any group. So
   "share a link so others can join a group" does not actually work.

## Goals

1. A group has a **drive**: a member-gated list of documents. Any member can
   add a doc; managers can remove one. A convenient `note` model is a
   "file" you create straight into a drive.
2. A group has a **channel**: an append-only, member-gated message log
   (who/what/when/channel). Any member can post; it is a signed, replicated,
   verifiable part of the group doc - not server-hosted chat.
3. **An invite grants the group.** Inviting with a group list and having a
   peer join that invite adds the joiner to those groups (applied by the
   *inviter*, secured by the invite nonce).
4. **The console makes it obvious**: create a group, see its members / drive
   / channel, and a prominent **invite link** (copy button + one-line "how to
   share so others can join") plus a **join** affordance.

## Non-goals (v1)

- Per-message edits/deletes (the log is append-only, like the activity feed).
- Multiple named channels *rendered* separately (the data model carries a
  `channel` tag per message and defaults to `general`; the v1 UI shows one
  channel per group). Multi-channel rendering is a UI follow-up.
- Strong per-document ACLs (a drive doc, once on a drive, is fetchable by any
  peer who knows its name; "belonging to a group" is the member-replicated
  fact). Per-doc access control is a follow-up.
- Presence (ephemeral online status) - a follow-up over the mesh.

## Data model

All new state lives **in the group doc** so it is signed, hash-chained,
replicated only to members, and `doc verify`-able - the same guarantees as
`members`. The L1 write template is a scalar (`$actor` / `$ts` /
`$payload.<f>` / literal), so a message is stored as **parallel arrays**
that a single `post` action appends to atomically (they always stay the same
length, in append order = chronological).

`group@1` adds:

| field        | type      | meaning                                   |
|--------------|-----------|-------------------------------------------|
| `msg_from`   | `string[]`| message authors (peer ids)                 |
| `msg_text`   | `string[]`| message text                               |
| `msg_ts`     | `number[]`| message timestamps (ms)                    |
| `msg_channel`| `string[]`| channel name per message (`general` def.)  |
| `drive`      | `string[]`| doc names in the group's drive             |

New reducers (existing `init`/`add-member`/`remove-member`/`add-manager`
unchanged):

- `post { text, channel }` - pre `actor-in: members`.
  Appends to all four `msg_*` arrays (`from=$actor`, `text`, `ts`, `channel`).
- `add-doc { name }` - pre `actor-in: members`. Appends `name` to `drive`.
- `remove-doc { name }` - pre `actor-in: managers`. Removes `name` from
  `drive`.

`note@1` - a "file": fields `title: string`, `body: string`; reducers
`init { title, body }` and `edit { title, body }`.

## Invite grant (the "others can join" mechanism)

Direction matters: the **inviter** holds the group doc (it is the group's
authority), so the inviter applies the grant when it accepts the joiner.

1. Inviter generates an invite (`Command::Invite { groups }`): mint the
   `InviteToken` (nonce `N`, groups `G`) and record `pending_invites[N] = G`
   in the daemon.
2. Joiner consumes it (`Command::Join`): as today - pin the inviter (TOFU),
   dial it, attach an `InviteAccept` (echoes `N`) in the handshake.
3. Inviter's engine `handle_invite_accept` verifies the proof and - in
   addition to adding a drive back - emits
   `EngineEvent::InviteAccepted { peer, nonce: N }`.
4. Inviter's daemon loop resolves `pending_invites.remove(N) -> G` and, for
   each group, runs `add-member { member: <joiner peer> }` on that group doc
   (the reducer is manager-gated, so it applies only if the inviter is a
   manager - exactly the authority we want).

Security: a joiner cannot claim arbitrary groups - the grant is resolved
from the inviter's own issued nonce, and the nonce is one-shot (removed on
use). The `InviteAccept` is signed by the joiner (TOFU-pinned) and echoes the
nonce, so a forged/replayed proof fails verification.

## API (all on the loopback settings server)

Existing, reused:
- `POST /api/groups` - create a group (seeds the local peer as a member and
  a manager — a manager is always a member, so the creator can use the
  channel and drive).
- `POST /api/groups/:name/action` - runs any group reducer; the UI uses it
  for `post`, `add-doc`, `remove-doc` (and the existing membership actions).
- `POST /api/invite` `{ groups: [...] }` - mint a grant-giving invite.
- `POST /api/join` `{ invite }` - consume an invite.

Added:
- `GET /api/groups/:name` - the group's full state (members, managers,
  `msg_*`, `drive`) so the console can render the space.
- `POST /api/groups/:name/drive` `{ title, body }` - create a `note@1` doc
  and add it to the group's drive in one call (the "new file" action).
- `GET /api/groups/:name/drive` - the drive: for each referenced name, its
  `{ name, model, title, body }` (missing names are flagged).
- `DELETE /api/groups/:name/drive/:doc` - `remove-doc`.

The channel needs no endpoint beyond `POST /api/groups/:name/action`
(`post`) and `GET /api/groups/:name` (the `msg_*` arrays).

## Console (v2.html, Groups tab)

- **Create group**: name -> `POST /api/groups`.
- **Invite** (prominent): a button per group -> `POST /api/invite` with that
  group -> shows the token in a copy-able field with a one-line instruction:
  *"send this to a member; they open it in their ph-reactor console (Groups
  -> Join) to become a member."* A **Join** form (paste a token ->
  `POST /api/join`) sits beside it.
- **Drive**: the group's docs (title + body excerpt), an "Add file" form
  (title/body), and per-doc open/remove.
- **Channel**: the message log (who/when/text) in order, and a post box
  (text -> `post`).
- **Members/Managers**: as today, with add/remove.

The tab leads with the invite so "how do I let others in" is the first thing
seen; drive and channel follow.

## Edge cases

- **Inviter restarts before the joiner joins:** `pending_invites` is
  in-memory and lost; the joiner still gets a drive but no group grant.
  Re-invite to recover. (Persistence of pending invites is a follow-up.)
- **Dangling drive ref:** a drive may list a doc that no longer exists; the
  console flags it rather than failing.
- **Non-manager inviter:** `add-member` is manager-gated, so an inviter who
  is not a manager of a granted group cannot add the joiner (the grant is
  refused) - correct authority behavior.
- **Parallel-array integrity:** each `post` appends to all four arrays in
  one atomic reduce, so they cannot drift.

## Files

- `src/model/group.rs` - new fields + `post`/`add-doc`/`remove-doc`.
- `src/model/realistic.rs` (or a new `note.rs`) - the `note@1` model.
- `src/p2p/invite.rs` - unchanged (token already carries `groups`).
- `src/p2p/mod.rs` - `EngineEvent::InviteAccepted`; emit from
  `handle_invite_accept`.
- `src/daemon.rs` - `pending_invites` on `Ctx`; record on `Invite`; apply the
  grant on `InviteAccepted` via `store.apply_local_action(add-member)`.
- `src/settings/mod.rs` - `GET /api/groups/:name`, drive endpoints.
- `console/v2.html` - Groups tab: create / invite / join / drive / channel.
- `tests/` - two-instance E2E: create group -> invite -> join -> membership
  + a posted message + a drive note, verified on both sides.
