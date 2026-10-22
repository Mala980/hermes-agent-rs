//! Terminal UI using ratatui + crossterm (Requirement 9.1-9.6).
//!
//! Implements the interactive terminal interface with:
//! - Message history rendering (9.1, 9.4)
//! - Input area with slash command auto-completion (9.2)
//! - Ctrl+C interrupt for tool execution (9.3)
//! - Streaming output display (9.5)
//! - Status bar with model/session info (9.6)
//! - Theme/skin engine support (9.8)

use std::collections::{HashMap, HashSet};
use std::io::Stdout;
use std::path::Path;
use std::time::Duration;

use crossterm::event::{Event as CrosstermEvent, KeyEvent};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use tokio::sync::mpsc;

use hermes_core::{AgentError, StreamChunk};
use hermes_tools::tools::process_registry::{global_process_manager, ProcessInfo, ProcessStatus};

use crate::app::App;
use crate::commands;
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Event
// ---------------------------------------------------------------------------

/// Events that the TUI can process.
#[derive(Debug, Clone)]
pub enum Event {
    /// A keyboard key was pressed.
    Key(KeyEvent),
    /// The terminal was resized.
    Resize(u16, u16),
    /// An asynchronous message (e.g. from agent streaming).
    Message(String),
    /// Agent produced a streaming delta.
    StreamDelta(String),
    /// Agent produced a full stream chunk (including control metadata).
    StreamChunk(StreamChunk),
    /// Agent finished processing.
    AgentDone,
    /// Interrupt signal (Ctrl+C).
    Interrupt,
    /// Periodic tick (150ms): drives the spinner and panel refreshes.
    Tick,
}

// ---------------------------------------------------------------------------
// InputMode
// ---------------------------------------------------------------------------

/// Current input mode for the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// Normal mode: keys are interpreted as commands.
    Normal,
    /// Insert mode: keys are inserted into the input buffer.
    Insert,
    /// Command mode: entering a slash command with auto-completion.
    Command,
}

impl std::fmt::Display for InputMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputMode::Normal => write!(f, "NORMAL"),
            InputMode::Insert => write!(f, "INSERT"),
            InputMode::Command => write!(f, "COMMAND"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tui
// ---------------------------------------------------------------------------

/// The terminal UI wrapper.
///
/// Owns the ratatui Terminal and provides methods for rendering,
/// event handling, and theme management.
pub struct Tui {
    /// The ratatui terminal backend.
    pub terminal: ratatui::Terminal<CrosstermBackend<Stdout>>,
    /// Channel receiver for async events.
    pub events: mpsc::UnboundedReceiver<Event>,
    /// Channel sender for async events.
    event_sender: mpsc::UnboundedSender<Event>,
    /// The active color theme.
    theme: Theme,
}

impl Tui {
    /// Create a new Tui instance, initializing the terminal.
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        stdout.execute(EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = ratatui::Terminal::new(backend)?;
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        Ok(Self {
            terminal,
            events: event_receiver,
            event_sender,
            theme: Theme::default_theme(),
        })
    }

    /// Restore the terminal to its original state.
    pub fn restore(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        disable_raw_mode()?;
        self.terminal.backend_mut().execute(LeaveAlternateScreen)?;
        self.terminal.show_cursor()?;
        Ok(())
    }

    /// Get a sender for injecting events (used by async tasks).
    pub fn event_sender(&self) -> mpsc::UnboundedSender<Event> {
        self.event_sender.clone()
    }

    /// Set the active theme.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// Get a reference to the current theme.
    pub fn theme(&self) -> &Theme {
        &self.theme
    }
}

// ---------------------------------------------------------------------------
// TuiState — holds the mutable state of the TUI between frames
// ---------------------------------------------------------------------------

/// Mutable state for the TUI rendering loop.
pub struct TuiState {
    /// Current input mode.
    pub mode: InputMode,
    /// Current input buffer (supports multi-line).
    pub input: String,
    /// Cursor position within the input buffer (byte offset).
    pub cursor_position: usize,
    /// Auto-completion suggestions (populated in Command mode).
    pub completions: Vec<String>,
    /// Currently selected completion index (if any).
    pub completion_index: Option<usize>,
    /// Whether the agent is currently processing.
    pub processing: bool,
    /// Buffer for streaming agent output.
    pub stream_buffer: String,
    /// Whether post-response deltas are currently muted.
    pub stream_muted: bool,
    /// Whether the next visible token should be prefixed by a paragraph break.
    pub stream_needs_break: bool,
    /// Status message shown in the status bar.
    pub status_message: String,
    /// Selection anchor for text selection (byte offset, None if no selection).
    pub selection_anchor: Option<usize>,
    /// Message history index for browsing previous messages.
    pub message_browse_index: Option<usize>,
    /// Whether we are in history search mode (Ctrl+R).
    pub history_search_active: bool,
    /// Current history search query.
    pub history_search_query: String,
    /// Spinner frame counter for tool execution indicator.
    pub spinner_frame: usize,
    /// Tool output sections with fold state (tool_name, output, is_expanded).
    pub tool_outputs: Vec<ToolOutputSection>,
    /// Active color theme (F2 toggles dark/light; restored from config at startup).
    pub theme: Theme,
    /// Session goal text set via `/goal`; None = not set (goal bar shows "—").
    pub goal: Option<String>,
    /// Number of submitted user turns this session (goal bar).
    pub turn_count: u32,
    /// Number of running background processes (goal bar).
    pub bg_count: usize,
    /// Whether the help overlay (F1) is open.
    pub show_help: bool,
    /// Whether the background-process panel (F4) is open.
    pub show_agents: bool,
    /// Last snapshot of the shared process registry; refreshed on tick while open.
    pub agents: Vec<ProcessInfo>,
    /// Refresh the agents snapshot on the next tick.
    pub agents_refresh_pending: bool,
    /// Whether the session-switch overlay (F3) is open.
    pub show_sessions: bool,
    /// Saved sessions, newest first.
    pub sessions: Vec<SessionEntry>,
    /// Highlighted index in the session overlay.
    pub session_idx: usize,
}

/// A section of tool output that can be folded/expanded.
#[derive(Debug, Clone)]
pub struct ToolOutputSection {
    /// Name of the tool that produced this output.
    pub tool_name: String,
    /// Full output text.
    pub output: String,
    /// Whether the section is expanded (showing full output).
    pub is_expanded: bool,
    /// Number of preview lines to show when collapsed.
    pub preview_lines: usize,
}

impl ToolOutputSection {
    pub fn new(tool_name: String, output: String) -> Self {
        Self {
            tool_name,
            output,
            is_expanded: false,
            preview_lines: 3,
        }
    }

    /// Get the display text (collapsed or expanded).
    pub fn display_text(&self) -> String {
        if self.is_expanded {
            self.output.clone()
        } else {
            let lines: Vec<&str> = self.output.lines().take(self.preview_lines).collect();
            let total_lines = self.output.lines().count();
            let mut text = lines.join("\n");
            if total_lines > self.preview_lines {
                text.push_str(&format!(
                    "\n  ... ({} more lines, press Enter to expand)",
                    total_lines - self.preview_lines
                ));
            }
            text
        }
    }
}

// ---------------------------------------------------------------------------
// Theme helpers (F2 toggle + config persistence)
// ---------------------------------------------------------------------------

/// Map a saved theme name to a [`Theme`]; unknown names fall back to dark.
fn theme_for_name(name: &str) -> Theme {
    match name {
        "light" => crate::theme::light_theme(),
        _ => crate::theme::default_theme(),
    }
}

/// Normalize a saved `tui.theme` config value to a known theme name.
fn restore_theme_name(saved: Option<&str>) -> &'static str {
    match saved {
        Some("light") => "light",
        _ => "dark",
    }
}

/// Persist the active theme by patching the on-disk config file: load the
/// raw file, patch only the `tui.theme` field, save it back. Because the
/// raw file (not the env-merged effective config) is round-tripped,
/// env-expanded secrets are never baked into disk.
fn persist_theme_to_config(theme_name: &str, home_dir: Option<&Path>) -> Result<(), String> {
    let home = match home_dir {
        Some(dir) => dir.to_path_buf(),
        None => hermes_config::hermes_home(),
    };
    let path = home.join("config.yaml");
    let mut raw = hermes_config::load_user_config_file(&path).map_err(|e| e.to_string())?;
    hermes_config::apply_user_config_patch(&mut raw, "tui.theme", theme_name)
        .map_err(|e| e.to_string())?;
    hermes_config::save_config_yaml(&path, &raw).map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Goal bar + /goal
// ---------------------------------------------------------------------------

/// Handle `/goal` in the TUI input: `/goal <text>` sets the session goal,
/// `/goal` shows it, `/goal clear` clears it. Feedback goes to the status
/// bar — never into `App::messages` (those go to the model).
/// Returns true when the input was a goal command (already handled).
fn handle_goal_command(state: &mut TuiState, text: &str) -> bool {
    let rest = if text == "/goal" {
        None
    } else if let Some(r) = text.strip_prefix("/goal ") {
        Some(r)
    } else {
        return false;
    };
    match rest {
        None => {
            let cur = state.goal.clone().unwrap_or_else(|| "—".to_string());
            state.status_message = format!("current goal: {cur}");
        }
        Some(r) if r.trim().is_empty() => {
            let cur = state.goal.clone().unwrap_or_else(|| "—".to_string());
            state.status_message = format!("current goal: {cur}");
        }
        Some(r) if r.trim() == "clear" || r.trim() == "-" => {
            state.goal = None;
            state.status_message = "goal cleared".to_string();
        }
        Some(r) => {
            let g = r.trim().to_string();
            state.status_message = format!("goal set: {g}");
            state.goal = Some(g);
        }
    }
    true
}

/// One-line goal bar text: `◈ turn N · [bg M] · goal: …`.
fn goal_bar_text(state: &TuiState) -> String {
    let goal = state.goal.as_deref().unwrap_or("—");
    format!(
        "◈ turn {} · [bg {}] · goal: {goal}",
        state.turn_count, state.bg_count
    )
}

// ---------------------------------------------------------------------------
// Diff-stat for tool results
// ---------------------------------------------------------------------------

/// Per-tool diff statistics: lines added (`+`) and removed (`-`) in a
/// unified diff carried by a tool result (e.g. `edit_file`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffStat {
    pub added: u64,
    pub removed: u64,
}

/// Count [`DiffStat`] from tool-result text. Only unified-diff hunk lines
/// count: `+`/`-` lines after a `@@` header, excluding the `+++`/`---` file
/// headers. Returns `None` when the text carries no diff.
fn diff_stat_from_text(content: &str) -> Option<DiffStat> {
    let mut added = 0u64;
    let mut removed = 0u64;
    let mut in_hunk = false;
    for line in content.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') => added += 1,
            Some(b'-') => removed += 1,
            _ => {}
        }
    }
    if added == 0 && removed == 0 {
        None
    } else {
        Some(DiffStat { added, removed })
    }
}

// ---------------------------------------------------------------------------
// Tool-call / tool-result pairing
// ---------------------------------------------------------------------------

/// A tool call paired with its result, rendered as one ✓ line.
#[derive(Debug, Clone)]
struct PairedToolCall {
    name: String,
    diff: Option<DiffStat>,
    preview: String,
}

/// Pairing of assistant tool calls with their Tool results.
#[derive(Debug, Default)]
struct ToolPairing {
    /// Assistant message index → paired results, in tool-call order.
    paired: HashMap<usize, Vec<PairedToolCall>>,
    /// Assistant message index → tool-call names still awaiting a result.
    pending: HashMap<usize, Vec<String>>,
    /// Tool message indices consumed into a ✓ line (not rendered separately).
    consumed: HashSet<usize>,
}

/// Pair assistant tool calls with their Tool results via `tool_call_id`,
/// with a positional fallback (first unmatched Tool message after the call).
/// Paired Tool messages are consumed into the ✓ line.
fn pair_tool_calls(messages: &[hermes_core::Message]) -> ToolPairing {
    use hermes_core::{MessageRole, ToolCall};
    let mut pairing = ToolPairing::default();
    // Tool message indices in order; each is consumable exactly once.
    let mut available: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == MessageRole::Tool)
        .map(|(i, _)| i)
        .collect();

    // Assistant tool calls in order: (message index, call).
    let mut calls: Vec<(usize, &ToolCall)> = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        if msg.role != MessageRole::Assistant {
            continue;
        }
        if let Some(cs) = msg.tool_calls.as_deref() {
            for call in cs {
                calls.push((i, call));
            }
        }
    }

    fn make_pair(call: &ToolCall, result: &hermes_core::Message) -> PairedToolCall {
        let content = result.content.as_deref().unwrap_or("");
        PairedToolCall {
            name: call.function.name.clone(),
            diff: diff_stat_from_text(content),
            preview: preview_text(content, 120),
        }
    }

    // Assigned tool-message index per call (None = still unpaired).
    let mut assigned: Vec<Option<usize>> = vec![None; calls.len()];

    // Pass 1: id matches take priority.
    for (k, (i, call)) in calls.iter().enumerate() {
        if call.id.is_empty() {
            continue;
        }
        if let Some(p) = available
            .iter()
            .position(|&j| j > *i && messages[j].tool_call_id.as_deref() == Some(call.id.as_str()))
        {
            assigned[k] = Some(available.remove(p));
        }
    }
    // Pass 2: positional fallback for the rest.
    for (k, (i, _)) in calls.iter().enumerate() {
        if assigned[k].is_some() {
            continue;
        }
        if let Some(p) = available.iter().position(|&j| j > *i) {
            assigned[k] = Some(available.remove(p));
        }
    }

    for (k, (i, call)) in calls.iter().enumerate() {
        match assigned[k] {
            Some(j) => {
                pairing.consumed.insert(j);
                pairing
                    .paired
                    .entry(*i)
                    .or_default()
                    .push(make_pair(call, &messages[j]));
            }
            None => {
                pairing
                    .pending
                    .entry(*i)
                    .or_default()
                    .push(call.function.name.clone());
            }
        }
    }
    pairing
}

/// Flatten whitespace and truncate to `max_chars` for one-line previews.
fn preview_text(content: &str, max_chars: usize) -> String {
    let flat: String = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(max_chars).collect();
    if flat.chars().count() > max_chars {
        out.push('…');
    }
    out
}

/// Truncate to `max` chars, appending `…` when truncated.
fn truncate_chars(s: &str, max: usize) -> String {
    let t: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        format!("{t}…")
    } else {
        t
    }
}

// ---------------------------------------------------------------------------
// Saved sessions (F3 switcher)
// ---------------------------------------------------------------------------

/// One saved session from `~/.hermes/sessions/*.json` (the `/save` format).
#[derive(Debug, Clone)]
pub struct SessionEntry {
    /// File stem (session id or custom name).
    pub name: String,
    /// First user message, truncated — shown as the title.
    pub title: String,
    /// Last-modified time of the session file.
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// Number of messages in the saved file.
    pub message_count: usize,
}

/// A session file parsed for switching.
#[derive(Debug)]
struct LoadedSession {
    session_id: String,
    model: Option<String>,
    messages: Vec<hermes_core::Message>,
}

/// Maximum sessions shown in the switcher overlay.
const SESSION_LIST_LIMIT: usize = 30;

/// List saved sessions newest-first. TUI-safe: never touches stdout.
fn list_saved_sessions() -> Vec<SessionEntry> {
    list_saved_sessions_in(&hermes_config::sessions_dir())
}

/// List saved sessions in `dir`, newest-first (testable without touching
/// the real home directory).
fn list_saved_sessions_in(dir: &Path) -> Vec<SessionEntry> {
    let mut entries = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return entries,
    };
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        if let Some(e) = session_entry_from_file(&path) {
            entries.push(e);
        }
    }
    entries.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
    entries.truncate(SESSION_LIST_LIMIT);
    entries
}

fn session_entry_from_file(path: &Path) -> Option<SessionEntry> {
    let content = std::fs::read_to_string(path).ok()?;
    let data: serde_json::Value = serde_json::from_str(&content).ok()?;
    let messages = data.get("messages")?.as_array()?;
    let title = messages
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("User"))
        .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
        .map(|c| c.lines().next().unwrap_or("").trim())
        .find(|t| !t.is_empty())
        .map(|t| truncate_chars(t, 48))
        .unwrap_or_else(|| "(untitled)".to_string());
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    Some(SessionEntry {
        name: path.file_stem().map(|s| s.to_string_lossy().into_owned())?,
        title,
        updated_at: chrono::DateTime::<chrono::Utc>::from(mtime),
        message_count: messages.len(),
    })
}

fn message_from_saved(role: &str, content: &str) -> hermes_core::Message {
    match role {
        "Assistant" => hermes_core::Message::assistant(content),
        "System" => hermes_core::Message::system(content),
        "Tool" => hermes_core::Message::tool_result("", content),
        _ => hermes_core::Message::user(content),
    }
}

fn load_session_file(path: &Path) -> Result<LoadedSession, String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("read failed: {e}"))?;
    let data: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| format!("parse failed: {e}"))?;
    let info = data.get("session_info");
    let session_id = info
        .and_then(|i| i.get("session_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    let model = info
        .and_then(|i| i.get("model"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let mut messages = Vec::new();
    if let Some(arr) = data.get("messages").and_then(|m| m.as_array()) {
        for m in arr {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("User");
            let content = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
            messages.push(message_from_saved(role, content));
        }
    }
    Ok(LoadedSession {
        session_id,
        model,
        messages,
    })
}

/// Compact relative time for the session list ("5m ago").
fn fmt_ago(dt: chrono::DateTime<chrono::Utc>) -> String {
    let s = chrono::Utc::now()
        .signed_duration_since(dt)
        .num_seconds()
        .max(0);
    if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86400)
    }
}

// ---------------------------------------------------------------------------
// Agents panel (F4) — fed by the shared process registry
// ---------------------------------------------------------------------------

/// Lines for the F4 background-process panel from a registry snapshot:
/// status glyph (●/✓/■/✗) + name/pid/command/status.
fn agents_panel_lines(procs: &[ProcessInfo], inner_width: u16) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut out = vec![
        Line::from(Span::styled(
            "Background processes — F4/Esc close",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    if procs.is_empty() {
        out.push(Line::from(Span::styled("No background processes.", dim)));
        out.push(Line::from(Span::styled(
            "exec with background: true registers processes here.",
            dim,
        )));
        return out;
    }
    let w = inner_width as usize;
    for p in procs.iter().take(20) {
        let (glyph, color, status) = match p.status {
            ProcessStatus::Running => ("●", Color::Yellow, "running".to_string()),
            ProcessStatus::Exited => (
                "✓",
                Color::Green,
                match p.exit_code {
                    Some(0) | None => "exited".to_string(),
                    Some(c) => format!("exit {c}"),
                },
            ),
            ProcessStatus::Stopped => ("■", Color::DarkGray, "stopped".to_string()),
            ProcessStatus::Failed => (
                "✗",
                Color::Red,
                match p.exit_code {
                    Some(c) => format!("failed (exit {c})"),
                    None => "failed".to_string(),
                },
            ),
        };
        let cmd = truncate_chars(&p.command, w.saturating_sub(28).max(8));
        out.push(Line::from(vec![
            Span::styled(
                format!("{glyph} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{} ", p.name), Style::default().fg(Color::White)),
            Span::styled(format!("pid {} ", p.pid), dim),
            Span::styled(cmd, Style::default().fg(color)),
            Span::styled(format!(" · {status}"), dim),
        ]));
    }
    if procs.len() > 20 {
        out.push(Line::from(Span::styled(
            format!("… and {} more", procs.len() - 20),
            dim,
        )));
    }
    out
}

// ---------------------------------------------------------------------------
// Overlay line builders
// ---------------------------------------------------------------------------

/// Lines for the F3 session-switch overlay: title (first user message),
/// relative time, message count; the active session is marked ●.
fn session_overlay_lines(
    state: &TuiState,
    active_id: &str,
    colors: &crate::theme::RatatuiColors,
    inner_width: u16,
) -> Vec<Line<'static>> {
    let mut out = vec![
        Line::from(Span::styled(
            "Switch session — Enter select · Esc/F3 close",
            Style::default()
                .fg(colors.accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    if state.sessions.is_empty() {
        out.push(Line::from(Span::styled(
            "No saved sessions yet — use /save first.",
            Style::default()
                .fg(colors.secondary)
                .add_modifier(Modifier::ITALIC),
        )));
        return out;
    }
    let w = (inner_width as usize).saturating_sub(24).max(10);
    for (i, s) in state.sessions.iter().enumerate() {
        let selected = i == state.session_idx;
        let mut title = truncate_chars(&s.title, w);
        if title.trim().is_empty() {
            title = "(untitled)".to_string();
        }
        let mut spans = vec![
            Span::styled(
                if selected { "▸ " } else { "  " },
                Style::default()
                    .fg(if selected {
                        colors.accent
                    } else {
                        colors.secondary
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                title,
                if selected {
                    Style::default()
                        .fg(colors.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(colors.foreground)
                },
            ),
        ];
        if s.name == active_id {
            spans.push(Span::styled(" ●", Style::default().fg(colors.success)));
        }
        spans.push(Span::styled(
            format!(" — {} ({} msgs)", fmt_ago(s.updated_at), s.message_count),
            Style::default().fg(colors.secondary),
        ));
        out.push(Line::from(spans));
    }
    out
}

/// Lines for the F1 help overlay: keys and commands the TUI supports.
fn help_lines(theme: &Theme) -> Vec<Line<'static>> {
    let colors = theme.colors.to_ratatui_colors();
    let dim = Style::default().fg(colors.secondary);
    let key = |k: &str, d: &str| {
        Line::from(vec![
            Span::styled(
                format!("  {k:<14}"),
                Style::default()
                    .fg(colors.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(d.to_string(), dim),
        ])
    };
    vec![
        Line::from(Span::styled(
            "Keys",
            Style::default()
                .fg(colors.foreground)
                .add_modifier(Modifier::BOLD),
        )),
        key("Enter", "send message (Shift+Enter: newline)"),
        key("Esc", "clear input / close popup / exit"),
        key("Ctrl+C", "interrupt running turn / exit"),
        key("F1 / ?", "this help (? when input is empty)"),
        key("F2", &format!("toggle theme (active: {})", theme.name)),
        key("F3", "switch session"),
        key("F4", "background processes (toggle)"),
        key("Up / Down", "history / navigate session list"),
        key("Tab", "accept completion"),
        Line::from(""),
        Line::from(Span::styled(
            "Commands",
            Style::default()
                .fg(colors.foreground)
                .add_modifier(Modifier::BOLD),
        )),
        key("/goal <text>", "set session goal (shown in goal bar)"),
        key("/goal", "show current goal"),
        key("/goal clear", "clear goal"),
        key("/save [name]", "save session to disk"),
        key("/load <name>", "load a saved session"),
        Line::from(""),
        Line::from(Span::styled(
            "Display",
            Style::default()
                .fg(colors.foreground)
                .add_modifier(Modifier::BOLD),
        )),
        key("splash", "logo art on empty session (terminal ≥104 cols)"),
        key("markdown", "assistant replies render headings, code, lists"),
        Line::from(""),
        Line::from(Span::styled(
            "Esc to close",
            Style::default()
                .fg(colors.secondary)
                .add_modifier(Modifier::ITALIC),
        )),
    ]
}

/// Centered rectangle for overlays: `percent_x` of the area width, fixed height.
fn centered_rect(percent_x: u16, height: u16, r: Rect) -> Rect {
    let w = (r.width * percent_x / 100).max(24).min(r.width);
    let h = height.min(r.height);
    Rect {
        x: r.x + r.width.saturating_sub(w) / 2,
        y: r.y + r.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            mode: InputMode::Insert,
            input: String::new(),
            cursor_position: 0,
            completions: Vec::new(),
            completion_index: None,
            processing: false,
            stream_buffer: String::new(),
            stream_muted: false,
            stream_needs_break: false,
            status_message: String::new(),
            selection_anchor: None,
            message_browse_index: None,
            history_search_active: false,
            history_search_query: String::new(),
            spinner_frame: 0,
            tool_outputs: Vec::new(),
            theme: Theme::default_theme(),
            goal: None,
            turn_count: 0,
            bg_count: 0,
            show_help: false,
            show_agents: false,
            agents: Vec::new(),
            agents_refresh_pending: false,
            show_sessions: false,
            sessions: Vec::new(),
            session_idx: 0,
        }
    }
}

impl TuiState {
    /// Handle a key event and return whether the app should quit.
    ///
    /// Global keys (F1–F4, Esc, `?`) are handled before the modal dispatch:
    /// - F1 toggles the help overlay (`?` opens it when the input is empty)
    /// - F2 toggles dark/light theme (persisted to the config file)
    /// - F3 toggles the session-switch overlay
    /// - F4 toggles the background-process panel
    /// - Esc priority: close sessions overlay → close help → close agents
    ///   panel → clear input → exit
    pub fn handle_key(&mut self, key: KeyEvent, app: &mut App) -> bool {
        use crossterm::event::KeyCode;

        // The session overlay captures its own navigation keys.
        if self.show_sessions {
            match key.code {
                KeyCode::Esc | KeyCode::F(3) => self.close_session_overlay(),
                KeyCode::Up => self.session_nav(-1),
                KeyCode::Down => self.session_nav(1),
                KeyCode::Enter => self.switch_to_selected_session(app),
                _ => {}
            }
            return false;
        }

        match key.code {
            KeyCode::F(1) => {
                self.show_help = !self.show_help;
                return false;
            }
            KeyCode::F(2) => {
                self.toggle_theme_persist(app);
                return false;
            }
            KeyCode::F(3) => {
                self.open_session_overlay();
                return false;
            }
            KeyCode::F(4) => {
                self.toggle_agents_panel();
                return false;
            }
            KeyCode::Esc => return self.on_esc(),
            KeyCode::Char('?') if self.input.is_empty() => {
                self.show_help = true;
                return false;
            }
            _ => {}
        }

        match self.mode {
            InputMode::Normal => self.handle_normal_key(key, app),
            InputMode::Insert => self.handle_insert_key(key, app),
            InputMode::Command => self.handle_command_key(key, app),
        }
    }

    /// Esc priority: close sessions overlay → close help → close agents
    /// panel → deactivate history search → clear input → exit.
    /// Returns true when the TUI should exit.
    fn on_esc(&mut self) -> bool {
        if self.show_sessions {
            self.close_session_overlay();
            false
        } else if self.show_help {
            self.show_help = false;
            false
        } else if self.show_agents {
            self.show_agents = false;
            false
        } else if self.history_search_active {
            self.history_search_active = false;
            self.history_search_query.clear();
            false
        } else if !self.input.is_empty() {
            self.input.clear();
            self.cursor_position = 0;
            self.completions.clear();
            self.completion_index = None;
            false
        } else {
            true
        }
    }

    /// Restore the active theme from a saved config name (`"dark"`/`"light"`);
    /// unknown names fall back to dark.
    pub fn set_theme_by_name(&mut self, name: &str) {
        self.theme = theme_for_name(name);
    }

    /// Toggle dark/light; returns the new theme name.
    pub fn cycle_theme(&mut self) -> &'static str {
        let next = if self.theme.name == "light" {
            "dark"
        } else {
            "light"
        };
        self.theme = theme_for_name(next);
        next
    }

    /// Toggle the theme and persist it to the on-disk config file.
    fn toggle_theme_persist(&mut self, app: &App) {
        let name = self.cycle_theme();
        let home = app.config.home_dir.clone().map(std::path::PathBuf::from);
        match persist_theme_to_config(name, home.as_deref()) {
            Ok(()) => self.status_message = format!("Theme: {name} (saved)"),
            Err(e) => self.status_message = format!("Theme: {name} (save failed: {e})"),
        }
    }

    /// Toggle the background-process panel (F4). Opening closes the other
    /// overlays; the snapshot refresh itself happens on the next tick.
    fn toggle_agents_panel(&mut self) {
        if self.show_agents {
            self.show_agents = false;
            self.agents_refresh_pending = false;
        } else {
            self.show_help = false;
            self.close_session_overlay();
            self.show_agents = true;
            self.agents_refresh_pending = true;
        }
    }

    /// Open the session-switch overlay, loading the saved-session list.
    /// Loading is TUI-safe (no stdout); failures go to the status bar.
    fn open_session_overlay(&mut self) {
        self.show_help = false;
        self.show_agents = false;
        self.sessions = list_saved_sessions();
        self.session_idx = 0;
        self.show_sessions = true;
    }

    fn close_session_overlay(&mut self) {
        self.show_sessions = false;
        self.session_idx = 0;
    }

    /// Navigate the session list; wraps around at both ends.
    fn session_nav(&mut self, dir: i32) {
        let n = self.sessions.len();
        if n == 0 {
            return;
        }
        self.session_idx = (self.session_idx as i32 + dir).rem_euclid(n as i32) as usize;
    }

    fn session_select(&self) -> Option<&SessionEntry> {
        self.sessions.get(self.session_idx)
    }

    /// Switch to the highlighted session. Guard: refuse while a turn is
    /// running. The session goal survives the switch.
    fn switch_to_selected_session(&mut self, app: &mut App) {
        if !self.check_switch_guard() {
            return;
        }
        let Some(sel) = self.session_select().cloned() else {
            return;
        };
        let path = hermes_config::sessions_dir().join(format!("{}.json", sel.name));
        match load_session_file(&path) {
            Ok(loaded) => {
                let count = loaded.messages.len();
                let title = sel.title.clone();
                app.session_id = loaded.session_id;
                if let Some(model) = loaded.model {
                    app.current_model = model;
                }
                app.messages = loaded.messages;
                self.apply_session_switch_state(&title, count);
            }
            Err(e) => {
                self.status_message = format!("Failed to load session: {e}");
            }
        }
    }

    /// Guard for session switching: refuse while a turn is running.
    /// Returns true when the switch may proceed.
    fn check_switch_guard(&mut self) -> bool {
        if self.processing {
            self.status_message =
                "Wait for the running turn to finish before switching sessions.".to_string();
            return false;
        }
        true
    }

    /// State-side half of a session switch (testable without `App`).
    /// The session goal is intentionally untouched — it survives switches.
    fn apply_session_switch_state(&mut self, title: &str, message_count: usize) {
        self.stream_buffer.clear();
        self.close_session_overlay();
        self.status_message = format!("Switched to session \"{title}\" ({message_count} messages)");
    }

    fn handle_normal_key(&mut self, key: KeyEvent, _app: &mut App) -> bool {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Char('i') => {
                self.mode = InputMode::Insert;
            }
            KeyCode::Char(':') => {
                self.mode = InputMode::Command;
                self.input.clear();
                self.cursor_position = 0;
            }
            KeyCode::Char('q') => {
                return true; // quit
            }
            _ => {}
        }
        false
    }

    fn handle_insert_key(&mut self, key: KeyEvent, app: &mut App) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mods = key.modifiers;
        match key.code {
            // Shift+Enter → insert newline (multi-line editing)
            KeyCode::Enter if mods.contains(KeyModifiers::SHIFT) => {
                self.input.insert(self.cursor_position, '\n');
                self.cursor_position += 1;
                self.selection_anchor = None;
                false
            }
            // Enter (with/without Ctrl/Alt) → submit handled by caller.
            KeyCode::Enter => false,
            // Ctrl+A → move to beginning of line
            KeyCode::Char('a') if mods.contains(KeyModifiers::CONTROL) => {
                self.cursor_position = self.line_start();
                self.selection_anchor = None;
                false
            }
            // Ctrl+E → move to end of line
            KeyCode::Char('e') if mods.contains(KeyModifiers::CONTROL) => {
                self.cursor_position = self.line_end();
                self.selection_anchor = None;
                false
            }
            // Ctrl+V → paste from clipboard (best-effort via crossterm)
            KeyCode::Char('v') if mods.contains(KeyModifiers::CONTROL) => {
                // Clipboard paste is platform-dependent; we handle pasted text
                // via the bracketed paste event in crossterm. This is a no-op
                // placeholder — actual paste arrives as rapid Char events.
                false
            }
            // Ctrl+R → toggle history search
            KeyCode::Char('r') if mods.contains(KeyModifiers::CONTROL) => {
                self.history_search_active = !self.history_search_active;
                if !self.history_search_active {
                    self.history_search_query.clear();
                }
                false
            }
            KeyCode::Home => {
                self.cursor_position = self.line_start();
                self.selection_anchor = None;
                false
            }
            KeyCode::End => {
                self.cursor_position = self.line_end();
                self.selection_anchor = None;
                false
            }
            KeyCode::Char(c) => {
                if self.history_search_active {
                    self.history_search_query.push(c);
                    // Search through history
                    if let Some(found) = app
                        .input_history
                        .iter()
                        .rev()
                        .find(|h| h.contains(&self.history_search_query))
                    {
                        self.input = found.clone();
                        self.cursor_position = self.input.len();
                    }
                    return false;
                }
                self.input.insert(self.cursor_position, c);
                self.cursor_position += c.len_utf8();
                self.selection_anchor = None;
                // Check for slash command auto-completion
                if self.input.starts_with('/') {
                    self.update_completions();
                }
                false
            }
            KeyCode::Backspace => {
                if self.history_search_active {
                    self.history_search_query.pop();
                    return false;
                }
                if self.cursor_position > 0 {
                    // Find the previous char boundary
                    let prev = self.input[..self.cursor_position]
                        .char_indices()
                        .last()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.input.drain(prev..self.cursor_position);
                    self.cursor_position = prev;
                }
                self.selection_anchor = None;
                if self.input.starts_with('/') {
                    self.update_completions();
                } else {
                    self.completions.clear();
                    self.completion_index = None;
                }
                false
            }
            KeyCode::Left => {
                if self.cursor_position > 0 {
                    // Move to previous char boundary
                    self.cursor_position = self.input[..self.cursor_position]
                        .char_indices()
                        .last()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                self.selection_anchor = None;
                false
            }
            KeyCode::Right => {
                if self.cursor_position < self.input.len() {
                    // Move to next char boundary
                    self.cursor_position = self.input[self.cursor_position..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| self.cursor_position + i)
                        .unwrap_or(self.input.len());
                }
                self.selection_anchor = None;
                false
            }
            KeyCode::Up => {
                // In multi-line: move cursor up one line
                if self.input.contains('\n') {
                    if let Some(new_pos) = self.cursor_up() {
                        self.cursor_position = new_pos;
                        return false;
                    }
                }
                // Single-line or at top: browse history
                if let Some(prev) = app.history_prev() {
                    self.input = prev.to_string();
                    self.cursor_position = self.input.len();
                }
                false
            }
            KeyCode::Down => {
                // In multi-line: move cursor down one line
                if self.input.contains('\n') {
                    if let Some(new_pos) = self.cursor_down() {
                        self.cursor_position = new_pos;
                        return false;
                    }
                }
                // Single-line or at bottom: browse history
                if let Some(next) = app.history_next() {
                    self.input = next.to_string();
                    self.cursor_position = self.input.len();
                }
                false
            }
            KeyCode::Tab => {
                // Accept completion
                if let Some(idx) = self.completion_index {
                    if idx < self.completions.len() {
                        self.input = self.completions[idx].clone();
                        self.cursor_position = self.input.len();
                    }
                } else if !self.completions.is_empty() {
                    self.input = self.completions[0].clone();
                    self.cursor_position = self.input.len();
                }
                self.completions.clear();
                self.completion_index = None;
                false
            }
            _ => false,
        }
    }

    fn handle_command_key(&mut self, key: KeyEvent, _app: &mut App) -> bool {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Enter => {
                let input = std::mem::take(&mut self.input);
                self.cursor_position = 0;
                self.mode = InputMode::Insert;
                self.completions.clear();
                self.completion_index = None;
                let _ = input; // Processed outside
                false
            }
            KeyCode::Tab => {
                // Cycle through completions
                if !self.completions.is_empty() {
                    let idx = self
                        .completion_index
                        .map(|i| (i + 1) % self.completions.len())
                        .unwrap_or(0);
                    self.completion_index = Some(idx);
                    self.input = self.completions[idx].clone();
                    self.cursor_position = self.input.len();
                }
                false
            }
            _ => {
                // Delegate to insert handler for typing
                self.handle_insert_key(key, _app)
            }
        }
    }

    /// Update auto-completion suggestions based on current input.
    fn update_completions(&mut self) {
        if self.input.starts_with('/') {
            self.completions = commands::autocomplete(&self.input)
                .into_iter()
                .map(String::from)
                .collect();
            self.completion_index = None;
        } else {
            self.completions.clear();
            self.completion_index = None;
        }
    }

    // -----------------------------------------------------------------------
    // Multi-line cursor helpers
    // -----------------------------------------------------------------------

    /// Get the byte offset of the start of the current line.
    fn line_start(&self) -> usize {
        self.input[..self.cursor_position]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0)
    }

    /// Get the byte offset of the end of the current line.
    fn line_end(&self) -> usize {
        self.input[self.cursor_position..]
            .find('\n')
            .map(|i| self.cursor_position + i)
            .unwrap_or(self.input.len())
    }

    /// Column offset within the current line.
    fn current_column(&self) -> usize {
        self.cursor_position - self.line_start()
    }

    /// Move cursor up one line, returning the new byte offset or None if at top.
    fn cursor_up(&self) -> Option<usize> {
        let line_start = self.line_start();
        if line_start == 0 {
            return None; // already on first line
        }
        let col = self.current_column();
        // Previous line ends at line_start - 1 (the '\n')
        let prev_line_end = line_start - 1;
        let prev_line_start = self.input[..prev_line_end]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let prev_line_len = prev_line_end - prev_line_start;
        Some(prev_line_start + col.min(prev_line_len))
    }

    /// Move cursor down one line, returning the new byte offset or None if at bottom.
    fn cursor_down(&self) -> Option<usize> {
        let line_end = self.line_end();
        if line_end >= self.input.len() {
            return None; // already on last line
        }
        let col = self.current_column();
        let next_line_start = line_end + 1;
        let next_line_end = self.input[next_line_start..]
            .find('\n')
            .map(|i| next_line_start + i)
            .unwrap_or(self.input.len());
        let next_line_len = next_line_end - next_line_start;
        Some(next_line_start + col.min(next_line_len))
    }

    /// Get the spinner character for the current frame.
    pub fn spinner_char(&self) -> char {
        const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        SPINNER[self.spinner_frame % SPINNER.len()]
    }

    /// Advance the spinner frame.
    pub fn tick_spinner(&mut self) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the full TUI frame.
pub fn render(frame: &mut Frame, app: &App, state: &TuiState) {
    let theme = &state.theme;
    let resolved = theme.resolved_styles();
    let colors = theme.colors.to_ratatui_colors();

    let size = frame.area();

    // Layout: header, messages (top), completions (optional), goal bar, input, status bar (bottom)
    let header_height = 1;
    let input_height = 3;
    let completion_height = if state.completions.is_empty() {
        0
    } else {
        state.completions.len() as u16 + 2
    };
    let goal_height = 1;
    let status_height = 1;

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),     // header (reasonix-style)
            Constraint::Min(5),                    // messages
            Constraint::Length(completion_height), // completions
            Constraint::Length(goal_height),       // goal bar
            Constraint::Length(input_height),      // input
            Constraint::Length(status_height),     // status
        ])
        .split(size);

    let header_area = vertical[0];
    let messages_area = vertical[1];
    let completions_area = vertical[2];
    let goal_area = vertical[3];
    let input_area = vertical[4];
    let status_area = vertical[5];

    // --- Header: ◆ hermes <version> · <model> · <cwd> (reasonix-style) ---
    frame.render_widget(
        Paragraph::new(crate::tui_visual::header_line(
            env!("CARGO_PKG_VERSION"),
            &app.current_model,
            &crate::tui_visual::short_cwd(),
            &colors,
        )),
        header_area,
    );

    // --- Render message history ---
    render_messages(frame, app, state, messages_area, &resolved, &colors);

    // --- Render completions ---
    if !state.completions.is_empty() {
        render_completions(
            frame,
            &state.completions,
            state.completion_index,
            completions_area,
        );
    }

    // --- Render goal bar (one line above the input) ---
    render_goal_bar(frame, state, goal_area, &colors);

    // --- Render input area ---
    render_input(frame, state, input_area, &colors);

    // --- Render status bar ---
    render_status(frame, app, state, status_area, &colors);

    // --- Overlays (topmost last) ---
    if state.show_help {
        let lines = help_lines(theme);
        render_popup(frame, size, " Help — F1/Esc close ", 70, lines);
    }
    if state.show_agents {
        let inner_w = popup_inner_width(size, 80);
        let lines = agents_panel_lines(&state.agents, inner_w);
        render_popup(
            frame,
            size,
            " Background processes — F4/Esc close ",
            80,
            lines,
        );
    }
    if state.show_sessions {
        let inner_w = popup_inner_width(size, 80);
        let lines = session_overlay_lines(state, &app.session_id, &colors, inner_w);
        render_popup(frame, size, " Switch session ", 80, lines);
    }
}

/// Inner text width of a popup rendered at `percent_x` of `area` (minus borders).
fn popup_inner_width(area: Rect, percent_x: u16) -> u16 {
    (area.width * percent_x / 100)
        .max(24)
        .min(area.width)
        .saturating_sub(2)
}

/// Render a centered popup box with a border, clearing the area behind it.
fn render_popup(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    percent_x: u16,
    lines: Vec<Line<'static>>,
) {
    let height = (lines.len() as u16 + 4).min(area.height.max(1));
    let rect = centered_rect(percent_x, height, area);
    frame.render_widget(Clear, rect);
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        inner,
    );
}

/// Rough wrapped-line count for a block of [`Line`]s at a given terminal width (Paragraph wraps).
fn approx_wrapped_line_count(lines: &[Line], text_width: u16) -> usize {
    let w = text_width.max(1) as usize;
    lines
        .iter()
        .map(|line| {
            let lw = line.width().max(1);
            lw.saturating_add(w - 1) / w
        })
        .sum()
}

/// Render the message history area.
fn render_messages(
    frame: &mut Frame,
    app: &App,
    state: &TuiState,
    area: Rect,
    styles: &crate::theme::ResolvedStyles,
    colors: &crate::theme::RatatuiColors,
) {
    let mut lines: Vec<Line> = Vec::new();

    // Splash screen on empty session (reasonix-style): logo + caduceus art,
    // only when the terminal is wide enough for the full banner.
    if app.messages.is_empty() && area.width >= crate::tui_visual::WIDE_MIN {
        lines.extend(crate::tui_visual::splash_lines(colors));
        lines.push(Line::from(""));
    }

    // Pair assistant tool calls with their Tool results so completed calls
    // render as one ✓ line; paired Tool messages are consumed into it.
    let pairing = pair_tool_calls(&app.messages);

    for (i, msg) in app.messages.iter().enumerate() {
        if pairing.consumed.contains(&i) {
            continue;
        }
        match msg.role {
            hermes_core::MessageRole::User => {
                if let Some(ref content) = msg.content {
                    for line in content.lines() {
                        lines.push(Line::from(Span::styled(
                            format!("You: {}", line),
                            styles.user_input,
                        )));
                    }
                }
            }
            hermes_core::MessageRole::Assistant => {
                if let Some(ref content) = msg.content {
                    // Assistant messages render as markdown (reasonix-style);
                    // the "Assistant: " label prefixes the first rendered line.
                    let label = Span::styled("Assistant: ", styles.assistant_response);
                    let md_lines = crate::tui_visual::markdown_lines(content, colors);
                    for (i, md_line) in md_lines.into_iter().enumerate() {
                        if i == 0 {
                            let mut spans = vec![label.clone()];
                            spans.extend(md_line.spans);
                            lines.push(Line::from(spans));
                        } else {
                            lines.push(md_line);
                        }
                    }
                }
                // Completed tool calls: ✓ <name> done [+a −r]: <preview>
                if let Some(paired_calls) = pairing.paired.get(&i) {
                    for p in paired_calls {
                        lines.push(crate::tui_visual::tool_done_line_full(
                            &p.name,
                            p.diff.as_ref().map(|ds| (ds.added, ds.removed)),
                            &p.preview,
                            colors,
                        ));
                    }
                }
                // Calls still awaiting a result: ● name (reasonix-style running indicator).
                if let Some(pending_calls) = pairing.pending.get(&i) {
                    for name in pending_calls {
                        lines.push(crate::tui_visual::tool_running_line(name, "", colors));
                    }
                }
            }
            hermes_core::MessageRole::Tool => {
                if let Some(ref content) = msg.content {
                    let preview: String = content.chars().take(200).collect();
                    lines.push(Line::from(Span::styled(
                        format!("  Tool result: {}", preview),
                        styles.tool_result,
                    )));
                }
            }
            hermes_core::MessageRole::System => {
                if let Some(ref content) = msg.content {
                    // AgentLoop injects a large system prompt (tools/skills). It is required for the
                    // model but is not a "chat transcript" line — dumping it makes the TUI look broken.
                    const MAX_SYSTEM_LINES: usize = 6;
                    const MAX_SYSTEM_CHARS: usize = 400;
                    let line_count = content.lines().count();
                    if content.len() > MAX_SYSTEM_CHARS || line_count > MAX_SYSTEM_LINES {
                        lines.push(Line::from(Span::styled(
                            format!(
                                "[System] <{} lines (~{} chars) hidden — internal prompt, not chat>",
                                line_count,
                                content.len()
                            ),
                            styles.system_message,
                        )));
                    } else {
                        for line in content.lines() {
                            lines.push(Line::from(Span::styled(
                                format!("[System] {}", line),
                                styles.system_message,
                            )));
                        }
                    }
                }
            }
        }
    }

    // Streaming buffer (partial assistant response)
    if !state.stream_buffer.is_empty() {
        for line in state.stream_buffer.lines() {
            lines.push(Line::from(Span::styled(
                format!("Assistant: {}", line),
                styles.assistant_response,
            )));
        }
    }

    // Thinking indicator (reasonix-style braille spinner) while the agent works.
    if state.processing && state.stream_buffer.is_empty() {
        let spin = crate::tui_visual::spinner_frame(state.spinner_frame);
        lines.push(Line::from(Span::styled(
            format!("{spin} thinking…"),
            Style::default()
                .fg(colors.secondary)
                .add_modifier(Modifier::ITALIC),
        )));
    }

    let inner_width = area.width.max(1);
    let total_lines = approx_wrapped_line_count(&lines, inner_width);
    let visible = area.height as usize;
    let scroll_y = total_lines.saturating_sub(visible).min(u16::MAX as usize) as u16;

    let paragraph = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::NONE))
        .wrap(Wrap { trim: false });

    // Ratatui Paragraph::scroll is (y, x); y skips wrapped lines from the top. Pin to bottom so
    // new replies stay in view (line_count on Paragraph is unstable/private in ratatui 0.29).
    frame.render_widget(paragraph.scroll((scroll_y, 0)), area);
}

/// Render the auto-completion suggestions.
fn render_completions(
    frame: &mut Frame,
    completions: &[String],
    selected: Option<usize>,
    area: Rect,
) {
    let items: Vec<Line> = completions
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let style = if selected == Some(i) {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Line::from(Span::styled(cmd.clone(), style))
        })
        .collect();

    let paragraph =
        Paragraph::new(Text::from(items)).block(Block::default().borders(Borders::NONE));
    frame.render_widget(paragraph, area);
}

/// Render the one-line goal bar above the input:
/// `◈ turn N · [bg M] · goal: …` (truncated to the available width).
fn render_goal_bar(
    frame: &mut Frame,
    state: &TuiState,
    area: Rect,
    colors: &crate::theme::RatatuiColors,
) {
    let mut text = goal_bar_text(state);
    let max = area.width as usize;
    if text.chars().count() > max {
        text = format!(
            "{}…",
            text.chars().take(max.saturating_sub(1)).collect::<String>()
        );
    }
    let line = Line::from(vec![
        Span::styled(" ", Style::default()),
        Span::styled(text, Style::default().fg(colors.secondary)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// Render the input area (supports multi-line display with wrapping).
fn render_input(
    frame: &mut Frame,
    state: &TuiState,
    area: Rect,
    colors: &crate::theme::RatatuiColors,
) {
    let mode_indicator = match state.mode {
        InputMode::Normal => " NORMAL ",
        InputMode::Insert => " INSERT ",
        InputMode::Command => " CMD ",
    };

    let mode_style = match state.mode {
        InputMode::Normal => Style::default().fg(Color::White).bg(Color::DarkGray),
        InputMode::Insert => Style::default().fg(Color::Black).bg(colors.success),
        InputMode::Command => Style::default().fg(Color::Black).bg(colors.accent),
    };

    let input_text = if state.input.is_empty() && state.mode == InputMode::Insert {
        if state.history_search_active {
            format!("(reverse-i-search)`{}': ", state.history_search_query)
        } else {
            "Type a message (Enter=send, Shift+Enter=newline)...".to_string()
        }
    } else if state.history_search_active {
        format!(
            "(reverse-i-search)`{}': {}",
            state.history_search_query, state.input
        )
    } else {
        state.input.clone()
    };

    // For multi-line, show line count indicator
    let line_count = state.input.matches('\n').count() + 1;
    let line_indicator = if line_count > 1 {
        format!(" L{}", line_count)
    } else {
        String::new()
    };

    let paragraph = Paragraph::new(Text::from(vec![Line::from(vec![
        Span::styled(mode_indicator, mode_style),
        Span::styled(line_indicator, Style::default().fg(Color::DarkGray)),
        // Reasonix-style input prompt.
        Span::styled(
            "› ",
            Style::default()
                .fg(colors.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(input_text),
    ])]))
    .block(Block::default().borders(Borders::BOTTOM))
    .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

/// Render the status bar at the bottom of the screen.
fn render_status(
    frame: &mut Frame,
    app: &App,
    state: &TuiState,
    area: Rect,
    colors: &crate::theme::RatatuiColors,
) {
    let processing_indicator = if state.processing {
        format!("{}", state.spinner_char())
    } else {
        "✓".to_string()
    };
    let model = &app.current_model;
    let session = &app.session_id[..8.min(app.session_id.len())];
    let msg_count = app.messages.len();

    let status_text = format!(
        " {} {} │ Model: {} │ Session: {} │ Messages: {} │ {}",
        processing_indicator, state.mode, model, session, msg_count, state.status_message,
    );

    let status_bar = Paragraph::new(Line::from(Span::styled(
        status_text,
        Style::default().fg(colors.foreground).bg(colors.primary),
    )));

    frame.render_widget(status_bar, area);
}

// ---------------------------------------------------------------------------
// Main TUI run loop
// ---------------------------------------------------------------------------

/// Run the interactive TUI with the given App.
///
/// This is the main entry point for the interactive TUI mode.
/// It sets up the terminal, renders frames, and handles events.
pub async fn run(mut app: App) -> Result<(), AgentError> {
    let mut tui = Tui::new().map_err(|e| AgentError::Config(e.to_string()))?;
    let mut state = TuiState::default();
    // Restore the theme persisted by F2 toggles (config `tui.theme`).
    state.set_theme_by_name(restore_theme_name(app.config.tui.theme.as_deref()));
    tui.set_theme(state.theme.clone());
    app.set_stream_handle(Some(StreamHandle::from(tui.event_sender())));

    // Spawn crossterm event reader
    let event_sender = tui.event_sender();
    let _event_task = tokio::spawn(async move {
        loop {
            if crossterm::event::poll(Duration::from_millis(100)).unwrap_or(false) {
                if let Ok(event) = crossterm::event::read() {
                    let msg = match event {
                        CrosstermEvent::Key(key) => Some(Event::Key(key)),
                        CrosstermEvent::Resize(w, h) => Some(Event::Resize(w, h)),
                        _ => None,
                    };
                    if let Some(msg) = msg {
                        let _ = event_sender.send(msg);
                    }
                }
            }
        }
    });

    // Spawn the 150ms ticker: drives the braille spinner while the agent is
    // processing and refreshes the F4 agents panel while it is open.
    let tick_sender = tui.event_sender();
    let _tick_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(150));
        loop {
            interval.tick().await;
            if tick_sender.send(Event::Tick).is_err() {
                break;
            }
        }
    });

    // Main event loop
    while app.running {
        // Render
        tui.terminal
            .draw(|f| {
                render(f, &app, &state);
            })
            .map_err(|e| AgentError::Config(e.to_string()))?;

        // Handle events
        tokio::select! {
            event = tui.events.recv() => {
                match event {
                    Some(Event::Key(key)) => {
                        // Handle Ctrl+C interrupt (Requirement 9.3)
                        if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                            && key.code == crossterm::event::KeyCode::Char('c')
                        {
                            if state.processing {
                                // Interrupt current tool execution
                                state.processing = false;
                                state.stream_buffer.clear();
                                state.status_message = "Interrupted".to_string();
                                tui.event_sender().send(Event::Interrupt).ok();
                            } else {
                                // Exit on second Ctrl+C
                                app.running = false;
                                break;
                            }
                        } else {
                            let should_quit = state.handle_key(key, &mut app);
                            if should_quit {
                                app.running = false;
                                break;
                            }
                            // Keep the Tui theme mirror in sync (F2 toggles state.theme).
                            tui.set_theme(state.theme.clone());

                            // Enter submits the input; Shift+Enter inserts newline.
                            let is_submit = key.code == crossterm::event::KeyCode::Enter
                                && !key
                                    .modifiers
                                    .contains(crossterm::event::KeyModifiers::SHIFT);

                            if is_submit {
                                let input = state.input.clone();
                                state.input.clear();
                                state.cursor_position = 0;

                                if !input.trim().is_empty() {
                                    state.turn_count = state.turn_count.saturating_add(1);
                                    // /goal is TUI-local: intercept it here so it
                                    // never reaches the model; feedback goes to
                                    // the status bar.
                                    if handle_goal_command(&mut state, input.trim()) {
                                        // handled locally
                                    } else {
                                        state.processing = true;
                                        state.status_message = "Processing...".to_string();

                                        // Re-render before processing
                                        tui.terminal.draw(|f| {
                                            render(f, &app, &state);
                                        }).map_err(|e| AgentError::Config(e.to_string()))?;

                                        match app.handle_input(&input).await {
                                            Ok(_) => {
                                                state.processing = false;
                                                state.status_message.clear();
                                            }
                                            Err(e) => {
                                                state.processing = false;
                                                state.status_message = format!("Error: {}", e);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Some(Event::Resize(_, _)) => {
                        // Terminal was resized — next render will adapt
                    }
                    Some(Event::Message(msg)) => {
                        state.status_message = msg;
                    }
                    Some(Event::StreamDelta(delta)) => {
                        state.stream_buffer.push_str(&delta);
                    }
                    Some(Event::StreamChunk(chunk)) => {
                        if let Some(delta) = chunk.delta {
                            if let Some(extra) = delta.extra.as_ref() {
                                if let Some(control) = extra.get("control").and_then(|v| v.as_str()) {
                                    if control == "mute_post_response" {
                                        state.stream_muted = extra
                                            .get("enabled")
                                            .and_then(|v| v.as_bool())
                                            .unwrap_or(false);
                                    } else if control == "stream_break" {
                                        state.stream_needs_break = true;
                                    }
                                }
                            }
                            if let Some(content) = delta.content {
                                if !state.stream_muted {
                                    if state.stream_needs_break {
                                        state.stream_buffer.push_str("\n\n");
                                        state.stream_needs_break = false;
                                    }
                                    state.stream_buffer.push_str(&content);
                                }
                            }
                        }
                    }
                    Some(Event::AgentDone) => {
                        state.processing = false;
                        state.stream_buffer.clear();
                        state.stream_muted = false;
                        state.stream_needs_break = false;
                        state.status_message.clear();
                    }
                    Some(Event::Interrupt) => {
                        state.processing = false;
                        state.stream_buffer.clear();
                        state.stream_muted = false;
                        state.stream_needs_break = false;
                    }
                    Some(Event::Tick) => {
                        // Advance the braille spinner while the agent works.
                        if state.processing {
                            state.tick_spinner();
                        }
                        // Refresh the F4 panel from the shared process
                        // registry (the same registry the exec tool uses).
                        if state.show_agents || state.agents_refresh_pending {
                            state.agents = global_process_manager().list().await;
                            state.agents_refresh_pending = false;
                            state.bg_count = state
                                .agents
                                .iter()
                                .filter(|p| p.status == ProcessStatus::Running)
                                .count();
                        }
                    }
                    None => {
                        // Channel closed
                        break;
                    }
                }
            }
        }
    }

    // Restore terminal
    tui.restore()
        .map_err(|e| AgentError::Config(e.to_string()))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Streaming support (Requirement 9.5)
// ---------------------------------------------------------------------------

/// A handle for sending streaming deltas to the TUI.
///
/// Clone this and pass it to the agent loop's streaming callback.
/// The TUI will accumulate deltas and display them in real time.
#[derive(Clone)]
pub struct StreamHandle {
    sender: mpsc::UnboundedSender<Event>,
}

impl StreamHandle {
    /// Send a streaming text delta to the TUI.
    pub fn send_delta(&self, text: &str) {
        let _ = self.sender.send(Event::StreamDelta(text.to_string()));
    }

    /// Send a full streaming chunk to the TUI event loop.
    pub fn send_chunk(&self, chunk: StreamChunk) {
        let _ = self.sender.send(Event::StreamChunk(chunk));
    }

    /// Signal that the agent has finished.
    pub fn send_done(&self) {
        let _ = self.sender.send(Event::AgentDone);
    }
}

impl From<mpsc::UnboundedSender<Event>> for StreamHandle {
    fn from(sender: mpsc::UnboundedSender<Event>) -> Self {
        Self { sender }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_input_mode_display() {
        assert_eq!(InputMode::Normal.to_string(), "NORMAL");
        assert_eq!(InputMode::Insert.to_string(), "INSERT");
        assert_eq!(InputMode::Command.to_string(), "COMMAND");
    }

    #[test]
    fn test_tui_state_default() {
        let state = TuiState::default();
        assert_eq!(state.mode, InputMode::Insert);
        assert!(state.input.is_empty());
        assert_eq!(state.cursor_position, 0);
        assert!(state.completions.is_empty());
        assert!(!state.processing);
        assert!(state.selection_anchor.is_none());
        assert!(!state.history_search_active);
    }

    #[test]
    fn test_multiline_cursor_helpers() {
        let mut state = TuiState::default();
        state.input = "line1\nline2\nline3".to_string();
        // Cursor at start of line2 (byte 6)
        state.cursor_position = 6;
        assert_eq!(state.line_start(), 6);
        assert_eq!(state.line_end(), 11);
        assert_eq!(state.current_column(), 0);

        // Move up should go to line1
        let up = state.cursor_up();
        assert_eq!(up, Some(0));

        // Cursor at middle of line2
        state.cursor_position = 8; // "li" of line2
        assert_eq!(state.current_column(), 2);
        let down = state.cursor_down();
        assert_eq!(down, Some(14)); // "li" of line3
    }

    #[test]
    fn test_spinner_char() {
        let mut state = TuiState::default();
        let c1 = state.spinner_char();
        state.tick_spinner();
        let c2 = state.spinner_char();
        assert_ne!(c1, c2);
    }

    #[test]
    fn test_tool_output_section() {
        let section = ToolOutputSection::new(
            "test_tool".to_string(),
            "line1\nline2\nline3\nline4\nline5".to_string(),
        );
        assert!(!section.is_expanded);
        let display = section.display_text();
        assert!(display.contains("line1"));
        assert!(display.contains("more lines"));
    }

    #[test]
    fn test_tui_state_completions_update() {
        let mut state = TuiState::default();
        state.input = "/mod".to_string();
        state.update_completions();
        assert!(state.completions.contains(&"/model".to_string()));

        state.input = "/st".to_string();
        state.update_completions();
        assert!(state.completions.contains(&"/steer".to_string()));
    }

    #[test]
    fn test_event_debug() {
        let event = Event::Message("hello".to_string());
        let debug_str = format!("{:?}", event);
        assert!(debug_str.contains("hello"));
    }

    #[test]
    fn test_stream_handle() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let handle: StreamHandle = tx.into();
        handle.send_delta("test delta");
        handle.send_done();
    }

    // -----------------------------------------------------------------------
    // Wave 6: theme mapping/cycle/restore
    // -----------------------------------------------------------------------

    #[test]
    fn test_theme_for_name_mapping() {
        assert_eq!(theme_for_name("dark").name, "dark");
        assert_eq!(theme_for_name("light").name, "light");
        // Unknown names fall back to dark.
        assert_eq!(theme_for_name("neon").name, "dark");
        assert_eq!(theme_for_name("").name, "dark");
    }

    #[test]
    fn test_cycle_theme_toggles_dark_light() {
        let mut state = TuiState::default();
        assert_eq!(state.theme.name, "dark");
        assert_eq!(state.cycle_theme(), "light");
        assert_eq!(state.theme.name, "light");
        assert_eq!(state.cycle_theme(), "dark");
        assert_eq!(state.theme.name, "dark");
    }

    #[test]
    fn test_restore_theme_name() {
        assert_eq!(restore_theme_name(Some("light")), "light");
        assert_eq!(restore_theme_name(Some("dark")), "dark");
        assert_eq!(restore_theme_name(None), "dark");
        assert_eq!(restore_theme_name(Some("bogus")), "dark");
        let mut state = TuiState::default();
        state.set_theme_by_name(restore_theme_name(Some("light")));
        assert_eq!(state.theme.name, "light");
        state.set_theme_by_name(restore_theme_name(None));
        assert_eq!(state.theme.name, "dark");
    }

    #[test]
    fn test_persist_theme_patches_only_theme_field() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.yaml");
        // A raw config with env-style values elsewhere: only tui.theme may change.
        std::fs::write(
            &cfg_path,
            "model: test-model\ntui:\n  theme: dark\nllm_providers:\n  openai:\n    api_key: ${OPENAI_API_KEY}\n",
        )
        .unwrap();
        persist_theme_to_config("light", Some(dir.path())).unwrap();
        let back = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(back.contains("theme: light"), "{back}");
        assert!(back.contains("model: test-model"), "{back}");
        assert!(
            back.contains("${OPENAI_API_KEY}"),
            "other fields must not be expanded: {back}"
        );
        // And the patched file loads back with the new theme.
        let loaded = hermes_config::load_user_config_file(&cfg_path).unwrap();
        assert_eq!(loaded.tui.theme.as_deref(), Some("light"));
    }

    // -----------------------------------------------------------------------
    // Wave 6: diff-stat
    // -----------------------------------------------------------------------

    #[test]
    fn test_diff_stat_counts_hunk_lines() {
        let text = "--- a/file.txt\n+++ b/file.txt\n@@ -1,3 +1,4 @@\n line1\n-removed1\n-removed2\n+added1\n+added2\n+added3\n";
        assert_eq!(
            diff_stat_from_text(text),
            Some(DiffStat {
                added: 3,
                removed: 2
            })
        );
    }

    #[test]
    fn test_diff_stat_excludes_file_headers() {
        // +++/--- headers (even mid-text) never count; only hunk lines do.
        let text =
            "+++ b/f\n--- a/f\n@@ -1 +1 @@\n-old\n+new\n--- a/g\n+++ b/g\n@@ -1 +1 @@\n-x\n+y\n";
        assert_eq!(
            diff_stat_from_text(text),
            Some(DiffStat {
                added: 2,
                removed: 2
            })
        );
    }

    #[test]
    fn test_diff_stat_none_without_diff() {
        assert_eq!(diff_stat_from_text("tool output\nno diff here"), None);
        assert_eq!(diff_stat_from_text(""), None);
        // +/- lines outside any hunk do not count.
        assert_eq!(diff_stat_from_text("+ not a diff\n- neither"), None);
    }

    // -----------------------------------------------------------------------
    // Wave 6: /goal
    // -----------------------------------------------------------------------

    #[test]
    fn test_goal_command_set_show_clear() {
        let mut state = TuiState::default();
        // Not goal commands.
        assert!(!handle_goal_command(&mut state, "/goals"));
        assert!(!handle_goal_command(&mut state, "/quit"));
        assert!(!handle_goal_command(&mut state, "goal without slash"));
        assert!(state.goal.is_none());

        assert!(handle_goal_command(
            &mut state,
            "/goal write the weekly report"
        ));
        assert_eq!(state.goal.as_deref(), Some("write the weekly report"));
        assert!(state.status_message.contains("write the weekly report"));

        // Show keeps the goal and reports it via the status bar.
        assert!(handle_goal_command(&mut state, "/goal"));
        assert_eq!(state.goal.as_deref(), Some("write the weekly report"));
        assert!(state.status_message.contains("write the weekly report"));

        // Clear.
        assert!(handle_goal_command(&mut state, "/goal clear"));
        assert!(state.goal.is_none());
        assert!(state.status_message.contains("clear"));
    }

    #[test]
    fn test_goal_command_empty_and_dash() {
        let mut state = TuiState::default();
        // "/goal " with nothing after it shows the current goal (placeholder),
        // it must not set an empty goal.
        assert!(handle_goal_command(&mut state, "/goal "));
        assert!(state.goal.is_none());
        assert!(state.status_message.contains('—'));
        // "-" is an alias for clear.
        state.goal = Some("x".to_string());
        assert!(handle_goal_command(&mut state, "/goal -"));
        assert!(state.goal.is_none());
    }

    // -----------------------------------------------------------------------
    // Wave 6: goal bar
    // -----------------------------------------------------------------------

    #[test]
    fn test_goal_bar_text() {
        let mut state = TuiState::default();
        let t = goal_bar_text(&state);
        assert!(t.starts_with("◈ turn 0"), "{t}");
        assert!(t.contains("[bg 0]"), "{t}");
        assert!(t.contains("goal: —"), "{t}");

        state.turn_count = 3;
        state.bg_count = 2;
        state.goal = Some("ship it".to_string());
        assert_eq!(goal_bar_text(&state), "◈ turn 3 · [bg 2] · goal: ship it");
    }

    #[test]
    fn test_goal_bar_renders_in_test_backend() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut state = TuiState::default();
        state.turn_count = 1;
        state.goal = Some("demo goal".to_string());
        let backend = TestBackend::new(50, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        let colors = state.theme.colors.to_ratatui_colors();
        terminal
            .draw(|f| render_goal_bar(f, &state, f.area(), &colors))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("◈ turn 1"), "{text}");
        assert!(text.contains("goal: demo goal"), "{text}");
    }

    // -----------------------------------------------------------------------
    // Wave 6: session overlay (F3)
    // -----------------------------------------------------------------------

    fn session_entry(name: &str, title: &str) -> SessionEntry {
        SessionEntry {
            name: name.to_string(),
            title: title.to_string(),
            updated_at: chrono::Utc::now(),
            message_count: 4,
        }
    }

    #[test]
    fn test_session_overlay_open_closes_other_overlays() {
        let mut state = TuiState::default();
        state.show_help = true;
        state.show_agents = true;
        // Loads from the real sessions dir (empty is fine); must not panic
        // and must be TUI-safe (no stdout).
        state.open_session_overlay();
        assert!(state.show_sessions);
        assert!(!state.show_help);
        assert!(!state.show_agents);
        assert_eq!(state.session_idx, 0);
    }

    #[test]
    fn test_session_overlay_nav_wraps() {
        let mut state = TuiState::default();
        state.sessions = vec![
            session_entry("a", "first"),
            session_entry("b", "second"),
            session_entry("c", "third"),
        ];
        state.session_idx = 0;
        state.show_sessions = true;

        state.session_nav(-1);
        assert_eq!(state.session_idx, 2, "nav wraps at the top");
        state.session_nav(1);
        assert_eq!(state.session_idx, 0, "nav wraps at the bottom");
        state.session_nav(1);
        assert_eq!(state.session_idx, 1);
        assert_eq!(state.session_select().unwrap().name, "b");

        state.close_session_overlay();
        assert!(!state.show_sessions);
        assert_eq!(state.session_idx, 0);
    }

    #[test]
    fn test_session_overlay_empty_lines() {
        let state = TuiState::default();
        let colors = state.theme.colors.to_ratatui_colors();
        let text = lines_text(&session_overlay_lines(&state, "x", &colors, 60));
        assert!(text.contains("No saved sessions"), "{text}");
    }

    #[test]
    fn test_session_overlay_marks_active_and_selected() {
        let mut state = TuiState::default();
        state.sessions = vec![
            session_entry("aaa", "hello world"),
            session_entry("bbb", "other"),
        ];
        state.session_idx = 1;
        state.show_sessions = true;
        let colors = state.theme.colors.to_ratatui_colors();
        let text = lines_text(&session_overlay_lines(&state, "aaa", &colors, 60));
        assert!(text.contains("hello world"), "{text}");
        assert!(text.contains('●'), "active session is marked: {text}");
        assert!(text.contains('▸'), "selected row is marked: {text}");
        assert!(text.contains("ago"), "relative time shown: {text}");
    }

    #[test]
    fn test_list_saved_sessions_newest_first_via_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let write_session = |name: &str, first_user: &str, count: usize| {
            let messages: Vec<serde_json::Value> = (0..count)
                .map(|i| {
                    let (role, content) = if i == 0 {
                        ("User", first_user.to_string())
                    } else {
                        ("Assistant", format!("reply {i}"))
                    };
                    serde_json::json!({"role": role, "content": content})
                })
                .collect();
            let data = serde_json::json!({
                "session_info": {"session_id": name, "model": "m", "message_count": count},
                "messages": messages,
            });
            std::fs::write(
                dir.path().join(format!("{name}.json")),
                serde_json::to_string_pretty(&data).unwrap(),
            )
            .unwrap();
        };
        write_session("old", "older session topic", 2);
        // Make "old" clearly older via mtime.
        let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(dir.path().join("old.json"))
            .unwrap()
            .set_modified(old_time)
            .unwrap();
        write_session("new", "newer session topic", 5);
        // A non-json file and a broken json file must be ignored.
        std::fs::write(dir.path().join("notes.txt"), "hello").unwrap();
        std::fs::write(dir.path().join("broken.json"), "{nope").unwrap();

        let list = list_saved_sessions_in(dir.path());
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "new", "newest first");
        assert_eq!(list[1].name, "old");
        assert_eq!(list[0].title, "newer session topic");
        assert_eq!(list[0].message_count, 5);
        assert_eq!(list[1].title, "older session topic");
    }

    #[test]
    fn test_session_switch_keeps_goal() {
        let mut state = TuiState::default();
        state.goal = Some("keep me".to_string());
        state.show_sessions = true;
        state.stream_buffer = "partial".to_string();
        state.apply_session_switch_state("some session", 7);
        // The goal survives the switch; transient state is reset.
        assert_eq!(state.goal.as_deref(), Some("keep me"));
        assert!(!state.show_sessions);
        assert!(state.stream_buffer.is_empty());
        assert!(state.status_message.contains("some session"));
    }

    #[test]
    fn test_switch_refuses_while_processing() {
        let mut state = TuiState::default();
        state.processing = true;
        assert!(!state.check_switch_guard());
        assert!(state.status_message.contains("running turn"));
        state.processing = false;
        assert!(state.check_switch_guard());
    }

    // -----------------------------------------------------------------------
    // Wave 6: Esc priority
    // -----------------------------------------------------------------------

    #[test]
    fn test_esc_priority_chain() {
        let mut state = TuiState::default();
        // Sessions overlay closes first.
        state.show_sessions = true;
        assert!(!state.on_esc());
        assert!(!state.show_sessions);
        // Then help.
        state.show_help = true;
        assert!(!state.on_esc());
        assert!(!state.show_help);
        // Then the agents panel.
        state.show_agents = true;
        assert!(!state.on_esc());
        assert!(!state.show_agents);
        // Then history search.
        state.history_search_active = true;
        assert!(!state.on_esc());
        assert!(!state.history_search_active);
        // Then input is cleared.
        state.input = "hello".to_string();
        assert!(!state.on_esc());
        assert!(state.input.is_empty());
        // Finally Esc exits.
        assert!(state.on_esc());
    }

    // -----------------------------------------------------------------------
    // Wave 6: agents panel (F4)
    // -----------------------------------------------------------------------

    #[test]
    fn test_toggle_agents_panel() {
        let mut state = TuiState::default();
        state.show_help = true;
        state.toggle_agents_panel();
        assert!(state.show_agents);
        assert!(!state.show_help, "opening closes other overlays");
        assert!(state.agents_refresh_pending);
        state.toggle_agents_panel();
        assert!(!state.show_agents);
        assert!(!state.agents_refresh_pending);
    }

    fn test_process_info(name: &str, status: ProcessStatus) -> ProcessInfo {
        ProcessInfo {
            name: name.to_string(),
            pid: 1234,
            command: "sleep 100".to_string(),
            args: vec![],
            working_dir: None,
            status,
            exit_code: None,
            started_at: chrono::Utc::now(),
            stopped_at: None,
        }
    }

    #[test]
    fn test_agents_panel_empty_lines() {
        let text = lines_text(&agents_panel_lines(&[], 60));
        assert!(text.contains("No background processes"), "{text}");
    }

    #[test]
    fn test_agents_panel_status_glyphs() {
        let procs = vec![
            test_process_info("run1", ProcessStatus::Running),
            test_process_info("done1", ProcessStatus::Exited),
            test_process_info("stop1", ProcessStatus::Stopped),
            test_process_info("fail1", ProcessStatus::Failed),
        ];
        let text = lines_text(&agents_panel_lines(&procs, 80));
        assert!(text.contains('●'), "running glyph: {text}");
        assert!(text.contains('✓'), "exited glyph: {text}");
        assert!(text.contains('■'), "stopped glyph: {text}");
        assert!(text.contains('✗'), "failed glyph: {text}");
        assert!(text.contains("run1"), "{text}");
        assert!(text.contains("1234"), "pid shown: {text}");
        assert!(text.contains("sleep 100"), "command shown: {text}");
        assert!(text.contains("running"), "status shown: {text}");
    }

    // -----------------------------------------------------------------------
    // Wave 6: help overlay (F1)
    // -----------------------------------------------------------------------

    #[test]
    fn test_help_mentions_bindings() {
        let theme = Theme::default_theme();
        let text = lines_text(&help_lines(&theme));
        for key in ["F1", "F2", "F3", "F4", "/goal", "/goal clear", "?"] {
            assert!(text.contains(key), "help mentions {key}: {text}");
        }
        assert!(text.contains("dark"), "help shows active theme: {text}");
    }

    // -----------------------------------------------------------------------
    // Wave 6: tool-result pairing + ✓ line
    // -----------------------------------------------------------------------

    fn tool_call(id: &str, name: &str) -> hermes_core::ToolCall {
        hermes_core::ToolCall {
            id: id.to_string(),
            function: hermes_core::FunctionCall {
                name: name.to_string(),
                arguments: "{}".to_string(),
            },
        }
    }

    #[test]
    fn test_pair_tool_calls_by_id_and_positional_fallback() {
        use hermes_core::Message;
        let diff = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-x\n+y\n+z\n";
        let messages = vec![
            Message::assistant_with_tool_calls(
                None,
                vec![tool_call("c1", "edit_file"), tool_call("c2", "exec")],
            ),
            // Matches c2 by id even though it comes first positionally.
            Message::tool_result("c2", "exec output"),
            // No id match for c1 → positional fallback consumes this one.
            Message::tool_result("other", diff),
            // Unrelated tool result stays legacy.
            Message::tool_result("zzz", "stray"),
        ];
        let pairing = pair_tool_calls(&messages);
        assert_eq!(pairing.paired[&0].len(), 2);
        assert_eq!(pairing.paired[&0][0].name, "edit_file");
        assert_eq!(pairing.paired[&0][1].name, "exec");
        // edit_file fell back positionally to the diff result.
        assert_eq!(
            pairing.paired[&0][0].diff,
            Some(DiffStat {
                added: 2,
                removed: 1
            })
        );
        assert!(pairing.consumed.contains(&1));
        assert!(pairing.consumed.contains(&2));
        assert!(!pairing.consumed.contains(&3), "stray result not consumed");
        assert!(pairing.pending.get(&0).is_none_or(|p| p.is_empty()));
    }

    #[test]
    fn test_pair_tool_calls_pending_when_no_result() {
        use hermes_core::Message;
        let messages = vec![Message::assistant_with_tool_calls(
            None,
            vec![tool_call("c9", "web_search")],
        )];
        let pairing = pair_tool_calls(&messages);
        assert!(pairing.paired.is_empty());
        assert_eq!(pairing.pending[&0], vec!["web_search".to_string()]);
    }

    #[test]
    fn test_tool_done_line_renders_diff_stat() {
        use ratatui::backend::TestBackend;
        use ratatui::widgets::Paragraph;
        use ratatui::Terminal;
        let theme = Theme::default_theme();
        let colors = theme.colors.to_ratatui_colors();
        let line = crate::tui_visual::tool_done_line_full(
            "edit_file",
            Some((2, 1)),
            "patched lib.rs",
            &colors,
        );
        let backend = TestBackend::new(60, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                f.render_widget(Paragraph::new(line), f.area());
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains('✓'), "{text}");
        assert!(text.contains("edit_file done"), "{text}");
        assert!(text.contains("[+2 −1]"), "{text}");
        assert!(text.contains("patched lib.rs"), "{text}");
    }

    /// Join the span contents of rendered lines for assertions.
    fn lines_text(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
