//! Telegram adapter — error classification, token redaction, backoff jitter.

use std::collections::HashMap;

use hermes_core::errors::GatewayError;

// ---------------------------------------------------------------------------
// Error classification, token redaction, backoff jitter
// ---------------------------------------------------------------------------

/// Redact the bot token from a Telegram API URL for safe logging and error
/// text.
///
/// `https://api.telegram.org/bot<TOKEN>/sendMessage` →
/// `https://api.telegram.org/bot<REDACTED>/sendMessage`.
/// URLs without a `/bot` segment are returned unchanged.
pub(crate) fn redact_url(url: &str) -> String {
    match url.find("/bot") {
        Some(idx) => {
            let rest = &url[idx + "/bot".len()..];
            match rest.find('/') {
                Some(slash) => format!("{}/bot<REDACTED>{}", &url[..idx], &rest[slash..]),
                None => format!("{}/bot<REDACTED>", &url[..idx]),
            }
        }
        None => url.to_string(),
    }
}

/// Replace any occurrence of the raw bot token in `msg` with `<REDACTED>`.
///
/// reqwest error `Display`s embed the request URL, so error strings built
/// from them must be scrubbed before they reach logs or callers.
pub(crate) fn redact_token(msg: &str, token: &str) -> String {
    if token.is_empty() {
        msg.to_string()
    } else {
        msg.replace(token, "<REDACTED>")
    }
}

/// Map a Telegram API failure to a [`GatewayError`].
///
/// `error_code` is Telegram's own code (from an `ok:false` body); `status`
/// is the HTTP status and is used as a fallback when the body carries no
/// code. Mapping:
/// - 401 → `Auth` (fatal: bad/revoked token — do not retry).
/// - 403 → `Platform("telegram_forbidden: …")` (terminal: e.g. "bot was
///   blocked by the user" — callers must not retry sends).
/// - 409 → `Platform("telegram_conflict: …")` (another consumer is polling
///   with this token — the poll loop matches on this marker).
/// - 404 / other 4xx → `SendFailed` (bad chat id, malformed request, …).
/// - 429 → `SendFailed` (rate limited).
/// - 5xx / anything else → `ConnectionFailed`.
pub(crate) fn classify_telegram_error(
    status: u16,
    error_code: Option<i64>,
    description: &str,
) -> GatewayError {
    let code = error_code.unwrap_or(status as i64);
    let detail = if description.trim().is_empty() {
        "no description from Telegram".to_string()
    } else {
        description.trim().to_string()
    };
    match code {
        401 => GatewayError::Auth(format!(
            "telegram_auth: bot token invalid or revoked ({}): {}",
            code, detail
        )),
        403 => GatewayError::Platform(format!(
            "telegram_forbidden: terminal, do not retry ({}): {}",
            code, detail
        )),
        409 => GatewayError::Platform(format!("telegram_conflict: {}", detail)),
        404 => GatewayError::SendFailed(format!("Telegram API error 404: {}", detail)),
        429 => GatewayError::SendFailed(format!("Telegram API rate limited (429): {}", detail)),
        400..=499 => {
            GatewayError::SendFailed(format!("Telegram API client error {}: {}", code, detail))
        }
        _ => GatewayError::ConnectionFailed(format!(
            "Telegram API error {} (HTTP {}): {}",
            code, status, detail
        )),
    }
}

/// True when the error is a Telegram 409 getUpdates conflict — another
/// consumer is polling with the same bot token.
pub(crate) fn is_conflict_error(e: &GatewayError) -> bool {
    matches!(e, GatewayError::Platform(msg) if msg.starts_with("telegram_conflict:"))
}

/// Apply ±20% jitter to a backoff delay (in ms) to avoid thundering herds.
///
/// `rand_01` is a float in `[0, 1)`; 0.0 → `0.8 * base`, 1.0 → `1.2 * base`.
pub(crate) fn apply_jitter(base_ms: u64, rand_01: f64) -> u64 {
    let clamped = rand_01.clamp(0.0, 1.0);
    let delta = (base_ms as f64) * 0.2 * (clamped * 2.0 - 1.0);
    base_ms.saturating_add_signed(delta as i64).max(1)
}

/// Time-seeded pseudo-random float in `[0, 1)` for backoff jitter.
/// Avoids pulling in a `rand` dependency for this one call site.
pub(crate) fn time_based_rand01() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut x = nanos.wrapping_add(0x9E37_79B9);
    x ^= x >> 15;
    x = x.wrapping_mul(0x85EB_CA6B);
    x ^= x >> 13;
    f64::from(x) / (f64::from(u32::MAX) + 1.0)
}

// ---------------------------------------------------------------------------
// Parse-entities fallback helpers
// ---------------------------------------------------------------------------

/// Detect Telegram's "can't parse entities" failure in a Bot API
/// description or an HTTP-level error message (case-insensitive).
pub(crate) fn is_parse_entities_error(description: &str) -> bool {
    description.to_lowercase().contains("parse entities")
}

/// Detect a 400-class "can't parse entities" send failure wrapped in
/// [`GatewayError`]. `post_json` embeds the HTTP status and the API body in
/// `SendFailed` for non-2xx responses, so both must match.
pub(crate) fn is_parse_entities_send_failure(e: &GatewayError) -> bool {
    match e {
        GatewayError::SendFailed(msg) => msg.contains("400") && is_parse_entities_error(msg),
        _ => false,
    }
}
// ---------------------------------------------------------------------------
// Typing-cooldown helpers
// ---------------------------------------------------------------------------

/// Pure per-chat typing cooldown check (unit-testable).
pub(crate) fn typing_on_cooldown(
    cooldowns: &std::sync::Mutex<HashMap<String, std::time::Instant>>,
    chat_id: &str,
) -> bool {
    match cooldowns.lock() {
        Ok(guard) => guard
            .get(chat_id)
            .is_some_and(|until| std::time::Instant::now() < *until),
        Err(_) => false,
    }
}

/// Extract the `retry_after=` seconds value from a `GatewayError::SendFailed`
/// message produced by `post_json`'s rate-limit handling.
pub(crate) fn extract_retry_after_seconds(msg: &str) -> Option<u64> {
    let idx = msg.find("retry_after=")?;
    let digits: String = msg[idx + "retry_after=".len()..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platforms::telegram::*;

    #[test]
    fn classify_401_is_auth_fatal() {
        let e = classify_telegram_error(200, Some(401), "Unauthorized");
        assert!(matches!(e, GatewayError::Auth(_)));
        assert!(e.to_string().contains("Unauthorized"));
    }
    #[test]
    fn classify_403_is_terminal_forbidden() {
        let e = classify_telegram_error(403, Some(403), "Forbidden: bot was blocked by the user");
        match &e {
            GatewayError::Platform(msg) => {
                assert!(msg.starts_with("telegram_forbidden:"));
                assert!(msg.contains("bot was blocked by the user"));
            }
            other => panic!("expected Platform, got {:?}", other),
        }
    }
    #[test]
    fn classify_409_is_conflict() {
        let e = classify_telegram_error(
            409,
            Some(409),
            "Conflict: terminated by other getUpdates request",
        );
        match &e {
            GatewayError::Platform(msg) => assert!(msg.starts_with("telegram_conflict:")),
            other => panic!("expected Platform, got {:?}", other),
        }
        assert!(is_conflict_error(&e));
        assert!(!is_conflict_error(&GatewayError::Auth("x".into())));
        assert!(!is_conflict_error(&GatewayError::Platform(
            "telegram_forbidden: x".into()
        )));
    }
    #[test]
    fn classify_429_is_send_failed() {
        let e = classify_telegram_error(429, None, "Too Many Requests: retry after 30");
        assert!(matches!(e, GatewayError::SendFailed(_)));
    }
    #[test]
    fn classify_other_codes() {
        // HTTP status used as fallback when the body carries no error_code.
        let e = classify_telegram_error(400, None, "Bad Request: chat not found");
        assert!(matches!(e, GatewayError::SendFailed(_)));
        assert!(e.to_string().contains("chat not found"));

        // error_code takes precedence over the HTTP status.
        let e = classify_telegram_error(200, Some(401), "Unauthorized");
        assert!(matches!(e, GatewayError::Auth(_)));

        let e = classify_telegram_error(403, None, "Forbidden: bot was kicked");
        match &e {
            GatewayError::Platform(msg) => assert!(msg.starts_with("telegram_forbidden:")),
            other => panic!("expected Platform, got {:?}", other),
        }

        let e = classify_telegram_error(500, None, "Internal Server Error");
        assert!(matches!(e, GatewayError::ConnectionFailed(_)));
        assert!(e.to_string().contains("500"));
    }
    #[test]
    fn classify_empty_description_has_fallback() {
        let e = classify_telegram_error(400, Some(400), "");
        assert!(e.to_string().contains("no description from Telegram"));
    }
    #[test]
    fn redact_url_hides_token() {
        let url = "https://api.telegram.org/botSECRET_TOKEN_123/sendMessage";
        let redacted = redact_url(url);
        assert_eq!(
            redacted,
            "https://api.telegram.org/bot<REDACTED>/sendMessage"
        );
        assert!(!redacted.contains("SECRET_TOKEN_123"));
    }
    #[test]
    fn redact_url_passthrough_without_bot_segment() {
        let url = "https://example.com/health";
        assert_eq!(redact_url(url), url);
    }
    #[test]
    fn redact_token_scrubs_reqwest_style_errors() {
        let token = "SECRET_TOKEN_123";
        let msg = format!(
            "error sending request for url (https://api.telegram.org/bot{}/sendMessage): connection reset",
            token
        );
        let scrubbed = redact_token(&msg, token);
        assert!(!scrubbed.contains(token));
        assert!(scrubbed.contains("<REDACTED>"));
    }
    #[test]
    fn redact_token_empty_token_is_passthrough() {
        let msg = "plain error, nothing to scrub";
        assert_eq!(redact_token(msg, ""), msg);
    }
    #[test]
    fn jitter_stays_within_20_percent() {
        for base in [1_000u64, 2_000, 60_000] {
            let expected_lo = (base as f64 * 0.8) as u64;
            let expected_hi = (base as f64 * 1.2) as u64;

            let lo = apply_jitter(base, 0.0);
            let mid = apply_jitter(base, 0.5);
            let hi = apply_jitter(base, 1.0);
            assert!(
                (lo as i64 - expected_lo as i64).abs() <= 1,
                "lo jitter out of bounds: base={base} lo={lo}"
            );
            assert!(
                (mid as i64 - base as i64).abs() <= 1,
                "mid jitter should be ~base: base={base} mid={mid}"
            );
            assert!(
                (hi as i64 - expected_hi as i64).abs() <= 1,
                "hi jitter out of bounds: base={base} hi={hi}"
            );

            // Arbitrary seeds stay in bounds too.
            for seed in [0.13, 0.37, 0.71, 0.99] {
                let j = apply_jitter(base, seed);
                assert!(
                    j + 1 >= expected_lo && j <= expected_hi + 1,
                    "jitter out of bounds: base={base} seed={seed} j={j}"
                );
            }
        }
    }
    #[test]
    fn jitter_clamps_seed() {
        // Out-of-range seeds are clamped, never produce a negative delay.
        let j = apply_jitter(1_000, -5.0);
        assert!((799..=801).contains(&j));
        let j = apply_jitter(1_000, 99.0);
        assert!((1199..=1201).contains(&j));
    }
    #[test]
    fn is_parse_entities_error_detects() {
        assert!(is_parse_entities_error(
            "Bad Request: can't parse entities: can't find end of the entity starting at byte offset 5"
        ));
        // Case-insensitive matching.
        assert!(is_parse_entities_error("BAD REQUEST: CAN'T PARSE ENTITIES"));
        assert!(!is_parse_entities_error(
            "Bad Request: message text is empty"
        ));
        assert!(!is_parse_entities_error(""));
    }
    #[test]
    fn is_parse_entities_send_failure_detects_400() {
        // HTTP 400 with a parse-entities description: retry as plain text.
        let err = GatewayError::SendFailed(
            "Telegram API returned HTTP 400: {\"ok\":false,\"description\":\"Bad Request: can't parse entities: ...\"}"
                .to_string(),
        );
        assert!(is_parse_entities_send_failure(&err));

        // 400 without a parse-entities description: no fallback.
        let other = GatewayError::SendFailed(
            "Telegram API returned HTTP 400: {\"ok\":false,\"description\":\"Bad Request: chat not found\"}"
                .to_string(),
        );
        assert!(!is_parse_entities_send_failure(&other));

        // Non-400 status mentioning parse entities: no fallback.
        let rate = GatewayError::SendFailed(
            "Telegram API returned HTTP 429: parse entities would fail".to_string(),
        );
        assert!(!is_parse_entities_send_failure(&rate));

        // Non-send errors never trigger the fallback.
        let conn = GatewayError::ConnectionFailed("boom".to_string());
        assert!(!is_parse_entities_send_failure(&conn));
    }
}
