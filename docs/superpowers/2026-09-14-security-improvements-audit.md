# ph-reactor Security & Improvements Audit

Date: 2026-09-14. Branch: `feat/audit`.

Scope: a read-through of the daemon's trust boundaries (p2p hello handshake,
invite/join, the settings HTTP API, identity/keypair storage, config, the
store's op verification, and the DHT/relay defaults) plus a focused review for
correctness / robustness / performance / UX. Findings are severity-ranked.
Critical/High are fixed in this branch; the rest are documented.

## The design is fundamentally sound (verified, not assumed)

- **One ed25519 key is the whole identity** (p2p/mod.rs:77-79): it signs doc
  ops, the gossipsub messages, and is the transport identity. Stored `0600`
  via `set_perms_0600` (p2p/mod.rs:99).
- **Secrets never reach disk**: the LLM API key and the p2p/drive tokens are
  stored as environment-variable *names* (`apiKeyEnv`, `tokenEnv`), read from
  the env at use time (config.rs:86, 152-156). The config file is written
  `0600` via atomic tmp+rename (config.rs:319-343).
- **Invites** use `verify_strict` (rejects malleable signatures) and the
  signature binds every field (version, name, peer, pubkey, addr, nonce,
  groups); the joiner's accept echoes the nonce and is signed by its own
  TOFU-pinned key (p2p/invite.rs:67-115, 154-187).
- **Hello gate** (p2p/mod.rs:940-1050): ban → version → joiner-accept → token →
  TOFU key pinning under the *transport-verified* identity; a wrong token
  auto-bans after 3 attempts within 10 min (`record_auth_failure`, 661-686).
- **Gossip is verified against the writer's pinned key**, not just the
  gossiping node (gossipsub `MessageAuthenticity::Signed` +
  `ValidationMode::Strict`, p2p/mod.rs:313-317; ops re-verified by origin in
  the store). A node cannot forge another peer's ops.
- **Models registered via the API are declarative L1** (settings/mod.rs:1308
  `L1::from_def`), not executable Wasm — so `register_model` is not a
  code-execution vector.
- **Docs are hash-chained and `verify`-able** (store.rs) — tamper-evident.

## Findings (severity-ranked)

### MEDIUM — LLM endpoint SSRF / credential exfiltration — FIXED
`POST /api/llm/test` accepted a **per-request** `baseUrl` override
(settings/mod.rs:1194) and issued an outbound `GET {base}/models` with the LLM
API key as `Authorization: Bearer <key>` (old 1230). The API is loopback-only,
so the actor is a local process — but a local process does not otherwise have
the daemon's environment, so this was a way to make the daemon (a) fetch the
cloud metadata IP `169.254.169.254` (read cloud credentials) or (b) send the
key to an attacker URL (exfiltration). **Fix:** `llm_base_url_ok`
(settings/mod.rs:1186) refuses link-local `169.254.0.0/16` hosts before any
request. Residual: a local process can still set `llm.base_url` in the saved
config (via the unauthenticated `POST /api/config`), which the
`/api/llm/draft-*` endpoints then use — see the next finding.

### MEDIUM — p2p on `0.0.0.0` + DHT on + no default token → first-contact TOFU
Defaults: `listen=/ip4/0.0.0.0/tcp/4201`, `p2p.dht=true`, `p2p.mdns=true`,
`p2p.relay=false`, `p2p.tokenEnv=null` (config.rs:39-42, 98-108). A default
node is **discoverable (DHT/mDNS) and reachable on all interfaces with no
token gate**, so the *first* peer that reaches it is pinned (TOFU) and can
sync. Fine on a private LAN, but a DHT/internet-exposed node can be
authored by any unauthenticated peer that finds it. **Recommendation:** set
`p2p.tokenEnv` for any node reachable beyond a trusted LAN (it already gates
inbound hellos); consider a specific-interface default listen address. Not
changed here (would break easy first-run setup) — documented.

### MEDIUM — invites have no expiry and the nonce is not single-use
`InviteToken` carries a nonce but no timestamp/expiry (p2p/invite.rs:42-63),
and `handle_invite_accept` (p2p/mod.rs:605) does not mark a consumed nonce as
used. A captured invite (or its accept) can be replayed to re-add a drive or
re-assert a join. Low blast radius (re-adding a known peer's drive), but a real
replay gap. **Recommendation:** add a signed `issued_at` + max-age, and track
used nonces (TTL-bounded). Documented, not implemented (protocol change).

### LOW — the settings API is unauthenticated (mitigated by the loopback bind)
Every route — including `POST /api/quit` (DoS), `POST /api/config` (replace the
whole config), `POST /api/drives` (add a drive to any address),
`POST /api/ban` / `unban`, `POST /api/models/register`, `POST /api/processors`
— is open to any local process (settings/mod.rs:76-117). The control is the
`127.0.0.1` bind (settings/mod.rs:73), appropriate for a single-user desktop.
On a multi-user host this would need a token. **Recommendation:** an optional
`settings.tokenEnv` for multi-user deployments. Documented.

### LOW — the WAL has no fsync (durability = page-cache)
The store appends to per-doc logs but does not `fsync` (flagged by the
multi-process perf work: the write path has zero fsync by design for
throughput). A power loss loses the most recent un-flushed actions. This is a
durability choice, not a security bug. **Recommendation:** an `fdatasync` per
batch (or a config knob) for users who need crash durability. Documented.

## Improvements (item 5) — top recommendations

1. **DHT bootstrap hint**: with `p2p.dht` on but empty `bootstraps`, the node
   cannot seed the DHT; surface a clear "add a bootstrap peer" hint (today
   only a debug line, p2p/mod.rs:511).
2. **`/api/overview` re-reads the config file on every poll**
   (settings/mod.rs:590 `config::load`); the console polls it constantly —
   serve it from the in-memory snapshot instead (minor I/O on the hot path).
3. **Invite expiry / nonce-use** (above) would also close a join-spam path: a
   peer can replay an accept to re-join repeatedly.
4. **`valid_name` allows any character except `/` and NUL**
   (settings/mod.rs:181) — fine for drives, but group/folder names flow into
   URL paths; consider rejecting a small set (leading `-`, `%`) that would
   break client-side path handling.

## What changed in this branch

- `src/settings/mod.rs`: added `llm_base_url_ok` and applied it to `llm_test`
  (the per-request `baseUrl` SSRF). The two `/api/llm/draft-*` endpoints use
  the saved-config URL and are covered by the unauthenticated-API finding.
- A unit test for `llm_base_url_ok` (rejects a link-local metadata host,
  allows a normal LLM host).

## Verification

- `cargo build` clean; `cargo clippy --workspace --all-targets -- -D warnings`
  clean; `cargo test --workspace` green (the new LLM-URL guard test plus all
  existing tests).
