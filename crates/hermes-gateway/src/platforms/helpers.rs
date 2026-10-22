//! Shared platform helper functions.
//!
//! Common text manipulation utilities used across platform adapters.

use std::time::Duration;

use hmac::{Hmac, Mac};
use regex::Regex;
use sha2::Sha256;

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

// ---------------------------------------------------------------------------
// Webhook signature verification (HMAC-SHA256, fail-closed)
// ---------------------------------------------------------------------------

/// Verify a webhook signature header of the form `sha256=<hex>` (e.g. Meta's
/// `X-Hub-Signature-256`) against the HMAC-SHA256 of the raw request body.
///
/// The comparison is constant-time via `hmac::Mac::verify` (never `==`).
/// Returns `false` when the header lacks the `sha256=` prefix, is not valid
/// 64-char hex, or the MAC does not match.
pub fn verify_hmac_sha256_signature(secret: &str, body: &[u8], header: &str) -> bool {
    let hex = match header.strip_prefix("sha256=") {
        Some(h) => h,
        None => return false,
    };
    let expected = match hex_decode_32(hex) {
        Some(b) => b,
        None => return false,
    };
    let mut mac = match Hmac::<Sha256>::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    mac.verify((&expected).into()).is_ok()
}

/// Constant-time string comparison (no early-exit byte comparison), for
/// webhook verify-token checks.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// Minimal HTTP server helpers for inbound webhook receivers
// ---------------------------------------------------------------------------

/// One parsed HTTP request: request line, lowercased header pairs, raw body.
pub type HttpRequest = (String, Vec<(String, String)>, Vec<u8>);

/// Read one HTTP request from `stream`: header block then `Content-Length`
/// body bytes, bounded by `max_body_bytes`.
pub async fn read_http_request(
    stream: &mut tokio::net::TcpStream,
    max_body_bytes: usize,
) -> Result<HttpRequest, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::AsyncReadExt;

    let mut head = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err("connection closed".into());
        }
        head.extend_from_slice(&chunk[..n]);
        if head.len() > 65536 {
            return Err("request header too large".into());
        }
        if head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let header_end = head
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(head.len());
    let header_text = String::from_utf8_lossy(&head[..header_end]).to_string();
    let mut lines = header_text.lines();
    let request_line = lines.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_lowercase(), v.trim().to_string()));
        }
    }
    let content_length: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0)
        .min(max_body_bytes);
    let mut body = head[header_end..].to_vec();
    body.truncate(content_length);
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        let take = (content_length - body.len()).min(n);
        body.extend_from_slice(&chunk[..take]);
    }
    Ok((request_line, headers, body))
}

/// Write a minimal HTTP response on `stream`.
pub async fn write_http_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::AsyncWriteExt;
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

/// Parse a URL query string into key/value pairs.
pub fn parse_http_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((http_url_decode(k), http_url_decode(v)))
        })
        .collect()
}

/// Minimal percent-decoding for query parameters.
pub fn http_url_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut bytes = s.as_bytes().iter();
    while let Some(&b) = bytes.next() {
        if b == b'%' {
            let hi = bytes.next().copied().unwrap_or(b'0');
            let lo = bytes.next().copied().unwrap_or(b'0');
            let hex = |c: u8| match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => 0,
            };
            out.push((hex(hi) << 4 | hex(lo)) as char);
        } else if b == b'+' {
            out.push(' ');
        } else {
            out.push(b as char);
        }
    }
    out
}

/// Decode a 64-char hex string into 32 bytes; `None` when malformed.
fn hex_decode_32(s: &str) -> Option<[u8; 32]> {
    let bytes = s.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = hex_nibble(bytes[2 * i])?;
        let lo = hex_nibble(bytes[2 * i + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod hmac_tests {
    use super::*;

    /// RFC 4231 test case 2 (HMAC-SHA-256): key "Jefe",
    /// data "what do ya want for nothing?".
    const RFC4231_SHA256_TC2: &str =
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";

    fn signed_header(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let out = mac.finalize().into_bytes();
        let mut hex = String::with_capacity(64);
        for b in out {
            use std::fmt::Write;
            let _ = write!(hex, "{b:02x}");
        }
        format!("sha256={hex}")
    }

    #[test]
    fn hmac_sha256_rfc4231_known_answer() {
        let header = format!("sha256={RFC4231_SHA256_TC2}");
        assert!(verify_hmac_sha256_signature(
            "Jefe",
            b"what do ya want for nothing?",
            &header
        ));
    }

    #[test]
    fn hmac_sha256_rejects_wrong_signature() {
        let mut bad = String::from("sha256=");
        bad.push_str(&"00".repeat(32));
        assert!(!verify_hmac_sha256_signature(
            "Jefe",
            b"what do ya want for nothing?",
            &bad
        ));
        // Tampered body against a genuine header.
        let header = signed_header("secret", b"original body");
        assert!(!verify_hmac_sha256_signature(
            "secret",
            b"tampered body",
            &header
        ));
        // Tampered secret against a genuine header.
        let header = signed_header("secret", b"body");
        assert!(!verify_hmac_sha256_signature(
            "other-secret",
            b"body",
            &header
        ));
    }

    #[test]
    fn hmac_sha256_rejects_malformed_headers() {
        assert!(!verify_hmac_sha256_signature("s", b"b", ""));
        assert!(!verify_hmac_sha256_signature("s", b"b", "not-a-signature"));
        // Missing sha256= prefix.
        assert!(!verify_hmac_sha256_signature("s", b"b", &"ab".repeat(32)));
        // Too short / too long hex.
        assert!(!verify_hmac_sha256_signature("s", b"b", "sha256=abcd"));
        assert!(!verify_hmac_sha256_signature(
            "s",
            b"b",
            &format!("sha256={}", "ab".repeat(33))
        ));
        // Non-hex chars.
        assert!(!verify_hmac_sha256_signature(
            "s",
            b"b",
            &format!("sha256={}", "zz".repeat(32))
        ));
    }

    #[test]
    fn hmac_sha256_accepts_uppercase_hex() {
        let header = format!("sha256={}", RFC4231_SHA256_TC2.to_uppercase());
        assert!(verify_hmac_sha256_signature(
            "Jefe",
            b"what do ya want for nothing?",
            &header
        ));
    }
}

// ---------------------------------------------------------------------------
// Rate-limit (HTTP 429) handling
// ---------------------------------------------------------------------------

/// Maximum time to wait for a single rate-limit backoff.
pub const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);

/// Fallback wait when a 429 response carries no usable `retry_after` info.
pub const DEFAULT_RETRY_WAIT: Duration = Duration::from_secs(5);

/// Extract a rate-limit wait from a `Retry-After` header map.
///
/// Handles delta-seconds (integer or float) and the IMF-fixdate HTTP-date
/// form (`Wed, 21 Oct 2015 07:28:00 GMT`), which is converted to a duration
/// relative to now (saturating at zero). Returns `None` when the header is
/// missing or unparseable.
pub fn retry_after_from_headers(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    parse_retry_after_value(value)
}

/// Extract a rate-limit wait from a JSON error body.
///
/// Handles the shapes used by the platforms in this crate:
/// - `{"retry_after": <secs>}` — Discord (seconds, may be fractional or a numeric string)
/// - `{"retry_after_ms": <ms>}` — Matrix `M_LIMIT_EXCEEDED`
/// - `{"parameters": {"retry_after": <secs>}}` — Telegram
///
/// Returns `None` when the body is not JSON or carries no usable field.
pub fn retry_after_from_body(body: &str) -> Option<Duration> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    // Telegram nests the field under `parameters`.
    let nested = v.get("parameters").and_then(|p| p.get("retry_after"));
    if let Some(d) = nested.and_then(|n| retry_after_number(n, false)) {
        return Some(d);
    }
    if let Some(d) = v
        .get("retry_after")
        .and_then(|n| retry_after_number(n, false))
    {
        return Some(d);
    }
    if let Some(d) = v
        .get("retry_after_ms")
        .and_then(|n| retry_after_number(n, true))
    {
        return Some(d);
    }
    None
}

/// Determine how long to wait from a rate-limited (`429`) response.
///
/// Checks the `Retry-After` header first, then JSON body shapes; the result
/// is capped at [`MAX_RETRY_WAIT`]. When the response is a 429 but carries no
/// usable wait info, falls back to [`DEFAULT_RETRY_WAIT`].
///
/// Returns `None` when the response status is not 429.
pub async fn retry_after_duration(resp: reqwest::Response) -> Option<Duration> {
    if resp.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
    if let Some(wait) = retry_after_from_headers(resp.headers()) {
        return Some(wait.min(MAX_RETRY_WAIT));
    }
    // `resp` is consumed here; the borrow of its headers above has ended.
    let body = resp.text().await.unwrap_or_default();
    let wait = retry_after_from_body(&body).unwrap_or(DEFAULT_RETRY_WAIT);
    Some(wait.min(MAX_RETRY_WAIT))
}

/// Perform an HTTP request, retrying ONCE after a rate-limit (`429`) response.
///
/// The wait is extracted with [`retry_after_duration`] (capped at
/// [`MAX_RETRY_WAIT`]). The retried response is returned as-is — a second
/// 429 is surfaced to the caller, never retried again.
///
/// `send` is a closure (not a pre-built request) because a
/// `reqwest::RequestBuilder` is consumed by `send()`.
pub async fn send_with_rate_limit_retry<F, Fut>(
    send: F,
) -> Result<reqwest::Response, reqwest::Error>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    let resp = send().await?;
    if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let wait = retry_after_duration(resp)
            .await
            .unwrap_or(DEFAULT_RETRY_WAIT);
        tracing::warn!(
            wait_secs = wait.as_secs(),
            "Rate limited (HTTP 429); backing off and retrying once"
        );
        tokio::time::sleep(wait).await;
        return send().await;
    }
    Ok(resp)
}

/// Parse a `Retry-After` header value: delta-seconds or an HTTP date.
fn parse_retry_after_value(value: &str) -> Option<Duration> {
    let v = value.trim();
    if let Ok(secs) = v.parse::<f64>() {
        if secs.is_finite() {
            return Some(Duration::from_secs_f64(secs.max(0.0)));
        }
    }
    // IMF-fixdate, e.g. "Wed, 21 Oct 2015 07:28:00 GMT".
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(v, "%a, %d %b %Y %H:%M:%S GMT") {
        let target = dt.and_utc().timestamp();
        let now = chrono::Utc::now().timestamp();
        return Some(Duration::from_secs((target - now).max(0) as u64));
    }
    None
}

/// Convert a JSON `retry_after` value (number or numeric string) to a
/// `Duration`; `millis` selects millisecond interpretation.
fn retry_after_number(v: &serde_json::Value, millis: bool) -> Option<Duration> {
    let f = match v {
        serde_json::Value::Number(n) => n.as_f64()?,
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if !f.is_finite() || f < 0.0 {
        return None;
    }
    let secs = if millis { f / 1000.0 } else { f };
    Some(Duration::from_secs_f64(secs))
}

#[cfg(test)]
mod rate_limit_tests {
    use super::*;
    use reqwest::header::HeaderMap;

    fn headers_with_retry_after(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            reqwest::header::RETRY_AFTER,
            value.parse().expect("valid header value"),
        );
        h
    }

    #[test]
    fn retry_after_header_integer_seconds() {
        let d = retry_after_from_headers(&headers_with_retry_after("5"));
        assert_eq!(d, Some(Duration::from_secs(5)));
    }

    #[test]
    fn retry_after_header_fractional_seconds() {
        let d = retry_after_from_headers(&headers_with_retry_after("1.5"));
        assert_eq!(d, Some(Duration::from_secs_f64(1.5)));
    }

    #[test]
    fn retry_after_header_http_date() {
        // 120 seconds in the future, formatted as an IMF-fixdate.
        let future = chrono::Utc::now() + chrono::Duration::seconds(120);
        let value = future.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let d = retry_after_from_headers(&headers_with_retry_after(&value))
            .expect("HTTP-date Retry-After should parse");
        assert!(
            d >= Duration::from_secs(110) && d <= Duration::from_secs(125),
            "unexpected wait: {d:?}"
        );
    }

    #[test]
    fn retry_after_header_missing_or_invalid() {
        assert_eq!(retry_after_from_headers(&HeaderMap::new()), None);
        assert_eq!(
            retry_after_from_headers(&headers_with_retry_after("soon")),
            None
        );
        assert_eq!(
            retry_after_from_headers(&headers_with_retry_after("")),
            None
        );
    }

    #[test]
    fn retry_after_body_discord_shape() {
        let d =
            retry_after_from_body(r#"{"message":"slow down","retry_after":1.24,"global":false}"#);
        assert_eq!(d, Some(Duration::from_secs_f64(1.24)));
    }

    #[test]
    fn retry_after_body_telegram_shape() {
        let d = retry_after_from_body(
            r#"{"ok":false,"error_code":429,"parameters":{"retry_after":7}}"#,
        );
        assert_eq!(d, Some(Duration::from_secs(7)));
    }

    #[test]
    fn retry_after_body_matrix_millis_shape() {
        let d = retry_after_from_body(
            r#"{"errcode":"M_LIMIT_EXCEEDED","error":"too fast","retry_after_ms":2500}"#,
        );
        assert_eq!(d, Some(Duration::from_millis(2500)));
    }

    #[test]
    fn retry_after_body_numeric_string() {
        let d = retry_after_from_body(r#"{"retry_after":"3"}"#);
        assert_eq!(d, Some(Duration::from_secs(3)));
    }

    #[test]
    fn retry_after_body_garbage() {
        assert_eq!(retry_after_from_body("not json"), None);
        assert_eq!(retry_after_from_body(r#"{"ok":true}"#), None);
        assert_eq!(retry_after_from_body(r#"{"retry_after":-2}"#), None);
    }

    // Async tests below exercise `retry_after_duration` and
    // `send_with_rate_limit_retry` against a mock HTTP server.

    async fn mock_server() -> wiremock::MockServer {
        wiremock::MockServer::start().await
    }

    #[tokio::test]
    async fn retry_after_duration_non_429_is_none() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = mock_server().await;
        Mock::given(method("GET"))
            .and(path("/ok"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let resp = reqwest::Client::new()
            .get(format!("{}/ok", server.uri()))
            .send()
            .await
            .unwrap();
        assert_eq!(retry_after_duration(resp).await, None);
    }

    #[tokio::test]
    async fn retry_after_duration_caps_at_60s() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = mock_server().await;
        Mock::given(method("GET"))
            .and(path("/limited"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "3600")
                    .set_body_string(r#"{"retry_after": 3600}"#),
            )
            .mount(&server)
            .await;

        let resp = reqwest::Client::new()
            .get(format!("{}/limited", server.uri()))
            .send()
            .await
            .unwrap();
        // Header wins over body here; both exceed the cap.
        assert_eq!(retry_after_duration(resp).await, Some(MAX_RETRY_WAIT));
    }

    #[tokio::test]
    async fn retry_after_duration_uses_body_when_no_header() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = mock_server().await;
        Mock::given(method("GET"))
            .and(path("/limited"))
            .respond_with(
                ResponseTemplate::new(429)
                    .set_body_string(r#"{"retry_after": 9, "global": false}"#),
            )
            .mount(&server)
            .await;

        let resp = reqwest::Client::new()
            .get(format!("{}/limited", server.uri()))
            .send()
            .await
            .unwrap();
        assert_eq!(
            retry_after_duration(resp).await,
            Some(Duration::from_secs(9))
        );
    }

    #[tokio::test]
    async fn send_with_rate_limit_retry_retries_once_then_succeeds() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = mock_server().await;
        let url = format!("{}/send", server.uri());

        // The one-shot 429 mock is mounted FIRST: wiremock gives precedence
        // to the earliest-mounted matching mock, so the 429 wins for the
        // first request only and the fallback serves the retry.
        Mock::given(method("POST"))
            .and(path("/send"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "0")
                    .set_body_string(r#"{"retry_after": 0}"#),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/send"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let resp = send_with_rate_limit_retry(|| client.post(&url).body("{}").send())
            .await
            .expect("request should succeed after retry");
        assert_eq!(resp.status(), 200);

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 2, "expected exactly one retry");
    }

    #[tokio::test]
    async fn send_with_rate_limit_retry_gives_up_after_one_retry() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = mock_server().await;
        let url = format!("{}/send", server.uri());

        Mock::given(method("POST"))
            .and(path("/send"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "0")
                    .set_body_string(r#"{"retry_after": 0}"#),
            )
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let resp = send_with_rate_limit_retry(|| client.post(&url).body("{}").send())
            .await
            .expect("transport should not fail");
        // Second 429 is returned as-is, not retried again.
        assert_eq!(resp.status(), 429);

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 2, "expected exactly one retry");
    }

    #[tokio::test]
    async fn send_with_rate_limit_retry_passes_through_success() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = mock_server().await;
        let url = format!("{}/send", server.uri());

        Mock::given(method("POST"))
            .and(path("/send"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let resp = send_with_rate_limit_retry(|| client.post(&url).body("{}").send())
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1, "no retry expected on success");
    }
}

#[cfg(test)]
mod webhook_http_tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("", "a"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn parse_http_query_decodes() {
        let q = parse_http_query("hub.mode=subscribe&hub.verify_token=tok%20123&empty=&x=a+b");
        let get = |k: &str| q.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        assert_eq!(get("hub.mode").as_deref(), Some("subscribe"));
        assert_eq!(get("hub.verify_token").as_deref(), Some("tok 123"));
        assert_eq!(get("x").as_deref(), Some("a b"));
    }

    #[test]
    fn http_url_decode_forms() {
        assert_eq!(http_url_decode("hello%20world"), "hello world");
        assert_eq!(http_url_decode("a+b"), "a b");
        assert_eq!(http_url_decode("%2B1555"), "+1555");
        assert_eq!(http_url_decode("plain"), "plain");
    }
}
