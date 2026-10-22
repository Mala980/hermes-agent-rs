//! `hermes voice` — text-to-speech (`say`) and speech-to-text (`listen`).
//!
//! Ported from hermes-rs. The fork has no `hermes_tools::media`, so:
//! - TTS goes through `TextToSpeechHandler` + `MultiTtsBackend` directly
//!   (ElevenLabs / OpenAI / MiniMax; keys from env, or the Nous-managed
//!   `openai-audio` tool gateway).
//! - STT goes through `TranscriptionHandler` directly (OpenAI Whisper,
//!   `VOICE_TOOLS_OPENAI_KEY` / `OPENAI_API_KEY`, or the managed gateway).
//!
//! NOTE: `voice_mode::transcribe_audio` takes raw f32 PCM samples, not a file,
//! so the file-based `TranscriptionHandler` is the right fit here. It does not
//! forward the `--language` hint (Whisper auto-detects); a warning is logged.

use std::sync::Arc;

use hermes_cli::cli::{Cli, VoiceCmd};
use hermes_config::{resolve_managed_tool_gateway, resolve_openai_audio_api_key, ResolveOptions};
use hermes_core::{AgentError, ToolHandler};
use hermes_tools::backends::tts::MultiTtsBackend;
use hermes_tools::tools::transcription::TranscriptionHandler;
use hermes_tools::tools::tts::TextToSpeechHandler;
use serde_json::json;

/// Dispatch `hermes voice <say|listen>`.
pub(crate) async fn cmd_voice(_cli: &Cli, cmd: &VoiceCmd) -> Result<(), AgentError> {
    match cmd {
        VoiceCmd::Say { text, out, voice } => say(text, out.as_deref(), voice.as_deref()).await,
        VoiceCmd::Listen { file, language } => listen(file, language.as_deref()).await,
    }
}

/// Synthesize `text` and write an MP3 to `out` (default `speech-<unix_ts>.mp3`).
async fn say(text: &str, out: Option<&str>, voice: Option<&str>) -> Result<(), AgentError> {
    check_tts_available()?;

    let handler = TextToSpeechHandler::new(Arc::new(MultiTtsBackend::new()));
    let result = handler
        .execute(json!({ "text": text, "voice": voice }))
        .await?;

    // The backend returns JSON `{"provider","file","voice","bytes"}` where
    // `file` is a temp-dir MP3; copy it to the requested destination.
    let tmp_path = parse_json_field(&result, "file")
        .ok_or_else(|| AgentError::Config(format!("unexpected TTS backend response: {result}")))?;
    let dest = resolve_output_path(out);
    tokio::fs::copy(&tmp_path, &dest)
        .await
        .map_err(|e| AgentError::Io(format!("writing {dest}: {e}")))?;
    let bytes = tokio::fs::metadata(&dest)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    println!("Audio saved: {dest} ({bytes} bytes)");
    Ok(())
}

/// Transcribe the audio file at `file` and print the text.
async fn listen(file: &str, language: Option<&str>) -> Result<(), AgentError> {
    check_stt_available()?;

    if let Some(lang) = language {
        // The fork's Whisper tool does not take a language hint.
        tracing::warn!("--language {lang} ignored: the Whisper tool auto-detects the language");
    }

    let handler = TranscriptionHandler;
    let result = handler.execute(json!({ "audio_path": file })).await?;

    let text = parse_json_field(&result, "text").ok_or_else(|| {
        AgentError::Config(format!("unexpected transcription response: {result}"))
    })?;
    println!("{text}");
    Ok(())
}

/// Fail fast with a clear message when no TTS backend can be used.
fn check_tts_available() -> Result<(), AgentError> {
    let direct_key = ["ELEVENLABS_API_KEY", "MINIMAX_API_KEY"]
        .iter()
        .any(|k| env_key_set(k))
        || !resolve_openai_audio_api_key().is_empty();
    let managed = resolve_managed_tool_gateway("openai-audio", ResolveOptions::default()).is_some();
    if direct_key || managed {
        return Ok(());
    }
    Err(AgentError::Config(
        "no TTS backend configured. Set one of ELEVENLABS_API_KEY, OPENAI_API_KEY \
         (or VOICE_TOOLS_OPENAI_KEY), MINIMAX_API_KEY, or enable the Nous-managed \
         openai-audio tool gateway (HERMES_ENABLE_NOUS_MANAGED_TOOLS)."
            .into(),
    ))
}

/// Fail fast with a clear message when Whisper transcription cannot run.
fn check_stt_available() -> Result<(), AgentError> {
    let direct_key = !resolve_openai_audio_api_key().is_empty();
    let managed = resolve_managed_tool_gateway("openai-audio", ResolveOptions::default()).is_some();
    if direct_key || managed {
        return Ok(());
    }
    Err(AgentError::Config(
        "Whisper transcription requires VOICE_TOOLS_OPENAI_KEY or OPENAI_API_KEY, \
         or a Nous-managed openai-audio tool gateway (HERMES_ENABLE_NOUS_MANAGED_TOOLS)."
            .into(),
    ))
}

/// True when the env var is set to a non-blank value.
fn env_key_set(key: &str) -> bool {
    std::env::var(key)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// Default output path when `--out` is not given: `speech-<unix_ts>.mp3`.
fn default_speech_path() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("speech-{ts}.mp3")
}

/// Honor an explicit `--out`; fall back to the timestamped default.
fn resolve_output_path(out: Option<&str>) -> String {
    match out {
        Some(p) if !p.trim().is_empty() => p.to_string(),
        _ => default_speech_path(),
    }
}

/// Extract a string field from a JSON object string (e.g. handler results).
fn parse_json_field(json_str: &str, field: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(json_str)
        .ok()?
        .get(field)?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_speech_path_has_expected_shape() {
        let p = default_speech_path();
        assert!(p.starts_with("speech-"), "got {p}");
        assert!(p.ends_with(".mp3"), "got {p}");
        let mid = &p["speech-".len()..p.len() - ".mp3".len()];
        assert!(
            mid.parse::<u64>().is_ok(),
            "timestamp part not numeric: {p}"
        );
    }

    #[test]
    fn resolve_output_path_honors_explicit_out() {
        assert_eq!(resolve_output_path(Some("hello.mp3")), "hello.mp3");
    }

    #[test]
    fn resolve_output_path_defaults_on_empty() {
        assert_eq!(resolve_output_path(None), default_speech_path());
        assert_eq!(
            resolve_output_path(Some("   ")),
            default_speech_path(),
            "blank --out should not produce a blank path"
        );
    }

    #[test]
    fn parse_json_field_extracts_string_fields() {
        let s = r#"{"file":"/tmp/x.mp3","bytes":123}"#;
        assert_eq!(parse_json_field(s, "file"), Some("/tmp/x.mp3".to_string()));
        assert_eq!(parse_json_field(s, "bytes"), None, "non-string field");
        assert_eq!(parse_json_field(s, "missing"), None);
        assert_eq!(parse_json_field("not json", "file"), None);
    }
}
