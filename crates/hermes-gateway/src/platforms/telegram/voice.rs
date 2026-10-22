//! Telegram adapter — inbound voice note transcription.
//!
//! Voice notes are downloaded through the shared [`MediaDownloader`] cache
//! and transcribed with the Whisper-backed [`TranscriptionHandler`]. Every
//! failure mode degrades to a placeholder string instead of an error, so
//! the text message flow never breaks because transcription failed.

use std::path::Path;

use hermes_core::ToolHandler;
use hermes_tools::tools::transcription::TranscriptionHandler;
use tracing::{debug, warn};

use super::media::MediaDownloader;

/// Placeholder surfaced when STT is disabled for voice notes.
const VOICE_PLACEHOLDER: &str = "[Voice message]";
/// Placeholder surfaced when transcription was attempted but failed.
const VOICE_UNAVAILABLE: &str = "[Voice message - transcription unavailable]";

/// Transcribe a voice note. Returns the display text for the message.
///
/// - Downloads via `downloader` (respects its cache).
/// - If `stt_enabled` is false → returns `"[Voice message]"` (no download,
///   no transcription attempted).
/// - If the downloader is disabled → returns the "unavailable" placeholder.
/// - On any failure (download error, API error, missing key, unparseable
///   output) → returns `"[Voice message - transcription unavailable]"`.
/// - On success → `"[Voice]: {transcript}"`.
///
/// Never returns `Err` — graceful degradation is the whole point: the text
/// flow must never break because transcription failed.
pub(crate) async fn transcribe_voice_note(
    downloader: &MediaDownloader,
    file_id: &str,
    file_unique_id: &str,
    stt_enabled: bool,
) -> String {
    if !stt_enabled {
        debug!("voice transcription disabled (stt_enabled=false); using placeholder");
        return VOICE_PLACEHOLDER.to_string();
    }
    if !downloader.enabled() {
        debug!("media downloader disabled; voice transcription unavailable");
        return VOICE_UNAVAILABLE.to_string();
    }
    let path = match downloader.download_file(file_id, file_unique_id).await {
        Ok(path) => path,
        Err(e) => {
            warn!("voice note download failed: {e}");
            return VOICE_UNAVAILABLE.to_string();
        }
    };
    match transcribe_audio_file(&path).await {
        Some(text) => format!("[Voice]: {text}"),
        None => VOICE_UNAVAILABLE.to_string(),
    }
}

/// Transcribe an already-downloaded audio file via the Whisper-backed
/// [`TranscriptionHandler`].
///
/// Returns `None` on *any* failure (missing API key, unreadable file, API
/// error, unparseable output) — callers degrade to a placeholder string.
///
/// This is the unit-test seam: it takes a plain path, so tests can pass a
/// bogus path and assert the graceful `None` without touching the network
/// or the media downloader.
async fn transcribe_audio_file(path: &Path) -> Option<String> {
    let handler = TranscriptionHandler;
    let params = serde_json::json!({ "audio_path": path.to_string_lossy() });
    match handler.execute(params).await {
        Ok(output) => extract_transcript(&output),
        Err(e) => {
            debug!("voice transcription failed for {}: {e}", path.display());
            None
        }
    }
}

/// Extract the transcript text from the [`TranscriptionHandler`] output.
///
/// The handler returns a JSON envelope of the form
/// `{"audio_path": ..., "text": ..., "transport": ..., "status": ...}` —
/// `text` is the primary source. If the output is not JSON at all (a future
/// handler revision might return plain text), the trimmed raw string is
/// used as a fallback. Empty transcripts degrade to `None`.
fn extract_transcript(output: &str) -> Option<String> {
    match serde_json::from_str::<serde_json::Value>(output) {
        Ok(value) => {
            let text = value.get("text").and_then(|t| t.as_str()).unwrap_or("");
            non_empty(text)
        }
        Err(_) => non_empty(output),
    }
}

/// The trimmed string as `Some`, or `None` when it is empty.
fn non_empty(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::TelegramConfig;
    use super::*;

    /// Build a downloader against a bogus token. The stub `MediaDownloader`
    /// (replaced 1:1 by Worker 1's full implementation) fails downloads
    /// fast without touching the network, which is exactly the degradation
    /// path these tests exercise.
    fn test_downloader() -> MediaDownloader {
        let config = TelegramConfig {
            token: "bogus-token".to_string(),
            webhook_url: None,
            polling: true,
            proxy: Default::default(),
            parse_markdown: false,
            parse_html: false,
            poll_timeout: 30,
            bot_username: None,
            allowed_users: None,
            group_require_mention: false,
            command_menu_max: None,
            message_reactions: false,
            command_menu_skills: true,
            media_cache_dir: None,
            media_max_mb: 20,
            media_enabled: true,
            stt_enabled: true,
        };
        MediaDownloader::new(&config, &reqwest::Client::new(), "bogus-token")
    }

    #[tokio::test]
    async fn stt_disabled_returns_placeholder_without_download() {
        let downloader = test_downloader();
        // Bogus file ids: if a download were attempted, the stub would fail
        // and we would get the "unavailable" string instead of the plain
        // placeholder — so this also proves the short-circuit.
        let text =
            transcribe_voice_note(&downloader, "bogus-file-id", "bogus-unique-id", false).await;
        assert_eq!(text, "[Voice message]");
    }

    #[tokio::test]
    async fn download_failure_degrades_to_unavailable_placeholder() {
        let downloader = test_downloader();
        let text =
            transcribe_voice_note(&downloader, "bogus-file-id", "bogus-unique-id", true).await;
        assert_eq!(text, "[Voice message - transcription unavailable]");
    }

    #[tokio::test]
    async fn transcribe_audio_file_returns_none_for_missing_file() {
        // Deterministic without network: with no API key the handler errors
        // before reading; with a key set it fails reading the nonexistent
        // path. Either way the seam yields None.
        let result = transcribe_audio_file(Path::new("/nonexistent/voice-note.ogg")).await;
        assert!(result.is_none());
    }

    #[test]
    fn extract_transcript_reads_text_field() {
        let output = r#"{"audio_path":"/tmp/a.ogg","text":"hello world","transport":"direct","status":"transcribed"}"#;
        assert_eq!(extract_transcript(output).as_deref(), Some("hello world"));
    }

    #[test]
    fn extract_transcript_rejects_empty_text() {
        let output = r#"{"audio_path":"/tmp/a.ogg","text":"   ","transport":"direct","status":"transcribed"}"#;
        assert_eq!(extract_transcript(output), None);
    }

    #[test]
    fn extract_transcript_falls_back_to_raw_string() {
        assert_eq!(
            extract_transcript("plain transcript").as_deref(),
            Some("plain transcript")
        );
        assert_eq!(extract_transcript("   "), None);
    }
}
