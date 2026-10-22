//! Telegram Bot API adapter.
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64};
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use tokio::sync::Notify;
use tracing::{info, warn};

use hermes_core::errors::GatewayError;
use hermes_core::traits::{ParseMode, PlatformAdapter};

use crate::adapter::BasePlatformAdapter;

pub mod error;
pub mod parsing;
pub mod polling;
pub mod sending;
pub mod types;

use error::*;
use sending::MAX_MESSAGE_LENGTH;

// Re-export the full types surface so `telegram::X` paths keep working.
pub use types::*;

pub struct TelegramAdapter {
    base: BasePlatformAdapter,
    config: TelegramConfig,
    client: Client,
    /// Base URL for Telegram Bot API calls.
    api_base: String,
    /// Current offset for long polling (tracks last processed update).
    poll_offset: AtomicI64,
    /// Notify handle to signal the polling loop to stop.
    stop_signal: Arc<Notify>,
    /// Current backoff delay in milliseconds for reconnection logic.
    backoff_ms: AtomicU64,
    /// Consecutive poll error count.
    consecutive_errors: AtomicU64,
    /// Per-chat cooldown deadline for `sendChatAction` (typing) pings.
    /// Set after a typing send fails so the keep-alive task doesn't
    /// retry-storm the API; sends for the chat are skipped until the
    /// deadline passes.
    typing_cooldown_until: std::sync::Mutex<HashMap<String, std::time::Instant>>,
    /// The bot's own Telegram user ID, resolved via `getMe` in `start()`.
    /// `0` means unknown — the own-message filter is fail-open until then.
    bot_id: AtomicI64,
}

impl TelegramAdapter {
    /// Create a new Telegram adapter with the given configuration.
    pub fn new(config: TelegramConfig) -> Result<Self, GatewayError> {
        let base = BasePlatformAdapter::new(&config.token).with_proxy(config.proxy.clone());

        base.validate_token()?;

        let client = base.build_client()?;
        let api_base = format!("https://api.telegram.org/bot{}", config.token);

        // Register the config-file allowlist so the gateway-level fail-closed
        // gate (`route_message`) honors it as a union with the env allowlist.
        if let Some(ref allowed) = config.allowed_users {
            crate::allowlist::register_config_allowlist("telegram", allowed.clone());
        }

        Ok(Self {
            base,
            config,
            client,
            api_base,
            poll_offset: AtomicI64::new(0),
            stop_signal: Arc::new(Notify::new()),
            backoff_ms: AtomicU64::new(0),
            consecutive_errors: AtomicU64::new(0),
            typing_cooldown_until: std::sync::Mutex::new(HashMap::new()),
            bot_id: AtomicI64::new(0),
        })
    }
    /// Get a reference to the configuration.
    pub fn config(&self) -> &TelegramConfig {
        &self.config
    }
    /// Re-arm typing after an outbound message lands (M-T2).
    ///
    /// Telegram clears the typing bubble as soon as a message arrives. The
    /// gateway's keep-alive task re-sends typing every 4s, but a failed
    /// `sendChatAction` leaves a per-chat cooldown (30s default) that would
    /// keep the bubble dead for the full window after each intermediate
    /// message. Clearing it here lets the next keep-alive tick re-arm typing
    /// immediately instead of waiting out the cooldown.
    pub(crate) fn notify_message_sent(&self, chat_id: &str) {
        if let Ok(mut cooldowns) = self.typing_cooldown_until.lock() {
            cooldowns.remove(chat_id);
        }
    }
}

#[async_trait]
impl PlatformAdapter for TelegramAdapter {
    async fn start(&self) -> Result<(), GatewayError> {
        info!(
            "Telegram adapter starting (token: {}...)",
            &self.config.token[..8.min(self.config.token.len())]
        );
        self.base.mark_running();
        // C4: resolve our own bot ID so the inbound path can drop the bot's
        // own messages (echo prevention). Fail-open on the filter only: a
        // getMe failure must never fail startup.
        if let Err(e) = self.resolve_bot_id().await {
            warn!("Telegram getMe failed ({e}); own-message filter disabled");
        }
        // Register the / command menu so users see available commands.
        // Best-effort: never fail startup.
        if let Err(e) = self.register_command_menu().await {
            warn!("Telegram setMyCommands failed: {e}");
        }
        Ok(())
    }

    async fn stop(&self) -> Result<(), GatewayError> {
        info!("Telegram adapter stopping");
        self.base.mark_stopped();
        self.stop_signal.notify_one();
        Ok(())
    }

    async fn send_message(
        &self,
        chat_id: &str,
        text: &str,
        parse_mode: Option<ParseMode>,
    ) -> Result<(), GatewayError> {
        let pm = self.resolve_parse_mode(parse_mode);
        self.send_text(chat_id, text, pm, None).await?;
        Ok(())
    }

    async fn edit_message(
        &self,
        chat_id: &str,
        message_id: &str,
        text: &str,
    ) -> Result<(), GatewayError> {
        let pm = self.resolve_parse_mode(None);
        self.edit_text(chat_id, message_id, text, pm).await
    }

    async fn send_message_raw(
        &self,
        chat_id: &str,
        text: &str,
        parse_mode: Option<ParseMode>,
    ) -> Result<Vec<i64>, GatewayError> {
        let pm = self.resolve_parse_mode(parse_mode);
        self.send_text(chat_id, text, pm, None).await
    }

    async fn delete_message(&self, chat_id: &str, message_id: i64) -> Result<(), GatewayError> {
        // Path resolution prefers the inherent method, so this calls the
        // best-effort delete above rather than recursing into the trait.
        TelegramAdapter::delete_message(self, chat_id, message_id).await
    }

    fn max_edit_chars(&self) -> usize {
        MAX_MESSAGE_LENGTH
    }

    async fn send_file(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<(), GatewayError> {
        let ext = std::path::Path::new(file_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        let (method, field) = Self::media_method_for_extension(&ext);
        self.send_multipart(chat_id, file_path, caption, method, field)
            .await?;
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }

    fn platform_name(&self) -> &str {
        "telegram"
    }

    async fn send_typing(&self, chat_id: &str) -> Result<(), GatewayError> {
        // Typing cooldown: skip the ping when a recent sendChatAction for
        // this chat failed, so the keep-alive task can't retry-storm the
        // API on 429/failure.
        if typing_on_cooldown(&self.typing_cooldown_until, chat_id) {
            return Ok(());
        }

        // Fire-and-forget UI hint: never fail the message flow on a typing
        // error, just log at debug.
        let url = format!("{}/sendChatAction", self.api_base);
        let body = serde_json::json!({
            "chat_id": chat_id,
            "action": "typing",
        });
        match self.post_json::<serde_json::Value>(&url, &body).await {
            Ok(_) => {
                // Success clears any stale cooldown entry for this chat.
                if let Ok(mut cooldowns) = self.typing_cooldown_until.lock() {
                    cooldowns.remove(chat_id);
                }
                Ok(())
            }
            Err(e) => {
                // Back off before the next typing ping: default 30s, honor
                // retry_after when larger, cap at 300s.
                let retry_after = extract_retry_after_seconds(&e.to_string()).unwrap_or(0);
                let cooldown_secs = retry_after.clamp(30, 300);
                let until =
                    std::time::Instant::now() + std::time::Duration::from_secs(cooldown_secs);
                if let Ok(mut cooldowns) = self.typing_cooldown_until.lock() {
                    cooldowns.insert(chat_id.to_string(), until);
                }
                tracing::debug!(
                    "Telegram sendChatAction failed for chat {chat_id} (typing cooldown {cooldown_secs}s): {e}"
                );
                Ok(())
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use super::types::*;
    use super::TelegramAdapter;
    use super::TelegramConfig;
    use crate::adapter::AdapterProxyConfig;

    pub(crate) fn make_chat(id: i64, chat_type: &str) -> Chat {
        Chat {
            id,
            chat_type: chat_type.into(),
            title: None,
            username: None,
        }
    }
    pub(crate) fn make_user(id: i64, username: Option<&str>) -> User {
        User {
            id,
            first_name: Some("Test".into()),
            username: username.map(|s| s.to_string()),
            is_bot: Some(false),
        }
    }
    pub(crate) fn make_text_message(
        msg_id: i64,
        chat: Chat,
        user: User,
        text: &str,
    ) -> TelegramMessage {
        TelegramMessage {
            message_id: msg_id,
            chat,
            from: Some(user),
            text: Some(text.into()),
            voice: None,
            photo: None,
            caption: None,
            sticker: None,
            reply_to_message: None,
            sender_chat: None,
            entities: None,
            caption_entities: None,
            message_thread_id: None,
        }
    }
    pub(crate) fn test_adapter_config() -> TelegramConfig {
        TelegramConfig {
            token: "test-token".into(),
            webhook_url: None,
            polling: true,
            proxy: Default::default(),
            parse_markdown: false,
            parse_html: false,
            poll_timeout: 30,
            bot_username: None,
            allowed_users: None,
            group_require_mention: false,
            command_menu_max: None,
            message_reactions: false,
        }
    }
    pub(crate) fn test_adapter() -> TelegramAdapter {
        TelegramAdapter::new(test_adapter_config()).unwrap()
    }
    /// Test adapter with a config-file `allowed_users` list.
    pub(crate) fn make_adapter_with_allowed_users(allowed: Option<Vec<String>>) -> TelegramAdapter {
        let mut config = test_adapter_config();
        config.allowed_users = allowed;
        TelegramAdapter::new(config).unwrap()
    }
    pub(crate) fn set_test_var(name: &str, v: Option<&str>) -> Option<String> {
        let old = std::env::var(name).ok();
        match v {
            Some(s) => std::env::set_var(name, s),
            None => std::env::remove_var(name),
        }
        old
    }
    pub(crate) fn make_dm_text_update(update_id: i64, user_id: i64, text: &str) -> Update {
        Update {
            update_id,
            channel_post: None,
            message: Some(make_text_message(
                100,
                make_chat(900, "private"),
                make_user(user_id, None),
                text,
            )),
            callback_query: None,
        }
    }
    /// Clear all env allowlist vars for the config-file gate tests below.
    pub(crate) fn clear_allowlist_env() -> [Option<String>; 3] {
        [
            set_test_var("TELEGRAM_ALLOWED_USERS", None),
            set_test_var("GATEWAY_ALLOWED_USERS", None),
            set_test_var("TELEGRAM_ALLOW_ALL_USERS", None),
        ]
    }
    pub(crate) fn restore_allowlist_env(old: [Option<String>; 3]) {
        let [allowed, fallback, all] = old;
        set_test_var("TELEGRAM_ALLOWED_USERS", allowed.as_deref());
        set_test_var("GATEWAY_ALLOWED_USERS", fallback.as_deref());
        set_test_var("TELEGRAM_ALLOW_ALL_USERS", all.as_deref());
    }
    pub(crate) fn text_link(offset: u32, length: u32, url: &str) -> MessageEntity {
        MessageEntity {
            offset,
            length,
            kind: "text_link".into(),
            url: Some(url.into()),
        }
    }
    pub(crate) fn make_adapter_with_bot_username(username: Option<&str>) -> TelegramAdapter {
        let config = TelegramConfig {
            token: "fake_token_12345".into(),
            webhook_url: None,
            polling: true,
            proxy: AdapterProxyConfig::default(),
            parse_markdown: false,
            parse_html: false,
            poll_timeout: 30,
            bot_username: username.map(|s| s.to_string()),
            allowed_users: None,
            group_require_mention: false,
            command_menu_max: None,
            message_reactions: false,
        };
        TelegramAdapter::new(config).unwrap()
    }
    pub(crate) fn make_group_gate_adapter(require_mention: bool) -> TelegramAdapter {
        let mut config = test_adapter_config();
        config.bot_username = Some("testbot".to_string());
        config.group_require_mention = require_mention;
        TelegramAdapter::new(config).unwrap()
    }
    pub(crate) fn make_group_text_update(update_id: i64, user_id: i64, text: &str) -> Update {
        Update {
            update_id,
            channel_post: None,
            message: Some(make_text_message(
                100,
                make_chat(901, "supergroup"),
                make_user(user_id, None),
                text,
            )),
            callback_query: None,
        }
    }
    pub(crate) fn set_open_allowlist_env() -> [Option<String>; 3] {
        [
            set_test_var("TELEGRAM_ALLOWED_USERS", None),
            set_test_var("GATEWAY_ALLOWED_USERS", None),
            set_test_var("TELEGRAM_ALLOW_ALL_USERS", Some("1")),
        ]
    }
    pub(crate) fn restore_open_allowlist_env(old: [Option<String>; 3]) {
        let [allowed, fallback, all] = old;
        set_test_var("TELEGRAM_ALLOWED_USERS", allowed.as_deref());
        set_test_var("GATEWAY_ALLOWED_USERS", fallback.as_deref());
        set_test_var("TELEGRAM_ALLOW_ALL_USERS", all.as_deref());
    }
    /// Test adapter whose API base points at a mock server instead of
    /// api.telegram.org. The field is private but accessible here since
    /// tests live in the same module.
    pub(crate) fn mock_server_adapter(server_uri: &str) -> TelegramAdapter {
        let mut adapter = test_adapter();
        adapter.api_base = server_uri.to_string();
        adapter
    }
    /// Test adapter with M-T4 message reactions enabled, pointed at a mock
    /// server instead of api.telegram.org.
    pub(crate) fn mock_server_adapter_with_reactions(server_uri: &str) -> TelegramAdapter {
        let mut config = test_adapter_config();
        config.message_reactions = true;
        let mut adapter = TelegramAdapter::new(config).unwrap();
        adapter.api_base = server_uri.to_string();
        adapter
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platforms::telegram::test_helpers::*;

    #[test]
    fn config_allowlist_registered_on_adapter_new() {
        // Serialize with other allowlist tests: the registry is global.
        let _guard = crate::allowlist::ENV_LOCK.lock().unwrap();
        // Creating the adapter registers its config-file allowlist, so the
        // gateway-level fail-closed gate admits the user without env vars.
        let _adapter = make_adapter_with_allowed_users(Some(vec!["565903951".to_string()]));
        assert!(crate::allowlist::is_platform_user_allowed(
            "telegram",
            "565903951"
        ));
        assert!(!crate::allowlist::is_platform_user_allowed(
            "telegram",
            "999999999"
        ));
        crate::allowlist::clear_config_allowlist("telegram");
        // After clearing, fail-closed again (no env vars in test env).
        assert!(!crate::allowlist::is_platform_user_allowed(
            "telegram",
            "565903951"
        ));
    }
    #[test]
    fn notify_message_sent_clears_typing_cooldown() {
        let adapter = test_adapter();
        let chat = "12345";

        // Seed a cooldown the way a failed sendChatAction would.
        adapter.typing_cooldown_until.lock().unwrap().insert(
            chat.to_string(),
            std::time::Instant::now() + std::time::Duration::from_secs(60),
        );
        assert!(typing_on_cooldown(&adapter.typing_cooldown_until, chat));

        adapter.notify_message_sent(chat);

        assert!(!typing_on_cooldown(&adapter.typing_cooldown_until, chat));
    }
    #[test]
    fn notify_message_sent_leaves_other_chats_alone() {
        let adapter = test_adapter();
        let other = "999";

        adapter.typing_cooldown_until.lock().unwrap().insert(
            other.to_string(),
            std::time::Instant::now() + std::time::Duration::from_secs(60),
        );

        // No-op for a chat that was never on cooldown.
        adapter.notify_message_sent("12345");

        // The other chat's cooldown survives.
        assert!(typing_on_cooldown(&adapter.typing_cooldown_until, other));
    }
}
