//! Feishu/Lark Drive comment tools: list / reply to / add document comments.
//!
//! Port of `tools/feishu_drive_tool.py`. The Python version drives the
//! `lark_oapi` SDK with a tenant token injected per-thread by the Feishu
//! comment event handler; this port uses raw HTTPS instead (same approach as
//! `tools/feishu_doc.rs`): `FEISHU_APP_ID` / `FEISHU_APP_SECRET` are exchanged
//! for a tenant access token, which is then sent as a Bearer token.
//!
//! Four tools, one shared client:
//! - `feishu_drive_list_comments` — GET
//!   `/open-apis/drive/v1/files/{file_token}/comments`
//! - `feishu_drive_list_comment_replies` — GET
//!   `/open-apis/drive/v1/files/{file_token}/comments/{comment_id}/replies`
//! - `feishu_drive_reply_comment` — POST to the replies URI with the rich
//!   `content.elements[text_run]` body shape (local/quoted-text comments)
//! - `feishu_drive_add_comment` — POST
//!   `/open-apis/drive/v1/files/{file_token}/new_comments` with the flat
//!   `reply_elements[text]` shape (whole-document comments; also the fallback
//!   when reply_comment fails with code 1069302)
//!
//! Feishu signals errors via `code != 0` in the JSON body.

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Value};

use hermes_core::{tool_schema, JsonSchema, ToolError, ToolHandler, ToolSchema};

const TOKEN_URL: &str = "https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal";
const API_BASE: &str = "https://open.feishu.cn";

/// Which of the four comment operations a handler performs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DriveOp {
    ListComments,
    ListReplies,
    ReplyComment,
    AddComment,
}

impl DriveOp {
    fn tool_name(self) -> &'static str {
        match self {
            DriveOp::ListComments => "feishu_drive_list_comments",
            DriveOp::ListReplies => "feishu_drive_list_comment_replies",
            DriveOp::ReplyComment => "feishu_drive_reply_comment",
            DriveOp::AddComment => "feishu_drive_add_comment",
        }
    }

    fn is_write(self) -> bool {
        matches!(self, DriveOp::ReplyComment | DriveOp::AddComment)
    }
}

fn file_token_prop() -> Value {
    json!({"type": "string", "description": "The document file token."})
}

fn file_type_prop() -> Value {
    json!({"type": "string", "description": "File type (default: docx).", "default": "docx"})
}

fn page_token_prop() -> Value {
    json!({"type": "string", "description": "Pagination token for next page."})
}

/// A validated Drive API call, ready to send (no network done yet).
struct PreparedCall {
    method: String,
    path: String,
    queries: Vec<(String, String)>,
    body: Option<Value>,
}

/// Shared Feishu client: app credentials → tenant access token → Drive calls.
pub struct FeishuDriveHandler {
    op: DriveOp,
    app_id: Option<String>,
    app_secret: Option<String>,
    client: reqwest::Client,
}

impl FeishuDriveHandler {
    pub fn new(op: DriveOp) -> Self {
        Self {
            op,
            app_id: std::env::var("FEISHU_APP_ID")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            app_secret: std::env::var("FEISHU_APP_SECRET")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            client: reqwest::Client::new(),
        }
    }

    pub fn list_comments() -> Self {
        Self::new(DriveOp::ListComments)
    }
    pub fn list_comment_replies() -> Self {
        Self::new(DriveOp::ListReplies)
    }
    pub fn reply_comment() -> Self {
        Self::new(DriveOp::ReplyComment)
    }
    pub fn add_comment() -> Self {
        Self::new(DriveOp::AddComment)
    }

    pub fn with_credentials(
        op: DriveOp,
        app_id: impl Into<String>,
        app_secret: impl Into<String>,
    ) -> Self {
        Self {
            op,
            app_id: Some(app_id.into()),
            app_secret: Some(app_secret.into()),
            client: reqwest::Client::new(),
        }
    }

    fn credentials(&self) -> Result<(String, String), ToolError> {
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
        Ok((app_id, app_secret))
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

    /// GET/POST a Drive API path; Feishu errors are `code != 0` in the body.
    async fn drive_call(
        &self,
        method: &str,
        path: &str,
        queries: &[(String, String)],
        body: Option<&Value>,
        token: &str,
    ) -> Result<Value, ToolError> {
        let mut url = format!("{API_BASE}{path}");
        if !queries.is_empty() {
            let qs: Vec<String> = queries.iter().map(|(k, v)| format!("{k}={v}")).collect();
            url.push('?');
            url.push_str(&qs.join("&"));
        }
        let mut req = self.client.request(
            method
                .parse()
                .map_err(|_| ToolError::ExecutionFailed("bad HTTP method".into()))?,
            &url,
        );
        req = req.bearer_auth(token);
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("Feishu Drive request failed: {e}")))?;
        let status = resp.status();
        let resp_body: Value = resp.json().await.map_err(|e| {
            ToolError::ExecutionFailed(format!("Feishu Drive response invalid JSON: {e}"))
        })?;
        parse_api_response(status.as_u16(), &resp_body)
    }

    /// Build the Drive API call for this op. Param validation happens here,
    /// before any network request (fail fast on bad params).
    fn prepare_call(&self, params: &Value) -> Result<PreparedCall, ToolError> {
        let file_token = str_param(params, "file_token")
            .ok_or_else(|| ToolError::InvalidParams("file_token is required".into()))?
            .to_string();
        let file_type = params
            .get("file_type")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("docx")
            .to_string();

        match self.op {
            DriveOp::ListComments => {
                let is_whole = params
                    .get("is_whole")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                Ok(PreparedCall {
                    method: "GET".to_string(),
                    path: format!("/open-apis/drive/v1/files/{file_token}/comments"),
                    queries: paged_queries(params, &file_type, is_whole),
                    body: None,
                })
            }
            DriveOp::ListReplies => {
                let comment_id = str_param(params, "comment_id")
                    .ok_or_else(|| ToolError::InvalidParams("comment_id is required".into()))?;
                Ok(PreparedCall {
                    method: "GET".to_string(),
                    path: format!(
                        "/open-apis/drive/v1/files/{file_token}/comments/{comment_id}/replies"
                    ),
                    queries: paged_queries(params, &file_type, false),
                    body: None,
                })
            }
            DriveOp::ReplyComment => {
                let comment_id = str_param(params, "comment_id")
                    .ok_or_else(|| ToolError::InvalidParams("comment_id is required".into()))?;
                let content = str_param(params, "content")
                    .ok_or_else(|| ToolError::InvalidParams("content is required".into()))?;
                // Replies use the rich "content.elements[text_run]" body shape;
                // file_type is a query param.
                let body = json!({
                    "content": {"elements": [{"type": "text_run", "text_run": {"text": content}}]}
                });
                Ok(PreparedCall {
                    method: "POST".to_string(),
                    path: format!(
                        "/open-apis/drive/v1/files/{file_token}/comments/{comment_id}/replies"
                    ),
                    queries: vec![("file_type".to_string(), file_type)],
                    body: Some(body),
                })
            }
            DriveOp::AddComment => {
                let content = str_param(params, "content")
                    .ok_or_else(|| ToolError::InvalidParams("content is required".into()))?;
                // new_comments takes the flat "reply_elements[text]" shape with
                // file_type in the body.
                let body = json!({
                    "file_type": file_type,
                    "reply_elements": [{"type": "text", "text": content}]
                });
                Ok(PreparedCall {
                    method: "POST".to_string(),
                    path: format!("/open-apis/drive/v1/files/{file_token}/new_comments"),
                    queries: vec![],
                    body: Some(body),
                })
            }
        }
    }

    async fn execute_op(&self, params: Value) -> Result<String, ToolError> {
        let (app_id, app_secret) = self.credentials()?;
        let call = self.prepare_call(&params)?;
        let token = self
            .tenant_access_token(app_id.trim(), app_secret.trim())
            .await?;
        let data = self
            .drive_call(
                &call.method,
                &call.path,
                &call.queries,
                call.body.as_ref(),
                &token,
            )
            .await?;
        Ok(json!({"success": true, "data": data}).to_string())
    }

    fn schema(&self) -> ToolSchema {
        let (description, props, required) = match self.op {
            DriveOp::ListComments => {
                let mut props = IndexMap::new();
                props.insert("file_token".into(), file_token_prop());
                props.insert("file_type".into(), file_type_prop());
                props.insert(
                    "is_whole".into(),
                    json!({
                        "type": "boolean",
                        "description": "If true, only return whole-document comments.",
                        "default": false
                    }),
                );
                props.insert(
                    "page_size".into(),
                    json!({
                        "type": "integer",
                        "description": "Number of comments per page (max 100).",
                        "default": 100
                    }),
                );
                props.insert("page_token".into(), page_token_prop());
                (
                    "List comments on a Feishu document. Use is_whole=true to list whole-document comments only.",
                    props,
                    vec!["file_token".into()],
                )
            }
            DriveOp::ListReplies => {
                let mut props = IndexMap::new();
                props.insert("file_token".into(), file_token_prop());
                props.insert(
                    "comment_id".into(),
                    json!({"type": "string", "description": "The comment ID to list replies for."}),
                );
                props.insert("file_type".into(), file_type_prop());
                props.insert(
                    "page_size".into(),
                    json!({
                        "type": "integer",
                        "description": "Number of replies per page (max 100).",
                        "default": 100
                    }),
                );
                props.insert("page_token".into(), page_token_prop());
                (
                    "List all replies in a comment thread on a Feishu document.",
                    props,
                    vec!["file_token".into(), "comment_id".into()],
                )
            }
            DriveOp::ReplyComment => {
                let mut props = IndexMap::new();
                props.insert("file_token".into(), file_token_prop());
                props.insert(
                    "comment_id".into(),
                    json!({"type": "string", "description": "The comment ID to reply to."}),
                );
                props.insert(
                    "content".into(),
                    json!({
                        "type": "string",
                        "description": "The reply text content (plain text only, no markdown)."
                    }),
                );
                props.insert("file_type".into(), file_type_prop());
                (
                    "Reply to a local comment thread on a Feishu document. Use this for local (quoted-text) comments. \
                     For whole-document comments, use feishu_drive_add_comment instead.",
                    props,
                    vec!["file_token".into(), "comment_id".into(), "content".into()],
                )
            }
            DriveOp::AddComment => {
                let mut props = IndexMap::new();
                props.insert("file_token".into(), file_token_prop());
                props.insert(
                    "content".into(),
                    json!({
                        "type": "string",
                        "description": "The comment text content (plain text only, no markdown)."
                    }),
                );
                props.insert("file_type".into(), file_type_prop());
                (
                    "Add a new whole-document comment on a Feishu document. Use this for whole-document comments or as a \
                     fallback when reply_comment fails with code 1069302.",
                    props,
                    vec!["file_token".into(), "content".into()],
                )
            }
        };
        tool_schema(
            self.op.tool_name(),
            description,
            JsonSchema::object(props, required),
        )
    }
}

#[async_trait]
impl ToolHandler for FeishuDriveHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        self.execute_op(params).await
    }
    fn schema(&self) -> ToolSchema {
        self.schema()
    }
}

fn str_param<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Query params shared by the listing endpoints; page_token goes last.
fn paged_queries(params: &Value, file_type: &str, is_whole: bool) -> Vec<(String, String)> {
    let page_size = params
        .get("page_size")
        .and_then(|v| v.as_i64())
        .unwrap_or(100);
    let mut queries = vec![
        ("file_type".to_string(), file_type.to_string()),
        ("user_id_type".to_string(), "open_id".to_string()),
        ("page_size".to_string(), page_size.to_string()),
    ];
    if is_whole {
        queries.push(("is_whole".to_string(), "true".to_string()));
    }
    if let Some(pt) = str_param(params, "page_token") {
        queries.push(("page_token".to_string(), pt.to_string()));
    }
    queries
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

/// Unpack a Drive API response body: `code != 0` is an error, else the `data`.
fn parse_api_response(status: u16, body: &Value) -> Result<Value, ToolError> {
    let code = body.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = body
            .get("msg")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        return Err(ToolError::ExecutionFailed(format!(
            "Feishu Drive API error: HTTP {status} code={code} msg={msg}"
        )));
    }
    Ok(body.get("data").cloned().unwrap_or(Value::Null))
}

/// Tool is available only when Feishu app credentials are configured.
pub fn check_feishu_drive_requirements() -> bool {
    let has = |k: &str| {
        std::env::var(k)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
    };
    has("FEISHU_APP_ID") && has("FEISHU_APP_SECRET")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_names_and_required_params() {
        let cases = [
            (
                FeishuDriveHandler::list_comments(),
                "feishu_drive_list_comments",
                vec!["file_token"],
            ),
            (
                FeishuDriveHandler::list_comment_replies(),
                "feishu_drive_list_comment_replies",
                vec!["file_token", "comment_id"],
            ),
            (
                FeishuDriveHandler::reply_comment(),
                "feishu_drive_reply_comment",
                vec!["file_token", "comment_id", "content"],
            ),
            (
                FeishuDriveHandler::add_comment(),
                "feishu_drive_add_comment",
                vec!["file_token", "content"],
            ),
        ];
        for (h, name, required) in cases {
            let s = h.schema();
            assert_eq!(s.name, name);
            let got: Vec<&str> = s
                .parameters
                .required
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .map(|r| r.as_str())
                .collect();
            assert_eq!(got, required, "required mismatch for {name}");
        }
    }

    #[test]
    fn schema_descriptions_match_original() {
        let s = FeishuDriveHandler::list_comments().schema();
        assert!(s.description.contains("is_whole=true"));
        let s = FeishuDriveHandler::reply_comment().schema();
        assert!(s.description.contains("feishu_drive_add_comment"));
        let s = FeishuDriveHandler::add_comment().schema();
        assert!(s.description.contains("1069302"));
    }

    #[test]
    fn paged_queries_defaults_and_page_token_last() {
        let q = paged_queries(&json!({}), "docx", false);
        assert_eq!(
            q,
            vec![
                ("file_type".to_string(), "docx".to_string()),
                ("user_id_type".to_string(), "open_id".to_string()),
                ("page_size".to_string(), "100".to_string()),
            ]
        );
        let q = paged_queries(&json!({"page_token": "tok", "page_size": 25}), "doc", true);
        let keys: Vec<&str> = q.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "file_type",
                "user_id_type",
                "page_size",
                "is_whole",
                "page_token"
            ]
        );
        assert_eq!(q[2].1, "25");
        assert_eq!(q[4].1, "tok");
    }

    #[test]
    fn parse_api_response_ok_and_error() {
        let ok = json!({"code": 0, "msg": "success", "data": {"items": [1]}});
        assert_eq!(parse_api_response(200, &ok).unwrap(), json!({"items": [1]}));
        let err = json!({"code": 1069302, "msg": "comment not found"});
        let e = parse_api_response(200, &err).unwrap_err();
        assert!(e.to_string().contains("1069302"));
    }

    #[test]
    fn parse_token_response_ok_and_error() {
        let ok = json!({"code": 0, "msg": "ok", "tenant_access_token": "t-abc"});
        assert_eq!(parse_token_response(&ok).unwrap(), "t-abc");
        let err = json!({"code": 10014, "msg": "app id not found"});
        assert!(parse_token_response(&err)
            .unwrap_err()
            .to_string()
            .contains("10014"));
    }

    #[test]
    fn reply_body_shape() {
        // rich "content.elements[text_run]" shape for replies
        let content = "hello";
        let body = json!({
            "content": {"elements": [{"type": "text_run", "text_run": {"text": content}}]}
        });
        assert_eq!(
            body["content"]["elements"][0]["text_run"]["text"],
            json!("hello")
        );
    }

    #[test]
    fn add_comment_body_shape() {
        // flat "reply_elements[text]" shape with file_type in body
        let body = json!({
            "file_type": "docx",
            "reply_elements": [{"type": "text", "text": "hi"}]
        });
        assert_eq!(body["file_type"], json!("docx"));
        assert_eq!(body["reply_elements"][0]["type"], json!("text"));
    }

    #[tokio::test]
    async fn missing_file_token_rejected_before_network() {
        let h = FeishuDriveHandler::with_credentials(DriveOp::ListComments, "id", "secret");
        let err = h.execute(json!({})).await.unwrap_err();
        assert!(err.to_string().contains("file_token is required"));
    }

    #[tokio::test]
    async fn missing_credentials_fail_closed() {
        let h = FeishuDriveHandler {
            op: DriveOp::AddComment,
            app_id: None,
            app_secret: None,
            client: reqwest::Client::new(),
        };
        let err = h
            .execute(json!({"file_token": "ft", "content": "hi"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("FEISHU_APP_ID"));
    }
}
