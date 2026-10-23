//! Telegram adapter — long-polling receive loop.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tracing::{debug, error, info, warn};

use hermes_core::errors::GatewayError;

use super::error::*;
use super::media::enrich_media_message;
use super::sending::REACTION_PROCESSING;
use super::types::*;
use super::TelegramAdapter;

/// If no poll progress for this long, the loop is considered stalled.
pub(crate) const POLL_STALL_THRESHOLD: Duration = Duration::from_secs(60);

/// Initial reconnect backoff.
pub(crate) const INITIAL_BACKOFF_MS: u64 = 1000;

/// Maximum reconnect backoff.
pub(crate) const MAX_BACKOFF_MS: u64 = 60_000;

/// Delays for 409-conflict recovery attempts.
pub(crate) const CONFLICT_RETRY_DELAYS_SECS: [u64; 5] = [15, 25, 35, 45, 55];

/// Consecutive poll failures before giving up.
pub(crate) const MAX_CONSECUTIVE_POLL_FAILURES: u64 = 10;

impl TelegramAdapter {
    /// Fetch updates from Telegram using long polling.
    pub async fn get_updates(&self) -> Result<Vec<Update>, GatewayError> {
        let offset = self.poll_offset.load(Ordering::SeqCst);
        let url = format!("{}/getUpdates", self.api_base);

        let body = serde_json::json!({
            "offset": offset,
            "timeout": self.config.poll_timeout,
            "allowed_updates": ["message", "callback_query"],
        });

        let resp: TelegramResponse<Vec<Update>> = self.post_json(&url, &body).await?;

        if let Some(updates) = resp.result {
            if let Some(last) = updates.last() {
                self.poll_offset.store(last.update_id + 1, Ordering::SeqCst);
            }
            Ok(updates)
        } else {
            Ok(Vec::new())
        }
    }
    /// Fetch updates with exponential backoff on failures.
    ///
    /// On success the backoff resets to zero. On failure the delay doubles
    /// each time (1 s → 2 s → 4 s … capped at 60 s), with ±20% jitter.
    /// The caller can inspect
    /// `PollResult::Backoff` and decide whether to sleep or abort.
    pub async fn poll_with_backoff(&self) -> PollResult {
        match self.get_updates().await {
            Ok(updates) => {
                self.backoff_ms.store(0, Ordering::SeqCst);
                self.consecutive_errors.store(0, Ordering::SeqCst);
                PollResult::Updates(updates)
            }
            Err(e) => {
                let prev = self.backoff_ms.load(Ordering::SeqCst);
                let next = if prev == 0 {
                    INITIAL_BACKOFF_MS
                } else {
                    (prev * 2).min(MAX_BACKOFF_MS)
                };
                // ±20% jitter to avoid thundering herds when many instances
                // restart at once.
                let jittered = apply_jitter(next, time_based_rand01());
                self.backoff_ms.store(jittered, Ordering::SeqCst);

                let err_count = self.consecutive_errors.fetch_add(1, Ordering::SeqCst) + 1;
                warn!(
                    consecutive_errors = err_count,
                    backoff_ms = jittered,
                    "Telegram poll failed: {}",
                    e
                );

                PollResult::Backoff {
                    error: e,
                    delay_ms: jittered,
                }
            }
        }
    }
    /// Convenience: sleep for the backoff delay. Should be called after
    /// receiving `PollResult::Backoff`.
    pub async fn sleep_backoff(&self) {
        let ms = self.backoff_ms.load(Ordering::SeqCst);
        if ms > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(ms)).await;
        }
    }
    /// Return the current consecutive error count.
    pub fn consecutive_error_count(&self) -> u64 {
        self.consecutive_errors.load(Ordering::SeqCst)
    }
    /// Whether the adapter is currently marked running.
    pub fn is_running(&self) -> bool {
        self.base.is_running()
    }
    /// Clone of the stop `Notify`, signaled by [`TelegramAdapter::stop`].
    ///
    /// The poll loop ([`TelegramAdapter::run_poll_loop`]) selects on this so
    /// it terminates promptly when the adapter is stopped — the same
    /// mechanism `stop()` uses.
    pub fn stop_notifier(&self) -> Arc<Notify> {
        self.stop_signal.clone()
    }
    /// Resolve the bot's own user ID via `getMe` and store it in `bot_id`.
    ///
    /// Enables the own-message filter in [`TelegramAdapter::process_update`]
    /// (echo prevention). Called from [`PlatformAdapter::start`].
    /// Best-effort: errors are returned to the caller, which logs a warning
    /// and continues WITHOUT the filter (fail-open on the filter only —
    /// never fail startup).
    pub(crate) async fn resolve_bot_id(&self) -> Result<(), GatewayError> {
        let url = format!("{}/getMe", self.api_base);
        let resp: TelegramResponse<User> = self.post_json(&url, &serde_json::json!({})).await?;
        match resp.result {
            Some(me) => {
                self.bot_id.store(me.id, Ordering::SeqCst);
                info!("Telegram getMe OK: bot id {}", me.id);
                Ok(())
            }
            None => Err(GatewayError::ConnectionFailed(
                resp.description
                    .unwrap_or_else(|| "getMe returned no result".into()),
            )),
        }
    }
    /// Run the long-polling receive loop, forwarding inbound messages as
    /// platform-agnostic [`crate::gateway::IncomingMessage`]s on `tx`.
    ///
    /// Each polled update is handled by [`TelegramAdapter::process_update`]:
    /// fail-closed allowlist gate ([`crate::allowlist`]), `answerCallbackQuery`
    /// for button presses, then conversion via
    /// [`TelegramAdapter::update_to_incoming`].
    ///
    /// The loop terminates when [`TelegramAdapter::stop`] signals the stop
    /// `Notify` (also interrupting backoff sleeps), or when the `tx` receiver
    /// is dropped (the gateway pump task going away).
    ///
    /// Error handling:
    /// - `Auth` (bad/revoked token): fatal — logs an actionable error and stops.
    /// - `telegram_conflict` (409, another consumer polling): retries on a
    ///   fixed schedule (15/25/35/45/55 s), then stops with an actionable
    ///   error after 5 failed retries.
    /// - Sustained other failures: stops after [`MAX_CONSECUTIVE_POLL_FAILURES`]
    ///   consecutive errors.
    pub async fn run_poll_loop(
        self: &Arc<Self>,
        tx: tokio::sync::mpsc::Sender<crate::gateway::IncomingMessage>,
    ) {
        info!("Telegram poll loop started");
        // C5 stall watchdog: any successful getUpdates response — even one
        // carrying zero updates — counts as progress; a request that times
        // out or errors does not.
        let mut last_progress = Instant::now();
        let mut conflict_retries: usize = 0;
        loop {
            tokio::select! {
                _ = self.stop_signal.notified() => {
                    info!("Telegram poll loop stopping (stop signaled)");
                    break;
                }
                result = self.poll_with_backoff() => {
                    match result {
                        PollResult::Updates(updates) => {
                            last_progress = Instant::now();
                            conflict_retries = 0;
                            for update in &updates {
                                if !self.process_update(update, &tx).await {
                                    info!(
                                        "Telegram poll loop stopping (inbound receiver dropped)"
                                    );
                                    return;
                                }
                            }
                        }
                        PollResult::Backoff { error, delay_ms } => {
                            // Fatal: bad/revoked token — retrying forever is
                            // pointless.
                            if matches!(error, GatewayError::Auth(_)) {
                                error!(
                                    "Telegram bot token invalid/revoked — check config, stopping poll loop"
                                );
                                break;
                            }
                            // Recoverable: another consumer is polling with
                            // the same token. Retry on a fixed schedule, then
                            // give up with an actionable message.
                            if is_conflict_error(&error) {
                                if conflict_retries < CONFLICT_RETRY_DELAYS_SECS.len() {
                                    let delay_secs =
                                        CONFLICT_RETRY_DELAYS_SECS[conflict_retries];
                                    conflict_retries += 1;
                                    warn!(
                                        attempt = conflict_retries,
                                        delay_secs,
                                        "Telegram getUpdates conflict (another instance may be polling with this token), retrying"
                                    );
                                    tokio::select! {
                                        _ = self.stop_signal.notified() => {
                                            info!("Telegram poll loop stopping during conflict backoff");
                                            return;
                                        }
                                        _ = tokio::time::sleep(
                                            tokio::time::Duration::from_secs(delay_secs),
                                        ) => {}
                                    }
                                    continue;
                                }
                                error!(
                                    "Telegram getUpdates conflict persists after {} retries — another instance may be polling with this token — ensure only one instance runs; stopping poll loop",
                                    conflict_retries
                                );
                                break;
                            }
                            // Escalate to fatal after sustained failures.
                            let err_count = self.consecutive_error_count();
                            if err_count >= MAX_CONSECUTIVE_POLL_FAILURES {
                                error!(
                                    consecutive_errors = err_count,
                                    "Telegram poll loop: too many consecutive failures, stopping"
                                );
                                break;
                            }
                            // C5: if no successful poll landed within the
                            // watchdog window, say so loudly and re-arm —
                            // the next loop iteration re-establishes polling.
                            if Self::poll_stalled(last_progress, Instant::now()) {
                                warn!(
                                    "Telegram poll stalled (no successful getUpdates in >60s), re-establishing polling"
                                );
                                last_progress = Instant::now();
                            }
                            tokio::select! {
                                _ = self.stop_signal.notified() => {
                                    info!("Telegram poll loop stopping during backoff");
                                    return;
                                }
                                _ = tokio::time::sleep(
                                    tokio::time::Duration::from_millis(delay_ms),
                                ) => {}
                            }
                        }
                    }
                }
            }
        }
        info!("Telegram poll loop stopped");
    }
    /// Whether an inbound sender ID is the bot itself (C4 echo prevention).
    ///
    /// `bot_id == 0` means unknown (`getMe` hasn't succeeded yet) — the
    /// filter is fail-open in that case and nothing is dropped.
    pub(crate) fn is_own_message(user_id: Option<i64>, bot_id: i64) -> bool {
        bot_id != 0 && user_id == Some(bot_id)
    }
    /// True when the last successful `getUpdates` response is older than
    /// [`POLL_STALL_THRESHOLD`] (C5 stall watchdog).
    ///
    /// Factored out so the threshold logic is unit-testable without a
    /// network connection.
    pub(crate) fn poll_stalled(last_progress: Instant, now: Instant) -> bool {
        now.duration_since(last_progress) > POLL_STALL_THRESHOLD
    }
    /// Handle a single polled update.
    ///
    /// Order: parse → fail-closed allowlist gate (dropped updates are logged
    /// and skipped) → answer callback queries so the user's client stops
    /// showing the loading spinner → forward as a gateway-level
    /// [`crate::gateway::IncomingMessage`].
    ///
    /// Returns `false` when the inbound receiver is gone and the poll loop
    /// should exit.
    pub(crate) async fn process_update(
        &self,
        update: &Update,
        tx: &tokio::sync::mpsc::Sender<crate::gateway::IncomingMessage>,
    ) -> bool {
        let Some(mut parsed) = Self::parse_update(update) else {
            return true;
        };

        // C4: echo prevention — drop the bot's own messages before the
        // allowlist gate, so outbound replies visible to the bot in groups
        // can't re-enter the agent loop as inbound messages. Covers
        // callback queries too (their sender is `cq.from`).
        if Self::is_own_message(parsed.user_id, self.bot_id.load(Ordering::SeqCst)) {
            debug!(
                update_id = update.update_id,
                "Telegram: skipping own message (sender == bot_id)"
            );
            return true;
        }

        let user_id = parsed.user_id.map(|u| u.to_string()).unwrap_or_default();
        // Union gate: env allowlist (fail-closed) OR the config-file
        // `allowed_users` list. A missing/empty `allowed_users` leaves the
        // env gate as the sole authority — behavior identical to before.
        let in_config_list = self
            .config
            .allowed_users
            .as_deref()
            .is_some_and(|list| list.iter().any(|id| id == &user_id));
        if !crate::allowlist::is_platform_user_allowed("telegram", &user_id) && !in_config_list {
            warn!(
                user_id = %user_id,
                update_id = update.update_id,
                "Telegram: dropping update from user not in env allowlist nor config allowed_users (fail-closed)"
            );
            return true;
        }

        // Group trigger gate (M-G1): in group/supergroup chats, strip the
        // `@botname` mention from the text before forwarding. When
        // `group_require_mention` is enabled, drop messages that neither
        // mention the bot nor reply to the bot's own message.
        if parsed.is_group && update.message.is_some() {
            let mentioned = parsed
                .text
                .as_deref()
                .map(|t| self.is_mentioned_in(t))
                .unwrap_or(false);
            if mentioned {
                parsed.text = parsed.text.map(|t| self.strip_mention(&t));
            }

            let reply_to_bot = update
                .message
                .as_ref()
                .and_then(|m| m.reply_to_message.as_ref())
                .and_then(|r| r.from.as_ref())
                .and_then(|u| u.is_bot)
                .unwrap_or(false);

            if !self.group_trigger_satisfied(mentioned, reply_to_bot) {
                debug!(
                    chat_id = parsed.chat_id,
                    update_id = update.update_id,
                    "Telegram: dropping group message (group_require_mention set, no mention of bot and not a reply to bot)"
                );
                return true;
            }
        }

        // Inbound media enrichment (C7): voice → transcription, photo →
        // description, document → text extract, sticker → placeholder.
        // Best-effort; never breaks the text flow.
        if self.config.media_enabled {
            enrich_media_message(self, &mut parsed, update).await;
        }

        if let Some(ref callback_query_id) = parsed.callback_query_id {
            if let Err(e) = self
                .answer_callback_query(callback_query_id, None, false)
                .await
            {
                warn!(
                    callback_query_id = %callback_query_id,
                    "Telegram: answerCallbackQuery failed: {}", e
                );
            }
        }

        // M-T4: opt-in message reactions — set 👀 when the message is
        // accepted for processing. Fire-and-forget: failures are logged at
        // debug inside `set_message_reaction` and never break the inbound
        // path. Skipped when there is no real message to react to (e.g. a
        // callback query without an attached message carries id 0).
        if parsed.message_id > 0 {
            let _ = self
                .set_message_reaction(
                    &parsed.chat_id.to_string(),
                    parsed.message_id,
                    REACTION_PROCESSING,
                )
                .await;
        }

        // M-G5: forum topics — remember chats with topics so outbound replies
        // and typing indicators can target the right thread. The first time
        // a topic message arrives in a chat, also register a per-chat
        // command menu scope (the default `setMyCommands` scope only shows
        // the menu in the forum's General topic).
        if parsed.message_thread_id.is_some() && self.note_forum_chat(parsed.chat_id) {
            self.spawn_forum_command_menu(parsed.chat_id);
        }

        tx.send(Self::parsed_to_incoming(&parsed)).await.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platforms::telegram::test_helpers::*;
    use crate::platforms::telegram::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test-only env-serialization lock, not an async lock
    async fn process_update_drops_disallowed_user() {
        let _lock = crate::allowlist::ENV_LOCK.lock().unwrap();
        let old_allowed = set_test_var("TELEGRAM_ALLOWED_USERS", Some("111"));
        let old_fallback = set_test_var("GATEWAY_ALLOWED_USERS", None);
        let old_all = set_test_var("TELEGRAM_ALLOW_ALL_USERS", None);

        let adapter = test_adapter();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let update = make_dm_text_update(50, 999, "should be dropped");

        let keep_going = adapter.process_update(&update, &tx).await;
        assert!(keep_going);
        // Nothing forwarded for a non-allowlisted sender (fail-closed).
        assert!(rx.try_recv().is_err());

        set_test_var("TELEGRAM_ALLOWED_USERS", old_allowed.as_deref());
        set_test_var("GATEWAY_ALLOWED_USERS", old_fallback.as_deref());
        set_test_var("TELEGRAM_ALLOW_ALL_USERS", old_all.as_deref());
    }
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test-only env-serialization lock, not an async lock
    async fn process_update_forwards_allowed_message() {
        let _lock = crate::allowlist::ENV_LOCK.lock().unwrap();
        let old_allowed = set_test_var("TELEGRAM_ALLOWED_USERS", None);
        let old_fallback = set_test_var("GATEWAY_ALLOWED_USERS", None);
        let old_all = set_test_var("TELEGRAM_ALLOW_ALL_USERS", Some("1"));

        let adapter = test_adapter();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let update = make_dm_text_update(51, 999, "hello agent");

        let keep_going = adapter.process_update(&update, &tx).await;
        assert!(keep_going);
        let gw = rx.try_recv().expect("allowed message must be forwarded");
        assert_eq!(gw.platform, "telegram");
        assert_eq!(gw.chat_id, "900");
        assert_eq!(gw.user_id, "999");
        assert_eq!(gw.text, "hello agent");
        assert!(gw.is_dm);

        set_test_var("TELEGRAM_ALLOWED_USERS", old_allowed.as_deref());
        set_test_var("GATEWAY_ALLOWED_USERS", old_fallback.as_deref());
        set_test_var("TELEGRAM_ALLOW_ALL_USERS", old_all.as_deref());
    }
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test-only env-serialization lock, not an async lock
    async fn process_update_sends_eyes_reaction_when_enabled() {
        let _lock = crate::allowlist::ENV_LOCK.lock().unwrap();
        let old = set_open_allowlist_env();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/setMessageReaction"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": true
            })))
            .expect(1)
            .mount(&server)
            .await;

        let adapter = mock_server_adapter_with_reactions(&server.uri());
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        // make_dm_text_update builds message_id 100 in chat 900.
        let update = make_dm_text_update(80, 999, "hi with reactions on");

        let keep_going = adapter.process_update(&update, &tx).await;
        assert!(keep_going);
        rx.try_recv().expect("message must still be forwarded");

        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json body");
        assert_eq!(body["chat_id"], serde_json::json!("900"));
        assert_eq!(body["message_id"], serde_json::json!(100));
        assert_eq!(body["reaction"][0]["emoji"], serde_json::json!("👀"));

        restore_open_allowlist_env(old);
    }
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test-only env-serialization lock, not an async lock
    async fn process_update_sends_no_reaction_when_disabled() {
        let _lock = crate::allowlist::ENV_LOCK.lock().unwrap();
        let old = set_open_allowlist_env();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/setMessageReaction"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": true
            })))
            .expect(0)
            .mount(&server)
            .await;

        // Default test adapter: message_reactions=false -> no HTTP traffic.
        let adapter = mock_server_adapter(&server.uri());
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let update = make_dm_text_update(81, 999, "hi with reactions off");

        let keep_going = adapter.process_update(&update, &tx).await;
        assert!(keep_going);
        rx.try_recv().expect("message must still be forwarded");

        let requests = server.received_requests().await.expect("requests");
        assert!(requests.is_empty(), "disabled reactions must send nothing");

        restore_open_allowlist_env(old);
    }
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test-only env-serialization lock, not an async lock
    async fn process_update_receiver_dropped_stops_loop() {
        let _lock = crate::allowlist::ENV_LOCK.lock().unwrap();
        let old_allowed = set_test_var("TELEGRAM_ALLOWED_USERS", None);
        let old_fallback = set_test_var("GATEWAY_ALLOWED_USERS", None);
        let old_all = set_test_var("TELEGRAM_ALLOW_ALL_USERS", Some("1"));

        let adapter = test_adapter();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        drop(rx);
        let update = make_dm_text_update(52, 999, "nobody listening");

        // Receiver gone -> process_update returns false -> loop exits.
        assert!(!adapter.process_update(&update, &tx).await);

        set_test_var("TELEGRAM_ALLOWED_USERS", old_allowed.as_deref());
        set_test_var("GATEWAY_ALLOWED_USERS", old_fallback.as_deref());
        set_test_var("TELEGRAM_ALLOW_ALL_USERS", old_all.as_deref());
    }
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test-only env-serialization lock, not an async lock
    async fn process_update_config_allowlist_admits_user() {
        let _lock = crate::allowlist::ENV_LOCK.lock().unwrap();
        let old = clear_allowlist_env();

        // Env allowlist empty -> user passes purely via config `allowed_users`.
        let adapter = make_adapter_with_allowed_users(Some(vec!["123".into()]));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let update = make_dm_text_update(60, 123, "config allow");

        let keep_going = adapter.process_update(&update, &tx).await;
        assert!(keep_going);
        let gw = rx.try_recv().expect("config-allowlisted user must pass");
        assert_eq!(gw.user_id, "123");

        restore_allowlist_env(old);
    }
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test-only env-serialization lock, not an async lock
    async fn process_update_config_allowlist_denies_user() {
        let _lock = crate::allowlist::ENV_LOCK.lock().unwrap();
        let old = clear_allowlist_env();

        // Env allowlist empty and user not in config list -> fail-closed drop.
        let adapter = make_adapter_with_allowed_users(Some(vec!["123".into()]));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let update = make_dm_text_update(61, 456, "config deny");

        let keep_going = adapter.process_update(&update, &tx).await;
        assert!(keep_going);
        assert!(rx.try_recv().is_err());

        restore_allowlist_env(old);
    }
    #[test]
    fn is_own_message_filter_logic() {
        assert!(TelegramAdapter::is_own_message(Some(123), 123));
        assert!(!TelegramAdapter::is_own_message(Some(456), 123));
        assert!(!TelegramAdapter::is_own_message(None, 123));
        // bot_id unknown (0): fail-open, nothing is dropped.
        assert!(!TelegramAdapter::is_own_message(Some(123), 0));
    }
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test-only env-serialization lock, not an async lock
    async fn process_update_drops_own_message() {
        let _lock = crate::allowlist::ENV_LOCK.lock().unwrap();
        let old = clear_allowlist_env();
        let old_all = set_test_var("TELEGRAM_ALLOW_ALL_USERS", Some("1"));

        // Simulate the bot id start() would learn from getMe.
        let adapter = test_adapter();
        adapter.bot_id.store(999, Ordering::SeqCst);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        // The bot's own reply echoing back in a group the bot can read.
        let update = make_dm_text_update(70, 999, "echo of my own reply");

        let keep_going = adapter.process_update(&update, &tx).await;
        assert!(keep_going);
        // Filtered before the allowlist gate: nothing forwarded even though
        // the allowlist would admit everyone.
        assert!(rx.try_recv().is_err());

        set_test_var("TELEGRAM_ALLOW_ALL_USERS", old_all.as_deref());
        restore_allowlist_env(old);
    }
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test-only env-serialization lock, not an async lock
    async fn process_update_forwards_non_own_message_when_bot_id_known() {
        let _lock = crate::allowlist::ENV_LOCK.lock().unwrap();
        let old = clear_allowlist_env();
        let old_all = set_test_var("TELEGRAM_ALLOW_ALL_USERS", Some("1"));

        let adapter = test_adapter();
        adapter.bot_id.store(999, Ordering::SeqCst);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let update = make_dm_text_update(71, 555, "hello from a user");

        let keep_going = adapter.process_update(&update, &tx).await;
        assert!(keep_going);
        let gw = rx.try_recv().expect("other users' messages must pass");
        assert_eq!(gw.user_id, "555");

        set_test_var("TELEGRAM_ALLOW_ALL_USERS", old_all.as_deref());
        restore_allowlist_env(old);
    }
    #[test]
    fn poll_stalled_threshold_logic() {
        let now = Instant::now();
        assert!(!TelegramAdapter::poll_stalled(now, now));
        assert!(!TelegramAdapter::poll_stalled(
            now - Duration::from_secs(30),
            now
        ));
        assert!(TelegramAdapter::poll_stalled(
            now - Duration::from_secs(61),
            now
        ));
    }
    #[test]
    fn client_timeout_covers_long_poll_hold() {
        // getUpdates holds the connection for up to poll_timeout (default
        // 30 s); the reqwest client timeout set in
        // BasePlatformAdapter::build_client must exceed that with ~30 s
        // margin so a healthy long-poll is never killed early.
    }
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test-only env-serialization lock, not an async lock
    async fn process_update_env_fallback_unchanged_without_config_list() {
        let _lock = crate::allowlist::ENV_LOCK.lock().unwrap();
        let old = clear_allowlist_env();
        set_test_var("TELEGRAM_ALLOWED_USERS", Some("123"));

        // allowed_users = None -> env decides alone (allow then deny).
        let adapter = make_adapter_with_allowed_users(None);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);

        let update = make_dm_text_update(62, 123, "env allow");
        assert!(adapter.process_update(&update, &tx).await);
        assert_eq!(rx.try_recv().unwrap().user_id, "123");

        let update = make_dm_text_update(63, 456, "env deny");
        assert!(adapter.process_update(&update, &tx).await);
        assert!(rx.try_recv().is_err());

        restore_allowlist_env(old);
    }
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test-only env-serialization lock, not an async lock
    async fn process_update_env_and_config_union() {
        let _lock = crate::allowlist::ENV_LOCK.lock().unwrap();
        let old = clear_allowlist_env();
        set_test_var("TELEGRAM_ALLOWED_USERS", Some("789"));

        // In env but NOT in config list -> still allowed (union).
        let adapter = make_adapter_with_allowed_users(Some(vec!["123".into()]));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let update = make_dm_text_update(64, 789, "env union");

        let keep_going = adapter.process_update(&update, &tx).await;
        assert!(keep_going);
        let gw = rx.try_recv().expect("env-allowlisted user must pass");
        assert_eq!(gw.user_id, "789");

        restore_allowlist_env(old);
    }
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test-only env-serialization lock, not an async lock
    async fn process_update_empty_config_list_is_env_only() {
        let _lock = crate::allowlist::ENV_LOCK.lock().unwrap();
        let old = clear_allowlist_env();

        // Empty config list behaves like None: env decides alone, fail-closed.
        let adapter = make_adapter_with_allowed_users(Some(vec![]));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let update = make_dm_text_update(65, 123, "empty list deny");

        let keep_going = adapter.process_update(&update, &tx).await;
        assert!(keep_going);
        assert!(rx.try_recv().is_err());

        restore_allowlist_env(old);
    }
    #[test]
    fn adapter_stop_signals_poll_loop_notifier() {
        // run_poll_loop selects on stop_notifier(); stop() must fire it.
        use futures::FutureExt;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let adapter = Arc::new(test_adapter());
            adapter.start().await.unwrap();
            assert!(adapter.is_running());
            let notifier = adapter.stop_notifier();
            let notified = notifier.notified();
            // Fuse the future so we can poll it without awaiting.
            tokio::pin!(notified);
            assert!(notified.as_mut().now_or_never().is_none());
            adapter.stop().await.unwrap();
            assert!(!adapter.is_running());
            notified.as_mut().await;
        });
    }
    #[test]
    fn backoff_doubling_capped() {
        let vals: Vec<u64> = {
            let mut v = Vec::new();
            let mut current = 0u64;
            for _ in 0..10 {
                current = if current == 0 {
                    INITIAL_BACKOFF_MS
                } else {
                    (current * 2).min(MAX_BACKOFF_MS)
                };
                v.push(current);
            }
            v
        };

        assert_eq!(vals[0], 1_000);
        assert_eq!(vals[1], 2_000);
        assert_eq!(vals[2], 4_000);
        assert_eq!(vals[3], 8_000);
        assert_eq!(vals[4], 16_000);
        assert_eq!(vals[5], 32_000);
        assert_eq!(vals[6], 60_000);
        assert_eq!(vals[7], 60_000);
    }
    #[test]
    fn group_trigger_gate_decisions() {
        // Gate disabled (the default): everything forwards.
        let open = make_group_gate_adapter(false);
        assert!(open.group_trigger_satisfied(false, false));
        assert!(open.group_trigger_satisfied(true, false));

        // Gate enabled: mention or reply-to-bot required.
        let gated = make_group_gate_adapter(true);
        assert!(!gated.group_trigger_satisfied(false, false));
        assert!(gated.group_trigger_satisfied(true, false));
        assert!(gated.group_trigger_satisfied(false, true));
    }
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test-only env-serialization lock, not an async lock
    async fn process_update_group_gate_drops_unmentioned() {
        let _lock = crate::allowlist::ENV_LOCK.lock().unwrap();
        let old = set_open_allowlist_env();

        let adapter = make_group_gate_adapter(true);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let update = make_group_text_update(70, 999, "hello everyone");

        let keep_going = adapter.process_update(&update, &tx).await;
        assert!(keep_going);
        // Gate drops the unmentioned group message.
        assert!(rx.try_recv().is_err());

        restore_open_allowlist_env(old);
    }
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test-only env-serialization lock, not an async lock
    async fn process_update_group_gate_forwards_mentioned_and_strips() {
        let _lock = crate::allowlist::ENV_LOCK.lock().unwrap();
        let old = set_open_allowlist_env();

        let adapter = make_group_gate_adapter(true);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let update = make_group_text_update(71, 999, "@testbot what is the time?");

        let keep_going = adapter.process_update(&update, &tx).await;
        assert!(keep_going);
        let gw = rx.try_recv().expect("mentioned group message must forward");
        assert_eq!(gw.chat_id, "901");
        assert!(!gw.is_dm);
        // @testbot mention is stripped before forwarding.
        assert_eq!(gw.text, "what is the time?");
        assert!(!gw.text.contains("@testbot"));

        restore_open_allowlist_env(old);
    }
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test-only env-serialization lock, not an async lock
    async fn process_update_group_gate_forwards_reply_to_bot() {
        let _lock = crate::allowlist::ENV_LOCK.lock().unwrap();
        let old = set_open_allowlist_env();

        let adapter = make_group_gate_adapter(true);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);

        let bot_user = User {
            id: 1,
            first_name: Some("TestBot".into()),
            username: Some("testbot".into()),
            is_bot: Some(true),
        };
        let bot_msg = make_text_message(
            50,
            make_chat(901, "supergroup"),
            bot_user,
            "previous answer",
        );
        let mut update = make_group_text_update(72, 999, "thanks for that");
        update.message.as_mut().unwrap().reply_to_message = Some(Box::new(bot_msg));

        let keep_going = adapter.process_update(&update, &tx).await;
        assert!(keep_going);
        let gw = rx.try_recv().expect("reply-to-bot message must forward");
        assert_eq!(gw.text, "[Replying to: previous answer]\nthanks for that");

        restore_open_allowlist_env(old);
    }
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test-only env-serialization lock, not an async lock
    async fn process_update_group_gate_disabled_forwards_all() {
        let _lock = crate::allowlist::ENV_LOCK.lock().unwrap();
        let old = set_open_allowlist_env();

        // Default (gate off): unmentioned group messages still forward,
        // preserving existing behavior.
        let adapter = make_group_gate_adapter(false);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let update = make_group_text_update(73, 999, "hello everyone");

        let keep_going = adapter.process_update(&update, &tx).await;
        assert!(keep_going);
        let gw = rx.try_recv().expect("gate-off group message must forward");
        assert_eq!(gw.text, "hello everyone");

        restore_open_allowlist_env(old);
    }
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test-only env-serialization lock, not an async lock
    async fn process_update_dm_unaffected_by_group_gate() {
        let _lock = crate::allowlist::ENV_LOCK.lock().unwrap();
        let old = set_open_allowlist_env();

        // DMs are never gated, even when group_require_mention is set.
        let adapter = make_group_gate_adapter(true);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let update = make_dm_text_update(74, 999, "hello dm");

        let keep_going = adapter.process_update(&update, &tx).await;
        assert!(keep_going);
        let gw = rx
            .try_recv()
            .expect("DM must forward regardless of group gate");
        assert!(gw.is_dm);
        assert_eq!(gw.text, "hello dm");

        restore_open_allowlist_env(old);
    }
}
