//! `hermes logs` — view, tail, and filter Hermes log files.
//!
//! Ported from NousResearch/hermes-agent's `hermes_cli/logs.py` (parser surface
//! from `hermes_cli/subcommands/logs.py`):
//!
//! * multiple named logs under `<hermes-home>/logs/` (`agent`, `errors`,
//!   `gateway`, `gui`, `desktop`, `update`, `handoff`, `mcp`) plus the fork's
//!   own `hermes` log;
//! * `hermes logs list` shows every known log with size and age;
//! * `-n/--lines`, `-f/--follow`, `--level`, `--session`, `--since` (`1h`,
//!   `30m`, `2d`, `45s`), and `--component` (logger-name prefix) filters.
//!
//! A line without its own timestamp continues the record above it, so
//! tracebacks and multi-line messages inherit that record's filter verdict
//! (mirrors the original's `_LineFilter`).

use anyhow::{bail, Context, Result};
use regex::Regex;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Known log files: CLI name → filename under `<home>/logs/`.
pub const LOG_FILES: &[(&str, &str)] = &[
    ("agent", "agent.log"),
    ("errors", "errors.log"),
    ("gateway", "gateway.log"),
    ("gui", "gui.log"),
    ("desktop", "desktop.log"),
    // Full stdout/stderr mirror of `hermes update` runs (append-only).
    ("update", "update.log"),
    // Desktop-driven update hand-off log.
    ("handoff", "desktop-update-handoff.log"),
    // Every stdio MCP subprocess's stderr.
    ("mcp", "mcp-stderr.log"),
    // Fork-specific: interactive CLI sessions log here.
    ("hermes", "hermes.log"),
];

static TS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2})").unwrap());
static LEVEL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s(DEBUG|INFO|WARNING|WARN|ERROR|CRITICAL)\s").unwrap());
static LOGGER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s(?:DEBUG|INFO|WARNING|WARN|ERROR|CRITICAL)(?:\s+\[.*?\])?\s+(\S+):").unwrap()
});
static SINCE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d+)\s*([smhd])$").unwrap());

/// Numeric severity for `--level` filtering (higher = more severe).
fn level_order(level: &str) -> Option<u8> {
    match level {
        "DEBUG" => Some(0),
        "INFO" => Some(1),
        "WARNING" | "WARN" => Some(2),
        "ERROR" => Some(3),
        "CRITICAL" => Some(4),
        _ => None,
    }
}

/// Canonical level name (`WARN` → `WARNING`).
fn canonical_level(level: &str) -> &str {
    match level {
        "WARN" => "WARNING",
        _ => level,
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct LogsArgs {
    /// Log to view: agent (default), errors, gateway, gui, desktop, update,
    /// handoff, mcp, hermes — or 'list' to show available files.
    pub log_name: Option<String>,

    /// Number of lines to show.
    #[arg(short = 'n', long, default_value_t = 50)]
    pub lines: u32,

    /// Follow the log in real time (like tail -f).
    #[arg(short = 'f', long)]
    pub follow: bool,

    /// Minimum log level to show (DEBUG, INFO, WARNING, ERROR, CRITICAL).
    #[arg(long)]
    pub level: Option<String>,

    /// Filter lines containing this session ID substring.
    #[arg(long)]
    pub session: Option<String>,

    /// Show lines since TIME ago (e.g. 1h, 30m, 2d).
    #[arg(long)]
    pub since: Option<String>,

    /// Only show lines whose logger name starts with this prefix
    /// (e.g. "tools", "gateway").
    #[arg(long)]
    pub component: Option<String>,

    /// Config dir override (wired by the parent from the global --config-dir).
    #[arg(skip)]
    pub config_dir: Option<PathBuf>,
}

/// Filename for a known log name, or `None` for an unknown name.
pub fn log_filename(name: &str) -> Option<&'static str> {
    LOG_FILES.iter().find(|(n, _)| *n == name).map(|(_, f)| *f)
}

/// Absolute path of a known log (existence is not checked).
pub fn log_file_path(home: &Path, name: &str) -> Option<PathBuf> {
    log_filename(name).map(|f| home.join("logs").join(f))
}

/// Parse a relative time like `1h`, `30m`, `2d`, `45s` into a [`Duration`];
/// `None` when unparseable. Mirrors the original's `_parse_since`.
pub fn parse_since(value: &str) -> Option<Duration> {
    let lowered = value.trim().to_lowercase();
    let caps = SINCE_RE.captures(lowered.as_str())?;
    let amount: u64 = caps.get(1)?.as_str().parse().ok()?;
    let secs = match caps.get(2)?.as_str() {
        "s" => amount,
        "m" => amount.checked_mul(60)?,
        "h" => amount.checked_mul(3600)?,
        "d" => amount.checked_mul(86400)?,
        _ => return None,
    };
    Some(Duration::from_secs(secs))
}

/// Extract the leading `YYYY-MM-DD HH:MM:SS` (or `T`-separated ISO) timestamp
/// from a log line; `None` when the line carries no stamp.
pub fn parse_line_timestamp(line: &str) -> Option<chrono::NaiveDateTime> {
    let caps = TS_RE.captures(line)?;
    let stamp = caps.get(1)?.as_str().replace('T', " ");
    chrono::NaiveDateTime::parse_from_str(&stamp, "%Y-%m-%d %H:%M:%S").ok()
}

/// Extract the log level token (`DEBUG`/`INFO`/`WARNING`/`ERROR`/`CRITICAL`).
pub fn extract_level(line: &str) -> Option<&str> {
    LEVEL_RE.captures(line)?.get(1).map(|m| m.as_str())
}

/// Extract the logger name: the token before `:` after the level and an
/// optional `[session]` tag, e.g. `gateway.run` in `INFO gateway.run: …`.
pub fn extract_logger_name(line: &str) -> Option<&str> {
    LOGGER_RE.captures(line)?.get(1).map(|m| m.as_str())
}

/// Active filters for one `hermes logs` invocation.
#[derive(Debug, Default, Clone)]
pub struct LogFilters {
    /// Minimum level (canonical name, e.g. `WARNING`).
    pub min_level: Option<String>,
    /// Session ID substring that must appear in the line.
    pub session: Option<String>,
    /// Cutoff timestamp; lines stamped older than this are dropped.
    pub since: Option<chrono::NaiveDateTime>,
    /// Logger-name prefixes; at least one must match.
    pub component_prefixes: Vec<String>,
}

impl LogFilters {
    /// Whether one line passes every active filter. A line without a
    /// timestamp/level passes those filters (the stateful [`LineFilter`]
    /// decides for unstamped continuation lines).
    pub fn matches(&self, line: &str) -> bool {
        if let Some(since) = self.since {
            if let Some(ts) = parse_line_timestamp(line) {
                if ts < since {
                    return false;
                }
            }
        }
        if let Some(min) = &self.min_level {
            if let Some(level) = extract_level(line) {
                let got = level_order(canonical_level(level)).unwrap_or(0);
                let want = level_order(min).unwrap_or(0);
                if got < want {
                    return false;
                }
            }
        }
        if let Some(session) = &self.session {
            if !line.contains(session) {
                return false;
            }
        }
        if !self.component_prefixes.is_empty() {
            match extract_logger_name(line) {
                Some(name) => {
                    let lower = name.to_lowercase();
                    if !self
                        .component_prefixes
                        .iter()
                        .any(|p| lower.starts_with(&p.to_lowercase()))
                    {
                        return false;
                    }
                }
                None => return false,
            }
        }
        true
    }

    pub fn has_any(&self) -> bool {
        self.min_level.is_some()
            || self.session.is_some()
            || self.since.is_some()
            || !self.component_prefixes.is_empty()
    }
}

/// Stateful filter over consecutive lines of one file. A line with no stamp
/// continues the record above it (traceback frames, multi-line messages), so
/// it takes that record's verdict. Mirrors the original's `_LineFilter`.
pub struct LineFilter {
    filters: LogFilters,
    carry: Option<bool>,
}

impl LineFilter {
    pub fn new(filters: LogFilters) -> Self {
        Self {
            filters,
            carry: None,
        }
    }

    pub fn test(&mut self, line: &str) -> bool {
        if parse_line_timestamp(line).is_some() {
            let verdict = self.filters.matches(line);
            self.carry = Some(verdict);
            verdict
        } else if let Some(carry) = self.carry {
            carry
        } else {
            // Before the first stamp: this record's time and level are
            // unknown. Drop it when a time/level filter is active.
            let timed = self.filters.since.is_some() || self.filters.min_level.is_some();
            !timed && self.filters.matches(line)
        }
    }
}

/// Read the last `num_lines` matching lines of `path`.
pub fn tail_lines(path: &Path, num_lines: usize, filters: &LogFilters) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read log file: {}", path.display()))?;
    let mut filter = LineFilter::new(filters.clone());
    let matched: Vec<String> = content
        .lines()
        .filter(|l| filter.test(l))
        .map(|l| l.to_string())
        .collect();
    let start = matched.len().saturating_sub(num_lines);
    Ok(matched[start..].to_vec())
}

/// Print every known log file with size and age (`hermes logs list`).
fn list_logs(home: &Path) -> Result<()> {
    println!("Available log files in {}:", home.join("logs").display());
    let mut any = false;
    for (name, filename) in LOG_FILES {
        let path = home.join("logs").join(filename);
        match std::fs::metadata(&path) {
            Ok(meta) => {
                any = true;
                let size = meta.len();
                let age = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map(human_duration)
                    .unwrap_or_else(|| "unknown age".to_string());
                println!("  {name:<8} {filename:<28} {size:>10} bytes  modified {age} ago");
            }
            Err(_) => {
                println!("  {name:<8} {filename:<28} (not created yet)");
            }
        }
    }
    if !any {
        println!("\nNo log files yet — logs are created when Hermes runs.");
    }
    Ok(())
}

fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Follow `path` in real time, printing new lines that pass the filters.
/// Stops on Ctrl+C. Handles truncation/rotation by resetting the read offset.
async fn follow_log(path: &Path, filters: &LogFilters) -> Result<()> {
    let mut filter = LineFilter::new(filters.clone());
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("Failed to open log file: {}", path.display()))?;
    let mut offset = file.seek(SeekFrom::End(0)).await?;
    let mut pending = String::new();

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("\n--- stopped ---");
                return Ok(());
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }

        let len = tokio::fs::metadata(path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        if len < offset {
            // Truncated or rotated: start over.
            offset = 0;
            pending.clear();
        }
        if len == offset {
            continue;
        }
        file.seek(SeekFrom::Start(offset)).await?;
        let mut buf = vec![0u8; (len - offset).min(1 << 20) as usize];
        let n = file.read(&mut buf).await?;
        if n == 0 {
            continue;
        }
        offset += n as u64;
        pending.push_str(&String::from_utf8_lossy(&buf[..n]));

        // Split off complete lines; keep the trailing partial line buffered.
        let mut owned: Vec<String> = pending.split('\n').map(|s| s.to_string()).collect();
        pending = owned.pop().unwrap_or_default();
        for line in &owned {
            if filter.test(line) {
                println!("{line}");
            }
        }
    }
}

/// Validate filter args and build [`LogFilters`].
fn build_filters(args: &LogsArgs) -> Result<LogFilters> {
    let min_level = match &args.level {
        Some(level) => {
            let upper = level.to_ascii_uppercase();
            let canon = canonical_level(&upper);
            if level_order(canon).is_none() {
                bail!("Invalid --level: {level:?}. Use DEBUG, INFO, WARNING, ERROR, or CRITICAL.");
            }
            Some(canon.to_string())
        }
        None => None,
    };

    let since = match &args.since {
        Some(since) => {
            let duration = parse_since(since).with_context(|| {
                format!("Invalid --since value: {since:?}. Use a format like '1h', '30m', '2d'.")
            })?;
            let now = chrono::Local::now().naive_local();
            Some(now - chrono::Duration::from_std(duration).unwrap_or(chrono::Duration::zero()))
        }
        None => None,
    };

    let component_prefixes = args
        .component
        .as_deref()
        .map(|c| vec![c.to_string()])
        .unwrap_or_default();

    Ok(LogFilters {
        min_level,
        session: args.session.clone(),
        since,
        component_prefixes,
    })
}

/// Handle `hermes logs [name] [-n N] [-f] [--level L] [--session S]
/// [--since T] [--component C]` and `hermes logs list`.
pub async fn run(args: &LogsArgs) -> Result<()> {
    let home = args
        .config_dir
        .clone()
        .unwrap_or_else(hermes_config::hermes_home);
    let name = args.log_name.as_deref().unwrap_or("agent").to_lowercase();

    if name == "list" {
        return list_logs(&home);
    }

    let Some(filename) = log_filename(&name) else {
        let available: Vec<&str> = LOG_FILES.iter().map(|(n, _)| *n).collect();
        bail!(
            "Unknown log: {name:?}. Available: {} (or 'list')",
            available.join(", ")
        );
    };
    let path = home.join("logs").join(filename);

    if !path.exists() {
        println!("Log file not found: {}", path.display());
        println!("(Logs are created when Hermes runs — try `hermes chat` first)");
        return Ok(());
    }

    let filters = build_filters(args)?;

    let lines = tail_lines(&path, args.lines as usize, &filters)?;

    let mut filter_parts = Vec::new();
    if let Some(l) = &filters.min_level {
        filter_parts.push(format!("level>={l}"));
    }
    if let Some(s) = &args.session {
        filter_parts.push(format!("session={s}"));
    }
    if let Some(c) = &args.component {
        filter_parts.push(format!("component={c}"));
    }
    if let Some(s) = &args.since {
        filter_parts.push(format!("since={s}"));
    }
    let filter_desc = if filter_parts.is_empty() {
        String::new()
    } else {
        format!(" [{}]", filter_parts.join(", "))
    };
    let mode = if args.follow {
        "Ctrl+C to stop".to_string()
    } else {
        format!("last {}", args.lines)
    };
    println!(
        "--- {}/{filename}{filter_desc} ({mode}) ---",
        home.display()
    );

    for line in &lines {
        println!("{line}");
    }

    if args.follow {
        follow_log(&path, &filters).await?;
    } else if lines.is_empty() {
        println!("(no matching lines)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_filename_resolves_known_names() {
        assert_eq!(log_filename("agent"), Some("agent.log"));
        assert_eq!(log_filename("errors"), Some("errors.log"));
        assert_eq!(log_filename("gateway"), Some("gateway.log"));
        assert_eq!(log_filename("handoff"), Some("desktop-update-handoff.log"));
        assert_eq!(log_filename("mcp"), Some("mcp-stderr.log"));
        assert_eq!(log_filename("hermes"), Some("hermes.log"));
        assert_eq!(log_filename("nope"), None);
    }

    #[test]
    fn parse_since_accepts_units() {
        assert_eq!(parse_since("45s"), Some(Duration::from_secs(45)));
        assert_eq!(parse_since("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_since("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_since("2d"), Some(Duration::from_secs(172800)));
        assert_eq!(parse_since(" 2H "), Some(Duration::from_secs(7200)));
    }

    #[test]
    fn parse_since_rejects_garbage() {
        assert_eq!(parse_since("yesterday"), None);
        assert_eq!(parse_since("1w"), None);
        assert_eq!(parse_since(""), None);
        assert_eq!(parse_since("h"), None);
    }

    #[test]
    fn extract_level_finds_token() {
        assert_eq!(
            extract_level("2026-10-23 02:00:00 INFO gateway.run: hello"),
            Some("INFO")
        );
        assert_eq!(
            extract_level("2026-10-23 02:00:00 [sess_1] ERROR tools.terminal: boom"),
            Some("ERROR")
        );
        assert_eq!(extract_level("no level here"), None);
    }

    #[test]
    fn extract_logger_name_finds_component() {
        assert_eq!(
            extract_logger_name("2026-10-23 02:00:00 INFO gateway.run: hello"),
            Some("gateway.run")
        );
        assert_eq!(
            extract_logger_name("2026-10-23 02:00:00 [sess_1] ERROR tools.terminal: boom"),
            Some("tools.terminal")
        );
        assert_eq!(extract_logger_name("plain line"), None);
    }

    #[test]
    fn parse_line_timestamp_handles_shapes() {
        assert!(parse_line_timestamp("2026-10-23 02:00:00 INFO x").is_some());
        assert!(parse_line_timestamp("2026-10-23T02:00:00+08:00 INFO x").is_some());
        assert!(parse_line_timestamp("no stamp here").is_none());
    }

    #[test]
    fn level_filter_drops_below_minimum() {
        let filters = LogFilters {
            min_level: Some("WARNING".to_string()),
            ..Default::default()
        };
        assert!(!filters.matches("2026-10-23 02:00:00 INFO gateway.run: hi"));
        assert!(filters.matches("2026-10-23 02:00:00 WARNING gateway.run: hi"));
        assert!(filters.matches("2026-10-23 02:00:00 ERROR gateway.run: hi"));
        // Lines without a level pass the level filter (LineFilter decides).
        assert!(filters.matches("continuation line"));
    }

    #[test]
    fn session_and_component_filters() {
        let filters = LogFilters {
            session: Some("abc123".to_string()),
            component_prefixes: vec!["tools".to_string()],
            ..Default::default()
        };
        assert!(filters.matches("2026-10-23 02:00:00 INFO [abc123] tools.terminal: ok"));
        assert!(!filters.matches("2026-10-23 02:00:00 INFO [other] tools.terminal: ok"));
        assert!(!filters.matches("2026-10-23 02:00:00 INFO [abc123] gateway.run: ok"));
    }

    #[test]
    fn since_filter_drops_old_stamped_lines() {
        let filters = LogFilters {
            since: Some(
                chrono::NaiveDate::from_ymd_opt(2026, 10, 23)
                    .unwrap()
                    .and_hms_opt(3, 0, 0)
                    .unwrap(),
            ),
            ..Default::default()
        };
        assert!(!filters.matches("2026-10-23 02:00:00 INFO x: old"));
        assert!(filters.matches("2026-10-23 04:00:00 INFO x: new"));
        assert!(filters.matches("unstamped line passes"));
    }

    #[test]
    fn line_filter_carries_verdict_to_continuation_lines() {
        let filters = LogFilters {
            min_level: Some("ERROR".to_string()),
            ..Default::default()
        };
        let mut f = LineFilter::new(filters);
        assert!(!f.test("2026-10-23 02:00:00 INFO app: start"));
        // Traceback frame continues the INFO record → dropped.
        assert!(!f.test("  File \"x.py\", line 1"));
        assert!(f.test("2026-10-23 02:00:01 ERROR app: boom"));
        // Continuation of the ERROR record → kept.
        assert!(f.test("Traceback (most recent call last):"));
    }

    #[test]
    fn tail_lines_reads_last_n_matching() {
        let dir = std::env::temp_dir().join("hermes-logs-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.log");
        let body: String = (0..10)
            .map(|i| format!("2026-10-23 02:00:{i:02} INFO app: line {i}\n"))
            .collect();
        std::fs::write(&path, body).unwrap();

        let filters = LogFilters::default();
        let lines = tail_lines(&path, 3, &filters).unwrap();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("line 7"));
        assert!(lines[2].contains("line 9"));

        let filtered = LogFilters {
            session: Some("line 8".to_string()),
            ..Default::default()
        };
        let lines = tail_lines(&path, 50, &filtered).unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("line 8"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_filters_rejects_bad_level_and_since() {
        let base = LogsArgs {
            log_name: None,
            lines: 50,
            follow: false,
            level: Some("VERBOSE".to_string()),
            session: None,
            since: None,
            component: None,
            config_dir: None,
        };
        assert!(build_filters(&base).is_err());

        let base = LogsArgs {
            level: Some("warn".to_string()),
            since: Some("tomorrow".to_string()),
            ..base
        };
        assert!(build_filters(&base).is_err());

        let base = LogsArgs {
            level: Some("warn".to_string()),
            since: Some("30m".to_string()),
            ..base
        };
        let filters = build_filters(&base).unwrap();
        assert_eq!(filters.min_level.as_deref(), Some("WARNING"));
        assert!(filters.since.is_some());
    }
}
