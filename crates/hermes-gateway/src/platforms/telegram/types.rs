//! Telegram adapter — Bot API types, TelegramConfig, and shared constants.

use serde::{Deserialize, Serialize};

use hermes_core::errors::GatewayError;

use crate::adapter::AdapterProxyConfig;

/// Default long-polling timeout in seconds.
pub(crate) const DEFAULT_POLL_TIMEOUT: u64 = 30;

// ---------------------------------------------------------------------------
// TelegramConfig
// ---------------------------------------------------------------------------

/// Configuration for the Telegram adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    /// Bot token from @BotFather.
    pub token: String,

    /// Optional webhook URL for receiving updates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,

    /// Whether to use polling (true) or webhooks (false) for updates.
    #[serde(default = "default_true")]
    pub polling: bool,

    /// Proxy configuration for outbound requests.
    #[serde(default)]
    pub proxy: AdapterProxyConfig,

    /// Whether to parse Markdown in messages.
    #[serde(default)]
    pub parse_markdown: bool,

    /// Whether to parse HTML in messages.
    #[serde(default)]
    pub parse_html: bool,

    /// Long-polling timeout in seconds.
    #[serde(default = "default_poll_timeout")]
    pub poll_timeout: u64,

    /// Bot username (without @), used for mention filtering in groups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_username: Option<String>,

    /// Config-file allowlist of Telegram user IDs (numeric, as strings)
    /// admitted in addition to the env-driven allowlist (see
    /// [`crate::allowlist`]). `None` (or an empty list) leaves the env
    /// allowlist as the sole gate — behavior unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_users: Option<Vec<String>>,

    /// When true, messages in group/supergroup chats are only forwarded to
    /// the agent when they mention the bot (`@bot_username`) or reply to
    /// the bot's own message. Defaults to false (all group messages are
    /// forwarded, preserving existing behavior); the gate is only active
    /// when explicitly enabled. Mentioned messages always have the
    /// `@botname` mention stripped before forwarding, regardless of this
    /// setting.
    #[serde(default)]
    pub group_require_mention: bool,

    /// Maximum number of commands registered in Telegram's `setMyCommands`
    /// menu. Commands in [`crate::commands::TELEGRAM_MENU_PRIORITY`] survive
    /// the cap first (in priority order); the rest fill alphabetically.
    /// Defaults to 60 when unset — mirrors the original, keeping the
    /// payload under Telegram's undocumented ~4KB menu limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_menu_max: Option<usize>,

    /// Include available skills in Telegram's `setMyCommands` menu (M-C2).
    /// When true, skill names (builtin skills merged with `SKILL.md` files
    /// discovered in the skills directory) are appended after the extension
    /// commands, in the non-priority alphabetical bucket; names colliding
    /// with builtin/extension commands are skipped. Defaults to true.
    #[serde(default = "default_true")]
    pub command_menu_skills: bool,

    /// Opt-in message reactions (M-T4): when true, the adapter sets a 👀
    /// reaction on an inbound message when processing starts, replacing it
    /// with 👍 once the agent's reply is delivered, or 👎 when processing
    /// fails (mirrors the original). Defaults to false (disabled).
    #[serde(default)]
    pub message_reactions: bool,
}

fn default_true() -> bool {
    true
}

fn default_poll_timeout() -> u64 {
    DEFAULT_POLL_TIMEOUT
}

// ---------------------------------------------------------------------------
// Telegram API types
// ---------------------------------------------------------------------------

/// Telegram API response wrapper.
#[derive(Debug, Deserialize)]
pub struct TelegramResponse<T> {
    pub ok: bool,
    pub result: Option<T>,
    pub description: Option<String>,
    #[serde(default)]
    pub error_code: Option<i64>,
    #[serde(default)]
    pub parameters: Option<ResponseParameters>,
}

/// Optional parameters returned on API errors (e.g. rate limiting).
#[derive(Debug, Clone, Deserialize)]
pub struct ResponseParameters {
    #[serde(default)]
    pub retry_after: Option<u64>,
    #[serde(default)]
    pub migrate_to_chat_id: Option<i64>,
}

/// Telegram Update object.
#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<TelegramMessage>,
    #[serde(default)]
    pub channel_post: Option<TelegramMessage>,
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
}

/// Telegram CallbackQuery from inline keyboard interactions.
#[derive(Debug, Clone, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    #[serde(default)]
    pub message: Option<TelegramMessage>,
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default)]
    pub chat_instance: Option<String>,
}

/// Telegram Message object.
#[derive(Debug, Clone, Deserialize)]
pub struct TelegramMessage {
    pub message_id: i64,
    pub chat: Chat,
    #[serde(default)]
    pub from: Option<User>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub voice: Option<Voice>,
    #[serde(default)]
    pub photo: Option<Vec<PhotoSize>>,
    #[serde(default)]
    pub caption: Option<String>,
    /// Formatting entities attached to `text` (M-G3).
    #[serde(default)]
    pub entities: Option<Vec<MessageEntity>>,
    /// Formatting entities attached to `caption` (M-G3).
    #[serde(default)]
    pub caption_entities: Option<Vec<MessageEntity>>,
    #[serde(default)]
    pub sticker: Option<Sticker>,
    #[serde(default)]
    pub reply_to_message: Option<Box<TelegramMessage>>,
    #[serde(default)]
    pub message_thread_id: Option<i64>,
    /// Chat on behalf of which the message was sent (anonymous group admins,
    /// channel-linked groups). `from` is `None` in that case (M-G2).
    #[serde(default)]
    pub sender_chat: Option<SenderChat>,
}

/// Telegram Chat object.
#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub chat_type: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
}

/// Telegram User object.
#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub is_bot: Option<bool>,
}

/// Telegram `sender_chat` object (minimal): the chat on behalf of which a
/// message was sent, e.g. an anonymous group admin or a linked channel.
#[derive(Debug, Clone, Deserialize)]
pub struct SenderChat {
    pub id: i64,
    #[serde(default)]
    pub title: Option<String>,
}

/// Telegram MessageEntity object: formatting markup attached to message text.
///
/// `offset` and `length` are in **UTF-16 code units** (Telegram convention),
/// not bytes or chars — see [`inline_text_link_urls`].
#[derive(Debug, Clone, Deserialize)]
pub struct MessageEntity {
    pub offset: u32,
    pub length: u32,
    /// Entity type: "text_link", "bold", "url", "mention", ...
    #[serde(rename = "type")]
    pub kind: String,
    /// Only present for "text_link" entities.
    #[serde(default)]
    pub url: Option<String>,
}

/// Telegram Voice object.
#[derive(Debug, Clone, Deserialize)]
pub struct Voice {
    pub file_id: String,
    pub file_unique_id: String,
    pub duration: u32,
}

/// Telegram PhotoSize object.
#[derive(Debug, Clone, Deserialize)]
pub struct PhotoSize {
    pub file_id: String,
    pub file_unique_id: String,
    pub width: u32,
    pub height: u32,
}

/// Telegram Sticker object.
#[derive(Debug, Clone, Deserialize)]
pub struct Sticker {
    pub file_id: String,
    pub file_unique_id: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub is_animated: Option<bool>,
    #[serde(default)]
    pub is_video: Option<bool>,
    #[serde(default)]
    pub emoji: Option<String>,
    #[serde(default)]
    pub set_name: Option<String>,
}

/// Telegram File object (from getFile).
#[derive(Debug, Clone, Deserialize)]
pub struct TelegramFile {
    pub file_id: String,
    #[serde(default)]
    pub file_path: Option<String>,
}

/// Telegram sent message result.
#[derive(Debug, Clone, Deserialize)]
pub struct SentMessage {
    pub message_id: i64,
}

/// Telegram ChatMember result.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatMember {
    pub status: String,
    pub user: User,
}

// ---------------------------------------------------------------------------
// Inline keyboard types
// ---------------------------------------------------------------------------

/// A single inline keyboard button.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineKeyboardButton {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// An inline keyboard markup containing rows of buttons.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineKeyboardMarkup {
    pub inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

// ---------------------------------------------------------------------------
// IncomingMessage
// ---------------------------------------------------------------------------

/// Incoming message parsed from a Telegram update.
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub chat_id: i64,
    pub user_id: Option<i64>,
    pub username: Option<String>,
    pub text: Option<String>,
    pub message_id: i64,
    pub is_voice: bool,
    pub is_photo: bool,
    pub is_sticker: bool,
    pub voice_file_id: Option<String>,
    pub photo_file_id: Option<String>,
    pub sticker_file_id: Option<String>,
    pub reply_to_message_id: Option<i64>,
    /// First ~500 chars of the replied-to message's text/caption, used as
    /// quote context in the incoming text (M-G4).
    pub reply_to_text: Option<String>,
    pub message_thread_id: Option<i64>,
    pub chat_type: ChatKind,
    pub is_group: bool,
    /// If this is a callback query, its ID (needed for `answerCallbackQuery`).
    pub callback_query_id: Option<String>,
    /// Data payload from a callback query button press.
    pub callback_data: Option<String>,
}

/// The kind of chat a message originated from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatKind {
    Private,
    Group,
    Supergroup,
    Channel,
    Unknown(String),
}

impl ChatKind {
    pub fn from_str(s: &str) -> Self {
        match s {
            "private" => ChatKind::Private,
            "group" => ChatKind::Group,
            "supergroup" => ChatKind::Supergroup,
            "channel" => ChatKind::Channel,
            other => ChatKind::Unknown(other.to_string()),
        }
    }

    pub fn is_group_like(&self) -> bool {
        matches!(self, ChatKind::Group | ChatKind::Supergroup)
    }
}

// ---------------------------------------------------------------------------
// PollResult – returned from poll_with_backoff
// ---------------------------------------------------------------------------

/// Outcome of a single `poll_with_backoff` call.
#[derive(Debug)]
pub enum PollResult {
    Updates(Vec<Update>),
    Backoff { error: GatewayError, delay_ms: u64 },
}

/// Maximum media caption length in Telegram (1024 characters).
pub(crate) const MAX_CAPTION_LENGTH: usize = 1024;

/// Maximum characters of a replied-to message included as quote context.
pub(crate) const REPLY_QUOTE_MAX_CHARS: usize = 500;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platforms::telegram::test_helpers::*;
    use crate::platforms::telegram::*;

    #[test]
    fn chat_kind_from_str_variants() {
        assert_eq!(ChatKind::from_str("private"), ChatKind::Private);
        assert_eq!(ChatKind::from_str("group"), ChatKind::Group);
        assert_eq!(ChatKind::from_str("supergroup"), ChatKind::Supergroup);
        assert_eq!(ChatKind::from_str("channel"), ChatKind::Channel);
        assert_eq!(
            ChatKind::from_str("something"),
            ChatKind::Unknown("something".into())
        );
    }
    #[test]
    fn chat_kind_is_group_like() {
        assert!(!ChatKind::Private.is_group_like());
        assert!(ChatKind::Group.is_group_like());
        assert!(ChatKind::Supergroup.is_group_like());
        assert!(!ChatKind::Channel.is_group_like());
        assert!(!ChatKind::Unknown("x".into()).is_group_like());
    }
    #[test]
    fn inline_keyboard_serialization() {
        let kb = InlineKeyboardMarkup {
            inline_keyboard: vec![
                vec![
                    InlineKeyboardButton {
                        text: "Option A".into(),
                        callback_data: Some("a".into()),
                        url: None,
                    },
                    InlineKeyboardButton {
                        text: "Option B".into(),
                        callback_data: Some("b".into()),
                        url: None,
                    },
                ],
                vec![InlineKeyboardButton {
                    text: "Visit".into(),
                    callback_data: None,
                    url: Some("https://example.com".into()),
                }],
            ],
        };

        let json = serde_json::to_value(&kb).unwrap();
        let rows = json["inline_keyboard"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].as_array().unwrap().len(), 2);
        assert_eq!(rows[0][0]["text"], "Option A");
        assert_eq!(rows[0][0]["callback_data"], "a");
        assert!(rows[0][0].get("url").is_none());
        assert_eq!(rows[1][0]["url"], "https://example.com");
        assert!(rows[1][0].get("callback_data").is_none());
    }
    #[test]
    fn inline_keyboard_deserialization() {
        let json = r#"{
            "inline_keyboard": [
                [{"text": "Go", "callback_data": "go"}],
                [{"text": "Link", "url": "https://x.com"}]
            ]
        }"#;
        let kb: InlineKeyboardMarkup = serde_json::from_str(json).unwrap();
        assert_eq!(kb.inline_keyboard.len(), 2);
        assert_eq!(kb.inline_keyboard[0][0].text, "Go");
        assert_eq!(kb.inline_keyboard[0][0].callback_data, Some("go".into()));
        assert_eq!(kb.inline_keyboard[1][0].url, Some("https://x.com".into()));
    }
    #[test]
    fn sticker_deserialization() {
        let json = r#"{
            "file_id": "stk_1",
            "file_unique_id": "stk_u1",
            "width": 512,
            "height": 512,
            "is_animated": true,
            "emoji": "🔥",
            "set_name": "HotPack"
        }"#;
        let sticker: Sticker = serde_json::from_str(json).unwrap();
        assert_eq!(sticker.file_id, "stk_1");
        assert_eq!(sticker.emoji, Some("🔥".into()));
        assert_eq!(sticker.is_animated, Some(true));
        assert_eq!(sticker.is_video, None);
    }
    #[test]
    fn sticker_deserialization_minimal() {
        let json = r#"{"file_id": "s1", "file_unique_id": "su1"}"#;
        let sticker: Sticker = serde_json::from_str(json).unwrap();
        assert_eq!(sticker.file_id, "s1");
        assert!(sticker.width.is_none());
        assert!(sticker.emoji.is_none());
    }
    #[test]
    fn callback_query_deserialization() {
        let json = r#"{
            "id": "cq_999",
            "from": {"id": 123, "first_name": "Alice", "is_bot": false},
            "data": "pressed_ok",
            "chat_instance": "ci"
        }"#;
        let cq: CallbackQuery = serde_json::from_str(json).unwrap();
        assert_eq!(cq.id, "cq_999");
        assert_eq!(cq.from.id, 123);
        assert_eq!(cq.data, Some("pressed_ok".into()));
        assert!(cq.message.is_none());
    }
    #[test]
    fn update_with_callback_query_deser() {
        let json = r#"{
            "update_id": 50,
            "callback_query": {
                "id": "cq_1",
                "from": {"id": 1, "first_name": "Bob"},
                "message": {
                    "message_id": 77,
                    "chat": {"id": 88, "type": "private"},
                    "text": "Pick one"
                },
                "data": "choice_a"
            }
        }"#;
        let update: Update = serde_json::from_str(json).unwrap();
        assert!(update.message.is_none());
        let cq = update.callback_query.as_ref().unwrap();
        assert_eq!(cq.id, "cq_1");
        assert_eq!(cq.data, Some("choice_a".into()));
        assert_eq!(cq.message.as_ref().unwrap().message_id, 77);
    }
    #[test]
    fn telegram_response_rate_limit_params() {
        let json = r#"{
            "ok": false,
            "description": "Too Many Requests: retry after 5",
            "parameters": {"retry_after": 5}
        }"#;
        let resp: TelegramResponse<serde_json::Value> = serde_json::from_str(json).unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.parameters.as_ref().unwrap().retry_after, Some(5));
    }
    #[test]
    fn telegram_response_no_params() {
        let json = r#"{"ok": true, "result": 42}"#;
        let resp: TelegramResponse<i32> = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.result, Some(42));
        assert!(resp.parameters.is_none());
    }
    #[test]
    fn chat_with_title() {
        let json = r#"{"id": 1, "type": "supergroup", "title": "My Group", "username": "mygrp"}"#;
        let chat: Chat = serde_json::from_str(json).unwrap();
        assert_eq!(chat.id, 1);
        assert_eq!(chat.chat_type, "supergroup");
        assert_eq!(chat.title, Some("My Group".into()));
        assert_eq!(chat.username, Some("mygrp".into()));
    }
    #[test]
    fn telegram_message_full_deser() {
        let json = r#"{
            "message_id": 10,
            "chat": {"id": 1, "type": "private"},
            "from": {"id": 2, "first_name": "X", "is_bot": false},
            "text": "hi",
            "message_thread_id": 42,
            "reply_to_message": {
                "message_id": 5,
                "chat": {"id": 1, "type": "private"},
                "text": "earlier"
            }
        }"#;
        let msg: TelegramMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.message_id, 10);
        assert_eq!(msg.message_thread_id, Some(42));
        let reply = msg.reply_to_message.as_ref().unwrap();
        assert_eq!(reply.message_id, 5);
        assert_eq!(reply.text, Some("earlier".into()));
    }
    #[test]
    fn user_is_bot_field() {
        let json = r#"{"id": 1, "first_name": "BotX", "is_bot": true}"#;
        let user: User = serde_json::from_str(json).unwrap();
        assert_eq!(user.is_bot, Some(true));
    }
    #[test]
    fn chat_member_deser() {
        let json = r#"{
            "status": "administrator",
            "user": {"id": 1, "first_name": "Admin"}
        }"#;
        let member: ChatMember = serde_json::from_str(json).unwrap();
        assert_eq!(member.status, "administrator");
        assert_eq!(member.user.id, 1);
    }
    #[test]
    fn config_defaults() {
        let json = r#"{"token": "abc"}"#;
        let cfg: TelegramConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.token, "abc");
        assert!(cfg.polling);
        assert!(!cfg.parse_markdown);
        assert!(!cfg.parse_html);
        assert_eq!(cfg.poll_timeout, DEFAULT_POLL_TIMEOUT);
        assert!(cfg.bot_username.is_none());
        // M-C3: menu cap defaults to None -> sending path falls back to 60.
        assert!(cfg.command_menu_max.is_none());
        // M-T4: reactions are opt-in — default off.
        assert!(!cfg.message_reactions);
    }
    #[test]
    fn config_message_reactions_opt_in() {
        let json = r#"{"token": "abc", "message_reactions": true}"#;
        let cfg: TelegramConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.message_reactions);
    }
    #[test]
    fn config_with_command_menu_max() {
        let json = r#"{"token": "abc", "command_menu_max": 25}"#;
        let cfg: TelegramConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.command_menu_max, Some(25));
    }
    #[test]
    fn config_with_bot_username() {
        let json = r#"{"token": "abc", "bot_username": "mybot"}"#;
        let cfg: TelegramConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.bot_username, Some("mybot".into()));
    }
}
