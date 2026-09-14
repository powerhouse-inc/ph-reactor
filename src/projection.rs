//! Projections: publishing a narrower record from one space into another.
//!
//! The transparency case — "show the finances from contributor billing on the
//! public forum" — is not one document shown two ways. Every member holds a
//! full replica, so redaction-by-view is a lie: the document is already on the
//! reader's disk, and a filter in the UI is a request, not a boundary.
//!
//! So it is two documents. The private one stays in its space. The public one
//! is created here, carrying only the fields the manifest named, and living in
//! a different space — which means the private one is structurally unreachable
//! rather than merely unrendered.
//!
//! The rule runs from the installed manifest, not from code inside the app, so
//! the operator saw it at install time and the publisher signed it.

use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::doc::ModelRef;
use crate::package::Projection;
use crate::store::{DocChange, Store};

/// Does this change trigger this projection?
///
/// Both halves matter. Matching the model alone would fire on every write to a
/// milestone, publishing a record when one is merely edited; matching the
/// reducer alone would fire on any model that happens to share a verb.
pub fn matches(p: &Projection, c: &DocChange) -> bool {
    c.model.name == p.from && c.action_kind.as_deref() == Some(p.on.as_str())
}

/// The name of the document a projection produces.
///
/// Derived from the source, so running the same projection twice targets the
/// same document and the second run is refused as a duplicate rather than
/// quietly publishing the record again.
pub fn target_name(p: &Projection, source_name: &str) -> String {
    format!("{source_name}-{}", p.to)
}

/// The payload for the published document: the named fields and nothing else.
///
/// Fields absent from the source are omitted rather than written as null. A
/// projection that names a field the source does not have is an authoring
/// mistake, and an explicit null would make the published record look like a
/// deliberate statement that the value is empty.
pub fn payload(p: &Projection, c: &DocChange, name: &str) -> Value {
    let mut out = Map::new();
    out.insert("name".into(), json!(name));
    for f in &p.fields {
        if let Some(v) = c.state.doc.fields.get(f) {
            if !v.deleted {
                out.insert(f.clone(), v.value.clone());
            }
        }
    }
    Value::Object(out)
}

/// Apply every matching projection for one change.
///
/// Failures are logged and skipped, never retried into a loop: a projection
/// that cannot be written (its space is absent, its model is not registered)
/// is a configuration problem, and a tight retry would turn it into a
/// livelock that also floods the log.
pub fn run_one(store: &Arc<Store>, projections: &[Projection], c: &DocChange) {
    for p in projections.iter().filter(|p| matches(p, c)) {
        let name = target_name(p, &c.name);
        if store.get(&name).is_some() {
            continue; // already published
        }
        let Some(space) = store.get(&p.into).map(|d| d.id) else {
            tracing::warn!(
                "projection {} -> {}: no space named '{}'; not publishing",
                p.from,
                p.to,
                p.into
            );
            continue;
        };
        let body = payload(p, c, &name);
        match store.create_doc_in_space(&name, &ModelRef::new(&p.to, "1"), &body, Some(space)) {
            Ok(_) => tracing::info!(
                "published {name} into '{}' ({} field(s))",
                p.into,
                p.fields.len()
            ),
            Err(e) => tracing::warn!("projection {} -> {}: {e}", p.from, p.to),
        }
    }
}

/// Feed-driven loop: every installed package's projections, applied to every
/// change. The manifest list is re-read per change so installing or removing a
/// package takes effect without a restart.
pub fn spawn(
    store: Arc<Store>,
    packages_file: std::path::PathBuf,
) -> tokio::task::JoinHandle<()> {
    let mut feed = store.subscribe_changes();
    tokio::spawn(async move {
        while let Some(change) = feed.recv().await {
            let installed = crate::package::install::Installed::load(&packages_file);
            for pkg in installed.list() {
                if pkg.manifest.projections.is_empty() {
                    continue;
                }
                run_one(&store, &pkg.manifest.projections, &change);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Doc, DocId, Field, VecClock};
    use crate::store::DocState;
    use std::collections::BTreeMap;

    fn proj() -> Projection {
        Projection {
            from: "milestone".into(),
            on: "accept".into(),
            to: "ledger-entry".into(),
            into: "commons".into(),
            fields: vec!["amount".into(), "org".into()],
        }
    }

    fn field(v: Value) -> Field {
        Field {
            value: v,
            version: VecClock::default(),
            ts: 1,
            origin: "alice".into(),
            deleted: false,
        }
    }

    fn change(model: &str, kind: &str) -> DocChange {
        let mut fields = BTreeMap::new();
        fields.insert("amount".to_string(), field(json!(500)));
        fields.insert("org".to_string(), field(json!("Powerhouse")));
        fields.insert("evidence".to_string(), field(json!("bank statement, invoice #7")));
        let doc = Doc {
            id: DocId::new(),
            name: "m1".into(),
            fields,
        };
        DocChange {
            doc_id: doc.id,
            name: "m1".into(),
            model: ModelRef::new(model, "1"),
            deleted: false,
            state: DocState {
                doc: doc.clone(),
                clock: VecClock::default(),
                deleted: false,
                log_hash: None,
                model: ModelRef::new(model, "1"),
                space: None,
            },
            ts: 1,
            action_kind: Some(kind.into()),
            action_field: None,
            action_value: None,
        }
    }

    #[test]
    fn both_the_model_and_the_reducer_must_match() {
        let p = proj();
        assert!(matches(&p, &change("milestone", "accept")));
        assert!(
            !matches(&p, &change("milestone", "reject")),
            "editing a milestone must not publish a payment record"
        );
        assert!(!matches(&p, &change("invoice", "accept")));
    }

    /// The whole point: what stays behind stays behind.
    #[test]
    fn only_the_named_fields_cross_the_boundary() {
        let p = proj();
        let body = payload(&p, &change("milestone", "accept"), "m1-ledger-entry");
        assert_eq!(body["amount"], json!(500));
        assert_eq!(body["org"], json!("Powerhouse"));
        assert!(
            body.get("evidence").is_none(),
            "a field the projection did not name must not be published: {body}"
        );
    }

    #[test]
    fn a_missing_field_is_omitted_rather_than_published_as_null() {
        let mut p = proj();
        p.fields.push("nonexistent".into());
        let body = payload(&p, &change("milestone", "accept"), "x");
        assert!(
            !body.as_object().unwrap().contains_key("nonexistent"),
            "an absent value must not be published as a deliberate empty one"
        );
    }

    #[test]
    fn the_target_name_is_derived_so_publishing_twice_is_refused() {
        let p = proj();
        assert_eq!(target_name(&p, "m1"), "m1-ledger-entry");
        assert_eq!(target_name(&p, "m1"), target_name(&p, "m1"));
    }

    #[test]
    fn the_prompt_says_what_will_be_published_where() {
        let d = proj().describe();
        assert!(d.contains("amount, org"), "{d}");
        assert!(d.contains("commons"), "{d}");
        assert!(d.contains("accept"), "{d}");
    }

    #[test]
    fn a_projection_that_publishes_nothing_is_refused() {
        let mut p = proj();
        p.fields.clear();
        assert!(p.validate().is_err());
        assert!(proj().validate().is_ok());
    }
}
