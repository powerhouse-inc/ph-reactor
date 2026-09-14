//! A package's whole journey, end to end: signed, carried as a document,
//! refused until the operator trusts the publisher, installed, and its editor
//! served from the isolated asset origin.
//!
//! Each step is checked for the thing it is actually responsible for. The point
//! of the test is the ORDER — integrity before trust, trust before bundle,
//! bundle before model registration — because every one of those checks is only
//! as good as the fact that it runs before the next one.

use ed25519_dalek::SigningKey;
use serde_json::json;

use ph_reactor::blob::BlobStore;
use ph_reactor::doc::ModelRef;
use ph_reactor::package::install::{self, InstallError, Installed};
use ph_reactor::package::trust::TrustStore;
use ph_reactor::package::{Capabilities, Manifest, PublisherInfo, WriteCapability};
use ph_reactor::store::Store;

fn publisher() -> SigningKey {
    SigningKey::from_bytes(&[11u8; 32])
}

fn node() -> SigningKey {
    SigningKey::from_bytes(&[12u8; 32])
}

fn rfp_def() -> serde_json::Value {
    json!({
        "name": "rfp", "version": "1",
        "fields": { "title": "string", "status": "string" },
        "reducers": { "init": {
            "payload": { "name": "string", "title": "string" },
            "writes": {
                "__name__": { "set": "$payload.name" },
                "title": { "set": "$payload.title" },
                "status": { "set": "open" }
            }
        } }
    })
}

const EDITOR: &str = "<!doctype html><title>Achra</title><h1>Marketplace</h1>";

fn achra_manifest(blobs: &BlobStore) -> Manifest {
    let bundle = blobs.put(EDITOR.as_bytes()).expect("store the editor");
    let k = publisher();
    let mut m = Manifest {
        name: "@powerhousedao/achra".into(),
        version: "1.0.0".into(),
        description: "Marketplace for global coordination".into(),
        category: "Coordination".into(),
        publisher: PublisherInfo {
            name: "Powerhouse".into(),
            url: "https://powerhouse.inc/".into(),
        },
        publisher_key: hex::encode(k.verifying_key().to_bytes()),
        document_models: vec![rfp_def()],
        processors: vec![],
        bundle: Some(bundle),
        capabilities: Capabilities {
            read: vec!["rfp@1".into()],
            write: vec![WriteCapability {
                model: "proposal@1".into(),
                kinds: vec!["init".into()],
            }],
        },
        ui: Default::default(),
        sig: String::new(),
        projections: Vec::new(),
        attention: Vec::new(),
    };
    m.sign(&k);
    m
}

/// The full path, in order, with each stage asserted for its own reason.
#[test]
fn a_package_travels_as_a_document_and_installs_only_once_trusted() {
    let dir = tempfile::tempdir().expect("tmp");
    let blobs = BlobStore::open(&dir.path().join("blobs")).expect("blobs");
    let store = Store::open(&dir.path().join("docs"), &node(), "node-a").expect("store");
    let manifest = achra_manifest(&blobs);

    // -- carried as a document -------------------------------------------
    //
    // `package@1` is built in, so a node can receive a package before it has
    // installed anything at all -- the circularity that made it built-in.
    let serialized = serde_json::to_string(&manifest).expect("serialize");
    store
        .create_doc_model(
            "pkg-achra-1.0.0",
            &ModelRef::parse("package@1").expect("ref"),
            &json!({
                "name": "pkg-achra-1.0.0",
                "publisher_key": manifest.publisher_key,
                "publisher_name": "Powerhouse",
                "package_name": manifest.name,
                "package_version": manifest.version,
                "description": manifest.description,
                "manifest": serialized,
            }),
        )
        .expect("publish the package document");

    let docs = ph_reactor::query::query_docs(&store, "package", None);
    assert_eq!(docs.len(), 1, "the package is on the mesh as a document");
    let carried: Manifest = serde_json::from_str(
        docs[0]["fields"]["manifest"]
            .as_str()
            .expect("the manifest field is a string"),
    )
    .expect("the manifest survives the document round trip");
    assert!(
        carried.verify().is_ok(),
        "a manifest must still verify after being stored and read back"
    );

    // -- refused: publisher not trusted ----------------------------------
    let trust_file = dir.path().join("publishers.json");
    let installed_file = dir.path().join("packages.json");
    let models_file = dir.path().join("models.json");
    let mut trust = TrustStore::load(&trust_file);

    match install::install(
        &carried,
        &trust,
        &blobs,
        &store,
        &installed_file,
        &models_file,
    ) {
        Err(InstallError::UntrustedPublisher { key, .. }) => {
            assert_eq!(key, manifest.publisher_key);
        }
        other => panic!("an unknown publisher must be refused, got {other:?}"),
    }
    assert!(
        store.model_refs().iter().all(|r| r.name != "rfp"),
        "a refused package must register NOTHING -- not even its models"
    );

    // -- the operator trusts the key -------------------------------------
    trust.trust(&carried.publisher_key, &carried.publisher.name);
    trust.save(&trust_file).expect("save trust");

    let pkg = install::install(
        &carried,
        &trust,
        &blobs,
        &store,
        &installed_file,
        &models_file,
    )
    .expect("install once trusted");
    assert_eq!(pkg.manifest.name, "@powerhousedao/achra");

    // -- the models are live, and durable --------------------------------
    assert!(
        store.model_refs().iter().any(|r| r.name == "rfp" && r.version == "1"),
        "the package's models are registered"
    );
    let persisted = ph_reactor::model::persist::load_all(&models_file);
    assert_eq!(
        persisted.len(),
        1,
        "models must be persisted or the package's documents replay empty on restart"
    );

    // -- the editor is retrievable and byte-exact ------------------------
    let served = blobs
        .get(pkg.manifest.bundle.as_ref().expect("has an editor"))
        .expect("the bundle reassembles");
    assert_eq!(served, EDITOR.as_bytes(), "served verbatim");

    // -- and it is recorded as installed ---------------------------------
    let all = Installed::load(&installed_file);
    assert_eq!(all.list().len(), 1);
    assert!(all.get("@powerhousedao/achra").is_some());
}

/// A package whose bundle has not arrived yet must be refused *specifically*,
/// so the console can fetch the chunks and retry rather than reporting a flat
/// failure the operator can do nothing about.
#[test]
fn a_missing_bundle_is_reported_as_incomplete_not_as_a_bad_package() {
    let dir = tempfile::tempdir().expect("tmp");
    let with_bundle = BlobStore::open(&dir.path().join("publisher-blobs")).expect("blobs");
    let manifest = achra_manifest(&with_bundle);

    // A second node that has the manifest but none of the chunks.
    let empty = BlobStore::open(&dir.path().join("receiver-blobs")).expect("blobs");
    let mut trust = TrustStore::default();
    trust.trust(&manifest.publisher_key, "Powerhouse");

    match install::check(&manifest, &trust, &empty) {
        Err(InstallError::BundleIncomplete { missing }) => {
            assert!(missing > 0, "it should say how much is missing");
        }
        other => panic!("expected BundleIncomplete, got {other:?}"),
    }
}

/// The impostor case, end to end. A package re-signed by a different key is
/// perfectly valid on its own terms -- `verify()` passes -- and must still be
/// refused, because integrity was never the question.
#[test]
fn a_validly_signed_package_from_the_wrong_key_is_refused() {
    let dir = tempfile::tempdir().expect("tmp");
    let blobs = BlobStore::open(&dir.path().join("blobs")).expect("blobs");

    let real = achra_manifest(&blobs);
    let impostor_key = SigningKey::from_bytes(&[99u8; 32]);
    let mut fake = real.clone();
    fake.publisher_key = hex::encode(impostor_key.verifying_key().to_bytes());
    fake.publisher.name = "Powerhouse".into(); // the label is not the identity
    fake.capabilities.read.push("group@1".into()); // and it wants more
    fake.sign(&impostor_key);

    assert!(
        fake.verify().is_ok(),
        "the impostor's package is internally consistent -- that is the problem"
    );

    // The operator trusted the REAL publisher, and only that one.
    let mut trust = TrustStore::default();
    trust.trust(&real.publisher_key, "Powerhouse");

    match install::check(&fake, &trust, &blobs) {
        Err(InstallError::UntrustedPublisher { .. }) => {}
        other => panic!("a package from an untrusted key must be refused, got {other:?}"),
    }
}

/// Integrity is checked before trust, and that ordering matters: a tampered
/// package must be rejected as tampered, never surfaced to the operator as a
/// trust question they might answer yes to.
#[test]
fn tampering_is_caught_before_the_operator_is_ever_asked() {
    let dir = tempfile::tempdir().expect("tmp");
    let blobs = BlobStore::open(&dir.path().join("blobs")).expect("blobs");

    let mut tampered = achra_manifest(&blobs);
    tampered.capabilities.write.push(WriteCapability {
        model: "group@1".into(),
        kinds: vec!["add-manager".into()],
    });

    // Even with the publisher fully trusted, this must not install.
    let mut trust = TrustStore::default();
    trust.trust(&tampered.publisher_key, "Powerhouse");

    match install::check(&tampered, &trust, &blobs) {
        Err(InstallError::Integrity(_)) => {}
        other => panic!("expected an integrity failure, got {other:?}"),
    }
}

/// Uninstalling reclaims only what nothing else needs. Two packages sharing a
/// bundle is not hypothetical -- identical content is one blob by construction.
#[test]
fn uninstall_collects_only_unreachable_chunks() {
    let dir = tempfile::tempdir().expect("tmp");
    let blobs = BlobStore::open(&dir.path().join("blobs")).expect("blobs");
    let store = Store::open(&dir.path().join("docs"), &node(), "node-a").expect("store");
    let installed_file = dir.path().join("packages.json");
    let models_file = dir.path().join("models.json");

    let shared = achra_manifest(&blobs);
    let mut other = shared.clone();
    other.name = "@powerhousedao/vault".into();
    other.document_models = vec![];
    other.sign(&publisher());

    let mut trust = TrustStore::default();
    trust.trust(&shared.publisher_key, "Powerhouse");
    for m in [&shared, &other] {
        install::install(m, &trust, &blobs, &store, &installed_file, &models_file)
            .expect("install");
    }

    let mut all = Installed::load(&installed_file);
    assert!(all.remove("@powerhousedao/achra"));
    all.save(&installed_file).expect("save");
    blobs.gc(&all.live_blobs()).expect("gc");

    assert!(
        blobs.get(other.bundle.as_ref().expect("bundle")).is_ok(),
        "the surviving package's editor must still be servable after gc"
    );
}

/// A node that PUBLISHED a package must keep serving it after uninstalling it.
///
/// Reachability GC with only "what is installed" as its root deletes the chunks
/// of a package the node is still advertising, leaving it offering a bundle it
/// cannot serve. Found by walking the install flow in the console: uninstall
/// collected the only chunk, and the very next install sat waiting for a peer
/// that did not exist.
#[test]
fn uninstalling_does_not_collect_a_package_this_node_still_offers() {
    let dir = tempfile::tempdir().expect("tmp");
    let blobs = BlobStore::open(&dir.path().join("blobs")).expect("blobs");
    let store = Store::open(&dir.path().join("docs"), &node(), "node-a").expect("store");
    let installed_file = dir.path().join("packages.json");
    let models_file = dir.path().join("models.json");

    let manifest = achra_manifest(&blobs);
    let bundle = manifest.bundle.clone().expect("has an editor");

    // Publish it -- the node now offers this package to its peers.
    store
        .create_doc_model(
            "pkg-achra-1.0.0",
            &ModelRef::parse("package@1").expect("ref"),
            &json!({
                "name": "pkg-achra-1.0.0",
                "publisher_key": manifest.publisher_key,
                "package_name": manifest.name,
                "package_version": manifest.version,
                "manifest": serde_json::to_string(&manifest).expect("serialize"),
            }),
        )
        .expect("publish");

    let mut trust = TrustStore::default();
    trust.trust(&manifest.publisher_key, "Powerhouse");
    install::install(&manifest, &trust, &blobs, &store, &installed_file, &models_file)
        .expect("install");

    // Uninstall, keeping exactly what the daemon keeps: installed bundles PLUS
    // every bundle named by a package document this node carries.
    let mut all = Installed::load(&installed_file);
    assert!(all.remove("@powerhousedao/achra"));
    all.save(&installed_file).expect("save");

    let mut keep = all.live_blobs();
    assert!(keep.is_empty(), "nothing is installed any more");
    for doc in ph_reactor::query::query_docs(&store, "package", None) {
        let m: Manifest = serde_json::from_str(doc["fields"]["manifest"].as_str().expect("string"))
            .expect("parse");
        if let Some(b) = m.bundle {
            keep.push(b);
        }
    }
    blobs.gc(&keep).expect("gc");

    assert!(
        blobs.is_complete(&bundle),
        "a package this node still advertises must still be servable"
    );
}

/// The history endpoint is gated on the READ capability for the model, and on
/// the named document actually being of that model.
///
/// Without the second check a read capability on `rfp@1` would be a capability
/// to read the action log of ANY document by name -- including a group's, whose
/// log carries its membership changes. The capability names a model, so the
/// model is what it grants.
#[test]
fn a_read_capability_names_a_model_not_every_document() {
    let caps = Capabilities {
        read: vec!["rfp@1".into()],
        write: vec![],
    };

    assert!(caps.may_read("rfp@1"));
    assert!(!caps.may_read("group@1"), "reading rfps is not reading groups");
    assert!(!caps.may_read("package@1"));
    assert!(
        !caps.may_read("rfp@2"),
        "a different version is a different model"
    );

    // And the store must be able to say what model a document actually is,
    // which is what the endpoint checks the requested name against.
    let dir = tempfile::tempdir().expect("tmp");
    let store = Store::open(&dir.path().join("docs"), &node(), "node-a").expect("store");
    let def = rfp_def();
    store.add_model(std::sync::Arc::new(
        ph_reactor::model::l1::L1::from_def(def).expect("rfp model"),
    ));
    store
        .create_doc_model(
            "an-rfp",
            &ModelRef::parse("rfp@1").expect("ref"),
            &json!({ "name": "an-rfp", "title": "A thing" }),
        )
        .expect("create");

    let doc = store.get("an-rfp").expect("exists");
    let state = store.full_state(doc.id).expect("state");
    assert_eq!(state.model.name, "rfp");
    assert_ne!(
        state.model.name, "group",
        "the model check is what stops a name-based read of someone else's log"
    );
}

/// The action log is the record, so it must actually carry who signed each
/// action -- an audit trail that cannot name the signer is decoration.
#[test]
fn the_action_log_names_who_signed_each_action() {
    let dir = tempfile::tempdir().expect("tmp");
    let store = Store::open(&dir.path().join("docs"), &node(), "node-a").expect("store");
    store.add_model(std::sync::Arc::new(
        ph_reactor::model::l1::L1::from_def(rfp_def()).expect("rfp model"),
    ));
    store
        .create_doc_model(
            "an-rfp",
            &ModelRef::parse("rfp@1").expect("ref"),
            &json!({ "name": "an-rfp", "title": "A thing" }),
        )
        .expect("create");

    let doc = store.get("an-rfp").expect("exists");
    let (_, actions) = store.catch_up(doc.id, &ph_reactor::doc::VecClock::default());
    assert!(!actions.is_empty(), "creating a document records an action");
    for a in &actions {
        assert!(!a.kind.is_empty(), "every action has a kind");
        assert!(!a.origin.is_empty(), "every action names the key that signed it");
        assert!(a.ts > 0, "every action is timestamped");
    }
}
