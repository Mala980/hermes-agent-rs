//! `xai_video_edit` / `xai_video_extend`: edit or extend a video with xAI Imagine.
//!
//! Port of `tools/xai_video_tools.py` (+ the `run_xai_video_edit` /
//! `run_xai_video_extend` backends in `plugins/video_gen/xai`). The original
//! resolves credentials through the xAI OAuth pool with `XAI_API_KEY` as
//! fallback; this port follows the `x_search` pattern and uses `XAI_API_KEY`
//! only (`XAI_BASE_URL` optionally overrides the API base).
//!
//! API shape (xAI Imagine video API):
//! - Submit: `POST {base}/videos/edits` (edit) or `{base}/videos/extensions`
//!   (extend) with `{"model": "grok-imagine-video", "prompt", "video":
//!   {"url": video_url}}` (+ `duration` for extend), `x-idempotency-key` header
//! - Poll: `GET {base}/videos/{request_id}` every 5s up to 240s; terminal
//!   statuses `done | failed | error | expired | cancelled`
//! - Result: on `done`, the video URL is `video.file_output.public_url` when
//!   xAI storage is enabled, else the temporary `video.url`
//! - Auth: `Authorization: Bearer <XAI_API_KEY>`
//!
//! Duration rules (from the original): edit has no duration parameter on the
//! wire (result reports the 8s default); extend clamps to [1, 10] with a 6s
//! default, and bools are rejected (so `duration=true` never becomes 1).

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

use hermes_core::{tool_schema, JsonSchema, ToolError, ToolHandler, ToolSchema};

const DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";
const DEFAULT_MODEL: &str = "grok-imagine-video";
const EDIT_DURATION: u64 = 8;
const EXTEND_DEFAULT_DURATION: u64 = 6;
const EXTEND_MAX_DURATION: u64 = 10;
const SUBMIT_TIMEOUT_SECS: u64 = 60;
const POLL_TIMEOUT_SECS: u64 = 30;
const POLL_INTERVAL_SECS: u64 = 5;
const POLL_DEADLINE_SECS: u64 = 240;

/// Terminal poll statuses for `/videos/{request_id}`.
const TERMINAL_STATUSES: &[&str] = &["done", "failed", "error", "expired", "cancelled"];

/// User-Agent for xAI calls (mirrors `x_search`).
fn user_agent() -> String {
    format!("hermes-agent/{}", env!("CARGO_PKG_VERSION"))
}

fn normalize_base_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        DEFAULT_BASE_URL.to_string()
    } else {
        trimmed.to_string()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum VideoOp {
    Edit,
    Extend,
}

impl VideoOp {
    fn name(self) -> &'static str {
        match self {
            VideoOp::Edit => "xai_video_edit",
            VideoOp::Extend => "xai_video_extend",
        }
    }

    fn endpoint(self) -> &'static str {
        match self {
            VideoOp::Edit => "edits",
            VideoOp::Extend => "extensions",
        }
    }

    fn label(self) -> &'static str {
        match self {
            VideoOp::Edit => "edit",
            VideoOp::Extend => "extend",
        }
    }
}

/// Shared xAI video client; one instance backs both tools.
pub struct XaiVideoHandler {
    op: VideoOp,
    api_key: Option<String>,
    base_url: String,
    client: reqwest::Client,
}

impl XaiVideoHandler {
    pub fn new(op: VideoOp) -> Self {
        Self {
            op,
            api_key: std::env::var("XAI_API_KEY")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            base_url: std::env::var("XAI_BASE_URL")
                .ok()
                .map(|s| normalize_base_url(&s))
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            client: reqwest::Client::new(),
        }
    }

    pub fn edit() -> Self {
        Self::new(VideoOp::Edit)
    }

    pub fn extend() -> Self {
        Self::new(VideoOp::Extend)
    }

    pub fn with_credentials(
        op: VideoOp,
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            op,
            api_key: Some(api_key.into()),
            base_url: normalize_base_url(&base_url.into()),
            client: reqwest::Client::new(),
        }
    }

    fn api_key(&self) -> Result<String, ToolError> {
        self.api_key
            .clone()
            .filter(|k| !k.trim().is_empty())
            .ok_or_else(|| {
                ToolError::ExecutionFailed(
                    "XAI_API_KEY not configured. Get one from https://console.x.ai/.".into(),
                )
            })
    }

    /// Submit payload to `/videos/{endpoint}`; returns the request_id.
    async fn submit(&self, payload: &Value, api_key: &str) -> Result<String, ToolError> {
        let url = format!("{}/videos/{}", self.base_url, self.op.endpoint());
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .header("User-Agent", user_agent())
            .header("x-idempotency-key", uuid::Uuid::new_v4().to_string())
            .json(payload)
            .timeout(Duration::from_secs(SUBMIT_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("xAI submit request failed: {e}")))?;
        let status = resp.status();
        let body: Value = resp.json().await.map_err(|e| {
            ToolError::ExecutionFailed(format!("xAI submit response invalid JSON: {e}"))
        })?;
        if !status.is_success() {
            let detail = body
                .get("error")
                .and_then(|e| e.as_str())
                .or_else(|| body.get("message").and_then(|m| m.as_str()))
                .unwrap_or("unknown error");
            return Err(ToolError::ExecutionFailed(format!(
                "xAI submit failed ({status}): {detail}"
            )));
        }
        body.get("request_id")
            .and_then(|r| r.as_str())
            .filter(|r| !r.is_empty())
            .map(|r| r.to_string())
            .ok_or_else(|| {
                ToolError::ExecutionFailed("xAI video response did not include request_id".into())
            })
    }

    /// Poll `/videos/{request_id}` to a terminal status; returns the final body.
    async fn poll(&self, request_id: &str, api_key: &str) -> Result<Value, ToolError> {
        let url = format!("{}/videos/{request_id}", self.base_url);
        let start = Instant::now();
        loop {
            let resp = self
                .client
                .get(&url)
                .header("Authorization", format!("Bearer {api_key}"))
                .header("User-Agent", user_agent())
                .timeout(Duration::from_secs(POLL_TIMEOUT_SECS))
                .send()
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("xAI poll request failed: {e}")))?;
            let body: Value = resp.json().await.map_err(|e| {
                ToolError::ExecutionFailed(format!("xAI poll response invalid JSON: {e}"))
            })?;
            let status = body
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_lowercase();
            if TERMINAL_STATUSES.contains(&status.as_str()) {
                return Ok(body);
            }
            if start.elapsed() >= Duration::from_secs(POLL_DEADLINE_SECS) {
                return Err(ToolError::Timeout(format!(
                    "Timed out waiting for xAI video request after {POLL_DEADLINE_SECS}s"
                )));
            }
            tokio::time::sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
        }
    }

    /// Build the submit payload for this op.
    fn build_payload(prompt: &str, video_url: &str, duration: Option<u64>, op: VideoOp) -> Value {
        let mut payload = json!({
            "model": DEFAULT_MODEL,
            "prompt": prompt,
            "video": {"url": video_url},
        });
        if op == VideoOp::Extend {
            payload["duration"] = json!(clamp_extend_duration(duration));
        }
        payload
    }

    /// Extract the result video URL from a terminal `done` body:
    /// stored files-cdn `public_url` first, else xAI's temporary `video.url`.
    fn extract_video_url(body: &Value) -> Option<String> {
        let video = body.get("video")?;
        let stored = video
            .get("file_output")
            .and_then(|f| f.get("public_url"))
            .and_then(|u| u.as_str())
            .map(str::trim)
            .filter(|u| !u.is_empty());
        let temporary = video
            .get("url")
            .and_then(|u| u.as_str())
            .map(str::trim)
            .filter(|u| !u.is_empty());
        stored.or(temporary).map(|u| u.to_string())
    }

    async fn execute_op(&self, params: Value) -> Result<String, ToolError> {
        let prompt = params
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ToolError::InvalidParams(format!(
                    "prompt is required for xAI video {}",
                    self.op.label()
                ))
            })?;
        let video_url = params
            .get("video_url")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| {
                let lower = s.to_lowercase();
                lower.starts_with("http://") || lower.starts_with("https://")
            })
            .ok_or_else(|| {
                ToolError::InvalidParams(
                    "video_url must be a public HTTPS MP4 URL (the `video`/`public_url` from a prior Imagine result)"
                        .into(),
                )
            })?;

        let api_key = self.api_key()?;
        let duration: Option<u64> = if self.op == VideoOp::Extend {
            // bools are rejected (unlike a plain int coercion) so
            // duration=true never becomes 1.
            match params.get("duration") {
                None | Some(Value::Null) => None,
                Some(Value::Bool(_)) => None,
                Some(Value::Number(n)) => n.as_u64(),
                Some(_) => None,
            }
        } else {
            None
        };

        let payload = Self::build_payload(prompt, video_url, duration, self.op);
        let request_id = self.submit(&payload, &api_key).await?;
        let body = self.poll(&request_id, &api_key).await?;
        let status = body
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_lowercase();
        if status != "done" {
            let message = body
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .or_else(|| body.get("message").and_then(|m| m.as_str()))
                .unwrap_or("unknown error");
            return Err(ToolError::ExecutionFailed(format!(
                "xAI video request ended with status '{status}': {message}"
            )));
        }
        let out_url = Self::extract_video_url(&body).ok_or_else(|| {
            ToolError::ExecutionFailed("xAI video request completed without a video URL".into())
        })?;
        let duration_out = body
            .get("video")
            .and_then(|v| v.get("duration"))
            .and_then(|d| d.as_u64())
            .unwrap_or_else(|| {
                if self.op == VideoOp::Extend {
                    clamp_extend_duration(duration)
                } else {
                    EDIT_DURATION
                }
            });
        Ok(json!({
            "success": true,
            "video": out_url,
            "model": body.get("model").and_then(|m| m.as_str()).unwrap_or(DEFAULT_MODEL),
            "prompt": prompt,
            "modality": self.op.label(),
            "operation": self.op.label(),
            "duration": duration_out,
            "provider": "xai",
            "request_id": request_id,
        })
        .to_string())
    }

    fn schema(&self) -> ToolSchema {
        let (verb, noun, prompt_verb) = match self.op {
            VideoOp::Edit => ("Edit", "editing", "modify"),
            VideoOp::Extend => ("Extend", "extension", "continue"),
        };
        let mut props = IndexMap::new();
        props.insert(
            "prompt".into(),
            json!({
                "type": "string",
                "description": format!("Instruction for how xAI should {prompt_verb} the source video.")
            }),
        );
        props.insert(
            "video_url".into(),
            json!({
                "type": "string",
                "description": "Public HTTPS MP4 URL of the source video — the `video` or `public_url` from a prior xAI Imagine result."
            }),
        );
        if self.op == VideoOp::Extend {
            props.insert(
                "duration".into(),
                json!({
                    "type": "integer",
                    "description": "Desired extension duration in seconds. xAI clamps this to its supported range."
                }),
            );
        }
        tool_schema(
            self.op.name(),
            format!(
                "{verb} an existing video with xAI Imagine. This is separate from `video_generate` because video {noun} \
                 is provider-specific. `video_url` must be the public HTTPS MP4 URL from a prior Imagine result \
                 (`video` or `public_url` on files-cdn)."
            ),
            JsonSchema::object(props, vec!["prompt".into(), "video_url".into()]),
        )
    }
}

#[async_trait]
impl ToolHandler for XaiVideoHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        self.execute_op(params).await
    }
    fn schema(&self) -> ToolSchema {
        self.schema()
    }
}

/// Clamp extend duration to [1, 10] with a 6s default (from the original).
fn clamp_extend_duration(duration: Option<u64>) -> u64 {
    duration
        .unwrap_or(EXTEND_DEFAULT_DURATION)
        .clamp(1, EXTEND_MAX_DURATION)
}

/// Tool is available only when an xAI API key is configured.
pub fn check_xai_video_requirements() -> bool {
    std::env::var("XAI_API_KEY")
        .map(|k| !k.trim().is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_names_required_and_params() {
        let edit = XaiVideoHandler::edit().schema();
        assert_eq!(edit.name, "xai_video_edit");
        assert!(edit
            .description
            .contains("Edit an existing video with xAI Imagine"));
        assert!(edit.description.contains("video_generate"));
        let props = edit.parameters.properties.as_ref().unwrap();
        assert!(props.get("prompt").is_some());
        assert!(props.get("video_url").is_some());
        assert!(props.get("duration").is_none());
        assert_eq!(
            edit.parameters.required.as_deref(),
            Some(&["prompt".to_string(), "video_url".to_string()][..])
        );

        let extend = XaiVideoHandler::extend().schema();
        assert_eq!(extend.name, "xai_video_extend");
        assert!(extend
            .description
            .contains("Extend an existing video with xAI Imagine"));
        let props = extend.parameters.properties.as_ref().unwrap();
        assert!(props.get("duration").is_some());
    }

    #[test]
    fn edit_payload_has_no_duration_extend_clamps() {
        let edit = XaiVideoHandler::build_payload("p", "https://x/v.mp4", None, VideoOp::Edit);
        assert_eq!(edit["model"], json!(DEFAULT_MODEL));
        assert_eq!(edit["video"]["url"], json!("https://x/v.mp4"));
        assert!(edit.get("duration").is_none());

        let extend =
            XaiVideoHandler::build_payload("p", "https://x/v.mp4", Some(99), VideoOp::Extend);
        assert_eq!(extend["duration"], json!(10));
        let extend = XaiVideoHandler::build_payload("p", "https://x/v.mp4", None, VideoOp::Extend);
        assert_eq!(extend["duration"], json!(6));
    }

    #[test]
    fn clamp_extend_duration_bounds() {
        assert_eq!(clamp_extend_duration(None), 6);
        assert_eq!(clamp_extend_duration(Some(0)), 1);
        assert_eq!(clamp_extend_duration(Some(4)), 4);
        assert_eq!(clamp_extend_duration(Some(50)), 10);
    }

    #[test]
    fn extract_video_url_prefers_public_url() {
        let body = json!({
            "status": "done",
            "video": {"url": "https://temp/x.mp4", "file_output": {"public_url": "https://cdn/y.mp4"}}
        });
        assert_eq!(
            XaiVideoHandler::extract_video_url(&body).as_deref(),
            Some("https://cdn/y.mp4")
        );
        let body = json!({"status": "done", "video": {"url": "https://temp/x.mp4"}});
        assert_eq!(
            XaiVideoHandler::extract_video_url(&body).as_deref(),
            Some("https://temp/x.mp4")
        );
        let body = json!({"status": "done", "video": {}});
        assert!(XaiVideoHandler::extract_video_url(&body).is_none());
    }

    #[test]
    fn terminal_statuses_cover_original_set() {
        for s in ["done", "failed", "error", "expired", "cancelled"] {
            assert!(TERMINAL_STATUSES.contains(&s), "{s} should be terminal");
        }
        assert!(!TERMINAL_STATUSES.contains(&"queued"));
    }

    #[tokio::test]
    async fn missing_prompt_rejected() {
        let h = XaiVideoHandler::with_credentials(VideoOp::Edit, "k", "https://api.x.ai/v1");
        let err = h
            .execute(json!({"video_url": "https://x/v.mp4"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("prompt is required"));
    }

    #[tokio::test]
    async fn non_url_video_rejected() {
        let h = XaiVideoHandler::with_credentials(VideoOp::Edit, "k", "https://api.x.ai/v1");
        let err = h
            .execute(json!({"prompt": "p", "video_url": "/local/path.mp4"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("public HTTPS MP4 URL"));
    }

    #[tokio::test]
    async fn missing_key_fails_closed() {
        let h = XaiVideoHandler {
            op: VideoOp::Extend,
            api_key: None,
            base_url: DEFAULT_BASE_URL.to_string(),
            client: reqwest::Client::new(),
        };
        let err = h
            .execute(json!({"prompt": "p", "video_url": "https://x/v.mp4"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("XAI_API_KEY"));
    }
}
