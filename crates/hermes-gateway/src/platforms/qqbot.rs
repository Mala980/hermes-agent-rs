//! QQ Bot (official QQ v2 API) adapter.
//!
//! Outbound via REST (`api.sgroup.qq.com`); inbound via the official
//! gateway WebSocket (token → `GET /gateway` → WS with Hello/Identify/
//! Resume/Heartbeat, dispatching `*_MESSAGE_CREATE` events).

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex, Notify, RwLock};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, info, warn};

use hermes_core::errors::GatewayError;
use hermes_core::traits::{ParseMode, PlatformAdapter};

use crate::adapter::{AdapterProxyConfig, BasePlatformAdapter};
use crate::gateway::IncomingMessage;

const QQ_TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";
const QQ_API_BASE: &str = "https://api.sgroup.qq.com";
const QQ_GATEWAY_URL: &str = "https://api.sgroup.qq.com/gateway";

/// Intents subscribed on Identify: C2C/group-at/public-guild/direct
/// messages + interaction, mirroring the original.
const QQ_INTENTS: u32 = (1 << 25) | (1 << 30) | (1 << 12) | (1 << 26);

/// Base delay between gateway reconnect attempts; doubles per attempt,
/// capped by [`WS_RECONNECT_MAX_DELAY`].
const WS_RECONNECT_BASE_DELAY: Duration = Duration::from_secs(2);
const WS_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);

/// Parsed incoming QQ message from a gateway `*_MESSAGE_CREATE` dispatch.
#[derive(Debug, Clone)]
pub struct IncomingQqMessage {
    pub chat_id: String,
    pub user_id: String,
    pub text: String,
    pub message_id: String,
    pub is_dm: bool,
}

/// Gateway session state for Identify/Resume across reconnects.
#[derive(Debug, Default)]
struct QqGatewaySession {
    session_id: Option<String>,
    sequence: Option<i64>,
}

/// Exponential backoff for gateway reconnect attempts.
fn reconnect_delay(attempt: u32) -> Duration {
    let shift = attempt.min(5);
    let secs = WS_RECONNECT_BASE_DELAY.as_secs() << shift;
    Duration::from_secs(secs.min(WS_RECONNECT_MAX_DELAY.as_secs()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QqBotConfig {
    pub app_id: String,
    pub client_secret: String,
    #[serde(default)]
    pub markdown_support: bool,
    #[serde(default)]
    pub proxy: AdapterProxyConfig,
}

pub struct QqBotAdapter {
    base: BasePlatformAdapter,
    config: QqBotConfig,
    client: Client,
    stop_signal: Arc<Notify>,
    access_token: RwLock<Option<(String, Instant)>>,
    /// Inbound channel to the gateway, set by the registry via
    /// [`QqBotAdapter::set_inbound_sender`].
    inbound_tx: Arc<Mutex<Option<mpsc::Sender<IncomingMessage>>>>,
}

impl QqBotAdapter {
    pub fn new(config: QqBotConfig) -> Result<Self, GatewayError> {
        if config.app_id.trim().is_empty() {
            return Err(GatewayError::Platform(
                "QQBot requires app_id (platforms.qqbot.extra.app_id)".into(),
            ));
        }
        if config.client_secret.trim().is_empty() {
            return Err(GatewayError::Platform(
                "QQBot requires client_secret (platforms.qqbot.extra.client_secret)".into(),
            ));
        }

        let base = BasePlatformAdapter::new(&config.app_id).with_proxy(config.proxy.clone());
        base.validate_token()?;
        let client = base.build_client()?;
        Ok(Self {
            base,
            config,
            client,
            stop_signal: Arc::new(Notify::new()),
            access_token: RwLock::new(None),
            inbound_tx: Arc::new(Mutex::new(None)),
        })
    }

    /// Attach the gateway's inbound channel. Parsed gateway dispatch
    /// events are forwarded on it by [`QqBotAdapter::run_ws_loop`].
    pub async fn set_inbound_sender(&self, tx: mpsc::Sender<IncomingMessage>) {
        *self.inbound_tx.lock().await = Some(tx);
    }

    /// Clone of the stop `Notify`, signaled by [`QqBotAdapter::stop`].
    pub fn stop_notifier(&self) -> Arc<Notify> {
        self.stop_signal.clone()
    }

    // -----------------------------------------------------------------------
    // Gateway WebSocket: inbound
    // -----------------------------------------------------------------------

    /// Fetch the gateway WebSocket URL (`GET /gateway`).
    async fn get_gateway_url(&self) -> Result<String, GatewayError> {
        let token = self.get_access_token().await?;
        let resp = self
            .client
            .get(QQ_GATEWAY_URL)
            .header("Authorization", format!("QQBot {token}"))
            .send()
            .await
            .map_err(|e| {
                GatewayError::ConnectionFailed(format!("QQBot gateway request failed: {e}"))
            })?;
        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::ConnectionFailed(format!(
                "QQBot gateway returned non-success: {text}"
            )));
        }
        let value: serde_json::Value = resp.json().await.map_err(|e| {
            GatewayError::ConnectionFailed(format!("QQBot gateway parse failed: {e}"))
        })?;
        value
            .get("url")
            .and_then(|v| v.as_str())
            .map(String::from)
            .filter(|u| !u.is_empty())
            .ok_or_else(|| GatewayError::ConnectionFailed("QQBot gateway missing url".into()))
    }

    /// Parse a gateway dispatch (`op` 0) event into an incoming message.
    ///
    /// Handles `C2C_MESSAGE_CREATE` (DM by user openid),
    /// `GROUP_AT_MESSAGE_CREATE` (group openid + member openid),
    /// `GUILD_MESSAGE_CREATE` / `GUILD_AT_MESSAGE_CREATE` (channel id +
    /// author id), and `DIRECT_MESSAGE_CREATE` (guild DM). Bot authors are
    /// skipped. Returns `None` for anything else.
    pub fn parse_dispatch(t: &str, d: &serde_json::Value) -> Option<IncomingQqMessage> {
        let str_field = |v: &serde_json::Value, k: &str| {
            v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
        };
        let author = d.get("author").filter(|a| a.is_object());
        let is_bot = author
            .and_then(|a| a.get("bot"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if is_bot {
            return None;
        }
        let content = str_field(d, "content").trim().to_string();
        if content.is_empty() {
            return None;
        }
        let message_id = str_field(d, "id");

        let (chat_id, user_id, is_dm) = match t {
            "C2C_MESSAGE_CREATE" => {
                let user_openid = author
                    .map(|a| str_field(a, "user_openid"))
                    .unwrap_or_default();
                if user_openid.is_empty() {
                    return None;
                }
                (user_openid.clone(), user_openid, true)
            }
            "GROUP_AT_MESSAGE_CREATE" => {
                let group_openid = str_field(d, "group_openid");
                let member = author
                    .map(|a| str_field(a, "member_openid"))
                    .unwrap_or_default();
                if group_openid.is_empty() {
                    return None;
                }
                (group_openid, member, false)
            }
            "GUILD_MESSAGE_CREATE" | "GUILD_AT_MESSAGE_CREATE" => {
                let channel_id = str_field(d, "channel_id");
                let author_id = author.map(|a| str_field(a, "id")).unwrap_or_default();
                if channel_id.is_empty() {
                    return None;
                }
                (channel_id, author_id, false)
            }
            "DIRECT_MESSAGE_CREATE" => {
                let guild_id = str_field(d, "guild_id");
                let author_id = author.map(|a| str_field(a, "id")).unwrap_or_default();
                if guild_id.is_empty() {
                    return None;
                }
                (guild_id, author_id, true)
            }
            _ => return None,
        };

        Some(IncomingQqMessage {
            chat_id,
            user_id,
            text: content,
            message_id,
            is_dm,
        })
    }

    /// Convert a parsed message into a gateway-level [`IncomingMessage`].
    pub fn to_incoming(msg: &IncomingQqMessage) -> IncomingMessage {
        IncomingMessage {
            platform: "qqbot".into(),
            chat_id: msg.chat_id.clone(),
            user_id: msg.user_id.clone(),
            text: msg.text.clone(),
            message_id: if msg.message_id.is_empty() {
                None
            } else {
                Some(msg.message_id.clone())
            },
            is_dm: msg.is_dm,
        }
    }

    /// Run the supervised gateway WebSocket loop, forwarding parsed
    /// dispatch events as [`IncomingMessage`]s on the inbound sender.
    ///
    /// Connects to the URL from [`QqBotAdapter::get_gateway_url`], answers
    /// Hello (op 10) with Identify (op 2) or Resume (op 6), keeps the
    /// heartbeat going (op 1 / 11), and reconnects with exponential
    /// backoff. The loop ends when [`QqBotAdapter::stop`] signals, or when
    /// the inbound receiver is dropped.
    pub async fn run_ws_loop(self: &Arc<Self>) {
        info!("QQBot gateway loop started");
        let mut session = QqGatewaySession::default();
        let mut attempt: u32 = 0;

        while self.base.is_running() {
            let url = match self.get_gateway_url().await {
                Ok(u) => u,
                Err(e) => {
                    warn!("QQBot gateway URL failed: {e}");
                    attempt = attempt.saturating_add(1);
                    let delay = reconnect_delay(attempt);
                    info!("QQBot retrying gateway in {delay:?} (attempt {attempt})");
                    tokio::select! {
                        _ = self.stop_signal.notified() => break,
                        _ = tokio::time::sleep(delay) => {}
                    }
                    continue;
                }
            };
            match connect_async(&url).await {
                Ok((ws_stream, _)) => {
                    attempt = 0;
                    if !self.serve_ws_connection(ws_stream, &mut session).await {
                        // Receiver dropped: end the loop.
                        return;
                    }
                }
                Err(e) => {
                    warn!("QQBot gateway connect failed: {e}");
                }
            }
            if !self.base.is_running() {
                break;
            }
            attempt = attempt.saturating_add(1);
            let delay = reconnect_delay(attempt);
            info!("QQBot gateway reconnecting in {delay:?} (attempt {attempt})");
            tokio::select! {
                _ = self.stop_signal.notified() => break,
                _ = tokio::time::sleep(delay) => {}
            }
        }
        info!("QQBot gateway loop stopped");
    }

    /// Serve one gateway WebSocket connection.
    ///
    /// Returns `true` to reconnect, `false` when the inbound receiver was
    /// dropped and the loop must end.
    async fn serve_ws_connection(
        &self,
        ws_stream: tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        session: &mut QqGatewaySession,
    ) -> bool {
        let (mut sink, mut stream) = ws_stream.split();
        let mut heartbeat_interval: Option<Duration> = None;
        let mut next_heartbeat: Option<tokio::time::Instant> = None;

        loop {
            let heartbeat_sleep = async {
                match next_heartbeat {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => futures::future::pending().await,
                }
            };

            tokio::select! {
                _ = self.stop_signal.notified() => {
                    let _ = sink.close().await;
                    return true;
                }
                _ = heartbeat_sleep => {
                    let hb = match session.sequence {
                        Some(seq) => serde_json::json!({"op": 1, "d": seq}),
                        None => serde_json::json!({"op": 1, "d": serde_json::Value::Null}),
                    };
                    if sink.send(WsMessage::Text(hb.to_string())).await.is_err() {
                        return true;
                    }
                    // Reschedule on the Hello interval.
                    next_heartbeat = heartbeat_interval
                        .map(|i| tokio::time::Instant::now() + i);
                }
                msg = stream.next() => {
                    match msg {
                        Some(Ok(WsMessage::Text(text))) => {
                            let payload: serde_json::Value = match serde_json::from_str(&text) {
                                Ok(p) => p,
                                Err(e) => {
                                    debug!("ignoring malformed QQ gateway payload: {e}");
                                    continue;
                                }
                            };
                            let op = payload.get("op").and_then(|v| v.as_i64()).unwrap_or(-1);
                            match op {
                                10 => {
                                    // Hello: heartbeat interval + Identify/Resume.
                                    if let Some(ms) = payload
                                        .get("d")
                                        .and_then(|d| d.get("heartbeat_interval"))
                                        .and_then(|v| v.as_u64())
                                    {
                                        let interval = Duration::from_millis(ms);
                                        heartbeat_interval = Some(interval);
                                        next_heartbeat =
                                            Some(tokio::time::Instant::now() + interval);
                                    }
                                    let auth = match self.get_access_token().await {
                                        Ok(t) => t,
                                        Err(e) => {
                                            warn!("QQBot identify auth failed: {e}");
                                            return true;
                                        }
                                    };
                                    let identify = if session.session_id.is_some()
                                        && session.sequence.is_some()
                                    {
                                        serde_json::json!({
                                            "op": 6,
                                            "d": {
                                                "token": format!("QQBot {auth}"),
                                                "session_id": session.session_id,
                                                "seq": session.sequence,
                                            }
                                        })
                                    } else {
                                        serde_json::json!({
                                            "op": 2,
                                            "d": {
                                                "token": format!("QQBot {auth}"),
                                                "intents": QQ_INTENTS,
                                                "shard": [0, 1],
                                                "properties": {
                                                    "$os": "linux",
                                                    "$browser": "hermes-agent",
                                                    "$device": "hermes-agent",
                                                },
                                            }
                                        })
                                    };
                                    if sink
                                        .send(WsMessage::Text(identify.to_string()))
                                        .await
                                        .is_err()
                                    {
                                        return true;
                                    }
                                }
                                0 => {
                                    // Dispatch.
                                    if let Some(seq) = payload.get("s").and_then(|v| v.as_i64()) {
                                        session.sequence = Some(seq);
                                    }
                                    let t = payload
                                        .get("t")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    if t == "READY" {
                                        session.session_id = payload
                                            .get("d")
                                            .and_then(|d| d.get("session_id"))
                                            .and_then(|v| v.as_str())
                                            .map(String::from);
                                        debug!("QQBot gateway READY");
                                        continue;
                                    }
                                    let d = payload.get("d").cloned().unwrap_or(serde_json::Value::Null);
                                    if let Some(parsed) = Self::parse_dispatch(t, &d) {
                                        let tx = self.inbound_tx.lock().await.clone();
                                        if let Some(tx) = tx {
                                            if tx.send(Self::to_incoming(&parsed)).await.is_err() {
                                                info!("QQBot loop stopping (inbound receiver dropped)");
                                                return false;
                                            }
                                        }
                                    }
                                }
                                7 => {
                                    debug!("QQBot gateway requested reconnect (op 7)");
                                    let _ = sink.close().await;
                                    return true;
                                }
                                9 => {
                                    // Invalid session: d=true → resume, else clear.
                                    let resumable = payload
                                        .get("d")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false);
                                    if !resumable {
                                        session.session_id = None;
                                        session.sequence = None;
                                    }
                                    let _ = sink.close().await;
                                    return true;
                                }
                                11 => {
                                    // Heartbeat ACK.
                                }
                                _ => {}
                            }
                        }
                        Some(Ok(WsMessage::Ping(p))) => {
                            let _ = sink.send(WsMessage::Pong(p)).await;
                        }
                        Some(Ok(WsMessage::Close(_))) | None => {
                            debug!("QQBot gateway closed by peer");
                            return true;
                        }
                        Some(Err(e)) => {
                            warn!("QQBot gateway WebSocket error: {e}");
                            return true;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    async fn get_access_token(&self) -> Result<String, GatewayError> {
        if let Some((token, expiry)) = self.access_token.read().await.clone() {
            if Instant::now() < expiry {
                return Ok(token);
            }
        }

        let body = serde_json::json!({
            "appId": self.config.app_id,
            "clientSecret": self.config.client_secret
        });
        let resp = self
            .client
            .post(QQ_TOKEN_URL)
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::Auth(format!("QQBot auth request failed: {e}")))?;
        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::Auth(format!(
                "QQBot token endpoint returned non-success: {text}"
            )));
        }
        let value: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| GatewayError::Auth(format!("QQBot auth parse failed: {e}")))?;
        let token = value
            .get("access_token")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                GatewayError::Auth("QQBot token response missing access_token".into())
            })?;
        let expires_in = value
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(7200);
        let expires_at = Instant::now() + Duration::from_secs(expires_in.saturating_sub(60));
        *self.access_token.write().await = Some((token.clone(), expires_at));
        Ok(token)
    }

    fn looks_like_group_chat(chat_id: &str) -> bool {
        let id = chat_id.trim().to_ascii_lowercase();
        id.starts_with("group_") || id.starts_with("grp_") || id.starts_with("qqgroup_")
    }

    async fn send_text(&self, chat_id: &str, text: &str) -> Result<(), GatewayError> {
        let token = self.get_access_token().await?;
        let endpoint = if Self::looks_like_group_chat(chat_id) {
            format!("{QQ_API_BASE}/v2/groups/{chat_id}/messages")
        } else {
            format!("{QQ_API_BASE}/v2/users/{chat_id}/messages")
        };
        let body = if self.config.markdown_support {
            serde_json::json!({
                "msg_type": 2,
                "markdown": { "content": text },
                "msg_seq": (chrono::Utc::now().timestamp_millis() % 65535)
            })
        } else {
            serde_json::json!({
                "msg_type": 0,
                "content": text,
                "msg_seq": (chrono::Utc::now().timestamp_millis() % 65535)
            })
        };
        let resp = self
            .client
            .post(&endpoint)
            .header("Authorization", format!("QQBot {token}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("QQBot send request failed: {e}")))?;
        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!("QQBot API error: {text}")));
        }
        Ok(())
    }
}

#[async_trait]
impl PlatformAdapter for QqBotAdapter {
    async fn start(&self) -> Result<(), GatewayError> {
        info!("QQBot adapter starting (app_id={})", self.config.app_id);
        self.base.mark_running();
        Ok(())
    }

    async fn stop(&self) -> Result<(), GatewayError> {
        info!("QQBot adapter stopping");
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
        debug!("QQBot does not support message editing in this adapter");
        Ok(())
    }

    async fn send_file(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<(), GatewayError> {
        let file_name = std::path::Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");
        let text = if let Some(c) = caption {
            if c.trim().is_empty() {
                format!("[Attachment: {file_name}]")
            } else {
                format!("[Attachment: {file_name}] {c}")
            }
        } else {
            format!("[Attachment: {file_name}]")
        };
        self.send_text(chat_id, &text).await
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }

    fn platform_name(&self) -> &str {
        "qqbot"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dispatch_c2c_message() {
        let d = serde_json::json!({
            "id": "msg-1",
            "content": "hello qq",
            "author": { "user_openid": "openid-u1", "bot": false }
        });
        let msg = QqBotAdapter::parse_dispatch("C2C_MESSAGE_CREATE", &d).unwrap();
        assert_eq!(msg.chat_id, "openid-u1");
        assert_eq!(msg.user_id, "openid-u1");
        assert_eq!(msg.text, "hello qq");
        assert!(msg.is_dm);
        let incoming = QqBotAdapter::to_incoming(&msg);
        assert_eq!(incoming.platform, "qqbot");
        assert_eq!(incoming.message_id, Some("msg-1".to_string()));
    }

    #[test]
    fn parse_dispatch_group_at_message() {
        let d = serde_json::json!({
            "id": "msg-2",
            "content": "group ping",
            "group_openid": "group-openid-1",
            "author": { "member_openid": "member-1", "bot": false }
        });
        let msg = QqBotAdapter::parse_dispatch("GROUP_AT_MESSAGE_CREATE", &d).unwrap();
        assert_eq!(msg.chat_id, "group-openid-1");
        assert_eq!(msg.user_id, "member-1");
        assert!(!msg.is_dm);
    }

    #[test]
    fn parse_dispatch_guild_and_dm() {
        let guild = serde_json::json!({
            "id": "msg-3",
            "content": "guild msg",
            "channel_id": "chan-1",
            "author": { "id": "author-1", "bot": false }
        });
        let msg = QqBotAdapter::parse_dispatch("GUILD_AT_MESSAGE_CREATE", &guild).unwrap();
        assert_eq!(msg.chat_id, "chan-1");
        assert!(!msg.is_dm);

        let dm = serde_json::json!({
            "id": "msg-4",
            "content": "guild dm",
            "guild_id": "guild-1",
            "author": { "id": "author-2", "bot": false }
        });
        let msg = QqBotAdapter::parse_dispatch("DIRECT_MESSAGE_CREATE", &dm).unwrap();
        assert_eq!(msg.chat_id, "guild-1");
        assert!(msg.is_dm);
    }

    #[test]
    fn parse_dispatch_skips_bots_empty_and_unknown() {
        let bot = serde_json::json!({
            "id": "msg-5",
            "content": "bot msg",
            "author": { "user_openid": "u", "bot": true }
        });
        assert!(QqBotAdapter::parse_dispatch("C2C_MESSAGE_CREATE", &bot).is_none());

        let empty = serde_json::json!({
            "id": "msg-6",
            "content": "   ",
            "author": { "user_openid": "u", "bot": false }
        });
        assert!(QqBotAdapter::parse_dispatch("C2C_MESSAGE_CREATE", &empty).is_none());

        let unknown = serde_json::json!({ "id": "x", "content": "y" });
        assert!(QqBotAdapter::parse_dispatch("SOMETHING_ELSE", &unknown).is_none());
    }

    #[test]
    fn qq_reconnect_delay_backoff() {
        assert_eq!(reconnect_delay(0), Duration::from_secs(2));
        assert_eq!(reconnect_delay(3), Duration::from_secs(16));
        assert_eq!(reconnect_delay(100), Duration::from_secs(60));
    }
}
