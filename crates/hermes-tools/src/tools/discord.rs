//! `discord` and `discord_admin`: read and participate in a Discord server via the
//! REST API (bot token).
//!
//! Port of `tools/discord_tool.py`. Both tools share one action table over
//! `https://discord.com/api/v10` with `Authorization: Bot <token>`:
//! - `discord` = the core participation trio (`fetch_messages`, `search_members`, `create_thread`)
//! - `discord_admin` = everything else (guild/channel/role introspection and moderation)
//!
//! Simplifications vs. the Python original (deliberate):
//! - No privileged-intent capability detection (`GET /applications/@me`): the
//!   fork has no dynamic-schema machinery, so all actions of each family are
//!   statically exposed. Actions that need the GUILD_MEMBERS intent fail at
//!   call time with guidance, and missing MESSAGE_CONTENT intent surfaces
//!   empty `content` from Discord itself.
//! - The `discord.server_actions` config allowlist is an env var here:
//!   `DISCORD_SERVER_ACTIONS` (comma-separated). It is enforced at execute
//!   time; the schema always lists the family's actions.
//! - Call-time 403s are enriched with per-action guidance, mirroring
//!   `_enrich_403` from the original.

use async_trait::async_trait;
use indexmap::IndexMap;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use std::collections::HashSet;

use hermes_core::{tool_schema, JsonSchema, ToolError, ToolHandler, ToolSchema};

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
const USER_AGENT: &str = "Hermes-Agent (https://github.com/NousResearch/hermes-agent)";
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const REQUEST_TIMEOUT_SECS: u64 = 15;

/// One row of the shared action table: handler name, required-param
/// signature (also rendered into the schema), description, and whether it
/// belongs to `discord_admin` (false = core `discord` action).
struct ActionDef {
    name: &'static str,
    signature: &'static str,
    description: &'static str,
    admin: bool,
}

/// Single source of truth: (action, required-param signature, description).
/// Order is the schema/enum order.
const ACTIONS: &[ActionDef] = &[
    ActionDef {
        name: "list_guilds",
        signature: "()",
        description: "list servers the bot is in",
        admin: true,
    },
    ActionDef {
        name: "server_info",
        signature: "(guild_id)",
        description: "server details + member counts",
        admin: true,
    },
    ActionDef {
        name: "list_channels",
        signature: "(guild_id)",
        description: "all channels grouped by category",
        admin: true,
    },
    ActionDef {
        name: "channel_info",
        signature: "(channel_id)",
        description: "single channel details",
        admin: true,
    },
    ActionDef {
        name: "list_roles",
        signature: "(guild_id)",
        description: "roles sorted by position",
        admin: true,
    },
    ActionDef {
        name: "member_info",
        signature: "(guild_id, user_id)",
        description: "lookup a specific member",
        admin: true,
    },
    ActionDef {
        name: "search_members",
        signature: "(guild_id, query)",
        description: "find members by name prefix",
        admin: false,
    },
    ActionDef {
        name: "fetch_messages",
        signature: "(channel_id)",
        description: "recent messages; optional before/after snowflakes",
        admin: false,
    },
    ActionDef {
        name: "list_pins",
        signature: "(channel_id)",
        description: "pinned messages in a channel",
        admin: true,
    },
    ActionDef {
        name: "pin_message",
        signature: "(channel_id, message_id)",
        description: "pin a message",
        admin: true,
    },
    ActionDef {
        name: "unpin_message",
        signature: "(channel_id, message_id)",
        description: "unpin a message",
        admin: true,
    },
    ActionDef {
        name: "delete_message",
        signature: "(channel_id, message_id)",
        description: "delete a message",
        admin: true,
    },
    ActionDef {
        name: "create_thread",
        signature: "(channel_id, name)",
        description: "create a public thread; optional message_id anchor",
        admin: false,
    },
    ActionDef {
        name: "add_role",
        signature: "(guild_id, user_id, role_id)",
        description: "assign a role",
        admin: true,
    },
    ActionDef {
        name: "remove_role",
        signature: "(guild_id, user_id, role_id)",
        description: "remove a role",
        admin: true,
    },
];

fn required_params(signature: &str) -> Vec<&str> {
    signature
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect()
}

fn family_actions(admin: bool) -> Vec<&'static ActionDef> {
    ACTIONS.iter().filter(|a| a.admin == admin).collect()
}

/// Comma-separated `DISCORD_SERVER_ACTIONS` allowlist; `None` = unrestricted.
/// Unknown names are dropped.
fn allowed_actions() -> Option<HashSet<String>> {
    let raw = std::env::var("DISCORD_SERVER_ACTIONS").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let known: HashSet<&str> = ACTIONS.iter().map(|a| a.name).collect();
    let names: HashSet<String> = trimmed
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && known.contains(s.as_str()))
        .collect();
    Some(names)
}

fn _channel_type_name(type_id: i64) -> String {
    match type_id {
        0 => "text".into(),
        2 => "voice".into(),
        4 => "category".into(),
        5 => "announcement".into(),
        10 => "announcement_thread".into(),
        11 => "public_thread".into(),
        12 => "private_thread".into(),
        13 => "stage".into(),
        15 => "forum".into(),
        16 => "media".into(),
        other => format!("unknown({other})"),
    }
}

/// Discord caps list endpoints at 100 per page.
fn clamp_limit(value: Option<&Value>, default: i64) -> i64 {
    let n = value
        .and_then(|v| v.as_i64().or_else(|| v.as_u64().map(|u| u as i64)))
        .unwrap_or(default);
    n.clamp(1, 100)
}

fn str_param<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Member row; `full` adds the avatar/join fields `member_info` exposes.
fn member_summary(m: &Value, full: bool) -> Value {
    let user = m.get("user").cloned().unwrap_or(Value::Null);
    let mut row = json!({
        "user_id": user.get("id"),
        "username": user.get("username"),
        "display_name": user.get("global_name"),
        "nickname": m.get("nick"),
        "avatar": user.get("avatar"),
        "bot": user.get("bot").and_then(|b| b.as_bool()).unwrap_or(false),
        "roles": m.get("roles").cloned().unwrap_or(json!([])),
        "joined_at": m.get("joined_at"),
        "premium_since": m.get("premium_since"),
    });
    if !full {
        if let Value::Object(ref mut map) = row {
            for k in ["avatar", "joined_at", "premium_since"] {
                map.remove(k);
            }
        }
    }
    row
}

fn message_summary(msg: &Value) -> Value {
    let author = msg.get("author").cloned().unwrap_or(Value::Null);
    let attachments: Vec<Value> = msg
        .get("attachments")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .map(|a| json!({"filename": a.get("filename"), "url": a.get("url"), "size": a.get("size")}))
                .collect()
        })
        .unwrap_or_default();
    let reactions: Vec<Value> = msg
        .get("reactions")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .map(|r| {
                    json!({
                        "emoji": r.get("emoji").and_then(|e| e.get("name")),
                        "count": r.get("count").and_then(|c| c.as_u64()).unwrap_or(0)
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "id": msg.get("id"),
        "content": msg.get("content").and_then(|c| c.as_str()).unwrap_or(""),
        "author": {
            "id": author.get("id"),
            "username": author.get("username"),
            "display_name": author.get("global_name"),
            "bot": author.get("bot").and_then(|b| b.as_bool()).unwrap_or(false),
        },
        "timestamp": msg.get("timestamp"),
        "edited_timestamp": msg.get("edited_timestamp"),
        "attachments": attachments,
        "reactions": reactions,
        "pinned": msg.get("pinned").and_then(|p| p.as_bool()).unwrap_or(false),
    })
}

/// Discord REST error with status code and (truncated) body.
struct DiscordApiError {
    status: u16,
    body: String,
}

impl std::fmt::Display for DiscordApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Discord API error {}: {}", self.status, self.body)
    }
}

// Per-action guidance for a call-time 403 (per-guild permissions are never pre-checked).
fn enrich_403(action: &str, body: &str) -> String {
    let no_manage_messages = "Bot lacks MANAGE_MESSAGES permission in this channel";
    let view_history =
        "Bot cannot view this channel (missing VIEW_CHANNEL or READ_MESSAGE_HISTORY).";
    let role_hierarchy = "Either the bot lacks MANAGE_ROLES, or the target role sits higher than the bot's highest role.";
    let hint: Option<String> = match action {
        "pin_message" => Some(format!(
            "{no_manage_messages}. Ask the server admin to grant the bot a role that has MANAGE_MESSAGES, or a per-channel overwrite.")),
        "unpin_message" => Some(no_manage_messages.to_string()),
        "delete_message" => Some(format!("{no_manage_messages}, or cannot view the channel/message.")),
        "create_thread" => Some("Bot lacks CREATE_PUBLIC_THREADS in this channel, or cannot view it.".to_string()),
        "add_role" => Some(format!(
            "{role_hierarchy} Roles can only be assigned below the bot's own position in the role hierarchy.")),
        "remove_role" => Some(role_hierarchy.to_string()),
        "fetch_messages" => Some(view_history.to_string()),
        "list_pins" => Some(view_history.to_string()),
        "channel_info" => Some("Bot cannot view this channel (missing VIEW_CHANNEL).".to_string()),
        "search_members" => Some(
            "Likely missing the Server Members privileged intent — enable it in the Discord Developer Portal under your bot's settings.".to_string()),
        "member_info" => Some(
            "Bot cannot see this guild member (missing Server Members intent or insufficient permissions).".to_string()),
        _ => None,
    };
    let base = format!("Discord API 403 (forbidden) on '{action}'.");
    match hint {
        Some(h) => format!("{base} {h} (Raw: {body})"),
        None => format!("{base} (Raw: {body})"),
    }
}

/// Shared Discord REST client backing both tools.
pub struct DiscordTool {
    tool_name: &'static str,
    admin: bool,
    token: Option<String>,
    client: reqwest::Client,
}

impl DiscordTool {
    pub fn new(tool_name: &'static str, admin: bool) -> Self {
        Self {
            tool_name,
            admin,
            token: std::env::var("DISCORD_BOT_TOKEN")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            client: reqwest::Client::new(),
        }
    }

    pub fn with_token(tool_name: &'static str, admin: bool, token: impl Into<String>) -> Self {
        Self {
            tool_name,
            admin,
            token: Some(token.into()),
            client: reqwest::Client::new(),
        }
    }

    fn actions(&self) -> Vec<&'static ActionDef> {
        family_actions(self.admin)
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
        token: &str,
    ) -> Result<Option<Value>, DiscordApiError> {
        let mut url = format!("{DISCORD_API_BASE}{path}");
        if !query.is_empty() {
            let qs: Vec<String> = query
                .iter()
                .map(|(k, v)| format!("{}={}", k, urlencoding(v)))
                .collect();
            url.push('?');
            url.push_str(&qs.join("&"));
        }
        let mut req = self
            .client
            .request(method, &url)
            .header("Authorization", format!("Bot {token}"))
            .header("User-Agent", USER_AGENT)
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS));
        if let Some(b) = body {
            req = req.header("Content-Type", "application/json").json(b);
        }
        let resp = req.send().await.map_err(|e| DiscordApiError {
            status: 0,
            body: format!("request failed: {e}"),
        })?;
        let status = resp.status();
        if status == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        let bytes = resp.bytes().await.map_err(|e| DiscordApiError {
            status: status.as_u16(),
            body: format!("failed to read response body: {e}"),
        })?;
        if !status.is_success() {
            let mut body_text =
                String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_ERROR_BODY_BYTES)])
                    .into_owned();
            if bytes.len() > MAX_ERROR_BODY_BYTES {
                body_text.push_str("…(truncated)");
            }
            return Err(DiscordApiError {
                status: status.as_u16(),
                body: body_text,
            });
        }
        if bytes.len() > MAX_BODY_BYTES {
            return Err(DiscordApiError {
                status: 502,
                body: format!("Discord API response body exceeded {MAX_BODY_BYTES} bytes."),
            });
        }
        let text = String::from_utf8_lossy(&bytes);
        serde_json::from_str::<Value>(&text)
            .map(Some)
            .map_err(|e| DiscordApiError {
                status: status.as_u16(),
                body: format!("invalid JSON response: {e}"),
            })
    }

    // -- read actions ---------------------------------------------------------

    async fn list_guilds(&self, token: &str) -> Result<String, DiscordApiError> {
        let guilds = self
            .request(Method::GET, "/users/@me/guilds", &[], None, token)
            .await?
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        let items: Vec<Value> = guilds
            .iter()
            .map(|g| {
                json!({
                    "id": g.get("id"),
                    "name": g.get("name"),
                    "icon": g.get("icon"),
                    "owner": g.get("owner").and_then(|o| o.as_bool()).unwrap_or(false),
                    "permissions": g.get("permissions"),
                })
            })
            .collect();
        Ok(listing("guilds", items))
    }

    async fn server_info(&self, token: &str, guild_id: &str) -> Result<String, DiscordApiError> {
        let g = self
            .request(
                Method::GET,
                &format!("/guilds/{guild_id}"),
                &[("with_counts", "true".to_string())],
                None,
                token,
            )
            .await?
            .unwrap_or(Value::Null);
        Ok(json!({
            "id": g.get("id"),
            "name": g.get("name"),
            "description": g.get("description"),
            "icon": g.get("icon"),
            "owner_id": g.get("owner_id"),
            "member_count": g.get("approximate_member_count"),
            "online_count": g.get("approximate_presence_count"),
            "features": g.get("features").cloned().unwrap_or(json!([])),
            "premium_tier": g.get("premium_tier"),
            "premium_subscription_count": g.get("premium_subscription_count"),
            "verification_level": g.get("verification_level"),
        })
        .to_string())
    }

    async fn list_channels(&self, token: &str, guild_id: &str) -> Result<String, DiscordApiError> {
        let channels = self
            .request(
                Method::GET,
                &format!("/guilds/{guild_id}/channels"),
                &[],
                None,
                token,
            )
            .await?
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        Ok(group_channels(&channels).to_string())
    }

    async fn channel_info(&self, token: &str, channel_id: &str) -> Result<String, DiscordApiError> {
        let ch = self
            .request(
                Method::GET,
                &format!("/channels/{channel_id}"),
                &[],
                None,
                token,
            )
            .await?
            .unwrap_or(Value::Null);
        Ok(json!({
            "id": ch.get("id"),
            "name": ch.get("name"),
            "type": _channel_type_name(ch.get("type").and_then(|t| t.as_i64()).unwrap_or(-1)),
            "guild_id": ch.get("guild_id"),
            "topic": ch.get("topic"),
            "nsfw": ch.get("nsfw").and_then(|n| n.as_bool()).unwrap_or(false),
            "position": ch.get("position"),
            "parent_id": ch.get("parent_id"),
            "rate_limit_per_user": ch.get("rate_limit_per_user").and_then(|r| r.as_u64()).unwrap_or(0),
            "last_message_id": ch.get("last_message_id"),
        })
        .to_string())
    }

    async fn list_roles(&self, token: &str, guild_id: &str) -> Result<String, DiscordApiError> {
        let mut roles = self
            .request(
                Method::GET,
                &format!("/guilds/{guild_id}/roles"),
                &[],
                None,
                token,
            )
            .await?
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        roles.sort_by_key(|r| {
            std::cmp::Reverse(r.get("position").and_then(|p| p.as_i64()).unwrap_or(0))
        });
        let items: Vec<Value> = roles
            .iter()
            .map(|r| {
                let color = r.get("color").and_then(|c| c.as_u64()).unwrap_or(0);
                json!({
                    "id": r.get("id"),
                    "name": r.get("name"),
                    "color": if color != 0 { Some(format!("#{color:06x}")) } else { None },
                    "position": r.get("position").and_then(|p| p.as_i64()).unwrap_or(0),
                    "mentionable": r.get("mentionable").and_then(|m| m.as_bool()).unwrap_or(false),
                    "managed": r.get("managed").and_then(|m| m.as_bool()).unwrap_or(false),
                    "member_count": r.get("member_count"),
                    "hoist": r.get("hoist").and_then(|h| h.as_bool()).unwrap_or(false),
                })
            })
            .collect();
        Ok(listing("roles", items))
    }

    async fn member_info(
        &self,
        token: &str,
        guild_id: &str,
        user_id: &str,
    ) -> Result<String, DiscordApiError> {
        let m = self
            .request(
                Method::GET,
                &format!("/guilds/{guild_id}/members/{user_id}"),
                &[],
                None,
                token,
            )
            .await?
            .unwrap_or(Value::Null);
        Ok(member_summary(&m, true).to_string())
    }

    async fn search_members(
        &self,
        token: &str,
        guild_id: &str,
        query: &str,
        limit: i64,
    ) -> Result<String, DiscordApiError> {
        let members = self
            .request(
                Method::GET,
                &format!("/guilds/{guild_id}/members/search"),
                &[("query", query.to_string()), ("limit", limit.to_string())],
                None,
                token,
            )
            .await?
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        let items: Vec<Value> = members.iter().map(|m| member_summary(m, false)).collect();
        Ok(listing("members", items))
    }

    async fn fetch_messages(
        &self,
        token: &str,
        channel_id: &str,
        params: &Value,
    ) -> Result<String, DiscordApiError> {
        let limit = clamp_limit(params.get("limit"), 50);
        let mut q = vec![("limit", limit.to_string())];
        if let Some(b) = str_param(params, "before") {
            q.push(("before", b.to_string()));
        }
        if let Some(a) = str_param(params, "after") {
            q.push(("after", a.to_string()));
        }
        let messages = self
            .request(
                Method::GET,
                &format!("/channels/{channel_id}/messages"),
                &q,
                None,
                token,
            )
            .await?
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        let items: Vec<Value> = messages.iter().map(message_summary).collect();
        Ok(listing("messages", items))
    }

    async fn list_pins(&self, token: &str, channel_id: &str) -> Result<String, DiscordApiError> {
        let messages = self
            .request(
                Method::GET,
                &format!("/channels/{channel_id}/pins"),
                &[],
                None,
                token,
            )
            .await?
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        let items: Vec<Value> = messages
            .iter()
            .map(|msg| {
                let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
                let truncated: String = content.chars().take(200).collect();
                json!({
                    "id": msg.get("id"),
                    "content": truncated,
                    "author": msg.get("author").and_then(|a| a.get("username")),
                    "timestamp": msg.get("timestamp"),
                })
            })
            .collect();
        Ok(listing("pinned_messages", items))
    }

    // -- write actions --------------------------------------------------------

    async fn create_thread(
        &self,
        token: &str,
        channel_id: &str,
        name: &str,
        params: &Value,
    ) -> Result<String, DiscordApiError> {
        let auto_archive: i64 = params
            .get("auto_archive_duration")
            .and_then(|v| v.as_i64())
            .unwrap_or(1440);
        let mut body = json!({"name": name, "auto_archive_duration": auto_archive});
        let path = match str_param(params, "message_id") {
            Some(message_id) => format!("/channels/{channel_id}/messages/{message_id}/threads"),
            None => {
                body["type"] = json!(11); // PUBLIC_THREAD
                format!("/channels/{channel_id}/threads")
            }
        };
        let thread = self
            .request(Method::POST, &path, &[], Some(&body), token)
            .await?
            .unwrap_or(Value::Null);
        Ok(
            json!({"success": true, "thread_id": thread.get("id"), "name": thread.get("name")})
                .to_string(),
        )
    }

    async fn mutation(
        &self,
        method: Method,
        path: String,
        message: String,
        token: &str,
    ) -> Result<String, DiscordApiError> {
        self.request(method, &path, &[], None, token).await?;
        Ok(json!({"success": true, "message": message}).to_string())
    }

    // -- dispatch -------------------------------------------------------------

    async fn dispatch(
        &self,
        action: &str,
        params: &Value,
        token: &str,
    ) -> Result<String, DiscordApiError> {
        let p = |key: &str| -> &str { str_param(params, key).unwrap_or("") };
        match action {
            "list_guilds" => self.list_guilds(token).await,
            "server_info" => self.server_info(token, p("guild_id")).await,
            "list_channels" => self.list_channels(token, p("guild_id")).await,
            "channel_info" => self.channel_info(token, p("channel_id")).await,
            "list_roles" => self.list_roles(token, p("guild_id")).await,
            "member_info" => self.member_info(token, p("guild_id"), p("user_id")).await,
            "search_members" => {
                self.search_members(
                    token,
                    p("guild_id"),
                    p("query"),
                    clamp_limit(params.get("limit"), 20),
                )
                .await
            }
            "fetch_messages" => self.fetch_messages(token, p("channel_id"), params).await,
            "list_pins" => self.list_pins(token, p("channel_id")).await,
            "pin_message" => {
                let (channel_id, message_id) = (p("channel_id"), p("message_id"));
                self.mutation(
                    Method::PUT,
                    format!("/channels/{channel_id}/pins/{message_id}"),
                    format!("Message {message_id} pinned."),
                    token,
                )
                .await
            }
            "unpin_message" => {
                let (channel_id, message_id) = (p("channel_id"), p("message_id"));
                self.mutation(
                    Method::DELETE,
                    format!("/channels/{channel_id}/pins/{message_id}"),
                    format!("Message {message_id} unpinned."),
                    token,
                )
                .await
            }
            "delete_message" => {
                let (channel_id, message_id) = (p("channel_id"), p("message_id"));
                self.mutation(
                    Method::DELETE,
                    format!("/channels/{channel_id}/messages/{message_id}"),
                    format!("Message {message_id} deleted."),
                    token,
                )
                .await
            }
            "create_thread" => {
                self.create_thread(token, p("channel_id"), p("name"), params)
                    .await
            }
            "add_role" => {
                let (guild_id, user_id, role_id) = (p("guild_id"), p("user_id"), p("role_id"));
                self.mutation(
                    Method::PUT,
                    format!("/guilds/{guild_id}/members/{user_id}/roles/{role_id}"),
                    format!("Role {role_id} added to user {user_id}."),
                    token,
                )
                .await
            }
            "remove_role" => {
                let (guild_id, user_id, role_id) = (p("guild_id"), p("user_id"), p("role_id"));
                self.mutation(
                    Method::DELETE,
                    format!("/guilds/{guild_id}/members/{user_id}/roles/{role_id}"),
                    format!("Role {role_id} removed from user {user_id}."),
                    token,
                )
                .await
            }
            other => Err(DiscordApiError {
                status: 0,
                body: format!("unknown action: {other}"),
            }),
        }
    }

    async fn run_action(&self, params: Value) -> Result<String, ToolError> {
        let token = self
            .token
            .clone()
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| {
                ToolError::ExecutionFailed("DISCORD_BOT_TOKEN not configured.".into())
            })?;
        let action = str_param(&params, "action").unwrap_or("").to_string();
        let valid: Vec<&str> = self.actions().iter().map(|a| a.name).collect();
        let def = self
            .actions()
            .into_iter()
            .find(|a| a.name == action)
            .ok_or_else(|| {
                ToolError::InvalidParams(format!(
                    "Unknown action: '{action}'. Available: {}",
                    valid.join(", ")
                ))
            })?;
        // Config-level allowlist gate (defense in depth): a stale schema from a
        // prior config must not let denied actions through.
        if let Some(allowlist) = allowed_actions() {
            if !allowlist.contains(action.as_str()) {
                let allowed = allowlist.iter().cloned().collect::<Vec<_>>().join(", ");
                return Err(ToolError::InvalidParams(format!(
                    "Action '{action}' is disabled by config (DISCORD_SERVER_ACTIONS). Allowed: {}",
                    if allowed.is_empty() {
                        "<none>"
                    } else {
                        &allowed
                    }
                )));
            }
        }
        let missing: Vec<&str> = required_params(def.signature)
            .into_iter()
            .filter(|p| str_param(&params, p).is_none())
            .collect();
        if !missing.is_empty() {
            return Err(ToolError::InvalidParams(format!(
                "Missing required parameters for '{action}': {}",
                missing.join(", ")
            )));
        }
        match self.dispatch(action.as_str(), &params, &token).await {
            Ok(out) => Ok(out),
            Err(e) if e.status == 403 => Err(ToolError::ExecutionFailed(enrich_403(
                action.as_str(),
                &e.body,
            ))),
            Err(e) => Err(ToolError::ExecutionFailed(e.to_string())),
        }
    }

    fn schema(&self) -> ToolSchema {
        let actions = self.actions();
        let action_names: Vec<&str> = actions.iter().map(|a| a.name).collect();
        let manifest_block: String = actions
            .iter()
            .map(|a| format!("  {}{}  — {}", a.name, a.signature, a.description))
            .collect::<Vec<_>>()
            .join("\n");
        let (lead, guidance) = if self.admin {
            (
                "Manage a Discord server via the REST API.",
                "Call list_guilds first to discover guild_ids, then list_channels for channel_ids. \
                 Runtime errors will tell you if the bot lacks a specific per-guild permission \
                 (e.g. MANAGE_ROLES for add_role).",
            )
        } else {
            (
                "Read and participate in a Discord server.",
                "Use the channel_id from the current conversation context. \
                 Use search_members to look up user IDs by name prefix.",
            )
        };
        let mut props = IndexMap::new();
        props.insert(
            "action".into(),
            json!({"type": "string", "enum": action_names}),
        );
        props.insert(
            "guild_id".into(),
            json!({"type": "string", "description": "Discord server (guild) ID."}),
        );
        props.insert(
            "channel_id".into(),
            json!({"type": "string", "description": "Discord channel ID."}),
        );
        props.insert(
            "user_id".into(),
            json!({"type": "string", "description": "Discord user ID."}),
        );
        props.insert(
            "role_id".into(),
            json!({"type": "string", "description": "Discord role ID."}),
        );
        props.insert(
            "message_id".into(),
            json!({"type": "string", "description": "Discord message ID."}),
        );
        props.insert(
            "query".into(),
            json!({"type": "string", "description": "Member name prefix to search for (search_members)."}),
        );
        props.insert(
            "name".into(),
            json!({"type": "string", "description": "New thread name (create_thread)."}),
        );
        props.insert(
            "limit".into(),
            json!({
                "type": "integer",
                "minimum": 1,
                "maximum": 100,
                "description": "Max results (default 50). Applies to fetch_messages, search_members."
            }),
        );
        props.insert(
            "before".into(),
            json!({"type": "string", "description": "Snowflake ID for reverse pagination (fetch_messages)."}),
        );
        props.insert(
            "after".into(),
            json!({"type": "string", "description": "Snowflake ID for forward pagination (fetch_messages)."}),
        );
        props.insert(
            "auto_archive_duration".into(),
            json!({
                "type": "integer",
                "enum": [60, 1440, 4320, 10080],
                "description": "Thread archive duration in minutes (create_thread, default 1440)."
            }),
        );
        tool_schema(
            self.tool_name,
            format!("{lead}\n\nAvailable actions:\n{manifest_block}\n\n{guidance}"),
            JsonSchema::object(props, vec!["action".into()]),
        )
    }
}

/// `discord`: the core participation trio (fetch_messages, search_members, create_thread).
pub struct DiscordCoreHandler {
    inner: DiscordTool,
}

/// `discord_admin`: server management actions.
pub struct DiscordAdminHandler {
    inner: DiscordTool,
}

impl DiscordCoreHandler {
    pub fn new() -> Self {
        Self {
            inner: DiscordTool::new("discord", false),
        }
    }
    pub fn with_token(token: impl Into<String>) -> Self {
        Self {
            inner: DiscordTool::with_token("discord", false, token),
        }
    }
}

impl Default for DiscordCoreHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl DiscordAdminHandler {
    pub fn new() -> Self {
        Self {
            inner: DiscordTool::new("discord_admin", true),
        }
    }
    pub fn with_token(token: impl Into<String>) -> Self {
        Self {
            inner: DiscordTool::with_token("discord_admin", true, token),
        }
    }
}

impl Default for DiscordAdminHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolHandler for DiscordCoreHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        self.inner.run_action(params).await
    }
    fn schema(&self) -> ToolSchema {
        self.inner.schema()
    }
}

#[async_trait]
impl ToolHandler for DiscordAdminHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        self.inner.run_action(params).await
    }
    fn schema(&self) -> ToolSchema {
        self.inner.schema()
    }
}

/// `{"key": items, "count": n}` listing envelope.
fn listing(key: &str, items: Vec<Value>) -> String {
    json!({ key: items, "count": items.len() }).to_string()
}

/// All channels grouped by category (uncategorized first), each sorted by position.
fn group_channels(channels: &[Value]) -> Value {
    let mut cats: Vec<&Value> = channels
        .iter()
        .filter(|ch| ch.get("type").and_then(|t| t.as_i64()) == Some(4))
        .collect();
    cats.sort_by_key(|c| c.get("position").and_then(|p| p.as_i64()).unwrap_or(0));

    let cat_ids: Vec<String> = cats
        .iter()
        .filter_map(|c| c.get("id").and_then(|i| i.as_str()).map(|s| s.to_string()))
        .collect();

    let mut uncategorized: Vec<Value> = Vec::new();
    let mut by_cat: Vec<(String, Vec<Value>)> =
        cat_ids.iter().map(|id| (id.clone(), Vec::new())).collect();

    for ch in channels {
        let t = ch.get("type").and_then(|t| t.as_i64()).unwrap_or(-1);
        if t == 4 {
            continue; // category header itself
        }
        let row = json!({
            "id": ch.get("id"),
            "name": ch.get("name").and_then(|n| n.as_str()).unwrap_or(""),
            "type": _channel_type_name(t),
            "position": ch.get("position").and_then(|p| p.as_i64()).unwrap_or(0),
            "topic": ch.get("topic"),
            "nsfw": ch.get("nsfw").and_then(|n| n.as_bool()).unwrap_or(false),
        });
        let parent = ch
            .get("parent_id")
            .and_then(|p| p.as_str())
            .map(|s| s.to_string());
        match parent.and_then(|pid| by_cat.iter_mut().find(|(id, _)| *id == pid)) {
            Some((_, group)) => group.push(row),
            None => uncategorized.push(row),
        }
    }
    let sort_group = |g: &mut Vec<Value>| {
        g.sort_by_key(|c| c.get("position").and_then(|p| p.as_i64()).unwrap_or(0));
    };
    sort_group(&mut uncategorized);
    for (_, g) in by_cat.iter_mut() {
        sort_group(g);
    }

    let mut result: Vec<Value> = Vec::new();
    if !uncategorized.is_empty() {
        result.push(json!({"category": null, "channels": uncategorized}));
    }
    for (cat, cat_id) in cats.iter().zip(cat_ids.iter()) {
        let group = by_cat
            .iter()
            .find(|(id, _)| id == cat_id)
            .map(|(_, g)| g.clone())
            .unwrap_or_default();
        result.push(json!({
            "category": {"id": cat_id, "name": cat.get("name")},
            "channels": group,
        }));
    }
    let total: usize = result
        .iter()
        .map(|g| {
            g.get("channels")
                .and_then(|c| c.as_array())
                .map(|a| a.len())
                .unwrap_or(0)
        })
        .sum();
    json!({"channel_groups": result, "total_channels": total})
}

/// Minimal percent-encoding for query values (mirrors urllib.parse.urlencode).
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Tool is available only when a Discord bot token is configured.
pub fn check_discord_requirements() -> bool {
    std::env::var("DISCORD_BOT_TOKEN")
        .map(|t| !t.trim().is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_schema_has_participation_trio() {
        let s = DiscordCoreHandler::new().schema();
        assert_eq!(s.name, "discord");
        let props = s.parameters.properties.as_ref().unwrap();
        let action_enum = props
            .get("action")
            .and_then(|a| a.get("enum"))
            .and_then(|e| e.as_array())
            .cloned()
            .unwrap_or_default();
        let names: Vec<String> = action_enum
            .iter()
            .filter_map(|v| v.as_str().map(|x| x.to_string()))
            .collect();
        assert_eq!(
            names,
            vec!["search_members", "fetch_messages", "create_thread"]
        );
        assert!(s
            .description
            .contains("Read and participate in a Discord server."));
    }

    #[test]
    fn admin_schema_has_twelve_actions() {
        let s = DiscordAdminHandler::new().schema();
        assert_eq!(s.name, "discord_admin");
        let props = s.parameters.properties.as_ref().unwrap();
        let action_enum = props
            .get("action")
            .and_then(|a| a.get("enum"))
            .and_then(|e| e.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(action_enum.len(), 12);
        let names: Vec<&str> = action_enum.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"list_guilds"));
        assert!(names.contains(&"add_role"));
        assert!(!names.contains(&"fetch_messages"));
        assert!(s
            .description
            .contains("Manage a Discord server via the REST API."));
    }

    #[test]
    fn required_params_parsed() {
        assert!(required_params("()").is_empty());
        assert_eq!(required_params("(guild_id)"), vec!["guild_id"]);
        assert_eq!(
            required_params("(guild_id, user_id, role_id)"),
            vec!["guild_id", "user_id", "role_id"]
        );
    }

    #[test]
    fn clamp_limit_caps_at_100() {
        assert_eq!(clamp_limit(Some(&json!(500)), 50), 100);
        assert_eq!(clamp_limit(Some(&json!(0)), 50), 1);
        assert_eq!(clamp_limit(None, 50), 50);
        assert_eq!(clamp_limit(Some(&json!(20)), 50), 20);
    }

    #[test]
    fn channel_type_names() {
        assert_eq!(_channel_type_name(0), "text");
        assert_eq!(_channel_type_name(11), "public_thread");
        assert_eq!(_channel_type_name(99), "unknown(99)");
    }

    #[test]
    fn member_summary_brief_drops_avatar_join_fields() {
        let m = json!({
            "user": {"id": "1", "username": "u", "global_name": "U", "avatar": "a", "bot": false},
            "nick": "nick", "roles": ["r1"], "joined_at": "t", "premium_since": null
        });
        let brief = member_summary(&m, false);
        assert!(brief.get("avatar").is_none());
        assert!(brief.get("joined_at").is_none());
        assert_eq!(brief.get("username").and_then(|v| v.as_str()), Some("u"));
        let full = member_summary(&m, true);
        assert_eq!(full.get("avatar").and_then(|v| v.as_str()), Some("a"));
    }

    #[test]
    fn message_summary_shape() {
        let msg = json!({
            "id": "m1", "content": "hi",
            "author": {"id": "u1", "username": "u", "global_name": "U", "bot": false},
            "timestamp": "ts", "edited_timestamp": null,
            "attachments": [{"filename": "f.png", "url": "http://x", "size": 10}],
            "reactions": [{"emoji": {"name": "👍"}, "count": 3}],
            "pinned": true
        });
        let s = message_summary(&msg);
        assert_eq!(s.get("content").and_then(|v| v.as_str()), Some("hi"));
        assert_eq!(s.get("pinned").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            s.get("attachments")
                .and_then(|a| a.as_array())
                .map(|a| a.len()),
            Some(1)
        );
        assert_eq!(
            s.get("reactions")
                .and_then(|r| r.as_array())
                .and_then(|a| a.first())
                .and_then(|r| r.get("emoji"))
                .and_then(|e| e.as_str()),
            Some("👍")
        );
    }

    #[test]
    fn group_channels_groups_by_category_sorted() {
        let channels = vec![
            json!({"id": "ch2", "name": "b", "type": 0, "position": 2, "parent_id": "cat1"}),
            json!({"id": "ch1", "name": "a", "type": 0, "position": 1, "parent_id": "cat1"}),
            json!({"id": "cat1", "name": "Cat", "type": 4, "position": 0}),
            json!({"id": "ch0", "name": "top", "type": 0, "position": 0}),
        ];
        let g = group_channels(&channels);
        assert_eq!(g.get("total_channels").and_then(|v| v.as_u64()), Some(3));
        let groups = g.get("channel_groups").and_then(|v| v.as_array()).unwrap();
        // uncategorized first
        assert!(groups[0].get("category").unwrap().is_null());
        let cat_channels = groups[1]
            .get("channels")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(
            cat_channels[0].get("id").and_then(|v| v.as_str()),
            Some("ch1")
        );
        assert_eq!(
            cat_channels[1].get("id").and_then(|v| v.as_str()),
            Some("ch2")
        );
    }

    #[test]
    fn enrich_403_includes_guidance() {
        let msg = enrich_403("add_role", "{\"message\": \"Missing Permissions\"}");
        assert!(msg.contains("MANAGE_ROLES"));
        assert!(msg.contains("403"));
        let plain = enrich_403("list_guilds", "forbidden");
        assert!(plain.contains("list_guilds"));
    }

    #[tokio::test]
    async fn missing_token_fails_closed() {
        let h = DiscordTool::with_token("discord", false, "   ");
        // empty token after trim -> token becomes Some("") via with_token; run_action must fail
        let err = h
            .run_action(json!({"action": "list_guilds"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("DISCORD_BOT_TOKEN"));
    }

    #[tokio::test]
    async fn unknown_action_rejected() {
        let h = DiscordTool::with_token("discord", false, "tok");
        let err = h
            .run_action(json!({"action": "nuke_server"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Unknown action"));
    }

    #[tokio::test]
    async fn core_tool_rejects_admin_action() {
        let h = DiscordCoreHandler::with_token("tok");
        let err = h
            .execute(json!({"action": "add_role", "guild_id": "g", "user_id": "u", "role_id": "r"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Unknown action"));
    }

    #[tokio::test]
    async fn missing_required_params_rejected() {
        let h = DiscordAdminHandler::with_token("tok");
        let err = h
            .execute(json!({"action": "server_info"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("guild_id"));
    }
}
