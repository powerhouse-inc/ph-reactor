//! Self-update: signed releases, distributed the same way packages are.
//!
//! # Why this reuses the package machinery
//!
//! A release is a signed artifact from a publisher this operator has decided to
//! trust, delivered as content-addressed chunks. That is exactly what a package
//! is, so this reuses [`crate::package::trust::TrustStore`], the blob store and
//! the same signing discipline rather than inventing a second trust model. A
//! second one would be a second thing to get right.
//!
//! # Why applying is not automatic by default
//!
//! Installing a plugin is deliberately an operator decision, because a valid
//! signature proves integrity and says nothing about whether the publisher is
//! one you accept. A binary is strictly more dangerous than a plugin: no
//! sandbox, no capability list, and a bad one takes the node down with it. It
//! would be incoherent to require consent for the lesser risk and not the
//! greater, so `update.auto` is opt-in and off by default.
//!
//! What is always on is the *check*: knowing an update exists costs nothing and
//! an operator who is not told cannot decide.

pub mod apply;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::blob::BlobRef;

/// A published build of the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Release {
    /// Semver, without a leading `v`.
    pub version: String,
    /// The target this binary runs on, e.g. `x86_64-unknown-linux-musl`. A
    /// release for another target is not an update, it is a brick.
    pub platform: String,
    #[serde(default)]
    pub notes: String,
    /// The binary itself, as content-addressed chunks.
    pub binary: BlobRef,
    /// The publisher's ed25519 public key, hex. Checked against the same trust
    /// store packages use.
    pub publisher_key: String,
    #[serde(default)]
    pub sig: String,
}

impl Release {
    /// The canonical bytes a publisher signs. Excludes the signature, exactly
    /// as `Action::message_bytes` and `Manifest::message_bytes` do.
    pub fn message_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.sig = String::new();
        serde_json::to_vec(&clone).expect("a release serializes")
    }

    pub fn sign(&mut self, key: &SigningKey) {
        let mb = self.message_bytes();
        self.sig = hex::encode(key.sign(&mb).to_bytes());
    }

    /// Integrity only: the release is what it was signed as. Whether that
    /// publisher should be trusted is a separate question the trust store
    /// answers — the same separation packages make.
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
            .map_err(|_| "signature does not verify: the release was altered".to_string())
    }

    /// Whether this release supersedes `current`.
    ///
    /// Strictly greater, never equal: re-applying the running version would be
    /// a restart loop wearing the costume of an upgrade.
    pub fn is_newer_than(&self, current: &str) -> bool {
        matches!(compare(&self.version, current), Some(std::cmp::Ordering::Greater))
    }

    /// Whether this release can run here at all.
    pub fn runs_on(&self, platform: &str) -> bool {
        self.platform == platform
    }
}

/// The target triple this binary was built for.
///
/// A release for a different target must never be applied — swapping in a
/// binary that cannot exec leaves a node with no daemon and no way to say why.
pub fn current_platform() -> &'static str {
    // Set at compile time from the actual build target rather than guessed at
    // runtime, so it cannot disagree with the binary it describes.
    concat!(
        env!("PH_TARGET_ARCH"),
        "-",
        env!("PH_TARGET_VENDOR"),
        "-",
        env!("PH_TARGET_OS"),
        env!("PH_TARGET_ENV_SUFFIX")
    )
}

/// Compares two dotted numeric versions.
///
/// `None` when either side is not something this understands, which callers
/// must treat as "do not upgrade". Guessing at a version string is how a node
/// talks itself into installing something older.
pub fn compare(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let parse = |s: &str| -> Option<Vec<u64>> {
        let core = s.trim().trim_start_matches('v');
        // Pre-release and build metadata are not ordered here; a version
        // carrying them is not comparable rather than silently truncated.
        if core.contains('-') || core.contains('+') || core.is_empty() {
            return None;
        }
        core.split('.').map(|p| p.parse::<u64>().ok()).collect()
    };
    let (mut x, mut y) = (parse(a)?, parse(b)?);
    let n = x.len().max(y.len());
    x.resize(n, 0);
    y.resize(n, 0);
    Some(x.cmp(&y))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn release(k: &SigningKey, version: &str) -> Release {
        let mut r = Release {
            version: version.into(),
            platform: "x86_64-unknown-linux-musl".into(),
            notes: "plugin packages".into(),
            binary: BlobRef {
                hash: crate::doc::Hash32::of(b"a binary"),
                size: 8,
                chunks: vec![crate::doc::Hash32::of(b"a binary")],
            },
            publisher_key: hex::encode(k.verifying_key().to_bytes()),
            sig: String::new(),
        };
        r.sign(k);
        r
    }

    #[test]
    fn a_signed_release_verifies() {
        assert!(release(&key(1), "1.9.0").verify().is_ok());
    }

    /// The binary hash is inside the signed bytes, so a release cannot be
    /// re-pointed at a different binary without breaking.
    #[test]
    fn swapping_the_binary_breaks_the_signature() {
        let mut r = release(&key(1), "1.9.0");
        r.binary.hash = crate::doc::Hash32::of(b"a different binary");
        assert!(r.verify().is_err());
    }

    #[test]
    fn tampering_with_the_version_breaks_the_signature() {
        let mut r = release(&key(1), "1.9.0");
        r.version = "99.0.0".into();
        assert!(r.verify().is_err());
    }

    #[test]
    fn version_ordering_is_numeric_not_lexical() {
        assert_eq!(compare("1.10.0", "1.9.0"), Some(Ordering::Greater));
        assert_eq!(compare("1.9.0", "1.10.0"), Some(Ordering::Less));
        assert_eq!(compare("2.0.0", "1.99.99"), Some(Ordering::Greater));
        assert_eq!(compare("1.8.0", "1.8.0"), Some(Ordering::Equal));
        // Shorter is padded, not treated as smaller by length.
        assert_eq!(compare("1.8", "1.8.0"), Some(Ordering::Equal));
        assert_eq!(compare("1.8.1", "1.8"), Some(Ordering::Greater));
        // A leading v is tolerated on either side.
        assert_eq!(compare("v1.9.0", "1.8.0"), Some(Ordering::Greater));
    }

    /// Anything not understood must be incomparable, so the caller declines to
    /// upgrade rather than guessing.
    #[test]
    fn an_unparseable_version_is_not_comparable() {
        for bad in ["", "nightly", "1.8.0-rc1", "1.8.0+build7", "1.x.0", "  "] {
            assert_eq!(compare(bad, "1.8.0"), None, "{bad:?} must not compare");
            assert_eq!(compare("1.8.0", bad), None, "{bad:?} must not compare");
        }
    }

    /// Equal is not newer. Applying the running version would restart forever.
    #[test]
    fn a_release_must_be_strictly_newer() {
        let r = release(&key(1), "1.8.0");
        assert!(!r.is_newer_than("1.8.0"));
        assert!(!r.is_newer_than("1.9.0"));
        assert!(r.is_newer_than("1.7.0"));
    }

    /// An unparseable running version must never be upgraded over -- the node
    /// cannot tell whether the release is ahead or behind.
    #[test]
    fn an_unknown_current_version_blocks_the_upgrade() {
        let r = release(&key(1), "1.9.0");
        assert!(!r.is_newer_than("some-dev-build"));
    }

    /// A release built for another target is not an update, it is a brick.
    #[test]
    fn a_release_for_another_platform_is_refused() {
        let r = release(&key(1), "1.9.0");
        assert!(r.runs_on("x86_64-unknown-linux-musl"));
        assert!(!r.runs_on("aarch64-apple-darwin"));
        assert!(!r.runs_on(""));
    }

    #[test]
    fn message_bytes_exclude_the_signature() {
        let r = release(&key(1), "1.9.0");
        let mut other = r.clone();
        other.sig = "deadbeef".into();
        assert_eq!(r.message_bytes(), other.message_bytes());
    }

    /// The platform string must describe the binary that reports it.
    #[test]
    fn the_current_platform_is_a_target_triple() {
        let p = current_platform();
        assert!(p.split('-').count() >= 3, "not a target triple: {p}");
        assert!(!p.contains("unknown-unknown"), "unresolved target: {p}");
    }
}
