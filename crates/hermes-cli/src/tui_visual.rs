//! Visual primitives ala Reasonix untuk TUI: art splash, spinner,
//! header, dan baris aktivitas tool.
//!
//! Art diambil verbatim dari referensi
//! `~/workspace/hermes-rs/crates/hermes-cli/src/tui.rs`; jangan
//! diketik ulang manual (braille harus byte-exact).

use crate::theme::RatatuiColors;
use ratatui::{
    style::{Modifier, Style},
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

/// Convert markdown text into styled ratatui lines.
///
/// Behavior is a verbatim port of the reference TUI renderer:
/// headings are bold accent with gaps around them, fenced code blocks emit a
/// `┌─ {lang}` label line plus a backgrounded body, lists use `• `/`N. `
/// markers, blockquotes are prefixed with `▎ ` per nesting level, links are
/// accent-underlined, soft/hard breaks end the current line, horizontal rules
/// render as 24 `─` chars, and raw HTML renders as italic secondary text.
pub fn markdown_lines(md: &str, colors: &RatatuiColors) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut st = MdInline::default();
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut in_code_block = false;
    let mut code_lang: String;
    let mut code_buf = String::new();
    let mut quote: u32 = 0;

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

    let parser = Parser::new_ext(md, Options::empty());
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
                cur.push(Span::styled(
                    format!("{indent}{bullet}"),
                    Style::default().fg(colors.secondary),
                ));
            }
            MdEvent::End(TagEnd::Item) => flush!(),
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
                if in_code_block {
                    code_buf.push_str(&t);
                } else {
                    cur.push(Span::styled(t.into_string(), st.style(colors)));
                }
            }
            MdEvent::Code(t) => cur.push(Span::styled(
                t.into_string(),
                Style::default()
                    .fg(colors.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            MdEvent::Html(t) | MdEvent::InlineHtml(t) => cur.push(Span::styled(
                t.into_string(),
                Style::default()
                    .fg(colors.secondary)
                    .add_modifier(Modifier::ITALIC),
            )),
            MdEvent::SoftBreak | MdEvent::HardBreak => flush!(),
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
}
