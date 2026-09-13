//! Loopback settings page + JSON API (axum on 127.0.0.1).
//!
//! The page polls `/api/status` for the shared snapshot and sends
//! mutations to the daemon's command channel, so page actions cannot
//! race the poller or the tray menu.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};

use crate::commands::Command;
use crate::query::{parse_filter, query_docs};
use crate::status::StatusSnapshot;
use crate::store::Store;

use crate::config;
use crate::doc::VecClock;
use crate::paths::StatePaths;

pub struct Settings {
    cmd_tx: mpsc::UnboundedSender<Command>,
    snap_rx: watch::Receiver<StatusSnapshot>,
    /// The doc store — the source of truth behind `/api/query`.
    store: Arc<Store>,
    /// The state paths (config file, log file) — read by the API handlers.
    paths: StatePaths,
    /// The user-configurable processor engine (subscriptions + reactions).
    processor_handle: crate::processor::ProcessorHandle,
}

pub struct SettingsHandle {
    task: tokio::task::JoinHandle<()>,
}

impl SettingsHandle {
    /// Stops the HTTP server (aborts the serve task).
    pub fn stop(&self) {
        self.task.abort();
    }
}

impl Settings {
    pub fn new(
        cmd_tx: mpsc::UnboundedSender<Command>,
        snap_rx: watch::Receiver<StatusSnapshot>,
        store: Arc<Store>,
        paths: StatePaths,
        processor_handle: crate::processor::ProcessorHandle,
    ) -> Self {
        Self {
            cmd_tx,
            snap_rx,
            store,
            paths,
            processor_handle,
        }
    }

    /// Binds the loopback listener and starts the server.
    pub async fn start(self, host: &str, port: u16) -> Result<SettingsHandle> {
        let listener = TcpListener::bind((host, port))
            .await
            .with_context(|| format!("bind {host}:{port}"))?;
        let app = Router::new()
            .route("/", get(page))
            .route("/api/status", get(status_api))
            .route("/api/drives", post(add_drive))
            .route("/api/drives/:name", delete(remove_drive))
            .route("/api/drives/:name/pause", post(pause_drive))
            .route("/api/drives/:name/resume", post(resume_drive))
            .route("/api/drives/:name/resync", post(resync_drive))
            .route("/api/config", get(config_api).post(set_config))
            .route("/api/quit", post(quit))
            .route("/api/docs", get(docs_api).post(add_doc))
            .route("/api/docs/action", post(action_doc))
            .route("/api/query", get(query_api))
            .route("/api/invite", post(make_invite))
            .route("/api/join", post(join_invite))
            .route("/api/ban", post(ban_peer))
            .route("/api/unban", post(unban_peer))
            .route("/api/overview", get(overview_api))
            .route(
                "/api/processors",
                get(processor_list_api).post(processor_create_api),
            )
            .route(
                "/api/processors/:name",
                put(processor_update_api).delete(processor_remove_api),
            )
            .route("/api/processors/:name/fires", get(processor_fires_api))
            .route("/api/groups", get(groups_api).post(create_group))
            .route("/api/groups/:name/action", post(group_action))
            .route("/api/groups/:name/activity", get(group_activity))
            .route("/api/docs/:name", get(doc_detail))
            .route("/api/llm/test", post(llm_test))
            .with_state(Arc::new(self));
        let task = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::warn!("settings server stopped: {err:#}");
            }
        });
        Ok(SettingsHandle { task })
    }
}

// ---------------------------------------------------------------------------
// API
// ---------------------------------------------------------------------------

async fn status_api(
    state: axum::extract::State<Arc<Settings>>,
) -> Result<axum::response::Response, StatusCode> {
    let snap = state.snap_rx.borrow().clone();
    Ok(axum::Json(snap).into_response())
}

#[derive(Deserialize)]
struct QueryParams {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    field: Option<String>,
    #[serde(default)]
    value: Option<String>,
}

/// `GET /api/query?model=<m>&field=<k>&value=<v>` — answer a query against
/// the maintained document set (the CQRS read model), as a JSON array of
/// `{ name, model, fields }` objects.
async fn query_api(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Query(params): axum::extract::Query<QueryParams>,
) -> Result<axum::response::Response, StatusCode> {
    let filter = match (&params.field, &params.value) {
        (Some(f), Some(v)) => parse_filter(&format!("{f}={v}")),
        _ => None,
    };
    let docs = query_docs(
        &state.store,
        params.model.as_deref().unwrap_or(""),
        filter.as_ref(),
    );
    Ok(axum::Json(docs).into_response())
}

#[derive(Deserialize)]
struct AddBody {
    #[serde(default)]
    name: String,
    addr: String,
    #[serde(default, rename = "tokenEnv")]
    token_env: Option<String>,
    #[serde(default, rename = "availableOffline")]
    available_offline: bool,
}

/// Drive names are config keys, menu labels, and URL path segments: any
/// characters are fine except the path separator and NUL.
fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && !name.contains('/') && !name.contains('\0')
}

async fn add_drive(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<AddBody>,
) -> Response {
    if !valid_name(&body.name) {
        return (
            StatusCode::BAD_REQUEST,
            "drive name may be any characters except '/' (max 64)",
        )
            .into_response();
    }
    if body.addr.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "drive addr required").into_response();
    }
    match libp2p::Multiaddr::from_str(body.addr.trim()) {
        Ok(_) => {}
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("not a valid multiaddr: {e}"),
            )
                .into_response();
        }
    }
    let cmd = Command::AddDrive {
        name: body.name,
        addr: body.addr.trim().into(),
        token_env: body.token_env,
        offline: body.available_offline,
    };
    let _ = state.cmd_tx.send(cmd);
    StatusCode::ACCEPTED.into_response()
}

/// Validation gate for the per-drive routes: `Some` is a rejection
/// (bad name), `None` means the command should be sent.
fn drive_cmd(name: &str) -> Option<Response> {
    if !valid_name(name) {
        return Some((StatusCode::BAD_REQUEST, "bad drive name").into_response());
    }
    None
}

async fn remove_drive(
    state: axum::extract::State<Arc<Settings>>,
    Path(name): Path<String>,
) -> Response {
    if let Some(r) = drive_cmd(&name) {
        return r;
    }
    let _ = state.cmd_tx.send(Command::RemoveDrive { name });
    StatusCode::ACCEPTED.into_response()
}

async fn pause_drive(
    state: axum::extract::State<Arc<Settings>>,
    Path(name): Path<String>,
) -> Response {
    if let Some(r) = drive_cmd(&name) {
        return r;
    }
    let _ = state.cmd_tx.send(Command::PauseDrive { name });
    StatusCode::ACCEPTED.into_response()
}

async fn resume_drive(
    state: axum::extract::State<Arc<Settings>>,
    Path(name): Path<String>,
) -> Response {
    if let Some(r) = drive_cmd(&name) {
        return r;
    }
    let _ = state.cmd_tx.send(Command::ResumeDrive { name });
    StatusCode::ACCEPTED.into_response()
}

async fn resync_drive(
    state: axum::extract::State<Arc<Settings>>,
    Path(name): Path<String>,
) -> Response {
    if let Some(r) = drive_cmd(&name) {
        return r;
    }
    let _ = state.cmd_tx.send(Command::ResyncDrive { name });
    StatusCode::ACCEPTED.into_response()
}

#[derive(Deserialize)]
struct ConfigBody {
    key: String,
    value: serde_json::Value,
}

async fn set_config(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<ConfigBody>,
) -> Response {
    let _ = state.cmd_tx.send(Command::SetConfig {
        key: body.key,
        value: body.value,
    });
    StatusCode::ACCEPTED.into_response()
}

async fn quit(state: axum::extract::State<Arc<Settings>>) -> Response {
    let _ = state.cmd_tx.send(Command::Quit);
    StatusCode::ACCEPTED.into_response()
}

#[derive(Deserialize)]
struct DocBody {
    name: String,
    #[serde(default)]
    fields: BTreeMap<String, serde_json::Value>,
}

/// Synchronous doc creation (unlike the drive mutations): the CLI awaits
/// the outcome, so the daemon replies through a one-shot channel.
async fn add_doc(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<DocBody>,
) -> Response {
    if !valid_name(&body.name) {
        return (
            StatusCode::BAD_REQUEST,
            "doc name may be any characters except '/' (max 64)",
        )
            .into_response();
    }
    let (reply, wait) = oneshot::channel();
    let cmd = Command::CreateDoc {
        name: body.name,
        fields: body.fields,
        reply,
    };
    if state.cmd_tx.send(cmd).is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "daemon command channel closed",
        )
            .into_response();
    }
    match tokio::time::timeout(Duration::from_secs(10), wait).await {
        Ok(Ok(Ok(()))) => (
            StatusCode::CREATED,
            axum::Json(serde_json::json!({ "ok": true })),
        )
            .into_response(),
        Ok(Ok(Err(e))) => {
            let status = if e.contains("already exists") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            (status, e).into_response()
        }
        _ => (StatusCode::SERVICE_UNAVAILABLE, "doc creation timed out").into_response(),
    }
}

#[derive(Deserialize)]
struct ActionBody {
    name: String,
    kind: String,
    payload: serde_json::Value,
    #[serde(default)]
    model: String,
}

/// Synchronous model-action application (like `add_doc`): the daemon builds
/// and signs a model action with its own key and applies it, publishing to
/// the mesh. The CLI awaits the applied action through a one-shot channel.
async fn action_doc(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<ActionBody>,
) -> Response {
    if !valid_name(&body.name) {
        return (
            StatusCode::BAD_REQUEST,
            "doc name may be any characters except '/' (max 64)",
        )
            .into_response();
    }
    let model = if body.model.trim().is_empty() {
        "open@1".to_string()
    } else {
        body.model.clone()
    };
    let (reply, wait) = oneshot::channel();
    let cmd = Command::CreateAction {
        name: body.name,
        model,
        kind: body.kind,
        payload: body.payload,
        reply,
    };
    if state.cmd_tx.send(cmd).is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "daemon command channel closed",
        )
            .into_response();
    }
    match tokio::time::timeout(Duration::from_secs(10), wait).await {
        Ok(Ok(Ok(action))) => {
            let json =
                serde_json::to_value(&action).unwrap_or_else(|_| serde_json::json!({ "ok": true }));
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Ok(Ok(Err(e))) => (StatusCode::BAD_REQUEST, e).into_response(),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "model action timed out").into_response(),
    }
}

#[derive(Deserialize)]
struct InviteBody {
    #[serde(default)]
    groups: Vec<String>,
}

/// Generate a signed invite (the daemon's identity + a challenge).
async fn make_invite(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<InviteBody>,
) -> Response {
    let (reply, wait) = oneshot::channel();
    let cmd = Command::Invite {
        groups: body.groups,
        reply,
    };
    if state.cmd_tx.send(cmd).is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "daemon command channel closed",
        )
            .into_response();
    }
    match tokio::time::timeout(Duration::from_secs(10), wait).await {
        Ok(Ok(Ok(token))) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({ "invite": token })),
        )
            .into_response(),
        Ok(Ok(Err(e))) => (StatusCode::BAD_REQUEST, e).into_response(),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            "invite generation timed out",
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct JoinBody {
    invite: String,
}

/// Consume an invite: verify it, pin the inviter (TOFU), add a drive with a
/// signed join-proof.
async fn join_invite(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<JoinBody>,
) -> Response {
    let (reply, wait) = oneshot::channel();
    let cmd = Command::Join {
        invite: body.invite,
        reply,
    };
    if state.cmd_tx.send(cmd).is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "daemon command channel closed",
        )
            .into_response();
    }
    match tokio::time::timeout(Duration::from_secs(15), wait).await {
        Ok(Ok(Ok(()))) => (
            StatusCode::ACCEPTED,
            axum::Json(serde_json::json!({ "ok": true })),
        )
            .into_response(),
        Ok(Ok(Err(e))) => (StatusCode::BAD_REQUEST, e).into_response(),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "join timed out").into_response(),
    }
}

#[derive(Deserialize)]
struct PeerBody {
    peer: String,
}

/// Ban a peer: its future handshakes are refused (synchronous).
async fn ban_peer(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<PeerBody>,
) -> Response {
    let (reply, wait) = oneshot::channel();
    let cmd = Command::Ban {
        peer: body.peer,
        reply,
    };
    if state.cmd_tx.send(cmd).is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "daemon command channel closed",
        )
            .into_response();
    }
    match tokio::time::timeout(Duration::from_secs(10), wait).await {
        Ok(Ok(Ok(()))) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({ "ok": true })),
        )
            .into_response(),
        Ok(Ok(Err(e))) => (StatusCode::BAD_REQUEST, e).into_response(),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "ban timed out").into_response(),
    }
}

/// Unban a peer: allow its handshakes again (synchronous).
async fn unban_peer(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<PeerBody>,
) -> Response {
    let (reply, wait) = oneshot::channel();
    let cmd = Command::Unban {
        peer: body.peer,
        reply,
    };
    if state.cmd_tx.send(cmd).is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "daemon command channel closed",
        )
            .into_response();
    }
    match tokio::time::timeout(Duration::from_secs(10), wait).await {
        Ok(Ok(Ok(()))) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({ "ok": true })),
        )
            .into_response(),
        Ok(Ok(Err(e))) => (StatusCode::BAD_REQUEST, e).into_response(),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "unban timed out").into_response(),
    }
}
// ---------------------------------------------------------------------------
// Console API (config, processors, groups, documents, llm)
// ---------------------------------------------------------------------------

/// `GET /api/config` — the current configuration (for the Settings form).
async fn config_api(state: axum::extract::State<Arc<Settings>>) -> Response {
    match config::load(&state.paths) {
        Ok(cfg) => (
            StatusCode::OK,
            axum::Json(serde_json::to_value(&cfg).unwrap_or_else(|_| json!({}))),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("loading config: {e}"),
        )
            .into_response(),
    }
}

/// `GET /api/overview` — the daemon's live background subsystems.
async fn overview_api(state: axum::extract::State<Arc<Settings>>) -> Response {
    let snap = state.snap_rx.borrow().clone();
    let cfg = config::load(&state.paths).unwrap_or_default();
    let log_path = state.paths.reactor_log();
    let log_size = std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
    let body = json!({
        "syncEngine": {
            "running": snap.reactor.running,
            "healthy": snap.reactor.healthy,
            "peerId": snap.reactor.peer_id,
            "listen": snap.reactor.listen,
            "configuredDrives": snap.drives.len(),
            "activeDrives": snap.reactor.active_drives,
            "lastEvent": snap.reactor.last_event,
            "mdns": cfg.p2p.mdns,
            "dht": cfg.p2p.dht,
            "relay": cfg.p2p.relay,
        },
        "store": {
            "docs": snap.reactor.docs,
            "lastDoc": snap.reactor.last_doc,
        },
        "settingsServer": {
            "url": snap.settings.url,
        },
        "logRotation": {
            "file": log_path.to_string_lossy(),
            "sizeBytes": log_size,
            "logLevel": cfg.log_level,
        },
        "statusPoller": {
            "intervalSec": crate::daemon::POLL_INTERVAL.as_secs(),
            "lastRefresh": snap.updated_at,
        },
        "llm": {
            "baseUrl": cfg.llm.base_url,
            "model": cfg.llm.model,
            "apiKeyEnv": cfg.llm.api_key_env,
        },
    });
    axum::Json(body).into_response()
}

/// `GET /api/groups` — every `group` document, with its membership.
/// The request body for creating / editing a processor spec.
#[derive(Deserialize)]
struct ProcessorBody {
    name: String,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    action_kind: Option<String>,
    #[serde(default)]
    field: Option<String>,
    #[serde(default)]
    value: Option<Value>,
    reaction: crate::processor::Reaction,
}

fn processor_spec_from(body: &ProcessorBody) -> crate::processor::ProcessorSpec {
    crate::processor::ProcessorSpec {
        name: body.name.clone(),
        models: body.models.clone(),
        action_kind: body.action_kind.clone(),
        field: body.field.clone(),
        value: body.value.clone(),
        reaction: body.reaction.clone(),
        created: crate::processor::now_ms(),
    }
}

/// `GET /api/processors` — the user-configurable processor specs.
async fn processor_list_api(state: axum::extract::State<Arc<Settings>>) -> Response {
    let specs: Vec<Value> = state
        .processor_handle
        .specs()
        .into_iter()
        .map(|s| serde_json::to_value(&s).unwrap_or_else(|_| json!({})))
        .collect();
    axum::Json(Value::Array(specs)).into_response()
}

/// `POST /api/processors` — create a processor spec.
async fn processor_create_api(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<ProcessorBody>,
) -> Response {
    if !valid_name(&body.name) {
        return (StatusCode::BAD_REQUEST, "bad processor name").into_response();
    }
    state.processor_handle.set(processor_spec_from(&body));
    (StatusCode::CREATED, axum::Json(json!({ "ok": true }))).into_response()
}

/// `PUT /api/processors/:name` — edit a processor spec (the path name wins).
async fn processor_update_api(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Json(body): axum::extract::Json<ProcessorBody>,
) -> Response {
    let mut spec = processor_spec_from(&body);
    spec.name = name;
    state.processor_handle.set(spec);
    (StatusCode::OK, axum::Json(json!({ "ok": true }))).into_response()
}

/// `DELETE /api/processors/:name` — remove a processor spec.
async fn processor_remove_api(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    state.processor_handle.remove(&name);
    (StatusCode::OK, axum::Json(json!({ "ok": true }))).into_response()
}

/// `GET /api/processors/:name/fires` — the spec's recent fire history.
async fn processor_fires_api(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    let fires: Vec<Value> = state
        .processor_handle
        .fires(&name)
        .into_iter()
        .map(|f| serde_json::to_value(&f).unwrap_or_else(|_| json!({})))
        .collect();
    axum::Json(Value::Array(fires)).into_response()
}

async fn groups_api(state: axum::extract::State<Arc<Settings>>) -> Response {
    let docs = query_docs(&state.store, "group", None);
    let mut groups = Vec::new();
    for d in docs {
        let f = d.get("fields").cloned().unwrap_or_else(|| json!({}));
        let members = f
            .get("members")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let managers = f
            .get("managers")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        groups.push(json!({
            "name": d.get("name").cloned().unwrap_or_default(),
            "members": members,
            "managers": managers,
            "memberCount": members.len(),
            "managerCount": managers.len(),
        }));
    }
    axum::Json(groups).into_response()
}

/// Sends a `CreateAction` (model + kind + payload) through the single-writer
/// command channel and awaits the one-shot reply.
async fn run_create_action(
    state: &axum::extract::State<Arc<Settings>>,
    name: &str,
    model: &str,
    kind: &str,
    payload: Value,
) -> Response {
    if !valid_name(name) {
        return (
            StatusCode::BAD_REQUEST,
            "name may be any characters except '/' (max 64)",
        )
            .into_response();
    }
    let (reply, wait) = oneshot::channel();
    let cmd = Command::CreateAction {
        name: name.to_string(),
        model: model.to_string(),
        kind: kind.to_string(),
        payload,
        reply,
    };
    if state.cmd_tx.send(cmd).is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "daemon command channel closed",
        )
            .into_response();
    }
    match tokio::time::timeout(Duration::from_secs(10), wait).await {
        Ok(Ok(Ok(action))) => {
            let j = serde_json::to_value(&action).unwrap_or_else(|_| json!({ "ok": true }));
            (StatusCode::OK, axum::Json(j)).into_response()
        }
        Ok(Ok(Err(e))) => (StatusCode::BAD_REQUEST, e).into_response(),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "action timed out").into_response(),
    }
}

/// Sends a `CreateDocModel` (model + init payload) through the command
/// channel and awaits the one-shot reply. Used to bootstrap a doc under a
/// specific model (e.g. a `group@1` group) via its `init` reducer.
async fn run_create_doc_model(
    state: &axum::extract::State<Arc<Settings>>,
    name: &str,
    model: &str,
    payload: Value,
) -> Response {
    if !valid_name(name) {
        return (
            StatusCode::BAD_REQUEST,
            "name may be any characters except '/' (max 64)",
        )
            .into_response();
    }
    let (reply, wait) = oneshot::channel();
    let cmd = Command::CreateDocModel {
        name: name.to_string(),
        model: model.to_string(),
        payload,
        reply,
    };
    if state.cmd_tx.send(cmd).is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "daemon command channel closed",
        )
            .into_response();
    }
    match tokio::time::timeout(Duration::from_secs(10), wait).await {
        Ok(Ok(Ok(()))) => (StatusCode::OK, axum::Json(json!({ "ok": true }))).into_response(),
        Ok(Ok(Err(e))) => (StatusCode::BAD_REQUEST, e).into_response(),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "create timed out").into_response(),
    }
}

#[derive(Deserialize)]
struct GroupBody {
    name: String,
    #[serde(default)]
    members: Vec<String>,
    #[serde(default)]
    managers: Vec<String>,
}

/// `POST /api/groups` — bootstrap a `group@1` document via its `init` reducer.
/// The local vault is the group's owner: its own peer id is added to the
/// managers so it can edit membership from the console. The two-person rule
/// (quorum) still gates adding a *second* manager.
async fn create_group(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<GroupBody>,
) -> Response {
    let mut managers = body.managers;
    if let Some(local) = state.snap_rx.borrow().reactor.peer_id.clone() {
        if !managers.iter().any(|m| m == &local) {
            managers.push(local);
        }
    }
    let payload = json!({
        "name": body.name,
        "members": body.members,
        "managers": managers,
    });
    run_create_doc_model(&state, &body.name, "group@1", payload).await
}

#[derive(Deserialize)]
struct GroupActionBody {
    kind: String,
    payload: Value,
}

/// `POST /api/groups/:name/action` — a group action (add-member, remove-member,
/// add-manager). The two-person rule is enforced by the model.
async fn group_action(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Json(body): axum::extract::Json<GroupActionBody>,
) -> Response {
    run_create_action(&state, &name, "group@1", &body.kind, body.payload).await
}

/// `GET /api/groups/:name/activity` — the group's recent signed actions.
async fn group_activity(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    let store = &state.store;
    let doc = match store.get(&name) {
        Some(d) => d,
        None => return (StatusCode::NOT_FOUND, "no such group").into_response(),
    };
    let (_, actions) = store.catch_up(doc.id, &VecClock::default());
    let out: Vec<Value> = actions
        .iter()
        .rev()
        .take(50)
        .map(|a| {
            json!({
                "kind": a.kind,
                "actor": a.origin,
                "ts": a.ts,
                "cosigners": a.cosig.iter().map(|c| c.origin.clone()).collect::<Vec<_>>(),
                "payload": a.payload,
            })
        })
        .collect();
    axum::Json(out).into_response()
}

#[derive(Deserialize)]
struct DocsQuery {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    field: Option<String>,
    #[serde(default)]
    value: Option<String>,
}

/// `GET /api/docs` — every document (optionally filtered by model / field).
async fn docs_api(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Query(params): axum::extract::Query<DocsQuery>,
) -> Response {
    let filter = match (&params.field, &params.value) {
        (Some(f), Some(v)) => parse_filter(&format!("{f}={v}")),
        _ => None,
    };
    axum::Json(query_docs(
        &state.store,
        params.model.as_deref().unwrap_or(""),
        filter.as_ref(),
    ))
    .into_response()
}

/// `GET /api/docs/:name` — one document's full state.
async fn doc_detail(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    let store = &state.store;
    let doc = match store.get(&name) {
        Some(d) => d,
        None => return (StatusCode::NOT_FOUND, "no such document").into_response(),
    };
    let st = match store.full_state(doc.id) {
        Some(s) => s,
        None => return (StatusCode::NOT_FOUND, "no such document").into_response(),
    };
    let fields: BTreeMap<String, Value> = st
        .doc
        .fields
        .iter()
        .map(|(k, v)| (k.clone(), v.value.clone()))
        .collect();
    axum::Json(json!({
        "name": st.doc.name,
        "model": st.model.name,
        "fields": fields,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct LlmTestBody {
    #[serde(default)]
    #[serde(rename = "baseUrl")]
    base_url: String,
    #[serde(default)]
    #[serde(rename = "apiKeyEnv")]
    api_key_env: String,
    #[serde(default)]
    model: String,
}

/// `POST /api/llm/test` — round-trip the configured OpenAI-compatible endpoint.
/// Optional body fields override the config (to test unsaved form values); the
/// API key is always read from the environment (never stored).
async fn llm_test(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<LlmTestBody>,
) -> Response {
    let cfg = config::load(&state.paths).unwrap_or_default();
    let base = pick(&body.base_url, &cfg.llm.base_url);
    let model = pick(&body.model, &cfg.llm.model);
    let key_env = pick(&body.api_key_env, &cfg.llm.api_key_env);

    if key_env.is_empty() {
        return (
            StatusCode::OK,
            axum::Json(json!({
                "ok": false, "model": model, "baseUrl": base,
                "error": "no apiKeyEnv configured",
            })),
        )
            .into_response();
    }
    let key = match std::env::var(&key_env) {
        Ok(k) if !k.is_empty() => k,
        _ => {
            return (
                StatusCode::OK,
                axum::Json(json!({
                    "ok": false, "model": model, "baseUrl": base,
                    "error": format!("environment variable {key_env} is not set"),
                })),
            )
                .into_response();
        }
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    let trimmed = base.trim_end_matches('/');
    let candidates = [format!("{trimmed}/models"), format!("{trimmed}/v1/models")];
    let mut last_err = String::new();
    for url in candidates {
        match client.get(&url).bearer_auth(&key).send().await {
            Ok(resp) if resp.status().is_success() => {
                let text = resp.text().await.unwrap_or_default();
                let v: Value =
                    serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text.clone()));
                let models = v
                    .get("data")
                    .and_then(|d| d.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|m| {
                                m.get("id").and_then(|i| i.as_str()).map(str::to_string)
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                return (
                    StatusCode::OK,
                    axum::Json(json!({
                        "ok": true, "model": model, "baseUrl": base,
                        "url": url, "models": models,
                    })),
                )
                    .into_response();
            }
            Ok(resp) => last_err = format!("HTTP {}", resp.status()),
            Err(e) => last_err = e.to_string(),
        }
    }
    (
        StatusCode::OK,
        axum::Json(json!({
            "ok": false, "model": model, "baseUrl": base, "error": last_err,
        })),
    )
        .into_response()
}

/// The first non-empty of (`override`, `default`).
fn pick(override_: &str, default: &str) -> String {
    if override_.trim().is_empty() {
        default.to_string()
    } else {
        override_.trim().to_string()
    }
}

// ---------------------------------------------------------------------------
// Single-page UI
// ---------------------------------------------------------------------------

async fn page() -> Html<&'static str> {
    Html(PAGE)
}

const PAGE: &str = include_str!("console.html");
