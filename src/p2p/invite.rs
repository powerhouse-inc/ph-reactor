//! One-shot invites.
//!
//! An [`InviteToken`] is a signed, shareable string a peer generates
//! (`ph-reactor invite`) that encodes the inviter's identity, a reachable
//! address, a challenge nonce, and the groups a joiner is granted. Another
//! peer consumes it (`ph-reactor join`) to establish a synced drive pair
//! without pre-configuring each other's addresses.
//!
//! The joiner proves it holds a valid invite with an [`InviteAccept`] (sent
//! in the handshake), so the inviter can safely add a drive for it: the
//! accept is signed by the joiner's key (pinned via TOFU) and echoes the
//! invite's nonce.
//!
//! This module is pure: no I/O, no libp2p — only an ed25519 key, `hex`,
//! `base64`, and `getrandom`.

use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

fn hex_ser<S: Serializer>(b: &[u8], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&hex::encode(b))
}
fn hex_de<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let s = String::deserialize(d)?;
    hex::decode(s).map_err(serde::de::Error::custom)
}

const TOKEN_MAGIC: &[u8] = b"ph-reactor/invite/v1";
const ACCEPT_MAGIC: &[u8] = b"ph-reactor/accept/v1";
const NONCE_BYTES: usize = 16;

fn push_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// A signed out-of-band invite. The inviter generates one; the joiner
/// consumes it. The signature binds every field to the inviter's key, so a
/// tampered or forged token fails verification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InviteToken {
    pub version: u32,
    /// The inviter's instance name (the drive's friendly name on the joiner).
    pub name: String,
    /// The inviter's peer id (base58).
    pub peer_id: String,
    /// The inviter's ed25519 public key (32 bytes, hex).
    #[serde(serialize_with = "hex_ser", deserialize_with = "hex_de")]
    pub pubkey: Vec<u8>,
    /// The inviter's reachable multiaddr (what the joiner dials).
    pub addr: String,
    /// A random challenge nonce (16 bytes, hex); the joiner echoes it in
    /// the [`InviteAccept`].
    #[serde(serialize_with = "hex_ser", deserialize_with = "hex_de")]
    pub nonce: Vec<u8>,
    /// Group names the joiner is granted on acceptance.
    #[serde(default)]
    pub groups: Vec<String>,
    /// ed25519 signature over [`InviteToken::message_bytes`] by the inviter.
    #[serde(serialize_with = "hex_ser", deserialize_with = "hex_de")]
    pub sig: Vec<u8>,
}

impl InviteToken {
    /// Canonical byte form signed over (the signature is excluded).
    pub fn message_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(TOKEN_MAGIC);
        out.extend_from_slice(&self.version.to_be_bytes());
        push_str(&mut out, &self.name);
        push_str(&mut out, &self.peer_id);
        out.extend_from_slice(&self.pubkey);
        push_str(&mut out, &self.addr);
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&(self.groups.len() as u32).to_be_bytes());
        for g in &self.groups {
            push_str(&mut out, g);
        }
        out
    }

    pub fn sign(&mut self, key: &SigningKey) {
        self.sig = key.sign(&self.message_bytes()).to_bytes().to_vec();
    }

    /// Create and sign a fresh invite for `key` (the inviter).
    pub fn make(
        name: &str,
        peer_id: &str,
        key: &SigningKey,
        addr: &str,
        groups: Vec<String>,
    ) -> Result<Self, String> {
        let mut t = Self {
            version: 1,
            name: name.into(),
            peer_id: peer_id.into(),
            pubkey: key.verifying_key().to_bytes().to_vec(),
            addr: addr.into(),
            nonce: new_nonce()?,
            groups,
            sig: vec![],
        };
        t.sign(key);
        Ok(t)
    }

    /// Verify the signature against this token's own `pubkey`.
    pub fn verify(&self) -> Result<(), String> {
        let pk = verifying_key(&self.pubkey)?;
        let sig =
            Signature::from_slice(&self.sig).map_err(|e| format!("bad invite sig: {e}"))?;
        pk.verify_strict(&self.message_bytes(), &sig)
            .map_err(|e| format!("invite signature invalid: {e}"))
    }

    /// Encode to a shareable base64 string.
    pub fn encode(&self) -> Result<String, String> {
        let j = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        Ok(base64::engine::general_purpose::STANDARD.encode(j))
    }

    /// Decode and verify a shareable string.
    pub fn decode(s: &str) -> Result<Self, String> {
        let j = base64::engine::general_purpose::STANDARD
            .decode(s.trim())
            .map_err(|e| format!("not a valid invite (bad encoding): {e}"))?;
        let t: Self = serde_json::from_slice(&j).map_err(|e| format!("bad invite: {e}"))?;
        t.verify()?;
        Ok(t)
    }
}

/// A signed proof that a joiner holds a valid invite, carried in the
/// handshake so the inviter can add a drive for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InviteAccept {
    /// The invite's nonce, echoed.
    #[serde(serialize_with = "hex_ser", deserialize_with = "hex_de")]
    pub nonce: Vec<u8>,
    /// The joiner's instance name (the drive's friendly name on the inviter).
    pub name: String,
    /// The joiner's peer id (base58).
    pub peer_id: String,
    /// The joiner's ed25519 public key (32 bytes, hex).
    #[serde(serialize_with = "hex_ser", deserialize_with = "hex_de")]
    pub pubkey: Vec<u8>,
    /// ed25519 signature over [`InviteAccept::message_bytes`] by the joiner.
    #[serde(serialize_with = "hex_ser", deserialize_with = "hex_de")]
    pub sig: Vec<u8>,
}

impl InviteAccept {
    pub fn message_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(ACCEPT_MAGIC);
        out.extend_from_slice(&self.nonce);
        push_str(&mut out, &self.name);
        push_str(&mut out, &self.peer_id);
        out.extend_from_slice(&self.pubkey);
        out
    }

    pub fn sign(&mut self, key: &SigningKey) {
        self.sig = key.sign(&self.message_bytes()).to_bytes().to_vec();
    }

    /// Create and sign a join-proof echoing `nonce`, for `key` (the joiner).
    pub fn make(nonce: &[u8], name: &str, peer_id: &str, key: &SigningKey) -> Self {
        let mut a = Self {
            nonce: nonce.to_vec(),
            name: name.into(),
            peer_id: peer_id.into(),
            pubkey: key.verifying_key().to_bytes().to_vec(),
            sig: vec![],
        };
        a.sign(key);
        a
    }

    /// Verify the signature against this accept's own `pubkey`.
    pub fn verify(&self) -> Result<(), String> {
        let pk = verifying_key(&self.pubkey)?;
        let sig =
            Signature::from_slice(&self.sig).map_err(|e| format!("bad accept sig: {e}"))?;
        pk.verify_strict(&self.message_bytes(), &sig)
            .map_err(|e| format!("accept signature invalid: {e}"))
    }
}

/// Build a [`VerifyingKey`] from a byte slice (validating the 32-byte
/// length), for use with the `pubkey` fields.
fn verifying_key(pk: &[u8]) -> Result<VerifyingKey, String> {
    if pk.len() != 32 {
        return Err(format!("pubkey must be 32 bytes, got {}", pk.len()));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(pk);
    VerifyingKey::from_bytes(&arr).map_err(|e| e.to_string())
}

/// A fresh random challenge nonce.
pub fn new_nonce() -> Result<Vec<u8>, String> {
    let mut n = [0u8; NONCE_BYTES];
    getrandom::getrandom(&mut n).map_err(|e| format!("nonce: {e}"))?;
    Ok(n.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> SigningKey {
        let mut b = [seed; 32];
        b[0] = 0;
        SigningKey::from_bytes(&b)
    }

    #[test]
    fn token_sign_verify_encode_decode_roundtrip() {
        let k = key(1);
        let mut t = InviteToken {
            version: 1,
            name: "inviter".into(),
            peer_id: "12D3KooW-inviter".into(),
            pubkey: k.verifying_key().to_bytes().to_vec(),
            addr: "/ip4/10.0.0.2/tcp/4201/p2p/12D3KooW-inviter".into(),
            nonce: new_nonce().unwrap(),
            groups: vec!["powerhouse".into()],
            sig: vec![],
        };
        t.sign(&k);
        let s = t.encode().unwrap();
        let back = InviteToken::decode(&s).unwrap();
        assert_eq!(back, t);
        assert_eq!(back.peer_id, "12D3KooW-inviter");
        assert_eq!(back.groups, vec!["powerhouse".to_string()]);
    }

    #[test]
    fn token_rejects_tampered_field() {
        let k = key(2);
        let mut t = InviteToken {
            version: 1,
            name: "inviter".into(),
            peer_id: "inviter".into(),
            pubkey: k.verifying_key().to_bytes().to_vec(),
            addr: "/ip4/10.0.0.2/tcp/4201".into(),
            nonce: new_nonce().unwrap(),
            groups: vec![],
            sig: vec![],
        };
        t.sign(&k);
        let s = t.encode().unwrap();

        // Flip a field in the JSON before re-encoding: the signature no
        // longer matches, so decode must fail.
        let j = base64::engine::general_purpose::STANDARD
            .decode(&s)
            .unwrap();
        let mut v: serde_json::Value = serde_json::from_slice(&j).unwrap();
        v["addr"] = serde_json::json!("/ip4/127.0.0.1/tcp/1");
        let tampered =
            base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&v).unwrap());
        assert!(InviteToken::decode(&tampered).is_err());
    }

    #[test]
    fn token_rejects_wrong_signer() {
        let inviter = key(3);
        let rogue = key(4);
        let mut t = InviteToken {
            version: 1,
            name: "inviter".into(),
            peer_id: "inviter".into(),
            // The token claims the inviter's key...
            pubkey: inviter.verifying_key().to_bytes().to_vec(),
            addr: "/ip4/10.0.0.2/tcp/4201".into(),
            nonce: new_nonce().unwrap(),
            groups: vec![],
            sig: vec![],
        };
        // ...but is signed by a rogue key.
        t.sign(&rogue);
        assert!(t.verify().is_err());
    }

    #[test]
    fn accept_sign_verify_and_tamper() {
        let joiner = key(5);
        let mut a = InviteAccept {
            nonce: new_nonce().unwrap(),
            name: "joiner".into(),
            peer_id: "12D3KooW-joiner".into(),
            pubkey: joiner.verifying_key().to_bytes().to_vec(),
            sig: vec![],
        };
        a.sign(&joiner);
        assert!(a.verify().is_ok());

        // Tampering with the echoed nonce breaks the signature.
        let mut bad = a.clone();
        bad.nonce = new_nonce().unwrap();
        assert!(bad.verify().is_err());
    }

    #[test]
    fn nonce_is_random_and_16_bytes() {
        let a = new_nonce().unwrap();
        let b = new_nonce().unwrap();
        assert_eq!(a.len(), NONCE_BYTES);
        assert_ne!(a, b);
    }
}
