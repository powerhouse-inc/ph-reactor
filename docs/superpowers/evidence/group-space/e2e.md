# Group shared space — two-daemon E2E

Verification that a `group@1` behaves as a shared space (channel + drive) and
that an invite grants group membership across the mesh. Two real daemons on
loopback, isolated state dirs, mDNS/DHT off so they connect only via the
invite's explicit dial.

## Setup
- A: `--state-dir /tmp/ph-e2e-a`, listen `127.0.0.1:4203`, settings `:4004`.
- B: `--state-dir /tmp/ph-e2e-b`, listen `127.0.0.1:4202`, settings `:4003`.
- Both `p2p.mdns=false`, `logLevel=debug`.

## Flow (all via the settings API)
1. `POST /api/groups` on A: create `core` (A is seeded as member+manager).
2. `POST /api/invite {groups:["core"]}` on A: mint a grant-giving invite.
3. `POST /api/join {invite}` on B: B pins A (TOFU), dials it, and the
   handshake carries B's join-proof; A resolves the grant and adds B to `core`.
4. Verify B is a member of `core` on **both** A and B (the grant replicated).
5. `POST /api/groups/core/action {kind:post, payload:{text,channel}}` on B →
   A sees the message (member-gated, signed, replicated).
6. `POST /api/groups/core/drive {title,body}` on A → a `note@1` file added to
   the drive; B sees it (member-gated read).
7. A posts a message (A is a member too) → B sees it.

## Results
| assertion                | A side | B side |
|--------------------------|--------|--------|
| B is a member of `core`  | yes    | yes    |
| `core` manager = creator | yes (A)| yes (A)|
| B's channel message      | received | local  |
| A's channel message      | local    | received |
| A's drive file "Q3 plan" | present  | present  |

All five assertions passed.

## The bug this caught
Before the fix, `POST /api/groups` seeded the creator as a **manager only**.
The channel `post` and drive `add-doc` reducers are **member-gated**, so the
creator could not post or add files to its own group (the drive add returned
`400 actor … is not in 'members'`). `POST /api/groups` now seeds the creator
as a member **and** a manager (a manager is always a member). The model unit
test `manager_not_in_members_cannot_post` pins the underlying contract.

## Suite
- `cargo test`: 101 tests, 0 failures (95 lib + 1 dht discovery + 2 invite/join
  + 3 two-engine sync; 11 of the lib tests are the new group-model permission
  matrix + write contracts).
- `cargo clippy --all-targets -- -D warnings`: clean.
- `cargo fmt --check`: clean.
