//! Telegram adapter — outbound messaging.

use std::path::{Path, PathBuf};

use serde_json;
use tracing::{debug, warn};

use hermes_core::errors::GatewayError;
use hermes_core::traits::ParseMode;

use super::error::*;
use super::types::*;
use super::TelegramAdapter;
use crate::markdown_split::split_markdown;

/// Reaction emoji set on an inbound message while the agent processes it
/// (M-T4, opt-in via [`TelegramConfig::message_reactions`]).
pub(crate) const REACTION_PROCESSING: &str = "👀";
/// Reaction emoji replacing the processing reaction once the agent's reply
/// has been delivered (M-T4).
pub(crate) const REACTION_SUCCESS: &str = "👍";
/// Reaction emoji replacing the processing reaction when processing the
/// message fails (M-T4).
pub(crate) const REACTION_ERROR: &str = "👎";

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

// ── Outbound media polish (M-M1…M-M4) ──────────────────────────────────
//
// Pure decision helpers (`photo_strategy`, `voice_route`,
// `parse_media_duration_secs`, `parse_video_meta`) are unit-tested.
// The ffmpeg/ffprobe wrappers are blocking — always call them inside
// `tokio::task::spawn_blocking` — and degrade gracefully to `None` when
// the binary is missing or the command fails, so the plain send path is
// always available as a fallback.

/// Photos larger than this are pre-compressed to JPEG before `sendPhoto`
/// (M-M1; mirrors the original's `_IMG_COMPRESS_THRESHOLD_BYTES`).
pub(crate) const PHOTO_COMPRESS_THRESHOLD_BYTES: u64 = 1_048_576;
/// JPEG quality for pre-compressed photos (original `_IMG_JPEG_QUALITY`).
/// ffmpeg's mjpeg `-q:v` scale runs 2 (best) → 31; 4 ≈ quality 85.
pub(crate) const PHOTO_FFMPEG_QV: &str = "4";
/// Longest side (px) for pre-compressed photos (original `_IMG_MAX_DIMENSION`).
pub(crate) const PHOTO_MAX_DIMENSION: u32 = 1600;
/// Practical `sendPhoto` ceiling (~10MB); larger photos go through
/// `sendDocument` instead (M-M1). Telegram bot documents allow up to 50MB.
pub(crate) const PHOTO_DOCUMENT_FALLBACK_BYTES: u64 = 10_485_760;
/// Bitrate for Ogg/Opus voice transcodes (original `transcode_to_ogg_opus`).
const VOICE_OPUS_BITRATE: &str = "32k";

/// How a photo file should be sent (M-M1). Pure — unit-tested.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PhotoStrategy {
    /// Send as-is via `sendPhoto`.
    Direct,
    /// Try JPEG pre-compression first, then `sendPhoto`.
    Compress,
    /// Too large for `sendPhoto`; use `sendDocument` instead.
    AsDocument,
}

/// Decide how a photo file should be sent (M-M1). Pure — unit-tested.
pub(crate) fn photo_strategy(file_size: u64, extension: &str) -> PhotoStrategy {
    if file_size > PHOTO_DOCUMENT_FALLBACK_BYTES {
        return PhotoStrategy::AsDocument;
    }
    match extension
        .trim_start_matches('.')
        .to_ascii_lowercase()
        .as_str()
    {
        // Already JPEG — recompressing gains nothing.
        "jpg" | "jpeg" => PhotoStrategy::Direct,
        _ if file_size > PHOTO_COMPRESS_THRESHOLD_BYTES => PhotoStrategy::Compress,
        _ => PhotoStrategy::Direct,
    }
}

/// How an audio file should be sent (M-M3). Pure — unit-tested.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum VoiceRoute {
    /// Native voice bubble (`sendVoice`; requires Ogg/Opus).
    VoiceBubble,
    /// Regular audio file (`sendAudio`; MP3/M4A per Bot API).
    Audio,
    /// Anything else Telegram can't play natively → `sendDocument`.
    Document,
}

/// Route an audio file by extension (M-M3). Pure — unit-tested.
pub(crate) fn voice_route(extension: &str) -> VoiceRoute {
    match extension
        .trim_start_matches('.')
        .to_ascii_lowercase()
        .as_str()
    {
        "ogg" | "opus" => VoiceRoute::VoiceBubble,
        "mp3" | "m4a" => VoiceRoute::Audio,
        _ => VoiceRoute::Document,
    }
}

/// Video metadata probed via ffprobe (M-M4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VideoMeta {
    pub duration_secs: u64,
    pub width: u32,
    pub height: u32,
}

/// Minimal `which(1)`: find `name` on PATH. Used for ffmpeg/ffprobe
/// detection (M-M1/M-M3/M-M4) without adding a dependency.
pub(crate) fn find_binary(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Unique temp path for transcoded media. The caller deletes it after use.
fn temp_media_path(prefix: &str, suffix: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("{prefix}{nanos}{suffix}"))
}

/// Pre-compress a large photo to JPEG via ffmpeg (M-M1).
///
/// The original does this with Pillow (`_compress_image_to_jpeg`); the
/// `image` crate is not a dependency of hermes-gateway (it could not be
/// added and build-verified from this sandbox, so the limitation is
/// documented here rather than papered over), so compression goes through
/// ffmpeg when available: longest side capped at [`PHOTO_MAX_DIMENSION`]
/// px, JPEG quality ≈85 (`-q:v 4`).
///
/// Returns the temp `.jpg` path (caller deletes it), or `None` when ffmpeg
/// is missing, the source is unreadable, or the encode fails — in which
/// case the caller uploads the original. Blocking — run in `spawn_blocking`.
fn compress_photo_to_jpeg(src: &Path) -> Option<PathBuf> {
    let ffmpeg = find_binary("ffmpeg")?;
    let tmp = temp_media_path("tg_compress_", ".jpg");
    // Cap the longest side, keep aspect ratio.
    let scale = format!(
        "scale='if(gt(iw,ih),{m},-2)':'if(gt(iw,ih),-2,{m})'",
        m = PHOTO_MAX_DIMENSION
    );
    let out = std::process::Command::new(ffmpeg)
        .args([
            "-v",
            "error",
            "-y",
            "-i",
            src.to_str()?,
            "-vf",
            &scale,
            "-q:v",
            PHOTO_FFMPEG_QV,
            tmp.to_str()?,
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if out.status.success() && tmp.is_file() && tmp.metadata().map(|m| m.len()).unwrap_or(0) > 0 {
        debug!(
            "Telegram: pre-compressed photo {} for upload",
            tmp.display()
        );
        Some(tmp)
    } else {
        debug!("Telegram: photo compression failed, uploading original");
        let _ = std::fs::remove_file(&tmp);
        None
    }
}

/// Transcode `src` to Ogg/Opus (voip-tuned, 32k) via ffmpeg, mirroring the
/// original's `transcode_to_ogg_opus` in `gateway/platforms/base.py`.
///
/// Returns the temp `.ogg` path (caller deletes it), or `None` when ffmpeg
/// is missing or the transcode fails. Blocking — run in `spawn_blocking`.
fn transcode_to_ogg_opus(src: &Path) -> Option<PathBuf> {
    transcode_with_ffmpeg(&find_binary("ffmpeg")?, src)
}

/// Core of [`transcode_to_ogg_opus`] with an explicit ffmpeg binary, so the
/// graceful-degradation path is unit-testable without touching PATH.
fn transcode_with_ffmpeg(ffmpeg: &Path, src: &Path) -> Option<PathBuf> {
    let tmp = temp_media_path("tg_voice_", ".ogg");
    let out = std::process::Command::new(ffmpeg)
        .args([
            "-v",
            "error",
            "-y",
            "-i",
            src.to_str()?,
            "-acodec",
            "libopus",
            "-ac",
            "1",
            "-b:a",
            VOICE_OPUS_BITRATE,
            "-vbr",
            "on",
            "-application",
            "voip",
            "-compression_level",
            "10",
            "-f",
            "ogg",
            tmp.to_str()?,
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if out.status.success() && tmp.is_file() && tmp.metadata().map(|m| m.len()).unwrap_or(0) > 0 {
        debug!(
            "Telegram: transcoded voice note to Ogg/Opus ({})",
            tmp.display()
        );
        Some(tmp)
    } else {
        debug!("Telegram: ffmpeg Ogg/Opus transcode failed; sending original format");
        let _ = std::fs::remove_file(&tmp);
        None
    }
}

/// Parse an ffprobe `format=duration` plain-text value into whole seconds.
/// Pure — unit-tested.
pub(crate) fn parse_media_duration_secs(stdout: &str) -> Option<u64> {
    stdout.trim().parse::<f64>().ok().map(|d| d.max(0.0) as u64)
}

/// Probe media duration in seconds via ffprobe. Returns `None` when ffprobe
/// is missing or probing fails. Blocking — run in `spawn_blocking`.
fn probe_media_duration_secs(path: &Path) -> Option<u64> {
    let ffprobe = find_binary("ffprobe")?;
    let out = std::process::Command::new(&ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
            path.to_str()?,
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if out.status.success() {
        parse_media_duration_secs(&String::from_utf8_lossy(&out.stdout))
    } else {
        None
    }
}

/// Parse `ffprobe -show_entries stream=width,height -show_entries
/// format=duration -of default=noprint_wrappers=1` output.
/// Pure — unit-tested.
pub(crate) fn parse_video_meta(output: &str) -> Option<VideoMeta> {
    let mut width = None;
    let mut height = None;
    let mut duration = None;
    for line in output.lines() {
        let (k, v) = line.trim().split_once('=')?;
        match k {
            "width" => width = v.parse().ok(),
            "height" => height = v.parse().ok(),
            "duration" => duration = parse_media_duration_secs(v),
            _ => {}
        }
    }
    Some(VideoMeta {
        duration_secs: duration?,
        width: width?,
        height: height?,
    })
}

/// Probe video duration/geometry via ffprobe (M-M4). Blocking — run in
/// `spawn_blocking`. `None` when ffprobe is missing or probing fails.
fn probe_video_meta(path: &Path) -> Option<VideoMeta> {
    let ffprobe = find_binary("ffprobe")?;
    let out = std::process::Command::new(&ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1",
            path.to_str()?,
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if out.status.success() {
        parse_video_meta(&String::from_utf8_lossy(&out.stdout))
    } else {
        None
    }
}

/// Extract a JPEG thumbnail (frame ~1s in) via ffmpeg (M-M4).
/// Returns the temp path (caller deletes it), or `None` on any failure.
/// Blocking — run in `spawn_blocking`.
fn extract_video_thumbnail(src: &Path) -> Option<PathBuf> {
    let ffmpeg = find_binary("ffmpeg")?;
    let tmp = temp_media_path("tg_thumb_", ".jpg");
    let out = std::process::Command::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-y",
            "-ss",
            "1",
            "-i",
            src.to_str()?,
            "-vframes",
            "1",
            "-q:v",
            PHOTO_FFMPEG_QV,
            tmp.to_str()?,
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if out.status.success() && tmp.is_file() {
        Some(tmp)
    } else {
        let _ = std::fs::remove_file(&tmp);
        None
    }
}

impl TelegramAdapter {
    /// Build the `setMyCommands` request body for the dynamic command menu.
    ///
    /// When `chat_scope` is `Some`, the commands are scoped to that single
    /// chat via `scope: {"type": "chat", "chat_id": ...}` (used for forum
    /// chats — see [`register_command_menu`]).
    fn command_menu_body(&self, chat_scope: Option<i64>) -> serde_json::Value {
        let max = self
            .config
            .command_menu_max
            .unwrap_or(crate::commands::TELEGRAM_MENU_DEFAULT_MAX);
        let menu = crate::commands::telegram_menu_commands_with_skills(
            max,
            self.config.command_menu_skills,
        );

        let commands: Vec<serde_json::Value> = menu
            .iter()
            .map(|(cmd, desc)| {
                let desc = desc.chars().take(256).collect::<String>();
                serde_json::json!({ "command": cmd, "description": desc })
            })
            .collect();

        let mut body = serde_json::json!({ "commands": commands });
        if let Some(chat_id) = chat_scope {
            body["scope"] = serde_json::json!({ "type": "chat", "chat_id": chat_id });
        }
        body
    }

    /// Register the bot's / command menu via `setMyCommands`.
    ///
    /// Mirrors the original's `_register_command_menu`: exposes the gateway's
    /// slash commands in Telegram's UI. Dynamic — built from
    /// [`crate::commands::telegram_menu_commands_with_skills`] (builtins +
    /// extensions + skills), ordered by [`crate::commands::TELEGRAM_MENU_PRIORITY`]
    /// and capped at `TelegramConfig::command_menu_max` (default 60, mirroring
    /// the original so the payload stays under Telegram's ~4KB menu limit).
    /// Skills are included unless `TelegramConfig::command_menu_skills` is
    /// false. Best-effort — failures are returned so the caller can log them,
    /// but must not fail startup.
    ///
    /// Limitation (M-G5): the default scope registered here only surfaces
    /// the menu in a forum supergroup's *General* topic — Telegram does not
    /// apply default-scope commands inside topics. The adapter compensates
    /// with [`spawn_forum_command_menu`], which registers a per-chat
    /// `{"type": "chat", ...}` scope the first time a topic message arrives
    /// in a forum chat, so `/` commands are visible inside topics too.
    pub async fn register_command_menu(&self) -> Result<(), GatewayError> {
        let body = self.command_menu_body(None);
        let url = format!("{}/setMyCommands", self.api_base);
        let resp: TelegramResponse<serde_json::Value> = self.post_json(&url, &body).await?;

        if resp.ok {
            tracing::info!(
                "Telegram setMyCommands OK ({} commands)",
                body["commands"].as_array().map(|a| a.len()).unwrap_or(0)
            );
        } else {
            tracing::warn!(
                "Telegram setMyCommands returned ok=false: {:?}",
                resp.description
            );
        }
        Ok(())
    }

    /// Register the command menu scoped to one forum chat (M-G5), in the
    /// background.
    ///
    /// The default-scope menu from [`register_command_menu`] does not show
    /// inside forum topics; a `{"type": "chat", "chat_id": ...}` scope makes
    /// the commands visible there. Callers must dedupe (e.g. via
    /// [`TelegramAdapter::note_forum_chat`]) — this spawns one fire-and-forget
    /// task per call and never fails the inbound path.
    pub(crate) fn spawn_forum_command_menu(&self, chat_id: i64) {
        let client = self.client.clone();
        let api_base = self.api_base.clone();
        let body = self.command_menu_body(Some(chat_id));
        tokio::spawn(async move {
            let url = format!("{api_base}/setMyCommands");
            match client.post(&url).json(&body).send().await {
                Ok(resp) => {
                    tracing::debug!(
                        chat_id,
                        status = %resp.status(),
                        "Telegram scoped setMyCommands sent for forum chat"
                    );
                }
                Err(e) => {
                    tracing::debug!(
                        chat_id,
                        error = %e,
                        "Telegram scoped setMyCommands failed for forum chat"
                    );
                }
            }
        });
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

        // M-T2: Telegram clears the typing bubble when a message lands. Drop
        // any typing cooldown so the keep-alive task re-arms typing on its
        // next tick instead of waiting out the full cooldown window.
        self.notify_message_sent(chat_id);

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
    /// Set an emoji reaction on a message via `setMessageReaction`.
    ///
    /// Lifecycle (M-T4, opt-in via `TelegramConfig::message_reactions`,
    /// mirrors the original):
    /// - 👀 ([`REACTION_PROCESSING`]) when
    ///   [`TelegramAdapter::process_update`] accepts an inbound message for
    ///   processing,
    /// - 👍 ([`REACTION_SUCCESS`]) once the agent's reply has been delivered,
    /// - 👎 ([`REACTION_ERROR`]) when processing the message fails.
    ///
    /// The 👀 hook is wired in `process_update`; the 👍/👎 transitions
    /// belong in `Gateway::route_message` (crates/hermes-gateway/src/gateway.rs):
    /// on the `route_result` `Ok` path call this with 👍, on the `Err`
    /// path with 👎, using `incoming.chat_id` and `incoming.message_id`
    /// (the Telegram message id as a string — parse back to `i64`). That
    /// wiring is deliberately deferred — gateway.rs sits at the 120KB push
    /// cap — so the adapter side is complete and each call site is a single
    /// `set_message_reaction` call away.
    ///
    /// Fire-and-forget: when the feature is disabled this is a pure no-op
    /// (no network traffic at all), and API failures are logged at debug
    /// and swallowed so a reaction can never break the message flow.
    pub async fn set_message_reaction(
        &self,
        chat_id: &str,
        message_id: i64,
        emoji: &str,
    ) -> Result<(), GatewayError> {
        if !self.config.message_reactions {
            return Ok(());
        }
        let url = format!("{}/setMessageReaction", self.api_base);
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "reaction": [{ "type": "emoji", "emoji": emoji }],
        });
        if let Err(e) = self.post_json::<serde_json::Value>(&url, &body).await {
            tracing::debug!(
                "Telegram setMessageReaction failed for chat {chat_id} message {message_id} ({emoji}): {e}"
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
    /// Send a photo file, with M-M1 polish:
    /// - files above [`PHOTO_DOCUMENT_FALLBACK_BYTES`] (~10MB, the practical
    ///   `sendPhoto` ceiling) are routed through `sendDocument` instead;
    /// - non-JPEG files above [`PHOTO_COMPRESS_THRESHOLD_BYTES`] (1MB) are
    ///   pre-compressed to JPEG (quality ~85, longest side capped at
    ///   [`PHOTO_MAX_DIMENSION`] px) when ffmpeg is available.
    ///
    /// Compression failures always fall back to uploading the original —
    /// sending the photo must never fail because of the polish path.
    pub async fn send_photo(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<i64, GatewayError> {
        let ext = Path::new(file_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let size = tokio::fs::metadata(file_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);

        match photo_strategy(size, ext) {
            PhotoStrategy::AsDocument => {
                debug!(
                    size_bytes = size,
                    "Telegram: photo exceeds 10MB sendPhoto ceiling, sending as document"
                );
                return self.send_document(chat_id, file_path, caption).await;
            }
            PhotoStrategy::Compress => {
                let src = PathBuf::from(file_path);
                let compressed = tokio::task::spawn_blocking(move || compress_photo_to_jpeg(&src))
                    .await
                    .ok()
                    .flatten();
                if let Some(tmp) = compressed {
                    let tmp_str = tmp.to_string_lossy().into_owned();
                    let result = self
                        .send_multipart(chat_id, &tmp_str, caption, "sendPhoto", "photo")
                        .await;
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return result;
                }
                debug!("Telegram: photo compression unavailable, uploading original");
            }
            PhotoStrategy::Direct => {}
        }
        self.send_multipart(chat_id, file_path, caption, "sendPhoto", "photo")
            .await
    }
    /// Send a photo from a remote URL (M-M2). Telegram fetches the URL
    /// itself, so no download is needed on our side.
    ///
    /// The URL must pass [`crate::ssrf::is_safe_url`] (public http/https
    /// only — mirrors the original's `is_safe_url` gate). Unsafe URLs are
    /// rejected with `SendFailed` and never requested.
    pub async fn send_photo_url(
        &self,
        chat_id: &str,
        url: &str,
        caption: Option<&str>,
    ) -> Result<i64, GatewayError> {
        if !crate::ssrf::is_safe_url(url) {
            warn!("Telegram: blocked unsafe photo URL (SSRF protection)");
            return Err(GatewayError::SendFailed(
                "Unsafe photo URL: must be a public http(s) URL".into(),
            ));
        }
        let api_url = format!("{}/sendPhoto", self.api_base);
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "photo": url,
        });
        if let Some(cap) = caption {
            body["caption"] = serde_json::json!(truncate_caption(cap));
        }
        let resp: TelegramResponse<SentMessage> = self.post_json(&api_url, &body).await?;
        resp.result.map(|m| m.message_id).ok_or_else(|| {
            GatewayError::SendFailed(
                resp.description
                    .unwrap_or_else(|| "sendPhoto (URL) failed".into()),
            )
        })
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
    /// Send a video with thumbnail and duration metadata (M-M4), mirroring
    /// the original's `send_video`: geometry/duration are probed via ffprobe
    /// and a JPEG thumbnail (first frame) is extracted via ffmpeg, then both
    /// ride along as `duration`/`width`/`height` fields and a `thumbnail`
    /// part. Without ffprobe/ffmpeg — or on any extraction failure — this
    /// degrades gracefully to a plain [`Self::send_video`].
    pub async fn send_video_smart(
        &self,
        chat_id: &str,
        video_path: &str,
        caption: Option<&str>,
    ) -> Result<i64, GatewayError> {
        if tokio::fs::metadata(video_path).await.is_err() {
            return Err(GatewayError::SendFailed(format!(
                "Video file not found: {}",
                video_path
            )));
        }
        let src = PathBuf::from(video_path);
        let probe_src = src.clone();
        let meta: Option<VideoMeta> =
            tokio::task::spawn_blocking(move || probe_video_meta(&probe_src))
                .await
                .ok()
                .flatten();

        // Extract thumbnail (blocking); failure → send without one.
        let thumb_src = src.clone();
        let thumb: Option<PathBuf> =
            tokio::task::spawn_blocking(move || extract_video_thumbnail(&thumb_src))
                .await
                .ok()
                .flatten();

        let mut extra_text: Vec<(String, String)> = Vec::new();
        if let Some(m) = meta {
            extra_text.push(("duration".to_string(), m.duration_secs.to_string()));
            extra_text.push(("width".to_string(), m.width.to_string()));
            extra_text.push(("height".to_string(), m.height.to_string()));
        }
        let extra_files: Vec<(String, PathBuf)> = thumb
            .iter()
            .map(|p| ("thumbnail".to_string(), p.clone()))
            .collect();

        let result = self
            .send_multipart_extra(
                chat_id,
                video_path,
                caption,
                "sendVideo",
                "video",
                &extra_text,
                &extra_files,
            )
            .await;

        for (_, p) in &extra_files {
            let _ = tokio::fs::remove_file(p).await;
        }
        result
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
    /// Send audio as a native Telegram voice message or audio file (M-M3),
    /// mirroring the original's `send_voice` routing:
    /// - `is_voice == true` and not already Ogg/Opus → transcode via ffmpeg
    ///   (when available) so the client renders a voice bubble;
    /// - `.ogg`/`.opus` → `sendVoice` (voice bubble);
    /// - `.mp3`/`.m4a` → `sendAudio` (Bot API audio);
    /// - anything else → `sendDocument`.
    ///
    /// Duration is probed via ffprobe when available and passed along.
    /// Every failure in the polish path degrades gracefully to the plain
    /// send — the audio must never be dropped because of it.
    pub async fn send_voice_smart(
        &self,
        chat_id: &str,
        audio_path: &str,
        caption: Option<&str>,
        is_voice: bool,
    ) -> Result<i64, GatewayError> {
        if tokio::fs::metadata(audio_path).await.is_err() {
            return Err(GatewayError::SendFailed(format!(
                "Audio file not found: {}",
                audio_path
            )));
        }
        let ext = Path::new(audio_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        // Transcode to Ogg/Opus for voice bubbles when needed (blocking).
        let mut transcoded: Option<PathBuf> = None;
        let mut effective = PathBuf::from(audio_path);
        if is_voice && !matches!(voice_route(ext), VoiceRoute::VoiceBubble) {
            let src = effective.clone();
            transcoded = tokio::task::spawn_blocking(move || transcode_to_ogg_opus(&src))
                .await
                .ok()
                .flatten();
            match &transcoded {
                Some(tmp) => effective = tmp.clone(),
                None => debug!(
                    "Telegram: voice transcode unavailable for {}, sending original format (install ffmpeg for voice bubbles)",
                    Path::new(audio_path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(audio_path)
                ),
            }
        }

        // Probe duration (blocking); missing ffprobe → None, no duration sent.
        let src_for_probe = effective.clone();
        let duration_secs: Option<u64> =
            tokio::task::spawn_blocking(move || probe_media_duration_secs(&src_for_probe))
                .await
                .ok()
                .flatten();

        let route_ext = effective.extension().and_then(|e| e.to_str()).unwrap_or("");
        let result = match voice_route(route_ext) {
            VoiceRoute::VoiceBubble => {
                let extra = duration_secs
                    .map(|d| vec![("duration".to_string(), d.to_string())])
                    .unwrap_or_default();
                self.send_multipart_extra(
                    chat_id,
                    effective.to_str().unwrap_or(audio_path),
                    caption,
                    "sendVoice",
                    "voice",
                    &extra,
                    &[],
                )
                .await
            }
            VoiceRoute::Audio => {
                let extra = duration_secs
                    .map(|d| vec![("duration".to_string(), d.to_string())])
                    .unwrap_or_default();
                self.send_multipart_extra(
                    chat_id,
                    effective.to_str().unwrap_or(audio_path),
                    caption,
                    "sendAudio",
                    "audio",
                    &extra,
                    &[],
                )
                .await
            }
            VoiceRoute::Document => {
                self.send_document(chat_id, effective.to_str().unwrap_or(audio_path), caption)
                    .await
            }
        };

        if let Some(tmp) = transcoded {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        result
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
        self.send_multipart_extra(chat_id, file_path, caption, method, field_name, &[], &[])
            .await
    }
    /// Multipart upload with extra text fields and extra file parts
    /// (M-M3/M-M4: `duration`/`width`/`height` text fields, `thumbnail`
    /// file part). Extra file parts that fail to read are skipped with a
    /// warning — the main file must still go out.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_multipart_extra(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
        method: &str,
        field_name: &str,
        extra_text: &[(String, String)],
        extra_files: &[(String, PathBuf)],
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

        for (key, value) in extra_text {
            form = form.text(key.clone(), value.clone());
        }

        for (field, path) in extra_files {
            match tokio::fs::read(path).await {
                Ok(bytes) => {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("file")
                        .to_string();
                    form = form.part(
                        field.clone(),
                        reqwest::multipart::Part::bytes(bytes).file_name(name),
                    );
                }
                Err(e) => {
                    warn!(
                        "Telegram: cannot read {} for {}; sending without it: {}",
                        path.display(),
                        method,
                        e
                    );
                }
            }
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
    #[tokio::test]
    async fn send_text_rearms_typing_after_cooldown() {
        let server = MockServer::start().await;
        // sendChatAction fails (403) -> 30s typing cooldown engaged.
        Mock::given(method("POST"))
            .and(path("/sendChatAction"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": false,
                "error_code": 403,
                "description": "Forbidden: bot was blocked by the user"
            })))
            .expect(2)
            .mount(&server)
            .await;
        // sendMessage succeeds.
        Mock::given(method("POST"))
            .and(path("/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "message_id": 7 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let adapter = mock_server_adapter(&server.uri());
        let chat = "12345";

        // First typing ping fails -> cooldown engaged, next tick would skip.
        adapter
            .send_typing(chat)
            .await
            .expect("typing is fire-and-forget");
        assert!(typing_on_cooldown(&adapter.typing_cooldown_until, chat));

        // A message lands -> M-T2 clears the cooldown.
        let ids = adapter
            .send_text(chat, "hello", None, None)
            .await
            .expect("send ok");
        assert_eq!(ids, vec![7]);
        assert!(!typing_on_cooldown(&adapter.typing_cooldown_until, chat));

        // The next keep-alive tick re-arms typing immediately (2nd POST).
        adapter
            .send_typing(chat)
            .await
            .expect("typing is fire-and-forget");
    }
    /// M-G5: a real topic id lands in the sendMessage body as
    /// `message_thread_id`.
    #[tokio::test]
    async fn send_message_threaded_includes_thread_id() {
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
        adapter
            .send_message_threaded("12345", "hello", None, Some("7"))
            .await
            .expect("send ok");

        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json body");
        assert_eq!(body["message_thread_id"], serde_json::json!(7));
    }
    /// M-G5: thread "1" (General) is normalized away — the Bot API rejects
    /// `message_thread_id: 1` and General is the default target anyway.
    /// No thread at all must also omit the field.
    #[tokio::test]
    async fn send_message_threaded_omits_general_and_missing_thread() {
        for thread in [Some("1"), None] {
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
            adapter
                .send_message_threaded("12345", "hello", None, thread)
                .await
                .expect("send ok");

            let requests = server.received_requests().await.expect("requests");
            assert_eq!(requests.len(), 1);
            let body: serde_json::Value =
                serde_json::from_slice(&requests[0].body).expect("json body");
            assert!(
                body.get("message_thread_id").is_none(),
                "thread {:?} must not produce message_thread_id",
                thread
            );
        }
    }
    /// M-G5: the threaded raw variant (streaming placeholder) also carries
    /// the thread id.
    #[tokio::test]
    async fn send_message_raw_threaded_includes_thread_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "message_id": 9 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let adapter = mock_server_adapter(&server.uri());
        let ids = adapter
            .send_message_raw_threaded("12345", "...", None, Some("7"))
            .await
            .expect("send ok");
        assert_eq!(ids, vec![9]);

        let requests = server.received_requests().await.expect("requests");
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json body");
        assert_eq!(body["message_thread_id"], serde_json::json!(7));
    }
    /// M-G5: the typing bubble needs `message_thread_id` on sendChatAction
    /// or it lands in the forum's General topic. Unlike sendMessage,
    /// sendChatAction accepts thread 1.
    #[tokio::test]
    async fn send_typing_threaded_includes_thread_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sendChatAction"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": true
            })))
            .expect(1)
            .mount(&server)
            .await;

        let adapter = mock_server_adapter(&server.uri());
        adapter
            .send_typing_threaded("12345", Some("9"))
            .await
            .expect("typing is fire-and-forget");

        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json body");
        assert_eq!(body["action"], serde_json::json!("typing"));
        assert_eq!(body["message_thread_id"], serde_json::json!(9));
    }
    /// M-G5: a known forum chat with no explicit thread falls back to
    /// General (1) for typing; a plain chat omits the field entirely.
    #[tokio::test]
    async fn send_typing_threaded_known_forum_falls_back_to_general() {
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
        Mock::given(method("POST"))
            .and(path("/sendChatAction"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": true
            })))
            .expect(2)
            .mount(&server)
            .await;

        let adapter = mock_server_adapter(&server.uri());
        // A threaded send teaches the adapter that 12345 is a forum chat.
        adapter
            .send_message_threaded("12345", "hello", None, Some("7"))
            .await
            .expect("send ok");

        // Thread-less typing in the known forum chat targets General (1).
        adapter
            .send_typing("12345")
            .await
            .expect("typing is fire-and-forget");

        // Thread-less typing in an unknown chat sends no thread id.
        adapter
            .send_typing("99999")
            .await
            .expect("typing is fire-and-forget");

        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 3);
        let forum_typing: serde_json::Value =
            serde_json::from_slice(&requests[1].body).expect("json body");
        assert_eq!(forum_typing["message_thread_id"], serde_json::json!(1));
        let plain_typing: serde_json::Value =
            serde_json::from_slice(&requests[2].body).expect("json body");
        assert!(plain_typing.get("message_thread_id").is_none());
    }
    /// M-G5: the per-forum-chat menu scope body carries the chat scope.
    #[test]
    fn command_menu_body_scoped_for_forum_chat() {
        let adapter = test_adapter();
        let scoped = adapter.command_menu_body(Some(-1001234));
        assert_eq!(
            scoped["scope"],
            serde_json::json!({"type": "chat", "chat_id": -1001234})
        );
        assert!(scoped["commands"].as_array().is_some());

        let default = adapter.command_menu_body(None);
        assert!(default.get("scope").is_none());
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
    #[tokio::test]
    async fn set_message_reaction_posts_correct_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/setMessageReaction"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": true
            })))
            .expect(1)
            .mount(&server)
            .await;

        let adapter = mock_server_adapter_with_reactions(&server.uri());
        adapter
            .set_message_reaction("12345", 777, REACTION_PROCESSING)
            .await
            .expect("reaction ok");

        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json body");
        assert_eq!(body["chat_id"], serde_json::json!("12345"));
        assert_eq!(body["message_id"], serde_json::json!(777));
        let reactions = body["reaction"].as_array().expect("reaction array");
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0]["type"], serde_json::json!("emoji"));
        assert_eq!(reactions[0]["emoji"], serde_json::json!("👀"));
    }
    #[tokio::test]
    async fn set_message_reaction_swallows_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/setMessageReaction"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Bad Request: message not found"
            })))
            .mount(&server)
            .await;

        let adapter = mock_server_adapter_with_reactions(&server.uri());
        // Fire-and-forget: a failed reaction must never fail the flow.
        adapter
            .set_message_reaction("12345", 777, REACTION_SUCCESS)
            .await
            .expect("reaction failure is swallowed");
    }
    #[tokio::test]
    async fn set_message_reaction_disabled_sends_no_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/setMessageReaction"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": true
            })))
            .expect(0)
            .mount(&server)
            .await;

        // Default config has message_reactions=false: pure no-op.
        let adapter = mock_server_adapter(&server.uri());
        assert!(!adapter.config().message_reactions);
        adapter
            .set_message_reaction("12345", 777, REACTION_PROCESSING)
            .await
            .expect("disabled reaction is a no-op");

        let requests = server.received_requests().await.expect("requests");
        assert!(requests.is_empty(), "disabled reactions must send nothing");
    }
    #[test]
    fn reaction_emoji_constants_match_original() {
        assert_eq!(REACTION_PROCESSING, "👀");
        assert_eq!(REACTION_SUCCESS, "👍");
        assert_eq!(REACTION_ERROR, "👎");
    }

    // ── Outbound media polish (M-M1…M-M4) ──────────────────────────

    #[test]
    fn photo_strategy_small_png_is_direct() {
        assert_eq!(photo_strategy(100, "png"), PhotoStrategy::Direct);
    }

    #[test]
    fn photo_strategy_large_png_compresses() {
        assert_eq!(
            photo_strategy(PHOTO_COMPRESS_THRESHOLD_BYTES + 1, "png"),
            PhotoStrategy::Compress
        );
        // Dotted extension and case-insensitivity.
        assert_eq!(
            photo_strategy(PHOTO_COMPRESS_THRESHOLD_BYTES + 1, ".PNG"),
            PhotoStrategy::Compress
        );
    }

    #[test]
    fn photo_strategy_large_jpeg_skips_compress() {
        // Already JPEG — recompressing gains nothing.
        assert_eq!(
            photo_strategy(PHOTO_COMPRESS_THRESHOLD_BYTES * 5, "jpg"),
            PhotoStrategy::Direct
        );
        assert_eq!(
            photo_strategy(PHOTO_COMPRESS_THRESHOLD_BYTES * 5, ".jpeg"),
            PhotoStrategy::Direct
        );
    }

    #[test]
    fn photo_strategy_huge_goes_as_document() {
        assert_eq!(
            photo_strategy(PHOTO_DOCUMENT_FALLBACK_BYTES + 1, "png"),
            PhotoStrategy::AsDocument
        );
        // Even a JPEG over the ceiling can't use sendPhoto.
        assert_eq!(
            photo_strategy(PHOTO_DOCUMENT_FALLBACK_BYTES + 1, "jpg"),
            PhotoStrategy::AsDocument
        );
    }

    #[test]
    fn voice_route_by_extension() {
        assert_eq!(voice_route("ogg"), VoiceRoute::VoiceBubble);
        assert_eq!(voice_route(".opus"), VoiceRoute::VoiceBubble);
        assert_eq!(voice_route("mp3"), VoiceRoute::Audio);
        assert_eq!(voice_route("M4A"), VoiceRoute::Audio);
        assert_eq!(voice_route("wav"), VoiceRoute::Document);
        assert_eq!(voice_route("flac"), VoiceRoute::Document);
        assert_eq!(voice_route(""), VoiceRoute::Document);
    }

    #[test]
    fn parse_media_duration_secs_parses_ffprobe_output() {
        assert_eq!(parse_media_duration_secs("12.345000\n"), Some(12));
        assert_eq!(parse_media_duration_secs("0.5"), Some(0));
        assert_eq!(parse_media_duration_secs("3600.0"), Some(3600));
        assert_eq!(parse_media_duration_secs("N/A"), None);
        assert_eq!(parse_media_duration_secs(""), None);
    }

    #[test]
    fn parse_video_meta_parses_ffprobe_output() {
        let out = "width=1920\nheight=1080\nduration=12.345000\n";
        assert_eq!(
            parse_video_meta(out),
            Some(VideoMeta {
                duration_secs: 12,
                width: 1920,
                height: 1080,
            })
        );
    }

    #[test]
    fn parse_video_meta_missing_field_is_none() {
        assert_eq!(parse_video_meta("width=1920\nheight=1080\n"), None);
        assert_eq!(parse_video_meta(""), None);
    }

    #[test]
    fn find_binary_missing_returns_none() {
        assert!(find_binary("definitely-not-a-real-binary-xyz-123").is_none());
    }

    #[test]
    fn find_binary_finds_sh() {
        // `sh` is present on every Unix CI/dev machine.
        assert!(find_binary("sh").is_some());
    }

    #[tokio::test]
    async fn send_photo_url_rejects_unsafe_url() {
        let server = MockServer::start().await;
        // No mock mounted: any request would fail the test via expect(0)-style
        // reasoning — instead assert the error comes back before any HTTP.
        let adapter = mock_server_adapter(&server.uri());
        let err = adapter
            .send_photo_url("12345", "http://169.254.169.254/latest/meta-data", None)
            .await
            .expect_err("private IP URL must be rejected");
        assert!(err.to_string().contains("Unsafe photo URL"));
        let requests = server.received_requests().await.expect("requests");
        assert!(requests.is_empty(), "unsafe URL must never be requested");
    }

    #[tokio::test]
    async fn send_photo_url_rejects_non_http_scheme() {
        let server = MockServer::start().await;
        let adapter = mock_server_adapter(&server.uri());
        adapter
            .send_photo_url("12345", "ftp://example.com/pic.jpg", None)
            .await
            .expect_err("non-http scheme must be rejected");
        let requests = server.received_requests().await.expect("requests");
        assert!(requests.is_empty());
    }

    #[tokio::test]
    async fn send_photo_url_posts_photo_field() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sendPhoto"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "message_id": 99 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let adapter = mock_server_adapter(&server.uri());
        let id = adapter
            .send_photo_url("12345", "https://example.com/pic.jpg", Some("nice pic"))
            .await
            .expect("send ok");
        assert_eq!(id, 99);

        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json body");
        assert_eq!(
            body["photo"],
            serde_json::json!("https://example.com/pic.jpg")
        );
        assert_eq!(body["caption"], serde_json::json!("nice pic"));
    }

    #[tokio::test]
    async fn send_voice_smart_missing_file_errors() {
        let server = MockServer::start().await;
        let adapter = mock_server_adapter(&server.uri());
        adapter
            .send_voice_smart("12345", "/nonexistent/path/voice.ogg", None, true)
            .await
            .expect_err("missing file must error, not hang");
    }

    #[tokio::test]
    async fn send_video_smart_missing_file_errors() {
        let server = MockServer::start().await;
        let adapter = mock_server_adapter(&server.uri());
        adapter
            .send_video_smart("12345", "/nonexistent/path/video.mp4", None)
            .await
            .expect_err("missing file must error, not hang");
    }

    #[test]
    fn transcode_with_unusable_ffmpeg_returns_none() {
        // An ffmpeg binary that can't be executed must degrade gracefully
        // (no panic, no temp file left behind) — no PATH mutation needed,
        // so this stays thread-safe alongside the other tests.
        let tmp = std::env::temp_dir().join("tg_test_voice_src.tmp");
        std::fs::write(&tmp, b"not real audio").unwrap();
        let result = transcode_with_ffmpeg(Path::new("/nonexistent-dir-xyz/ffmpeg"), &tmp);
        let _ = std::fs::remove_file(&tmp);
        assert!(result.is_none(), "unusable ffmpeg → no transcode, no panic");
    }
}
