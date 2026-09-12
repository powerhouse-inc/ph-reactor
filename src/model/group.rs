//! `group` — the built-in membership model, an [`L1`](super::l1) definition.
//!
//! A group is a document with two string-array fields:
//! - `members` — every peer granted read access to the group's topics;
//! - `managers` — the peers who may edit membership.
//!
//! Membership is a signed, hash-chained, replicated log — time-versioned
//! for free and `doc verify`-able like any document:
//!
//! - `init { name, members, managers }` — bootstrap the group (no
//!   precondition; sets the name, members, and managers).
//! - `add-member { member }` / `remove-member { member }` — a **manager**
//!   edits the member list.
//! - `add-manager { member }` — **quorum-gated**: needs `min` (2) distinct,
//!   valid co-signers who are existing members — the two-person rule.
//!
//! The quorum is checked by the store against the group's own `members`
//! field (`$self`), so a new manager requires two current members to
//! co-sign.

use serde_json::json;

use crate::model::l1::L1;

/// The JSON definition of the built-in `group` model.
pub fn group_def() -> serde_json::Value {
    json!({
        "name": "group",
        "version": "1",
        "fields": {
            "members": "string[]",
            "managers": "string[]"
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
                    "managers": { "set": "$payload.managers" }
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
            }
        }
    })
}

/// The built-in `group` model (an [`L1`] interpreter over [`group_def`]).
pub fn group() -> L1 {
    L1::from_def(group_def()).expect("the built-in group definition is well-formed")
}
