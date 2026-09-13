//! The query surface over the read models. This is what the daemon's
//! settings page and the `query` CLI call: answer a query against the
//! maintained indexes (rather than scanning the action log) and return the
//! result as JSON.

use serde_json::{json, Value};

use crate::document_view::{DocFilter, DocumentView};
use crate::read_model::DocSnapshot;
use crate::relationship::RelationshipIndex;

/// Answers queries against a set of maintained read models.
pub struct QueryService {
    view: std::sync::Arc<DocumentView>,
    index: Option<std::sync::Arc<RelationshipIndex>>,
}

impl QueryService {
    pub fn new(view: std::sync::Arc<DocumentView>, index: Option<std::sync::Arc<RelationshipIndex>>) -> Self {
        Self { view, index }
    }

    /// The snapshot view (for direct access to the full document set).
    pub fn view(&self) -> &std::sync::Arc<DocumentView> {
        &self.view
    }

    /// Answer a document query: every live doc of `model` (all models when
    /// empty), optionally filtered by a field-equality predicate, as a JSON
    /// array of `{ name, model, fields }` objects.
    pub fn query(&self, model: &str, filter: Option<DocFilter>) -> Value {
        let docs = self.view.find(model, filter.as_ref());
        Value::Array(docs.iter().map(to_json).collect())
    }

    /// Follow a relationship: the outgoing targets of `source` for `field`.
    pub fn outgoing(&self, source: &str, field: &str) -> Value {
        let Some(ix) = &self.index else {
            return Value::Array(Vec::new());
        };
        Value::Array(
            ix.outgoing(source, field)
                .into_iter()
                .map(Value::String)
                .collect(),
        )
    }

    /// The incoming sources for `target` on `field`.
    pub fn incoming(&self, target: &str, field: &str) -> Value {
        let Some(ix) = &self.index else {
            return Value::Array(Vec::new());
        };
        Value::Array(ix.incoming(target, field).into_iter().map(Value::String).collect())
    }
}

fn to_json(s: &DocSnapshot) -> Value {
    json!({
        "name": s.name,
        "model": s.model.name,
        "fields": s.fields,
    })
}
