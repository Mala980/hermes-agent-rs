//! `hermes debug` — upload logs and system info for support.
//!
//! Ported from NousResearch/hermes-agent's `hermes_cli/debug.py`:
//! `hermes debug share` uploads a redacted debug report (the `hermes dump`
//! summary plus recent log tails) to paste.rs (falling back to dpaste.com)
//! and prints a shareable URL; `hermes debug delete <url>` removes a
//! previously uploaded paste.rs paste. The Nous-internal S3 upload
//! (`--nous`) has no counterpart in this fork and is refused honestly.

use hermes_config::hermes_home;
use hermes_intelligence::redact::Redactor;
use std::io::IsTerminal;
use std::time::Duration;

const PASTE_RS_URL: &str = "https://paste.rs/";
const DPASTE_COM_URL: &str = "https://dpaste.com/api/";
/// Per-log-file upload cap (paste.rs caps at ~1 MB).
const MAX_LOG_BYTES: usize = 512_000;
const REDACTION_BANNER: &str =
    "[This report was automatically redacted: secrets were masked before upload.]\n\n";

/// CLI args for `hermes debug`.
#[derive(clap::Args, Debug, Clone)]
pub struct DebugArgs {
    #[command(subcommand)]
    pub command: Option<DebugCommand>,
}

/// `hermes debug` subcommands.
#[derive(Clone, clap::Subcommand, Debug)]
pub enum DebugCommand {
    /// Upload a debug report to a paste service and print a shareable URL.
    Share(ShareArgs),
    /// Delete a paste previously uploaded by `hermes debug share`.
    Delete {
        /// One or more paste URLs to delete (e.g. https://paste.rs/abc123).
        urls: Vec<String>,
    },
}

/// Args for `hermes debug share`.
#[derive(clap::Args, Debug, Clone)]
pub struct ShareArgs {
    /// Number of log lines to include per log file.
    #[arg(long, default_value_t = 200)]
    pub lines: usize,
    /// dpaste.com fallback retention in days (minimum 1). paste.rs pastes
    /// always auto-delete after ~6 hours.
    #[arg(long, default_value_t = 1)]
    pub expire: u32,
    /// Print the report locally instead of uploading.
    #[arg(long)]
    pub local: bool,
    /// Skip the confirmation prompt and upload immediately. Required in
    /// non-interactive contexts; without it and no TTY, the command refuses
    /// rather than uploading silently.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Disable upload-time secret redaction (default: redact).
    #[arg(long)]
    pub no_redact: bool,
    /// Upload to Nous-internal storage (not available in this fork).
    #[arg(long)]
    pub nous: bool,
}

/// Last `n` lines of `text`, capped at [`MAX_LOG_BYTES`] on a line boundary.
pub fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    let mut out = lines[start..].join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    let bytes = out.as_bytes();
    if bytes.len() > MAX_LOG_BYTES {
        // Cut back to the first complete line within the budget.
        let cut = MAX_LOG_BYTES;
        let mut end = cut;
        while end > 0 && bytes[end - 1] != b'\n' {
            end -= 1;
        }
        out = String::from_utf8_lossy(&bytes[..end.max(1).min(bytes.len())]).into_owned();
    }
    out
}

/// Build the upload-ready debug report: dump summary + log tails.
pub fn collect_debug_report(log_lines: usize, redact: bool, dump_text: Option<String>) -> String {
    let redactor = Redactor::new();
    let mut report = String::new();
    let dump = dump_text.unwrap_or_else(|| super::dump_cmd::collect_dump_summary(false));
    report.push_str(&dump);
    report.push('\n');

    let logs_dir = hermes_home().join("logs");
    let mut log_files: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&logs_dir) {
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().map(|e| e == "log").unwrap_or(false) {
                log_files.push(path);
            }
        }
    }
    log_files.sort();
    for path in log_files {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let tail = tail_lines(&content, log_lines);
        let tail = if redact { redactor.redact(&tail) } else { tail };
        report.push_str(&format!(
            "\n--- {name} (last {log_lines} lines) ---\n{tail}\n"
        ));
    }
    if redact {
        format!("{REDACTION_BANNER}{report}")
    } else {
        report
    }
}

/// Paste ID from a paste.rs URL.
fn extract_paste_id(url: &str) -> Option<&str> {
    let url = url.trim();
    for prefix in ["https://paste.rs/", "http://paste.rs/"] {
        if let Some(id) = url.strip_prefix(prefix) {
            let id = id.trim_end_matches('/');
            if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Some(id);
            }
        }
    }
    None
}

fn is_dpaste_url(url: &str) -> bool {
    let url = url.trim();
    url.starts_with("https://dpaste.com/") || url.starts_with("http://dpaste.com/")
}

fn post_paste(
    service: &str,
    endpoint: &str,
    body: &[u8],
    content_type: &str,
) -> anyhow::Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let resp = client
        .post(endpoint)
        .header("Content-Type", content_type)
        .body(body.to_vec())
        .send()
        .map_err(|e| anyhow::anyhow!("{service} upload failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "{service} upload failed: HTTP {}",
            resp.status()
        ));
    }
    Ok(resp.text()?.trim().to_string())
}

fn upload_paste_rs(content: &str) -> anyhow::Result<String> {
    post_paste(
        "paste.rs",
        PASTE_RS_URL,
        content.as_bytes(),
        "text/plain; charset=utf-8",
    )
}

fn upload_dpaste_com(content: &str, expiry_days: u32) -> anyhow::Result<String> {
    let expiry_days = expiry_days.max(1).to_string();
    let boundary = "hermes-debug-boundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"content\"\r\n\r\n{content}\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"expiry_days\"\r\n\r\n{expiry_days}\r\n\
         --{boundary}--\r\n"
    );
    post_paste(
        "dpaste.com",
        DPASTE_COM_URL,
        body.as_bytes(),
        &format!("multipart/form-data; boundary={boundary}"),
    )
}

/// Upload to paste.rs, falling back to dpaste.com. Returns the shareable URL.
fn upload_to_pastebin(content: &str, expiry_days: u32) -> anyhow::Result<(String, &'static str)> {
    match upload_paste_rs(content) {
        Ok(url) => Ok((url, "paste.rs")),
        Err(first_err) => match upload_dpaste_com(content, expiry_days) {
            Ok(url) => Ok((url, "dpaste.com")),
            Err(_) => Err(first_err),
        },
    }
}

fn confirm_upload() -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    print!("Upload this debug report to a public paste service? [y/N]: ");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    match std::io::stdin().read_line(&mut input) {
        Ok(_) => matches!(input.trim().to_lowercase().as_str(), "y" | "yes"),
        Err(_) => false,
    }
}

async fn run_share(args: &ShareArgs) -> anyhow::Result<()> {
    if args.nous {
        println!(
            "The --nous upload target (Nous-internal S3 storage) is not available in this fork.\n\
             Omit --nous to upload to the public paste service, or use --local to print the report."
        );
        return Ok(());
    }
    let redact = !args.no_redact;
    let report = collect_debug_report(args.lines, redact, None);
    if args.local {
        println!("{report}");
        return Ok(());
    }
    if !args.yes && !confirm_upload() {
        if std::io::stdin().is_terminal() {
            println!("Aborted.");
        } else {
            println!(
                "Refusing to upload without confirmation in a non-interactive context.\n\
                 Re-run with `hermes debug share --yes` to upload."
            );
        }
        return Ok(());
    }
    match upload_to_pastebin(&report, args.expire) {
        Ok((url, service)) => {
            println!("Debug report uploaded via {service}:");
            println!("  {url}");
            if service == "paste.rs" {
                println!("  Pastes auto-delete after ~6 hours.");
                println!("  Delete sooner with: hermes debug delete {url}");
            } else {
                println!(
                    "  dpaste.com fallback pastes are kept for {} day(s) and cannot be deleted.",
                    args.expire.max(1)
                );
            }
        }
        Err(e) => {
            println!("Upload failed: {e}");
            println!("Use `hermes debug share --local` to print the report instead.");
        }
    }
    Ok(())
}

async fn run_delete(urls: &[String]) -> anyhow::Result<()> {
    if urls.is_empty() {
        println!("Usage: hermes debug delete <url> [url...]");
        return Ok(());
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    for url in urls {
        if is_dpaste_url(url) {
            println!(
                "Cannot delete {url}: dpaste.com pastes cannot be deleted (anonymous pastes)."
            );
            continue;
        }
        match extract_paste_id(url) {
            Some(id) => {
                let endpoint = format!("{PASTE_RS_URL}{id}");
                match client.delete(&endpoint).send() {
                    Ok(resp) if resp.status().is_success() => {
                        println!("Deleted {url}");
                    }
                    Ok(resp) => {
                        println!("Failed to delete {url}: HTTP {}", resp.status());
                    }
                    Err(e) => println!("Failed to delete {url}: {e}"),
                }
            }
            None => println!("Cannot delete {url}: only paste.rs URLs are supported."),
        }
    }
    Ok(())
}

/// Dispatch `hermes debug`.
pub async fn run(args: &DebugArgs) -> anyhow::Result<()> {
    match &args.command {
        Some(DebugCommand::Share(share)) => run_share(share).await,
        Some(DebugCommand::Delete { urls }) => run_delete(urls).await,
        None => {
            println!("Usage:");
            println!("  hermes debug share [--lines N] [--expire D] [--local] [-y] [--no-redact]");
            println!("  hermes debug delete <url> [url...]");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_lines_returns_last_n_lines() {
        let text = "a\nb\nc\nd\ne\n";
        assert_eq!(tail_lines(text, 2), "d\ne\n");
        assert_eq!(tail_lines(text, 99), text);
        assert_eq!(tail_lines("", 5), "");
    }

    #[test]
    fn tail_lines_respects_byte_cap_on_line_boundary() {
        let line = "x".repeat(100);
        let text = (0..10_000)
            .map(|_| line.clone())
            .collect::<Vec<_>>()
            .join("\n");
        let tail = tail_lines(&text, 10_000);
        assert!(tail.len() <= MAX_LOG_BYTES);
        assert!(tail.ends_with('\n'));
        assert!(tail.lines().all(|l| l.len() == 100));
    }

    #[test]
    fn extract_paste_id_parses_paste_rs_urls() {
        assert_eq!(extract_paste_id("https://paste.rs/abc123"), Some("abc123"));
        assert_eq!(
            extract_paste_id("  http://paste.rs/xyz789/ "),
            Some("xyz789")
        );
        assert_eq!(extract_paste_id("https://dpaste.com/abc"), None);
        assert_eq!(extract_paste_id("https://paste.rs/"), None);
        assert_eq!(extract_paste_id("not a url"), None);
    }

    #[test]
    fn is_dpaste_url_detects_fallback_host() {
        assert!(is_dpaste_url("https://dpaste.com/abc123"));
        assert!(!is_dpaste_url("https://paste.rs/abc123"));
    }

    #[test]
    fn debug_report_contains_dump_header_and_banner() {
        let report = collect_debug_report(
            10,
            true,
            Some("--- hermes dump ---\ntest\n--- end dump ---".to_string()),
        );
        assert!(report.contains("--- hermes dump ---"));
        assert!(report.contains("automatically redacted"));
    }

    #[test]
    fn debug_report_redaction_masks_secrets() {
        let redactor = Redactor::new();
        let leaked = "api_key=sk-ant-real-secret-value-here-12345";
        let redacted = redactor.redact(leaked);
        assert!(!redacted.contains("sk-ant-real-secret-value-here-12345"));
    }
}
