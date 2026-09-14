//! A runtime-registered model must survive a restart.
//!
//! Models can be built into the binary *or* registered at runtime; both are
//! supported and runtime registration is a requirement. But the store rebuilds
//! every document by reducing its action log through the model that wrote it,
//! so a model that exists only in memory takes its documents down with it: on
//! the next start they replay with no name and no fields.
//!
//! This was found in production — rolling the cluster reactor emptied three
//! Achra documents while the laptop, which had not restarted, kept them intact.
//! The `group` document survived on both because `group` is built in.

use std::sync::Arc;

use ed25519_dalek::SigningKey;
use serde_json::{json, Value};

use ph_reactor::doc::ModelRef;
use ph_reactor::model::{l1::L1, persist, Model};
use ph_reactor::store::Store;

fn key() -> SigningKey {
    SigningKey::from_bytes(&[3u8; 32])
}

/// A minimal runtime model: one that the binary knows nothing about.
fn ticket_def() -> Value {
    json!({
        "name": "ticket",
        "version": "1",
        "fields": { "title": "string", "status": "string" },
        "reducers": {
            "init": {
                "payload": { "name": "string", "title": "string" },
                "writes": {
                    "__name__": { "set": "$payload.name" },
                    "title": { "set": "$payload.title" },
                    "status": { "set": "open" }
                }
            },
            "close": {
                "payload": {},
                "writes": { "status": { "set": "closed" } }
            }
        }
    })
}

fn seed_from(defs: Vec<Value>) -> Vec<Arc<dyn Model>> {
    defs.into_iter()
        .map(|d| Arc::new(L1::from_def(d).expect("valid definition")) as Arc<dyn Model>)
        .collect()
}

/// The bug, pinned: reopening WITHOUT the model replays the document empty.
///
/// Kept as a test so the failure mode stays visible and understood — if this
/// ever starts passing with no fields lost, the replay semantics changed and
/// the fix below may no longer be load-bearing.
#[test]
fn without_its_model_a_document_replays_empty() {
    let dir = tempfile::tempdir().expect("tmp");
    let docs = dir.path().join("docs");

    {
        let store =
            Store::open_with_models(&docs, &key(), "origin-a", seed_from(vec![ticket_def()]))
                .expect("open");
        store
            .create_doc_model(
                "t-1",
                &ModelRef::new("ticket", "1"),
                &json!({ "name": "t-1", "title": "Fix the thing" }),
            )
            .expect("create");
        assert_eq!(
            store.get("t-1").expect("present").fields["title"].value,
            json!("Fix the thing")
        );
    }

    // Reopen with built-ins only, exactly as a restart did before the fix.
    let store = Store::open(&docs, &key(), "origin-a").expect("reopen");
    assert!(
        store.get("t-1").is_none(),
        "without its model the document cannot be reduced, so it is unnamed"
    );
}

/// The fix: registering the model before the replay restores the document.
#[test]
fn with_its_model_a_document_survives_a_restart() {
    let dir = tempfile::tempdir().expect("tmp");
    let docs = dir.path().join("docs");

    {
        let store =
            Store::open_with_models(&docs, &key(), "origin-a", seed_from(vec![ticket_def()]))
                .expect("open");
        store
            .create_doc_model(
                "t-1",
                &ModelRef::new("ticket", "1"),
                &json!({ "name": "t-1", "title": "Fix the thing" }),
            )
            .expect("create");
        store
            .apply_local_action("t-1", &ModelRef::new("ticket", "1"), "close", &json!({}))
            .expect("close it");
    }

    let store = Store::open_with_models(&docs, &key(), "origin-a", seed_from(vec![ticket_def()]))
        .expect("reopen");
    let doc = store.get("t-1").expect("the document must survive");
    assert_eq!(doc.fields["title"].value, json!("Fix the thing"));
    assert_eq!(
        doc.fields["status"].value,
        json!("closed"),
        "the full action history must replay, not just creation"
    );
}

/// End to end through the file the daemon actually uses: persist a definition,
/// then reopen seeding from that file alone.
#[test]
fn a_persisted_definition_restores_its_documents() {
    let dir = tempfile::tempdir().expect("tmp");
    let docs = dir.path().join("docs");
    let models_file = dir.path().join("models.json");

    persist::save(&models_file, &ticket_def()).expect("persist the definition");

    {
        let store = Store::open_with_models(
            &docs,
            &key(),
            "origin-a",
            seed_from(persist::load_all(&models_file)),
        )
        .expect("open");
        store
            .create_doc_model(
                "t-1",
                &ModelRef::new("ticket", "1"),
                &json!({ "name": "t-1", "title": "Persisted" }),
            )
            .expect("create");
    }

    // A fresh process would do exactly this: read the file, seed, then replay.
    let restored = persist::load_all(&models_file);
    assert_eq!(restored.len(), 1, "the definition must be on disk");
    let store =
        Store::open_with_models(&docs, &key(), "origin-a", seed_from(restored)).expect("reopen");
    assert_eq!(
        store.get("t-1").expect("survives").fields["title"].value,
        json!("Persisted")
    );
}

/// Built-in models were never affected — `Store::open` seeds them itself. That
/// asymmetry is why the gap went unnoticed until a domain model was deployed.
#[test]
fn built_in_models_survive_without_any_persistence() {
    let dir = tempfile::tempdir().expect("tmp");
    let docs = dir.path().join("docs");

    {
        let store = Store::open(&docs, &key(), "origin-a").expect("open");
        store
            .create_doc_model(
                "g-1",
                &ModelRef::new("group", "1"),
                &json!({ "name": "g-1", "members": ["origin-a"], "managers": ["origin-a"] }),
            )
            .expect("create a group");
    }

    let store = Store::open(&docs, &key(), "origin-a").expect("reopen");
    let doc = store
        .get("g-1")
        .expect("a built-in model's document survives");
    assert_eq!(doc.fields["members"].value, json!(["origin-a"]));
}
