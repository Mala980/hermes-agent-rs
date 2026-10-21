//! Generic webhook platform adapter.
//!
//! Receives incoming HTTP webhooks with HMAC-SHA256 signature verification
//! and routes JSON payloads to the gateway. Outbound messages are queued
//! for the next poll from the external service.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, RwLock};
use tracing::{debug, error, info, warn};

use hermes_core::errors::GatewayError;
use hermes_core::traits::{ParseMode, PlatformAdapter};

use crate::adapter::BasePlatformAdapter;
use crate::platforms::helpers::verify_hmac_sha256_signature;

// ---------------------------------------------------------------------------
// Incoming webhook authentication
// ---------------------------------------------------------------------------

/// Replay window for incoming webhooks: |now - timestamp| must be <= 300s.
const REPLAY_WINDOW_SECS: u64 = 300;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    #[serde(default = "default_webhook_port")]
    pub port: u16,
    #[serde(default = "default_webhook_path")]
    pub path: String,
    pub secret: String,
}

fn default_webhook_port() -> u16 {
    9000
}
fn default_webhook_path() -> String {
    "/webhook".to_string()
}

// ---------------------------------------------------------------------------
// Incoming payload
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookPayload {
    pub chat_id: String,
    pub user_id: Option<String>,
    pub text: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Outbound message queue
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct OutboundMessage {
    pub chat_id: String,
    pub text: String,
}

// ---------------------------------------------------------------------------
// WebhookAdapter
// ---------------------------------------------------------------------------

pub struct WebhookAdapter {
    base: BasePlatformAdapter,
    config: WebhookConfig,
    stop_signal: Arc<Notify>,
    shutdown_tx: RwLock<Option<tokio::sync::oneshot::Sender<()>>>,
    outbound_queue: Arc<RwLock<VecDeque<OutboundMessage>>>,
    inbound_tx: Arc<RwLock<Option<tokio::sync::mpsc::Sender<WebhookPayload>>>>,
}

impl WebhookAdapter {
    pub fn new(config: WebhookConfig) -> Self {
        let base = BasePlatformAdapter::new(&config.secret);
        Self {
            base,
            config,
            stop_signal: Arc::new(Notify::new()),
            shutdown_tx: RwLock::new(None),
            outbound_queue: Arc::new(RwLock::new(VecDeque::new())),
            inbound_tx: Arc::new(RwLock::new(None)),
        }
    }

    pub fn config(&self) -> &WebhookConfig {
        &self.config
    }

    /// Set a channel to forward inbound webhook payloads to.
    pub async fn set_inbound_sender(&self, tx: tokio::sync::mpsc::Sender<WebhookPayload>) {
        *self.inbound_tx.write().await = Some(tx);
    }

    /// Drain all queued outbound messages.
    pub async fn drain_outbound(&self) -> Vec<OutboundMessage> {
        let mut queue = self.outbound_queue.write().await;
        queue.drain(..).collect()
    }

    /// Decide whether an incoming webhook POST is authentic, before any body
    /// processing. Fail-closed.
    ///
    /// BREAKING CHANGE (was silently fail-open): the old `verify_signature`
    /// computed a keyed md5 digest that it NEVER compared and returned
    /// `!sig_clean.is_empty()` -- i.e. ANY non-empty `X-Signature` header was
    /// accepted. That function has been REPLACED, not extended, with real
    /// HMAC-SHA256 verification plus a 5-minute replay window:
    ///
    /// 1. HMAC-SHA256 of the raw body with the configured secret, compared in
    ///    constant time ([`verify_hmac_sha256_signature`], not `==`) against
    ///    the `X-Hub-Signature-256` header (`sha256=<hex>` form). The legacy
    ///    `X-Signature` header name is still accepted as an alias. Header names
    ///    are matched case-insensitively.
    /// 2. Replay window: timestamp from the `X-Webhook-Timestamp` header (unix
    ///    seconds), falling back to a numeric/`"numeric"` `timestamp` field in
    ///    the JSON body. Accepted only when `|now - ts| <= 300`s -- rejects
    ///    stale replays and extreme future clock skew alike.
    ///
    /// The legacy plaintext-token behavior (comparing a `token` field in the
    /// body against the secret, or accepting any non-empty signature header)
    /// is REMOVED, not kept as a fallback: a body token does not sign the
    /// payload and cannot be compared in constant time, so keeping it would
    /// defeat this fix. Clients must migrate to HMAC signatures + timestamps.
    ///
    /// - missing/empty secret -> `Err((503, _))` + warn (fail-closed),
    /// - bad signature or missing/stale timestamp -> `Err((403, _))` + warn,
    /// - both valid -> `Ok(())`.
    pub fn verify_incoming_webhook(
        secret: Option<&str>,
        body: &[u8],
        signature: Option<&str>,
        timestamp_header: Option<&str>,
        json_body: &serde_json::Value,
    ) -> Result<(), (u16, &'static str)> {
        let secret = match secret {
            Some(s) if !s.is_empty() => s,
            _ => {
                warn!("incoming webhook secret not configured; rejecting POST (fail-closed, 503)");
                return Err((503, "webhook not configured"));
            }
        };
        if !verify_hmac_sha256_signature(secret, body, signature.unwrap_or("")) {
            warn!("invalid incoming webhook signature; rejecting (403)");
            return Err((403, "invalid webhook signature"));
        }
        let ts: Option<u64> = timestamp_header
            .and_then(|h| h.trim().parse().ok())
            .or_else(|| body_timestamp(json_body));
        match ts {
            Some(t) if timestamp_fresh(t) => Ok(()),
            _ => {
                warn!("incoming webhook timestamp missing or stale; rejecting (403)");
                Err((403, "invalid or stale webhook timestamp"))
            }
        }
    }
}

/// Extract the `timestamp` field from a JSON body (number or numeric string).
fn body_timestamp(v: &serde_json::Value) -> Option<u64> {
    match v.get("timestamp") {
        Some(serde_json::Value::Number(n)) => n.as_u64(),
        Some(serde_json::Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `true` when the unix-seconds timestamp is within the replay window.
fn timestamp_fresh(ts: u64) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now.abs_diff(ts) <= REPLAY_WINDOW_SECS
}

#[async_trait]
impl PlatformAdapter for WebhookAdapter {
    async fn start(&self) -> Result<(), GatewayError> {
        info!(
            "Webhook adapter starting on port {} at path {}",
            self.config.port, self.config.path
        );

        let addr: SocketAddr = format!("0.0.0.0:{}", self.config.port)
            .parse()
            .map_err(|e| GatewayError::ConnectionFailed(format!("Invalid address: {e}")))?;

        let secret = self.config.secret.clone();
        let expected_path = self.config.path.clone();
        let outbound_queue = self.outbound_queue.clone();
        let inbound_tx = self.inbound_tx.clone();

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        *self.shutdown_tx.write().await = Some(shutdown_tx);

        tokio::spawn(async move {
            let listener = match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => l,
                Err(e) => {
                    error!("Webhook server failed to bind: {e}");
                    return;
                }
            };

            info!("Webhook server listening on {addr}");
            let mut shutdown_rx = shutdown_rx;

            loop {
                tokio::select! {
                    accept = listener.accept() => {
                        match accept {
                            Ok((stream, peer)) => {
                                let secret = secret.clone();
                                let expected_path = expected_path.clone();
                                let outbound_queue = outbound_queue.clone();
                                let inbound_tx = inbound_tx.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = handle_webhook_request(
                                        stream, peer, &secret, &expected_path,
                                        outbound_queue, inbound_tx,
                                    ).await {
                                        debug!("Webhook connection error from {peer}: {e}");
                                    }
                                });
                            }
                            Err(e) => warn!("Webhook accept error: {e}"),
                        }
                    }
                    _ = &mut shutdown_rx => {
                        info!("Webhook server shutting down");
                        break;
                    }
                }
            }
        });

        self.base.mark_running();
        Ok(())
    }

    async fn stop(&self) -> Result<(), GatewayError> {
        info!("Webhook adapter stopping");
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
        let mut queue = self.outbound_queue.write().await;
        queue.push_back(OutboundMessage {
            chat_id: chat_id.to_string(),
            text: text.to_string(),
        });
        Ok(())
    }

    async fn edit_message(
        &self,
        chat_id: &str,
        _message_id: &str,
        text: &str,
    ) -> Result<(), GatewayError> {
        self.send_message(chat_id, text, None).await
    }

    async fn send_file(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<(), GatewayError> {
        let text = if let Some(cap) = caption {
            format!("[file:{}] {}", file_path, cap)
        } else {
            format!("[file:{}]", file_path)
        };
        self.send_message(chat_id, &text, None).await
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }
    fn platform_name(&self) -> &str {
        "webhook"
    }
}

// ---------------------------------------------------------------------------
// HTTP request handler
// ---------------------------------------------------------------------------

async fn handle_webhook_request(
    stream: tokio::net::TcpStream,
    _peer: SocketAddr,
    secret: &str,
    expected_path: &str,
    outbound_queue: Arc<RwLock<VecDeque<OutboundMessage>>>,
    inbound_tx: Arc<RwLock<Option<tokio::sync::mpsc::Sender<WebhookPayload>>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf = vec![0u8; 65536];
    let (mut reader, mut writer) = stream.into_split();
    let n = reader.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }

    let request = String::from_utf8_lossy(&buf[..n]);
    let first_line = request.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    let method = parts.first().copied().unwrap_or("GET");
    let path = parts.get(1).copied().unwrap_or("/");

    let signature = header_value(&request, "x-hub-signature-256")
        .or_else(|| header_value(&request, "x-signature"))
        .unwrap_or_default();
    let timestamp_header = header_value(&request, "x-webhook-timestamp");

    if method == "POST" && path == expected_path {
        let body_start = request.find("\r\n\r\n").map(|i| i + 4).unwrap_or(n);
        let body_bytes = &buf[body_start..n];

        let body_str = String::from_utf8_lossy(body_bytes);
        let json_body: serde_json::Value = match serde_json::from_str(&body_str) {
            Ok(v) => v,
            Err(e) => {
                let body = format!("{{\"error\":\"invalid json: {e}\"}}");
                let resp = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(), body
                );
                writer.write_all(resp.as_bytes()).await?;
                return Ok(());
            }
        };

        // Authenticate before any payload processing (fail-closed).
        let secret_opt = if secret.is_empty() {
            None
        } else {
            Some(secret)
        };
        if let Err((status, msg)) = WebhookAdapter::verify_incoming_webhook(
            secret_opt,
            body_bytes,
            (!signature.is_empty()).then_some(signature.as_str()),
            timestamp_header.as_deref(),
            &json_body,
        ) {
            let reason = match status {
                503 => "503 Service Unavailable",
                _ => "403 Forbidden",
            };
            let body = format!("{{\"error\":\"{msg}\"}}");
            let resp = format!(
                "HTTP/1.1 {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            writer.write_all(resp.as_bytes()).await?;
            return Ok(());
        }

        match serde_json::from_value::<WebhookPayload>(json_body) {
            Ok(payload) => {
                if let Some(tx) = inbound_tx.read().await.as_ref() {
                    let _ = tx.send(payload).await;
                }
                let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\r\n{\"status\":\"ok\"}";
                writer.write_all(resp.as_bytes()).await?;
            }
            Err(e) => {
                let body = format!("{{\"error\":\"invalid payload: {e}\"}}");
                let resp = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(), body
                );
                writer.write_all(resp.as_bytes()).await?;
            }
        }
    } else if method == "GET" && path == format!("{}/outbound", expected_path).as_str() {
        let messages = outbound_queue.write().await.drain(..).collect::<Vec<_>>();
        let body = serde_json::to_string(&messages)?;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        writer.write_all(resp.as_bytes()).await?;
    } else {
        let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        writer.write_all(resp.as_bytes()).await?;
    }

    Ok(())
}

/// Case-insensitive HTTP header lookup on the raw request head.
fn header_value(request: &str, name: &str) -> Option<String> {
    let prefix = format!("{name}:");
    request.lines().find_map(|l| {
        if l.len() >= prefix.len() && l[..prefix.len()].eq_ignore_ascii_case(&prefix) {
            Some(l[prefix.len()..].trim().to_string())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

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

    fn now_ts() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn body_with_ts(ts: u64) -> (Vec<u8>, serde_json::Value) {
        let body = format!(r#"{{"chat_id":"c1","text":"hi","timestamp":{ts}}}"#).into_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        (body, json)
    }

    /// RFC 4231 test case 2 (HMAC-SHA-256): key "Jefe",
    /// data "what do ya want for nothing?".
    #[test]
    fn webhook_signature_rfc4231_known_answer() {
        let body = b"what do ya want for nothing?";
        let header = "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";
        let json_v: serde_json::Value = serde_json::from_slice(&body_with_ts(now_ts()).0).unwrap();
        assert!(WebhookAdapter::verify_incoming_webhook(
            Some("Jefe"),
            body,
            Some(header),
            Some(&now_ts().to_string()),
            &json_v,
        )
        .is_ok());
    }

    #[test]
    fn webhook_valid_signature_and_fresh_timestamp_ok() {
        let (body, json) = body_with_ts(now_ts());
        let sig = sign("topsecret", &body);
        assert!(WebhookAdapter::verify_incoming_webhook(
            Some("topsecret"),
            &body,
            Some(&sig),
            None, // timestamp falls back to body field
            &json,
        )
        .is_ok());
    }

    #[test]
    fn webhook_wrong_signature_rejected_403() {
        let (body, json) = body_with_ts(now_ts());
        // Any non-empty garbage used to be accepted (fail-open); now 403.
        assert_eq!(
            WebhookAdapter::verify_incoming_webhook(
                Some("topsecret"),
                &body,
                Some("sha256=0000000000000000000000000000000000000000000000000000000000000000"),
                Some(&now_ts().to_string()),
                &json,
            ),
            Err((403, "invalid webhook signature"))
        );
        // Legacy plaintext token in body does NOT authenticate.
        let legacy = br#"{"chat_id":"c1","text":"hi","token":"topsecret"}"#.to_vec();
        let legacy_json: serde_json::Value = serde_json::from_slice(&legacy).unwrap();
        assert_eq!(
            WebhookAdapter::verify_incoming_webhook(
                Some("topsecret"),
                &legacy,
                Some("sha256=0000000000000000000000000000000000000000000000000000000000000000"),
                Some(&now_ts().to_string()),
                &legacy_json,
            ),
            Err((403, "invalid webhook signature"))
        );
        // Missing signature header entirely.
        assert_eq!(
            WebhookAdapter::verify_incoming_webhook(
                Some("topsecret"),
                &body,
                None,
                Some(&now_ts().to_string()),
                &json,
            ),
            Err((403, "invalid webhook signature"))
        );
    }

    #[test]
    fn webhook_missing_secret_rejected_503_fail_closed() {
        let (body, json) = body_with_ts(now_ts());
        let sig = sign("whatever", &body);
        assert_eq!(
            WebhookAdapter::verify_incoming_webhook(
                None,
                &body,
                Some(&sig),
                Some(&now_ts().to_string()),
                &json,
            ),
            Err((503, "webhook not configured"))
        );
        assert_eq!(
            WebhookAdapter::verify_incoming_webhook(
                Some(""),
                &body,
                Some(&sig),
                Some(&now_ts().to_string()),
                &json,
            ),
            Err((503, "webhook not configured"))
        );
    }

    #[test]
    fn webhook_stale_and_missing_timestamp_rejected_403() {
        let secret = "topsecret";
        // Stale: 10 minutes old.
        let (body, json) = body_with_ts(now_ts() - 600);
        let sig = sign(secret, &body);
        assert_eq!(
            WebhookAdapter::verify_incoming_webhook(Some(secret), &body, Some(&sig), None, &json,),
            Err((403, "invalid or stale webhook timestamp"))
        );
        // Extreme future skew: 10 minutes ahead.
        let (body, json) = body_with_ts(now_ts() + 600);
        let sig = sign(secret, &body);
        assert_eq!(
            WebhookAdapter::verify_incoming_webhook(Some(secret), &body, Some(&sig), None, &json,),
            Err((403, "invalid or stale webhook timestamp"))
        );
        // No timestamp anywhere.
        let body = br#"{"chat_id":"c1","text":"hi"}"#.to_vec();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let sig = sign(secret, &body);
        assert_eq!(
            WebhookAdapter::verify_incoming_webhook(Some(secret), &body, Some(&sig), None, &json,),
            Err((403, "invalid or stale webhook timestamp"))
        );
    }

    #[test]
    fn webhook_timestamp_header_preferred_over_body() {
        let secret = "topsecret";
        // Body timestamp is stale, but a fresh header timestamp saves it.
        let (body, _json) = body_with_ts(now_ts() - 600);
        let sig = sign(secret, &body);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(WebhookAdapter::verify_incoming_webhook(
            Some(secret),
            &body,
            Some(&sig),
            Some(&now_ts().to_string()),
            &json,
        )
        .is_ok());
    }

    #[test]
    fn header_value_is_case_insensitive() {
        let req = "POST /webhook HTTP/1.1\r\nX-Hub-Signature-256: sha256=abc\r\nx-webhook-timestamp: 123\r\n\r\n";
        assert_eq!(
            header_value(req, "x-hub-signature-256").as_deref(),
            Some("sha256=abc")
        );
        assert_eq!(
            header_value(req, "X-Webhook-Timestamp").as_deref(),
            Some("123")
        );
        assert!(header_value(req, "x-missing").is_none());
    }
}
