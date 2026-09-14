//! `open@1` — the backward-compatible, open-field document model.
//!
//! The default model, and the one that reproduces the v1 behaviour exactly:
//! an `open@1` action carries a `field` (and, for `set`, a `value`) and
//! reduces 1:1 to the v1 field-write [`Op`]. A document created with `doc add`
//! before the model layer existed is therefore byte-identical under `open@1`
//! — the rescue is non-breaking.
//!
//! Kinds:
//! - `create` / `set` — payload `{ "field": string, "value": <json> }`.
//! - `delete` — payload `{ "field": string }` (delete field) or
//!   `{}` / `{ "field": null }` (delete the whole doc).
//!
//! The open model has no preconditions: any peer may write any field — exactly
//! the v1 semantic. Authorization is a feature of the richer models, not of
//! `open`.

use crate::action::Action;
use crate::doc::{Doc, ModelRef, Op};
use crate::model::{Model, Reject};

/// The open field model. Holds its own [`ModelRef`] (name + version).
pub struct Open {
    ref_: ModelRef,
}

impl Open {
    pub fn new() -> Self {
        Self {
            ref_: ModelRef::new("open", "1"),
        }
    }
}

impl Default for Open {
    fn default() -> Self {
        Self::new()
    }
}

impl Model for Open {
    fn ref_(&self) -> &ModelRef {
        &self.ref_
    }

    fn validate_payload(&self, kind: &str, payload: &serde_json::Value) -> Result<(), Reject> {
        match kind {
            "create" | "set" => {
                let field = payload
                    .get("field")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| Reject::BadPayload("requires payload.field (string)".into()))?;
                if field.is_empty() {
                    return Err(Reject::BadPayload("payload.field must be non-empty".into()));
                }
                Ok(())
            }
            "delete" => Ok(()), // field may be a string or null
            other => Err(Reject::UnknownKind(other.into())),
        }
    }

    fn check_precondition(&self, _state: &Doc, _action: &Action) -> Result<(), Reject> {
        Ok(())
    }

    fn reduce(&self, _state: &Doc, action: &Action) -> Result<Vec<Op>, Reject> {
        let op = Op {
            doc_id: action.doc_id,
            key: action
                .payload
                .get("field")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            value: action.payload.get("value").cloned(),
            ts: action.ts,
            clock: action.clock.clone(),
            origin: action.origin.clone(),
            sig: action.sig,
        };
        Ok(vec![op])
    }

    fn check_state(&self, _state: &Doc) -> Result<(), Reject> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{DocId, VecClock};
    use ed25519_dalek::SigningKey;

    fn key(seed: u8) -> SigningKey {
        let mut b = [seed; 32];
        b[0] = 0;
        SigningKey::from_bytes(&b)
    }

    fn action(k: &SigningKey, kind: &str, field: &str, value: Option<serde_json::Value>) -> Action {
        let mut a = Action {
            doc_id: DocId::new(),
            model: Open::new().ref_().clone(),
            kind: kind.into(),
            payload: serde_json::json!({ "field": field }),
            ts: 1,
            clock: VecClock::from_pairs(&[("o1".to_string(), 1)]),
            origin: "o1".into(),
            cosig: Vec::new(),
            prev_hash: None,
            sig: [0; 64],
            space: None,
        };
        if let Some(v) = value {
            a.payload["value"] = v;
        }
        a.sign(k);
        a
    }

    fn empty_doc(id: DocId) -> Doc {
        Doc {
            id,
            name: String::new(),
            fields: Default::default(),
        }
    }

    #[test]
    fn set_reduces_to_field_write() {
        let m = Open::new();
        let a = action(&key(1), "set", "body", Some(serde_json::json!("hi")));
        let ops = m.reduce(&empty_doc(a.doc_id), &a).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].key.as_deref(), Some("body"));
        assert_eq!(ops[0].value, Some(serde_json::json!("hi")));
    }

    #[test]
    fn delete_reduces_to_tombstone() {
        let m = Open::new();
        let a = action(&key(1), "delete", "body", None);
        let ops = m.reduce(&empty_doc(a.doc_id), &a).unwrap();
        assert_eq!(ops[0].key.as_deref(), Some("body"));
        assert_eq!(ops[0].value, None);
    }

    #[test]
    fn doc_delete_reduces_to_key_none() {
        let m = Open::new();
        let mut a = action(&key(1), "delete", "ignored", None);
        a.payload = serde_json::json!({});
        let ops = m.reduce(&empty_doc(a.doc_id), &a).unwrap();
        assert_eq!(ops[0].key, None);
    }

    #[test]
    fn rejects_unknown_kind_and_missing_field() {
        let m = Open::new();
        assert!(matches!(
            m.validate_payload("explode", &serde_json::json!({})),
            Err(Reject::UnknownKind(_))
        ));
        assert!(matches!(
            m.validate_payload("set", &serde_json::json!({})),
            Err(Reject::BadPayload(_))
        ));
    }
}
