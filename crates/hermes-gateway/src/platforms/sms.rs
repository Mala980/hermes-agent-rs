//! SMS gateway adapter (Twilio API).
//!
//! Outbound SMS/MMS via the Twilio REST API; inbound via a Twilio webhook
//! server (form-encoded POST, `X-Twilio-Signature` HMAC-SHA1 validation).

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use tokio::sync::{Notify, RwLock};
use tracing::{debug, error, info, warn};

use hermes_core::errors::GatewayError;
use hermes_core::traits::{ParseMode, PlatformAdapter};

use crate::adapter::{AdapterProxyConfig, BasePlatformAdapter};
use crate::gateway::IncomingMessage;
use crate::platforms::helpers::{
    constant_time_eq, parse_http_query, read_http_request, write_http_response,
};

const TWILIO_API_BASE: &str = "https://api.twilio.com/2010-04-01";

/// Default bind host for the inbound webhook server.
const DEFAULT_WEBHOOK_HOST: &str = "0.0.0.0";

/// Default port for the inbound webhook server.
const DEFAULT_WEBHOOK_PORT: u16 = 8090;

/// Default path for the inbound webhook server (mirrors the original).
const DEFAULT_WEBHOOK_PATH: &str = "/webhooks/twilio";

/// Maximum webhook POST body accepted.
const WEBHOOK_MAX_BODY_BYTES: usize = 1024 * 1024;

/// Parsed incoming SMS from a Twilio webhook POST.
#[derive(Debug, Clone)]
pub struct IncomingSmsMessage {
    pub from: String,
    pub to: String,
    pub body: String,
    pub message_sid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmsConfig {
    pub provider: String,
    pub account_sid: String,
    pub auth_token: String,
    pub from_number: String,
    /// Bind host for the inbound webhook server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_host: Option<String>,
    /// Port for the inbound webhook server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_port: Option<u16>,
    /// Path for the inbound webhook server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_path: Option<String>,
    /// Public URL Twilio posts to (used for `X-Twilio-Signature`
    /// validation). When empty, signature validation is skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
    #[serde(default)]
    pub proxy: AdapterProxyConfig,
}

pub struct SmsAdapter {
    base: BasePlatformAdapter,
    config: SmsConfig,
    client: Client,
    stop_signal: Arc<Notify>,
    /// Inbound channel to the gateway, set by the registry via
    /// [`SmsAdapter::set_inbound_sender`].
    inbound_tx: Arc<RwLock<Option<tokio::sync::mpsc::Sender<IncomingMessage>>>>,
    /// Shutdown signal for the webhook server task spawned by `start()`.
    shutdown_tx: RwLock<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl SmsAdapter {
    pub fn new(config: SmsConfig) -> Result<Self, GatewayError> {
        let base = BasePlatformAdapter::new(&config.account_sid).with_proxy(config.proxy.clone());
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

    pub fn config(&self) -> &SmsConfig {
        &self.config
    }

    /// Attach the gateway's inbound channel. Parsed webhook messages are
    /// forwarded on it by the server spawned in [`Self::start`].
    pub async fn set_inbound_sender(&self, tx: tokio::sync::mpsc::Sender<IncomingMessage>) {
        *self.inbound_tx.write().await = Some(tx);
    }

    /// Parse a Twilio form-encoded webhook body into an incoming message.
    ///
    /// Returns `None` when `From`/`Body` are missing, or when the message is
    /// an echo of our own number.
    pub fn parse_webhook_form(
        form: &[(String, String)],
        own_number: &str,
    ) -> Option<IncomingSmsMessage> {
        let get = |k: &str| {
            form.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.trim())
                .unwrap_or("")
        };
        let from = get("From").to_string();
        let to = get("To").to_string();
        let body = get("Body").to_string();
        let message_sid = get("MessageSid").to_string();
        if from.is_empty() || body.is_empty() {
            return None;
        }
        if from == own_number {
            return None;
        }
        Some(IncomingSmsMessage {
            from,
            to,
            body,
            message_sid,
        })
    }

    /// Validate Twilio's `X-Twilio-Signature` header (HMAC-SHA1, base64).
    ///
    /// The signed payload is `url + sorted(key + value)` for the POST
    /// params. Fail-closed: an empty auth token never validates.
    pub fn validate_twilio_signature(
        auth_token: &str,
        url: &str,
        params: &[(String, String)],
        signature: &str,
    ) -> bool {
        if auth_token.is_empty() || signature.is_empty() {
            return false;
        }
        let mut sorted: Vec<(&str, &str)> = params
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        sorted.sort_by(|a, b| a.0.cmp(b.0));
        let mut data = url.to_string();
        for (k, v) in sorted {
            data.push_str(k);
            data.push_str(v);
        }
        let mut mac = match Hmac::<Sha1>::new_from_slice(auth_token.as_bytes()) {
            Ok(m) => m,
            Err(_) => return false,
        };
        mac.update(data.as_bytes());
        let computed =
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        constant_time_eq(&computed, signature)
    }

    /// Convert a parsed message into a gateway-level [`IncomingMessage`].
    /// One session per sender number; every SMS is a DM.
    pub fn to_incoming(msg: &IncomingSmsMessage) -> IncomingMessage {
        IncomingMessage {
            platform: "sms".into(),
            chat_id: msg.from.clone(),
            user_id: msg.from.clone(),
            text: msg.body.clone(),
            message_id: if msg.message_sid.is_empty() {
                None
            } else {
                Some(msg.message_sid.clone())
            },
            is_dm: true,
        }
    }

    /// Send an SMS via Twilio API.
    pub async fn send_sms(&self, to: &str, body: &str) -> Result<(), GatewayError> {
        let url = format!(
            "{}/Accounts/{}/Messages.json",
            TWILIO_API_BASE, self.config.account_sid
        );

        let params = [
            ("To", to),
            ("From", &self.config.from_number),
            ("Body", body),
        ];

        let resp = self
            .client
            .post(&url)
            .basic_auth(&self.config.account_sid, Some(&self.config.auth_token))
            .form(&params)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("Twilio send failed: {}", e)))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "Twilio API error: {}",
                text
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl PlatformAdapter for SmsAdapter {
    async fn start(&self) -> Result<(), GatewayError> {
        info!(
            "SMS adapter starting (provider: {}, from: {})",
            self.config.provider, self.config.from_number
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
        let webhook_url = self.config.webhook_url.clone().unwrap_or_default();
        let auth_token = self.config.auth_token.clone();
        let own_number = self.config.from_number.clone();
        if webhook_url.is_empty() {
            warn!(
                "SMS webhook_url is not set — X-Twilio-Signature validation is disabled. \
                 Set webhook_url to the public URL Twilio posts to."
            );
        }
        let addr: std::net::SocketAddr = format!("{host}:{port}").parse().map_err(|e| {
            GatewayError::ConnectionFailed(format!("SMS webhook invalid address: {e}"))
        })?;
        let inbound_tx = self.inbound_tx.clone();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        *self.shutdown_tx.write().await = Some(shutdown_tx);

        tokio::spawn(async move {
            let listener = match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => l,
                Err(e) => {
                    error!("SMS webhook server failed to bind {addr}: {e}");
                    return;
                }
            };
            info!("SMS webhook listening on {addr}{path}");
            let mut shutdown_rx = shutdown_rx;
            loop {
                tokio::select! {
                    accept = listener.accept() => {
                        match accept {
                            Ok((stream, peer)) => {
                                let webhook_url = webhook_url.clone();
                                let auth_token = auth_token.clone();
                                let own_number = own_number.clone();
                                let path = path.clone();
                                let inbound_tx = inbound_tx.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = handle_sms_webhook_request(
                                        stream,
                                        &path,
                                        &webhook_url,
                                        &auth_token,
                                        &own_number,
                                        inbound_tx,
                                    )
                                    .await
                                    {
                                        debug!("SMS webhook request error from {peer}: {e}");
                                    }
                                });
                            }
                            Err(e) => warn!("SMS webhook accept error: {e}"),
                        }
                    }
                    _ = &mut shutdown_rx => {
                        info!("SMS webhook server shutting down");
                        break;
                    }
                }
            }
        });

        self.base.mark_running();
        Ok(())
    }

    async fn stop(&self) -> Result<(), GatewayError> {
        info!("SMS adapter stopping");
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
        self.send_sms(chat_id, text).await
    }

    async fn edit_message(
        &self,
        _chat_id: &str,
        _message_id: &str,
        _text: &str,
    ) -> Result<(), GatewayError> {
        debug!("SMS does not support message editing");
        Ok(())
    }

    async fn send_file(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<(), GatewayError> {
        // Twilio MMS: send a message with MediaUrl pointing to a publicly accessible URL.
        // If file_path is a local file, we need to host it or base64-encode it.
        // For URLs, we can use them directly via the MediaUrl parameter.
        let url = format!(
            "{}/Accounts/{}/Messages.json",
            TWILIO_API_BASE, self.config.account_sid
        );

        let body_text = caption.unwrap_or("");

        if file_path.starts_with("http://") || file_path.starts_with("https://") {
            // Direct URL - Twilio can fetch it
            let params = [
                ("To", chat_id),
                ("From", &self.config.from_number),
                ("Body", body_text),
                ("MediaUrl", file_path),
            ];

            let resp = self
                .client
                .post(&url)
                .basic_auth(&self.config.account_sid, Some(&self.config.auth_token))
                .form(&params)
                .send()
                .await
                .map_err(|e| GatewayError::SendFailed(format!("Twilio MMS send failed: {e}")))?;

            if !resp.status().is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(GatewayError::SendFailed(format!(
                    "Twilio MMS error: {text}"
                )));
            }
        } else {
            // Local file: send as text with file info since Twilio requires a public URL
            let file_name = std::path::Path::new(file_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file");
            let msg = format!("[Attachment: {}] {}", file_name, body_text);
            self.send_sms(chat_id, &msg).await?;
        }
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }
    fn platform_name(&self) -> &str {
        "sms"
    }
}

// ---------------------------------------------------------------------------
// Inbound Twilio webhook server
// ---------------------------------------------------------------------------

/// Empty TwiML response: acknowledges the webhook without sending a reply.
fn twiml_empty() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?><Response></Response>"#.to_string()
}

/// Handle one inbound Twilio webhook connection.
///
/// - `POST <path>`: Twilio sends form-encoded fields (`From`, `To`,
///   `Body`, `MessageSid`). When `webhook_url` is configured, the
///   `X-Twilio-Signature` header is validated (fail-closed 403). Parsed
///   messages are forwarded to the gateway inbound channel. The response
///   is always TwiML (Twilio retries non-2xx, so even rejections are 2xx
///   TwiML except for malformed bodies).
async fn handle_sms_webhook_request(
    mut stream: tokio::net::TcpStream,
    expected_path: &str,
    webhook_url: &str,
    auth_token: &str,
    own_number: &str,
    inbound_tx: Arc<RwLock<Option<tokio::sync::mpsc::Sender<IncomingMessage>>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (request_line, headers, body) =
        read_http_request(&mut stream, WEBHOOK_MAX_BODY_BYTES).await?;
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    let method = parts.first().copied().unwrap_or("GET");
    let target = parts.get(1).copied().unwrap_or("/");
    let path = target.split('?').next().unwrap_or("/");
    if method != "POST" || path != expected_path {
        write_http_response(&mut stream, 404, "text/plain", "not found").await?;
        return Ok(());
    }

    let raw = String::from_utf8_lossy(&body);
    let form = parse_http_query(&raw);

    // Signature check (only when the public webhook URL is configured).
    if !webhook_url.is_empty() {
        let signature = headers
            .iter()
            .find(|(k, _)| k == "x-twilio-signature")
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        if signature.is_empty()
            || !SmsAdapter::validate_twilio_signature(auth_token, webhook_url, &form, signature)
        {
            warn!("SMS webhook: invalid or missing X-Twilio-Signature");
            write_http_response(&mut stream, 200, "text/xml", &twiml_empty()).await?;
            return Ok(());
        }
    }

    if let Some(msg) = SmsAdapter::parse_webhook_form(&form, own_number) {
        let tx = inbound_tx.read().await.clone();
        if let Some(tx) = tx {
            let incoming = SmsAdapter::to_incoming(&msg);
            if tx.send(incoming).await.is_err() {
                debug!("SMS inbound receiver dropped");
            }
        }
    }
    write_http_response(&mut stream, 200, "text/xml", &twiml_empty()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_twilio_form() {
        let raw = "From=%2B15559876543&To=%2B15551112222&Body=hello+sms&MessageSid=SMabc123";
        let form = parse_http_query(raw);
        let msg = SmsAdapter::parse_webhook_form(&form, "+15551112222").unwrap();
        assert_eq!(msg.from, "+15559876543");
        assert_eq!(msg.to, "+15551112222");
        assert_eq!(msg.body, "hello sms");
        assert_eq!(msg.message_sid, "SMabc123");
        let incoming = SmsAdapter::to_incoming(&msg);
        assert_eq!(incoming.platform, "sms");
        assert_eq!(incoming.chat_id, "+15559876543");
        assert!(incoming.is_dm);
        assert_eq!(incoming.message_id, Some("SMabc123".to_string()));
    }

    #[test]
    fn parse_twilio_form_skips_echo_and_empty() {
        let raw = "From=%2B15551112222&To=%2B15559876543&Body=echo&MessageSid=SM1";
        let form = parse_http_query(raw);
        // Echo of our own number is dropped.
        assert!(SmsAdapter::parse_webhook_form(&form, "+15551112222").is_none());

        let empty = parse_http_query("From=%2B15559876543&Body=");
        assert!(SmsAdapter::parse_webhook_form(&empty, "+15551112222").is_none());
    }

    #[test]
    fn twilio_signature_validation() {
        // Known-answer check: compute the signature the same way Twilio does
        // and verify our validator accepts it.
        let url = "https://example.com/webhooks/twilio";
        let params = vec![
            ("Body".to_string(), "hello".to_string()),
            ("From".to_string(), "+15559876543".to_string()),
        ];
        let mut mac = Hmac::<Sha1>::new_from_slice(b"auth-token").unwrap();
        let mut data = url.to_string();
        data.push_str("Bodyhello");
        data.push_str("From+15559876543");
        mac.update(data.as_bytes());
        let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

        assert!(SmsAdapter::validate_twilio_signature(
            "auth-token",
            url,
            &params,
            &sig
        ));
        // Params must be sorted by key before signing (Body before From).
        let unsorted = vec![
            ("From".to_string(), "+15559876543".to_string()),
            ("Body".to_string(), "hello".to_string()),
        ];
        assert!(SmsAdapter::validate_twilio_signature(
            "auth-token",
            url,
            &unsorted,
            &sig
        ));
        assert!(!SmsAdapter::validate_twilio_signature(
            "auth-token",
            url,
            &params,
            "bogus"
        ));
        // Fail-closed on empty token.
        assert!(!SmsAdapter::validate_twilio_signature(
            "", url, &params, &sig
        ));
    }
}
