//! Minimal MCP client for the switchboard's `/mcp` endpoint.
//!
//! The switchboard serves the MCP server (reactor-api + reactor-mcp) over
//! the official Streamable HTTP transport in *stateless* mode: every
//! message is a plain `POST /mcp` with a JSON-RPC 2.0 body; responses come
//! back either as `application/json` or as an SSE stream. Local daemons
//! run with auth disabled (OPEN policy), so no token is required; an
//! optional bearer token is sent when configured.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// The protocol version requested during `initialize`. The server may
/// answer with a different (supported) version.
const PROTOCOL_VERSION: &str = "2025-03-26";

#[derive(Debug, Clone)]
pub struct Mcp {
    client: reqwest::Client,
    url: String,
    token: Option<String>,
}

/// One entry of `tools/list`.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
}

/// The result of a `tools/call`.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallResult {
    /// The first text content block (tool outputs are JSON-encoded text).
    pub text: Option<String>,
    /// `structuredContent` when the server provides it (newer protocol
    /// versions), else `None`.
    pub structured: Option<Value>,
    pub is_error: bool,
}

impl Mcp {
    /// Client for a switchboard on `127.0.0.1:<port>`.
    pub fn new(port: u16, token: Option<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("client builds"),
            url: format!("http://127.0.0.1:{port}/mcp"),
            token,
        }
    }

    /// `initialize` + `notifications/initialized`; returns the negotiated
    /// server protocol version.
    pub async fn session(&self) -> Result<String> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "ph-reactor",
                "version": env!("CARGO_PKG_VERSION"),
            },
        });
        let res = self.rpc("initialize", Some(params)).await?;
        let version = res
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        self.notification("notifications/initialized").await?;
        Ok(version)
    }

    /// `tools/list` (first page; the switchboard's tool set fits one page).
    pub async fn tools_list(&self) -> Result<Vec<ToolInfo>> {
        let res = self.rpc("tools/list", Some(json!({}))).await?;
        let tools = res
            .get("tools")
            .and_then(|t| t.as_array())
            .context("tools/list response has no tools array")?;
        Ok(tools
            .iter()
            .map(|t| ToolInfo {
                name: t
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .into(),
                description: t
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .into(),
            })
            .collect())
    }

    /// `tools/call` by name with a JSON arguments object.
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<ToolCallResult> {
        let res = self
            .rpc(
                "tools/call",
                Some(json!({ "name": name, "arguments": args })),
            )
            .await?;
        Ok(ToolCallResult {
            text: res
                .get("content")
                .and_then(|c| c.as_array())
                .and_then(|arr| {
                    arr.iter()
                        .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                })
                .and_then(|b| b.get("text").and_then(|t| t.as_str()))
                .map(str::to_string),
            structured: res.get("structuredContent").cloned(),
            is_error: res
                .get("isError")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        })
    }

    /// `addRemoteDrive` tool: connects the local reactor to a remote drive.
    /// Resolves with the added drive's id.
    pub async fn add_remote_drive(&self, url: &str, available_offline: bool) -> Result<String> {
        let result = self
            .call_tool(
                "addRemoteDrive",
                json!({
                    "url": url,
                    "options": { "availableOffline": available_offline },
                }),
            )
            .await?;
        if result.is_error {
            bail!(
                "addRemoteDrive failed: {}",
                result.text.unwrap_or_else(|| "no detail".into())
            );
        }
        let mut drive_id: Option<String> = None;
        if let Some(Value::Object(map)) = &result.structured {
            if let Some(Value::String(s)) = map.get("driveId") {
                drive_id = Some(s.clone());
            }
        }
        if drive_id.is_none() {
            if let Some(text) = &result.text {
                if let Ok(Value::Object(map)) = serde_json::from_str(text) {
                    if let Some(Value::String(s)) = map.get("driveId") {
                        drive_id = Some(s.clone());
                    }
                }
            }
        }
        let drive_id = drive_id.context("addRemoteDrive response has no driveId")?;
        Ok(drive_id)
    }

    // -- wire helpers ------------------------------------------------------

    async fn rpc(&self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = next_id();
        let body = match params {
            Some(p) => json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": p }),
            None => json!({ "jsonrpc": "2.0", "id": id, "method": method }),
        };
        let raw = self.post(body).await?;
        let msg = parse_jsonrpc_message(&raw).with_context(|| format!("{method} response"))?;
        if let Some(err) = msg.get("error") {
            let message = err
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            bail!("MCP {method}: {message}");
        }
        Ok(msg.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notification(&self, method: &str) -> Result<()> {
        let body = json!({ "jsonrpc": "2.0", "method": method });
        let _ = self.post(body).await?;
        Ok(())
    }

    async fn post(&self, body: Value) -> Result<String> {
        let mut req = self
            .client
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .json(&body);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("POST {url}", url = self.url))?;
        let status = resp.status();
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let text = resp.text().await?;
        if !status.is_success() {
            bail!(
                "MCP HTTP {status}: {}",
                text.chars().take(300).collect::<String>()
            );
        }
        Ok(match parse_kind(&ctype) {
            ResponseKind::Json => text,
            ResponseKind::Sse => extract_sse_payload(&text)?,
        })
    }
}

static ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_id() -> u64 {
    ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseKind {
    Json,
    Sse,
}

fn parse_kind(content_type: &str) -> ResponseKind {
    if content_type.contains("text/event-stream") {
        ResponseKind::Sse
    } else {
        ResponseKind::Json
    }
}

/// Parses a JSON-RPC message body (already JSON text).
fn parse_jsonrpc_message(raw: &str) -> Result<Value> {
    serde_json::from_str(raw).with_context(|| format!("not JSON: {raw}"))
}

/// Extracts the JSON-RPC payload from an SSE body: the last `data:` line
/// carrying an object with an `id` (keeps the response, not pings or
/// session events).
fn extract_sse_payload(body: &str) -> Result<String> {
    let mut found: Option<String> = None;
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(data) {
            if v.get("id").is_some() {
                found = Some(data.to_string());
            }
        }
    }
    found.context("SSE stream contained no JSON-RPC response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::StatusCode, response::Response, routing::post, Router};
    use serde_json::json;
    use tokio::net::TcpListener;

    /// A mock of the switchboard's /mcp endpoint: validates the MCP
    /// handshake shape and answers tools/call(addRemoteDrive) over SSE.
    fn mock_router() -> Router {
        let handler = |req: axum::extract::Json<Value>| async move {
            let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let id = req.get("id").cloned().unwrap_or(Value::Null);
            match method {
                "initialize" => {
                    assert_eq!(
                        req.get("params")
                            .and_then(|p| p.get("protocolVersion"))
                            .and_then(|v| v.as_str()),
                        Some(PROTOCOL_VERSION)
                    );
                    json_resp(
                        StatusCode::OK,
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": "2025-03-26",
                                "capabilities": {"tools": {}},
                                "serverInfo": {"name": "mock-switchboard", "version": "1"},
                            },
                        }),
                    )
                }
                "notifications/initialized" => Response::builder()
                    .status(StatusCode::ACCEPTED)
                    .body(Body::empty())
                    .unwrap(),
                "tools/list" => json_resp(
                    StatusCode::OK,
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [
                                {"name": "addRemoteDrive", "description": "Connect to a remote drive"},
                                {"name": "getDrives", "description": "List drives"},
                            ],
                        },
                    }),
                ),
                "tools/call" => {
                    let name = req
                        .get("params")
                        .and_then(|p| p.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if name != "addRemoteDrive" {
                        return json_resp(
                            StatusCode::OK,
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {"code": -32602, "message": format!("Unknown tool: {name}")},
                            }),
                        );
                    }
                    let sse = format!(
                        "event: ping\r\ndata: {}\r\n\r\nevent: message\r\ndata: {}\r\n\r\n",
                        json!({}),
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{"type": "text", "text": json!({"driveId": "drt-42"}).to_string()}],
                                "structuredContent": {"driveId": "drt-42"},
                            },
                        }),
                    );
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/event-stream")
                        .body(Body::from(sse))
                        .unwrap()
                }
                other => json_resp(
                    StatusCode::BAD_REQUEST,
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32601, "message": format!("unknown method {other}")},
                    }),
                ),
            }
        };
        Router::new().route("/mcp", post(handler))
    }

    fn json_resp(status: StatusCode, v: Value) -> Response {
        Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap()
    }

    async fn start_mock() -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, mock_router()).await;
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn full_roundtrip_against_mock() {
        let (base, _keep) = start_mock().await;
        // Mcp::new binds 127.0.0.1:port — derive the port from the mock.
        let port: u16 = base
            .split_once("://")
            .unwrap()
            .1
            .split_once(':')
            .unwrap()
            .1
            .parse()
            .unwrap();
        let mcp = Mcp::new(port, None);
        let version = mcp.session().await.unwrap();
        assert_eq!(version, "2025-03-26");

        let tools = mcp.tools_list().await.unwrap();
        assert_eq!(tools[0].name, "addRemoteDrive");
        assert_eq!(tools.len(), 2);

        let drive_id = mcp
            .add_remote_drive("https://example.com/d/demo", true)
            .await
            .unwrap();
        assert_eq!(drive_id, "drt-42");
    }

    #[test]
    fn sse_extraction_ignores_pings_and_picks_last() {
        let body = "event: ping\ndata: {}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"a\":1}}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"a\":2}}\n\n";
        let out = extract_sse_payload(body).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["result"]["a"], 2);
        assert!(extract_sse_payload("event: ping\ndata: {}\n\n").is_err());
    }

    #[test]
    fn kind_detection() {
        assert_eq!(
            parse_kind("text/event-stream; charset=utf-8"),
            ResponseKind::Sse
        );
        assert_eq!(parse_kind("application/json"), ResponseKind::Json);
    }

    #[tokio::test]
    async fn jsonrpc_error_surfaces() {
        let (base, _keep) = start_mock().await;
        let port: u16 = base
            .split_once("://")
            .unwrap()
            .1
            .split_once(':')
            .unwrap()
            .1
            .parse()
            .unwrap();
        let mcp = Mcp::new(port, None);
        let _ = mcp.session().await.unwrap();
        let err = mcp.call_tool("nope", json!({})).await.unwrap_err();
        assert!(err.to_string().contains("Unknown tool"), "{err}");
    }
}
