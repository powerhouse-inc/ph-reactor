//! Loopback settings page + JSON API (axum on 127.0.0.1).
//!
//! The page polls `/api/status` for the shared snapshot and sends
//! mutations to the daemon's command channel, so page actions cannot
//! race the poller or the tray menu.

pub mod assets;
pub mod packages;
pub mod spaces;
pub mod updates;

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
    /// Content-addressed chunks: plugin editor bundles live here.
    blobs: Arc<crate::blob::BlobStore>,
    /// Whether a verified newer release from a trusted publisher is applied
    /// without asking. Reported by `/api/updates` so the console can say which
    /// mode this node is in rather than leave it to be inferred.
    update_auto: bool,
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
        blobs: Arc<crate::blob::BlobStore>,
        update_auto: bool,
    ) -> Self {
        Self {
            cmd_tx,
            snap_rx,
            store,
            paths,
            processor_handle,
            blobs,
            update_auto,
        }
    }

    /// Binds the loopback listener and starts the server.
    pub async fn start(self, host: &str, port: u16) -> Result<SettingsHandle> {
        let listener = TcpListener::bind((host, port))
            .await
            .with_context(|| format!("bind {host}:{port}"))?;
        let app = Router::new()
            .route("/", get(page_v2))
            .route("/console", get(page))
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
            .route("/api/propose", post(propose_api))
            .route("/api/cosign", post(cosign_api))
            .route("/api/submit", post(submit_api))
            .route("/api/plugins", get(plugins_api))
            .route("/api/plugins/:name", delete(packages::uninstall))
            .route("/api/packages", get(packages::list).post(packages::publish))
            .route("/api/packages/:doc/install", post(packages::install_pkg))
            .route(
                "/api/publishers",
                get(packages::publishers).post(packages::trust_publisher),
            )
            .route("/api/updates", get(updates::list))
            .route("/api/updates/apply", post(updates::apply_update))
            .route(
                "/api/releases",
                // A release binary is tens of megabytes; the default 2 MiB
                // limit rejects every real one. Raised on THIS route only, so
                // nothing else on the API gains a large-body surface.
                post(updates::publish)
                    .layer(axum::extract::DefaultBodyLimit::max(512 * 1024 * 1024)),
            )
            .route("/api/publishers/:key/revoke", post(packages::revoke_publisher))
            .route("/api/plugins/:name/query", post(plugin_query))
            .route("/api/plugins/:name/action", post(plugin_action))
            .route("/api/plugins/:name/history", post(plugin_history))
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
            .route("/api/spaces", get(spaces::list).post(spaces::create))
            .route("/api/spaces/:name/action", post(spaces::action))
            .route("/api/spaces/:name/apps", post(spaces::enable_app))
            .route("/api/spaces/:name/apps/:app", delete(spaces::disable_app))
            .route("/api/spaces/:name/docs", post(spaces::add_doc))
            .route("/api/groups", get(groups_api).post(create_group))
            .route("/api/groups/:name", get(group_detail))
            .route("/api/groups/:name/drive", get(drive_list).post(drive_add))
            .route("/api/groups/:name/drive/:doc", delete(drive_remove))
            .route("/api/groups/:name/action", post(group_action))
            .route("/api/groups/:name/activity", get(group_activity))
            .route("/api/groups/:name/channels", post(channel_add))
            .route("/api/groups/:name/channels/:chan", delete(channel_remove))
            .route("/api/groups/:name/drive/folder", post(drive_folder))
            .route("/api/groups/:name/drive/doc", post(drive_doc))
            .route("/api/folders", get(folders_api).post(create_folder))
            .route("/api/folders/:name/action", post(folder_action))
            .route("/api/docs/:name", get(doc_detail))
            .route("/api/llm/test", post(llm_test))
            .route("/api/models", get(models_api).post(register_model))
            .route("/api/models/register", post(register_model))
            .route("/api/llm/draft-type", post(llm_draft_type))
            .route("/api/llm/draft-processor", post(llm_draft_processor))
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
    /// Optional model (`name` or `name@version`) to create the doc under its
    /// `init` reducer. When absent, a generic `open`-model doc is created.
    #[serde(default)]
    model: Option<String>,
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
    if let Some(model) = body.model {
        let model = if model.contains('@') {
            model
        } else {
            match state
                .store
                .model_refs()
                .into_iter()
                .find(|r| r.name == model)
            {
                Some(r) => format!("{}@{}", r.name, r.version),
                None => model,
            }
        };
        let mut payload = serde_json::to_value(&body.fields).unwrap_or_else(|_| json!({}));
        if !payload
            .get("name")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty())
        {
            payload["name"] = json!(body.name);
        }
        return run_create_doc_model(&state, &body.name, &model, payload).await;
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
        let m = body.model.trim();
        if m.contains('@') {
            m.to_string()
        } else {
            match state.store.model_refs().into_iter().find(|r| r.name == m) {
                Some(r) => format!("{}@{}", r.name, r.version),
                None => m.to_string(),
            }
        }
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
    // Every node counts as a peer (itself), so this number is never zero even
    // when it is the only node on the mesh; the rest is the set of remote
    // peers this store has authenticated. Computed here (backend) so the
    // console, settings, and profile views all see the same value.
    let known_peers = state.store.known_peers();
    let peer_count = known_peers.len() + 1;
    let body = json!({
        "peerId": snap.reactor.peer_id,
        "instanceName": cfg.instance.name,
        "listen": snap.reactor.listen,
        "peerCount": peer_count,
        "knownPeers": known_peers,
        "liveDocCount": snap.reactor.docs,
        "docsDir": state.paths.docs_dir.to_string_lossy(),
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
        let msgs = f
            .get("msg_text")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let drive = f
            .get("drive")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        groups.push(json!({
            "name": d.get("name").cloned().unwrap_or_default(),
            "members": members,
            "managers": managers,
            "memberCount": members.len(),
            "managerCount": managers.len(),
            "msgCount": msgs.len(),
            "driveCount": drive.len(),
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
    run_create_doc_in_space(state, name, model, payload, None).await
}

/// Create a document that belongs to a space. `space` is a space document's
/// name; `None` creates a document with no space, which replicates to every
/// peer exactly as documents did before spaces existed.
pub(super) async fn run_create_doc_in_space(
    state: &axum::extract::State<Arc<Settings>>,
    name: &str,
    model: &str,
    payload: Value,
    space: Option<String>,
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
        space,
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
/// The local vault is the group's owner: its own peer id is added as both a
/// member and a manager, so it can use the channel and drive and edit
/// membership from the console. The two-person rule (quorum) still gates
/// adding a *second* manager.
async fn create_group(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<GroupBody>,
) -> Response {
    let mut members = body.members;
    let mut managers = body.managers;
    if let Some(local) = state.snap_rx.borrow().reactor.peer_id.clone() {
        // A manager is always a member: the creator needs the channel/drive
        // (member-gated) as well as membership control (manager-gated).
        if !members.iter().any(|m| m == &local) {
            members.push(local.clone());
        }
        if !managers.iter().any(|m| m == &local) {
            managers.push(local);
        }
    }
    let payload = json!({
        "name": body.name,
        "members": members,
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

#[derive(serde::Deserialize)]
struct ProposeBody {
    /// The document the action targets. Named `group` because the first
    /// quorum-gated reducer was `group`'s `add-manager`, but any document
    /// whose model declares a quorum works.
    group: String,
    kind: String,
    #[serde(default)]
    payload: Value,
    /// The governing model. Defaults to `group@1` for the membership actions
    /// this was built for; domain models that declare their own quorum (the
    /// Achra `rfp` model gates `award` on two approvers) pass their own.
    #[serde(default)]
    model: Option<String>,
}

#[derive(serde::Deserialize)]
struct TokenBody {
    token: String,
}

/// Runs a command that answers with a single string, shared by the three
/// co-signing endpoints.
async fn string_command<F>(state: &Arc<Settings>, make: F) -> Response
where
    F: FnOnce(oneshot::Sender<Result<String, String>>) -> Command,
{
    let (reply, wait) = oneshot::channel();
    if state.cmd_tx.send(make(reply)).is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "daemon command channel closed",
        )
            .into_response();
    }
    match tokio::time::timeout(Duration::from_secs(15), wait).await {
        Ok(Ok(Ok(s))) => (StatusCode::OK, axum::Json(json!({ "token": s }))).into_response(),
        Ok(Ok(Err(e))) => (StatusCode::BAD_REQUEST, e).into_response(),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "timed out").into_response(),
    }
}

/// `POST /api/propose` — build a quorum-gated action and return a token for
/// other members to co-sign.
async fn propose_api(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<ProposeBody>,
) -> Response {
    let model = match body.model {
        Some(m) if m.contains('@') => m,
        Some(m) => format!("{m}@1"),
        None => "group@1".into(),
    };
    string_command(&state, |reply| Command::Propose {
        name: body.group,
        model,
        kind: body.kind,
        payload: body.payload,
        reply,
    })
    .await
}

/// `POST /api/cosign` — add this node's co-signature to a proposal.
async fn cosign_api(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<TokenBody>,
) -> Response {
    string_command(&state, |reply| Command::CoSign {
        token: body.token,
        reply,
    })
    .await
}

/// `POST /api/submit` — apply a proposal that has reached its quorum.
async fn submit_api(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<TokenBody>,
) -> Response {
    string_command(&state, |reply| Command::Submit {
        token: body.token,
        reply,
    })
    .await
}

// -- plugin bridge ----------------------------------------------------------
//
// Capability enforcement lives HERE, in Rust, not in the console's JavaScript.
// The host page mediates the iframe's postMessage calls, but if that were the
// only check then anything able to reach this origin would bypass it. Enforcing
// server-side means the console's JS is a convenience, not the security
// boundary.

#[derive(serde::Deserialize)]
struct PluginQueryBody {
    /// `name@version`, matched exactly against the declared capability.
    model: String,
    #[serde(default)]
    filter: Option<crate::query::FieldFilter>,
    /// The space the plugin is open in. Results are confined to it.
    #[serde(default)]
    space: Option<String>,
}

#[derive(serde::Deserialize)]
struct PluginActionBody {
    model: String,
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    payload: Value,
    /// The space the plugin is open in. A document it creates lands here.
    #[serde(default)]
    space: Option<String>,
}

/// Looks up an installed package. `None` means not installed.
fn installed_package(
    state: &Arc<Settings>,
    name: &str,
) -> Option<crate::package::install::InstalledPackage> {
    crate::package::install::Installed::load(&state.paths.packages_file())
        .get(name)
        .cloned()
}

/// `GET /api/plugins` — what is installed, for the console's plugin list.
async fn plugins_api(state: axum::extract::State<Arc<Settings>>) -> Response {
    let installed = crate::package::install::Installed::load(&state.paths.packages_file());
    let out: Vec<Value> = installed
        .list()
        .iter()
        .map(|p| {
            json!({
                "name": p.manifest.name,
                "version": p.manifest.version,
                "description": p.manifest.description,
                "publisher": p.manifest.publisher.name,
                "publisherKey": p.manifest.publisher_key,
                "hasEditor": p.manifest.bundle.is_some(),
                "capabilities": p.manifest.capabilities.describe(),
                "nav": p.manifest.ui.nav,
                "installedAt": p.installed_at,
            })
        })
        .collect();
    (StatusCode::OK, axum::Json(out)).into_response()
}

/// `POST /api/plugins/:name/query` — a read, gated by the declared capability.
async fn plugin_query(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Json(body): axum::extract::Json<PluginQueryBody>,
) -> Response {
    let Some(pkg) = installed_package(&state, &name) else {
        return (StatusCode::NOT_FOUND, "no such package installed").into_response();
    };
    if !pkg.manifest.capabilities.may_read(&body.model) {
        tracing::warn!(
            "plugin {name} attempted to read {} without the capability",
            body.model
        );
        return (
            StatusCode::FORBIDDEN,
            format!("this plugin may not read {}", body.model),
        )
            .into_response();
    }
    // A capability says *what* a plugin may read; the space says *where*.
    // Without the second half a capability is node-wide, and a billing plugin
    // open in one client's space could read another client's invoices -- same
    // model, same node, different space.
    if let Some(space) = &body.space {
        if !spaces::app_enabled_in(&state, space, &name) {
            tracing::warn!("plugin {name} read in space '{space}' where it is not enabled");
            return (
                StatusCode::FORBIDDEN,
                format!("'{name}' is not enabled in this space"),
            )
                .into_response();
        }
    }
    let model = body.model.split('@').next().unwrap_or_default().to_string();
    let mut docs = crate::query::query_docs(&state.store, &model, body.filter.as_ref());
    if let Some(space) = &body.space {
        let space_id = state.store.get(space).map(|d| d.id);
        docs.retain(|d| {
            d.get("id")
                .and_then(|v| v.as_str())
                .and_then(|s| crate::doc::DocId::parse(s).ok())
                .and_then(|id| state.store.space_of(id))
                == space_id
        });
    }
    (StatusCode::OK, axum::Json(docs)).into_response()
}

/// `POST /api/plugins/:name/action` — a write, gated by the declared capability.
///
/// The capability narrows what the plugin may ATTEMPT. The action then goes
/// through the ordinary path, so the model's own `auth`, `pre` and quorum rules
/// still decide whether it applies — a capability never widens what the store
/// permits.
async fn plugin_action(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Json(body): axum::extract::Json<PluginActionBody>,
) -> Response {
    let Some(pkg) = installed_package(&state, &name) else {
        return (StatusCode::NOT_FOUND, "no such package installed").into_response();
    };
    if !pkg.manifest.capabilities.may_write(&body.model, &body.kind) {
        tracing::warn!(
            "plugin {name} attempted {} on {} without the capability",
            body.kind,
            body.model
        );
        return (
            StatusCode::FORBIDDEN,
            format!(
                "this plugin may not perform {} on {}",
                body.kind, body.model
            ),
        )
            .into_response();
    }
    if let Some(space) = &body.space {
        if !spaces::app_enabled_in(&state, space, &name) {
            tracing::warn!("plugin {name} wrote in space '{space}' where it is not enabled");
            return (
                StatusCode::FORBIDDEN,
                format!("'{name}' is not enabled in this space"),
            )
                .into_response();
        }
    }
    // `init` creates a document; every other kind acts on an existing one.
    if body.kind == "init" {
        let mut payload = body.payload.clone();
        // The reducer sets __name__ from payload.name, so make sure it is
        // there rather than silently creating an unnamed document.
        if payload
            .get("name")
            .and_then(|v| v.as_str())
            .is_none_or(str::is_empty)
        {
            payload["name"] = json!(body.name);
        }
        // Created in the space the plugin is open in, so the document is born
        // with the access it should have rather than being moved later --
        // which the signed, immutable binding makes impossible anyway.
        return run_create_doc_in_space(&state, &body.name, &body.model, payload, body.space).await;
    }
    run_create_action(&state, &body.name, &body.model, &body.kind, body.payload).await
}

#[derive(serde::Deserialize)]
struct PluginHistoryBody {
    /// The governing model, checked against the READ capability: seeing who
    /// did what to a document is a read of that document.
    model: String,
    /// The document to show the trail for.
    name: String,
    #[serde(default)]
    limit: Option<usize>,
}

/// `POST /api/plugins/:name/history` — a document's signed action log.
///
/// This is the part of an event-sourced store that is worth showing: every
/// action carries the key that signed it, so "who agreed to what, and when" is
/// answerable from the document itself rather than from a server's word for it.
///
/// Gated on the read capability, not a separate one. A plugin that may read a
/// document may see how it came to say what it says — the history is the
/// document, and pretending otherwise would suggest the current state is
/// somehow less revealing than the log that produced it.
async fn plugin_history(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Json(body): axum::extract::Json<PluginHistoryBody>,
) -> Response {
    let Some(pkg) = installed_package(&state, &name) else {
        return (StatusCode::NOT_FOUND, "no such package installed").into_response();
    };
    if !pkg.manifest.capabilities.may_read(&body.model) {
        tracing::warn!(
            "plugin {name} attempted to read the history of {} without the capability",
            body.model
        );
        return (
            StatusCode::FORBIDDEN,
            format!("this plugin may not read {}", body.model),
        )
            .into_response();
    }
    let Some(doc) = state.store.get(&body.name) else {
        return (StatusCode::NOT_FOUND, "no such document").into_response();
    };
    // The capability names a model, so check the document actually IS one --
    // otherwise a read capability on `rfp@1` would open every document by name.
    let model_name = body.model.split('@').next().unwrap_or_default();
    let is_that_model = state
        .store
        .full_state(doc.id)
        .map(|st| st.model.name == model_name)
        .unwrap_or(false);
    if !is_that_model {
        return (
            StatusCode::FORBIDDEN,
            format!("{} is not a {}", body.name, body.model),
        )
            .into_response();
    }

    let (_, actions) = state.store.catch_up(doc.id, &VecClock::default());
    let limit = body.limit.unwrap_or(50).min(200);
    let out: Vec<Value> = actions
        .iter()
        .rev()
        .take(limit)
        .map(|a| {
            json!({
                "kind": a.kind,
                "actor": a.origin,
                // Raw, as the store records it: MICROseconds since the epoch.
                // Kept because it is what the signed record contains.
                "ts": a.ts,
                // And the same instant unambiguously. Processor fires in this
                // same API are milliseconds, so a bare integer is a unit a
                // consumer has to guess at -- and guessing wrong renders an
                // audit trail dated to the year 58000, which is how this was
                // noticed.
                "at": crate::status::rfc3339(a.ts / 1_000_000),
                "cosigners": a.cosig.iter().map(|c| c.origin.clone()).collect::<Vec<_>>(),
                "payload": a.payload,
            })
        })
        .collect();
    (StatusCode::OK, axum::Json(out)).into_response()
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

/// `GET /api/groups/:name` — the group's full state: membership, its
/// channels (the `channels` array), its messages (the `msg_*` arrays), and
/// its drive. This is what the console renders as the shared space.
async fn group_detail(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    let store = &state.store;
    let doc = match store.get(&name) {
        Some(d) => d,
        None => return (StatusCode::NOT_FOUND, "no such group").into_response(),
    };
    let st = match store.full_state(doc.id) {
        Some(s) => s,
        None => return (StatusCode::NOT_FOUND, "no such group").into_response(),
    };
    let fields: BTreeMap<String, Value> = st
        .doc
        .fields
        .iter()
        .map(|(k, v)| (k.clone(), v.value.clone()))
        .collect();
    let arr = |k: &str| -> Value { fields.get(k).cloned().unwrap_or_else(|| json!([])) };
    axum::Json(json!({
        "name": st.doc.name,
        "members": arr("members"),
        "managers": arr("managers"),
        "msgFrom": arr("msg_from"),
        "msgText": arr("msg_text"),
        "msgTs": arr("msg_ts"),
        "msgChannel": arr("msg_channel"),
        "channels": arr("channels"),
        "drive": arr("drive"),
    }))
    .into_response()
}

/// `GET /api/groups/:name/drive` — the group's drive as a flat list of items
/// (folders + docs), each with `kind`, `model`, and `parent` (null = root).
/// The console turns this into a folder tree with a breadcrumb. A legacy
/// `string` entry (pre-folder drive) is treated as a root `note` doc. Docs
/// carry their current `fields`; a gone doc is flagged `missing`.
async fn drive_list(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    let store = state.store.as_ref();
    let gdoc = match store.get(&name) {
        Some(d) => d,
        None => return (StatusCode::NOT_FOUND, "no such group").into_response(),
    };
    let gst = match store.full_state(gdoc.id) {
        Some(s) => s,
        None => return (StatusCode::NOT_FOUND, "no such group").into_response(),
    };
    let drive = gst
        .doc
        .fields
        .get("drive")
        .map(|f| f.value.clone())
        .unwrap_or_else(|| json!([]));
    let mut out = Vec::new();
    for entry in drive.as_array().cloned().unwrap_or_default() {
        // Normalize a legacy string entry into a root note item.
        let item = match entry.as_str() {
            Some(s) => json!({ "name": s, "kind": "doc", "model": "note", "parent": null }),
            None => entry.clone(),
        };
        let item_name = drive_item_name(&item).to_string();
        let kind = item.get("kind").and_then(Value::as_str).unwrap_or("doc");
        let mut o = json!({
            "name": item_name,
            "kind": kind,
            "model": item.get("model").cloned().unwrap_or(Value::Null),
            "parent": item.get("parent").cloned(),
        });
        if kind == "doc" && !item_name.is_empty() {
            match store.get(&item_name).and_then(|d| store.full_state(d.id)) {
                Some(ds) => {
                    let fields: BTreeMap<String, Value> = ds
                        .doc
                        .fields
                        .iter()
                        .map(|(k, v)| (k.clone(), v.value.clone()))
                        .collect();
                    o["model"] = Value::String(ds.model.name.clone());
                    let title = fields
                        .get("title")
                        .cloned()
                        .unwrap_or_else(|| Value::String(item_name.clone()));
                    o["fields"] = Value::Object(fields.into_iter().collect());
                    o["title"] = title;
                }
                None => {
                    o["missing"] = json!(true);
                }
            }
        }
        out.push(o);
    }
    axum::Json(out).into_response()
}

/// `POST /api/groups/:name/channels` — add a channel (a **member** may).
/// `visibility` is `public` (any member posts) or `private` (only `members`).
#[derive(Deserialize)]
struct ChannelBody {
    name: String,
    #[serde(default)]
    visibility: String,
    #[serde(default)]
    members: Vec<String>,
}
async fn channel_add(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Json(body): axum::extract::Json<ChannelBody>,
) -> Response {
    if !valid_name(&body.name) {
        return (
            StatusCode::BAD_REQUEST,
            "a channel name is required (no '/', max 64)",
        )
            .into_response();
    }
    let vis = if body.visibility == "private" {
        "private"
    } else {
        "public"
    };
    let item = json!({ "name": body.name, "visibility": vis, "members": body.members });
    run_create_action(
        &state,
        &name,
        "group@1",
        "add-channel",
        json!({ "item": item }),
    )
    .await
}

/// `DELETE /api/groups/:name/channels/:chan` — remove a channel (a **manager**
/// may). Recomputes the `channels` array and `set`s it (per-field LWW).
async fn channel_remove(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path((name, chan)): axum::extract::Path<(String, String)>,
) -> Response {
    let store = state.store.as_ref();
    let gdoc = match store.get(&name) {
        Some(d) => d,
        None => return (StatusCode::NOT_FOUND, "no such group").into_response(),
    };
    let gst = match store.full_state(gdoc.id) {
        Some(s) => s,
        None => return (StatusCode::NOT_FOUND, "no such group").into_response(),
    };
    let channels = gst
        .doc
        .fields
        .get("channels")
        .map(|f| f.value.clone())
        .unwrap_or_else(|| Value::Array(vec![]));
    let next: Vec<Value> = channels
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.get("name").and_then(Value::as_str) != Some(chan.as_str()))
        .collect();
    set_group_field(
        &state,
        &name,
        "channels",
        "remove-channel",
        Value::Array(next),
    )
    .await
}

/// `POST /api/groups/:name/drive/folder` — add a folder to the drive (a
/// **member** may). `parent` (null = root) names the enclosing folder.
#[derive(Deserialize)]
struct DriveFolderBody {
    name: String,
    #[serde(default)]
    parent: Option<String>,
}
async fn drive_folder(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Json(body): axum::extract::Json<DriveFolderBody>,
) -> Response {
    if !valid_name(&body.name) {
        return (
            StatusCode::BAD_REQUEST,
            "a folder name is required (no '/', max 64)",
        )
            .into_response();
    }
    let item = json!({
        "name": body.name,
        "kind": "folder",
        "model": Value::Null,
        "parent": body.parent,
    });
    run_create_action(
        &state,
        &name,
        "group@1",
        "add-folder",
        json!({ "item": item }),
    )
    .await
}

/// `POST /api/groups/:name/drive/doc` — create a doc of the chosen model type
/// in the group's drive (a **member** may). `model` is a type name (or
/// `name@version`) from the model catalog; `parent` (null = root) places it;
/// `fields` are the initial values (missing ones default by type).
#[derive(Deserialize)]
struct DriveDocBody {
    #[serde(default)]
    name: Option<String>,
    model: String,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    fields: Value,
}
async fn drive_doc(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Json(body): axum::extract::Json<DriveDocBody>,
) -> Response {
    if body.model.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "a model type is required").into_response();
    }
    let model = match state
        .store
        .model_refs()
        .into_iter()
        .find(|r| r.name == body.model)
    {
        Some(r) => format!("{}@{}", r.name, r.version),
        None => body.model.clone(),
    };
    let type_name = model.split('@').next().unwrap_or(&body.model).to_string();
    let doc_name = match &body.name {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => {
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            format!("{type_name}-{ms}")
        }
    };
    let def = crate::doc::ModelRef::parse(&model)
        .ok()
        .and_then(|r| state.store.model_definition(&r));
    let payload = init_payload_with_defaults(def.as_ref(), &doc_name, &body.fields);
    let created = run_create_doc_model(&state, &doc_name, &model, payload).await;
    if created.status() != StatusCode::OK {
        return created;
    }
    let item = json!({
        "name": doc_name,
        "kind": "doc",
        "model": type_name,
        "parent": body.parent,
    });
    run_create_action(&state, &name, "group@1", "add-doc", json!({ "item": item })).await
}

/// `POST /api/groups/:name/drive` — compatibility "new file": create a `note`
/// doc at the drive root. (The folder-aware equivalents are
/// `.../drive/folder` and `.../drive/doc`.)
#[derive(Deserialize)]
struct DriveAddBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
}
async fn drive_add(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Json(body): axum::extract::Json<DriveAddBody>,
) -> Response {
    let note_name = match &body.name {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => {
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            format!("note-{ms}")
        }
    };
    let title = if body.title.trim().is_empty() {
        note_name.clone()
    } else {
        body.title.clone()
    };
    let payload = json!({ "name": note_name, "title": title, "body": body.body });
    let created = run_create_doc_model(&state, &note_name, "note@1", payload).await;
    if created.status() != StatusCode::OK {
        return created;
    }
    let item = json!({
        "name": note_name,
        "kind": "doc",
        "model": "note",
        "parent": Value::Null,
    });
    run_create_action(&state, &name, "group@1", "add-doc", json!({ "item": item })).await
}

/// `DELETE /api/groups/:name/drive/:item` — remove a drive item (a folder or
/// a doc) from the group's drive (a **manager** may). Recomputes the `drive`
/// array and `set`s it (per-field LWW).
async fn drive_remove(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path((name, item)): axum::extract::Path<(String, String)>,
) -> Response {
    let store = state.store.as_ref();
    let gdoc = match store.get(&name) {
        Some(d) => d,
        None => return (StatusCode::NOT_FOUND, "no such group").into_response(),
    };
    let gst = match store.full_state(gdoc.id) {
        Some(s) => s,
        None => return (StatusCode::NOT_FOUND, "no such group").into_response(),
    };
    let drive = gst
        .doc
        .fields
        .get("drive")
        .map(|f| f.value.clone())
        .unwrap_or_else(|| Value::Array(vec![]));
    let next: Vec<Value> = drive
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| drive_item_name(i) != item.as_str())
        .collect();
    set_group_field(&state, &name, "drive", "remove-item", Value::Array(next)).await
}

/// `set` one of a group's array fields through a (manager-gated) reducer —
/// the "recompute the whole array" pattern, which converges under per-field
/// last-writer-wins like any other field write.
async fn set_group_field(
    state: &axum::extract::State<Arc<Settings>>,
    group: &str,
    field: &str,
    reducer: &str,
    next: Value,
) -> Response {
    let mut payload = serde_json::Map::new();
    payload.insert(field.to_string(), next);
    run_create_action(state, group, "group@1", reducer, Value::Object(payload)).await
}

/// The name of a drive item, whether it is a legacy string or an object.
fn drive_item_name(item: &Value) -> &str {
    item.as_str()
        .unwrap_or_else(|| item.get("name").and_then(Value::as_str).unwrap_or(""))
}

/// A type-appropriate default for a model field type.
fn default_value(t: &str) -> Value {
    match t {
        "string" => Value::String(String::new()),
        "number" => Value::from(0),
        "boolean" => Value::Bool(false),
        "object" => Value::Object(serde_json::Map::new()),
        "string[]" | "number[]" | "boolean[]" | "array" => Value::Array(vec![]),
        _ => Value::Null,
    }
}

/// Build an `init` payload: for each field in the model's init schema, use
/// the caller's value if present, else a type default. The doc `name` always
/// wins. Extra caller fields pass through (the reducer ignores them).
fn init_payload_with_defaults(def: Option<&Value>, name: &str, fields: &Value) -> Value {
    let mut out = serde_json::Map::new();
    let schema = def
        .and_then(|d| d.get("reducers"))
        .and_then(|r| r.get("init"))
        .and_then(|r| r.get("payload"));
    if let Some(obj) = schema.and_then(Value::as_object) {
        for (f, t) in obj {
            let v = fields
                .get(f)
                .cloned()
                .unwrap_or_else(|| default_value(t.as_str().unwrap_or("")));
            out.insert(f.clone(), v);
        }
    }
    if let Some(obj) = fields.as_object() {
        for (k, v) in obj {
            out.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    out.insert("name".into(), Value::String(name.to_string()));
    Value::Object(out)
}

/// `GET /api/folders` — the registered folder documents.
async fn folders_api(state: axum::extract::State<Arc<Settings>>) -> Response {
    let docs = query_docs(&state.store, "folder", None);
    let mut out = Vec::new();
    for d in docs {
        let f = d.get("fields").cloned().unwrap_or_else(|| json!({}));
        let members = f
            .get("members")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        out.push(json!({
            "name": d.get("name").cloned().unwrap_or_default(),
            "description": f.get("description").cloned().unwrap_or(Value::String(String::new())),
            "members": members,
            "memberCount": members.len(),
        }));
    }
    axum::Json(out).into_response()
}

#[derive(Deserialize)]
struct FolderBody {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    members: Vec<String>,
}

/// `POST /api/folders` — create a `folder@1` document.
async fn create_folder(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<FolderBody>,
) -> Response {
    let payload = json!({
        "name": body.name,
        "description": body.description,
        "members": body.members,
    });
    run_create_doc_model(&state, &body.name, "folder@1", payload).await
}

/// `POST /api/folders/:name/action` — a folder action (add-member,
/// remove-member, set-description).
async fn folder_action(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Json(body): axum::extract::Json<GroupActionBody>,
) -> Response {
    run_create_action(&state, &name, "folder@1", &body.kind, body.payload).await
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

/// Refuse an LLM base URL whose host is a link-local address (the cloud
/// metadata range `169.254.0.0/16`): a per-request `baseUrl` on
/// `POST /api/llm/test` could otherwise point the daemon at that address to
/// read cloud credentials, or at an attacker URL to exfiltrate the API key.
/// The settings API is loopback-only, so this blocks the SSRF/exfil path
/// without a token.
fn llm_base_url_ok(base: &str) -> Result<(), String> {
    let after_scheme = base.split_once("://").map(|(_, r)| r).unwrap_or(base);
    let hostport = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host = if let Some(s) = hostport.strip_prefix('[') {
        s.split(']').next().unwrap_or("")
    } else {
        hostport.split(':').next().unwrap_or(hostport)
    };
    if let Ok(std::net::IpAddr::V4(v4)) = host.parse::<std::net::IpAddr>() {
        if v4.to_bits() >> 16 == 169 * 256 + 254 {
            return Err("refusing LLM request to a link-local (cloud metadata) address".into());
        }
    }
    Ok(())
}

/// `POST /api/llm/test` — round-trip the configured OpenAI-compatible endpoint.
/// Optional body fields override the config (to test unsaved form values); the
/// API key is always read from the environment (never stored).
async fn llm_test(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<LlmTestBody>,
) -> Response {
    let cfg = config::load(&state.paths).unwrap_or_default();
    let base = match llm_base_url_ok(&body.base_url) {
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({
                    "ok": false,
                    "model": "",
                    "baseUrl": body.base_url,
                    "error": msg
                })),
            )
                .into_response();
        }
        Ok(()) => pick(&body.base_url, &cfg.llm.base_url),
    };
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

/// `GET /api/models` — the registered document-model types (for the
/// create-document picker and the rich editor). `enums` maps a field name to
/// its allowed values, so the editor can render a dropdown for that field.
async fn models_api(state: axum::extract::State<Arc<Settings>>) -> Response {
    let mut out = Vec::new();
    for r in state.store.model_refs() {
        let def = state.store.model_definition(&r).unwrap_or(Value::Null);
        out.push(json!({
            "name": r.name,
            "version": r.version,
            "fields": def.get("fields").cloned().unwrap_or(json!({})),
            "reducers": def.get("reducers").cloned().unwrap_or(json!({})),
            "enums": def.get("enums").cloned().unwrap_or(json!({})),
        }));
    }
    axum::Json(out).into_response()
}

/// `POST /api/models/register` — validate and register a model definition
/// (from the LLM draft or the manual editor) as an `L1` interpreter.
#[derive(Deserialize)]
struct RegisterModelBody {
    definition: Value,
}

async fn register_model(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<RegisterModelBody>,
) -> Response {
    match crate::model::l1::L1::from_def(body.definition.clone()) {
        Ok(m) => {
            let name = crate::model::Model::ref_(&m).name.clone();
            state.store.add_model(Arc::new(m));
            // Persist it, or the next restart replays this model's documents
            // with no definition to reduce them and they come back empty.
            if let Err(e) =
                crate::model::persist::save(&state.paths.models_file(), &body.definition)
            {
                tracing::warn!("model '{name}' registered but not persisted: {e}");
            }
            (
                StatusCode::CREATED,
                axum::Json(json!({ "ok": true, "name": name })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({ "ok": false, "error": e })),
        )
            .into_response(),
    }
}

/// `POST /api/llm/draft-type` — ask the configured LLM to draft a model
/// definition from a description (returned for review, not yet registered).
#[derive(Deserialize)]
struct DraftTypeBody {
    description: String,
}

async fn llm_draft_type(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<DraftTypeBody>,
) -> Response {
    let cfg = config::load(&state.paths).unwrap_or_default();
    let key = std::env::var(&cfg.llm.api_key_env).unwrap_or_default();
    if key.is_empty() {
        return (
            StatusCode::OK,
            axum::Json(json!({
                "ok": false,
                "error": "no LLM API key set (the value of the configured apiKeyEnv); use the manual type editor",
            })),
        )
            .into_response();
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .build()
        .unwrap_or_default();
    let sys = r#"You design document models for an event-sourced store. Given a short description, return ONE valid JSON object and nothing else: no prose, no markdown fences, no trailing commas.

Shape — "fields" maps each field name to exactly one of the type strings string | number | boolean | object | array:

{
  "name": "lowercase-hyphenated",
  "version": "1",
  "fields": { "<field>": "<string|number|boolean|object|array>" },
  "reducers": {
    "init": {
      "payload": { "name": "string", "<field>": "<type>" },
      "writes": { "__name__": { "set": "$payload.name" }, "<field>": { "set": "$payload.<field>" } },
      "pre": []
    },
    "set-<field>": {
      "payload": { "<field>": "<type>" },
      "writes": { "<field>": { "set": "$payload.<field>" } },
      "pre": []
    }
  }
}

Rules:
- "name" is lowercase-hyphenated; "version" is the string "1".
- "name" is the document's identity, not a field: it appears only in init's payload and as the "__name__" write, never in "fields".
- Every reducer has "payload", "writes", and a "pre" array; leave "pre" empty unless the description demands a guard.
- "init" creates the document: its payload holds the name plus every field, and its writes set "__name__" to "$payload.name" and each field to "$payload.<field>".
- Add one "set-<field>" reducer per field so each can be updated on its own.
- Every write value is an object with exactly one key: "set" (normal fields), "append" (add to an array field), or "remove".
- Templates available in writes: "$payload.<field>", "$actor", "$ts".

Complete example, for "a note with a body and a number of stars":
{
  "name": "note",
  "version": "1",
  "fields": { "body": "string", "stars": "number" },
  "reducers": {
    "init": {
      "payload": { "name": "string", "body": "string", "stars": "number" },
      "writes": { "__name__": { "set": "$payload.name" }, "body": { "set": "$payload.body" }, "stars": { "set": "$payload.stars" } },
      "pre": []
    },
    "set-body": { "payload": { "body": "string" }, "writes": { "body": { "set": "$payload.body" } }, "pre": [] },
    "set-stars": { "payload": { "stars": "number" }, "writes": { "stars": { "set": "$payload.stars" } }, "pre": [] }
  }
}

Return only the JSON object for the described document."#;
    let url = format!(
        "{}/chat/completions",
        cfg.llm.base_url.trim_end_matches('/')
    );
    let mut last_raw = String::new();
    // A mid-size model occasionally truncates or mis-frames the JSON; retry a
    // couple of times with a "complete, valid JSON" nudge before giving up.
    for attempt in 0..3u32 {
        let user = if attempt == 0 {
            body.description.clone()
        } else {
            format!(
                "{} — Return ONLY a single complete, well-formed JSON object \
                 (no markdown fences, no trailing prose). Do not truncate the \
                 output.",
                body.description
            )
        };
        match client
            .post(&url)
            .bearer_auth(&key)
            .json(&json!({
                "model": cfg.llm.model,
                "temperature": 0.2,
                "max_tokens": 8192,
                "messages": [
                    {"role": "system", "content": sys},
                    {"role": "user", "content": user},
                ],
            }))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                let v: Value = r.json().await.unwrap_or(Value::Null);
                let content = v
                    .get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(d) = extract_json_object(&content) {
                    return (
                        StatusCode::OK,
                        axum::Json(json!({ "ok": true, "definition": d })),
                    )
                        .into_response();
                }
                last_raw = content;
            }
            Ok(r) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    axum::Json(json!({ "ok": false, "error": format!("LLM returned HTTP {}", r.status()) })),
                )
                    .into_response();
            }
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    axum::Json(json!({ "ok": false, "error": format!("LLM request failed: {e}") })),
                )
                    .into_response();
            }
        }
    }
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({
            "ok": false,
            "error": "the model did not return a valid JSON definition after 3 attempts",
            "raw": last_raw,
        })),
    )
        .into_response()
}

/// `POST /api/llm/draft-processor` — ask the configured LLM to draft a
/// subscription (a processor spec: a trigger plus a reaction) from a short
/// description. Returned for review; the console activates it with
/// `POST /api/processors`. Mirrors [`llm_draft_type`].
#[derive(Deserialize)]
struct DraftProcessorBody {
    description: String,
}

async fn llm_draft_processor(
    state: axum::extract::State<Arc<Settings>>,
    axum::extract::Json(body): axum::extract::Json<DraftProcessorBody>,
) -> Response {
    let cfg = config::load(&state.paths).unwrap_or_default();
    let key = std::env::var(&cfg.llm.api_key_env).unwrap_or_default();
    if key.is_empty() {
        return (
            StatusCode::OK,
            axum::Json(json!({
                "ok": false,
                "error": "no LLM API key set (the value of the configured apiKeyEnv); fill in the trigger and reaction by hand",
            })),
        )
            .into_response();
    }
    // The models the LLM can target, each with its fields and reducers, so it
    // writes a trigger that actually matches a real action.
    let mut model_lines = Vec::new();
    for r in state.store.model_refs() {
        let def = state.store.model_definition(&r).unwrap_or(Value::Null);
        let fields = def.get("fields").cloned().unwrap_or(Value::Null);
        let reducers = def
            .get("reducers")
            .and_then(|v| v.as_object())
            .map(|o| o.keys().cloned().collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        model_lines.push(format!(
            "- {} : fields {} ; reducers [{}]",
            r.name, fields, reducers
        ));
    }
    let sys = format!(
        r#"You write "subscriptions" for an event-sourced document store. A subscription watches for a specific change to documents and, when it matches, runs a reaction. Given a short description of what the user wants, return ONE valid JSON object and nothing else: no prose, no markdown fences, no trailing commas.

Schema:
{{
  "name": "lowercase-hyphenated",
  "models": ["<model>", ...],
  "action_kind": "<reducer-kind>",
  "field": "<field-name>",
  "value": <any json value>,
  "reaction": {{ "kind": "<kind>", ... }}
}}

- "models": the document models to watch (choose from the list below). An empty [] means any model.
- "action_kind": the reducer kind that produced the change (e.g. "set-status"). Use null when the description means "any change".
- "field": the field being written (e.g. "status"). Use null to not care.
- "value": the value the field must be written to (e.g. "accepted"). Use null to not care.
- A change matches only when EVERY non-null constraint matches, so a specific model + a set-<field> action_kind + the field + the target value is the most precise trigger.
- "reaction": what to do when it matches. Kinds:
    - {{ "kind": "run", "command": "<shell command>" }}  -- runs locally (15s timeout); the triggering doc name is $PH_DOC and its model is $PH_MODEL, available as shell env vars. Use them in the command.
    - {{ "kind": "log", "message": "<text>" }}          -- appends to the daemon log; $doc and $model are substituted.
    - {{ "kind": "emit", "event": "<name>" }}           -- records a named event for a UI feed.
    - {{ "kind": "create-doc", "model": "<m>", "name": "<n>", "fields": {{ ... }} }}  -- creates a document.

Available models (target real ones; prefer a specific set-<field> reducer so the subscription fires on that exact transition):
{model_lines}

Rules:
- "name" is lowercase-hyphenated, derived from the description.
- Prefer a SPECIFIC, matchable trigger: a real model + its set-<field> action_kind + the field + the target value. Use null/empty for action_kind/field/value only when the description genuinely means "any change to these models".
- The reaction is usually a "run" command: keep it a single, self-contained shell command that uses $PH_DOC and $PH_MODEL where it needs the triggering document. Keep it safe and idempotent.
- Output valid JSON only.

Example, for "when an invoice is accepted, log that it was paid":
{{
  "name": "invoice-accepted",
  "models": ["invoice"],
  "action_kind": "set-status",
  "field": "status",
  "value": "accepted",
  "reaction": {{ "kind": "run", "command": "echo \"invoice $PH_DOC accepted - paid\"" }}
}}

Return only the JSON object for the described subscription."#,
        model_lines = model_lines.join("\n")
    );
    let url = format!(
        "{}/chat/completions",
        cfg.llm.base_url.trim_end_matches('/')
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .build()
        .unwrap_or_default();
    let mut last_raw = String::new();
    for attempt in 0..3u32 {
        let user = if attempt == 0 {
            body.description.clone()
        } else {
            format!(
                "{} — Return ONLY a single complete, well-formed JSON object \
                 (no markdown fences, no trailing prose). Do not truncate the \
                 output.",
                body.description
            )
        };
        match client
            .post(&url)
            .bearer_auth(&key)
            .json(&json!({
                "model": cfg.llm.model,
                "temperature": 0.2,
                "max_tokens": 2048,
                "messages": [
                    {"role": "system", "content": sys},
                    {"role": "user", "content": user},
                ],
            }))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                let v: Value = r.json().await.unwrap_or(Value::Null);
                let content = v
                    .get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(spec) = extract_json_object(&content) {
                    return (
                        StatusCode::OK,
                        axum::Json(json!({ "ok": true, "spec": spec })),
                    )
                        .into_response();
                }
                last_raw = content;
            }
            Ok(r) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    axum::Json(json!({ "ok": false, "error": format!("LLM returned HTTP {}", r.status()) })),
                )
                    .into_response();
            }
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    axum::Json(json!({ "ok": false, "error": format!("LLM request failed: {e}") })),
                )
                    .into_response();
            }
        }
    }
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({
            "ok": false,
            "error": "the model did not return a valid subscription after 3 attempts",
            "raw": last_raw,
        })),
    )
        .into_response()
}

/// Pull the first balanced JSON object out of a model response that may be
/// wrapped in prose or markdown fences.
fn extract_json_object(s: &str) -> Option<Value> {
    let s = s.trim();
    if let Ok(v) = serde_json::from_str::<Value>(s) {
        if v.is_object() {
            return Some(v);
        }
    }
    // Strip a markdown fence if present.
    let un = s
        .strip_prefix("```")
        .map(|rest| rest.strip_prefix("json").unwrap_or(rest));
    let un = un
        .unwrap_or(s)
        .trim_end()
        .strip_suffix("```")
        .unwrap_or(un.unwrap_or(s))
        .trim();
    if let Ok(v) = serde_json::from_str::<Value>(un) {
        if v.is_object() {
            return Some(v);
        }
    }
    // Scan for the first balanced {...} block (string-aware).
    let chars: Vec<char> = s.chars().collect();
    let start = chars.iter().position(|&c| c == '{')?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    for i in start..chars.len() {
        let c = chars[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let block: String = chars[start..=i].iter().collect();
                    return serde_json::from_str::<Value>(&block).ok();
                }
            }
            _ => {}
        }
    }
    None
}

async fn page() -> Html<&'static str> {
    Html(PAGE)
}

/// The redesigned console (v2) - the default landing page.
async fn page_v2() -> Html<&'static str> {
    Html(PAGE_V2)
}

const PAGE: &str = include_str!("console.html");
const PAGE_V2: &str = include_str!("../../console/v2.html");

#[cfg(test)]
mod console_tests {
    use super::PAGE_V2;

    /// The plugin iframe needs BOTH sandbox tokens, for reasons that are easy
    /// to get wrong in opposite directions.
    ///
    /// `allow-same-origin` must stay absent: granting it would put the editor
    /// on the console's origin and hand it the whole unauthenticated API.
    ///
    /// `allow-forms` must stay present: without it Chrome blocks the submit
    /// EVENT, not merely the navigation, so an editor whose form only calls
    /// preventDefault and posts over the bridge silently does nothing. That
    /// failure is invisible -- no exception, no request, just a form that never
    /// fires -- which is exactly how it was found.
    /// Every bridge call carries the space the plugin is open in. Without it
    /// the daemon falls back to node-wide scope and a plugin open in one
    /// client's space reads another client's documents of the same model.
    #[test]
    fn the_bridge_tells_the_daemon_which_space_it_is_in() {
        for endpoint in ["/query", "/action"] {
            let line = PAGE_V2
                .lines()
                .find(|l| l.contains(&format!("/api/plugins/${{encodeURIComponent(name)}}{endpoint}")))
                .unwrap_or_else(|| panic!("the bridge posts to {endpoint}"));
            assert!(
                line.contains("space: currentSpace()"),
                "{endpoint} must carry the space: {line}"
            );
        }
    }

    /// The console must not describe "protected" in a way that reads like a
    /// private Slack channel. Every member holds a full plaintext replica and
    /// keeps it after removal; if the wording ever loses that, someone puts
    /// payroll in one.
    #[test]
    fn the_protected_tier_says_what_it_actually_guarantees() {
        let i = PAGE_V2
            .find("const TIER_MEANING")
            .expect("the console defines the tier wording");
        let block = &PAGE_V2[i..i + 900];
        assert!(
            block.contains("unencrypted"),
            "protected must say the copies are unencrypted: {block}"
        );
        assert!(
            block.contains("keeps what they saw"),
            "protected must say removal is not retroactive: {block}"
        );
        assert!(
            block.contains("no copy of it anywhere else"),
            "private must say it removes the implicit backup: {block}"
        );
    }

    /// A space's tier is fixed at creation, and the form has to say so before
    /// the choice is made rather than after.
    #[test]
    fn the_create_form_warns_that_the_tier_is_permanent() {
        assert!(
            PAGE_V2.contains("This cannot be changed later."),
            "the space creation form must say the tier is permanent"
        );
    }

    #[test]
    fn the_plugin_iframe_sandbox_is_exactly_right() {
        let line = PAGE_V2
            .lines()
            .find(|l| l.contains("sandbox="))
            .expect("the plugin iframe declares a sandbox");
        assert!(
            line.contains("allow-scripts"),
            "an editor without scripts is not an editor: {line}"
        );
        assert!(
            line.contains("allow-forms"),
            "without allow-forms the submit event never fires: {line}"
        );
        assert!(
            !line.contains("allow-same-origin"),
            "allow-same-origin would put the plugin on the console's origin: {line}"
        );
    }

    /// The bridge must identify its caller by window, not by trusting whatever
    /// arrives. A sandboxed frame reports origin "null", so the source check is
    /// the one doing the work.
    #[test]
    fn the_bridge_checks_the_message_source() {
        assert!(
            PAGE_V2.contains("ev.source !== frame.contentWindow"),
            "the bridge must only answer its own frame"
        );
    }

    /// A plugin-supplied label reaches the sidebar, so it must be escaped on
    /// the way in. Validation already refuses control characters and the
    /// console's own names; escaping is what stops the label being markup.
    #[test]
    fn plugin_sidebar_labels_are_escaped() {
        // Search the whole page: `renderPluginNav` appears at its call sites
        // too, so splitting on the name lands in the wrong place.
        let markup = PAGE_V2
            .lines()
            .find(|l| l.contains("data-plugin="))
            .expect("the nav item template");
        for field in ["esc(n.plugin)", "esc(n.view"] {
            assert!(
                markup.contains(field),
                "the nav item template must escape {field}: {markup}"
            );
        }
        let label_line = PAGE_V2
            .lines()
            .find(|l| l.contains("esc(n.label)"))
            .expect("the label is escaped");
        assert!(
            label_line.contains("esc(n.icon"),
            "the icon is escaped alongside the label: {label_line}"
        );
    }

    /// The asset origin is derived, never hardcoded, and is always one port up
    /// from the console -- which is what makes it a different origin.
    #[test]
    fn the_asset_origin_is_the_console_port_plus_one() {
        assert!(
            PAGE_V2.contains("Number(location.port || 80) + 1"),
            "the console must address the asset server at port + 1"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::llm_base_url_ok;

    #[test]
    fn llm_base_url_rejects_link_local_metadata_allows_real_llm() {
        // The cloud metadata IP (AWS/Azure) is in the 169.254.0.0/16 range.
        assert!(llm_base_url_ok("http://169.254.169.254/latest/meta-data/").is_err());
        assert!(llm_base_url_ok("http://169.254.0.1:8080").is_err());
        // A normal LLM endpoint is allowed (an https host, and a local http
        // LLM server like Ollama on loopback).
        assert!(llm_base_url_ok("https://api.openai.com").is_ok());
        assert!(llm_base_url_ok("http://127.0.0.1:11434").is_ok());
        // A non-IP host is allowed (no range to check).
        assert!(llm_base_url_ok("https://llm.example.com/v1").is_ok());
    }
}
