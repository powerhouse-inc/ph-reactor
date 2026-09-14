//! Out-of-band co-signing of quorum-gated actions.
//!
//! Some reducers (`add-manager`) declare a quorum: a minimum number of
//! distinct group members must sign the action before the store will apply
//! it. Nothing in the daemon could produce such an action — `Action` carried
//! a `cosig` field and the store verified it, but no interface ever populated
//! it, so every quorum-gated reducer was unreachable.
//!
//! This carries a partially-signed action between members as a token, the
//! same shape the invite flow already uses:
//!
//! ```text
//! proposer:  propose  -> PROPOSAL:<base64>     (origin-signed)
//! member:    cosign   -> PROPOSAL:<base64>     (same bytes + their CoSig)
//! proposer:  submit                            (applies + gossips)
//! ```
//!
//! It is safe to pass through any channel. [`Action::message_bytes`] excludes
//! both `sig` and `cosig`, so every signer covers identical bytes; tampering
//! with the payload invalidates the existing signatures, and [`decode`]
//! verifies them before returning.
//!
//! A proposal is pinned to the document's log position through `prev_hash`,
//! which the store enforces strictly. If the document changes between propose
//! and submit, the proposal is rejected and must be re-made — that is the
//! property that keeps the log tamper-evident, not a defect.

use ed25519_dalek::{SigningKey, VerifyingKey};

use crate::action::{Action, CoSig};

/// Prefix so a pasted token is recognisable, and so a truncated paste fails
/// with a clear message rather than a base64 error.
const PREFIX: &str = "PROPOSAL:";

/// Encodes an action as a shareable token.
pub fn encode(action: &Action) -> Result<String, String> {
    let json = serde_json::to_vec(action).map_err(|e| format!("encoding the proposal: {e}"))?;
    Ok(format!("{PREFIX}{}", base64_encode(&json)))
}

/// Decodes a token and verifies every signature it carries.
///
/// `key_of` resolves an origin to its public key; an origin it does not know
/// is an error rather than a skipped signature, so an unknown co-signer can
/// never silently count toward a quorum.
pub fn decode<F>(token: &str, key_of: F) -> Result<Action, String>
where
    F: Fn(&str) -> Option<VerifyingKey>,
{
    let body = token
        .trim()
        .strip_prefix(PREFIX)
        .ok_or_else(|| format!("not a proposal token (expected it to start with {PREFIX})"))?;
    let raw = base64_decode(body)?;
    let action: Action =
        serde_json::from_slice(&raw).map_err(|e| format!("malformed proposal: {e}"))?;

    let origin_key = key_of(&action.origin)
        .ok_or_else(|| format!("unknown proposer {}: no key for it", action.origin))?;
    if !action.verify(&origin_key) {
        return Err("the proposer's signature does not verify (the proposal was altered)".into());
    }
    for (i, cs) in action.cosig.iter().enumerate() {
        let k = key_of(&cs.origin)
            .ok_or_else(|| format!("unknown co-signer {}: no key for it", cs.origin))?;
        if !action.verify_cosig(i, &k) {
            return Err(format!(
                "co-signature from {} does not verify (the proposal was altered)",
                cs.origin
            ));
        }
    }
    Ok(action)
}

/// Adds `origin`'s co-signature to an action.
///
/// Re-signing by an origin that already signed is refused: it would look like
/// progress toward a quorum while adding no second party.
pub fn add_cosig(action: &mut Action, origin: &str, key: &SigningKey) -> Result<(), String> {
    if action.origin == origin {
        return Err(
            "you proposed this action; your signature already counts toward the quorum".into(),
        );
    }
    if action.cosig.iter().any(|c| c.origin == origin) {
        return Err("you have already co-signed this proposal".into());
    }
    let mb = action.message_bytes();
    let sig = ed25519_dalek::Signer::sign(key, &mb).to_bytes();
    action.cosig.push(CoSig {
        origin: origin.to_string(),
        sig,
    });
    Ok(())
}

/// How many distinct members have endorsed, counting the proposer. Mirrors
/// the store's quorum arithmetic so a CLI can report progress without
/// attempting the apply.
pub fn endorser_count(action: &Action, members: &[String]) -> usize {
    let mut seen: Vec<&str> = Vec::new();
    let is_member = |o: &str| members.iter().any(|m| m == o);
    if is_member(&action.origin) {
        seen.push(action.origin.as_str());
    }
    for cs in &action.cosig {
        if is_member(&cs.origin) && !seen.contains(&cs.origin.as_str()) {
            seen.push(cs.origin.as_str());
        }
    }
    seen.len()
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .map_err(|e| format!("proposal is not valid base64: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{DocId, ModelRef, VecClock};
    use serde_json::json;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn action(origin: &str, k: &SigningKey) -> Action {
        let mut a = Action {
            doc_id: DocId::default(),
            model: ModelRef::new("group", "1"),
            kind: "add-manager".into(),
            payload: json!({ "member": "12D3KooW-new" }),
            ts: 1,
            clock: VecClock::default(),
            origin: origin.to_string(),
            cosig: Vec::new(),
            prev_hash: None,
            sig: [0u8; 64],
            space: None,
        };
        a.sign(k);
        a
    }

    fn keyring(pairs: Vec<(&'static str, VerifyingKey)>) -> impl Fn(&str) -> Option<VerifyingKey> {
        move |o| pairs.iter().find(|(n, _)| *n == o).map(|(_, k)| *k)
    }

    #[test]
    fn token_round_trips_with_a_cosignature() {
        let (ka, kb) = (key(1), key(2));
        let mut a = action("alice", &ka);
        add_cosig(&mut a, "bob", &kb).expect("cosign");

        let token = encode(&a).expect("encode");
        assert!(token.starts_with(PREFIX));
        let back = decode(
            &token,
            keyring(vec![
                ("alice", ka.verifying_key()),
                ("bob", kb.verifying_key()),
            ]),
        )
        .expect("decode");
        assert_eq!(back.cosig.len(), 1);
        assert_eq!(back.cosig[0].origin, "bob");
    }

    #[test]
    fn a_tampered_payload_is_rejected() {
        let ka = key(1);
        let a = action("alice", &ka);
        let mut tampered = a.clone();
        tampered.payload = json!({ "member": "12D3KooW-attacker" });
        // Re-encode WITHOUT re-signing: this is what an interceptor can do.
        let token = encode(&tampered).expect("encode");
        let err =
            decode(&token, keyring(vec![("alice", ka.verifying_key())])).expect_err("must reject");
        assert!(err.contains("does not verify"), "got: {err}");
    }

    #[test]
    fn a_forged_cosignature_is_rejected() {
        let (ka, kb, kc) = (key(1), key(2), key(3));
        let mut a = action("alice", &ka);
        // Carol signs, but claims to be Bob.
        let mb = a.message_bytes();
        a.cosig.push(CoSig {
            origin: "bob".into(),
            sig: ed25519_dalek::Signer::sign(&kc, &mb).to_bytes(),
        });
        let token = encode(&a).expect("encode");
        let err = decode(
            &token,
            keyring(vec![
                ("alice", ka.verifying_key()),
                ("bob", kb.verifying_key()),
            ]),
        )
        .expect_err("must reject");
        assert!(err.contains("does not verify"), "got: {err}");
    }

    #[test]
    fn an_unknown_cosigner_is_an_error_not_a_silent_skip() {
        let (ka, kb) = (key(1), key(2));
        let mut a = action("alice", &ka);
        add_cosig(&mut a, "mallory", &kb).expect("cosign");
        let token = encode(&a).expect("encode");
        let err =
            decode(&token, keyring(vec![("alice", ka.verifying_key())])).expect_err("must reject");
        assert!(err.contains("unknown co-signer"), "got: {err}");
    }

    #[test]
    fn the_proposer_cannot_cosign_their_own_action() {
        let ka = key(1);
        let mut a = action("alice", &ka);
        let err = add_cosig(&mut a, "alice", &ka).expect_err("must refuse");
        assert!(err.contains("already counts"), "got: {err}");
        assert!(a.cosig.is_empty());
    }

    #[test]
    fn cosigning_twice_is_refused() {
        let (ka, kb) = (key(1), key(2));
        let mut a = action("alice", &ka);
        add_cosig(&mut a, "bob", &kb).expect("first");
        let err = add_cosig(&mut a, "bob", &kb).expect_err("second must fail");
        assert!(err.contains("already co-signed"), "got: {err}");
        assert_eq!(a.cosig.len(), 1);
    }

    #[test]
    fn endorsers_count_the_proposer_and_ignore_non_members() {
        let (ka, kb, kc) = (key(1), key(2), key(3));
        let members = vec!["alice".to_string(), "bob".to_string()];

        let mut a = action("alice", &ka);
        assert_eq!(
            endorser_count(&a, &members),
            1,
            "the proposer counts as one"
        );

        add_cosig(&mut a, "bob", &kb).expect("bob");
        assert_eq!(endorser_count(&a, &members), 2, "two distinct members");

        add_cosig(&mut a, "carol", &kc).expect("carol");
        assert_eq!(
            endorser_count(&a, &members),
            2,
            "a non-member must not count toward the quorum"
        );
    }

    #[test]
    fn a_non_proposal_string_fails_clearly() {
        let err = decode("hello", |_| None).expect_err("must reject");
        assert!(err.contains("not a proposal token"), "got: {err}");
    }
}
