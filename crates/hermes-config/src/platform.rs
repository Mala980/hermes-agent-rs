//! Platform-specific configuration.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// UnauthorizedDmBehavior
// ---------------------------------------------------------------------------

/// How to handle unauthorized direct messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum UnauthorizedDmBehavior {
    /// Pair the user with the bot (create a session).
    Pair,
    /// Silently ignore the message.
    #[default]
    Ignore,
}

// ---------------------------------------------------------------------------
// PlatformConfig
// ---------------------------------------------------------------------------

/// Configuration for a specific platform (e.g. discord, slack).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PlatformConfig {
    /// Whether this platform adapter is enabled.
    #[serde(default)]
    pub enabled: bool,

    /// Bot authentication token (may be an env-var reference).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

    /// Optional webhook URL for incoming events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,

    /// Whether the bot must be @mentioned in group channels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_mention: Option<bool>,

    /// How to handle DMs from unauthorized users.
    #[serde(default)]
    pub unauthorized_dm_behavior: UnauthorizedDmBehavior,

    /// Whether each user gets their own group session context.
    #[serde(default)]
    pub group_sessions_per_user: bool,

    /// Home / landing channel for the bot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_channel: Option<String>,

    /// Users explicitly allowed to interact with the bot.
    #[serde(default)]
    pub allowed_users: Vec<String>,

    /// Users with administrative privileges.
    #[serde(default)]
    pub admin_users: Vec<String>,

    /// Keys not declared above (Python Hermes platform blocks often use many
    /// adapter-specific names); they deserialize here so YAML stays compatible.
    #[serde(default, flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl PlatformConfig {
    /// Deep-merge a JSON overlay into this configuration.
    ///
    /// Values present in `overlay` take precedence; missing keys in the
    /// overlay are left unchanged. The `extra` map is merged recursively.
    pub fn merge_with_json(&mut self, overlay: &serde_json::Value) {
        if let serde_json::Value::Object(map) = overlay {
            // Standard fields
            if let Some(v) = map.get("enabled").and_then(|v| v.as_bool()) {
                self.enabled = v;
            }
            if let Some(v) = map.get("token").and_then(|v| v.as_str()) {
                self.token = Some(v.to_string());
            }
            if let Some(v) = map.get("webhook_url").and_then(|v| v.as_str()) {
                self.webhook_url = Some(v.to_string());
            }
            if let Some(v) = map.get("require_mention").and_then(|v| v.as_bool()) {
                self.require_mention = Some(v);
            }
            if let Some(v) = map.get("unauthorized_dm_behavior").and_then(|v| v.as_str()) {
                if let Ok(behavior) = serde_json::from_value::<UnauthorizedDmBehavior>(
                    serde_json::Value::String(v.to_string()),
                ) {
                    self.unauthorized_dm_behavior = behavior;
                }
            }
            if let Some(v) = map.get("group_sessions_per_user").and_then(|v| v.as_bool()) {
                self.group_sessions_per_user = v;
            }
            if let Some(v) = map.get("home_channel").and_then(|v| v.as_str()) {
                self.home_channel = Some(v.to_string());
            }
            if let Some(v) = map.get("allowed_users") {
                if let Ok(users) = serde_json::from_value::<Vec<String>>(v.clone()) {
                    self.allowed_users = users;
                }
            }
            if let Some(v) = map.get("admin_users") {
                if let Ok(users) = serde_json::from_value::<Vec<String>>(v.clone()) {
                    self.admin_users = users;
                }
            }
            // Merge extra map recursively
            if let Some(serde_json::Value::Object(extra_overlay)) = map.get("extra") {
                for (k, v) in extra_overlay {
                    self.extra.insert(k.clone(), v.clone());
                }
            }
        }
    }

    /// Union of the first-class `allowed_users` field and `extra["allowed_users"]`
    /// (JSON array of strings or comma-separated string), trimmed, non-empty,
    /// and deduplicated.
    ///
    /// The field is the canonical place (what the `gateway setup` wizard
    /// writes); the `extra` fallback covers hand-written configs that nested
    /// the list under `extra`.
    pub fn allowed_users_union(&self) -> Vec<String> {
        let mut users: Vec<String> = self
            .allowed_users
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        if let Some(extra) = extra_string_list(self, "allowed_users") {
            users.extend(extra);
        }
        users.sort();
        users.dedup();
        users
    }
}

// ---------------------------------------------------------------------------
// Shared parsing helpers (`extra` map + token; Python / YAML–compatible)
// ---------------------------------------------------------------------------

/// `extra[key]` as a trimmed, non-empty string when the value is a JSON string.
pub fn extra_string(cfg: &PlatformConfig, key: &str) -> Option<String> {
    cfg.extra
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// `extra[key]` as a list of trimmed, non-empty strings.
///
/// Accepts either a JSON array of strings (e.g. `["123", "456"]`) or a
/// single comma-separated string (hand-written configs, e.g. `"123, 456"`).
/// Array items and CSV segments are trimmed and empties are dropped.
/// Returns `None` when the key is absent, the value is of another JSON
/// type, or no entries survive trimming.
pub fn extra_string_list(cfg: &PlatformConfig, key: &str) -> Option<Vec<String>> {
    let value = cfg.extra.get(key)?;
    let entries: Vec<String> = match value {
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|item| item.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        serde_json::Value::String(raw) => raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        _ => return None,
    };
    if entries.is_empty() {
        None
    } else {
        Some(entries)
    }
}

/// Bot token from [`PlatformConfig::token`] or `extra["token"]`, trimmed.
pub fn platform_token_or_extra(cfg: &PlatformConfig) -> Option<String> {
    cfg.token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| extra_string(cfg, "token"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extra_string_trims_and_skips_empty() {
        let mut pc = PlatformConfig::default();
        pc.extra.insert("k".to_string(), json!("  hi  "));
        assert_eq!(extra_string(&pc, "k").as_deref(), Some("hi"));
        pc.extra.insert("empty".to_string(), json!("   "));
        assert!(extra_string(&pc, "empty").is_none());
    }

    #[test]
    fn platform_token_or_extra_prefers_top_level_token() {
        let mut pc = PlatformConfig::default();
        pc.token = Some("  top  ".to_string());
        pc.extra.insert("token".to_string(), json!("extra"));
        assert_eq!(platform_token_or_extra(&pc).as_deref(), Some("top"));
    }

    #[test]
    fn extra_string_list_reads_json_array() {
        let mut pc = PlatformConfig::default();
        pc.extra
            .insert("k".to_string(), json!([" 123 ", "", "456", 7]));
        assert_eq!(
            extra_string_list(&pc, "k"),
            Some(vec!["123".to_string(), "456".to_string()])
        );
    }

    #[test]
    fn extra_string_list_accepts_comma_separated_string() {
        let mut pc = PlatformConfig::default();
        pc.extra.insert("k".to_string(), json!(" 123 , , 456 "));
        assert_eq!(
            extra_string_list(&pc, "k"),
            Some(vec!["123".to_string(), "456".to_string()])
        );
    }

    #[test]
    fn extra_string_list_empty_inputs_are_none() {
        let mut pc = PlatformConfig::default();
        assert!(extra_string_list(&pc, "missing").is_none());
        pc.extra.insert("empty_arr".to_string(), json!([]));
        assert!(extra_string_list(&pc, "empty_arr").is_none());
        pc.extra.insert("empty_csv".to_string(), json!(" , "));
        assert!(extra_string_list(&pc, "empty_csv").is_none());
        pc.extra.insert("wrong_type".to_string(), json!(42));
        assert!(extra_string_list(&pc, "wrong_type").is_none());
    }

    #[test]
    fn allowed_users_union_merges_field_and_extra() {
        let mut pc = PlatformConfig::default();
        pc.allowed_users = vec![" 123 ".to_string(), String::new(), "123".to_string()];
        pc.extra
            .insert("allowed_users".to_string(), json!(["456", " 789 "]));
        // Field is trimmed, empties dropped, both sources merged, deduped.
        assert_eq!(
            pc.allowed_users_union(),
            vec!["123".to_string(), "456".to_string(), "789".to_string()]
        );
    }

    #[test]
    fn allowed_users_union_empty_when_nothing_set() {
        let pc = PlatformConfig::default();
        assert!(pc.allowed_users_union().is_empty());
    }

    #[test]
    fn platform_token_or_extra_falls_back_to_extra() {
        let mut pc = PlatformConfig::default();
        pc.extra.insert("token".to_string(), json!("  from_extra "));
        assert_eq!(platform_token_or_extra(&pc).as_deref(), Some("from_extra"));
    }

    #[test]
    fn platform_config_default() {
        let pc = PlatformConfig::default();
        assert!(!pc.enabled);
        assert!(pc.token.is_none());
        assert_eq!(pc.unauthorized_dm_behavior, UnauthorizedDmBehavior::Ignore);
    }

    #[test]
    fn unauthorized_dm_behavior_serde() {
        let b = UnauthorizedDmBehavior::Pair;
        let json = serde_json::to_string(&b).unwrap();
        assert_eq!(json, "\"pair\"");
        let back: UnauthorizedDmBehavior = serde_json::from_str(&json).unwrap();
        assert_eq!(back, UnauthorizedDmBehavior::Pair);
    }

    #[test]
    fn merge_with_json_overlay() {
        let mut pc = PlatformConfig::default();
        let overlay = serde_json::json!({
            "enabled": true,
            "token": "abc123",
            "extra": {
                "custom_field": "value"
            }
        });
        pc.merge_with_json(&overlay);
        assert!(pc.enabled);
        assert_eq!(pc.token.as_deref(), Some("abc123"));
        assert_eq!(
            pc.extra.get("custom_field").unwrap().as_str().unwrap(),
            "value"
        );
    }
}
