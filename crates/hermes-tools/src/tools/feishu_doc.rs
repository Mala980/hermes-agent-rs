//! feishu_doc_read: read a Feishu/Lark document as plain text.
//!
//! Port of `tools/feishu_doc_tool.py`. The Python version uses the `lark_oapi`
//! SDK inside a Feishu comment context; this port uses raw HTTPS instead:
//! app_id/app_secret (env `FEISHU_APP_ID` / `FEISHU_APP_SECRET`) are exchanged
//! for a tenant access token, which is then used to read the document's raw
//! content.

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Value};

use hermes_core::{tool_schema, JsonSchema, ToolError, ToolHandler, ToolSchema};

const TOKEN_URL: &str = "https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal";
const RAW_CONTENT_URL: &str =
    "https://open.feishu.cn/open-apis/docx/v1/documents/{doc_token}/raw_content";

/// Read Feishu/Lark document content.
pub struct FeishuDocHandler {
    app_id: Option<String>,
    app_secret: Option<String>,
    client: reqwest::Client,
}

impl FeishuDocHandler {
    pub fn new() -> Self {
        Self {
            app_id: std::env::var("FEISHU_APP_ID")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            app_secret: std::env::var("FEISHU_APP_SECRET")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            client: reqwest::Client::new(),
        }
    }

    pub fn with_credentials(app_id: impl Into<String>, app_secret: impl Into<String>) -> Self {
        Self {
            app_id: Some(app_id.into()),
            app_secret: Some(app_secret.into()),
            client: reqwest::Client::new(),
        }
    }

    async fn tenant_access_token(
        &self,
        app_id: &str,
        app_secret: &str,
    ) -> Result<String, ToolError> {
        let resp = self
            .client
            .post(TOKEN_URL)
            .json(&json!({"app_id": app_id, "app_secret": app_secret}))
            .send()
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("Feishu token request failed: {e}")))?;
        let body: Value = resp.json().await.map_err(|e| {
            ToolError::ExecutionFailed(format!("Feishu token response invalid JSON: {e}"))
        })?;
        parse_token_response(&body)
    }

    async fn raw_content(&self, token: &str, doc_token: &str) -> Result<String, ToolError> {
        let url = RAW_CONTENT_URL.replace("{doc_token}", doc_token);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("Feishu doc request failed: {e}")))?;
        let status = resp.status();
        let body: Value = resp.json().await.map_err(|e| {
            ToolError::ExecutionFailed(format!("Feishu doc response invalid JSON: {e}"))
        })?;
        parse_doc_response(status.as_u16(), &body)
    }
}

impl Default for FeishuDocHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the tenant access token; Feishu signals errors via `code != 0`.
fn parse_token_response(body: &Value) -> Result<String, ToolError> {
    let code = body.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = body
            .get("msg")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        return Err(ToolError::ExecutionFailed(format!(
            "Feishu token error: code={code} msg={msg}"
        )));
    }
    body.get("tenant_access_token")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .ok_or_else(|| {
            ToolError::ExecutionFailed("Feishu token response missing tenant_access_token".into())
        })
}

/// Extract document content from the raw_content response.
fn parse_doc_response(status: u16, body: &Value) -> Result<String, ToolError> {
    let code = body.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = body
            .get("msg")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        return Err(ToolError::ExecutionFailed(format!(
            "Failed to read document: HTTP {status} code={code} msg={msg}"
        )));
    }
    Ok(body
        .get("data")
        .and_then(|d| d.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string())
}

#[async_trait]
impl ToolHandler for FeishuDocHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let doc_token = params
            .get("doc_token")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| ToolError::InvalidParams("doc_token is required".into()))?;

        let app_id = self
            .app_id
            .clone()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                ToolError::ExecutionFailed(
                    "Feishu credentials not configured. Set FEISHU_APP_ID and FEISHU_APP_SECRET."
                        .into(),
                )
            })?;
        let app_secret = self
            .app_secret
            .clone()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                ToolError::ExecutionFailed(
                    "Feishu credentials not configured. Set FEISHU_APP_ID and FEISHU_APP_SECRET."
                        .into(),
                )
            })?;

        let token = self
            .tenant_access_token(app_id.trim(), app_secret.trim())
            .await?;
        let content = self.raw_content(&token, doc_token).await?;
        Ok(json!({"success": true, "content": content}).to_string())
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "doc_token".into(),
            json!({
                "type": "string",
                "description": "The document token (from the document URL)."
            }),
        );
        tool_schema(
            "feishu_doc_read",
            "Read the full content of a Feishu/Lark document as plain text.",
            JsonSchema::object(props, vec!["doc_token".into()]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_token_response_ok() {
        let body = json!({"code": 0, "msg": "ok", "tenant_access_token": "t-abc", "expire": 7200});
        assert_eq!(parse_token_response(&body).unwrap(), "t-abc");
    }

    #[test]
    fn parse_token_response_error_code() {
        let body = json!({"code": 10014, "msg": "app id not found"});
        let err = parse_token_response(&body).unwrap_err();
        assert!(err.to_string().contains("10014"));
    }

    #[test]
    fn parse_token_response_missing_token() {
        let body = json!({"code": 0, "msg": "ok"});
        assert!(parse_token_response(&body).is_err());
    }

    #[test]
    fn parse_doc_response_ok() {
        let body = json!({"code": 0, "msg": "success", "data": {"content": "hello doc"}});
        assert_eq!(parse_doc_response(200, &body).unwrap(), "hello doc");
    }

    #[test]
    fn parse_doc_response_error() {
        let body = json!({"code": 234001, "msg": "document not found"});
        let err = parse_doc_response(404, &body).unwrap_err();
        assert!(err.to_string().contains("234001"));
    }

    #[test]
    fn parse_doc_response_empty_content() {
        let body = json!({"code": 0, "data": {}});
        assert_eq!(parse_doc_response(200, &body).unwrap(), "");
    }

    #[test]
    fn schema_has_expected_shape() {
        let s = FeishuDocHandler::new().schema();
        assert_eq!(s.name, "feishu_doc_read");
    }
}
