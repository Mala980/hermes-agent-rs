//! A2A (Agent-to-Agent) platform adapter.
//!
//! Serves the A2A v1.0 protocol over plain HTTP with manual request parsing
//! (no extra web-framework dependency), mirroring the original Python adapter:
//!
//! * `POST /` — JSON-RPC 2.0 dispatch. `SendMessage` / `message/send` requests
//!   are converted into inbound gateway messages; the agent's reply is
//!   returned as the JSON-RPC result wrapping a completed A2A Task.
//! * `GET /.well-known/agent.json` and `/.well-known/agent-card.json` — the
//!   A2A Agent Card describing this agent.
//! * `GET /health` — liveness probe.
//!
//! Security posture: requests are fail-closed (malformed JSON-RPC yields a
//! JSON-RPC error, never a panic), request bodies are capped at 1 MiB, and
//! when no bearer token is configured the server is forced to bind loopback
//! only. Streaming (`message/stream`) and the task/push-config RPCs are not
//! implemented in v1 and return JSON-RPC method-not-found.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Mutex, Notify, RwLock};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use hermes_core::errors::GatewayError;
use hermes_core::traits::{ParseMode, PlatformAdapter};

use crate::adapter::BasePlatformAdapter;
use crate::gateway::IncomingMessage;

// ---------------------------------------------------------------------------
// Protocol constants (A2A v1.0)
// ---------------------------------------------------------------------------

/// A2A protocol version served by this adapter.
pub const A2A_PROTOCOL_VERSION: &str = "1.0";

// JSON-RPC error codes. -32001/-32002 are A2A-spec; -32050+ is the
// implementation-defined block, kept clear of the A2A range.
pub const ERR_PARSE: i64 = -32700;
pub const ERR_INVALID_REQUEST: i64 = -32600;
pub const ERR_METHOD_NOT_FOUND: i64 = -32601;
pub const ERR_INVALID_PARAMS: i64 = -32602;
pub const ERR_INTERNAL: i64 = -32603;
pub const ERR_UNAUTHORIZED: i64 = -32050;

// Task lifecycle states (SCREAMING_SNAKE_CASE per A2A v1.0).
pub const STATE_SUBMITTED: &str = "TASK_STATE_SUBMITTED";
pub const STATE_WORKING: &str = "TASK_STATE_WORKING";
pub const STATE_INPUT_REQUIRED: &str = "TASK_STATE_INPUT_REQUIRED";
pub const STATE_COMPLETED: &str = "TASK_STATE_COMPLETED";
pub const STATE_FAILED: &str = "TASK_STATE_FAILED";
pub const STATE_CANCELED: &str = "TASK_STATE_CANCELED";

pub const ROLE_USER: &str = "ROLE_USER";
pub const ROLE_AGENT: &str = "ROLE_AGENT";

/// A reply starting with this marker is a clarification request: the task
/// moves to TASK_STATE_INPUT_REQUIRED and the marker is stripped.
pub const INPUT_REQUIRED_MARKER: &str = "[INPUT_REQUIRED]";

/// Max request body: 1 MiB (DoS guard, same as the original adapter).
pub const MAX_BODY_BYTES: usize = 1_048_576;

fn default_bind_addr() -> String {
    "127.0.0.1".to_string()
}
fn default_a2a_port() -> u16 {
    9900
}
fn default_reply_timeout_secs() -> u64 {
    300
}
fn default_agent_name() -> String {
    "hermes-agent".to_string()
}
fn default_agent_description() -> String {
    "Hermes Agent \u{2014} a general-purpose agent reachable over A2A.".to_string()
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the A2A adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2aConfig {
    /// Interface to bind (default loopback).
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    /// TCP port to listen on.
    #[serde(default = "default_a2a_port")]
    pub port: u16,
    /// Bearer token required on every request. Empty = no auth, in which
    /// case the server is forced onto loopback (fail-closed).
    #[serde(default)]
    pub bearer_token: String,
    /// Agent name advertised on the Agent Card.
    #[serde(default = "default_agent_name")]
    pub agent_name: String,
    /// Agent description advertised on the Agent Card.
    #[serde(default = "default_agent_description")]
    pub description: String,
    /// Seconds to wait for the agent to answer a `message/send` request.
    #[serde(default = "default_reply_timeout_secs")]
    pub reply_timeout_secs: u64,
}

impl Default for A2aConfig {
    fn default() -> Self {
        Self {
            bind_addr: default_bind_addr(),
            port: default_a2a_port(),
            bearer_token: String::new(),
            agent_name: default_agent_name(),
            description: default_agent_description(),
            reply_timeout_secs: default_reply_timeout_secs(),
        }
    }
}

impl A2aConfig {
    /// Build from a generic [`hermes_config::platform::PlatformConfig`],
    /// reading adapter-specific keys from `extra`.
    pub fn from_platform_config(cfg: &hermes_config::platform::PlatformConfig) -> Self {
        let mut out = Self::default();
        if let Some(v) = hermes_config::extra_string(cfg, "bind_addr") {
            out.bind_addr = v;
        }
        if let Some(v) = cfg.extra.get("port").and_then(Value::as_u64) {
            out.port = v.min(u16::MAX as u64) as u16;
        }
        if let Some(v) = hermes_config::extra_string(cfg, "bearer_token") {
            out.bearer_token = v;
        }
        if let Some(v) = hermes_config::extra_string(cfg, "agent_name") {
            out.agent_name = v;
        }
        if let Some(v) = hermes_config::extra_string(cfg, "description") {
            out.description = v;
        }
        if let Some(v) = cfg.extra.get("reply_timeout_secs").and_then(Value::as_u64) {
            out.reply_timeout_secs = v.max(1);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC helpers (pure; unit-tested)
// ---------------------------------------------------------------------------

/// A validated JSON-RPC 2.0 request.
#[derive(Debug, Clone)]
pub struct ParsedRequest {
    /// Request id; `None` for notifications (which get no response).
    pub id: Option<Value>,
    pub method: String,
    pub params: Value,
}

/// A JSON-RPC error ready to be serialized into an error response.
#[derive(Debug, Clone)]
pub struct RpcError {
    pub id: Option<Value>,
    pub code: i64,
    pub message: String,
}

/// Validate the JSON-RPC 2.0 envelope of a raw request body (fail-closed).
pub fn parse_jsonrpc_request(body: &[u8]) -> Result<ParsedRequest, RpcError> {
    let value: Value = serde_json::from_slice(body).map_err(|e| RpcError {
        id: None,
        code: ERR_PARSE,
        message: format!("invalid JSON: {e}"),
    })?;
    let obj = value.as_object().ok_or_else(|| RpcError {
        id: None,
        code: ERR_INVALID_REQUEST,
        message: "request must be a JSON object".to_string(),
    })?;
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(RpcError {
            id: obj.get("id").cloned(),
            code: ERR_INVALID_REQUEST,
            message: "jsonrpc must be \"2.0\"".to_string(),
        });
    }
    let method = obj
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError {
            id: obj.get("id").cloned(),
            code: ERR_INVALID_REQUEST,
            message: "missing \"method\"".to_string(),
        })?
        .to_string();
    Ok(ParsedRequest {
        id: obj.get("id").cloned(),
        method,
        params: obj.get("params").cloned().unwrap_or(Value::Null),
    })
}

/// Build a JSON-RPC 2.0 success envelope.
pub fn jsonrpc_result(id: Option<Value>, result: Value) -> Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// Build a JSON-RPC 2.0 error envelope.
pub fn jsonrpc_error(id: Option<Value>, code: i64, message: impl Into<String>) -> Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

impl RpcError {
    /// Serialize into a JSON-RPC error response value.
    pub fn into_response(self) -> Value {
        jsonrpc_error(self.id, self.code, self.message)
    }
}

// ---------------------------------------------------------------------------
// A2A message helpers (pure; unit-tested)
// ---------------------------------------------------------------------------

fn short_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        &Uuid::new_v4().to_string().replace('-', "")[..16]
    )
}

/// New task id (`task-<16 hex>`).
pub fn new_task_id() -> String {
    short_id("task")
}

/// New context id (`ctx-<16 hex>`).
pub fn new_context_id() -> String {
    short_id("ctx")
}

fn now_iso() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// A v1.0 text Part (member-presence discriminated, no `kind`).
pub fn text_part(text: &str) -> Value {
    serde_json::json!({"text": text, "mediaType": "text/plain"})
}

/// A v1.0 Message with a single text Part.
pub fn text_message(role: &str, text: &str, context_id: &str) -> Value {
    let mut msg = serde_json::json!({
        "role": role,
        "parts": [text_part(text)],
        "messageId": short_id("msg"),
    });
    if !context_id.is_empty() {
        msg["contextId"] = Value::String(context_id.to_string());
    }
    msg
}

/// Concatenated text from an A2A Message payload. Text Parts contribute
/// their text; file Parts render as a `[file: name] url` note (raw base64
/// is noted, not decoded); data Parts render their JSON.
pub fn extract_text(message: &Value) -> String {
    let mut chunks: Vec<String> = Vec::new();
    if let Some(parts) = message.get("parts").and_then(Value::as_array) {
        for part in parts {
            let Some(p) = part.as_object() else {
                continue;
            };
            if let Some(t) = p.get("text").and_then(Value::as_str) {
                chunks.push(t.to_string());
            } else if let Some(url) = p.get("url").and_then(Value::as_str) {
                if url.is_empty() {
                    continue;
                }
                let name = p
                    .get("filename")
                    .or_else(|| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let label = if name.is_empty() {
                    "[file]".to_string()
                } else {
                    format!("[file: {name}]")
                };
                chunks.push(format!("{label} {url}"));
            } else if let Some(raw) = p.get("raw").and_then(Value::as_str) {
                chunks.push(format!("[file: {} bytes base64-encoded]", raw.len()));
            } else if let Some(data) = p.get("data") {
                let rendered = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string());
                chunks.push(format!("[data]\n{rendered}"));
            }
        }
    }
    chunks.join("\n").trim().to_string()
}

/// v1.0 puts `messageId` inside the Message; a peer retrying a send repeats it.
pub fn extract_message_id(params: &Value) -> String {
    params
        .get("message")
        .and_then(|m| m.get("messageId"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// v1.0 puts `contextId` inside the Message; tolerate a legacy top-level one.
pub fn extract_context_id(params: &Value) -> String {
    let from_message = params
        .get("message")
        .and_then(|m| m.get("contextId"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if !from_message.is_empty() {
        return from_message.to_string();
    }
    params
        .get("contextId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Build an A2A v1.0 Task. A non-empty `agent_text` attaches an agent
/// message to the status and, for completed tasks, an artifact.
pub fn build_task(task_id: &str, context_id: &str, state: &str, agent_text: &str) -> Value {
    let mut status = serde_json::json!({"state": state, "timestamp": now_iso()});
    if !agent_text.is_empty() {
        status["message"] = text_message(ROLE_AGENT, agent_text, context_id);
    }
    let mut task = serde_json::json!({
        "id": task_id,
        "contextId": context_id,
        "status": status,
    });
    if state == STATE_COMPLETED && !agent_text.is_empty() {
        task["artifacts"] = serde_json::json!([
            {"artifactId": short_id("artifact"), "parts": [text_part(agent_text)]}
        ]);
    }
    task
}

/// Build the A2A v1.0 Agent Card for this server.
pub fn build_agent_card(config: &A2aConfig) -> Value {
    let url = format!("http://{}:{}/", config.bind_addr, config.port);
    let mut card = serde_json::json!({
        "name": config.agent_name,
        "description": config.description,
        "url": url,
        "version": "1.0.0",
        "protocolVersion": A2A_PROTOCOL_VERSION,
        "provider": {"organization": "Hermes Agent", "url": url},
        "supportedInterfaces": [
            {"url": url, "protocolBinding": "JSONRPC", "protocolVersion": A2A_PROTOCOL_VERSION}
        ],
        "capabilities": {
            "streaming": false,
            "pushNotifications": false,
            "stateTransitionHistory": false,
        },
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "skills": [],
    });
    if !config.bearer_token.is_empty() {
        card["securitySchemes"] =
            serde_json::json!({"bearer": {"type": "http", "scheme": "bearer"}});
        card["security"] = serde_json::json!([{"bearer": []}]);
    }
    card
}

/// Split an agent reply into a task state and final text: a leading
/// `[INPUT_REQUIRED]` marker flags a clarification request.
pub fn finalize_reply(reply: &str) -> (&'static str, String) {
    let stripped = reply.trim_start();
    if stripped.to_uppercase().starts_with(INPUT_REQUIRED_MARKER) {
        let rest = stripped[INPUT_REQUIRED_MARKER.len()..].trim().to_string();
        (STATE_INPUT_REQUIRED, rest)
    } else {
        (STATE_COMPLETED, reply.to_string())
    }
}

/// Constant-time byte comparison (avoids pulling in a crypto crate for one check).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Check the `Authorization: Bearer <token>` header. No configured token
/// means no auth is required.
pub fn bearer_authorized(headers: &HashMap<String, String>, expected_token: &str) -> bool {
    if expected_token.is_empty() {
        return true;
    }
    let got = headers
        .get("authorization")
        .map(String::as_str)
        .unwrap_or("");
    let want = format!("Bearer {expected_token}");
    constant_time_eq(got.as_bytes(), want.as_bytes())
}

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

/// Shared state for the HTTP connection handlers.
#[derive(Clone)]
struct Shared {
    config: A2aConfig,
    inbound_tx: Arc<RwLock<Option<mpsc::Sender<IncomingMessage>>>>,
    /// Pending `message/send` requests keyed by A2A context id, fulfilled by
    /// [`A2aAdapter::send_message`] when the gateway delivers the reply.
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
}

/// A2A platform adapter: an HTTP server speaking A2A v1.0 JSON-RPC.
pub struct A2aAdapter {
    base: BasePlatformAdapter,
    config: A2aConfig,
    stop_signal: Arc<Notify>,
    shutdown_tx: RwLock<Option<oneshot::Sender<()>>>,
    inbound_tx: Arc<RwLock<Option<mpsc::Sender<IncomingMessage>>>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
}

impl A2aAdapter {
    pub fn new(config: A2aConfig) -> Self {
        let base = BasePlatformAdapter::new("");
        Self {
            base,
            config,
            stop_signal: Arc::new(Notify::new()),
            shutdown_tx: RwLock::new(None),
            inbound_tx: Arc::new(RwLock::new(None)),
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn config(&self) -> &A2aConfig {
        &self.config
    }

    /// Set the channel used to forward inbound A2A messages to the gateway.
    pub async fn set_inbound_sender(&self, tx: mpsc::Sender<IncomingMessage>) {
        *self.inbound_tx.write().await = Some(tx);
    }

    fn shared(&self) -> Shared {
        Shared {
            config: self.config.clone(),
            inbound_tx: self.inbound_tx.clone(),
            pending: self.pending.clone(),
        }
    }
}

#[async_trait]
impl PlatformAdapter for A2aAdapter {
    async fn start(&self) -> Result<(), GatewayError> {
        // Fail-closed bind: with no bearer token, never listen on a
        // non-loopback interface, whatever the config says.
        let mut bind_addr = self.config.bind_addr.clone();
        if self.config.bearer_token.is_empty()
            && bind_addr != "127.0.0.1"
            && bind_addr != "localhost"
            && bind_addr != "::1"
        {
            warn!("A2A: no bearer token configured; forcing loopback bind (was {bind_addr})");
            bind_addr = "127.0.0.1".to_string();
        }
        let addr: SocketAddr = format!("{}:{}", bind_addr, self.config.port)
            .parse()
            .map_err(|e| GatewayError::ConnectionFailed(format!("A2A: invalid address: {e}")))?;

        info!(
            "A2A adapter starting on {addr} (auth: {})",
            if self.config.bearer_token.is_empty() {
                "none (loopback only)"
            } else {
                "bearer token"
            }
        );

        let shared = self.shared();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        *self.shutdown_tx.write().await = Some(shutdown_tx);

        tokio::spawn(async move {
            let listener = match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => l,
                Err(e) => {
                    error!("A2A server failed to bind {addr}: {e}");
                    return;
                }
            };
            info!("A2A server listening on {addr}");
            let mut shutdown_rx = shutdown_rx;
            loop {
                tokio::select! {
                    accept = listener.accept() => {
                        match accept {
                            Ok((stream, peer)) => {
                                let shared = shared.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = handle_connection(stream, peer, shared).await {
                                        debug!("A2A connection error from {peer}: {e}");
                                    }
                                });
                            }
                            Err(e) => warn!("A2A accept error: {e}"),
                        }
                    }
                    _ = &mut shutdown_rx => {
                        info!("A2A server shutting down");
                        break;
                    }
                }
            }
        });

        self.base.mark_running();
        Ok(())
    }

    async fn stop(&self) -> Result<(), GatewayError> {
        info!("A2A adapter stopping");
        if let Some(tx) = self.shutdown_tx.write().await.take() {
            let _ = tx.send(());
        }
        // Fail any still-pending requests so handlers do not hang.
        let mut pending = self.pending.lock().await;
        pending.clear();
        drop(pending);
        self.base.mark_stopped();
        self.stop_signal.notify_one();
        Ok(())
    }

    async fn send_message(
        &self,
        chat_id: &str,
        text: &str,
        _parse_mode: Option<ParseMode>,
    ) -> Result<(), GatewayError> {
        if let Some(tx) = self.pending.lock().await.remove(chat_id) {
            let _ = tx.send(text.to_string());
            debug!(chat_id, "A2A: fulfilled pending message/send request");
        } else {
            debug!(
                chat_id,
                chars = text.len(),
                "A2A: no pending request for context; outbound text dropped"
            );
        }
        Ok(())
    }

    async fn edit_message(
        &self,
        chat_id: &str,
        _message_id: &str,
        text: &str,
    ) -> Result<(), GatewayError> {
        // v1 has no streaming: edits have no live request to update.
        debug!(
            chat_id,
            chars = text.len(),
            "A2A: edit_message is a no-op without streaming"
        );
        Ok(())
    }

    async fn send_file(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<(), GatewayError> {
        let text = match caption {
            Some(cap) => format!("[file:{file_path}] {cap}"),
            None => format!("[file:{file_path}]"),
        };
        self.send_message(chat_id, &text, None).await
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }

    fn platform_name(&self) -> &str {
        "a2a"
    }
}

// ---------------------------------------------------------------------------
// Minimal HTTP/1.1 handling (manual parsing, like webhook.rs)
// ---------------------------------------------------------------------------

struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

async fn read_http_request(
    reader: &mut (impl AsyncReadExt + Unpin),
) -> Result<HttpRequest, String> {
    let mut head: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = reader
            .read(&mut chunk)
            .await
            .map_err(|e| format!("read error: {e}"))?;
        if n == 0 {
            return Err("connection closed".to_string());
        }
        head.extend_from_slice(&chunk[..n]);
        if head.len() > 65_536 {
            return Err("headers too large".to_string());
        }
        if let Some(pos) = find_header_end(&head) {
            let header_text = String::from_utf8_lossy(&head[..pos]).to_string();
            let mut body = head[pos + 4..].to_vec();

            let mut lines = header_text.lines();
            let request_line = lines.next().unwrap_or("");
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let raw_path = parts.next().unwrap_or("/");
            let path = raw_path.split(['?', '#']).next().unwrap_or("/").to_string();

            let mut headers = HashMap::new();
            for line in lines {
                if let Some((k, v)) = line.split_once(':') {
                    headers.insert(k.trim().to_lowercase(), v.trim().to_string());
                }
            }

            let content_length: usize = headers
                .get("content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            if content_length > MAX_BODY_BYTES {
                return Err("body too large".to_string());
            }
            while body.len() < content_length {
                let n = reader
                    .read(&mut chunk)
                    .await
                    .map_err(|e| format!("read error: {e}"))?;
                if n == 0 {
                    return Err("connection closed mid-body".to_string());
                }
                body.extend_from_slice(&chunk[..n]);
                if body.len() > MAX_BODY_BYTES {
                    return Err("body too large".to_string());
                }
            }
            body.truncate(content_length);
            return Ok(HttpRequest {
                method,
                path,
                headers,
                body,
            });
        }
    }
}

async fn write_response(
    writer: &mut (impl AsyncWriteExt + Unpin),
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    writer.write_all(head.as_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await
}

fn json_body(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec())
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    shared: Shared,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (mut reader, mut writer) = stream.into_split();
    let req = match read_http_request(&mut reader).await {
        Ok(r) => r,
        Err(e) => {
            debug!("A2A: bad HTTP request from {peer}: {e}");
            write_response(&mut writer, "400 Bad Request", "text/plain", b"bad request").await?;
            return Ok(());
        }
    };

    match req.method.as_str() {
        "GET" => handle_get(&mut writer, &req.path, &shared).await?,
        "POST" => handle_post(&mut writer, &req, peer, &shared).await?,
        _ => {
            write_response(
                &mut writer,
                "405 Method Not Allowed",
                "text/plain",
                b"method not allowed",
            )
            .await?;
        }
    }
    Ok(())
}

async fn handle_get(
    writer: &mut (impl AsyncWriteExt + Unpin),
    path: &str,
    shared: &Shared,
) -> std::io::Result<()> {
    match path {
        "/.well-known/agent.json" | "/.well-known/agent-card.json" => {
            let card = build_agent_card(&shared.config);
            write_response(writer, "200 OK", "application/json", &json_body(&card)).await
        }
        "/health" => {
            write_response(
                writer,
                "200 OK",
                "application/json",
                br#"{"status":"ok","protocol":"a2a/1.0"}"#,
            )
            .await
        }
        _ => write_response(writer, "404 Not Found", "text/plain", b"not found").await,
    }
}

async fn handle_post(
    writer: &mut (impl AsyncWriteExt + Unpin),
    req: &HttpRequest,
    peer: SocketAddr,
    shared: &Shared,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if req.path != "/" {
        write_response(writer, "404 Not Found", "text/plain", b"not found").await?;
        return Ok(());
    }
    if req.body.len() > MAX_BODY_BYTES {
        write_response(
            writer,
            "413 Payload Too Large",
            "text/plain",
            b"body too large",
        )
        .await?;
        return Ok(());
    }
    if !bearer_authorized(&req.headers, &shared.config.bearer_token) {
        warn!("A2A: unauthorized request from {peer}");
        let body = jsonrpc_error(
            None,
            ERR_UNAUTHORIZED,
            "unauthorized: bad or missing bearer token",
        );
        write_response(
            writer,
            "401 Unauthorized",
            "application/json",
            &json_body(&body),
        )
        .await?;
        return Ok(());
    }

    let parsed = match parse_jsonrpc_request(&req.body) {
        Ok(p) => p,
        Err(e) => {
            debug!("A2A: invalid JSON-RPC from {peer}: {}", e.message);
            let body = e.into_response();
            write_response(writer, "200 OK", "application/json", &json_body(&body)).await?;
            return Ok(());
        }
    };

    let response: Option<Value> = match parsed.method.as_str() {
        "SendMessage" | "message/send" => Some(
            handle_message_send(shared, parsed.id.clone(), &parsed.params, &peer.to_string()).await,
        ),
        "SendStreamingMessage" | "message/stream" => Some(jsonrpc_error(
            parsed.id.clone(),
            ERR_METHOD_NOT_FOUND,
            "streaming is not supported by this server",
        )),
        "GetTask"
        | "tasks/get"
        | "ListTasks"
        | "tasks/list"
        | "CancelTask"
        | "tasks/cancel"
        | "SubscribeToTask"
        | "tasks/subscribe"
        | "CreateTaskPushNotificationConfig"
        | "tasks/pushNotificationConfig/create"
        | "tasks/pushNotificationConfig/set"
        | "tasks/pushNotification/set"
        | "GetTaskPushNotificationConfig"
        | "tasks/pushNotificationConfig/get"
        | "ListTaskPushNotificationConfigs"
        | "tasks/pushNotificationConfig/list"
        | "DeleteTaskPushNotificationConfig"
        | "tasks/pushNotificationConfig/delete" => Some(jsonrpc_error(
            parsed.id.clone(),
            ERR_METHOD_NOT_FOUND,
            format!(
                "method '{}' is not implemented by this server",
                parsed.method
            ),
        )),
        other => {
            debug!("A2A: unknown method '{other}' from {peer}");
            Some(jsonrpc_error(
                parsed.id.clone(),
                ERR_METHOD_NOT_FOUND,
                format!("method not found: {other}"),
            ))
        }
    };

    match (parsed.id, response) {
        // Notification: no response body per JSON-RPC.
        (None, _) => {
            write_response(writer, "202 Accepted", "text/plain", b"").await?;
        }
        (Some(_), Some(body)) => {
            write_response(writer, "200 OK", "application/json", &json_body(&body)).await?;
        }
        (Some(_), None) => {
            write_response(writer, "202 Accepted", "text/plain", b"").await?;
        }
    }
    Ok(())
}

/// Handle a `SendMessage` / `message/send` request: validate params
/// (fail-closed), forward the text to the gateway, and await the reply.
async fn handle_message_send(
    shared: &Shared,
    req_id: Option<Value>,
    params: &Value,
    peer: &str,
) -> Value {
    let err = |code: i64, msg: &str| jsonrpc_error(req_id.clone(), code, msg);

    let message = match params.get("message").and_then(Value::as_object) {
        Some(m) => Value::Object(m.clone()),
        None => {
            return err(
                ERR_INVALID_PARAMS,
                "params.message is required (A2A Message with parts)",
            );
        }
    };
    let text = extract_text(&message);
    if text.is_empty() {
        return err(ERR_INVALID_PARAMS, "message contains no text");
    }

    let task_id = new_task_id();
    let mut context_id = extract_context_id(params);
    if context_id.is_empty() {
        context_id = new_context_id();
    }
    let message_id = {
        let m = extract_message_id(params);
        if m.is_empty() {
            short_id("msg")
        } else {
            m
        }
    };

    // Notifications (no id) are fire-and-forget: forward, don't wait.
    let waiter = if req_id.is_some() {
        let (tx, rx) = oneshot::channel::<String>();
        shared.pending.lock().await.insert(context_id.clone(), tx);
        Some(rx)
    } else {
        None
    };

    let incoming = IncomingMessage {
        platform: "a2a".to_string(),
        chat_id: context_id.clone(),
        user_id: peer.to_string(),
        text,
        message_id: Some(message_id),
        is_dm: true,
        thread_id: None,
    };
    let forwarded = match shared.inbound_tx.read().await.clone() {
        Some(tx) => tx.send(incoming).await.is_ok(),
        None => false,
    };
    if !forwarded {
        if waiter.is_some() {
            shared.pending.lock().await.remove(&context_id);
        }
        error!("A2A: no inbound channel; dropping message/send from {peer}");
        return err(ERR_INTERNAL, "gateway inbound channel unavailable");
    }

    let Some(rx) = waiter else {
        return serde_json::json!({});
    };

    let timeout = Duration::from_secs(shared.config.reply_timeout_secs.max(1));
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(reply)) => {
            let (state, final_text) = finalize_reply(&reply);
            debug!("A2A: reply for task {task_id} (state {state})");
            let task = build_task(&task_id, &context_id, state, &final_text);
            jsonrpc_result(req_id.clone(), serde_json::json!({"task": task}))
        }
        Ok(Err(_)) => {
            // Sender dropped (adapter stopped); pending entry is already gone.
            err(ERR_INTERNAL, "request cancelled: adapter shutting down")
        }
        Err(_) => {
            shared.pending.lock().await.remove(&context_id);
            warn!("A2A: agent did not reply in time for task {task_id}");
            let task = build_task(
                &task_id,
                &context_id,
                STATE_FAILED,
                "[agent did not reply in time]",
            );
            jsonrpc_result(req_id.clone(), serde_json::json!({"task": task}))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (no live network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn send_request(method: &str, params: Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": "req-1",
            "method": method,
            "params": params,
        }))
        .unwrap()
    }

    fn text_send_params(text: &str) -> Value {
        serde_json::json!({
            "message": {
                "role": "ROLE_USER",
                "messageId": "m-123",
                "contextId": "ctx-abc",
                "parts": [{"text": text, "mediaType": "text/plain"}],
            }
        })
    }

    #[test]
    fn parse_valid_request() {
        let body = send_request("message/send", text_send_params("hello"));
        let req = parse_jsonrpc_request(&body).expect("valid request");
        assert_eq!(req.method, "message/send");
        assert_eq!(req.id, Some(Value::String("req-1".to_string())));
        assert_eq!(extract_text(&req.params["message"]), "hello");
    }

    #[test]
    fn parse_rejects_malformed_json() {
        let err = parse_jsonrpc_request(b"{not json").expect_err("must fail");
        assert_eq!(err.code, ERR_PARSE);
        assert!(err.id.is_none());
    }

    #[test]
    fn parse_rejects_non_object() {
        let err = parse_jsonrpc_request(b"[1,2,3]").expect_err("must fail");
        assert_eq!(err.code, ERR_INVALID_REQUEST);
    }

    #[test]
    fn parse_rejects_wrong_jsonrpc_version() {
        let body = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "1.0", "id": 7, "method": "message/send", "params": {},
        }))
        .unwrap();
        let err = parse_jsonrpc_request(&body).expect_err("must fail");
        assert_eq!(err.code, ERR_INVALID_REQUEST);
        assert_eq!(err.id, Some(Value::Number(7.into())));
    }

    #[test]
    fn parse_rejects_missing_method() {
        let body = serde_json::to_vec(&serde_json::json!({"jsonrpc": "2.0", "id": 1})).unwrap();
        let err = parse_jsonrpc_request(&body).expect_err("must fail");
        assert_eq!(err.code, ERR_INVALID_REQUEST);
    }

    #[test]
    fn parse_accepts_notification_without_id() {
        let body = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0", "method": "message/send", "params": {},
        }))
        .unwrap();
        let req = parse_jsonrpc_request(&body).expect("notification is valid");
        assert!(req.id.is_none());
    }

    #[test]
    fn extract_text_joins_parts_and_renders_files() {
        let msg = serde_json::json!({
            "parts": [
                {"text": "hello", "mediaType": "text/plain"},
                {"text": "world"},
                {"url": "https://x.test/a.png", "filename": "a.png", "mediaType": "image/png"},
                {"raw": "aGVsbG8=", "filename": "b.bin"},
                {"data": {"k": 1}},
                "not-an-object",
            ]
        });
        let text = extract_text(&msg);
        assert!(text.contains("hello\nworld"), "text parts joined: {text}");
        assert!(
            text.contains("[file: a.png] https://x.test/a.png"),
            "file note: {text}"
        );
        assert!(text.contains("base64-encoded"), "raw note: {text}");
        assert!(text.contains("\"k\":1"), "data rendered: {text}");
    }

    #[test]
    fn extract_ids_prefers_message_scoped_values() {
        let params = serde_json::json!({
            "message": {"messageId": "m-1", "contextId": "ctx-1"},
            "contextId": "ctx-legacy",
        });
        assert_eq!(extract_message_id(&params), "m-1");
        assert_eq!(extract_context_id(&params), "ctx-1");

        let legacy = serde_json::json!({"contextId": "ctx-legacy"});
        assert_eq!(extract_context_id(&legacy), "ctx-legacy");
        assert_eq!(extract_context_id(&serde_json::json!({})), "");
    }

    #[test]
    fn build_task_completed_shape() {
        let task = build_task("task-1", "ctx-1", STATE_COMPLETED, "done!");
        assert_eq!(task["id"], "task-1");
        assert_eq!(task["contextId"], "ctx-1");
        assert_eq!(task["status"]["state"], STATE_COMPLETED);
        assert!(task["status"]["timestamp"].is_string());
        assert_eq!(task["status"]["message"]["role"], ROLE_AGENT);
        assert_eq!(task["status"]["message"]["parts"][0]["text"], "done!");
        assert_eq!(task["artifacts"][0]["parts"][0]["text"], "done!");
    }

    #[test]
    fn build_task_failed_has_no_artifacts() {
        let task = build_task("task-2", "ctx-2", STATE_FAILED, "oops");
        assert_eq!(task["status"]["state"], STATE_FAILED);
        assert!(task.get("artifacts").is_none());
    }

    #[test]
    fn finalize_reply_detects_input_required_marker() {
        let (state, text) = finalize_reply("[INPUT_REQUIRED] which file?");
        assert_eq!(state, STATE_INPUT_REQUIRED);
        assert_eq!(text, "which file?");

        let (state, text) = finalize_reply("all done");
        assert_eq!(state, STATE_COMPLETED);
        assert_eq!(text, "all done");
    }

    #[test]
    fn bearer_auth_gate() {
        let mut headers = HashMap::new();
        // No token configured: everything passes.
        assert!(bearer_authorized(&headers, ""));
        // Token configured: missing/wrong header fails.
        assert!(!bearer_authorized(&headers, "s3cret"));
        headers.insert("authorization".to_string(), "Bearer wrong".to_string());
        assert!(!bearer_authorized(&headers, "s3cret"));
        headers.insert("authorization".to_string(), "Bearer s3cret".to_string());
        assert!(bearer_authorized(&headers, "s3cret"));
        // Header lookup is case-insensitive on the name (stored lowercased).
        let mut upper = HashMap::new();
        upper.insert("Authorization".to_string(), "Bearer s3cret".to_string());
        assert!(!bearer_authorized(&upper, "s3cret"));
    }

    #[test]
    fn agent_card_shape_and_auth_advertisement() {
        let plain = A2aConfig::default();
        let card = build_agent_card(&plain);
        assert_eq!(card["protocolVersion"], "1.0");
        assert_eq!(card["capabilities"]["streaming"], false);
        assert!(card.get("securitySchemes").is_none());

        let mut authed = A2aConfig::default();
        authed.bearer_token = "tok".to_string();
        let card = build_agent_card(&authed);
        assert_eq!(card["securitySchemes"]["bearer"]["scheme"], "bearer");
    }

    #[test]
    fn config_from_platform_config_reads_extras() {
        use hermes_config::platform::PlatformConfig;
        let mut cfg = PlatformConfig::default();
        cfg.extra
            .insert("port".to_string(), serde_json::json!(9999));
        cfg.extra
            .insert("bind_addr".to_string(), serde_json::json!("0.0.0.0"));
        cfg.extra
            .insert("bearer_token".to_string(), serde_json::json!("tok"));
        cfg.extra
            .insert("reply_timeout_secs".to_string(), serde_json::json!(42));
        let a2a = A2aConfig::from_platform_config(&cfg);
        assert_eq!(a2a.port, 9999);
        assert_eq!(a2a.bind_addr, "0.0.0.0");
        assert_eq!(a2a.bearer_token, "tok");
        assert_eq!(a2a.reply_timeout_secs, 42);
    }

    #[test]
    fn config_defaults_are_sane() {
        let cfg = A2aConfig::default();
        assert_eq!(cfg.bind_addr, "127.0.0.1");
        assert_eq!(cfg.port, 9900);
        assert!(cfg.bearer_token.is_empty());
        assert_eq!(cfg.reply_timeout_secs, 300);
    }

    #[test]
    fn ids_are_unique_and_prefixed() {
        let a = new_task_id();
        let b = new_task_id();
        assert!(a.starts_with("task-"));
        assert_ne!(a, b);
        assert!(new_context_id().starts_with("ctx-"));
    }

    #[test]
    fn error_response_envelope_shape() {
        let body = jsonrpc_error(
            Some(Value::String("r1".to_string())),
            ERR_METHOD_NOT_FOUND,
            "nope",
        );
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], "r1");
        assert_eq!(body["error"]["code"], -32601);
        assert_eq!(body["error"]["message"], "nope");
        assert!(body.get("result").is_none());
    }

    #[tokio::test]
    async fn send_message_fulfills_pending_request() {
        let adapter = A2aAdapter::new(A2aConfig::default());
        let (tx, rx) = oneshot::channel::<String>();
        adapter.pending.lock().await.insert("ctx-1".to_string(), tx);
        adapter
            .send_message("ctx-1", "hello back", None)
            .await
            .expect("send");
        assert_eq!(rx.await.expect("fulfilled"), "hello back");
        assert!(adapter.pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn send_message_without_pending_is_harmless() {
        let adapter = A2aAdapter::new(A2aConfig::default());
        adapter
            .send_message("ctx-unknown", "late reply", None)
            .await
            .expect("send never fails");
    }
}
