//! Buzz team-messaging platform adapter.
//!
//! Buzz is a Slack/Discord-style team chat platform. This adapter talks to it
//! over plain HTTPS:
//!
//! Inbound: a minimal HTTP webhook server receives Buzz event JSON on
//! `POST {webhook_path}` (default `/buzz/webhook`). Every request must carry
//! an `X-Buzz-Signature` header holding `sha256=<hex>` HMAC-SHA256 over the
//! raw body, verified fail-closed against `webhook_secret`. Verified payloads
//! shaped `{ "user_id": "...", "channel_id": "...", "text": "...",
//! "message_id": "..." }` are forwarded to the gateway via an `mpsc` inbound
//! channel. `GET` on the same path answers a health check.
//!
//! Outbound: `POST {webhook_url}/messages` with a JSON body
//! `{ "channel_id": "...", "text": "..." }` (Bearer auth from `bot_token` when
//! set); files go to `POST {webhook_url}/files` as multipart uploads.
//!
//! Webhooks redelivered by the platform are deduplicated on `message_id`
//! (TTL-bounded, so the cache cannot grow without bound).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::{Client, RequestBuilder};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Notify, RwLock};
use tracing::{debug, error, info, warn};

use hermes_config::BuzzConfig;
use hermes_core::errors::GatewayError;
use hermes_core::traits::{ParseMode, PlatformAdapter};

use super::helpers::{
    mime_from_extension, read_http_request, verify_hmac_sha256_signature, write_http_response,
};
use crate::adapter::{AdapterProxyConfig, BasePlatformAdapter};
use crate::gateway::IncomingMessage;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Request header carrying the webhook HMAC signature (`sha256=<hex>`).
const BUZZ_SIGNATURE_HEADER: &str = "x-buzz-signature";
/// Cap on inbound webhook bodies; larger payloads are rejected (fail-closed).
const WEBHOOK_BODY_MAX_BYTES: usize = 1_048_576; // 1 MiB
/// How long a seen message id suppresses redeliveries.
const DEDUP_TTL: Duration = Duration::from_secs(600);
/// Max tracked message ids before evicting an arbitrary oldest entry.
const DEDUP_MAX: usize = 1000;
/// Cap for `send_file` uploads.
const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;
/// Outbound text cap (char-based, so multi-byte text is never split).
const MAX_MESSAGE_CHARS: usize = 4000;

// ---------------------------------------------------------------------------
// Webhook payload parsing
// ---------------------------------------------------------------------------

/// One parsed Buzz inbound webhook payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuzzWebhookMessage {
    /// Buzz user id of the sender.
    pub user_id: String,
    /// Channel the message was posted in.
    pub channel_id: String,
    /// Message text.
    pub text: String,
    /// Platform message id (deduplication key), when provided.
    pub message_id: Option<String>,
    /// Whether this arrived as a DM.
    pub is_dm: bool,
}

/// Parse a Buzz webhook body.
///
/// Expected shape: `{ "user_id": "...", "channel_id": "...", "text": "...",
/// "message_id": "..." }`. Returns `None` when `text` is missing or blank;
/// other fields degrade to empty/defaults. An explicit `"is_dm": true` (or
/// `"channel_type": "dm"`) marks the message as a DM.
pub fn parse_webhook_message(body: &[u8]) -> Option<BuzzWebhookMessage> {
    let obj = serde_json::from_slice::<Value>(body)
        .ok()?
        .as_object()?
        .clone();
    let text = obj.get("text")?.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    let field = |k: &str| obj.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let is_dm = obj.get("is_dm").and_then(Value::as_bool).unwrap_or(false)
        || obj.get("channel_type").and_then(Value::as_str) == Some("dm");
    Some(BuzzWebhookMessage {
        user_id: field("user_id"),
        channel_id: field("channel_id"),
        text: text.to_string(),
        message_id: obj
            .get("message_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        is_dm,
    })
}

/// Convert a parsed webhook payload into a gateway [`IncomingMessage`].
pub fn incoming_message(msg: &BuzzWebhookMessage) -> IncomingMessage {
    IncomingMessage {
        platform: "buzz".to_string(),
        chat_id: msg.channel_id.clone(),
        user_id: msg.user_id.clone(),
        text: msg.text.clone(),
        message_id: msg.message_id.clone(),
        is_dm: msg.is_dm,
        thread_id: None,
    }
}

/// Verify the `X-Buzz-Signature` header: `sha256=<hex>` HMAC-SHA256 over the
/// raw body. A missing/malformed signature — or an empty secret — fails
/// closed.
pub fn verify_webhook_signature(secret: &str, body: &[u8], signature: &str) -> bool {
    if secret.trim().is_empty() {
        return false;
    }
    verify_hmac_sha256_signature(secret, body, signature.trim())
}

/// Truncate outbound text to the Buzz limit (char-based, never splitting a
/// multi-byte char), warning when it cuts.
fn truncate_text(text: &str) -> String {
    let truncated: String = text.chars().take(MAX_MESSAGE_CHARS).collect();
    if truncated.len() < text.len() {
        warn!(
            "buzz: truncating message from {} to {} chars (Buzz limit)",
            text.chars().count(),
            MAX_MESSAGE_CHARS
        );
    }
    truncated
}

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

struct BuzzInner {
    config: BuzzConfig,
    client: Client,
    inbound_tx: RwLock<Option<mpsc::Sender<IncomingMessage>>>,
    seen: RwLock<HashMap<String, Instant>>,
    stop: Notify,
    base: BasePlatformAdapter,
}

pub struct BuzzAdapter {
    inner: Arc<BuzzInner>,
    run_task: RwLock<Option<tokio::task::JoinHandle<()>>>,
}

impl BuzzAdapter {
    /// Create the adapter. Fails closed when `webhook_url` is empty (nothing
    /// can be sent) or `webhook_secret` is empty (inbound signatures cannot
    /// be verified).
    pub fn new(config: BuzzConfig, proxy: AdapterProxyConfig) -> Result<Self, GatewayError> {
        if config.webhook_url.trim().is_empty() {
            return Err(GatewayError::Platform(
                "buzz webhook_url must not be empty".into(),
            ));
        }
        if config.webhook_secret.trim().is_empty() {
            return Err(GatewayError::Auth(
                "buzz webhook_secret must not be empty (webhook signatures cannot be verified)"
                    .into(),
            ));
        }
        // Outbound auth is optional; the base token is informational only.
        let token = config.bot_token.clone().unwrap_or_default();
        let base = BasePlatformAdapter::new(token).with_proxy(proxy);
        let client = base.build_client()?;
        let inner = Arc::new(BuzzInner {
            config,
            client,
            inbound_tx: RwLock::new(None),
            seen: RwLock::new(HashMap::new()),
            stop: Notify::new(),
            base,
        });
        Ok(Self {
            inner,
            run_task: RwLock::new(None),
        })
    }

    pub fn config(&self) -> &BuzzConfig {
        &self.inner.config
    }

    /// Attach the gateway's inbound channel (mirrors the ntfy/line pattern).
    pub async fn set_inbound_sender(&self, tx: mpsc::Sender<IncomingMessage>) {
        *self.inner.inbound_tx.write().await = Some(tx);
    }

    #[cfg(test)]
    fn test_inner(&self) -> Arc<BuzzInner> {
        self.inner.clone()
    }

    /// Attach Bearer auth when `bot_token` is configured.
    fn authed(&self, req: RequestBuilder) -> RequestBuilder {
        match self
            .inner
            .config
            .bot_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(token) => req.bearer_auth(token),
            None => req,
        }
    }

    /// POST a text message to the Buzz API.
    async fn post_message(&self, channel_id: &str, text: &str) -> Result<(), GatewayError> {
        let url = format!(
            "{}/messages",
            self.inner.config.webhook_url.trim_end_matches('/')
        );
        let resp = self
            .authed(self.inner.client.post(&url))
            .json(&json!({
                "channel_id": channel_id,
                "text": truncate_text(text),
            }))
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("buzz send failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let detail: String = resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect();
            return Err(GatewayError::SendFailed(format!(
                "buzz send HTTP {status}: {detail}"
            )));
        }
        Ok(())
    }

    /// Webhook accept loop; runs until `stop` is notified.
    async fn serve_loop(inner: Arc<BuzzInner>) {
        let addr: SocketAddr = match inner.config.bind_address().parse() {
            Ok(a) => a,
            Err(e) => {
                error!(
                    "buzz: invalid bind address {}: {e}",
                    inner.config.bind_address()
                );
                inner.base.mark_stopped();
                return;
            }
        };
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                error!("buzz: webhook server failed to bind {addr}: {e}");
                inner.base.mark_stopped();
                return;
            }
        };
        info!("buzz: webhook server listening on {addr}");
        loop {
            tokio::select! {
                _ = inner.stop.notified() => {
                    info!("buzz: webhook server shutting down");
                    return;
                }
                accept = listener.accept() => match accept {
                    Ok((stream, peer)) => {
                        let inner = inner.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, peer, inner).await {
                                debug!("buzz: webhook connection error from {peer}: {e}");
                            }
                        });
                    }
                    Err(e) => warn!("buzz: webhook accept error: {e}"),
                },
            }
        }
    }
}

/// TTL-bounded duplicate suppression on platform message ids.
async fn is_duplicate(inner: &BuzzInner, msg_id: &str) -> bool {
    let mut seen = inner.seen.write().await;
    let now = Instant::now();
    seen.retain(|_, t| now.duration_since(*t) < DEDUP_TTL);
    if seen.contains_key(msg_id) {
        return true;
    }
    if seen.len() >= DEDUP_MAX {
        if let Some(oldest) = seen.keys().next().cloned() {
            seen.remove(&oldest);
        }
    }
    seen.insert(msg_id.to_string(), now);
    false
}

async fn forward_inbound(inner: &BuzzInner, msg: BuzzWebhookMessage) {
    debug!(
        "buzz: inbound message from {} in {}",
        msg.user_id, msg.channel_id
    );
    let incoming = incoming_message(&msg);
    if let Some(tx) = inner.inbound_tx.read().await.clone() {
        tokio::spawn(async move {
            let _ = tx.send(incoming).await;
        });
    }
}

/// Handle one webhook connection: verify the HMAC signature (fail-closed),
/// parse the JSON payload, deduplicate on `message_id`, forward inbound, and
/// acknowledge with 200.
async fn handle_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    inner: Arc<BuzzInner>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (request_line, headers, body) =
        match read_http_request(&mut stream, WEBHOOK_BODY_MAX_BYTES).await {
            Ok(t) => t,
            Err(e) => {
                debug!("buzz: failed to read webhook request from {peer}: {e}");
                write_http_response(
                    &mut stream,
                    400,
                    "application/json",
                    r#"{"error":"bad request"}"#,
                )
                .await?;
                return Ok(());
            }
        };
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let raw_path = parts.next().unwrap_or("/");
    let path = raw_path.split('?').next().unwrap_or("/");

    if method == "GET" {
        // Health check for load balancers / tunnel probes.
        write_http_response(
            &mut stream,
            200,
            "application/json",
            r#"{"status":"ok","platform":"buzz"}"#,
        )
        .await?;
        return Ok(());
    }
    if method != "POST" || path != inner.config.webhook_path {
        write_http_response(
            &mut stream,
            404,
            "application/json",
            r#"{"error":"not found"}"#,
        )
        .await?;
        return Ok(());
    }

    // Fail-closed: missing or wrong signature → reject without touching the body.
    let signature = headers
        .get(BUZZ_SIGNATURE_HEADER)
        .map(String::as_str)
        .unwrap_or("");
    if !verify_webhook_signature(&inner.config.webhook_secret, &body, signature) {
        warn!("buzz: rejecting webhook from {peer}: invalid signature");
        write_http_response(
            &mut stream,
            401,
            "application/json",
            r#"{"error":"invalid signature"}"#,
        )
        .await?;
        return Ok(());
    }

    let Some(msg) = parse_webhook_message(&body) else {
        write_http_response(
            &mut stream,
            400,
            "application/json",
            r#"{"error":"bad payload"}"#,
        )
        .await?;
        return Ok(());
    };

    // At-least-once redelivery guard on message_id.
    if let Some(id) = msg.message_id.as_deref() {
        if is_duplicate(&inner, id).await {
            debug!("buzz: ignoring duplicate webhook message {id}");
            write_http_response(&mut stream, 200, "application/json", r#"{"status":"ok"}"#).await?;
            return Ok(());
        }
    }

    forward_inbound(&inner, msg).await;
    write_http_response(&mut stream, 200, "application/json", r#"{"status":"ok"}"#).await?;
    Ok(())
}

#[async_trait]
impl PlatformAdapter for BuzzAdapter {
    async fn start(&self) -> Result<(), GatewayError> {
        info!(
            "buzz adapter starting: webhook on {} path {}",
            self.inner.config.bind_address(),
            self.inner.config.webhook_path
        );
        if self.run_task.read().await.is_some() {
            debug!("buzz: webhook server already running");
            return Ok(());
        }
        self.inner.base.mark_running();
        let inner = self.inner.clone();
        let handle = tokio::spawn(async move { Self::serve_loop(inner).await });
        *self.run_task.write().await = Some(handle);
        Ok(())
    }

    async fn stop(&self) -> Result<(), GatewayError> {
        info!("buzz adapter stopping");
        self.inner.base.mark_stopped();
        self.inner.stop.notify_waiters();
        if let Some(handle) = self.run_task.write().await.take() {
            handle.abort();
            let _ = handle.await;
        }
        Ok(())
    }

    async fn send_message(
        &self,
        chat_id: &str,
        text: &str,
        _parse_mode: Option<ParseMode>,
    ) -> Result<(), GatewayError> {
        let chat_id = chat_id.trim();
        if chat_id.is_empty() {
            return Err(GatewayError::SendFailed(
                "buzz: chat_id must not be empty".into(),
            ));
        }
        self.post_message(chat_id, text).await
    }

    async fn edit_message(
        &self,
        _chat_id: &str,
        _message_id: &str,
        _text: &str,
    ) -> Result<(), GatewayError> {
        debug!("buzz: message editing is not supported by the Buzz API");
        Ok(())
    }

    async fn send_file(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<(), GatewayError> {
        let chat_id = chat_id.trim();
        if chat_id.is_empty() {
            return Err(GatewayError::SendFailed(
                "buzz: chat_id must not be empty".into(),
            ));
        }
        let bytes = tokio::fs::read(file_path)
            .await
            .map_err(|e| GatewayError::SendFailed(format!("buzz: failed to read file: {e}")))?;
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err(GatewayError::SendFailed(format!(
                "buzz: file too large ({} bytes > {} byte limit)",
                bytes.len(),
                MAX_ATTACHMENT_BYTES
            )));
        }
        let path = std::path::Path::new(file_path);
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment")
            .to_string();
        let mime = path
            .extension()
            .and_then(|e| e.to_str())
            .map(mime_from_extension)
            .unwrap_or("application/octet-stream");
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename)
            .mime_str(mime)
            .map_err(|e| GatewayError::SendFailed(format!("buzz: invalid mime type: {e}")))?;
        let mut form = reqwest::multipart::Form::new()
            .text("channel_id", chat_id.to_string())
            .part("file", part);
        if let Some(caption) = caption.map(str::trim).filter(|s| !s.is_empty()) {
            form = form.text("caption", caption.to_string());
        }
        let url = format!(
            "{}/files",
            self.inner.config.webhook_url.trim_end_matches('/')
        );
        let resp = self
            .authed(self.inner.client.post(&url))
            .multipart(form)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("buzz file upload failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(GatewayError::SendFailed(format!(
                "buzz file upload HTTP {}",
                resp.status()
            )));
        }
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.inner.base.is_running()
    }

    fn platform_name(&self) -> &str {
        "buzz"
    }
}

// ---------------------------------------------------------------------------
// Tests (no live network beyond localhost loopback)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn test_config() -> BuzzConfig {
        BuzzConfig {
            webhook_url: "https://buzz.example.com/api".to_string(),
            bot_token: Some("bot-token-123".to_string()),
            webhook_secret: "test-secret".to_string(),
            bind_addr: "127.0.0.1".to_string(),
            webhook_port: 18765,
            webhook_path: "/buzz/webhook".to_string(),
            enabled: true,
        }
    }

    /// Compute the `sha256=<hex>` signature header value for a body.
    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
        mac.update(body);
        let hex: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("sha256={hex}")
    }

    // -- payload parsing --------------------------------------------------------

    #[test]
    fn parse_webhook_message_valid() {
        let body = br#"{"user_id":"U42","channel_id":"C7","text":"hello buzz","message_id":"m-1"}"#;
        let msg = parse_webhook_message(body).expect("parses");
        assert_eq!(msg.user_id, "U42");
        assert_eq!(msg.channel_id, "C7");
        assert_eq!(msg.text, "hello buzz");
        assert_eq!(msg.message_id.as_deref(), Some("m-1"));
        assert!(!msg.is_dm);
    }

    #[test]
    fn parse_webhook_message_trims_text_and_defaults_missing_fields() {
        let body = br#"{"text":"  padded  "}"#;
        let msg = parse_webhook_message(body).expect("parses");
        assert_eq!(msg.text, "padded");
        assert!(msg.user_id.is_empty());
        assert!(msg.channel_id.is_empty());
        assert!(msg.message_id.is_none());
        assert!(!msg.is_dm);
    }

    #[test]
    fn parse_webhook_message_rejects_missing_or_blank_text() {
        for body in [
            br#"{"user_id":"U1","channel_id":"C1"}"#.as_slice(),
            br#"{"text":""}"#.as_slice(),
            br#"{"text":"   "}"#.as_slice(),
            b"not json",
            b"[1,2,3]",
            b"",
        ] {
            assert!(parse_webhook_message(body).is_none(), "{body:?}");
        }
    }

    #[test]
    fn parse_webhook_message_dm_detection() {
        let dm = parse_webhook_message(br#"{"text":"hi","is_dm":true}"#).expect("parses");
        assert!(dm.is_dm);
        let dm_type =
            parse_webhook_message(br#"{"text":"hi","channel_type":"dm"}"#).expect("parses");
        assert!(dm_type.is_dm);
        let channel =
            parse_webhook_message(br#"{"text":"hi","channel_type":"channel"}"#).expect("parses");
        assert!(!channel.is_dm);
    }

    // -- signature verification -------------------------------------------------

    #[test]
    fn verify_webhook_signature_accepts_valid() {
        let body = br#"{"text":"hello"}"#;
        let sig = sign("test-secret", body);
        assert!(verify_webhook_signature("test-secret", body, &sig));
    }

    #[test]
    fn verify_webhook_signature_rejects_bad_or_missing() {
        let body = br#"{"text":"hello"}"#;
        let good = sign("test-secret", body);
        // wrong secret
        assert!(!verify_webhook_signature("other-secret", body, &good));
        // tampered body
        assert!(!verify_webhook_signature("test-secret", b"tampered", &good));
        // bare hex without the sha256= prefix
        let bare = good.strip_prefix("sha256=").unwrap().to_string();
        assert!(!verify_webhook_signature("test-secret", body, &bare));
        // garbage / empty
        assert!(!verify_webhook_signature("test-secret", body, "bogus"));
        assert!(!verify_webhook_signature("test-secret", body, ""));
        // empty secret always fails closed
        assert!(!verify_webhook_signature("", body, &good));
        assert!(!verify_webhook_signature("   ", body, &good));
    }

    // -- message conversion -----------------------------------------------------

    #[test]
    fn incoming_message_conversion() {
        let msg = BuzzWebhookMessage {
            user_id: "U42".to_string(),
            channel_id: "C7".to_string(),
            text: "hello buzz".to_string(),
            message_id: Some("m-1".to_string()),
            is_dm: true,
        };
        let incoming = incoming_message(&msg);
        assert_eq!(incoming.platform, "buzz");
        assert_eq!(incoming.chat_id, "C7");
        assert_eq!(incoming.user_id, "U42");
        assert_eq!(incoming.text, "hello buzz");
        assert_eq!(incoming.message_id.as_deref(), Some("m-1"));
        assert!(incoming.is_dm);
        assert!(incoming.thread_id.is_none());
    }

    // -- construction -----------------------------------------------------------

    #[test]
    fn new_rejects_empty_webhook_url() {
        let mut cfg = test_config();
        cfg.webhook_url = "   ".to_string();
        let result = BuzzAdapter::new(cfg, AdapterProxyConfig::default());
        assert!(result.is_err());
        assert!(matches!(
            result.err().expect("expected construction to fail"),
            GatewayError::Platform(_)
        ));
    }

    #[test]
    fn new_rejects_empty_webhook_secret() {
        let mut cfg = test_config();
        cfg.webhook_secret = String::new();
        let result = BuzzAdapter::new(cfg, AdapterProxyConfig::default());
        assert!(result.is_err());
        assert!(matches!(
            result.err().expect("expected construction to fail"),
            GatewayError::Auth(_)
        ));
    }

    #[test]
    fn new_accepts_valid_config() {
        let adapter =
            BuzzAdapter::new(test_config(), AdapterProxyConfig::default()).expect("valid config");
        assert_eq!(adapter.platform_name(), "buzz");
        assert!(!adapter.is_running());
        assert_eq!(adapter.config().webhook_port, 18765);
    }

    #[test]
    fn truncate_text_enforces_char_limit() {
        let short = "hello";
        assert_eq!(truncate_text(short), short);
        let exactly: String = "x".repeat(MAX_MESSAGE_CHARS);
        assert_eq!(truncate_text(&exactly).len(), MAX_MESSAGE_CHARS);
        let long: String = "y".repeat(MAX_MESSAGE_CHARS + 100);
        assert_eq!(truncate_text(&long).chars().count(), MAX_MESSAGE_CHARS);
        // multi-byte chars are never split
        let emoji: String = "🚀".repeat(MAX_MESSAGE_CHARS + 10);
        let truncated = truncate_text(&emoji);
        assert_eq!(truncated.chars().count(), MAX_MESSAGE_CHARS);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    // -- live loopback dispatch (localhost only, no external network) -----------

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
            request.push_str(&format!("X-Buzz-Signature: {sig}\r\n"));
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

    async fn spawn_test_server(inner: Arc<BuzzInner>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
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
    async fn webhook_dispatches_message_to_inbound_channel() {
        let adapter =
            BuzzAdapter::new(test_config(), AdapterProxyConfig::default()).expect("valid config");
        let (tx, mut rx) = mpsc::channel::<IncomingMessage>(16);
        adapter.set_inbound_sender(tx).await;
        let inner = adapter.test_inner();

        let (addr, server) = spawn_test_server(inner).await;
        let body = br#"{"user_id":"U42","channel_id":"C7","text":"hello buzz","message_id":"m-1"}"#;
        let sig = sign("test-secret", body);
        let resp = post_raw(addr, "/buzz/webhook", body, Some(&sig)).await;
        server.await.expect("server task");

        assert!(resp.starts_with("HTTP/1.1 200 OK"), "resp was: {resp}");

        let msg = rx.recv().await.expect("inbound message");
        assert_eq!(msg.platform, "buzz");
        assert_eq!(msg.chat_id, "C7");
        assert_eq!(msg.user_id, "U42");
        assert_eq!(msg.text, "hello buzz");
        assert_eq!(msg.message_id.as_deref(), Some("m-1"));
        assert!(!msg.is_dm);
    }

    #[tokio::test]
    async fn webhook_rejects_bad_signature_with_401() {
        let adapter =
            BuzzAdapter::new(test_config(), AdapterProxyConfig::default()).expect("valid config");
        let (tx, mut rx) = mpsc::channel::<IncomingMessage>(16);
        adapter.set_inbound_sender(tx).await;
        let inner = adapter.test_inner();

        let (addr, server) = spawn_test_server(inner).await;
        let body = br#"{"user_id":"U42","channel_id":"C7","text":"hello buzz"}"#;
        let resp = post_raw(addr, "/buzz/webhook", body, Some("sha256=deadbeef")).await;
        server.await.expect("server task");

        assert!(resp.starts_with("HTTP/1.1 401"), "resp was: {resp}");
        assert!(rx.try_recv().is_err(), "no message should be forwarded");
    }

    #[tokio::test]
    async fn webhook_rejects_missing_signature_with_401() {
        let adapter =
            BuzzAdapter::new(test_config(), AdapterProxyConfig::default()).expect("valid config");
        let (tx, mut rx) = mpsc::channel::<IncomingMessage>(16);
        adapter.set_inbound_sender(tx).await;
        let inner = adapter.test_inner();

        let (addr, server) = spawn_test_server(inner).await;
        let body = br#"{"user_id":"U42","channel_id":"C7","text":"hello buzz"}"#;
        let resp = post_raw(addr, "/buzz/webhook", body, None).await;
        server.await.expect("server task");

        assert!(resp.starts_with("HTTP/1.1 401"), "resp was: {resp}");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn webhook_health_check_get_returns_200() {
        let adapter =
            BuzzAdapter::new(test_config(), AdapterProxyConfig::default()).expect("valid config");
        let inner = adapter.test_inner();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.expect("accept");
            handle_connection(stream, peer, inner)
                .await
                .expect("handle");
        });

        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(b"GET /buzz/webhook HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("write");
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).await.expect("read");
        server.await.expect("server task");

        let resp = String::from_utf8_lossy(&resp);
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "resp was: {resp}");
        assert!(resp.contains(r#""platform":"buzz""#));
    }

    #[tokio::test]
    async fn dedup_suppresses_repeat_message_ids() {
        let adapter =
            BuzzAdapter::new(test_config(), AdapterProxyConfig::default()).expect("valid config");
        let inner = adapter.test_inner();
        assert!(!is_duplicate(&inner, "m-1").await);
        assert!(is_duplicate(&inner, "m-1").await);
        assert!(!is_duplicate(&inner, "m-2").await);
    }
}
