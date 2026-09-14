//! `/api/inbox` — what needs you, across every space.
//!
//! The Inbox is the landing screen because the alternative is visiting five
//! spaces to discover that nothing is waiting, which is how a coordination
//! tool starts feeling like work.
//!
//! Rows come from [`Attention`] rules declared in each installed app's signed
//! manifest. The evaluation is a pure function over documents so it can be
//! tested without a daemon, and so a slow or broken app cannot slow down the
//! screen everyone lands on.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::Value;

use super::Settings;
use crate::doc::DocId;
use crate::package::Attention;
use crate::store::Store;

/// One thing waiting on you.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Row {
    /// Document name — also the row's identity, since names are unique.
    pub doc: String,
    /// The model, so the console can open the right app view.
    pub model: String,
    /// The app that asked for this row.
    pub app: String,
    pub label: String,
    /// The space the document lives in. Every row carries it: a cross-space
    /// list without it is ambiguous, and ambiguity about scope is the failure
    /// this whole design exists to remove.
    pub space: String,
    /// That space's tier, so the row can be coloured by exposure.
    #[serde(rename = "spaceTier")]
    pub space_tier: String,
}

/// Does every `when` clause hold for this document?
///
/// All of them, not any: a rule with two conditions that fires on one is a
/// rule the author did not write.
fn conditions_hold(rule: &Attention, fields: &Value) -> bool {
    rule.when.iter().all(|(k, want)| {
        fields
            .get(k)
            .map(|got| got == want)
            .unwrap_or(false)
    })
}

/// Is `me` named in the field the rule says is on the hook?
///
/// The field may be a list (`approvers`) or a single value (`org`). Both are
/// ordinary ways for an app to say who is responsible, and refusing one would
/// just push apps into modelling it the other way.
fn names_me(fields: &Value, field: &str, me: &str) -> bool {
    match fields.get(field) {
        Some(Value::Array(a)) => a.iter().any(|v| v.as_str() == Some(me)),
        Some(Value::String(s)) => s == me,
        _ => false,
    }
}

/// Evaluate every installed app's rules against every document.
///
/// Pure over the store so it is testable without a daemon.
pub fn rows_for(store: &Arc<Store>, rules: &[(String, Attention)], me: &str) -> Vec<Row> {
    let mut out = Vec::new();
    for (app, rule) in rules {
        for doc in crate::query::query_docs(store, &rule.model, None) {
            let Some(name) = doc.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let fields = doc.get("fields").cloned().unwrap_or(Value::Null);
            if !conditions_hold(rule, &fields) || !names_me(&fields, &rule.needs, me) {
                continue;
            }
            // A document with no space has no scope to report, and the Inbox
            // is a cross-space screen: a row that cannot say where it lives
            // would be the one ambiguous row on the page.
            let Some(space) = doc_space(store, name) else {
                continue;
            };
            out.push(Row {
                doc: name.to_string(),
                model: rule.model.clone(),
                app: app.clone(),
                label: rule.label.clone(),
                space: space.0,
                space_tier: space.1,
            });
        }
    }
    out.sort_by(|a, b| (&a.space, &a.doc).cmp(&(&b.space, &b.doc)));
    out
}

/// The name and tier of the space a document lives in.
fn doc_space(store: &Arc<Store>, doc_name: &str) -> Option<(String, String)> {
    let id: DocId = store.get(doc_name)?.id;
    let space_id = store.space_of(id)?;
    let tier = store.space_visibility(id)?;
    Some((store.doc_name(space_id), tier))
}

/// Every attention rule on the node, tagged with the app that declared it.
pub fn installed_rules(state: &Arc<Settings>) -> Vec<(String, Attention)> {
    crate::package::install::Installed::load(&state.paths.packages_file())
        .list()
        .iter()
        .flat_map(|p| {
            p.manifest
                .attention
                .iter()
                .cloned()
                .map(|r| (p.manifest.name.clone(), r))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// `GET /api/inbox`
pub async fn list(state: axum::extract::State<Arc<Settings>>) -> Response {
    let me = state
        .snap_rx
        .borrow()
        .reactor
        .peer_id
        .clone()
        .unwrap_or_default();
    let rows = rows_for(&state.store, &installed_rules(&state), &me);
    (StatusCode::OK, axum::Json(rows)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule() -> Attention {
        Attention {
            model: "proposal".into(),
            when: json!({ "status": "submitted" }).as_object().unwrap().clone(),
            needs: "approvers".into(),
            label: "Proposal awaiting your review".into(),
        }
    }

    #[test]
    fn every_condition_must_hold_not_just_one() {
        let mut r = rule();
        r.when = json!({ "status": "submitted", "kind": "grant" })
            .as_object()
            .unwrap()
            .clone();
        assert!(conditions_hold(
            &r,
            &json!({ "status": "submitted", "kind": "grant" })
        ));
        assert!(
            !conditions_hold(&r, &json!({ "status": "submitted", "kind": "loan" })),
            "a rule with two conditions that fires on one is not the rule the author wrote"
        );
    }

    #[test]
    fn a_missing_field_does_not_match() {
        assert!(!conditions_hold(&rule(), &json!({})));
    }

    /// `approvers` is a list, `org` is a single value. Both are ordinary ways
    /// for an app to say who is responsible.
    #[test]
    fn the_responsible_field_may_be_a_list_or_a_single_value() {
        assert!(names_me(&json!({ "approvers": ["a", "b"] }), "approvers", "b"));
        assert!(!names_me(&json!({ "approvers": ["a"] }), "approvers", "b"));
        assert!(names_me(&json!({ "org": "a" }), "org", "a"));
        assert!(!names_me(&json!({ "org": "a" }), "org", "b"));
        assert!(!names_me(&json!({}), "approvers", "a"));
    }
}
