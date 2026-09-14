//! The update API: publish a release, see what is offered, apply one.
//!
//! The shape mirrors `packages.rs` deliberately — a release is a signed
//! artifact from a trusted publisher, delivered as chunks, and the decision to
//! run it is the operator's. What differs is the stakes, so the differences are
//! all in the direction of refusing more: the platform must match exactly, the
//! version must be strictly newer, the publisher must already be trusted (there
//! is no trust-on-apply here, unlike a package's install prompt), and the bytes
//! are re-checked after they land on disk.
//!
//! See `src/update/apply.rs` for the ordering that makes a failed update safe.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::package::trust::TrustStore;
use crate::update::{self, apply, Release};

use super::Settings;

fn release_of(doc: &Value) -> Option<Release> {
    let raw = doc.get("fields")?.get("manifest")?.as_str()?;
    serde_json::from_str(raw).ok()
}

/// Every release document on this node, with this node's view of each.
fn offers(state: &Arc<Settings>) -> Vec<(String, Release, bool)> {
    let trust = TrustStore::load(&state.paths.publishers_file());
    crate::query::query_docs(&state.store, "release", None)
        .iter()
        .filter(|d| {
            d.get("fields")
                .and_then(|f| f.get("status"))
                .and_then(Value::as_str)
                != Some("withdrawn")
        })
        .filter_map(|d| {
            let name = d.get("name")?.as_str()?.to_string();
            let r = release_of(d)?;
            let trusted = trust.is_trusted(&r.publisher_key);
            Some((name, r, trusted))
        })
        .collect()
}

/// The newest release this node would actually be willing to run.
///
/// Every condition that would make applying fail is applied here instead, so a
/// caller never has to interpret "available" — an offer that survives this is
/// one the apply path will accept.
pub fn best_candidate(state: &Arc<Settings>) -> Option<(String, Release)> {
    let current = env!("CARGO_PKG_VERSION");
    let platform = update::current_platform();
    let mut best: Option<(String, Release)> = None;
    for (doc, r, trusted) in offers(state) {
        if !trusted || r.verify().is_err() || !r.runs_on(platform) || !r.is_newer_than(current) {
            continue;
        }
        let better = match &best {
            None => true,
            Some((_, b)) => matches!(
                update::compare(&r.version, &b.version),
                Some(std::cmp::Ordering::Greater)
            ),
        };
        if better {
            best = Some((doc, r));
        }
    }
    best
}

/// `GET /api/updates` — what this node knows about newer builds.
pub async fn list(state: State<Arc<Settings>>) -> Response {
    let current = env!("CARGO_PKG_VERSION");
    let platform = update::current_platform();
    let out: Vec<Value> = offers(&state)
        .into_iter()
        .map(|(doc, r, trusted)| {
            let integrity = r.verify();
            let missing = state.blobs.missing(&r.binary).len();
            json!({
                "doc": doc,
                "version": r.version,
                "platform": r.platform,
                "notes": r.notes,
                "publisherKey": r.publisher_key,
                "trusted": trusted,
                "signatureOk": integrity.is_ok(),
                "signatureError": integrity.err(),
                "runsHere": r.runs_on(platform),
                "newer": r.is_newer_than(current),
                "bytesMissing": missing,
                "ready": missing == 0,
            })
        })
        .collect();
    (
        StatusCode::OK,
        axum::Json(json!({
            "current": current,
            "platform": platform,
            "auto": state.update_auto,
            "releases": out,
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct ApplyBody {
    /// Which release document to apply. Absent means "the best candidate",
    /// which is what the console's one-click Update sends.
    #[serde(default)]
    pub doc: Option<String>,
}

/// `POST /api/updates/apply` — install a release and stop, so the supervisor
/// starts the new binary.
///
/// The daemon does not exec the replacement itself: restarting through whatever
/// normally starts it means the upgrade path is the path that is already known
/// to work, rather than one only ever taken during an upgrade.
pub async fn apply_update(
    state: State<Arc<Settings>>,
    body: Option<axum::Json<ApplyBody>>,
) -> Response {
    let wanted = body.and_then(|b| b.0.doc);
    let picked = match &wanted {
        Some(doc) => offers(&state)
            .into_iter()
            .find(|(d, _, _)| d == doc),
        None => best_candidate(&state).map(|(d, r)| (d, r, true)),
    };
    let Some((doc, release, trusted)) = picked else {
        return (StatusCode::NOT_FOUND, "no such release").into_response();
    };

    if let Err(e) = release.verify() {
        return (StatusCode::BAD_REQUEST, format!("signature: {e}")).into_response();
    }
    // No trust-on-apply. A package's install prompt can offer to pin a new key
    // because the operator is sitting there reading its capabilities; a binary
    // has none to read, so the key must already have been accepted.
    if !trusted {
        return (
            StatusCode::CONFLICT,
            format!(
                "the key that signed {} is not trusted; approve it under Plugins first",
                release.version
            ),
        )
            .into_response();
    }
    if state.blobs.missing(&release.binary).is_empty().eq(&false) {
        let _ = state.cmd_tx.send(crate::commands::Command::FetchBlob {
            blob: release.binary.clone(),
        });
        return (
            StatusCode::ACCEPTED,
            axum::Json(json!({
                "status": "fetching",
                "missing": state.blobs.missing(&release.binary).len(),
            })),
        )
            .into_response();
    }

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cannot locate the running binary: {e}"),
            )
                .into_response()
        }
    };
    match apply::apply(
        &release,
        &state.blobs,
        &exe,
        &state.paths.root,
        env!("CARGO_PKG_VERSION"),
    ) {
        Ok(target) => {
            tracing::warn!(
                "applied release {} from {doc} to {}; stopping so the supervisor starts it",
                release.version,
                target.display()
            );
            let _ = state.cmd_tx.send(crate::commands::Command::Quit);
            (
                StatusCode::OK,
                axum::Json(json!({
                    "applied": release.version,
                    "installedAt": target.to_string_lossy(),
                    // When this is false the running image and the running
                    // binary have diverged, and the caller is told rather than
                    // left to discover it from a version mismatch later.
                    "replacedRunningBinary": target == exe,
                    "restarting": true,
                })),
            )
                .into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

/// Query parameters for publishing. The binary is the request BODY.
///
/// Not base64 in a JSON field: a release binary is tens of megabytes, and
/// base64 inflates it by a third before it even reaches the body limit. Raw
/// bytes stream, and there is nothing to decode wrongly.
#[derive(Deserialize)]
pub struct PublishParams {
    pub version: String,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub group: Option<String>,
}

/// `POST /api/releases?version=1.9.0` with the binary as the request body.
pub async fn publish(
    state: State<Arc<Settings>>,
    axum::extract::Query(p): axum::extract::Query<PublishParams>,
    bytes: axum::body::Bytes,
) -> Response {
    let platform = p
        .platform
        .unwrap_or_else(|| update::current_platform().to_string());

    // A version no node can compare is a version no node can act on, so it is
    // refused at publish rather than silently ignored by every receiver.
    if update::compare(&p.version, "0.0.0").is_none() {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "{:?} is not a comparable version, so no node could tell whether it is an upgrade",
                p.version
            ),
        )
            .into_response();
    }
    if bytes.is_empty() {
        return (StatusCode::BAD_REQUEST, "the request body is the binary; it is empty")
            .into_response();
    }

    let binary = match state.blobs.put(&bytes) {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    let key = state.store.key();
    let mut release = Release {
        version: p.version.clone(),
        platform,
        notes: p.notes,
        binary,
        publisher_key: hex::encode(key.verifying_key().to_bytes()),
        sig: String::new(),
    };
    release.sign(&key);
    if let Err(e) = release.verify() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("signed a release that does not verify: {e}"),
        )
            .into_response();
    }

    let doc = format!("rel-{}-{}", release.version, release.platform);
    if crate::query::query_docs(&state.store, "release", None)
        .iter()
        .any(|d| d.get("name").and_then(Value::as_str) == Some(doc.as_str()))
    {
        return (
            StatusCode::CONFLICT,
            format!(
                "{} is already published for {}; a signed release cannot be replaced",
                release.version, release.platform
            ),
        )
            .into_response();
    }

    let serialized = match serde_json::to_string(&release) {
        Ok(s) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let payload = json!({
        "name": doc,
        "publisher_key": release.publisher_key,
        "release_version": release.version,
        "platform": release.platform,
        "notes": release.notes,
        "manifest": serialized,
    });
    let created = super::run_create_doc_model(&state, &doc, "release@1", payload).await;
    if created.status() != StatusCode::OK {
        return created;
    }
    if let Some(group) = p.group.as_deref().filter(|g| !g.trim().is_empty()) {
        let item = json!({ "name": doc, "kind": "doc", "model": "release", "parent": null });
        let _ =
            super::run_create_action(&state, group, "group@1", "add-doc", json!({ "item": item }))
                .await;
    }
    (
        StatusCode::OK,
        axum::Json(
            json!({ "published": doc, "version": release.version, "bytes": bytes.len() }),
        ),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_without_a_readable_release_is_skipped() {
        assert!(release_of(&json!({ "fields": {} })).is_none());
        assert!(release_of(&json!({ "fields": { "manifest": "not json" } })).is_none());
    }
}
