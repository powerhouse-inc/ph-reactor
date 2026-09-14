# Spaces: access is the container, apps are the extension — Design

Replaces `group@1` with **`space@1`**: a container whose only job is to say
who may read and who may manage. Everything else a space contains — chat, a
drive, project management, contributor billing, a forum — becomes an **app**,
installed on the node and *enabled* in a space, built exactly the way Achra is
built today.

## Problem

Groups and plugins are two different worlds, and nothing composes across them.

- A `group@1` document hardcodes its own channels and drive. Those features
  are privileged: no plugin can add one like it, and the group model must grow
  a field for every feature anyone wants.
- Plugins install node-wide. Their documents float free — an Achra `rfp`
  belongs to no group, no org, nothing.
- Every app therefore reinvents membership. `rfp` carries `publisher` and
  `approvers`; `agreement` carries `org` and `builder`. Each is a bespoke
  access list that the store does not enforce. Contributor billing would
  invent `payer`/`payee`; project management would invent `assignees`. Five
  apps, five different answers to *who may see this*, none of them checked.

That is the disconnection. It is not missing features — it is that the core
and the extensions are built differently, so they cannot be composed into a
suite.

Underneath sits a second problem: **the system has no confidentiality at all**,
so a tiered space model cannot be built on it as it stands. See Constraints.

## Goals

1. **Three tiers that mean what they say.** `public` — anyone on the mesh
   reads, managers manage. `protected` — only members read, managers manage.
   `private` — only you.
2. **One membership list per space**, enforced by the store, inherited by
   every app in that space. Removing someone is one edit.
3. **Apps are uniform.** Chat, Drive, Achra, billing, time tracking: the same
   manifest, the same capability prompt, the same nav mechanism, the same
   enable action. No privileged core.
4. **Space-scoped capabilities.** A billing app open in one client's space
   cannot read another client's invoices, even though the node holds both and
   they are the same model.
5. **Selective transparency.** A protected space can publish a derived,
   narrower record into a public one — without the private document ever
   being reachable.

## Non-goals (v1)

- **Encryption at rest or in transit beyond Noise.** Protected means
  *non-members cannot fetch it*, not *members cannot leak it*. See Constraints.
- **Forward secrecy / retroactive revocation.** Someone removed from a space
  keeps everything they already replicated. This is inherent to a replicated
  store and is not solved here.
- **Multi-device private spaces.** A private space is local to one node.
- **Moderation of the commons.** Open problem, called out in Risks.
- **Nested spaces / sub-spaces.** One level.

## Constraints discovered in the code

These are not assumptions. Each was read and, where it is a defect, reproduced
by a test.

1. **There is one gossipsub topic** (`src/p2p/mod.rs:447`). Every action from
   every node fans out to every peer on the mesh.
2. **`Store::summary()` is unfiltered** (`src/store.rs:335`) and the `CatchUp`
   handler (`src/p2p/mod.rs:1203`) serves any `doc_id` to any peer with no
   authorization check.
3. **A channel's `visibility: "private"` gates posting, not reading.** It is a
   model `auth` rule on the apply path. The text is plaintext in every replica.
4. **Document identity and transport identity are the same key.**
   `signing_key()` (`src/p2p/mod.rs:141`) derives the signing key from the
   libp2p identity keypair, so the peer authenticated by the Noise handshake
   is the same principal a member list names. *This is what makes any of this
   enforceable.*
5. **The document envelope has no space.** `Doc { id, name, fields }`
   (`src/doc.rs:118`); `Action` (`src/action.rs:50`) likewise.
6. **`append`/`remove` reduce to a whole-array write** (`src/model/l1.rs:127`)
   that merges last-writer-wins, and `apply_action` never enforces `prev_hash`
   (only `verify` did). Two nodes receiving the same two concurrent actions in
   opposite orders diverged permanently — a member removed on one node stayed
   a member on the other. **Fixed before this spec** (`caf6f89`): actions now
   have a canonical order and an out-of-order arrival re-folds the log. This
   was a prerequisite: the defect is a nuisance while membership gates writes
   and a confidentiality failure the moment it gates reads.

## Data model

### `space@1` — built-in

Built in, like `package` and `release`: a space is the carrier of access
decisions, so it cannot be delivered inside one.

| field        | type       | meaning                                  |
|--------------|------------|------------------------------------------|
| `visibility` | `string`   | `public` \| `protected` \| `private`     |
| `members`    | `string[]` | may read; the space replicates to them   |
| `managers`   | `string[]` | may edit membership and enable apps      |
| `apps`       | `array`    | `{ name, version }` enabled here         |

Reducers: `init { name, visibility, members, managers }`;
`add-member`/`remove-member` (manager); `add-manager` (quorum of 2, keeping
the existing two-person rule); `enable-app`/`disable-app` (manager).

**Visibility is immutable after `init`.** There is no honest
`set-visibility`. Protected→public is a retroactive bulk disclosure of
everything ever written in the space; public→protected is a lie, because the
data is already on every node in the mesh. Changing a space's tier means
creating a new space and deciding what to copy into it — which is the
decision the operator should be making anyway.

**The commons** is a well-known public space, seeded at init, that every node
is implicitly a member of. Its `DocId` is a fixed constant compiled into the
daemon rather than generated per node — every node must name the same
document, or there is no shared commons to publish into. Achra is installed there. This is why there is no
such thing as a "global app": a global app is a space app enabled in the
commons.

### The document envelope gains a space

`Action` gains `space: Option<DocId>` — a space is itself a document, so a
space id is that document's `DocId` and needs no new type. It is set at
`init` and immutable thereafter; `Doc` carries the same, derived.

**It must live on the signed action, not in the field map.** As an ordinary
field it would be subject to last-writer-wins, and a concurrent write could
*move* a document out of a protected space. On the action, the binding is as
strong as the signature.

**It must not break existing signatures.** `Action::message_bytes` is the root
of all trust in this system. The field is serialized with
`skip_serializing_if`, so a document with no space produces byte-identical
output to today and every historical action still verifies — the same
technique that kept the manifest's `ui` field from invalidating published
packages (`bdf1b64`), applied somewhere far less forgiving.

## Access enforcement

Three points, and only three.

1. **`CatchUp`** checks the requesting peer against the space's `members`
   before serving. Sound because of Constraint 4.
2. **`Summary`** is filtered per peer. A non-member must not learn that a
   document *exists*, which is metadata `summary()` leaks today.
3. **Gossip.** **Non-public spaces cannot use gossipsub at all.** Topic
   subscription is unauthenticated — any peer may subscribe to any topic — so
   a per-space topic enforces nothing. Actions in protected and private spaces
   go to member peers over the direct, authenticated sync protocol. Gossip
   becomes a public-space optimization, which also puts all enforcement in one
   place instead of splitting it across two transports.

A **private** space is the degenerate case: never gossiped, never served.
Genuinely enforced today, at the cost of no multi-device.

### What this does and does not buy

It buys confidentiality against non-members who speak the protocol. It does
**not** buy confidentiality against a member who defects and re-serves what
they hold, and it does not buy revocation of what someone already has. The
console must say so in those words — see Risks.

## Apps

**Install and enable are different decisions and stay separate.**

- **Install** is node-level and unchanged: you trust a publisher key, the
  package's models register, its UI is served. Already shipped; not touched.
- **Enable** is space-level and new: a manager enables app X in space Y. That
  is what puts X in the nav while you are in Y, and what scopes its access.

**Capabilities resolve against the current space.** Today
`Capabilities::may_read(model)` is node-wide. It becomes

```
may_read(model) && doc.space == current_space && enabled_in(current_space)
```

so the bridge cannot read across spaces even for a model the plugin is
entitled to. This is a security improvement over what ships today, not merely
a reorganization.

### Chat and Drive stop being special

They lift out of `group@1` and ship as built-in apps — in-tree, but going
through the same manifest, capability, nav and enable path as Achra. After
this, "project management, contributor billing, sales funnel, time tracking"
is a list of things built like the chat that already works, rather than a
second category of thing.

## Selective transparency: projections

An app declares in its manifest that a transition in one space emits a
derived document into another:

```
"projections": [
  { "from": "milestone", "on": "accept", "into": "commons",
    "to": "ledger-entry", "fields": ["amount", "org"] }
]
```

Two documents, two homes. The private one is structurally unreachable rather
than depending on a view filter being correct — which matters because in a
system where every member holds a full replica, redaction-by-view is a lie.

Declaring it in the manifest means the install prompt can say *"this app will
publish payment records publicly."* That is the consent that matters, and it
is only possible because projections are declared rather than coded.

## Console

The sidebar becomes: **space switcher**, then the current space's apps, then
node-level entries (Settings, Profile). Achra appears when you are in the
commons. A space's tier is shown next to its name, with the honest wording
from Risk 7 one click away.

## Migration

No in-place upgrade preserves verification: changing a model definition
changes its content hash, and documents written under the old hash no longer
verify against the new one. `group@1` documents are re-issued as `space@1`
with `visibility: protected` (closest to today's semantics), their channels
and drive becoming Chat and Drive app documents stamped with the space.

Three rules:

1. **Explicit, never silent.** Re-issuing is a signed act. It is a command,
   with a `--dry-run` that prints every document and the space it would land
   in, before anything is signed.
2. **No default for unstamped documents.** Existing documents predate the
   field and both defaults are wrong: defaulting to the commons publishes
   everything irreversibly; defaulting to a private space makes Achra look
   broken. The mapping is stated per model in the migration input and
   reviewed in the dry-run.
3. **One node migrates; the rest receive it.** The laptop and the cluster both
   hold `ph-bootstrap`. Migrating independently produces two spaces with two
   ids and a permanent fork. The new space id is derived deterministically
   from the old document id so a re-run is idempotent.

## Risks and how each is handled

| # | Risk | Handling |
|---|------|----------|
| 1 | Concurrent membership edits diverge; a removed member is resurrected | **Fixed in `caf6f89`** before this work, with two regression tests |
| 2 | Changing `message_bytes` silently partitions old and new nodes | `skip_serializing_if`; a mixed-version sync test (a real 1.9.0 node against a new build) is a merge gate; cluster auto-update **already turned off** (`38ddce81` in the hosting repo) |
| 3 | No safe default for unstamped documents | Explicit per-model mapping + `--dry-run`; no implicit default exists |
| 4 | ACL bootstrap is circular; revocation is eventually consistent | A space document is served to anyone named in the server's own copy of it. Stated plainly: **revocation is best-effort and forward-only**; someone who stops syncing keeps a valid-looking membership |
| 5 | Correct withholding and broken sync look identical | The withhold path is counted and logged distinctly from the empty path; the console shows per-space sync state. Without this, every bug presents as "sometimes my data doesn't appear" |
| 6 | Split-brain migration across laptop and cluster | Deterministic space id from the old document id; migrate on one node |
| 7 | "Protected" is trusted more than it deserves | The console states the actual guarantee: *every member holds a full plaintext copy, and anyone who ever had access keeps what they saw.* The likeliest source of real harm, and it is a copy problem, not a code problem |
| 8 | Private means no replica anywhere — laptop dies, data gone | Export before private spaces ship; the tier says so where it is chosen |
| 9 | The commons is world-writable: spam, and GC cannot collect referenced junk | **Unsolved.** Out of scope for v1 and called out as a gap in the marketplace story |
| 10 | One long branch against a deployed, self-updating daemon | Phased below; each phase ships on its own |

## Phases

Each is independently shippable and independently valuable.

1. ~~Canonical action order and re-fold~~ — **done** (`caf6f89`).
2. **The signed envelope alone.** `space` on the action, always absent, no
   enforcement. Ships with the mixed-version sync test. This is the only
   change that can partition the mesh, so it travels by itself where it is
   observable.
3. **`space@1` and enforcement.** The model, the three tiers, the `CatchUp`
   and `Summary` checks, direct sends for non-public spaces.
4. **Apps in spaces.** Enable/disable, space-scoped capabilities, the space
   switcher.
5. **Chat and Drive as apps**, and the `group@1` migration.
6. **Projections.**

Phase 2 is where the risk is concentrated; phases 4 and 5 are where the
feeling of a connected suite arrives.

## Open questions

- Does a private space need to reach the user's other devices? Saying yes
  makes it a different design (per-identity replication, or encryption).
- Is quorum-of-2 still right for `add-manager` in a space of one?
- Should `enable-app` require the app to be installed on every member's node,
  or is a per-node "you don't have this app" state acceptable?
