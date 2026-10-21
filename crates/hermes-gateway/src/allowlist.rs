//! Per-platform fail-closed user allowlists for inbound gateway messages.
//!
//! Each supported platform reads `<PLATFORM>_ALLOWED_USERS` union
//! `GATEWAY_ALLOWED_USERS` (comma-separated IDs; `"*"` = everyone).
//! `GATEWAY_ALLOWED_USERS` is the cross-platform fallback applied to every
//! platform.
//!
//! `<PLATFORM>_ALLOW_ALL_USERS` / `GATEWAY_ALLOW_ALL_USERS` (true/1/yes)
//! disable the gate entirely — intended for local development only; a warn
//! is logged when the gate is bypassed this way.
//!
//! Fail-closed: when neither env var carries entries for a platform, NO user
//! is allowed and inbound messages are skipped with a warn log. Configure one
//! of the env vars (or the allow-all dev switch) to admit users.
//!
//! Supported platforms: `discord`, `slack`, `matrix`, `email`. Other platform
//! names are not gated here (legacy behavior preserved); adding a new
//! platform only requires a new entry in [`platform_env_prefix`].
//!
//! Interaction with [`crate::dm::DmManager`]: this gate runs FIRST in
//! [`crate::gateway::Gateway::route_message`], before the DM pairing check,
//! and applies to group/channel messages as well as DMs. With the default
//! Pair behavior, pairing requests only flow for users who pass this gate,
//! so operators who want open pairing must set
//! `<PLATFORM>_ALLOWED_USERS="*"` (or the allow-all dev switch). `DmManager`
//! remains authoritative for DM pairing decisions among users who pass the
//! gate; its authorized/admin state is in-memory only, whereas this allowlist
//! is env-driven and survives restarts.

use std::collections::HashSet;

/// Lock serializing env-mutating allowlist tests. Rust runs tests in
/// parallel and these tests share env vars (`*_ALLOWED_USERS`, ...), so
/// every test that mutates them — here and in `gateway.rs` — must hold
/// this lock.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Read one allowlist CSV env var into a set of IDs (empty when unset).
pub fn read_allow_csv(var: &str) -> HashSet<String> {
    std::env::var(var)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// True when the env var is true/1/yes (case-insensitive).
pub fn env_truthy(var: &str) -> bool {
    matches!(
        std::env::var(var)
            .unwrap_or_default()
            .trim()
            .to_lowercase()
            .as_str(),
        "true" | "1" | "yes"
    )
}

/// Map an [`crate::gateway::IncomingMessage`] platform name to the env var
/// prefix used for its allowlist (e.g. `"discord"` → `"DISCORD"`).
///
/// Returns `None` for platforms without a configured allowlist env shape;
/// those platforms are not gated by this module.
pub fn platform_env_prefix(platform: &str) -> Option<&'static str> {
    match platform {
        "discord" => Some("DISCORD"),
        "slack" => Some("SLACK"),
        "matrix" => Some("MATRIX"),
        "email" => Some("EMAIL"),
        _ => None,
    }
}

/// Union of `<PLATFORM>_ALLOWED_USERS` and `GATEWAY_ALLOWED_USERS`.
///
/// Returns `None` when the platform has no env shape, or when both vars are
/// empty → fail-closed (no user allowed).
pub fn platform_allowlist(platform: &str) -> Option<HashSet<String>> {
    let prefix = platform_env_prefix(platform)?;
    let mut ids = read_allow_csv(&format!("{prefix}_ALLOWED_USERS"));
    ids.extend(read_allow_csv("GATEWAY_ALLOWED_USERS"));
    if ids.is_empty() {
        None
    } else {
        Some(ids)
    }
}

/// "Allow everyone" dev switch: `<PLATFORM>_ALLOW_ALL_USERS` or the global
/// `GATEWAY_ALLOW_ALL_USERS` set to true/1/yes.
pub fn platform_allow_all(platform: &str) -> bool {
    match platform_env_prefix(platform) {
        Some(prefix) => {
            env_truthy(&format!("{prefix}_ALLOW_ALL_USERS"))
                || env_truthy("GATEWAY_ALLOW_ALL_USERS")
        }
        None => false,
    }
}

/// Pure decision: is `user_id` admitted on `platform`?
///
/// Order: allow-all switch first, then the allowlist (`"*"` matches
/// everyone), default deny (fail-closed). Platforms without an env shape are
/// not gated and always return `true` (legacy behavior).
pub fn is_platform_user_allowed(platform: &str, user_id: &str) -> bool {
    if platform_env_prefix(platform).is_none() {
        return true;
    }
    if platform_allow_all(platform) {
        return true;
    }
    match platform_allowlist(platform) {
        None => false,
        Some(set) => set.contains("*") || set.contains(user_id),
    }
}

// ---------------------------------------------------------------------------
// Per-platform convenience helpers (mirrors the reference allowlist shape)
// ---------------------------------------------------------------------------

/// Discord allowlist: `DISCORD_ALLOWED_USERS` ∪ `GATEWAY_ALLOWED_USERS`.
pub fn discord_allowlist() -> Option<HashSet<String>> {
    platform_allowlist("discord")
}

/// Discord allow-all switch: `DISCORD_ALLOW_ALL_USERS` ∪ `GATEWAY_ALLOW_ALL_USERS`.
pub fn discord_allow_all() -> bool {
    platform_allow_all("discord")
}

/// Slack allowlist: `SLACK_ALLOWED_USERS` ∪ `GATEWAY_ALLOWED_USERS`.
pub fn slack_allowlist() -> Option<HashSet<String>> {
    platform_allowlist("slack")
}

/// Slack allow-all switch: `SLACK_ALLOW_ALL_USERS` ∪ `GATEWAY_ALLOW_ALL_USERS`.
pub fn slack_allow_all() -> bool {
    platform_allow_all("slack")
}

/// Matrix allowlist: `MATRIX_ALLOWED_USERS` ∪ `GATEWAY_ALLOWED_USERS`.
pub fn matrix_allowlist() -> Option<HashSet<String>> {
    platform_allowlist("matrix")
}

/// Matrix allow-all switch: `MATRIX_ALLOW_ALL_USERS` ∪ `GATEWAY_ALLOW_ALL_USERS`.
pub fn matrix_allow_all() -> bool {
    platform_allow_all("matrix")
}

/// Email allowlist: `EMAIL_ALLOWED_USERS` ∪ `GATEWAY_ALLOWED_USERS`.
pub fn email_allowlist() -> Option<HashSet<String>> {
    platform_allowlist("email")
}

/// Email allow-all switch: `EMAIL_ALLOW_ALL_USERS` ∪ `GATEWAY_ALLOW_ALL_USERS`.
pub fn email_allow_all() -> bool {
    platform_allow_all("email")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_var(name: &str, v: Option<&str>) -> Option<String> {
        let old = std::env::var(name).ok();
        match v {
            Some(s) => std::env::set_var(name, s),
            None => std::env::remove_var(name),
        }
        old
    }

    fn restore_var(name: &str, old: Option<String>) {
        match old {
            Some(s) => std::env::set_var(name, s),
            None => std::env::remove_var(name),
        }
    }

    const VARS: &[&str] = &[
        "DISCORD_ALLOWED_USERS",
        "DISCORD_ALLOW_ALL_USERS",
        "SLACK_ALLOWED_USERS",
        "SLACK_ALLOW_ALL_USERS",
        "MATRIX_ALLOWED_USERS",
        "MATRIX_ALLOW_ALL_USERS",
        "EMAIL_ALLOWED_USERS",
        "EMAIL_ALLOW_ALL_USERS",
        "GATEWAY_ALLOWED_USERS",
        "GATEWAY_ALLOW_ALL_USERS",
    ];

    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn clear() -> Self {
            let saved = VARS.iter().map(|&n| (n, set_var(n, None))).collect();
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, old) in self.saved.drain(..) {
                restore_var(name, old);
            }
        }
    }

    #[test]
    fn fail_closed_when_unset() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::clear();

        for platform in ["discord", "slack", "matrix", "email"] {
            assert_eq!(
                platform_allowlist(platform),
                None,
                "{platform} allowlist must be None when unset"
            );
            assert!(
                !is_platform_user_allowed(platform, "anyone"),
                "{platform} must deny when unset (fail-closed)"
            );
        }
    }

    #[test]
    fn platform_specific_allowlist() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::clear();

        set_var("DISCORD_ALLOWED_USERS", Some("  111 , 222,,"));
        assert!(is_platform_user_allowed("discord", "111"));
        assert!(is_platform_user_allowed("discord", "222"));
        assert!(!is_platform_user_allowed("discord", "333"));
        // Other platforms stay fail-closed.
        assert!(!is_platform_user_allowed("slack", "111"));
    }

    #[test]
    fn gateway_fallback_unions_across_platforms() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::clear();

        set_var("DISCORD_ALLOWED_USERS", Some("111"));
        set_var("GATEWAY_ALLOWED_USERS", Some("222"));
        assert!(is_platform_user_allowed("discord", "111"));
        assert!(is_platform_user_allowed("discord", "222"));
        // GATEWAY_ALLOWED_USERS also covers other platforms.
        assert!(is_platform_user_allowed("matrix", "222"));
        assert!(!is_platform_user_allowed("matrix", "111"));
    }

    #[test]
    fn wildcard_allows_everyone() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::clear();

        set_var("EMAIL_ALLOWED_USERS", Some("*"));
        assert!(is_platform_user_allowed("email", "anyone@example.com"));
        assert!(is_platform_user_allowed("email", "whatever"));
    }

    #[test]
    fn allow_all_switch_bypasses_allowlist() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::clear();

        // Platform-specific switch.
        set_var("SLACK_ALLOW_ALL_USERS", Some("1"));
        assert!(slack_allow_all());
        assert!(is_platform_user_allowed("slack", "anyone"));

        // Global switch.
        set_var("SLACK_ALLOW_ALL_USERS", None);
        set_var("GATEWAY_ALLOW_ALL_USERS", Some("yes"));
        assert!(is_platform_user_allowed("slack", "anyone"));
        assert!(is_platform_user_allowed("matrix", "anyone"));
    }

    #[test]
    fn convenience_helpers_match_generic() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::clear();

        set_var("MATRIX_ALLOWED_USERS", Some("u1"));
        assert_eq!(matrix_allowlist(), platform_allowlist("matrix"));
        assert_eq!(matrix_allow_all(), platform_allow_all("matrix"));
        assert!(!matrix_allow_all());
    }

    #[test]
    fn unknown_platform_is_not_gated() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::clear();

        assert_eq!(platform_env_prefix("test"), None);
        assert!(is_platform_user_allowed("test", "anyone"));
        assert!(!platform_allow_all("test"));
    }

    #[test]
    fn prefix_mapping() {
        assert_eq!(platform_env_prefix("discord"), Some("DISCORD"));
        assert_eq!(platform_env_prefix("slack"), Some("SLACK"));
        assert_eq!(platform_env_prefix("matrix"), Some("MATRIX"));
        assert_eq!(platform_env_prefix("email"), Some("EMAIL"));
        assert_eq!(platform_env_prefix("telegram"), None);
    }
}
