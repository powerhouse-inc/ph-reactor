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

So the gap is not power. It is that L1 offers no validation feedback until you
load a definition, and no way to exercise a reducer without writing Rust — and
"it is safer" is not an answer to that. Note what the gap is *not*: nobody
needs a nicer syntax for the JSON. They need to know it is wrong before
shipping it, and to be able to try it.

### What changes

1. **A JSON Schema and two CLI verbs** — *not* an authoring layer.

   A typed TypeScript builder that emits L1 was considered and **rejected**. It
   would put Node, npm and a build step inside a Rust project whose pitch is
   not needing them, create two representations to keep in sync forever, and
   leave you debugging generated JSON rather than what you wrote. It buys
   autocomplete on a 24KB file — and it does not even answer the complaint it
   was meant to answer, because a DSL that emits JSON is not "real code in a
   reducer" either. It pays the full cost and lands nowhere near the ask.

   Plain JSON also happens to be the right format for who writes these now:
   models are increasingly authored by agents, and a schema-backed JSON object
   is the most tractable thing for that. A bespoke DSL is a language with one
   user and no training data.

   What closes the actual gap:

   - **A JSON Schema for L1** — autocomplete and inline errors in any editor,
     no build step, nothing to keep in sync, and it doubles as the reference.
   - **`ph-reactor model check <file>`** — runs the real `L1::from_def`, so
     validation *is* the loader rather than a second implementation that can
     drift from it.
   - **`ph-reactor model try <file> --action <kind> --payload '{…}'`** — apply
     actions to a scratch document and print the resulting state, or the
     precondition that refused. The reducer test loop, without writing Rust.

   `/api/models` and `/api/models/register` already exist, so the CLI is a thin
   wrapper over machinery that is in the tree.
2. **One new write operation.** Of the two gaps `knowledge-note` appeared to
   have, only one is real:
   - `splice` — `{offset, removeCount, insert}` on a string field.
   - ~~a dynamic write target~~ — **dropped.** `setMetadataField(field, value)`
     exists only because one model does duty for many note kinds:
     `knowledge-note` carries `scope`, `confidence`, `severity`, `editor`,
     `modelId`, `modules`, `computes`, `inputs`, `outputs`, `consumedBy`,
     `context`, `alternatives`, `consequences`, `decisionStatus`, `sourceType`,
     `targetType`, `relationType`, `cardinality`, `errorMessage`, `rootCause`
     and `correctPattern` as optional fields behind a `noteType` discriminator,
     with a hand-maintained `STRING_METADATA_FIELDS` whitelist to police them.
     That is several models wearing one name. Since we are rewriting rather
     than transliterating, the fix is to split them — or to carry the variable
     part as a single `object` field written whole. Either way L1 needs nothing
     new.

   So **L1 gains exactly one operation, `splice`.**

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

**The old reactor is not being kept, and rewriting is accepted.** That removes
code reuse as an argument, so the case for services has to stand on its own —
and it does, on marginal cost.

Some extensions cannot be sandboxed at all: `vetra-github-auth` signs GitHub
App JWTs, `vetra-cloud-secrets` calls the Kubernetes API, `vetra-cloud-observability`
presigns S3 URLs, and embeddings need an HTTP call. A WASM module with no
sockets cannot do any of it, and granting it ambient network defeats the
sandbox. So a trusted, networked runtime has to exist regardless.

Once it exists, a read model costs nothing extra to run there — whereas putting
read models *inside* the daemon means designing a host ABI, a fuel and memory
regime, SQL or KV host calls, and rebuild machinery. The marginal cost of one
more service is near zero; the marginal cost of tier 2 is a new runtime surface
kept stable for years.

So the daemon provides **no storage substrate for extensions**. A service brings
its own database and the daemon does not care which.

A service keeps its own storage. The daemon provides four things:

1. **The cursored change feed** from the foundations work, authenticated and space-filtered.
2. **A scoped write API** so a service can write documents back.
3. **A service identity** — its own ed25519 keypair. Everything it writes is
   signed and attributable instead of "the reactor did it".
4. **Endpoint proxying** at `/api/services/<name>/*` through the console. An
   editor's CSP is `connect-src 'none'`, so a plugin can only reach anything
   via the host bridge; routing services through it means no CORS, no second
   origin, and the space scope travels with the call.

That is a small and stable surface, and it is deliberately the *whole* of it.
The daemon gains no database, no query language and no plugin runtime for
extensions — which is what keeps this affordable to build and to keep working.

Since these are rewrites rather than ports, a service is free to choose its own
language, database and libraries. What it inherits from this design is not
code: it is a defined input (the feed), a defined way to write back, an
identity, and a way to be reached.

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

## Secrets

Services need real credentials: GitHub App private keys, `kubeconfig`, AWS
keys, Dalux/Speckle/SharePoint tokens, database passwords, and an LLM key for
embeddings. How those are handled is a design decision, not an operational
detail, so it is settled here.

### One principle

**The daemon never holds a secret value. It holds, at most, a reference to
one.**

This is already the convention and it only needs extending. `src/config.rs`
stores `tokenEnv` and `apiKeyEnv` — the *names* of environment variables, never
their contents (`src/config.rs:110,141,177`). That is precisely why
`/api/config` can serialize the entire configuration and leak nothing. The node
identity key follows the same discipline: on the cluster it lives in OpenBao,
is projected in at runtime by the External Secrets Operator, and local copies
were shredded after minting.

### Five rules

1. **No secret in a document. Ever. In any space.** Documents replicate, and a
   protected space is *not* a secret store — every member holds a full
   plaintext copy and keeps it after removal. Private spaces are no better:
   "private" means not replicated, not encrypted, and the WAL and snapshots are
   plain JSON on disk. This rule has no exceptions and no tier.
2. **No secret through the daemon.** A service is a separate process, so its
   credentials go *to it* — never through the daemon and never into the
   daemon's memory. This is a real security gain of the service architecture,
   not an accident: the daemon is the component exposed to the mesh and to an
   unauthenticated console, so a daemon compromise must not yield cloud
   credentials. There is therefore **no secret-broker API**: nothing a service
   can call to ask the daemon for a credential.
3. **Declared by reference, and disclosed at install.** A service manifest
   names what it needs and where it comes from — `{"secrets": [{"name":
   "GITHUB_APP_KEY", "from": "env"}]}` — never a value. Install shows the set in
   plain language beside the scope, the way capabilities, projections and
   attention rules already are: *"this service reads GITHUB_APP_KEY and
   AWS_SECRET_ACCESS_KEY from the environment."* An operator can consent to a
   list of names; nobody can consent to an opaque blob.
4. **Never in a response, never in a log.** `/api/services` reports each
   reference and *whether it resolved* — never the value. The existing practice
   of never printing OpenBao values to a terminal extends unchanged.
5. **Rotation is external.** Because the daemon holds references, rotation
   happens in OpenBao / the k8s Secret and takes effect when the service
   restarts. The daemon's job is to report that a reference failed to resolve,
   not to manage the lifecycle of a credential it cannot see.

### Service authentication, without inventing a secret

A service must authenticate to the daemon, and the obvious designs create a
new secret to manage. They are not needed: **a service generates its own
ed25519 keypair on first run and the operator approves its public key.**

That is the TOFU publisher-trust flow that already exists for packages, applied
to processes — so no shared secret is ever created, transmitted or stored, and
the same keypair makes every write the service performs *signed and
attributable*. A bearer token would be a secret to issue, store, rotate and
leak; a public key is not a secret at all.

### On the cluster

Nothing new is required. A service's secrets are Kubernetes Secrets projected
as environment variables, sourced from OpenBao through the External Secrets
Operator — the same path the node identity already takes. `readOnlyRootFilesystem`
and the existing NetworkPolicy continue to apply, and a service that needs
egress declares it there rather than the daemon widening its own.

Note that `vetra-cloud-secrets` — a 1.9k-line subgraph that manages secrets via
the Kubernetes API — becomes an ordinary tier-3 service under this design. It
holds credentials; the daemon still never sees them.

## Tier 2 — WASM views, deferred

A host ABI is the most novel and least reversible thing here: designed once and
kept stable for years. The evidence says nobody needs it yet — field indexes
belong natively in the store (tier 0), and real compute belongs in a service
with npm (tier 3).

It is documented rather than built, and the trigger for revisiting it is
concrete: **something that must run inside the daemon, cannot hold credentials,
and is too hot for a round trip.** If that arrives, the ABI is an ordered KV
with prefix scan — small and stable — because real SQL lives in tier 3.

## What this means for the knowledge-vault rewrite

Not part of this spec, but the reason it exists, and it is now much cheaper:

- 12 document models → L1 definitions, TS-authored, using `splice` and dynamic
  targets.
- Graph indexer + subgraph → **one tier-3 service**, free to pick its own
  database rather than inheriting pglite. Embeddings need network and an API
  key, which settles the tier by itself.
- 12 React editors → single-file bridge editors. **This is the real remaining
  work** and it is a rewrite, not a port: `@powerhousedao/reactor-browser`
  hooks and the design system do not exist inside the sandbox.
- Live documents are exported once and imported; the old reactor is then
  switched off rather than kept in step. Every `powerhouse-knowledge` skill
  that drives it through the Switchboard CLI needs repointing at the new API —
  known and accepted, not a discovery.
- The 12 models are an opportunity to fix `knowledge-note` splitting into
  distinct types rather than carrying 21 optional fields behind a
  discriminator.

## Risks

| # | risk | handling |
|---|---|---|
| 1 | A service is trusted code; a compromised one can write anything its scope allows | Declared scope enforced by the daemon, its own signing identity so every write is attributable, and install is local and deliberate — never over the mesh |
| 2 | "No Node, no npm, no child processes" is in the README; services contradict it | The claim is that you do not need Node *to run a reactor*. A service that provisions Kubernetes is not the reactor. The daemon stays self-sufficient for document work; services are optional. The README must say this rather than be quietly falsified |
| 3 | A service that is down silently stops maintaining its read model | Cursor position is observable per service; a stale cursor is a reportable state, not an invisible one |
| 4 | Field indexes drift from documents | Built on the single apply path that everything already goes through, and rebuildable from the log like any derived state |
| 5 | A resumable feed lets a service read history it should not | The feed is filtered by the same predicate as replication, and a service's scope names its spaces |
| 6 | The JSON Schema drifts from what `L1::from_def` actually accepts | `model check` validates through the real loader, not the schema, so the schema is an editor aid and never the authority. A test asserts every in-tree model passes both |
| 7 | Adding L1 operations tempts unbounded growth | Each addition requires a named application that cannot be expressed without it. One is earned (`splice`); the bar does not move |
| 8 | A secret reaches a document and replicates to every member of a space, permanently and unencryptably | The rule is absolute and tierless (Secrets, rule 1). A service writing back is scope-limited to declared models and reducers, and no reducer takes a credential-shaped payload |
| 9 | A daemon compromise yields cloud credentials | It cannot: the daemon never holds a secret value and offers no broker API. Credentials exist only in the service process that uses them |
| 10 | Service authentication creates a new secret to manage | It does not. The service generates its own keypair and the operator approves the public key, reusing the package TOFU flow |

## Non-goals

- A TypeScript (or any) authoring layer that compiles to L1. Rejected above
  with reasons, not deferred.
- WASM or JavaScript reducers. Documented as an escape hatch in tier 2's
  discussion, deliberately not built. If it is ever built, the coherent form is
  QuickJS-in-WASM with no clock, network or randomness, and a distribution
  class that is never auto-fetched from a peer.
- GraphQL in the daemon. A service may speak GraphQL; the daemon does not.
- Backwards compatibility with the old reactor. It is not being kept, so
  nothing here preserves Powerhouse interfaces, pglite schemas or the
  Switchboard CLI. Ports are rewrites, and that is the accepted cost.
- A secret-broker API in the daemon. Explicitly rejected: it would put
  credentials in the memory of the process that is exposed to the mesh and to
  an unauthenticated console.
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
