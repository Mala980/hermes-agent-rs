//! Shared platform helper functions.
//!
//! Common text manipulation utilities used across platform adapters.

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
