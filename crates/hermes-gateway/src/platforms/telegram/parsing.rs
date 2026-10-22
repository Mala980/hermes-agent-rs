//! Telegram adapter — update/message parsing.

use tracing::debug;

use super::types::*;
use super::TelegramAdapter;

/// Inline `text_link` entity URLs after their anchor text, e.g.
/// `anchor (https://example.com)` (M-G3).
///
/// Telegram entity offsets are in **UTF-16 code units**, so the text is
/// expanded to UTF-16, entity ranges are spliced there, and the result is
/// decoded back to a `String`. Entities are applied from the end of the
/// text backwards so earlier offsets stay valid. Malformed entities (out
/// of bounds, overflow, or missing URL) are skipped with a debug log —
/// this never panics.
pub fn inline_text_link_urls(text: &str, entities: &[MessageEntity]) -> String {
    let mut links: Vec<(usize, usize, &str)> = Vec::new();
    for e in entities {
        if e.kind != "text_link" {
            continue;
        }
        match e.url.as_deref() {
            Some(url) => links.push((e.offset as usize, e.length as usize, url)),
            None => debug!("Telegram: text_link entity without url, skipping"),
        }
    }
    if links.is_empty() {
        return text.to_string();
    }

    // Apply back-to-front so splices don't shift pending offsets.
    links.sort_by_key(|a| std::cmp::Reverse(a.0));

    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    for (offset, length, url) in links {
        let end = match offset.checked_add(length) {
            Some(end) => end,
            None => {
                debug!(
                    offset,
                    length, "Telegram: text_link entity offset overflow, skipping"
                );
                continue;
            }
        };
        if length == 0 || end > utf16.len() {
            debug!(
                offset,
                length,
                text_len_utf16 = utf16.len(),
                "Telegram: text_link entity out of bounds, skipping"
            );
            continue;
        }
        let anchor: String = char::decode_utf16(utf16[offset..end].iter().copied())
            .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect();
        let replacement: Vec<u16> = format!("{anchor} ({url})").encode_utf16().collect();
        utf16.splice(offset..end, replacement);
    }
    String::from_utf16_lossy(&utf16)
}

impl TelegramAdapter {
    /// Parse a Telegram Update into an IncomingMessage.
    ///
    /// Handles regular messages, channel posts (parsed like messages, with
    /// the channel id as the user id), and callback queries.
    pub fn parse_update(update: &Update) -> Option<IncomingMessage> {
        if let Some(ref cq) = update.callback_query {
            return Self::parse_callback_query(cq);
        }

        // Channel posts are parsed exactly like regular messages. They carry
        // no `from` user, so the channel's own id becomes the user id for a
        // stable fail-closed allowlist identifier.
        let (msg, is_channel_post) = match (&update.message, &update.channel_post) {
            (Some(m), _) => (m, false),
            (None, Some(m)) => (m, true),
            (None, None) => return None,
        };

        let mut parsed = Self::parse_telegram_message(msg, None)?;
        if is_channel_post && parsed.user_id.is_none() {
            parsed.user_id = Some(msg.chat.id);
        }
        Some(parsed)
    }
    /// Parse a regular `TelegramMessage` into `IncomingMessage`.
    pub(crate) fn parse_telegram_message(
        msg: &TelegramMessage,
        callback: Option<(&str, &str)>,
    ) -> Option<IncomingMessage> {
        let raw_text = msg.text.clone().or_else(|| msg.caption.clone());
        // Inline `text_link` URLs after their anchor text. Entities attach
        // to `text`; caption entities attach to `caption`.
        let entities = if msg.text.is_some() {
            msg.entities.as_deref()
        } else {
            msg.caption_entities.as_deref()
        };
        let text = raw_text.map(|t| match entities {
            Some(ents) if !ents.is_empty() => inline_text_link_urls(&t, ents),
            _ => t,
        });

        // Anonymous-admin messages carry `from: None` but include
        // `sender_chat`; fall back to it so the fail-closed allowlist sees
        // an identifier instead of silently dropping the message.
        let user_id = msg
            .from
            .as_ref()
            .map(|u| u.id)
            .or_else(|| msg.sender_chat.as_ref().map(|c| c.id));
        let username = msg
            .from
            .as_ref()
            .and_then(|u| u.username.clone())
            .or_else(|| msg.sender_chat.as_ref().and_then(|c| c.title.clone()));
        if msg.from.is_none() {
            if let Some(sc) = msg.sender_chat.as_ref() {
                debug!(
                    sender_chat_id = sc.id,
                    "Telegram: using sender_chat as message author (from is None)"
                );
            }
        }

        let is_voice = msg.voice.is_some();
        let voice_file_id = msg.voice.as_ref().map(|v| v.file_id.clone());

        let is_photo = msg.photo.is_some();
        let photo_file_id = msg
            .photo
            .as_ref()
            .and_then(|photos| photos.last().map(|p| p.file_id.clone()));

        let is_sticker = msg.sticker.is_some();
        let sticker_file_id = msg.sticker.as_ref().map(|s| s.file_id.clone());

        let reply_to_message_id = msg.reply_to_message.as_ref().map(|r| r.message_id);

        // Quoted context (M-G4): the agent only receives flattened text, so
        // include the replied-to message's text/caption (first ~500 chars).
        let reply_to_text = msg.reply_to_message.as_ref().and_then(|r| {
            let quoted = r.text.clone().or_else(|| r.caption.clone())?;
            Some(quoted.chars().take(REPLY_QUOTE_MAX_CHARS).collect())
        });

        let chat_type = ChatKind::from_str(&msg.chat.chat_type);
        let is_group = chat_type.is_group_like();

        let (cb_id, cb_data) = match callback {
            Some((id, data)) => (Some(id.to_string()), Some(data.to_string())),
            None => (None, None),
        };

        Some(IncomingMessage {
            chat_id: msg.chat.id,
            user_id,
            username,
            text,
            message_id: msg.message_id,
            is_voice,
            is_photo,
            is_sticker,
            voice_file_id,
            photo_file_id,
            sticker_file_id,
            reply_to_message_id,
            reply_to_text,
            message_thread_id: msg.message_thread_id,
            chat_type,
            is_group,
            callback_query_id: cb_id,
            callback_data: cb_data,
        })
    }
    /// Parse a `CallbackQuery` into an `IncomingMessage`.
    pub(crate) fn parse_callback_query(cq: &CallbackQuery) -> Option<IncomingMessage> {
        let msg = cq.message.as_ref();
        let chat_id = msg.map(|m| m.chat.id).unwrap_or(0);
        let message_id = msg.map(|m| m.message_id).unwrap_or(0);

        let chat_type = msg
            .map(|m| ChatKind::from_str(&m.chat.chat_type))
            .unwrap_or(ChatKind::Private);
        let is_group = chat_type.is_group_like();

        Some(IncomingMessage {
            chat_id,
            user_id: Some(cq.from.id),
            username: cq.from.username.clone(),
            text: cq.data.clone(),
            message_id,
            is_voice: false,
            is_photo: false,
            is_sticker: false,
            voice_file_id: None,
            photo_file_id: None,
            sticker_file_id: None,
            reply_to_message_id: None,
            reply_to_text: None,
            message_thread_id: msg.and_then(|m| m.message_thread_id),
            chat_type,
            is_group,
            callback_query_id: Some(cq.id.clone()),
            callback_data: cq.data.clone(),
        })
    }
    /// Convert a Telegram `Update` into the platform-agnostic gateway
    /// [`crate::gateway::IncomingMessage`].
    ///
    /// Returns `None` when the update carries no parseable message or
    /// callback query (same condition as [`TelegramAdapter::parse_update`]).
    pub fn update_to_incoming(update: &Update) -> Option<crate::gateway::IncomingMessage> {
        let parsed = Self::parse_update(update)?;
        Some(Self::parsed_to_incoming(&parsed))
    }
    /// Convert a parsed telegram-local [`IncomingMessage`] into the
    /// platform-agnostic gateway [`crate::gateway::IncomingMessage`].
    ///
    /// Field mapping: `platform` is always `"telegram"`; numeric Telegram IDs
    /// become strings; `is_dm` is true only for private chats; callback
    /// queries carry the button `data` as `text`.
    pub fn parsed_to_incoming(parsed: &IncomingMessage) -> crate::gateway::IncomingMessage {
        // Prepend the quoted reply context so the agent sees what the user
        // was responding to (M-G4).
        let mut text = parsed.text.clone().unwrap_or_default();
        if let Some(quoted) = parsed.reply_to_text.as_deref() {
            if !quoted.is_empty() {
                text = format!("[Replying to: {quoted}]\n{text}");
            }
        }
        crate::gateway::IncomingMessage {
            platform: "telegram".to_string(),
            chat_id: parsed.chat_id.to_string(),
            user_id: parsed.user_id.map(|u| u.to_string()).unwrap_or_default(),
            text,
            message_id: Some(parsed.message_id.to_string()),
            is_dm: parsed.chat_type == ChatKind::Private,
        }
    }
    /// Check whether a text message mentions this bot (for group filtering).
    ///
    /// Returns `true` if the message contains `@bot_username` or if the
    /// bot_username is not configured (pass-through).
    pub fn is_mentioned_in(&self, text: &str) -> bool {
        match self.config.bot_username {
            Some(ref bot_user) => {
                let mention = format!("@{}", bot_user);
                text.contains(&mention)
            }
            None => true,
        }
    }
    /// Strip the bot mention from text, returning the cleaned message.
    pub fn strip_mention(&self, text: &str) -> String {
        match self.config.bot_username {
            Some(ref bot_user) => {
                let mention = format!("@{}", bot_user);
                text.replace(&mention, "").trim().to_string()
            }
            None => text.to_string(),
        }
    }
    /// Group trigger gate decision for group/supergroup messages.
    ///
    /// When `group_require_mention` is false (the default) the gate is
    /// disabled and all group messages forward (existing behavior).
    /// Otherwise the message must mention the bot or reply to the bot.
    pub(crate) fn group_trigger_satisfied(&self, mentioned: bool, reply_to_bot: bool) -> bool {
        !self.config.group_require_mention || mentioned || reply_to_bot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platforms::telegram::test_helpers::*;
    use crate::platforms::telegram::*;

    #[test]
    fn parse_update_text_message() {
        let update = Update {
            update_id: 1,
            channel_post: None,
            message: Some(make_text_message(
                42,
                make_chat(100, "private"),
                make_user(200, Some("testuser")),
                "hello bot",
            )),
            callback_query: None,
        };

        let incoming = TelegramAdapter::parse_update(&update).unwrap();
        assert_eq!(incoming.chat_id, 100);
        assert_eq!(incoming.user_id, Some(200));
        assert_eq!(incoming.text, Some("hello bot".into()));
        assert!(!incoming.is_voice);
        assert!(!incoming.is_photo);
        assert!(!incoming.is_sticker);
        assert!(!incoming.is_group);
        assert_eq!(incoming.chat_type, ChatKind::Private);
        assert!(incoming.callback_query_id.is_none());
    }
    #[test]
    fn parse_update_voice_message() {
        let update = Update {
            update_id: 2,
            channel_post: None,
            message: Some(TelegramMessage {
                message_id: 43,
                chat: make_chat(100, "private"),
                from: Some(make_user(200, None)),
                text: None,
                voice: Some(Voice {
                    file_id: "voice123".into(),
                    file_unique_id: "unique123".into(),
                    duration: 5,
                }),
                photo: None,
                caption: None,
                sticker: None,
                reply_to_message: None,
                sender_chat: None,
                entities: None,
                caption_entities: None,
                message_thread_id: None,
            }),
            callback_query: None,
        };

        let incoming = TelegramAdapter::parse_update(&update).unwrap();
        assert!(incoming.is_voice);
        assert_eq!(incoming.voice_file_id, Some("voice123".into()));
    }
    #[test]
    fn parse_update_photo_message() {
        let update = Update {
            update_id: 3,
            channel_post: None,
            message: Some(TelegramMessage {
                message_id: 44,
                chat: make_chat(100, "group"),
                from: Some(make_user(200, None)),
                text: None,
                voice: None,
                photo: Some(vec![
                    PhotoSize {
                        file_id: "small".into(),
                        file_unique_id: "s1".into(),
                        width: 90,
                        height: 90,
                    },
                    PhotoSize {
                        file_id: "large".into(),
                        file_unique_id: "s2".into(),
                        width: 800,
                        height: 600,
                    },
                ]),
                caption: Some("my photo".into()),
                sticker: None,
                reply_to_message: None,
                sender_chat: None,
                entities: None,
                caption_entities: None,
                message_thread_id: None,
            }),
            callback_query: None,
        };

        let incoming = TelegramAdapter::parse_update(&update).unwrap();
        assert!(incoming.is_photo);
        assert_eq!(incoming.photo_file_id, Some("large".into()));
        assert_eq!(incoming.text, Some("my photo".into()));
        assert!(incoming.is_group);
        assert_eq!(incoming.chat_type, ChatKind::Group);
    }
    #[test]
    fn parse_update_no_message() {
        let update = Update {
            update_id: 4,
            channel_post: None,
            message: None,
            callback_query: None,
        };
        assert!(TelegramAdapter::parse_update(&update).is_none());
    }
    #[test]
    fn parse_update_sticker_message() {
        let update = Update {
            update_id: 10,
            channel_post: None,
            message: Some(TelegramMessage {
                message_id: 100,
                chat: make_chat(300, "private"),
                from: Some(make_user(400, Some("stickeruser"))),
                text: None,
                voice: None,
                photo: None,
                caption: None,
                sticker: Some(Sticker {
                    file_id: "sticker_abc".into(),
                    file_unique_id: "su_abc".into(),
                    width: Some(512),
                    height: Some(512),
                    is_animated: Some(false),
                    is_video: Some(false),
                    emoji: Some("😀".into()),
                    set_name: Some("TestPack".into()),
                }),
                reply_to_message: None,
                sender_chat: None,
                entities: None,
                caption_entities: None,
                message_thread_id: None,
            }),
            callback_query: None,
        };

        let incoming = TelegramAdapter::parse_update(&update).unwrap();
        assert!(incoming.is_sticker);
        assert_eq!(incoming.sticker_file_id, Some("sticker_abc".into()));
        assert!(!incoming.is_voice);
        assert!(!incoming.is_photo);
    }
    #[test]
    fn parse_update_callback_query() {
        let update = Update {
            update_id: 20,
            channel_post: None,
            message: None,
            callback_query: Some(CallbackQuery {
                id: "cq_123".into(),
                from: make_user(500, Some("cbuser")),
                message: Some(TelegramMessage {
                    message_id: 200,
                    chat: make_chat(600, "private"),
                    from: None,
                    text: Some("Original message".into()),
                    voice: None,
                    photo: None,
                    caption: None,
                    sticker: None,
                    reply_to_message: None,
                    sender_chat: None,
                    entities: None,
                    caption_entities: None,
                    message_thread_id: None,
                }),
                data: Some("btn_action_1".into()),
                chat_instance: Some("inst".into()),
            }),
        };

        let incoming = TelegramAdapter::parse_update(&update).unwrap();
        assert_eq!(incoming.callback_query_id, Some("cq_123".into()));
        assert_eq!(incoming.callback_data, Some("btn_action_1".into()));
        assert_eq!(incoming.user_id, Some(500));
        assert_eq!(incoming.chat_id, 600);
        assert_eq!(incoming.message_id, 200);
        assert_eq!(incoming.text, Some("btn_action_1".into()));
    }
    #[test]
    fn parse_update_callback_query_no_message() {
        let update = Update {
            update_id: 21,
            channel_post: None,
            message: None,
            callback_query: Some(CallbackQuery {
                id: "cq_456".into(),
                from: make_user(500, None),
                message: None,
                data: Some("data".into()),
                chat_instance: None,
            }),
        };

        let incoming = TelegramAdapter::parse_update(&update).unwrap();
        assert_eq!(incoming.callback_query_id, Some("cq_456".into()));
        assert_eq!(incoming.chat_id, 0);
        assert_eq!(incoming.message_id, 0);
    }
    #[test]
    fn update_to_incoming_text_message() {
        let update = Update {
            update_id: 40,
            channel_post: None,
            message: Some(make_text_message(
                11,
                make_chat(700, "private"),
                make_user(500, Some("cbuser")),
                "hello",
            )),
            callback_query: None,
        };

        let gw = TelegramAdapter::update_to_incoming(&update).unwrap();
        assert_eq!(gw.platform, "telegram");
        assert_eq!(gw.chat_id, "700");
        assert_eq!(gw.user_id, "500");
        assert_eq!(gw.text, "hello");
        assert_eq!(gw.message_id, Some("11".into()));
        assert!(gw.is_dm);
    }
    #[test]
    fn update_to_incoming_callback_query() {
        let update = Update {
            update_id: 41,
            channel_post: None,
            message: None,
            callback_query: Some(CallbackQuery {
                id: "cq_789".into(),
                from: make_user(501, Some("btnuser")),
                message: Some(TelegramMessage {
                    message_id: 201,
                    chat: make_chat(601, "private"),
                    from: None,
                    text: Some("Pick one".into()),
                    voice: None,
                    photo: None,
                    caption: None,
                    sticker: None,
                    reply_to_message: None,
                    sender_chat: None,
                    entities: None,
                    caption_entities: None,
                    message_thread_id: None,
                }),
                data: Some("btn_action_2".into()),
                chat_instance: None,
            }),
        };

        // The callback query must be answerable: parse_update keeps its id.
        let parsed = TelegramAdapter::parse_update(&update).unwrap();
        assert_eq!(parsed.callback_query_id, Some("cq_789".into()));

        let gw = TelegramAdapter::update_to_incoming(&update).unwrap();
        assert_eq!(gw.platform, "telegram");
        assert_eq!(gw.chat_id, "601");
        assert_eq!(gw.user_id, "501");
        // Button data becomes the message text for the agent loop.
        assert_eq!(gw.text, "btn_action_2");
        assert_eq!(gw.message_id, Some("201".into()));
        assert!(gw.is_dm);
    }
    #[test]
    fn update_to_incoming_group_is_not_dm() {
        let update = Update {
            update_id: 42,
            channel_post: None,
            message: Some(make_text_message(
                12,
                make_chat(800, "supergroup"),
                make_user(502, None),
                "hi all",
            )),
            callback_query: None,
        };

        let gw = TelegramAdapter::update_to_incoming(&update).unwrap();
        assert_eq!(gw.chat_id, "800");
        assert_eq!(gw.text, "hi all");
        assert!(!gw.is_dm);
    }
    #[test]
    fn update_to_incoming_media_without_text_defaults_empty() {
        let mut msg = make_text_message(13, make_chat(700, "private"), make_user(503, None), "x");
        msg.text = None;
        msg.voice = Some(Voice {
            file_id: "voice123".into(),
            file_unique_id: "vu123".into(),
            duration: 5,
        });
        let update = Update {
            update_id: 43,
            channel_post: None,
            message: Some(msg),
            callback_query: None,
        };

        let gw = TelegramAdapter::update_to_incoming(&update).unwrap();
        assert_eq!(gw.text, "");
        assert_eq!(gw.message_id, Some("13".into()));
    }
    #[test]
    fn update_to_incoming_empty_update_is_none() {
        let update = Update {
            update_id: 44,
            channel_post: None,
            message: None,
            callback_query: None,
        };
        assert!(TelegramAdapter::update_to_incoming(&update).is_none());
    }
    #[test]
    fn parse_update_with_reply_and_thread() {
        let reply_msg = TelegramMessage {
            message_id: 10,
            chat: make_chat(100, "supergroup"),
            from: Some(make_user(50, None)),
            text: Some("original".into()),
            voice: None,
            photo: None,
            caption: None,
            sticker: None,
            reply_to_message: None,
            sender_chat: None,
            entities: None,
            caption_entities: None,
            message_thread_id: None,
        };

        let update = Update {
            update_id: 30,
            channel_post: None,
            message: Some(TelegramMessage {
                message_id: 55,
                chat: make_chat(100, "supergroup"),
                from: Some(make_user(200, None)),
                text: Some("replying".into()),
                voice: None,
                photo: None,
                caption: None,
                sticker: None,
                reply_to_message: Some(Box::new(reply_msg)),
                sender_chat: None,
                entities: None,
                caption_entities: None,
                message_thread_id: Some(999),
            }),
            callback_query: None,
        };

        let incoming = TelegramAdapter::parse_update(&update).unwrap();
        assert_eq!(incoming.reply_to_message_id, Some(10));
        assert_eq!(incoming.message_thread_id, Some(999));
        assert!(incoming.is_group);
        assert_eq!(incoming.chat_type, ChatKind::Supergroup);
    }
    #[test]
    fn parse_update_sender_chat_fallback() {
        // Anonymous-admin message: from is null, sender_chat carries the id.
        let json = serde_json::json!({
            "update_id": 101,
            "message": {
                "message_id": 1,
                "chat": {"id": -1001, "type": "supergroup", "title": "Group"},
                "from": null,
                "sender_chat": {"id": -1002, "type": "channel", "title": "AnonAdmin"},
                "text": "admin announcement"
            }
        });
        let update: Update = serde_json::from_value(json).unwrap();

        let incoming = TelegramAdapter::parse_update(&update).unwrap();
        assert_eq!(incoming.user_id, Some(-1002));
        assert_eq!(incoming.username, Some("AnonAdmin".into()));
        assert_eq!(incoming.text, Some("admin announcement".into()));

        // The flattened form must carry a non-empty user id so the
        // fail-closed allowlist doesn't silently drop it.
        let gw = TelegramAdapter::parsed_to_incoming(&incoming);
        assert_eq!(gw.user_id, "-1002");
    }
    #[test]
    fn parse_update_sender_chat_absent_keeps_none() {
        // No `from` and no `sender_chat` at all: user_id stays None.
        let json = serde_json::json!({
            "update_id": 102,
            "message": {
                "message_id": 2,
                "chat": {"id": -1001, "type": "supergroup"},
                "text": "mystery message"
            }
        });
        let update: Update = serde_json::from_value(json).unwrap();
        let incoming = TelegramAdapter::parse_update(&update).unwrap();
        assert_eq!(incoming.user_id, None);
    }
    #[test]
    fn parse_update_channel_post_uses_channel_id() {
        let json = serde_json::json!({
            "update_id": 103,
            "channel_post": {
                "message_id": 5,
                "chat": {"id": -1001234567890i64, "type": "channel", "title": "News"},
                "text": "breaking news"
            }
        });
        let update: Update = serde_json::from_value(json).unwrap();

        let incoming = TelegramAdapter::parse_update(&update).unwrap();
        assert_eq!(incoming.chat_id, -1001234567890);
        // Channel posts have no `from`; the channel id becomes the user id.
        assert_eq!(incoming.user_id, Some(-1001234567890));
        assert_eq!(incoming.text, Some("breaking news".into()));
        assert_eq!(incoming.chat_type, ChatKind::Channel);

        let gw = TelegramAdapter::parsed_to_incoming(&incoming);
        assert_eq!(gw.user_id, "-1001234567890");
    }
    #[test]
    fn inline_text_link_urls_basic() {
        let entities = vec![text_link(6, 4, "https://example.com")];
        assert_eq!(
            inline_text_link_urls("Check this out", &entities),
            "Check this (https://example.com) out"
        );
    }
    #[test]
    fn inline_text_link_urls_ignores_non_links() {
        let entities = vec![
            MessageEntity {
                offset: 0,
                length: 5,
                kind: "bold".into(),
                url: None,
            },
            text_link(6, 4, "https://example.com"),
        ];
        assert_eq!(
            inline_text_link_urls("Hello link", &entities),
            "Hello link (https://example.com)"
        );
    }
    #[test]
    fn inline_text_link_urls_emoji_utf16_offsets() {
        // '👉' (U+1F449) is a surrogate pair = 2 UTF-16 code units, while it
        // is a single char. Entity offsets are in UTF-16 units: in
        // "Click 👉 here", units 0..6 are "Click ", 6..8 the emoji, 8 the
        // space, and "here" sits at units 9..13.
        let text = "Click 👉 here";
        let entities = vec![text_link(9, 4, "https://x.y")];
        assert_eq!(
            inline_text_link_urls(text, &entities),
            "Click 👉 here (https://x.y)"
        );
    }
    #[test]
    fn inline_text_link_urls_malformed_offsets_skipped() {
        let entities = vec![
            text_link(999, 5, "https://bad.example"), // out of bounds
            text_link(0, 5, "https://good.example"),  // valid
        ];
        assert_eq!(
            inline_text_link_urls("hello", &entities),
            "hello (https://good.example)"
        );
    }
    #[test]
    fn inline_text_link_urls_missing_url_skipped() {
        let entities = vec![MessageEntity {
            offset: 0,
            length: 5,
            kind: "text_link".into(),
            url: None,
        }];
        assert_eq!(inline_text_link_urls("hello", &entities), "hello");
    }
    #[test]
    fn parse_telegram_message_inlines_caption_entities() {
        let msg = TelegramMessage {
            message_id: 70,
            chat: make_chat(100, "private"),
            from: Some(make_user(200, None)),
            text: None,
            voice: None,
            photo: None,
            caption: Some("see this".into()),
            caption_entities: Some(vec![text_link(4, 4, "https://cap.example")]),
            sticker: None,
            reply_to_message: None,
            sender_chat: None,
            entities: None,
            message_thread_id: None,
        };
        let update = Update {
            update_id: 104,
            channel_post: None,
            message: Some(msg),
            callback_query: None,
        };
        let incoming = TelegramAdapter::parse_update(&update).unwrap();
        assert_eq!(incoming.text, Some("see this (https://cap.example)".into()));
    }
    #[test]
    fn reply_to_text_is_prepended() {
        let reply_msg = TelegramMessage {
            message_id: 10,
            chat: make_chat(100, "private"),
            from: Some(make_user(50, None)),
            text: Some("the original question".into()),
            voice: None,
            photo: None,
            caption: None,
            sticker: None,
            reply_to_message: None,
            sender_chat: None,
            entities: None,
            caption_entities: None,
            message_thread_id: None,
        };
        let mut outer = make_text_message(
            11,
            make_chat(100, "private"),
            make_user(200, None),
            "my answer",
        );
        outer.reply_to_message = Some(Box::new(reply_msg));

        let update = Update {
            update_id: 105,
            channel_post: None,
            message: Some(outer),
            callback_query: None,
        };
        let parsed = TelegramAdapter::parse_update(&update).unwrap();
        assert_eq!(
            parsed.reply_to_text.as_deref(),
            Some("the original question")
        );
        let gw = TelegramAdapter::parsed_to_incoming(&parsed);
        assert_eq!(gw.text, "[Replying to: the original question]\nmy answer");
    }
    #[test]
    fn reply_to_text_truncated_to_500_chars() {
        let long = "x".repeat(600);
        let reply_msg = TelegramMessage {
            message_id: 10,
            chat: make_chat(100, "private"),
            from: Some(make_user(50, None)),
            text: Some(long),
            voice: None,
            photo: None,
            caption: None,
            sticker: None,
            reply_to_message: None,
            sender_chat: None,
            entities: None,
            caption_entities: None,
            message_thread_id: None,
        };
        let mut outer =
            make_text_message(11, make_chat(100, "private"), make_user(200, None), "reply");
        outer.reply_to_message = Some(Box::new(reply_msg));

        let update = Update {
            update_id: 106,
            channel_post: None,
            message: Some(outer),
            callback_query: None,
        };
        let parsed = TelegramAdapter::parse_update(&update).unwrap();
        let quoted = parsed.reply_to_text.as_ref().unwrap();
        assert_eq!(quoted.chars().count(), 500);
        let gw = TelegramAdapter::parsed_to_incoming(&parsed);
        assert!(gw.text.starts_with("[Replying to: "));
    }
    #[test]
    fn parse_group_message_is_group_flag() {
        let update = Update {
            update_id: 40,
            channel_post: None,
            message: Some(make_text_message(
                60,
                make_chat(700, "supergroup"),
                make_user(800, Some("groupuser")),
                "hello group",
            )),
            callback_query: None,
        };

        let incoming = TelegramAdapter::parse_update(&update).unwrap();
        assert!(incoming.is_group);
        assert_eq!(incoming.chat_type, ChatKind::Supergroup);
    }
    #[test]
    fn is_mentioned_with_username() {
        let adapter = make_adapter_with_bot_username(Some("mybot"));
        assert!(adapter.is_mentioned_in("Hello @mybot how are you?"));
        assert!(!adapter.is_mentioned_in("Hello @otherbot"));
        assert!(!adapter.is_mentioned_in("Hello world"));
    }
    #[test]
    fn is_mentioned_without_username_passthrough() {
        let adapter = make_adapter_with_bot_username(None);
        assert!(adapter.is_mentioned_in("anything"));
        assert!(adapter.is_mentioned_in(""));
    }
    #[test]
    fn strip_mention_removes_at_mention() {
        let adapter = make_adapter_with_bot_username(Some("mybot"));
        assert_eq!(adapter.strip_mention("@mybot do something"), "do something");
        assert_eq!(
            adapter.strip_mention("hey @mybot please help"),
            "hey  please help"
        );
    }
    #[test]
    fn strip_mention_no_username_passthrough() {
        let adapter = make_adapter_with_bot_username(None);
        assert_eq!(adapter.strip_mention("hello world"), "hello world");
    }
}
