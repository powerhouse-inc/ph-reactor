//! The action envelope: a signed, model-referenced *intent*.
//!
//! A v1 [`Op`](crate::doc::Op) is a single field write; an [`Action`] is
//! the durable unit of the per-document log. It names the
//! [`ModelRef`](crate::doc::ModelRef) it invokes (`name@version[#hash]`)
//! and a reducer `kind`, carries the reducer `payload`, and is signed by
//! its `origin` (plus optional [`CoSig`]s for quorum-gated reducers).
//!
//! Two things make the log an audit trail rather than just a journal:
//! - every action is signed by its origin over [`Action::message_bytes`]
//!   (which, unlike the v1 op, *includes* the vector clock), so an
//!   action's full identity is non-repudiable;
//! - each action's `prev_hash` is the content hash of the previous action
//!   in the same document, chaining the log so any insertion, deletion,
//!   or reorder is detectable.
//!
//! This module is pure: no I/O, no libp2p, no tokio — it only borrows the
//! data types from [`crate::doc`] and an ed25519 key.

use crate::doc::{DocId, Hash32, ModelRef, Origin, VecClock};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

fn sig_ser<S: serde::Serializer>(sig: &[u8; 64], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&hex::encode(sig))
}

fn sig_de<'de, D: serde::Deserializer<'de>>(d: D) -> Result<[u8; 64], D::Error> {
    let s = String::deserialize(d)?;
    let v = hex::decode(&s).map_err(serde::de::Error::custom)?;
    v.try_into()
        .map_err(|_| serde::de::Error::custom("signature must be 64 bytes"))
}

/// An additional signer on a quorum-gated action. Each co-signer signs the
/// action's [`Action::message_bytes`] with their own key; the model's
/// quorum precondition requires a minimum number of *distinct* valid
/// co-signers (usually from a named group).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoSig {
    pub origin: Origin,
    #[serde(serialize_with = "sig_ser", deserialize_with = "sig_de")]
    pub sig: [u8; 64],
}

/// A signed, model-referenced intent. Reduces (via its model) to a set of
/// field writes ([`Op`](crate::doc::Op)) that update the document's read
/// model through the existing per-field merge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub doc_id: DocId,
    /// The model this action invokes, pinned by name + version (and hash).
    pub model: ModelRef,
    /// The reducer to run — a key of the model's `reducers` map.
    pub kind: String,
    /// The reducer payload. Validated against the reducer's schema.
    pub payload: serde_json::Value,
    /// Sender-assigned timestamp (same semantics as
    /// [`Op::ts`](crate::doc::Op::ts)).
    pub ts: u64,
    /// The sender's per-document vector clock *after* this action.
    pub clock: VecClock,
    /// The peer that created (and signed) this action.
    pub origin: Origin,
    /// Additional signers, for quorum-gated reducers.
    #[serde(default)]
    pub cosig: Vec<CoSig>,
    /// Hash of the previous action in this document's log (`None` for the
    /// first). Chains the log so it is tamper-evident.
    #[serde(default)]
    pub prev_hash: Option<Hash32>,
    /// ed25519 signature over [`Action::message_bytes`] by `origin`.
    #[serde(serialize_with = "sig_ser", deserialize_with = "sig_de")]
    pub sig: [u8; 64],
}

/// Canonical byte form an action is signed over. The primary signature and
/// the co-signatures are excluded — a signature cannot be part of the bytes
/// it covers — so the origin and every co-signer sign this same message:
///
/// `doc_id(16) || 0x01 name 0x01 version [0x01 hash | 0x00]
///  || 0x01 kind || 0x01 payload || ts(8) || 0x01 clock
///  || 0x01 origin || [0x01 prev_hash | 0x00]`
impl Action {
    pub fn message_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        out.extend_from_slice(&self.doc_id.as_bytes());

        out.push(0x01);
        out.extend_from_slice(&(self.model.name.len() as u32).to_be_bytes());
        out.extend_from_slice(self.model.name.as_bytes());
        out.push(0x01);
        out.extend_from_slice(&(self.model.version.len() as u32).to_be_bytes());
        out.extend_from_slice(self.model.version.as_bytes());
        match &self.model.hash {
            Some(h) => {
                out.push(0x01);
                out.extend_from_slice(&h.as_bytes());
            }
            None => out.push(0x00),
        }

        out.push(0x01);
        out.extend_from_slice(&(self.kind.len() as u32).to_be_bytes());
        out.extend_from_slice(self.kind.as_bytes());

        let pj = serde_json::to_vec(&self.payload).expect("payload serializes");
        out.push(0x01);
        out.extend_from_slice(&(pj.len() as u32).to_be_bytes());
        out.extend_from_slice(&pj);

        out.extend_from_slice(&self.ts.to_be_bytes());

        let cb = vec_clock_bytes(&self.clock);
        out.push(0x01);
        out.extend_from_slice(&(cb.len() as u32).to_be_bytes());
        out.extend_from_slice(&cb);

        out.push(0x01);
        let o = self.origin.as_bytes();
        out.extend_from_slice(&(o.len() as u32).to_be_bytes());
        out.extend_from_slice(o);

        // The co-signatures are deliberately excluded: each co-signer signs
        // this same message (see the doc comment), so the bytes are stable
        // regardless of how many cosigs are attached.

        match &self.prev_hash {
            Some(h) => {
                out.push(0x01);
                out.extend_from_slice(&h.as_bytes());
            }
            None => out.push(0x00),
        }
        out
    }

    /// Sign this action in place with the sender's identity key.
    pub fn sign(&mut self, key: &SigningKey) {
        let sig = key.sign(&self.message_bytes());
        self.sig = sig.to_bytes();
    }

    /// Verify the primary (origin) signature against `pk`.
    pub fn verify(&self, pk: &VerifyingKey) -> bool {
        match Signature::from_slice(&self.sig) {
            Ok(s) => pk.verify_strict(&self.message_bytes(), &s).is_ok(),
            Err(_) => false,
        }
    }

    /// Verify co-signer `i` against `pk`.
    pub fn verify_cosig(&self, i: usize, pk: &VerifyingKey) -> bool {
        let Some(c) = self.cosig.get(i) else {
            return false;
        };
        match Signature::from_slice(&c.sig) {
            Ok(s) => pk.verify_strict(&self.message_bytes(), &s).is_ok(),
            Err(_) => false,
        }
    }

    /// The content hash of this action: the target of the next action's
    /// `prev_hash`, and a node in the per-document hash chain.
    pub fn hash(&self) -> Hash32 {
        Hash32::of(&self.message_bytes())
    }
}

/// Canonical encoding of a [`VecClock`]: for each (origin, count) in
/// sorted-by-origin order, `origin_len(4 BE) || origin || count(8 BE)`.
/// Deterministic, so it can be part of a signed byte form.
fn vec_clock_bytes(c: &VecClock) -> Vec<u8> {
    let mut out = Vec::new();
    for (origin, n) in c.iter() {
        let o = origin.as_bytes();
        out.extend_from_slice(&(o.len() as u32).to_be_bytes());
        out.extend_from_slice(o);
        out.extend_from_slice(&n.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> SigningKey {
        let mut b = [seed; 32];
        b[0] = 0;
        SigningKey::from_bytes(&b)
    }

    fn sample_action(key: &SigningKey) -> Action {
        let mut a = Action {
            doc_id: DocId::parse("00000000-0000-0000-0000-000000000001").unwrap(),
            model: ModelRef::new("invoice", "1.0.0"),
            kind: "add-line".into(),
            payload: serde_json::json!({ "id": "L1", "qty": 2 }),
            ts: 42,
            clock: VecClock::from_pairs(&[("12D3KooWQbz".to_string(), 3)]),
            origin: "12D3KooWQbz".into(),
            cosig: Vec::new(),
            prev_hash: None,
            sig: [0; 64],
        };
        a.sign(key);
        a
    }

    #[test]
    fn sign_verify_roundtrip() {
        let k = key(1);
        let a = sample_action(&k);
        assert!(a.verify(&k.verifying_key()));
    }

    #[test]
    fn rejects_wrong_key() {
        let a = sample_action(&key(1));
        let other = key(2);
        assert!(!a.verify(&other.verifying_key()));
    }

    #[test]
    fn hash_is_stable_and_distinguishes_fields() {
        let k = key(1);
        let a = sample_action(&k);
        assert_eq!(a.hash(), sample_action(&k).hash());
        let mut b = sample_action(&k);
        b.kind = "delete".into();
        b.sign(&k);
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn message_bytes_covers_the_clock() {
        let k = key(1);
        let a = sample_action(&k);
        let mut b = sample_action(&k);
        b.clock = VecClock::from_pairs(&[("12D3KooWQbz".to_string(), 9)]);
        assert_ne!(a.message_bytes(), b.message_bytes());
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn cosig_verifies_against_its_own_key() {
        let k = key(1);
        let cosigner = key(2);
        let mut a = sample_action(&k);
        a.cosig = vec![CoSig {
            origin: "12D3KooCosigner".into(),
            sig: cosigner.sign(&a.message_bytes()).to_bytes(),
        }];
        assert!(a.verify_cosig(0, &cosigner.verifying_key()));
        assert!(!a.verify_cosig(0, &k.verifying_key()));
        assert!(!a.verify_cosig(1, &cosigner.verifying_key()));
    }
}
