//! `group` — the built-in membership model, an [`L1`](super::l1) definition.
//!
//! A group is a membership container that doubles as a shared space. Fields:
//! - `members` — every peer granted read access; the group doc (and its
//!   channel and drive) is replicated to them;
//! - `managers` — the peers who may edit membership and remove drive docs.
//!
//! The space lives in the group doc so it is signed, hash-chained, replicated
//! only to members, and `doc verify`-able like any document:
//! - a **channel** — `msg_from` / `msg_text` / `msg_ts` / `msg_channel`,
//!   parallel arrays appended atomically by `post` (a member's message);
//! - a **drive** — `drive`, doc names a member adds and a manager removes.
//!
//! Reducers:
//! - `init { name, members, managers }` — bootstrap (no precondition).
//! - `add-member` / `remove-member { member }` — a **manager** edits members.
//! - `add-manager { member }` — **quorum-gated**: 2 co-signers who are members
//!   (the two-person rule, checked by the store against the group's `members`).
//! - `post { text, channel }` — a **member** appends a message to the channel.
//! - `add-doc { name }` — a **member** adds a doc to the drive;
//!   `remove-doc { name }` — a **manager** removes one.

use serde_json::json;

use crate::model::l1::L1;

/// The JSON definition of the built-in `group` model.
pub fn group_def() -> serde_json::Value {
    json!({
        "name": "group",
        "version": "1",
        "fields": {
            "members": "string[]",
            "managers": "string[]",
            "msg_from": "string[]",
            "msg_text": "string[]",
            "msg_ts": "number[]",
            "msg_channel": "string[]",
            "drive": "string[]"
        },
        "reducers": {
            "init": {
                "payload": {
                    "name": "string",
                    "members": "string[]",
                    "managers": "string[]"
                },
                "writes": {
                    "__name__": { "set": "$payload.name" },
                    "members": { "set": "$payload.members" },
                    "managers": { "set": "$payload.managers" },
                    "msg_from": { "set": [] },
                    "msg_text": { "set": [] },
                    "msg_ts": { "set": [] },
                    "msg_channel": { "set": [] },
                    "drive": { "set": [] }
                },
                "pre": []
            },
            "add-member": {
                "payload": { "member": "string" },
                "writes": { "members": { "append": "$payload.member" } },
                "pre": [ { "actor-in": "managers" } ]
            },
            "remove-member": {
                "payload": { "member": "string" },
                "writes": { "members": { "remove": "$payload.member" } },
                "pre": [ { "actor-in": "managers" } ]
            },
            "add-manager": {
                "payload": { "member": "string" },
                "writes": {
                    "members": { "append": "$payload.member" },
                    "managers": { "append": "$payload.member" }
                },
                "pre": [ { "quorum": { "group": "$self", "min": 2, "field": "members" } } ]
            },
            "post": {
                "payload": { "text": "string", "channel": "string" },
                "writes": {
                    "msg_from": { "append": "$actor" },
                    "msg_text": { "append": "$payload.text" },
                    "msg_ts": { "append": "$ts" },
                    "msg_channel": { "append": "$payload.channel" }
                },
                "pre": [ { "actor-in": "members" } ]
            },
            "add-doc": {
                "payload": { "name": "string" },
                "writes": { "drive": { "append": "$payload.name" } },
                "pre": [ { "actor-in": "members" } ]
            },
            "remove-doc": {
                "payload": { "name": "string" },
                "writes": { "drive": { "remove": "$payload.name" } },
                "pre": [ { "actor-in": "managers" } ]
            }
        }
    })
}

/// The built-in `group` model (an [`L1`] interpreter over [`group_def`]).
pub fn group() -> L1 {
    L1::from_def(group_def()).expect("the built-in group definition is well-formed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    use crate::action::Action;
    use crate::doc::{Doc, Field, ModelRef, Op};
    use crate::model::{Model, Reject};

    /// The built-in group model, as an interpreter, for reducing sample actions.
    fn model() -> L1 {
        super::group()
    }

    fn action(kind: &str, payload: Value, origin: &str) -> Action {
        Action {
            doc_id: crate::doc::DocId::new(),
            model: ModelRef::new("group", "1"),
            kind: kind.into(),
            payload,
            ts: 1,
            clock: Default::default(),
            origin: origin.into(),
            cosig: Vec::new(),
            prev_hash: None,
            sig: [0; 64],
        }
    }

    fn f(value: Value) -> Field {
        Field {
            value,
            version: Default::default(),
            ts: 0,
            origin: "x".into(),
            deleted: false,
        }
    }

    fn arr(items: &[&str]) -> Value {
        Value::Array(items.iter().map(|s| Value::String((*s).into())).collect())
    }

    /// A group doc with the given members and managers; empty channel and drive.
    fn doc(members: &[&str], managers: &[&str]) -> Doc {
        let mut v = serde_json::Map::new();
        v.insert("members".into(), arr(members));
        v.insert("managers".into(), arr(managers));
        v.insert("msg_from".into(), Value::Array(vec![]));
        v.insert("msg_text".into(), Value::Array(vec![]));
        v.insert("msg_ts".into(), Value::Array(vec![]));
        v.insert("msg_channel".into(), Value::Array(vec![]));
        v.insert("drive".into(), Value::Array(vec![]));
        let fields = v
            .into_iter()
            .map(|(k, val)| {
                (
                    k,
                    Field {
                        value: val,
                        version: Default::default(),
                        ts: 0,
                        origin: "x".into(),
                        deleted: false,
                    },
                )
            })
            .collect();
        Doc {
            id: crate::doc::DocId::new(),
            name: "core".into(),
            fields,
        }
    }

    /// Map the ops a reduce produced, keyed by field, for order-independent asserts.
    fn writes(ops: &[Op]) -> std::collections::BTreeMap<String, Value> {
        ops.iter()
            .map(|o| {
                (
                    o.key.as_deref().unwrap().to_string(),
                    o.value.clone().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn init_writes_membership_and_empty_space() {
        let m = model();
        let ops = m
            .reduce(
                &doc(&[], &[]),
                &action(
                    "init",
                    json!({ "name": "core", "members": ["a"], "managers": ["a"] }),
                    "a",
                ),
            )
            .unwrap();
        let w = writes(&ops);
        assert_eq!(w.get("members"), Some(&json!(["a"])));
        assert_eq!(w.get("managers"), Some(&json!(["a"])));
        assert_eq!(w.get("drive"), Some(&json!([])));
        assert_eq!(w.get("msg_text"), Some(&json!([])));
    }

    #[test]
    fn post_by_member_appends_to_every_channel_field() {
        let m = model();
        let ops = m
            .reduce(
                &doc(&["alice"], &["alice"]),
                &action(
                    "post",
                    json!({ "text": "hi", "channel": "general" }),
                    "alice",
                ),
            )
            .unwrap();
        let w = writes(&ops);
        assert_eq!(w.len(), 4);
        assert_eq!(w.get("msg_from"), Some(&json!(["alice"])));
        assert_eq!(w.get("msg_text"), Some(&json!(["hi"])));
        assert_eq!(w.get("msg_channel"), Some(&json!(["general"])));
        // ts is opaque to the exact numeric type; assert shape, not value.
        assert_eq!(w.get("msg_ts").unwrap().as_array().unwrap().len(), 1);
    }

    #[test]
    fn post_preserves_existing_messages() {
        let m = model();
        let mut d = doc(&["alice"], &["alice"]);
        d.fields.insert("msg_from".into(), f(json!(["bob"])));
        d.fields.insert("msg_text".into(), f(json!(["earlier"])));
        let ops = m
            .reduce(
                &d,
                &action(
                    "post",
                    json!({ "text": "again", "channel": "general" }),
                    "alice",
                ),
            )
            .unwrap();
        let w = writes(&ops);
        assert_eq!(w.get("msg_text"), Some(&json!(["earlier", "again"])));
        assert_eq!(w.get("msg_from"), Some(&json!(["bob", "alice"])));
    }

    #[test]
    fn post_by_nonmember_is_rejected() {
        let m = model();
        let d = doc(&["alice"], &["alice"]);
        assert!(matches!(
            m.check_precondition(
                &d,
                &action("post", json!({ "text": "x", "channel": "general" }), "eve")
            ),
            Err(Reject::Precondition(_))
        ));
    }

    /// A manager who is not also a member cannot post — the contract the
    /// create-group seeding relies on (the creator must be a member, not only a
    /// manager, to use the channel and drive).
    #[test]
    fn manager_not_in_members_cannot_post() {
        let m = model();
        let d = doc(&[], &["alice"]); // alice is a manager, not a member
        assert!(matches!(
            m.check_precondition(
                &d,
                &action(
                    "post",
                    json!({ "text": "x", "channel": "general" }),
                    "alice"
                )
            ),
            Err(Reject::Precondition(_))
        ));
    }

    #[test]
    fn add_doc_by_member_appends_to_drive() {
        let m = model();
        let ops = m
            .reduce(
                &doc(&["alice"], &["alice"]),
                &action("add-doc", json!({ "name": "note-1" }), "alice"),
            )
            .unwrap();
        let w = writes(&ops);
        assert_eq!(w.get("drive"), Some(&json!(["note-1"])));
    }

    #[test]
    fn add_doc_by_nonmember_is_rejected() {
        let m = model();
        let d = doc(&["alice"], &["alice"]);
        assert!(matches!(
            m.check_precondition(&d, &action("add-doc", json!({ "name": "n" }), "eve")),
            Err(Reject::Precondition(_))
        ));
    }

    #[test]
    fn remove_doc_by_manager_removes_from_drive() {
        let m = model();
        let mut d = doc(&["alice"], &["alice"]);
        d.fields.insert("drive".into(), f(json!(["d1", "d2"])));
        let ops = m
            .reduce(&d, &action("remove-doc", json!({ "name": "d1" }), "alice"))
            .unwrap();
        let w = writes(&ops);
        assert_eq!(w.get("drive"), Some(&json!(["d2"])));
    }

    #[test]
    fn remove_doc_by_nonmanager_is_rejected() {
        let m = model();
        let mut d = doc(&["alice", "bob"], &["alice"]); // bob: member, not manager
        d.fields.insert("drive".into(), f(json!(["d1"])));
        assert!(matches!(
            m.check_precondition(&d, &action("remove-doc", json!({ "name": "d1" }), "bob")),
            Err(Reject::Precondition(_))
        ));
    }

    #[test]
    fn add_member_by_manager_appends_to_members() {
        let m = model();
        let ops = m
            .reduce(
                &doc(&["alice"], &["alice"]),
                &action("add-member", json!({ "member": "bob" }), "alice"),
            )
            .unwrap();
        let w = writes(&ops);
        assert_eq!(w.get("members"), Some(&json!(["alice", "bob"])));
    }

    #[test]
    fn add_member_by_nonmanager_is_rejected() {
        let m = model();
        let d = doc(&["alice", "bob"], &["alice"]); // bob: member, not manager
        assert!(matches!(
            m.check_precondition(
                &d,
                &action("add-member", json!({ "member": "carol" }), "bob")
            ),
            Err(Reject::Precondition(_))
        ));
    }
}
