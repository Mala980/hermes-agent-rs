//! Shared platform helper functions.
//!
//! Common text manipulation utilities used across platform adapters.

use regex::Regex;

/// Split a long message into chunks that respect word and sentence boundaries.
///
/// Prefers breaking at sentence endings (`. `, `! `, `? `), then at newlines,
/// then at word boundaries (spaces), and only hard-splits as a last resort.
pub fn split_long_message(text: &str, max_len: usize) -> Vec<String> {
    if max_len == 0 {
        return vec![text.to_string()];
    }
    if text.len() <= max_len {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            chunks.push(remaining.to_string());
            break;
        }

        let window = &remaining[..max_len];

        // Try sentence boundary first
        let break_at = find_last_sentence_break(window)
            .or_else(|| window.rfind('\n'))
            .or_else(|| window.rfind(' '))
            .unwrap_or(max_len);

        let break_at = if break_at == 0 { max_len } else { break_at };

        chunks.push(remaining[..break_at].trim_end().to_string());
        remaining = remaining[break_at..].trim_start();
    }

    chunks
}

fn find_last_sentence_break(text: &str) -> Option<usize> {
    let terminators = [". ", "! ", "? ", ".\n", "!\n", "?\n"];
    terminators
        .iter()
        .filter_map(|t| text.rfind(t).map(|i| i + t.len()))
        .max()
}

/// Escape Markdown special characters.
pub fn escape_markdown(text: &str) -> String {
    const SPECIAL_CHARS: &[char] = &[
        '\\', '`', '*', '_', '{', '}', '[', ']', '(', ')', '#', '+', '-', '.', '!', '|', '~', '>',
    ];

    let mut result = String::with_capacity(text.len() + text.len() / 8);
    for ch in text.chars() {
        if SPECIAL_CHARS.contains(&ch) {
            result.push('\\');
        }
        result.push(ch);
    }
    result
}

/// Truncate text to `max_len` characters, appending an ellipsis if truncated.
pub fn truncate_with_ellipsis(text: &str, max_len: usize) -> String {
    if max_len < 4 {
        return text.chars().take(max_len).collect();
    }
    if text.len() <= max_len {
        return text.to_string();
    }

    let truncated = &text[..text.floor_char_boundary(max_len - 3)];
    // Try to break at a word boundary
    let break_at = truncated.rfind(' ').unwrap_or(truncated.len());
    format!("{}...", &truncated[..break_at])
}

/// Extract all URLs from text.
pub fn extract_urls(text: &str) -> Vec<String> {
    let re = Regex::new(r"https?://[^\s<>\[\](){}]+").expect("valid regex");
    re.find_iter(text).map(|m| m.as_str().to_string()).collect()
}

/// Format a code block with optional language tag.
pub fn format_code_block(code: &str, lang: Option<&str>) -> String {
    match lang {
        Some(l) if !l.is_empty() => format!("```{}\n{}\n```", l, code),
        _ => format!("```\n{}\n```", code),
    }
}

/// Sanitize HTML by stripping tags, keeping only text content.
pub fn sanitize_html(text: &str) -> String {
    let re = Regex::new(r"<[^>]+>").expect("valid regex");
    let cleaned = re.replace_all(text, "");
    // Decode common HTML entities
    cleaned
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// Estimate reading time in seconds for the given text.
///
/// Assumes an average reading speed of 200 words per minute.
pub fn estimate_read_time(text: &str) -> u32 {
    let word_count = text.split_whitespace().count() as f64;
    let minutes = word_count / 200.0;
    (minutes * 60.0).ceil() as u32
}

/// Detect MIME type from a file extension.
pub fn mime_from_extension(ext: &str) -> &'static str {
    match ext.to_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "pdf" => "application/pdf",
        "doc" | "docx" => "application/msword",
        "xls" | "xlsx" => "application/vnd.ms-excel",
        "zip" => "application/zip",
        "json" => "application/json",
        "xml" => "application/xml",
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "csv" => "text/csv",
        _ => "application/octet-stream",
    }
}

/// Determine the file's media category from its extension.
pub fn media_category(ext: &str) -> &'static str {
    match ext.to_lowercase().as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "svg" | "bmp" | "tiff" => "image",
        "mp4" | "webm" | "mov" | "avi" | "mkv" => "video",
        "mp3" | "ogg" | "oga" | "wav" | "flac" | "aac" | "m4a" => "audio",
        _ => "document",
    }
}

// ── HTTP / webhook helpers ──

use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Send an HTTP request, retrying once after a 429 rate-limit response.
pub async fn send_with_rate_limit_retry<F, Fut>(
    send_request: F,
) -> Result<reqwest::Response, reqwest::Error>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    let resp = send_request().await?;
    if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        return send_request().await;
    }
    Ok(resp)
}

/// Constant-time byte-slice comparison (for HMAC verification).
pub fn constant_time_eq<A: AsRef<[u8]>, B: AsRef<[u8]>>(a: A, b: B) -> bool {
    let a = a.as_ref();
    let b = b.as_ref();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Parse a URL query string or `application/x-www-form-urlencoded` body
/// into key/value pairs, percent-decoding keys and values (`+` → space).
pub fn parse_http_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((
                String::from_utf8_lossy(&http_url_decode(k)).into_owned(),
                String::from_utf8_lossy(&http_url_decode(v)).into_owned(),
            ))
        })
        .collect()
}

/// Percent-decode a URL-encoded string.
pub fn http_url_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    out
}

/// Read a minimal HTTP/1.1 request from a TCP stream.
/// Returns `(request_line, headers, body)`.
pub async fn read_http_request(
    stream: &mut tokio::net::TcpStream,
    max_bytes: usize,
) -> Result<(String, HashMap<String, String>, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > max_bytes {
            return Err("request too large".into());
        }
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(buf.len());
    let header_str = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = header_str.lines();
    let request_line = lines.next().unwrap_or("").to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
        if body.len() > max_bytes {
            return Err("body too large".into());
        }
    }
    body.truncate(content_length);
    Ok((request_line, headers, body))
}

/// Write a minimal HTTP/1.1 response to a TCP stream.
pub async fn write_http_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        reason,
        content_type,
        body.len(),
        body
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Verify an HMAC-SHA256 webhook signature (`sha256=<hex>` format).
///
/// The `sha256=` prefix is REQUIRED: a bare-hex header is rejected, since
/// Meta (and our own webhook docs) always send the prefixed form and
/// silently accepting unprefixed input weakens format strictness.
pub fn verify_hmac_sha256_signature(secret: &str, body: &[u8], header: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let hex = match header.strip_prefix("sha256=") {
        Some(h) => h,
        None => return false,
    };
    let expected = match hex_decode(hex) {
        Some(v) => v,
        None => return false,
    };
    let mut mac = match Hmac::<Sha256>::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    let result = mac.finalize().into_bytes();
    constant_time_eq(&result[..], &expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_long_message_short() {
        let chunks = split_long_message("hello world", 100);
        assert_eq!(chunks, vec!["hello world"]);
    }

    #[test]
    fn test_split_long_message_sentence_break() {
        let text = "First sentence. Second sentence. Third sentence is long.";
        let chunks = split_long_message(text, 35);
        assert!(chunks.len() >= 2);
        assert!(chunks[0].ends_with('.'));
    }

    #[test]
    fn test_escape_markdown() {
        assert_eq!(escape_markdown("hello *world*"), "hello \\*world\\*");
        assert_eq!(escape_markdown("no_special"), "no\\_special");
    }

    #[test]
    fn test_truncate_with_ellipsis() {
        assert_eq!(truncate_with_ellipsis("short", 100), "short");
        let result = truncate_with_ellipsis("this is a long sentence that should be truncated", 20);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 20);
    }

    #[test]
    fn test_extract_urls() {
        let text = "Visit https://example.com and http://foo.bar/baz for more.";
        let urls = extract_urls(text);
        assert_eq!(urls.len(), 2);
        assert!(urls[0].starts_with("https://"));
    }

    #[test]
    fn test_format_code_block() {
        assert_eq!(
            format_code_block("let x = 1;", Some("rust")),
            "```rust\nlet x = 1;\n```"
        );
        assert_eq!(format_code_block("hello", None), "```\nhello\n```");
    }

    #[test]
    fn test_sanitize_html() {
        assert_eq!(
            sanitize_html("<b>bold</b> &amp; <i>italic</i>"),
            "bold & italic"
        );
    }

    #[test]
    fn test_estimate_read_time() {
        let words_200: String = (0..200).map(|_| "word").collect::<Vec<_>>().join(" ");
        let time = estimate_read_time(&words_200);
        assert_eq!(time, 60);
    }

    #[test]
    fn test_mime_from_extension() {
        assert_eq!(mime_from_extension("png"), "image/png");
        assert_eq!(mime_from_extension("mp4"), "video/mp4");
        assert_eq!(mime_from_extension("xyz"), "application/octet-stream");
    }

    #[test]
    fn test_media_category() {
        assert_eq!(media_category("jpg"), "image");
        assert_eq!(media_category("mp4"), "video");
        assert_eq!(media_category("mp3"), "audio");
        assert_eq!(media_category("pdf"), "document");
    }
}
