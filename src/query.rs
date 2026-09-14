//! A query projection over the store's documents: the CQRS read model the
//! daemon exposes for the `query` CLI and the settings `/api/query` endpoint.
//!
//! The store is the source of truth. This is a direct, stateless projection
//! over it: every live doc, selected by model and a field-equality filter.
//!
//! It deliberately lives in the *core* crate rather than `ph-reactor-views`.
//! That crate is a consumer (it depends on this one to reach the store), so
//! it cannot be a dependency of the daemon — adding it would be a circular
//! crate dependency, which Cargo rejects. The projection here is the minimal
//! index the daemon needs to answer a query; the richer read models and the
//! processor framework remain in `ph-reactor-views` for standalone use.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::store::Store;

/// A field-equality predicate for [`query_docs`].
///
/// Deserializable so a plugin can send one over the bridge; it is a pure
/// predicate with no way to express anything but field equality, which is what
/// keeps an untrusted caller from smuggling a query it was not granted.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FieldFilter {
    pub field: String,
    pub value: Value,
}

impl FieldFilter {
    pub fn new(field: impl Into<String>, value: Value) -> Self {
        Self {
            field: field.into(),
            value,
        }
    }
}

/// Parse a `"K=V"` filter string. `V` is parsed as JSON when it looks like a
/// JSON value (number, boolean, object, array); otherwise it is kept as a
/// plain string. Returns `None` when there is no `=`.
pub fn parse_filter(spec: &str) -> Option<FieldFilter> {
    let (field, value) = spec.split_once('=')?;
    let value =
        serde_json::from_str::<Value>(value).unwrap_or_else(|_| Value::String(value.to_string()));
    Some(FieldFilter::new(field.trim(), value))
}

/// Query the store's live docs.
///
/// `model` selects one model (empty = every model); `filter` (if set) keeps
/// only docs whose `filter.field` equals `filter.value`. Returns a JSON array
/// of `{ name, model, fields }` objects, sorted by doc name.
pub fn query_docs(store: &Store, model: &str, filter: Option<&FieldFilter>) -> Vec<Value> {
    let mut out = Vec::new();
    for id in store.doc_ids() {
        let Some(state) = store.full_state(id) else {
            continue;
        };
        if state.deleted {
            continue;
        }
        let model_name = state.model.name.clone();
        if !model.is_empty() && model_name != model {
            continue;
        }
        if let Some(f) = filter {
            match state.doc.fields.get(&f.field) {
                Some(field) if field.value == f.value => {}
                _ => continue,
            }
        }
        let fields: BTreeMap<String, Value> = state
            .doc
            .fields
            .iter()
            .map(|(k, v)| (k.clone(), v.value.clone()))
            .collect();
        out.push(json!({
            "name": state.doc.name,
            "model": model_name,
            "fields": fields,
        }));
    }
    out.sort_by(|a, b| a["name"].to_string().cmp(&b["name"].to_string()));
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use ed25519_dalek::SigningKey;
    use serde_json::Value;

    use crate::store::Store;

    use super::{parse_filter, query_docs, FieldFilter};

    fn open_store(dir: &std::path::Path) -> Arc<Store> {
        let mut bytes = [9u8; 32];
        bytes[0] = 0;
        let key = SigningKey::from_bytes(&bytes);
        Store::open(dir, &key, "test-origin").expect("store opens")
    }

    #[test]
    fn selects_by_model_and_filters_by_field() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        let mut f1 = BTreeMap::new();
        f1.insert("status".to_string(), Value::String("todo".into()));
        f1.insert("n".to_string(), Value::from(42));
        s.create_doc("t1", f1).unwrap();
        let mut f2 = BTreeMap::new();
        f2.insert("status".to_string(), Value::String("doing".into()));
        s.create_doc("t2", f2).unwrap();

        // Both docs are the built-in `open` model: selected by model, and
        // empty model returns them all.
        assert_eq!(query_docs(&s, "open", None).len(), 2);
        assert_eq!(query_docs(&s, "", None).len(), 2);
        // An unknown model matches nothing.
        assert!(query_docs(&s, "task", None).is_empty());

        // A string field-equality filter.
        let doing = query_docs(
            &s,
            "",
            Some(&FieldFilter::new("status", Value::String("doing".into()))),
        );
        assert_eq!(doing.len(), 1);
        assert_eq!(doing[0]["name"], "t2");

        // A JSON-number field matches a numeric value...
        let numeric = query_docs(&s, "", Some(&FieldFilter::new("n", Value::from(42))));
        assert_eq!(numeric.len(), 1);
        assert_eq!(numeric[0]["name"], "t1");

        // ...but the *string* "42" must not match the numeric field.
        let as_string = query_docs(
            &s,
            "",
            Some(&FieldFilter::new("n", Value::String("42".into()))),
        );
        assert!(
            as_string.is_empty(),
            "string \"42\" must not match the numeric field"
        );

        // A missing field never matches.
        assert!(query_docs(&s, "", Some(&FieldFilter::new("missing", Value::from(1)))).is_empty());
    }

    #[test]
    fn filter_parsing_treats_json_as_values() {
        assert_eq!(parse_filter("n=42").unwrap().value, Value::from(42));
        assert_eq!(
            parse_filter("status=todo").unwrap().value,
            Value::String("todo".into())
        );
        assert_eq!(parse_filter("ok=true").unwrap().value, Value::Bool(true));
        assert!(parse_filter("noequalsign").is_none());
    }
}
