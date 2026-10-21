//! WhatsApp Business Cloud API adapter.
//!
//! Implements the `PlatformAdapter` trait for WhatsApp using the Cloud API.
//! Sends messages via `POST /v1/messages` and receives via webhook.

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use hermes_core::errors::GatewayError;
use hermes_core::traits::{ParseMode, PlatformAdapter};

use crate::adapter::{AdapterProxyConfig, BasePlatformAdapter};
use crate::platforms::helpers::verify_hmac_sha256_signature;

const WHATSAPP_API_BASE: &str = "https://graph.facebook.com/v18.0";

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

    /// Proxy configuration for outbound requests.
    #[serde(default)]
    pub proxy: AdapterProxyConfig,
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
        })
    }

    pub fn config(&self) -> &WhatsAppConfig {
        &self.config
    }

    /// Send a text message via WhatsApp Cloud API.
    pub async fn send_text(&self, to: &str, text: &str) -> Result<(), GatewayError> {
        let phone_id = self
            .config
            .phone_number_id
            .as_deref()
            .ok_or_else(|| GatewayError::SendFailed("phone_number_id not configured".into()))?;

        let url = format!("{}/{}/messages", WHATSAPP_API_BASE, phone_id);
        let body = serde_json::json!({
            "messaging_product": "whatsapp",
            "to": to,
            "type": "text",
            "text": { "body": text }
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("WhatsApp send failed: {}", e)))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "WhatsApp API error: {}",
                text
            )));
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

#[async_trait]
impl PlatformAdapter for WhatsAppAdapter {
    async fn start(&self) -> Result<(), GatewayError> {
        info!("WhatsApp adapter starting");
        self.base.mark_running();
        Ok(())
    }

    async fn stop(&self) -> Result<(), GatewayError> {
        info!("WhatsApp adapter stopping");
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
}
