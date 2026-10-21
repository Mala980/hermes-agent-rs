//! File tools: read, write, patch, and search

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use hermes_core::{tool_schema, JsonSchema, TerminalBackend, ToolError, ToolHandler, ToolSchema};

use std::path::Path;
use std::sync::Arc;

use crate::credential_guard::CredentialGuard;

/// Maximum lines returned per `read_file` page (matches the reference).
const MAX_READ_LINES: u64 = 2000;
/// Soft cap on page text, mirroring the reference's ~100K char read budget.
const MAX_PAGE_CHARS: usize = 100_000;

/// `sha256:<hex>` token over the given content. Emitted by `read_file` as the
/// `content_hash` trailer and accepted by `write_file` as `expected_hash` for
/// the stale-write guard.
pub(crate) fn content_hash(content: &str) -> String {
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    format!("sha256:{:x}", h.finalize())
}

/// Accept `sha256:<hex>` or bare-hex hash tokens from callers.
pub(crate) fn normalize_hash_token(token: &str) -> &str {
    token.strip_prefix("sha256:").unwrap_or(token)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

// ---------------------------------------------------------------------------
// ReadFileHandler
// ---------------------------------------------------------------------------

/// Tool for reading file contents via the terminal backend.
pub struct ReadFileHandler {
    backend: Arc<dyn TerminalBackend>,
}

impl ReadFileHandler {
    pub fn new(backend: Arc<dyn TerminalBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl ToolHandler for ReadFileHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let path = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("Missing 'path' parameter".into()))?;

        CredentialGuard::new().check_read_access(Path::new(path))?;

        // 1-indexed, per schema; clamp the page size like the reference.
        let offset = params
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1);
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(MAX_READ_LINES)
            .clamp(1, MAX_READ_LINES);

        let content = self
            .backend
            .read_file(path, None, None)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let start = ((offset as usize).saturating_sub(1)).min(total);
        let end = (start + limit as usize).min(total);
        let page = &lines[start..end];

        let numbered: Vec<String> = page
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{:6}  {}", offset as usize + i, l))
            .collect();

        // Offset of the following page; absent (no trailer) once EOF is reached.
        let next_offset = if end < total {
            Some(offset + page.len() as u64)
        } else {
            None
        };

        let last_shown = offset + page.len().saturating_sub(1) as u64;
        let mut body = format!(
            "--- {} (lines {}-{} of {}) ---\n{}",
            path,
            offset,
            last_shown,
            total,
            truncate_chars(&numbered.join("\n"), MAX_PAGE_CHARS),
        );
        if let Some(n) = next_offset {
            body.push_str(&format!("\nnext_offset: {n}"));
        }
        body.push_str(&format!("\ncontent_hash: {}", content_hash(&content)));
        Ok(body)
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "path".into(),
            json!({
                "type": "string",
                "description": "The file path to read"
            }),
        );
        props.insert(
            "offset".into(),
            json!({
                "type": "integer",
                "description": "Line number to start reading from (1-indexed). Use the `next_offset` trailer value from the previous result to read the next page."
            }),
        );
        props.insert(
            "limit".into(),
            json!({
                "type": "integer",
                "description": "Maximum number of lines to read (default 2000, max 2000)"
            }),
        );

        tool_schema(
            "read_file",
            "Read file contents with optional offset and line limit. Returns the file content as a string with line numbers, plus `next_offset` (next page offset; absent at EOF) and `content_hash` (sha256 token of the full content, usable as write_file's `expected_hash`) trailer lines.",
            JsonSchema::object(props, vec!["path".into()]),
        )
    }
}

// ---------------------------------------------------------------------------
// WriteFileHandler
// ---------------------------------------------------------------------------

/// Tool for writing content to files via the terminal backend.
pub struct WriteFileHandler {
    backend: Arc<dyn TerminalBackend>,
}

impl WriteFileHandler {
    pub fn new(backend: Arc<dyn TerminalBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl ToolHandler for WriteFileHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let path = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("Missing 'path' parameter".into()))?;

        let content = params
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("Missing 'content' parameter".into()))?;

        let expected_hash = params.get("expected_hash").and_then(|v| v.as_str());
        let force = params
            .get("force")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        CredentialGuard::new().check_write_access(Path::new(path), content)?;

        // Stale-write guard: when the caller passes the `content_hash` token
        // from an earlier read_file and the file has since changed, refuse
        // unless `force: true`. Without `expected_hash`, behavior is unchanged.
        if let Some(expected) = expected_hash {
            if !force {
                let exists = self
                    .backend
                    .file_exists(path)
                    .await
                    .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
                if exists {
                    let current = self
                        .backend
                        .read_file(path, None, None)
                        .await
                        .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
                    if normalize_hash_token(&content_hash(&current))
                        != normalize_hash_token(expected)
                    {
                        return Err(ToolError::ExecutionFailed(format!(
                            "Refusing to overwrite '{}': the file changed since it was read \
                             (content hash mismatch). Re-read the file to get the latest \
                             content_hash, or pass force:true to overwrite anyway.",
                            path
                        )));
                    }
                }
            }
        }

        self.backend
            .write_file(path, content)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        Ok(format!(
            "Successfully wrote {} bytes to {}\ncontent_hash: {}",
            content.len(),
            path,
            content_hash(content)
        ))
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "path".into(),
            json!({
                "type": "string",
                "description": "The file path to write to"
            }),
        );
        props.insert(
            "content".into(),
            json!({
                "type": "string",
                "description": "The content to write to the file"
            }),
        );
        props.insert(
            "expected_hash".into(),
            json!({
                "type": "string",
                "description": "Optional: the `content_hash` token from an earlier read_file. If the file changed since it was read and `force` is not true, the write is refused."
            }),
        );
        props.insert("force".into(), json!({
            "type": "boolean",
            "description": "If true, write even when `expected_hash` does not match (default: false)",
            "default": false
        }));

        tool_schema(
            "write_file",
            "Write content to a file. Creates the file and parent directories if they don't exist. Overwrites existing content.",
            JsonSchema::object(props, vec!["path".into(), "content".into()]),
        )
    }
}

// ---------------------------------------------------------------------------
// PatchHandler
// ---------------------------------------------------------------------------

/// Backend trait for file patching operations.
#[async_trait]
pub trait PatchBackend: Send + Sync {
    /// Apply a patch to a file using fuzzy matching.
    async fn patch_file(
        &self,
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> Result<String, ToolError>;
}

/// Tool for patching files with fuzzy matching (find-and-replace).
pub struct PatchHandler {
    backend: Arc<dyn PatchBackend>,
}

impl PatchHandler {
    pub fn new(backend: Arc<dyn PatchBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl ToolHandler for PatchHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let path = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("Missing 'path' parameter".into()))?;

        let old_string = params
            .get("old_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("Missing 'old_string' parameter".into()))?;

        let new_string = params
            .get("new_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let replace_all = params
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        self.backend
            .patch_file(path, old_string, new_string, replace_all)
            .await
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "path".into(),
            json!({
                "type": "string",
                "description": "The file path to patch"
            }),
        );
        props.insert(
            "old_string".into(),
            json!({
                "type": "string",
                "description": "The text to find in the file. Must match exactly once (exact match first, then whitespace-normalized fuzzy fallback); zero or multiple matches is an error and the file is left unchanged. Include surrounding context lines to disambiguate."
            }),
        );
        props.insert(
            "new_string".into(),
            json!({
                "type": "string",
                "description": "The replacement text (use empty string to delete). Must differ from old_string."
            }),
        );
        props.insert("replace_all".into(), json!({
            "type": "boolean",
            "description": "Replace all occurrences instead of requiring a unique match (default: false)",
            "default": false
        }));

        tool_schema(
            "patch",
            "Apply targeted find-and-replace edits to a file. Minor whitespace/indentation differences won't break matching. The result includes a unified diff (context radius 3) of the change.",
            JsonSchema::object(props, vec!["path".into(), "old_string".into()]),
        )
    }
}

// ---------------------------------------------------------------------------
// SearchFilesHandler
// ---------------------------------------------------------------------------

/// Backend trait for file search operations.
#[async_trait]
pub trait SearchBackend: Send + Sync {
    /// Search file contents by regex pattern.
    async fn search_content(
        &self,
        pattern: &str,
        path: &str,
        file_glob: Option<&str>,
        max_results: Option<usize>,
    ) -> Result<String, ToolError>;

    /// Search files by name (glob pattern).
    async fn search_files(&self, pattern: &str, path: &str) -> Result<String, ToolError>;
}

/// Render content-search results per `output_mode`, mirroring the reference
/// grep modes: `content` (default, `matches` array), `files_only` (unique
/// `files` array), `count` (per-file `counts`).
fn apply_content_output_mode(raw: &str, output_mode: &str, pattern: &str) -> String {
    if output_mode == "content" {
        return raw.to_string();
    }
    let parsed: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return raw.to_string(),
    };
    let empty = Vec::new();
    let matches = parsed
        .get("matches")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    match output_mode {
        "files_only" => {
            let mut seen = std::collections::HashSet::new();
            let mut files = Vec::new();
            for m in matches {
                if let Some(f) = m.get("file").and_then(|v| v.as_str()) {
                    if seen.insert(f) {
                        files.push(json!(f));
                    }
                }
            }
            let total = files.len();
            json!({"files": files, "total": total, "pattern": pattern}).to_string()
        }
        "count" => {
            let mut counts: Vec<(String, usize)> = Vec::new();
            for m in matches {
                if let Some(f) = m.get("file").and_then(|v| v.as_str()) {
                    match counts.iter_mut().find(|(p, _)| p == f) {
                        Some((_, n)) => *n += 1,
                        None => counts.push((f.to_string(), 1)),
                    }
                }
            }
            let total_matches: usize = counts.iter().map(|(_, n)| n).sum();
            let total_files = counts.len();
            let entries: Vec<Value> = counts
                .into_iter()
                .map(|(file, count)| json!({"file": file, "count": count}))
                .collect();
            json!({
                "counts": entries,
                "total_matches": total_matches,
                "total_files": total_files,
                "pattern": pattern,
            })
            .to_string()
        }
        _ => raw.to_string(),
    }
}

/// Tool for searching files by content or filename.
pub struct SearchFilesHandler {
    backend: Arc<dyn SearchBackend>,
}

impl SearchFilesHandler {
    pub fn new(backend: Arc<dyn SearchBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl ToolHandler for SearchFilesHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let pattern = params
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("Missing 'pattern' parameter".into()))?;

        let path = params.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        let target = params
            .get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("content");

        let file_glob = params.get("file_glob").and_then(|v| v.as_str());

        let max_results = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);

        match target {
            "content" => {
                let raw = self
                    .backend
                    .search_content(pattern, path, file_glob, max_results)
                    .await?;
                let output_mode = params
                    .get("output_mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("content");
                Ok(apply_content_output_mode(&raw, output_mode, pattern))
            }
            "files" => self.backend.search_files(pattern, path).await,
            other => Err(ToolError::InvalidParams(format!(
                "Unknown target: '{}'. Use 'content' or 'files'.",
                other
            ))),
        }
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "pattern".into(),
            json!({
                "type": "string",
                "description": "Regex pattern to search for (content) or glob pattern (files)"
            }),
        );
        props.insert(
            "path".into(),
            json!({
                "type": "string",
                "description": "Directory or file to search in (default: '.')"
            }),
        );
        props.insert("target".into(), json!({
            "type": "string",
            "description": "Search target: 'content' for file contents or 'files' for filenames",
            "enum": ["content", "files"],
            "default": "content"
        }));
        props.insert("output_mode".into(), json!({
            "type": "string",
            "description": "Result shape for content search: 'content' returns matching lines under `matches`; 'files_only' returns unique file paths under `files`; 'count' returns per-file match counts under `counts`",
            "enum": ["content", "files_only", "count"],
            "default": "content"
        }));
        props.insert(
            "file_glob".into(),
            json!({
                "type": "string",
                "description": "Filter files by glob pattern when searching content (e.g. '*.py')"
            }),
        );
        props.insert(
            "limit".into(),
            json!({
                "type": "integer",
                "description": "Maximum number of results to return"
            }),
        );

        tool_schema(
            "search_files",
            "Search file contents or find files by name. Uses ripgrep-backed regex search for content and glob patterns for filenames.",
            JsonSchema::object(props, vec!["pattern".into()]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::{AgentError, CommandOutput};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory terminal backend for handler tests. `read_file` with no
    /// offset/limit returns the full stored content; the handler paginates.
    struct MockBackend {
        files: Mutex<HashMap<String, String>>,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                files: Mutex::new(HashMap::new()),
            }
        }
        fn with_file(path: &str, content: &str) -> Self {
            let b = Self::new();
            b.files
                .lock()
                .unwrap()
                .insert(path.to_string(), content.to_string());
            b
        }
    }

    #[async_trait]
    impl TerminalBackend for MockBackend {
        async fn execute_command(
            &self,
            _cmd: &str,
            _timeout: Option<u64>,
            _workdir: Option<&str>,
            _bg: bool,
            _pty: bool,
        ) -> Result<CommandOutput, AgentError> {
            Ok(CommandOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
        async fn read_file(
            &self,
            path: &str,
            _offset: Option<u64>,
            _limit: Option<u64>,
        ) -> Result<String, AgentError> {
            self.files
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| AgentError::Io(format!("not found: {path}")))
        }
        async fn write_file(&self, path: &str, content: &str) -> Result<(), AgentError> {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_string(), content.to_string());
            Ok(())
        }
        async fn file_exists(&self, path: &str) -> Result<bool, AgentError> {
            Ok(self.files.lock().unwrap().contains_key(path))
        }
    }

    /// Pull a `key: value` trailer line from a read_file result body.
    fn trailer(body: &str, key: &str) -> Option<String> {
        let prefix = format!("{key}: ");
        body.lines()
            .find_map(|l| l.strip_prefix(&prefix).map(|v| v.trim().to_string()))
    }

    /// Content lines of a page (strip the 8-char `{:6}  ` line-number prefix).
    fn page_lines(body: &str) -> Vec<String> {
        body.lines()
            .skip(1) // `--- ... ---` header
            .take_while(|l| !l.starts_with("next_offset:") && !l.starts_with("content_hash:"))
            .map(|l| l.get(8..).unwrap_or("").to_string())
            .collect()
    }

    #[tokio::test]
    async fn test_read_file_handler() {
        let handler = ReadFileHandler::new(Arc::new(MockBackend::with_file(
            "/tmp/test.txt",
            "contents of /tmp/test.txt",
        )));
        let result = handler
            .execute(json!({"path": "/tmp/test.txt"}))
            .await
            .unwrap();
        assert!(result.contains("/tmp/test.txt"));
        // EOF on a single-line file: no next_offset trailer, but a hash trailer.
        assert!(trailer(&result, "next_offset").is_none());
        let hash = trailer(&result, "content_hash").expect("content_hash trailer missing");
        assert!(hash.starts_with("sha256:"));
    }

    #[tokio::test]
    async fn test_read_file_pagination_walk_to_eof() {
        let expected: Vec<String> = (1..=250).map(|i| format!("line {i}")).collect();
        let backend = MockBackend::with_file("/tmp/long.txt", &expected.join("\n"));
        let handler = ReadFileHandler::new(Arc::new(backend));

        let mut collected: Vec<String> = Vec::new();
        let mut offset = 1u64;
        let mut pages = 0;
        let mut first_hash = String::new();
        loop {
            let result = handler
                .execute(json!({"path": "/tmp/long.txt", "offset": offset, "limit": 100}))
                .await
                .unwrap();
            if pages == 0 {
                first_hash = trailer(&result, "content_hash").expect("hash trailer missing");
                assert!(first_hash.starts_with("sha256:"));
            } else {
                // The hash is over the full content: stable across pages.
                assert_eq!(
                    trailer(&result, "content_hash").as_deref(),
                    Some(first_hash.as_str())
                );
            }
            collected.extend(page_lines(&result));
            pages += 1;
            match trailer(&result, "next_offset") {
                Some(n) => offset = n.parse::<u64>().expect("next_offset must be numeric"),
                None => break,
            }
            assert!(pages < 10, "pagination did not reach EOF");
        }
        assert_eq!(pages, 3, "250 lines / limit 100 must take 3 pages");
        assert_eq!(collected, expected);
    }

    #[tokio::test]
    async fn test_read_file_limit_clamped_to_2000() {
        let lines: Vec<String> = (1..=2100).map(|i| format!("line {i}")).collect();
        let backend = MockBackend::with_file("/tmp/big.txt", &lines.join("\n"));
        let handler = ReadFileHandler::new(Arc::new(backend));

        let result = handler
            .execute(json!({"path": "/tmp/big.txt", "limit": 99999}))
            .await
            .unwrap();
        assert_eq!(page_lines(&result).len(), 2000);
        assert!(
            result.contains("(lines 1-2000 of 2100)"),
            "header: {}",
            result.lines().next().unwrap_or("")
        );
        assert!(trailer(&result, "next_offset").is_some());
    }

    #[tokio::test]
    async fn test_write_file_handler() {
        let handler = WriteFileHandler::new(Arc::new(MockBackend::new()));
        let result = handler
            .execute(json!({"path": "/tmp/test.txt", "content": "hello"}))
            .await
            .unwrap();
        assert!(result.contains("Successfully wrote"));
        assert!(trailer(&result, "content_hash").is_some());
    }

    #[tokio::test]
    async fn test_write_file_stale_hash_refused_then_forced() {
        let backend = Arc::new(MockBackend::with_file("/tmp/doc.txt", "version one"));
        let read = ReadFileHandler::new(backend.clone());
        let write = WriteFileHandler::new(backend.clone());

        // Read to obtain the current content_hash token.
        let res = read.execute(json!({"path": "/tmp/doc.txt"})).await.unwrap();
        let token = trailer(&res, "content_hash").expect("hash trailer missing");

        // External change outside the read->write flow.
        backend.files.lock().unwrap().insert(
            "/tmp/doc.txt".to_string(),
            "version two (external)".to_string(),
        );

        // Stale token -> refused, file untouched.
        let err = write
            .execute(json!({
                "path": "/tmp/doc.txt",
                "content": "version three",
                "expected_hash": token,
            }))
            .await
            .expect_err("stale write must be refused");
        assert!(
            err.to_string().contains("changed since it was read"),
            "unexpected error: {err}"
        );
        assert_eq!(
            backend.files.lock().unwrap()["/tmp/doc.txt"],
            "version two (external)"
        );

        // force:true -> allowed.
        write
            .execute(json!({
                "path": "/tmp/doc.txt",
                "content": "version three",
                "expected_hash": token,
                "force": true,
            }))
            .await
            .expect("forced write must succeed");
        assert_eq!(
            backend.files.lock().unwrap()["/tmp/doc.txt"],
            "version three"
        );

        // Fresh token -> allowed without force.
        let res = read.execute(json!({"path": "/tmp/doc.txt"})).await.unwrap();
        let token2 = trailer(&res, "content_hash").unwrap();
        write
            .execute(json!({
                "path": "/tmp/doc.txt",
                "content": "version four",
                "expected_hash": token2,
            }))
            .await
            .expect("write with fresh token must succeed");
        assert_eq!(
            backend.files.lock().unwrap()["/tmp/doc.txt"],
            "version four"
        );

        // Bare-hex token (no sha256: prefix) is accepted too.
        let res = read.execute(json!({"path": "/tmp/doc.txt"})).await.unwrap();
        let bare = trailer(&res, "content_hash")
            .unwrap()
            .trim_start_matches("sha256:")
            .to_string();
        write
            .execute(json!({
                "path": "/tmp/doc.txt",
                "content": "version five",
                "expected_hash": bare,
            }))
            .await
            .expect("bare-hex token must be accepted");
        assert_eq!(
            backend.files.lock().unwrap()["/tmp/doc.txt"],
            "version five"
        );

        // New file (does not exist yet): guard is skipped, write succeeds.
        write
            .execute(json!({
                "path": "/tmp/brand-new.txt",
                "content": "hello",
                "expected_hash": "sha256:deadbeef",
            }))
            .await
            .expect("write to new file must succeed");
    }

    #[test]
    fn test_content_hash_and_normalize() {
        let h = content_hash("hello");
        assert!(h.starts_with("sha256:"));
        assert_eq!(h.len(), "sha256:".len() + 64);
        assert_eq!(content_hash("hello"), h, "hash must be deterministic");
        assert_ne!(content_hash("hello"), content_hash("world"));
        assert_eq!(normalize_hash_token(&h), &h["sha256:".len()..]);
        assert_eq!(normalize_hash_token("abcdef"), "abcdef");
    }

    #[tokio::test]
    async fn test_read_file_schema() {
        let handler = ReadFileHandler::new(Arc::new(MockBackend::new()));
        let schema = handler.schema();
        assert_eq!(schema.name, "read_file");
    }

    #[tokio::test]
    async fn test_write_file_schema() {
        let handler = WriteFileHandler::new(Arc::new(MockBackend::new()));
        let schema = handler.schema();
        assert_eq!(schema.name, "write_file");
        let s = serde_json::to_string(&schema).unwrap();
        assert!(s.contains("expected_hash"));
        assert!(s.contains("force"));
    }

    struct StubPatchBackend {
        calls: Mutex<Vec<(String, String, String, bool)>>,
    }

    #[async_trait]
    impl PatchBackend for StubPatchBackend {
        async fn patch_file(
            &self,
            path: &str,
            old_string: &str,
            new_string: &str,
            replace_all: bool,
        ) -> Result<String, ToolError> {
            self.calls.lock().unwrap().push((
                path.to_string(),
                old_string.to_string(),
                new_string.to_string(),
                replace_all,
            ));
            Ok(r#"{"status":"ok"}"#.to_string())
        }
    }

    #[tokio::test]
    async fn test_patch_handler_delegates_params() {
        let backend = Arc::new(StubPatchBackend {
            calls: Mutex::new(Vec::new()),
        });
        let handler = PatchHandler::new(backend.clone());
        handler
            .execute(json!({
                "path": "/tmp/a.txt",
                "old_string": "x",
                "new_string": "y",
                "replace_all": true,
            }))
            .await
            .unwrap();
        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            (
                "/tmp/a.txt".to_string(),
                "x".to_string(),
                "y".to_string(),
                true
            )
        );
    }

    struct StubSearchBackend;

    #[async_trait]
    impl SearchBackend for StubSearchBackend {
        async fn search_content(
            &self,
            _pattern: &str,
            _path: &str,
            _file_glob: Option<&str>,
            _max_results: Option<usize>,
        ) -> Result<String, ToolError> {
            Ok(json!({
                "matches": [
                    {"file": "a.rs", "line": 2, "content": "hit one"},
                    {"file": "b.rs", "line": 5, "content": "hit two"},
                    {"file": "a.rs", "line": 9, "content": "hit three"},
                ],
                "total": 3,
                "pattern": "hit",
            })
            .to_string())
        }
        async fn search_files(&self, _pattern: &str, _path: &str) -> Result<String, ToolError> {
            Ok(json!({"files": [], "total": 0, "pattern": ""}).to_string())
        }
    }

    #[tokio::test]
    async fn test_search_output_modes() {
        let handler = SearchFilesHandler::new(Arc::new(StubSearchBackend));

        // Default: content mode keeps the `matches` array.
        let raw = handler.execute(json!({"pattern": "hit"})).await.unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["total"], 3);
        assert_eq!(v["matches"].as_array().unwrap().len(), 3);

        // files_only: unique file paths under `files`.
        let raw = handler
            .execute(json!({"pattern": "hit", "output_mode": "files_only"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        let files: Vec<&str> = v["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert_eq!(files, vec!["a.rs", "b.rs"]);
        assert_eq!(v["total"], 2);

        // count: per-file counts under `counts`.
        let raw = handler
            .execute(json!({"pattern": "hit", "output_mode": "count"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["total_matches"], 3);
        assert_eq!(v["total_files"], 2);
        let counts = v["counts"].as_array().unwrap();
        assert!(counts
            .iter()
            .any(|c| c["file"] == "a.rs" && c["count"] == 2));
        assert!(counts
            .iter()
            .any(|c| c["file"] == "b.rs" && c["count"] == 1));
    }

    #[tokio::test]
    async fn test_search_schema_declares_output_mode() {
        let handler = SearchFilesHandler::new(Arc::new(StubSearchBackend));
        let s = serde_json::to_string(&handler.schema()).unwrap();
        assert!(s.contains("output_mode"));
        for m in ["content", "files_only", "count"] {
            assert!(s.contains(m), "schema: {s}");
        }
    }
}
