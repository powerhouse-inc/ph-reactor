//! Which publishers this node accepts packages from.
//!
//! [`super::Manifest::verify`] establishes **integrity**: nobody altered the
//! package after signing. That is not enough on its own, because anyone can
//! produce a perfectly valid signature over a malicious package using their own
//! key — `anyone_can_produce_a_validly_signed_package` in the parent module
//! demonstrates exactly that.
//!
//! **Authenticity** is the separate question this file answers: is the key that
//! signed it one this operator accepts? That is a human decision, pinned on
//! first use in the same way the store pins peer keys, and never inferred from
//! a `publisher.name` that anyone can type.
//!
//! Conflating the two is the classic supply-chain mistake, so they are kept in
//! different modules with different failure messages.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// One trusted publisher, as recorded when the operator approved it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedPublisher {
    /// ed25519 public key, hex. The identity.
    pub key: String,
    /// What the publisher called itself when approved. Recorded so a later
    /// rename is visible rather than silent; it is never used to match.
    #[serde(default)]
    pub name: String,
    /// RFC 3339 UTC of the approval.
    #[serde(default)]
    pub approved_at: String,
}

/// The set of publishers this node will install from.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrustStore {
    #[serde(default)]
    publishers: BTreeMap<String, TrustedPublisher>,
}

impl TrustStore {
    /// Loads the store. A missing file means nothing is trusted yet, which is
    /// the correct starting state rather than an error.
    pub fn load(path: &Path) -> Self {
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match serde_json::from_str(&raw) {
            Ok(s) => s,
            Err(e) => {
                // Failing closed is the safe direction: an unreadable trust
                // file must not be silently treated as "trust everything".
                tracing::error!(
                    "{} is unreadable ({e}); treating every publisher as untrusted",
                    path.display()
                );
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let body = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, body).map_err(|e| format!("writing {}: {e}", path.display()))
    }

    pub fn is_trusted(&self, key: &str) -> bool {
        self.publishers.contains_key(key)
    }

    pub fn get(&self, key: &str) -> Option<&TrustedPublisher> {
        self.publishers.get(key)
    }

    pub fn list(&self) -> Vec<&TrustedPublisher> {
        self.publishers.values().collect()
    }

    /// Records an operator's decision to trust `key`.
    ///
    /// Idempotent, and it does **not** overwrite the recorded name: the name
    /// under which a key was first approved is the useful one, so a publisher
    /// that later renames itself cannot quietly rewrite history.
    pub fn trust(&mut self, key: &str, name: &str) {
        self.publishers
            .entry(key.to_string())
            .or_insert_with(|| TrustedPublisher {
                key: key.to_string(),
                name: name.to_string(),
                approved_at: crate::status::rfc3339_now(),
            });
    }

    /// Withdraws trust. Already-installed packages are unaffected — removing
    /// them is a separate decision, because uninstalling silently would be a
    /// surprising side effect of an unrelated action.
    pub fn revoke(&mut self, key: &str) -> bool {
        self.publishers.remove(key).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_trusted_by_default() {
        let dir = tempfile::tempdir().expect("tmp");
        let s = TrustStore::load(&dir.path().join("absent.json"));
        assert!(!s.is_trusted("aabb"));
        assert!(s.list().is_empty());
    }

    #[test]
    fn trust_round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("tmp");
        let p = dir.path().join("publishers.json");
        let mut s = TrustStore::default();
        s.trust("aabb", "Powerhouse");
        s.save(&p).expect("save");

        let back = TrustStore::load(&p);
        assert!(back.is_trusted("aabb"));
        assert_eq!(back.get("aabb").expect("present").name, "Powerhouse");
        assert!(!back.get("aabb").expect("present").approved_at.is_empty());
    }

    /// A publisher that renames itself must not rewrite the name the operator
    /// actually approved.
    #[test]
    fn re_trusting_does_not_overwrite_the_approved_name() {
        let mut s = TrustStore::default();
        s.trust("aabb", "Powerhouse");
        s.trust("aabb", "Definitely Powerhouse");
        assert_eq!(s.get("aabb").expect("present").name, "Powerhouse");
        assert_eq!(s.list().len(), 1, "trusting twice adds one entry");
    }

    #[test]
    fn revoke_removes_only_that_publisher() {
        let mut s = TrustStore::default();
        s.trust("aabb", "A");
        s.trust("ccdd", "B");
        assert!(s.revoke("aabb"));
        assert!(!s.is_trusted("aabb"));
        assert!(s.is_trusted("ccdd"));
        assert!(!s.revoke("aabb"), "revoking twice reports nothing removed");
    }

    /// An unreadable trust file must fail CLOSED. Treating a corrupt file as
    /// "trust everything" would turn a disk error into a supply-chain hole.
    #[test]
    fn a_corrupt_trust_file_trusts_nobody() {
        let dir = tempfile::tempdir().expect("tmp");
        let p = dir.path().join("publishers.json");
        std::fs::write(&p, "{ this is not json").expect("write");
        let s = TrustStore::load(&p);
        assert!(s.list().is_empty());
        assert!(!s.is_trusted("aabb"));
    }

    /// Trust is per-key, never per-name: two publishers may both call
    /// themselves "Powerhouse" and only one can be the approved key.
    #[test]
    fn trust_is_keyed_by_the_key_not_the_name() {
        let mut s = TrustStore::default();
        s.trust("aabb", "Powerhouse");
        assert!(s.is_trusted("aabb"));
        assert!(
            !s.is_trusted("eeff"),
            "an impostor using the same display name is not trusted"
        );
    }
}
