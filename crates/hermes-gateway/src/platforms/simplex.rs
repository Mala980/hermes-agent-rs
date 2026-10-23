//! SimpleX Chat adapter.
//!
//! SimpleX Chat is a privacy-first messenger with **no user IDs** — contacts
//! are display names (e.g. `@alice`). Bots talk to a locally running
//! SimpleX chat agent over WebSocket (`agent_url`, default
//! `ws://localhost:5225`) using the agent's CLI-command-over-WebSocket
//! protocol.
//!
//! Inbound: a WebSocket client connects to the agent and listens for message
//! events. Each received event has the (simplified) shape
//! `{ "resp": { "type": "received", "contact": "alice", "msg": "hello" } }`;
//! the contact name becomes both `chat_id` and `user_id` (there is no numeric
//! id to use) and the message is forwarded to the gateway via an `mpsc`
//! inbound channel. Non-message frames (command acknowledgements, connection
//! notices, other event types) are ignored. The connection reconnects with
//! backoff when the agent is unreachable or drops the connection.
//!
//! Outbound: sends `{"cmd": "/send @<contact> <text>"}` command frames over
//! the same WebSocket connection. `chat_id` selects the contact; an empty
//! `chat_id` falls back to the configured default `contact`.
//!
//! Limitations of the simplified command protocol implemented here:
//! message editing and file attachments are not supported (both log and are
//! otherwise no-ops), and only the simplified event shape above is parsed —
//! the full SimpleX agent protocol carries richer response types that this
//! adapter intentionally does not model.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex, Notify, RwLock};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

use hermes_config::SimplexConfig;
use hermes_core::errors::GatewayError;
use hermes_core::traits::{ParseMode, PlatformAdapter};

use crate::adapter::{AdapterProxyConfig, BasePlatformAdapter};
use crate::gateway::IncomingMessage;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Max message text chars per `/send` command (char-based; multi-byte text
/// is never split). Longer texts are sent as multiple commands.
pub const MAX_MESSAGE_LENGTH: usize = 4096;
/// Reconnect backoff schedule (seconds); resets after 60 s of healthy uptime.
const RECONNECT_BACKOFF_SECS: [u64; 5] = [2, 5, 10, 30, 60];
/// How long a connection attempt may take before it counts as failed.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = futures::stream::SplitSink<WsStream, WsMessage>;
type WsReadHalf = futures::stream::SplitStream<WsStream>;

// ---------------------------------------------------------------------------
// Agent event parsing
// ---------------------------------------------------------------------------

/// One inbound message parsed from the SimpleX agent event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimplexInbound {
    /// Contact name the message arrived from (leading `@` stripped).
    pub contact: String,
    /// Message text.
    pub msg: String,
    /// Optional agent-assigned message id (for threading/replies).
    pub message_id: Option<String>,
}

/// Parse one agent event frame into an inbound message.
///
/// Accepts the simplified shape
/// `{ "resp": { "type": "received", "contact": "alice", "msg": "hello" } }`
/// and tolerates `contact`/`msg` carried at the top level instead of under
/// `resp`. Anything else — command acknowledgements, connection notices,
/// other event types, malformed JSON, empty contact/message — returns `None`.
pub fn parse_agent_event(raw: &str) -> Option<SimplexInbound> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let resp = value.get("resp").unwrap_or(&value);
    let obj = resp.as_object()?;
    let kind = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if !kind.eq_ignore_ascii_case("received") {
        return None;
    }
    let contact = obj
        .get("contact")
        .and_then(|c| c.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let msg = obj
        .get("msg")
        .and_then(|m| m.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    Some(SimplexInbound {
        contact: contact.trim_start_matches('@').to_string(),
        msg: msg.to_string(),
        message_id: obj
            .get("msg_id")
            .or_else(|| obj.get("message_id"))
            .and_then(|i| i.as_str())
            .map(str::to_string),
    })
}

/// Convert a parsed agent event into a gateway [`IncomingMessage`].
///
/// SimpleX has no user IDs, so the contact name doubles as `chat_id` and
/// `user_id`. Every contact conversation is treated as a DM.
pub fn to_incoming_message(inbound: SimplexInbound) -> IncomingMessage {
    IncomingMessage {
        platform: "simplex".to_string(),
        chat_id: inbound.contact.clone(),
        user_id: inbound.contact,
        text: inbound.msg,
        message_id: inbound.message_id,
        is_dm: true,
        thread_id: None,
    }
}

// ---------------------------------------------------------------------------
// Outbound command construction
// ---------------------------------------------------------------------------

/// Normalize a contact target: trim whitespace and strip a leading `@`.
/// Empty input stays empty so callers can fall back to the configured
/// default contact.
pub fn normalize_contact(target: &str) -> String {
    target.trim().trim_start_matches('@').trim().to_string()
}

/// Build the outbound command frame for a text message.
///
/// Wire format is the agent JSON command
/// `{"cmd": "/send @<contact> <text>"}` (the SimpleX chat agent's
/// CLI-command-over-WebSocket protocol).
pub fn send_command_json(target: &str, text: &str) -> String {
    let contact = normalize_contact(target);
    serde_json::json!({ "cmd": format!("/send @{contact} {text}") }).to_string()
}

/// Backoff delay for reconnect attempt `idx` (0-based), capped at the last
/// schedule entry.
pub fn backoff_delay(idx: usize) -> Duration {
    Duration::from_secs(RECONNECT_BACKOFF_SECS[idx.min(RECONNECT_BACKOFF_SECS.len() - 1)])
}

/// Split text into char-safe chunks of at most `max_len` chars (multi-byte
/// characters are never split mid-codepoint).
pub fn split_text(text: &str, max_len: usize) -> Vec<String> {
    if max_len == 0 {
        return vec![text.to_string()];
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_len {
        return vec![text.to_string()];
    }
    chars
        .chunks(max_len)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

struct SimplexInner {
    config: SimplexConfig,
    /// Gateway inbound channel. `std` lock: writes are uncontended and the
    /// setter is intentionally synchronous (matches the irc/ntfy call shape).
    inbound_tx: std::sync::RwLock<Option<mpsc::Sender<IncomingMessage>>>,
    /// Active agent connection, replaced on every (re)connect. `None` while
    /// disconnected; `send_message` fails closed in that state.
    sink: Mutex<Option<WsSink>>,
    stop: Notify,
    base: BasePlatformAdapter,
}

pub struct SimplexAdapter {
    inner: Arc<SimplexInner>,
    run_task: RwLock<Option<tokio::task::JoinHandle<()>>>,
}

impl SimplexAdapter {
    /// Create the adapter. Fails when `agent_url` is empty or not a
    /// `ws://`/`wss://` URL (fail-closed).
    pub fn new(config: SimplexConfig, proxy: AdapterProxyConfig) -> Result<Self, GatewayError> {
        let url = config.agent_url.trim();
        if url.is_empty() {
            return Err(GatewayError::Platform(
                "simplex agent_url must not be empty".into(),
            ));
        }
        if !(url.starts_with("ws://") || url.starts_with("wss://")) {
            return Err(GatewayError::Platform(format!(
                "simplex agent_url must be a ws:// or wss:// URL, got {url:?}"
            )));
        }
        // The local agent needs no auth token; the base only tracks run state.
        let base = BasePlatformAdapter::new(String::new()).with_proxy(proxy);
        let inner = Arc::new(SimplexInner {
            config,
            inbound_tx: std::sync::RwLock::new(None),
            sink: Mutex::new(None),
            stop: Notify::new(),
            base,
        });
        Ok(Self {
            inner,
            run_task: RwLock::new(None),
        })
    }

    pub fn config(&self) -> &SimplexConfig {
        &self.inner.config
    }

    /// Attach the gateway's inbound channel.
    pub fn set_inbound_sender(&self, tx: mpsc::Sender<IncomingMessage>) {
        if let Ok(mut guard) = self.inner.inbound_tx.write() {
            *guard = Some(tx);
        }
    }

    /// Resolve the outbound target: `chat_id` wins; an empty `chat_id`
    /// falls back to the configured default contact. `None` when neither
    /// is set (send fails closed).
    fn resolve_target(&self, chat_id: &str) -> Option<String> {
        let direct = normalize_contact(chat_id);
        if !direct.is_empty() {
            return Some(direct);
        }
        let fallback = normalize_contact(&self.inner.config.contact);
        if fallback.is_empty() {
            None
        } else {
            Some(fallback)
        }
    }

    /// One connection attempt: connect, then read frames until the agent
    /// closes the connection, a read fails, or the adapter stops.
    async fn consume_connection(inner: &SimplexInner) -> ConnOutcome {
        let url = inner.config.agent_url.clone();
        debug!("simplex: connecting to {url}");
        let ws = match tokio::time::timeout(CONNECT_TIMEOUT, connect_async(&url)).await {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(e)) => {
                warn!("simplex: websocket connect failed: {e}");
                return ConnOutcome::Retry;
            }
            Err(_) => {
                warn!("simplex: websocket connect to {url} timed out after {CONNECT_TIMEOUT:?}");
                return ConnOutcome::Retry;
            }
        };
        info!("simplex: connected to SimpleX agent at {url}");
        let (sink, mut read_half) = ws.split();
        *inner.sink.lock().await = Some(sink);
        let outcome = Self::read_loop(inner, &mut read_half).await;
        *inner.sink.lock().await = None;
        outcome
    }

    /// Read frames until close/error/stop.
    async fn read_loop(inner: &SimplexInner, read_half: &mut WsReadHalf) -> ConnOutcome {
        loop {
            tokio::select! {
                _ = inner.stop.notified() => return ConnOutcome::Stopped,
                frame = read_half.next() => match frame {
                    Some(Ok(WsMessage::Text(text))) => Self::handle_text(inner, &text).await,
                    Some(Ok(WsMessage::Ping(payload))) => {
                        // Reply to keep-alive pings via the shared sink.
                        let mut sink = inner.sink.lock().await;
                        let alive = match sink.as_mut() {
                            Some(s) => s.send(WsMessage::Pong(payload)).await.is_ok(),
                            None => false,
                        };
                        if !alive {
                            return ConnOutcome::Retry;
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => {
                        debug!("simplex: agent closed the connection");
                        return ConnOutcome::Retry;
                    }
                    Some(Ok(_)) => {} // binary/pong frames are ignored
                    Some(Err(e)) => {
                        warn!("simplex: websocket read error: {e}");
                        return ConnOutcome::Retry;
                    }
                },
            }
        }
    }

    /// Parse one text frame and forward inbound messages to the gateway.
    async fn handle_text(inner: &SimplexInner, raw: &str) {
        let Some(inbound) = parse_agent_event(raw) else {
            debug!("simplex: ignoring non-message agent frame");
            return;
        };
        debug!("simplex: inbound message from {}", inbound.contact);
        let incoming = to_incoming_message(inbound);
        let tx = inner
            .inbound_tx
            .read()
            .map(|guard| guard.clone())
            .unwrap_or(None);
        if let Some(tx) = tx {
            tokio::spawn(async move {
                let _ = tx.send(incoming).await;
            });
        }
    }

    /// Connection loop with reconnect backoff; the schedule resets after
    /// 60 s of healthy uptime.
    async fn connect_loop(inner: Arc<SimplexInner>) {
        let mut backoff_idx = 0usize;
        while inner.base.is_running() {
            let started = Instant::now();
            match Self::consume_connection(&inner).await {
                ConnOutcome::Stopped => return,
                ConnOutcome::Retry => {}
            }
            if !inner.base.is_running() {
                return;
            }
            if started.elapsed() >= Duration::from_secs(60) {
                backoff_idx = 0;
            }
            let delay = backoff_delay(backoff_idx);
            info!(
                "simplex: connection ended, reconnecting in {}s",
                delay.as_secs()
            );
            tokio::select! {
                _ = inner.stop.notified() => return,
                _ = tokio::time::sleep(delay) => {}
            }
            backoff_idx += 1;
        }
    }

    /// Send one `/send` command frame over the active connection.
    async fn send_command(&self, target: &str, text: &str) -> Result<(), GatewayError> {
        let frame = send_command_json(target, text);
        let mut sink = self.inner.sink.lock().await;
        let sink = sink.as_mut().ok_or_else(|| {
            GatewayError::SendFailed("simplex: not connected to the SimpleX agent".into())
        })?;
        sink.send(WsMessage::Text(frame))
            .await
            .map_err(|e| GatewayError::SendFailed(format!("simplex: send failed: {e}")))
    }
}

/// Internal outcome of one connection attempt.
enum ConnOutcome {
    /// Stop requested — do not reconnect.
    Stopped,
    /// Transient failure — reconnect with backoff.
    Retry,
}

#[async_trait]
impl PlatformAdapter for SimplexAdapter {
    async fn start(&self) -> Result<(), GatewayError> {
        info!(
            "simplex adapter starting (agent: {})",
            self.inner.config.agent_url
        );
        if self.run_task.read().await.is_some() {
            debug!("simplex: connect loop already running");
            return Ok(());
        }
        self.inner.base.mark_running();
        let inner = self.inner.clone();
        let handle = tokio::spawn(async move { Self::connect_loop(inner).await });
        *self.run_task.write().await = Some(handle);
        Ok(())
    }

    async fn stop(&self) -> Result<(), GatewayError> {
        info!("simplex adapter stopping");
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
        let target = self.resolve_target(chat_id).ok_or_else(|| {
            GatewayError::SendFailed(
                "simplex: no contact target (empty chat_id and no default contact configured)"
                    .into(),
            )
        })?;
        for chunk in split_text(text, MAX_MESSAGE_LENGTH) {
            self.send_command(&target, &chunk).await?;
        }
        Ok(())
    }

    async fn edit_message(
        &self,
        _chat_id: &str,
        _message_id: &str,
        _text: &str,
    ) -> Result<(), GatewayError> {
        debug!("simplex: message editing is not supported by the simplified command protocol");
        Ok(())
    }

    fn max_edit_chars(&self) -> usize {
        // Matches the per-command text cap; the streaming manager should
        // split longer responses instead of editing.
        MAX_MESSAGE_LENGTH
    }

    async fn send_file(
        &self,
        _chat_id: &str,
        _file_path: &str,
        _caption: Option<&str>,
    ) -> Result<(), GatewayError> {
        debug!("simplex: file attachments are not supported by the simplified command protocol");
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.inner.base.is_running()
    }

    fn platform_name(&self) -> &str {
        "simplex"
    }
}

// ---------------------------------------------------------------------------
// Tests (no live network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_config::PlatformConfig;

    fn test_config() -> SimplexConfig {
        SimplexConfig {
            agent_url: "ws://localhost:5225".to_string(),
            contact: "@alice".to_string(),
            enabled: true,
        }
    }

    fn received_event() -> &'static str {
        r#"{"resp":{"type":"received","contact":"alice","msg":"hello"}}"#
    }

    #[test]
    fn config_defaults() {
        let cfg = SimplexConfig::default();
        assert_eq!(cfg.agent_url, "ws://localhost:5225");
        assert!(cfg.contact.is_empty());
        assert!(!cfg.enabled);
    }

    #[test]
    fn config_from_platform_config() {
        let mut pc = PlatformConfig::default();
        pc.extra.insert(
            "agent_url".to_string(),
            serde_json::json!("ws://agent:5226/"),
        );
        pc.extra
            .insert("contact".to_string(), serde_json::json!("@bob"));
        pc.extra
            .insert("enabled".to_string(), serde_json::json!(true));
        let cfg = SimplexConfig::from_platform_config(&pc);
        assert_eq!(cfg.agent_url, "ws://agent:5226");
        assert_eq!(cfg.contact, "@bob");
        assert!(cfg.enabled);
    }

    #[test]
    fn parse_received_event() {
        let inbound = parse_agent_event(received_event()).expect("parses");
        assert_eq!(
            inbound,
            SimplexInbound {
                contact: "alice".to_string(),
                msg: "hello".to_string(),
                message_id: None,
            }
        );
    }

    #[test]
    fn parse_event_accepts_top_level_shape_and_msg_id() {
        let inbound = parse_agent_event(
            r#"{"type":"received","contact":"@bob","msg":"hi there","msg_id":"m-42"}"#,
        )
        .expect("parses");
        assert_eq!(inbound.contact, "bob");
        assert_eq!(inbound.msg, "hi there");
        assert_eq!(inbound.message_id.as_deref(), Some("m-42"));
    }

    #[test]
    fn parse_event_rejects_non_message_frames() {
        // command acknowledgement
        assert!(parse_agent_event(r#"{"resp":{"type":"ack","cmd":"/send"}}"#).is_none());
        // connection notice
        assert!(parse_agent_event(r#"{"resp":{"type":"connected"}}"#).is_none());
        // malformed json
        assert!(parse_agent_event("not json").is_none());
        // empty message body
        assert!(
            parse_agent_event(r#"{"resp":{"type":"received","contact":"alice","msg":"  "}}"#)
                .is_none()
        );
        // missing contact
        assert!(parse_agent_event(r#"{"resp":{"type":"received","msg":"hi"}}"#).is_none());
        // missing type
        assert!(parse_agent_event(r#"{"resp":{"contact":"alice","msg":"hi"}}"#).is_none());
    }

    #[test]
    fn normalize_contact_cases() {
        assert_eq!(normalize_contact("@alice"), "alice");
        assert_eq!(normalize_contact("  @bob  "), "bob");
        assert_eq!(normalize_contact("carol"), "carol");
        assert_eq!(normalize_contact(""), "");
        assert_eq!(normalize_contact("   "), "");
    }

    #[test]
    fn send_command_json_shape() {
        let frame = send_command_json("alice", "hello");
        let value: serde_json::Value = serde_json::from_str(&frame).expect("valid json");
        assert_eq!(
            value.get("cmd").and_then(|c| c.as_str()),
            Some("/send @alice hello")
        );
        // leading @ on the target is normalized, not doubled
        let frame = send_command_json("@bob", "hi");
        let value: serde_json::Value = serde_json::from_str(&frame).expect("valid json");
        assert_eq!(
            value.get("cmd").and_then(|c| c.as_str()),
            Some("/send @bob hi")
        );
    }

    #[test]
    fn backoff_delay_schedule() {
        let expected = [2, 5, 10, 30, 60, 60, 60];
        for (idx, secs) in expected.iter().enumerate() {
            assert_eq!(backoff_delay(idx), Duration::from_secs(*secs), "idx {idx}");
        }
    }

    #[test]
    fn split_text_is_char_safe() {
        assert_eq!(split_text("hello", 10), vec!["hello".to_string()]);
        let long: String = "x".repeat(MAX_MESSAGE_LENGTH + 10);
        let chunks = split_text(&long, MAX_MESSAGE_LENGTH);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), MAX_MESSAGE_LENGTH);
        // multi-byte chars are never split mid-codepoint
        let emoji: String = "🚀".repeat(MAX_MESSAGE_LENGTH + 5);
        let chunks = split_text(&emoji, MAX_MESSAGE_LENGTH);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), MAX_MESSAGE_LENGTH);
        assert!(chunks[1].is_char_boundary(chunks[1].len()));
    }

    #[test]
    fn new_rejects_invalid_agent_url() {
        let mut cfg = test_config();
        cfg.agent_url = "   ".to_string();
        let err = SimplexAdapter::new(cfg, AdapterProxyConfig::default())
            .err()
            .expect("empty url fails");
        assert!(matches!(err, GatewayError::Platform(_)));

        let mut cfg = test_config();
        cfg.agent_url = "http://localhost:5225".to_string();
        let err = SimplexAdapter::new(cfg, AdapterProxyConfig::default())
            .err()
            .expect("http url fails");
        assert!(matches!(err, GatewayError::Platform(_)));

        let adapter =
            SimplexAdapter::new(test_config(), AdapterProxyConfig::default()).expect("valid url");
        assert_eq!(adapter.platform_name(), "simplex");
        assert!(!adapter.is_running());
    }

    #[test]
    fn resolve_target_prefers_chat_id_then_default_contact() {
        let adapter =
            SimplexAdapter::new(test_config(), AdapterProxyConfig::default()).expect("adapter");
        assert_eq!(adapter.resolve_target("bob").as_deref(), Some("bob"));
        assert_eq!(adapter.resolve_target("@carol").as_deref(), Some("carol"));
        // empty chat_id falls back to the configured default contact
        assert_eq!(adapter.resolve_target("").as_deref(), Some("alice"));
        assert_eq!(adapter.resolve_target("  ").as_deref(), Some("alice"));

        let mut cfg = test_config();
        cfg.contact = String::new();
        let adapter = SimplexAdapter::new(cfg, AdapterProxyConfig::default()).expect("adapter");
        assert_eq!(adapter.resolve_target("bob").as_deref(), Some("bob"));
        assert!(adapter.resolve_target("").is_none());
    }

    #[test]
    fn incoming_message_fields() {
        let inbound = parse_agent_event(received_event()).expect("parses");
        let msg = to_incoming_message(inbound);
        assert_eq!(msg.platform, "simplex");
        assert_eq!(msg.chat_id, "alice");
        assert_eq!(msg.user_id, "alice");
        assert_eq!(msg.text, "hello");
        assert!(msg.message_id.is_none());
        assert!(msg.is_dm);
        assert!(msg.thread_id.is_none());
    }

    #[tokio::test]
    async fn handle_text_forwards_inbound_message() {
        let adapter =
            SimplexAdapter::new(test_config(), AdapterProxyConfig::default()).expect("adapter");
        let (tx, mut rx) = mpsc::channel(8);
        adapter.set_inbound_sender(tx);

        SimplexAdapter::handle_text(&adapter.inner, received_event()).await;
        let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("receive timeout")
            .expect("message");
        assert_eq!(msg.platform, "simplex");
        assert_eq!(msg.chat_id, "alice");
        assert_eq!(msg.text, "hello");

        // non-message frames produce nothing
        SimplexAdapter::handle_text(&adapter.inner, r#"{"resp":{"type":"ack"}}"#).await;
        let none = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(none.is_err(), "no message expected for ack frames");
    }
}
