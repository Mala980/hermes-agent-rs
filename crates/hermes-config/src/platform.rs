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
// LineConfig
// ---------------------------------------------------------------------------

/// LINE Messaging API adapter configuration.
///
/// Deserializes with `#[serde(default)]`; an empty `channel_secret` or
/// `channel_access_token` fails adapter construction (fail-closed).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LineConfig {
    /// LINE channel secret used for webhook HMAC-SHA256 signature verification.
    pub channel_secret: String,
    /// LINE channel access token (bearer token for the Reply/Push APIs).
    pub channel_access_token: String,
    /// Local bind address for the webhook listener.
    pub bind_addr: String,
    /// Local port for the webhook listener.
    pub port: u16,
    /// Webhook path the LINE platform posts events to.
    pub path: String,
}

impl Default for LineConfig {
    fn default() -> Self {
        Self {
            channel_secret: String::new(),
            channel_access_token: String::new(),
            bind_addr: "0.0.0.0".to_string(),
            port: 8646,
            path: "/line/webhook".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// NtfyConfig
// ---------------------------------------------------------------------------

/// ntfy push-notification adapter configuration.
///
/// Deserializes with `#[serde(default)]`; an empty `topic` fails adapter
/// construction (fail-closed). Auth is optional — ntfy topics are public
/// unless the server protects them (Bearer token or Basic `user:pass`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NtfyConfig {
    /// ntfy server base URL, e.g. `https://ntfy.sh`.
    pub server: String,
    /// Topic to subscribe to; also the default publish target. Required.
    pub topic: String,
    /// Publish target topic; defaults to `topic` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_topic: Option<String>,
    /// Bearer access token (or `user:pass`, which becomes Basic auth).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    /// Basic-auth username (alternative to `user:pass` in `access_token`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Basic-auth password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Notification title, sent as the `X-Title` header on publish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Publish with `Markdown: yes` so the server renders the body as Markdown.
    pub markdown: bool,
    /// Tag stamped on outgoing messages; inbound messages carrying it are
    /// dropped for echo-loop prevention (ntfy has no sender identity).
    pub echo_tag: String,
}

impl Default for NtfyConfig {
    fn default() -> Self {
        Self {
            server: "https://ntfy.sh".to_string(),
            topic: String::new(),
            publish_topic: None,
            access_token: None,
            username: None,
            password: None,
            title: None,
            markdown: false,
            echo_tag: "hermes-agent".to_string(),
        }
    }
}

impl NtfyConfig {
    /// Build from a generic [`PlatformConfig`].
    ///
    /// `extra["access_token"]` wins; the first-class `token` (or
    /// `extra["token"]`) is the fallback. Everything else comes from `extra`.
    pub fn from_platform_config(p: &PlatformConfig) -> Self {
        let mut cfg = Self::default();
        if let Some(v) = extra_string(p, "server") {
            cfg.server = v.trim_end_matches('/').to_string();
        }
        if let Some(v) = extra_string(p, "topic") {
            cfg.topic = v;
        }
        cfg.publish_topic = extra_string(p, "publish_topic");
        cfg.access_token = extra_string(p, "access_token").or_else(|| platform_token_or_extra(p));
        cfg.username = extra_string(p, "username");
        cfg.password = extra_string(p, "password");
        cfg.title = extra_string(p, "title");
        if let Some(v) = p.extra.get("markdown").and_then(|v| v.as_bool()) {
            cfg.markdown = v;
        }
        if let Some(v) = extra_string(p, "echo_tag") {
            cfg.echo_tag = v;
        }
        cfg
    }

    /// Effective publish topic: explicit `publish_topic`, else `topic`.
    pub fn effective_publish_topic(&self) -> &str {
        self.publish_topic
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.topic)
    }
}

// ---------------------------------------------------------------------------
// BuzzConfig
// ---------------------------------------------------------------------------

/// Buzz team-messaging adapter configuration.
///
/// Deserializes with `#[serde(default)]`. An empty `webhook_url` (nothing can
/// be sent) or an empty `webhook_secret` (inbound signatures cannot be
/// verified) fails adapter construction (fail-closed).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BuzzConfig {
    /// Buzz API base URL for outgoing messages, e.g. `https://buzz.example.com/api`.
    pub webhook_url: String,
    /// Bearer token for the Buzz API (outbound auth).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token: Option<String>,
    /// Shared secret for HMAC-SHA256 (`sha256=<hex>`) verification of inbound
    /// webhook payloads. Required.
    pub webhook_secret: String,
    /// Local bind address for the inbound webhook listener.
    pub bind_addr: String,
    /// Local port for the inbound webhook listener.
    pub webhook_port: u16,
    /// Webhook path Buzz posts events to.
    pub webhook_path: String,
    /// Whether this adapter is enabled.
    pub enabled: bool,
}

impl Default for BuzzConfig {
    fn default() -> Self {
        Self {
            webhook_url: String::new(),
            bot_token: None,
            webhook_secret: String::new(),
            bind_addr: "127.0.0.1".to_string(),
            webhook_port: 8765,
            webhook_path: "/buzz/webhook".to_string(),
            enabled: true,
        }
    }
}

/// Parse a port from a JSON number or a numeric string.
fn parse_port(v: &serde_json::Value) -> Option<u16> {
    if let Some(n) = v.as_u64() {
        return u16::try_from(n).ok();
    }
    v.as_str()?.trim().parse().ok()
}

impl BuzzConfig {
    /// Build from a generic [`PlatformConfig`].
    ///
    /// `extra["bot_token"]` wins; the first-class `token` (or
    /// `extra["token"]`) is the fallback. `extra["webhook_port"]` accepts a
    /// JSON number or a string. `enabled` follows `extra["enabled"]` when set,
    /// otherwise the top-level platform `enabled` flag.
    pub fn from_platform_config(p: &PlatformConfig) -> Self {
        let mut cfg = Self::default();
        if let Some(v) = extra_string(p, "webhook_url") {
            cfg.webhook_url = v.trim_end_matches('/').to_string();
        }
        cfg.bot_token = extra_string(p, "bot_token").or_else(|| platform_token_or_extra(p));
        if let Some(v) = extra_string(p, "webhook_secret") {
            cfg.webhook_secret = v;
        }
        if let Some(v) = extra_string(p, "bind_addr") {
            cfg.bind_addr = v;
        }
        if let Some(port) = p.extra.get("webhook_port").and_then(parse_port) {
            cfg.webhook_port = port;
        }
        if let Some(v) = extra_string(p, "webhook_path") {
            cfg.webhook_path = v;
        }
        cfg.enabled = p
            .extra
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(p.enabled);
        cfg
    }

    /// `bind_addr:webhook_port` the webhook listener binds to.
    pub fn bind_address(&self) -> String {
        format!("{}:{}", self.bind_addr, self.webhook_port)
    }
}

// ---------------------------------------------------------------------------
// TeamsConfig
// ---------------------------------------------------------------------------

/// Microsoft Teams (Bot Framework) adapter configuration.
///
/// Deserializes with `#[serde(default)]`; an empty `app_id` or `app_password`
/// fails adapter construction (fail-closed). The Bot Framework default
/// webhook port is 3978.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TeamsConfig {
    /// Bot Framework app (client) ID.
    pub app_id: String,
    /// Bot Framework app password (client secret): used for OAuth token
    /// acquisition and inbound Bearer-token validation.
    pub app_password: String,
    /// Local port for the `/api/messages` webhook listener.
    pub webhook_port: u16,
    /// Fallback Bot Framework service URL used for outbound calls when the
    /// target conversation has not supplied one via an inbound activity.
    pub service_url: String,
    /// Master switch for the adapter.
    pub enabled: bool,
}

impl Default for TeamsConfig {
    fn default() -> Self {
        Self {
            app_id: String::new(),
            app_password: String::new(),
            webhook_port: 3978,
            service_url: String::new(),
            enabled: false,
        }
    }
}

impl TeamsConfig {
    /// Build from a generic [`PlatformConfig`].
    ///
    /// `extra["app_password"]` wins; the first-class `token` (or
    /// `extra["token"]`) is the fallback. Everything else comes from `extra`.
    pub fn from_platform_config(p: &PlatformConfig) -> Self {
        let mut cfg = Self::default();
        if let Some(v) = extra_string(p, "app_id") {
            cfg.app_id = v;
        }
        cfg.app_password = extra_string(p, "app_password")
            .or_else(|| platform_token_or_extra(p))
            .unwrap_or_default();
        if let Some(port) = p
            .extra
            .get("webhook_port")
            .and_then(|v| v.as_u64())
            .and_then(|v| u16::try_from(v).ok())
        {
            cfg.webhook_port = port;
        }
        if let Some(v) = extra_string(p, "service_url") {
            cfg.service_url = v.trim_end_matches('/').to_string();
        }
        if let Some(v) = p.extra.get("enabled").and_then(|v| v.as_bool()) {
            cfg.enabled = v;
        }
        cfg
    }
}

// ---------------------------------------------------------------------------
// GoogleChatConfig
// ---------------------------------------------------------------------------

/// Google Chat adapter configuration.
///
/// Deserializes with `#[serde(default)]`. Inbound events arrive on a local
/// HTTP webhook listener (`bind_addr:webhook_port`) as Google Chat event
/// JSON; each request must carry `Authorization: Bearer <verification_token>`
/// or it is rejected (fail-closed). Outbound messages go to
/// `{webhook_url}/spaces/{space}/messages` with `bot_token` as the Bearer
/// token (a service-account OAuth2 access token in production).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GoogleChatConfig {
    /// Outgoing API base URL, e.g. `https://chat.googleapis.com/v1`.
    pub webhook_url: String,
    /// Bearer token for the Chat API (OAuth2 access token). Required;
    /// the adapter refuses to construct without one (fail-closed).
    pub bot_token: String,
    /// Token Google Chat must present as `Authorization: Bearer <token>` on
    /// inbound webhook calls. When empty, all inbound webhooks are rejected.
    pub verification_token: String,
    /// Local bind address for the inbound webhook listener.
    pub bind_addr: String,
    /// Local port for the inbound webhook listener.
    pub webhook_port: u16,
    /// Master switch for the adapter.
    pub enabled: bool,
}

impl Default for GoogleChatConfig {
    fn default() -> Self {
        Self {
            webhook_url: "https://chat.googleapis.com/v1".to_string(),
            bot_token: String::new(),
            verification_token: String::new(),
            bind_addr: "0.0.0.0".to_string(),
            webhook_port: 8647,
            enabled: false,
        }
    }
}

impl GoogleChatConfig {
    /// Build from a generic [`PlatformConfig`].
    ///
    /// `extra["bot_token"]` wins; the first-class `token` (or
    /// `extra["token"]`) is the fallback. `enabled` mirrors the platform
    /// entry's flag unless `extra["enabled"]` overrides it. Everything else
    /// comes from `extra`.
    pub fn from_platform_config(p: &PlatformConfig) -> Self {
        let mut cfg = Self::default();
        if let Some(v) = extra_string(p, "webhook_url") {
            cfg.webhook_url = v.trim_end_matches('/').to_string();
        }
        cfg.bot_token = extra_string(p, "bot_token")
            .or_else(|| platform_token_or_extra(p))
            .unwrap_or_default();
        if let Some(v) = extra_string(p, "verification_token") {
            cfg.verification_token = v;
        }
        if let Some(v) = extra_string(p, "bind_addr") {
            cfg.bind_addr = v;
        }
        if let Some(port) = p
            .extra
            .get("webhook_port")
            .and_then(|v| v.as_u64())
            .and_then(|v| u16::try_from(v).ok())
        {
            cfg.webhook_port = port;
        }
        cfg.enabled = p.enabled;
        if let Some(v) = p.extra.get("enabled").and_then(|v| v.as_bool()) {
            cfg.enabled = v;
        }
        cfg
    }
}

// ---------------------------------------------------------------------------
// SimplexConfig
// ---------------------------------------------------------------------------

/// SimpleX Chat adapter configuration.
///
/// Deserializes with `#[serde(default)]`. SimpleX bots talk to a locally
/// running SimpleX chat agent over WebSocket (`agent_url`, default
/// `ws://localhost:5225`). SimpleX has no user IDs: contacts are display
/// names (e.g. `@alice`) and double as conversation targets. An empty
/// `agent_url` fails adapter construction (fail-closed).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SimplexConfig {
    /// SimpleX chat agent WebSocket URL, e.g. `ws://localhost:5225`.
    pub agent_url: String,
    /// Default contact to message when `chat_id` is empty (e.g. `@alice`).
    pub contact: String,
    /// Master switch for the adapter.
    pub enabled: bool,
}

impl Default for SimplexConfig {
    fn default() -> Self {
        Self {
            agent_url: "ws://localhost:5225".to_string(),
            contact: String::new(),
            enabled: false,
        }
    }
}

impl SimplexConfig {
    /// Build from a generic [`PlatformConfig`].
    ///
    /// `extra["agent_url"]` and `extra["contact"]` set the agent endpoint and
    /// the default contact; `extra["enabled"]` (bool) flips the master switch.
    pub fn from_platform_config(p: &PlatformConfig) -> Self {
        let mut cfg = Self::default();
        if let Some(v) = extra_string(p, "agent_url") {
            cfg.agent_url = v.trim_end_matches('/').to_string();
        }
        if let Some(v) = extra_string(p, "contact") {
            cfg.contact = v;
        }
        if let Some(v) = p.extra.get("enabled").and_then(|v| v.as_bool()) {
            cfg.enabled = v;
        }
        cfg
    }
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
    fn line_config_defaults() {
        let cfg = LineConfig::default();
        assert_eq!(cfg.port, 8646);
        assert_eq!(cfg.path, "/line/webhook");
        assert_eq!(cfg.bind_addr, "0.0.0.0");
        assert!(cfg.channel_secret.is_empty());
        assert!(cfg.channel_access_token.is_empty());
    }

    #[test]
    fn ntfy_config_defaults() {
        let cfg = NtfyConfig::default();
        assert_eq!(cfg.server, "https://ntfy.sh");
        assert!(cfg.topic.is_empty());
        assert!(cfg.publish_topic.is_none());
        assert!(cfg.access_token.is_none());
        assert!(!cfg.markdown);
        assert_eq!(cfg.echo_tag, "hermes-agent");
        assert_eq!(cfg.effective_publish_topic(), "");
    }

    #[test]
    fn ntfy_config_serde_default() {
        let cfg: NtfyConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(cfg.server, "https://ntfy.sh");
        assert_eq!(cfg.echo_tag, "hermes-agent");
    }

    #[test]
    fn ntfy_config_from_platform_config() {
        let mut pc = PlatformConfig::default();
        pc.token = Some("top-level-token".to_string());
        for (k, v) in [
            ("server", "https://ntfy.example.com/"),
            ("topic", "alerts"),
            ("publish_topic", "replies"),
            ("username", "user"),
            ("title", "Hermes"),
        ] {
            pc.extra.insert(k.to_string(), json!(v));
        }
        pc.extra.insert("markdown".to_string(), json!(true));
        let cfg = NtfyConfig::from_platform_config(&pc);
        assert_eq!(cfg.server, "https://ntfy.example.com");
        assert_eq!(cfg.topic, "alerts");
        assert_eq!(cfg.publish_topic.as_deref(), Some("replies"));
        // first-class token falls back to access_token
        assert_eq!(cfg.access_token.as_deref(), Some("top-level-token"));
        assert_eq!(cfg.username.as_deref(), Some("user"));
        assert_eq!(cfg.title.as_deref(), Some("Hermes"));
        assert!(cfg.markdown);
        assert_eq!(cfg.effective_publish_topic(), "replies");
    }

    #[test]
    fn google_chat_config_defaults() {
        let cfg = GoogleChatConfig::default();
        assert_eq!(cfg.webhook_url, "https://chat.googleapis.com/v1");
        assert!(cfg.bot_token.is_empty());
        assert!(cfg.verification_token.is_empty());
        assert_eq!(cfg.bind_addr, "0.0.0.0");
        assert_eq!(cfg.webhook_port, 8647);
        assert!(!cfg.enabled);
    }

    #[test]
    fn google_chat_config_serde_default() {
        let cfg: GoogleChatConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(cfg.webhook_url, "https://chat.googleapis.com/v1");
        assert_eq!(cfg.webhook_port, 8647);
        assert!(!cfg.enabled);
    }

    #[test]
    fn google_chat_config_from_platform_config() {
        let mut pc = PlatformConfig::default();
        pc.enabled = true;
        pc.token = Some("top-level-token".to_string());
        for (k, v) in [
            ("webhook_url", "https://chat.example.com/v1/"),
            ("verification_token", "hook-secret"),
            ("bind_addr", "127.0.0.1"),
        ] {
            pc.extra.insert(k.to_string(), json!(v));
        }
        pc.extra.insert("webhook_port".to_string(), json!(9001));
        let cfg = GoogleChatConfig::from_platform_config(&pc);
        assert_eq!(cfg.webhook_url, "https://chat.example.com/v1");
        // first-class token falls back to bot_token
        assert_eq!(cfg.bot_token, "top-level-token");
        assert_eq!(cfg.verification_token, "hook-secret");
        assert_eq!(cfg.bind_addr, "127.0.0.1");
        assert_eq!(cfg.webhook_port, 9001);
        assert!(cfg.enabled);
    }

    #[test]
    fn google_chat_config_bot_token_prefers_extra() {
        let mut pc = PlatformConfig::default();
        pc.token = Some("top-level-token".to_string());
        pc.extra
            .insert("bot_token".to_string(), json!("extra-bot-token"));
        let cfg = GoogleChatConfig::from_platform_config(&pc);
        assert_eq!(cfg.bot_token, "extra-bot-token");
    }

    #[test]
    fn ntfy_config_access_token_prefers_extra() {
        let mut pc = PlatformConfig::default();
        pc.token = Some("top-level-token".to_string());
        pc.extra
            .insert("access_token".to_string(), json!("extra-token"));
        let cfg = NtfyConfig::from_platform_config(&pc);
        assert_eq!(cfg.access_token.as_deref(), Some("extra-token"));
        assert_eq!(cfg.effective_publish_topic(), "");
    }

    #[test]
    fn line_config_serde_default() {
        let cfg: LineConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(cfg.port, 8646);
        let cfg: LineConfig = serde_json::from_value(serde_json::json!({"port": 9001})).unwrap();
        assert_eq!(cfg.port, 9001);
    }

    #[test]
    fn teams_config_defaults() {
        let cfg = TeamsConfig::default();
        assert!(cfg.app_id.is_empty());
        assert!(cfg.app_password.is_empty());
        assert_eq!(cfg.webhook_port, 3978);
        assert!(cfg.service_url.is_empty());
        assert!(!cfg.enabled);
    }

    #[test]
    fn teams_config_from_platform_config() {
        let mut pc = PlatformConfig::default();
        pc.token = Some("top-level-token".to_string());
        pc.extra.insert("app_id".to_string(), json!("appid-123"));
        pc.extra.insert(
            "service_url".to_string(),
            json!("https://smba.trafficmanager.net/teams/"),
        );
        pc.extra.insert("webhook_port".to_string(), json!(5000));
        pc.extra.insert("enabled".to_string(), json!(true));
        let cfg = TeamsConfig::from_platform_config(&pc);
        assert_eq!(cfg.app_id, "appid-123");
        // first-class token falls back to app_password
        assert_eq!(cfg.app_password, "top-level-token");
        assert_eq!(cfg.webhook_port, 5000);
        assert_eq!(cfg.service_url, "https://smba.trafficmanager.net/teams");
        assert!(cfg.enabled);
    }

    #[test]
    fn teams_config_app_password_prefers_extra() {
        let mut pc = PlatformConfig::default();
        pc.token = Some("top-level-token".to_string());
        pc.extra
            .insert("app_password".to_string(), json!("secret-abc"));
        let cfg = TeamsConfig::from_platform_config(&pc);
        assert_eq!(cfg.app_password, "secret-abc");
    }

    #[test]
    fn buzz_config_defaults() {
        let cfg = BuzzConfig::default();
        assert!(cfg.webhook_url.is_empty());
        assert!(cfg.bot_token.is_none());
        assert!(cfg.webhook_secret.is_empty());
        assert_eq!(cfg.bind_addr, "127.0.0.1");
        assert_eq!(cfg.webhook_port, 8765);
        assert_eq!(cfg.webhook_path, "/buzz/webhook");
        assert!(cfg.enabled);
        assert_eq!(cfg.bind_address(), "127.0.0.1:8765");
    }

    #[test]
    fn buzz_config_serde_default() {
        let cfg: BuzzConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(cfg.webhook_port, 8765);
        assert_eq!(cfg.webhook_path, "/buzz/webhook");
        let cfg: BuzzConfig =
            serde_json::from_value(serde_json::json!({"webhook_port": 9001})).unwrap();
        assert_eq!(cfg.webhook_port, 9001);
    }

    #[test]
    fn buzz_config_from_platform_config() {
        let mut pc = PlatformConfig::default();
        pc.enabled = true;
        pc.token = Some("top-level-token".to_string());
        for (k, v) in [
            ("webhook_url", "https://buzz.example.com/api/"),
            ("webhook_secret", "s3cr3t"),
            ("bind_addr", "0.0.0.0"),
            ("webhook_path", "/hooks/buzz"),
        ] {
            pc.extra.insert(k.to_string(), json!(v));
        }
        pc.extra.insert("webhook_port".to_string(), json!(9123));
        let cfg = BuzzConfig::from_platform_config(&pc);
        assert_eq!(cfg.webhook_url, "https://buzz.example.com/api");
        assert_eq!(cfg.webhook_secret, "s3cr3t");
        assert_eq!(cfg.bind_addr, "0.0.0.0");
        assert_eq!(cfg.webhook_port, 9123);
        assert_eq!(cfg.webhook_path, "/hooks/buzz");
        assert!(cfg.enabled);
        // first-class token falls back to bot_token
        assert_eq!(cfg.bot_token.as_deref(), Some("top-level-token"));
        assert_eq!(cfg.bind_address(), "0.0.0.0:9123");
    }

    #[test]
    fn buzz_config_port_from_string_and_token_prefers_extra() {
        let mut pc = PlatformConfig::default();
        pc.token = Some("top-level-token".to_string());
        pc.extra.insert("webhook_port".to_string(), json!("9999"));
        pc.extra
            .insert("bot_token".to_string(), json!("extra-bot-token"));
        let cfg = BuzzConfig::from_platform_config(&pc);
        assert_eq!(cfg.webhook_port, 9999);
        assert_eq!(cfg.bot_token.as_deref(), Some("extra-bot-token"));
        // follows the top-level platform enabled flag
        assert!(!cfg.enabled);
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
