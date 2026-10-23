//! BlueBubbles REST API adapter (iMessage).

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, RwLock};
use tracing::{debug, error, info, warn};

use hermes_core::errors::GatewayError;
use hermes_core::traits::{ParseMode, PlatformAdapter};

use crate::adapter::{AdapterProxyConfig, BasePlatformAdapter};
use crate::gateway::IncomingMessage;
use crate::platforms::helpers::{
    constant_time_eq, http_url_decode, parse_http_query, read_http_request, write_http_response,
};

/// Default bind host for the inbound webhook server.
const DEFAULT_WEBHOOK_HOST: &str = "0.0.0.0";

/// Default port for the inbound webhook server (mirrors the original).
const DEFAULT_WEBHOOK_PORT: u16 = 8090;

/// Default path for the inbound webhook server (mirrors the original).
const DEFAULT_WEBHOOK_PATH: &str = "/bluebubbles-webhook";

/// Maximum webhook POST body accepted.
const WEBHOOK_MAX_BODY_BYTES: usize = 3 * 1024 * 1024;

/// Webhook event types that carry user messages (mirrors the original).
const MESSAGE_EVENTS: &[&str] = &["new-message", "message", "updated-message"];

/// Parsed incoming BlueBubbles message from a webhook event.
#[derive(Debug, Clone)]
pub struct IncomingBlueBubblesMessage {
    pub chat_guid: String,
    pub chat_identifier: String,
    pub sender: String,
    pub text: String,
    pub is_group: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlueBubblesConfig {
    pub server_url: String,
    pub password: String,
    /// Bind host for the inbound webhook server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_host: Option<String>,
    /// Port for the inbound webhook server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_port: Option<u16>,
    /// Path for the inbound webhook server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_path: Option<String>,
    #[serde(default)]
    pub proxy: AdapterProxyConfig,
}

pub struct BlueBubblesAdapter {
    base: BasePlatformAdapter,
    config: BlueBubblesConfig,
    client: Client,
    stop_signal: Arc<Notify>,
    /// Inbound channel to the gateway, set by the registry via
    /// [`BlueBubblesAdapter::set_inbound_sender`].
    inbound_tx: Arc<RwLock<Option<tokio::sync::mpsc::Sender<IncomingMessage>>>>,
    /// Shutdown signal for the webhook server task spawned by `start()`.
    shutdown_tx: RwLock<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl BlueBubblesAdapter {
    pub fn new(config: BlueBubblesConfig) -> Result<Self, GatewayError> {
        let base = BasePlatformAdapter::new(&config.password).with_proxy(config.proxy.clone());
        base.validate_token()?;
        let client = base.build_client()?;
        Ok(Self {
            base,
            config,
            client,
            stop_signal: Arc::new(Notify::new()),
            inbound_tx: Arc::new(RwLock::new(None)),
            shutdown_tx: RwLock::new(None),
        })
    }

    pub fn config(&self) -> &BlueBubblesConfig {
        &self.config
    }

    /// Attach the gateway's inbound channel. Parsed webhook messages are
    /// forwarded on it by the server spawned in [`Self::start`].
    pub async fn set_inbound_sender(&self, tx: tokio::sync::mpsc::Sender<IncomingMessage>) {
        *self.inbound_tx.write().await = Some(tx);
    }

    /// First non-empty string candidate, mirroring the original's `_value`.
    fn first_str(candidates: &[Option<&str>]) -> Option<String> {
        candidates
            .iter()
            .filter_map(|c| *c)
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(String::from)
    }

    /// Get a string field from a JSON object.
    fn str_field<'a>(v: &'a serde_json::Value, k: &str) -> Option<&'a str> {
        v.get(k).and_then(|x| x.as_str())
    }

    /// Extract the message record from the many BlueBubbles webhook payload
    /// shapes (`data` dict/list, `message` dict, or the payload itself).
    fn payload_record(payload: &serde_json::Value) -> &serde_json::Value {
        if let Some(data) = payload.get("data") {
            if data.is_object() {
                return data;
            }
            if let Some(arr) = data.as_array() {
                if let Some(first) = arr.iter().find(|v| v.is_object()) {
                    return first;
                }
            }
        }
        if let Some(msg) = payload.get("message") {
            if msg.is_object() {
                return msg;
            }
        }
        payload
    }

    /// Parse a BlueBubbles webhook body into an incoming message.
    ///
    /// Returns `None` for non-message events (silently acked), own
    /// (`isFromMe`) messages, and tapback reactions — mirroring the
    /// original's filters.
    pub fn parse_webhook_event(body: &serde_json::Value) -> Option<IncomingBlueBubblesMessage> {
        let str_field = Self::str_field;
        let event_type = Self::first_str(&[str_field(body, "type"), str_field(body, "event")])
            .unwrap_or_default();
        if !event_type.is_empty() && !MESSAGE_EVENTS.contains(&event_type.as_str()) {
            return None;
        }

        let record = Self::payload_record(body);
        let is_from_me = record
            .get("isFromMe")
            .or_else(|| record.get("fromMe"))
            .or_else(|| record.get("is_from_me"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if is_from_me {
            return None;
        }
        // Tapback reactions are delivered as messages; skip them.
        if record
            .get("associatedMessageType")
            .and_then(|v| v.as_i64())
            .is_some_and(|t| t != 0)
        {
            return None;
        }

        let text = Self::first_str(&[
            str_field(record, "text"),
            str_field(record, "message"),
            str_field(record, "body"),
        ])
        .unwrap_or_default();

        let chat_guid = Self::first_str(&[
            str_field(record, "chatGuid"),
            str_field(body, "chatGuid"),
            str_field(record, "chat_guid"),
            str_field(body, "chat_guid"),
            str_field(body, "guid"),
        ])
        .or_else(|| {
            record
                .get("chats")
                .and_then(|c| c.as_array())
                .and_then(|arr| arr.first())
                .and_then(|chat| {
                    Self::first_str(&[str_field(chat, "guid"), str_field(chat, "chatGuid")])
                })
        })
        .unwrap_or_default();
        let chat_identifier = Self::first_str(&[
            str_field(record, "chatIdentifier"),
            str_field(record, "identifier"),
            str_field(body, "chatIdentifier"),
            str_field(body, "identifier"),
        ])
        .unwrap_or_default();
        let handle = record.get("handle");
        let handle_address = handle
            .and_then(|h| h.as_object())
            .and_then(|h| h.get("address"))
            .and_then(|v| v.as_str());
        let sender = Self::first_str(&[
            handle_address,
            str_field(record, "sender"),
            str_field(record, "from"),
            str_field(record, "address"),
        ])
        .or_else(|| {
            if !chat_identifier.is_empty() {
                Some(chat_identifier.clone())
            } else if !chat_guid.is_empty() {
                Some(chat_guid.clone())
            } else {
                None
            }
        })
        .unwrap_or_default();

        if sender.is_empty()
            || (chat_guid.is_empty() && chat_identifier.is_empty())
            || text.is_empty()
        {
            return None;
        }

        let is_group = record
            .get("isGroup")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || chat_guid.contains(";+;");

        Some(IncomingBlueBubblesMessage {
            chat_guid,
            chat_identifier,
            sender,
            text,
            is_group,
        })
    }

    /// Convert a parsed message into a gateway-level [`IncomingMessage`].
    pub fn to_incoming(msg: &IncomingBlueBubblesMessage) -> IncomingMessage {
        IncomingMessage {
            platform: "bluebubbles".into(),
            chat_id: if msg.chat_guid.is_empty() {
                msg.chat_identifier.clone()
            } else {
                msg.chat_guid.clone()
            },
            user_id: msg.sender.clone(),
            text: msg.text.clone(),
            message_id: None,
            thread_id: None,
            is_dm: !msg.is_group,
        }
    }

    /// Send a text message via BlueBubbles REST API.
    pub async fn send_text(&self, chat_guid: &str, text: &str) -> Result<(), GatewayError> {
        let url = format!("{}/api/v1/message/text", self.config.server_url);
        let body = serde_json::json!({
            "chatGuid": chat_guid,
            "message": text,
            "method": "private-api"
        });

        let resp = self
            .client
            .post(&url)
            .query(&[("password", &self.config.password)])
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("BlueBubbles send failed: {}", e)))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "BlueBubbles API error: {}",
                text
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl PlatformAdapter for BlueBubblesAdapter {
    async fn start(&self) -> Result<(), GatewayError> {
        info!(
            "BlueBubbles adapter starting (server: {})",
            self.config.server_url
        );

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
        let password = self.config.password.clone();
        let addr: std::net::SocketAddr = format!("{host}:{port}").parse().map_err(|e| {
            GatewayError::ConnectionFailed(format!("BlueBubbles webhook invalid address: {e}"))
        })?;
        let inbound_tx = self.inbound_tx.clone();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        *self.shutdown_tx.write().await = Some(shutdown_tx);

        tokio::spawn(async move {
            let listener = match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => l,
                Err(e) => {
                    error!("BlueBubbles webhook server failed to bind {addr}: {e}");
                    return;
                }
            };
            info!("BlueBubbles webhook listening on {addr}{path}");
            let mut shutdown_rx = shutdown_rx;
            loop {
                tokio::select! {
                    accept = listener.accept() => {
                        match accept {
                            Ok((stream, peer)) => {
                                let password = password.clone();
                                let path = path.clone();
                                let inbound_tx = inbound_tx.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = handle_bluebubbles_webhook_request(
                                        stream, &path, &password, inbound_tx,
                                    )
                                    .await
                                    {
                                        debug!("BlueBubbles webhook request error from {peer}: {e}");
                                    }
                                });
                            }
                            Err(e) => warn!("BlueBubbles webhook accept error: {e}"),
                        }
                    }
                    _ = &mut shutdown_rx => {
                        info!("BlueBubbles webhook server shutting down");
                        break;
                    }
                }
            }
        });

        self.base.mark_running();
        Ok(())
    }

    async fn stop(&self) -> Result<(), GatewayError> {
        info!("BlueBubbles adapter stopping");
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
        debug!("BlueBubbles does not support message editing");
        Ok(())
    }

    async fn send_file(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<(), GatewayError> {
        use crate::platforms::helpers::mime_from_extension;

        let path = std::path::Path::new(file_path);
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let mime = mime_from_extension(ext);
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let file_bytes = tokio::fs::read(file_path)
            .await
            .map_err(|e| GatewayError::SendFailed(format!("Failed to read file: {e}")))?;

        // BlueBubbles supports attachment sending via multipart upload
        let url = format!("{}/api/v1/message/attachment", self.config.server_url);
        let part = reqwest::multipart::Part::bytes(file_bytes)
            .file_name(file_name.to_string())
            .mime_str(mime)
            .map_err(|e| GatewayError::SendFailed(format!("MIME error: {e}")))?;
        let form = reqwest::multipart::Form::new()
            .text("chatGuid", chat_id.to_string())
            .text("name", file_name.to_string())
            .part("attachment", part);

        let resp = self
            .client
            .post(&url)
            .query(&[("password", &self.config.password)])
            .multipart(form)
            .send()
            .await
            .map_err(|e| {
                GatewayError::SendFailed(format!("BlueBubbles attachment send failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "BlueBubbles attachment error: {text}"
            )));
        }

        // Send caption as a follow-up text if provided
        if let Some(cap) = caption {
            let _ = self.send_text(chat_id, cap).await;
        }
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }
    fn platform_name(&self) -> &str {
        "bluebubbles"
    }
}

// ---------------------------------------------------------------------------
// Inbound webhook server
// ---------------------------------------------------------------------------

/// Handle one inbound BlueBubbles webhook connection.
///
/// - `POST <path>`: the request must carry the server password as the
///   `password`/`guid` query param or the `x-password`/`x-guid`/
///   `x-bluebubbles-guid` header (constant-time compare, fail-closed 401).
///   Message-type events (`new-message`, `message`, `updated-message`) are
///   parsed via [`BlueBubblesAdapter::parse_webhook_event`] and forwarded
///   to the gateway inbound channel. All other events are acked silently.
async fn handle_bluebubbles_webhook_request(
    mut stream: tokio::net::TcpStream,
    expected_path: &str,
    password: &str,
    inbound_tx: Arc<RwLock<Option<tokio::sync::mpsc::Sender<IncomingMessage>>>>,
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
    if method != "POST" || path != expected_path {
        write_http_response(&mut stream, 404, "text/plain", "not found").await?;
        return Ok(());
    }

    // Webhook token: password as query param or header.
    let q = parse_http_query(query);
    let query_token = q
        .iter()
        .find(|(k, _)| k.as_str() == "password" || k.as_str() == "guid")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    let header_token = headers
        .iter()
        .find(|(k, _)| {
            k.as_str() == "x-password"
                || k.as_str() == "x-guid"
                || k.as_str() == "x-bluebubbles-guid"
        })
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    // Query params are already percent-decoded by `parse_http_query`;
    // only the raw header value still needs decoding.
    let provided: Vec<u8> = if !query_token.is_empty() {
        query_token.as_bytes().to_vec()
    } else {
        http_url_decode(header_token)
    };
    if !constant_time_eq(&provided, password) {
        warn!("BlueBubbles webhook: unauthorized (bad password)");
        write_http_response(
            &mut stream,
            401,
            "application/json",
            r#"{"error":"unauthorized"}"#,
        )
        .await?;
        return Ok(());
    }

    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            // Fall back to form-encoded bodies with a JSON field, mirroring
            // the original's `_parse_webhook_body`.
            let form = parse_http_query(&String::from_utf8_lossy(&body));
            let json_str = form
                .iter()
                .find(|(k, _)| {
                    k.as_str() == "payload" || k.as_str() == "data" || k.as_str() == "message"
                })
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            match serde_json::from_str(&json_str) {
                Ok(v) => v,
                Err(_) => {
                    write_http_response(
                        &mut stream,
                        400,
                        "application/json",
                        r#"{"error":"invalid payload"}"#,
                    )
                    .await?;
                    return Ok(());
                }
            }
        }
    };

    if let Some(msg) = BlueBubblesAdapter::parse_webhook_event(&payload) {
        let tx = inbound_tx.read().await.clone();
        if let Some(tx) = tx {
            let incoming = BlueBubblesAdapter::to_incoming(&msg);
            if tx.send(incoming).await.is_err() {
                debug!("BlueBubbles inbound receiver dropped");
            }
        }
    }
    write_http_response(&mut stream, 200, "text/plain", "ok").await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_new_message_event() {
        let body = serde_json::json!({
            "type": "new-message",
            "data": {
                "guid": "msg-guid-1",
                "text": "hello from imessage",
                "isFromMe": false,
                "chatGuid": "iMessage;-;+15551234567",
                "handle": { "address": "+15559876543" }
            }
        });
        let msg = BlueBubblesAdapter::parse_webhook_event(&body).unwrap();
        assert_eq!(msg.text, "hello from imessage");
        assert_eq!(msg.sender, "+15559876543");
        assert!(!msg.is_group);
        let incoming = BlueBubblesAdapter::to_incoming(&msg);
        assert_eq!(incoming.platform, "bluebubbles");
        assert_eq!(incoming.chat_id, "iMessage;-;+15551234567");
        assert!(incoming.is_dm);
    }

    #[test]
    fn parse_group_message_event() {
        let body = serde_json::json!({
            "type": "new-message",
            "data": {
                "text": "group hi",
                "chatGuid": "iMessage;+;chat-guid-9",
                "chatIdentifier": "chat-guid-9",
                "sender": "+15551110000",
                "isGroup": true
            }
        });
        let msg = BlueBubblesAdapter::parse_webhook_event(&body).unwrap();
        assert!(msg.is_group);
        assert!(!BlueBubblesAdapter::to_incoming(&msg).is_dm);
    }

    #[test]
    fn parse_skips_own_messages_and_other_events() {
        // Own messages are dropped.
        let own = serde_json::json!({
            "type": "new-message",
            "data": { "text": "echo", "isFromMe": true, "chatGuid": "g", "sender": "s" }
        });
        assert!(BlueBubblesAdapter::parse_webhook_event(&own).is_none());

        // Non-message events are acked silently (None).
        let typing = serde_json::json!({ "type": "typing-indicator", "data": {} });
        assert!(BlueBubblesAdapter::parse_webhook_event(&typing).is_none());

        // Tapback reactions are skipped.
        let tapback = serde_json::json!({
            "type": "new-message",
            "data": {
                "text": "Loved it",
                "associatedMessageType": 2000,
                "chatGuid": "g",
                "sender": "s"
            }
        });
        assert!(BlueBubblesAdapter::parse_webhook_event(&tapback).is_none());
    }
}
