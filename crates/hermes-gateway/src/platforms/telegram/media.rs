//! Telegram inbound media pipeline: `getFile` download + local cache (M-M1).
//!
//! [`MediaDownloader`] resolves Telegram file ids to local paths, caching
//! downloads by `file_unique_id` under the configured media cache dir.
//! Files larger than `media_max_mb` are rejected ("too large") — checked
//! against `Content-Length` before the download and again on the streamed
//! byte count so an oversized file can never fill memory or disk.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, warn};

use hermes_core::errors::GatewayError;
use hermes_core::ToolHandler;

use hermes_tools::backends::vision::OpenAiVisionBackend;
use hermes_tools::tools::vision::VisionAnalyzeHandler;

use super::error::{classify_telegram_error, redact_token, redact_url};
use super::types::{IncomingMessage, TelegramConfig, TelegramResponse, Update};
use super::voice::transcribe_voice_note;
use super::TelegramAdapter;

/// `getFile` response payload (`result` field of [`TelegramResponse`]).
#[derive(Debug, Deserialize)]
struct GetFileResult {
    #[serde(default)]
    file_path: Option<String>,
}

/// Resolve the media cache directory: the `media_cache_dir` config
/// override, else `{hermes_home}/media_cache`, else
/// `{temp_dir}/hermes-media-cache`. `hermes_home()` falls back to `"."`
/// when no home is discoverable, which is when the temp-dir fallback
/// kicks in.
pub(crate) fn resolve_cache_dir(config: &TelegramConfig) -> PathBuf {
    if let Some(dir) = &config.media_cache_dir {
        return dir.clone();
    }
    let home = hermes_config::paths::hermes_home();
    if home.as_os_str().is_empty() || home == Path::new(".") {
        std::env::temp_dir().join("hermes-media-cache")
    } else {
        home.join("media_cache")
    }
}

/// Guard against path traversal via an untrusted `file_unique_id`.
fn checked_cache_path(cache_dir: &Path, file_unique_id: &str) -> Result<PathBuf, GatewayError> {
    if file_unique_id.is_empty()
        || file_unique_id == "."
        || file_unique_id == ".."
        || file_unique_id.contains('/')
        || file_unique_id.contains('\\')
    {
        return Err(GatewayError::Platform(format!(
            "telegram media: refusing to cache unsafe file_unique_id {file_unique_id:?}"
        )));
    }
    Ok(cache_dir.join(file_unique_id))
}

/// Downloads Telegram files (`getFile`) into a local cache, keyed by
/// `file_unique_id`.
///
/// Built in [`TelegramAdapter::new`](super::TelegramAdapter::new) from the
/// adapter config; inbound media processing workers reach it via
/// [`media_downloader()`](super::TelegramAdapter::media_downloader).
pub(crate) struct MediaDownloader {
    client: reqwest::Client,
    token: String,
    /// Base URL for Bot API JSON calls (`.../bot<token>`).
    api_base: String,
    /// Base URL for file downloads (`.../file/bot<token>`).
    file_base: String,
    cache_dir: PathBuf,
    max_bytes: u64,
    enabled: bool,
}

impl MediaDownloader {
    /// Build from the adapter config, sharing the adapter's reqwest client.
    ///
    /// Cache dir resolution: [`resolve_cache_dir`]; the dir is created
    /// best-effort — failure is logged at warn, never fatal.
    pub(crate) fn new(config: &TelegramConfig, client: &reqwest::Client, token: &str) -> Self {
        let api_base = format!("https://api.telegram.org/bot{token}");
        let file_base = format!("https://api.telegram.org/file/bot{token}");
        Self::build(config, client, token, api_base, file_base)
    }

    /// Test-only constructor pointing the API and file endpoints at a mock
    /// server instead of `api.telegram.org`.
    #[cfg(test)]
    pub(crate) fn new_with_base_urls(
        config: &TelegramConfig,
        client: &reqwest::Client,
        token: &str,
        api_base: &str,
        file_base: &str,
    ) -> Self {
        Self::build(
            config,
            client,
            token,
            api_base.to_string(),
            file_base.to_string(),
        )
    }

    fn build(
        config: &TelegramConfig,
        client: &reqwest::Client,
        token: &str,
        api_base: String,
        file_base: String,
    ) -> Self {
        let cache_dir = resolve_cache_dir(config);
        if let Err(e) = std::fs::create_dir_all(&cache_dir) {
            warn!(
                dir = %cache_dir.display(),
                error = %e,
                "telegram media: failed to create cache dir (continuing best-effort)"
            );
        }
        Self {
            client: client.clone(),
            token: token.to_string(),
            api_base,
            file_base,
            cache_dir,
            max_bytes: (config.media_max_mb as u64) * 1024 * 1024,
            enabled: config.media_enabled,
        }
    }

    /// Whether inbound media processing is enabled (`media_enabled`).
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Configured size cap in bytes (`media_max_mb`).
    pub(crate) fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// The resolved cache directory.
    pub(crate) fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Local cache path for a Telegram `file_unique_id`.
    pub(crate) fn cache_path_for(&self, file_unique_id: &str) -> PathBuf {
        self.cache_dir.join(file_unique_id)
    }

    /// Download a Telegram file by `file_id`, caching by `file_unique_id`.
    ///
    /// Steps: POST `{api_base}/getFile` → parse `result.file_path` → GET
    /// `{file_base}/{file_path}` → check `Content-Length` and the streamed
    /// byte count against the configured cap → write to
    /// `{cache_dir}/{file_unique_id}` → return the path. An already-cached
    /// file returns immediately without any network call.
    pub(crate) async fn download_file(
        &self,
        file_id: &str,
        file_unique_id: &str,
    ) -> Result<PathBuf, GatewayError> {
        if !self.enabled {
            return Err(GatewayError::Platform(
                "telegram media: inbound media processing is disabled (media_enabled=false)".into(),
            ));
        }
        let dest = checked_cache_path(&self.cache_dir, file_unique_id)?;
        if dest.exists() {
            debug!(
                file_id = %file_id,
                path = %dest.display(),
                "telegram media: cache hit, skipping download"
            );
            return Ok(dest);
        }

        let file_path = self.get_file_path(file_id).await?;

        let url = format!("{}/{}", self.file_base.trim_end_matches('/'), file_path);
        debug!(url = %redact_url(&url), "telegram media: downloading file");
        let resp = self.client.get(&url).send().await.map_err(|e| {
            GatewayError::ConnectionFailed(format!(
                "telegram media: file download request failed: {}",
                redact_token(&e.to_string(), &self.token)
            ))
        })?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let parsed = serde_json::from_str::<TelegramResponse<serde_json::Value>>(&text).ok();
            return Err(classify_telegram_error(
                status.as_u16(),
                parsed.as_ref().and_then(|p| p.error_code),
                &parsed
                    .as_ref()
                    .and_then(|p| p.description.clone())
                    .unwrap_or(text),
            ));
        }

        // Fail fast on a declared Content-Length over the cap, then enforce
        // the cap again on the streamed bytes (a lying or missing
        // Content-Length must not let an oversized file through).
        if let Some(len) = resp.content_length() {
            self.check_size(len, "declared Content-Length")?;
        }
        let mut downloaded: u64 = 0;
        let mut buf: Vec<u8> = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                GatewayError::ConnectionFailed(format!(
                    "telegram media: file download interrupted: {}",
                    redact_token(&e.to_string(), &self.token)
                ))
            })?;
            downloaded = downloaded.saturating_add(chunk.len() as u64);
            self.check_size(downloaded, "downloaded bytes")?;
            buf.extend_from_slice(&chunk);
        }

        tokio::fs::write(&dest, &buf).await.map_err(|e| {
            GatewayError::Platform(format!(
                "telegram media: failed to write cache file {}: {e}",
                dest.display()
            ))
        })?;
        debug!(
            path = %dest.display(),
            bytes = downloaded,
            "telegram media: file cached"
        );
        Ok(dest)
    }

    /// Enforce the configured size cap. Pure and unit-testable.
    fn check_size(&self, len: u64, what: &str) -> Result<(), GatewayError> {
        if len > self.max_bytes {
            return Err(GatewayError::Platform(format!(
                "telegram media: file too large ({} = {len} bytes exceeds {} MB cap)",
                what,
                self.max_bytes / (1024 * 1024)
            )));
        }
        Ok(())
    }

    /// Call `getFile` and return the file's `file_path`. API errors go
    /// through [`classify_telegram_error`] so 401/403/409/429 keep the
    /// adapter's standard semantics.
    async fn get_file_path(&self, file_id: &str) -> Result<String, GatewayError> {
        let url = format!("{}/getFile", self.api_base);
        let redacted_url = redact_url(&url);
        let body = serde_json::json!({ "file_id": file_id });
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                GatewayError::ConnectionFailed(format!(
                    "telegram getFile request failed at {redacted_url}: {}",
                    redact_token(&e.to_string(), &self.token)
                ))
            })?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let parsed: TelegramResponse<GetFileResult> = serde_json::from_str(&text).map_err(|e| {
            GatewayError::Platform(format!(
                "telegram getFile: invalid response from {redacted_url}: {}",
                redact_token(&e.to_string(), &self.token)
            ))
        })?;
        if !status.is_success() || !parsed.ok {
            return Err(classify_telegram_error(
                status.as_u16(),
                parsed.error_code,
                parsed.description.as_deref().unwrap_or(""),
            ));
        }
        parsed
            .result
            .and_then(|r| r.file_path)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                GatewayError::Platform(format!(
                    "telegram getFile: no file_path in response for file_id {file_id}"
                ))
            })
    }
}

// ---------------------------------------------------------------------------
// describe_* helpers — photo/document/sticker → agent-readable text (C7)
// ---------------------------------------------------------------------------

/// Describe a photo for the agent.
///
/// Downloads the largest photo, then:
/// - if a vision backend is available → vision description (see
///   [`describe_image_with_vision`]);
/// - otherwise → the caption if present, else `"[Photo received]"`.
pub(crate) async fn describe_photo(
    downloader: &MediaDownloader,
    file_id: &str,
    file_unique_id: &str,
    caption: Option<&str>,
) -> String {
    let fallback = caption
        .filter(|c| !c.is_empty())
        .unwrap_or("[Photo received]")
        .to_string();
    let path = match downloader.download_file(file_id, file_unique_id).await {
        Ok(p) => p,
        Err(e) => {
            debug!("Telegram: photo download failed ({e}); using caption fallback");
            return fallback;
        }
    };
    describe_image_with_vision(&path).await.unwrap_or(fallback)
}

/// Run the vision backend over a local image file.
///
/// This is a REAL wiring, not a stub: [`OpenAiVisionBackend`] base64-encodes
/// local file paths (`encode_image_if_local`) and calls an OpenAI-compatible
/// `/chat/completions` endpoint, so no public image URL is needed. It is
/// only attempted when `OPENAI_API_KEY` is set (via `from_env`); otherwise
/// `None` is returned and the caller falls back to the caption. Vision
/// failures are logged at debug and also degrade to the fallback — never an
/// error to the agent.
async fn describe_image_with_vision(path: &Path) -> Option<String> {
    let backend = match OpenAiVisionBackend::from_env() {
        Ok(b) => b,
        Err(_) => {
            debug!(
                "Telegram: vision backend unavailable (OPENAI_API_KEY not set); \
                 skipping image description"
            );
            return None;
        }
    };
    let handler = VisionAnalyzeHandler::new(Arc::new(backend));
    match handler
        .execute(json!({
            "image_url": path.to_string_lossy(),
            "question": "Describe this image briefly.",
        }))
        .await
    {
        Ok(desc) => {
            let desc = desc.trim().to_string();
            if desc.is_empty() {
                debug!("Telegram: vision backend returned an empty description");
                None
            } else {
                Some(desc)
            }
        }
        Err(e) => {
            debug!("Telegram: vision analysis failed: {e}");
            None
        }
    }
}

/// Handle a document: download it; if it is at most 100 KiB and looks like
/// text (valid UTF-8 without NUL bytes), return the first ~2000 chars as
/// text; otherwise return `"[Document: {filename}]"`.
pub(crate) async fn describe_document(
    downloader: &MediaDownloader,
    file_id: &str,
    file_unique_id: &str,
    file_name: Option<&str>,
) -> String {
    let label = file_name.unwrap_or("unnamed");
    let bytes = match downloader.download_file(file_id, file_unique_id).await {
        Ok(path) => match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) => {
                warn!("Telegram: failed to read cached document {label}: {e}");
                return format!("[Document: {label}]");
            }
        },
        Err(e) => {
            debug!("Telegram: document download failed ({e})");
            return format!("[Document: {label}]");
        }
    };
    const MAX_INLINE_BYTES: usize = 100 * 1024;
    const MAX_INLINE_CHARS: usize = 2000;
    if bytes.len() <= MAX_INLINE_BYTES {
        if let Some(text) = extract_document_text(&bytes, MAX_INLINE_CHARS) {
            return format!("[Document: {label}]\n{text}");
        }
    }
    format!("[Document: {label}]")
}

/// Pure text-detection helper for [`describe_document`].
///
/// Returns the first `max_chars` characters when `bytes` are valid UTF-8
/// text. `None` for binary data: invalid UTF-8, or UTF-8 containing NUL
/// bytes (a cheap binary sniff — text files don't embed NULs).
pub(crate) fn extract_document_text(bytes: &[u8], max_chars: usize) -> Option<String> {
    let s = std::str::from_utf8(bytes).ok()?;
    if s.contains('\0') {
        return None;
    }
    Some(s.chars().take(max_chars).collect())
}

/// Stickers need no download: `"[Sticker]"`, or `"[Sticker 😀]"` when the
/// sticker's emoji is known.
pub(crate) fn describe_sticker(emoji: Option<&str>) -> String {
    match emoji.filter(|e| !e.is_empty()) {
        Some(e) => format!("[Sticker {e}]"),
        None => "[Sticker]".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Enrichment entry point
// ---------------------------------------------------------------------------

/// Enrich an inbound media message in place (C7).
///
/// Called from
/// [`TelegramAdapter::process_update`](super::TelegramAdapter::process_update)
/// after the allowlist and group-trigger gates pass and before the message
/// is forwarded. Voice notes are transcribed, photos are described (vision
/// or caption), documents are text-extracted, and stickers become an emoji
/// placeholder.
///
/// Best-effort by design: every step degrades to a placeholder on missing
/// data or backend failure, so the text flow can never break or panic.
pub(crate) async fn enrich_media_message(
    adapter: &TelegramAdapter,
    parsed: &mut IncomingMessage,
    update: &Update,
) {
    let downloader = adapter.media_downloader();
    if !downloader.enabled() {
        return;
    }

    // Voice → transcription (STT).
    if parsed.is_voice {
        if let (Some(file_id), Some(unique_id)) = (
            parsed.voice_file_id.as_deref(),
            parsed.voice_file_unique_id.as_deref(),
        ) {
            parsed.text = Some(
                transcribe_voice_note(downloader, file_id, unique_id, adapter.config.stt_enabled)
                    .await,
            );
        }
    }

    // Photo → vision description (or caption fallback).
    if parsed.is_photo {
        if let (Some(file_id), Some(unique_id)) = (
            parsed.photo_file_id.as_deref(),
            parsed.photo_file_unique_id.as_deref(),
        ) {
            let caption = parsed.text.clone();
            let desc = describe_photo(downloader, file_id, unique_id, caption.as_deref()).await;
            parsed.text = Some(match caption.filter(|c| !c.is_empty()) {
                // describe_photo fell back to the caption itself: don't
                // repeat it.
                Some(cap) if desc == cap => format!("[Photo]: {desc}"),
                Some(cap) => format!("[Photo]: {desc}\nCaption: {cap}"),
                None => format!("[Photo]: {desc}"),
            });
        }
    }

    // Document → text extract (or filename placeholder).
    if parsed.is_document {
        if let (Some(file_id), Some(unique_id)) = (
            parsed.document_file_id.as_deref(),
            parsed.document_file_unique_id.as_deref(),
        ) {
            parsed.text = Some(
                describe_document(
                    downloader,
                    file_id,
                    unique_id,
                    parsed.document_file_name.as_deref(),
                )
                .await,
            );
        }
    }

    // Sticker → emoji placeholder (no download needed).
    if parsed.is_sticker {
        let emoji = update
            .message
            .as_ref()
            .or(update.channel_post.as_ref())
            .and_then(|m| m.sticker.as_ref())
            .and_then(|s| s.emoji.as_deref());
        parsed.text = Some(describe_sticker(emoji));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platforms::telegram::types::DEFAULT_MEDIA_MAX_MB;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_config(cache_dir: Option<PathBuf>, max_mb: usize, enabled: bool) -> TelegramConfig {
        TelegramConfig {
            token: "test-token-abc".into(),
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
            media_cache_dir: cache_dir,
            media_max_mb: max_mb,
            media_enabled: enabled,
            stt_enabled: true,
        }
    }

    fn downloader_for_mock(
        server: &MockServer,
        cache_dir: PathBuf,
        max_mb: usize,
        enabled: bool,
    ) -> MediaDownloader {
        let config = test_config(Some(cache_dir), max_mb, enabled);
        let client = reqwest::Client::new();
        MediaDownloader::new_with_base_urls(
            &config,
            &client,
            "test-token-abc",
            &server.uri(),
            &format!("{}/file/bottest-token-abc", server.uri()),
        )
    }

    #[test]
    fn resolve_cache_dir_override_wins() {
        let cfg = test_config(Some(PathBuf::from("/tmp/custom-tg-media")), 20, true);
        assert_eq!(
            resolve_cache_dir(&cfg),
            PathBuf::from("/tmp/custom-tg-media")
        );
    }

    #[test]
    fn resolve_cache_dir_defaults_below_hermes_home() {
        let cfg = test_config(None, 20, true);
        let dir = resolve_cache_dir(&cfg);
        let home = hermes_config::paths::hermes_home();
        if home == Path::new(".") {
            assert_eq!(dir, std::env::temp_dir().join("hermes-media-cache"));
        } else {
            assert_eq!(dir, home.join("media_cache"));
        }
    }

    #[test]
    fn enabled_reflects_config() {
        let tmp = tempfile::tempdir().unwrap();
        let on = MediaDownloader::new(
            &test_config(Some(tmp.path().into()), 20, true),
            &reqwest::Client::new(),
            "tok",
        );
        assert!(on.enabled());
        assert_eq!(on.max_bytes(), 20 * 1024 * 1024);
        assert_eq!(on.cache_dir(), tmp.path());

        let off = MediaDownloader::new(
            &test_config(Some(tmp.path().into()), 20, false),
            &reqwest::Client::new(),
            "tok",
        );
        assert!(!off.enabled());
    }

    #[test]
    fn check_size_rejects_over_cap() {
        let tmp = tempfile::tempdir().unwrap();
        // media_max_mb = 0 → max_bytes = 0: any byte is too large.
        let dl = MediaDownloader::new(
            &test_config(Some(tmp.path().into()), 0, true),
            &reqwest::Client::new(),
            "tok",
        );
        let err = dl.check_size(1, "downloaded bytes").unwrap_err();
        match &err {
            GatewayError::Platform(msg) => assert!(msg.contains("too large"), "msg: {msg}"),
            other => panic!("expected Platform, got {other:?}"),
        }
        // Exactly at the cap is fine.
        assert!(dl.check_size(0, "downloaded bytes").is_ok());

        let big = MediaDownloader::new(
            &test_config(Some(tmp.path().into()), DEFAULT_MEDIA_MAX_MB, true),
            &reqwest::Client::new(),
            "tok",
        );
        assert!(big.check_size(20 * 1024 * 1024, "x").is_ok());
        assert!(big.check_size(20 * 1024 * 1024 + 1, "x").is_err());
    }

    #[test]
    fn checked_cache_path_rejects_traversal() {
        let dir = Path::new("/tmp/cache");
        assert!(checked_cache_path(dir, "uniq_123").is_ok());
        for bad in ["", ".", "..", "a/b", "a\\b", "../evil"] {
            assert!(
                checked_cache_path(dir, bad).is_err(),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn get_file_result_deserialization() {
        let json = r#"{"ok":true,"result":{"file_id":"f1","file_unique_id":"u1","file_path":"docs/small.bin"}}"#;
        let resp: TelegramResponse<GetFileResult> = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert_eq!(
            resp.result.unwrap().file_path.as_deref(),
            Some("docs/small.bin")
        );

        // Missing file_path (file not ready / deleted).
        let json = r#"{"ok":true,"result":{"file_id":"f1","file_unique_id":"u1"}}"#;
        let resp: TelegramResponse<GetFileResult> = serde_json::from_str(json).unwrap();
        assert!(resp.result.unwrap().file_path.is_none());
    }

    #[tokio::test]
    async fn download_file_cache_hit_skips_network() {
        let tmp = tempfile::tempdir().unwrap();
        let cached = tmp.path().join("uniq_cached");
        std::fs::write(&cached, b"already here").unwrap();

        // Unreachable API base: any network attempt would fail, proving the
        // cache-hit path does no I/O.
        let config = test_config(Some(tmp.path().into()), 20, true);
        let dl = MediaDownloader::new_with_base_urls(
            &config,
            &reqwest::Client::new(),
            "tok",
            "http://127.0.0.1:1",
            "http://127.0.0.1:1",
        );
        let path = dl.download_file("file_id_1", "uniq_cached").await.unwrap();
        assert_eq!(path, cached);
        assert_eq!(std::fs::read(&path).unwrap(), b"already here");
    }

    #[tokio::test]
    async fn download_file_disabled_errors_without_network() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(Some(tmp.path().into()), 20, false);
        let dl = MediaDownloader::new_with_base_urls(
            &config,
            &reqwest::Client::new(),
            "tok",
            "http://127.0.0.1:1",
            "http://127.0.0.1:1",
        );
        let err = dl.download_file("file_id_1", "uniq_x").await.unwrap_err();
        match &err {
            GatewayError::Platform(msg) => assert!(msg.contains("disabled"), "msg: {msg}"),
            other => panic!("expected Platform, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn download_file_rejects_oversize_content_length() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/getFile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {
                    "file_id": "f_big",
                    "file_unique_id": "u_big",
                    "file_path": "docs/big.bin"
                }
            })))
            .mount(&server)
            .await;

        // 2048-byte body declared via Content-Length; media_max_mb = 0
        // rejects it in the pre-download size check.
        let body = vec![0xABu8; 2048];
        Mock::given(method("GET"))
            .and(path("/file/bottest-token-abc/docs/big.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let dl = downloader_for_mock(&server, tmp.path().into(), 0, true);
        let err = dl.download_file("f_big", "u_big").await.unwrap_err();
        match &err {
            GatewayError::Platform(msg) => assert!(msg.contains("too large"), "msg: {msg}"),
            other => panic!("expected Platform, got {other:?}"),
        }
        // Nothing was written to the cache.
        assert!(!tmp.path().join("u_big").exists());
    }

    #[tokio::test]
    async fn download_file_caches_and_returns_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/getFile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {
                    "file_id": "f_small",
                    "file_unique_id": "u_small",
                    "file_path": "voice/small.ogg"
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/file/bottest-token-abc/voice/small.ogg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"fake-ogg-data".to_vec()))
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let dl = downloader_for_mock(&server, tmp.path().into(), 20, true);

        let path = dl.download_file("f_small", "u_small").await.unwrap();
        assert_eq!(path, tmp.path().join("u_small"));
        assert_eq!(std::fs::read(&path).unwrap(), b"fake-ogg-data");

        // Second call is a cache hit (no getFile re-request needed).
        let path2 = dl.download_file("f_small", "u_small").await.unwrap();
        assert_eq!(path2, path);
    }

    #[tokio::test]
    async fn download_file_getfile_error_classified() {
        let server = MockServer::start().await;
        // 400 with an ok:false body → classified as SendFailed via
        // classify_telegram_error.
        Mock::given(method("POST"))
            .and(path("/getFile"))
            .respond_with(ResponseTemplate::new(400).set_body_json(
                serde_json::json!({"ok": false, "description": "Bad Request: file_id invalid"}),
            ))
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let dl = downloader_for_mock(&server, tmp.path().into(), 20, true);
        let err = dl.download_file("f_bad", "u_bad").await.unwrap_err();
        match &err {
            GatewayError::SendFailed(msg) => assert!(msg.contains("400"), "msg: {msg}"),
            other => panic!("expected SendFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn download_file_missing_file_path_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/getFile"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "result": {"file_id": "f1"}})),
            )
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let dl = downloader_for_mock(&server, tmp.path().into(), 20, true);
        let err = dl.download_file("f1", "u1").await.unwrap_err();
        match &err {
            GatewayError::Platform(msg) => {
                assert!(msg.contains("no file_path"), "msg: {msg}")
            }
            other => panic!("expected Platform, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod describe_tests {
    use super::*;
    use crate::platforms::telegram::test_helpers::*;

    #[test]
    fn describe_sticker_variants() {
        assert_eq!(describe_sticker(None), "[Sticker]");
        assert_eq!(describe_sticker(Some("")), "[Sticker]");
        assert_eq!(describe_sticker(Some("🔥")), "[Sticker 🔥]");
        assert_eq!(describe_sticker(Some("😀")), "[Sticker 😀]");
    }

    #[test]
    fn extract_document_text_plain_utf8() {
        assert_eq!(
            extract_document_text(b"hello world", 2000),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn extract_document_text_truncates_to_max_chars() {
        assert_eq!(extract_document_text(b"abcdef", 3), Some("abc".to_string()));
    }

    #[test]
    fn extract_document_text_truncation_is_char_not_byte_based() {
        // 'é' is 2 bytes / 1 char: a 2-char limit must yield 2 chars, and a
        // multi-byte char must never be split.
        assert_eq!(
            extract_document_text("éa😀x".as_bytes(), 2),
            Some("éa".to_string())
        );
        assert_eq!(
            extract_document_text("éa😀x".as_bytes(), 3),
            Some("éa😀".to_string())
        );
    }

    #[test]
    fn extract_document_text_rejects_invalid_utf8() {
        let bytes = &[0x68, 0x69, 0xff, 0xfe];
        assert_eq!(extract_document_text(bytes, 2000), None);
    }

    #[test]
    fn extract_document_text_rejects_nul_bytes() {
        // Valid UTF-8 but binary-looking (embedded NUL) → not text.
        let bytes = b"PK\x03\x04\x00binary\x00data";
        assert_eq!(extract_document_text(bytes, 2000), None);
    }

    #[test]
    fn extract_document_text_empty_is_empty_text() {
        assert_eq!(extract_document_text(b"", 2000), Some(String::new()));
    }

    /// C7 integration: `media_enabled=false` → `enrich_media_message` is a
    /// strict no-op — the parsed message must be byte-identical afterwards.
    #[tokio::test]
    async fn enrich_media_message_noop_when_media_disabled() {
        let mut config = test_adapter_config();
        config.media_enabled = false;
        let adapter = TelegramAdapter::new(config).unwrap();

        // A voice message: with media enabled this would attempt a download
        // + STT; disabled it must leave everything untouched.
        let json = serde_json::json!({
            "update_id": 900,
            "message": {
                "message_id": 42,
                "chat": {"id": 300, "type": "private"},
                "from": {"id": 400, "first_name": "Vox"},
                "voice": {"file_id": "v_1", "file_unique_id": "v_u1", "duration": 7}
            }
        });
        let update: Update = serde_json::from_value(json).unwrap();
        let mut parsed = TelegramAdapter::parse_update(&update).unwrap();
        assert!(parsed.is_voice);
        // Pre-seed text the way a caption would: it must survive untouched.
        parsed.text = Some("original text".into());

        enrich_media_message(&adapter, &mut parsed, &update).await;

        assert_eq!(parsed.text, Some("original text".into()));
        assert!(parsed.is_voice);
        assert_eq!(parsed.voice_file_id, Some("v_1".into()));
        assert_eq!(parsed.voice_file_unique_id, Some("v_u1".into()));
    }

    /// Same no-op guarantee for a document message with media disabled.
    #[tokio::test]
    async fn enrich_media_message_noop_for_document_when_disabled() {
        let mut config = test_adapter_config();
        config.media_enabled = false;
        let adapter = TelegramAdapter::new(config).unwrap();

        let json = serde_json::json!({
            "update_id": 901,
            "message": {
                "message_id": 43,
                "chat": {"id": 300, "type": "private"},
                "from": {"id": 400, "first_name": "Doc"},
                "document": {
                    "file_id": "doc_1",
                    "file_unique_id": "doc_u1",
                    "file_name": "notes.txt"
                }
            }
        });
        let update: Update = serde_json::from_value(json).unwrap();
        let mut parsed = TelegramAdapter::parse_update(&update).unwrap();
        assert!(parsed.is_document);
        assert!(parsed.text.is_none());

        enrich_media_message(&adapter, &mut parsed, &update).await;

        assert!(parsed.text.is_none());
        assert_eq!(parsed.document_file_name, Some("notes.txt".into()));
    }

    /// Sticker enrichment needs no network: with media enabled and the
    /// downloader pointing at an unreachable host, the sticker placeholder
    /// still lands because stickers never download.
    #[tokio::test]
    async fn enrich_media_message_sticker_placeholder_without_network() {
        let mut config = test_adapter_config();
        config.media_enabled = true;
        config.media_cache_dir = Some(std::env::temp_dir().join("tg-media-test-sticker"));
        let adapter = TelegramAdapter::new(config).unwrap();

        let json = serde_json::json!({
            "update_id": 902,
            "message": {
                "message_id": 44,
                "chat": {"id": 300, "type": "private"},
                "from": {"id": 400, "first_name": "Stk"},
                "sticker": {
                    "file_id": "stk_1",
                    "file_unique_id": "stk_u1",
                    "emoji": "🔥"
                }
            }
        });
        let update: Update = serde_json::from_value(json).unwrap();
        let mut parsed = TelegramAdapter::parse_update(&update).unwrap();
        assert!(parsed.is_sticker);

        enrich_media_message(&adapter, &mut parsed, &update).await;

        assert_eq!(parsed.text, Some("[Sticker 🔥]".to_string()));
    }

    /// A voice message with a missing `file_unique_id` must not panic and
    /// must leave the text alone (defensive: Telegram always sends it, but
    /// hand-built `IncomingMessage`s might not).
    #[tokio::test]
    async fn enrich_media_message_missing_ids_are_skipped() {
        let adapter = test_adapter();
        assert!(adapter.media_downloader().enabled());

        let update = make_dm_text_update(903, 400, "plain text");
        let mut parsed = TelegramAdapter::parse_update(&update).unwrap();
        // Forge a media flag without ids — enrichment must skip gracefully.
        parsed.is_photo = true;
        parsed.photo_file_id = None;
        parsed.photo_file_unique_id = None;

        enrich_media_message(&adapter, &mut parsed, &update).await;

        assert_eq!(parsed.text, Some("plain text".to_string()));
    }
}
