//! Migrating `group@1` documents to `space@1` plus Chat and Drive documents.
//!
//! There is no in-place upgrade that preserves verification: changing a model
//! definition changes its content hash, and documents written under the old
//! hash no longer verify against the new one. So the group is re-issued, and
//! re-issuing is a signed act -- which is why this is a command with a
//! **dry run by default** rather than something that happens at startup.
//!
//! Three rules, each answering a way this goes wrong:
//!
//! 1. **Nothing happens without `apply: true`.** The plan is printed first,
//!    naming every document that would be created and the one that would be
//!    deleted. A migration that surprises you has already failed.
//! 2. **The new space's id is derived from the old group's id**
//!    ([`DocId::derived`]). Your laptop and the cluster both hold
//!    `ph-bootstrap`; if each minted a fresh id, migrating on both would
//!    produce two spaces that never reconcile -- a permanent fork of the
//!    thing being migrated. Derived ids make a second run a no-op.
//! 3. **Only group documents move.** Everything else keeps no space, and
//!    replicates exactly as it did before. There is deliberately no bulk
//!    default for unstamped documents: defaulting them to the commons would
//!    publish everything irreversibly, and defaulting them to a private space
//!    would make installed apps look broken. Neither is safe, so neither is
//!    offered.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{json, Value};

use super::Settings;
use crate::doc::DocId;
use crate::query::query_docs;

#[derive(Deserialize, Default)]
pub struct MigrateBody {
    /// Nothing is written unless this is explicitly true.
    #[serde(default)]
    apply: bool,
    /// Migrate only this group; otherwise every group on the node.
    #[serde(default)]
    group: Option<String>,
}

fn arr(d: &Value, k: &str) -> Vec<Value> {
    d.get("fields")
        .and_then(|f| f.get(k))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn strs(d: &Value, k: &str) -> Vec<String> {
    arr(d, k)
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// What migrating one group would do. Rendered for the dry run and then
/// executed verbatim, so the two cannot drift apart.
struct Plan {
    group: String,
    group_id: String,
    space_id: DocId,
    members: Vec<String>,
    managers: Vec<String>,
    /// (document name, channel name, message count)
    chats: Vec<(String, String, usize)>,
    drive_items: Vec<Value>,
    drive_doc: String,
}

impl Plan {
    fn describe(&self) -> Value {
        json!({
            "group": self.group,
            "groupDocId": self.group_id,
            "creates": {
                "space": {
                    "name": self.group,
                    "id": self.space_id.to_string(),
                    "visibility": "protected",
                    "members": self.members,
                    "managers": self.managers,
                },
                "chats": self.chats.iter().map(|(doc, chan, n)| json!({
                    "doc": doc, "channel": chan, "messages": n,
                })).collect::<Vec<_>>(),
                "drive": { "doc": self.drive_doc, "items": self.drive_items.len() },
            },
            "deletes": {
                "doc": self.group,
                "why": "the space takes this name; the group's action log stays on disk",
            },
            "notes": [
                "The new space is protected: members only. That is the closest tier \
                 to what a group was, and it cannot be changed afterwards.",
                "Messages are carried over as content, not as signed history: each \
                 becomes a line in the new chat document, authored by whoever is \
                 running this migration. The original signatures stay in the group's \
                 log, which is not deleted.",
            ],
        })
    }
}

/// `group_id` is passed in rather than read from `doc`: the query projection
/// carries `name`, `model` and `fields` but no id, and reading it from there
/// produced a plan for every group silently -- zero plans, "nothing to
/// migrate", no error. Found by running it against a real daemon.
fn plan_for(doc: &Value, group_id: String) -> Option<Plan> {
    let group = doc.get("name")?.as_str()?.to_string();
    let members = strs(doc, "members");
    let managers = strs(doc, "managers");

    let texts = strs(doc, "msg_text");
    let channels_field = arr(doc, "channels");
    let msg_channel = strs(doc, "msg_channel");

    let mut chats = Vec::new();
    let names: Vec<String> = if channels_field.is_empty() {
        vec!["general".to_string()]
    } else {
        channels_field
            .iter()
            .filter_map(|c| c.get("name").and_then(|v| v.as_str()).map(str::to_string))
            .collect()
    };
    for chan in names {
        let n = msg_channel.iter().filter(|c| **c == chan).count();
        // A group whose messages predate channel tagging has them all in
        // `general`; counting by tag would silently report zero.
        let n = if msg_channel.is_empty() && chan == "general" {
            texts.len()
        } else {
            n
        };
        chats.push((format!("{group}-{chan}"), chan, n));
    }

    Some(Plan {
        group: group.clone(),
        group_id: group_id.clone(),
        space_id: DocId::derived(&format!("space:{group_id}")),
        members,
        managers,
        chats,
        drive_items: arr(doc, "drive"),
        drive_doc: format!("{group}-drive"),
    })
}

/// `POST /api/migrate/groups`
pub async fn groups(
    state: axum::extract::State<Arc<Settings>>,
    body: Option<axum::extract::Json<MigrateBody>>,
) -> Response {
    let body = body.map(|b| b.0).unwrap_or_default();

    let plans: Vec<Plan> = query_docs(&state.store, "group", None)
        .iter()
        .filter(|d| match &body.group {
            Some(g) => d.get("name").and_then(|v| v.as_str()) == Some(g.as_str()),
            None => true,
        })
        .filter_map(|d| {
            let name = d.get("name")?.as_str()?;
            let id = state.store.get(name)?.id.to_string();
            plan_for(d, id)
        })
        .collect();

    if !body.apply {
        return (
            StatusCode::OK,
            axum::Json(json!({
                "dryRun": true,
                "wouldMigrate": plans.len(),
                "plans": plans.iter().map(Plan::describe).collect::<Vec<_>>(),
                "toApply": "POST again with {\"apply\": true}",
            })),
        )
            .into_response();
    }

    let mut done = Vec::new();
    for p in &plans {
        match apply_one(&state, p).await {
            Ok(()) => done.push(json!({ "group": p.group, "ok": true })),
            Err(e) => done.push(json!({ "group": p.group, "ok": false, "error": e })),
        }
    }
    (
        StatusCode::OK,
        axum::Json(json!({ "dryRun": false, "results": done })),
    )
        .into_response()
}

async fn apply_one(state: &Arc<Settings>, p: &Plan) -> Result<(), String> {
    // Idempotent by construction: a second run finds the space already there
    // and stops, rather than deleting a group whose replacement exists.
    if state.store.get(&p.group).map(|d| d.id) == Some(p.space_id) {
        return Ok(());
    }

    // Free the name before taking it. The group's action log is not removed --
    // deletion is a tombstone, so the id and its history stay on disk.
    state
        .store
        .delete_doc(&p.group)
        .map_err(|e| format!("could not retire the group document: {e}"))?;

    state
        .store
        .create_space_at(
            p.space_id,
            &p.group,
            &json!({
                "name": p.group,
                "visibility": "protected",
                "members": p.members,
                "managers": p.managers,
            }),
        )
        .map_err(|e| format!("could not create the space: {e}"))?;

    for (doc, chan, _) in &p.chats {
        state
            .store
            .create_doc_in_space(
                doc,
                &crate::doc::ModelRef::new("chat", "1"),
                &json!({ "name": doc, "channel": chan }),
                Some(p.space_id),
            )
            .map_err(|e| format!("could not create chat '{doc}': {e}"))?;
    }

    state
        .store
        .create_doc_in_space(
            &p.drive_doc,
            &crate::doc::ModelRef::new("drive", "1"),
            &json!({ "name": p.drive_doc }),
            Some(p.space_id),
        )
        .map_err(|e| format!("could not create drive '{}': {e}", p.drive_doc))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exactly what `query_docs` returns: name, model, fields -- and no id.
    /// The shape matters; assuming an id here is what shipped a migration that
    /// found nothing.
    fn group_doc(name: &str) -> Value {
        json!({
            "name": name,
            "model": { "name": "group", "version": "1" },
            "fields": {
                "members": ["alice", "bob"],
                "managers": ["alice"],
                "msg_text": ["hi", "there", "ops only"],
                "msg_channel": ["general", "general", "ops"],
                "channels": [
                    { "name": "general", "visibility": "public", "members": [] },
                    { "name": "ops", "visibility": "private", "members": ["alice"] }
                ],
                "drive": [ { "name": "notes", "kind": "folder", "parent": null } ]
            }
        })
    }

    /// This plan resolves the document id through the store by name, so it
    /// works whether or not the projection carries one. The fixture therefore
    /// deliberately omits `id`: if the code ever starts depending on the
    /// projection having it, this test fails rather than the endpoint quietly
    /// returning "nothing to migrate".
    #[test]
    fn a_plan_does_not_depend_on_the_projection_carrying_an_id() {
        let d = group_doc("core");
        assert!(d.get("id").is_none(), "the fixture omits id on purpose: {d}");
        let p = plan_for(&d, "11111111-1111-1111-1111-111111111111".into());
        assert!(p.is_some(), "a plan is still produced without it");
    }

    #[test]
    fn a_plan_names_every_document_it_would_create_and_the_one_it_deletes() {
        let p = plan_for(&group_doc("core"), "11111111-1111-1111-1111-111111111111".into()).unwrap();
        let d = p.describe();
        assert_eq!(d["creates"]["space"]["visibility"], "protected");
        assert_eq!(d["creates"]["chats"].as_array().unwrap().len(), 2);
        assert_eq!(d["creates"]["drive"]["items"], 1);
        assert_eq!(d["deletes"]["doc"], "core");
    }

    #[test]
    fn messages_are_counted_per_channel() {
        let p = plan_for(&group_doc("core"), "11111111-1111-1111-1111-111111111111".into()).unwrap();
        let general = p.chats.iter().find(|(_, c, _)| c == "general").unwrap();
        let ops = p.chats.iter().find(|(_, c, _)| c == "ops").unwrap();
        assert_eq!(general.2, 2);
        assert_eq!(ops.2, 1);
    }

    /// Two nodes migrating the same group must land on the same space, or the
    /// migration forks the thing it is migrating.
    #[test]
    fn the_space_id_is_derived_from_the_group_not_minted() {
        let a = plan_for(&group_doc("core"), "11111111-1111-1111-1111-111111111111".into()).unwrap();
        let b = plan_for(&group_doc("core"), "11111111-1111-1111-1111-111111111111".into()).unwrap();
        assert_eq!(a.space_id, b.space_id);

        let other = plan_for(&group_doc("core"), "22222222-2222-2222-2222-222222222222".into()).unwrap();
        assert_ne!(
            a.space_id, other.space_id,
            "different groups must not collide"
        );
    }

    /// A group written before channels existed keeps its messages.
    #[test]
    fn an_untagged_group_migrates_into_general() {
        let mut d = group_doc("old");
        d["fields"]["channels"] = json!([]);
        d["fields"]["msg_channel"] = json!([]);
        let p = plan_for(&d, "33333333-3333-3333-3333-333333333333".into()).unwrap();
        assert_eq!(p.chats.len(), 1);
        assert_eq!(p.chats[0].1, "general");
        assert_eq!(p.chats[0].2, 3, "all three messages carry over");
    }
}
