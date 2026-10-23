//! WhatsApp Business Cloud API adapter.
//!
//! Implements the `PlatformAdapter` trait for WhatsApp using the Cloud API.
//! Sends messages via `POST /v1/messages` and receives via webhook.

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Notify, RwLock};
use tracing::{debug, error, info, warn};

use hermes_core::errors::GatewayError;
use hermes_core::traits::{ParseMode, PlatformAdapter};

use crate::adapter::{AdapterProxyConfig, BasePlatformAdapter};
use crate::gateway::IncomingMessage;
use crate::markdown_split::split_markdown;
use crate::platforms::helpers::{
    constant_time_eq, parse_http_query, read_http_request, send_with_rate_limit_retry,
    verify_hmac_sha256_signature, write_http_response,
};

const WHATSAPP_API_BASE: &str = "https://graph.facebook.com/v18.0";

/// Maximum text length per WhatsApp message (4096 chars per Meta's Cloud
/// API); longer texts are split fence-aware into sequential messages.
const MAX_MESSAGE_LENGTH: usize = 4096;

/// Maximum number of inbound wamids kept for duplicate suppression.
pub const MAX_DEDUP_WAMIDS: usize = 5000;

/// Default bind host for the inbound webhook server.
const DEFAULT_WEBHOOK_HOST: &str = "0.0.0.0";

/// Default port for the inbound webhook server (mirrors the original).
const DEFAULT_WEBHOOK_PORT: u16 = 8090;

/// Default path for the inbound webhook server (mirrors the original).
const DEFAULT_WEBHOOK_PATH: &str = "/whatsapp/webhook";

/// Maximum webhook POST body accepted (Meta caps at 3MB).
const WEBHOOK_MAX_BODY_BYTES: usize = 3 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Incoming message types
// ---------------------------------------------------------------------------

/// Parsed incoming WhatsApp message from a webhook notification.
#[derive(Debug, Clone)]
pub struct IncomingWhatsAppMessage {
    pub from: String,
    pub message_id: String,
    pub text: String,
    pub message_type: String,
    pub timestamp: String,
}

// ---------------------------------------------------------------------------
// WhatsAppConfig
// ---------------------------------------------------------------------------

/// Configuration for the WhatsApp adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhatsAppConfig {
    /// WhatsApp Business API access token.
    pub token: String,

    /// Phone number ID for sending messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone_number_id: Option<String>,

    /// WhatsApp Business Account ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub business_account_id: Option<String>,

    /// Webhook verify token for incoming events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_token: Option<String>,

    /// Meta app secret used to verify `X-Hub-Signature-256` on inbound
    /// webhook POST bodies (HMAC-SHA256, fail-closed).
    ///
    /// Falls back to the `WHATSAPP_APP_SECRET` environment variable when
    /// unset. Webhook POSTs are rejected (503) when neither is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_secret: Option<String>,

    /// Bind host for the inbound webhook server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_host: Option<String>,

    /// Port for the inbound webhook server (default 8090).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_port: Option<u16>,

    /// Path for the inbound webhook server (default `/whatsapp/webhook`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_path: Option<String>,

    /// Proxy configuration for outbound requests.
    #[serde(default)]
    pub proxy: AdapterProxyConfig,
}

/// In-memory duplicate suppression for inbound WhatsApp message IDs (wamids).
///
/// Meta's webhook may redeliver the same message (retries, multi-node
/// fanout); messages whose `id` was already processed are dropped here
/// before reaching the agent loop. Keeps the most recent
/// [`MAX_DEDUP_WAMIDS`] IDs, evicting the oldest.
#[derive(Debug, Default)]
struct WamidDedup {
    ids: HashSet<String>,
    order: VecDeque<String>,
}

impl WamidDedup {
    /// `true` when `id` was already seen (duplicate → drop it).
    /// Empty IDs are never deduped.
    fn check_and_insert(&mut self, id: &str) -> bool {
        if id.is_empty() {
            return false;
        }
        if self.ids.contains(id) {
            return true;
        }
        self.ids.insert(id.to_string());
        self.order.push_back(id.to_string());
        self.trim_to_cap();
        false
    }

    /// Evict oldest entries so at most [`MAX_DEDUP_WAMIDS`] IDs are kept.
    fn trim_to_cap(&mut self) {
        while self.order.len() > MAX_DEDUP_WAMIDS {
            match self.order.pop_front() {
                Some(old) => {
                    self.ids.remove(&old);
                }
                None => break,
            }
        }
    }

    fn len(&self) -> usize {
        self.ids.len()
    }
}

// ---------------------------------------------------------------------------
// WhatsAppAdapter
// ---------------------------------------------------------------------------

/// WhatsApp Business API platform adapter.
pub struct WhatsAppAdapter {
    base: BasePlatformAdapter,
    config: WhatsAppConfig,
    client: Client,
    stop_signal: Arc<Notify>,
    dedup: std::sync::Mutex<WamidDedup>,
    /// Inbound channel to the gateway, set by the registry via
    /// [`WhatsAppAdapter::set_inbound_sender`].
    inbound_tx: Arc<RwLock<Option<mpsc::Sender<IncomingMessage>>>>,
    /// Shutdown signal for the webhook server task spawned by `start()`.
    shutdown_tx: RwLock<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl WhatsAppAdapter {
    /// Create a new WhatsApp adapter with the given configuration.
    pub fn new(config: WhatsAppConfig) -> Result<Self, GatewayError> {
        let base = BasePlatformAdapter::new(&config.token).with_proxy(config.proxy.clone());
        base.validate_token()?;
        let client = base.build_client()?;

        Ok(Self {
            base,
            config,
            client,
            stop_signal: Arc::new(Notify::new()),
            dedup: std::sync::Mutex::new(WamidDedup::default()),
            inbound_tx: Arc::new(RwLock::new(None)),
            shutdown_tx: RwLock::new(None),
        })
    }

    pub fn config(&self) -> &WhatsAppConfig {
        &self.config
    }

    /// Attach the gateway's inbound channel. Parsed webhook messages are
    /// forwarded on it by the server spawned in [`Self::start`].
    pub async fn set_inbound_sender(&self, tx: mpsc::Sender<IncomingMessage>) {
        *self.inbound_tx.write().await = Some(tx);
    }

    /// Convert a parsed webhook [`IncomingWhatsAppMessage`] into a
    /// gateway-level [`IncomingMessage`], applying wamid dedup.
    ///
    /// Returns `None` when the message is a redelivery (already seen).
    fn wamid_dedup_convert(
        dedup: &std::sync::Mutex<WamidDedup>,
        msg: &IncomingWhatsAppMessage,
    ) -> Option<IncomingMessage> {
        if dedup
            .lock()
            .map(|mut d| d.check_and_insert(&msg.message_id))
            .unwrap_or(false)
        {
            debug!(wamid = %msg.message_id, "WhatsApp: dropping redelivered message");
            return None;
        }
        Some(IncomingMessage {
            platform: "whatsapp".into(),
            chat_id: msg.from.clone(),
            user_id: msg.from.clone(),
            text: msg.text.clone(),
            message_id: Some(msg.message_id.clone()),
            thread_id: None,
            // WhatsApp has no groups in this adapter: every message is a DM.
            is_dm: true,
        })
    }

    /// Send a text message via WhatsApp Cloud API.
    ///
    /// Texts longer than [`MAX_MESSAGE_LENGTH`] are split fence-aware into
    /// sequential messages.
    pub async fn send_text(&self, to: &str, text: &str) -> Result<(), GatewayError> {
        let phone_id = self
            .config
            .phone_number_id
            .as_deref()
            .ok_or_else(|| GatewayError::SendFailed("phone_number_id not configured".into()))?;

        let url = format!("{}/{}/messages", WHATSAPP_API_BASE, phone_id);

        for chunk in split_markdown(text, MAX_MESSAGE_LENGTH) {
            let body = serde_json::json!({
                "messaging_product": "whatsapp",
                "to": to,
                "type": "text",
                "text": { "body": chunk }
            });

            let send_request = || {
                self.client
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", self.config.token))
                    .json(&body)
                    .send()
            };
            // Retries once after an HTTP 429.
            let resp = send_with_rate_limit_retry(send_request)
                .await
                .map_err(|e| GatewayError::SendFailed(format!("WhatsApp send failed: {}", e)))?;

            if !resp.status().is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(GatewayError::SendFailed(format!(
                    "WhatsApp API error: {}",
                    text
                )));
            }
        }
        Ok(())
    }

    /// Verify a WhatsApp webhook subscription challenge.
    ///
    /// Returns `Some(challenge)` if `mode` is `"subscribe"` and `token` matches
    /// the configured `verify_token`; otherwise returns `None`.
    pub fn verify_webhook(
        mode: &str,
        token: &str,
        challenge: &str,
        verify_token: &str,
    ) -> Option<String> {
        if mode == "subscribe" && token == verify_token {
            Some(challenge.to_string())
        } else {
            None
        }
    }

    /// Verify a WhatsApp webhook POST before any body processing.
    ///
    /// Checks Meta's `X-Hub-Signature-256` header (`sha256=<hex>`) against the
    /// HMAC-SHA256 of the raw request body, using a constant-time comparison
    /// (see [`verify_hmac_sha256_signature`]).
    ///
    /// The app secret comes from [`WhatsAppConfig::app_secret`], falling back
    /// to the `WHATSAPP_APP_SECRET` environment variable. Verification is
    /// fail-closed:
    /// - secret missing/empty -> `Err((503, _))` + warn log,
    /// - header missing/invalid -> `Err((403, _))` + warn log,
    /// - match -> `Ok(())`.
    ///
    /// NOTE: this only authenticates the subscription *challenge*
    /// ([`verify_webhook`]); [`parse_webhook_event`] itself does not
    /// authenticate the caller, so always run this check first on the raw
    /// body for every inbound POST.
    pub fn verify_webhook_post(
        &self,
        body: &[u8],
        signature_header: Option<&str>,
    ) -> Result<(), (u16, &'static str)> {
        let secret = self
            .config
            .app_secret
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                std::env::var("WHATSAPP_APP_SECRET")
                    .ok()
                    .filter(|s| !s.is_empty())
            });
        Self::verify_webhook_post_with_secret(secret.as_deref(), body, signature_header)
    }

    /// Core of [`verify_webhook_post`] with the secret resolved explicitly,
    /// so the fail-closed behavior is unit-testable without env mutation.
    fn verify_webhook_post_with_secret(
        secret: Option<&str>,
        body: &[u8],
        signature_header: Option<&str>,
    ) -> Result<(), (u16, &'static str)> {
        let secret = match secret {
            Some(s) if !s.is_empty() => s,
            _ => {
                warn!("WHATSAPP_APP_SECRET not set; rejecting WhatsApp webhook (fail-closed, 503)");
                return Err((503, "whatsapp webhook not configured"));
            }
        };
        let header = signature_header.unwrap_or("");
        if verify_hmac_sha256_signature(secret, body, header) {
            Ok(())
        } else {
            warn!("invalid WhatsApp webhook signature; rejecting (403)");
            Err((403, "invalid webhook signature"))
        }
    }

    /// Parse incoming messages from a WhatsApp webhook notification body.
    ///
    /// Walks through `entry[].changes[].value.messages[]` and extracts text
    /// messages (other types are recorded with an empty `text` field).
    pub fn parse_webhook_event(body: &serde_json::Value) -> Vec<IncomingWhatsAppMessage> {
        let mut messages = Vec::new();

        let entries = match body.get("entry").and_then(|v| v.as_array()) {
            Some(arr) => arr,
            None => return messages,
        };

        for entry in entries {
            let changes = match entry.get("changes").and_then(|v| v.as_array()) {
                Some(arr) => arr,
                None => continue,
            };
            for change in changes {
                let value = match change.get("value") {
                    Some(v) => v,
                    None => continue,
                };
                let msgs = match value.get("messages").and_then(|v| v.as_array()) {
                    Some(arr) => arr,
                    None => continue,
                };
                for msg in msgs {
                    let from = msg
                        .get("from")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let message_id = msg
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let message_type = msg
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("text")
                        .to_string();
                    let timestamp = msg
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    let text = msg
                        .get("text")
                        .and_then(|t| t.get("body"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    messages.push(IncomingWhatsAppMessage {
                        from,
                        message_id,
                        text,
                        message_type,
                        timestamp,
                    });
                }
            }
        }

        messages
    }

    /// Drop inbound messages whose wamid was already processed.
    ///
    /// Apply this to the output of [`Self::parse_webhook_event`] before
    /// dispatching to the agent loop so Meta webhook redeliveries are not
    /// processed twice. Thread-safe (`&self`); no `.await` needed.
    pub fn dedup_messages(
        &self,
        messages: Vec<IncomingWhatsAppMessage>,
    ) -> Vec<IncomingWhatsAppMessage> {
        let mut dedup = self.dedup.lock().unwrap_or_else(|e| e.into_inner());
        messages
            .into_iter()
            .filter(|m| {
                if dedup.check_and_insert(&m.message_id) {
                    debug!(wamid = %m.message_id, "Dropping duplicate WhatsApp message");
                    false
                } else {
                    true
                }
            })
            .collect()
    }

    /// Number of wamids currently tracked (for tests/observability).
    #[cfg(test)]
    pub fn dedup_len(&self) -> usize {
        self.dedup.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Mark a message as read via the WhatsApp Cloud API.
    pub async fn mark_as_read(&self, message_id: &str) -> Result<(), GatewayError> {
        let phone_id = self
            .config
            .phone_number_id
            .as_deref()
            .ok_or_else(|| GatewayError::SendFailed("phone_number_id not configured".into()))?;

        let url = format!("{}/{}/messages", WHATSAPP_API_BASE, phone_id);
        let body = serde_json::json!({
            "messaging_product": "whatsapp",
            "status": "read",
            "message_id": message_id
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                GatewayError::SendFailed(format!("WhatsApp mark_as_read failed: {}", e))
            })?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "WhatsApp mark_as_read error: {}",
                text
            )));
        }
        Ok(())
    }

    /// Send a reaction emoji to a specific message.
    pub async fn send_reaction(
        &self,
        to: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), GatewayError> {
        let phone_id = self
            .config
            .phone_number_id
            .as_deref()
            .ok_or_else(|| GatewayError::SendFailed("phone_number_id not configured".into()))?;

        let url = format!("{}/{}/messages", WHATSAPP_API_BASE, phone_id);
        let body = serde_json::json!({
            "messaging_product": "whatsapp",
            "to": to,
            "type": "reaction",
            "reaction": {
                "message_id": message_id,
                "emoji": emoji
            }
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                GatewayError::SendFailed(format!("WhatsApp reaction send failed: {}", e))
            })?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "WhatsApp reaction error: {}",
                text
            )));
        }
        Ok(())
    }

    /// Send a media message (image/document) via WhatsApp Cloud API.
    pub async fn send_media(
        &self,
        to: &str,
        media_type: &str,
        media_url: &str,
        caption: Option<&str>,
    ) -> Result<(), GatewayError> {
        let phone_id = self
            .config
            .phone_number_id
            .as_deref()
            .ok_or_else(|| GatewayError::SendFailed("phone_number_id not configured".into()))?;

        let url = format!("{}/{}/messages", WHATSAPP_API_BASE, phone_id);
        let mut media_obj = serde_json::json!({ "link": media_url });
        if let Some(cap) = caption {
            media_obj["caption"] = serde_json::Value::String(cap.to_string());
        }

        let body = serde_json::json!({
            "messaging_product": "whatsapp",
            "to": to,
            "type": media_type,
            media_type: media_obj
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("WhatsApp media send failed: {}", e)))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "WhatsApp API error: {}",
                text
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Inbound webhook server
// ---------------------------------------------------------------------------

// HTTP request/response plumbing lives in `crate::platforms::helpers`
// (`read_http_request`, `write_http_response`, `parse_http_query`,
// `constant_time_eq`) and is shared with the Feishu/BlueBubbles/SMS
// webhook receivers.

/// Handle one inbound webhook connection.
///
/// - `GET <path>?hub.mode=subscribe&hub.verify_token=...&hub.challenge=...`:
///   Meta subscription handshake; echoes the challenge on success
///   (fail-closed 503/400/403 without a configured/matching verify token).
/// - `POST <path>`: Meta event notification; the raw body is HMAC-SHA256
///   verified against the app secret (fail-closed 503/401), then parsed
///   via [`WhatsAppAdapter::parse_webhook_event`] and forwarded to the
///   gateway inbound channel. Always 200 once verified — Meta retries
///   non-200 deliveries for up to 7 days, which would multiply agent work.
async fn handle_webhook_request(
    mut stream: tokio::net::TcpStream,
    expected_path: &str,
    verify_token: Option<&str>,
    app_secret: Option<&str>,
    inbound_tx: Arc<RwLock<Option<mpsc::Sender<IncomingMessage>>>>,
    dedup: Arc<std::sync::Mutex<WamidDedup>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (request_line, headers, body) =
        read_http_request(&mut stream, WEBHOOK_MAX_BODY_BYTES).await?;
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    let method = parts.first().copied().unwrap_or("GET");
    let target = parts.get(1).copied().unwrap_or("/");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    if path != expected_path {
        write_http_response(&mut stream, 404, "text/plain", "not found").await?;
        return Ok(());
    }

    match method {
        "GET" => {
            let Some(token) = verify_token.filter(|t| !t.is_empty()) else {
                // Refuse rather than accept any token, which would let an
                // attacker subscribe.
                write_http_response(
                    &mut stream,
                    503,
                    "text/plain",
                    "verify_token not configured",
                )
                .await?;
                return Ok(());
            };
            let q = parse_http_query(query);
            let get = |k: &str| q.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
            let mode = get("hub.mode").unwrap_or("");
            let challenge = get("hub.challenge").unwrap_or("");
            let provided = get("hub.verify_token").unwrap_or("");
            if mode != "subscribe" {
                write_http_response(&mut stream, 400, "text/plain", "bad mode").await?;
                return Ok(());
            }
            // Constant-time comparison.
            let matched = constant_time_eq(provided, token);
            if !matched {
                warn!("WhatsApp webhook GET: verify_token mismatch");
                write_http_response(&mut stream, 403, "text/plain", "verify_token mismatch")
                    .await?;
                return Ok(());
            }
            if challenge.is_empty() {
                write_http_response(&mut stream, 400, "text/plain", "missing challenge").await?;
                return Ok(());
            }
            write_http_response(&mut stream, 200, "text/plain", challenge).await?;
        }
        "POST" => {
            // Without the app secret the sender can't be authenticated.
            let Some(secret) = app_secret.filter(|s| !s.is_empty()) else {
                warn!("WhatsApp webhook POST refused: app_secret unset");
                write_http_response(&mut stream, 503, "text/plain", "app_secret not configured")
                    .await?;
                return Ok(());
            };
            if body.len() >= WEBHOOK_MAX_BODY_BYTES {
                write_http_response(&mut stream, 413, "text/plain", "body too large").await?;
                return Ok(());
            }
            let signature = headers
                .iter()
                .find(|(k, _)| k == "x-hub-signature-256")
                .map(|(_, v)| v.as_str())
                .unwrap_or("");
            if !verify_hmac_sha256_signature(secret, &body, signature) {
                warn!("WhatsApp webhook POST: invalid X-Hub-Signature-256");
                write_http_response(&mut stream, 401, "text/plain", "invalid signature").await?;
                return Ok(());
            }
            match serde_json::from_slice::<serde_json::Value>(&body) {
                Ok(payload) => {
                    let messages = WhatsAppAdapter::parse_webhook_event(&payload);
                    let tx = inbound_tx.read().await.clone();
                    if let Some(tx) = tx {
                        for msg in &messages {
                            let Some(incoming) = WhatsAppAdapter::wamid_dedup_convert(&dedup, msg)
                            else {
                                continue;
                            };
                            if tx.send(incoming).await.is_err() {
                                debug!("WhatsApp inbound receiver dropped");
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("WhatsApp webhook body is not valid JSON: {e}");
                }
            }
            write_http_response(&mut stream, 200, "text/plain", "ok").await?;
        }
        _ => {
            write_http_response(&mut stream, 404, "text/plain", "not found").await?;
        }
    }
    Ok(())
}

#[async_trait]
impl PlatformAdapter for WhatsAppAdapter {
    async fn start(&self) -> Result<(), GatewayError> {
        info!("WhatsApp adapter starting");
        let host = self
            .config
            .webhook_host
            .clone()
            .unwrap_or_else(|| DEFAULT_WEBHOOK_HOST.to_string());
        let port = self.config.webhook_port.unwrap_or(DEFAULT_WEBHOOK_PORT);
        let path = self
            .config
            .webhook_path
            .clone()
            .unwrap_or_else(|| DEFAULT_WEBHOOK_PATH.to_string());
        let verify_token = self.config.verify_token.clone();
        // Resolve the app secret once; the request handler re-checks it
        // per request so the fail-closed 503 behavior of
        // `verify_webhook_post` is preserved.
        let app_secret = self
            .config
            .app_secret
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                std::env::var("WHATSAPP_APP_SECRET")
                    .ok()
                    .filter(|s| !s.is_empty())
            });
        if app_secret.is_none() {
            warn!(
                "WHATSAPP_APP_SECRET is not set — incoming webhook POSTs will be refused with 503. \
                 Set the app secret to enable inbound message delivery."
            );
        }
        if verify_token.as_deref().map(str::is_empty).unwrap_or(true) {
            warn!(
                "WhatsApp verify_token is not set — the GET subscription handshake will fail until it is."
            );
        }

        let addr: SocketAddr = format!("{host}:{port}").parse().map_err(|e| {
            GatewayError::ConnectionFailed(format!("WhatsApp webhook invalid address: {e}"))
        })?;
        let inbound_tx = self.inbound_tx.clone();
        let dedup = Arc::new(std::sync::Mutex::new(WamidDedup::default()));
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        *self.shutdown_tx.write().await = Some(shutdown_tx);

        tokio::spawn(async move {
            let listener = match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => l,
                Err(e) => {
                    error!("WhatsApp webhook server failed to bind {addr}: {e}");
                    return;
                }
            };
            info!("WhatsApp webhook listening on {addr}{path}");
            let mut shutdown_rx = shutdown_rx;
            loop {
                tokio::select! {
                    accept = listener.accept() => {
                        match accept {
                            Ok((stream, peer)) => {
                                let verify_token = verify_token.clone();
                                let app_secret = app_secret.clone();
                                let path = path.clone();
                                let inbound_tx = inbound_tx.clone();
                                let dedup = dedup.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = handle_webhook_request(
                                        stream,
                                        &path,
                                        verify_token.as_deref(),
                                        app_secret.as_deref(),
                                        inbound_tx,
                                        dedup,
                                    )
                                    .await
                                    {
                                        debug!("WhatsApp webhook request error from {peer}: {e}");
                                    }
                                });
                            }
                            Err(e) => warn!("WhatsApp webhook accept error: {e}"),
                        }
                    }
                    _ = &mut shutdown_rx => {
                        info!("WhatsApp webhook server shutting down");
                        break;
                    }
                }
            }
        });

        self.base.mark_running();
        Ok(())
    }

    async fn stop(&self) -> Result<(), GatewayError> {
        info!("WhatsApp adapter stopping");
        if let Some(tx) = self.shutdown_tx.write().await.take() {
            let _ = tx.send(());
        }
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
        self.send_text(chat_id, text).await
    }

    async fn edit_message(
        &self,
        _chat_id: &str,
        _message_id: &str,
        _text: &str,
    ) -> Result<(), GatewayError> {
        // WhatsApp does not natively support message editing
        debug!("WhatsApp does not support message editing");
        Ok(())
    }

    async fn send_file(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<(), GatewayError> {
        use crate::platforms::helpers::{media_category, mime_from_extension};

        let phone_id = self
            .config
            .phone_number_id
            .as_deref()
            .ok_or_else(|| GatewayError::SendFailed("phone_number_id not configured".into()))?;

        let path = std::path::Path::new(file_path);
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let mime = mime_from_extension(ext);
        let file_bytes = tokio::fs::read(file_path)
            .await
            .map_err(|e| GatewayError::SendFailed(format!("Failed to read file: {e}")))?;
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");

        // Step 1: Upload media to WhatsApp Cloud API
        let upload_url = format!("{}/{}/media", WHATSAPP_API_BASE, phone_id);
        let part = reqwest::multipart::Part::bytes(file_bytes)
            .file_name(file_name.to_string())
            .mime_str(mime)
            .map_err(|e| GatewayError::SendFailed(format!("MIME error: {e}")))?;
        let form = reqwest::multipart::Form::new()
            .text("messaging_product", "whatsapp")
            .part("file", part);

        let resp = self
            .client
            .post(&upload_url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .multipart(form)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("WhatsApp media upload failed: {e}")))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "WhatsApp media upload error: {text}"
            )));
        }

        let result: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("WhatsApp upload parse failed: {e}")))?;
        let media_id = result.get("id").and_then(|v| v.as_str()).unwrap_or("");

        // Step 2: Send message with uploaded media ID
        let media_type = match media_category(ext) {
            "image" => "image",
            "video" => "video",
            "audio" => "audio",
            _ => "document",
        };

        let send_url = format!("{}/{}/messages", WHATSAPP_API_BASE, phone_id);
        let mut media_obj = serde_json::json!({ "id": media_id });
        if let Some(cap) = caption {
            media_obj["caption"] = serde_json::Value::String(cap.to_string());
        }
        if media_type == "document" {
            media_obj["filename"] = serde_json::Value::String(file_name.to_string());
        }

        let body = serde_json::json!({
            "messaging_product": "whatsapp",
            "to": chat_id,
            "type": media_type,
            media_type: media_obj
        });

        let resp = self
            .client
            .post(&send_url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("WhatsApp media send failed: {e}")))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "WhatsApp media send error: {text}"
            )));
        }
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }
    fn platform_name(&self) -> &str {
        "whatsapp"
    }

    /// Trim the wamid dedup cache to [`MAX_DEDUP_WAMIDS`] entries so
    /// long-running gateways reclaim memory.
    async fn maintenance_prune(&self) {
        let len = {
            let mut dedup = self.dedup.lock().unwrap_or_else(|e| e.into_inner());
            dedup.trim_to_cap();
            dedup.len()
        };
        debug!(dedup_ids = len, "WhatsApp wamid dedup pruned");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    #[test]
    fn split_markdown_whatsapp_limit() {
        // WhatsApp splits (never truncates) at 4096 chars, fence-aware.
        let code: String = (0..300)
            .map(|i| format!("let v_{i} = f({i});"))
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!("Intro\n```rust\n{code}\n```\nOutro");
        let chunks = split_markdown(&text, MAX_MESSAGE_LENGTH);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.len() <= 4196, "chunk exceeds WhatsApp limit: {}", c.len());
            assert_eq!(c.matches("```").count() % 2, 0);
        }
    }

    fn test_adapter() -> WhatsAppAdapter {
        WhatsAppAdapter::new(WhatsAppConfig {
            token: "tok".into(),
            phone_number_id: None,
            business_account_id: None,
            verify_token: None,
            app_secret: None,
            webhook_host: None,
            webhook_port: None,
            webhook_path: None,
            proxy: Default::default(),
        })
        .expect("adapter")
    }

    fn webhook_body(ids: &[&str]) -> serde_json::Value {
        let messages: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| {
                serde_json::json!({
                    "from": "15551234567",
                    "id": id,
                    "timestamp": "1760000000",
                    "text": { "body": "hi" },
                    "type": "text"
                })
            })
            .collect();
        serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": { "messages": messages }
                }]
            }]
        })
    }

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let out = mac.finalize().into_bytes();
        let mut hex = String::with_capacity(64);
        for b in out {
            use std::fmt::Write;
            let _ = write!(hex, "{b:02x}");
        }
        format!("sha256={hex}")
    }

    #[test]
    fn whatsapp_dedup_drops_redelivered_wamids() {
        let adapter = test_adapter();

        let batch = WhatsAppAdapter::parse_webhook_event(&webhook_body(&["wamid.A1", "wamid.A2"]));
        let kept = adapter.dedup_messages(batch);
        assert_eq!(kept.len(), 2);
        assert_eq!(adapter.dedup_len(), 2);

        // Meta redelivers the same webhook → both dropped.
        let redelivered =
            WhatsAppAdapter::parse_webhook_event(&webhook_body(&["wamid.A1", "wamid.A2"]));
        let kept = adapter.dedup_messages(redelivered);
        assert!(kept.is_empty());
        assert_eq!(adapter.dedup_len(), 2);

        // A fresh message still passes.
        let fresh = WhatsAppAdapter::parse_webhook_event(&webhook_body(&["wamid.A3"]));
        let kept = adapter.dedup_messages(fresh);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].message_id, "wamid.A3");
        assert_eq!(adapter.dedup_len(), 3);
    }

    #[test]
    fn whatsapp_dedup_evicts_oldest_past_cap() {
        let adapter = test_adapter();

        {
            let mut dedup = adapter.dedup.lock().unwrap();
            for i in 0..(MAX_DEDUP_WAMIDS + 100) {
                dedup.check_and_insert(&format!("wamid.{i:06}"));
            }
        }
        assert_eq!(adapter.dedup_len(), MAX_DEDUP_WAMIDS);

        // The oldest 100 were evicted → their IDs are treated as new again.
        // NOTE: `check_and_insert` mutates (it inserts new IDs), so the
        // still-tracked check must come first — re-inserting an evicted ID
        // pushes another ID out of the capped deque.
        {
            let mut dedup = adapter.dedup.lock().unwrap();
            assert!(dedup.check_and_insert("wamid.000100")); // still tracked
            assert!(!dedup.check_and_insert("wamid.000000")); // evicted → new again
        }
        assert_eq!(adapter.dedup_len(), MAX_DEDUP_WAMIDS);
    }

    #[tokio::test]
    async fn whatsapp_maintenance_prune_trims_dedup() {
        let adapter = test_adapter();
        {
            let mut dedup = adapter.dedup.lock().unwrap();
            for i in 0..(MAX_DEDUP_WAMIDS + 50) {
                dedup.check_and_insert(&format!("wamid.{i:06}"));
            }
        }
        // `check_and_insert` already trims, so prune is a no-op guard that
        // must keep the cache at the cap.
        PlatformAdapter::maintenance_prune(&adapter).await;
        assert_eq!(adapter.dedup_len(), MAX_DEDUP_WAMIDS);
    }

    /// RFC 4231 test case 2 (HMAC-SHA-256): key "Jefe",
    /// data "what do ya want for nothing?".
    #[test]
    fn whatsapp_signature_rfc4231_known_answer() {
        let body = b"what do ya want for nothing?";
        let header = "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";
        assert!(
            WhatsAppAdapter::verify_webhook_post_with_secret(Some("Jefe"), body, Some(header))
                .is_ok()
        );
    }

    #[test]
    fn whatsapp_signature_wrong_secret_or_body_rejected_403() {
        let body = b"hello";
        let header = sign("correct-secret", body);
        // Wrong secret.
        assert_eq!(
            WhatsAppAdapter::verify_webhook_post_with_secret(
                Some("wrong-secret"),
                body,
                Some(&header)
            ),
            Err((403, "invalid webhook signature"))
        );
        // Tampered body.
        assert_eq!(
            WhatsAppAdapter::verify_webhook_post_with_secret(
                Some("correct-secret"),
                b"tampered",
                Some(&header)
            ),
            Err((403, "invalid webhook signature"))
        );
        // Missing header.
        assert_eq!(
            WhatsAppAdapter::verify_webhook_post_with_secret(Some("correct-secret"), body, None),
            Err((403, "invalid webhook signature"))
        );
        // Header without sha256= prefix.
        assert_eq!(
            WhatsAppAdapter::verify_webhook_post_with_secret(
                Some("correct-secret"),
                body,
                Some(header.trim_start_matches("sha256="))
            ),
            Err((403, "invalid webhook signature"))
        );
    }

    #[test]
    fn whatsapp_signature_missing_secret_rejected_503_fail_closed() {
        let body = b"hello";
        let header = sign("any", body);
        assert_eq!(
            WhatsAppAdapter::verify_webhook_post_with_secret(None, body, Some(&header)),
            Err((503, "whatsapp webhook not configured"))
        );
        assert_eq!(
            WhatsAppAdapter::verify_webhook_post_with_secret(Some(""), body, Some(&header)),
            Err((503, "whatsapp webhook not configured"))
        );
    }

    #[test]
    fn whatsapp_verify_webhook_post_uses_config_secret() {
        let cfg = WhatsAppConfig {
            token: "tok".into(),
            phone_number_id: None,
            business_account_id: None,
            verify_token: None,
            app_secret: Some("cfg-secret".into()),
            webhook_host: None,
            webhook_port: None,
            webhook_path: None,
            proxy: Default::default(),
        };
        let adapter = WhatsAppAdapter::new(cfg).expect("adapter");
        let body = b"ping";
        let header = sign("cfg-secret", body);
        assert!(adapter.verify_webhook_post(body, Some(&header)).is_ok());
        // Config secret beats the env fallback; env is not consulted here.
        assert_eq!(
            adapter.verify_webhook_post(body, None),
            Err((403, "invalid webhook signature"))
        );
    }

    #[test]
    fn whatsapp_verify_webhook_post_env_fallback_fail_closed() {
        // Config has no secret; env fallback is consulted.
        let cfg = WhatsAppConfig {
            token: "tok".into(),
            phone_number_id: None,
            business_account_id: None,
            verify_token: None,
            app_secret: None,
            webhook_host: None,
            webhook_port: None,
            webhook_path: None,
            proxy: Default::default(),
        };
        let adapter = WhatsAppAdapter::new(cfg).expect("adapter");
        let body = b"ping";
        std::env::set_var("WHATSAPP_APP_SECRET", "env-secret-xyz");
        let header = sign("env-secret-xyz", body);
        assert!(adapter.verify_webhook_post(body, Some(&header)).is_ok());
        std::env::remove_var("WHATSAPP_APP_SECRET");
        assert_eq!(
            adapter.verify_webhook_post(body, Some(&header)),
            Err((503, "whatsapp webhook not configured"))
        );
    }

    #[test]
    fn whatsapp_verify_webhook_challenge() {
        assert_eq!(
            WhatsAppAdapter::verify_webhook("subscribe", "tok123", "chal", "tok123"),
            Some("chal".to_string())
        );
        assert_eq!(
            WhatsAppAdapter::verify_webhook("subscribe", "wrong", "chal", "tok123"),
            None
        );
        assert_eq!(
            WhatsAppAdapter::verify_webhook("unsubscribe", "tok123", "chal", "tok123"),
            None
        );
    }

    #[test]
    fn whatsapp_wamid_dedup_convert_drops_redelivery() {
        let dedup = std::sync::Mutex::new(WamidDedup::default());
        let msg = IncomingWhatsAppMessage {
            from: "15551234567".to_string(),
            message_id: "wamid.1".to_string(),
            text: "hi".to_string(),
            message_type: "text".to_string(),
            timestamp: "1760000000".to_string(),
        };
        let incoming = WhatsAppAdapter::wamid_dedup_convert(&dedup, &msg).expect("first delivery");
        assert_eq!(incoming.platform, "whatsapp");
        assert_eq!(incoming.chat_id, "15551234567");
        assert!(incoming.is_dm);
        assert_eq!(incoming.message_id, Some("wamid.1".to_string()));
        // Redelivery of the same wamid is dropped.
        assert!(WhatsAppAdapter::wamid_dedup_convert(&dedup, &msg).is_none());
    }

    #[test]
    fn whatsapp_parse_webhook_event_text() {
        let payload = webhook_body(&["wamid.abc"]);
        let messages = WhatsAppAdapter::parse_webhook_event(&payload);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "15551234567");
        assert_eq!(messages[0].message_id, "wamid.abc");
        assert_eq!(messages[0].text, "hi");
    }
}
