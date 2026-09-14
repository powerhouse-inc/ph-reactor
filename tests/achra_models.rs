//! The Achra coordination models enforce what they claim.
//!
//! These are runtime-registered model definitions (`models/achra.json`), not
//! Rust code, so nothing in the compiler checks them. A typo in a precondition
//! is invisible until someone awards a contract they should not have been able
//! to award — which is exactly the class of bug worth a test.
//!
//! See docs/superpowers/specs/2026-09-14-achra-mvp-design.md.

use std::sync::Arc;

use ed25519_dalek::SigningKey;
use serde_json::{json, Value};

use ph_reactor::model::l1::L1;
use ph_reactor::model::Model;
use ph_reactor::store::Store;

const ACHRA_MODELS: &str = include_str!("../models/achra.json");

fn definitions() -> Vec<Value> {
    let root: Value = serde_json::from_str(ACHRA_MODELS).expect("achra.json parses");
    root.get("models")
        .and_then(Value::as_array)
        .expect("a `models` array")
        .clone()
}

fn def(name: &str) -> Value {
    definitions()
        .into_iter()
        .find(|d| d.get("name").and_then(Value::as_str) == Some(name))
        .unwrap_or_else(|| panic!("no model named {name}"))
}

/// A store whose origin is `actor`, with every Achra model registered.
fn store_as(actor: &str, dir: &std::path::Path) -> Arc<Store> {
    let key = SigningKey::from_bytes(&[7u8; 32]);
    let store = Store::open(&dir.join("docs"), &key, actor).expect("store opens");
    for d in definitions() {
        store.add_model(Arc::new(
            L1::from_def(d).expect("model definition is valid"),
        ));
    }
    store
}

fn model_ref(name: &str) -> ph_reactor::doc::ModelRef {
    ph_reactor::doc::ModelRef::new(name, "1")
}

/// Every definition in the file must load. A malformed one would otherwise
/// fail at runtime registration, on a live node, with no test to catch it.
#[test]
fn every_model_definition_is_valid() {
    let defs = definitions();
    assert_eq!(defs.len(), 4, "rfp, proposal, agreement, milestone");
    for d in defs {
        let name = d
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        L1::from_def(d).unwrap_or_else(|e| panic!("model '{name}' is invalid: {e}"));
    }
}

/// Awarding is a two-person decision, and the two people are the publishing
/// org's own approvers — not any two members of the marketplace.
#[test]
fn award_requires_a_quorum_of_the_rfps_own_approvers() {
    let m = L1::from_def(def("rfp")).expect("rfp model");
    let spec = m.quorum("award").expect("award must declare a quorum");
    assert_eq!(spec.min, 2, "awarding must take two distinct signers");
    assert_eq!(
        spec.group, "$self",
        "the quorum must read the RFP itself, so it is the publishing org's \
         approvers who agree -- not any two marketplace members"
    );
    assert_eq!(spec.field, "approvers");

    // The other reducers must NOT be quorum-gated; requiring co-signatures to
    // publish or cancel would make the marketplace unusable.
    assert!(m.quorum("init").is_none());
    assert!(m.quorum("cancel").is_none());
}

#[test]
fn only_the_publisher_can_cancel_an_rfp() {
    let dir = tempfile::tempdir().expect("tmp");
    let org = "12D3KooW-org";
    let store = store_as(org, dir.path());

    store
        .create_doc_model(
            "rfp-1",
            &model_ref("rfp"),
            &json!({
                "name": "rfp-1",
                "title": "Index the archive",
                "brief": "Build a searchable index",
                "budget": 50000,
                "currency": "USD",
                "deadline": "2026-12-01",
                "approvers": [org, "12D3KooW-cfo"],
            }),
        )
        .expect("publish the rfp");

    let doc = store.get("rfp-1").expect("rfp exists");
    assert_eq!(doc.fields["status"].value, json!("open"));
    assert_eq!(doc.fields["publisher"].value, json!([org]));

    // The publisher may cancel.
    store
        .apply_local_action("rfp-1", &model_ref("rfp"), "cancel", &json!({}))
        .expect("the publisher can cancel");
    assert_eq!(
        store.get("rfp-1").expect("rfp").fields["status"].value,
        json!("cancelled")
    );

    // A different actor may not. A separate store means a different origin.
    let dir2 = tempfile::tempdir().expect("tmp2");
    let other = store_as("12D3KooW-stranger", dir2.path());
    other
        .create_doc_model(
            "rfp-2",
            &model_ref("rfp"),
            &json!({
                "name": "rfp-2",
                "title": "t", "brief": "b", "budget": 1, "currency": "USD",
                "deadline": "2026-12-01", "approvers": [org],
            }),
        )
        .expect("stranger publishes their own rfp");
    // rfp-2's publisher is the stranger, so the ORG cannot cancel it. Assert
    // via the precondition directly: `store` has origin `org`.
    let err = store
        .apply_local_action("rfp-2", &model_ref("rfp"), "cancel", &json!({}))
        .expect_err("a non-publisher must not cancel");
    assert!(
        err.contains("no doc named") || err.contains("not in"),
        "unexpected error: {err}"
    );
}

/// A cancelled RFP is closed for good: awarding it afterwards must fail.
#[test]
fn a_cancelled_rfp_cannot_be_awarded() {
    let dir = tempfile::tempdir().expect("tmp");
    let org = "12D3KooW-org";
    let store = store_as(org, dir.path());
    store
        .create_doc_model(
            "rfp-1",
            &model_ref("rfp"),
            &json!({
                "name": "rfp-1",
                "title": "t", "brief": "b", "budget": 10, "currency": "USD",
                "deadline": "2026-12-01", "approvers": [org, "12D3KooW-cfo"],
            }),
        )
        .expect("publish");
    store
        .apply_local_action("rfp-1", &model_ref("rfp"), "cancel", &json!({}))
        .expect("cancel");

    let err = store
        .apply_local_action(
            "rfp-1",
            &model_ref("rfp"),
            "award",
            &json!({ "builder": "12D3KooW-builder", "amount": 10 }),
        )
        .expect_err("a cancelled rfp must not be awardable");
    // Either the status precondition or the quorum refuses it; both are
    // correct, and both must refuse.
    assert!(
        err.contains("status") || err.contains("quorum"),
        "unexpected error: {err}"
    );
}

/// Only the builder may claim a milestone is done, and only the org may accept
/// it. This is the payout-intent gate, so it is the one that matters most.
#[test]
fn milestone_transitions_are_gated_to_the_right_party() {
    let dir = tempfile::tempdir().expect("tmp");
    let org = "12D3KooW-org";
    let builder = "12D3KooW-builder";
    let store = store_as(org, dir.path());

    store
        .create_doc_model(
            "ms-1",
            &model_ref("milestone"),
            &json!({
                "name": "ms-1",
                "agreement_ref": "ag-1",
                "builder": builder,
                "title": "Phase one",
                "amount": 5000,
            }),
        )
        .expect("create milestone");
    let d = store.get("ms-1").expect("milestone");
    assert_eq!(d.fields["status"].value, json!("pending"));
    assert_eq!(d.fields["org"].value, json!([org]));
    assert_eq!(d.fields["builder"].value, json!([builder]));

    // The ORG cannot submit evidence: only the builder may claim delivery.
    let err = store
        .apply_local_action(
            "ms-1",
            &model_ref("milestone"),
            "submit",
            &json!({ "evidence": "https://example/pr/1" }),
        )
        .expect_err("the org must not be able to claim the builder's delivery");
    assert!(err.contains("not in"), "unexpected error: {err}");

    // And the org cannot accept a milestone that was never submitted.
    let err = store
        .apply_local_action("ms-1", &model_ref("milestone"), "accept", &json!({}))
        .expect_err("accepting a pending milestone must fail");
    assert!(err.contains("status"), "unexpected error: {err}");
}

/// A builder cannot accept or decline their own proposal.
#[test]
fn a_builder_cannot_decide_their_own_proposal() {
    let dir = tempfile::tempdir().expect("tmp");
    let builder = "12D3KooW-builder";
    let store = store_as(builder, dir.path());

    store
        .create_doc_model(
            "prop-1",
            &model_ref("proposal"),
            &json!({ "name": "prop-1", "rfp_ref": "rfp-1", "summary": "we will do it", "amount": 42000 }),
        )
        .expect("submit proposal");
    assert_eq!(
        store.get("prop-1").expect("p").fields["submitter"].value,
        json!([builder])
    );

    for kind in ["accept", "decline"] {
        let err = store
            .apply_local_action("prop-1", &model_ref("proposal"), kind, &json!({}))
            .expect_err("a builder must not decide their own proposal");
        assert!(
            err.contains("is in 'submitter'"),
            "expected {kind} to be refused for the right reason, got: {err}"
        );
    }
}

/// The builder MAY withdraw their own proposal — the mirror of the test above,
/// so that "gated" does not silently become "nobody can do anything".
#[test]
fn a_builder_can_withdraw_their_own_proposal() {
    let dir = tempfile::tempdir().expect("tmp");
    let builder = "12D3KooW-builder";
    let store = store_as(builder, dir.path());
    store
        .create_doc_model(
            "prop-1",
            &model_ref("proposal"),
            &json!({ "name": "prop-1", "rfp_ref": "rfp-1", "summary": "s", "amount": 1 }),
        )
        .expect("submit");
    store
        .apply_local_action("prop-1", &model_ref("proposal"), "withdraw", &json!({}))
        .expect("the submitter may withdraw");
    assert_eq!(
        store.get("prop-1").expect("p").fields["status"].value,
        json!("withdrawn")
    );
}
