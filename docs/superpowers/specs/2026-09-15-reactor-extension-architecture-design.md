# Extension architecture: reducers, views and services — Design

ph-reactor can store, sign, replicate and verify documents. It cannot host an
application. This design adds the three extension points a real Powerhouse
package turns out to need, each matched to a different trust level, and says
plainly which one is deferred.

## Problem

A subscription is not a processor. `src/processor.rs:74` gives a processor
exactly four reactions — `Log`, `Run`, `Emit`, `CreateDoc`. It can write to the
log, shell out, record a fire, or create a document. **It cannot hold state,
and nothing can query it.** The change feed gives you an arrow with nothing at
the end of it.

That is not a theory. Three independent production packages were read for this
design, and all three have the same shape:

| package | models | editors | processors | subgraphs | what the extensions touch |
|---|---|---|---|---|---|
| `bai-knowledge-note` | 12 | 12 | graph indexer (2.2k) | 1 (1.2k) | SQL index + vector embeddings |
| `vetra-cloud-package` | 1 | 1 | 1 | **6 (18k)** | k8s API, GitHub App keys, S3, Postgres |
| `dtbau-package` | 21 | 7 | 5 (5.5k) | 5 (7.1k) | pglite+kysely, glTF, Dalux, Speckle, SharePoint |

The architecture they share:

```
event-sourced doc store → processor → embedded SQL (pglite + kysely) → subgraph (GraphQL) → client
```

`dtbau-read` is the clearest example: a *processor* of that name reads four
document models and writes into pglite via kysely, and a *subgraph* of the same
name serves queries over that same pglite. ph-reactor has none of the
right-hand side.

### And the read path will not survive an application

Independent of any port, measured from the code:

- `query_docs` (`src/query.rs:55`) iterates every document id and calls
  `full_state(id)` per document — and `full_state` takes `self.inner.lock()`
  **separately each time**. One query is O(docs) mutex acquisitions plus a full
  clone of every document's field map, then filters in Rust. There is no index;
  a field-equality filter is a scan.
- `summary_for` clones every clock, then calls `may_peer_read` per document —
  another lock per document.
- `/api/inbox` runs every attention rule against a full scan.
- `installed_rules` re-reads and re-parses `packages.json` **from disk on every
  request**; `projection::spawn` does it **on every change**.
- There is no push feed to clients, so the SDK polls: 3s in Chat, 4s in Drive,
  5s in Achra. Every poll is a full scan. Three open plugins means the daemon
  scans the whole store roughly once a second, permanently.

## Who this is for

Three audiences with genuinely different requirements, which is why one
extension mechanism cannot serve them:

- **A model author** wants types, tests and refactoring.
- **A processor author** wants npm, SQL and a language they already use.
- **An operator** wants to know what a thing may read, write and reach before
  it runs — and wants the daemon to keep working when an extension does not.

## The three tiers

| tier | what it is | runtime | trust required | cost of being wrong |
|---|---|---|---|---|
| **1 · Reducers** | how an action changes state | declarative L1, TS-authored | **none** — it is data, auto-fetched from peers | permanent divergence; unverifiable history |
| **2 · Views** | indexes and derived state inside the daemon | WASM, no network | operator install, capability-disclosed | a stale index; rebuild it |
| **3 · Services** | processors and subgraphs with real I/O | separate process, own storage | **full** — deliberately-run trusted code | an outage, not corruption |

The tiers are not a hierarchy of power; they are a hierarchy of *consequence*.
Everything else in this document follows from that column on the right.

## Tier 1 — reducers stay declarative, and get real tooling

### Why declarative is not negotiable here

`src/p2p/mod.rs:1478`: when a node receives an action for a model it does not
have, it **requests the definition from a peer, checks name/version/hash, and
registers it automatically.** No operator decision, no prompt. That is the
mechanism by which an app travels over the mesh at all, and it is safe only
because a definition is *data interpreted by one fixed engine*. The same line
with a WASM module is "download and execute a binary from an untrusted peer",
and the hash proves only that it is the binary the action named.

Two further consequences of making reducers code:

- **Every module build would have to be archived forever.** A model's content
  hash is pinned into every action and `find()` refuses a mismatch. `doc verify`
  on a two-year-old document requires the exact module that wrote it. Rust
  builds are not reproducible by default, so every recompile mints a new model
  version. Today that artifact is a 24KB JSON file you can diff by eye — and it
  still bit us twice (the manifest `ui` field, then group→space).
- **Replay gets expensive where it must stay cheap.** `verify` re-reduces an
  entire log; the canonical fold re-reduces on every out-of-order arrival.

And `quorum` / `space-member` are *declared by the model and checked by the
store*, specifically so a model cannot lie about its own authorization. Code
reducers create constant gravity to move auth inside the module, after which
authorization is unreadable.

### The complaint is real, and it is about tooling

The objection "it is only a template engine, not real code" deserves evidence
rather than argument. All five reducer files of `knowledge-note` — the model its
package is named after — total **292 lines**, and across all of them:

- external library imports: **zero** (only TS types and error classes)
- `Date.now`, `new Date`, `Math.random`, `fetch`, `require`, dynamic `import`:
  **none**

`updatedAt` is passed in through the payload rather than read from the clock:
the authors already impose purity by hand, with nothing enforcing it. What the
292 lines do is assign fields, push to arrays, guard status transitions, check
one length limit, check a field-name whitelist, and perform one string splice.

One of those guards is *weaker* than its declarative equivalent.
`approveNoteOperation` refuses self-approval by comparing
`state.provenance.author === action.input.actor` — where `actor` is a **string
the caller supplies**, defeated by typing someone else's name. The L1 form
compares against `$actor`, the key that signed the action, and the store
enforces it before reduce on one path.

So the gap is not power. It is that L1 offers no types, no autocomplete, no
refactoring and no test runner, and "it is safer" is not an answer to that.

### What changes

1. **A TypeScript authoring layer.** A typed builder that emits an L1
   definition. Real code, real types, vitest — and the artifact that travels
   the mesh is still a hashable, auditable definition. This is the answer to
   the tooling complaint and it changes nothing at runtime.
2. **Two new write operations**, which are the only two things `knowledge-note`
   genuinely cannot express:
   - `splice` — `{offset, removeCount, insert}` on a string field.
   - a **dynamic write target** — the field named by the payload. The
     whitelist lives in the reducer's own `writes` entry (e.g.
     `{"$payload.field": {"set": "$payload.value", "among": [...]}}`), so the
     complete set of fields a reducer can write is still readable in the
     definition. A payload naming a field outside the list is rejected before
     reduce, like any other precondition failure.

   L1's ops today are exactly `set`, `append`, `remove`; field types are
   `string`, `number`, `boolean`, `object`, `array`, `string[]`, `number[]`.
   Everything else in the 12 knowledge models already maps — nested
   `provenance` as an `object` set, `links`/`topics`/`lifecycleEvents` as object
   arrays, and the DRAFT→IN_REVIEW→CANONICAL machine as `field-is`
   preconditions, which makes the state machine legible in the definition
   instead of spread across five `if` statements.

**L1 grows when an application proves it needs an operation, and not before.**
These two are earned; the next one must be too.

## Foundations — the fast path, and the feed services need

Deliberately not numbered as a tier: it adds no extension point. It is the
prerequisite for tier 3, because the feed is what a service consumes.

- **One lock per query, not one per document.**
- **Field indexes maintained on the single apply path** — and deliberately
  **node-local, never part of the model definition**. Putting `indexes` in the
  definition would change its content hash and break every existing document.
  An index is a local performance decision, not part of a model's identity.
- **Cursor and limit on query.** It returns everything today.
- **Cache the installed-package set** with an mtime check.
- **A cursored, ordered, space-filtered change feed** on the console API (SSE).
  `Store::subscribe_changes` already exists and nothing exposes it outward.
  Live-only is not enough: a processor that goes offline must catch up, so the
  feed replays from a cursor. The action log makes that natural.

## Tier 3 — services

The evidence says the daemon should **not** provide a database. Every processor
in all three packages is already written against pglite and kysely with its own
schema and migrations; any substrate the daemon invents — KV, SQLite, or
pglite-in-wasmtime — forces a rewrite of working code and buys nothing.

A service keeps its own storage. The daemon provides four things:

1. **The cursored change feed** from the foundations work, authenticated and space-filtered.
2. **A scoped write API** so a service can write documents back.
3. **A service identity** — its own ed25519 keypair. Everything it writes is
   signed and attributable instead of "the reactor did it".
4. **Endpoint proxying** at `/api/services/<name>/*` through the console. An
   editor's CSP is `connect-src 'none'`, so a plugin can only reach anything
   via the host bridge; routing services through it means no CORS, no second
   origin, and the space scope travels with the call.

With that, the knowledge graph indexer (~3.4k lines), the Vetra subgraphs (18k)
and the dtbau subgraphs (7.1k) port with their SQL, kysely, octokit, k8s and
glTF code substantially intact. Only their input changes.

### A service is trusted code and must be installed like it

A package arrives over the mesh and is sandboxed. **A service is not a
package.** It holds cloud credentials and its mistakes are outages, so
installing one is a deliberate, local, visible act — never something that
arrives from a peer.

It still declares a scope: which models it may read, which reducers it may
invoke, in which spaces. The daemon enforces that on the same path it already
enforces plugin capabilities. Powerhouse subgraphs today run with ambient
reactor privilege and no declared scope; **declaring the scope and signing the
writes is the one place this design is strictly better than what it replaces.**

## Tier 2 — WASM views, deferred

A host ABI is the most novel and least reversible thing here: designed once and
kept stable for years. The evidence says nobody needs it yet — field indexes
belong natively in the store (tier 0), and real compute belongs in a service
with npm (tier 3).

It is documented rather than built, and the trigger for revisiting it is
concrete: **something that must run inside the daemon, cannot hold credentials,
and is too hot for a round trip.** If that arrives, the ABI is an ordered KV
with prefix scan — small and stable — because real SQL lives in tier 3.

## What this means for the knowledge-vault port

Not part of this spec, but the reason it exists, and it is now much cheaper:

- 12 document models → L1 definitions, TS-authored, using `splice` and dynamic
  targets.
- Graph indexer + subgraph → **one tier-3 service**, keeping its SQL and its
  embedder. Embeddings need network, which settles the tier by itself.
- 12 React editors → single-file bridge editors. **This is the real remaining
  work** and it is a rewrite, not a port: `@powerhousedao/reactor-browser`
  hooks and the design system do not exist inside the sandbox.
- Live documents must be migrated off Switchboard, and every
  `powerhouse-knowledge` skill that drives it through the Switchboard CLI needs
  repointing. That is the cost of the move and belongs in a decision, not a
  discovery.

## Risks

| # | risk | handling |
|---|---|---|
| 1 | A service is trusted code; a compromised one can write anything its scope allows | Declared scope enforced by the daemon, its own signing identity so every write is attributable, and install is local and deliberate — never over the mesh |
| 2 | "No Node, no npm, no child processes" is in the README; services contradict it | The claim is that you do not need Node *to run a reactor*. A service that provisions Kubernetes is not the reactor. The daemon stays self-sufficient for document work; services are optional. The README must say this rather than be quietly falsified |
| 3 | A service that is down silently stops maintaining its read model | Cursor position is observable per service; a stale cursor is a reportable state, not an invisible one |
| 4 | Field indexes drift from documents | Built on the single apply path that everything already goes through, and rebuildable from the log like any derived state |
| 5 | A resumable feed lets a service read history it should not | The feed is filtered by the same predicate as replication, and a service's scope names its spaces |
| 6 | The TS authoring layer diverges from what L1 accepts | The builder emits a definition that is then loaded by the real `L1::from_def`; a round-trip test is the gate |
| 7 | Adding L1 operations tempts unbounded growth | Each addition requires a named application that cannot be expressed without it. Two are earned; the bar does not move |

## Non-goals

- WASM or JavaScript reducers. Documented as an escape hatch in tier 2's
  discussion, deliberately not built. If it is ever built, the coherent form is
  QuickJS-in-WASM with no clock, network or randomness, and a distribution
  class that is never auto-fetched from a peer.
- GraphQL in the daemon. A service may speak GraphQL; the daemon does not.
- Replacing Switchboard. Services are an extension point, not a migration plan.
- Multi-tenant service hosting. One node, one operator, as with the console.

## Open questions

- Does a service authenticate with its own keypair over the loopback API, or
  does the operator issue it a token? A keypair makes its writes attributable
  for free, which argues for the keypair.
- Should the change feed carry full document state per change, or ids the
  service fetches? Full state is simpler and matches `DocChange`; ids are
  cheaper on a busy store.
- Do services get their own tier in the console's "This node" region, beside
  Software, or are they listed as a kind of software?
