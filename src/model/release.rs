//! `release` — the built-in model that carries a daemon build across the mesh.
//!
//! Exactly the shape [`super::package`] takes, and for the same reason: a
//! release is a signed artifact that should ride the sync, replication and
//! history that documents already have, rather than reaching nodes over a
//! second channel with a second trust model to get right.
//!
//! Built in rather than shipped as a definition because of the same
//! circularity: a node has to be able to learn about a newer daemon using the
//! daemon it is running now.
//!
//! Anyone may publish. Publishing is not the trust decision — applying is, and
//! that decision is made against the operator's trust store. A release document
//! arriving is an offer, and `update.auto` is what decides whether an offer
//! from an already-trusted key is acted on without asking again.

use serde_json::json;

use crate::model::l1::L1;

/// The JSON definition of the built-in `release` model.
pub fn release_def() -> serde_json::Value {
    json!({
        "name": "release",
        "version": "1",
        "$comment": "A published daemon build. `manifest` holds the signed \
                     release JSON; the fields beside it are duplicated out so a \
                     node can filter without parsing every one.",
        "fields": {
            "publishers": "string[]",
            "publisher_key": "string",
            "release_version": "string",
            "platform": "string",
            "notes": "string",
            "manifest": "string",
            "status": "string"
        },
        "reducers": {
            "init": {
                "payload": {
                    "name": "string",
                    "publisher_key": "string",
                    "release_version": "string",
                    "platform": "string",
                    "manifest": "string"
                },
                "writes": {
                    "__name__": { "set": "$payload.name" },
                    "publishers": { "append": "$actor" },
                    "publisher_key": { "set": "$payload.publisher_key" },
                    "release_version": { "set": "$payload.release_version" },
                    "platform": { "set": "$payload.platform" },
                    "notes": { "set": "$payload.notes" },
                    "manifest": { "set": "$payload.manifest" },
                    "status": { "set": "published" }
                }
            },
            "withdraw": {
                "payload": { "reason": "string" },
                "pre": [
                    { "actor-in": "publishers" },
                    { "field-is": { "field": "status", "value": "published" } }
                ],
                "writes": {
                    "status": { "set": "withdrawn" },
                    "notes": { "set": "$payload.reason" }
                }
            }
        }
    })
}

/// The built-in `release@1` model.
pub fn release() -> L1 {
    L1::from_def(release_def()).expect("the built-in release definition is valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Model;

    #[test]
    fn the_definition_loads() {
        let m = release();
        assert_eq!(m.ref_().name, "release");
        assert_eq!(m.ref_().version, "1");
    }

    #[test]
    fn init_names_the_document() {
        let def = release_def();
        assert_eq!(def["reducers"]["init"]["writes"]["__name__"]["set"], "$payload.name");
    }

    /// Withdrawal is how a publisher retracts a build that should not be run.
    /// It matters more here than for packages: a bad plugin is sandboxed, a bad
    /// binary is not.
    #[test]
    fn only_the_publisher_may_withdraw_a_release() {
        let def = release_def();
        let pre = def["reducers"]["withdraw"]["pre"]
            .as_array()
            .expect("withdraw has preconditions")
            .clone();
        assert!(pre.iter().any(|p| p["actor-in"] == "publishers"));
    }

    /// The platform is a field, not an afterthought: a node must be able to
    /// ignore a release for another target without parsing the manifest.
    #[test]
    fn a_release_states_its_platform() {
        let def = release_def();
        assert_eq!(def["fields"]["platform"], "string");
        assert_eq!(
            def["reducers"]["init"]["writes"]["platform"]["set"],
            "$payload.platform"
        );
    }
}
