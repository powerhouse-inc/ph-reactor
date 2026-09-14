//! `space` — the built-in model that says who may read and who may manage.
//!
//! A space is the **unit of access**. It carries nothing else: no channels, no
//! drive, no features. Everything a space *contains* is an app, enabled here
//! and storing its own documents stamped with this space's id.
//!
//! That split is the point. `group@1` hardcoded its channels and its drive, so
//! those features were privileged — no plugin could add one like them, and the
//! model had to grow a field for every feature anyone wanted. Meanwhile every
//! plugin reinvented membership: Achra's `rfp` carries `publisher` and
//! `approvers`, its `agreement` carries `org` and `builder`, each a bespoke
//! access list the store never enforced. One membership list, enforced once,
//! inherited by every app, is what makes a suite out of a pile of plugins.
//!
//! Built in rather than shipped as a definition, for the same circularity as
//! [`super::package`]: a space is how access is decided, so it cannot arrive
//! inside something whose access has to be decided.
//!
//! Three tiers:
//!
//! - `public` — anyone on the mesh may read. Gossiped.
//! - `protected` — only `members` may read. Not gossiped (see below); served
//!   only to member peers.
//! - `private` — only this node. Never gossiped, never served.
//!
//! **Visibility is fixed at `init`.** There is no honest `set-visibility`.
//! Protected → public is a retroactive bulk disclosure of everything ever
//! written in the space; public → protected is a lie, because the data is
//! already on every node in the mesh. Changing a space's tier means making a
//! new space and deciding what to copy into it — which is the decision the
//! operator should be making anyway.
//!
//! **What protected actually guarantees.** Non-members cannot fetch it. That
//! is all. Every member holds a full plaintext replica, so a member who
//! defects can re-serve it, and someone removed keeps everything they already
//! had. Revocation is forward-only and propagates at sync speed. The console
//! says this in those words; so does the design doc. It is the likeliest
//! source of real-world harm in this feature and it is not a code problem.

use serde_json::json;

use crate::model::l1::L1;

/// The commons: the one public space every node is a member of.
///
/// A fixed id compiled into the daemon, not generated per node — every node
/// must name the same document or there is no shared commons to publish into.
/// This is also why there is no such thing as a "global app": Achra is a space
/// app, enabled in the commons.
pub const COMMONS_ID: &str = "00000000-0000-0000-0000-000000000c00";

/// The three tiers, as they appear in the `visibility` field.
pub const PUBLIC: &str = "public";
pub const PROTECTED: &str = "protected";
pub const PRIVATE: &str = "private";

/// Whether documents in a space with this visibility may be gossiped.
///
/// Only public ones. Gossipsub subscription is unauthenticated — any peer may
/// subscribe to any topic — so a per-space topic would enforce nothing.
/// Anything narrower than public goes to member peers over the direct,
/// authenticated sync protocol instead, which also keeps every access decision
/// on one path rather than split across two transports.
pub fn may_gossip(visibility: &str) -> bool {
    visibility == PUBLIC
}

/// Whether a space with this visibility replicates to other nodes at all.
pub fn replicates(visibility: &str) -> bool {
    visibility != PRIVATE
}

/// The JSON definition of the built-in `space` model.
pub fn space_def() -> serde_json::Value {
    json!({
        "name": "space",
        "version": "1",
        "$comment": "The unit of access. `visibility` is fixed at init -- there \
                     is no reducer that changes it, deliberately. `apps` holds \
                     the packages enabled here; installing a package is a \
                     node-level trust decision, enabling it here is a separate \
                     one about data.",
        "fields": {
            "visibility": "string",
            "members": "string[]",
            "managers": "string[]",
            "apps": "array"
        },
        "reducers": {
            "init": {
                "payload": {
                    "name": "string",
                    "visibility": "string",
                    "members": "string[]",
                    "managers": "string[]"
                },
                "writes": {
                    "__name__": { "set": "$payload.name" },
                    "visibility": { "set": "$payload.visibility" },
                    "members": { "set": "$payload.members" },
                    "managers": { "set": "$payload.managers" },
                    "apps": { "set": [] }
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
            "enable-app": {
                "payload": { "item": "object" },
                "writes": { "apps": { "append": "$payload.item" } },
                "pre": [ { "actor-in": "managers" } ]
            },
            "disable-app": {
                "payload": { "apps": "array" },
                "writes": { "apps": { "set": "$payload.apps" } },
                "pre": [ { "actor-in": "managers" } ]
            }
        }
    })
}

pub fn space() -> L1 {
    L1::from_def(space_def()).expect("the built-in space definition is well-formed")
}

/// Read a space document's visibility, defaulting to the safest tier.
///
/// A document that does not say is treated as `private`: an unreadable space
/// is a nuisance, a space that turns out to have been public is a disclosure.
pub fn visibility_of(doc: &crate::doc::Doc) -> String {
    doc.fields
        .get("visibility")
        .and_then(|f| f.value.as_str())
        .unwrap_or(PRIVATE)
        .to_string()
}

/// The members named by a space document.
pub fn members_of(doc: &crate::doc::Doc) -> Vec<String> {
    doc.fields
        .get("members")
        .and_then(|f| f.value.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Model;

    #[test]
    fn the_definition_loads() {
        let m = space();
        assert_eq!(m.ref_().name, "space");
        assert_eq!(m.ref_().version, "1");
    }

    #[test]
    fn there_is_no_reducer_that_changes_visibility() {
        let def = space_def();
        let reducers = def["reducers"].as_object().unwrap();
        for (name, r) in reducers {
            if name == "init" {
                continue;
            }
            let writes = r["writes"].as_object().unwrap();
            assert!(
                !writes.contains_key("visibility"),
                "'{name}' writes visibility: a space's tier is fixed at init"
            );
        }
    }

    #[test]
    fn only_a_public_space_is_gossiped() {
        assert!(may_gossip(PUBLIC));
        assert!(!may_gossip(PROTECTED), "gossip topics are unauthenticated");
        assert!(!may_gossip(PRIVATE));
    }

    #[test]
    fn a_private_space_does_not_leave_the_node() {
        assert!(replicates(PUBLIC));
        assert!(replicates(PROTECTED));
        assert!(!replicates(PRIVATE));
    }

    #[test]
    fn a_space_that_does_not_say_is_treated_as_private() {
        let doc = crate::doc::Doc {
            id: crate::doc::DocId::new(),
            name: "mystery".into(),
            fields: Default::default(),
        };
        assert_eq!(
            visibility_of(&doc),
            PRIVATE,
            "an unreadable space is a nuisance; an accidentally public one is a disclosure"
        );
    }
}
