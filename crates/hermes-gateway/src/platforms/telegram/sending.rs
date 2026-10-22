//! Telegram adapter — outbound messaging.

use serde_json;
use tracing::{debug, warn};

use hermes_core::errors::GatewayError;
use hermes_core::traits::ParseMode;

use super::error::*;
use super::types::*;
use super::TelegramAdapter;
use crate::markdown_split::split_markdown;

/// Maximum message length for Telegram (4096 characters).
pub(crate) const MAX_MESSAGE_LENGTH: usize = 4096;

/// Maximum number of retries for rate-limited requests.
pub(crate) const RATE_LIMIT_MAX_RETRIES: u32 = 3;

/// Cap (seconds) on the inline sleep for Telegram 429 responses. A large
/// `retry_after` must not pin the loop for hours — above this cap we return
/// a retryable error and let the caller's own backoff apply.
pub(crate) const RATE_LIMIT_INLINE_SLEEP_CAP_SECS: u64 = 5;

/// Truncate a media caption to Telegram's 1024-character limit at a char
/// boundary (M-S2). Captions longer than the limit make the whole
/// `sendPhoto`/`sendVideo`/… request fail with HTTP 400, so the send path
/// caps them instead. Logs at debug when truncation happens.
fn truncate_caption(caption: &str) -> String {
    if caption.chars().count() <= MAX_CAPTION_LENGTH {
        return caption.to_string();
    }
    debug!(
        original_chars = caption.chars().count(),
        "Telegram: caption exceeds {} chars, truncating", MAX_CAPTION_LENGTH
    );
    caption.chars().take(MAX_CAPTION_LENGTH).collect()
}

impl TelegramAdapter {
    /// Register the bot's / command menu via `setMyCommands`.
    ///
    /// Mirrors the original's `_register_command_menu`: exposes the gateway's
    /// slash commands in Telegram's UI. Dynamic — built from
    /// [`crate::commands::telegram_menu_commands`] (builtins + extensions).
    /// Best-effort — failures are returned so the caller can log them, but
    /// must not fail startup.
    pub async fn register_command_menu(&self) -> Result<(), GatewayError> {
        let menu = crate::commands::telegram_menu_commands();

        let commands: Vec<serde_json::Value> = menu
            .iter()
            .map(|(cmd, desc)| {
                let desc = desc.chars().take(256).collect::<String>();
                serde_json::json!({ "command": cmd, "description": desc })
            })
            .collect();

        let url = format!("{}/setMyCommands", self.api_base);
        let body = serde_json::json!({ "commands": commands });
        let resp: TelegramResponse<serde_json::Value> = self.post_json(&url, &body).await?;

        if resp.ok {
            tracing::info!("Telegram setMyCommands OK ({} commands)", menu.len());
        } else {
            tracing::warn!(
                "Telegram setMyCommands returned ok=false: {:?}",
                resp.description
            );
        }
        Ok(())
    }

    /// Send a text message, splitting into multiple messages if it exceeds
    /// the 4096 character limit.
    pub async fn send_text(
        &self,
        chat_id: &str,
        text: &str,
        parse_mode: Option<&str>,
        reply_to_message_id: Option<i64>,
    ) -> Result<Vec<i64>, GatewayError> {
        self.send_text_inner(chat_id, text, parse_mode, reply_to_message_id, None, None)
            .await
    }
    /// Send a text message with an inline keyboard attached.
    pub async fn send_text_with_keyboard(
        &self,
        chat_id: &str,
        text: &str,
        keyboard: InlineKeyboardMarkup,
        parse_mode: Option<&str>,
        reply_to_message_id: Option<i64>,
        message_thread_id: Option<i64>,
    ) -> Result<Vec<i64>, GatewayError> {
        self.send_text_inner(
            chat_id,
            text,
            parse_mode,
            reply_to_message_id,
            Some(keyboard),
            message_thread_id,
        )
        .await
    }
    pub(crate) async fn send_text_inner(
        &self,
        chat_id: &str,
        text: &str,
        parse_mode: Option<&str>,
        reply_to_message_id: Option<i64>,
        keyboard: Option<InlineKeyboardMarkup>,
        message_thread_id: Option<i64>,
    ) -> Result<Vec<i64>, GatewayError> {
        let chunks = split_markdown(text, MAX_MESSAGE_LENGTH);
        let mut message_ids = Vec::new();

        for (i, chunk) in chunks.iter().enumerate() {
            let mut body = serde_json::json!({
                "chat_id": chat_id,
                "text": chunk,
            });

            if let Some(pm) = parse_mode {
                body["parse_mode"] = serde_json::Value::String(pm.to_string());
            }

            if let Some(thread_id) = message_thread_id {
                body["message_thread_id"] = serde_json::Value::Number(thread_id.into());
            }

            // Only reply to the original message for the first chunk.
            if i == 0 {
                if let Some(reply_id) = reply_to_message_id {
                    body["reply_to_message_id"] = serde_json::Value::Number(reply_id.into());
                }
            }

            // Attach keyboard only to the last chunk.
            if i == chunks.len() - 1 {
                if let Some(ref kb) = keyboard {
                    body["reply_markup"] =
                        serde_json::to_value(kb).unwrap_or(serde_json::Value::Null);
                }
            }

            let url = format!("{}/sendMessage", self.api_base);
            let resp = match self.post_json::<SentMessage>(&url, &body).await {
                Ok(r) if r.ok => r,
                // HTTP 200 with ok=false and a parse-entities description.
                Ok(r) if is_parse_entities_error(r.description.as_deref().unwrap_or_default()) => {
                    warn!(
                        chat_id = %chat_id,
                        description = ?r.description,
                        "Telegram rejected parse_mode for chunk, retrying once as plain text"
                    );
                    let retry = self.resend_chunk_plain(&url, &mut body).await?;
                    Self::check_plain_retry_ok(retry)?
                }
                Ok(r) => {
                    // Non-parse API failure (ok=false): preserve prior
                    // behavior (drop this chunk) but make the loss visible.
                    warn!(
                        chat_id = %chat_id,
                        description = ?r.description,
                        "Telegram sendMessage returned ok=false, dropping chunk"
                    );
                    continue;
                }
                // HTTP 400 whose body carries a parse-entities description.
                Err(e) if is_parse_entities_send_failure(&e) => {
                    warn!(
                        chat_id = %chat_id,
                        error = %e,
                        "Telegram returned 400 can't-parse-entities, retrying chunk once as plain text"
                    );
                    let retry = self.resend_chunk_plain(&url, &mut body).await?;
                    Self::check_plain_retry_ok(retry)?
                }
                Err(e) => return Err(e),
            };

            if let Some(msg) = resp.result {
                message_ids.push(msg.message_id);
            }
        }

        Ok(message_ids)
    }
    /// Resend a chunk body with `parse_mode` removed (plain-text retry).
    /// Exactly one attempt — callers must not loop on this.
    pub(crate) async fn resend_chunk_plain(
        &self,
        url: &str,
        body: &mut serde_json::Value,
    ) -> Result<TelegramResponse<SentMessage>, GatewayError> {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("parse_mode");
        }
        self.post_json(url, body).await
    }
    /// Convert a plain-text retry response into the response, or an error
    /// when the retry itself failed (the original error is already logged).
    pub(crate) fn check_plain_retry_ok(
        retry: TelegramResponse<SentMessage>,
    ) -> Result<TelegramResponse<SentMessage>, GatewayError> {
        if retry.ok {
            Ok(retry)
        } else {
            Err(GatewayError::SendFailed(format!(
                "Telegram sendMessage failed after plain-text retry: {}",
                retry
                    .description
                    .unwrap_or_else(|| "unknown error".to_string())
            )))
        }
    }
    /// Edit an existing message's text.
    pub async fn edit_text(
        &self,
        chat_id: &str,
        message_id: &str,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<(), GatewayError> {
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id.parse::<i64>().unwrap_or(0),
            "text": &text[..text.floor_char_boundary(text.len().min(MAX_MESSAGE_LENGTH))],
        });

        if let Some(pm) = parse_mode {
            body["parse_mode"] = serde_json::Value::String(pm.to_string());
        }

        let url = format!("{}/editMessageText", self.api_base);
        let _resp: TelegramResponse<serde_json::Value> = self.post_json(&url, &body).await?;
        Ok(())
    }
    /// Delete a message. Best-effort cleanup: failures are logged at debug
    /// and swallowed so they never break the message flow.
    pub async fn delete_message(&self, chat_id: &str, message_id: i64) -> Result<(), GatewayError> {
        let url = format!("{}/deleteMessage", self.api_base);
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
        });
        if let Err(e) = self.post_json::<serde_json::Value>(&url, &body).await {
            tracing::debug!(
                "Telegram deleteMessage failed for chat {chat_id} message {message_id}: {e}"
            );
        }
        Ok(())
    }
    /// Answer a callback query (acknowledges the button press to the user).
    pub async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: Option<&str>,
        show_alert: bool,
    ) -> Result<(), GatewayError> {
        let mut body = serde_json::json!({
            "callback_query_id": callback_query_id,
            "show_alert": show_alert,
        });

        if let Some(t) = text {
            body["text"] = serde_json::Value::String(t.to_string());
        }

        let url = format!("{}/answerCallbackQuery", self.api_base);
        let _resp: TelegramResponse<bool> = self.post_json(&url, &body).await?;
        Ok(())
    }
    /// Send a document file.
    pub async fn send_document(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<i64, GatewayError> {
        self.send_multipart(chat_id, file_path, caption, "sendDocument", "document")
            .await
    }
    /// Send a photo file.
    pub async fn send_photo(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<i64, GatewayError> {
        self.send_multipart(chat_id, file_path, caption, "sendPhoto", "photo")
            .await
    }
    /// Send an audio file.
    pub async fn send_audio(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<i64, GatewayError> {
        self.send_multipart(chat_id, file_path, caption, "sendAudio", "audio")
            .await
    }
    /// Send a video file.
    pub async fn send_video(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<i64, GatewayError> {
        self.send_multipart(chat_id, file_path, caption, "sendVideo", "video")
            .await
    }
    /// Send a voice message (OGG Opus).
    pub async fn send_voice(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<i64, GatewayError> {
        self.send_multipart(chat_id, file_path, caption, "sendVoice", "voice")
            .await
    }
    /// Send an animation (GIF / MPEG4).
    pub async fn send_animation(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<i64, GatewayError> {
        self.send_multipart(chat_id, file_path, caption, "sendAnimation", "animation")
            .await
    }
    /// Send a sticker by file path.
    pub async fn send_sticker(&self, chat_id: &str, file_path: &str) -> Result<i64, GatewayError> {
        self.send_multipart(chat_id, file_path, None, "sendSticker", "sticker")
            .await
    }
    /// Send a sticker by its `file_id` (already on Telegram servers).
    pub async fn send_sticker_by_id(
        &self,
        chat_id: &str,
        sticker_file_id: &str,
    ) -> Result<i64, GatewayError> {
        let body = serde_json::json!({
            "chat_id": chat_id,
            "sticker": sticker_file_id,
        });

        let url = format!("{}/sendSticker", self.api_base);
        let resp: TelegramResponse<SentMessage> = self.post_json(&url, &body).await?;

        resp.result.map(|m| m.message_id).ok_or_else(|| {
            GatewayError::SendFailed(
                resp.description
                    .unwrap_or_else(|| "sendSticker failed".into()),
            )
        })
    }
    /// Shared multipart upload for all media-sending endpoints.
    pub(crate) async fn send_multipart(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
        method: &str,
        field_name: &str,
    ) -> Result<i64, GatewayError> {
        let url = format!("{}/{}", self.api_base, method);

        let file_bytes = tokio::fs::read(file_path).await.map_err(|e| {
            GatewayError::SendFailed(format!("Failed to read file {}: {}", file_path, e))
        })?;

        let file_name = std::path::Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();

        let part = reqwest::multipart::Part::bytes(file_bytes).file_name(file_name);

        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part(field_name.to_string(), part);

        if let Some(cap) = caption {
            // Cap at Telegram's 1024-char caption limit (M-S2): an over-long
            // caption fails the whole request with HTTP 400.
            form = form.text("caption", truncate_caption(cap));
        }

        let resp = self
            .client
            .post(&url)
            .multipart(form)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("{} failed: {}", method, e)))?;

        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "Rate limited on {}: {}",
                method, body_text
            )));
        }

        let result: TelegramResponse<SentMessage> = resp.json().await.map_err(|e| {
            GatewayError::SendFailed(format!("Failed to parse {} response: {}", method, e))
        })?;

        result.result.map(|m| m.message_id).ok_or_else(|| {
            GatewayError::SendFailed(
                result
                    .description
                    .unwrap_or_else(|| format!("{} failed", method)),
            )
        })
    }
    /// Download a file from Telegram by file_id.
    /// Returns the URL from which the file can be downloaded.
    pub async fn get_file_url(&self, file_id: &str) -> Result<String, GatewayError> {
        let url = format!("{}/getFile", self.api_base);
        let body = serde_json::json!({ "file_id": file_id });

        let resp: TelegramResponse<TelegramFile> = self.post_json(&url, &body).await?;

        let file = resp.result.ok_or_else(|| {
            GatewayError::ConnectionFailed(
                resp.description.unwrap_or_else(|| "getFile failed".into()),
            )
        })?;

        let file_path = file
            .file_path
            .ok_or_else(|| GatewayError::ConnectionFailed("File path not available".into()))?;

        Ok(format!(
            "https://api.telegram.org/file/bot{}/{}",
            self.config.token, file_path
        ))
    }
    /// Get information about a chat member (useful for admin checks).
    pub async fn get_chat_member(
        &self,
        chat_id: &str,
        user_id: i64,
    ) -> Result<ChatMember, GatewayError> {
        let url = format!("{}/getChatMember", self.api_base);
        let body = serde_json::json!({
            "chat_id": chat_id,
            "user_id": user_id,
        });

        let resp: TelegramResponse<ChatMember> = self.post_json(&url, &body).await?;

        resp.result.ok_or_else(|| {
            GatewayError::Platform(
                resp.description
                    .unwrap_or_else(|| "getChatMember failed".into()),
            )
        })
    }
    /// Check if a user is an admin or creator in a chat.
    pub async fn is_admin(&self, chat_id: &str, user_id: i64) -> Result<bool, GatewayError> {
        let member = self.get_chat_member(chat_id, user_id).await?;
        Ok(matches!(
            member.status.as_str(),
            "administrator" | "creator"
        ))
    }
    /// POST JSON to a Telegram API endpoint and deserialize the response.
    ///
    /// Error handling:
    /// - HTTP 429 (rate limited): extracts `retry_after` from the body,
    ///   sleeps (capped at [`RATE_LIMIT_INLINE_SLEEP_CAP_SECS`]) and retries
    ///   up to `RATE_LIMIT_MAX_RETRIES` times. A `retry_after` above the cap
    ///   returns a retryable error instead of sleeping inline for hours.
    /// - Other non-2xx statuses and HTTP-200 bodies with `"ok": false` are
    ///   mapped via [`classify_telegram_error`]: 401 → `Auth` (fatal),
    ///   403 → terminal `Platform`, 409 → `Platform` with a
    ///   `telegram_conflict:` marker for the poll loop.
    /// - The bot token is redacted from every log line and error message.
    pub(crate) async fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<TelegramResponse<T>, GatewayError> {
        let redacted_url = redact_url(url);
        let mut retries = 0u32;

        loop {
            let resp = self.client.post(url).json(body).send().await.map_err(|e| {
                GatewayError::ConnectionFailed(format!(
                    "Telegram API request failed: {}",
                    redact_token(&e.to_string(), &self.config.token)
                ))
            })?;

            let status = resp.status();

            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let text = resp.text().await.unwrap_or_default();

                let retry_after = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|v| v.get("parameters")?.get("retry_after")?.as_u64())
                    .unwrap_or(5);

                retries += 1;
                if retries > RATE_LIMIT_MAX_RETRIES {
                    return Err(GatewayError::SendFailed(format!(
                        "Rate limited after {} retries (retry_after={}s) at {}",
                        retries, retry_after, redacted_url
                    )));
                }

                // Never sleep inline for hours: a large `retry_after` is
                // returned as a retryable error so the caller's own backoff
                // (e.g. the poll loop) applies instead.
                if retry_after > RATE_LIMIT_INLINE_SLEEP_CAP_SECS {
                    warn!(
                        retry_after_secs = retry_after,
                        cap_secs = RATE_LIMIT_INLINE_SLEEP_CAP_SECS,
                        attempt = retries,
                        url = %redacted_url,
                        "Telegram API rate limited: retry_after exceeds inline cap, returning retryable error"
                    );
                    return Err(GatewayError::SendFailed(format!(
                        "telegram_rate_limited: retry_after={}s exceeds inline cap of {}s; backing off at caller level",
                        retry_after, RATE_LIMIT_INLINE_SLEEP_CAP_SECS
                    )));
                }

                warn!(
                    retry_after_secs = retry_after,
                    attempt = retries,
                    url = %redacted_url,
                    "Telegram API rate limited, backing off"
                );
                tokio::time::sleep(tokio::time::Duration::from_secs(retry_after)).await;
                continue;
            }

            if !status.is_success() {
                // Best-effort: lift Telegram's own error description out of
                // the body before classifying.
                let text = resp.text().await.unwrap_or_default();
                let parsed =
                    serde_json::from_str::<TelegramResponse<serde_json::Value>>(&text).ok();
                let description = parsed
                    .as_ref()
                    .and_then(|r| r.description.clone())
                    .unwrap_or(text);
                let error_code = parsed.and_then(|r| r.error_code);
                return Err(classify_telegram_error(
                    status.as_u16(),
                    error_code,
                    &description,
                ));
            }

            // HTTP 2xx with `"ok": false` is still an API error.
            let parsed: TelegramResponse<T> = resp.json().await.map_err(|e| {
                GatewayError::ConnectionFailed(format!(
                    "Failed to parse Telegram API response at {}: {}",
                    redacted_url,
                    redact_token(&e.to_string(), &self.config.token)
                ))
            })?;

            if !parsed.ok {
                let description = parsed
                    .description
                    .unwrap_or_else(|| "no description from Telegram".to_string());
                return Err(classify_telegram_error(
                    status.as_u16(),
                    parsed.error_code,
                    &description,
                ));
            }

            return Ok(parsed);
        }
    }
    /// Resolve a `ParseMode` to the Telegram API string.
    pub(crate) fn resolve_parse_mode(&self, parse_mode: Option<ParseMode>) -> Option<&'static str> {
        match parse_mode {
            Some(ParseMode::Markdown) => Some("MarkdownV2"),
            Some(ParseMode::Html) => Some("HTML"),
            Some(ParseMode::Plain) | None => {
                if self.config.parse_markdown {
                    Some("MarkdownV2")
                } else if self.config.parse_html {
                    Some("HTML")
                } else {
                    None
                }
            }
        }
    }
    /// Determine the appropriate send method for a file based on extension.
    pub(crate) fn media_method_for_extension(ext: &str) -> (&'static str, &'static str) {
        match ext {
            "jpg" | "jpeg" | "png" | "webp" => ("sendPhoto", "photo"),
            "gif" => ("sendAnimation", "animation"),
            "mp4" | "mov" | "avi" | "mkv" | "webm" => ("sendVideo", "video"),
            "mp3" | "flac" | "aac" | "m4a" | "wav" => ("sendAudio", "audio"),
            "ogg" | "oga" => ("sendVoice", "voice"),
            "webm_sticker" | "tgs" => ("sendSticker", "sticker"),
            _ => ("sendDocument", "document"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platforms::telegram::test_helpers::*;
    use crate::platforms::telegram::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn split_markdown_short() {
        let chunks = split_markdown("hello", 4096);
        assert_eq!(chunks, vec!["hello"]);
    }
    #[test]
    fn split_markdown_exact_boundary() {
        let text = "a".repeat(4096);
        let chunks = split_markdown(&text, 4096);
        assert_eq!(chunks.len(), 1);
    }
    #[test]
    fn split_markdown_long() {
        let text = "a".repeat(5000);
        let chunks = split_markdown(&text, 4096);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 4096);
        assert_eq!(chunks[1].len(), 904);
    }
    #[test]
    fn split_markdown_prefers_newline() {
        let mut text = "a".repeat(4000);
        text.push('\n');
        text.push_str(&"b".repeat(200));
        let chunks = split_markdown(&text, 4096);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].ends_with('\n'));
    }
    #[test]
    fn split_markdown_telegram_fence_aware() {
        let code: String = (0..250)
            .map(|i| format!("line {i}: some_code();"))
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!("Intro\n```rust\n{code}\n```\nOutro");
        let chunks = split_markdown(&text, 4096);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.len() <= 4196, "chunk exceeds Telegram limit: {}", c.len());
            assert_eq!(c.matches("```").count() % 2, 0);
        }
    }
    #[test]
    fn truncate_caption_short_is_unchanged() {
        let cap = "a".repeat(1024);
        assert_eq!(truncate_caption(&cap), cap);
        assert_eq!(truncate_caption("hello"), "hello");
    }
    #[test]
    fn truncate_caption_long_is_capped_at_1024_chars() {
        let cap = "a".repeat(2000);
        let truncated = truncate_caption(&cap);
        assert_eq!(truncated.chars().count(), 1024);
        assert_eq!(truncated, "a".repeat(1024));
    }
    #[test]
    fn truncate_caption_cuts_at_char_boundary() {
        // Multibyte chars must not be split: 1023 'a' + 'é' + 'b' would
        // panic on a byte-slice cut, but char-wise take is safe.
        let mut cap = "a".repeat(1023);
        cap.push('é');
        cap.push('b');
        let truncated = truncate_caption(&cap);
        assert_eq!(truncated.chars().count(), 1024);
        assert!(truncated.ends_with('é'));
        assert!(truncated.is_char_boundary(truncated.len()));
    }
    #[test]
    fn media_method_for_known_extensions() {
        assert_eq!(
            TelegramAdapter::media_method_for_extension("jpg"),
            ("sendPhoto", "photo")
        );
        assert_eq!(
            TelegramAdapter::media_method_for_extension("png"),
            ("sendPhoto", "photo")
        );
        assert_eq!(
            TelegramAdapter::media_method_for_extension("gif"),
            ("sendAnimation", "animation")
        );
        assert_eq!(
            TelegramAdapter::media_method_for_extension("mp4"),
            ("sendVideo", "video")
        );
        assert_eq!(
            TelegramAdapter::media_method_for_extension("mp3"),
            ("sendAudio", "audio")
        );
        assert_eq!(
            TelegramAdapter::media_method_for_extension("ogg"),
            ("sendVoice", "voice")
        );
        assert_eq!(
            TelegramAdapter::media_method_for_extension("pdf"),
            ("sendDocument", "document")
        );
        assert_eq!(
            TelegramAdapter::media_method_for_extension("zip"),
            ("sendDocument", "document")
        );
    }
    #[tokio::test]
    async fn delete_message_posts_delete_message_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/deleteMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": true
            })))
            .expect(1)
            .mount(&server)
            .await;

        let adapter = mock_server_adapter(&server.uri());
        adapter
            .delete_message("12345", 777)
            .await
            .expect("delete ok");

        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json body");
        assert_eq!(body["chat_id"], serde_json::json!("12345"));
        assert_eq!(body["message_id"], serde_json::json!(777));
    }
    #[tokio::test]
    async fn delete_message_swallows_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/deleteMessage"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Bad Request: message to delete not found"
            })))
            .mount(&server)
            .await;

        let adapter = mock_server_adapter(&server.uri());
        // Best-effort cleanup: a failed delete must never fail the flow.
        adapter
            .delete_message("12345", 777)
            .await
            .expect("delete swallows errors");
    }
    #[tokio::test]
    async fn send_message_raw_returns_message_ids() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "message_id": 42 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let adapter = mock_server_adapter(&server.uri());
        let ids = adapter
            .send_message_raw("12345", "hello", None)
            .await
            .expect("send ok");
        assert_eq!(ids, vec![42]);
    }
    #[test]
    fn max_edit_chars_matches_telegram_limit() {
        let adapter = test_adapter();
        assert_eq!(adapter.max_edit_chars(), 4096);
    }
    #[test]
    fn extract_retry_after_seconds_parses() {
        assert_eq!(
            extract_retry_after_seconds(
                "Rate limited after 4 retries (retry_after=37s): {\"ok\":false}"
            ),
            Some(37)
        );
        assert_eq!(extract_retry_after_seconds("retry_after="), None);
        assert_eq!(extract_retry_after_seconds("nothing here"), None);
    }
    #[test]
    fn typing_on_cooldown_skips_until_expiry() {
        let cooldowns: std::sync::Mutex<HashMap<String, std::time::Instant>> =
            std::sync::Mutex::new(HashMap::new());
        let chat = "12345";

        // No entry: not on cooldown.
        assert!(!typing_on_cooldown(&cooldowns, chat));

        // Future deadline: on cooldown.
        cooldowns.lock().unwrap().insert(
            chat.to_string(),
            std::time::Instant::now() + std::time::Duration::from_secs(60),
        );
        assert!(typing_on_cooldown(&cooldowns, chat));

        // Past deadline: no longer on cooldown.
        cooldowns.lock().unwrap().insert(
            chat.to_string(),
            std::time::Instant::now() - std::time::Duration::from_secs(1),
        );
        assert!(!typing_on_cooldown(&cooldowns, chat));

        // Other chats unaffected.
        assert!(!typing_on_cooldown(&cooldowns, "other"));
    }
}
