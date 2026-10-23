//! Microsoft Teams (Bot Framework) adapter.
//!
//! Inbound: a local HTTP listener receives Bot Framework activities POSTed to
//! `/api/messages` by the Bot Framework Connector. Only activities with
//! `type == "message"` and non-empty text are forwarded to the gateway via an
//! `mpsc` inbound channel; every other activity type (`typing`,
//! `conversationUpdate`, `event`, ...) is acknowledged with 200 and ignored.
//!
//! The `Authorization: Bearer <token>` header is validated fail-closed: when
//! `app_password` is configured the token must match it exactly (compared in
//! constant time); with no configured password any well-formed Bearer token
//! is accepted. Full Bot Framework JWT verification is NOT implemented.
//!
//! Outbound: replies are POSTed to
//! `{serviceUrl}/v3/conversations/{conversationId}/activities` as
//! `{"type":"message","text":"..."}` with a Bot Framework OAuth2 token
//! (client-credentials grant, cached until shortly before expiry). The
//! `serviceUrl` is learned per conversation from inbound activities;
//! `config.service_url` is the fallback for proactive messages.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Notify, RwLock};
use tracing::{debug, error, info, warn};

use hermes_config::TeamsConfig;
use hermes_core::errors::GatewayError;
use hermes_core::traits::{ParseMode, PlatformAdapter};

use crate::adapter::{AdapterProxyConfig, BasePlatformAdapter};
use crate::gateway::IncomingMessage;
use crate::platforms::helpers::{constant_time_eq, read_http_request, write_http_response};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Bot Framework OAuth2 token endpoint (Azure AD v2, bot tenant).
const BOTFRAMEWORK_TOKEN_URL: &str =
    "https://login.microsoftonline.com/botframework.com/oauth2/v2.0/token";
/// OAuth2 scope for the Bot Framework Connector API.
const BOTFRAMEWORK_SCOPE: &str = "https://api.botframework.com/.default";
/// Refresh the cached token this long before it expires.
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(300);
/// Cap for outbound message text (chars); Teams accepts longer texts but the
/// Connector chokes on huge payloads.
const MAX_MESSAGE_LENGTH: usize = 8000;
/// Cap for inbound webhook request bodies (1 MiB).
const WEBHOOK_BODY_MAX_BYTES: usize = 1024 * 1024;
/// Cap for `send_file` attachments.
const MAX_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;
/// Webhook path Bot Framework posts activities to.
const MESSAGES_PATH: &str = "/api/messages";

// ---------------------------------------------------------------------------
// Activity parsing
// ---------------------------------------------------------------------------

/// A parsed Bot Framework activity (only the fields the adapter needs).
#[derive(Debug, Clone, Default)]
pub struct TeamsActivity {
    /// Activity type (`message`, `typing`, `conversationUpdate`, ...).
    pub activity_type: String,
    /// Message text (for `message` activities).
    pub text: Option<String>,
    /// Activity id (for threading / dedup).
    pub id: Option<String>,
    /// Sender id (`from.id`).
    pub from_id: Option<String>,
    /// Sender display name (`from.name`).
    pub from_name: Option<String>,
    /// Conversation id (`conversation.id`); becomes the reply `chat_id`.
    pub conversation_id: Option<String>,
    /// Conversation kind (`conversation.conversationType`): `personal`,
    /// `channel`, or `groupChat`.
    pub conversation_type: Option<String>,
    /// `serviceUrl` — where outbound calls for this conversation go.
    pub service_url: Option<String>,
    /// `channelId` (normally `msteams`).
    pub channel_id: Option<String>,
}

/// Parse a Bot Framework activity from the raw webhook body.
///
/// Returns `None` for invalid JSON or bodies that are not JSON objects.
pub fn parse_activity(body: &[u8]) -> Option<TeamsActivity> {
    let obj = serde_json::from_slice::<Value>(body)
        .ok()?
        .as_object()?
        .clone();
    let field = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let nested =
        |outer: &str, inner: &str| obj.get(outer)?.get(inner)?.as_str().map(str::to_string);
    Some(TeamsActivity {
        activity_type: field("type").unwrap_or_default(),
        text: field("text"),
        id: field("id"),
        from_id: nested("from", "id"),
        from_name: nested("from", "name"),
        conversation_id: nested("conversation", "id"),
        conversation_type: nested("conversation", "conversationType"),
        service_url: field("serviceUrl"),
        channel_id: field("channelId"),
    })
}

/// Convert an activity into an [`IncomingMessage`].
///
/// Only `message` activities with non-empty text qualify; everything else
/// returns `None` so the caller can acknowledge-but-ignore it.
pub fn activity_to_incoming(activity: &TeamsActivity) -> Option<IncomingMessage> {
    if activity.activity_type != "message" {
        debug!(
            "teams: ignoring non-message activity of type '{}'",
            activity.activity_type
        );
        return None;
    }
    let text = activity.text.as_deref().unwrap_or("").trim();
    if text.is_empty() {
        debug!("teams: ignoring message activity with empty text");
        return None;
    }
    let conversation_id = activity
        .conversation_id
        .clone()
        .unwrap_or_default()
        .trim()
        .to_string();
    let is_dm = activity.conversation_type.as_deref() == Some("personal");
    debug!("teams: inbound message in conversation '{conversation_id}' (dm: {is_dm})");
    Some(IncomingMessage {
        platform: "teams".to_string(),
        chat_id: conversation_id,
        user_id: activity.from_id.clone().unwrap_or_default(),
        text: text.to_string(),
        message_id: activity.id.clone(),
        is_dm,
        thread_id: None,
    })
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/// Validate the `Authorization` header of an inbound Bot Framework request.
///
/// Fail-closed: the header must be a well-formed `Bearer <token>` (a missing,
/// empty, or non-Bearer header is always rejected). When `app_password` is
/// configured the token must match it exactly, compared in constant time;
/// with no configured password any non-empty Bearer token is accepted (the
/// caller has no other way to recognize the platform).
pub fn validate_auth_header(header: Option<&str>, app_password: &str) -> bool {
    let header = match header {
        Some(h) => h,
        None => {
            debug!("teams: rejecting request with missing Authorization header");
            return false;
        }
    };
    let Some(token) = header.strip_prefix("Bearer ") else {
        debug!("teams: rejecting request without Bearer authorization");
        return false;
    };
    let token = token.trim();
    if token.is_empty() {
        debug!("teams: rejecting request with empty bearer token");
        return false;
    }
    let secret = app_password.trim();
    if secret.is_empty() {
        // No configured secret: accept any well-formed bearer token.
        return true;
    }
    let ok = constant_time_eq(token.as_bytes(), secret.as_bytes());
    if !ok {
        debug!("teams: rejecting request with mismatched bearer token");
    }
    ok
}

// ---------------------------------------------------------------------------
// OAuth2 token
// ---------------------------------------------------------------------------

/// Cached Bot Framework OAuth2 access token.
#[derive(Debug, Clone)]
struct CachedToken {
    token: String,
    /// When the token should be treated as expired (a bit before the
    /// `expires_in` deadline, to stay safe).
    expires_at: Instant,
}

/// Acquire (or reuse the cached) Bot Framework OAuth2 access token via the
/// client-credentials grant.
async fn botframework_access_token(inner: &TeamsInner) -> Result<String, GatewayError> {
    {
        let cache = inner.token_cache.read().await;
        if let Some(cached) = cache.as_ref() {
            if Instant::now() + TOKEN_REFRESH_MARGIN < cached.expires_at {
                return Ok(cached.token.clone());
            }
        }
    }
    debug!("teams: acquiring Bot Framework OAuth2 token");
    let params = [
        ("client_id", inner.config.app_id.as_str()),
        ("client_secret", inner.config.app_password.as_str()),
        ("grant_type", "client_credentials"),
        ("scope", BOTFRAMEWORK_SCOPE),
    ];
    let resp = inner
        .client
        .post(BOTFRAMEWORK_TOKEN_URL)
        .form(&params)
        .send()
        .await
        .map_err(|e| GatewayError::Auth(format!("teams: token request failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(GatewayError::Auth(format!(
            "teams: token request HTTP {}",
            resp.status()
        )));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| GatewayError::Auth(format!("teams: token response parse failed: {e}")))?;
    let token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| GatewayError::Auth("teams: token response has no access_token".into()))?;
    let expires_in = body
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(3600);
    *inner.token_cache.write().await = Some(CachedToken {
        token: token.clone(),
        expires_at: Instant::now() + Duration::from_secs(expires_in.max(60)),
    });
    debug!("teams: OAuth2 token acquired (expires in {expires_in}s)");
    Ok(token)
}

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

struct TeamsInner {
    config: TeamsConfig,
    client: Client,
    inbound_tx: RwLock<Option<mpsc::Sender<IncomingMessage>>>,
    /// `serviceUrl` learned per conversation from inbound activities.
    conversation_service_urls: RwLock<HashMap<String, String>>,
    token_cache: RwLock<Option<CachedToken>>,
    stop: Notify,
    base: BasePlatformAdapter,
}

pub struct TeamsAdapter {
    inner: Arc<TeamsInner>,
    run_task: RwLock<Option<tokio::task::JoinHandle<()>>>,
}

impl TeamsAdapter {
    /// Create the adapter. Fails when `app_id` or `app_password` is empty
    /// (fail-closed: without credentials we can neither send nor authenticate
    /// inbound requests).
    pub fn new(config: TeamsConfig, proxy: AdapterProxyConfig) -> Result<Self, GatewayError> {
        if config.app_id.trim().is_empty() {
            return Err(GatewayError::Platform(
                "teams app_id must not be empty".into(),
            ));
        }
        if config.app_password.trim().is_empty() {
            return Err(GatewayError::Platform(
                "teams app_password must not be empty".into(),
            ));
        }
        let base = BasePlatformAdapter::new(config.app_password.clone()).with_proxy(proxy);
        let client = base.build_client()?;
        let inner = Arc::new(TeamsInner {
            config,
            client,
            inbound_tx: RwLock::new(None),
            conversation_service_urls: RwLock::new(HashMap::new()),
            token_cache: RwLock::new(None),
            stop: Notify::new(),
            base,
        });
        Ok(Self {
            inner,
            run_task: RwLock::new(None),
        })
    }

    pub fn config(&self) -> &TeamsConfig {
        &self.inner.config
    }

    /// Attach the gateway's inbound channel (mirrors the ntfy pattern).
    pub async fn set_inbound_sender(&self, tx: mpsc::Sender<IncomingMessage>) {
        *self.inner.inbound_tx.write().await = Some(tx);
    }

    /// Webhook accept loop for `/api/messages`.
    async fn serve_loop(inner: Arc<TeamsInner>) {
        let addr = format!("0.0.0.0:{}", inner.config.webhook_port);
        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                error!("teams: webhook server failed to bind {addr}: {e}");
                inner.base.mark_stopped();
                return;
            }
        };
        info!("teams: webhook server listening on {addr} ({MESSAGES_PATH})");
        loop {
            tokio::select! {
                accept = listener.accept() => {
                    match accept {
                        Ok((stream, peer)) => {
                            let inner = inner.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(stream, peer, inner).await {
                                    debug!("teams: webhook connection error from {peer}: {e}");
                                }
                            });
                        }
                        Err(e) => warn!("teams: webhook accept error: {e}"),
                    }
                }
                _ = inner.stop.notified() => {
                    info!("teams: webhook server shutting down");
                    return;
                }
            }
        }
    }

    /// Resolve the service URL for outbound calls to a conversation:
    /// the `serviceUrl` learned from inbound activities, else the configured
    /// fallback. Fails closed when neither is known.
    async fn service_url_for(&self, chat_id: &str) -> Result<String, GatewayError> {
        if let Some(url) = self
            .inner
            .conversation_service_urls
            .read()
            .await
            .get(chat_id)
        {
            return Ok(url.clone());
        }
        let fallback = self.inner.config.service_url.trim();
        if fallback.is_empty() {
            return Err(GatewayError::SendFailed(format!(
                "teams: no service URL known for conversation '{chat_id}' \
                 (send an inbound message first or configure service_url)"
            )));
        }
        Ok(fallback.to_string())
    }

    /// POST an activity to the Bot Framework Connector for a conversation.
    async fn post_activity(
        inner: &TeamsInner,
        service_url: &str,
        conversation_id: &str,
        payload: &Value,
    ) -> Result<(), GatewayError> {
        let token = botframework_access_token(inner).await?;
        let url = format!(
            "{}/v3/conversations/{}/activities",
            service_url.trim_end_matches('/'),
            conversation_id
        );
        let resp = inner
            .client
            .post(&url)
            .bearer_auth(token)
            .json(payload)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("teams: activity POST failed: {e}")))?;
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
                "teams: activity POST HTTP {status}: {detail}"
            )));
        }
        Ok(())
    }

    /// Truncate outbound text to the Connector-safe limit (char-based, so
    /// multi-byte text is never split), warning when it cuts.
    fn truncate_text(text: &str) -> String {
        let truncated: String = text.chars().take(MAX_MESSAGE_LENGTH).collect();
        if truncated.len() < text.len() {
            warn!(
                "teams: truncating message from {} to {} chars (Bot Framework limit)",
                text.chars().count(),
                MAX_MESSAGE_LENGTH
            );
        }
        truncated
    }
}

/// Handle one webhook connection: validate the Bearer token (fail-closed),
/// acknowledge and dispatch `message` activities, ignore the rest.
async fn handle_connection(
    mut stream: TcpStream,
    peer: std::net::SocketAddr,
    inner: Arc<TeamsInner>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (request_line, headers, body) = read_http_request(&mut stream, WEBHOOK_BODY_MAX_BYTES)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("teams: failed to read request: {e}").into()
        })?;

    // Health check for load balancers / tunnel probes.
    if request_line.starts_with("GET ") {
        write_http_response(
            &mut stream,
            200,
            "application/json",
            r#"{"status":"ok","platform":"teams"}"#,
        )
        .await?;
        return Ok(());
    }

    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    if request_line.starts_with("POST ") && path == MESSAGES_PATH {
        // Fail-closed: missing or wrong bearer token → reject without touching
        // the body.
        if !validate_auth_header(
            headers.get("authorization").map(String::as_str),
            &inner.config.app_password,
        ) {
            warn!("teams: rejecting webhook from {peer}: invalid authorization");
            write_http_response(
                &mut stream,
                401,
                "application/json",
                r#"{"error":"invalid authorization"}"#,
            )
            .await?;
            return Ok(());
        }

        let activity = match serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|_| parse_activity(&body))
        {
            Some(a) => a,
            None => {
                write_http_response(
                    &mut stream,
                    400,
                    "application/json",
                    r#"{"error":"bad activity json"}"#,
                )
                .await?;
                return Ok(());
            }
        };

        // Learn the serviceUrl for this conversation for outbound calls.
        if let (Some(conversation_id), Some(service_url)) = (
            activity.conversation_id.clone(),
            activity.service_url.clone(),
        ) {
            let service_url = service_url.trim_end_matches('/').to_string();
            inner
                .conversation_service_urls
                .write()
                .await
                .entry(conversation_id)
                .or_insert(service_url);
        }

        if let Some(incoming) = activity_to_incoming(&activity) {
            if let Some(tx) = inner.inbound_tx.read().await.clone() {
                tokio::spawn(async move {
                    let _ = tx.send(incoming).await;
                });
            }
        }

        write_http_response(&mut stream, 200, "application/json", r#"{"status":"ok"}"#).await?;
        return Ok(());
    }

    write_http_response(
        &mut stream,
        404,
        "application/json",
        r#"{"error":"not found"}"#,
    )
    .await?;
    Ok(())
}

#[async_trait]
impl PlatformAdapter for TeamsAdapter {
    async fn start(&self) -> Result<(), GatewayError> {
        info!(
            "teams adapter starting: webhook on port {} ({MESSAGES_PATH})",
            self.inner.config.webhook_port
        );
        if self.run_task.read().await.is_some() {
            debug!("teams: webhook server already running");
            return Ok(());
        }
        self.inner.base.mark_running();
        let inner = self.inner.clone();
        let handle = tokio::spawn(async move { Self::serve_loop(inner).await });
        *self.run_task.write().await = Some(handle);
        Ok(())
    }

    async fn stop(&self) -> Result<(), GatewayError> {
        info!("teams adapter stopping");
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
                "teams: chat_id (conversation id) must not be empty".into(),
            ));
        }
        let service_url = self.service_url_for(chat_id).await?;
        let payload = serde_json::json!({
            "type": "message",
            "text": Self::truncate_text(text),
        });
        Self::post_activity(&self.inner, &service_url, chat_id, &payload).await
    }

    async fn edit_message(
        &self,
        chat_id: &str,
        message_id: &str,
        text: &str,
    ) -> Result<(), GatewayError> {
        // Bot Framework supports updating an activity: PUT
        // /v3/conversations/{conversationId}/activities/{activityId}.
        // `chat_id` is the conversation id, `message_id` the activity id.
        let chat_id = chat_id.trim();
        let service_url = self.service_url_for(chat_id).await?;
        let token = botframework_access_token(&self.inner).await?;
        let url = format!(
            "{}/v3/conversations/{}/activities/{}",
            service_url.trim_end_matches('/'),
            chat_id,
            message_id
        );
        let payload = serde_json::json!({
            "type": "message",
            "text": Self::truncate_text(text),
        });
        let resp = self
            .inner
            .client
            .put(&url)
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("teams: activity PUT failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(GatewayError::SendFailed(format!(
                "teams: activity PUT HTTP {}",
                resp.status()
            )));
        }
        Ok(())
    }

    async fn send_file(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<(), GatewayError> {
        // The Bot Framework Connector has no inline file-upload API; file
        // bytes must be hosted at a reachable URL first. Send a placeholder
        // message naming the file (with the caption) instead.
        let bytes = tokio::fs::read(file_path)
            .await
            .map_err(|e| GatewayError::SendFailed(format!("teams: failed to read file: {e}")))?;
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err(GatewayError::SendFailed(format!(
                "teams: file too large ({} bytes > {} byte limit)",
                bytes.len(),
                MAX_ATTACHMENT_BYTES
            )));
        }
        let filename = std::path::Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment");
        let mut text = format!("📎 {filename}");
        if let Some(caption) = caption.map(str::trim).filter(|s| !s.is_empty()) {
            text.push_str("\n\n");
            text.push_str(caption);
        }
        self.send_message(chat_id, &text, None).await
    }

    fn is_running(&self) -> bool {
        self.inner.base.is_running()
    }

    fn platform_name(&self) -> &str {
        "teams"
    }
}

// ---------------------------------------------------------------------------
// Tests (no live network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> TeamsConfig {
        TeamsConfig {
            app_id: "appid-123".to_string(),
            app_password: "secret-abc".to_string(),
            webhook_port: 3978,
            service_url: "https://smba.trafficmanager.net/teams".to_string(),
            enabled: true,
        }
    }

    fn message_activity_json() -> &'static str {
        r#"{
            "type": "message",
            "id": "activity-1",
            "text": "Hello, bot!",
            "from": { "id": "user-29", "name": "Chewy" },
            "conversation": { "id": "conv-19", "conversationType": "personal" },
            "serviceUrl": "https://smba.trafficmanager.net/teams",
            "channelId": "msteams"
        }"#
    }

    #[test]
    fn parse_message_activity() {
        let activity = parse_activity(message_activity_json().as_bytes()).expect("parses");
        assert_eq!(activity.activity_type, "message");
        assert_eq!(activity.text.as_deref(), Some("Hello, bot!"));
        assert_eq!(activity.id.as_deref(), Some("activity-1"));
        assert_eq!(activity.from_id.as_deref(), Some("user-29"));
        assert_eq!(activity.from_name.as_deref(), Some("Chewy"));
        assert_eq!(activity.conversation_id.as_deref(), Some("conv-19"));
        assert_eq!(activity.conversation_type.as_deref(), Some("personal"));
        assert_eq!(
            activity.service_url.as_deref(),
            Some("https://smba.trafficmanager.net/teams")
        );
        assert_eq!(activity.channel_id.as_deref(), Some("msteams"));
    }

    #[test]
    fn parse_rejects_invalid_json() {
        assert!(parse_activity(b"").is_none());
        assert!(parse_activity(b"not json").is_none());
        // valid JSON but not an object
        assert!(parse_activity(b"[1,2,3]").is_none());
    }

    #[test]
    fn activity_to_incoming_converts_message() {
        let activity = parse_activity(message_activity_json().as_bytes()).expect("parses");
        let incoming = activity_to_incoming(&activity).expect("converts");
        assert_eq!(incoming.platform, "teams");
        assert_eq!(incoming.chat_id, "conv-19");
        assert_eq!(incoming.user_id, "user-29");
        assert_eq!(incoming.text, "Hello, bot!");
        assert_eq!(incoming.message_id.as_deref(), Some("activity-1"));
        assert!(incoming.is_dm);
        assert!(incoming.thread_id.is_none());
    }

    #[test]
    fn activity_to_incoming_filters_non_message_types() {
        for activity_type in ["typing", "conversationUpdate", "event", "endOfConversation"] {
            let body = format!(
                r#"{{"type":"{activity_type}","text":"ignored","from":{{"id":"u"}},"conversation":{{"id":"c","conversationType":"channel"}}}}"#
            );
            let activity = parse_activity(body.as_bytes()).expect("parses");
            assert!(
                activity_to_incoming(&activity).is_none(),
                "type {activity_type} should be filtered"
            );
        }
    }

    #[test]
    fn activity_to_incoming_filters_empty_text() {
        for body in [
            r#"{"type":"message","conversation":{"id":"c","conversationType":"personal"}}"#,
            r#"{"type":"message","text":"","conversation":{"id":"c","conversationType":"personal"}}"#,
            r#"{"type":"message","text":"   ","conversation":{"id":"c","conversationType":"personal"}}"#,
        ] {
            let activity = parse_activity(body.as_bytes()).expect("parses");
            assert!(
                activity_to_incoming(&activity).is_none(),
                "empty text should be filtered: {body}"
            );
        }
    }

    #[test]
    fn channel_conversation_is_not_dm() {
        let activity = parse_activity(
            r#"{"type":"message","text":"hi","conversation":{"id":"c1","conversationType":"channel"}}"#
                .as_bytes(),
        )
        .expect("parses");
        let incoming = activity_to_incoming(&activity).expect("converts");
        assert!(!incoming.is_dm);
    }

    #[test]
    fn auth_header_validates_against_configured_password() {
        let secret = "secret-abc";
        assert!(validate_auth_header(Some("Bearer secret-abc"), secret));
        // wrong token rejected (fail-closed)
        assert!(!validate_auth_header(Some("Bearer wrong-token"), secret));
        // missing header rejected
        assert!(!validate_auth_header(None, secret));
        // malformed (no Bearer scheme) rejected
        assert!(!validate_auth_header(Some("secret-abc"), secret));
        // empty token rejected
        assert!(!validate_auth_header(Some("Bearer   "), secret));
        // case-sensitive scheme
        assert!(!validate_auth_header(Some("bearer secret-abc"), secret));
    }

    #[test]
    fn auth_header_without_configured_password_accepts_well_formed_bearer() {
        assert!(validate_auth_header(Some("Bearer some-jwt-ish-token"), ""));
        assert!(validate_auth_header(Some("Bearer   spaced  "), ""));
        // still rejects missing / malformed / empty headers
        assert!(!validate_auth_header(None, ""));
        assert!(!validate_auth_header(Some("Basic abc"), ""));
        assert!(!validate_auth_header(Some("Bearer "), ""));
    }

    #[test]
    fn new_rejects_missing_credentials() {
        let mut cfg = test_config();
        cfg.app_id = "   ".to_string();
        let result = TeamsAdapter::new(cfg, AdapterProxyConfig::default());
        assert!(result.is_err());
        let err = result.err().expect("expected construction to fail");
        assert!(matches!(err, GatewayError::Platform(_)));

        let mut cfg = test_config();
        cfg.app_password = String::new();
        let result = TeamsAdapter::new(cfg, AdapterProxyConfig::default());
        assert!(result.is_err());
        let err = result.err().expect("expected construction to fail");
        assert!(matches!(err, GatewayError::Platform(_)));
    }

    #[test]
    fn adapter_metadata() {
        let adapter =
            TeamsAdapter::new(test_config(), AdapterProxyConfig::default()).expect("adapter");
        assert_eq!(adapter.platform_name(), "teams");
        assert_eq!(adapter.config().webhook_port, 3978);
        assert!(!adapter.is_running());
    }
}
