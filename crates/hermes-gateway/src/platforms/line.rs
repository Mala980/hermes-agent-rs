//! LINE Messaging API adapter.
//!
//! Mirrors `plugins/platforms/line/adapter.py` in the reference implementation:
//! an HTTP webhook server receives LINE events (verified with HMAC-SHA256 via
//! the `X-Line-Signature` header, fail-closed), text messages are forwarded to
//! the gateway inbound channel, and responses go out through the Reply API
//! (free, single-use reply token) with the metered Push API as fallback.
//!
//! Endpoints (LINE Messaging API):
//! - `POST https://api.line.me/v2/bot/message/reply` — reply to a webhook event
//! - `POST https://api.line.me/v2/bot/message/push` — proactive send to a chat
//! - `POST https://api.line.me/v2/bot/chat/loading/start` — typing indicator (DM only)

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Notify, RwLock};
use tracing::{debug, error, info, warn};

use hermes_core::errors::GatewayError;
use hermes_core::traits::{ParseMode, PlatformAdapter};

use crate::adapter::BasePlatformAdapter;
use crate::gateway::IncomingMessage;

/// LINE Messaging API configuration (defined in hermes-config).
pub use hermes_config::platform::LineConfig;

const LINE_REPLY_URL: &str = "https://api.line.me/v2/bot/message/reply";
const LINE_PUSH_URL: &str = "https://api.line.me/v2/bot/message/push";
const LINE_LOADING_URL: &str = "https://api.line.me/v2/bot/chat/loading/start";

/// Hard per-bubble limit is 5000 chars; chunk conservatively (as in the original).
const LINE_SAFE_BUBBLE_CHARS: usize = 4500;
/// LINE accepts at most 5 message objects per API call.
const LINE_MAX_MESSAGES_PER_CALL: usize = 5;
/// Reply tokens expire ~60 s after the webhook; consume them well within that.
const LINE_REPLY_TOKEN_TTL: Duration = Duration::from_secs(50);
/// Webhooks are tiny JSON; reject anything larger (fail-closed).
const WEBHOOK_BODY_MAX_BYTES: usize = 1_048_576; // 1 MiB
/// Cap on the raw header block we will read before bailing out.
const HEADER_BLOCK_MAX_BYTES: usize = 65_536;
/// How long we remember seen webhook event ids for redelivery dedup.
const SEEN_EVENT_TTL: Duration = Duration::from_secs(600);

/// LINE source block shapes: user → DM, group/room → group chat.
const DM_PREFIX: &str = "U";

// ---------------------------------------------------------------------------
// Signature verification
// ---------------------------------------------------------------------------

/// Verify `X-Line-Signature`: `base64(HMAC-SHA256(channel_secret, raw_body))`.
///
/// Fail-closed: an empty secret, empty/malformed signature, or any crypto
/// error returns `false`. The comparison is constant-time via `hmac::Mac`.
pub fn verify_line_signature(secret: &str, body: &[u8], signature: &str) -> bool {
    if secret.is_empty() || signature.trim().is_empty() {
        return false;
    }
    let provided = match BASE64.decode(signature.trim()) {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };
    let mut mac = match Hmac::<Sha256>::new_from_slice(secret.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => return false,
    };
    mac.update(body);
    mac.verify_slice(&provided).is_ok()
}

// ---------------------------------------------------------------------------
// Webhook event parsing
// ---------------------------------------------------------------------------

/// A text message extracted from a LINE webhook event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineTextMessage {
    /// Destination chat id (`userId` for DMs, `groupId`/`roomId` for groups).
    pub chat_id: String,
    /// LINE user id of the sender (falls back to `chat_id`).
    pub user_id: String,
    /// LINE message id (for reply threading).
    pub message_id: Option<String>,
    /// Message text.
    pub text: String,
    /// `true` for 1:1 user chats, `false` for groups/rooms.
    pub is_dm: bool,
    /// Single-use token for the Reply API (`None` for non-message events).
    pub reply_token: Option<String>,
}

/// Extract text messages from a parsed LINE webhook payload.
///
/// Handles `message` events with `message.type == "text"`; lifecycle events
/// (`follow`, `unfollow`, `join`, `leave`, `memberJoined`, `memberLeft`) and
/// non-text messages are ignored here (logged by the caller).
pub fn extract_text_events(payload: &Value) -> Vec<LineTextMessage> {
    let events = match payload.get("events").and_then(Value::as_array) {
        Some(events) => events,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for event in events {
        if event.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let message = match event.get("message") {
            Some(m) => m,
            None => continue,
        };
        if message.get("type").and_then(Value::as_str) != Some("text") {
            continue; // image/audio/video/file/location/sticker handled elsewhere
        }
        let text = message.get("text").and_then(Value::as_str).unwrap_or("");
        if text.is_empty() {
            continue;
        }
        let source = event.get("source");
        let (chat_id, is_dm) = resolve_chat(source);
        if chat_id.is_empty() {
            debug!("LINE: message event without a resolvable source; skipping");
            continue;
        }
        let user_id = source
            .and_then(|s| s.get("userId"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(&chat_id)
            .to_string();
        out.push(LineTextMessage {
            chat_id,
            user_id,
            message_id: message
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string),
            text: text.to_string(),
            is_dm,
            reply_token: event
                .get("replyToken")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        });
    }
    out
}

/// Resolve `(chat_id, is_dm)` from a LINE event `source` block.
fn resolve_chat(source: Option<&Value>) -> (String, bool) {
    let source = match source {
        Some(s) => s,
        None => return (String::new(), true),
    };
    match source.get("type").and_then(Value::as_str) {
        Some("user") => (
            source
                .get("userId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            true,
        ),
        Some("group") => (
            source
                .get("groupId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            false,
        ),
        Some("room") => (
            source
                .get("roomId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            false,
        ),
        _ => (String::new(), true),
    }
}

// ---------------------------------------------------------------------------
// Outbound payload builders
// ---------------------------------------------------------------------------

/// Chunk text into LINE bubbles (char-boundary safe; LINE rejects >5000 chars
/// per bubble and >5 bubbles per call).
pub fn chunk_text(text: &str) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;
    for ch in text.chars() {
        if current_len >= LINE_SAFE_BUBBLE_CHARS {
            chunks.push(std::mem::take(&mut current));
            current_len = 0;
        }
        current.push(ch);
        current_len += 1;
    }
    if !current.is_empty() || chunks.is_empty() {
        chunks.push(current);
    }
    if chunks.len() > LINE_MAX_MESSAGES_PER_CALL {
        chunks.truncate(LINE_MAX_MESSAGES_PER_CALL);
        if let Some(last) = chunks.last_mut() {
            // Mark truncation the same way the reference implementation does.
            let trimmed: String = last.chars().take(LINE_SAFE_BUBBLE_CHARS - 1).collect();
            *last = format!("{trimmed}…");
        }
    }
    chunks
}

/// Build the Reply API payload: `{"replyToken": ..., "messages": [...]}`.
pub fn build_reply_payload(reply_token: &str, texts: &[String]) -> Value {
    json!({
        "replyToken": reply_token,
        "messages": texts.iter().map(|t| json!({"type": "text", "text": t})).collect::<Vec<_>>(),
    })
}

/// Build the Push API payload: `{"to": chat_id, "messages": [...]}`.
pub fn build_push_payload(chat_id: &str, texts: &[String]) -> Value {
    json!({
        "to": chat_id,
        "messages": texts.iter().map(|t| json!({"type": "text", "text": t})).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

struct LineInner {
    config: LineConfig,
    client: Client,
    base: BasePlatformAdapter,
    inbound_tx: RwLock<Option<mpsc::Sender<IncomingMessage>>>,
    /// chat_id → (reply token, received at); tokens are single-use, ~60 s TTL.
    reply_tokens: RwLock<HashMap<String, (String, Instant)>>,
    /// webhookEventId → seen at; guards against LINE's at-least-once redelivery.
    seen_event_ids: RwLock<HashMap<String, Instant>>,
}

pub struct LineAdapter {
    inner: Arc<LineInner>,
    stop_signal: Arc<Notify>,
    shutdown_tx: RwLock<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl LineAdapter {
    /// Create the adapter. Fails closed when `channel_secret` or
    /// `channel_access_token` is empty — without them the webhook cannot be
    /// authenticated and nothing can be sent.
    pub fn new(config: LineConfig) -> Result<Self, GatewayError> {
        if config.channel_secret.trim().is_empty() {
            return Err(GatewayError::Auth(
                "LINE channel_secret must not be empty (webhook signatures cannot be verified)"
                    .into(),
            ));
        }
        let base = BasePlatformAdapter::new(&config.channel_access_token);
        base.validate_token()?;
        let client = base.build_client()?;
        Ok(Self {
            inner: Arc::new(LineInner {
                config,
                client,
                base,
                inbound_tx: RwLock::new(None),
                reply_tokens: RwLock::new(HashMap::new()),
                seen_event_ids: RwLock::new(HashMap::new()),
            }),
            stop_signal: Arc::new(Notify::new()),
            shutdown_tx: RwLock::new(None),
        })
    }

    pub fn config(&self) -> &LineConfig {
        &self.inner.config
    }

    /// Set the channel inbound webhook payloads are forwarded to.
    pub async fn set_inbound_sender(&self, tx: mpsc::Sender<IncomingMessage>) {
        *self.inner.inbound_tx.write().await = Some(tx);
    }

    /// Take and invalidate the fresh reply token for a chat, if any.
    async fn consume_reply_token(&self, chat_id: &str) -> Option<String> {
        let mut tokens = self.inner.reply_tokens.write().await;
        match tokens.remove(chat_id) {
            Some((token, received_at)) if received_at.elapsed() < LINE_REPLY_TOKEN_TTL => {
                Some(token)
            }
            _ => None,
        }
    }

    async fn post_json(&self, url: &str, payload: &Value) -> Result<(), GatewayError> {
        let resp = self
            .inner
            .client
            .post(url)
            .bearer_auth(&self.inner.config.channel_access_token)
            .json(payload)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("LINE API request failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(200).collect();
            return Err(GatewayError::SendFailed(format!(
                "LINE API error {status}: {snippet}"
            )));
        }
        Ok(())
    }

    /// Send text bubbles, preferring the Reply API (free) while the webhook's
    /// reply token is fresh, falling back to the Push API (metered).
    async fn send_text_bubbles(&self, chat_id: &str, texts: &[String]) -> Result<(), GatewayError> {
        if let Some(token) = self.consume_reply_token(chat_id).await {
            match self
                .post_json(LINE_REPLY_URL, &build_reply_payload(&token, texts))
                .await
            {
                Ok(()) => return Ok(()),
                Err(e) => warn!("LINE reply API failed, falling back to push: {e}"),
            }
        }
        self.post_json(LINE_PUSH_URL, &build_push_payload(chat_id, texts))
            .await
    }

    #[cfg(test)]
    fn test_inner(&self) -> Arc<LineInner> {
        self.inner.clone()
    }
}

#[async_trait]
impl PlatformAdapter for LineAdapter {
    async fn start(&self) -> Result<(), GatewayError> {
        info!(
            "LINE adapter starting: webhook on {}:{} path {}",
            self.inner.config.bind_addr, self.inner.config.port, self.inner.config.path
        );

        let addr: SocketAddr =
            format!("{}:{}", self.inner.config.bind_addr, self.inner.config.port)
                .parse()
                .map_err(|e| {
                    GatewayError::ConnectionFailed(format!("Invalid LINE bind address: {e}"))
                })?;

        let inner = self.inner.clone();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        *self.shutdown_tx.write().await = Some(shutdown_tx);

        tokio::spawn(async move {
            let listener = match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => l,
                Err(e) => {
                    error!("LINE webhook server failed to bind {addr}: {e}");
                    return;
                }
            };
            info!("LINE webhook server listening on {addr}");
            let mut shutdown_rx = shutdown_rx;
            loop {
                tokio::select! {
                    accept = listener.accept() => {
                        match accept {
                            Ok((stream, peer)) => {
                                let inner = inner.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = handle_connection(stream, peer, inner).await {
                                        debug!("LINE webhook connection error from {peer}: {e}");
                                    }
                                });
                            }
                            Err(e) => warn!("LINE webhook accept error: {e}"),
                        }
                    }
                    _ = &mut shutdown_rx => {
                        info!("LINE webhook server shutting down");
                        break;
                    }
                }
            }
        });

        self.inner.base.mark_running();
        Ok(())
    }

    async fn stop(&self) -> Result<(), GatewayError> {
        info!("LINE adapter stopping");
        if let Some(tx) = self.shutdown_tx.write().await.take() {
            let _ = tx.send(());
        }
        self.inner.base.mark_stopped();
        self.stop_signal.notify_one();
        Ok(())
    }

    async fn send_message(
        &self,
        chat_id: &str,
        text: &str,
        _parse_mode: Option<ParseMode>,
    ) -> Result<(), GatewayError> {
        let chunks = chunk_text(text);
        self.send_text_bubbles(chat_id, &chunks).await
    }

    async fn edit_message(
        &self,
        _chat_id: &str,
        _message_id: &str,
        _text: &str,
    ) -> Result<(), GatewayError> {
        // LINE has no message-edit API; log and treat as delivered.
        debug!("LINE does not support message editing");
        Ok(())
    }

    async fn send_file(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<(), GatewayError> {
        // LINE only accepts media as public HTTPS URLs. Local files cannot be
        // fetched by the LINE platform, so they are announced as text.
        if file_path.starts_with("http://") || file_path.starts_with("https://") {
            let mut messages = vec![json!({
                "type": "image",
                "originalContentUrl": file_path,
                "previewImageUrl": file_path,
            })];
            if let Some(cap) = caption.filter(|c| !c.is_empty()) {
                for chunk in chunk_text(cap) {
                    messages.push(json!({"type": "text", "text": chunk}));
                }
            }
            let payload = json!({ "to": chat_id, "messages": messages });
            return self.post_json(LINE_PUSH_URL, &payload).await;
        }
        let file_name = std::path::Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");
        let text = match caption {
            Some(cap) if !cap.is_empty() => format!("[file:{file_name}] {cap}"),
            _ => format!("[file:{file_name}]"),
        };
        self.send_message(chat_id, &text, None).await
    }

    /// Typing indicator via the chat loading API (DM only; LINE rejects
    /// groups/rooms). Best-effort: failures are logged, never raised.
    async fn send_typing(&self, chat_id: &str) -> Result<(), GatewayError> {
        if !chat_id.starts_with(DM_PREFIX) {
            return Ok(());
        }
        let payload = json!({ "chatId": chat_id, "loadingSeconds": 5 });
        if let Err(e) = self.post_json(LINE_LOADING_URL, &payload).await {
            debug!("LINE loading indicator failed: {e}");
        }
        Ok(())
    }

    /// Prune expired reply tokens and seen event ids.
    async fn maintenance_prune(&self) {
        {
            let mut tokens = self.inner.reply_tokens.write().await;
            tokens.retain(|_, (_, at)| at.elapsed() < LINE_REPLY_TOKEN_TTL);
        }
        {
            let mut seen = self.inner.seen_event_ids.write().await;
            seen.retain(|_, at| at.elapsed() < SEEN_EVENT_TTL);
        }
    }

    fn is_running(&self) -> bool {
        self.inner.base.is_running()
    }

    fn platform_name(&self) -> &str {
        "line"
    }
}

// ---------------------------------------------------------------------------
// Minimal HTTP webhook server
// ---------------------------------------------------------------------------

/// Parsed HTTP request (enough for LINE's webhook POSTs).
struct ParsedRequest {
    method: String,
    path: String,
    signature: Option<String>,
    body: Vec<u8>,
}

enum ReadOutcome {
    Request(ParsedRequest),
    PayloadTooLarge,
    BadRequest,
}

/// Read one HTTP request: headers (capped), then exactly `Content-Length` body
/// bytes (capped at [`WEBHOOK_BODY_MAX_BYTES`]).
async fn read_http_request(stream: &mut TcpStream) -> Result<ReadOutcome, std::io::Error> {
    let mut header_buf = Vec::with_capacity(4096);
    loop {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await?;
        header_buf.push(byte[0]);
        if header_buf.len() >= 4 && header_buf[header_buf.len() - 4..] == *b"\r\n\r\n" {
            break;
        }
        if header_buf.len() > HEADER_BLOCK_MAX_BYTES {
            return Ok(ReadOutcome::BadRequest);
        }
    }
    let head = String::from_utf8_lossy(&header_buf);
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let mut signature: Option<String> = None;
    let mut content_length: usize = 0;
    for line in lines {
        // Split on the first ':' and compare the header NAME case-insensitively;
        // the VALUE must keep its original case (base64 is case-sensitive).
        if let Some((name, value)) = line.split_once(':') {
            match name.trim().to_lowercase().as_str() {
                "x-line-signature" => signature = Some(value.trim().to_string()),
                "content-length" => content_length = value.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
    }

    if content_length > WEBHOOK_BODY_MAX_BYTES {
        return Ok(ReadOutcome::PayloadTooLarge);
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        stream.read_exact(&mut body).await?;
    }
    Ok(ReadOutcome::Request(ParsedRequest {
        method,
        path,
        signature,
        body,
    }))
}

async fn write_response(
    stream: &mut TcpStream,
    status: &str,
    body: &str,
) -> Result<(), std::io::Error> {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(resp.as_bytes()).await
}

/// Handle one webhook connection: verify the signature (fail-closed),
/// dispatch events, and acknowledge with 200 on success.
async fn handle_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    inner: Arc<LineInner>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let request = match read_http_request(&mut stream).await {
        Ok(ReadOutcome::Request(r)) => r,
        Ok(ReadOutcome::PayloadTooLarge) => {
            write_response(
                &mut stream,
                "413 Payload Too Large",
                r#"{"error":"payload too large"}"#,
            )
            .await?;
            return Ok(());
        }
        Ok(ReadOutcome::BadRequest) | Err(_) => {
            write_response(&mut stream, "400 Bad Request", r#"{"error":"bad request"}"#).await?;
            return Ok(());
        }
    };

    if request.method == "GET" {
        // Health check for load balancers / tunnel probes.
        write_response(
            &mut stream,
            "200 OK",
            r#"{"status":"ok","platform":"line"}"#,
        )
        .await?;
        return Ok(());
    }

    if request.method != "POST" || request.path != inner.config.path {
        write_response(&mut stream, "404 Not Found", r#"{"error":"not found"}"#).await?;
        return Ok(());
    }

    // Fail-closed: missing or wrong signature → reject without touching the body.
    let signature = request.signature.as_deref().unwrap_or("");
    if !verify_line_signature(&inner.config.channel_secret, &request.body, signature) {
        warn!("LINE: rejecting webhook from {peer}: invalid signature");
        write_response(
            &mut stream,
            "401 Unauthorized",
            r#"{"error":"invalid signature"}"#,
        )
        .await?;
        return Ok(());
    }

    let payload: Value = match serde_json::from_slice(&request.body) {
        Ok(p) => p,
        Err(_) => {
            write_response(&mut stream, "400 Bad Request", r#"{"error":"bad json"}"#).await?;
            return Ok(());
        }
    };

    dispatch_events(&payload, &inner).await;

    write_response(&mut stream, "200 OK", r#"{"status":"ok"}"#).await?;
    Ok(())
}

/// Dispatch LINE webhook events: forward text messages, log lifecycle events.
async fn dispatch_events(payload: &Value, inner: &LineInner) {
    let events = match payload.get("events").and_then(Value::as_array) {
        Some(e) => e,
        None => return,
    };
    for event in events {
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
        let event_id = event
            .get("webhookEventId")
            .and_then(Value::as_str)
            .unwrap_or("");

        // At-least-once redelivery guard.
        if !event_id.is_empty() {
            let mut seen = inner.seen_event_ids.write().await;
            if seen.contains_key(event_id) {
                debug!("LINE: ignoring duplicate webhook event {event_id}");
                continue;
            }
            seen.insert(event_id.to_string(), Instant::now());
        }

        match event_type {
            "message" => {
                for msg in extract_text_events(&json!({ "events": [event] })) {
                    stash_reply_token(inner, &msg).await;
                    forward_inbound(inner, msg).await;
                }
                // Non-text messages (image/audio/video/file/sticker/location)
                // are intentionally not forwarded in this adapter.
            }
            "follow" | "unfollow" | "join" | "leave" | "memberJoined" | "memberLeft" => {
                let source_user = event
                    .get("source")
                    .and_then(|s| s.get("userId"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                info!("LINE: lifecycle event {event_type} from {source_user}");
            }
            other => debug!("LINE: ignoring event type {other:?}"),
        }
    }
}

/// Remember the reply token so the next `send_message` for this chat can use
/// the free Reply API while the token is fresh.
async fn stash_reply_token(inner: &LineInner, msg: &LineTextMessage) {
    if let Some(token) = msg.reply_token.clone() {
        inner
            .reply_tokens
            .write()
            .await
            .insert(msg.chat_id.clone(), (token, Instant::now()));
    }
}

async fn forward_inbound(inner: &LineInner, msg: LineTextMessage) {
    let tx = inner.inbound_tx.read().await;
    if let Some(tx) = tx.as_ref() {
        let inbound = IncomingMessage {
            platform: "line".to_string(),
            chat_id: msg.chat_id,
            user_id: msg.user_id,
            text: msg.text,
            message_id: msg.message_id,
            is_dm: msg.is_dm,
        };
        if tx.send(inbound).await.is_err() {
            warn!("LINE: inbound channel closed; dropping message");
        }
    } else {
        debug!("LINE: no inbound sender registered; dropping message");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_secret() -> &'static str {
        "test_channel_secret"
    }

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
        mac.update(body);
        BASE64.encode(mac.finalize().into_bytes())
    }

    fn test_config() -> LineConfig {
        LineConfig {
            channel_secret: test_secret().to_string(),
            channel_access_token: "test_channel_access_token".to_string(),
            bind_addr: "127.0.0.1".to_string(),
            port: 0,
            path: "/line/webhook".to_string(),
        }
    }

    fn sample_payload() -> Value {
        json!({
            "destination": "Ubot",
            "events": [
                {
                    "type": "message",
                    "message": {"id": "m1", "type": "text", "text": "hello hermes"},
                    "replyToken": "reply-token-1",
                    "source": {"type": "user", "userId": "Ualice"},
                    "webhookEventId": "evt-1",
                },
                {
                    "type": "message",
                    "message": {"id": "m2", "type": "text", "text": "hi group"},
                    "replyToken": "reply-token-2",
                    "source": {"type": "group", "groupId": "Cgroup1", "userId": "Ubob"},
                    "webhookEventId": "evt-2",
                },
                {
                    "type": "message",
                    "message": {"id": "m3", "type": "sticker", "packageId": "1", "stickerId": "1"},
                    "replyToken": "reply-token-3",
                    "source": {"type": "user", "userId": "Ualice"},
                    "webhookEventId": "evt-3",
                },
                {
                    "type": "follow",
                    "replyToken": "reply-token-4",
                    "source": {"type": "user", "userId": "Ucarol"},
                    "webhookEventId": "evt-4",
                },
            ]
        })
    }

    // -- signature verification ------------------------------------------------

    #[test]
    fn verify_signature_accepts_correct() {
        let body = br#"{"events":[]}"#;
        let sig = sign(test_secret(), body);
        assert!(verify_line_signature(test_secret(), body, &sig));
    }

    #[test]
    fn verify_signature_rejects_tampered_body() {
        let body = br#"{"events":[]}"#;
        let sig = sign(test_secret(), body);
        assert!(!verify_line_signature(
            test_secret(),
            br#"{"events":[{"x":1}]}"#,
            &sig
        ));
    }

    #[test]
    fn verify_signature_rejects_wrong_secret() {
        let body = br#"{"events":[]}"#;
        let sig = sign("other_secret", body);
        assert!(!verify_line_signature(test_secret(), body, &sig));
    }

    #[test]
    fn verify_signature_fail_closed_on_missing_secret() {
        let body = br#"{"events":[]}"#;
        let sig = sign(test_secret(), body);
        assert!(!verify_line_signature("", body, &sig));
    }

    #[test]
    fn verify_signature_fail_closed_on_empty_signature() {
        let body = br#"{"events":[]}"#;
        assert!(!verify_line_signature(test_secret(), body, ""));
        assert!(!verify_line_signature(test_secret(), body, "   "));
    }

    #[test]
    fn verify_signature_rejects_malformed_base64() {
        let body = br#"{"events":[]}"#;
        assert!(!verify_line_signature(
            test_secret(),
            body,
            "!!!not-base64!!!"
        ));
    }

    // -- event parsing ----------------------------------------------------------

    #[test]
    fn extract_text_events_parses_dm_and_group() {
        let msgs = extract_text_events(&sample_payload());
        assert_eq!(msgs.len(), 2);

        let dm = &msgs[0];
        assert_eq!(dm.chat_id, "Ualice");
        assert_eq!(dm.user_id, "Ualice");
        assert_eq!(dm.text, "hello hermes");
        assert!(dm.is_dm);
        assert_eq!(dm.reply_token.as_deref(), Some("reply-token-1"));
        assert_eq!(dm.message_id.as_deref(), Some("m1"));

        let group = &msgs[1];
        assert_eq!(group.chat_id, "Cgroup1");
        assert_eq!(group.user_id, "Ubob");
        assert!(!group.is_dm);
    }

    #[test]
    fn extract_text_events_ignores_non_text_and_lifecycle() {
        let msgs = extract_text_events(&sample_payload());
        // sticker + follow events produce no text messages
        assert!(msgs.iter().all(|m| !m.text.is_empty()));
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn extract_text_events_empty_when_no_events_array() {
        assert!(extract_text_events(&json!({})).is_empty());
        assert!(extract_text_events(&json!({"events": null})).is_empty());
        assert!(extract_text_events(&json!({"events": []})).is_empty());
    }

    #[test]
    fn extract_text_events_room_source() {
        let payload = json!({
            "events": [{
                "type": "message",
                "message": {"id": "m9", "type": "text", "text": "room msg"},
                "source": {"type": "room", "roomId": "Rroom1", "userId": "Udave"},
            }]
        });
        let msgs = extract_text_events(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].chat_id, "Rroom1");
        assert_eq!(msgs[0].user_id, "Udave");
        assert!(!msgs[0].is_dm);
        assert!(msgs[0].reply_token.is_none());
    }

    #[test]
    fn extract_text_events_user_id_falls_back_to_chat_id() {
        let payload = json!({
            "events": [{
                "type": "message",
                "message": {"id": "m9", "type": "text", "text": "hi"},
                "source": {"type": "group", "groupId": "Cgroup9"},
            }]
        });
        let msgs = extract_text_events(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].user_id, "Cgroup9");
    }

    #[test]
    fn extract_text_events_skips_empty_text() {
        let payload = json!({
            "events": [{
                "type": "message",
                "message": {"id": "m9", "type": "text", "text": ""},
                "source": {"type": "user", "userId": "Ualice"},
            }]
        });
        assert!(extract_text_events(&payload).is_empty());
    }

    // -- chunking -----------------------------------------------------------------

    #[test]
    fn chunk_text_short_message_is_single_bubble() {
        assert_eq!(chunk_text("hello"), vec!["hello".to_string()]);
        assert_eq!(chunk_text(""), vec!["".to_string()]);
    }

    #[test]
    fn chunk_text_splits_at_bubble_limit() {
        let text: String = "a".repeat(9001);
        let chunks = chunk_text(&text);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), LINE_SAFE_BUBBLE_CHARS);
        assert_eq!(chunks[1].len(), LINE_SAFE_BUBBLE_CHARS);
        assert_eq!(chunks[2].len(), 1);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn chunk_text_is_char_boundary_safe() {
        let text = "🎉".repeat(5000);
        let chunks = chunk_text(&text);
        assert_eq!(chunks.len(), 2);
        assert!(chunks
            .iter()
            .all(|c| c.chars().count() <= LINE_SAFE_BUBBLE_CHARS));
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn chunk_text_caps_at_five_bubbles_with_ellipsis() {
        let text: String = "b".repeat(30_000);
        let chunks = chunk_text(&text);
        assert_eq!(chunks.len(), LINE_MAX_MESSAGES_PER_CALL);
        // LINE's bubble limit is in characters, not bytes.
        assert!(chunks
            .iter()
            .all(|c| c.chars().count() <= LINE_SAFE_BUBBLE_CHARS));
        assert!(chunks[4].ends_with('…'));
    }

    // -- payload builders ------------------------------------------------------------

    #[test]
    fn build_reply_payload_shape() {
        let texts = vec!["hi".to_string(), "there".to_string()];
        let payload = build_reply_payload("tok123", &texts);
        assert_eq!(payload["replyToken"], json!("tok123"));
        let messages = payload["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["type"], json!("text"));
        assert_eq!(messages[0]["text"], json!("hi"));
    }

    #[test]
    fn build_push_payload_shape() {
        let payload = build_push_payload("Ualice", &["hi".to_string()]);
        assert_eq!(payload["to"], json!("Ualice"));
        assert_eq!(payload["messages"][0]["type"], json!("text"));
        assert_eq!(payload["messages"][0]["text"], json!("hi"));
    }

    // -- constructor fail-closed -------------------------------------------------------

    #[test]
    fn new_rejects_empty_channel_secret() {
        let mut cfg = test_config();
        cfg.channel_secret = String::new();
        assert!(LineAdapter::new(cfg).is_err());
    }

    #[test]
    fn new_rejects_empty_access_token() {
        let mut cfg = test_config();
        cfg.channel_access_token = String::new();
        assert!(LineAdapter::new(cfg).is_err());
    }

    #[test]
    fn new_accepts_valid_config() {
        let adapter = LineAdapter::new(test_config()).expect("valid config");
        assert_eq!(adapter.platform_name(), "line");
        assert!(!adapter.is_running());
    }

    // -- live loopback dispatch (localhost only, no external network) -------------------

    async fn post_raw(
        addr: SocketAddr,
        path: &str,
        body: &[u8],
        signature: Option<&str>,
    ) -> String {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        let mut request = format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        );
        if let Some(sig) = signature {
            request.push_str(&format!("X-Line-Signature: {sig}\r\n"));
        }
        request.push_str("\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write head");
        stream.write_all(body).await.expect("write body");
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).await.expect("read");
        String::from_utf8_lossy(&resp).into_owned()
    }

    async fn spawn_test_server(inner: Arc<LineInner>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let handle = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.expect("accept");
            handle_connection(stream, peer, inner)
                .await
                .expect("handle");
        });
        (addr, handle)
    }

    #[tokio::test]
    async fn webhook_dispatches_text_event_to_inbound_channel() {
        let adapter = LineAdapter::new(test_config()).expect("valid config");
        let (tx, mut rx) = mpsc::channel::<IncomingMessage>(16);
        adapter.set_inbound_sender(tx).await;
        let inner = adapter.test_inner();

        let (addr, server) = spawn_test_server(inner).await;
        let body = serde_json::to_vec(&sample_payload()).unwrap();
        let sig = sign(test_secret(), &body);
        let resp = post_raw(addr, "/line/webhook", &body, Some(&sig)).await;
        server.await.expect("server task");

        assert!(resp.starts_with("HTTP/1.1 200 OK"), "resp was: {resp}");

        let first = rx.recv().await.expect("inbound message");
        assert_eq!(first.platform, "line");
        assert_eq!(first.chat_id, "Ualice");
        assert_eq!(first.user_id, "Ualice");
        assert_eq!(first.text, "hello hermes");
        assert!(first.is_dm);

        let second = rx.recv().await.expect("inbound message");
        assert_eq!(second.chat_id, "Cgroup1");
        assert!(!second.is_dm);
    }

    #[tokio::test]
    async fn webhook_rejects_bad_signature_with_401() {
        let adapter = LineAdapter::new(test_config()).expect("valid config");
        let (tx, mut rx) = mpsc::channel::<IncomingMessage>(16);
        adapter.set_inbound_sender(tx).await;
        let inner = adapter.test_inner();

        let (addr, server) = spawn_test_server(inner).await;
        let body = serde_json::to_vec(&sample_payload()).unwrap();
        let resp = post_raw(addr, "/line/webhook", &body, Some("bogus")).await;
        server.await.expect("server task");

        assert!(resp.starts_with("HTTP/1.1 401"), "resp was: {resp}");
        assert!(rx.try_recv().is_err(), "no message should be forwarded");
    }

    #[tokio::test]
    async fn webhook_rejects_missing_signature_with_401() {
        let adapter = LineAdapter::new(test_config()).expect("valid config");
        let (tx, mut rx) = mpsc::channel::<IncomingMessage>(16);
        adapter.set_inbound_sender(tx).await;
        let inner = adapter.test_inner();

        let (addr, server) = spawn_test_server(inner).await;
        let body = serde_json::to_vec(&sample_payload()).unwrap();
        let resp = post_raw(addr, "/line/webhook", &body, None).await;
        server.await.expect("server task");

        assert!(resp.starts_with("HTTP/1.1 401"), "resp was: {resp}");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn webhook_dedups_redelivered_event_ids() {
        let adapter = LineAdapter::new(test_config()).expect("valid config");
        let (tx, mut rx) = mpsc::channel::<IncomingMessage>(16);
        adapter.set_inbound_sender(tx).await;
        let inner = adapter.test_inner();

        // Dispatch the same payload twice through dispatch_events: the second
        // pass must not re-forward (webhookEventId dedup).
        let payload = sample_payload();
        dispatch_events(&payload, &inner).await;
        dispatch_events(&payload, &inner).await;

        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(count, 2, "duplicate redelivery must not re-forward");
    }

    #[tokio::test]
    async fn reply_token_stashed_for_fresh_send() {
        let adapter = LineAdapter::new(test_config()).expect("valid config");
        let inner = adapter.test_inner();
        let payload = sample_payload();
        dispatch_events(&payload, &inner).await;

        // Token for the DM chat should be consumable exactly once.
        let token = adapter.consume_reply_token("Ualice").await;
        assert_eq!(token.as_deref(), Some("reply-token-1"));
        assert!(adapter.consume_reply_token("Ualice").await.is_none());
    }

    #[tokio::test]
    async fn maintenance_prune_drops_stale_state() {
        let adapter = LineAdapter::new(test_config()).expect("valid config");
        let stale = Instant::now()
            .checked_sub(SEEN_EVENT_TTL + Duration::from_secs(1))
            .expect("past instant");
        {
            let inner = adapter.test_inner();
            inner
                .reply_tokens
                .write()
                .await
                .insert("Uold".to_string(), ("tok".to_string(), stale));
            inner
                .seen_event_ids
                .write()
                .await
                .insert("evt-old".to_string(), stale);
        }
        adapter.maintenance_prune().await;
        let inner = adapter.test_inner();
        assert!(!inner.reply_tokens.read().await.contains_key("Uold"));
        assert!(!inner.seen_event_ids.read().await.contains_key("evt-old"));
    }
}
