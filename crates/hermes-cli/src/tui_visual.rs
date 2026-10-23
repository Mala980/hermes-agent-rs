//! Visual primitives ala Reasonix untuk TUI: art splash, spinner,
//! header, dan baris aktivitas tool.
//!
//! Art diambil verbatim dari referensi
//! `~/workspace/hermes-rs/crates/hermes-cli/src/tui.rs`; jangan
//! diketik ulang manual (braille harus byte-exact).

use crate::theme::RatatuiColors;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// Banner logo "HERMES AGENT" (6 baris, block ASCII).
pub const LOGO_ART: &[&str] = &[
        "██╗  ██╗███████╗██████╗ ███╗   ███╗███████╗███████╗       █████╗  ██████╗ ███████╗███╗   ██╗████████╗",
        "██║  ██║██╔════╝██╔══██╗████╗ ████║██╔════╝██╔════╝      ██╔══██╗██╔════╝ ██╔════╝████╗  ██║╚══██╔══╝",
        "███████║█████╗  ██████╔╝██╔████╔██║█████╗  ███████╗█████╗███████║██║  ███╗█████╗  ██╔██╗ ██║   ██║   ",
        "██╔══██║██╔══╝  ██╔══██╗██║╚██╔╝██║██╔══╝  ╚════██║╚════╝██╔══██║██║   ██║██╔══╝  ██║╚██╗██║   ██║   ",
        "██║  ██║███████╗██║  ██║██║ ╚═╝ ██║███████╗███████║      ██║  ██║╚██████╔╝███████╗██║ ╚████║   ██║   ",
        "╚═╝  ╚═╝╚══════╝╚═╝  ╚═╝╚═╝     ╚═╝╚══════╝╚══════╝      ╚═╝  ╚═╝ ╚═════╝ ╚══════╝╚═╝  ╚═══╝   ╚═╝   ",
];

/// Logo caduceus braille (15 baris).
pub const CADUCEUS_ART: &[&str] = &[
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣀⡀⠀⣀⣀⠀⢀⣀⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⢀⣠⣴⣾⣿⣿⣇⠸⣿⣿⠇⣸⣿⣿⣷⣦⣄⡀⠀⠀⠀⠀⠀⠀",
    "⠀⢀⣠⣴⣶⠿⠋⣩⡿⣿⡿⠻⣿⡇⢠⡄⢸⣿⠟⢿⣿⢿⣍⠙⠿⣶⣦⣄⡀⠀",
    "⠀⠀⠉⠉⠁⠶⠟⠋⠀⠉⠀⢀⣈⣁⡈⢁⣈⣁⡀⠀⠉⠀⠙⠻⠶⠈⠉⠉⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣴⣿⡿⠛⢁⡈⠛⢿⣿⣦⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠿⣿⣦⣤⣈⠁⢠⣴⣿⠿⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⠉⠻⢿⣿⣦⡉⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠘⢷⣦⣈⠛⠃⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢠⣴⠦⠈⠙⠿⣦⡄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠸⣿⣤⡈⠁⢤⣿⠇⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠉⠛⠷⠄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣀⠑⢶⣄⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣿⠁⢰⡆⠈⡿⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⠳⠈⣡⠞⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
];

/// Lebar minimum terminal agar banner logo penuh (100 kolom + margin) tampil.
pub const WIDE_MIN: u16 = 104;

/// Frame spinner braille untuk indikator "sedang berpikir" (10 frame).
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// 6 baris logo, satu warna per baris, gradasi
/// `[primary, primary, success, success, secondary, secondary]`.
pub fn logo_lines(colors: &RatatuiColors) -> Vec<Line<'static>> {
    let gradient = [
        colors.primary,
        colors.primary,
        colors.success,
        colors.success,
        colors.secondary,
        colors.secondary,
    ];
    LOGO_ART
        .iter()
        .zip(gradient.iter())
        .map(|(text, fg)| Line::from(Span::styled(*text, Style::default().fg(*fg))))
        .collect()
}

/// 15 baris caduceus, gradasi
/// `[secondary, secondary, success, success, primary, primary,
///  success, success, secondary, secondary,
///  foreground, foreground, foreground, foreground, foreground]`.
pub fn caduceus_lines(colors: &RatatuiColors) -> Vec<Line<'static>> {
    let gradient = [
        colors.secondary,
        colors.secondary,
        colors.success,
        colors.success,
        colors.primary,
        colors.primary,
        colors.success,
        colors.success,
        colors.secondary,
        colors.secondary,
        colors.foreground,
        colors.foreground,
        colors.foreground,
        colors.foreground,
        colors.foreground,
    ];
    CADUCEUS_ART
        .iter()
        .zip(gradient.iter())
        .map(|(text, fg)| Line::from(Span::styled(*text, Style::default().fg(*fg))))
        .collect()
}

/// Splash lengkap: logo + baris kosong + caduceus + baris kosong + hint.
pub fn splash_lines(colors: &RatatuiColors) -> Vec<Line<'static>> {
    let mut lines = logo_lines(colors);
    lines.push(Line::from(""));
    lines.extend(caduceus_lines(colors));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Type a message and press Enter \u{b7} /quit to exit \u{b7} /clear to clear history",
        Style::default().fg(colors.secondary),
    )));
    lines
}

/// Frame spinner ke-`tick` (berputar siklis).
pub fn spinner_frame(tick: usize) -> &'static str {
    SPINNER_FRAMES[tick % SPINNER_FRAMES.len()]
}

/// Header satu baris ala Reasonix: `◆ hermes <version> · <model> · <cwd>`.
pub fn header_line(version: &str, model: &str, cwd: &str, colors: &RatatuiColors) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "\u{25c6} ",
            Style::default()
                .fg(colors.primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("hermes {version} \u{b7} {model} \u{b7} {cwd}"),
            Style::default().fg(colors.secondary),
        ),
    ])
}

/// Current working directory with $HOME shortened to `~` (for the header line).
pub fn short_cwd() -> String {
    match std::env::current_dir() {
        Ok(p) => {
            let s = p.display().to_string();
            if let Ok(home) = std::env::var("HOME") {
                if let Some(rest) = s.strip_prefix(&home) {
                    return format!("~{rest}");
                }
            }
            s
        }
        Err(_) => String::new(),
    }
}

/// Baris aktivitas tool yang sedang berjalan: `● <name>[: <summary>]`.
pub fn tool_running_line(name: &str, summary: &str, colors: &RatatuiColors) -> Line<'static> {
    let label = if summary.is_empty() {
        name.to_string()
    } else {
        format!("{name}: {summary}")
    };
    Line::from(vec![
        Span::styled(
            "\u{25cf} ",
            Style::default()
                .fg(colors.warning)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(label, Style::default().fg(colors.warning)),
    ])
}

/// Baris aktivitas tool yang selesai: `✓ <name> done (<durasi>)[: <preview>]`.
pub fn tool_done_line(
    name: &str,
    duration_ms: u64,
    preview: &str,
    colors: &RatatuiColors,
) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            "\u{2713} ",
            Style::default()
                .fg(colors.success)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{name} done ({})", fmt_duration(duration_ms)),
            Style::default().fg(colors.success),
        ),
    ];
    if !preview.is_empty() {
        spans.push(Span::styled(
            format!(": {preview}"),
            Style::default().fg(colors.secondary),
        ));
    }
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// Streaming & thinking indicators (parity with ui-tui `thinking.tsx`)
// ---------------------------------------------------------------------------

/// Format an elapsed [`Duration`] like the original's `fmtElapsed`:
/// one decimal below 10s (`3.2s`), whole seconds at/above (`12s`).
pub fn fmt_elapsed_dur(d: Duration) -> String {
    let secs = d.as_secs_f64().max(0.0);
    if secs < 10.0 {
        format!("{secs:.1}s")
    } else {
        format!("{}s", secs.round() as u64)
    }
}

/// Thinking indicator: `<braille> Thinking… <elapsed>`
/// (original `Thinking`: accent spinner + dim label + suffix).
pub fn thinking_line(frame: &str, elapsed: &str, colors: &RatatuiColors) -> Line<'static> {
    let mut spans = vec![
        Span::styled(format!("{frame} "), Style::default().fg(colors.accent)),
        Span::styled(
            "Thinking…",
            Style::default()
                .fg(colors.secondary)
                .add_modifier(Modifier::DIM),
        ),
    ];
    if !elapsed.is_empty() {
        spans.push(Span::styled(
            format!(" {elapsed}"),
            Style::default()
                .fg(colors.secondary)
                .add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

/// Live tool-call line: `<braille> <name> (<elapsed>)`
/// (original `ToolTrail` active tool: tool-variant spinner + label + elapsed).
pub fn live_tool_line(
    name: &str,
    frame: &str,
    elapsed: &str,
    colors: &RatatuiColors,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{frame} "), Style::default().fg(colors.warning)),
        Span::styled(name.to_string(), Style::default().fg(colors.warning)),
        Span::styled(
            format!(" ({elapsed})"),
            Style::default()
                .fg(colors.secondary)
                .add_modifier(Modifier::DIM),
        ),
    ])
}

/// One dim preview line of streamed reasoning text.
pub fn thinking_preview_line(text: &str, colors: &RatatuiColors) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {text}"),
        Style::default()
            .fg(colors.secondary)
            .add_modifier(Modifier::DIM),
    ))
}

/// Blinking block cursor appended to in-flight streaming text
/// (original `StreamCursor`: `▍`, toggled roughly every 420ms).
pub fn stream_cursor_span(colors: &RatatuiColors) -> Span<'static> {
    Span::styled("▍", Style::default().fg(colors.accent))
}

/// Baris aktivitas tool yang gagal: `✗ <name> failed`.
pub fn tool_failed_line(name: &str, colors: &RatatuiColors) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "\u{2717} ",
            Style::default()
                .fg(colors.error)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{name} failed"), Style::default().fg(colors.error)),
    ])
}

/// Format durasi: `{ms}ms` bila < 1000, else `{:.1}s`.
fn fmt_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

// ---------------------------------------------------------------------------
// Markdown renderer (ported from reference TUI)
// ---------------------------------------------------------------------------

use pulldown_cmark::{CodeBlockKind, Event as MdEvent, Options, Parser, Tag, TagEnd};
use std::time::Duration;

/// Inline formatting state while walking the markdown event stream.
#[derive(Default)]
struct MdInline {
    bold: u32,
    italic: u32,
    code: u32,
    link: u32,
    strike: u32,
    heading: bool,
}

impl MdInline {
    /// Resolve the current inline state to a ratatui style.
    ///
    /// Color mapping (from the reference's own theme struct):
    /// - reference `accent`  → [`RatatuiColors::accent`]
    /// - reference `system`  → [`RatatuiColors::secondary`]
    /// - reference `border`  → [`RatatuiColors::secondary`]
    /// - reference `code_bg` → [`RatatuiColors::background`]
    /// - reference `code_fg` → [`RatatuiColors::foreground`]
    fn style(&self, colors: &RatatuiColors) -> Style {
        let mut s = Style::default();
        if self.code > 0 || self.heading {
            s = s.fg(colors.accent).add_modifier(Modifier::BOLD);
        } else if self.link > 0 {
            s = s.fg(colors.accent).add_modifier(Modifier::UNDERLINED);
        }
        if self.bold > 0 {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.italic > 0 {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            s = s.add_modifier(Modifier::CROSSED_OUT);
        }
        s
    }
}

/// Accumulator for a single markdown table while the event stream walks it.
/// Note: pulldown-cmark 0.13 emits the header cells directly under
/// `TableHead` (no `TableRow` wrapper); body rows do use `TableRow`.
#[derive(Default)]
struct TableState {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
}

/// Render an accumulated table as padded `│` columns with an `├─` header
/// separator (mirrors the original `markdown.tsx` table branch, minus
/// syntax highlighting inside cells).
fn render_table(t: &TableState, colors: &RatatuiColors) -> Vec<Line<'static>> {
    let col_count = t
        .header
        .len()
        .max(t.rows.iter().map(|r| r.len()).max().unwrap_or(0));
    if col_count == 0 {
        return Vec::new();
    }
    // Column widths from content, capped so one wide cell can't blow out
    // the terminal; the Paragraph widget wraps anything still too wide.
    let mut widths = vec![3usize; col_count];
    for (i, h) in t.header.iter().enumerate() {
        widths[i] = widths[i].max(str_width(h).clamp(3, 40));
    }
    for row in &t.rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(str_width(cell).clamp(3, 40));
        }
    }
    let border = Style::default().fg(colors.secondary);
    let head_style = Style::default()
        .fg(colors.accent)
        .add_modifier(Modifier::BOLD);
    let cell_style = Style::default().fg(colors.foreground);
    let row_line = |cells: &[String], style: Style| {
        let mut spans = vec![Span::styled("│ ", border)];
        for (i, w) in widths.iter().enumerate().take(col_count) {
            let text = cells
                .get(i)
                .map(|c| truncate_str(c, *w))
                .unwrap_or_default();
            let pad = w.saturating_sub(str_width(&text));
            spans.push(Span::styled(text, style));
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(" │ ", border));
        }
        Line::from(spans)
    };
    let mut lines = Vec::new();
    if !t.header.is_empty() {
        lines.push(row_line(&t.header, head_style));
        let sep: String = widths
            .iter()
            .map(|w| "─".repeat(*w))
            .collect::<Vec<_>>()
            .join("─┼─");
        lines.push(Line::from(Span::styled(format!("├─{sep}─┤"), border)));
    }
    for row in &t.rows {
        lines.push(row_line(row, cell_style));
    }
    lines
}

/// Convert markdown text into styled ratatui lines.
///
/// Behavior is a verbatim port of the reference TUI renderer:
/// headings are bold accent with gaps around them, fenced code blocks emit a
/// `┌─ {lang}` label line plus a backgrounded body, lists use `• `/`N. `
/// markers (task lists render `☐ `/`☑ `), blockquotes are prefixed with `▎ `
/// per nesting level, links are accent-underlined, tables render as padded
/// `│` columns with a `├─` header separator, soft/hard breaks end the current
/// line, horizontal rules render as 24 `─` chars, and raw HTML renders as
/// italic secondary text.
pub fn markdown_lines(md: &str, colors: &RatatuiColors) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut st = MdInline::default();
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut in_code_block = false;
    let mut code_lang: String;
    let mut code_buf = String::new();
    let mut quote: u32 = 0;
    let mut table: Option<TableState> = None;
    // The item marker is deferred until the first inline content because a
    // `TaskListMarker` (`- [ ] x`) arrives after `Item` starts — pushing the
    // bullet eagerly would put `•` before `☐`.
    let mut pending_bullet: Option<(String, String)> = None;
    let mut pending_task: Option<bool> = None;

    // Push the in-progress line to the result (no doubled empty lines).
    macro_rules! flush {
        () => {
            if !cur.is_empty() {
                let mut spans = std::mem::take(&mut cur);
                if quote > 0 {
                    let mut q = vec![Span::styled(
                        "▎ ".repeat(quote as usize),
                        Style::default().fg(colors.secondary),
                    )];
                    q.append(&mut spans);
                    spans = q;
                }
                lines.push(Line::from(spans));
            }
        };
    }
    // Empty-line separator (avoid two in a row).
    macro_rules! gap {
        () => {
            if lines.last().map(|l| l.width() != 0).unwrap_or(false) {
                lines.push(Line::from(""));
            }
        };
    }
    // Emit the deferred list-item marker (bullet or task checkbox) before
    // the first inline content of the item.
    macro_rules! push_bullet {
        () => {
            if let Some((indent, bullet)) = pending_bullet.take() {
                let marker = match pending_task.take() {
                    Some(true) => "☑ ".to_string(),
                    Some(false) => "☐ ".to_string(),
                    None => bullet,
                };
                cur.push(Span::styled(
                    format!("{indent}{marker}"),
                    Style::default().fg(colors.secondary),
                ));
            }
        };
    }

    let parser = Parser::new_ext(
        md,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS,
    );
    for ev in parser {
        match ev {
            MdEvent::Start(Tag::Heading { .. }) => {
                gap!();
                st.heading = true;
            }
            MdEvent::End(TagEnd::Heading(_)) => {
                flush!();
                gap!();
                st.heading = false;
            }
            MdEvent::Start(Tag::Paragraph) => {}
            MdEvent::End(TagEnd::Paragraph) => {
                flush!();
                gap!();
            }
            MdEvent::Start(Tag::CodeBlock(kind)) => {
                in_code_block = true;
                code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.into_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                let label = if code_lang.is_empty() {
                    "kode".to_string()
                } else {
                    code_lang.clone()
                };
                lines.push(Line::from(Span::styled(
                    format!("┌─ {label}"),
                    Style::default().fg(colors.secondary),
                )));
            }
            MdEvent::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                let bg = Style::default().fg(colors.foreground).bg(colors.background);
                let mut any = false;
                for l in code_buf.lines() {
                    lines.push(Line::from(Span::styled(l.to_string(), bg)));
                    any = true;
                }
                if !any {
                    lines.push(Line::from(Span::styled(String::new(), bg)));
                }
                code_buf.clear();
                gap!();
            }
            MdEvent::Start(Tag::List(n)) => lists.push(n),
            MdEvent::End(TagEnd::List(_)) => {
                lists.pop();
                gap!();
            }
            MdEvent::Start(Tag::Item) => {
                let depth = lists.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let bullet = match lists.last_mut() {
                    Some(slot @ Some(_)) => {
                        let n = (*slot).unwrap_or(1);
                        let s = format!("{n}. ");
                        *slot = Some(n + 1);
                        s
                    }
                    _ => "• ".to_string(),
                };
                pending_bullet = Some((indent, bullet));
                pending_task = None;
            }
            MdEvent::End(TagEnd::Item) => {
                push_bullet!();
                flush!();
            }
            MdEvent::TaskListMarker(checked) => {
                // Inside tables the marker is meaningless; everywhere else it
                // decides the deferred bullet shape.
                if table.is_none() {
                    pending_task = Some(checked);
                }
            }
            MdEvent::Start(Tag::Emphasis) => st.italic += 1,
            MdEvent::End(TagEnd::Emphasis) => st.italic = st.italic.saturating_sub(1),
            MdEvent::Start(Tag::Strong) => st.bold += 1,
            MdEvent::End(TagEnd::Strong) => st.bold = st.bold.saturating_sub(1),
            MdEvent::Start(Tag::Strikethrough) => st.strike += 1,
            MdEvent::End(TagEnd::Strikethrough) => st.strike = st.strike.saturating_sub(1),
            MdEvent::Start(Tag::Link { .. }) => st.link += 1,
            MdEvent::End(TagEnd::Link) => st.link = st.link.saturating_sub(1),
            MdEvent::Start(Tag::BlockQuote(_)) => quote += 1,
            MdEvent::End(TagEnd::BlockQuote(_)) => {
                quote = quote.saturating_sub(1);
                flush!();
                gap!();
            }
            MdEvent::Text(t) => {
                if let Some(tb) = table.as_mut() {
                    tb.current_cell.push_str(&t);
                } else if in_code_block {
                    code_buf.push_str(&t);
                } else {
                    push_bullet!();
                    cur.push(Span::styled(t.into_string(), st.style(colors)));
                }
            }
            MdEvent::Code(t) => {
                if let Some(tb) = table.as_mut() {
                    tb.current_cell.push_str(&t);
                } else {
                    push_bullet!();
                    cur.push(Span::styled(
                        t.into_string(),
                        Style::default()
                            .fg(colors.accent)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
            }
            MdEvent::Html(t) | MdEvent::InlineHtml(t) => {
                push_bullet!();
                cur.push(Span::styled(
                    t.into_string(),
                    Style::default()
                        .fg(colors.secondary)
                        .add_modifier(Modifier::ITALIC),
                ));
            }
            MdEvent::SoftBreak | MdEvent::HardBreak => {
                push_bullet!();
                flush!();
            }
            MdEvent::Start(Tag::Table(_)) => {
                table = Some(TableState::default());
            }
            MdEvent::End(TagEnd::Table) => {
                if let Some(tb) = table.take() {
                    lines.extend(render_table(&tb, colors));
                    gap!();
                }
            }
            MdEvent::Start(Tag::TableHead) => {
                if let Some(tb) = table.as_mut() {
                    tb.current_row.clear();
                }
            }
            MdEvent::End(TagEnd::TableHead) => {
                if let Some(tb) = table.as_mut() {
                    tb.header = std::mem::take(&mut tb.current_row);
                }
            }
            MdEvent::Start(Tag::TableRow) => {
                if let Some(tb) = table.as_mut() {
                    tb.current_row.clear();
                }
            }
            MdEvent::End(TagEnd::TableRow) => {
                if let Some(tb) = table.as_mut() {
                    tb.rows.push(std::mem::take(&mut tb.current_row));
                }
            }
            MdEvent::Start(Tag::TableCell) => {
                if let Some(tb) = table.as_mut() {
                    tb.current_cell.clear();
                }
            }
            MdEvent::End(TagEnd::TableCell) => {
                if let Some(tb) = table.as_mut() {
                    tb.current_row.push(std::mem::take(&mut tb.current_cell));
                }
            }
            MdEvent::Rule => {
                gap!();
                lines.push(Line::from(Span::styled(
                    "─".repeat(24),
                    Style::default().fg(colors.secondary),
                )));
                gap!();
            }
            _ => {}
        }
    }
    flush!();
    while lines.last().map(|l| l.width() == 0).unwrap_or(false) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

/// Render a completed tool call: `✓ <name> done [+a −r]: <preview>`
/// (reasonix-style, theme-colored).
pub fn tool_done_line_full(
    name: &str,
    diff: Option<(u64, u64)>,
    preview: &str,
    colors: &RatatuiColors,
) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            "\u{2713} ",
            Style::default()
                .fg(colors.success)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{name} done"), Style::default().fg(colors.success)),
    ];
    if let Some((added, removed)) = diff {
        spans.push(Span::styled(
            format!(" [+{added} \u{2212}{removed}]"),
            Style::default().fg(colors.success),
        ));
    }
    if !preview.is_empty() {
        spans.push(Span::styled(
            format!(": {preview}"),
            Style::default().fg(colors.secondary),
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn test_colors() -> RatatuiColors {
        RatatuiColors {
            primary: Color::Cyan,
            secondary: Color::DarkGray,
            accent: Color::Blue,
            background: Color::Black,
            foreground: Color::White,
            error: Color::Red,
            warning: Color::Yellow,
            success: Color::Green,
        }
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn logo_lines_count_and_content() {
        let colors = test_colors();
        let lines = logo_lines(&colors);
        assert_eq!(lines.len(), 6, "logo must have 6 lines");
        let text = line_text(&lines[0]);
        assert_eq!(text, LOGO_ART[0], "logo line must match art verbatim");
        assert_eq!(lines[0].spans[0].style.fg, Some(colors.primary));
        assert_eq!(lines[2].spans[0].style.fg, Some(colors.success));
        assert_eq!(lines[5].spans[0].style.fg, Some(colors.secondary));
    }

    #[test]
    fn caduceus_lines_count_and_content() {
        let colors = test_colors();
        let lines = caduceus_lines(&colors);
        assert_eq!(lines.len(), 15, "caduceus must have 15 lines");
        let text = line_text(&lines[0]);
        assert_eq!(
            text, CADUCEUS_ART[0],
            "caduceus line must match art verbatim"
        );
        assert_eq!(lines[0].spans[0].style.fg, Some(colors.secondary));
        assert_eq!(lines[4].spans[0].style.fg, Some(colors.primary));
        assert_eq!(lines[14].spans[0].style.fg, Some(colors.foreground));
    }

    #[test]
    fn splash_lines_composition() {
        let colors = test_colors();
        let lines = splash_lines(&colors);
        // 6 logo + 1 empty + 15 caduceus + 1 empty + 1 hint = 24
        assert_eq!(lines.len(), 24, "splash must be 6+1+15+1+1 lines");
        assert_eq!(line_text(&lines[6]), "");
        assert_eq!(line_text(&lines[22]), "");
        let hint = line_text(&lines[23]);
        assert!(hint.contains("/quit to exit"), "hint text: {hint}");
        assert_eq!(lines[23].spans[0].style.fg, Some(colors.secondary));
    }

    #[test]
    fn spinner_cycles_through_frames() {
        assert_eq!(SPINNER_FRAMES.len(), 10, "spinner must have 10 frames");
        assert_eq!(spinner_frame(0), SPINNER_FRAMES[0]);
        assert_eq!(
            spinner_frame(10),
            SPINNER_FRAMES[0],
            "tick 10 wraps to frame 0"
        );
        assert_eq!(spinner_frame(11), SPINNER_FRAMES[1]);
        for tick in 0..30 {
            assert_eq!(spinner_frame(tick), SPINNER_FRAMES[tick % 10]);
        }
    }

    #[test]
    fn header_line_contains_version_model_cwd() {
        let colors = test_colors();
        let line = header_line("0.2.0", "atmo/deepseek-v4.1-flash", "/home/chewy", &colors);
        let text = line_text(&line);
        assert!(
            text.starts_with("◆ "),
            "header starts with diamond glyph: {text}"
        );
        assert!(text.contains("0.2.0"), "header contains version: {text}");
        assert!(
            text.contains("atmo/deepseek-v4.1-flash"),
            "header contains model: {text}"
        );
        assert!(text.contains("/home/chewy"), "header contains cwd: {text}");
    }

    #[test]
    fn tool_running_line_glyph_and_summary() {
        let colors = test_colors();
        let with_summary = tool_running_line("exec", "ls -la", &colors);
        let text = line_text(&with_summary);
        assert!(text.starts_with("● "), "running glyph ●: {text}");
        assert!(text.contains("exec: ls -la"), "name and summary: {text}");
        assert_eq!(with_summary.spans[0].style.fg, Some(colors.warning));

        let bare = tool_running_line("exec", "", &colors);
        let bare_text = line_text(&bare);
        assert_eq!(
            bare_text, "● exec",
            "empty summary shows name only: {bare_text}"
        );
    }

    #[test]
    fn tool_done_line_glyph_duration_preview() {
        let colors = test_colors();
        let ms_line = tool_done_line("exec", 250, "", &colors);
        let ms_text = line_text(&ms_line);
        assert!(ms_text.starts_with("✓ "), "done glyph ✓: {ms_text}");
        assert!(
            ms_text.contains("exec done (250ms)"),
            "ms duration: {ms_text}"
        );

        let s_line = tool_done_line("exec", 1500, "ok", &colors);
        let s_text = line_text(&s_line);
        assert!(
            s_text.contains("exec done (1.5s)"),
            "second duration: {s_text}"
        );
        assert!(s_text.ends_with(": ok"), "preview suffix: {s_text}");

        assert_eq!(ms_line.spans[0].style.fg, Some(colors.success));
    }

    #[test]
    fn tool_failed_line_glyph() {
        let colors = test_colors();
        let line = tool_failed_line("exec", &colors);
        let text = line_text(&line);
        assert!(text.starts_with("✗ "), "failed glyph ✗: {text}");
        assert!(text.contains("exec failed"), "name + failed: {text}");
        assert_eq!(line.spans[0].style.fg, Some(colors.error));
    }

    #[test]
    fn fmt_duration_formatting() {
        assert_eq!(fmt_duration(0), "0ms");
        assert_eq!(fmt_duration(999), "999ms");
        assert_eq!(fmt_duration(1000), "1.0s");
        assert_eq!(fmt_duration(1500), "1.5s");
        assert_eq!(fmt_duration(61_234), "61.2s");
    }

    fn md_test_colors() -> RatatuiColors {
        RatatuiColors {
            primary: Color::Cyan,
            secondary: Color::Blue,
            accent: Color::Red,
            background: Color::Black,
            foreground: Color::White,
            error: Color::LightRed,
            warning: Color::Yellow,
            success: Color::Green,
        }
    }

    #[test]
    fn heading_renders_bold_accent() {
        let colors = md_test_colors();
        let lines = markdown_lines("# Title", &colors);
        assert!(
            lines.iter().any(|l| l.spans.iter().any(|s| {
                s.content.as_ref() == "Title"
                    && s.style.fg == Some(colors.accent)
                    && s.style.add_modifier.contains(Modifier::BOLD)
            })),
            "expected a bold accent 'Title' span, got {lines:?}"
        );
    }

    #[test]
    fn fenced_code_block_emits_lang_label() {
        let colors = md_test_colors();
        let lines = markdown_lines("```rust\nlet x = 1;\n```", &colors);
        assert!(
            !lines.is_empty(),
            "expected non-empty lines for a fenced code block"
        );
        let label: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(label, "┌─ rust");
        // Body lines use background/foreground colors.
        let body_ok = lines[1..].iter().any(|l| {
            l.spans.iter().any(|s| {
                s.content.as_ref() == "let x = 1;"
                    && s.style.fg == Some(colors.foreground)
                    && s.style.bg == Some(colors.background)
            })
        });
        assert!(body_ok, "expected backgrounded body line, got {lines:?}");
    }

    #[test]
    fn bullet_list_emits_bullet_marker() {
        let colors = md_test_colors();
        let lines = markdown_lines("- item one", &colors);
        assert!(
            lines.iter().any(|l| {
                let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                text.starts_with("• ")
            }),
            "expected a '• ' bullet marker, got {lines:?}"
        );
    }

    #[test]
    fn blockquote_emits_quote_prefix() {
        let colors = md_test_colors();
        let lines = markdown_lines("> quoted text", &colors);
        assert!(
            lines.iter().any(|l| {
                let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                text.starts_with("▎ ")
            }),
            "expected a '▎ ' quote prefix, got {lines:?}"
        );
    }

    #[test]
    fn inline_code_is_bold_accent() {
        let colors = md_test_colors();
        let lines = markdown_lines("use `cargo` now", &colors);
        assert!(
            lines.iter().any(|l| l.spans.iter().any(|s| {
                s.content.as_ref() == "cargo"
                    && s.style.fg == Some(colors.accent)
                    && s.style.add_modifier.contains(Modifier::BOLD)
            })),
            "expected bold accent 'cargo' span, got {lines:?}"
        );
    }

    #[test]
    fn empty_input_yields_single_empty_line() {
        let colors = md_test_colors();
        let lines = markdown_lines("", &colors);
        assert_eq!(lines.len(), 1, "expected exactly one line, got {lines:?}");
        assert_eq!(lines[0].width(), 0, "expected the line to be empty");
    }

    #[test]
    fn tool_done_line_full_renders_diff_and_preview() {
        let colors = test_colors();
        let line = tool_done_line_full("edit_file", Some((2, 1)), "patched lib.rs", &colors);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("\u{2713}"), "must have check glyph");
        assert!(text.contains("edit_file done"), "must have name+done");
        assert!(text.contains("[+2"), "must have added count");
        assert!(text.contains("patched lib.rs"), "must have preview");
        // No diff, no preview
        let line = tool_done_line_full("read_file", None, "", &colors);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("read_file done"));
        assert!(!text.contains("[+"));
    }

    #[test]
    fn table_renders_padded_columns_with_header_separator() {
        let colors = md_test_colors();
        let lines = markdown_lines("| a | bb |\n|---|---|\n| ccc | d |\n", &colors);
        let body: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(body.contains("│ a   │ bb  │"), "header row: {body}");
        assert!(body.contains("├─────┼─────┤"), "separator row: {body}");
        assert!(body.contains("│ ccc │ d   │"), "body row: {body}");
        // Header cells are bold accent.
        assert!(
            lines.iter().any(|l| l.spans.iter().any(|s| {
                s.content.as_ref() == "a"
                    && s.style.fg == Some(colors.accent)
                    && s.style.add_modifier.contains(Modifier::BOLD)
            })),
            "header cell should be bold accent: {lines:?}"
        );
    }

    #[test]
    fn task_list_renders_checkboxes() {
        let colors = md_test_colors();
        let lines = markdown_lines("- [ ] todo\n- [x] done\n- plain\n", &colors);
        let body: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(body.contains("☐ todo"), "unchecked: {body}");
        assert!(body.contains("☑ done"), "checked: {body}");
        assert!(body.contains("• plain"), "plain bullet kept: {body}");
        assert!(!body.contains("• ☐"), "no doubled marker: {body}");
    }

    #[test]
    fn strikethrough_renders_crossed_out() {
        let colors = md_test_colors();
        let lines = markdown_lines("a ~~gone~~ b", &colors);
        assert!(
            lines.iter().any(|l| l.spans.iter().any(|s| {
                s.content.as_ref() == "gone" && s.style.add_modifier.contains(Modifier::CROSSED_OUT)
            })),
            "expected crossed-out 'gone', got {lines:?}"
        );
    }

    #[test]
    fn fmt_elapsed_dur_shapes() {
        assert_eq!(fmt_elapsed_dur(Duration::from_millis(0)), "0.0s");
        assert_eq!(fmt_elapsed_dur(Duration::from_millis(3240)), "3.2s");
        assert_eq!(fmt_elapsed_dur(Duration::from_millis(9999)), "10.0s");
        assert_eq!(fmt_elapsed_dur(Duration::from_secs(12)), "12s");
        assert_eq!(fmt_elapsed_dur(Duration::from_secs(90)), "90s");
    }

    #[test]
    fn thinking_line_shape() {
        let colors = test_colors();
        let line = thinking_line("⠋", "3.2s", &colors);
        let text = line_text(&line);
        assert!(text.starts_with("⠋ "), "spinner frame first: {text}");
        assert!(text.contains("Thinking…"), "label: {text}");
        assert!(text.contains("3.2s"), "elapsed: {text}");
        assert_eq!(line.spans[0].style.fg, Some(colors.accent));

        let bare = thinking_line("⠋", "", &colors);
        assert_eq!(line_text(&bare), "⠋ Thinking…");
    }

    #[test]
    fn live_tool_line_shape() {
        let colors = test_colors();
        let line = live_tool_line("exec", "⠙", "1.5s", &colors);
        let text = line_text(&line);
        assert_eq!(text, "⠙ exec (1.5s)", "shape: {text}");
        assert_eq!(line.spans[0].style.fg, Some(colors.warning));
        assert_eq!(line.spans[1].style.fg, Some(colors.warning));
    }

    #[test]
    fn stream_cursor_span_shape() {
        let colors = test_colors();
        let span = stream_cursor_span(&colors);
        assert_eq!(span.content.as_ref(), "▍");
        assert_eq!(span.style.fg, Some(colors.accent));
    }

    #[test]
    fn thinking_preview_line_is_dim() {
        let colors = test_colors();
        let line = thinking_preview_line("considering options", &colors);
        let text = line_text(&line);
        assert!(text.contains("considering options"), "{text}");
        assert!(
            line.spans[0].style.add_modifier.contains(Modifier::DIM),
            "preview should be dim: {line:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Brand parity with the original Ink TUI (`ui-tui`)
// ---------------------------------------------------------------------------
//
// Palette hexes are verbatim from `ui-tui/src/theme.ts` (`DARK_SEEDS`, plus
// the derived `label`/`muted` literals documented in `deriveTones`).
// Layout tiers, glyphs and line shapes mirror `banner.ts`, `branding.tsx`,
// `messageLine.tsx` (+ `domain/roles.ts`), `goalBar.tsx`, `loopBar.tsx`,
// `agentsPanel.tsx` (+ `lib/subagentGlyph.ts`, `lib/processGlyph.ts`,
// `lib/subagentTree.ts`), `appChrome.tsx` (`StatusRule`) and `helpHint.tsx`.

/// Exact Hermes dark palette (original `DARK_SEEDS` + derived label/muted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HermesColors {
    pub primary: Color,
    pub accent: Color,
    pub border: Color,
    pub text: Color,
    pub muted: Color,
    pub label: Color,
    pub ok: Color,
    pub error: Color,
    pub warn: Color,
    pub status_good: Color,
    pub status_warn: Color,
    pub status_bad: Color,
    pub status_critical: Color,
    pub shell_dollar: Color,
    pub prompt: Color,
}

const fn hermes_rgb(hex: u32) -> Color {
    Color::Rgb(
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
    )
}

/// Verbatim dark palette from the original `theme.ts`.
pub fn hermes_colors() -> HermesColors {
    HermesColors {
        primary: hermes_rgb(0xFFD700),
        accent: hermes_rgb(0xFFBF00),
        border: hermes_rgb(0xCD7F32),
        text: hermes_rgb(0xFFF8DC),
        muted: hermes_rgb(0xCC9B1F),
        label: hermes_rgb(0xDAA520),
        ok: hermes_rgb(0x4CAF50),
        error: hermes_rgb(0xEF5350),
        warn: hermes_rgb(0xFFA726),
        status_good: hermes_rgb(0x8FBC8F),
        status_warn: hermes_rgb(0xFFD700),
        status_bad: hermes_rgb(0xFF8C00),
        status_critical: hermes_rgb(0xFF6B6B),
        shell_dollar: hermes_rgb(0x4DABF7),
        prompt: hermes_rgb(0xFFF8DC),
    }
}

/// Brand strings from the original `theme.ts` (`BRAND`) and the i18n catalog.
pub const BRAND_ICON: &str = "☤";
pub const BRAND_NAME: &str = "Hermes Agent";
/// `brand.prompt` — the composer glyph, also the user-message gutter.
pub const PROMPT_GLYPH: &str = "❯";
/// `brand.tool` — the assistant-message gutter.
pub const TOOL_GLYPH: &str = "┊";
pub const TAG_FULL: &str = "Nous Research · Messenger of the Digital Gods";
pub const TAG_MID: &str = "Messenger of the Digital Gods";
pub const TAG_TINY: &str = "Nous Research";
/// Quick-help header (`help.quickHelp`).
pub const QUICK_HELP_HEADER: &str = "(^_^)? Commands";

/// Display width of the HERMES logo art (block glyphs are 1 cell each).
pub const LOGO_COLS: usize = 100;

/// HERMES logo with the original per-row gradient
/// `[primary, primary, accent, accent, border, border]`
/// (`banner.ts`: `LOGO_GRADIENT = [0,0,1,1,2,2]` over
/// `[primary, accent, border, muted]`).
pub fn logo_lines_hermes(colors: &HermesColors) -> Vec<Line<'static>> {
    let gradient = [
        colors.primary,
        colors.primary,
        colors.accent,
        colors.accent,
        colors.border,
        colors.border,
    ];
    LOGO_ART
        .iter()
        .zip(gradient.iter())
        .map(|(text, fg)| Line::from(Span::styled(*text, Style::default().fg(*fg))))
        .collect()
}

/// Caduceus with the original gradient
/// `[border, border, accent, accent, primary, primary, accent, accent,
///  border, border, muted, muted, muted, muted, muted]`
/// (`banner.ts`: `CADUC_GRADIENT = [2,2,1,1,0,0,1,1,2,2,3,3,3,3,3]`).
pub fn caduceus_lines_hermes(colors: &HermesColors) -> Vec<Line<'static>> {
    let gradient = [
        colors.border,
        colors.border,
        colors.accent,
        colors.accent,
        colors.primary,
        colors.primary,
        colors.accent,
        colors.accent,
        colors.border,
        colors.border,
        colors.muted,
        colors.muted,
        colors.muted,
        colors.muted,
        colors.muted,
    ];
    CADUCEUS_ART
        .iter()
        .zip(gradient.iter())
        .map(|(text, fg)| Line::from(Span::styled(*text, Style::default().fg(*fg))))
        .collect()
}

/// Display width of a plain string.
fn str_width(s: &str) -> usize {
    Span::raw(s).width()
}

/// Cut `s` to at most `max` display cells, without an ellipsis.
fn cut_str(s: &str, max: usize) -> String {
    if str_width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    for ch in s.chars() {
        let next = format!("{out}{ch}");
        if str_width(&next) > max {
            break;
        }
        out = next;
    }
    out
}

/// Truncate `s` to at most `max` display cells, with an ellipsis when cut.
pub fn truncate_str(s: &str, max: usize) -> String {
    if str_width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut cut = cut_str(s, max.saturating_sub(1));
    cut.push('…');
    cut
}

/// Hard-truncate a styled line to `max` display cells (truncate-end).
pub fn truncate_line(line: Line<'static>, max: usize) -> Line<'static> {
    if line.width() <= max {
        return line;
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut w = 0;
    for span in line.spans {
        let sw = span.width();
        if w + sw < max {
            w += sw;
            spans.push(span);
        } else {
            let rest = max.saturating_sub(w).saturating_sub(1);
            if rest > 0 {
                spans.push(Span::styled(
                    cut_str(span.content.as_ref(), rest),
                    span.style,
                ));
            }
            spans.push(Span::styled("…", span.style));
            break;
        }
    }
    Line::from(spans)
}

fn center_in(s: &str, w: usize) -> String {
    let clipped = truncate_str(s, w);
    let sw = str_width(&clipped);
    if sw >= w {
        return clipped;
    }
    let left = (w - sw) / 2;
    format!(
        "{}{}{}",
        " ".repeat(left),
        clipped,
        " ".repeat(w - sw - left)
    )
}

fn rule_in(label: &str, w: usize) -> String {
    let label = truncate_str(label, w.saturating_sub(4).max(1));
    let lw = str_width(&label);
    let slack = w.saturating_sub(lw + 2);
    let left = slack / 2;
    format!("{} {label} {}", "─".repeat(left), "─".repeat(slack - left))
}

/// Responsive banner tiers from `branding.tsx`:
/// full art (≥102 cols) → compact rule (≥58) → text (≥34) → hidden.
pub fn banner_lines(cols: usize, colors: &HermesColors) -> Vec<Line<'static>> {
    const HIDE_BELOW: usize = 34;
    const COMPACT_FROM: usize = 58;
    if cols < HIDE_BELOW {
        return Vec::new();
    }
    if cols >= LOGO_COLS + 2 {
        let mut lines = logo_lines_hermes(colors);
        lines.push(Line::from(Span::styled(
            format!("{BRAND_ICON} {TAG_FULL}"),
            Style::default().fg(colors.muted),
        )));
        return lines;
    }
    if cols >= COMPACT_FROM {
        let w = cols.saturating_sub(4).max(28);
        return vec![
            Line::from(Span::styled(
                rule_in(BRAND_NAME, w),
                Style::default().fg(colors.primary),
            )),
            Line::from(Span::styled(
                center_in(TAG_FULL, w),
                Style::default().fg(colors.muted),
            )),
            Line::from(Span::styled(
                "─".repeat(w),
                Style::default().fg(colors.primary),
            )),
        ];
    }
    let name = if cols >= 52 { BRAND_NAME } else { "Hermes" };
    let tag = if cols >= 64 {
        TAG_FULL
    } else if cols >= 46 {
        TAG_MID
    } else {
        TAG_TINY
    };
    vec![
        Line::from(Span::styled(
            format!("{BRAND_ICON} {name}"),
            Style::default()
                .fg(colors.primary)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("{BRAND_ICON} {tag}"),
            Style::default().fg(colors.muted),
        )),
    ]
}

/// Info for the intro session panel (`SessionPanel` in `branding.tsx`).
pub struct SessionPanelInfo<'a> {
    pub version: &'a str,
    pub model: &'a str,
    pub cwd: &'a str,
    pub tool_count: usize,
    pub skill_count: usize,
}

/// Rounded-border session panel: centered brand+version title, accent model
/// line, muted cwd, and the tools/skills summary + `/help` hint.
pub fn session_panel_lines(
    info: &SessionPanelInfo,
    width: usize,
    colors: &HermesColors,
) -> Vec<Line<'static>> {
    let w = width.max(28);
    let inner = w - 2; // between the │ borders
    let border = Style::default().fg(colors.border);

    let title = format!("{BRAND_NAME} v{}", info.version);
    let model = info.model.rsplit('/').next().unwrap_or(info.model);
    let summary = format!(
        "{} tools · {} skills · type /help for commands",
        info.tool_count, info.skill_count
    );
    let body: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            center_in(&title, inner),
            Style::default()
                .fg(colors.primary)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled(format!("  {model}"), Style::default().fg(colors.accent)),
            Span::styled(" · Nous Research", Style::default().fg(colors.muted)),
        ]),
        Line::from(Span::styled(
            format!("  {}", info.cwd),
            Style::default().fg(colors.muted),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("  {summary}"),
            Style::default().fg(colors.text),
        )),
    ];

    let mut lines = Vec::with_capacity(body.len() + 2);
    lines.push(Line::from(vec![
        Span::styled("╭", border),
        Span::styled("─".repeat(inner), border),
        Span::styled("╮", border),
    ]));
    for b in body {
        let b = truncate_line(b, inner);
        let pad = inner.saturating_sub(b.width());
        let mut spans = vec![Span::styled("│", border)];
        spans.extend(b.spans);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled("│", border));
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(vec![
        Span::styled("╰", border),
        Span::styled("─".repeat(inner), border),
        Span::styled("╯", border),
    ]));
    lines
}

/// User gutter: bold label-colored `❯ ` (`domain/roles.ts`: user → glyph
/// `brand.prompt`, color `label`, bold).
pub fn user_prompt_spans(colors: &HermesColors) -> Vec<Span<'static>> {
    vec![Span::styled(
        format!("{PROMPT_GLYPH} "),
        Style::default()
            .fg(colors.label)
            .add_modifier(Modifier::BOLD),
    )]
}

/// Assistant gutter: border-colored `┊ ` (`domain/roles.ts`: assistant →
/// glyph `brand.tool`, color `border`).
pub fn assistant_gutter_spans(colors: &HermesColors) -> Vec<Span<'static>> {
    vec![Span::styled(
        format!("{TOOL_GLYPH} "),
        Style::default().fg(colors.border),
    )]
}

/// System gutter: muted `· `.
pub fn system_glyph_spans(colors: &HermesColors) -> Vec<Span<'static>> {
    vec![Span::styled("· ", Style::default().fg(colors.muted))]
}

/// Tool-result gutter: muted `⚡ `.
pub fn tool_glyph_spans(colors: &HermesColors) -> Vec<Span<'static>> {
    vec![Span::styled("⚡ ", Style::default().fg(colors.muted))]
}

/// Timeline event row: dim `◈ <text>` (`messageLine.tsx` event branch).
pub fn event_line(text: &str, colors: &HermesColors) -> Line<'static> {
    Line::from(Span::styled(
        format!("◈ {text}"),
        Style::default()
            .fg(colors.muted)
            .add_modifier(Modifier::DIM),
    ))
}

/// Turn separator above later user messages (`───` in `border`,
/// from `appLayout.tsx`).
pub fn user_turn_separator(colors: &HermesColors) -> Line<'static> {
    Line::from(Span::styled("───", Style::default().fg(colors.border)))
}

/// Standing-goal row (`goalBar.tsx`): `⊙ <label> · <detail> · <title>`.
#[derive(Debug, Clone, Default)]
pub struct GoalStatus {
    pub label: String,
    pub detail: String,
    pub title: String,
}

pub fn goal_bar_line(goal: &GoalStatus, cols: usize, colors: &HermesColors) -> Line<'static> {
    let head_w = str_width(&format!("⊙ {} · {} · ", goal.label, goal.detail));
    let title = truncate_str(&goal.title, cols.saturating_sub(head_w).max(8));
    Line::from(vec![
        Span::styled(
            format!("⊙ {}", goal.label),
            Style::default().fg(colors.accent),
        ),
        Span::styled(
            format!(" · {} · ", goal.detail),
            Style::default().fg(colors.muted),
        ),
        Span::styled(title, Style::default().fg(colors.text)),
    ])
}

/// Standing-loop row (`loopBar.tsx`): `↻ <label> · <detail> · <prompt>`.
#[derive(Debug, Clone, Default)]
pub struct LoopStatus {
    pub label: String,
    pub detail: String,
    pub prompt: String,
}

pub fn loop_bar_line(job: &LoopStatus, cols: usize, colors: &HermesColors) -> Line<'static> {
    let head_w = str_width(&format!("↻ {} · {} · ", job.label, job.detail));
    let prompt = truncate_str(&job.prompt, cols.saturating_sub(head_w).max(8));
    Line::from(vec![
        Span::styled(
            format!("↻ {}", job.label),
            Style::default().fg(colors.accent),
        ),
        Span::styled(
            format!(" · {} · ", job.detail),
            Style::default().fg(colors.muted),
        ),
        Span::styled(prompt, Style::default().fg(colors.text)),
    ])
}

/// Subagent status → glyph/color table (`lib/subagentGlyph.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Running,
    Queued,
    Finalizing,
    Completed,
    Interrupted,
    Failed,
}

impl AgentStatus {
    pub fn glyph(self) -> &'static str {
        match self {
            AgentStatus::Running => "●",
            AgentStatus::Queued => "○",
            AgentStatus::Finalizing => "◐",
            AgentStatus::Completed => "✓",
            AgentStatus::Interrupted => "■",
            AgentStatus::Failed => "✗",
        }
    }

    pub fn color(self, colors: &HermesColors) -> Color {
        match self {
            AgentStatus::Running | AgentStatus::Finalizing => colors.accent,
            AgentStatus::Queued => colors.muted,
            AgentStatus::Completed => colors.status_good,
            AgentStatus::Interrupted => colors.warn,
            AgentStatus::Failed => colors.error,
        }
    }

    pub fn is_live(self) -> bool {
        matches!(
            self,
            AgentStatus::Running | AgentStatus::Queued | AgentStatus::Finalizing
        )
    }
}

/// One live-agent dock row.
#[derive(Debug, Clone)]
pub struct AgentRow {
    pub goal: String,
    pub detail: String,
    pub status: AgentStatus,
    pub elapsed_secs: Option<u64>,
}

/// Background-process status → glyph (`lib/processGlyph.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStatus {
    Running,
    Done,
    Failed,
    Killed,
}

impl ProcessStatus {
    pub fn glyph(self) -> &'static str {
        match self {
            ProcessStatus::Running => "⚙",
            ProcessStatus::Done => "✔",
            ProcessStatus::Failed | ProcessStatus::Killed => "✘",
        }
    }

    pub fn color(self, colors: &HermesColors) -> Color {
        match self {
            ProcessStatus::Running => colors.accent,
            ProcessStatus::Done => colors.status_good,
            ProcessStatus::Failed => colors.error,
            ProcessStatus::Killed => colors.warn,
        }
    }
}

/// One background-process dock row.
#[derive(Debug, Clone)]
pub struct ProcessRow {
    pub command: String,
    pub detail: String,
    pub status: ProcessStatus,
    pub elapsed_secs: u64,
}

/// `Ns` / `Nm` / `Nm Ss` (`lib/subagentTree.ts::fmtDuration`).
pub fn fmt_duration_secs(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    let m = secs / 60;
    let s = secs % 60;
    if s == 0 {
        format!("{m}m")
    } else {
        format!("{m}m {s}s")
    }
}

/// Live agents dock (`agentsPanel.tsx`): header + `● goal <dur>` rows with
/// `↳ detail` sub-rows, then the background-process block. Renders nothing
/// when there are no live agents and no processes.
pub fn agents_panel_lines(
    rows: &[AgentRow],
    processes: &[ProcessRow],
    collapsed: bool,
    cols: usize,
    colors: &HermesColors,
) -> Vec<Line<'static>> {
    let live: Vec<&AgentRow> = rows.iter().filter(|r| r.status.is_live()).collect();
    if live.is_empty() && processes.is_empty() {
        return Vec::new();
    }
    let plural = |n: usize, one: &str, many: &str| {
        if n == 1 {
            one.to_string()
        } else {
            many.to_string()
        }
    };
    let mut lines: Vec<Line> = Vec::new();
    if collapsed {
        let summary = match live.first() {
            Some(first) if !first.detail.is_empty() => format!(
                "▸ {} {} · {}",
                live.len(),
                plural(live.len(), "live agent", "live agents"),
                truncate_str(&first.detail, cols.saturating_sub(24).max(8))
            ),
            _ => format!(
                "▸ {} {}",
                live.len(),
                plural(live.len(), "live agent", "live agents")
            ),
        };
        lines.push(truncate_line(
            Line::from(Span::styled(
                summary,
                Style::default()
                    .fg(colors.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            cols,
        ));
        return lines;
    }
    if !live.is_empty() {
        let header = format!(
            "▾ {} {}",
            live.len(),
            plural(live.len(), "live agent", "live agents")
        );
        lines.push(truncate_line(
            Line::from(Span::styled(
                header,
                Style::default()
                    .fg(colors.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            cols,
        ));
        for row in live {
            let elapsed = row
                .elapsed_secs
                .map(|e| format!(" {}", fmt_duration_secs(e)))
                .unwrap_or_default();
            lines.push(truncate_line(
                Line::from(vec![
                    Span::styled(
                        format!("{} ", row.status.glyph()),
                        Style::default().fg(row.status.color(colors)),
                    ),
                    Span::styled(
                        truncate_str(&row.goal, cols.saturating_sub(12).max(8)),
                        Style::default().fg(colors.text),
                    ),
                    Span::styled(elapsed, Style::default().fg(colors.muted)),
                ]),
                cols,
            ));
            if !row.detail.is_empty() {
                lines.push(truncate_line(
                    Line::from(Span::styled(
                        format!(
                            "  ↳ {}",
                            truncate_str(&row.detail, cols.saturating_sub(6).max(8))
                        ),
                        Style::default().fg(colors.muted),
                    )),
                    cols,
                ));
            }
        }
    }
    if !processes.is_empty() {
        let running = processes
            .iter()
            .filter(|p| p.status == ProcessStatus::Running)
            .count();
        let done = processes.len() - running;
        let mut summary = format!(
            "▾ {} {}",
            processes.len(),
            plural(processes.len(), "process", "processes")
        );
        let mut bits = Vec::new();
        if running > 0 {
            bits.push(format!("{running} running"));
        }
        if done > 0 {
            bits.push(format!("{done} done"));
        }
        if !bits.is_empty() {
            summary.push_str(&format!(" · {}", bits.join(" · ")));
        }
        lines.push(truncate_line(
            Line::from(Span::styled(
                summary,
                Style::default()
                    .fg(colors.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            cols,
        ));
        for p in processes {
            let activity = if p.status == ProcessStatus::Running {
                format!("{} · {}", fmt_duration_secs(p.elapsed_secs), p.detail)
            } else {
                p.detail.clone()
            };
            lines.push(truncate_line(
                Line::from(vec![
                    Span::styled(
                        format!("{} ", p.status.glyph()),
                        Style::default().fg(p.status.color(colors)),
                    ),
                    Span::styled(
                        truncate_str(&p.command, cols.saturating_sub(16).max(8)),
                        Style::default().fg(colors.text),
                    ),
                    Span::styled(
                        format!(
                            " · {}",
                            truncate_str(&activity, cols.saturating_sub(16).max(8))
                        ),
                        Style::default().fg(colors.muted),
                    ),
                ]),
                cols,
            ));
        }
    }
    lines
}

/// Compact number: `12k`, `1.2M` (`apps/shared/src/format.ts::compactNumber`).
pub fn compact_number(n: u64) -> String {
    let scaled = |v: f64, suffix: &str| {
        let s = format!("{v:.1}");
        let s = s.strip_suffix(".0").unwrap_or(&s);
        format!("{s}{suffix}")
    };
    if n >= 999_950 {
        scaled(n as f64 / 1_000_000.0, "M")
    } else if n >= 1000 {
        scaled(n as f64 / 1000.0, "k")
    } else {
        n.to_string()
    }
}

/// Context fill bar: `█████░░░░░` (`appChrome.tsx::ctxBar`).
pub fn ctx_bar(pct: f64, width: usize) -> String {
    let p = pct.clamp(0.0, 100.0);
    let filled = ((p / 100.0) * width as f64).round() as usize;
    format!(
        "{}{}",
        "█".repeat(filled.min(width)),
        "░".repeat(width.saturating_sub(filled))
    )
}

/// Context bar color by fill (`appChrome.tsx::ctxBarColor`).
pub fn ctx_bar_color(pct: Option<f64>, colors: &HermesColors) -> Color {
    match pct {
        None => colors.muted,
        Some(p) if p >= 95.0 => colors.status_critical,
        Some(p) if p > 80.0 => colors.status_bad,
        Some(p) if p >= 50.0 => colors.status_warn,
        _ => colors.status_good,
    }
}

/// Rotating busy verbs (mirrors the `VERBS` ticker in `appChrome.tsx`).
const BUSY_VERBS: &[&str] = &[
    "thinking",
    "working",
    "reasoning",
    "pondering",
    "deliberating",
    "composing",
    "analyzing",
    "synthesizing",
];

pub fn busy_verb(tick: usize) -> &'static str {
    BUSY_VERBS[(tick / 25) % BUSY_VERBS.len()]
}

/// Short model label (`appChrome.tsx::shortModelLabel`): drop the provider
/// prefix, turn `-`/`_` into spaces.
pub fn short_model_label(model: &str) -> String {
    let base = model.rsplit('/').next().unwrap_or(model);
    let base = base
        .strip_prefix("claude-")
        .or_else(|| base.strip_prefix("claude_"))
        .unwrap_or(base);
    let base = base
        .strip_prefix("anthropic-")
        .or_else(|| base.strip_prefix("anthropic_"))
        .unwrap_or(base);
    base.replace(['-', '_'], " ").trim().to_string()
}

/// `$HOME` → `~` for the status-bar cwd segment.
pub fn short_path(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = path.strip_prefix(home.as_str()) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

/// Status-rule inputs (the `StatusRule` props in `appChrome.tsx`, trimmed).
pub struct StatusInfo<'a> {
    pub busy: bool,
    pub tick: usize,
    pub elapsed_secs: Option<u64>,
    pub model: &'a str,
    pub ctx_used: Option<u64>,
    pub ctx_max: Option<u64>,
    pub session_secs: Option<u64>,
    pub cwd: &'a str,
}

/// Bottom status rule:
/// `─ <spinner> <verb>… · <elapsed> │ <model> │ <ctx> │ [<bar>] <pct>% │ <session>   │ <cwd>`.
pub fn status_rule_line(info: &StatusInfo, cols: usize, colors: &HermesColors) -> Line<'static> {
    let cols = cols.max(24);
    let dim = Style::default().fg(colors.muted);
    let mut spans = vec![Span::styled("─ ", Style::default().fg(colors.border))];
    if info.busy {
        let mut s = format!("{} {}…", spinner_frame(info.tick), busy_verb(info.tick));
        if let Some(e) = info.elapsed_secs {
            s.push_str(&format!(" · {}", fmt_duration_secs(e)));
        }
        spans.push(Span::styled(s, Style::default().fg(colors.accent)));
    } else {
        spans.push(Span::styled(
            "✓ ready",
            Style::default().fg(colors.status_good),
        ));
    }
    spans.push(Span::styled(" │ ", dim));
    spans.push(Span::styled(short_model_label(info.model), dim));
    if let (Some(used), Some(max)) = (info.ctx_used, info.ctx_max) {
        if max > 0 {
            let pct = used as f64 / max as f64 * 100.0;
            spans.push(Span::styled(
                format!(" │ {}/{}", compact_number(used), compact_number(max)),
                dim,
            ));
            spans.push(Span::styled(
                format!(" [{}] {:.0}%", ctx_bar(pct, 10), pct),
                Style::default().fg(ctx_bar_color(Some(pct), colors)),
            ));
        }
    }
    if let Some(s) = info.session_secs {
        spans.push(Span::styled(format!(" │ {}", fmt_duration_secs(s)), dim));
    }
    // Right-anchored cwd; it yields first on narrow terminals.
    let left_w = Line::from(spans.clone()).width();
    let max_right = cols.saturating_sub(left_w + 3).min(cols / 3);
    if max_right >= 10 {
        let cwd = truncate_str(&short_path(info.cwd), max_right);
        spans.push(Span::styled(" │ ", dim));
        spans.push(Span::styled(cwd, Style::default().fg(colors.label)));
    }
    truncate_line(Line::from(spans), cols)
}

/// One-line key hints under the status rule (from `helpHint.tsx`'s `?` overlay).
pub fn key_hints_line(colors: &HermesColors) -> Line<'static> {
    let key = Style::default().fg(colors.label);
    let dim = Style::default().fg(colors.muted);
    Line::from(vec![
        Span::styled("? ", key),
        Span::styled("quick help", dim),
        Span::styled(" │ ", dim),
        Span::styled("/help", key),
        Span::styled(" commands", dim),
        Span::styled(" │ ", dim),
        Span::styled("Ctrl+C", key),
        Span::styled(" interrupt", dim),
        Span::styled(" │ ", dim),
        Span::styled("Ctrl+D", key),
        Span::styled(" exit", dim),
    ])
}

/// Floating `?` quick-help box (`helpHint.tsx`): `(^_^)? Commands` header,
/// common commands, then hotkeys — rounded primary border.
pub fn help_hint_lines(colors: &HermesColors) -> Vec<Line<'static>> {
    const COMMANDS: &[(&str, &str)] = &[
        ("/help", "full list of commands + hotkeys"),
        ("/clear", "start a new session"),
        ("/resume", "switch live or resume past sessions"),
        ("/details", "control transcript detail level"),
        ("/copy", "copy selection or last assistant message"),
        ("/quit", "exit hermes"),
    ];
    const HOTKEYS: &[(&str, &str)] = &[
        ("Ctrl+C", "interrupt agent"),
        ("Ctrl+D", "exit"),
        ("Ctrl+G", "open editor"),
        ("Ctrl+L", "redraw / repaint"),
        ("Alt+V", "paste text"),
        ("Esc Esc", "discard draft"),
        ("Tab", "apply completion"),
        ("↑/↓", "completions / history"),
    ];
    let label_w = COMMANDS
        .iter()
        .chain(HOTKEYS.iter())
        .map(|(k, _)| str_width(k))
        .max()
        .unwrap_or(8);
    let pad = |k: &str| {
        format!(
            "{}{}",
            k,
            " ".repeat(label_w.saturating_sub(str_width(k)) + 2)
        )
    };

    let mut rows: Vec<Line> = Vec::new();
    rows.push(Line::from(vec![
        Span::styled(
            QUICK_HELP_HEADER,
            Style::default()
                .fg(colors.primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  ·  type /help for the full panel",
            Style::default().fg(colors.muted),
        ),
    ]));
    rows.push(Line::from(""));
    rows.push(Line::from(Span::styled(
        "Common commands",
        Style::default()
            .fg(colors.accent)
            .add_modifier(Modifier::BOLD),
    )));
    for (k, v) in COMMANDS {
        rows.push(Line::from(vec![
            Span::styled(pad(k), Style::default().fg(colors.label)),
            Span::styled(*v, Style::default().fg(colors.muted)),
        ]));
    }
    rows.push(Line::from(""));
    rows.push(Line::from(Span::styled(
        "Hotkeys",
        Style::default()
            .fg(colors.accent)
            .add_modifier(Modifier::BOLD),
    )));
    for (k, v) in HOTKEYS {
        rows.push(Line::from(vec![
            Span::styled(pad(k), Style::default().fg(colors.label)),
            Span::styled(*v, Style::default().fg(colors.muted)),
        ]));
    }

    let inner = rows.iter().map(|l| l.width()).max().unwrap_or(0) + 2;
    let border = Style::default().fg(colors.primary);
    let frame_row = |left: &'static str, right: &'static str| {
        Line::from(vec![
            Span::styled(left, border),
            Span::styled("─".repeat(inner), border),
            Span::styled(right, border),
        ])
    };
    let mut lines = vec![frame_row("╭", "╮")];
    for r in rows {
        let r = truncate_line(r, inner.saturating_sub(2));
        let pad_w = inner.saturating_sub(2).saturating_sub(r.width());
        let mut spans = vec![Span::styled("│ ", border)];
        spans.extend(r.spans);
        spans.push(Span::raw(" ".repeat(pad_w)));
        spans.push(Span::styled(" │", border));
        lines.push(Line::from(spans));
    }
    lines.push(frame_row("╰", "╯"));
    lines
}

#[cfg(test)]
mod brand_tests {
    use super::*;

    fn hermes() -> HermesColors {
        hermes_colors()
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn palette_matches_original_dark_seeds() {
        let c = hermes();
        assert_eq!(c.primary, Color::Rgb(0xFF, 0xD7, 0x00));
        assert_eq!(c.accent, Color::Rgb(0xFF, 0xBF, 0x00));
        assert_eq!(c.border, Color::Rgb(0xCD, 0x7F, 0x32));
        assert_eq!(c.text, Color::Rgb(0xFF, 0xF8, 0xDC));
        assert_eq!(c.muted, Color::Rgb(0xCC, 0x9B, 0x1F));
        assert_eq!(c.label, Color::Rgb(0xDA, 0xA5, 0x20));
        assert_eq!(c.ok, Color::Rgb(0x4C, 0xAF, 0x50));
        assert_eq!(c.error, Color::Rgb(0xEF, 0x53, 0x50));
        assert_eq!(c.warn, Color::Rgb(0xFF, 0xA7, 0x26));
        assert_eq!(c.status_good, Color::Rgb(0x8F, 0xBC, 0x8F));
        assert_eq!(c.shell_dollar, Color::Rgb(0x4D, 0xAB, 0xF7));
    }

    #[test]
    fn logo_hermes_uses_original_gradient() {
        let c = hermes();
        let lines = logo_lines_hermes(&c);
        assert_eq!(lines.len(), 6);
        assert_eq!(line_text(&lines[0]), LOGO_ART[0]);
        let fg: Vec<Option<Color>> = lines.iter().map(|l| l.spans[0].style.fg).collect();
        assert_eq!(
            fg,
            vec![
                Some(c.primary),
                Some(c.primary),
                Some(c.accent),
                Some(c.accent),
                Some(c.border),
                Some(c.border),
            ]
        );
    }

    #[test]
    fn caduceus_hermes_uses_original_gradient() {
        let c = hermes();
        let lines = caduceus_lines_hermes(&c);
        assert_eq!(lines.len(), 15);
        assert_eq!(line_text(&lines[0]), CADUCEUS_ART[0]);
        assert_eq!(lines[0].spans[0].style.fg, Some(c.border));
        assert_eq!(lines[4].spans[0].style.fg, Some(c.primary));
        assert_eq!(lines[14].spans[0].style.fg, Some(c.muted));
    }

    #[test]
    fn banner_tiers_by_width() {
        let c = hermes();
        // Full art tier.
        let full = banner_lines(120, &c);
        assert_eq!(full.len(), 7, "6 logo rows + tagline");
        assert!(line_text(&full[6]).contains("Nous Research"));
        // Compact tier.
        let compact = banner_lines(70, &c);
        assert_eq!(compact.len(), 3);
        assert!(line_text(&compact[0]).contains("Hermes Agent"));
        // Text tier.
        let text = banner_lines(55, &c);
        assert_eq!(text.len(), 2);
        assert!(line_text(&text[0]).contains("Hermes Agent"));
        // Narrow text tier uses the short name.
        let narrow = banner_lines(40, &c);
        assert!(line_text(&narrow[0]).contains("Hermes"));
        // Hidden tier.
        assert!(banner_lines(30, &c).is_empty());
    }

    #[test]
    fn session_panel_has_rounded_border_and_title() {
        let c = hermes();
        let info = SessionPanelInfo {
            version: "0.2.0",
            model: "provider/deepseek-chat",
            cwd: "/home/chewy",
            tool_count: 51,
            skill_count: 8,
        };
        let lines = session_panel_lines(&info, 60, &c);
        let first = line_text(&lines[0]);
        let last = line_text(&lines[lines.len() - 1]);
        assert!(first.starts_with('╭') && first.ends_with('╮'), "{first}");
        assert!(last.starts_with('╰') && last.ends_with('╯'), "{last}");
        let body: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(body.contains("Hermes Agent v0.2.0"), "{body}");
        assert!(body.contains("deepseek-chat · Nous Research"), "{body}");
        assert!(body.contains("51 tools · 8 skills"), "{body}");
    }

    #[test]
    fn goal_and_loop_bars_shape() {
        let c = hermes();
        let goal = GoalStatus {
            label: "goal".to_string(),
            detail: "3/20 turns".to_string(),
            title: "Ship the TUI parity".to_string(),
        };
        let text = line_text(&goal_bar_line(&goal, 80, &c));
        assert_eq!(text, "⊙ goal · 3/20 turns · Ship the TUI parity", "{text}");

        let job = LoopStatus {
            label: "loop".to_string(),
            detail: "every 45m".to_string(),
            prompt: "run the checks".to_string(),
        };
        let text = line_text(&loop_bar_line(&job, 80, &c));
        assert_eq!(text, "↻ loop · every 45m · run the checks", "{text}");
    }

    #[test]
    fn agents_panel_empty_renders_nothing() {
        let c = hermes();
        assert!(agents_panel_lines(&[], &[], false, 80, &c).is_empty());
    }

    #[test]
    fn agents_panel_rows_and_processes() {
        let c = hermes();
        let rows = vec![AgentRow {
            goal: "Research TUI layout".to_string(),
            detail: "reading appLayout.tsx".to_string(),
            status: AgentStatus::Running,
            elapsed_secs: Some(42),
        }];
        let procs = vec![ProcessRow {
            command: "cargo test".to_string(),
            detail: "compiling".to_string(),
            status: ProcessStatus::Running,
            elapsed_secs: 7,
        }];
        let lines = agents_panel_lines(&rows, &procs, false, 100, &c);
        let body: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(body.contains("▾ 1 live agent"), "{body}");
        assert!(body.contains("● Research TUI layout 42s"), "{body}");
        assert!(body.contains("↳ reading appLayout.tsx"), "{body}");
        assert!(body.contains("▾ 1 process · 1 running"), "{body}");
        assert!(body.contains("⚙ cargo test · 7s · compiling"), "{body}");

        let collapsed = agents_panel_lines(&rows, &[], true, 100, &c);
        assert_eq!(collapsed.len(), 1);
        assert!(line_text(&collapsed[0]).starts_with("▸ 1 live agent"));
    }

    #[test]
    fn fmt_duration_secs_shapes() {
        assert_eq!(fmt_duration_secs(0), "0s");
        assert_eq!(fmt_duration_secs(59), "59s");
        assert_eq!(fmt_duration_secs(60), "1m");
        assert_eq!(fmt_duration_secs(90), "1m 30s");
        assert_eq!(fmt_duration_secs(3600), "60m");
    }

    #[test]
    fn compact_number_shapes() {
        assert_eq!(compact_number(0), "0");
        assert_eq!(compact_number(999), "999");
        assert_eq!(compact_number(12_000), "12k");
        assert_eq!(compact_number(1_500_000), "1.5M");
    }

    #[test]
    fn ctx_bar_fill_and_color_thresholds() {
        let c = hermes();
        assert_eq!(ctx_bar(45.0, 10), "█████░░░░░");
        assert_eq!(ctx_bar(0.0, 4), "░░░░");
        assert_eq!(ctx_bar_color(Some(96.0), &c), c.status_critical);
        assert_eq!(ctx_bar_color(Some(85.0), &c), c.status_bad);
        assert_eq!(ctx_bar_color(Some(60.0), &c), c.status_warn);
        assert_eq!(ctx_bar_color(Some(10.0), &c), c.status_good);
        assert_eq!(ctx_bar_color(None, &c), c.muted);
    }

    #[test]
    fn short_model_label_strips_provider() {
        assert_eq!(
            short_model_label("atmo/deepseek-v4.1-flash"),
            "deepseek v4.1 flash"
        );
        assert_eq!(short_model_label("anthropic/claude-sonnet-4"), "sonnet 4");
        assert_eq!(short_model_label("gpt-4o"), "gpt 4o");
    }

    #[test]
    fn status_rule_busy_and_idle() {
        let c = hermes();
        let busy = StatusInfo {
            busy: true,
            tick: 0,
            elapsed_secs: Some(65),
            model: "atmo/deepseek-v4.1-flash",
            ctx_used: Some(12_000),
            ctx_max: Some(128_000),
            session_secs: Some(185),
            cwd: "/home/chewy/project",
        };
        let text = line_text(&status_rule_line(&busy, 120, &c));
        assert!(text.starts_with("─ "), "{text}");
        assert!(text.contains("thinking…"), "{text}");
        assert!(text.contains("1m 5s"), "{text}");
        assert!(text.contains("deepseek v4.1 flash"), "{text}");
        assert!(text.contains("12k/128k"), "{text}");
        assert!(text.contains("3m 5s"), "{text}");

        let idle = StatusInfo {
            busy: false,
            tick: 0,
            elapsed_secs: None,
            model: "gpt-4o",
            ctx_used: None,
            ctx_max: None,
            session_secs: None,
            cwd: "/tmp",
        };
        let text = line_text(&status_rule_line(&idle, 120, &c));
        assert!(text.contains("✓ ready"), "{text}");
        assert!(text.contains("gpt 4o"), "{text}");
    }

    #[test]
    fn help_hint_is_bordered_box() {
        let c = hermes();
        let lines = help_hint_lines(&c);
        let first = line_text(&lines[0]);
        let last = line_text(&lines[lines.len() - 1]);
        assert!(first.starts_with('╭') && first.ends_with('╮'), "{first}");
        assert!(last.starts_with('╰') && last.ends_with('╯'), "{last}");
        let body: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(body.contains("(^_^)? Commands"), "{body}");
        assert!(body.contains("Common commands"), "{body}");
        assert!(body.contains("/quit"), "{body}");
        assert!(body.contains("Hotkeys"), "{body}");
        assert!(body.contains("Ctrl+C"), "{body}");
    }

    #[test]
    fn truncate_helpers() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello world", 5), "hell…");
        assert_eq!(truncate_str("hello", 0), "");
        let line = Line::from(vec![
            Span::styled("hello ", Style::default().fg(Color::Red)),
            Span::raw("world"),
        ]);
        let text = line_text(&truncate_line(line, 8));
        assert_eq!(text, "hello w…", "{text}");
    }

    #[test]
    fn message_glyph_spans() {
        let c = hermes();
        assert_eq!(line_text(&Line::from(user_prompt_spans(&c))), "❯ ");
        assert_eq!(line_text(&Line::from(assistant_gutter_spans(&c))), "┊ ");
        assert_eq!(line_text(&Line::from(system_glyph_spans(&c))), "· ");
        assert_eq!(line_text(&Line::from(tool_glyph_spans(&c))), "⚡ ");
        assert!(line_text(&event_line("model switched", &c)).starts_with("◈ "));
        assert_eq!(line_text(&user_turn_separator(&c)), "───");
    }
}
