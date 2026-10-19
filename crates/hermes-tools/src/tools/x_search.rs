//! x_search: search X (Twitter) posts via xAI's Responses API built-in tool.
//!
//! Port of `tools/x_search_tool.py`. Simplified: credentials come only from the
//! `XAI_API_KEY` environment variable (no OAuth flow). Date filters are
//! validated client-side so malformed windows fail fast instead of burning a
//! billable call.

use async_trait::async_trait;
use chrono::NaiveDate;
use indexmap::IndexMap;
use serde_json::{json, Value};

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

impl XSearchHandler {
    pub fn new() -> Self {
        Self {
            api_key: std::env::var("XAI_API_KEY")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            base_url: std::env::var("XAI_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
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
            base_url: base_url.into(),
            model: model.into(),
            client: reqwest::Client::new(),
        }
    }

    async fn post_with_retries(
        &self,
        url: &str,
        api_key: &str,
        payload: &Value,
    ) -> Result<Value, ToolError> {
        let mut last_err = String::new();
        for attempt in 0..=MAX_RETRIES {
            let resp = self
                .client
                .post(url)
                .header("Authorization", format!("Bearer {api_key}"))
                .header("Content-Type", "application/json")
                .json(payload)
                .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
                .send()
                .await;
            match resp {
                Ok(r) => {
                    let status = r.status();
                    if status.is_success() {
                        return r.json::<Value>().await.map_err(|e| {
                            ToolError::ExecutionFailed(format!("xAI: invalid JSON response: {e}"))
                        });
                    }
                    if status.is_server_error() && attempt < MAX_RETRIES {
                        last_err = format!("HTTP {status}");
                        tokio::time::sleep(std::time::Duration::from_secs_f64(
                            (1.5 * (attempt + 1) as f64).min(5.0),
                        ))
                        .await;
                        continue;
                    }
                    let body = r.text().await.unwrap_or_default();
                    let short: String = body.chars().take(500).collect();
                    return Err(ToolError::ExecutionFailed(format!(
                        "xAI x_search failed: HTTP {status}: {short}"
                    )));
                }
                Err(e) => {
                    if attempt < MAX_RETRIES && (e.is_timeout() || e.is_connect()) {
                        last_err = e.to_string();
                        tokio::time::sleep(std::time::Duration::from_secs_f64(
                            (1.5 * (attempt + 1) as f64).min(5.0),
                        ))
                        .await;
                        continue;
                    }
                    return Err(ToolError::ExecutionFailed(format!(
                        "xAI x_search request failed: {e}"
                    )));
                }
            }
        }
        Err(ToolError::ExecutionFailed(format!(
            "xAI x_search failed after retries: {last_err}"
        )))
    }
}

impl Default for XSearchHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// Strip leading `@`, drop blanks; at most MAX_HANDLES handles.
fn normalize_handles(handles: &[String], field: &str) -> Result<Vec<String>, ToolError> {
    let cleaned: Vec<String> = handles
        .iter()
        .map(|h| h.trim().trim_start_matches('@').to_string())
        .filter(|h| !h.is_empty())
        .collect();
    if cleaned.len() > MAX_HANDLES {
        return Err(ToolError::InvalidParams(format!(
            "{field} supports at most {MAX_HANDLES} handles"
        )));
    }
    Ok(cleaned)
}

/// Strict YYYY-MM-DD or None for blank.
fn parse_iso_date(raw: &str, field: &str) -> Result<Option<NaiveDate>, ToolError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .map(Some)
        .map_err(|_| ToolError::InvalidParams(format!("{field} must be YYYY-MM-DD (got {raw:?})")))
}

/// Both parse as YYYY-MM-DD; from <= to; from not after today (UTC).
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
    allowed: &[String],
    excluded: &[String],
    from_date: &str,
    to_date: &str,
    enable_image_understanding: bool,
    enable_video_understanding: bool,
) -> Result<(Value, Vec<String>), ToolError> {
    let allowed = normalize_handles(allowed, "allowed_x_handles")?;
    let excluded = normalize_handles(excluded, "excluded_x_handles")?;
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

fn str_list(params: &Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
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
            &str_list(&params, "allowed_x_handles"),
            &str_list(&params, "excluded_x_handles"),
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

        let payload = json!({
            "model": self.model,
            "input": [{"role": "user", "content": query}],
            "tools": [tool_def],
            "store": false,
        });

        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let data = self
            .post_with_retries(&url, api_key.trim(), &payload)
            .await?;

        let citations: Vec<Value> = data
            .get("citations")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        let inline_citations = extract_inline_citations(&data);
        // xAI returns 200 with a synthesized answer even when no posts match the
        // narrowing filters; with both citation channels empty the answer came
        // from training data, not the X index.
        let degraded =
            !active_filters.is_empty() && citations.is_empty() && inline_citations.is_empty();

        Ok(json!({
            "success": true,
            "provider": "xai",
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
    fn normalize_handles_strips_at_and_blanks() {
        let out = normalize_handles(
            &["@alice".to_string(), "  ".to_string(), "bob".to_string()],
            "allowed_x_handles",
        )
        .unwrap();
        assert_eq!(out, vec!["alice".to_string(), "bob".to_string()]);
    }

    #[test]
    fn normalize_handles_enforces_max() {
        let many: Vec<String> = (0..11).map(|i| format!("h{i}")).collect();
        assert!(normalize_handles(&many, "allowed_x_handles").is_err());
    }

    #[test]
    fn parse_iso_date_strict() {
        assert_eq!(
            parse_iso_date("2026-10-19", "from_date").unwrap(),
            Some(NaiveDate::from_ymd_opt(2026, 10, 19).unwrap())
        );
        assert_eq!(parse_iso_date("", "from_date").unwrap(), None);
        assert_eq!(parse_iso_date("   ", "to_date").unwrap(), None);
        assert!(parse_iso_date("19-10-2026", "from_date").is_err());
        assert!(parse_iso_date("2026-13-01", "from_date").is_err());
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
        let r = build_tool_def(&["a".to_string()], &["b".to_string()], "", "", false, false);
        assert!(r.is_err());
    }

    #[test]
    fn build_tool_def_tracks_active_filters() {
        let (def, filters) =
            build_tool_def(&[], &["spam".to_string()], "2026-01-01", "", true, false).unwrap();
        assert_eq!(def["type"], "x_search");
        assert_eq!(def["excluded_x_handles"], json!(["spam"]));
        assert_eq!(def["from_date"], "2026-01-01");
        assert_eq!(def["enable_image_understanding"], true);
        assert!(filters.contains(&"excluded_x_handles".to_string()));
        assert!(filters.contains(&"from_date".to_string()));
    }

    #[test]
    fn extract_response_text_prefers_output_text() {
        let p = json!({"output_text": "hello", "output": []});
        assert_eq!(extract_response_text(&p), "hello");
    }

    #[test]
    fn extract_response_text_falls_back_to_message_parts() {
        let p = json!({"output": [{"type": "message", "content": [
            {"type": "output_text", "text": "part one"},
            {"type": "output_text", "text": "part two"},
        ]}]});
        assert_eq!(extract_response_text(&p), "part one\n\npart two");
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
    }
}
