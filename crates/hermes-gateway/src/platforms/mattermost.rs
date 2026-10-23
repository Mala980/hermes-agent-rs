//! Mattermost REST API + WebSocket adapter.

use std::sync::Arc;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Notify};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, info, warn};

use hermes_core::errors::GatewayError;
use hermes_core::traits::{ParseMode, PlatformAdapter};

use crate::adapter::{AdapterProxyConfig, BasePlatformAdapter};
use crate::gateway::IncomingMessage;

/// WebSocket path for the Mattermost real-time API.
const WS_PATH: &str = "/api/v4/websocket";

/// Base delay between WebSocket reconnect attempts; doubles per attempt,
/// capped by [`WS_RECONNECT_MAX_DELAY`].
const WS_RECONNECT_BASE_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
const WS_RECONNECT_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

/// Exponential backoff for WebSocket reconnect attempts.
fn reconnect_delay(attempt: u32) -> std::time::Duration {
    let shift = attempt.min(5);
    let secs = WS_RECONNECT_BASE_DELAY.as_secs() << shift;
    std::time::Duration::from_secs(secs.min(WS_RECONNECT_MAX_DELAY.as_secs()))
}

/// Build the WebSocket URL from the configured server URL
/// (`http(s)://host` → `ws(s)://host/api/v4/websocket`).
fn ws_url(server_url: &str) -> Result<String, GatewayError> {
    let trimmed = server_url.trim_end_matches('/');
    let ws = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        return Err(GatewayError::Platform(format!(
            "Mattermost server_url must start with http:// or https://: {server_url}"
        )));
    };
    Ok(format!("{ws}{WS_PATH}"))
}

// ---------------------------------------------------------------------------
// Incoming message types
// ---------------------------------------------------------------------------

/// A WebSocket event frame from the Mattermost real-time API.
#[derive(Debug, Clone, Deserialize)]
pub struct MattermostWsEvent {
    pub event: String,
    pub data: Option<serde_json::Value>,
    pub broadcast: Option<serde_json::Value>,
    pub seq: Option<i64>,
}

/// Parsed incoming message extracted from a `posted` WebSocket event.
#[derive(Debug, Clone)]
pub struct IncomingMattermostMessage {
    pub post_id: String,
    pub channel_id: String,
    pub user_id: String,
    pub message: String,
    pub is_bot: bool,
}

// ---------------------------------------------------------------------------
// MattermostConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MattermostConfig {
    pub server_url: String,
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    #[serde(default)]
    pub proxy: AdapterProxyConfig,
}

pub struct MattermostAdapter {
    base: BasePlatformAdapter,
    config: MattermostConfig,
    client: Client,
    stop_signal: Arc<Notify>,
}

impl MattermostAdapter {
    pub fn new(config: MattermostConfig) -> Result<Self, GatewayError> {
        let base = BasePlatformAdapter::new(&config.token).with_proxy(config.proxy.clone());
        base.validate_token()?;
        let client = base.build_client()?;
        Ok(Self {
            base,
            config,
            client,
            stop_signal: Arc::new(Notify::new()),
        })
    }

    pub fn config(&self) -> &MattermostConfig {
        &self.config
    }

    /// Send a message via Mattermost REST API.
    pub async fn send_text(&self, channel_id: &str, text: &str) -> Result<String, GatewayError> {
        let url = format!("{}/api/v4/posts", self.config.server_url);
        let body = serde_json::json!({
            "channel_id": channel_id,
            "message": text
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("Mattermost send failed: {}", e)))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "Mattermost API error: {}",
                text
            )));
        }

        let result: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("Mattermost parse failed: {}", e)))?;
        Ok(result
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    /// Parse a Mattermost WebSocket event into an incoming message.
    ///
    /// Only `posted` events that contain a valid post JSON are returned.
    pub fn parse_ws_event(event: &MattermostWsEvent) -> Option<IncomingMattermostMessage> {
        if event.event != "posted" {
            return None;
        }

        let data = event.data.as_ref()?;
        let post_str = data.get("post").and_then(|v| v.as_str())?;
        let post: serde_json::Value = serde_json::from_str(post_str).ok()?;

        let post_id = post.get("id").and_then(|v| v.as_str())?.to_string();
        let channel_id = post.get("channel_id").and_then(|v| v.as_str())?.to_string();
        let user_id = post.get("user_id").and_then(|v| v.as_str())?.to_string();
        let message = post
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let is_bot = post
            .get("props")
            .and_then(|p| p.get("from_bot"))
            .and_then(|v| v.as_str())
            .map(|v| v == "true")
            .unwrap_or(false);

        Some(IncomingMattermostMessage {
            post_id,
            channel_id,
            user_id,
            message,
            is_bot,
        })
    }

    // -----------------------------------------------------------------------
    // WebSocket: supervised receive loop
    // -----------------------------------------------------------------------

    /// Clone of the stop `Notify`, signaled by [`MattermostAdapter::stop`].
    pub fn stop_notifier(&self) -> Arc<Notify> {
        self.stop_signal.clone()
    }

    /// Convert a parsed [`IncomingMattermostMessage`] into a gateway-level
    /// [`IncomingMessage`]. Mattermost has no first-class DM flag on WS
    /// events, so `is_dm` is left false; the DM manager still applies its
    /// own pairing rules.
    pub fn to_incoming(msg: &IncomingMattermostMessage) -> IncomingMessage {
        IncomingMessage {
            platform: "mattermost".into(),
            chat_id: msg.channel_id.clone(),
            user_id: msg.user_id.clone(),
            text: msg.message.clone(),
            message_id: Some(msg.post_id.clone()),
            thread_id: None,
            is_dm: false,
        }
    }

    /// Run the supervised WebSocket receive loop, forwarding parsed
    /// `posted` events as platform-agnostic [`IncomingMessage`]s on `tx`.
    ///
    /// Connects to `/api/v4/websocket`, authenticates with the
    /// `authentication` action (token-based), and reconnects with
    /// exponential backoff on disconnect. Bot posts are skipped. The loop
    /// ends when [`MattermostAdapter::stop`] signals, or when the `tx`
    /// receiver is dropped.
    pub async fn run_ws_loop(self: &Arc<Self>, tx: mpsc::Sender<IncomingMessage>) {
        info!("Mattermost WebSocket loop started");
        let url = match ws_url(&self.config.server_url) {
            Ok(u) => u,
            Err(e) => {
                warn!("Mattermost WebSocket URL error: {e}");
                return;
            }
        };
        let mut attempt: u32 = 0;
        while self.base.is_running() {
            match connect_async(&url).await {
                Ok((ws_stream, _)) => {
                    attempt = 0;
                    if !self.serve_ws_connection(ws_stream, &tx).await {
                        // Receiver dropped: end the loop.
                        return;
                    }
                }
                Err(e) => {
                    warn!("Mattermost WebSocket connect failed: {e}");
                }
            }
            if !self.base.is_running() {
                break;
            }
            attempt = attempt.saturating_add(1);
            let delay = reconnect_delay(attempt);
            info!("Mattermost WebSocket reconnecting in {delay:?} (attempt {attempt})");
            tokio::select! {
                _ = self.stop_signal.notified() => break,
                _ = tokio::time::sleep(delay) => {}
            }
        }
        info!("Mattermost WebSocket loop stopped");
    }

    /// Serve one WebSocket connection: authenticate, then dispatch events.
    ///
    /// Returns `true` to reconnect, `false` when the inbound receiver was
    /// dropped and the loop must end.
    async fn serve_ws_connection(
        &self,
        ws_stream: tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tx: &mpsc::Sender<IncomingMessage>,
    ) -> bool {
        let (mut sink, mut stream) = ws_stream.split();
        let mut seq: i64 = 1;

        loop {
            tokio::select! {
                _ = self.stop_signal.notified() => {
                    let _ = sink.close().await;
                    return true;
                }
                msg = stream.next() => {
                    match msg {
                        Some(Ok(WsMessage::Text(text))) => {
                            let event: MattermostWsEvent = match serde_json::from_str(&text) {
                                Ok(e) => e,
                                Err(e) => {
                                    debug!("ignoring malformed Mattermost WS payload: {e}");
                                    continue;
                                }
                            };
                            if event.event == "hello" {
                                // Authenticate with the token.
                                let auth = serde_json::json!({
                                    "seq": seq,
                                    "action": "authentication",
                                    "data": { "token": self.config.token },
                                });
                                seq += 1;
                                if sink
                                    .send(WsMessage::Text(auth.to_string()))
                                    .await
                                    .is_err()
                                {
                                    return true;
                                }
                                continue;
                            }
                            if let Some(parsed) = Self::parse_ws_event(&event) {
                                if parsed.is_bot {
                                    continue;
                                }
                                if tx.send(Self::to_incoming(&parsed)).await.is_err() {
                                    info!(
                                        "Mattermost WS loop stopping (inbound receiver dropped)"
                                    );
                                    return false;
                                }
                            }
                        }
                        Some(Ok(WsMessage::Ping(p))) => {
                            let _ = sink.send(WsMessage::Pong(p)).await;
                        }
                        Some(Ok(WsMessage::Close(_))) | None => {
                            debug!("Mattermost WebSocket closed by peer");
                            return true;
                        }
                        Some(Err(e)) => {
                            warn!("Mattermost WebSocket error: {e}");
                            return true;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Fetch the authenticated user's profile (`GET /api/v4/users/me`).
    pub async fn get_me(&self) -> Result<serde_json::Value, GatewayError> {
        let url = format!("{}/api/v4/users/me", self.config.server_url);
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .send()
            .await
            .map_err(|e| {
                GatewayError::ConnectionFailed(format!("Mattermost get_me failed: {}", e))
            })?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::ConnectionFailed(format!(
                "Mattermost get_me error: {}",
                text
            )));
        }

        resp.json().await.map_err(|e| {
            GatewayError::ConnectionFailed(format!("Mattermost get_me parse failed: {}", e))
        })
    }

    /// Edit a message via Mattermost REST API.
    pub async fn edit_text(&self, post_id: &str, text: &str) -> Result<(), GatewayError> {
        let url = format!("{}/api/v4/posts/{}", self.config.server_url, post_id);
        let body = serde_json::json!({ "id": post_id, "message": text });

        let resp = self
            .client
            .put(&url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("Mattermost edit failed: {}", e)))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "Mattermost edit error: {}",
                text
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl PlatformAdapter for MattermostAdapter {
    async fn start(&self) -> Result<(), GatewayError> {
        info!(
            "Mattermost adapter starting (server: {})",
            self.config.server_url
        );
        self.base.mark_running();
        Ok(())
    }

    async fn stop(&self) -> Result<(), GatewayError> {
        info!("Mattermost adapter stopping");
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
        self.send_text(chat_id, text).await?;
        Ok(())
    }

    async fn edit_message(
        &self,
        _chat_id: &str,
        message_id: &str,
        text: &str,
    ) -> Result<(), GatewayError> {
        self.edit_text(message_id, text).await
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

        // Step 1: Upload file via Mattermost API
        let upload_url = format!("{}/api/v4/files", self.config.server_url);
        let part = reqwest::multipart::Part::bytes(file_bytes)
            .file_name(file_name.to_string())
            .mime_str(mime)
            .map_err(|e| GatewayError::SendFailed(format!("MIME error: {e}")))?;
        let form = reqwest::multipart::Form::new()
            .text("channel_id", chat_id.to_string())
            .part("files", part);

        let resp = self
            .client
            .post(&upload_url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .multipart(form)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("Mattermost file upload failed: {e}")))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "Mattermost upload error: {text}"
            )));
        }

        let result: serde_json::Value = resp.json().await.map_err(|e| {
            GatewayError::SendFailed(format!("Mattermost upload parse failed: {e}"))
        })?;
        let file_ids: Vec<String> = result
            .get("file_infos")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|f| f.get("id").and_then(|id| id.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // Step 2: Create a post with the uploaded file IDs
        let post_url = format!("{}/api/v4/posts", self.config.server_url);
        let body = serde_json::json!({
            "channel_id": chat_id,
            "message": caption.unwrap_or(""),
            "file_ids": file_ids
        });

        let resp = self
            .client
            .post(&post_url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("Mattermost file post failed: {e}")))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "Mattermost file post error: {text}"
            )));
        }
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }
    fn platform_name(&self) -> &str {
        "mattermost"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_scheme_mapping() {
        assert_eq!(
            ws_url("https://mm.example.com").unwrap(),
            "wss://mm.example.com/api/v4/websocket"
        );
        assert_eq!(
            ws_url("http://mm.example.com:8065/").unwrap(),
            "ws://mm.example.com:8065/api/v4/websocket"
        );
        assert!(ws_url("mm.example.com").is_err());
    }

    #[test]
    fn to_incoming_routes_on_channel() {
        let msg = IncomingMattermostMessage {
            post_id: "post1".to_string(),
            channel_id: "chan1".to_string(),
            user_id: "user1".to_string(),
            message: "hello mm".to_string(),
            is_bot: false,
        };
        let incoming = MattermostAdapter::to_incoming(&msg);
        assert_eq!(incoming.platform, "mattermost");
        assert_eq!(incoming.chat_id, "chan1");
        assert_eq!(incoming.user_id, "user1");
        assert_eq!(incoming.text, "hello mm");
        assert_eq!(incoming.message_id, Some("post1".to_string()));
    }

    #[test]
    fn parse_ws_event_posted() {
        let post = serde_json::json!({
            "id": "post9",
            "channel_id": "chan9",
            "user_id": "user9",
            "message": "ws hello",
            "props": {}
        })
        .to_string();
        let event = MattermostWsEvent {
            event: "posted".to_string(),
            data: Some(serde_json::json!({ "post": post })),
            broadcast: None,
            seq: Some(3),
        };
        let msg = MattermostAdapter::parse_ws_event(&event).unwrap();
        assert_eq!(msg.post_id, "post9");
        assert_eq!(msg.channel_id, "chan9");
        assert_eq!(msg.message, "ws hello");
        assert!(!msg.is_bot);

        // Non-posted events are ignored.
        let typing = MattermostWsEvent {
            event: "typing".to_string(),
            data: None,
            broadcast: None,
            seq: None,
        };
        assert!(MattermostAdapter::parse_ws_event(&typing).is_none());
    }
}
