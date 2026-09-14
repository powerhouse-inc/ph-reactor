//! Document model: event-sourced documents with per-field last-writer-wins
//! merge. This module is pure — no I/O, no libp2p, no tokio — so the merge
//! semantics are unit-testable on their own.
//!
//! Concurrency model: every op carries the sender's per-document vector
//! clock (including the op itself). An op is *fresh* when its origin has
//! not already advanced past the op's sequence in the receiver's clock
//! (unknown-work rule). Fresh ops from different origins are all applied;
//! same-field conflicts resolve by last-writer-wins on `(ts, origin)` —
//! a total, deterministic order — so the final field map is identical
//! regardless of the order ops arrive in.

use std::collections::BTreeMap;
use std::fmt;

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A stable document identifier (UUID v4, serialized as its string form).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocId(Uuid);

impl DocId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        Uuid::parse_str(s)
            .map(Self)
            .map_err(|e| format!("invalid doc id '{s}': {e}"))
    }

    /// A stable id derived from `seed`.
    ///
    /// Migration needs this: the laptop and the cluster both hold the same
    /// group, and if each generated a fresh id for its replacement space they
    /// would produce two spaces that never reconcile -- a permanent fork of
    /// the thing being migrated. Deriving the id from the old document's id
    /// makes the migration idempotent and makes running it twice, anywhere, a
    /// no-op instead of a split.
    pub fn derived(seed: &str) -> Self {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(seed.as_bytes());
        let d = h.finalize();
        let mut b = [0u8; 16];
        b.copy_from_slice(&d[..16]);
        Self(Uuid::from_bytes(b))
    }
}

impl Default for DocId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for DocId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Debug for DocId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DocId({self})")
    }
}

impl Serialize for DocId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for DocId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// A peer's identity as an op origin. Kept as an opaque string here so
/// this module stays free of libp2p; the p2p layer fills it with the
/// peer's PeerId.
pub type Origin = String;

/// One field value with the version of the write that set it.
///
/// `version` is the writer's vector clock at write time — it decides
/// causality between writes of the same field. `ts`/`origin` break
/// ties for concurrent writes (last-writer-wins).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub value: serde_json::Value,
    /// The writer's vector clock at write time.
    pub version: VecClock,
    /// Lamport-style timestamp; strictly increasing per origin. Used
    /// for LWW tie-breaks of concurrent writes.
    pub ts: u64,
    /// The peer that last wrote this field.
    pub origin: Origin,
    /// True when this entry is a delete tombstone.
    #[serde(default)]
    pub deleted: bool,
}

impl Field {
    /// A fresh value field from an op.
    fn from_op(op: &Op) -> Self {
        Field {
            value: op.value.clone().unwrap_or(serde_json::Value::Null),
            version: op.clock.clone(),
            ts: op.ts,
            origin: op.origin.clone(),
            deleted: false,
        }
    }

    /// A delete tombstone from an op.
    fn tombstone_from_op(op: &Op) -> Self {
        Field {
            value: serde_json::Value::Null,
            version: op.clock.clone(),
            ts: op.ts,
            origin: op.origin.clone(),
            deleted: true,
        }
    }
}

/// A document: a unique name plus a field map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Doc {
    pub id: DocId,
    pub name: String,
    pub fields: BTreeMap<String, Field>,
}

impl Doc {
    /// A copy with delete tombstones removed (the user-facing view).
    pub fn live(&self) -> Doc {
        let fields = self
            .fields
            .iter()
            .filter(|(_, f)| !f.deleted)
            .map(|(k, f)| (k.clone(), f.clone()))
            .collect();
        Doc {
            id: self.id,
            name: self.name.clone(),
            fields,
        }
    }
}

/// Per-document vector clock: origin -> highest op sequence applied from
/// that origin. Empty values are omitted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VecClock(BTreeMap<Origin, u64>);

impl VecClock {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn get(&self, origin: &str) -> u64 {
        self.0.get(origin).copied().unwrap_or(0)
    }

    /// Bump `origin`'s counter by one and return the new sequence number.
    pub fn tick(&mut self, origin: &str) -> u64 {
        let slot = self.0.entry(origin.to_string()).or_insert(0);
        *slot += 1;
        *slot
    }

    /// Element-wise maximum merge (commutative, associative, idempotent).
    pub fn merge(&mut self, other: &Self) {
        for (k, v) in &other.0 {
            match self.0.get_mut(k) {
                Some(slot) => *slot = (*slot).max(*v),
                None => {
                    self.0.insert(k.clone(), *v);
                }
            }
        }
    }

    /// `self ⊇ other`: every origin in `other` is at or below `self`.
    pub fn covers(&self, other: &Self) -> bool {
        other.0.iter().all(|(k, v)| self.get(k) >= *v)
    }

    /// Entries of `other` that exceed `self` (the gaps a peer is missing).
    pub fn gaps(&self, other: &Self) -> Vec<(Origin, u64)> {
        other
            .0
            .iter()
            .filter(|(k, v)| self.get(k) < **v)
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    }
}

/// An operation: upsert or delete of a single field, or delete the whole
/// document (`key: None`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Op {
    pub doc_id: DocId,
    /// `None` deletes the whole document; `Some(k)` with `value: None`
    /// deletes field `k`.
    pub key: Option<String>,
    pub value: Option<serde_json::Value>,
    /// Sender-assigned timestamp: strictly increasing per origin, and
    /// never below the largest ts the sender has observed for this doc.
    pub ts: u64,
    /// The sender's clock for this document *after* this op (i.e. it
    /// includes this op's own sequence).
    pub clock: VecClock,
    /// The peer that created this op.
    pub origin: Origin,
    /// ed25519 signature over [`Op::message_bytes`], stored as hex.
    #[serde(serialize_with = "sig_ser", deserialize_with = "sig_de")]
    pub sig: [u8; 64],
}

fn sig_ser<S: serde::Serializer>(sig: &[u8; 64], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&hex::encode(sig))
}

fn sig_de<'de, D: serde::Deserializer<'de>>(d: D) -> Result<[u8; 64], D::Error> {
    let s = String::deserialize(d)?;
    let v = hex::decode(&s).map_err(serde::de::Error::custom)?;
    v.try_into()
        .map_err(|_| serde::de::Error::custom("signature must be 64 bytes"))
}

/// The canonical byte form an op is signed over:
/// `doc_id (16) || 0x01 || key_len || key || 0x01 | 0x00 || value ||
/// ts (8 BE) || origin_len || origin`
fn message_bytes(op: &Op) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(op.doc_id.0.as_bytes());
    match &op.key {
        Some(k) => {
            out.push(0x01);
            out.extend_from_slice(&(k.len() as u32).to_be_bytes());
            out.extend_from_slice(k.as_bytes());
        }
        None => out.push(0x00),
    }
    match &op.value {
        Some(v) => {
            out.push(0x01);
            let bytes = serde_json::to_vec(v).expect("json value serializes");
            out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            out.extend_from_slice(&bytes);
        }
        None => out.push(0x00),
    }
    out.extend_from_slice(&op.ts.to_be_bytes());
    let origin = op.origin.as_bytes();
    out.extend_from_slice(&(origin.len() as u32).to_be_bytes());
    out.extend_from_slice(origin);
    out
}

impl Op {
    /// Sign this op in place with the sender's identity key.
    pub fn sign(&mut self, key: &SigningKey) {
        self.sig = key.sign(&message_bytes(self)).to_bytes();
    }

    /// Verify the signature against `public_key` (the resolved key of
    /// `op.origin`).
    pub fn verify(&self, public_key: &VerifyingKey) -> bool {
        public_key
            .verify(&message_bytes(self), &self.sig.into())
            .is_ok()
    }
}

/// Result of [`apply_op`].
pub struct ApplyResult {
    /// The op changed the read model (applied, not skipped as
    /// stale/duplicate).
    pub applied: bool,
    /// The op deleted the whole document.
    pub doc_deleted: bool,
}

fn field_from_op(op: &Op) -> Field {
    if op.value.is_none() {
        Field::tombstone_from_op(op)
    } else {
        Field::from_op(op)
    }
}

/// Apply `op` to a document's read model and document clock.
///
/// Field ops merge **per-field by version vector**:
/// - the current field/tombstone is set by a write whose version
///   covers the incoming op's version: the current write causally
///   postdates (or equals) the incoming one — skip (stale/duplicate);
/// - the incoming version covers the current: the incoming write
///   causally postdates the current — replace;
/// - concurrent versions (neither covers the other): last-writer-wins
///   on `(ts, origin)` — a total, deterministic order, so the final
///   value of every field is the same no matter the order ops arrive.
///
/// A doc-level delete (`key: None`) is terminal for the doc id and
/// idempotent: once deleted, further ops for it are ignored
/// (re-adding the name means a new doc id).
///
/// The doc-level [`VecClock`] is a running union of everything seen —
/// used by the sync protocol, not for merge decisions.
pub fn apply_op(doc: &mut Doc, clock: &mut VecClock, deleted: &mut bool, op: &Op) -> ApplyResult {
    clock.merge(&op.clock);
    if *deleted {
        // Terminal: ignore post-deletion work for this doc id.
        return ApplyResult {
            applied: false,
            doc_deleted: true,
        };
    }
    let (applied, doc_deleted) = match &op.key {
        None => {
            doc.fields.clear();
            *deleted = true;
            (true, true)
        }
        Some(key) => {
            let changed = match doc.fields.get_mut(key) {
                None => match &op.value {
                    None => false, // deleting an absent field: no-op
                    Some(_) => {
                        doc.fields.insert(key.clone(), field_from_op(op));
                        true
                    }
                },
                Some(current) => {
                    if current.version.covers(&op.clock) {
                        false // stale or duplicate
                    } else if op.clock.covers(&current.version) {
                        *current = field_from_op(op);
                        true
                    } else {
                        let wins =
                            (op.ts, op.origin.as_str()) > (current.ts, current.origin.as_str());
                        if wins {
                            *current = field_from_op(op);
                        }
                        wins
                    }
                }
            };
            (changed, false)
        }
    };
    ApplyResult {
        applied,
        doc_deleted,
    }
}

// ---- content hashing & model references ----------------------------------
//
// Added for the model core: an action hashes its content (chaining the
// per-document log) and references a model by `name@version[#hash]`.
// These are pure data types shared by the action envelope, the model
// layer, and the store.

/// A 32-byte SHA-256 digest, serialized as lowercase hex.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash32([u8; 32]);

impl Hash32 {
    /// SHA-256 of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(bytes);
        let mut out = [0u8; 32];
        out.copy_from_slice(h.finalize().as_slice());
        Self(out)
    }

    pub fn from_hex(s: &str) -> Result<Self, String> {
        let v = hex::decode(s).map_err(|e| format!("invalid hex: {e}"))?;
        if v.len() != 32 {
            return Err(format!("hash must be 32 bytes (got {})", v.len()));
        }
        Ok(Self(v.try_into().unwrap()))
    }

    pub fn as_bytes(&self) -> [u8; 32] {
        self.0
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0; 32]
    }
}

impl fmt::Display for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl Serialize for Hash32 {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(self.0))
    }
}

impl<'de> Deserialize<'de> for Hash32 {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// A model reference: `name@version`, optionally content-addressed with
/// `#<hash>`. Pinned per document so a doc verifies against exactly the
// model it was written under (a Wasm model pins its module hash; a
/// declarative model pins the hash of its canonical definition).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ModelRef {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub hash: Option<Hash32>,
}

impl ModelRef {
    pub fn new(name: &str, version: &str) -> Self {
        Self {
            name: name.to_string(),
            version: version.to_string(),
            hash: None,
        }
    }

    /// Parse `name@version[#hash]`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let (nv, hash) = match s.split_once('#') {
            Some((nv, h)) => (
                nv,
                Some(Hash32::from_hex(h).map_err(|e| format!("'{s}': {e}"))?),
            ),
            None => (s, None),
        };
        let (name, version) = nv
            .rsplit_once('@')
            .ok_or_else(|| format!("model ref '{s}' must be name@version"))?;
        if name.is_empty() || version.is_empty() {
            return Err(format!("model ref '{s}' has an empty name or version"));
        }
        Ok(Self {
            name: name.to_string(),
            version: version.to_string(),
            hash,
        })
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.version)?;
        if let Some(h) = &self.hash {
            write!(f, "#{h}")?;
        }
        Ok(())
    }
}

impl DocId {
    /// The 16-byte UUID form (for canonical signed/hashed bytes).
    pub fn as_bytes(&self) -> [u8; 16] {
        *self.0.as_bytes()
    }
}

impl VecClock {
    /// Build a clock from `(origin, count)` pairs (zero counts dropped).
    pub fn from_pairs(pairs: &[(String, u64)]) -> Self {
        let mut m = BTreeMap::new();
        for (o, n) in pairs {
            if *n > 0 {
                m.insert(o.clone(), *n);
            }
        }
        Self(m)
    }

    /// Entries in canonical (sorted-by-origin) order.
    pub fn iter(&self) -> impl Iterator<Item = (&Origin, &u64)> {
        self.0.iter()
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> SigningKey {
        let mut bytes = [seed; 32];
        bytes[0] = 0;
        SigningKey::from_bytes(&bytes)
    }

    fn origin(seed: u8) -> (SigningKey, Origin) {
        let k = key(seed);
        (k, format!("peer-{seed:02x}"))
    }

    /// Build a fresh op with a valid signature.
    fn make_op(
        doc_id: DocId,
        signer: &SigningKey,
        from: &Origin,
        k: Option<&str>,
        v: Option<serde_json::Value>,
        ts: u64,
        clock: VecClock,
    ) -> Op {
        let mut op = Op {
            doc_id,
            key: k.map(str::to_string),
            value: v,
            ts,
            clock,
            origin: from.clone(),
            sig: [0; 64],
        };
        op.sign(signer);
        op
    }

    #[test]
    fn doc_id_round_trips() {
        let id = DocId::new();
        let json = serde_json::to_string(&id).unwrap();
        let back: DocId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
        assert!(DocId::parse("nope").is_err());
    }

    #[test]
    fn vector_clock_merge_and_covers() {
        // two peers fork from a shared base {p1:1}
        let mut base = VecClock::default();
        base.tick("p1");
        let mut a = base.clone();
        a.tick("p1"); // {p1:2}
        let mut b = base.clone();
        b.tick("p2"); // {p1:1, p2:1}

        // concurrent: neither covers the other
        assert!(!a.covers(&b));
        assert!(!b.covers(&a));
        // the merge covers both
        let mut after = a.clone();
        after.merge(&b);
        assert!(after.covers(&a));
        assert!(after.covers(&b));
        // gaps: what `a` is missing from `b`, and vice versa
        assert_eq!(a.gaps(&b), vec![("p2".into(), 1)]);
        assert_eq!(b.gaps(&a), vec![("p1".into(), 2)]);
    }

    #[test]
    fn op_signature_valid_wrong_key_tampered() {
        let (ka, a) = origin(1);
        let (kb, _b) = origin(2);
        let id = DocId::new();
        let mut op = make_op(id, &ka, &a, Some("title"), Some("hello".into()), 1, {
            let mut c = VecClock::default();
            c.tick(&a);
            c
        });
        assert!(op.verify(&ka.verifying_key()));
        assert!(!op.verify(&kb.verifying_key()));
        // tamper the value
        op.value = Some("evil".into());
        assert!(!op.verify(&ka.verifying_key()));
    }

    #[test]
    fn duplicate_op_is_not_reapplied() {
        let (ka, a) = origin(1);
        let id = DocId::new();
        let mut c = VecClock::default();
        c.tick(&a);
        let op = make_op(id, &ka, &a, Some("k"), Some(1i64.into()), 1, c);
        let mut doc = Doc {
            id,
            name: "n".into(),
            fields: Default::default(),
        };
        let mut rclock = VecClock::default();
        let mut deleted = false;
        let r1 = apply_op(&mut doc, &mut rclock, &mut deleted, &op);
        assert!(r1.applied);
        // same op again (re-delivered): equal versions -> covered -> skip
        let r2 = apply_op(&mut doc, &mut rclock, &mut deleted, &op);
        assert!(!r2.applied);
        assert_eq!(doc.fields.len(), 1);
    }

    #[test]
    fn concurrent_same_field_resolves_by_lww_independently_of_order() {
        let (ka, a) = origin(1);
        let (kb, b) = origin(2);
        let id = DocId::new();
        // two concurrent writers, both from the empty doc
        let mut ca = VecClock::default();
        ca.tick(&a);
        let op_a = make_op(id, &ka, &a, Some("f"), Some("from-a".into()), 5, ca);
        let mut cb = VecClock::default();
        cb.tick(&b);
        let op_b = make_op(id, &kb, &b, Some("f"), Some("from-b".into()), 7, cb);

        for order in [vec![&op_a, &op_b], vec![&op_b, &op_a]] {
            let mut doc = Doc {
                id,
                name: "n".into(),
                fields: Default::default(),
            };
            let mut rclock = VecClock::default();
            let mut deleted = false;
            for op in order {
                // the LWW winner applies; a concurrent loser is
                // correctly skipped
                apply_op(&mut doc, &mut rclock, &mut deleted, op);
            }
            assert_eq!(doc.fields["f"].value, "from-b");
            assert_eq!(doc.fields["f"].ts, 7);
            assert_eq!(doc.fields["f"].origin, b);
        }
    }

    /// The 50-peer convergence guarantee (item: "does group-chat work with
    /// 50 peers, and are there conflicts?"). 50 origins each write (a) one
    /// DISTINCT field and (b) the SAME field "counter" concurrently — 100
    /// concurrent ops. The set is applied in 10 different orders (each peer
    /// sees a different gossip delivery order). The final Doc must be
    /// identical in every order (no divergence): every distinct field is
    /// retained, and the same-field conflict resolves to one deterministic
    /// winner (LWW on (ts, origin)).
    #[test]
    fn fifty_concurrent_origins_converge_regardless_of_order() {
        const N: usize = 50;
        let id = DocId::new();
        let mut keys = Vec::with_capacity(N);
        let mut origins = Vec::with_capacity(N);
        for i in 0..N {
            let (k, o) = origin(i as u8);
            keys.push(k);
            origins.push(o);
        }

        let mut ops = Vec::with_capacity(2 * N);
        for i in 0..N {
            // (a) a distinct field per origin — all must survive.
            let mut c = VecClock::default();
            c.tick(&origins[i]);
            ops.push(make_op(
                id,
                &keys[i],
                &origins[i],
                Some(&format!("f{i:02}")),
                Some(serde_json::json!(i)),
                1,
                c,
            ));
            // (b) the same field "counter", concurrently, with distinct ts.
            let mut c2 = VecClock::default();
            c2.tick(&origins[i]);
            ops.push(make_op(
                id,
                &keys[i],
                &origins[i],
                Some("counter"),
                Some(serde_json::json!(i)),
                (2 * i) as u64,
                c2,
            ));
        }

        // The LWW winner of "counter" is the max (ts, origin): origin[N-1]
        // has the largest ts (2*(N-1)) and value N-1.
        let win_origin = origins[N - 1].clone();
        let win_ts = (2 * (N - 1)) as u64;

        // Several distinct application orders: forward, reverse, 8 shuffles.
        let base: Vec<&Op> = ops.iter().collect();
        let mut perms: Vec<Vec<&Op>> = Vec::new();
        perms.push(base.clone());
        let mut rev = base.clone();
        rev.reverse();
        perms.push(rev);
        for seed in 0..8u32 {
            let mut sh = base.clone();
            let mut x = 0x1234_5678u32 ^ seed.wrapping_mul(0x9E37_79B9);
            for i in (1..sh.len()).rev() {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let j = (x % (i as u32 + 1)) as usize;
                sh.swap(i, j);
            }
            perms.push(sh);
        }

        let mut baseline: Option<(Doc, VecClock)> = None;
        for perm in &perms {
            let mut doc = Doc {
                id,
                name: "group".into(),
                fields: Default::default(),
            };
            let mut rclock = VecClock::default();
            let mut deleted = false;
            for op in perm {
                apply_op(&mut doc, &mut rclock, &mut deleted, op);
            }
            assert!(!deleted);
            match baseline.as_ref() {
                None => baseline = Some((doc, rclock)),
                Some((bd, bc)) => {
                    assert_eq!(&doc, bd, "doc fields diverged across orders");
                    assert_eq!(&rclock, bc, "doc clock diverged across orders");
                }
            }
        }
        let (final_doc, _) = baseline.unwrap();

        // Every distinct field is retained with its value.
        for i in 0..N {
            let f = &final_doc.fields[&format!("f{i:02}")];
            assert_eq!(f.value, i, "distinct field lost or wrong");
        }
        // The same-field conflict resolved to exactly one deterministic winner.
        let counter = &final_doc.fields["counter"];
        assert_eq!(counter.value, N - 1);
        assert_eq!(counter.ts, win_ts);
        assert_eq!(counter.origin, win_origin);
        // 50 distinct + 1 "counter" = 51 fields.
        assert_eq!(final_doc.fields.len(), N + 1);
    }

    /// Two concurrent writers with the SAME timestamp break the tie by the
    /// origin string (the (ts, origin) total order) — deterministic in both
    /// arrival orders.
    #[test]
    fn same_ts_same_field_breaks_by_origin() {
        let (ka, a) = origin(1);
        let (kb, b) = origin(2);
        let id = DocId::new();
        let mut ca = VecClock::default();
        ca.tick(&a);
        let op_a = make_op(id, &ka, &a, Some("f"), Some("A".into()), 5, ca);
        let mut cb = VecClock::default();
        cb.tick(&b);
        let op_b = make_op(id, &kb, &b, Some("f"), Some("B".into()), 5, cb);
        // origin(2) = "peer-02" sorts above origin(1) = "peer-01", so B wins.
        assert!(b > a);
        for order in [vec![&op_a, &op_b], vec![&op_b, &op_a]] {
            let mut doc = Doc {
                id,
                name: "n".into(),
                fields: Default::default(),
            };
            let mut rclock = VecClock::default();
            let mut deleted = false;
            for op in order {
                apply_op(&mut doc, &mut rclock, &mut deleted, op);
            }
            assert_eq!(doc.fields["f"].value, "B");
        }
    }

    #[test]
    fn causally_later_write_wins_regardless_of_arrival_order() {
        // A writes f. B observes A's write (merges A's clock) and then
        // writes f itself. B causally postdates A, so B's value must
        // win even when A's op arrives after B's.
        let (ka, a) = origin(1);
        let (kb, b) = origin(2);
        let id = DocId::new();
        let mut ca = VecClock::default();
        ca.tick(&a); // {a:1}
        let op_a = make_op(id, &ka, &a, Some("f"), Some("from-a".into()), 5, ca.clone());
        let mut cb = ca.clone();
        cb.tick(&b); // {a:1, b:1}
        let op_b = make_op(id, &kb, &b, Some("f"), Some("from-b".into()), 7, cb);

        for order in [vec![&op_a, &op_b], vec![&op_b, &op_a]] {
            let mut doc = Doc {
                id,
                name: "n".into(),
                fields: Default::default(),
            };
            let mut rclock = VecClock::default();
            let mut deleted = false;
            for op in order {
                apply_op(&mut doc, &mut rclock, &mut deleted, op);
            }
            assert_eq!(doc.fields["f"].value, "from-b");
        }
    }

    /// Convergence: two offline histories, merged in each direction,
    /// must end in identical field maps and identical clocks.
    #[test]
    fn fork_merge_converges_regardless_of_direction() {
        let (ka, a) = origin(1);
        let (kb, b) = origin(2);
        let id = DocId::new();

        // Shared history: one op from a.
        let mut base = VecClock::default();
        base.tick(&a);
        let shared = make_op(id, &ka, &a, Some("x"), Some(1i64.into()), 1, base);

        // Fork A (origin a) and Fork B (origin b) diverge from the
        // same base and observe each other as they go (merged clocks).
        let mut clock_a = {
            let mut c = VecClock::default();
            c.tick(&a);
            c
        };
        let mut clock_b = {
            let mut c = VecClock::default();
            c.tick(&a);
            c
        };
        let mut ops_a = Vec::new();
        let mut ops_b = Vec::new();
        for (i, ts) in [2u64, 3, 4].into_iter().enumerate() {
            clock_a.tick(&a);
            clock_a.merge(&clock_b);
            ops_a.push(make_op(
                id,
                &ka,
                &a,
                Some(&format!("field-{i}")),
                Some(serde_json::json!(100 + ts)),
                ts,
                clock_a.clone(),
            ));
            clock_b.tick(&b);
            clock_b.merge(&clock_a);
            ops_b.push(make_op(
                id,
                &kb,
                &b,
                Some(&format!("field-{i}")),
                Some(serde_json::json!(200 + ts)),
                ts + 1,
                clock_b.clone(),
            ));
            // one shared field both write concurrently
            clock_a.tick(&a);
            ops_a.push(make_op(
                id,
                &ka,
                &a,
                Some("shared"),
                Some(serde_json::json!(format!("a says {ts}"))),
                ts + 2,
                clock_a.clone(),
            ));
            clock_b.tick(&b);
            ops_b.push(make_op(
                id,
                &kb,
                &b,
                Some("shared"),
                Some(serde_json::json!(format!("b says {ts}"))),
                ts + 3,
                clock_b.clone(),
            ));
        }

        let run = |first: &[Op], second: &[Op]| {
            let mut doc = Doc {
                id,
                name: "n".into(),
                fields: Default::default(),
            };
            let mut clock = VecClock::default();
            let mut deleted = false;
            apply_op(&mut doc, &mut clock, &mut deleted, &shared);
            for op in first {
                apply_op(&mut doc, &mut clock, &mut deleted, op);
            }
            for op in second {
                apply_op(&mut doc, &mut clock, &mut deleted, op);
            }
            (doc, clock)
        };
        let (doc_ab, clock_ab) = run(&ops_a, &ops_b);
        let (doc_ba, clock_ba) = run(&ops_b, &ops_a);

        assert_eq!(doc_ab.fields, doc_ba.fields);
        assert_eq!(clock_ab, clock_ba);

        // all non-conflicting fields from both forks are present
        for i in 0..3 {
            assert!(doc_ab.fields.contains_key(&format!("field-{i}")));
        }
        // the shared field resolved to one writer, identically in both
        // directions (B's final write has the highest ts)
        assert_eq!(doc_ab.fields["shared"].origin, b);

        // fixed point: re-applying ops changes nothing
        let (mut doc2, mut clock2) = (doc_ab.clone(), clock_ab.clone());
        let mut deleted2 = false;
        assert!(!apply_op(&mut doc2, &mut clock2, &mut deleted2, &shared).applied);
        assert!(!apply_op(&mut doc2, &mut clock2, &mut deleted2, &ops_a[0]).applied);
        assert!(!apply_op(&mut doc2, &mut clock2, &mut deleted2, &ops_b[0]).applied);
        assert_eq!(doc2.fields, doc_ab.fields);
    }

    #[test]
    fn field_delete_leaves_tombstone_and_doc_delete_is_terminal() {
        let (ka, a) = origin(1);
        let id = DocId::new();
        // one receiver clock, advanced as a real receiver would
        let mut rclock = VecClock::default();
        let mut op1 = {
            rclock.tick(&a);
            make_op(id, &ka, &a, Some("k"), Some(1i64.into()), 1, rclock.clone())
        };
        let mut op2 = {
            rclock.tick(&a);
            make_op(id, &ka, &a, Some("k"), None, 2, rclock.clone())
        };
        let mut op3 = {
            rclock.tick(&a);
            make_op(id, &ka, &a, None, None, 3, rclock.clone())
        };
        let mut op4 = {
            rclock.tick(&a);
            make_op(
                id,
                &ka,
                &a,
                Some("k"),
                Some(99i64.into()),
                4,
                rclock.clone(),
            )
        };
        let _ = (&mut op1, &mut op2, &mut op3, &mut op4);

        let mut doc = Doc {
            id,
            name: "n".into(),
            fields: Default::default(),
        };
        let mut deleted = false;
        let r1 = apply_op(&mut doc, &mut rclock, &mut deleted, &op1);
        assert!(r1.applied);
        assert_eq!(doc.live().fields.len(), 1);
        let r2 = apply_op(&mut doc, &mut rclock, &mut deleted, &op2);
        assert!(r2.applied);
        // tombstone: raw map keeps it (propagates the delete), the
        // user-facing view does not
        assert!(doc.fields["k"].deleted);
        assert!(doc.live().fields.is_empty());
        let r3 = apply_op(&mut doc, &mut rclock, &mut deleted, &op3);
        assert!(r3.applied && r3.doc_deleted);
        assert!(deleted);
        let r4 = apply_op(&mut doc, &mut rclock, &mut deleted, &op4);
        assert!(!r4.applied && r4.doc_deleted);
        assert!(doc.fields.is_empty());
    }

    #[test]
    fn lww_tiebreak_on_equal_ts() {
        // same ts, different origins: the lexicographic origin order
        // is the deterministic tie-break
        let (ka, a) = origin(1);
        let (kb, b) = origin(2);
        let id = DocId::new();
        let mut ca = VecClock::default();
        ca.tick(&a);
        let op_a = make_op(id, &ka, &a, Some("f"), Some("a".into()), 5, ca);
        let mut cb = VecClock::default();
        cb.tick(&b);
        let op_b = make_op(id, &kb, &b, Some("f"), Some("b".into()), 5, cb);

        for order in [vec![&op_a, &op_b], vec![&op_b, &op_a]] {
            let mut doc = Doc {
                id,
                name: "n".into(),
                fields: Default::default(),
            };
            let mut rclock = VecClock::default();
            let mut deleted = false;
            for op in order {
                apply_op(&mut doc, &mut rclock, &mut deleted, op);
            }
            // "peer-01" < "peer-02", so B's write wins deterministically
            assert_eq!(doc.fields["f"].origin, b);
        }
    }

    /// Property-style: random op streams from two origins, merged in
    /// both directions, always converge.
    #[test]
    fn random_forks_converge() {
        let (ka, a) = origin(1);
        let (kb, b) = origin(2);
        let id = DocId::new();
        let mut rng = 0x1234_5678_u64;
        let mut next = || {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (rng >> 33) as u32
        };

        let mut mk_stream = |signer: &SigningKey, me: &Origin, _other: &Origin, start_ts: u64| {
            let mut clock = VecClock::default();
            let mut ops = Vec::new();
            let mut ts = start_ts;
            for _i in 0..20 {
                let key = format!("k{}", next() % 4);
                ts += 1;
                clock.tick(me);
                let op = make_op(
                    id,
                    signer,
                    me,
                    Some(&key),
                    Some(serde_json::json!(next())),
                    ts,
                    clock.clone(),
                );
                ops.push(op);
            }
            ops
        };
        let ops_a = mk_stream(&ka, &a, &b, 0);
        let ops_b = mk_stream(&kb, &b, &a, 0);

        let run = |first: &[Op], second: &[Op]| {
            let mut doc = Doc {
                id,
                name: "n".into(),
                fields: Default::default(),
            };
            let mut clock = VecClock::default();
            let mut deleted = false;
            for op in first.iter().chain(second.iter()) {
                apply_op(&mut doc, &mut clock, &mut deleted, op);
            }
            (doc, clock)
        };
        let (d1, c1) = run(&ops_a, &ops_b);
        let (d2, c2) = run(&ops_b, &ops_a);
        assert_eq!(d1.fields, d2.fields);
        assert_eq!(c1, c2);
    }
}
