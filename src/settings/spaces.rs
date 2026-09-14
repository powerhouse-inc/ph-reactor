//! `/api/spaces` — create spaces, manage membership, enable apps in them.
//!
//! A space is the unit of access; an app is the unit of schema and UI. They
//! meet at the document, which carries both. These handlers are the only way
//! the console changes either.
//!
//! **Installing an app and enabling it here are different decisions and stay
//! separate.** Installing is a node-level judgement about code: you trust a
//! publisher key, its models register, its UI is served. Enabling is a
//! space-level judgement about data: these members' documents are now visible
//! to that code. Collapsing the two would mean trusting a publisher once and
//! silently granting them every space you are in.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{json, Value};

use super::Settings;
use crate::model::space::{PRIVATE, PROTECTED, PUBLIC};
use crate::query::query_docs;

fn field<'a>(d: &'a Value, k: &str) -> Option<&'a Value> {
    d.get("fields").and_then(|f| f.get(k))
}

fn str_array(d: &Value, k: &str) -> Vec<String> {
    field(d, k)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// What a tier actually guarantees, in the words the operator needs.
///
/// Not decoration. "Protected" reads like a private Slack channel and is not
/// one: every member holds a full plaintext replica, and anyone who ever had
/// access keeps what they saw. Someone will otherwise put payroll in one. This
/// string is the console's only chance to say so at the moment of the choice.
pub fn tier_meaning(visibility: &str) -> &'static str {
    match visibility {
        PUBLIC => "Anyone on the mesh can read this. Managers manage it.",
        PROTECTED => {
            "Only members can read this. Every member holds a full, unencrypted \
             copy, so anyone who ever had access keeps what they saw — removing \
             someone stops new data reaching them, it does not take back the old."
        }
        PRIVATE => {
            "Only this node can read this. It is never sent anywhere, which also \
             means there is no copy of it anywhere else if this machine is lost."
        }
        _ => "Unknown tier — treated as private.",
    }
}

/// `GET /api/spaces`
pub async fn list(state: axum::extract::State<Arc<Settings>>) -> Response {
    let mut out = Vec::new();
    for d in query_docs(&state.store, "space", None) {
        let visibility = field(&d, "visibility")
            .and_then(|v| v.as_str())
            .unwrap_or(PRIVATE)
            .to_string();
        let members = str_array(&d, "members");
        let managers = str_array(&d, "managers");
        let apps = field(&d, "apps")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        out.push(json!({
            "name": d.get("name").cloned().unwrap_or_default(),
            "visibility": visibility,
            "meaning": tier_meaning(&visibility),
            "members": members,
            "managers": managers,
            "apps": apps,
        }));
    }
    (StatusCode::OK, axum::Json(json!(out))).into_response()
}

#[derive(Deserialize)]
pub struct SpaceBody {
    name: String,
    visibility: String,
    #[serde(default)]
    members: Vec<String>,
    #[serde(default)]
    managers: Vec<String>,
}

/// `POST /api/spaces`
pub async fn create(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<SpaceBody>,
) -> Response {
    let visibility = body.visibility.trim().to_lowercase();
    if ![PUBLIC, PROTECTED, PRIVATE].contains(&visibility.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            "visibility must be public, protected or private",
        )
            .into_response();
    }

    let mut members = body.members;
    let mut managers = body.managers;
    if let Some(local) = state.snap_rx.borrow().reactor.peer_id.clone() {
        // A manager is always a member: managing a space you cannot read is
        // not a coherent state.
        if !members.iter().any(|m| m == &local) {
            members.push(local.clone());
        }
        if !managers.iter().any(|m| m == &local) {
            managers.push(local);
        }
    }

    let (reply, wait) = tokio::sync::oneshot::channel();
    let cmd = crate::commands::Command::CreateSpace {
        name: body.name.clone(),
        visibility,
        members,
        managers,
        reply,
    };
    if state.cmd_tx.send(cmd).is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "daemon command channel closed",
        )
            .into_response();
    }
    match tokio::time::timeout(std::time::Duration::from_secs(10), wait).await {
        Ok(Ok(Ok(()))) => (StatusCode::OK, axum::Json(json!({ "ok": true }))).into_response(),
        Ok(Ok(Err(e))) => (StatusCode::BAD_REQUEST, e).into_response(),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "create timed out").into_response(),
    }
}

#[derive(Deserialize)]
pub struct SpaceActionBody {
    kind: String,
    payload: Value,
}

/// `POST /api/spaces/:name/action` — add-member, remove-member, add-manager.
///
/// There is deliberately no reducer here that changes `visibility`; the model
/// has none. See `model::space`.
pub async fn action(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Json(body): axum::extract::Json<SpaceActionBody>,
) -> Response {
    super::run_create_action(&state, &name, "space@1", &body.kind, body.payload).await
}

#[derive(Deserialize)]
pub struct EnableBody {
    /// The installed package's name.
    app: String,
}

/// `POST /api/spaces/:name/apps` — enable an installed app in this space.
///
/// Refuses an app that is not installed on this node. Enabling something that
/// is not here would put an entry in the sidebar that cannot open, and worse,
/// would record a grant against a package whose capabilities nobody has read.
pub async fn enable_app(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Json(body): axum::extract::Json<EnableBody>,
) -> Response {
    let installed = crate::package::install::Installed::load(&state.paths.packages_file());
    let Some(pkg) = installed.get(&body.app) else {
        return (
            StatusCode::BAD_REQUEST,
            format!("'{}' is not installed on this node", body.app),
        )
            .into_response();
    };
    let item = json!({ "name": pkg.manifest.name, "version": pkg.manifest.version });
    super::run_create_action(&state, &name, "space@1", "enable-app", json!({ "item": item })).await
}

/// `DELETE /api/spaces/:name/apps/:app` — disable an app in this space.
///
/// The whole list is recomputed and `set`, the same shape `remove-channel`
/// uses: it converges under the canonical fold.
pub async fn disable_app(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path((name, app)): axum::extract::Path<(String, String)>,
) -> Response {
    let Some(doc) = query_docs(&state.store, "space", None)
        .into_iter()
        .find(|d| d.get("name").and_then(|v| v.as_str()) == Some(name.as_str()))
    else {
        return (StatusCode::NOT_FOUND, "no such space").into_response();
    };
    let remaining: Vec<Value> = field(&doc, "apps")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a.get("name").and_then(|v| v.as_str()) != Some(app.as_str()))
        .collect();
    super::run_create_action(
        &state,
        &name,
        "space@1",
        "disable-app",
        json!({ "apps": remaining }),
    )
    .await
}

#[derive(Deserialize)]
pub struct SpaceDocBody {
    name: String,
    model: String,
    #[serde(default)]
    payload: Value,
}

/// `POST /api/spaces/:name/docs` — create a document inside a space.
pub async fn add_doc(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path(space): axum::extract::Path<String>,
    axum::extract::Json(body): axum::extract::Json<SpaceDocBody>,
) -> Response {
    super::run_create_doc_in_space(
        &state,
        &body.name,
        &body.model,
        body.payload,
        Some(space),
    )
    .await
}

/// Is `app` enabled in the space named `space`?
///
/// The other half of a capability check: a manifest says what a plugin may
/// read, and this says where. Without it a capability is node-wide, and a
/// billing plugin open in one client's space could read another client's
/// invoices -- same model, same node, different space.
pub fn app_enabled_in(state: &Arc<Settings>, space: &str, app: &str) -> bool {
    query_docs(&state.store, "space", None)
        .into_iter()
        .find(|d| d.get("name").and_then(|v| v.as_str()) == Some(space))
        .and_then(|d| field(&d, "apps").and_then(|v| v.as_array()).cloned())
        .map(|apps| {
            apps.iter()
                .any(|a| a.get("name").and_then(|v| v.as_str()) == Some(app))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tier_says_what_it_actually_guarantees() {
        assert!(tier_meaning(PUBLIC).contains("Anyone"));

        let prot = tier_meaning(PROTECTED);
        assert!(
            prot.contains("unencrypted") && prot.contains("keeps what they saw"),
            "protected must not read like a private Slack channel: {prot}"
        );

        let priv_ = tier_meaning(PRIVATE);
        assert!(
            priv_.contains("no copy of it anywhere else"),
            "private removes the implicit backup every other document has: {priv_}"
        );
    }

    #[test]
    fn an_unknown_tier_is_described_as_private() {
        assert!(tier_meaning("weird").contains("private"));
    }
}
