//! Desktop terminal-pane bridges: `close_terminal` and `read_terminal`.
//!
//! Both tools are GUI-only (the `desktop_ui` toolset in the reference
//! implementation). They round-trip through a bridge that the desktop gateway
//! injects; without a wired bridge every call fails with a clear error
//! instead of silently doing nothing.
//!
//! LIMITATIONS vs the Python original:
//! - `close_terminal` in Python routes through the process registry's
//!   `on_close` sink; here the bridge is a single injected trait object.
//! - `read_terminal` in Python shares its body with `read_preview` /
//!   `read_window_below`; only the terminal variant is ported here.

use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Value};

use hermes_core::{tool_schema, JsonSchema, ToolError, ToolHandler, ToolSchema};

// ---------------------------------------------------------------------------
// Bridge (injected by the desktop gateway)
// ---------------------------------------------------------------------------

/// Bridge to the desktop renderer's terminal pane.
///
/// The gateway implements this against its blocking-prompt bridge
/// (`terminal.read.request` / `terminal.read.respond`, `terminal.close`).
#[async_trait]
pub trait DesktopTerminalBridge: Send + Sync {
    /// Ask the GUI to drop the read-only tab for `process_id`.
    /// Must NOT kill the process itself.
    async fn close_terminal_tab(&self, process_id: &str) -> Result<String, ToolError>;
    /// Read the in-app terminal pane buffer.
    async fn read_terminal(
        &self,
        start_line: Option<u64>,
        count: Option<u64>,
    ) -> Result<String, ToolError>;
}

fn no_bridge(tool: &str) -> ToolError {
    ToolError::ExecutionFailed(format!(
        "{tool} is only available in the Hermes desktop app: no desktop bridge is wired."
    ))
}

// ---------------------------------------------------------------------------
// close_terminal
// ---------------------------------------------------------------------------

/// Hide a background process's terminal tab (the process keeps running).
pub struct CloseTerminalHandler {
    bridge: Option<Arc<dyn DesktopTerminalBridge>>,
}

impl CloseTerminalHandler {
    pub fn new(bridge: Option<Arc<dyn DesktopTerminalBridge>>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl ToolHandler for CloseTerminalHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let process_id = params
            .get("process_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ToolError::InvalidParams(
                    "process_id is required (the background process whose tab to close).".into(),
                )
            })?;
        let bridge = self.bridge.as_ref().ok_or_else(|| no_bridge("close_terminal"))?;
        bridge.close_terminal_tab(process_id).await
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "process_id".into(),
            json!({
                "type": "string",
                "description": "The background process's session id (from terminal(background=true) output or process(action='list')) whose tab should be closed."
            }),
        );
        tool_schema(
            "close_terminal",
            "Hide a background process's terminal tab (process keeps running) in the Hermes desktop GUI. This does NOT kill the process — it only drops the tab/view; output keeps buffering and the user can reopen it. To actually stop the process, use process(action='kill') instead.",
            JsonSchema::object(props, vec!["process_id".into()]),
        )
    }
}

// ---------------------------------------------------------------------------
// read_terminal
// ---------------------------------------------------------------------------

/// Read the in-app terminal pane beside the chat.
pub struct ReadTerminalHandler {
    bridge: Option<Arc<dyn DesktopTerminalBridge>>,
}

impl ReadTerminalHandler {
    pub fn new(bridge: Option<Arc<dyn DesktopTerminalBridge>>) -> Self {
        Self { bridge }
    }
}

/// Validate and normalize the paging window. Pure logic, unit-tested.
fn normalize_window(start_line: Option<&Value>, count: Option<&Value>) -> Result<(Option<u64>, Option<u64>), ToolError> {
    let parse = |v: &Value, floor: u64, name: &str| -> Result<u64, ToolError> {
        let n = v
            .as_u64()
            .or_else(|| v.as_i64().and_then(|i| u64::try_from(i).ok()))
            .ok_or_else(|| {
                ToolError::InvalidParams(format!("{name} must be an integer."))
            })?;
        Ok(n.max(floor))
    };
    let start = start_line
        .filter(|v| !v.is_null())
        .map(|v| parse(v, 0, "start_line"))
        .transpose()?;
    let count = count
        .filter(|v| !v.is_null())
        .map(|v| parse(v, 1, "count"))
        .transpose()?;
    Ok((start, count))
}

#[async_trait]
impl ToolHandler for ReadTerminalHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let (start, count) =
            normalize_window(params.get("start_line"), params.get("count"))?;
        let bridge = self.bridge.as_ref().ok_or_else(|| no_bridge("read_terminal"))?;
        bridge.read_terminal(start, count).await
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "start_line".into(),
            json!({
                "type": "integer",
                "description": "0-indexed first line (0 = oldest). Omit for the visible screen."
            }),
        );
        props.insert(
            "count".into(),
            json!({
                "type": "integer",
                "description": "Lines to read from start_line. Defaults to the visible row count."
            }),
        );
        tool_schema(
            "read_terminal",
            "Read the in-app terminal pane beside this chat. No args = visible screen + total_lines; page scrollback with start_line (0 = oldest) + count. JSON: {total_lines, start, end, viewport_rows, cursor_row, text}.",
            JsonSchema::object(props, vec![]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_empty_is_valid() {
        assert_eq!(normalize_window(None, None).unwrap(), (None, None));
    }

    #[test]
    fn window_values_pass_through() {
        let s = json!(10);
        let c = json!(25);
        assert_eq!(
            normalize_window(Some(&s), Some(&c)).unwrap(),
            (Some(10), Some(25))
        );
    }

    #[test]
    fn window_rejects_non_integers() {
        let s = json!("ten");
        assert!(normalize_window(Some(&s), None).is_err());
    }

    #[test]
    fn window_rejects_negative_start() {
        let s = json!(-3);
        assert!(normalize_window(Some(&s), None).is_err());
    }

    #[test]
    fn window_floors_zero_count_to_one() {
        // Python floors count at 1; here count=0 is normalized up to 1.
        let c = json!(0);
        assert_eq!(normalize_window(None, Some(&c)).unwrap(), (None, Some(1)));
    }

    #[test]
    fn schema_names_match_original() {
        assert_eq!(CloseTerminalHandler::new(None).schema().name, "close_terminal");
        assert_eq!(ReadTerminalHandler::new(None).schema().name, "read_terminal");
    }
}
