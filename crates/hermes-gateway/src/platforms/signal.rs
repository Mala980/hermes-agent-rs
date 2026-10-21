//! Signal messaging adapter via signal-cli REST API.

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use hermes_core::errors::GatewayError;
use hermes_core::traits::{ParseMode, PlatformAdapter};

use crate::adapter::{AdapterProxyConfig, BasePlatformAdapter};
use crate::markdown_split::split_markdown;
use crate::platforms::helpers::send_with_rate_limit_retry;

/// Maximum message length for Signal sends (8000 chars); longer texts are
/// split into sequential messages, never truncated.
const MAX_MESSAGE_LENGTH: usize = 8000;

// ---------------------------------------------------------------------------
// Incoming message types
// ---------------------------------------------------------------------------

/// Parsed incoming Signal message from the signal-cli receive endpoint.
#[derive(Debug, Clone)]
pub struct IncomingSignalMessage {
    pub source: String,
    pub timestamp: u64,
    pub text: String,
    pub group_id: Option<String>,
    pub attachments: Vec<String>,
}

// ---------------------------------------------------------------------------
// SignalConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalConfig {
    /// Signal number (e.g., "+1234567890").
    pub phone_number: String,
    /// Signal CLI REST API URL.
    #[serde(default = "default_signal_api_url")]
    pub api_url: String,
    #[serde(default)]
    pub proxy: AdapterProxyConfig,
}

fn default_signal_api_url() -> String {
    "http://localhost:8080".to_string()
}

// ---------------------------------------------------------------------------
// SignalAdapter
// ---------------------------------------------------------------------------

pub struct SignalAdapter {
    base: BasePlatformAdapter,
    config: SignalConfig,
    client: Client,
    stop_signal: Arc<Notify>,
}

impl SignalAdapter {
    pub fn new(config: SignalConfig) -> Result<Self, GatewayError> {
        let base = BasePlatformAdapter::new(&config.phone_number).with_proxy(config.proxy.clone());
        base.validate_token()?;
        let client = base.build_client()?;
        Ok(Self {
            base,
            config,
            client,
            stop_signal: Arc::new(Notify::new()),
        })
    }

    pub fn config(&self) -> &SignalConfig {
        &self.config
    }

    /// Send a message via signal-cli REST API.
    ///
    /// Texts longer than [`MAX_MESSAGE_LENGTH`] are split fence-aware into
    /// sequential messages.
    pub async fn send_text(&self, recipient: &str, text: &str) -> Result<(), GatewayError> {
        let url = format!("{}/v2/send", self.config.api_url);

        for chunk in split_markdown(text, MAX_MESSAGE_LENGTH) {
            let body = serde_json::json!({
                "message": chunk,
                "number": self.config.phone_number,
                "recipients": [recipient]
            });

            let send_request = || self.client.post(&url).json(&body).send();
            // Retries once after an HTTP 429.
            let resp = send_with_rate_limit_retry(send_request)
                .await
                .map_err(|e| GatewayError::SendFailed(format!("Signal send failed: {}", e)))?;

            if !resp.status().is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(GatewayError::SendFailed(format!(
                    "Signal API error: {}",
                    text
                )));
            }
        }
        Ok(())
    }

    /// Parse a single message from signal-cli's receive endpoint into a typed struct.
    ///
    /// Expects the signal-cli JSON envelope format with `envelope.dataMessage`.
    pub fn parse_received_message(msg: &serde_json::Value) -> Option<IncomingSignalMessage> {
        let envelope = msg.get("envelope")?;
        let source = envelope.get("source").and_then(|v| v.as_str())?.to_string();
        let timestamp = envelope
            .get("timestamp")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let data_message = envelope.get("dataMessage")?;
        let text = data_message
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let group_id = data_message
            .get("groupInfo")
            .and_then(|g| g.get("groupId"))
            .and_then(|v| v.as_str())
            .map(String::from);

        let attachments = data_message
            .get("attachments")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| a.get("id").and_then(|v| v.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        Some(IncomingSignalMessage {
            source,
            timestamp,
            text,
            group_id,
            attachments,
        })
    }

    /// Gate a parsed inbound message before dispatch to the agent loop.
    ///
    /// DMs (no `group_id`) always pass. Group messages pass only when their
    /// `groupId` is listed in `SIGNAL_GROUP_ALLOWED_USERS` (fail-closed for
    /// groups; see [`crate::allowlist::is_signal_group_allowed`]). Denied
    /// group messages return `false` and are logged so a misconfigured
    /// allowlist is visible.
    ///
    /// Call this on every value returned by [`Self::parse_received_message`]
    /// before routing it into the gateway.
    pub fn is_incoming_allowed(msg: &IncomingSignalMessage) -> bool {
        if crate::allowlist::is_signal_group_allowed(msg.group_id.as_deref()) {
            true
        } else {
            warn!(
                group_id = msg.group_id.as_deref().unwrap_or("<unknown>"),
                source = %msg.source,
                "Skipping Signal group message: group not on SIGNAL_GROUP_ALLOWED_USERS (fail-closed)"
            );
            false
        }
    }

    /// Receive messages via signal-cli REST API polling.
    pub async fn receive_messages(&self) -> Result<Vec<serde_json::Value>, GatewayError> {
        let url = format!(
            "{}/v1/receive/{}",
            self.config.api_url, self.config.phone_number
        );
        let resp =
            self.client.get(&url).send().await.map_err(|e| {
                GatewayError::ConnectionFailed(format!("Signal receive failed: {}", e))
            })?;

        let messages: Vec<serde_json::Value> = resp
            .json()
            .await
            .map_err(|e| GatewayError::ConnectionFailed(format!("Signal parse failed: {}", e)))?;
        Ok(messages)
    }
}

#[async_trait]
impl PlatformAdapter for SignalAdapter {
    async fn start(&self) -> Result<(), GatewayError> {
        info!(
            "Signal adapter starting (number: {})",
            self.config.phone_number
        );
        self.base.mark_running();
        Ok(())
    }

    async fn stop(&self) -> Result<(), GatewayError> {
        info!("Signal adapter stopping");
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
        debug!("Signal does not support message editing");
        Ok(())
    }

    async fn send_file(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<(), GatewayError> {
        let file_bytes = tokio::fs::read(file_path)
            .await
            .map_err(|e| GatewayError::SendFailed(format!("Failed to read file: {e}")))?;

        let b64 = base64_encode(&file_bytes);

        let url = format!("{}/v2/send", self.config.api_url);
        let body = serde_json::json!({
            "message": caption.unwrap_or(""),
            "number": self.config.phone_number,
            "recipients": [chat_id],
            "base64_attachments": [b64]
        });

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("Signal attachment send failed: {e}")))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "Signal attachment error: {text}"
            )));
        }
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }
    fn platform_name(&self) -> &str {
        "signal"
    }
}

/// Simple base64 encoding using the `base64` crate convention (standard alphabet).
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        result.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowlist::ENV_LOCK;

    #[test]
    fn split_markdown_signal_limit() {
        // Signal splits (never truncates) at 8000 chars, fence-aware.
        let code: String = (0..600)
            .map(|i| format!("let v_{i} = f({i});"))
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!("Intro\n```rust\n{code}\n```\nOutro");
        let chunks = split_markdown(&text, MAX_MESSAGE_LENGTH);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.len() <= 8100, "chunk exceeds Signal limit: {}", c.len());
            assert_eq!(c.matches("```").count() % 2, 0);
        }
    }

    fn signal_msg(group_id: Option<&str>) -> IncomingSignalMessage {
        IncomingSignalMessage {
            source: "+15551234567".to_string(),
            timestamp: 1760000000,
            text: "hi".to_string(),
            group_id: group_id.map(str::to_string),
            attachments: vec![],
        }
    }

    const VARS: &[&str] = &[
        "SIGNAL_GROUP_ALLOWED_USERS",
        "SIGNAL_GROUP_ALLOW_ALL_USERS",
        "GATEWAY_ALLOWED_USERS",
        "GATEWAY_ALLOW_ALL_USERS",
    ];

    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn clear() -> Self {
            let saved = VARS
                .iter()
                .map(|&n| {
                    let old = std::env::var(n).ok();
                    std::env::remove_var(n);
                    (n, old)
                })
                .collect();
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, old) in self.saved.drain(..) {
                match old {
                    Some(s) => std::env::set_var(name, s),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    #[test]
    fn group_message_denied_when_allowlist_unset_fail_closed() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::clear();

        assert!(crate::allowlist::signal_group_allowlist().is_none());
        assert!(!crate::allowlist::is_signal_group_allowed(Some("group-1")));
        assert!(!SignalAdapter::is_incoming_allowed(&signal_msg(Some(
            "group-1"
        ))));
    }

    #[test]
    fn group_message_allowed_when_group_id_listed() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::clear();

        std::env::set_var("SIGNAL_GROUP_ALLOWED_USERS", " group-1 ,group-2 ");
        assert!(SignalAdapter::is_incoming_allowed(&signal_msg(Some(
            "group-1"
        ))));
        assert!(SignalAdapter::is_incoming_allowed(&signal_msg(Some(
            "group-2"
        ))));
        assert!(!SignalAdapter::is_incoming_allowed(&signal_msg(Some(
            "group-3"
        ))));
        // GATEWAY_ALLOWED_USERS unions in as well.
        std::env::set_var("GATEWAY_ALLOWED_USERS", "group-9");
        assert!(SignalAdapter::is_incoming_allowed(&signal_msg(Some(
            "group-9"
        ))));
    }

    #[test]
    fn dm_passthrough_even_when_allowlist_unset() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::clear();

        // DMs are never gated: no groupId → always allowed, even fail-closed.
        assert!(SignalAdapter::is_incoming_allowed(&signal_msg(None)));
        assert!(crate::allowlist::is_signal_group_allowed(None));
    }

    #[test]
    fn wildcard_and_allow_all_switch_admit_every_group() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::clear();

        std::env::set_var("SIGNAL_GROUP_ALLOWED_USERS", "*");
        assert!(SignalAdapter::is_incoming_allowed(&signal_msg(Some(
            "any-group"
        ))));

        std::env::set_var("SIGNAL_GROUP_ALLOWED_USERS", "");
        std::env::set_var("SIGNAL_GROUP_ALLOW_ALL_USERS", "yes");
        assert!(crate::allowlist::signal_group_allow_all());
        assert!(SignalAdapter::is_incoming_allowed(&signal_msg(Some(
            "any-group"
        ))));
    }
}
