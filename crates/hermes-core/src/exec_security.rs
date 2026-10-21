//! Exec security helpers shared by every child-process spawn site.
//!
//! Two defenses, both fail-closed and non-reallowable:
//!
//! 1. **Credential env blocklist** ([`is_credential_env_var`],
//!    [`apply_sanitized_env`], [`sanitize_spawn_env`]): environment variables
//!    whose names look like credentials are stripped from a child command's
//!    environment — both the inherited parent env and any explicitly passed-in
//!    env map. Everything else (`PATH`, `HOME`, …) passes through unchanged.
//! 2. **Secret redaction** ([`redact_secrets`]): well-known secret shapes in
//!    command output (key=value assignments, `Bearer` headers, AWS key IDs,
//!    JWTs, Slack/GitHub tokens) are replaced with `[REDACTED]` before the
//!    output becomes a tool result or is stored.
//!
//! Ports the equivalent logic from the reference implementation's
//! `shell.rs` (`is_credential_env_var`, `apply_sanitized_env`,
//! `redact_secrets`).

use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// Credential env var blocklist
// ---------------------------------------------------------------------------

/// Returns true if `name` looks like it holds a credential and therefore must
/// never be inherited by (or explicitly passed to) a spawned child process.
///
/// Matching is case-insensitive and non-reallowable — there is no allowlist
/// override. Rules, in order:
/// - exact names: `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
///   `AWS_SESSION_TOKEN`, `GITHUB_TOKEN`, `TELEGRAM_BOT_TOKEN`
/// - prefixes: `AWS_`, `AZURE_`, `GOOGLE_`, `ANTHROPIC_`, `OPENAI_`, `XAI_`
/// - suffixes: `_API_KEY`, `_TOKEN`, `_SECRET`, `_PASSWORD`, `_PASSWD`,
///   `_PRIVATE_KEY`, `_CLIENT_SECRET`
pub fn is_credential_env_var(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    const EXACT: &[&str] = &[
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "GITHUB_TOKEN",
        "TELEGRAM_BOT_TOKEN",
    ];
    if EXACT.iter().any(|e| upper == *e) {
        return true;
    }
    const PREFIXES: &[&str] = &["AWS_", "AZURE_", "GOOGLE_", "ANTHROPIC_", "OPENAI_", "XAI_"];
    if PREFIXES.iter().any(|p| upper.starts_with(p)) {
        return true;
    }
    const SUFFIXES: &[&str] = &[
        "_API_KEY",
        "_TOKEN",
        "_SECRET",
        "_PASSWORD",
        "_PASSWD",
        "_PRIVATE_KEY",
        "_CLIENT_SECRET",
    ];
    SUFFIXES.iter().any(|s| upper.ends_with(s))
}

/// Strip credential-looking variables from a child command's environment.
///
/// 1. Removes matches inherited from the parent process env (via
///    [`std::process::Command::env_remove`]).
/// 2. Applies `extra_env` (if any) with credential-looking entries filtered
///    out.
///
/// Benign variables (`PATH`, `HOME`, …) are passed through untouched.
pub fn apply_sanitized_env(
    cmd: &mut std::process::Command,
    extra_env: Option<&HashMap<String, String>>,
) {
    // 1. Strip credentials inherited from the parent env.
    for (k, _v) in std::env::vars_os() {
        if let Some(name) = k.to_str() {
            if is_credential_env_var(name) {
                cmd.env_remove(&k);
            }
        }
    }
    // 2. Apply explicitly passed env with credentials filtered out.
    if let Some(extra) = extra_env {
        for (k, v) in extra {
            if !is_credential_env_var(k) {
                cmd.env(k, v);
            }
        }
    }
}

/// Convenience wrapper around [`apply_sanitized_env`] when there is no
/// explicit env map — just strip inherited credentials.
///
/// For `tokio::process::Command`, call with `cmd.as_std_mut()`.
pub fn sanitize_spawn_env(cmd: &mut std::process::Command) {
    apply_sanitized_env(cmd, None);
}

// ---------------------------------------------------------------------------
// Secret redaction for command output
// ---------------------------------------------------------------------------

/// Redact well-known secret shapes in `text`, replacing values with
/// `[REDACTED]`.
///
/// Covers (in this order):
/// - `key=value` / `key: value` assignments whose key looks like
///   `api_key`/`token`/`secret`/`password`/`passwd`/`bearer` (key name kept)
/// - standalone `Bearer <token>` headers
/// - AWS access key IDs (`AKIA` + 16 chars)
/// - JWTs (`eyJ.<b64url>.<b64url>`)
/// - Slack tokens (`xox[bap]-…`)
/// - GitHub tokens (`gh[pousr]_…`)
///
/// Idempotent: running it over already-redacted text is a no-op.
pub fn redact_secrets(text: &str) -> String {
    static RES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
        vec![
            // key=value or key: value (case-insensitive); key name is kept.
            (
                Regex::new(
                    r#"(?i)([\w.-]*?(?:api[_-]?key|token|secret|password|passwd|bearer))(\s*[=:]\s*)("[^"]*"|'[^']*'|\S+)"#,
                )
                .unwrap(),
                "${1}${2}[REDACTED]",
            ),
            // "Bearer <token>" as in an Authorization header (no =/:).
            // Restricted to the word `bearer`: the space form for other key
            // words (`token`, `password`, …) appears too often in prose.
            (
                Regex::new(r"(?i)\b(bearer)(\s+)(\S+)").unwrap(),
                "${1}${2}[REDACTED]",
            ),
            // AWS access key ID: AKIA + 16 characters.
            (
                Regex::new(r"\bAKIA[0-9A-Z]{16}\b").unwrap(),
                "[REDACTED]",
            ),
            // JWT: eyJ.<base64url>.<base64url>.
            (
                Regex::new(r"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b")
                    .unwrap(),
                "[REDACTED]",
            ),
            // Slack tokens: xox[bap]-...
            (
                Regex::new(r"\bxox[bap]-[A-Za-z0-9-]+\b").unwrap(),
                "[REDACTED]",
            ),
            // GitHub tokens: ghp_/gho_/ghu_/ghs_/ghr_...
            (
                Regex::new(r"\bgh[pousr]_[A-Za-z0-9_]+\b").unwrap(),
                "[REDACTED]",
            ),
        ]
    });
    let mut out = text.to_string();
    for (re, rep) in RES.iter() {
        out = re.replace_all(&out, *rep).into_owned();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_var_matching_rules() {
        // Suffixes (case-insensitive).
        for n in [
            "OPENAI_API_KEY",
            "my_api_key",
            "DB_PASSWORD",
            "db_passwd",
            "S3_SECRET",
            "id_rsa_PRIVATE_KEY",
            "oauth_CLIENT_SECRET",
            "AUTH_TOKEN",
            "MY_TOKEN",
        ] {
            assert!(is_credential_env_var(n), "{n} should be a credential var");
        }
        // Exact names (case-insensitive).
        for n in [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "GITHUB_TOKEN",
            "github_token",
            "TELEGRAM_BOT_TOKEN",
        ] {
            assert!(is_credential_env_var(n), "{n} should be a credential var");
        }
        // Prefixes (case-insensitive).
        for n in [
            "AWS_REGION",
            "AZURE_TENANT_ID",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "ANTHROPIC_BASE_URL",
            "OPENAI_BASE_URL",
            "XAI_FOO",
        ] {
            assert!(is_credential_env_var(n), "{n} should be a credential var");
        }
        // Negatives: must not match.
        for n in [
            "PATH",
            "HOME",
            "MY_NORMAL_VAR",
            "API_KEYS_LIST", // "_API_KEY" in the middle, not a suffix
            "SECRETARY",
            "PASSWORDLESS",
            "TOKENIZER",
        ] {
            assert!(
                !is_credential_env_var(n),
                "{n} must not be a credential var"
            );
        }
    }

    #[test]
    fn sanitize_strips_extra_env_credentials_but_keeps_normal_vars() {
        let mut extra = HashMap::new();
        extra.insert(
            "HERMES_SEC_TEST_API_KEY".to_string(),
            "TOPSECRET1".to_string(),
        );
        extra.insert(
            "HERMES_SEC_TEST_TOKEN".to_string(),
            "TOPSECRET2".to_string(),
        );
        extra.insert("HERMES_SEC_TEST_NORMAL".to_string(), "keepme".to_string());

        let mut cmd = std::process::Command::new("env");
        apply_sanitized_env(&mut cmd, Some(&extra));
        let out = cmd.output().expect("spawn env failed");
        let text = String::from_utf8_lossy(&out.stdout);

        assert!(
            !text
                .lines()
                .any(|l| l.starts_with("HERMES_SEC_TEST_API_KEY=")),
            "credential var leaked to child"
        );
        assert!(!text.contains("TOPSECRET1"), "credential value leaked");
        assert!(
            !text
                .lines()
                .any(|l| l.starts_with("HERMES_SEC_TEST_TOKEN=")),
            "credential var leaked to child"
        );
        assert!(!text.contains("TOPSECRET2"), "credential value leaked");
        assert!(
            text.lines().any(|l| l == "HERMES_SEC_TEST_NORMAL=keepme"),
            "normal var must pass through"
        );
        assert!(
            text.lines().any(|l| l.starts_with("PATH=")),
            "PATH must pass through"
        );
    }

    #[test]
    fn sanitize_strips_inherited_credential_vars() {
        // Unique name so parallel tests cannot collide with it.
        std::env::set_var("HERMES_SEC_TEST_INHERIT_API_KEY", "INHERITSECRET");
        let mut cmd = std::process::Command::new("env");
        sanitize_spawn_env(&mut cmd);
        let out = cmd.output().expect("spawn env failed");
        std::env::remove_var("HERMES_SEC_TEST_INHERIT_API_KEY");
        let text = String::from_utf8_lossy(&out.stdout);

        assert!(
            !text
                .lines()
                .any(|l| l.starts_with("HERMES_SEC_TEST_INHERIT_API_KEY=")),
            "inherited credential var leaked to child"
        );
        assert!(
            !text.contains("INHERITSECRET"),
            "inherited secret value leaked"
        );
    }

    #[test]
    fn redact_secrets_covers_each_shape() {
        let cases = [
            ("api_key=hunter2value", "api_key=[REDACTED]"),
            ("token: abc123", "token: [REDACTED]"),
            ("db_password='s3cr3t!'", "db_password=[REDACTED]"),
            ("my secret=shh", "my secret=[REDACTED]"),
            ("Bearer abc123token", "Bearer [REDACTED]"),
            ("key is AKIAIOSFODNN7EXAMPLE here", "key is [REDACTED] here"),
            ("jwt eyJzdWI.eyJzdWI.c2ln end", "jwt [REDACTED] end"),
            ("token xoxb-1234-abcdef", "token [REDACTED]"),
            ("using ghp_abcdef1234567890", "using [REDACTED]"),
        ];
        for (input, expected) in cases {
            let got = redact_secrets(input);
            assert!(
                got.contains(expected),
                "redact({input:?}) = {got:?}, expected to contain {expected:?}"
            );
        }
        // No raw secret values may survive (shaped tokens only — a bare
        // unknown word like "hunter2value" has no recognizable shape).
        for secret in ["AKIAIOSFODNN7EXAMPLE", "ghp_abcdef1234567890"] {
            assert!(
                !redact_secrets(secret).contains(secret),
                "secret leaked: {secret}"
            );
        }
    }

    #[test]
    fn redact_secrets_leaves_normal_output_untouched() {
        let text = "hello world\nexit code 0\nbuilt 42 crates in 3s";
        assert_eq!(redact_secrets(text), text);
        // Idempotent: already-redacted text is unchanged.
        let once = redact_secrets("api_key=hunter2value");
        assert_eq!(redact_secrets(&once), once);
    }
}
