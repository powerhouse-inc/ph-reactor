# Achra MVP on ph-reactor — Design

## Problem

Achra is positioned as "The Marketplace For Global Coordination": organizations
publish objectives, builder teams submit structured proposals, work is awarded,
and milestone payouts follow. Its own site names **Renown**, **Vetra** and
**Powerhouse** as its stack.

The question this design answers is what it takes to build that loop on
`ph-reactor`.

### What already exists (verified, not assumed)

`rfp-hub.vetra.io` answers HTTP 200 today, backed by a switchboard whose schema
already carries an Achra-shaped spine:

| Model | Observed fields |
|---|---|
| `GrantSystem` | — |
| `GrantPool` | Publisher, Reviewer, Submitter, FundingAmount, ContextDocument |
| `GrantApplication` | Submitter, FundingAmount, Social, Payout, PayoutAddress |
| `Governance` | Policy, Rfc, PublisherDecision, Dispute |
| `Project` | ProjectSocial, RelevantPool |

That is *org posts objective → builder applies → decision → payout*, running in
production.

### Why that does not settle the question

**ph-reactor and that stack are different runtimes with no interoperability.**
ph-reactor contains no GraphQL and no switchboard code — its README is explicit
that "the daemon *is* the reactor". Its models are JSON reducer definitions over
ed25519-signed actions synced with libp2p; the Grant models are TypeScript
document models behind GraphQL. Nothing bridges the two action formats.

Building Achra on ph-reactor therefore means re-expressing the domain, not
reusing the existing implementation. That was chosen deliberately, for what
ph-reactor uniquely provides: offline-first operation, peer-to-peer sync with no
central server, a cryptographically auditable log, and quorum-gated governance.

### What this costs less than expected

Two findings materially reduce the estimate:

- **Domain models are data, not code.** `POST /api/models/register` registers a
  model definition at runtime. The Achra models need no Rust changes and no
  redeploy.
- **A workflow primitive already exists.** `src/processor.rs` provides
  `ActionFilter` → `Reaction` → `Fire`: subscriptions on document changes that
  trigger actions. Achra's "Atlas — contracts as self-running workflows" has a
  foundation here.

The dominant cost is the **user interface**. ph-reactor's console is a
deliberately framework-free control panel for a daemon, not a marketplace.

## Goals

1. A pilot organization publishes an RFP, several builders submit proposals, the
   org awards one, and milestones are tracked to a recorded payout — end to end,
   on ph-reactor, usable by people other than its authors.
2. Participants may use a **browser or their own node**, by choice, without
   changing their identity.
3. Competing proposals stay confidential.
4. No changes to signature, membership or quorum semantics.

## Non-goals (deliberately cut from the MVP)

- **Settlement.** Milestones record completion and a payout *intent*; no
  stablecoin movement, escrow or on-chain integration. An MVP that records
  agreements truthfully is demonstrable; one that moves money is a different
  risk class and belongs in its own spec.
- **Tax reporting** (1099/W-9), **runway tracking**, and the **operator services
  catalog** (Legal/Finance/People Ops). All are real Achra features; none are
  needed to prove the coordination loop.
- **Replacing rfp-hub.** This runs alongside it, not over it.
- **Reputation.** Renown exists as a separate service; binding proposals to
  verified contributor history is a later slice.

## Decisions and their rationale

### Identity is a portable ed25519 key; browser and node are just containers

A libp2p ed25519 peer id is derived from the public key — `peer_id_of()` is
`kp.public().to_peer_id()`. A browser that generates an ed25519 keypair can
therefore derive **the same peer id**, without running a node.

The consequence is the single most important property of this design:
**membership, `auth` blocks, quorum arithmetic and the `known_keys` registry all
work unchanged.** Actors are peer ids either way; some peer ids simply have no
daemon behind them. Approach 3 (org on a node, builders in a browser) initially
looked like surgery on the security core; it is not.

It also makes identity portable: a builder can start in a browser and later
import that key into a node at `<state>/key`, keeping the same actor id and their
entire history. That exact path is already proven — the bootstrap node's identity
is materialised into that file from OpenBao.

**This rests on an unverified assumption** and is the first thing to test: that
JavaScript-side ed25519 key handling and peer-id derivation produce byte-identical
results to Rust. If it does not hold, the design changes substantially. See
*Spikes*.

### Confidentiality by group topology, not by encryption

`authorize(state, action)` runs on **actions** — writes. There is **no read
gating**, and sync converges whole documents to every group member. A builder
running a node would therefore receive every competing proposal, which is
disqualifying for sealed bids.

Resolved with the primitives that already exist rather than new cryptography:

```
group "achra"                     public marketplace
  members: orgs + all builders
  documents: rfp, org profile, builder profile, award

group "engagement-<org>-<builder>"   one per relationship
  members: the org + that one builder
  documents: proposal, agreement, milestone
```

A builder syncs the public marketplace plus only their own engagements. The org
is a member of every engagement group, so it sees all bids; no builder sees
another's.

Rejected: encrypting proposal bodies. ph-reactor has no encryption today, so that
means designing key management, rotation and recovery into the core — a security
workstream in its own right, for a problem that group topology already solves.

Cost accepted: more groups to manage, and awarding must reference across groups.

### A narrow public endpoint, separate from the console

The console API has **no authentication**; the whole system treats the bind
address as the security boundary, and it is loopback-only by default. A hosted UI
cannot use it.

The MVP adds a **second, restricted HTTP surface** that may be exposed publicly
and accepts exactly two things:

1. a fully-formed, signed `Action` (the `/api/submit` path, which already exists
   for co-signing and already verifies signature, membership, preconditions and
   quorum), and
2. actor key registration (below).

It must expose nothing else — no `/api/config`, `/api/drives`, `/api/quit`. This
is the most security-sensitive component in the slice: the submit path is safe
because every action is signed and authorised, but the surrounding API is not,
and the separation must be structural rather than a filter someone can
misconfigure.

### One client, two backends

Because signing happens client-side, the marketplace UI is the same code whether
it submits to the user's own local reactor or to the hosted endpoint. Only the
target URL differs. This is what lets a builder move from browser to node without
changing tools or identity.

## Architecture

### Documents

Registered at runtime; no recompile, no redeploy.

```
rfp         publisher, title, brief, budget, currency, deadline,
            quorum_above,      award amount above which two members must
                               co-sign; 0 means every award needs a quorum,
                               and omitting it means none does
            status: open | awarded | cancelled
proposal    rfp_id, submitter, summary, amount, milestones[],
            status: submitted | withdrawn | accepted | declined
agreement   rfp_id, proposal_id, org, builder, total,
            status: active | completed | terminated
milestone   agreement_id, title, amount, evidence,
            status: pending | submitted | accepted
award       rfp_id, builder, amount        (public, in the marketplace group)
```

`award` exists so the marketplace can show an outcome without revealing the
losing bids, which live in engagement groups the public group cannot see.

### Authorization

Expressed in the models' `auth` blocks and `pre` conditions — the mechanisms the
`group` model already uses:

| Action | Gate |
|---|---|
| publish an rfp | actor is a member of the marketplace group |
| submit a proposal | actor is the submitter and a member of that engagement group |
| award | actor is the rfp's publisher |
| award above `rfp.quorum_above` | quorum ≥ 2 distinct members — the co-signing shipped on 2026-09-14 |
| accept a milestone | actor is the org party to the agreement |

### Flow

```
org (node)            marketplace group          builder (browser or node)
  │                         │                          │
  ├─ publish rfp ──────────▶│◀───────── browse rfps ───┤
  │                         │                          │
  │   engagement group (org + this builder only)       │
  ├─────────────────────────┼◀──────── submit proposal ┤
  ├─ review, award ────────▶│                          │
  ├─ create agreement ──────┤──────────────────────────▶
  │                         │                          │
  ├─ accept milestone ◀─────┼───── submit evidence ────┤
  │                         │                          │
  └─ publish award ────────▶│  (public; bids stay private)
```

### Deployment

The marketplace UI deploys as its own tenant through `powerhouse-chart` — the
same shape `rfp-hub` already uses (`app.enabled`, Traefik ingress, cert-manager).
It talks to the narrow endpoint on the bootstrap node, which is already public,
already has a stable peer id, and already serves as the mesh rendezvous.

## Spikes — RESOLVED 2026-09-14, both pass

Both were run before implementation. Results:

**1. Cross-language identity — PASS.** A Node script derived
`12D3KooWPTgWt8RdEdD23qDpkUM3vNeXebm7u2E7XAvhWi5Htdeg` from the bootstrap
node's real 32-byte seed, identical to the peer id Rust produces. The
derivation is `base58btc(multihash(identity, protobuf(PublicKey{Ed25519,
pubkey})))`. Browser actors can therefore be group members with no change to
membership, auth or quorum.

**2. Canonical signing bytes — PASS, with a required constraint.** JS
reproduces `Action::message_bytes()` byte for byte, **but only when the payload
is serialised with sorted keys.** Rust serialises a `Value::Object` from a
BTreeMap, so its JSON is key-sorted; `JSON.stringify` preserves insertion order
and produces different bytes, which would make every browser signature fail.

The client must therefore use canonical (sorted-key) JSON. This is not a
preference — it is a correctness requirement, and it is the single easiest way
to break the browser half by accident.

`action::wire_format_tests::message_bytes_golden_vector` pins the exact bytes
so a change to the canonical form fails a test instead of silently invalidating
every signature in the fleet.

### Original statement of the spikes

Both were cheap and both could have invalidated the design:

1. **Cross-language identity.** A JS-generated ed25519 key must derive the same
   peer id as Rust, byte for byte. If not, browser actors cannot be group members
   and the identity model must change.
2. **Canonical signing bytes.** JS must reproduce `Action::message_bytes()`
   exactly — it excludes `sig` and `cosig`, and any field-ordering or encoding
   difference makes every browser-signed action fail verification.

Neither is exotic, but the entire browser half of the design depends on them, so
they come first and their result is reported before anything is built on top.

## Testing

| Level | What |
|---|---|
| Unit | Model reducers: state transitions, and that each `auth` gate refuses the wrong actor |
| Unit | Awarding above the threshold without a quorum is refused; with one, applies |
| Integration | Two reactors, two engagement groups: builder A cannot observe builder B's proposal |
| Integration | A browser-signed action verifies and applies on a reactor |
| Integration | Identity portability: export a browser key into a node; the actor id and history are unchanged |
| Security | The public endpoint exposes only submit and register — assert every other route is absent, the way `validate.sh` guards the console having no Ingress |
| E2E | The full loop of the Goals section, driven through the UI |

## Risks

| Risk | Mitigation |
|---|---|
| JS/Rust identity or signing mismatch | Spikes 1 and 2, before anything depends on them |
| The public endpoint widens over time until the unauthenticated API is exposed | Structural separation, plus a test asserting the route list |
| Group-per-relationship multiplies groups | Acceptable at pilot scale; revisit if a pilot exceeds ~dozens of engagements |
| A builder loses their browser key and their identity with it | Export/import is part of the UI from the start, not an afterthought; the node import path already exists |
| Building the domain twice (here and in the switchboard stack) | Accepted knowingly. The two are not compared feature-for-feature here — that was considered and rejected as a goal — so the duplicated effort buys a runtime evaluation, and that should be stated plainly to anyone funding it rather than discovered later |
| Scope creep back toward settlement | Non-goals are listed above and payouts are recorded as intent only |
