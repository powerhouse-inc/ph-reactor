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
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};

use crate::commands::Command;
use crate::status::StatusSnapshot;

pub struct Settings {
    cmd_tx: mpsc::UnboundedSender<Command>,
    snap_rx: watch::Receiver<StatusSnapshot>,
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
    ) -> Self {
        Self { cmd_tx, snap_rx }
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
            .route("/api/config", post(set_config))
            .route("/api/quit", post(quit))
            .route("/api/docs", post(add_doc))
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
// ---------------------------------------------------------------------------
// Single-page UI
// ---------------------------------------------------------------------------

async fn page() -> &'static str {
    PAGE
}

const PAGE: &str = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>ph-reactor</title>
<style>
  :root { color-scheme: dark; --bg:#101418; --panel:#1a2129; --line:#2a3441;
          --fg:#dce3ea; --dim:#8b98a5; --ok:#4caf7d; --warn:#d9a441; --err:#d96a5f; }
  body { margin:0; font:14px/1.5 system-ui, sans-serif; background:var(--bg); color:var(--fg); }
  header { padding:16px 24px; border-bottom:1px solid var(--line); }
  h1 { font-size:16px; margin:0 0 4px; font-weight:600; }
  .sub { color:var(--dim); font-size:12px; }
  main { padding:24px; max-width:960px; }
  .reactor { background:var(--panel); border:1px solid var(--line); border-radius:8px;
             padding:12px 16px; margin-bottom:24px; }
  .reactor .state { font-weight:600; }
  .chip { display:inline-block; padding:1px 8px; border-radius:10px; font-size:12px;
          background:var(--line); color:var(--dim); }
  .chip.synced, .chip.ok { background:rgba(76,175,125,.15); color:var(--ok); }
  .chip.error, .chip.paused, .chip.requires-auth { background:rgba(217,106,95,.15); color:var(--err); }
  .chip.offline, .chip.connecting { background:rgba(217,164,65,.15); color:var(--warn); }
  table { width:100%; border-collapse:collapse; }
  th, td { text-align:left; padding:8px 10px; border-bottom:1px solid var(--line); vertical-align:top; }
  th { color:var(--dim); font-weight:500; font-size:12px; text-transform:uppercase; letter-spacing:.04em; }
  td.addr { color:var(--dim); font-size:12px; word-break:break-all; max-width:340px; }
  td.detail { color:var(--dim); font-size:12px; max-width:300px; }
  button { background:var(--panel); color:var(--fg); border:1px solid var(--line);
           border-radius:6px; padding:3px 10px; margin:0 4px 0 0; cursor:pointer; font-size:12px; }
  button:hover { border-color:var(--dim); }
  button.danger { color:var(--err); }
  form.add { display:flex; gap:8px; flex-wrap:wrap; margin-top:8px; }
  input { background:var(--panel); border:1px solid var(--line); border-radius:6px;
          color:var(--fg); padding:6px 10px; font-size:13px; }
  input[name=addr] { flex:1; min-width:280px; }
  .hint { color:var(--dim); font-size:12px; margin-top:12px; }
  .toast { position:fixed; bottom:16px; left:16px; background:var(--panel);
           border:1px solid var(--line); border-radius:8px; padding:8px 14px;
           font-size:13px; opacity:0; transition:opacity .3s; pointer-events:none; }
  .toast.show { opacity:1; }
</style>
</head>
<body>
<header>
  <h1>ph-reactor <span class="chip" id="ver"></span></h1>
  <div class="sub" id="settingsline"></div>
</header>
<main>
  <div class="reactor">
    <span class="state" id="rxstate">…</span>
    <span class="sub" id="rxdetail"></span>
  </div>

  <h2 style="font-size:14px; margin:0 0 8px;">Drives</h2>
  <table>
    <thead><tr><th>Name</th><th>Address</th><th>Status</th><th></th></tr></thead>
    <tbody id="drives"></tbody>
  </table>

  <h2 style="font-size:14px; margin:24px 0 8px;">Add a drive</h2>
  <form class="add" id="addform">
    <input name="addr" placeholder="/ip4/10.0.0.2/tcp/4201/p2p/12D3Koo…" required>
    <input name="name" placeholder="name (optional)">
    <input name="tokenEnv" placeholder="token env (optional)" style="width:150px">
    <label class="sub" style="align-self:center;"><input type="checkbox" name="offline"> offline</label>
    <button type="submit">Add</button>
  </form>
  <div class="hint">
    The address is a multiaddr of the remote ph-reactor (the peer part is
    optional — it is resolved on connect). Tokens are read from the named
    environment variable; they are never stored. Pausing stops syncing but
    keeps the local docs.
  </div>
</main>
<div class="toast" id="toast"></div>
<script>
const $ = (id) => document.getElementById(id);
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => (
  {"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[c]));

async function get(url) {
  const res = await fetch(url, { headers: { "Accept": "application/json" } });
  if (!res.ok) throw new Error(url + " -> " + res.status);
  return res.json();
}
async function post(url, body) {
  const res = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!res.ok) throw new Error(url + " -> " + res.status);
  return res.status;
}

let toastTimer = 0;
function toast(msg) {
  const el = $("toast");
  el.textContent = msg;
  el.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove("show"), 3500);
}

function renderDrive(d) {
  const tr = document.createElement("tr");
  const actions = [
    ["resync", "resync", post(`/api/drives/${encodeURIComponent(d.name)}/resync`)],
    d.paused ? ["resume", "resume", post(`/api/drives/${encodeURIComponent(d.name)}/resume`)]
             : ["pause", "pause", post(`/api/drives/${encodeURIComponent(d.name)}/pause`)],
    ["remove", "remove", post(`/api/drives/${encodeURIComponent(d.name)}`, { method: "DELETE" })],
  ];
  tr.innerHTML =
    `<td>${esc(d.name)}</td>` +
    `<td class="addr">${esc(d.addr)}</td>` +
    `<td><span class="chip ${esc(d.status)}">${esc(d.status)}</span>` +
    `<div class="detail">${esc(d.detail)}</div></td>` +
    `<td>${actions
      .map(([k, label, fn]) =>
        `<button class="${k === "remove" ? "danger" : ""}" data-a="${k}">${label}</button>`)
      .join("")}</td>`;
  tr.querySelectorAll("button").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const [k, , fn] = actions.find(([x]) => x === btn.dataset.a);
      try { await fn(); toast(k + " " + d.name); await refresh(); }
      catch (e) { toast(String(e)); }
    });
  });
  return tr;
}

async function refresh() {
  try {
    const s = await get("/api/status");
    $("ver").textContent = s.version;
    $("settingsline").textContent =
      "settings: " + s.settings.url + "  ·  updated " + s.updated_at;
    const r = s.reactor;
    const state = !r.running ? "stopped" : r.healthy ? "healthy" : "starting";
    $("rxstate").textContent = "reactor: " + state;
    $("rxstate").className = "state";
    $("rxdetail").textContent =
      `peer ${r.peer_id ?? "?"} · listen ${r.listen} · ${r.docs} docs` +
      (r.last_event ? ` · ${r.last_event}` : "");
    const tbody = $("drives");
    tbody.innerHTML = "";
    for (const d of s.drives) tbody.appendChild(renderDrive(d));
    if (!s.drives.length) {
      tbody.innerHTML =
        `<tr><td colspan="4" class="detail">no drives configured</td></tr>`;
    }
  } catch (e) {
    $("rxstate").textContent = "daemon unreachable";
  }
}

$("addform").addEventListener("submit", async (e) => {
  e.preventDefault();
  const f = new FormData(e.target);
  try {
    await post("/api/drives", {
      addr: f.get("addr"),
      name: f.get("name") || "",
      tokenEnv: f.get("tokenEnv") || null,
      availableOffline: f.get("offline") === "on",
    });
    e.target.reset();
    toast("drive added");
    await refresh();
  } catch (err) { toast(String(err)); }
});

refresh();
setInterval(refresh, 5000);
</script>
</body>
</html>
"#;
