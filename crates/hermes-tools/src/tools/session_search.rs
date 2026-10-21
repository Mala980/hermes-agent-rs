//! Session search tool: search past conversations using FTS5

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Value};

use hermes_core::{tool_schema, JsonSchema, ToolError, ToolHandler, ToolSchema};

use std::sync::Arc;

// ---------------------------------------------------------------------------
// SessionSearchBackend trait
// ---------------------------------------------------------------------------

/// One archived conversation message, for the read/scroll shapes.
#[derive(Debug, Clone)]
pub struct SessionMessage {
    /// Role as stored ("user", "assistant", "tool", ...).
    pub role: String,
    /// Message content.
    pub content: String,
}

/// Backend for searching past conversation sessions.
#[async_trait]
pub trait SessionSearchBackend: Send + Sync {
    /// Search past conversations using FTS5 full-text search.
    async fn search(
        &self,
        query: Option<&str>,
        role_filter: Option<&str>,
        limit: usize,
        current_session_id: Option<&str>,
    ) -> Result<String, ToolError>;

    /// Load one session's messages for the read/scroll shapes.
    ///
    /// Returns `Ok(None)` when the session id is unknown. The default
    /// implementation reports "not supported" so existing backends keep
    /// compiling.
    async fn session_messages(
        &self,
        session_id: &str,
    ) -> Result<Option<Vec<SessionMessage>>, ToolError> {
        let _ = session_id;
        Err(ToolError::NotFound(
            "session read is not supported by this backend".into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// SessionSearchHandler
// ---------------------------------------------------------------------------

/// Tool for searching past conversation sessions using FTS5.
pub struct SessionSearchHandler {
    backend: Arc<dyn SessionSearchBackend>,
}

impl SessionSearchHandler {
    pub fn new(backend: Arc<dyn SessionSearchBackend>) -> Self {
        Self { backend }
    }
}

/// Max chars of one message's content in the read/scroll shapes (one
/// archived message can be tens of thousands of chars).
const READ_MAX_CONTENT: usize = 2000;
/// Read shape: head + tail when the session is long.
const READ_HEAD: usize = 20;
const READ_TAIL: usize = 10;

impl SessionSearchHandler {
    async fn load(&self, session_id: &str) -> Result<Vec<SessionMessage>, ToolError> {
        self.backend
            .session_messages(session_id)
            .await?
            .ok_or_else(|| ToolError::NotFound(format!("session_id not found: {session_id}")))
    }

    fn shape_message(idx: usize, msg: &SessionMessage, anchor: bool) -> String {
        let mut text = msg.content.clone();
        if text.chars().count() > READ_MAX_CONTENT {
            let total = text.chars().count();
            let cut: String = text.chars().take(READ_MAX_CONTENT).collect();
            text = format!("{cut}… [truncated from {total} chars]");
        }
        let mark = if anchor { " → anchor" } else { "" };
        format!("[{idx}] {}{mark}: {text}", msg.role)
    }

    /// Read shape: one full session (head + tail when long).
    async fn read(&self, session_id: &str) -> Result<String, ToolError> {
        let msgs = self.load(session_id).await?;
        let total = msgs.len();
        let head = format!("mode=read, session_id={session_id}, {total} messages");
        if total <= READ_HEAD + READ_TAIL {
            let body: Vec<String> = msgs
                .iter()
                .enumerate()
                .map(|(i, m)| Self::shape_message(i, m, false))
                .collect();
            return Ok(format!("{head}\n{}", body.join("\n")));
        }
        let mut body: Vec<String> = msgs
            .iter()
            .take(READ_HEAD)
            .enumerate()
            .map(|(i, m)| Self::shape_message(i, m, false))
            .collect();
        body.push(format!(
            "… [truncated: session has {total} messages; showing first {READ_HEAD} + last {READ_TAIL}. \
             For the middle: session_search(session_id=\"{session_id}\", around=<index>)]"
        ));
        body.extend(
            msgs.iter()
                .enumerate()
                .skip(total - READ_TAIL)
                .map(|(i, m)| Self::shape_message(i, m, false)),
        );
        Ok(format!("{head}\n{}", body.join("\n")))
    }

    /// Scroll shape: a ±window of messages around a 0-based anchor index.
    async fn scroll(
        &self,
        session_id: &str,
        around: usize,
        window: usize,
    ) -> Result<String, ToolError> {
        let msgs = self.load(session_id).await?;
        let total = msgs.len();
        if around >= total {
            return Err(ToolError::InvalidParams(format!(
                "around={around} out of range: session has {total} messages (indices 0–{})",
                total.saturating_sub(1)
            )));
        }
        let lo = around.saturating_sub(window);
        let hi = (around + window + 1).min(total);
        let body: Vec<String> = msgs
            .iter()
            .enumerate()
            .skip(lo)
            .take(hi - lo)
            .map(|(i, m)| Self::shape_message(i, m, i == around))
            .collect();
        Ok(format!(
            "mode=scroll, session_id={session_id}, indices {lo}–{} of {total} messages (anchor: {around}). \
             Forward: around=<last index>; back: around=<first index>.\n{}",
            hi.saturating_sub(1),
            body.join("\n")
        ))
    }
}

#[async_trait]
impl ToolHandler for SessionSearchHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        // Four shapes (parity with the original's session_search tool):
        // - session_id + around => scroll (message window around an anchor)
        // - session_id            => read (one full session)
        // - query                 => discover (FTS5 across sessions)
        // - nothing               => browse (recent sessions)
        if let Some(sid) = params.get("session_id").and_then(|v| v.as_str()) {
            let sid = sid.trim();
            if sid.is_empty() {
                return Err(ToolError::InvalidParams(
                    "session_id cannot be empty".into(),
                ));
            }
            let around = params
                .get("around")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
            let window = params
                .get("window")
                .and_then(|v| v.as_u64())
                .unwrap_or(5)
                .clamp(1, 20) as usize;
            return match around {
                Some(a) => self.scroll(sid, a, window).await,
                None => self.read(sid).await,
            };
        }

        let query = params.get("query").and_then(|v| v.as_str());
        let role_filter = params.get("role_filter").and_then(|v| v.as_str());
        let current_session_id = params.get("current_session_id").and_then(|v| v.as_str());
        let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(3) as usize;
        let capped_limit = limit.min(5);
        self.backend
            .search(query, role_filter, capped_limit, current_session_id)
            .await
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "query".into(),
            json!({
                "type": "string",
                "description": "Search query for finding past conversations. Omit to list recent sessions."
            }),
        );
        props.insert(
            "role_filter".into(),
            json!({
                "type": "string",
                "description": "Optional comma-separated roles to include, e.g. 'user,assistant'."
            }),
        );
        props.insert(
            "limit".into(),
            json!({
                "type": "integer",
                "description": "Max sessions to return/summarize (default: 3, max: 5).",
                "default": 3
            }),
        );
        props.insert(
            "current_session_id".into(),
            json!({
                "type": "string",
                "description": "Optional active session id used to exclude current lineage from search results."
            }),
        );
        props.insert(
            "session_id".into(),
            json!({
                "type": "string",
                "description": "Read shape: id of one session to read fully (head + tail when long). With 'around': scroll shape instead."
            }),
        );
        props.insert(
            "around".into(),
            json!({
                "type": "integer",
                "description": "Scroll shape: 0-based message index anchoring a ±window of messages. Requires 'session_id'."
            }),
        );
        props.insert(
            "window".into(),
            json!({
                "type": "integer",
                "description": "Scroll shape: messages shown on each side of the anchor (default: 5, max: 20).",
                "default": 5
            }),
        );

        tool_schema(
            "session_search",
            "Recall past conversations. Four shapes chosen by args: 'query' = discover (matching sessions); 'session_id' + 'around' = scroll (message window around an anchor index); 'session_id' alone = read (one full session); no args = browse (recent sessions).",
            JsonSchema::object(props, vec![]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockSessionSearchBackend;
    #[async_trait]
    impl SessionSearchBackend for MockSessionSearchBackend {
        async fn search(
            &self,
            query: Option<&str>,
            role_filter: Option<&str>,
            limit: usize,
            current_session_id: Option<&str>,
        ) -> Result<String, ToolError> {
            Ok(format!(
                "Found {} results for '{:?}' with role_filter={:?}, current_session_id={:?} (limit: {})",
                0, query, role_filter, current_session_id, limit
            ))
        }
    }

    #[tokio::test]
    async fn test_session_search_schema() {
        let handler = SessionSearchHandler::new(Arc::new(MockSessionSearchBackend));
        assert_eq!(handler.schema().name, "session_search");
    }

    #[tokio::test]
    async fn test_session_search_execute() {
        let handler = SessionSearchHandler::new(Arc::new(MockSessionSearchBackend));
        let result = handler
            .execute(json!({"query": "rust async"}))
            .await
            .unwrap();
        assert!(result.contains("rust async"));
    }

    struct MockReadableBackend {
        messages: Vec<SessionMessage>,
    }
    #[async_trait]
    impl SessionSearchBackend for MockReadableBackend {
        async fn search(
            &self,
            _query: Option<&str>,
            _role_filter: Option<&str>,
            _limit: usize,
            _current_session_id: Option<&str>,
        ) -> Result<String, ToolError> {
            Ok("browse/discover".into())
        }
        async fn session_messages(
            &self,
            session_id: &str,
        ) -> Result<Option<Vec<SessionMessage>>, ToolError> {
            if session_id == "s1" {
                Ok(Some(self.messages.clone()))
            } else {
                Ok(None)
            }
        }
    }

    fn readable_handler() -> SessionSearchHandler {
        let messages = (0..10)
            .map(|i| SessionMessage {
                role: if i % 2 == 0 {
                    "user".into()
                } else {
                    "assistant".into()
                },
                content: format!("message {i}"),
            })
            .collect();
        SessionSearchHandler::new(Arc::new(MockReadableBackend { messages }))
    }

    #[tokio::test]
    async fn test_session_search_read_shape() {
        let result = readable_handler()
            .execute(json!({"session_id": "s1"}))
            .await
            .unwrap();
        assert!(result.contains("mode=read"));
        assert!(result.contains("[0] user: message 0"));
        assert!(result.contains("[9] assistant: message 9"));
    }

    #[tokio::test]
    async fn test_session_search_read_unknown_session() {
        let err = readable_handler()
            .execute(json!({"session_id": "nope"}))
            .await
            .expect_err("unknown session must fail");
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_session_search_scroll_shape() {
        let result = readable_handler()
            .execute(json!({"session_id": "s1", "around": 5, "window": 2}))
            .await
            .unwrap();
        assert!(result.contains("mode=scroll"));
        assert!(result.contains("[5] assistant → anchor: message 5"));
        assert!(result.contains("[3] assistant: message 3"));
        assert!(result.contains("[7] assistant: message 7"));
        // Outside the window: not present.
        assert!(!result.contains("[2] user: message 2"));
        assert!(!result.contains("[8] user: message 8"));
    }

    #[tokio::test]
    async fn test_session_search_scroll_clamps_edges() {
        let result = readable_handler()
            .execute(json!({"session_id": "s1", "around": 0, "window": 5}))
            .await
            .unwrap();
        assert!(result.contains("[0] user → anchor: message 0"));
        assert!(result.contains("[5] assistant: message 5"));
        assert!(!result.contains("[6] user: message 6"));
    }

    #[tokio::test]
    async fn test_session_search_scroll_out_of_range() {
        let err = readable_handler()
            .execute(json!({"session_id": "s1", "around": 99}))
            .await
            .expect_err("out-of-range anchor must fail");
        assert!(err.to_string().contains("out of range"));
    }

    #[tokio::test]
    async fn test_session_search_read_truncates_long_message() {
        let backend = MockReadableBackend {
            messages: vec![SessionMessage {
                role: "user".into(),
                content: "y".repeat(5000),
            }],
        };
        let result = SessionSearchHandler::new(Arc::new(backend))
            .execute(json!({"session_id": "s1"}))
            .await
            .unwrap();
        assert!(result.contains("truncated from 5000 chars"));
    }
}
