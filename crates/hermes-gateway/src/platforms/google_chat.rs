//! Google Chat platform adapter.
//!
//! Inbound: a local HTTP webhook listener (tokio `TcpListener` on
//! `bind_addr:webhook_port`) receives Google Chat event JSON via POST:
//! `MESSAGE`, `ADDED_TO_SPACE`, `REMOVED_FROM_SPACE`, `CARD_CLICKED`.
//! Every request must carry `Authorization: Bearer <verification_token>`;
//! missing or wrong tokens are rejected with HTTP 401, and when no
//! verification token is configured all inbound webhooks are rejected
//! (fail-closed). `MESSAGE` events from human senders are converted to
//! [`IncomingMessage`] and forwarded to the gateway through an `mpsc`
//! inbound channel. Messages sent by the bot itself (`sender.type == "BOT"`)
//! are dropped for echo-loop prevention.
//!
//! Outbound: `POST {webhook_url}/spaces/{space}/messages` with
//! `Authorization: Bearer <bot_token>` and a JSON `{"text": ...}` body.
//! `bot_token` is a service-account OAuth2 access token in production; the
//! adapter makes no attempt to mint one itself. `chat_id` is the space
//! resource name (`spaces/AAA`); a bare space id (`AAA`) is accepted too.
//! Long messages are split at 4096 chars (the Chat API limit).
//!
//! Gaps vs. the Chat API: message edits use `PATCH {message.name}`
//! (the resource name carried in `IncomingMessage.message_id`); threaded
//! replies fall back to plain space messages (the default
//! `send_message_threaded`); file attachments are not supported (the Chat
//! API needs a Drive upload + multipart create, out of scope here) and
//! `send_file` returns a descriptive error.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Notify, RwLock};
use tracing::{debug, info, warn};

use hermes_config::GoogleChatConfig;
use hermes_core::errors::GatewayError;
use hermes_core::traits::{ParseMode, PlatformAdapter};

use super::helpers::{
    constant_time_eq, read_http_request, split_long_message, write_http_response,
};
use crate::adapter::{AdapterProxyConfig, BasePlatformAdapter};
use crate::gateway::IncomingMessage;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Google Chat message text limit (chars).
pub const MAX_MESSAGE_CHARS: usize = 4096;
/// Max inbound webhook request size (headers + body).
const MAX_WEBHOOK_BYTES: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// Google Chat event JSON
// ---------------------------------------------------------------------------

/// A Google Chat space (`spaces/AAA`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ChatSpace {
    /// Resource name, e.g. `spaces/AAA`.
    pub name: String,
    /// `ROOM`, `DM`, or `GROUP_CHAT`.
    #[serde(rename = "type")]
    pub space_type: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
}

/// A Google Chat user (`users/123`); also used for the message sender.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ChatUser {
    /// Resource name, e.g. `users/123`.
    pub name: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    /// `HUMAN` or `BOT`.
    #[serde(rename = "type")]
    pub user_type: String,
}

/// A Google Chat thread (`spaces/AAA/threads/CCC`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ChatThread {
    /// Resource name, e.g. `spaces/AAA/threads/CCC`.
    pub name: String,
}

/// A Google Chat message.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ChatMessage {
    /// Resource name, e.g. `spaces/AAA/messages/BBB`.
    pub name: String,
    pub sender: ChatUser,
    /// Full text including any `@bot` mention.
    pub text: String,
    /// Text with the bot mention removed (set by Google Chat).
    #[serde(rename = "argumentText")]
    pub argument_text: String,
    pub thread: Option<ChatThread>,
    pub space: ChatSpace,
}

/// Top-level Google Chat event pushed to the webhook.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GoogleChatEvent {
    /// `MESSAGE`, `ADDED_TO_SPACE`, `REMOVED_FROM_SPACE`, `CARD_CLICKED`.
    #[serde(rename = "type")]
    pub event_type: String,
    pub message: Option<ChatMessage>,
    pub user: ChatUser,
    pub space: ChatSpace,
}

/// Parse a Google Chat event from a webhook request body.
///
/// Returns `None` for empty bodies, invalid JSON, non-object JSON, or
/// payloads without an event `type`.
pub fn parse_chat_event(body: &[u8]) -> Option<GoogleChatEvent> {
    if body.is_empty() {
        return None;
    }
    let event = serde_json::from_slice::<GoogleChatEvent>(body).ok()?;
    if event.event_type.trim().is_empty() {
        return None;
    }
    Some(event)
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/// Verify the inbound webhook's `Authorization` header against the configured
/// verification token.
///
/// Fail-closed: an empty `configured` token never verifies, and any missing
/// or malformed header is rejected. Comparison is constant-time.
pub fn verify_inbound_token(authorization: Option<&str>, configured: &str) -> bool {
    let configured = configured.trim();
    if configured.is_empty() {
        return false;
    }
    let header = authorization.unwrap_or("").trim();
    let Some((scheme, credentials)) = header.split_once(char::is_whitespace) else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("bearer") {
        return false;
    }
    let presented = credentials.trim();
    if presented.is_empty() {
        return false;
    }
    constant_time_eq(presented.as_bytes(), configured.as_bytes())
}

// ---------------------------------------------------------------------------
// Event -> IncomingMessage
// ---------------------------------------------------------------------------

/// Strip a leading `@mention` from message text.
///
/// Google Chat prepends `@BotName` to DMs/mentions; when `argumentText` is
/// absent this best-effort fallback removes it so the agent never sees the
/// mention. Returns `""` when the text was only a mention.
pub fn strip_bot_mention(text: &str) -> &str {
    let trimmed = text.trim_start();
    let Some(rest) = trimmed.strip_prefix('@') else {
        return trimmed;
    };
    match rest.find(char::is_whitespace) {
        Some(idx) => rest[idx..].trim_start(),
        None => "",
    }
}

/// Convert a Google Chat event into a gateway [`IncomingMessage`].
///
/// Only `MESSAGE` events from human senders with non-empty text qualify;
/// everything else (`ADDED_TO_SPACE`, `REMOVED_FROM_SPACE`,
/// `CARD_CLICKED`, bot echo, empty bodies) returns `None`.
///
/// Mapping: `chat_id` is the space resource name (`spaces/AAA`),
/// `user_id` is the sender resource name (`users/123`), `message_id` is
/// the message resource name, `thread_id` is the thread resource name when
/// the message was posted in a thread, and `is_dm` is true for `DM` spaces.
pub fn event_to_incoming(event: &GoogleChatEvent) -> Option<IncomingMessage> {
    if event.event_type != "MESSAGE" {
        debug!("google_chat: ignoring event type {}", event.event_type);
        return None;
    }
    let message = event.message.as_ref()?;
    // Echo-loop prevention: never answer our own messages.
    if message.sender.user_type.eq_ignore_ascii_case("BOT") {
        debug!("google_chat: ignoring own bot message");
        return None;
    }
    // Prefer Google's mention-stripped argumentText; fall back to a manual
    // strip of the leading @mention.
    let text = if message.argument_text.trim().is_empty() {
        strip_bot_mention(&message.text)
    } else {
        message.argument_text.trim()
    };
    if text.is_empty() {
        debug!("google_chat: ignoring message with empty text");
        return None;
    }
    // Prefer the message-level space; fall back to the top-level one.
    let (space_name, space_type) = if message.space.name.is_empty() {
        (event.space.name.clone(), event.space.space_type.as_str())
    } else {
        (
            message.space.name.clone(),
            message.space.space_type.as_str(),
        )
    };
    Some(IncomingMessage {
        platform: "google_chat".to_string(),
        chat_id: space_name,
        user_id: message.sender.name.clone(),
        text: text.to_string(),
        message_id: Some(message.name.clone()).filter(|s| !s.trim().is_empty()),
        thread_id: message
            .thread
            .as_ref()
            .map(|t| t.name.clone())
            .filter(|s| !s.trim().is_empty()),
        is_dm: space_type.eq_ignore_ascii_case("DM"),
    })
}

// ---------------------------------------------------------------------------
// Outbound URL helpers
// ---------------------------------------------------------------------------

/// Accept `spaces/AAA` or a bare `AAA` and return the space resource id
/// (`AAA`).
pub fn space_resource_id(chat_id: &str) -> &str {
    let chat_id = chat_id.trim().trim_matches('/');
    chat_id.strip_prefix("spaces/").unwrap_or(chat_id)
}

/// Build the `spaces.messages.create` endpoint for a chat.
///
/// `chat_id` may be `spaces/AAA` or a bare `AAA`.
pub fn message_post_url(config: &GoogleChatConfig, chat_id: &str) -> String {
    format!(
        "{}/spaces/{}/messages",
        config.webhook_url.trim_end_matches('/'),
        space_resource_id(chat_id)
    )
}

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

struct GoogleChatInner {
    config: GoogleChatConfig,
    client: Client,
    inbound_tx: RwLock<Option<mpsc::Sender<IncomingMessage>>>,
    stop: Notify,
    base: BasePlatformAdapter,
}

pub struct GoogleChatAdapter {
    inner: Arc<GoogleChatInner>,
    run_task: RwLock<Option<tokio::task::JoinHandle<()>>>,
}

impl GoogleChatAdapter {
    /// Create the adapter. Fails when `bot_token` is empty (fail-closed:
    /// without it no outbound call can authenticate).
    pub fn new(config: GoogleChatConfig, proxy: AdapterProxyConfig) -> Result<Self, GatewayError> {
        if config.bot_token.trim().is_empty() {
            return Err(GatewayError::Auth(
                "google_chat bot_token must not be empty".into(),
            ));
        }
        if config.verification_token.trim().is_empty() {
            warn!(
                "google_chat: verification_token is empty — all inbound webhooks \
                 will be rejected (fail-closed)"
            );
        }
        let base = BasePlatformAdapter::new(config.bot_token.clone()).with_proxy(proxy);
        let client = base.build_client()?;
        let inner = Arc::new(GoogleChatInner {
            config,
            client,
            inbound_tx: RwLock::new(None),
            stop: Notify::new(),
            base,
        });
        Ok(Self {
            inner,
            run_task: RwLock::new(None),
        })
    }

    pub fn config(&self) -> &GoogleChatConfig {
        &self.inner.config
    }

    /// Attach the gateway's inbound channel (mirrors the ntfy/line pattern).
    pub async fn set_inbound_sender(&self, tx: mpsc::Sender<IncomingMessage>) {
        *self.inner.inbound_tx.write().await = Some(tx);
    }
}

/// Handle one inbound webhook connection: validate the Bearer token, parse
/// the event, forward qualifying messages, and always answer with a status.
async fn handle_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    inner: Arc<GoogleChatInner>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (request_line, headers, body) = read_http_request(&mut stream, MAX_WEBHOOK_BYTES).await?;
    let method = request_line.split_whitespace().next().unwrap_or("");
    if method != "POST" {
        write_http_response(&mut stream, 404, "text/plain", "not found").await?;
        return Ok(());
    }
    if !verify_inbound_token(
        headers.get("authorization").map(String::as_str),
        &inner.config.verification_token,
    ) {
        warn!("google_chat: rejecting webhook from {peer}: missing or invalid bearer token");
        write_http_response(&mut stream, 401, "text/plain", "unauthorized").await?;
        return Ok(());
    }
    let event = match parse_chat_event(&body) {
        Some(e) => e,
        None => {
            warn!("google_chat: rejecting webhook from {peer}: unparsable event body");
            write_http_response(&mut stream, 400, "text/plain", "bad request").await?;
            return Ok(());
        }
    };
    info!(
        "google_chat: {} event from {} in space {}",
        event.event_type, event.user.name, event.space.name
    );
    if let Some(incoming) = event_to_incoming(&event) {
        debug!(
            "google_chat: inbound message in {} from {}",
            incoming.chat_id, incoming.user_id
        );
        let tx = inner.inbound_tx.read().await.clone();
        match tx {
            Some(tx) => {
                if tx.send(incoming).await.is_err() {
                    warn!("google_chat: inbound channel closed; dropping message");
                }
            }
            None => warn!("google_chat: no inbound sender attached; dropping message"),
        }
    }
    // Google Chat only needs a timely 2xx; an empty JSON body is fine.
    write_http_response(&mut stream, 200, "application/json", "{}").await?;
    Ok(())
}

#[async_trait]
impl PlatformAdapter for GoogleChatAdapter {
    async fn start(&self) -> Result<(), GatewayError> {
        if !self.inner.config.enabled {
            info!("google_chat adapter disabled; webhook listener not started");
            return Ok(());
        }
        if self.run_task.read().await.is_some() {
            debug!("google_chat: webhook server already running");
            return Ok(());
        }
        let addr: SocketAddr = format!(
            "{}:{}",
            self.inner.config.bind_addr, self.inner.config.webhook_port
        )
        .parse()
        .map_err(|e| {
            GatewayError::ConnectionFailed(format!("invalid google_chat bind address: {e}"))
        })?;
        let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
            GatewayError::ConnectionFailed(format!("google_chat webhook bind {addr} failed: {e}"))
        })?;
        info!("google_chat webhook listening on {addr}");
        self.inner.base.mark_running();
        let inner = self.inner.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = inner.stop.notified() => {
                        info!("google_chat webhook server shutting down");
                        break;
                    }
                    accept = listener.accept() => match accept {
                        Ok((stream, peer)) => {
                            let inner = inner.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(stream, peer, inner).await {
                                    debug!("google_chat webhook connection error from {peer}: {e}");
                                }
                            });
                        }
                        Err(e) => warn!("google_chat webhook accept error: {e}"),
                    },
                }
            }
        });
        *self.run_task.write().await = Some(handle);
        Ok(())
    }

    async fn stop(&self) -> Result<(), GatewayError> {
        info!("google_chat adapter stopping");
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
        if text.trim().is_empty() {
            debug!("google_chat: skipping empty message");
            return Ok(());
        }
        let space = space_resource_id(chat_id);
        if space.is_empty() {
            return Err(GatewayError::SendFailed(
                "google_chat: chat_id must not be empty".into(),
            ));
        }
        let url = message_post_url(&self.inner.config, chat_id);
        for chunk in split_long_message(text, MAX_MESSAGE_CHARS) {
            let resp = self
                .inner
                .client
                .post(&url)
                .bearer_auth(self.inner.config.bot_token.trim())
                .json(&serde_json::json!({ "text": chunk }))
                .send()
                .await
                .map_err(|e| GatewayError::SendFailed(format!("google_chat send failed: {e}")))?;
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
                    "google_chat send HTTP {status}: {detail}"
                )));
            }
        }
        Ok(())
    }

    async fn edit_message(
        &self,
        _chat_id: &str,
        message_id: &str,
        text: &str,
    ) -> Result<(), GatewayError> {
        // The Chat API edits a message via PATCH on its resource name
        // (`spaces/AAA/messages/BBB`), which inbound events carry as
        // `IncomingMessage.message_id`.
        let name = message_id.trim().trim_start_matches('/');
        if name.is_empty() {
            return Err(GatewayError::SendFailed(
                "google_chat: cannot edit a message without a message id".into(),
            ));
        }
        let url = format!(
            "{}/{}",
            self.inner.config.webhook_url.trim_end_matches('/'),
            name
        );
        let resp = self
            .inner
            .client
            .patch(&url)
            .bearer_auth(self.inner.config.bot_token.trim())
            .json(&serde_json::json!({ "text": text }))
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("google_chat edit failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(GatewayError::SendFailed(format!(
                "google_chat edit HTTP {}",
                resp.status()
            )));
        }
        Ok(())
    }

    fn max_edit_chars(&self) -> usize {
        MAX_MESSAGE_CHARS
    }

    async fn send_file(
        &self,
        _chat_id: &str,
        file_path: &str,
        _caption: Option<&str>,
    ) -> Result<(), GatewayError> {
        // The Chat API only accepts attachments via a Drive upload followed
        // by a multipart message create; that flow is out of scope here.
        debug!("google_chat: file attachments are not supported ({file_path})");
        Err(GatewayError::SendFailed(
            "google_chat: file attachments are not supported by this adapter; send the caption as text instead".into(),
        ))
    }

    fn is_running(&self) -> bool {
        self.inner.base.is_running()
    }

    fn platform_name(&self) -> &str {
        "google_chat"
    }
}

// ---------------------------------------------------------------------------
// Tests (no live network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_config::GoogleChatConfig as Cfg;

    fn test_config() -> Cfg {
        Cfg {
            webhook_url: "https://chat.googleapis.com/v1".to_string(),
            bot_token: "bot-token-123".to_string(),
            verification_token: "verify-me".to_string(),
            bind_addr: "127.0.0.1".to_string(),
            webhook_port: 8647,
            enabled: true,
        }
    }

    fn message_event_json() -> &'static str {
        r#"{
            "type": "MESSAGE",
            "eventTime": "2026-10-23T11:00:00.000Z",
            "message": {
                "name": "spaces/AAAA/messages/BBBB",
                "sender": {"name": "users/123", "displayName": "Chewy", "type": "HUMAN"},
                "createTime": "2026-10-23T11:00:00.000Z",
                "text": "@Hermes halo apa kabar",
                "argumentText": "halo apa kabar",
                "thread": {"name": "spaces/AAAA/threads/CCCC"},
                "space": {"name": "spaces/AAAA", "type": "ROOM", "displayName": "general"}
            },
            "user": {"name": "users/123", "displayName": "Chewy", "type": "HUMAN"},
            "space": {"name": "spaces/AAAA", "type": "ROOM", "displayName": "general"}
        }"#
    }

    fn added_to_space_json() -> &'static str {
        r#"{
            "type": "ADDED_TO_SPACE",
            "eventTime": "2026-10-23T11:00:00.000Z",
            "user": {"name": "users/123", "displayName": "Chewy", "type": "HUMAN"},
            "space": {"name": "spaces/AAAA", "type": "ROOM", "displayName": "general"}
        }"#
    }

    #[test]
    fn config_defaults() {
        let cfg = Cfg::default();
        assert_eq!(cfg.webhook_url, "https://chat.googleapis.com/v1");
        assert!(cfg.bot_token.is_empty());
        assert!(cfg.verification_token.is_empty());
        assert_eq!(cfg.bind_addr, "0.0.0.0");
        assert_eq!(cfg.webhook_port, 8647);
        assert!(!cfg.enabled);
    }

    #[test]
    fn config_from_platform_config() {
        use hermes_config::PlatformConfig;
        use serde_json::json;
        let mut pc = PlatformConfig::default();
        pc.enabled = true;
        pc.token = Some("top-level-token".to_string());
        pc.extra
            .insert("verification_token".to_string(), json!("hook-secret"));
        pc.extra.insert("webhook_port".to_string(), json!(9001));
        pc.extra.insert(
            "webhook_url".to_string(),
            json!("https://chat.example.com/v1/"),
        );
        let cfg = Cfg::from_platform_config(&pc);
        // first-class token falls back to bot_token
        assert_eq!(cfg.bot_token, "top-level-token");
        assert_eq!(cfg.verification_token, "hook-secret");
        assert_eq!(cfg.webhook_port, 9001);
        assert_eq!(cfg.webhook_url, "https://chat.example.com/v1");
        assert!(cfg.enabled);
    }

    #[test]
    fn config_bot_token_prefers_extra() {
        use hermes_config::PlatformConfig;
        use serde_json::json;
        let mut pc = PlatformConfig::default();
        pc.token = Some("top-level-token".to_string());
        pc.extra
            .insert("bot_token".to_string(), json!("extra-bot-token"));
        let cfg = Cfg::from_platform_config(&pc);
        assert_eq!(cfg.bot_token, "extra-bot-token");
    }

    #[test]
    fn parse_message_event() {
        let event = parse_chat_event(message_event_json().as_bytes()).expect("parses");
        assert_eq!(event.event_type, "MESSAGE");
        let message = event.message.expect("has message");
        assert_eq!(message.name, "spaces/AAAA/messages/BBBB");
        assert_eq!(message.sender.name, "users/123");
        assert_eq!(message.sender.user_type, "HUMAN");
        assert_eq!(message.argument_text, "halo apa kabar");
        assert_eq!(
            message.thread.expect("has thread").name,
            "spaces/AAAA/threads/CCCC"
        );
        assert_eq!(message.space.name, "spaces/AAAA");
        assert_eq!(message.space.space_type, "ROOM");
        assert_eq!(event.space.name, "spaces/AAAA");
    }

    #[test]
    fn parse_added_to_space_event() {
        let event = parse_chat_event(added_to_space_json().as_bytes()).expect("parses");
        assert_eq!(event.event_type, "ADDED_TO_SPACE");
        assert!(event.message.is_none());
        assert_eq!(event.user.name, "users/123");
        assert_eq!(event.space.name, "spaces/AAAA");
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_chat_event(b"").is_none());
        assert!(parse_chat_event(b"not json").is_none());
        assert!(parse_chat_event(b"[1,2,3]").is_none());
        assert!(parse_chat_event(br#"{"foo":"bar"}"#).is_none());
        assert!(parse_chat_event(br#"{"type":""}"#).is_none());
    }

    #[test]
    fn verify_inbound_token_accepts_correct_bearer() {
        assert!(verify_inbound_token(Some("Bearer verify-me"), "verify-me"));
        // scheme is case-insensitive, surrounding whitespace is tolerated
        assert!(verify_inbound_token(
            Some("bearer   verify-me  "),
            "  verify-me "
        ));
    }

    #[test]
    fn verify_inbound_token_rejects_bad_input() {
        assert!(!verify_inbound_token(
            Some("Bearer wrong-token"),
            "verify-me"
        ));
        assert!(!verify_inbound_token(None, "verify-me"));
        assert!(!verify_inbound_token(Some(""), "verify-me"));
        assert!(!verify_inbound_token(
            Some("Basic dmVyaWZ5LW1l"),
            "verify-me"
        ));
        assert!(!verify_inbound_token(Some("verify-me"), "verify-me"));
        assert!(!verify_inbound_token(Some("Bearer "), "verify-me"));
    }

    #[test]
    fn verify_inbound_token_fail_closed_without_configured_token() {
        // No verification token configured: nothing ever verifies.
        assert!(!verify_inbound_token(Some("Bearer anything"), ""));
        assert!(!verify_inbound_token(Some("Bearer anything"), "   "));
        assert!(!verify_inbound_token(None, ""));
    }

    #[test]
    fn event_to_incoming_message_fields() {
        let event = parse_chat_event(message_event_json().as_bytes()).expect("parses");
        let incoming = event_to_incoming(&event).expect("converts");
        assert_eq!(incoming.platform, "google_chat");
        assert_eq!(incoming.chat_id, "spaces/AAAA");
        assert_eq!(incoming.user_id, "users/123");
        // argumentText (mention already stripped by Google) is preferred
        assert_eq!(incoming.text, "halo apa kabar");
        assert_eq!(
            incoming.message_id.as_deref(),
            Some("spaces/AAAA/messages/BBBB")
        );
        assert_eq!(
            incoming.thread_id.as_deref(),
            Some("spaces/AAAA/threads/CCCC")
        );
        assert!(!incoming.is_dm);
    }

    #[test]
    fn event_to_incoming_strips_mention_without_argument_text() {
        let mut event = parse_chat_event(message_event_json().as_bytes()).expect("parses");
        let message = event.message.as_mut().expect("has message");
        message.argument_text.clear();
        message.thread = None;
        let incoming = event_to_incoming(&event).expect("converts");
        assert_eq!(incoming.text, "halo apa kabar");
        assert!(incoming.thread_id.is_none());
    }

    #[test]
    fn event_to_incoming_ignores_bot_sender() {
        let mut event = parse_chat_event(message_event_json().as_bytes()).expect("parses");
        event
            .message
            .as_mut()
            .expect("has message")
            .sender
            .user_type = "BOT".to_string();
        assert!(event_to_incoming(&event).is_none());
    }

    #[test]
    fn event_to_incoming_ignores_empty_text() {
        let mut event = parse_chat_event(message_event_json().as_bytes()).expect("parses");
        let message = event.message.as_mut().expect("has message");
        message.text = "@Hermes".to_string();
        message.argument_text.clear();
        assert!(event_to_incoming(&event).is_none());
    }

    #[test]
    fn event_to_incoming_marks_dm_spaces() {
        let mut event = parse_chat_event(message_event_json().as_bytes()).expect("parses");
        event.space.space_type = "DM".to_string();
        event
            .message
            .as_mut()
            .expect("has message")
            .space
            .space_type = "DM".to_string();
        let incoming = event_to_incoming(&event).expect("converts");
        assert!(incoming.is_dm);
    }

    #[test]
    fn added_to_space_is_not_forwarded() {
        let event = parse_chat_event(added_to_space_json().as_bytes()).expect("parses");
        assert!(event_to_incoming(&event).is_none());
        // other non-message types are dropped as well
        let mut removed = event.clone();
        removed.event_type = "REMOVED_FROM_SPACE".to_string();
        assert!(event_to_incoming(&removed).is_none());
        let mut clicked = event;
        clicked.event_type = "CARD_CLICKED".to_string();
        assert!(event_to_incoming(&clicked).is_none());
    }

    #[test]
    fn strip_bot_mention_cases() {
        assert_eq!(strip_bot_mention("@Hermes halo"), "halo");
        assert_eq!(strip_bot_mention("  @Hermes  halo  "), "halo  ");
        assert_eq!(strip_bot_mention("@Hermes"), "");
        assert_eq!(strip_bot_mention("halo dunia"), "halo dunia");
        assert_eq!(strip_bot_mention(""), "");
    }

    #[test]
    fn space_resource_id_normalization() {
        assert_eq!(space_resource_id("spaces/AAAA"), "AAAA");
        assert_eq!(space_resource_id("AAAA"), "AAAA");
        assert_eq!(space_resource_id("  spaces/AAAA  "), "AAAA");
        assert_eq!(space_resource_id(""), "");
    }

    #[test]
    fn message_post_url_format() {
        let cfg = test_config();
        assert_eq!(
            message_post_url(&cfg, "spaces/AAAA"),
            "https://chat.googleapis.com/v1/spaces/AAAA/messages"
        );
        assert_eq!(
            message_post_url(&cfg, "AAAA"),
            "https://chat.googleapis.com/v1/spaces/AAAA/messages"
        );
    }

    #[test]
    fn new_rejects_empty_bot_token() {
        let mut cfg = test_config();
        cfg.bot_token = "   ".to_string();
        let result = GoogleChatAdapter::new(cfg, AdapterProxyConfig::default());
        assert!(result.is_err());
        assert!(matches!(result.err().unwrap(), GatewayError::Auth(_)));
    }

    #[test]
    fn new_accepts_valid_config() {
        let adapter =
            GoogleChatAdapter::new(test_config(), AdapterProxyConfig::default()).expect("adapter");
        assert_eq!(adapter.platform_name(), "google_chat");
        assert!(!adapter.is_running());
        assert_eq!(adapter.config().webhook_port, 8647);
    }
}
