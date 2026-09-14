# Plugin packages for ph-reactor — Design

## Problem

ph-reactor should be extensible: a network should be able to ship **Achra**, a
**knowledge vault**, or anything else as a plugin carrying its own document
models, processors and user interface — and those plugins should be
**verified** and **distributed over the mesh** rather than installed by hand on
every node.

Today a model can be added at runtime and will even travel between peers, but
there is no notion of a package, no publisher identity, and no way to ship a UI
at all.

## What already exists (verified in the code, not assumed)

**Model distribution is built and integrity-checked.** A peer lacking a model
requests it (`SyncMsg::ModelRequest` → `ModelDef`) and the requester enforces
name, version and SHA-256 before registering it:

```rust
if model_def_hash(&def) != want {
    tracing::warn!("model {} definition failed hash verification; dropped", d.ref_);
    return;
}
```

`ModelRef { name, version, hash }` is content-addressed, and `model_def_hash`
is the SHA-256 of the canonical JSON. Models also now persist across restarts
(`<state>/models.json`, added 2026-09-14).

**Processors** (`ActionFilter` → `Reaction` → `Fire`) are declarative specs
persisted in `<state>/processors.json` — local only, never distributed.

**The console is compiled in.** `include_str!("console.html")` and
`include_str!("../../console/v2.html")`. There is no UI extension point.

## What the Powerhouse monorepo offers (and what it does not)

The monorepo at `/home/f/projects/powerhouse` already defines the package shape
this design needs, and the ecosystem uses it (`paperless-sync`,
`knowledge-note`, `contributor-billing`, `project-management`, …):

```
document-models/  editors/  processors/  subgraphs/  reactor/
powerhouse.manifest.json
```

```json
{ "name": "@powerhousedao/project-management",
  "description": "...", "category": "Project Management",
  "publisher": { "name": "Powerhouse", "url": "https://powerhouse.inc/" },
  "documentModels": [ { "id": "powerhouse/scopeofwork", "name": "ScopeOfWork" } ],
  "apps": [], "editors": [], "subgraphs": [], "processors": [], "config": [] }
```

Reusable, and adopted here:

| Asset | Use |
|---|---|
| `powerhouse.manifest.json` shape and vocabulary | Adopted directly |
| `@powerhousedao/design-system` (React) | Plugin editors are built with it |
| `reactor-browser` — Renown in-page sign-in, hook vocabulary | Reference for the browser-actor work |

**Not reusable, and this is the decisive finding.** A Powerhouse document-model
is a JSON spec **plus hand-written TypeScript reducers**:

```ts
import { scopeOfWorkDeliverablesOperations } from "../src/reducers/deliverables.js";
```

ph-reactor's models are pure declarative JSON. Executing a Powerhouse
document-model would require a JavaScript engine inside the daemon, which
contradicts its founding constraint — "No Node, no npm, no child processes".

So **existing Powerhouse packages will not run unmodified on ph-reactor.** This
design adopts their *format*, not their *runtime*. A plugin's models are
declarative JSON; only its UI is JavaScript, and that runs in the browser where
JavaScript already belongs.

One gap in their format worth noting: `publisher` is `{name, url}` —
descriptive metadata with **no signature**. The cryptographic provenance below
is ph-reactor's addition, not duplicated work, and could flow back upstream.

## Goals

1. A network can publish a plugin; a node on that network can discover,
   verify and install it.
2. Installation is a deliberate act by the node's operator, never automatic.
3. A plugin's UI is real React using the Powerhouse design system, so it looks
   like the rest of the ecosystem.
4. A compromised or malicious plugin cannot use the console's unauthenticated
   API as the operator.
5. Distribution works peer-to-peer, offline, with no central server.

## Non-goals

- Running existing Powerhouse packages unmodified. Ruled out above.
- A JavaScript engine in the daemon. Models stay declarative.
- A plugin marketplace, ratings, or discovery beyond "what this network carries".
- Automatic updates. A new version is a new install decision.

## Decisions and their rationale

### A package is a document

Packages are documents under a built-in `package` model, so they ride the
sync, replication, history and group-scoping that already exist. No new
distribution logic, and a team's drive carries its own apps: join the network,
the package document arrives, the operator is prompted.

The rejected alternative was new `PackageRequest`/`PackageDef` message types
mirroring model distribution. That duplicates machinery documents already
provide.

### Integrity and authenticity are separate checks

- **Integrity** — the signature matches the content. Automatic, no human.
- **Authenticity** — *is this publisher one I accept?* An operator decision,
  TOFU-pinned exactly as `known_keys` pins peer keys.

Conflating them is the classic supply-chain mistake: a correct hash proves only
that nobody tampered with it *after* publication. The manifest gains
`publisher_key` (ed25519) and `sig` over canonical bytes excluding the
signature — the pattern `Action::message_bytes` and `InviteToken` already use,
so this is the third instance rather than a new invention.

A package may arrive and be verified with no human involved. It is **never
installed** without explicit consent.

### Plugin UI is sandboxed, and capabilities are declared

React UI is executable code from a third party, and the console API has **no
authentication** — the bind address is its only boundary. A plugin editor
loaded same-origin could drive `/api/config`, `/api/drives` and `/api/quit` as
the operator.

Therefore: the editor runs in an **iframe on a separate origin** and cannot
reach the API directly. Every call crosses a `postMessage` bridge that enforces
a **capability allowlist declared in the manifest**:

```json
"capabilities": {
  "read":   ["rfp@1", "proposal@1"],
  "write":  [ { "model": "proposal@1", "kinds": ["init", "withdraw"] } ]
}
```

Those capabilities are shown to the operator at install time in plain language.
Publisher trust decides *whether* a plugin runs; capabilities decide *what it
can touch*. Defence in depth, because a publisher key can be stolen.

### Bundles need a blob transport

```rust
pub const MAX_MSG_BYTES: u32 = 1 << 20;   // 1 MiB
```

ph-reactor has no blob or attachment mechanism, and a React bundle carrying the
design system exceeds 1 MiB. Models and processors are small JSON and sync
fine; **the UI bundle is the part that cannot travel today.**

This design adds a **chunked, content-addressed blob transport**: the bundle is
split into hash-addressed chunks, requested from any peer that has them, and
reassembled and verified against the bundle hash in the manifest. A node that
holds a package can serve it to one that does not.

Rejected alternatives:

- **Fetch the bundle over HTTPS from a pinned URL.** Far less work and
  integrity is preserved by the hash, but it reintroduces a central dependency
  and breaks installation offline — exactly when peer distribution matters
  most.
- **Chunk the bundle into ordinary documents.** Works with today's sync, but
  every byte lands in an append-only log forever with no deduplication, so each
  version permanently bloats the store.
- **Raise `MAX_MSG_BYTES`.** The cap bounds memory against hostile peers;
  raising it to tens of megabytes weakens that for every message type, not just
  packages.

## Architecture

### Package contents

```
manifest         powerhouse.manifest.json + publisher_key + sig + capabilities
                 + bundle: { hash, size, chunks }
documentModels   declarative JSON, registered through the existing path
processors       ActionFilter -> Reaction specs
editors          a built React bundle (design-system based), delivered in chunks
```

### Install flow

```
package document arrives over the mesh
        │
        ├─ verify signature against publisher_key        automatic
        ├─ publisher already trusted? ──no──▶ prompt operator (TOFU pin)
        │                                     show capabilities in plain language
        ├─ operator approves
        ├─ fetch bundle chunks from peers, verify against bundle.hash
        ├─ register models  (persisted -> survives restart)
        ├─ register processors
        └─ mount editor at a plugin route, sandboxed iframe + capability bridge
```

### New components

| Component | Responsibility |
|---|---|
| `src/package/manifest.rs` | Parse, canonicalise, sign and verify manifests |
| `src/package/trust.rs` | The publisher trust store; TOFU pinning |
| `src/package/install.rs` | Lifecycle: install, enable, disable, remove |
| `src/blob/` | Chunk store, content addressing, garbage collection |
| `src/p2p/blob.rs` | `ChunkRequest` / `ChunkData` messages |
| console bridge | `postMessage` capability enforcement for iframes |

### What is reused rather than built

Mesh sync, hash verification, `models.json` persistence, ed25519 signing, TOFU
key pinning, the auth/quorum engine, the processor runner, and the Powerhouse
manifest format and design system.

## The user interface

Plugins are only real when someone can use them, so the interface is part of
this design rather than a follow-up.

### Two surfaces, one seam

| Surface | Stack | Origin |
|---|---|---|
| Host console — daemon control panel, plugin list, install prompts | framework-free, as today | `settings.host:4002` |
| Plugin editors — Achra, knowledge vault, … | React + `@powerhousedao/design-system` | `settings.host:4003` |

They never share a page. The host renders a plugin at `#/plugins/<name>` as an
**iframe** pointing at the asset origin, and that iframe is the entire
security boundary.

This is also why two UI stacks is not the inconsistency it appears to be: the
host is a shell for operating a daemon, plugins are applications. They are
separated by a boundary that has to exist anyway.

### The plugin asset server

A **second listener**, serving static bundle files and nothing else — no API
routes, not one. Port is part of the browser's origin tuple, so
`127.0.0.1:4003` is a different origin from `127.0.0.1:4002` and the
same-origin policy does the enforcement for us.

The separation is **structural, not a filter**: the asset server is a distinct
router with no access to the command channel or the store. The console's 42
API routes remain where they are. This is the same discipline as the narrow
submit endpoint in the Achra design — a boundary someone can misconfigure is
not a boundary.

### The capability bridge

The editor cannot reach the API directly, so every call crosses `postMessage`:

```
editor (iframe, :4003)                host console (:4002)
  ph.query("rfp@1", {status:"open"})
        │  postMessage {id, op:"query", model, filter}
        ▼
                          check: is "rfp@1" in the package's read allowlist?
                          no  -> {id, error:"capability not granted"}
                          yes -> GET /api/query?model=rfp  -> {id, result}
```

Writes carry the reducer kind, checked against the write allowlist, and are
then subject to the model's own `auth`, `pre` and quorum rules exactly as any
other action. The bridge narrows what a plugin may attempt; it never widens
what the store permits.

The host is the only component that talks to the API. A plugin holds no
credentials and cannot construct a request the bridge did not mediate.

### The editor SDK

Plugin authors import a small client that wraps the bridge, so nobody
hand-writes `postMessage` plumbing:

```ts
import { useQuery, useSubmit } from "@powerhousedao/ph-reactor-sdk";

const rfps = useQuery("rfp@1", { field: "status", value: "open" });
const submit = useSubmit();
await submit("proposal@1", "init", { name, rfp_ref, summary, amount });
```

Deliberately shaped after `reactor-browser`'s hook vocabulary so the mental
model transfers for anyone who has written a Powerhouse editor, even though the
transport underneath is different.

### Host console additions

- **Plugins** in the sidebar: installed packages, and packages seen on the mesh
  but not installed.
- **Install prompt** stating the publisher, whether that key is already trusted,
  and the requested capabilities in plain language — "read RFPs and proposals;
  create and withdraw proposals" — not a JSON blob.
- **Plugin routes** rendering the iframe, with the plugin's name and publisher
  visible in the frame so a page can never impersonate the console itself.

### One decision left open

The host console stays framework-free. It is a daemon control panel, the
existing policy is deliberate, and the iframe boundary means nothing is shared
with plugin UI anyway. **Recommendation: keep it that way**; if the console
should instead be rebuilt on React and the design system, that is a separate
piece of work with its own justification, not a side effect of adding plugins.

## Implementation sequencing

Each slice is independently useful, which is what makes this safe to stage:

1. **Blob transport** — chunked, content-addressed, with garbage collection.
   Useful on its own; everything else depends on it.
2. **Package format + trust** — manifest, signing, verification, the publisher
   trust store and the install lifecycle. At this point packages distribute and
   verify with no UI.
3. **Asset server + bridge** — the second listener and capability enforcement.
   The highest-risk slice; it is where the security tests live.
4. **Host console plugin views** — list, install prompt, plugin routes.
5. **Editor SDK + the first real editor** — Achra's marketplace UI, which is
   also the proof that the whole path works end to end.

## Testing



| Level | What |
|---|---|
| Unit | Manifest canonicalisation is stable; a tampered manifest fails verification |
| Unit | An unknown publisher is refused, never silently trusted |
| Unit | Capability enforcement: a call outside the allowlist is rejected |
| Unit | Chunking round-trips; a corrupted chunk fails its hash and is refetched |
| Integration | Two reactors: publish on one, verify and install on the other with no HTTP |
| Integration | Install offline from a peer that has the bundle, with no internet |
| Integration | A plugin's models survive a restart (they go through `models.json`) |
| Security | The iframe cannot reach `/api/config`, `/api/drives` or `/api/quit` — asserted, not assumed |
| Security | The asset server exposes no API route at all — assert the route list, as `validate.sh` asserts the console has no Ingress |
| Security | A bridge call outside the declared capabilities is refused, and refusal is logged |
| UI | The install prompt renders the requested capabilities in plain language, not raw JSON |
| UI | An editor renders, queries and submits through the bridge against a live reactor |
| Security | A package signed by an untrusted key is never installed, even if its hash is valid |

## Risks

| Risk | Mitigation |
|---|---|
| A stolen publisher key installs a malicious plugin everywhere | Capability allowlists limit blast radius; install is per-node and explicit |
| The iframe sandbox is subtly wrong and leaks API access | It is the highest-risk component; tested by asserting the negative, and the capability bridge is the only path in |
| Blob transport becomes a DoS vector (peers requesting endless chunks) | Chunk size and in-flight limits, reusing the existing `MAX_MSG_BYTES` discipline |
| Chunk store grows without bound | Garbage-collect chunks not referenced by an installed package |
| Divergence from Powerhouse's format as it evolves | Adopt their manifest verbatim and add fields rather than fork it; feed the signature work upstream |
| Plugin UI expects Powerhouse document-model semantics | Documented non-goal: editors are written against ph-reactor's API, using the design system for presentation only |

## As built — where reality differed from the design

The design held. Five things it did not anticipate, each found by running the
thing rather than by reading it, and each now covered by a test:

**`package@1` is a built-in model, not a shipped definition.** The design said
"packages are documents" without naming what carries them. It cannot be a
distributed definition, because packages are how definitions are distributed —
so the carrier is built into the binary alongside `open@1` and `group@1`.

**A sandboxed iframe needs `allow-forms`.** Without it Chrome blocks the submit
*event*, not merely the navigation, so an editor whose form calls
`preventDefault()` and posts over the bridge silently does nothing — no
exception, no request, no clue. Granting it changes nothing about isolation,
because the bundle's own CSP sets `form-action 'none'`, so the browser still
refuses to send a form anywhere. `allow-same-origin` stays absent, and a test
asserts all three facts together.

**Garbage collection has two roots, not one.** Collecting by "what is installed"
deleted the chunks of a package *this node had published*, leaving it
advertising a bundle it could no longer serve. The second root is every bundle
named by a `package@1` document this node carries: an offer is a promise to
serve.

**The bridge identifies its caller by window, not by origin.** A sandboxed frame
reports origin `"null"`, which is not addressable as a `postMessage` target and
is not unique to any one frame. `ev.source === frame.contentWindow` is the check
that means something, and the reply necessarily goes to `"*"` — safe, because
`postMessage` delivers to that window and no other.

**A published version is immutable, so publishing over one is a conflict.** The
signature covers the content and peers may already hold it, so republishing the
same `name@version` is refused with that reason rather than with the store's
internal duplicate-document error.

### What shipped

| Piece | Where |
|---|---|
| Built-in `package@1` model | `src/model/package.rs` |
| Package API: publish, list, install, uninstall, trust | `src/settings/packages.rs` |
| Isolated asset origin (console port + 1) | `src/settings/assets.rs` |
| Capability-checked bridge endpoints | `src/settings/mod.rs` |
| Editor SDK (`useQuery` / `useSubmit`) | `packages/sdk/ph-reactor-sdk.js` |
| The Achra marketplace editor | `packages/achra/` |
| Build and publish a package | `scripts/publish-package.sh` |
| Console: plugin list, install prompt, plugin routes, bridge | `console/v2.html` |
| End-to-end lifecycle tests | `tests/package_lifecycle.rs` |
