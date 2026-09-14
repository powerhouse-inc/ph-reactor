//! The package API: publish, discover, install, remove.
//!
//! These handlers sit between three things that are deliberately separate:
//!
//! - **the mesh**, where packages arrive as `package@1` documents — an offer,
//!   never an install;
//! - **the trust store**, which records which publisher keys this operator
//!   accepts, pinned on first use;
//! - **the blob store**, which holds the editor bundle in verified chunks.
//!
//! [`crate::package::install::check`] runs those checks in a fixed order and
//! reports *which* one failed, so the console can respond to each properly:
//! prompt for trust, or fetch the missing chunks, rather than saying only that
//! install failed. That ordering is the reason this file is thin — the
//! decisions live in `src/package/`, and these handlers are transport.
//!
//! See docs/superpowers/specs/2026-09-14-plugin-packages-design.md.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::package::install::{self, Installed, InstallError};
use crate::package::trust::TrustStore;
use crate::package::Manifest;

use super::Settings;

/// Parses the manifest out of a `package@1` document.
///
/// A malformed one is skipped rather than failing the whole listing: anyone may
/// publish, so a bad document on the mesh must not be able to blank the
/// console's package list for everyone.
fn manifest_of(doc: &Value) -> Option<Manifest> {
    let raw = doc.get("fields")?.get("manifest")?.as_str()?;
    serde_json::from_str(raw).ok()
}

/// `GET /api/packages` — every package seen on the mesh, with this node's view
/// of it: is the signature good, is the publisher trusted, is the bundle here,
/// is it installed.
///
/// The console needs all four to render an honest install prompt, and computing
/// them here means the console cannot accidentally show a package as safe
/// because it forgot a check.
pub async fn list(state: State<Arc<Settings>>) -> Response {
    let trust = TrustStore::load(&state.paths.publishers_file());
    let installed = Installed::load(&state.paths.packages_file());
    let docs = crate::query::query_docs(&state.store, "package", None);

    let mut out = Vec::new();
    for doc in &docs {
        let name = doc.get("name").and_then(Value::as_str).unwrap_or_default();
        let status = doc
            .get("fields")
            .and_then(|f| f.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("published");

        let Some(m) = manifest_of(doc) else {
            out.push(json!({
                "doc": name,
                "status": status,
                "error": "this package document does not contain a readable manifest",
            }));
            continue;
        };

        let integrity = m.verify();
        let missing = m
            .bundle
            .as_ref()
            .map(|b| state.blobs.missing(b).len())
            .unwrap_or(0);
        let chunks = m.bundle.as_ref().map(|b| b.chunks.len()).unwrap_or(0);

        out.push(json!({
            "doc": name,
            "status": status,
            "name": m.name,
            "version": m.version,
            "description": m.description,
            "category": m.category,
            "publisher": m.publisher.name,
            "publisherUrl": m.publisher.url,
            "publisherKey": m.publisher_key,
            "trusted": trust.is_trusted(&m.publisher_key),
            "signatureOk": integrity.is_ok(),
            "signatureError": integrity.err(),
            "capabilities": m.capabilities.describe(),
            "models": m.document_models.len(),
            "hasEditor": m.bundle.is_some(),
            "bundleChunks": chunks,
            "bundleMissing": missing,
            "installed": installed.get(&m.name).map(|p| p.manifest.version.clone()),
        }));
    }
    (StatusCode::OK, axum::Json(out)).into_response()
}

#[derive(Deserialize)]
pub struct InstallBody {
    /// The operator's answer to "do you accept this publisher?". Required the
    /// first time a key is seen; ignored once it is pinned.
    #[serde(default)]
    pub trust_publisher: bool,
}

/// `POST /api/packages/:doc/install` — install the package in that document.
///
/// The failure cases are the interesting part, and each returns something the
/// console can act on rather than a flat error:
///
/// - the publisher is not trusted → `409` naming the key, so the console can
///   show the install prompt and retry with `trust_publisher`;
/// - the bundle is incomplete → `202`, chunk requests already sent to peers,
///   so the console can say "fetching" and poll;
/// - the signature is bad → `400`, and no amount of operator confirmation
///   changes that, because the package is not what it says it is.
pub async fn install_pkg(
    state: State<Arc<Settings>>,
    Path(doc): Path<String>,
    body: Option<axum::Json<InstallBody>>,
) -> Response {
    let trust_publisher = body.map(|b| b.0.trust_publisher).unwrap_or(false);

    let docs = crate::query::query_docs(&state.store, "package", None);
    let Some(found) = docs
        .iter()
        .find(|d| d.get("name").and_then(Value::as_str) == Some(doc.as_str()))
    else {
        return (StatusCode::NOT_FOUND, "no such package document").into_response();
    };
    let Some(manifest) = manifest_of(found) else {
        return (
            StatusCode::BAD_REQUEST,
            "this package document does not contain a readable manifest",
        )
            .into_response();
    };

    // Integrity first, before anything else is considered: a package that is
    // not what it was signed as should not reach a trust prompt at all.
    if let Err(e) = manifest.verify() {
        return (StatusCode::BAD_REQUEST, format!("signature: {e}")).into_response();
    }

    let mut trust = TrustStore::load(&state.paths.publishers_file());
    if !trust.is_trusted(&manifest.publisher_key) {
        if !trust_publisher {
            return (
                StatusCode::CONFLICT,
                axum::Json(json!({
                    "error": "publisher not trusted",
                    "publisherKey": manifest.publisher_key,
                    "publisher": manifest.publisher.name,
                    "capabilities": manifest.capabilities.describe(),
                })),
            )
                .into_response();
        }
        trust.trust(&manifest.publisher_key, &manifest.publisher.name);
        if let Err(e) = trust.save(&state.paths.publishers_file()) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
        tracing::info!(
            "operator trusted publisher {} ({})",
            manifest.publisher.name,
            manifest.publisher_key
        );
    }

    // Ask peers for what is missing before reporting the shortfall, so the
    // console's retry has a chance of succeeding.
    if let Some(bundle) = &manifest.bundle {
        let missing = state.blobs.missing(bundle);
        if !missing.is_empty() {
            let _ = state.cmd_tx.send(crate::commands::Command::FetchBlob {
                blob: bundle.clone(),
            });
            return (
                StatusCode::ACCEPTED,
                axum::Json(json!({
                    "status": "fetching",
                    "missing": missing.len(),
                    "chunks": bundle.chunks.len(),
                })),
            )
                .into_response();
        }
    }

    match install::install(
        &manifest,
        &trust,
        &state.blobs,
        &state.store,
        &state.paths.packages_file(),
        &state.paths.models_file(),
    ) {
        Ok(pkg) => (
            StatusCode::OK,
            axum::Json(json!({
                "installed": pkg.manifest.name,
                "version": pkg.manifest.version,
                "models": pkg.manifest.document_models.len(),
                "hasEditor": pkg.manifest.bundle.is_some(),
            })),
        )
            .into_response(),
        Err(InstallError::BundleIncomplete { missing }) => (
            StatusCode::ACCEPTED,
            axum::Json(json!({ "status": "fetching", "missing": missing })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

/// Every blob this node must keep.
///
/// Two roots, not one. The obvious root is what is installed. The second is
/// every bundle named by a `package@1` document, and forgetting it is a real
/// failure that this walkthrough produced: uninstall collected the chunks of a
/// package *this node had published*, so the node went on advertising a bundle
/// it could no longer serve. A package offer is a promise to serve it.
fn blobs_to_keep(state: &Arc<Settings>) -> Vec<crate::blob::BlobRef> {
    let mut keep = Installed::load(&state.paths.packages_file()).live_blobs();
    for doc in crate::query::query_docs(&state.store, "package", None) {
        if let Some(b) = manifest_of(&doc).and_then(|m| m.bundle) {
            keep.push(b);
        }
    }
    keep
}

/// `DELETE /api/plugins/:name` — remove an installed package.
///
/// Its models stay registered. That is deliberate: documents created under them
/// still exist, and unregistering a model would make those documents replay
/// empty — the failure this codebase has already met once. Removing the package
/// stops its editor being served and its capabilities being honoured, which is
/// what "uninstall" needs to mean here.
pub async fn uninstall(state: State<Arc<Settings>>, Path(name): Path<String>) -> Response {
    let file = state.paths.packages_file();
    let mut installed = Installed::load(&file);
    if !installed.remove(&name) {
        return (StatusCode::NOT_FOUND, "not installed").into_response();
    }
    if let Err(e) = installed.save(&file) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    // GC is by reachability, so a bundle another installed package shares --
    // or one this node offers to the mesh -- survives.
    match state.blobs.gc(&blobs_to_keep(&state)) {
        Ok(freed) => (
            StatusCode::OK,
            axum::Json(json!({ "removed": name, "chunksFreed": freed })),
        )
            .into_response(),
        Err(e) => {
            tracing::warn!("uninstalled {name} but blob gc failed: {e}");
            (
                StatusCode::OK,
                axum::Json(json!({ "removed": name, "gcError": e })),
            )
                .into_response()
        }
    }
}

/// `POST /api/publishers/:key/revoke` — withdraw trust from a publisher.
pub async fn revoke_publisher(state: State<Arc<Settings>>, Path(key): Path<String>) -> Response {
    let file = state.paths.publishers_file();
    let mut trust = TrustStore::load(&file);
    if !trust.revoke(&key) {
        return (StatusCode::NOT_FOUND, "not a trusted publisher").into_response();
    }
    match trust.save(&file) {
        Ok(()) => (StatusCode::OK, axum::Json(json!({ "revoked": key }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// `GET /api/publishers` — the keys this operator has accepted.
pub async fn publishers(state: State<Arc<Settings>>) -> Response {
    let trust = TrustStore::load(&state.paths.publishers_file());
    let out: Vec<Value> = trust
        .list()
        .iter()
        .map(|p| json!({ "key": p.key, "name": p.name, "approvedAt": p.approved_at }))
        .collect();
    (StatusCode::OK, axum::Json(out)).into_response()
}

#[derive(Deserialize)]
pub struct PublishBody {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub publisher_name: String,
    #[serde(default)]
    pub publisher_url: String,
    #[serde(default)]
    pub document_models: Vec<Value>,
    #[serde(default)]
    pub processors: Vec<Value>,
    #[serde(default)]
    pub capabilities: crate::package::Capabilities,
    /// The editor, as an HTML document. Stored as content-addressed chunks and
    /// referenced from the manifest by hash.
    #[serde(default)]
    pub editor: Option<String>,
}

/// `POST /api/packages` — sign and publish a package from this node.
///
/// The node's own identity key is the publisher key. That is the honest choice:
/// this node is what actually vouches for the package, and inventing a separate
/// publishing key would imply a provenance the daemon cannot back up.
pub async fn publish(state: State<Arc<Settings>>, body: axum::Json<PublishBody>) -> Response {
    let b = body.0;

    // Reject a definition that will not load before anything is signed or
    // stored, so a broken package never reaches the mesh in the first place.
    for def in &b.document_models {
        if let Err(e) = crate::model::l1::L1::from_def(def.clone()) {
            return (StatusCode::BAD_REQUEST, format!("model definition: {e}")).into_response();
        }
    }

    let bundle = match b.editor.as_ref() {
        Some(html) => match state.blobs.put(html.as_bytes()) {
            Ok(r) => Some(r),
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        },
        None => None,
    };

    let key = state.store.key();
    let mut manifest = Manifest {
        name: b.name.clone(),
        version: b.version.clone(),
        description: b.description,
        category: b.category,
        publisher: crate::package::PublisherInfo {
            name: b.publisher_name,
            url: b.publisher_url,
        },
        publisher_key: hex::encode(key.verifying_key().to_bytes()),
        document_models: b.document_models,
        processors: b.processors,
        bundle,
        capabilities: b.capabilities,
        sig: String::new(),
    };
    manifest.sign(&key);

    // Signing then immediately verifying looks redundant, and is not: it proves
    // the canonical bytes round-trip on this build before anyone else has to
    // rely on them.
    if let Err(e) = manifest.verify() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("signed a manifest that does not verify: {e}"),
        )
            .into_response();
    }

    let doc = format!("pkg-{}-{}", b.name.replace(['/', '@'], "-"), b.version);

    // A published version is immutable -- its signature covers its content, and
    // peers may already hold it. So say that, rather than let the store's
    // duplicate-name error surface an internal document name to a publisher who
    // never chose it.
    if crate::query::query_docs(&state.store, "package", None)
        .iter()
        .any(|d| d.get("name").and_then(Value::as_str) == Some(doc.as_str()))
    {
        return (
            StatusCode::CONFLICT,
            format!(
                "{} {} is already published; a signed version cannot be replaced, so publish a new version",
                manifest.name, manifest.version
            ),
        )
            .into_response();
    }

    let serialized = match serde_json::to_string(&manifest) {
        Ok(s) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let payload = json!({
        "name": doc,
        "publisher_key": manifest.publisher_key,
        "publisher_name": manifest.publisher.name,
        "package_name": manifest.name,
        "package_version": manifest.version,
        "description": manifest.description,
        "manifest": serialized,
    });

    super::run_create_doc_model(&state, &doc, "package@1", payload).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_document_without_a_manifest_is_skipped_not_fatal() {
        assert!(manifest_of(&json!({ "name": "x", "fields": {} })).is_none());
        assert!(manifest_of(&json!({ "name": "x", "fields": { "manifest": "{{{" } })).is_none());
    }

    #[test]
    fn a_well_formed_manifest_parses_out_of_a_document() {
        let m = Manifest {
            name: "achra".into(),
            version: "1.0.0".into(),
            description: String::new(),
            category: String::new(),
            publisher: Default::default(),
            publisher_key: String::new(),
            document_models: vec![],
            processors: vec![],
            bundle: None,
            capabilities: Default::default(),
            sig: String::new(),
        };
        let doc = json!({
            "name": "pkg-achra-1.0.0",
            "fields": { "manifest": serde_json::to_string(&m).expect("serialize") },
        });
        assert_eq!(manifest_of(&doc).expect("parses").name, "achra");
    }
}
