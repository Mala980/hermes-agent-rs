//! Terminal UI using ratatui + crossterm (Requirement 9.1-9.6).
//!
//! Implements the interactive terminal interface with:
//! - Message history rendering (9.1, 9.4)
//! - Input area with slash command auto-completion (9.2)
//! - Ctrl+C interrupt for tool execution (9.3)
//! - Streaming output display (9.5)
//! - Status bar with model/session info (9.6)
//! - Theme/skin engine support (9.8)

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::Stdout;
use std::time::{Duration, Instant};

use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use hermes_agent::InterruptController;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use tokio::sync::mpsc;

use hermes_core::{AgentError, Message, StreamChunk};

use crate::app::App;
use crate::commands;
use crate::theme::Theme;
use crate::tui_visual::{self as tv, HermesColors};

/// Maximum redraw rate (~60fps). Streaming deltas arriving faster than this
/// are coalesced: ratatui diffs each draw against the previous buffer, so
/// only changed cells are repainted — no flicker, no full-screen redraws.
const FRAME_BUDGET: Duration = Duration::from_millis(16);
/// Spinner / thinking-indicator animation tick while the agent is working.
const SPINNER_TICK: Duration = Duration::from_millis(100);
/// How many completion rows the floating popup shows before windowing.
const COMPLETION_PAGE: usize = 10;

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
    /// Set when the user presses Enter mid-turn: the text typed into the
    /// composer during the turn is submitted as the next turn as soon as
    /// the current one finishes.
    pub queued_submit: bool,
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
    /// Standing goal shown in the goal bar above the input (original `GoalBar`).
    pub goal: Option<tv::GoalStatus>,
    /// Standing loop job shown in the loop bar above the input (original `LoopBar`).
    pub loop_job: Option<tv::LoopStatus>,
    /// Live subagent rows for the agents panel (original `LiveAgentsPanel`).
    pub agents: Vec<tv::AgentRow>,
    /// Background process rows for the agents panel.
    pub processes: Vec<tv::ProcessRow>,
    /// Whether the agents panel is collapsed to its one-line summary.
    pub agents_collapsed: bool,
    /// Context tokens used (status rule), when known.
    pub ctx_used: Option<u64>,
    /// Context window size (status rule), when known.
    pub ctx_max: Option<u64>,
    /// When the current agent turn started (status rule elapsed clock).
    pub turn_started: Option<Instant>,
    /// When this TUI session started (status rule session clock).
    pub session_started: Instant,
    /// Transcript lines scrolled up from the bottom (0 = pinned to latest).
    pub scroll_offset: usize,
    /// Visible height of the transcript area, refreshed each frame (drives PgUp/PgDn paging).
    pub viewport_height: u16,
    /// Draft input stashed when the user starts browsing history with Up/Down.
    pub history_draft: Option<String>,
    /// Tool calls currently executing, tracked from streaming tool_calls deltas.
    pub live_tools: Vec<LiveToolCall>,
    /// Accumulated `thinking` text streamed via `extra.thinking`.
    pub thinking_buffer: String,
    /// Rendered-markdown cache for settled assistant messages (content hash → lines),
    /// so frames re-rendering the transcript don't re-tokenize every message.
    pub md_cache: HashMap<u64, Vec<Line<'static>>>,
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

impl Default for TuiState {
    fn default() -> Self {
        Self {
            mode: InputMode::Insert,
            input: String::new(),
            cursor_position: 0,
            completions: Vec::new(),
            completion_index: None,
            processing: false,
            queued_submit: false,
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
            goal: None,
            loop_job: None,
            agents: Vec::new(),
            processes: Vec::new(),
            agents_collapsed: false,
            ctx_used: None,
            ctx_max: None,
            turn_started: None,
            session_started: Instant::now(),
            scroll_offset: 0,
            viewport_height: 0,
            history_draft: None,
            live_tools: Vec::new(),
            thinking_buffer: String::new(),
            md_cache: HashMap::new(),
        }
    }
}

impl TuiState {
    /// Handle a key event and return whether the app should quit.
    pub fn handle_key(&mut self, key: KeyEvent, app: &mut App) -> bool {
        match self.mode {
            InputMode::Normal => self.handle_normal_key(key, app),
            InputMode::Insert => self.handle_insert_key(key, app),
            InputMode::Command => self.handle_command_key(key, app),
        }
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
            KeyCode::Char('q') | KeyCode::Esc => {
                return true; // quit
            }
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_add(self.page_size());
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(self.page_size());
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
                // Editing discards any stashed history draft.
                self.history_draft = None;
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
                // Editing discards any stashed history draft.
                self.history_draft = None;
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
                // Popup open: navigate suggestions instead of history.
                if self.completion_visible() {
                    self.cycle_completion(-1);
                    return false;
                }
                // In multi-line: move cursor up one line
                if self.input.contains('\n') {
                    if let Some(new_pos) = self.cursor_up() {
                        self.cursor_position = new_pos;
                        return false;
                    }
                }
                // Single-line or at top: browse history, stashing the draft
                // so Down past the newest entry restores what was typed.
                if !app.input_history.is_empty() {
                    if self.history_draft.is_none() {
                        self.history_draft = Some(self.input.clone());
                    }
                    if let Some(prev) = app.history_prev() {
                        self.input = prev.to_string();
                        self.cursor_position = self.input.len();
                    }
                }
                false
            }
            KeyCode::Down => {
                // Popup open: navigate suggestions instead of history.
                if self.completion_visible() {
                    self.cycle_completion(1);
                    return false;
                }
                // In multi-line: move cursor down one line
                if self.input.contains('\n') {
                    if let Some(new_pos) = self.cursor_down() {
                        self.cursor_position = new_pos;
                        return false;
                    }
                }
                // Single-line or at bottom: browse history; past the newest
                // entry, restore the stashed draft.
                if let Some(next) = app.history_next() {
                    self.input = next.to_string();
                    self.cursor_position = self.input.len();
                } else if let Some(draft) = self.history_draft.take() {
                    self.input = draft;
                    self.cursor_position = self.input.len();
                }
                false
            }
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_add(self.page_size());
                false
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(self.page_size());
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
            KeyCode::Esc => {
                if self.history_search_active {
                    self.history_search_active = false;
                    self.history_search_query.clear();
                    return false;
                }
                // Dismiss the completion popup before leaving insert mode.
                if self.completion_visible() {
                    self.completions.clear();
                    self.completion_index = None;
                    return false;
                }
                self.mode = InputMode::Normal;
                false
            }
            _ => false,
        }
    }

    fn handle_command_key(&mut self, key: KeyEvent, _app: &mut App) -> bool {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Enter => {
                // Return to insert mode; the caller submits `input`.
                self.mode = InputMode::Insert;
                self.completions.clear();
                self.completion_index = None;
                false
            }
            KeyCode::Esc => {
                self.mode = InputMode::Insert;
                self.input.clear();
                self.cursor_position = 0;
                self.completions.clear();
                self.completion_index = None;
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
        tv::spinner_frame(self.spinner_frame)
            .chars()
            .next()
            .unwrap_or('⠋')
    }

    /// Advance the spinner frame.
    pub fn tick_spinner(&mut self) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
    }

    /// Whether the floating slash-command popup should be shown.
    pub fn completion_visible(&self) -> bool {
        !self.completions.is_empty() && self.input.starts_with('/')
    }

    /// Move the popup selection by `delta` rows, wrapping around.
    pub fn cycle_completion(&mut self, delta: isize) {
        let n = self.completions.len();
        if n == 0 {
            return;
        }
        let cur = self
            .completion_index
            .unwrap_or(if delta > 0 { n - 1 } else { 0 });
        self.completion_index = Some((cur as isize + delta).rem_euclid(n as isize) as usize);
    }

    /// Transcript page size for PgUp/PgDn (viewport minus one overlap line).
    fn page_size(&self) -> usize {
        self.viewport_height.saturating_sub(1).max(1) as usize
    }

    /// Handle a key pressed while an agent turn is running.
    ///
    /// The turn loop cannot touch `App` (the turn future borrows it), so
    /// this covers only state-local actions: scrolling the transcript and
    /// editing the composer. Text typed mid-turn accumulates in
    /// [`Self::input`] as a queued draft; pressing Enter sets
    /// [`Self::queued_submit`] so the caller submits the draft as the
    /// next turn once the current one finishes.
    fn handle_key_while_processing(&mut self, key: &KeyEvent) -> MidTurnKey {
        use crossterm::event::{KeyCode, KeyModifiers};
        if is_ctrl_c(key) {
            return MidTurnKey::Interrupt;
        }
        let mods = key.modifiers;
        match key.code {
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_add(self.page_size());
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(self.page_size());
            }
            // Enter queues the draft for submission after this turn.
            KeyCode::Enter if !mods.contains(KeyModifiers::SHIFT) => {
                return MidTurnKey::QueuedSubmit;
            }
            // Shift+Enter → newline in the queued draft.
            KeyCode::Enter => {
                self.input.insert(self.cursor_position, '\n');
                self.cursor_position += 1;
                self.selection_anchor = None;
            }
            KeyCode::Char(c)
                if !mods.contains(KeyModifiers::CONTROL) && !mods.contains(KeyModifiers::ALT) =>
            {
                self.input.insert(self.cursor_position, c);
                self.cursor_position += c.len_utf8();
                self.selection_anchor = None;
            }
            // Ctrl+A / Ctrl+E → line start / end, as in the idle composer.
            KeyCode::Char('a') if mods.contains(KeyModifiers::CONTROL) => {
                self.cursor_position = self.line_start();
                self.selection_anchor = None;
            }
            KeyCode::Char('e') if mods.contains(KeyModifiers::CONTROL) => {
                self.cursor_position = self.line_end();
                self.selection_anchor = None;
            }
            KeyCode::Backspace => {
                if self.cursor_position > 0 {
                    let prev = self.input[..self.cursor_position]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.input.drain(prev..self.cursor_position);
                    self.cursor_position = prev;
                    self.selection_anchor = None;
                }
            }
            KeyCode::Delete => {
                if self.cursor_position < self.input.len() {
                    let next = self.input[self.cursor_position..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| self.cursor_position + i)
                        .unwrap_or(self.input.len());
                    self.input.drain(self.cursor_position..next);
                    self.selection_anchor = None;
                }
            }
            KeyCode::Left => {
                if self.cursor_position > 0 {
                    self.cursor_position = self.input[..self.cursor_position]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                self.selection_anchor = None;
            }
            KeyCode::Right => {
                if self.cursor_position < self.input.len() {
                    self.cursor_position = self.input[self.cursor_position..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| self.cursor_position + i)
                        .unwrap_or(self.input.len());
                }
                self.selection_anchor = None;
            }
            KeyCode::Home => {
                self.cursor_position = self.line_start();
                self.selection_anchor = None;
            }
            KeyCode::End => {
                self.cursor_position = self.line_end();
                self.selection_anchor = None;
            }
            _ => return MidTurnKey::Ignored,
        }
        MidTurnKey::Handled
    }
}

/// What a key pressed during an agent turn should make the turn loop do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MidTurnKey {
    /// Ctrl+C — interrupt the running agent.
    Interrupt,
    /// Enter — submit the queued draft once this turn finishes.
    QueuedSubmit,
    /// The key was consumed (scroll or composer edit).
    Handled,
    /// The key has no mid-turn meaning and is dropped.
    Ignored,
}

// ---------------------------------------------------------------------------
// Streaming helpers
// ---------------------------------------------------------------------------

/// A tool call observed in-flight via streaming `tool_calls` deltas.
#[derive(Debug, Clone)]
pub struct LiveToolCall {
    /// Tool name as streamed by the provider.
    pub name: String,
    /// When the call was first seen (for the elapsed readout).
    pub started: Instant,
}

impl LiveToolCall {
    /// Elapsed time since the call started, as `3.2s`.
    pub fn elapsed(&self) -> String {
        tv::fmt_elapsed_dur(self.started.elapsed())
    }
}

/// Record a live tool call unless it's already tracked.
fn upsert_live_tool(tools: &mut Vec<LiveToolCall>, name: &str) {
    if tools.iter().any(|t| t.name == name) {
        return;
    }
    tools.push(LiveToolCall {
        name: name.to_string(),
        started: Instant::now(),
    });
}

/// Borrowed view of what the transcript renderer needs from [`App`].
///
/// The agent turn runs as an in-flight future holding `&mut App`, so while
/// streaming the event loop renders from a snapshot instead.
pub struct TuiView<'a> {
    pub messages: &'a [Message],
    pub current_model: &'a str,
    pub session_id: &'a str,
    pub tool_count: usize,
}

impl<'a> TuiView<'a> {
    pub fn from_app(app: &'a App, tool_count: usize) -> Self {
        Self {
            messages: &app.messages,
            current_model: &app.current_model,
            session_id: &app.session_id,
            tool_count,
        }
    }
}

/// Owned transcript snapshot captured before a turn starts; the agent future
/// holds `&mut App` for the whole turn, so frames render from this.
struct TurnSnapshot {
    messages: Vec<Message>,
    current_model: String,
    session_id: String,
    tool_count: usize,
}

impl TurnSnapshot {
    fn capture(app: &App) -> Self {
        Self {
            messages: app.messages.clone(),
            current_model: app.current_model.clone(),
            session_id: app.session_id.clone(),
            tool_count: app.tool_registry.list_tools().len(),
        }
    }

    fn view(&self) -> TuiView<'_> {
        TuiView {
            messages: &self.messages,
            current_model: &self.current_model,
            session_id: &self.session_id,
            tool_count: self.tool_count,
        }
    }
}

/// Render markdown with a per-content cache: settled transcript messages are
/// parsed once and frozen (like the original's `StreamingMd` settled blocks);
/// only the in-flight tail re-parses each frame. The theme name is part of
/// the key so an F2 theme switch re-renders instead of serving stale colors.
fn cached_markdown_lines(
    state: &mut TuiState,
    theme_name: &str,
    text: &str,
    colors: &crate::theme::RatatuiColors,
) -> Vec<Line<'static>> {
    let mut h = DefaultHasher::new();
    theme_name.hash(&mut h);
    text.hash(&mut h);
    let hash = h.finish();
    if let Some(hit) = state.md_cache.get(&hash) {
        return hit.clone();
    }
    let rendered = tv::markdown_lines(text, colors);
    if state.md_cache.len() > 256 {
        state.md_cache.clear();
    }
    state.md_cache.insert(hash, rendered.clone());
    rendered
}

/// Scroll math for the transcript: returns `(scroll_y, clamped_offset)`.
/// `scroll_y` lines are skipped from the top; offset 0 pins to the latest.
fn compute_scroll(total_lines: usize, visible: usize, scroll_offset: usize) -> (u16, usize) {
    let max_scroll = total_lines.saturating_sub(visible);
    let offset = scroll_offset.min(max_scroll);
    let scroll_y = total_lines
        .saturating_sub(visible)
        .saturating_sub(offset)
        .min(u16::MAX as usize) as u16;
    (scroll_y, offset)
}

/// Visible window into the completion list so the selected row stays on screen.
fn completion_window(total: usize, selected: usize, max_items: usize) -> (usize, usize) {
    let max_items = max_items.max(1);
    let start = selected
        .saturating_sub(max_items - 1)
        .min(total.saturating_sub(max_items));
    let end = (start + max_items).min(total);
    (start, end)
}

/// Terminal (x, y) for the composer cursor, accounting for the prompt prefix,
/// newlines, and soft wrapping.
fn input_cursor_xy(area: Rect, prefix_width: usize, input: &str, cursor_byte: usize) -> (u16, u16) {
    let inner_w = area.width.max(1) as usize;
    let cursor_byte = cursor_byte.min(input.len());
    let before = input.get(..cursor_byte).unwrap_or("");
    let segments: Vec<&str> = before.split('\n').collect();
    let last_idx = segments.len().saturating_sub(1);
    let mut row = 0usize;
    let mut col = 0usize;
    for (i, seg) in segments.iter().enumerate() {
        let w = Line::from(*seg).width() + if i == 0 { prefix_width } else { 0 };
        if i == last_idx {
            row += w / inner_w;
            col = w % inner_w;
        } else {
            row += w / inner_w + 1;
        }
    }
    let x = area
        .x
        .saturating_add(col.min(inner_w.saturating_sub(1)) as u16);
    let y = area
        .y
        .saturating_add(row.min(area.height.saturating_sub(1) as usize) as u16);
    (x, y)
}

/// Status shown when the user queues a follow-up prompt mid-turn.
const QUEUED_STATUS: &str = "Queued — sends when this turn finishes";

/// Whether the key event is Ctrl+C.
fn is_ctrl_c(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
}

/// Whether the key event submits the composer (Enter without Shift).
fn is_submit(key: &KeyEvent) -> bool {
    key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::SHIFT)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the full TUI frame.
///
/// Layout mirrors the original Ink TUI (`appLayout.tsx`): transcript on top,
/// then the live-work dock (goal bar / loop bar / agents panel), the
/// composer input, and finally the status rule + key hints. The slash-command
/// popup floats above the input (modelPicker-style) instead of squeezing the
/// layout.
pub fn render(frame: &mut Frame, view: &TuiView, theme: &Theme, state: &mut TuiState) {
    let resolved = theme.resolved_styles();
    let colors = theme.colors.to_ratatui_colors();
    let hermes = tv::hermes_colors();

    let size = frame.area();
    let cols = size.width.max(1) as usize;

    // Live-work dock rows (goal bar, loop bar, agents panel) sit between the
    // transcript and the input, like the original's composer pane.
    let chrome = chrome_lines(state, cols, &hermes);
    let chrome_height = chrome.len().min(12) as u16;

    // Grow the composer with multi-line input so wrapped rows stay visible.
    let input_height = (3 + state.input.matches('\n').count() as u16).min(8);
    let status_height = 2;

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),                // messages
            Constraint::Length(chrome_height), // goal/loop/agents dock
            Constraint::Length(input_height),  // input
            Constraint::Length(status_height), // status
        ])
        .split(size);

    let messages_area = vertical[0];
    let chrome_area = vertical[1];
    let input_area = vertical[2];
    let status_area = vertical[3];

    // --- Render message history ---
    let rcx = RenderCtx {
        theme_name: &theme.name,
        styles: &resolved,
        colors: &colors,
        hermes: &hermes,
    };
    render_messages(frame, view, &rcx, messages_area, state);

    // --- Render live-work dock (goal / loop / agents) ---
    if chrome_height > 0 {
        let paragraph = Paragraph::new(Text::from(chrome)).wrap(Wrap { trim: false });
        frame.render_widget(paragraph, chrome_area);
    }

    // --- Render input area ---
    render_input(frame, state, input_area, &hermes);

    // --- Render status bar ---
    render_status(frame, view, state, status_area, &colors, &hermes);

    // --- Floating slash-command popup (overlays the transcript) ---
    render_completions_popup(frame, state, input_area, &colors, &hermes);
}

/// Build the live-work dock lines: goal bar, loop bar, agents panel.
fn chrome_lines(state: &TuiState, cols: usize, hermes: &HermesColors) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(goal) = &state.goal {
        lines.push(tv::goal_bar_line(goal, cols, hermes));
    }
    if let Some(job) = &state.loop_job {
        lines.push(tv::loop_bar_line(job, cols, hermes));
    }
    lines.extend(tv::agents_panel_lines(
        &state.agents,
        &state.processes,
        state.agents_collapsed,
        cols,
        hermes,
    ));
    lines
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

/// Borrowed render context: theme pieces needed by the transcript renderer.
struct RenderCtx<'a> {
    theme_name: &'a str,
    styles: &'a crate::theme::ResolvedStyles,
    colors: &'a crate::theme::RatatuiColors,
    hermes: &'a HermesColors,
}

/// Render the message history area.
///
/// Message shapes mirror the original `messageLine.tsx`:
/// - user rows open with a bold label-colored `❯ ` gutter, a `───` separator
///   above later turns, and a blank line around the block;
/// - assistant rows use the border-colored `┊ ` gutter with markdown body;
/// - tool results render as muted `⚡ <preview>` rows;
/// - oversized system messages collapse to a one-line summary.
fn render_messages(
    frame: &mut Frame,
    view: &TuiView,
    ctx: &RenderCtx,
    area: Rect,
    state: &mut TuiState,
) {
    let cols = area.width.max(1) as usize;
    state.viewport_height = area.height;
    let mut lines: Vec<Line> = Vec::new();

    // Intro: brand banner + session panel on a fresh transcript
    // (original `Banner` + `SessionPanel` intro message).
    if view.messages.is_empty() && state.stream_buffer.is_empty() {
        lines.extend(tv::banner_lines(cols, ctx.hermes));
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        let info = tv::SessionPanelInfo {
            version: env!("CARGO_PKG_VERSION"),
            model: view.current_model,
            cwd: &tv::short_cwd(),
            tool_count: view.tool_count,
            skill_count: 0,
        };
        lines.extend(tv::session_panel_lines(&info, cols.min(100), ctx.hermes));
        lines.push(Line::from(""));
    }

    let mut first_user_seen = false;
    for msg in view.messages {
        match msg.role {
            hermes_core::MessageRole::User => {
                if first_user_seen {
                    lines.push(tv::user_turn_separator(ctx.hermes));
                }
                first_user_seen = true;
                lines.push(Line::from(""));
                if let Some(ref content) = msg.content {
                    let mut first = true;
                    for line in content.lines() {
                        let mut spans = if first {
                            tv::user_prompt_spans(ctx.hermes)
                        } else {
                            vec![Span::raw("  ")]
                        };
                        spans.push(Span::styled(line.to_string(), ctx.styles.user_input));
                        lines.push(Line::from(spans));
                        first = false;
                    }
                    if content.is_empty() {
                        lines.push(Line::from(tv::user_prompt_spans(ctx.hermes)));
                    }
                }
                lines.push(Line::from(""));
            }
            hermes_core::MessageRole::Assistant => {
                if let Some(ref content) = msg.content {
                    // Settled messages render from the cache: parsed once,
                    // frozen — only the in-flight tail re-parses per frame.
                    let mut md_lines =
                        cached_markdown_lines(state, ctx.theme_name, content, ctx.colors);
                    if md_lines.is_empty() {
                        md_lines.push(Line::from(""));
                    }
                    for (i, md_line) in md_lines.into_iter().enumerate() {
                        let mut spans = if i == 0 {
                            tv::assistant_gutter_spans(ctx.hermes)
                        } else {
                            vec![Span::raw("  ")]
                        };
                        spans.extend(md_line.spans);
                        lines.push(Line::from(spans));
                    }
                }
                // Show tool calls if present
                if let Some(ref tool_calls) = msg.tool_calls {
                    for tc in tool_calls {
                        lines.push(tv::tool_done_line_full(
                            &tc.function.name,
                            None,
                            "",
                            ctx.colors,
                        ));
                    }
                }
                lines.push(Line::from(""));
            }
            hermes_core::MessageRole::Tool => {
                if let Some(ref content) = msg.content {
                    let preview: String = content.chars().take(200).collect();
                    let mut spans = tv::tool_glyph_spans(ctx.hermes);
                    spans.push(Span::styled(preview, ctx.styles.tool_result));
                    lines.push(tv::truncate_line(Line::from(spans), cols));
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
                        let mut spans = tv::system_glyph_spans(ctx.hermes);
                        spans.push(Span::styled(
                            format!(
                                "<{} lines (~{} chars) hidden — internal prompt, not chat>",
                                line_count,
                                content.len()
                            ),
                            ctx.styles.system_message,
                        ));
                        lines.push(Line::from(spans));
                    } else {
                        let mut first = true;
                        for line in content.lines() {
                            let mut spans = if first {
                                tv::system_glyph_spans(ctx.hermes)
                            } else {
                                vec![Span::raw("  ")]
                            };
                            spans.push(Span::styled(line.to_string(), ctx.styles.system_message));
                            lines.push(Line::from(spans));
                            first = false;
                        }
                    }
                }
            }
        }
    }

    // Live turn: thinking indicator, in-flight tool calls, streaming text.
    // Mirrors the original `StreamingAssistant`/`Thinking`/`ToolTrail` blocks:
    // a spinner + "Thinking…" while no tokens have arrived yet, animated
    // per-tool rows with elapsed time, and the streaming markdown tail with
    // a blinking block cursor.
    if state.processing || !state.stream_buffer.is_empty() || !state.live_tools.is_empty() {
        if state.processing && state.stream_buffer.is_empty() {
            let elapsed = state
                .turn_started
                .map(|t| tv::fmt_elapsed_dur(t.elapsed()))
                .unwrap_or_default();
            lines.push(tv::thinking_line(
                tv::spinner_frame(state.spinner_frame),
                &elapsed,
                ctx.colors,
            ));
            // Dim preview of streamed reasoning (newest last).
            let preview: Vec<&str> = state.thinking_buffer.lines().rev().take(3).collect();
            for tl in preview.into_iter().rev() {
                let t = tl.trim();
                if !t.is_empty() {
                    lines.push(tv::thinking_preview_line(t, ctx.colors));
                }
            }
        }
        for lt in &state.live_tools {
            lines.push(tv::live_tool_line(
                &lt.name,
                tv::spinner_frame(state.spinner_frame),
                &lt.elapsed(),
                ctx.colors,
            ));
        }
        if !state.stream_buffer.is_empty() {
            let mut md_lines = tv::markdown_lines(&state.stream_buffer, ctx.colors);
            // Blinking block cursor on the in-flight tail (original
            // `StreamCursor`, toggled ~every 420ms at the 100ms tick).
            if state.spinner_frame % 8 < 4 {
                if let Some(last) = md_lines.last_mut() {
                    last.spans.push(tv::stream_cursor_span(ctx.colors));
                }
            }
            for (i, md_line) in md_lines.into_iter().enumerate() {
                let mut spans = if i == 0 {
                    tv::assistant_gutter_spans(ctx.hermes)
                } else {
                    vec![Span::raw("  ")]
                };
                spans.extend(md_line.spans);
                lines.push(Line::from(spans));
            }
        }
        lines.push(Line::from(""));
    }

    // `?` quick-help overlay floats above the input (original `HelpHint`).
    if state.input == "?" && state.mode == InputMode::Insert {
        lines.push(Line::from(""));
        lines.extend(tv::help_hint_lines(ctx.hermes));
    }

    let inner_width = area.width.max(1);
    let total_lines = approx_wrapped_line_count(&lines, inner_width);
    let visible = area.height as usize;
    let (scroll_y, clamped) = compute_scroll(total_lines, visible, state.scroll_offset);
    state.scroll_offset = clamped;

    let paragraph = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::NONE))
        .wrap(Wrap { trim: false });

    // Ratatui Paragraph::scroll is (y, x); y skips wrapped lines from the top.
    // Offset 0 pins to the latest reply; PgUp/PgDn move through history.
    frame.render_widget(paragraph.scroll((scroll_y, 0)), area);

    // Scrollback indicator when not pinned to the latest.
    if state.scroll_offset > 0 && area.height > 0 {
        let hint = format!(" ▲ {} · PgDn for latest ", state.scroll_offset);
        let hint_area = Rect {
            x: area.x,
            y: area.y + area.height - 1,
            width: area.width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default()
                    .fg(ctx.colors.secondary)
                    .add_modifier(Modifier::DIM),
            )))
            .alignment(Alignment::Right),
            hint_area,
        );
    }
}

/// Floating slash-command popup (modelPicker-style): a bordered box floating
/// just above the composer, listing matches with their descriptions. The
/// selected row stays visible via windowing; `Tab` accepts, `↑`/`↓` navigate,
/// `Esc` dismisses.
fn render_completions_popup(
    frame: &mut Frame,
    state: &TuiState,
    input_area: Rect,
    colors: &crate::theme::RatatuiColors,
    hermes: &HermesColors,
) {
    if !state.completion_visible() {
        return;
    }
    let total = state.completions.len();
    let selected = state.completion_index.unwrap_or(0).min(total - 1);
    let (start, end) = completion_window(total, selected, COMPLETION_PAGE);

    let mut width: usize = 28;
    for cmd in &state.completions[start..end] {
        let desc = commands::help_for(cmd).unwrap_or("");
        width = width.max(cmd.len() + 2 + desc.len().min(44) + 6);
    }
    let width = (width.min(76) as u16)
        .min(input_area.width.saturating_sub(2))
        .max(20);
    let height = (end - start) as u16 + 2;
    if height < 3 || input_area.y < 2 {
        return;
    }
    let area = Rect {
        x: input_area.x + 1,
        y: input_area.y.saturating_sub(height),
        width,
        height,
    };

    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(colors.secondary))
        .title(Span::styled(
            " commands ",
            Style::default().fg(colors.secondary),
        ))
        .title_bottom(Span::styled(
            " Tab accept · ↑↓ navigate · Esc dismiss ",
            Style::default()
                .fg(hermes.muted)
                .add_modifier(Modifier::DIM),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    for (row, cmd) in state.completions[start..end].iter().enumerate() {
        if row as u16 >= inner.height {
            break;
        }
        let idx = start + row;
        let desc = commands::help_for(cmd).unwrap_or("");
        let is_sel = idx == selected;
        let row_text = tv::truncate_str(&format!("{cmd:<14} {desc}"), inner.width as usize);
        let style = if is_sel {
            Style::default()
                .fg(colors.accent)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(colors.foreground)
        };
        let line = Line::from(vec![
            Span::styled(if is_sel { "▸ " } else { "  " }, style),
            Span::styled(row_text, style),
        ]);
        frame.render_widget(
            Paragraph::new(line),
            Rect {
                x: inner.x,
                y: inner.y + row as u16,
                width: inner.width,
                height: 1,
            },
        );
    }
}

/// Render the input area (supports multi-line display with wrapping).
///
/// The composer prompt is the original's bold `❯` glyph; the empty-input
/// placeholder mirrors the original welcome text. The hardware cursor is
/// placed explicitly (original `textInput.tsx` declares its cursor too) —
/// without this it sits at whatever cell the last draw left behind.
fn render_input(frame: &mut Frame, state: &TuiState, area: Rect, hermes: &HermesColors) {
    let prompt_text = format!("{} ", tv::PROMPT_GLYPH);
    let prefix_width = Span::raw(prompt_text.clone()).width();
    let prompt = Span::styled(
        prompt_text,
        Style::default()
            .fg(hermes.prompt)
            .add_modifier(Modifier::BOLD),
    );

    let (input_text, input_style) = if state.input.is_empty() && state.mode == InputMode::Insert {
        let text = if state.history_search_active {
            format!("(reverse-i-search)`{}': ", state.history_search_query)
        } else {
            // Original brand.welcome.
            "Type your message or /help for commands.".to_string()
        };
        (
            text,
            Style::default()
                .fg(hermes.muted)
                .add_modifier(Modifier::DIM),
        )
    } else if state.history_search_active {
        (
            format!(
                "(reverse-i-search)`{}': {}",
                state.history_search_query, state.input
            ),
            Style::default().fg(hermes.text),
        )
    } else {
        (state.input.clone(), Style::default().fg(hermes.text))
    };

    // For multi-line, show line count indicator
    let line_count = state.input.matches('\n').count() + 1;
    let line_indicator = if line_count > 1 {
        format!(" L{}", line_count)
    } else {
        String::new()
    };

    let paragraph = Paragraph::new(Text::from(vec![Line::from(vec![
        prompt,
        Span::styled(input_text, input_style),
        Span::styled(line_indicator, Style::default().fg(hermes.muted)),
    ])]))
    .block(Block::default().borders(Borders::NONE))
    .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);

    // Place the cursor: the reverse-i-search prefix shifts it right.
    let search_prefix_width = if state.history_search_active {
        Line::from(format!(
            "(reverse-i-search)`{}': ",
            state.history_search_query
        ))
        .width()
    } else {
        0
    };
    let (cx, cy) = input_cursor_xy(
        area,
        prefix_width + search_prefix_width,
        &state.input,
        state.cursor_position,
    );
    frame.set_cursor_position((cx, cy));
}

/// Render the status bar at the bottom of the screen.
///
/// Two rows: the original-style status rule
/// (`─ <spinner> <verb>… · <elapsed> │ <model> │ <ctx> │ [<bar>] <pct>% │ <session> │ <cwd>`)
/// plus the key-hints line (and any transient status message).
fn render_status(
    frame: &mut Frame,
    view: &TuiView,
    state: &TuiState,
    area: Rect,
    _colors: &crate::theme::RatatuiColors,
    hermes: &HermesColors,
) {
    let cols = area.width.max(1) as usize;
    let info = tv::StatusInfo {
        busy: state.processing,
        tick: state.spinner_frame,
        elapsed_secs: state.turn_started.map(|t| t.elapsed().as_secs()),
        model: view.current_model,
        ctx_used: state.ctx_used,
        ctx_max: state.ctx_max,
        session_secs: Some(state.session_started.elapsed().as_secs()),
        cwd: &tv::short_cwd(),
    };
    let rule = tv::status_rule_line(&info, cols, hermes);

    let mut hints = tv::key_hints_line(hermes);
    if state.mode != InputMode::Insert {
        hints
            .spans
            .push(Span::styled(" │ ", Style::default().fg(hermes.muted)));
        hints.spans.push(Span::styled(
            state.mode.to_string(),
            Style::default()
                .fg(hermes.warn)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if !state.status_message.is_empty() {
        hints
            .spans
            .push(Span::styled(" │ ", Style::default().fg(hermes.muted)));
        hints.spans.push(Span::styled(
            state.status_message.clone(),
            Style::default().fg(hermes.warn),
        ));
    }
    let hints = tv::truncate_line(hints, cols);

    let paragraph = Paragraph::new(Text::from(vec![rule, hints])).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

// ---------------------------------------------------------------------------
// Main TUI run loop
// ---------------------------------------------------------------------------

/// Run the interactive TUI with the given App.
///
/// This is the main entry point for the interactive TUI mode.
/// It sets up the terminal, renders frames, and handles events.
///
/// Frame pacing: draws are capped at ~60fps and every draw is throttled —
/// streaming deltas arriving faster coalesce into the next frame, and
/// ratatui's diff rendering repaints only changed cells (no flicker).
/// While the agent runs, an inner event pump keeps the loop draining stream
/// chunks so the transcript updates live instead of flushing at the end.
pub async fn run(mut app: App) -> Result<(), AgentError> {
    let mut tui = Tui::new().map_err(|e| AgentError::Config(e.to_string()))?;
    let mut state = TuiState::default();
    app.set_stream_handle(Some(StreamHandle::from(tui.event_sender())));
    // Cloned before any borrow of `app`: lets Ctrl+C reach the agent loop
    // while the turn future holds `&mut app`.
    let interrupt_ctl = app.interrupt_controller.clone();
    // Tool count is stable for the session; don't re-clone the registry per frame.
    let tool_count = app.tool_registry.list_tools().len();

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

    draw(&mut tui, &TuiView::from_app(&app, tool_count), &mut state)?;
    let mut last_draw = Instant::now();

    // Main event loop
    while app.running {
        let animate = state.processing;
        tokio::select! {
            biased;
            event = tui.events.recv() => {
                let Some(event) = event else { break };
                match event {
                    Event::Key(key) => {
                        // Ctrl+C when idle exits; Ctrl+D always exits.
                        if is_ctrl_c(&key) {
                            app.running = false;
                            break;
                        }
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && key.code == KeyCode::Char('d')
                        {
                            app.running = false;
                            break;
                        }
                        let should_quit = state.handle_key(key, &mut app);
                        if should_quit {
                            app.running = false;
                            break;
                        }

                        // Enter submits the input; Shift+Enter inserts newline.
                        if is_submit(&key) {
                            let input = std::mem::take(&mut state.input);
                            state.cursor_position = 0;
                            state.history_draft = None;
                            state.scroll_offset = 0;
                            if !input.trim().is_empty() {
                                run_agent_turn(
                                    &mut tui,
                                    &mut app,
                                    &mut state,
                                    &interrupt_ctl,
                                    input,
                                    &mut last_draw,
                                )
                                .await?;
                                // Drain follow-ups queued with Enter while a
                                // turn was running; text typed without Enter
                                // stays in the composer as a draft.
                                while state.queued_submit {
                                    state.queued_submit = false;
                                    let next = std::mem::take(&mut state.input);
                                    state.cursor_position = 0;
                                    state.scroll_offset = 0;
                                    if next.trim().is_empty() {
                                        break;
                                    }
                                    run_agent_turn(
                                        &mut tui,
                                        &mut app,
                                        &mut state,
                                        &interrupt_ctl,
                                        next,
                                        &mut last_draw,
                                    )
                                    .await?;
                                }
                            }
                        }
                        draw_throttled(&mut tui, &TuiView::from_app(&app, tool_count), &mut state, &mut last_draw)?;
                    }
                    _ => {
                        apply_event(&mut state, event);
                        draw_throttled(&mut tui, &TuiView::from_app(&app, tool_count), &mut state, &mut last_draw)?;
                    }
                }
            }
            // Animate the spinner / thinking indicator / stream cursor while busy.
            _ = tokio::time::sleep(SPINNER_TICK), if animate => {
                state.tick_spinner();
                draw_throttled(&mut tui, &TuiView::from_app(&app, tool_count), &mut state, &mut last_draw)?;
            }
        }
    }

    // Restore terminal
    tui.restore()
        .map_err(|e| AgentError::Config(e.to_string()))?;

    Ok(())
}

/// Draw one frame with the TUI's active theme.
fn draw(tui: &mut Tui, view: &TuiView, state: &mut TuiState) -> Result<(), AgentError> {
    let theme = tui.theme().clone();
    tui.terminal
        .draw(|f| render(f, view, &theme, state))
        .map_err(|e| AgentError::Config(e.to_string()))?;
    Ok(())
}

/// Draw, but no more often than [`FRAME_BUDGET`] — faster event bursts
/// coalesce into the next frame instead of redrawing per delta.
fn draw_throttled(
    tui: &mut Tui,
    view: &TuiView,
    state: &mut TuiState,
    last_draw: &mut Instant,
) -> Result<(), AgentError> {
    if last_draw.elapsed() >= FRAME_BUDGET {
        draw(tui, view, state)?;
        *last_draw = Instant::now();
    }
    Ok(())
}

/// Run one agent turn while pumping TUI events so streaming stays live.
///
/// The agent future holds `&mut app` for the whole turn, so frames render
/// from a pre-turn snapshot. Stream chunks, thinking text, and tool-call
/// deltas are applied as they arrive; Ctrl+C interrupts the agent loop via
/// the shared [`InterruptController`].
async fn run_agent_turn(
    tui: &mut Tui,
    app: &mut App,
    state: &mut TuiState,
    interrupt_ctl: &InterruptController,
    input: String,
    last_draw: &mut Instant,
) -> Result<(), AgentError> {
    state.processing = true;
    state.turn_started = Some(Instant::now());
    state.stream_buffer.clear();
    state.thinking_buffer.clear();
    state.live_tools.clear();
    state.stream_muted = false;
    state.stream_needs_break = false;
    state.status_message = "Processing...".to_string();

    let snap = TurnSnapshot::capture(app);
    draw(tui, &snap.view(), state)?;
    *last_draw = Instant::now();

    let mut fut = Box::pin(app.handle_input(&input));
    let result = loop {
        tokio::select! {
            biased;
            r = &mut fut => break r,
            event = tui.events.recv() => {
                let Some(event) = event else { break Ok(()); };
                match event {
                    Event::Key(key) => {
                        match state.handle_key_while_processing(&key) {
                            MidTurnKey::Interrupt => {
                                // Real interrupt: the agent loop polls this controller.
                                interrupt_ctl.interrupt(None);
                                state.status_message = "Interrupted".to_string();
                            }
                            MidTurnKey::QueuedSubmit => {
                                state.queued_submit = true;
                                state.status_message = QUEUED_STATUS.to_string();
                            }
                            MidTurnKey::Handled | MidTurnKey::Ignored => {}
                        }
                    }
                    ev => apply_event(state, ev),
                }
                draw_throttled(tui, &snap.view(), state, last_draw)?;
            }
            _ = tokio::time::sleep(SPINNER_TICK) => {
                state.tick_spinner();
                draw_throttled(tui, &snap.view(), state, last_draw)?;
            }
        }
    };

    // Defensive reset in case AgentDone never arrived (e.g. early error);
    // the queued AgentDone, if any, re-applies the same state right after.
    state.processing = false;
    state.turn_started = None;
    state.live_tools.clear();
    state.stream_muted = false;
    state.stream_needs_break = false;
    if let Err(e) = result {
        state.stream_buffer.clear();
        state.thinking_buffer.clear();
        state.status_message = format!("Error: {e}");
        tracing::warn!("agent turn failed: {e}");
    } else if state.status_message == "Processing..." || state.status_message == QUEUED_STATUS {
        // Slash commands never send AgentDone; don't leave the sentinel stuck.
        // The queued sentinel is likewise consumed here — the caller acts on
        // `queued_submit` right after this returns.
        state.status_message.clear();
    }
    Ok(())
}

/// Apply a non-key event to the TUI state.
fn apply_event(state: &mut TuiState, event: Event) {
    match event {
        Event::Message(msg) => {
            state.status_message = msg;
        }
        Event::StreamDelta(delta) => {
            if !state.stream_muted {
                state.stream_buffer.push_str(&delta);
            }
        }
        Event::StreamChunk(chunk) => apply_stream_chunk(state, &chunk),
        Event::AgentDone | Event::Interrupt => {
            state.processing = false;
            state.turn_started = None;
            state.stream_buffer.clear();
            state.thinking_buffer.clear();
            state.live_tools.clear();
            state.stream_muted = false;
            state.stream_needs_break = false;
            state.status_message.clear();
        }
        Event::Key(_) | Event::Resize(_, _) => {}
    }
}

/// Fold one stream chunk into the TUI state: text deltas, `thinking` extras,
/// live tool-call tracking, and the agent loop's control messages.
fn apply_stream_chunk(state: &mut TuiState, chunk: &StreamChunk) {
    let Some(delta) = chunk.delta.as_ref() else {
        return;
    };
    if let Some(extra) = delta.extra.as_ref() {
        if let Some(control) = extra.get("control").and_then(|v| v.as_str()) {
            match control {
                "mute_post_response" => {
                    state.stream_muted = extra
                        .get("enabled")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                }
                "stream_break" => {
                    // Tool round finished: settled history shows the calls.
                    state.stream_needs_break = true;
                    state.live_tools.clear();
                }
                _ => {}
            }
        }
        // Provider thinking blocks, forwarded by the agent loop.
        if let Some(thinking) = extra.get("thinking").and_then(|v| v.as_str()) {
            state.thinking_buffer.push_str(thinking);
        }
    }
    // In-flight tool calls for the live `● name (elapsed)` rows.
    if let Some(tool_calls) = delta.tool_calls.as_ref() {
        for tc in tool_calls {
            if let Some(name) = tc.function.as_ref().and_then(|f| f.name.as_ref()) {
                if !name.is_empty() {
                    upsert_live_tool(&mut state.live_tools, name);
                }
            }
        }
    }
    if let Some(content) = delta.content.as_ref() {
        if !state.stream_muted {
            if state.stream_needs_break {
                state.stream_buffer.push_str("\n\n");
                state.stream_needs_break = false;
            }
            state.stream_buffer.push_str(content);
        }
    }
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

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn test_mid_turn_typing_accumulates_queue_draft() {
        let mut state = TuiState::default();
        state.processing = true;
        for c in "next prompt".chars() {
            assert_eq!(
                state.handle_key_while_processing(&key(KeyCode::Char(c))),
                MidTurnKey::Handled
            );
        }
        assert_eq!(state.input, "next prompt");
        assert_eq!(state.cursor_position, "next prompt".len());
        // Backspace edits the draft; the turn itself is untouched.
        assert_eq!(
            state.handle_key_while_processing(&key(KeyCode::Backspace)),
            MidTurnKey::Handled
        );
        assert_eq!(state.input, "next promp");
    }

    #[test]
    fn test_mid_turn_enter_queues_submit_and_ctrl_c_interrupts() {
        let mut state = TuiState::default();
        state.processing = true;
        assert_eq!(
            state.handle_key_while_processing(&key(KeyCode::Enter)),
            MidTurnKey::QueuedSubmit
        );
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            state.handle_key_while_processing(&ctrl_c),
            MidTurnKey::Interrupt
        );
        // Ctrl-modified chars are commands, not draft text.
        let ctrl_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(
            state.handle_key_while_processing(&ctrl_x),
            MidTurnKey::Ignored
        );
        assert!(state.input.is_empty());
    }

    #[test]
    fn test_mid_turn_scroll_keys_adjust_offset() {
        let mut state = TuiState::default();
        state.processing = true;
        state.viewport_height = 20;
        assert_eq!(
            state.handle_key_while_processing(&key(KeyCode::PageUp)),
            MidTurnKey::Handled
        );
        assert_eq!(state.scroll_offset, 19);
        assert_eq!(
            state.handle_key_while_processing(&key(KeyCode::PageDown)),
            MidTurnKey::Handled
        );
        assert_eq!(state.scroll_offset, 0);
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

    #[test]
    fn test_compute_scroll_pins_and_clamps() {
        // Pinned: skip everything above the visible window.
        assert_eq!(compute_scroll(100, 20, 0), (80, 0));
        // Scrolled up 10 lines.
        assert_eq!(compute_scroll(100, 20, 10), (70, 10));
        // Clamped to the oldest line.
        assert_eq!(compute_scroll(100, 20, 999), (0, 80));
        // Short transcript: nothing to scroll.
        assert_eq!(compute_scroll(10, 20, 0), (0, 0));
        assert_eq!(compute_scroll(10, 20, 5), (0, 0));
    }

    #[test]
    fn test_completion_window_keeps_selection_visible() {
        assert_eq!(completion_window(24, 0, 10), (0, 10));
        assert_eq!(completion_window(24, 9, 10), (0, 10));
        assert_eq!(completion_window(24, 10, 10), (1, 11));
        assert_eq!(completion_window(24, 23, 10), (14, 24));
        assert_eq!(completion_window(5, 4, 10), (0, 5));
        assert_eq!(completion_window(0, 0, 10), (0, 0));
    }

    #[test]
    fn test_completion_visible_and_cycle() {
        let mut state = TuiState::default();
        assert!(!state.completion_visible());

        state.input = "/s".to_string();
        state.update_completions();
        assert!(state.completion_visible());

        // Cycling wraps around.
        let n = state.completions.len();
        assert!(n > 1);
        state.cycle_completion(1);
        assert_eq!(state.completion_index, Some(0));
        state.cycle_completion(-1);
        assert_eq!(state.completion_index, Some(n - 1));
        state.cycle_completion(1);
        assert_eq!(state.completion_index, Some(0));

        // Leaving slash input hides the popup.
        state.input = "hello".to_string();
        state.update_completions();
        assert!(!state.completion_visible());
    }

    #[test]
    fn test_upsert_live_tool_dedups() {
        let mut tools = Vec::new();
        upsert_live_tool(&mut tools, "exec");
        upsert_live_tool(&mut tools, "read_file");
        upsert_live_tool(&mut tools, "exec");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "exec");
        assert_eq!(tools[1].name, "read_file");
        assert!(tools[0].elapsed().ends_with('s'));
    }

    fn tool_chunk(name: &str) -> StreamChunk {
        StreamChunk {
            delta: Some(hermes_core::StreamDelta {
                content: None,
                tool_calls: Some(vec![hermes_core::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".to_string()),
                    function: Some(hermes_core::FunctionCallDelta {
                        name: Some(name.to_string()),
                        arguments: None,
                    }),
                }]),
                extra: None,
            }),
            finish_reason: None,
            usage: None,
        }
    }

    #[test]
    fn test_apply_stream_chunk_tracks_live_tools() {
        let mut state = TuiState::default();
        apply_stream_chunk(&mut state, &tool_chunk("exec"));
        assert_eq!(state.live_tools.len(), 1);
        assert_eq!(state.live_tools[0].name, "exec");
        // Duplicate deltas don't duplicate the row.
        apply_stream_chunk(&mut state, &tool_chunk("exec"));
        assert_eq!(state.live_tools.len(), 1);
        apply_stream_chunk(&mut state, &tool_chunk("read_file"));
        assert_eq!(state.live_tools.len(), 2);
    }

    #[test]
    fn test_apply_stream_chunk_stream_break_clears_live_tools() {
        let mut state = TuiState::default();
        apply_stream_chunk(&mut state, &tool_chunk("exec"));
        assert_eq!(state.live_tools.len(), 1);
        let brk = StreamChunk {
            delta: Some(hermes_core::StreamDelta {
                content: None,
                tool_calls: None,
                extra: Some(serde_json::json!({"control": "stream_break"})),
            }),
            finish_reason: None,
            usage: None,
        };
        apply_stream_chunk(&mut state, &brk);
        assert!(state.live_tools.is_empty());
        assert!(state.stream_needs_break);
    }

    #[test]
    fn test_apply_stream_chunk_accumulates_thinking() {
        let mut state = TuiState::default();
        let chunk = StreamChunk {
            delta: Some(hermes_core::StreamDelta {
                content: None,
                tool_calls: None,
                extra: Some(serde_json::json!({"thinking": "let me consider "})),
            }),
            finish_reason: None,
            usage: None,
        };
        apply_stream_chunk(&mut state, &chunk);
        apply_stream_chunk(&mut state, &chunk);
        assert_eq!(state.thinking_buffer, "let me consider let me consider ");
    }

    #[test]
    fn test_input_cursor_xy_basic() {
        let area = Rect::new(0, 0, 80, 24);
        // "› " prefix is 2 wide; cursor at end of "hi".
        assert_eq!(input_cursor_xy(area, 2, "hi", 2), (4, 0));
        assert_eq!(input_cursor_xy(area, 2, "hi", 0), (2, 0));
        // Multi-line: second line has no prompt prefix.
        assert_eq!(input_cursor_xy(area, 2, "hi\nbye", 6), (3, 1));
        // Wrapping: 80-wide area, cursor past the edge wraps.
        let long = "x".repeat(100);
        assert_eq!(input_cursor_xy(area, 2, &long, 100), (22, 1));
    }

    #[test]
    fn test_is_ctrl_c_and_is_submit() {
        use crossterm::event::{KeyCode as KC, KeyEvent as KE, KeyModifiers as KM};
        let ctrl_c = KE::new(KC::Char('c'), KM::CONTROL);
        assert!(is_ctrl_c(&ctrl_c));
        assert!(!is_ctrl_c(&KE::new(KC::Char('c'), KM::empty())));
        assert!(!is_ctrl_c(&KE::new(KC::Char('d'), KM::CONTROL)));

        let enter = KE::new(KC::Enter, KM::empty());
        assert!(is_submit(&enter));
        assert!(!is_submit(&KE::new(KC::Enter, KM::SHIFT)));
    }

    #[test]
    fn test_tui_state_default_has_streaming_fields() {
        let state = TuiState::default();
        assert_eq!(state.scroll_offset, 0);
        assert!(state.history_draft.is_none());
        assert!(state.live_tools.is_empty());
        assert!(state.thinking_buffer.is_empty());
        assert!(state.md_cache.is_empty());
        assert!(!state.completion_visible());
    }
}
