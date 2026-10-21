//! x_search: search X (Twitter) posts via xAI's Responses API built-in tool.
//!
//! Port of `tools/x_search_tool.py`. Simplified: credentials come only from the
//! `XAI_API_KEY` environment variable (no OAuth flow). Date filters are
//! validated client-side so malformed windows fail fast instead of burning a
//! billable call.
//!
//! Behavioral parity with hermes-rs `social.rs` (xAI Responses API):
//! - Endpoint: POST `{base_url}/responses`, default
//!   `https://api.x.ai/v1/responses`.
//! - Auth: `Authorization: Bearer <XAI_API_KEY>` + `User-Agent` header;
//!   missing/blank key fails before any HTTP call (fail-closed).
//! - Body: `{model, input:[{role:"user", content}], tools:[{type:"x_search",
//!   ...filters}], store:false}` — no `reasoning` field, no result count
//!   (the xAI `x_search` tool has no count field; the response is the model
//!   answer + citations, not raw tweets).
//! - Retry: 5xx / timeout / connect errors retried 2x with 1.5s, 3.0s backoff
//!   (capped at 5s). 4xx is never retried — it is auth/param failure, so
//!   retrying would only burn billable calls.
//! - HTTP error bodies of the form `{"code","error"}` surface as
//!   `"code: error"`.
//! - `degraded` is true when narrowing filters are active but xAI returns no
//!   citations on either channel (the answer then came from the model's
//!   training data, not the X index).

use async_trait::async_trait;
use chrono::NaiveDate;
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::time::Duration;

use hermes_core::{tool_schema, JsonSchema, ToolError, ToolHandler, ToolSchema};

const DEFAULT_MODEL: &str = "grok-4.5";
const DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";
const MAX_HANDLES: usize = 10;
const REQUEST_TIMEOUT_SECS: u64 = 180;
const MAX_RETRIES: u32 = 2;

/// Search X via xAI Responses API.
pub struct XSearchHandler {
    api_key: Option<String>,
    base_url: String,
    model: String,
    client: reqwest::Client,
}

/// Trim whitespace and trailing slashes; fall back to the default base URL
/// when nothing usable remains.
fn normalize_base_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        DEFAULT_BASE_URL.to_string()
    } else {
        trimmed.to_string()
    }
}

/// `POST {base}/responses`; base is expected to be already normalized.
fn responses_url(base_url: &str) -> String {
    format!("{}/responses", base_url.trim_end_matches('/'))
}

/// User-Agent for xAI calls. (hermes-rs uses `hermes-rs/<version>`; the
/// Lumio fork identifies as `hermes-agent/<version>`.)
fn user_agent() -> String {
    format!("hermes-agent/{}", env!("CARGO_PKG_VERSION"))
}

/// Backoff between retries: min(5.0, 1.5*(attempt+1)) seconds — 1.5s, 3.0s.
fn retry_backoff(attempt: u32) -> Duration {
    Duration::from_secs_f64((1.5 * f64::from(attempt) + 1.5).min(5.0))
}

impl XSearchHandler {
    pub fn new() -> Self {
        Self {
            api_key: std::env::var("XAI_API_KEY")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            base_url: std::env::var("XAI_BASE_URL")
                .ok()
                .map(|s| normalize_base_url(&s))
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            model: std::env::var("XAI_X_SEARCH_MODEL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            client: reqwest::Client::new(),
        }
    }

    pub fn with_credentials(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            api_key: Some(api_key.into()),
            base_url: normalize_base_url(&base_url.into()),
            model: model.into(),
            client: reqwest::Client::new(),
        }
    }

    /// POST to /responses. Ok only for 2xx. Err = (message, error kind:
    /// "http" | "timeout" | "transport"). 5xx / timeout / connect errors are
    /// retried up to MAX_RETRIES times; 4xx is returned immediately (auth or
    /// param failure — retrying would burn billable calls).
    async fn post_responses(
        &self,
        url: &str,
        api_key: &str,
        payload: &Value,
    ) -> Result<reqwest::Response, (String, &'static str)> {
        let mut attempt = 0u32;
        loop {
            let res = self
                .client
                .post(url)
                .header("Authorization", format!("Bearer {api_key}"))
                .header("User-Agent", user_agent())
                .json(payload)
                .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
                .send()
                .await;
            match res {
                Ok(resp) if resp.status().is_success() => return Ok(resp),
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    if status.is_server_error() && attempt < MAX_RETRIES {
                        tokio::time::sleep(retry_backoff(attempt)).await;
                        attempt += 1;
                        continue;
                    }
                    return Err((http_error_message(status.as_u16(), &body), "http"));
                }
                Err(e) => {
                    let kind = if e.is_timeout() {
                        "timeout"
                    } else {
                        "transport"
                    };
                    if (e.is_timeout() || e.is_connect()) && attempt < MAX_RETRIES {
                        tokio::time::sleep(retry_backoff(attempt)).await;
                        attempt += 1;
                        continue;
                    }
                    return Err((format!("xAI x_search request failed: {e}"), kind));
                }
            }
        }
    }
}

impl Default for XSearchHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// Normalize a handle list: must be an array of strings, strip whitespace
/// and leading `@`, drop blanks; at most MAX_HANDLES handles.
fn normalize_handles(value: Option<&Value>, field: &str) -> Result<Vec<String>, ToolError> {
    let mut out = Vec::new();
    if let Some(v) = value {
        let arr = v.as_array().ok_or_else(|| {
            ToolError::InvalidParams(format!("{field} must be an array of strings"))
        })?;
        for h in arr {
            let s = h
                .as_str()
                .ok_or_else(|| {
                    ToolError::InvalidParams(format!("{field} must be an array of strings"))
                })?
                .trim()
                .trim_start_matches('@')
                .trim();
            if !s.is_empty() {
                out.push(s.to_string());
            }
        }
    }
    if out.len() > MAX_HANDLES {
        return Err(ToolError::InvalidParams(format!(
            "{field} supports at most {MAX_HANDLES} handles"
        )));
    }
    Ok(out)
}

/// Strict YYYY-MM-DD (exactly 10 chars, digits in the right positions) or
/// None for blank. Chrono's `%Y-%m-%d` alone would also accept non-padded
/// forms like "2026-4-1"; xAI/the original require the padded form, so we
/// enforce it byte-wise before parsing.
fn parse_iso_date(raw: &str, field: &str) -> Result<Option<NaiveDate>, ToolError> {
    let s = raw.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let b = s.as_bytes();
    let strict = s.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && s[..4].chars().all(|c| c.is_ascii_digit())
        && s[5..7].chars().all(|c| c.is_ascii_digit())
        && s[8..10].chars().all(|c| c.is_ascii_digit());
    if !strict {
        return Err(ToolError::InvalidParams(format!(
            "{field} must be YYYY-MM-DD (got {raw:?})"
        )));
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map(Some)
        .map_err(|_| ToolError::InvalidParams(format!("{field} must be YYYY-MM-DD (got {raw:?})")))
}

/// Both parse as YYYY-MM-DD; from <= to; from not after today (UTC).
/// `to` may be in the future (matches the original).
fn validate_date_range(from_date: &str, to_date: &str) -> Result<(), ToolError> {
    let from = parse_iso_date(from_date, "from_date")?;
    let to = parse_iso_date(to_date, "to_date")?;
    if let (Some(f), Some(t)) = (from, to) {
        if f > t {
            return Err(ToolError::InvalidParams(format!(
                "from_date ({}) must be on or before to_date ({})",
                f.format("%Y-%m-%d"),
                t.format("%Y-%m-%d")
            )));
        }
    }
    if let Some(f) = from {
        let today = chrono::Utc::now().date_naive();
        if f > today {
            return Err(ToolError::InvalidParams(format!(
                "from_date ({}) is in the future; X Search only indexes past posts",
                f.format("%Y-%m-%d")
            )));
        }
    }
    Ok(())
}

/// Build the xAI `x_search` tool definition; returns (tool_def, active_filters).
fn build_tool_def(
    allowed: Vec<String>,
    excluded: Vec<String>,
    from_date: &str,
    to_date: &str,
    enable_image_understanding: bool,
    enable_video_understanding: bool,
) -> Result<(Value, Vec<String>), ToolError> {
    if !allowed.is_empty() && !excluded.is_empty() {
        return Err(ToolError::InvalidParams(
            "allowed_x_handles and excluded_x_handles cannot be used together".into(),
        ));
    }
    validate_date_range(from_date, to_date)?;

    let mut tool_def = json!({"type": "x_search"});
    let mut active_filters: Vec<String> = Vec::new();
    if !allowed.is_empty() {
        tool_def["allowed_x_handles"] = json!(allowed);
        active_filters.push("allowed_x_handles".into());
    }
    if !excluded.is_empty() {
        tool_def["excluded_x_handles"] = json!(excluded);
        active_filters.push("excluded_x_handles".into());
    }
    if !from_date.trim().is_empty() {
        tool_def["from_date"] = json!(from_date.trim());
        active_filters.push("from_date".into());
    }
    if !to_date.trim().is_empty() {
        tool_def["to_date"] = json!(to_date.trim());
        active_filters.push("to_date".into());
    }
    if enable_image_understanding {
        tool_def["enable_image_understanding"] = json!(true);
    }
    if enable_video_understanding {
        tool_def["enable_video_understanding"] = json!(true);
    }
    Ok((tool_def, active_filters))
}

/// Build the Responses API request body: `{model, input, tools, store:false}`.
fn build_request_payload(model: &str, query: &str, tool_def: Value) -> Value {
    json!({
        "model": model,
        "input": [{"role": "user", "content": query}],
        "tools": [tool_def],
        "store": false,
    })
}

/// Error message for a non-2xx HTTP response: bodies of the form
/// `{"code","error"}` surface as `"code: error"`; otherwise the first 500
/// chars of the body (or the status when the body is empty).
fn http_error_message(status: u16, body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        if v.is_object() {
            let code = v.get("code").and_then(|c| c.as_str()).unwrap_or("").trim();
            let message = v.get("error").and_then(|e| e.as_str()).unwrap_or("").trim();
            let message = if message.is_empty() {
                v.to_string()
            } else {
                message.to_string()
            };
            if !code.is_empty() && !message.contains(code) {
                return format!("{code}: {message}");
            }
            if !message.is_empty() {
                return message;
            }
        }
    }
    let t = body.trim();
    if t.is_empty() {
        format!("HTTP {status}")
    } else {
        t.chars().take(500).collect()
    }
}

/// `degraded` is true when narrowing filters were active but xAI returned no
/// citations on either channel — the answer came from the model's training
/// data, not the X index.
fn is_degraded(active_filters: &[String], citations: &[Value], inline_citations: &[Value]) -> bool {
    !active_filters.is_empty() && citations.is_empty() && inline_citations.is_empty()
}

fn message_contents(payload: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    if let Some(items) = payload.get("output").and_then(|o| o.as_array()) {
        for item in items {
            if item.get("type").and_then(|t| t.as_str()) == Some("message") {
                if let Some(contents) = item.get("content").and_then(|c| c.as_array()) {
                    out.extend(contents.iter());
                }
            }
        }
    }
    out
}

fn extract_response_text(payload: &Value) -> String {
    if let Some(t) = payload
        .get("output_text")
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        return t.to_string();
    }
    message_contents(payload)
        .iter()
        .filter(|c| {
            matches!(
                c.get("type").and_then(|t| t.as_str()),
                Some("output_text") | Some("text")
            )
        })
        .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn extract_inline_citations(payload: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for content in message_contents(payload) {
        if let Some(annotations) = content.get("annotations").and_then(|a| a.as_array()) {
            for a in annotations {
                if a.get("type").and_then(|t| t.as_str()) == Some("url_citation") {
                    out.push(json!({
                        "url": a.get("url").and_then(|u| u.as_str()).unwrap_or(""),
                        "title": a.get("title").and_then(|t| t.as_str()).unwrap_or(""),
                        "start_index": a.get("start_index"),
                        "end_index": a.get("end_index"),
                    }));
                }
            }
        }
    }
    out
}

#[async_trait]
impl ToolHandler for XSearchHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| ToolError::InvalidParams("query is required for x_search".into()))?;

        let api_key = self
            .api_key
            .clone()
            .filter(|k| !k.trim().is_empty())
            .ok_or_else(|| {
                ToolError::ExecutionFailed("No xAI credentials available. Set XAI_API_KEY.".into())
            })?;

        let (tool_def, active_filters) = build_tool_def(
            normalize_handles(params.get("allowed_x_handles"), "allowed_x_handles")?,
            normalize_handles(params.get("excluded_x_handles"), "excluded_x_handles")?,
            params
                .get("from_date")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            params.get("to_date").and_then(|v| v.as_str()).unwrap_or(""),
            params
                .get("enable_image_understanding")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            params
                .get("enable_video_understanding")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        )?;

        let payload = build_request_payload(&self.model, query, tool_def);
        let url = responses_url(&self.base_url);
        let data = match self.post_responses(&url, api_key.trim(), &payload).await {
            Ok(resp) => resp.json::<Value>().await.map_err(|e| {
                ToolError::ExecutionFailed(format!("xAI: invalid JSON response: {e}"))
            })?,
            Err((message, kind)) => {
                return Err(match kind {
                    "timeout" => ToolError::Timeout(message),
                    _ => ToolError::ExecutionFailed(message),
                })
            }
        };

        let citations: Vec<Value> = data
            .get("citations")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        let inline_citations = extract_inline_citations(&data);
        let degraded = is_degraded(&active_filters, &citations, &inline_citations);

        Ok(json!({
            "success": true,
            "provider": "xai",
            "credential_source": "xai",
            "tool": "x_search",
            "model": self.model,
            "query": query,
            "answer": extract_response_text(&data),
            "citations": citations,
            "inline_citations": inline_citations,
            "degraded": degraded,
            "degraded_reason": if degraded {
                Some(format!("no citations returned despite filters: {}", active_filters.join(", ")))
            } else { None },
        })
        .to_string())
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "query".into(),
            json!({"type": "string", "description": "What to look up on X."}),
        );
        props.insert(
            "allowed_x_handles".into(),
            json!({
                "type": "array",
                "items": {"type": "string"},
                "description": "Optional list of X handles to include exclusively (max 10)."
            }),
        );
        props.insert(
            "excluded_x_handles".into(),
            json!({
                "type": "array",
                "items": {"type": "string"},
                "description": "Optional list of X handles to exclude (max 10)."
            }),
        );
        props.insert(
            "from_date".into(),
            json!({"type": "string", "description": "Optional start date in YYYY-MM-DD format."}),
        );
        props.insert(
            "to_date".into(),
            json!({"type": "string", "description": "Optional end date in YYYY-MM-DD format."}),
        );
        props.insert(
            "enable_image_understanding".into(),
            json!({
                "type": "boolean",
                "description": "Whether xAI should analyze images attached to matching X posts.",
                "default": false
            }),
        );
        props.insert(
            "enable_video_understanding".into(),
            json!({
                "type": "boolean",
                "description": "Whether xAI should analyze videos attached to matching X posts.",
                "default": false
            }),
        );
        tool_schema(
            "x_search",
            "Search X (Twitter) posts, profiles, and threads using xAI's built-in X Search tool. Read-only discovery only.",
            JsonSchema::object(props, vec!["query".into()]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_base_url_trims_slashes_and_spaces() {
        assert_eq!(
            normalize_base_url("https://api.x.ai/v1///"),
            "https://api.x.ai/v1"
        );
        assert_eq!(
            normalize_base_url("  https://example.com/base/ "),
            "https://example.com/base"
        );
        assert_eq!(normalize_base_url("   "), DEFAULT_BASE_URL);
        assert_eq!(normalize_base_url(""), DEFAULT_BASE_URL);
    }

    #[test]
    fn responses_url_joins_base_and_path_exactly() {
        assert_eq!(
            responses_url("https://api.x.ai/v1"),
            "https://api.x.ai/v1/responses"
        );
        // Trailing slash on the base must not produce a double slash.
        assert_eq!(
            responses_url("https://api.x.ai/v1/"),
            "https://api.x.ai/v1/responses"
        );
        // The shipped default base yields the documented xAI endpoint.
        assert_eq!(
            responses_url(normalize_base_url(DEFAULT_BASE_URL).as_str()),
            "https://api.x.ai/v1/responses"
        );
    }

    #[test]
    fn user_agent_identifies_the_fork() {
        let ua = user_agent();
        assert!(ua.starts_with("hermes-agent/"), "ua: {ua}");
    }

    #[test]
    fn normalize_handles_strips_at_and_blanks() {
        let out =
            normalize_handles(Some(&json!(["@alice", "  ", "bob"])), "allowed_x_handles").unwrap();
        assert_eq!(out, vec!["alice".to_string(), "bob".to_string()]);
    }

    #[test]
    fn normalize_handles_rejects_non_array_or_non_string() {
        assert!(normalize_handles(Some(&json!("not-an-array")), "allowed_x_handles").is_err());
        assert!(normalize_handles(Some(&json!(["alice", 42])), "allowed_x_handles").is_err());
        assert!(normalize_handles(None, "allowed_x_handles")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn normalize_handles_enforces_max() {
        let many = json!((0..11).map(|i| format!("h{i}")).collect::<Vec<_>>());
        assert!(normalize_handles(Some(&many), "allowed_x_handles").is_err());
    }

    #[test]
    fn parse_iso_date_strict() {
        assert_eq!(
            parse_iso_date("2026-10-19", "from_date").unwrap(),
            Some(NaiveDate::from_ymd_opt(2026, 10, 19).unwrap())
        );
        assert_eq!(parse_iso_date("", "from_date").unwrap(), None);
        assert_eq!(parse_iso_date("   ", "to_date").unwrap(), None);
        // Non-padded forms must be rejected even though chrono would accept them.
        assert!(parse_iso_date("2026-1-1", "from_date").is_err());
        assert!(parse_iso_date("2026-01-1", "from_date").is_err());
        assert!(parse_iso_date("2026-1-01", "from_date").is_err());
        assert!(parse_iso_date("19-10-2026", "from_date").is_err());
        assert!(parse_iso_date("2026-13-01", "from_date").is_err());
        assert!(parse_iso_date("2026-02-30", "from_date").is_err());
        assert!(parse_iso_date("not-a-date", "from_date").is_err());
    }

    #[test]
    fn validate_date_range_rejects_inverted() {
        assert!(validate_date_range("2026-10-20", "2026-10-19").is_err());
        assert!(validate_date_range("2026-10-19", "2026-10-20").is_ok());
        assert!(validate_date_range("", "").is_ok());
    }

    #[test]
    fn validate_date_range_rejects_future_from() {
        assert!(validate_date_range("2999-01-01", "").is_err());
    }

    #[test]
    fn build_tool_def_rejects_both_handle_lists() {
        let r = build_tool_def(
            vec!["a".to_string()],
            vec!["b".to_string()],
            "",
            "",
            false,
            false,
        );
        assert!(r.is_err());
    }

    #[test]
    fn build_tool_def_tracks_active_filters() {
        let (def, filters) = build_tool_def(
            vec![],
            vec!["spam".to_string()],
            "2026-01-01",
            "",
            true,
            false,
        )
        .unwrap();
        assert_eq!(def["type"], "x_search");
        assert_eq!(def["excluded_x_handles"], json!(["spam"]));
        assert_eq!(def["from_date"], "2026-01-01");
        assert_eq!(def["enable_image_understanding"], true);
        assert!(filters.contains(&"excluded_x_handles".to_string()));
        assert!(filters.contains(&"from_date".to_string()));
    }

    #[test]
    fn build_tool_def_full_shape() {
        let (def, _) = build_tool_def(
            vec!["xai".to_string(), "grok".to_string()],
            vec![],
            "2026-04-01",
            "2026-04-10",
            true,
            false,
        )
        .unwrap();
        assert_eq!(
            def,
            json!({
                "type": "x_search",
                "allowed_x_handles": ["xai", "grok"],
                "from_date": "2026-04-01",
                "to_date": "2026-04-10",
                "enable_image_understanding": true,
            })
        );
    }

    #[test]
    fn build_request_payload_matches_responses_api_shape() {
        let (tool_def, _) =
            build_tool_def(vec!["xai".to_string()], vec![], "", "", false, false).unwrap();
        let payload =
            build_request_payload("grok-4.5", "Apa kata orang tentang xAI di X?", tool_def);
        assert_eq!(payload["model"], "grok-4.5");
        assert_eq!(
            payload["input"],
            json!([{"role": "user", "content": "Apa kata orang tentang xAI di X?"}])
        );
        assert_eq!(payload["tools"][0]["type"], "x_search");
        assert_eq!(payload["tools"][0]["allowed_x_handles"], json!(["xai"]));
        assert_eq!(payload["store"], false);
        // No stray fields the API does not know about.
        assert!(payload.get("reasoning").is_none());
        assert!(payload.get("max_results").is_none());
    }

    #[test]
    fn http_error_message_extracts_code_and_error() {
        assert_eq!(
            http_error_message(
                403,
                r#"{"code":"forbidden","error":"x_search is not enabled for this model"}"#
            ),
            "forbidden: x_search is not enabled for this model"
        );
        // Body without an "error" field falls back to the raw JSON.
        let v = r#"{"detail":"bad key"}"#;
        assert_eq!(http_error_message(401, v), v);
        // Plain-text body.
        assert_eq!(http_error_message(400, "bad request"), "bad request");
        // Empty body: only the status is left.
        assert_eq!(http_error_message(401, ""), "HTTP 401");
        // Long bodies are truncated to 500 chars.
        let long = "x".repeat(600);
        assert_eq!(http_error_message(500, &long).len(), 500);
    }

    #[test]
    fn retry_backoff_matches_reference_schedule() {
        assert_eq!(retry_backoff(0), Duration::from_secs_f64(1.5));
        assert_eq!(retry_backoff(1), Duration::from_secs_f64(3.0));
        assert_eq!(retry_backoff(2), Duration::from_secs_f64(4.5));
        assert_eq!(retry_backoff(100), Duration::from_secs_f64(5.0));
    }

    #[test]
    fn is_degraded_only_with_filters_and_no_citations() {
        let filters = vec!["from_date".to_string()];
        assert!(is_degraded(&filters, &[], &[]));
        assert!(!is_degraded(&[], &[], &[]));
        assert!(!is_degraded(&filters, &[json!({"url": "u"})], &[]));
        assert!(!is_degraded(&filters, &[], &[json!({"url": "u"})]));
    }

    /// Canned Responses API shape: `output_text` + top-level `citations`
    /// (captured from the reference's mock-integration test).
    #[test]
    fn extract_response_text_canned_full() {
        let p = json!({
            "output_text": "Orang di X membahas peluncuran terbaru xAI.",
            "citations": [{"url": "https://x.com/contoh/status/1", "title": "Contoh postingan"}]
        });
        assert_eq!(
            extract_response_text(&p),
            "Orang di X membahas peluncuran terbaru xAI."
        );
    }

    /// Canned Responses API shape without `output_text`: the answer is
    /// assembled from `output[].message.content[]` text parts.
    #[test]
    fn extract_response_text_falls_back_to_message_parts() {
        let p = json!({"output": [{"type": "message", "content": [
            {"type": "output_text", "text": "part one"},
            {"type": "output_text", "text": "part two"},
        ]}]});
        assert_eq!(extract_response_text(&p), "part one\n\npart two");
    }

    /// Canned annotation shape from the reference's mock-integration test.
    #[test]
    fn extract_inline_citations_canned() {
        let p = json!({"output": [{
            "type": "message",
            "content": [{
                "type": "output_text",
                "text": "xAI memposting pembaruan di X.",
                "annotations": [{
                    "type": "url_citation",
                    "url": "https://x.com/xai/status/123",
                    "title": "xAI update",
                    "start_index": 0,
                    "end_index": 3
                }],
            }],
        }]});
        assert_eq!(
            extract_inline_citations(&p),
            vec![json!({
                "url": "https://x.com/xai/status/123",
                "title": "xAI update",
                "start_index": 0,
                "end_index": 3,
            })]
        );
    }

    #[test]
    fn extract_inline_citations_filters_url_citations() {
        let p = json!({"output": [{"type": "message", "content": [{
            "type": "output_text", "text": "x",
            "annotations": [
                {"type": "url_citation", "url": "https://x.com/1", "title": "t"},
                {"type": "other", "url": "https://x.com/2"},
            ],
        }]}]});
        let c = extract_inline_citations(&p);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0]["url"], "https://x.com/1");
    }

    #[test]
    fn schema_has_expected_shape() {
        let s = XSearchHandler::new().schema();
        assert_eq!(s.name, "x_search");
        assert_eq!(s.parameters.schema_type.as_deref(), Some("object"));
        assert_eq!(
            s.parameters.required.as_deref(),
            Some(["query".to_string()].as_slice())
        );
        assert!(s
            .parameters
            .properties
            .as_ref()
            .unwrap()
            .contains_key("query"));
    }
}
