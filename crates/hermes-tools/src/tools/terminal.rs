//! Terminal tools: terminal command execution and process management
//!
//! Exec semantics ported from the hermes-rs reference (`shell.rs`):
//! - `timeout` is the canonical timeout (seconds); `timeout_seconds` and
//!   `timeout_secs` are accepted as aliases, with `timeout` winning when
//!   several are present. Values > 600 are promoted to background execution.
//! - Long output is truncated to head+tail 25k chars with a
//!   `... [truncated N chars] ...` marker; the full (redacted) output is
//!   spilled to `$TMPDIR/hermes-exec-spills/` and the path is reported.
//! - The working directory persists across calls: the handler tracks one
//!   cwd per instance (the tool layer has no per-session concept), updated
//!   by an explicit `workdir` param or a leading `cd <dir>` in the command.
//! - Local shell invocation prefers `bash -c`, falling back to `sh -c`
//!   when bash is unavailable (implemented in the `LocalBackend` of
//!   hermes-environments).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Value};

use hermes_core::{
    redact_secrets, tool_schema, CommandOutput, JsonSchema, TerminalBackend, ToolError,
    ToolHandler, ToolSchema,
};

use crate::approval::{ApprovalDecision, ApprovalManager, ApprovalMode};

// ---------------------------------------------------------------------------
// Exec semantics (ported from hermes-rs shell.rs)
// ---------------------------------------------------------------------------

/// Head/tail truncation budget for exec output, in characters.
const EXEC_TRUNC_HEAD: usize = 25_000;
const EXEC_TRUNC_TAIL: usize = 25_000;

/// Foreground executions with a timeout above this (seconds) are promoted
/// to background processes instead of being clamped.
const FOREGROUND_TIMEOUT_LIMIT_SECS: u64 = 600;

/// Sequence counter so spill file names stay unique within this process.
static SPILL_SEQ: AtomicU64 = AtomicU64::new(0);

/// Resolve the exec timeout from params: `timeout` is canonical,
/// `timeout_seconds`/`timeout_secs` are aliases (`timeout` wins, then
/// `timeout_seconds`). `None` means "backend default". A non-positive value
/// is an error, not a silent clamp (same as the reference).
fn resolve_timeout_seconds(params: &Value) -> Result<Option<u64>, ToolError> {
    let raw = params
        .get("timeout")
        .and_then(|v| v.as_i64())
        .or_else(|| params.get("timeout_seconds").and_then(|v| v.as_i64()))
        .or_else(|| params.get("timeout_secs").and_then(|v| v.as_i64()));
    match raw {
        Some(v) if v <= 0 => Err(ToolError::InvalidParams(format!(
            "timeout must be a positive number of seconds (got {v})."
        ))),
        Some(v) => Ok(Some(v as u64)),
        None => Ok(None),
    }
}

/// Truncate `s` to head+tail when it exceeds the budget.
///
/// Returns `(display_text, Option<spill_path>)`: when truncated, the full
/// text is also written to a spill file (best-effort) and its path is
/// returned so it can be reported in the result.
fn truncate_head_tail(s: &str) -> (String, Option<PathBuf>) {
    let total = s.chars().count();
    if total <= EXEC_TRUNC_HEAD + EXEC_TRUNC_TAIL {
        return (s.to_string(), None);
    }
    let head: String = s.chars().take(EXEC_TRUNC_HEAD).collect();
    let tail: String = s.chars().skip(total - EXEC_TRUNC_TAIL).collect();
    let cut = total - EXEC_TRUNC_HEAD - EXEC_TRUNC_TAIL;
    let shown = format!("{head}\n... [truncated {cut} chars] ...\n{tail}");
    (shown, write_spill_file(s))
}

/// Write full output to a spill file under `$TMPDIR/hermes-exec-spills`;
/// best-effort (failure → `None`, the tool still succeeds without a path).
fn write_spill_file(full: &str) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("hermes-exec-spills");
    std::fs::create_dir_all(&dir).ok()?;
    let name = format!(
        "exec-{}-{}-{}.log",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        SPILL_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let path = dir.join(name);
    std::fs::write(&path, full).ok()?;
    Some(path)
}

/// Characters marking an unquoted `path` as a shell expression rather than
/// a plain path (e.g. `cd /tmp || x`) — leave those to the shell.
fn has_shell_metachars(p: &str) -> bool {
    p.chars().any(|c| {
        matches!(
            c,
            '|' | '&' | '<' | '>' | '(' | ')' | '$' | '`' | '!' | '*' | '?' | '#' | '\\' | '\n'
        )
    })
}

/// Parse a leading `cd <path>` at the start of a command.
///
/// Returns `(path, rest)` when the command starts with `cd <path>` followed
/// by end-of-command, `&&` or `;`. The path may be quoted (`"..."`/`'...'`)
/// to allow spaces/`;`. Anything that is not a pure leading `cd` (different
/// first word, bare `cd`, unquoted path containing shell operators) returns
/// `None` and the command is executed as-is by the shell.
fn parse_leading_cd(command: &str) -> Option<(String, String)> {
    let s = command.trim_start();
    let rest = s.strip_prefix("cd")?;
    // `cd` must be followed by whitespace — not `cdx`, `cd..`, etc.
    if !rest.starts_with(|c: char| c.is_whitespace()) {
        return None;
    }
    let after = rest.trim_start();
    if after.is_empty() {
        return None; // Bare `cd` → let the shell handle it.
    }
    let (path, rest, quoted) = match after.chars().next() {
        Some(q @ ('"' | '\'')) => {
            let inner = &after[q.len_utf8()..];
            let end = inner.find(q)?;
            (
                inner[..end].to_string(),
                inner[end + q.len_utf8()..].to_string(),
                true,
            )
        }
        _ => {
            // Unquoted path ends at `&&`, `;`, or end of command.
            let cut = after.find("&&").into_iter().chain(after.find(';')).min();
            let (p, r) = match cut {
                Some(i) => (after[..i].trim_end().to_string(), after[i..].to_string()),
                None => (after.trim_end().to_string(), String::new()),
            };
            (p, r, false)
        }
    };
    if path.is_empty() {
        return None;
    }
    if !quoted && has_shell_metachars(&path) {
        return None;
    }
    let rest_t = rest.trim_start();
    if rest_t.is_empty() || rest_t.starts_with("&&") || rest_t.starts_with(';') {
        Some((path, rest_t.to_string()))
    } else {
        None
    }
}

/// Expand a leading `~` in a path (home directory).
fn expand_tilde(p: &str) -> String {
    if p == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return home;
        }
    } else if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    p.to_string()
}

/// Apply `cd <path>` to the persistent session cwd.
///
/// Absolute paths are used directly; relative paths resolve against the
/// current session cwd. The target is canonicalized (missing → clear
/// error) so symlinks/`..` cannot slip past the check. On failure the cwd
/// is left unchanged.
fn apply_cd(cwd: &mut PathBuf, path: &str) -> Result<(), ToolError> {
    let raw = PathBuf::from(expand_tilde(path));
    let target = if raw.is_absolute() {
        raw
    } else {
        cwd.join(raw)
    };
    let resolved = target
        .canonicalize()
        .map_err(|_| ToolError::InvalidParams(format!("cd: no such directory: {path}")))?;
    if !resolved.is_dir() {
        return Err(ToolError::InvalidParams(format!(
            "cd: not a directory: {path}"
        )));
    }
    *cwd = resolved;
    Ok(())
}

// ---------------------------------------------------------------------------
// TerminalHandler
// ---------------------------------------------------------------------------

/// Tool for executing terminal commands via an injected backend.
pub struct TerminalHandler {
    backend: std::sync::Arc<dyn TerminalBackend>,
    approval: ApprovalManager,
    /// Persistent working directory across calls. The tool layer has no
    /// per-session concept, so a single cwd is tracked per handler
    /// instance; the handler is constructed once per agent session (see
    /// `register_builtins`), so in practice this is per-session state.
    /// Updated by an explicit `workdir` param or a leading `cd <dir>` in
    /// the command (mirrors the reference's per-session cwd).
    session_cwd: Arc<Mutex<PathBuf>>,
}

impl TerminalHandler {
    pub fn new(backend: std::sync::Arc<dyn TerminalBackend>) -> Self {
        Self {
            backend,
            approval: ApprovalManager::new(),
            session_cwd: Arc::new(Mutex::new(Self::initial_cwd())),
        }
    }

    /// Handler with an explicit unattended mode (`--readonly` / `--yes`).
    pub fn with_approval_mode(
        backend: std::sync::Arc<dyn TerminalBackend>,
        mode: ApprovalMode,
    ) -> Self {
        Self {
            backend,
            approval: ApprovalManager::with_mode(mode),
            session_cwd: Arc::new(Mutex::new(Self::initial_cwd())),
        }
    }

    fn initial_cwd() -> PathBuf {
        std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir())
    }

    /// Resolve the workdir for this call and update the persistent cwd.
    ///
    /// - Explicit `workdir` param: absolutized against the current cwd,
    ///   used for this call, and persisted for subsequent calls.
    /// - Otherwise, a leading `cd <dir>` in the command updates the
    ///   persistent cwd; this call still runs from the previous cwd (the
    ///   shell performs the `cd` inside the command itself).
    /// - Otherwise the stored cwd is reused.
    fn resolve_workdir(&self, explicit: Option<&str>, command: &str) -> Result<PathBuf, ToolError> {
        let mut guard = self.session_cwd.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(w) = explicit {
            let raw = PathBuf::from(expand_tilde(w));
            let target = if raw.is_absolute() {
                raw
            } else {
                guard.join(raw)
            };
            if !target.is_dir() {
                return Err(ToolError::InvalidParams(format!(
                    "workdir: no such directory: {w}"
                )));
            }
            let stored = target.canonicalize().unwrap_or(target);
            *guard = stored.clone();
            return Ok(stored);
        }
        if let Some((cd_path, _rest)) = parse_leading_cd(command) {
            let previous = guard.clone();
            apply_cd(&mut guard, &cd_path)?;
            return Ok(previous);
        }
        Ok(guard.clone())
    }
}

#[async_trait]
impl ToolHandler for TerminalHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let command = params
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("Missing 'command' parameter".into()))?;

        match self.approval.check_approval(command) {
            ApprovalDecision::Denied => {
                return Err(ToolError::ExecutionFailed(format!(
                    "Command denied by security policy: {}",
                    command
                )));
            }
            ApprovalDecision::RequiresConfirmation => match self.approval.mode() {
                // --readonly: fail closed — medium/high-risk commands are
                // denied outright, only Approved (low-risk) commands run.
                ApprovalMode::ReadOnly => {
                    return Err(ToolError::ExecutionFailed(format!(
                        "Command denied by --readonly policy (requires confirmation): {}",
                        command
                    )));
                }
                // --yes: confirmation is granted without prompting.
                ApprovalMode::AutoApprove => {
                    tracing::warn!(
                        command,
                        "command requires confirmation — auto-approved via --yes"
                    );
                }
                // Default: historical behaviour — auto-approved in agent mode
                // with a warning log.
                ApprovalMode::Interactive => {
                    tracing::warn!(
                        command,
                        "command requires confirmation — auto-approved in agent mode"
                    );
                }
            },
            ApprovalDecision::Approved => {}
        }

        let timeout = resolve_timeout_seconds(&params)?;

        let explicit_workdir = params.get("workdir").and_then(|v| v.as_str());
        let workdir = self.resolve_workdir(explicit_workdir, command)?;
        let workdir_str = workdir.to_string_lossy();

        let explicit_bg = params
            .get("background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // A requested timeout above the foreground limit promotes the call
        // to a background process instead of being silently clamped (mirrors
        // the reference): the backend's background path returns the handle.
        let promoted =
            !explicit_bg && matches!(timeout, Some(t) if t > FOREGROUND_TIMEOUT_LIMIT_SECS);
        let background = explicit_bg || promoted;

        let pty = params.get("pty").and_then(|v| v.as_bool()).unwrap_or(false);

        match self
            .backend
            .execute_command(command, timeout, Some(&workdir_str), background, pty)
            .await
        {
            Ok(output) => {
                let mut result = format_command_output(command, &output);
                if promoted {
                    result = format!(
                        "Timeout {}s exceeds the {}s foreground limit; promoted to background process.\n{result}",
                        timeout.unwrap_or(0),
                        FOREGROUND_TIMEOUT_LIMIT_SECS
                    );
                }
                Ok(result)
            }
            Err(e) => Err(ToolError::ExecutionFailed(e.to_string())),
        }
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "command".into(),
            json!({
                "type": "string",
                "description": "The command to execute"
            }),
        );
        props.insert(
            "timeout".into(),
            json!({
                "type": "integer",
                "description": "Timeout in seconds (backend default when omitted). `timeout` is canonical; `timeout_seconds`/`timeout_secs` are aliases (`timeout` wins). Values above 600 promote the command to a background process."
            }),
        );
        props.insert(
            "timeout_seconds".into(),
            json!({
                "type": "integer",
                "description": "Alias for `timeout` (seconds)."
            }),
        );
        props.insert(
            "timeout_secs".into(),
            json!({
                "type": "integer",
                "description": "Legacy alias for `timeout` (seconds)."
            }),
        );
        props.insert(
            "workdir".into(),
            json!({
                "type": "string",
                "description": "Working directory for the command. Persists across calls: it becomes the session cwd for subsequent calls (same for a leading `cd <dir>` in the command)."
            }),
        );
        props.insert(
            "background".into(),
            json!({
                "type": "boolean",
                "description": "Run command in background (default: false)",
                "default": false
            }),
        );
        props.insert(
            "pty".into(),
            json!({
                "type": "boolean",
                "description": "Run command in PTY mode for interactive programs (default: false)",
                "default": false
            }),
        );

        tool_schema(
            "terminal",
            "Execute a terminal command. Returns stdout, stderr, and exit code.",
            JsonSchema::object(props, vec!["command".into()]),
        )
    }
}

/// Human-readable note for a signal number, or `None` when unknown.
fn signal_exit_note(signum: i32) -> Option<&'static str> {
    match signum {
        3 => Some("SIGQUIT (quit from keyboard)"),
        4 => Some("SIGILL (illegal instruction — corrupt binary or wrong architecture)"),
        6 => Some("SIGABRT (abort — assertion failure, fatal runtime error, or glibc abort)"),
        7 => Some("SIGBUS (bus error — misaligned or unmapped memory access)"),
        8 => Some("SIGFPE (fatal arithmetic error, e.g. integer division by zero)"),
        9 => Some(
            "SIGKILL — often the kernel OOM killer on memory exhaustion, or an explicit kill -9",
        ),
        11 => Some("SIGSEGV (segmentation fault — the program crashed)"),
        13 => Some("SIGPIPE (wrote to a closed pipe — e.g. output piped to a reader that exited)"),
        15 => Some("SIGTERM (terminated — kill/timeout or shutdown requested it to stop)"),
        24 => Some("SIGXCPU (CPU time limit exceeded)"),
        25 => Some("SIGXFSZ (file size limit exceeded)"),
        _ => None,
    }
}

/// Interpret a non-zero exit code into a short note for the model, so it
/// doesn't burn turns investigating results that are actually normal.
/// Mirrors the reference's `_interpret_exit_code`: signal termination
/// (negative code, or the shell's 128+signum convention) and informative
/// per-command exit codes (grep=1 "no matches", diff=1 "files differ", …).
fn interpret_exit_code(command: &str, exit_code: i32) -> Option<String> {
    if exit_code == 0 {
        return None;
    }
    // Signal termination: negative = subprocess semantics (certain);
    // 128+signum = shell convention (heuristic — "usually"). SIGINT is
    // excluded (it belongs to the interrupt-marker path).
    if exit_code < 0 {
        let signum = -exit_code;
        if signum == 2 {
            return None;
        }
        return Some(match signal_exit_note(signum) {
            Some(note) => format!("Command terminated by signal {signum}: {note}"),
            None => format!("Command terminated by signal {signum}"),
        });
    }
    if exit_code > 128 {
        if let Some(note) = signal_exit_note(exit_code - 128) {
            let signum = exit_code - 128;
            return Some(format!(
                "Exit code {exit_code} usually means the command was terminated by signal {signum}: {note}"
            ));
        }
    }
    // Base command = first word of the last pipeline/chain segment that is
    // not a VAR=value assignment; the last segment determines the exit code.
    let last_segment = command
        .rsplit(['|', '&', ';'])
        .next()
        .unwrap_or(command)
        .trim();
    let base_cmd = last_segment
        .split_whitespace()
        .find(|w| !w.contains('=') || w.starts_with('-'))
        .map(|w| w.rsplit('/').next().unwrap_or(w))
        .unwrap_or("");
    let semantics: Option<&str> = match (base_cmd, exit_code) {
        ("grep" | "egrep" | "fgrep" | "rg" | "ag" | "ack", 1) => {
            Some("No matches found (not an error)")
        }
        ("diff" | "colordiff", 1) => Some("Files differ (expected, not an error)"),
        ("test" | "[", 1) => Some("Condition evaluated to false (expected, not an error)"),
        ("find", 1) => {
            Some("Some directories were inaccessible (partial results may still be valid)")
        }
        ("curl", 6) => Some("Could not resolve host"),
        ("curl", 7) => Some("Failed to connect to host"),
        ("curl", 22) => Some("HTTP response code indicated error (e.g. 404, 500)"),
        ("curl", 28) => Some("Operation timed out"),
        ("git", 1) => {
            Some("Non-zero exit (often normal — e.g. 'git diff' returns 1 when files differ)")
        }
        _ => None,
    };
    semantics.map(|s| s.to_string())
}

/// Format command output for display.
///
/// Secrets are redacted before the text becomes a tool result, so every
/// [`TerminalBackend`] (local, docker, ssh, daytona, modal, singularity)
/// gets output redaction at this single choke point. Redaction applies to
/// the full text first (including what lands in the spill file); long
/// output is then truncated to head+tail with the full output spilled to a
/// file whose path is reported in the result. Non-zero exit codes get an
/// interpretive note ([`interpret_exit_code`]) so the model doesn't burn
/// turns investigating normal results.
fn format_command_output(command: &str, output: &CommandOutput) -> String {
    let mut result = String::new();
    if !output.stdout.is_empty() {
        result.push_str(&output.stdout);
    }
    if !output.stderr.is_empty() {
        if !result.is_empty() {
            result.push_str("\n--- STDERR ---\n");
        }
        result.push_str(&output.stderr);
    }
    if output.exit_code != 0 {
        result.push_str(&format!("\n[exit code: {}]", output.exit_code));
        if let Some(note) = interpret_exit_code(command, output.exit_code) {
            result.push_str(&format!("\n[note: {note}]"));
        }
    }
    if result.is_empty() {
        result = format!("[exit code: {}]", output.exit_code);
    }
    let redacted = redact_secrets(&result);
    let (shown, spill) = truncate_head_tail(&redacted);
    match spill {
        Some(path) => format!(
            "{shown}\n[full output ({} chars) saved to: {}]",
            redacted.chars().count(),
            path.display()
        ),
        None => shown,
    }
}

// ---------------------------------------------------------------------------
// ProcessHandler
// ---------------------------------------------------------------------------

/// Backend trait for process management operations.
#[async_trait]
pub trait ProcessBackend: Send + Sync {
    /// List all background processes.
    async fn list_processes(&self) -> Result<String, ToolError>;
    /// Poll a process for output (non-blocking).
    async fn poll_process(&self, pid: &str) -> Result<String, ToolError>;
    /// Wait for a process to complete and return its full output.
    async fn wait_process(&self, pid: &str, timeout: Option<u64>) -> Result<String, ToolError>;
    /// Kill a background process.
    async fn kill_process(&self, pid: &str) -> Result<String, ToolError>;
    /// Write stdin to a running process.
    async fn write_stdin(&self, pid: &str, data: &str) -> Result<String, ToolError>;
    /// Submit input to a process and get output.
    async fn submit_process(&self, pid: &str, input: &str) -> Result<String, ToolError>;
    /// Close stdin of a process.
    async fn close_process(&self, pid: &str) -> Result<String, ToolError>;
}

/// Tool for managing background processes.
pub struct ProcessHandler {
    backend: std::sync::Arc<dyn ProcessBackend>,
}

impl ProcessHandler {
    pub fn new(backend: std::sync::Arc<dyn ProcessBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl ToolHandler for ProcessHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("Missing 'action' parameter".into()))?;

        match action {
            "list" => self.backend.list_processes().await,
            "poll" => {
                let pid = params
                    .get("pid")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidParams("Missing 'pid' parameter".into()))?;
                self.backend.poll_process(pid).await
            }
            "wait" => {
                let pid = params
                    .get("pid")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidParams("Missing 'pid' parameter".into()))?;
                let timeout = params.get("timeout").and_then(|v| v.as_u64());
                self.backend.wait_process(pid, timeout).await
            }
            "kill" => {
                let pid = params
                    .get("pid")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidParams("Missing 'pid' parameter".into()))?;
                self.backend.kill_process(pid).await
            }
            "write" => {
                let pid = params
                    .get("pid")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidParams("Missing 'pid' parameter".into()))?;
                let data = params
                    .get("data")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidParams("Missing 'data' parameter".into()))?;
                self.backend.write_stdin(pid, data).await
            }
            "submit" => {
                let pid = params
                    .get("pid")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidParams("Missing 'pid' parameter".into()))?;
                let input = params
                    .get("input")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidParams("Missing 'input' parameter".into()))?;
                self.backend.submit_process(pid, input).await
            }
            "close" => {
                let pid = params
                    .get("pid")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidParams("Missing 'pid' parameter".into()))?;
                self.backend.close_process(pid).await
            }
            other => Err(ToolError::InvalidParams(format!(
                "Unknown action: {}",
                other
            ))),
        }
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "action".into(),
            json!({
                "type": "string",
                "description": "Action to perform: list, poll, wait, kill, write, submit, close",
                "enum": ["list", "poll", "wait", "kill", "write", "submit", "close"]
            }),
        );
        props.insert(
            "pid".into(),
            json!({
                "type": "string",
                "description": "Process ID (required for all actions except 'list')"
            }),
        );
        props.insert(
            "timeout".into(),
            json!({
                "type": "integer",
                "description": "Timeout in milliseconds for 'wait' action"
            }),
        );
        props.insert(
            "data".into(),
            json!({
                "type": "string",
                "description": "Data to write to process stdin (for 'write' action)"
            }),
        );
        props.insert(
            "input".into(),
            json!({
                "type": "string",
                "description": "Input to submit to the process (for 'submit' action)"
            }),
        );

        tool_schema(
            "process",
            "Manage background processes: list, poll output, wait for completion, kill, write to stdin, or close.",
            JsonSchema::object(props, vec!["action".into()]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::AgentError;

    struct MockBackend;
    #[async_trait]
    impl TerminalBackend for MockBackend {
        async fn execute_command(
            &self,
            cmd: &str,
            _timeout: Option<u64>,
            _workdir: Option<&str>,
            _bg: bool,
            _pty: bool,
        ) -> Result<CommandOutput, AgentError> {
            Ok(CommandOutput {
                exit_code: 0,
                stdout: format!("output of: {}", cmd),
                stderr: String::new(),
            })
        }
        async fn read_file(
            &self,
            _path: &str,
            _offset: Option<u64>,
            _limit: Option<u64>,
        ) -> Result<String, AgentError> {
            Ok(String::new())
        }
        async fn write_file(&self, _path: &str, _content: &str) -> Result<(), AgentError> {
            Ok(())
        }
        async fn file_exists(&self, _path: &str) -> Result<bool, AgentError> {
            Ok(true)
        }
    }

    #[tokio::test]
    async fn test_terminal_handler_schema() {
        let handler = TerminalHandler::new(std::sync::Arc::new(MockBackend));
        let schema = handler.schema();
        assert_eq!(schema.name, "terminal");
    }

    #[tokio::test]
    async fn test_terminal_handler_execute() {
        let handler = TerminalHandler::new(std::sync::Arc::new(MockBackend));
        let result = handler
            .execute(json!({"command": "echo hello"}))
            .await
            .unwrap();
        assert!(result.contains("echo hello"));
    }

    #[test]
    fn test_format_command_output() {
        let output = CommandOutput {
            exit_code: 0,
            stdout: "hello".to_string(),
            stderr: String::new(),
        };
        assert_eq!(format_command_output("echo hello", &output), "hello");

        let output_with_stderr = CommandOutput {
            exit_code: 1,
            stdout: "out".to_string(),
            stderr: "err".to_string(),
        };
        let formatted = format_command_output("echo hi", &output_with_stderr);
        assert!(formatted.contains("out"));
        assert!(formatted.contains("err"));
        assert!(formatted.contains("exit code: 1"));
    }

    #[test]
    fn test_format_command_output_redacts_secrets() {
        let output = CommandOutput {
            exit_code: 0,
            stdout: "api_key=hunter2value\nBearer abc123token".to_string(),
            stderr: "token xoxb-1234-abcdef".to_string(),
        };
        let formatted = format_command_output("echo secret", &output);
        assert!(
            !formatted.contains("hunter2value"),
            "secret leaked in formatted output"
        );
        assert!(!formatted.contains("abc123token"));
        assert!(!formatted.contains("xoxb-1234-abcdef"));
        assert!(formatted.contains("[REDACTED]"));
    }

    /// `--readonly`: a command that would require confirmation is DENIED,
    /// even though it is not a `Denied` command.
    #[tokio::test]
    async fn test_readonly_denies_confirmation_commands() {
        let handler = TerminalHandler::with_approval_mode(
            std::sync::Arc::new(MockBackend),
            ApprovalMode::ReadOnly,
        );
        let err = handler
            .execute(json!({"command": "sudo apt install something"}))
            .await
            .expect_err("--readonly must deny confirmation-required commands");
        assert!(
            err.to_string().contains("--readonly"),
            "unexpected error: {err}"
        );
    }

    /// `--readonly`: low-risk (Approved) commands still run.
    #[tokio::test]
    async fn test_readonly_allows_low_risk_commands() {
        let handler = TerminalHandler::with_approval_mode(
            std::sync::Arc::new(MockBackend),
            ApprovalMode::ReadOnly,
        );
        let result = handler
            .execute(json!({"command": "echo hello"}))
            .await
            .expect("low-risk command must run in readonly mode");
        assert!(result.contains("echo hello"));
    }

    /// `--readonly`: outright-denied commands stay denied.
    #[tokio::test]
    async fn test_readonly_denied_commands_stay_denied() {
        let handler = TerminalHandler::with_approval_mode(
            std::sync::Arc::new(MockBackend),
            ApprovalMode::ReadOnly,
        );
        handler
            .execute(json!({"command": "rm -rf /"}))
            .await
            .expect_err("denied commands must stay denied");
    }

    /// `--yes`: a command that would require confirmation is auto-approved
    /// and actually runs against the backend.
    #[tokio::test]
    async fn test_yes_auto_approves_confirmation_commands() {
        let handler = TerminalHandler::with_approval_mode(
            std::sync::Arc::new(MockBackend),
            ApprovalMode::AutoApprove,
        );
        let result = handler
            .execute(json!({"command": "sudo apt install something"}))
            .await
            .expect("--yes must auto-approve confirmation-required commands");
        assert!(result.contains("sudo apt install something"));
    }

    // -----------------------------------------------------------------------
    // Exec-semantics tests (ported from hermes-rs shell.rs behaviors)
    // -----------------------------------------------------------------------

    #[derive(Debug)]
    struct RecordedCall {
        command: String,
        timeout: Option<u64>,
        workdir: Option<String>,
        background: bool,
        pty: bool,
    }

    /// Backend that records its call parameters and returns a fixed stdout.
    struct RecordingBackend {
        calls: Arc<Mutex<Vec<RecordedCall>>>,
        stdout: String,
    }

    impl RecordingBackend {
        fn new(stdout: &str) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                stdout: stdout.to_string(),
            }
        }

        fn last_call(&self) -> RecordedCall {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop()
                .expect("expected at least one recorded call")
        }
    }

    #[async_trait]
    impl TerminalBackend for RecordingBackend {
        async fn execute_command(
            &self,
            cmd: &str,
            timeout: Option<u64>,
            workdir: Option<&str>,
            background: bool,
            pty: bool,
        ) -> Result<CommandOutput, AgentError> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(RecordedCall {
                    command: cmd.to_string(),
                    timeout,
                    workdir: workdir.map(|s| s.to_string()),
                    background,
                    pty,
                });
            Ok(CommandOutput {
                exit_code: 0,
                stdout: self.stdout.clone(),
                stderr: String::new(),
            })
        }
        async fn read_file(
            &self,
            _path: &str,
            _offset: Option<u64>,
            _limit: Option<u64>,
        ) -> Result<String, AgentError> {
            Ok(String::new())
        }
        async fn write_file(&self, _path: &str, _content: &str) -> Result<(), AgentError> {
            Ok(())
        }
        async fn file_exists(&self, _path: &str) -> Result<bool, AgentError> {
            Ok(true)
        }
    }

    fn recording_handler(stdout: &str) -> (TerminalHandler, Arc<RecordingBackend>) {
        let backend = Arc::new(RecordingBackend::new(stdout));
        let handler = TerminalHandler::new(backend.clone());
        (handler, backend)
    }

    #[tokio::test]
    async fn test_timeout_alias_timeout_seconds_honored() {
        let (handler, backend) = recording_handler("ok");
        handler
            .execute(json!({"command": "true", "timeout_seconds": 45}))
            .await
            .unwrap();
        assert_eq!(backend.last_call().timeout, Some(45));
    }

    #[tokio::test]
    async fn test_timeout_canonical_wins_over_aliases() {
        let (handler, backend) = recording_handler("ok");
        handler
            .execute(json!({
                "command": "true",
                "timeout": 10,
                "timeout_seconds": 20,
                "timeout_secs": 30
            }))
            .await
            .unwrap();
        assert_eq!(backend.last_call().timeout, Some(10));
    }

    #[tokio::test]
    async fn test_timeout_seconds_wins_over_timeout_secs() {
        let (handler, backend) = recording_handler("ok");
        handler
            .execute(json!({
                "command": "true",
                "timeout_seconds": 20,
                "timeout_secs": 30
            }))
            .await
            .unwrap();
        assert_eq!(backend.last_call().timeout, Some(20));
    }

    #[tokio::test]
    async fn test_timeout_nonpositive_is_rejected() {
        let (handler, _backend) = recording_handler("ok");
        let err = handler
            .execute(json!({"command": "true", "timeout": 0}))
            .await
            .expect_err("timeout 0 must be rejected");
        assert!(
            err.to_string().contains("positive"),
            "unexpected error: {err}"
        );
        let err = handler
            .execute(json!({"command": "true", "timeout_seconds": -3}))
            .await
            .expect_err("negative timeout must be rejected");
        assert!(
            err.to_string().contains("positive"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_timeout_over_600_promotes_to_background() {
        let (handler, backend) = recording_handler("ok");
        let result = handler
            .execute(json!({"command": "true", "timeout": 900}))
            .await
            .unwrap();
        let call = backend.last_call();
        assert!(call.background, "timeout > 600 must run as background");
        assert_eq!(call.timeout, Some(900));
        assert!(
            result.contains("promoted to background"),
            "result must explain the promotion: {result}"
        );
        assert!(result.contains("900"), "result must name the timeout");
    }

    #[tokio::test]
    async fn test_timeout_600_stays_foreground() {
        let (handler, backend) = recording_handler("ok");
        let result = handler
            .execute(json!({"command": "true", "timeout": 600}))
            .await
            .unwrap();
        assert!(
            !backend.last_call().background,
            "timeout == 600 must stay foreground"
        );
        assert!(
            !result.contains("promoted to background"),
            "no promotion note expected: {result}"
        );
    }

    #[tokio::test]
    async fn test_explicit_background_not_double_promoted() {
        let (handler, backend) = recording_handler("ok");
        let result = handler
            .execute(json!({"command": "true", "background": true, "timeout": 900}))
            .await
            .unwrap();
        assert!(backend.last_call().background);
        assert!(
            !result.contains("promoted to background"),
            "explicit background must not add a promotion note: {result}"
        );
    }

    #[tokio::test]
    async fn test_truncation_keeps_head_and_tail_with_spill() {
        let big: String = "A".repeat(30_000) + &"B".repeat(30_000);
        let total = big.chars().count();
        assert_eq!(total, 60_000);
        let (handler, _backend) = recording_handler(&big);
        let result = handler.execute(json!({"command": "true"})).await.unwrap();

        // Split off the spill report line.
        let (shown, spill_line) = result
            .rsplit_once("\n[full output (")
            .expect("spill report line must be present");
        let head: String = "A".repeat(25_000);
        let tail: String = "B".repeat(25_000);
        assert_eq!(
            shown,
            format!("{head}\n... [truncated 10000 chars] ...\n{tail}"),
            "head+tail truncation with marker"
        );
        assert!(
            shown.len() < big.len(),
            "display text must be shorter than the full output"
        );

        // The spill file must exist and hold the full (redacted) output.
        let path = spill_line
            .trim_start_matches("60000 chars) saved to: ")
            .trim_end_matches(']');
        let spilled = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("spill file missing at {path}: {e}"));
        assert_eq!(spilled, big, "spill must contain the full output");
        assert!(
            path.contains("hermes-exec-spills"),
            "spill must live under hermes-exec-spills: {path}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn test_short_output_is_not_truncated_or_spilled() {
        let (handler, _backend) = recording_handler("hello");
        let result = handler.execute(json!({"command": "true"})).await.unwrap();
        assert_eq!(result, "hello");
        assert!(!result.contains("truncated"));
    }

    #[tokio::test]
    async fn test_cwd_persists_across_calls() {
        let (handler, backend) = recording_handler("ok");
        handler
            .execute(json!({"command": "true", "workdir": "/tmp"}))
            .await
            .unwrap();
        assert_eq!(backend.last_call().workdir.as_deref(), Some("/tmp"));

        // No workdir param → the previous cwd is reused.
        handler.execute(json!({"command": "true"})).await.unwrap();
        assert_eq!(
            backend.last_call().workdir.as_deref(),
            Some("/tmp"),
            "workdir must persist across calls"
        );
    }

    #[tokio::test]
    async fn test_leading_cd_updates_session_cwd() {
        let (handler, backend) = recording_handler("ok");
        handler
            .execute(json!({"command": "true", "workdir": "/tmp"}))
            .await
            .unwrap();

        // Leading `cd /`: this call still runs from /tmp, but the stored
        // cwd becomes / for subsequent calls.
        handler
            .execute(json!({"command": "cd / && true"}))
            .await
            .unwrap();
        assert_eq!(
            backend.last_call().workdir.as_deref(),
            Some("/tmp"),
            "the cd call itself runs from the previous cwd"
        );
        handler.execute(json!({"command": "true"})).await.unwrap();
        assert_eq!(
            backend.last_call().workdir.as_deref(),
            Some("/"),
            "leading cd must update the session cwd"
        );
    }

    #[tokio::test]
    async fn test_failed_cd_keeps_cwd_and_errors() {
        let (handler, backend) = recording_handler("ok");
        handler
            .execute(json!({"command": "true", "workdir": "/tmp"}))
            .await
            .unwrap();

        let err = handler
            .execute(json!({"command": "cd /nonexistent-dir-xyz-123 && true"}))
            .await
            .expect_err("cd to a missing dir must fail");
        assert!(
            err.to_string().contains("no such directory"),
            "unexpected error: {err}"
        );

        // The stored cwd is unchanged after the failed cd.
        handler.execute(json!({"command": "true"})).await.unwrap();
        assert_eq!(backend.last_call().workdir.as_deref(), Some("/tmp"));
    }

    #[tokio::test]
    async fn test_missing_workdir_is_rejected() {
        let (handler, _backend) = recording_handler("ok");
        let err = handler
            .execute(json!({"command": "true", "workdir": "/nonexistent-dir-xyz-123"}))
            .await
            .expect_err("missing workdir must fail");
        assert!(
            err.to_string().contains("no such directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_leading_cd_cases() {
        assert_eq!(
            parse_leading_cd("cd /tmp"),
            Some(("/tmp".to_string(), String::new()))
        );
        assert_eq!(
            parse_leading_cd("cd /tmp && ls"),
            Some(("/tmp".to_string(), "&& ls".to_string()))
        );
        assert_eq!(
            parse_leading_cd("cd \"/my dir\"; ls"),
            Some(("/my dir".to_string(), "; ls".to_string()))
        );
        assert_eq!(
            parse_leading_cd("  cd /tmp  "),
            Some(("/tmp".to_string(), String::new()))
        );
        // Not a pure leading cd.
        assert_eq!(parse_leading_cd("cdx /tmp"), None);
        assert_eq!(parse_leading_cd("echo cd /tmp"), None);
        assert_eq!(parse_leading_cd("cd"), None);
        assert_eq!(parse_leading_cd("cd /tmp || x"), None); // shell operators
        assert_eq!(parse_leading_cd("cd /tmp | grep x"), None);
    }
}
