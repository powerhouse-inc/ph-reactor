//! `package` — the built-in model that carries a plugin package across the mesh.
//!
//! Packages are documents, not a new message type. That is the whole point: a
//! document already replicates, hash-chains, scopes to a group and shows its
//! history, so publishing a package needs no distribution logic of its own.
//! Join a team's drive and its apps arrive with everything else.
//!
//! # Why the manifest is a string
//!
//! The L1 field types are `string`, `number` and their array forms — there is
//! no object type, and adding one to carry a single field would be a large
//! change for a small reason. So the manifest travels as its serialized JSON.
//!
//! That is not merely a workaround, it is the safer shape: the signature covers
//! the manifest's own canonical bytes, and a string is stored and replicated
//! verbatim rather than being re-encoded field by field on the way through. The
//! `publisher_key`, `package_name` and `package_version` fields are duplicated
//! out of it only so the console can list packages without parsing every one;
//! the manifest inside is what any decision is made against.
//!
//! # Trust
//!
//! Anyone may publish: `init` has no precondition, exactly as a public
//! marketplace requires. Publishing is not the trust decision — installing is,
//! and that decision is made against [`crate::package::trust::TrustStore`] by
//! the operator. A package document arriving is an offer, never an install.
//!
//! `withdraw` lets a publisher retract a version they should not have shipped.
//! It is gated on `publishers`, a single-entry `string[]` because the
//! precondition DSL's `actor-is` compares against a literal, so membership of a
//! list field is the only way to say "the actor must be this document's
//! publisher" (the same convention the Achra models use).

use serde_json::json;

use crate::model::l1::L1;

/// The JSON definition of the built-in `package` model.
pub fn package_def() -> serde_json::Value {
    json!({
        "name": "package",
        "version": "1",
        "$comment": "A plugin package offered on the mesh. Publishing is open; \
                     installing is the operator's decision.",
        "fields": {
            "publishers": "string[]",
            "publisher_key": "string",
            "publisher_name": "string",
            "package_name": "string",
            "package_version": "string",
            "description": "string",
            "manifest": "string",
            "status": "string"
        },
        "reducers": {
            "init": {
                "payload": {
                    "name": "string",
                    "publisher_key": "string",
                    "package_name": "string",
                    "package_version": "string",
                    "manifest": "string"
                },
                "writes": {
                    "__name__": { "set": "$payload.name" },
                    "publishers": { "append": "$actor" },
                    "publisher_key": { "set": "$payload.publisher_key" },
                    "publisher_name": { "set": "$payload.publisher_name" },
                    "package_name": { "set": "$payload.package_name" },
                    "package_version": { "set": "$payload.package_version" },
                    "description": { "set": "$payload.description" },
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
                    "description": { "set": "$payload.reason" }
                }
            }
        }
    })
}

/// The built-in `package@1` model.
pub fn package() -> L1 {
    L1::from_def(package_def()).expect("the built-in package definition is valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Model;

    /// A definition that does not load is a daemon that does not start, so the
    /// check belongs in the test suite rather than in `expect` at runtime.
    #[test]
    fn the_definition_loads() {
        let m = package();
        assert_eq!(m.ref_().name, "package");
        assert_eq!(m.ref_().version, "1");
    }

    /// Publishing must be open. A marketplace where only pre-approved keys can
    /// offer a package is a different product.
    #[test]
    fn init_has_no_precondition() {
        let def = package_def();
        assert!(
            def["reducers"]["init"].get("pre").is_none(),
            "anyone may publish; the trust decision happens at install"
        );
    }

    /// Without a `__name__` write the document is created and then unfindable
    /// by name — a mistake this codebase has already made once.
    #[test]
    fn init_names_the_document() {
        let def = package_def();
        assert_eq!(def["reducers"]["init"]["writes"]["__name__"]["set"], "$payload.name");
    }

    /// Withdrawal is the publisher's retraction, so only the publisher may do
    /// it — and only once, or a withdrawn package could be silently re-listed.
    #[test]
    fn withdraw_is_restricted_to_the_publisher() {
        let def = package_def();
        let pre = def["reducers"]["withdraw"]["pre"]
            .as_array()
            .expect("withdraw has preconditions")
            .clone();
        assert!(pre.iter().any(|p| p["actor-in"] == "publishers"));
        assert!(pre
            .iter()
            .any(|p| p["field-is"]["field"] == "status" && p["field-is"]["value"] == "published"));
    }
}
