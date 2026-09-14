//! Plugin packages: the manifest, its signature, and publisher trust.
//!
//! A package bundles document models, processors and a UI so a network can
//! ship an application — Achra, a knowledge vault — to the nodes on it.
//!
//! The format is Powerhouse's `powerhouse.manifest.json`, adopted rather than
//! reinvented so a ph-reactor plugin *is* a Powerhouse package. Two things are
//! added, because their format does not have them:
//!
//! - **`publisher_key` + `sig`.** Their `publisher` is `{name, url}` —
//!   descriptive metadata that anyone can copy. Provenance needs a key.
//! - **`capabilities`.** What the plugin's UI may read and write, declared up
//!   front and shown to the operator at install time.
//!
//! One deliberate deviation: `documentModels` carries the full model
//! *definitions*, not `{id, name}` references. ph-reactor registers models
//! from their definitions, and a reference to something fetched elsewhere would
//! be a hole in exactly the provenance this module exists to establish.
//!
//! See docs/superpowers/specs/2026-09-14-plugin-packages-design.md.

pub mod trust;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::blob::BlobRef;

/// Human-facing publisher metadata, as Powerhouse defines it.
///
/// Descriptive only — anyone can write "Powerhouse" here. [`Manifest::publisher_key`]
/// is what actually identifies a publisher.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublisherInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: String,
}

/// What a plugin's UI is permitted to do.
///
/// The bridge refuses anything not listed here. This narrows what a plugin may
/// *attempt*; it never widens what the store permits, because a write still
/// passes the model's own `auth`, `pre` and quorum rules afterwards.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Models the UI may query, as `name@version`.
    #[serde(default)]
    pub read: Vec<String>,
    /// Reducers the UI may invoke.
    #[serde(default)]
    pub write: Vec<WriteCapability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteCapability {
    pub model: String,
    pub kinds: Vec<String>,
}

impl Capabilities {
    pub fn may_read(&self, model: &str) -> bool {
        self.read.iter().any(|m| m == model)
    }

    pub fn may_write(&self, model: &str, kind: &str) -> bool {
        self.write
            .iter()
            .any(|w| w.model == model && w.kinds.iter().any(|k| k == kind))
    }

    /// A plain-language summary for the install prompt.
    ///
    /// An operator cannot make a trust decision about a JSON blob, so the
    /// prompt shows this instead.
    pub fn describe(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.read.is_empty() {
            out.push(format!("read documents of type: {}", self.read.join(", ")));
        }
        for w in &self.write {
            out.push(format!("perform {} on {}", w.kinds.join(", "), w.model));
        }
        if out.is_empty() {
            out.push("nothing — this plugin requests no access".into());
        }
        out
    }
}

/// A package manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub publisher: PublisherInfo,
    /// The publisher's ed25519 public key, hex-encoded. This, not
    /// `publisher.name`, is the identity a trust decision is made about.
    pub publisher_key: String,
    /// Full model definitions, registered on install.
    #[serde(default, rename = "documentModels")]
    pub document_models: Vec<Value>,
    #[serde(default)]
    pub processors: Vec<Value>,
    /// The UI bundle, delivered as content-addressed chunks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<BlobRef>,
    #[serde(default)]
    pub capabilities: Capabilities,
    /// ed25519 signature over [`Manifest::message_bytes`], hex-encoded.
    #[serde(default)]
    pub sig: String,
}

impl Manifest {
    /// The canonical bytes a publisher signs.
    ///
    /// The signature is excluded — a signature cannot cover itself — exactly as
    /// `Action::message_bytes` and `InviteToken::message_bytes` do. Serde's
    /// `Map` is a sorted `BTreeMap`, so the JSON is canonical and the same
    /// manifest produces the same bytes on every node.
    pub fn message_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.sig = String::new();
        serde_json::to_vec(&clone).expect("a manifest serializes")
    }

    pub fn sign(&mut self, key: &SigningKey) {
        let mb = self.message_bytes();
        self.sig = hex::encode(key.sign(&mb).to_bytes());
    }

    /// Verifies the signature against the key the manifest itself names.
    ///
    /// This establishes **integrity** — nobody altered the package after it was
    /// signed. It says nothing about whether that publisher should be trusted,
    /// which is a separate and deliberately human decision; see
    /// [`trust::TrustStore`]. Conflating the two is the classic supply-chain
    /// mistake.
    pub fn verify(&self) -> Result<(), String> {
        let key_bytes = hex::decode(&self.publisher_key)
            .map_err(|e| format!("publisher_key is not hex: {e}"))?;
        let key_arr: [u8; 32] = key_bytes
            .try_into()
            .map_err(|_| "publisher_key must be 32 bytes".to_string())?;
        let vk = VerifyingKey::from_bytes(&key_arr)
            .map_err(|e| format!("publisher_key is not a valid ed25519 key: {e}"))?;

        let sig_bytes = hex::decode(&self.sig).map_err(|e| format!("signature is not hex: {e}"))?;
        let sig_arr: [u8; 64] = sig_bytes
            .try_into()
            .map_err(|_| "signature must be 64 bytes".to_string())?;

        vk.verify(&self.message_bytes(), &Signature::from_bytes(&sig_arr))
            .map_err(|_| "signature does not verify: the package was altered".to_string())
    }

    /// `name@version`, how a package is referred to.
    pub fn id(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn manifest(k: &SigningKey) -> Manifest {
        let mut m = Manifest {
            name: "@powerhousedao/achra".into(),
            version: "1.0.0".into(),
            description: "Marketplace for global coordination".into(),
            category: "Coordination".into(),
            publisher: PublisherInfo {
                name: "Powerhouse".into(),
                url: "https://powerhouse.inc/".into(),
            },
            publisher_key: hex::encode(k.verifying_key().to_bytes()),
            document_models: vec![json!({ "name": "rfp", "version": "1" })],
            processors: vec![],
            bundle: None,
            capabilities: Capabilities {
                read: vec!["rfp@1".into()],
                write: vec![WriteCapability {
                    model: "proposal@1".into(),
                    kinds: vec!["init".into(), "withdraw".into()],
                }],
            },
            sig: String::new(),
        };
        m.sign(k);
        m
    }

    #[test]
    fn a_signed_manifest_verifies() {
        let k = key(1);
        assert!(manifest(&k).verify().is_ok());
    }

    /// The whole point of signing: content cannot change after publication.
    #[test]
    fn tampering_with_any_field_breaks_the_signature() {
        let k = key(1);
        for (label, mutate) in [
            (
                "name",
                Box::new(|m: &mut Manifest| m.name = "evil".into()) as Box<dyn Fn(&mut Manifest)>,
            ),
            (
                "models",
                Box::new(|m: &mut Manifest| m.document_models.push(json!({"name":"backdoor"}))),
            ),
            (
                "capabilities",
                Box::new(|m: &mut Manifest| m.capabilities.read.push("*".into())),
            ),
            (
                "version",
                Box::new(|m: &mut Manifest| m.version = "9.9.9".into()),
            ),
        ] {
            let mut m = manifest(&k);
            mutate(&mut m);
            assert!(
                m.verify().is_err(),
                "tampering with {label} must break the signature"
            );
        }
    }

    /// Swapping in another key does not help an attacker: the signature was
    /// made over bytes that include the key, so it no longer matches.
    #[test]
    fn substituting_the_publisher_key_breaks_verification() {
        let mut m = manifest(&key(1));
        m.publisher_key = hex::encode(key(2).verifying_key().to_bytes());
        assert!(m.verify().is_err());
    }

    /// Re-signing with a different key produces a VALID manifest — which is
    /// precisely why integrity is not enough, and why the trust store exists.
    #[test]
    fn anyone_can_produce_a_validly_signed_package() {
        let impostor = key(9);
        let mut m = manifest(&key(1));
        m.publisher_key = hex::encode(impostor.verifying_key().to_bytes());
        m.publisher.name = "Powerhouse".into(); // the label is not identity
        m.sign(&impostor);
        assert!(
            m.verify().is_ok(),
            "a self-consistent package verifies; only the trust store can \
             tell this is not the publisher you meant"
        );
    }

    #[test]
    fn an_unsigned_manifest_does_not_verify() {
        let mut m = manifest(&key(1));
        m.sig = String::new();
        assert!(m.verify().is_err());
    }

    #[test]
    fn message_bytes_exclude_the_signature() {
        let k = key(1);
        let m = manifest(&k);
        let mut without = m.clone();
        without.sig = "deadbeef".into();
        assert_eq!(
            m.message_bytes(),
            without.message_bytes(),
            "the signature must not be part of what it covers"
        );
    }

    #[test]
    fn capability_checks_are_exact() {
        let c = Capabilities {
            read: vec!["rfp@1".into()],
            write: vec![WriteCapability {
                model: "proposal@1".into(),
                kinds: vec!["init".into()],
            }],
        };
        assert!(c.may_read("rfp@1"));
        assert!(!c.may_read("proposal@1"), "read is not implied by write");
        assert!(
            !c.may_read("rfp@2"),
            "a different version is a different model"
        );
        assert!(c.may_write("proposal@1", "init"));
        assert!(!c.may_write("proposal@1", "accept"), "kind must match");
        assert!(!c.may_write("rfp@1", "init"), "model must match");
    }

    #[test]
    fn capabilities_describe_themselves_in_plain_language() {
        let c = Capabilities {
            read: vec!["rfp@1".into()],
            write: vec![WriteCapability {
                model: "proposal@1".into(),
                kinds: vec!["init".into(), "withdraw".into()],
            }],
        };
        let lines = c.describe();
        assert!(lines.iter().any(|l| l.contains("read documents")));
        assert!(lines.iter().any(|l| l.contains("init, withdraw")));

        // A plugin asking for nothing must say so, not render an empty list.
        assert_eq!(Capabilities::default().describe().len(), 1);
        assert!(Capabilities::default().describe()[0].contains("nothing"));
    }

    /// A manifest must survive the round trip it will actually take: written
    /// into a document, synced, parsed on another node, and verified there.
    #[test]
    fn a_manifest_survives_json_round_trip_and_still_verifies() {
        let m = manifest(&key(1));
        let wire = serde_json::to_string(&m).expect("serialize");
        let back: Manifest = serde_json::from_str(&wire).expect("deserialize");
        assert_eq!(back, m);
        assert!(back.verify().is_ok());
    }
}
