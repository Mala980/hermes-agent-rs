//! ntfy push-notification adapter.
//!
//! Inbound: subscribes to `GET {server}/{topic}/json` (line-delimited JSON
//! stream, `poll=false`) and forwards each `message` event to the gateway via
//! an `mpsc` inbound channel. The stream reconnects with backoff; HTTP 401/404
//! are fatal and stop the loop.
//!
//! Outbound: publishes with `POST {server}/{topic}` (attachments via `PUT`
//! with a `Filename` header).
//!
//! ntfy has no sender identity, so echo-loop prevention relies on a tag
//! (`echo_tag`, default `hermes-agent`) stamped on outgoing messages and
//! dropped on inbound ones. Each topic is treated as one trusted channel
//! (`user_id == topic`; the publisher-controlled `title` must never drive
//! authorization), so protect private topics with a read/write token on the
//! server side.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::Client;
use tokio::sync::{mpsc, Notify, RwLock};
use tracing::{debug, error, info, warn};

use hermes_config::NtfyConfig;
use hermes_core::errors::GatewayError;
use hermes_core::traits::{ParseMode, PlatformAdapter};

use crate::adapter::{AdapterProxyConfig, BasePlatformAdapter};
use crate::gateway::IncomingMessage;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// ntfy per-message body limit (chars).
pub const MAX_MESSAGE_LENGTH: usize = 4096;
/// How long a seen message id suppresses duplicates.
const DEDUP_TTL: Duration = Duration::from_secs(300);
/// Max tracked message ids before evicting an arbitrary oldest entry.
const DEDUP_MAX: usize = 1000;
/// Reconnect backoff schedule (seconds); resets after 60 s of healthy uptime.
const RECONNECT_BACKOFF_SECS: [u64; 5] = [2, 5, 10, 30, 60];
/// Cap for `send_file` attachments.
const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Stream event parsing
// ---------------------------------------------------------------------------

/// One parsed line from the ntfy `/json` stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NtfyEvent {
    /// Event kind: `open`, `keepalive`, `message`, ...
    pub event: String,
    /// Server-assigned message id (deduplication key).
    pub id: Option<String>,
    /// Topic the message was published to.
    pub topic: Option<String>,
    /// Notification title (publisher-controlled; never used for auth).
    pub title: Option<String>,
    /// Message body.
    pub message: Option<String>,
    /// Tags attached at publish time.
    pub tags: Vec<String>,
    /// Publish time (unix seconds).
    pub time: Option<i64>,
}

/// Parse a single line of the ntfy `/json` stream.
///
/// Accepts raw JSON lines and SSE `data: {...}` payloads (the same logical
/// format ntfy serves on `/{topic}/sse`); SSE `event: <name>` framing lines
/// and anything unparsable return `None`.
pub fn parse_stream_line(line: &str) -> Option<NtfyEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let payload = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
    if payload.starts_with("event:") {
        return None;
    }
    let obj = serde_json::from_str::<serde_json::Value>(payload)
        .ok()?
        .as_object()?
        .clone();
    let field = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(str::to_string);
    Some(NtfyEvent {
        event: field("event").unwrap_or_default(),
        id: field("id"),
        topic: field("topic"),
        title: field("title"),
        message: field("message"),
        tags: obj
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        time: obj.get("time").and_then(|v| v.as_i64()),
    })
}

/// Decide whether an event becomes an inbound message.
///
/// Only `message` events with non-empty bodies qualify. Messages carrying our
/// echo tag are our own publishes (ntfy exposes no sender id) and are dropped.
pub fn inbound_text(event: &NtfyEvent, echo_tag: &str) -> Option<String> {
    if event.event != "message" {
        return None;
    }
    if !echo_tag.is_empty() && event.tags.iter().any(|t| t == echo_tag) {
        debug!("ntfy: skipping own message (echo tag)");
        return None;
    }
    let text = event.message.as_deref().unwrap_or("").trim();
    if text.is_empty() {
        return None;
    }
    Some(text.to_string())
}

// ---------------------------------------------------------------------------
// Auth + publish request construction
// ---------------------------------------------------------------------------

/// Build the `Authorization` header value from ntfy credentials.
///
/// ntfy accepts a Bearer token (`access_token`) or Basic auth. When
/// `username`+`password` are both set they win; otherwise a `user:pass`-shaped
/// `access_token` becomes Basic and any other token becomes Bearer.
pub fn build_auth_header(
    username: Option<&str>,
    password: Option<&str>,
    access_token: Option<&str>,
) -> Option<String> {
    if let (Some(u), Some(p)) = (username, password) {
        let (u, p) = (u.trim(), p.trim());
        if !u.is_empty() && !p.is_empty() {
            let creds = base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"));
            return Some(format!("Basic {creds}"));
        }
    }
    let token = access_token?.trim();
    if token.is_empty() {
        return None;
    }
    if token.contains(':') {
        let creds = base64::engine::general_purpose::STANDARD.encode(token);
        Some(format!("Basic {creds}"))
    } else {
        Some(format!("Bearer {token}"))
    }
}

/// Encode a header value, falling back to RFC 2047 `=?UTF-8?B?...?=` for
/// non-ASCII text (ntfy decodes RFC 2047 on `X-Title`/`X-Message`/`X-Tags`).
fn header_value_rfc2047(s: &str) -> Option<HeaderValue> {
    if s.is_ascii() {
        return HeaderValue::from_str(s).ok();
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
    HeaderValue::from_str(&format!("=?UTF-8?B?{b64}?=")).ok()
}

/// Headers for a publish POST: plain-text body, the echo tag, optional title
/// (`X-Title`), optional Markdown rendering, optional auth.
pub fn publish_headers(config: &NtfyConfig) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    let echo_tag = config.echo_tag.trim();
    if !echo_tag.is_empty() {
        if let Some(v) = header_value_rfc2047(echo_tag) {
            headers.insert("X-Tags", v);
        }
    }
    if let Some(title) = config
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Some(v) = header_value_rfc2047(title) {
            headers.insert("X-Title", v);
        }
    }
    if config.markdown {
        headers.insert("Markdown", HeaderValue::from_static("yes"));
    }
    if let Some(auth) = build_auth_header(
        config.username.as_deref(),
        config.password.as_deref(),
        config.access_token.as_deref(),
    ) {
        if let Ok(v) = HeaderValue::from_str(&auth) {
            headers.insert("Authorization", v);
        }
    }
    headers
}

/// Truncate a message body to the ntfy limit (char-based, so multi-byte text
/// is never split), warning when it cuts.
pub fn truncate_body(text: &str) -> String {
    let truncated: String = text.chars().take(MAX_MESSAGE_LENGTH).collect();
    if truncated.len() < text.len() {
        warn!(
            "ntfy: truncating message from {} to {} chars (ntfy limit)",
            text.chars().count(),
            MAX_MESSAGE_LENGTH
        );
    }
    truncated
}

/// Subscription URL for the `/json` stream.
pub fn subscribe_url(config: &NtfyConfig) -> String {
    format!(
        "{}/{}/json",
        config.server.trim_end_matches('/'),
        config.topic
    )
}

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

struct NtfyInner {
    config: NtfyConfig,
    client: Client,
    inbound_tx: RwLock<Option<mpsc::Sender<IncomingMessage>>>,
    seen: RwLock<HashMap<String, Instant>>,
    stop: Notify,
    base: BasePlatformAdapter,
}

pub struct NtfyAdapter {
    inner: Arc<NtfyInner>,
    run_task: RwLock<Option<tokio::task::JoinHandle<()>>>,
}

impl NtfyAdapter {
    /// Create the adapter. Fails when `topic` is empty (fail-closed).
    pub fn new(config: NtfyConfig, proxy: AdapterProxyConfig) -> Result<Self, GatewayError> {
        if config.topic.trim().is_empty() {
            return Err(GatewayError::Platform(
                "ntfy topic must not be empty".into(),
            ));
        }
        // ntfy auth is optional; the base token is informational only.
        let token = config.access_token.clone().unwrap_or_default();
        let base = BasePlatformAdapter::new(token).with_proxy(proxy);
        let client = base.build_client()?;
        let inner = Arc::new(NtfyInner {
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

    pub fn config(&self) -> &NtfyConfig {
        &self.inner.config
    }

    /// Attach the gateway's inbound channel (mirrors the weixin/irc pattern).
    pub async fn set_inbound_sender(&self, tx: mpsc::Sender<IncomingMessage>) {
        *self.inner.inbound_tx.write().await = Some(tx);
    }

    /// Outcome of one streaming connection attempt.
    async fn consume_stream(inner: &NtfyInner) -> StreamOutcome {
        let url = subscribe_url(&inner.config);
        let mut req = inner.client.get(&url);
        if let Some(auth) = build_auth_header(
            inner.config.username.as_deref(),
            inner.config.password.as_deref(),
            inner.config.access_token.as_deref(),
        ) {
            req = req.header("Authorization", auth);
        }
        // `poll=false` keeps one long-lived connection alive with keepalive events.
        let resp = match req.query(&[("poll", "false")]).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!("ntfy: subscribe request failed: {e}");
                return StreamOutcome::Retry;
            }
        };
        match resp.status().as_u16() {
            401 => {
                error!("ntfy: authentication failed (401) — stopping subscribe loop; check the access token");
                inner.base.mark_stopped();
                return StreamOutcome::Stopped;
            }
            404 => {
                error!(
                    "ntfy: topic not found (404): {} — stopping subscribe loop",
                    inner.config.topic
                );
                inner.base.mark_stopped();
                return StreamOutcome::Stopped;
            }
            s if !(200..300).contains(&s) => {
                warn!("ntfy: subscribe failed with HTTP {s}");
                return StreamOutcome::Retry;
            }
            _ => {}
        }

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        loop {
            tokio::select! {
                _ = inner.stop.notified() => return StreamOutcome::Stopped,
                chunk = stream.next() => match chunk {
                    Some(Ok(bytes)) => {
                        buf.extend_from_slice(&bytes);
                        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                            let line: Vec<u8> = buf.drain(..=nl).collect();
                            Self::handle_line(inner, &line).await;
                        }
                    }
                    Some(Err(e)) => {
                        warn!("ntfy: stream read error: {e}");
                        return StreamOutcome::Retry;
                    }
                    None => {
                        debug!("ntfy: server closed the stream");
                        return StreamOutcome::Retry;
                    }
                },
            }
        }
    }

    /// Subscribe loop with reconnect backoff; resets after 60 s of uptime.
    async fn subscribe_loop(inner: Arc<NtfyInner>) {
        let mut backoff_idx = 0usize;
        while inner.base.is_running() {
            debug!("ntfy: opening subscription stream");
            let started = Instant::now();
            match Self::consume_stream(&inner).await {
                StreamOutcome::Stopped => return,
                StreamOutcome::Retry => {}
            }
            if !inner.base.is_running() {
                return;
            }
            if started.elapsed() >= Duration::from_secs(60) {
                backoff_idx = 0;
            }
            let delay = RECONNECT_BACKOFF_SECS[backoff_idx.min(RECONNECT_BACKOFF_SECS.len() - 1)];
            info!("ntfy: stream ended, reconnecting in {delay}s");
            tokio::select! {
                _ = inner.stop.notified() => return,
                _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
            }
            backoff_idx += 1;
        }
    }

    /// Handle one raw stream line: parse, filter, dedupe, forward inbound.
    async fn handle_line(inner: &NtfyInner, line: &[u8]) {
        let line = String::from_utf8_lossy(line);
        let Some(event) = parse_stream_line(&line) else {
            return;
        };
        let Some(text) = inbound_text(&event, &inner.config.echo_tag) else {
            return;
        };
        let msg_id = event.id.clone();
        if let Some(id) = msg_id.as_deref() {
            if Self::is_duplicate(inner, id).await {
                debug!("ntfy: duplicate message {id}, skipping");
                return;
            }
        }
        // ntfy exposes no sender identity: the topic is the channel identity
        // (the publisher-controlled `title` must never drive authorization).
        let topic = event
            .topic
            .clone()
            .unwrap_or_else(|| inner.config.topic.clone());
        debug!("ntfy: inbound message on topic {topic}");
        let incoming = IncomingMessage {
            platform: "ntfy".to_string(),
            chat_id: topic,
            user_id: inner.config.topic.clone(),
            text,
            message_id: msg_id,
            is_dm: true,
            thread_id: None,
        };
        if let Some(tx) = inner.inbound_tx.read().await.clone() {
            tokio::spawn(async move {
                let _ = tx.send(incoming).await;
            });
        }
    }

    /// TTL-bounded duplicate suppression on server message ids.
    async fn is_duplicate(inner: &NtfyInner, msg_id: &str) -> bool {
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

    /// Publish a text body to a topic.
    async fn publish(inner: &NtfyInner, topic: &str, text: &str) -> Result<(), GatewayError> {
        let body = truncate_body(text);
        let url = format!("{}/{}", inner.config.server.trim_end_matches('/'), topic);
        let resp = inner
            .client
            .post(&url)
            .headers(publish_headers(&inner.config))
            .body(body)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("ntfy publish failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let detail = resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect::<String>();
            return Err(GatewayError::SendFailed(format!(
                "ntfy publish HTTP {status}: {detail}"
            )));
        }
        Ok(())
    }

    /// Resolve the publish target: `chat_id` selects the topic (inbound
    /// messages carry the topic as `chat_id`, so replies echo there); an
    /// empty `chat_id` falls back to the configured publish topic.
    fn publish_topic(&self, chat_id: &str) -> String {
        let chat_id = chat_id.trim();
        if chat_id.is_empty() {
            self.inner.config.effective_publish_topic().to_string()
        } else {
            chat_id.to_string()
        }
    }
}

/// Internal outcome of one streaming connection attempt.
enum StreamOutcome {
    /// Stop requested or fatal error — do not reconnect.
    Stopped,
    /// Transient failure — reconnect with backoff.
    Retry,
}

#[async_trait]
impl PlatformAdapter for NtfyAdapter {
    async fn start(&self) -> Result<(), GatewayError> {
        info!(
            "ntfy adapter starting (server: {}, topic: {})",
            self.inner.config.server, self.inner.config.topic
        );
        if self.run_task.read().await.is_some() {
            debug!("ntfy: subscribe loop already running");
            return Ok(());
        }
        self.inner.base.mark_running();
        let inner = self.inner.clone();
        let handle = tokio::spawn(async move { Self::subscribe_loop(inner).await });
        *self.run_task.write().await = Some(handle);
        Ok(())
    }

    async fn stop(&self) -> Result<(), GatewayError> {
        info!("ntfy adapter stopping");
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
        let topic = self.publish_topic(chat_id);
        Self::publish(&self.inner, &topic, text).await
    }

    async fn edit_message(
        &self,
        _chat_id: &str,
        _message_id: &str,
        _text: &str,
    ) -> Result<(), GatewayError> {
        debug!("ntfy: message editing is not supported by the ntfy API");
        Ok(())
    }

    fn max_edit_chars(&self) -> usize {
        // ntfy truncates bodies at 4096 chars; the streaming manager should
        // split long responses into separate notifications instead.
        MAX_MESSAGE_LENGTH
    }

    async fn send_file(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<(), GatewayError> {
        let bytes = tokio::fs::read(file_path)
            .await
            .map_err(|e| GatewayError::SendFailed(format!("ntfy: failed to read file: {e}")))?;
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err(GatewayError::SendFailed(format!(
                "ntfy: file too large ({} bytes > {} byte limit)",
                bytes.len(),
                MAX_ATTACHMENT_BYTES
            )));
        }
        let topic = self.publish_topic(chat_id);
        let filename = std::path::Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment");
        let url = format!(
            "{}/{}",
            self.inner.config.server.trim_end_matches('/'),
            topic
        );
        // Attachment publish: file bytes as the PUT body, name via `Filename`,
        // message via `X-Message`. Drop the text content-type for binary data.
        let mut headers = publish_headers(&self.inner.config);
        headers.remove("Content-Type");
        if let Some(v) = header_value_rfc2047(filename) {
            headers.insert("Filename", v);
        }
        if let Some(caption) = caption.map(str::trim).filter(|s| !s.is_empty()) {
            if let Some(v) = header_value_rfc2047(&truncate_body(caption)) {
                headers.insert("X-Message", v);
            }
        }
        let resp = self
            .inner
            .client
            .put(&url)
            .headers(headers)
            .body(bytes)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("ntfy attachment upload failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(GatewayError::SendFailed(format!(
                "ntfy attachment upload HTTP {}",
                resp.status()
            )));
        }
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.inner.base.is_running()
    }

    fn platform_name(&self) -> &str {
        "ntfy"
    }
}

// ---------------------------------------------------------------------------
// Tests (no live network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> NtfyConfig {
        NtfyConfig {
            server: "https://ntfy.sh".to_string(),
            topic: "test-topic".to_string(),
            publish_topic: None,
            access_token: None,
            username: None,
            password: None,
            title: None,
            markdown: false,
            echo_tag: "hermes-agent".to_string(),
        }
    }

    fn message_event_json() -> &'static str {
        r#"{"id":"abc123","time":1700000000,"expires":1700086400,"event":"message","topic":"test-topic","message":"hello world","title":"Someone","tags":["info"],"priority":3}"#
    }

    #[test]
    fn parse_message_event() {
        let event = parse_stream_line(message_event_json()).expect("parses");
        assert_eq!(event.event, "message");
        assert_eq!(event.id.as_deref(), Some("abc123"));
        assert_eq!(event.topic.as_deref(), Some("test-topic"));
        assert_eq!(event.message.as_deref(), Some("hello world"));
        assert_eq!(event.title.as_deref(), Some("Someone"));
        assert_eq!(event.tags, vec!["info".to_string()]);
        assert_eq!(event.time, Some(1700000000));
    }

    #[test]
    fn parse_keepalive_and_open_events() {
        let keepalive = parse_stream_line(r#"{"event":"keepalive","id":"x","time":1700000001}"#)
            .expect("parses");
        assert_eq!(keepalive.event, "keepalive");
        assert!(inbound_text(&keepalive, "hermes-agent").is_none());

        let open = parse_stream_line(r#"{"id":"y","time":1700000002,"event":"open","topic":"t"}"#)
            .expect("parses");
        assert!(inbound_text(&open, "hermes-agent").is_none());
    }

    #[test]
    fn parse_sse_data_prefixed_line() {
        let event = parse_stream_line(&format!("data: {}", message_event_json())).expect("parses");
        assert_eq!(event.event, "message");
        assert_eq!(event.message.as_deref(), Some("hello world"));
    }

    #[test]
    fn parse_rejects_garbage_and_framing_lines() {
        assert!(parse_stream_line("").is_none());
        assert!(parse_stream_line("   ").is_none());
        assert!(parse_stream_line("event: message").is_none());
        assert!(parse_stream_line("not json at all").is_none());
        assert!(parse_stream_line("[1,2,3]").is_none());
    }

    #[test]
    fn inbound_text_accepts_plain_message() {
        let event = parse_stream_line(message_event_json()).expect("parses");
        assert_eq!(
            inbound_text(&event, "hermes-agent").as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn inbound_text_skips_echo_tagged_message() {
        let event = parse_stream_line(
            r#"{"id":"own1","event":"message","topic":"t","message":"bot reply","tags":["hermes-agent"]}"#,
        )
        .expect("parses");
        assert!(inbound_text(&event, "hermes-agent").is_none());
        // a different echo tag does not filter
        assert!(inbound_text(&event, "other-tag").is_some());
    }

    #[test]
    fn inbound_text_skips_empty_body() {
        for body in [
            r#"{"id":"e1","event":"message","topic":"t","message":""}"#,
            r#"{"id":"e2","event":"message","topic":"t","message":"   "}"#,
            r#"{"id":"e3","event":"message","topic":"t"}"#,
        ] {
            let event = parse_stream_line(body).expect("parses");
            assert!(inbound_text(&event, "hermes-agent").is_none(), "{body}");
        }
    }

    #[test]
    fn auth_header_bearer_token() {
        assert_eq!(
            build_auth_header(None, None, Some("tok_123")).as_deref(),
            Some("Bearer tok_123")
        );
        // surrounding whitespace is stripped
        assert_eq!(
            build_auth_header(None, None, Some("  tok_123\n")).as_deref(),
            Some("Bearer tok_123")
        );
    }

    #[test]
    fn auth_header_basic_from_user_pass_token() {
        // "user:pass" -> Basic base64("user:pass")
        assert_eq!(
            build_auth_header(None, None, Some("user:pass")).as_deref(),
            Some("Basic dXNlcjpwYXNz")
        );
    }

    #[test]
    fn auth_header_username_password_wins_over_token() {
        assert_eq!(
            build_auth_header(Some("u"), Some("p"), Some("tok_123")).as_deref(),
            Some("Basic dTpw")
        );
    }

    #[test]
    fn auth_header_none_when_unset() {
        assert!(build_auth_header(None, None, None).is_none());
        assert!(build_auth_header(None, None, Some("   ")).is_none());
        assert!(build_auth_header(Some("u"), None, None).is_none());
    }

    #[test]
    fn publish_headers_defaults() {
        let headers = publish_headers(&test_config());
        assert_eq!(
            headers.get("X-Tags").map(|v| v.to_str().unwrap()),
            Some("hermes-agent")
        );
        assert_eq!(
            headers.get("Content-Type").map(|v| v.to_str().unwrap()),
            Some("text/plain; charset=utf-8")
        );
        assert!(headers.get("X-Title").is_none());
        assert!(headers.get("Markdown").is_none());
        assert!(headers.get("Authorization").is_none());
    }

    #[test]
    fn publish_headers_title_markdown_and_auth() {
        let mut cfg = test_config();
        cfg.title = Some("Deploy done".to_string());
        cfg.markdown = true;
        cfg.access_token = Some("tok_123".to_string());
        let headers = publish_headers(&cfg);
        assert_eq!(
            headers.get("X-Title").map(|v| v.to_str().unwrap()),
            Some("Deploy done")
        );
        assert_eq!(
            headers.get("Markdown").map(|v| v.to_str().unwrap()),
            Some("yes")
        );
        assert_eq!(
            headers.get("Authorization").map(|v| v.to_str().unwrap()),
            Some("Bearer tok_123")
        );
    }

    #[test]
    fn publish_headers_unicode_title_rfc2047() {
        let mut cfg = test_config();
        cfg.title = Some("Selesai ✅".to_string());
        let headers = publish_headers(&cfg);
        let value = headers
            .get("X-Title")
            .expect("title header")
            .to_str()
            .unwrap();
        assert!(
            value.starts_with("=?UTF-8?B?") && value.ends_with("?="),
            "unexpected encoding: {value}"
        );
        // round-trips back to the original title
        let b64 = value
            .strip_prefix("=?UTF-8?B?")
            .unwrap()
            .strip_suffix("?=")
            .unwrap();
        let decoded = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(decoded, "Selesai ✅");
    }

    #[test]
    fn truncate_body_enforces_limit() {
        let short = "hello";
        assert_eq!(truncate_body(short), short);
        let exactly: String = "x".repeat(MAX_MESSAGE_LENGTH);
        assert_eq!(truncate_body(&exactly).len(), MAX_MESSAGE_LENGTH);
        let long: String = "y".repeat(MAX_MESSAGE_LENGTH + 100);
        assert_eq!(truncate_body(&long).chars().count(), MAX_MESSAGE_LENGTH);
        // multi-byte chars are never split
        let emoji: String = "🚀".repeat(MAX_MESSAGE_LENGTH + 10);
        let truncated = truncate_body(&emoji);
        assert_eq!(truncated.chars().count(), MAX_MESSAGE_LENGTH);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn subscribe_url_format() {
        let mut cfg = test_config();
        cfg.server = "https://ntfy.example.com/".to_string();
        assert_eq!(
            subscribe_url(&cfg),
            "https://ntfy.example.com/test-topic/json"
        );
    }

    #[test]
    fn new_rejects_empty_topic() {
        let mut cfg = test_config();
        cfg.topic = "   ".to_string();
        let result = NtfyAdapter::new(cfg, AdapterProxyConfig::default());
        assert!(result.is_err());
        let err = result.err().expect("expected construction to fail");
        assert!(matches!(err, GatewayError::Platform(_)));
    }

    #[test]
    fn publish_topic_selection() {
        let adapter =
            NtfyAdapter::new(test_config(), AdapterProxyConfig::default()).expect("adapter");
        // chat_id selects the topic (inbound chat_id == topic)
        assert_eq!(adapter.publish_topic("alerts"), "alerts");
        // empty chat_id falls back to the configured topic
        assert_eq!(adapter.publish_topic(""), "test-topic");
        assert_eq!(adapter.publish_topic("  "), "test-topic");

        let mut cfg = test_config();
        cfg.publish_topic = Some("replies".to_string());
        let adapter = NtfyAdapter::new(cfg, AdapterProxyConfig::default()).expect("adapter");
        assert_eq!(adapter.publish_topic(""), "replies");
        assert_eq!(adapter.publish_topic("alerts"), "alerts");
    }
}
