//! Discord Bot API adapter.
//!
//! Implements the `PlatformAdapter` trait for Discord using the REST API
//! for message operations and the Gateway WebSocket for receiving events.
//! Supports message splitting at 2000 characters, file uploads via
//! multipart form data, embeds, threads, reactions, slash commands, and
//! Gateway event handling (IDENTIFY, HEARTBEAT, RESUME, READY,
//! MESSAGE_CREATE, MESSAGE_UPDATE, INTERACTION_CREATE, VOICE_STATE_UPDATE,
//! MESSAGE_REACTION_ADD, MESSAGE_REACTION_REMOVE).

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

use hermes_core::errors::GatewayError;
use hermes_core::traits::{ParseMode, PlatformAdapter};

use crate::adapter::{AdapterProxyConfig, BasePlatformAdapter};
use crate::gateway::IncomingMessage;
use crate::markdown_split::split_markdown;
use crate::platforms::helpers::send_with_rate_limit_retry;

/// Maximum message length for Discord (2000 characters).
const MAX_MESSAGE_LENGTH: usize = 2000;

/// Discord API base URL.
const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

/// Discord Gateway WebSocket URL.
const DISCORD_GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

/// Maximum number of inbound message IDs kept for deduplication.
///
/// Discord replays recent events after a reconnect/RESUME; keeping a bounded
/// window of seen IDs prevents double-processing. Oldest entries are evicted
/// first (insertion order).
const MAX_SEEN_MESSAGE_IDS: usize = 5000;

/// Base reconnect delay before jitter, in milliseconds.
const RECONNECT_BASE_DELAY_MS: u64 = 1_000;

/// Maximum reconnect delay (cap for the exponential backoff), in milliseconds.
const RECONNECT_MAX_DELAY_MS: u64 = 60_000;

// ---------------------------------------------------------------------------
// DiscordConfig
// ---------------------------------------------------------------------------

/// Configuration for the Discord adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordConfig {
    /// Discord bot token.
    pub token: String,

    /// Application ID for interactions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,

    /// Proxy configuration for outbound requests.
    #[serde(default)]
    pub proxy: AdapterProxyConfig,

    /// Whether the bot must be @mentioned in group channels.
    #[serde(default)]
    pub require_mention: bool,

    /// Gateway intents bitmask (default: GUILDS | GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT).
    #[serde(default = "default_intents")]
    pub intents: u64,
}

fn default_intents() -> u64 {
    // GUILDS (1<<0) | GUILD_MESSAGES (1<<9) | DIRECT_MESSAGES (1<<12) | MESSAGE_CONTENT (1<<15)
    // DIRECT_MESSAGES is required to receive DM events; it is not privileged.
    (1 << 0) | (1 << 9) | (1 << 12) | (1 << 15)
}

// ---------------------------------------------------------------------------
// Reliability helpers: reconnect backoff + inbound message dedup
// ---------------------------------------------------------------------------

/// Compute the *base* reconnect delay for a 0-based attempt index.
///
/// Pure exponential backoff: 1s, 2s, 4s, …, capped at 60s. Kept separate from
/// [`reconnect_delay`] so the progression is unit-testable without jitter.
pub fn reconnect_base_delay(attempt: u32) -> Duration {
    let shift = attempt.min(6); // 2^6 = 64s -> capped below
    let millis = RECONNECT_BASE_DELAY_MS.saturating_mul(1u64 << shift);
    Duration::from_millis(millis.min(RECONNECT_MAX_DELAY_MS))
}

/// Deterministic xorshift64* PRNG step, used only for backoff jitter.
fn jitter_step(mut x: u64) -> f64 {
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    let r = x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
    r as f64 / (1u64 << 53) as f64 // [0, 1)
}

/// Compute the actual reconnect delay for a 0-based attempt index: the base
/// delay from [`reconnect_base_delay`] with ±25% jitter applied.
pub fn reconnect_delay(attempt: u32) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let factor = 0.75 + 0.5 * jitter_step(nanos ^ (attempt as u64).wrapping_mul(0x9E37_79B9));
    let millis = (reconnect_base_delay(attempt).as_millis() as f64 * factor) as u64;
    Duration::from_millis(millis.max(1))
}

/// Bounded in-memory set of seen inbound message IDs.
///
/// Backed by a [`HashSet`] for O(1) membership checks plus a [`VecDeque`]
/// that tracks insertion order so the oldest IDs are evicted first once the
/// cap ([`MAX_SEEN_MESSAGE_IDS`]) is reached.
#[derive(Debug, Default)]
pub struct MessageDedup {
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl MessageDedup {
    /// Create an empty dedup set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a message ID.
    ///
    /// Returns `true` if the ID was new (the caller should process the
    /// message), `false` if it was already seen (the caller should skip it).
    pub fn insert(&mut self, id: String) -> bool {
        if !self.set.insert(id.clone()) {
            return false;
        }
        self.order.push_back(id);
        self.trim();
        true
    }

    /// Number of IDs currently tracked.
    pub fn len(&self) -> usize {
        self.set.len()
    }

    /// Returns `true` when no IDs are tracked.
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    /// Evict oldest entries until the set is within [`MAX_SEEN_MESSAGE_IDS`].
    pub fn trim(&mut self) {
        while self.order.len() > MAX_SEEN_MESSAGE_IDS {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            } else {
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Discord Gateway opcodes & payload
// ---------------------------------------------------------------------------

/// Discord Gateway payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayPayload {
    pub op: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub d: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t: Option<String>,
}

/// Discord Gateway opcodes.
pub mod opcodes {
    pub const DISPATCH: u8 = 0;
    pub const HEARTBEAT: u8 = 1;
    pub const IDENTIFY: u8 = 2;
    pub const PRESENCE_UPDATE: u8 = 3;
    pub const VOICE_STATE: u8 = 4;
    pub const RESUME: u8 = 6;
    pub const RECONNECT: u8 = 7;
    pub const REQUEST_GUILD_MEMBERS: u8 = 8;
    pub const INVALID_SESSION: u8 = 9;
    pub const HELLO: u8 = 10;
    pub const HEARTBEAT_ACK: u8 = 11;
}

/// Discord IDENTIFY payload data.
#[derive(Debug, Serialize)]
pub struct IdentifyData {
    pub token: String,
    pub intents: u64,
    pub properties: IdentifyProperties,
}

/// Discord IDENTIFY connection properties.
#[derive(Debug, Serialize)]
pub struct IdentifyProperties {
    pub os: String,
    pub browser: String,
    pub device: String,
}

/// Discord RESUME payload data.
#[derive(Debug, Serialize)]
pub struct ResumeData {
    pub token: String,
    pub session_id: String,
    pub seq: u64,
}

// ---------------------------------------------------------------------------
// Gateway state machine
// ---------------------------------------------------------------------------

/// Actions that the external WebSocket driver should take after processing
/// a gateway event through [`GatewaySession::handle_gateway_event`].
#[derive(Debug, Clone, PartialEq)]
pub enum GatewayAction {
    /// Send an IDENTIFY payload to the gateway.
    SendIdentify,
    /// Send a HEARTBEAT payload with the current sequence number.
    SendHeartbeat,
    /// Send a RESUME payload to continue a disconnected session.
    SendResume,
    /// The gateway requested a reconnect – close and reconnect.
    Reconnect,
    /// The session has been invalidated; if `bool` is true the session
    /// is resumable, otherwise a fresh IDENTIFY is required.
    InvalidSession(bool),
    /// A dispatch event arrived. Contains the event name and its data.
    Dispatch(String, serde_json::Value),
}

/// Manages the client-side state for a single Discord Gateway connection.
///
/// This is a pure state machine: feed it [`GatewayPayload`]s received from
/// the WebSocket and it will return a list of [`GatewayAction`]s that the
/// driver should execute. The struct never performs I/O itself, making it
/// easy to test and compose with any WebSocket library.
#[derive(Debug)]
pub struct GatewaySession {
    /// Last received sequence number.
    pub sequence: Option<u64>,
    /// Session ID from the READY event.
    pub session_id: Option<String>,
    /// Resume gateway URL from the READY event.
    pub resume_gateway_url: Option<String>,
    /// Heartbeat interval in milliseconds, extracted from HELLO.
    pub heartbeat_interval_ms: Option<u64>,
    /// Whether the last heartbeat was acknowledged.
    pub heartbeat_acknowledged: bool,
    /// Tracks whether we have successfully identified.
    pub identified: bool,
}

impl GatewaySession {
    pub fn new() -> Self {
        Self {
            sequence: None,
            session_id: None,
            resume_gateway_url: None,
            heartbeat_interval_ms: None,
            heartbeat_acknowledged: true,
            identified: false,
        }
    }

    /// Returns `true` if the session holds enough data to attempt a RESUME.
    pub fn can_resume(&self) -> bool {
        self.session_id.is_some() && self.sequence.is_some()
    }

    /// Process an incoming gateway payload and return the actions the driver
    /// should perform.
    pub fn handle_gateway_event(&mut self, payload: &GatewayPayload) -> Vec<GatewayAction> {
        if let Some(seq) = payload.s {
            self.sequence = Some(seq);
        }

        match payload.op {
            opcodes::HELLO => self.handle_hello(payload),
            opcodes::HEARTBEAT_ACK => self.handle_heartbeat_ack(),
            opcodes::HEARTBEAT => self.handle_heartbeat_request(),
            opcodes::RECONNECT => vec![GatewayAction::Reconnect],
            opcodes::INVALID_SESSION => self.handle_invalid_session(payload),
            opcodes::DISPATCH => self.handle_dispatch(payload),
            _ => {
                debug!("unhandled gateway opcode {}", payload.op);
                vec![]
            }
        }
    }

    fn handle_hello(&mut self, payload: &GatewayPayload) -> Vec<GatewayAction> {
        let mut actions = Vec::new();

        if let Some(d) = &payload.d {
            if let Some(interval) = d.get("heartbeat_interval").and_then(|v| v.as_u64()) {
                self.heartbeat_interval_ms = Some(interval);
                debug!("gateway HELLO: heartbeat_interval={}ms", interval);
            }
        }

        actions.push(GatewayAction::SendHeartbeat);

        if self.can_resume() {
            actions.push(GatewayAction::SendResume);
        } else {
            actions.push(GatewayAction::SendIdentify);
        }

        actions
    }

    fn handle_heartbeat_ack(&mut self) -> Vec<GatewayAction> {
        self.heartbeat_acknowledged = true;
        debug!("heartbeat ACK received");
        vec![]
    }

    fn handle_heartbeat_request(&self) -> Vec<GatewayAction> {
        vec![GatewayAction::SendHeartbeat]
    }

    fn handle_invalid_session(&mut self, payload: &GatewayPayload) -> Vec<GatewayAction> {
        let resumable = payload
            .d
            .as_ref()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if !resumable {
            self.session_id = None;
            self.sequence = None;
            self.identified = false;
        }

        warn!("INVALID_SESSION received (resumable={})", resumable);
        vec![GatewayAction::InvalidSession(resumable)]
    }

    fn handle_dispatch(&mut self, payload: &GatewayPayload) -> Vec<GatewayAction> {
        let event_name = match &payload.t {
            Some(name) => name.clone(),
            None => return vec![],
        };

        let data = payload.d.clone().unwrap_or(serde_json::Value::Null);

        if event_name == "READY" {
            self.handle_ready(&data);
        }

        vec![GatewayAction::Dispatch(event_name, data)]
    }

    fn handle_ready(&mut self, data: &serde_json::Value) {
        self.identified = true;

        if let Some(sid) = data.get("session_id").and_then(|v| v.as_str()) {
            self.session_id = Some(sid.to_string());
        }
        if let Some(url) = data.get("resume_gateway_url").and_then(|v| v.as_str()) {
            self.resume_gateway_url = Some(url.to_string());
        }

        info!(
            "READY: session_id={:?}, resume_url={:?}",
            self.session_id, self.resume_gateway_url
        );
    }

    /// Mark a heartbeat as sent (used by the driver before sending).
    pub fn heartbeat_sent(&mut self) {
        self.heartbeat_acknowledged = false;
    }

    /// Returns `true` if the last heartbeat was not acknowledged, indicating
    /// the connection is likely zombied and should be reconnected.
    pub fn is_zombie(&self) -> bool {
        !self.heartbeat_acknowledged
    }

    /// Reset the session state for a fresh connection.
    pub fn reset(&mut self) {
        self.sequence = None;
        self.session_id = None;
        self.resume_gateway_url = None;
        self.heartbeat_interval_ms = None;
        self.heartbeat_acknowledged = true;
        self.identified = false;
    }
}

impl Default for GatewaySession {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Discord REST API types
// ---------------------------------------------------------------------------

/// Discord Message object.
#[derive(Debug, Clone, Deserialize)]
pub struct DiscordMessage {
    pub id: String,
    pub channel_id: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub author: Option<DiscordUser>,
}

/// Discord User object.
#[derive(Debug, Clone, Deserialize)]
pub struct DiscordUser {
    pub id: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub bot: Option<bool>,
}

/// Incoming message parsed from a Discord MESSAGE_CREATE event.
#[derive(Debug, Clone)]
pub struct IncomingDiscordMessage {
    pub channel_id: String,
    pub message_id: String,
    pub user_id: Option<String>,
    pub username: Option<String>,
    pub content: String,
    pub is_bot: bool,
    /// Guild the message was sent in, if any. `None` means a DM channel.
    pub guild_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Event types: MESSAGE_UPDATE
// ---------------------------------------------------------------------------

/// Parsed data from a `MESSAGE_UPDATE` dispatch event.
///
/// Discord may send partial updates — only `id` and `channel_id` are
/// guaranteed; other fields are optional.
#[derive(Debug, Clone)]
pub struct MessageUpdateEvent {
    pub channel_id: String,
    pub message_id: String,
    pub content: Option<String>,
    pub author_id: Option<String>,
    pub guild_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Event types: INTERACTION_CREATE (slash commands)
// ---------------------------------------------------------------------------

/// Parsed interaction from `INTERACTION_CREATE`.
#[derive(Debug, Clone)]
pub struct InteractionData {
    pub id: String,
    pub application_id: String,
    /// Interaction type (2 = APPLICATION_COMMAND, 3 = MESSAGE_COMPONENT, …).
    pub interaction_type: u8,
    pub token: String,
    pub channel_id: Option<String>,
    pub guild_id: Option<String>,
    pub user_id: Option<String>,
    pub command_name: Option<String>,
    pub command_options: Vec<InteractionOption>,
}

/// A single option supplied to a slash command invocation.
#[derive(Debug, Clone)]
pub struct InteractionOption {
    pub name: String,
    pub value: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Event types: Reactions
// ---------------------------------------------------------------------------

/// Parsed data from `MESSAGE_REACTION_ADD` / `MESSAGE_REACTION_REMOVE`.
#[derive(Debug, Clone)]
pub struct ReactionEvent {
    pub user_id: String,
    pub channel_id: String,
    pub message_id: String,
    pub guild_id: Option<String>,
    pub emoji_name: Option<String>,
    pub emoji_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Event types: Voice state
// ---------------------------------------------------------------------------

/// Parsed `VOICE_STATE_UPDATE` event.
#[derive(Debug, Clone)]
pub struct VoiceState {
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
    pub user_id: String,
    pub session_id: String,
    pub deaf: bool,
    pub mute: bool,
    pub self_deaf: bool,
    pub self_mute: bool,
    pub suppress: bool,
}

// ---------------------------------------------------------------------------
// Slash command registration types
// ---------------------------------------------------------------------------

/// Definition of a slash command to register with Discord.
#[derive(Debug, Clone, Serialize)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<SlashCommandOption>>,
    /// Command type (1 = CHAT_INPUT, 2 = USER, 3 = MESSAGE). Default 1.
    #[serde(rename = "type", default = "default_command_type")]
    pub command_type: u8,
}

fn default_command_type() -> u8 {
    1
}

/// A single option for a slash command.
#[derive(Debug, Clone, Serialize)]
pub struct SlashCommandOption {
    pub name: String,
    pub description: String,
    /// Option type (3 = STRING, 4 = INTEGER, 5 = BOOLEAN, 6 = USER, …).
    #[serde(rename = "type")]
    pub option_type: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choices: Option<Vec<SlashCommandChoice>>,
}

/// A predefined choice for a slash command option.
#[derive(Debug, Clone, Serialize)]
pub struct SlashCommandChoice {
    pub name: String,
    pub value: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Embed types
// ---------------------------------------------------------------------------

/// A Discord rich embed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordEmbed {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footer: Option<EmbedFooter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<EmbedMedia>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<EmbedMedia>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<EmbedAuthor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<EmbedField>,
}

impl DiscordEmbed {
    pub fn new() -> Self {
        Self {
            title: None,
            description: None,
            url: None,
            color: None,
            timestamp: None,
            footer: None,
            image: None,
            thumbnail: None,
            author: None,
            fields: Vec::new(),
        }
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    pub fn with_color(mut self, color: u32) -> Self {
        self.color = Some(color);
        self
    }

    pub fn with_footer(mut self, text: impl Into<String>) -> Self {
        self.footer = Some(EmbedFooter {
            text: text.into(),
            icon_url: None,
        });
        self
    }

    pub fn with_timestamp(mut self, ts: impl Into<String>) -> Self {
        self.timestamp = Some(ts.into());
        self
    }

    pub fn add_field(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
        inline: bool,
    ) -> Self {
        self.fields.push(EmbedField {
            name: name.into(),
            value: value.into(),
            inline: Some(inline),
        });
        self
    }
}

impl Default for DiscordEmbed {
    fn default() -> Self {
        Self::new()
    }
}

/// Embed footer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedFooter {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
}

/// Embed media (image / thumbnail).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedMedia {
    pub url: String,
}

/// Embed author.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedAuthor {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
}

/// A single field in an embed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedField {
    pub name: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline: Option<bool>,
}

// ---------------------------------------------------------------------------
// Thread creation result
// ---------------------------------------------------------------------------

/// Response from creating a thread.
#[derive(Debug, Clone, Deserialize)]
pub struct DiscordThread {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub thread_type: Option<u8>,
    pub guild_id: Option<String>,
    pub parent_id: Option<String>,
}

// ---------------------------------------------------------------------------
// DiscordAdapter
// ---------------------------------------------------------------------------

/// Discord Bot API platform adapter.
pub struct DiscordAdapter {
    base: BasePlatformAdapter,
    config: DiscordConfig,
    client: Client,
    stop_signal: Arc<Notify>,
    /// In-memory set of seen inbound message IDs, used to skip duplicate
    /// MESSAGE_CREATE / MESSAGE_UPDATE events replayed after a reconnect.
    seen_messages: Arc<Mutex<MessageDedup>>,
    /// Inbound channel to the gateway. Set via
    /// [`DiscordAdapter::set_inbound_sender`] by the registry; `None` means
    /// inbound events are parsed but dropped (send-only mode).
    inbound_tx: Arc<Mutex<Option<mpsc::Sender<IncomingMessage>>>>,
}

impl DiscordAdapter {
    /// Create a new Discord adapter with the given configuration.
    pub fn new(config: DiscordConfig) -> Result<Self, GatewayError> {
        let base = BasePlatformAdapter::new(&config.token).with_proxy(config.proxy.clone());

        base.validate_token()?;

        let client = base.build_client()?;

        Ok(Self {
            base,
            config,
            client,
            stop_signal: Arc::new(Notify::new()),
            seen_messages: Arc::new(Mutex::new(MessageDedup::new())),
            inbound_tx: Arc::new(Mutex::new(None)),
        })
    }

    /// Get a reference to the configuration.
    pub fn config(&self) -> &DiscordConfig {
        &self.config
    }

    /// Attach the gateway's inbound channel. Parsed MESSAGE_CREATE events
    /// are forwarded on it by [`DiscordAdapter::run_supervised`].
    pub async fn set_inbound_sender(&self, tx: mpsc::Sender<IncomingMessage>) {
        *self.inbound_tx.lock().await = Some(tx);
    }

    /// Clone of the stop `Notify`, signaled by [`DiscordAdapter::stop`].
    pub fn stop_notifier(&self) -> Arc<Notify> {
        self.stop_signal.clone()
    }

    /// Convert a parsed MESSAGE_CREATE event into a gateway-level
    /// [`IncomingMessage`]. Guild channels route on the channel ID; DMs
    /// (no `guild_id`) are flagged via `is_dm`.
    pub fn to_incoming(msg: &IncomingDiscordMessage) -> IncomingMessage {
        IncomingMessage {
            platform: "discord".into(),
            chat_id: msg.channel_id.clone(),
            user_id: msg.user_id.clone().unwrap_or_default(),
            text: msg.content.clone(),
            message_id: Some(msg.message_id.clone()),
            thread_id: None,
            is_dm: msg.guild_id.is_none(),
        }
    }

    /// Whether the bot is @-mentioned in `content` (`<@id>` or `<@!id>`
    /// forms), using the configured application ID.
    fn mentioned_self(&self, content: &str) -> bool {
        let Some(app_id) = self.config.application_id.as_deref() else {
            return false;
        };
        content.contains(&format!("<@{app_id}>")) || content.contains(&format!("<@!{app_id}>"))
    }

    /// Forward a parsed MESSAGE_CREATE event to the inbound channel.
    ///
    /// Bot authors (including the bot itself) are always skipped. When
    /// `require_mention` is set, guild-channel messages must @-mention the
    /// bot; DMs always pass.
    async fn forward_message_create(&self, msg: &IncomingDiscordMessage) {
        if msg.is_bot {
            return;
        }
        if self.config.require_mention
            && msg.guild_id.is_some()
            && !self.mentioned_self(&msg.content)
        {
            debug!(message_id = %msg.message_id, "Discord: dropping guild message without mention (require_mention)");
            return;
        }
        let tx = self.inbound_tx.lock().await.clone();
        if let Some(tx) = tx {
            let incoming = Self::to_incoming(msg);
            if tx.send(incoming).await.is_err() {
                debug!("Discord inbound receiver dropped; stopping receive loop");
                self.base.mark_stopped();
            }
        }
    }

    /// Return the authorization header value.
    fn auth_header(&self) -> String {
        format!("Bot {}", self.config.token)
    }

    // -----------------------------------------------------------------------
    // REST API: Sending messages
    // -----------------------------------------------------------------------

    /// Build the JSON body for a plain-text channel message.
    ///
    /// `allowed_mentions.parse` is restricted to `["users"]` so bot replies
    /// can mention users but never ping `@everyone` / `@here` or roles.
    pub fn text_message_body(content: &str) -> serde_json::Value {
        serde_json::json!({
            "content": content,
            "allowed_mentions": { "parse": ["users"] },
        })
    }

    /// Build the JSON body for an embed message.
    ///
    /// Uses the same users-only `allowed_mentions` policy as
    /// [`Self::text_message_body`].
    pub fn embed_message_body(content: Option<&str>, embeds: &[DiscordEmbed]) -> serde_json::Value {
        let mut body = serde_json::json!({
            "embeds": embeds,
            "allowed_mentions": { "parse": ["users"] },
        });
        if let Some(text) = content {
            body["content"] = serde_json::Value::String(text.to_string());
        }
        body
    }

    /// Send a message to a Discord channel, splitting if it exceeds 2000 chars.
    pub async fn send_text(
        &self,
        channel_id: &str,
        content: &str,
    ) -> Result<Vec<String>, GatewayError> {
        let chunks = split_markdown(content, MAX_MESSAGE_LENGTH);
        let mut message_ids = Vec::new();

        for chunk in &chunks {
            let url = format!("{}/channels/{}/messages", DISCORD_API_BASE, channel_id);
            let body = Self::text_message_body(chunk);

            let send_request = || {
                self.client
                    .post(&url)
                    .header("Authorization", self.auth_header())
                    .header("Content-Type", "application/json")
                    .json(&body)
                    .send()
            };
            // Retries once after an HTTP 429 (Discord sends `retry_after`
            // seconds in the JSON body / `Retry-After` header).
            let resp = send_with_rate_limit_retry(send_request)
                .await
                .map_err(|e| GatewayError::SendFailed(format!("Discord send failed: {}", e)))?;

            if !resp.status().is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(GatewayError::SendFailed(format!(
                    "Discord API error: {}",
                    text
                )));
            }

            let msg: DiscordMessage = resp.json().await.map_err(|e| {
                GatewayError::SendFailed(format!("Failed to parse Discord response: {}", e))
            })?;

            message_ids.push(msg.id);
        }

        Ok(message_ids)
    }

    /// Edit an existing message in a Discord channel.
    pub async fn edit_text(
        &self,
        channel_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<(), GatewayError> {
        let url = format!(
            "{}/channels/{}/messages/{}",
            DISCORD_API_BASE, channel_id, message_id
        );

        let body = serde_json::json!({
            "content": &content[..content.floor_char_boundary(content.len().min(MAX_MESSAGE_LENGTH))],
        });

        let resp = self
            .client
            .patch(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("Discord edit failed: {}", e)))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "Discord edit API error: {}",
                text
            )));
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // REST API: Embeds
    // -----------------------------------------------------------------------

    /// Send a message with one or more embeds to a Discord channel.
    pub async fn send_embed(
        &self,
        channel_id: &str,
        content: Option<&str>,
        embeds: &[DiscordEmbed],
    ) -> Result<String, GatewayError> {
        let url = format!("{}/channels/{}/messages", DISCORD_API_BASE, channel_id);

        let body = Self::embed_message_body(content, embeds);

        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("Discord embed send failed: {}", e)))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "Discord embed API error: {}",
                text
            )));
        }

        let msg: DiscordMessage = resp.json().await.map_err(|e| {
            GatewayError::SendFailed(format!("Failed to parse Discord response: {}", e))
        })?;

        Ok(msg.id)
    }

    // -----------------------------------------------------------------------
    // REST API: File uploads
    // -----------------------------------------------------------------------

    /// Upload a file to a Discord channel using multipart form data.
    pub async fn upload_file(
        &self,
        channel_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<String, GatewayError> {
        let url = format!("{}/channels/{}/messages", DISCORD_API_BASE, channel_id);

        let file_bytes = tokio::fs::read(file_path).await.map_err(|e| {
            GatewayError::SendFailed(format!("Failed to read file {}: {}", file_path, e))
        })?;

        let file_name = std::path::Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();

        let part = reqwest::multipart::Part::bytes(file_bytes).file_name(file_name);

        let mut form = reqwest::multipart::Form::new().part("files[0]", part);

        if let Some(cap) = caption {
            let payload = serde_json::json!({ "content": cap });
            form = form.text("payload_json", payload.to_string());
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .multipart(form)
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("Discord file upload failed: {}", e)))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "Discord file upload API error: {}",
                text
            )));
        }

        let msg: DiscordMessage = resp.json().await.map_err(|e| {
            GatewayError::SendFailed(format!("Failed to parse Discord response: {}", e))
        })?;

        Ok(msg.id)
    }

    // -----------------------------------------------------------------------
    // REST API: Reactions
    // -----------------------------------------------------------------------

    /// Add a reaction to a message.
    ///
    /// `emoji` should be a URL-encoded unicode emoji (e.g. `%F0%9F%91%8D`)
    /// or a custom emoji in the form `name:id`.
    pub async fn add_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), GatewayError> {
        let url = format!(
            "{}/channels/{}/messages/{}/reactions/{}/@me",
            DISCORD_API_BASE, channel_id, message_id, emoji
        );

        let resp = self
            .client
            .put(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Length", "0")
            .send()
            .await
            .map_err(|e| GatewayError::SendFailed(format!("Discord add_reaction failed: {}", e)))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "Discord add_reaction API error: {}",
                text
            )));
        }

        Ok(())
    }

    /// Remove the bot's own reaction from a message.
    pub async fn remove_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), GatewayError> {
        let url = format!(
            "{}/channels/{}/messages/{}/reactions/{}/@me",
            DISCORD_API_BASE, channel_id, message_id, emoji
        );

        let resp = self
            .client
            .delete(&url)
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| {
                GatewayError::SendFailed(format!("Discord remove_reaction failed: {}", e))
            })?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "Discord remove_reaction API error: {}",
                text
            )));
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // REST API: Threads
    // -----------------------------------------------------------------------

    /// Create a public thread from an existing message.
    pub async fn create_thread(
        &self,
        channel_id: &str,
        message_id: &str,
        name: &str,
        auto_archive_duration: Option<u32>,
    ) -> Result<DiscordThread, GatewayError> {
        let url = format!(
            "{}/channels/{}/messages/{}/threads",
            DISCORD_API_BASE, channel_id, message_id
        );

        let mut body = serde_json::json!({ "name": name });
        if let Some(dur) = auto_archive_duration {
            body["auto_archive_duration"] = serde_json::Value::Number(dur.into());
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                GatewayError::SendFailed(format!("Discord create_thread failed: {}", e))
            })?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "Discord create_thread API error: {}",
                text
            )));
        }

        let thread: DiscordThread = resp.json().await.map_err(|e| {
            GatewayError::SendFailed(format!("Failed to parse thread response: {}", e))
        })?;

        Ok(thread)
    }

    // -----------------------------------------------------------------------
    // REST API: Slash command registration
    // -----------------------------------------------------------------------

    /// Register (overwrite) global application commands.
    ///
    /// This uses the bulk-overwrite endpoint which replaces all existing
    /// global commands with the ones provided.
    pub async fn register_slash_commands(
        &self,
        commands: &[SlashCommand],
    ) -> Result<(), GatewayError> {
        let app_id = self.config.application_id.as_deref().ok_or_else(|| {
            GatewayError::Platform("application_id required for slash commands".into())
        })?;

        let url = format!("{}/applications/{}/commands", DISCORD_API_BASE, app_id);

        let resp = self
            .client
            .put(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(commands)
            .send()
            .await
            .map_err(|e| {
                GatewayError::SendFailed(format!("Discord register_commands failed: {}", e))
            })?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "Discord register_commands API error: {}",
                text
            )));
        }

        info!("registered {} global slash commands", commands.len());
        Ok(())
    }

    /// Register application commands scoped to a specific guild (faster
    /// propagation, useful during development).
    pub async fn register_guild_slash_commands(
        &self,
        guild_id: &str,
        commands: &[SlashCommand],
    ) -> Result<(), GatewayError> {
        let app_id = self.config.application_id.as_deref().ok_or_else(|| {
            GatewayError::Platform("application_id required for slash commands".into())
        })?;

        let url = format!(
            "{}/applications/{}/guilds/{}/commands",
            DISCORD_API_BASE, app_id, guild_id
        );

        let resp = self
            .client
            .put(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(commands)
            .send()
            .await
            .map_err(|e| {
                GatewayError::SendFailed(format!("Discord register_guild_commands failed: {}", e))
            })?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "Discord register_guild_commands API error: {}",
                text
            )));
        }

        info!(
            "registered {} guild slash commands for {}",
            commands.len(),
            guild_id
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // REST API: Interaction responses
    // -----------------------------------------------------------------------

    /// Send an initial response to an interaction (slash command, button, etc.).
    pub async fn respond_to_interaction(
        &self,
        interaction_id: &str,
        interaction_token: &str,
        content: &str,
    ) -> Result<(), GatewayError> {
        let url = format!(
            "{}/interactions/{}/{}/callback",
            DISCORD_API_BASE, interaction_id, interaction_token
        );

        let body = serde_json::json!({
            "type": 4, // CHANNEL_MESSAGE_WITH_SOURCE
            "data": { "content": content }
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                GatewayError::SendFailed(format!("Discord interaction response failed: {}", e))
            })?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "Discord interaction response API error: {}",
                text
            )));
        }

        Ok(())
    }

    /// Send a deferred response (shows "thinking..." indicator).
    pub async fn defer_interaction(
        &self,
        interaction_id: &str,
        interaction_token: &str,
    ) -> Result<(), GatewayError> {
        let url = format!(
            "{}/interactions/{}/{}/callback",
            DISCORD_API_BASE, interaction_id, interaction_token
        );

        let body = serde_json::json!({
            "type": 5, // DEFERRED_CHANNEL_MESSAGE_WITH_SOURCE
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                GatewayError::SendFailed(format!("Discord defer interaction failed: {}", e))
            })?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::SendFailed(format!(
                "Discord defer interaction API error: {}",
                text
            )));
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Gateway WebSocket helpers
    // -----------------------------------------------------------------------

    /// Build an IDENTIFY payload for the Discord Gateway.
    pub fn build_identify_payload(&self) -> GatewayPayload {
        GatewayPayload {
            op: opcodes::IDENTIFY,
            d: Some(
                serde_json::to_value(IdentifyData {
                    token: self.config.token.clone(),
                    intents: self.config.intents,
                    properties: IdentifyProperties {
                        os: "linux".into(),
                        browser: "hermes-agent".into(),
                        device: "hermes-agent".into(),
                    },
                })
                .unwrap(),
            ),
            s: None,
            t: None,
        }
    }

    /// Build a HEARTBEAT payload.
    pub fn build_heartbeat_payload(sequence: Option<u64>) -> GatewayPayload {
        GatewayPayload {
            op: opcodes::HEARTBEAT,
            d: sequence.map(|s| serde_json::Value::Number(s.into())),
            s: None,
            t: None,
        }
    }

    /// Build a RESUME payload.
    pub fn build_resume_payload(&self, session_id: &str, seq: u64) -> GatewayPayload {
        GatewayPayload {
            op: opcodes::RESUME,
            d: Some(
                serde_json::to_value(ResumeData {
                    token: self.config.token.clone(),
                    session_id: session_id.to_string(),
                    seq,
                })
                .unwrap(),
            ),
            s: None,
            t: None,
        }
    }

    // -----------------------------------------------------------------------
    // Gateway WebSocket supervised connection
    // -----------------------------------------------------------------------

    /// Send one gateway payload as a JSON text frame.
    async fn send_gateway_payload(
        sink: &mut futures::stream::SplitSink<
            WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
            WsMessage,
        >,
        payload: &GatewayPayload,
    ) -> Result<(), GatewayError> {
        let text = serde_json::to_string(payload)
            .map_err(|e| GatewayError::SendFailed(format!("failed to encode payload: {}", e)))?;
        sink.send(WsMessage::Text(text))
            .await
            .map_err(|e| GatewayError::ConnectionFailed(format!("gateway send failed: {}", e)))
    }

    /// Serve a single gateway WebSocket connection.
    ///
    /// Drives `session` through the handshake (HELLO → IDENTIFY or RESUME),
    /// the heartbeat loop, and inbound dispatch (with message-ID dedup).
    /// Returns when the connection closes, errors, the gateway requests a
    /// reconnect, the session is invalidated, or [`Self::stop`] is called.
    /// The session state is preserved so the caller can RESUME on reconnect.
    async fn serve_connection(
        &self,
        ws_stream: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
        session: &mut GatewaySession,
    ) {
        let (mut sink, mut stream) = ws_stream.split();
        let mut next_heartbeat: Option<tokio::time::Instant> = None;

        loop {
            let heartbeat_sleep = async {
                match next_heartbeat {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => futures::future::pending().await,
                }
            };

            tokio::select! {
                _ = self.stop_signal.notified() => {
                    let _ = sink.close().await;
                    return;
                }
                _ = heartbeat_sleep => {
                    // A missed ACK means the connection is zombied: drop it so
                    // the supervise loop reconnects (and RESUMEs if possible).
                    if session.is_zombie() {
                        warn!("Discord heartbeat not acknowledged; reconnecting");
                        let _ = sink.close().await;
                        return;
                    }
                    session.heartbeat_sent();
                    let payload = Self::build_heartbeat_payload(session.sequence);
                    if Self::send_gateway_payload(&mut sink, &payload).await.is_err() {
                        return;
                    }
                    if let Some(ms) = session.heartbeat_interval_ms {
                        next_heartbeat =
                            Some(tokio::time::Instant::now() + Duration::from_millis(ms));
                    }
                }
                msg = stream.next() => {
                    match msg {
                        Some(Ok(WsMessage::Text(text))) => {
                            let payload: GatewayPayload = match serde_json::from_str(text.as_str()) {
                                Ok(p) => p,
                                Err(e) => {
                                    debug!("ignoring malformed gateway payload: {}", e);
                                    continue;
                                }
                            };
                            let actions = session.handle_gateway_event(&payload);
                            if next_heartbeat.is_none() {
                                if let Some(ms) = session.heartbeat_interval_ms {
                                    next_heartbeat = Some(
                                        tokio::time::Instant::now() + Duration::from_millis(ms),
                                    );
                                }
                            }
                            let mut drop_connection = false;
                            for action in actions {
                                let send_result = match action {
                                    GatewayAction::SendIdentify => {
                                        Self::send_gateway_payload(
                                            &mut sink,
                                            &self.build_identify_payload(),
                                        )
                                        .await
                                    }
                                    GatewayAction::SendHeartbeat => {
                                        session.heartbeat_sent();
                                        Self::send_gateway_payload(
                                            &mut sink,
                                            &Self::build_heartbeat_payload(session.sequence),
                                        )
                                        .await
                                    }
                                    GatewayAction::SendResume => {
                                        match (session.session_id.clone(), session.sequence) {
                                            (Some(sid), Some(seq)) => {
                                                Self::send_gateway_payload(
                                                    &mut sink,
                                                    &self.build_resume_payload(&sid, seq),
                                                )
                                                .await
                                            }
                                            // Session data vanished mid-flight; fall back
                                            // to a fresh IDENTIFY.
                                            _ => {
                                                Self::send_gateway_payload(
                                                    &mut sink,
                                                    &self.build_identify_payload(),
                                                )
                                                .await
                                            }
                                        }
                                    }
                                    GatewayAction::Reconnect => {
                                        debug!("gateway requested reconnect");
                                        drop_connection = true;
                                        Ok(())
                                    }
                                    GatewayAction::InvalidSession(_) => {
                                        // handle_invalid_session already cleared the
                                        // session state when not resumable.
                                        drop_connection = true;
                                        Ok(())
                                    }
                                    GatewayAction::Dispatch(name, data) => {
                                        // Dedup applies to MESSAGE_CREATE /
                                        // MESSAGE_UPDATE; other events pass through.
                                        let event = self.dispatch_event(&name, &data).await;
                                        match event {
                                            Some(DispatchEvent::MessageCreate(msg)) => {
                                                self.forward_message_create(&msg).await;
                                            }
                                            Some(_) => {
                                                debug!(event = %name, "dispatch event received");
                                            }
                                            None => {
                                                debug!(event = %name, "duplicate dispatch event skipped");
                                            }
                                        }
                                        Ok(())
                                    }
                                };
                                if send_result.is_err() {
                                    drop_connection = true;
                                    break;
                                }
                            }
                            if drop_connection {
                                let _ = sink.close().await;
                                return;
                            }
                        }
                        Some(Ok(WsMessage::Ping(p))) => {
                            let _ = sink.send(WsMessage::Pong(p)).await;
                        }
                        Some(Ok(WsMessage::Close(_))) | None => {
                            debug!("Discord gateway connection closed by peer");
                            return;
                        }
                        Some(Err(e)) => {
                            warn!("Discord gateway WebSocket error: {}", e);
                            return;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Run the supervised gateway connection loop until [`Self::stop`].
    ///
    /// Connects to the Discord Gateway WebSocket, serves the connection via
    /// [`Self::serve_connection`], and on any disconnect reconnects with
    /// exponential backoff ([`reconnect_delay`]). When the [`GatewaySession`]
    /// still holds a `session_id` and sequence number, the reconnect attempts
    /// a RESUME; otherwise it performs a fresh IDENTIFY.
    ///
    /// This is an explicit opt-in driver: spawn it from the gateway
    /// orchestrator, e.g.
    /// `tokio::spawn(async move { adapter.run_supervised().await })`.
    /// It is deliberately *not* started from [`PlatformAdapter::start`] so
    /// that callers keep control over the connection lifecycle.
    pub async fn run_supervised(&self) {
        let mut attempt: u32 = 0;
        let mut session = GatewaySession::new();

        while self.base.is_running() {
            let url = session
                .resume_gateway_url
                .clone()
                .unwrap_or_else(|| DISCORD_GATEWAY_URL.to_string());

            match connect_async(&url).await {
                Ok((ws_stream, _)) => {
                    info!("Discord gateway connected");
                    attempt = 0;
                    self.serve_connection(ws_stream, &mut session).await;
                    debug!("Discord gateway connection ended; will reconnect");
                }
                Err(e) => {
                    warn!("Discord gateway connect failed: {}", e);
                }
            }

            if !self.base.is_running() {
                break;
            }

            let delay = reconnect_delay(attempt);
            attempt = attempt.saturating_add(1);
            info!("Discord reconnecting in {:?} (attempt {})", delay, attempt);
            tokio::select! {
                _ = self.stop_signal.notified() => break,
                _ = tokio::time::sleep(delay) => {}
            }
        }

        info!("Discord supervised gateway loop exited");
    }

    // -----------------------------------------------------------------------
    // Event parsing
    // -----------------------------------------------------------------------

    /// Parse a MESSAGE_CREATE dispatch event into an IncomingDiscordMessage.
    pub fn parse_message_create(data: &serde_json::Value) -> Option<IncomingDiscordMessage> {
        let channel_id = data.get("channel_id")?.as_str()?.to_string();
        let message_id = data.get("id")?.as_str()?.to_string();
        let content = data
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let author = data.get("author");
        let user_id = author
            .and_then(|a| a.get("id"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let username = author
            .and_then(|a| a.get("username"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let is_bot = author
            .and_then(|a| a.get("bot"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let guild_id = data
            .get("guild_id")
            .and_then(|v| v.as_str())
            .map(String::from);

        Some(IncomingDiscordMessage {
            channel_id,
            message_id,
            user_id,
            username,
            content,
            is_bot,
            guild_id,
        })
    }

    /// Parse a MESSAGE_UPDATE dispatch event.
    pub fn parse_message_update(data: &serde_json::Value) -> Option<MessageUpdateEvent> {
        let channel_id = data.get("channel_id")?.as_str()?.to_string();
        let message_id = data.get("id")?.as_str()?.to_string();

        let content = data
            .get("content")
            .and_then(|v| v.as_str())
            .map(String::from);
        let author_id = data
            .get("author")
            .and_then(|a| a.get("id"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let guild_id = data
            .get("guild_id")
            .and_then(|v| v.as_str())
            .map(String::from);

        Some(MessageUpdateEvent {
            channel_id,
            message_id,
            content,
            author_id,
            guild_id,
        })
    }

    /// Parse an INTERACTION_CREATE dispatch event.
    pub fn parse_interaction_create(data: &serde_json::Value) -> Option<InteractionData> {
        let id = data.get("id")?.as_str()?.to_string();
        let application_id = data.get("application_id")?.as_str()?.to_string();
        let interaction_type = data.get("type")?.as_u64()? as u8;
        let token = data.get("token")?.as_str()?.to_string();

        let channel_id = data
            .get("channel_id")
            .and_then(|v| v.as_str())
            .map(String::from);
        let guild_id = data
            .get("guild_id")
            .and_then(|v| v.as_str())
            .map(String::from);

        // User ID can be in `member.user.id` (guild) or `user.id` (DM).
        let user_id = data
            .get("member")
            .and_then(|m| m.get("user"))
            .and_then(|u| u.get("id"))
            .and_then(|v| v.as_str())
            .or_else(|| {
                data.get("user")
                    .and_then(|u| u.get("id"))
                    .and_then(|v| v.as_str())
            })
            .map(String::from);

        let cmd_data = data.get("data");
        let command_name = cmd_data
            .and_then(|d| d.get("name"))
            .and_then(|v| v.as_str())
            .map(String::from);

        let command_options = cmd_data
            .and_then(|d| d.get("options"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|opt| {
                        let name = opt.get("name")?.as_str()?.to_string();
                        let value = opt.get("value").cloned().unwrap_or(serde_json::Value::Null);
                        Some(InteractionOption { name, value })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Some(InteractionData {
            id,
            application_id,
            interaction_type,
            token,
            channel_id,
            guild_id,
            user_id,
            command_name,
            command_options,
        })
    }

    /// Parse a MESSAGE_REACTION_ADD or MESSAGE_REACTION_REMOVE event.
    pub fn parse_reaction_event(data: &serde_json::Value) -> Option<ReactionEvent> {
        let user_id = data.get("user_id")?.as_str()?.to_string();
        let channel_id = data.get("channel_id")?.as_str()?.to_string();
        let message_id = data.get("message_id")?.as_str()?.to_string();

        let guild_id = data
            .get("guild_id")
            .and_then(|v| v.as_str())
            .map(String::from);

        let emoji = data.get("emoji");
        let emoji_name = emoji
            .and_then(|e| e.get("name"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let emoji_id = emoji
            .and_then(|e| e.get("id"))
            .and_then(|v| v.as_str())
            .map(String::from);

        Some(ReactionEvent {
            user_id,
            channel_id,
            message_id,
            guild_id,
            emoji_name,
            emoji_id,
        })
    }

    /// Parse a VOICE_STATE_UPDATE event.
    pub fn parse_voice_state_update(data: &serde_json::Value) -> Option<VoiceState> {
        let user_id = data.get("user_id")?.as_str()?.to_string();
        let session_id = data.get("session_id")?.as_str()?.to_string();

        let guild_id = data
            .get("guild_id")
            .and_then(|v| v.as_str())
            .map(String::from);
        let channel_id = data
            .get("channel_id")
            .and_then(|v| v.as_str())
            .map(String::from);

        let deaf = data.get("deaf").and_then(|v| v.as_bool()).unwrap_or(false);
        let mute = data.get("mute").and_then(|v| v.as_bool()).unwrap_or(false);
        let self_deaf = data
            .get("self_deaf")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let self_mute = data
            .get("self_mute")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let suppress = data
            .get("suppress")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Some(VoiceState {
            guild_id,
            channel_id,
            user_id,
            session_id,
            deaf,
            mute,
            self_deaf,
            self_mute,
            suppress,
        })
    }

    /// Route a dispatch event by name to the appropriate parser.
    ///
    /// Returns a [`DispatchEvent`] for known event types, or `None`.
    pub fn parse_dispatch(event_name: &str, data: &serde_json::Value) -> Option<DispatchEvent> {
        match event_name {
            "MESSAGE_CREATE" => Self::parse_message_create(data).map(DispatchEvent::MessageCreate),
            "MESSAGE_UPDATE" => Self::parse_message_update(data).map(DispatchEvent::MessageUpdate),
            "INTERACTION_CREATE" => {
                Self::parse_interaction_create(data).map(DispatchEvent::InteractionCreate)
            }
            "MESSAGE_REACTION_ADD" => {
                Self::parse_reaction_event(data).map(DispatchEvent::ReactionAdd)
            }
            "MESSAGE_REACTION_REMOVE" => {
                Self::parse_reaction_event(data).map(DispatchEvent::ReactionRemove)
            }
            "VOICE_STATE_UPDATE" => {
                Self::parse_voice_state_update(data).map(DispatchEvent::VoiceStateUpdate)
            }
            _ => None,
        }
    }

    /// Parse and filter an inbound dispatch event, applying message-ID dedup.
    ///
    /// Like [`Self::parse_dispatch`], but `MESSAGE_CREATE` / `MESSAGE_UPDATE`
    /// events whose message ID was already seen are dropped (returns `None`)
    /// so reconnect replays cannot double-process a message. This is the
    /// inbound path used by [`Self::run_supervised`].
    pub async fn dispatch_event(
        &self,
        event_name: &str,
        data: &serde_json::Value,
    ) -> Option<DispatchEvent> {
        let event = Self::parse_dispatch(event_name, data)?;
        let message_id = match &event {
            DispatchEvent::MessageCreate(m) => Some(m.message_id.clone()),
            DispatchEvent::MessageUpdate(m) => Some(m.message_id.clone()),
            _ => None,
        };
        if let Some(id) = message_id {
            let mut seen = self.seen_messages.lock().await;
            if !seen.insert(id.clone()) {
                debug!("skipping duplicate Discord message {}", id);
                return None;
            }
        }
        Some(event)
    }
}

// ---------------------------------------------------------------------------
// Typed dispatch events
// ---------------------------------------------------------------------------

/// A strongly-typed dispatch event produced by [`DiscordAdapter::parse_dispatch`].
#[derive(Debug, Clone)]
pub enum DispatchEvent {
    MessageCreate(IncomingDiscordMessage),
    MessageUpdate(MessageUpdateEvent),
    InteractionCreate(InteractionData),
    ReactionAdd(ReactionEvent),
    ReactionRemove(ReactionEvent),
    VoiceStateUpdate(VoiceState),
}

// ---------------------------------------------------------------------------
// PlatformAdapter trait impl
// ---------------------------------------------------------------------------

#[async_trait]
impl PlatformAdapter for DiscordAdapter {
    async fn start(&self) -> Result<(), GatewayError> {
        info!(
            "Discord adapter starting (token: {}...)",
            &self.config.token[..8.min(self.config.token.len())]
        );
        self.base.mark_running();
        Ok(())
    }

    async fn stop(&self) -> Result<(), GatewayError> {
        info!("Discord adapter stopping");
        self.base.mark_stopped();
        self.stop_signal.notify_one();
        Ok(())
    }

    async fn send_message(
        &self,
        chat_id: &str,
        text: &str,
        _parse_mode: Option<ParseMode>,
    ) -> Result<(), GatewayError> {
        self.send_text(chat_id, text).await?;
        Ok(())
    }

    async fn edit_message(
        &self,
        chat_id: &str,
        message_id: &str,
        text: &str,
    ) -> Result<(), GatewayError> {
        self.edit_text(chat_id, message_id, text).await
    }

    async fn send_file(
        &self,
        chat_id: &str,
        file_path: &str,
        caption: Option<&str>,
    ) -> Result<(), GatewayError> {
        self.upload_file(chat_id, file_path, caption).await?;
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }

    fn platform_name(&self) -> &str {
        "discord"
    }

    async fn maintenance_prune(&self) {
        self.seen_messages.lock().await.trim();
    }
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

/// URL-encode a unicode emoji for use in reaction endpoints.
pub fn encode_emoji(emoji: &str) -> String {
    percent_encode_emoji(emoji)
}

fn percent_encode_emoji(s: &str) -> String {
    let mut out = String::new();
    for byte in s.as_bytes() {
        if byte.is_ascii_alphanumeric() || *byte == b'-' || *byte == b'_' || *byte == b':' {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- existing tests (preserved) -----------------------------------------

    #[test]
    fn split_markdown_discord_limit() {
        // Fence-aware splitting at Discord's 2000-char limit.
        let code: String = (0..150)
            .map(|i| format!("let value_{i} = compute({i});"))
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!("Intro\n```rust\n{code}\n```\nOutro");
        let chunks = split_markdown(&text, 2000);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.len() <= 2100, "chunk exceeds Discord limit: {}", c.len());
            assert_eq!(c.matches("```").count() % 2, 0);
        }
    }

    #[test]
    fn gateway_payload_identify() {
        let config = DiscordConfig {
            token: "test-token".into(),
            application_id: None,
            proxy: AdapterProxyConfig::default(),
            require_mention: false,
            intents: default_intents(),
        };
        let adapter = DiscordAdapter::new(config).unwrap();
        let payload = adapter.build_identify_payload();
        assert_eq!(payload.op, opcodes::IDENTIFY);
        assert!(payload.d.is_some());
    }

    #[test]
    fn gateway_payload_heartbeat() {
        let payload = DiscordAdapter::build_heartbeat_payload(Some(42));
        assert_eq!(payload.op, opcodes::HEARTBEAT);
        assert_eq!(payload.d, Some(serde_json::Value::Number(42.into())));
    }

    #[test]
    fn parse_message_create_event() {
        let data = serde_json::json!({
            "id": "msg123",
            "channel_id": "ch456",
            "content": "hello world",
            "author": {
                "id": "user789",
                "username": "testuser",
                "bot": false
            }
        });

        let msg = DiscordAdapter::parse_message_create(&data).unwrap();
        assert_eq!(msg.channel_id, "ch456");
        assert_eq!(msg.message_id, "msg123");
        assert_eq!(msg.content, "hello world");
        assert_eq!(msg.user_id, Some("user789".into()));
        assert_eq!(msg.username, Some("testuser".into()));
        assert!(!msg.is_bot);
    }

    #[test]
    fn parse_message_create_bot() {
        let data = serde_json::json!({
            "id": "msg1",
            "channel_id": "ch1",
            "content": "bot msg",
            "author": { "id": "bot1", "username": "mybot", "bot": true }
        });

        let msg = DiscordAdapter::parse_message_create(&data).unwrap();
        assert!(msg.is_bot);
    }

    #[test]
    fn parse_message_create_extracts_guild_id() {
        let data = serde_json::json!({
            "id": "msg1",
            "channel_id": "ch1",
            "guild_id": "guild9",
            "content": "hello",
            "author": { "id": "u1", "username": "user", "bot": false }
        });

        let msg = DiscordAdapter::parse_message_create(&data).unwrap();
        assert_eq!(msg.guild_id, Some("guild9".into()));

        // DM channels have no guild_id.
        let dm = serde_json::json!({
            "id": "msg2",
            "channel_id": "dm1",
            "content": "hi",
            "author": { "id": "u1", "username": "user", "bot": false }
        });
        let msg = DiscordAdapter::parse_message_create(&dm).unwrap();
        assert_eq!(msg.guild_id, None);
    }

    #[test]
    fn to_incoming_dm_vs_guild() {
        let dm_msg = IncomingDiscordMessage {
            channel_id: "dm1".into(),
            message_id: "m1".into(),
            user_id: Some("u1".into()),
            username: None,
            content: "hi".into(),
            is_bot: false,
            guild_id: None,
        };
        let incoming = DiscordAdapter::to_incoming(&dm_msg);
        assert_eq!(incoming.platform, "discord");
        assert_eq!(incoming.chat_id, "dm1");
        assert_eq!(incoming.user_id, "u1");
        assert!(incoming.is_dm);
        assert_eq!(incoming.message_id, Some("m1".into()));

        let guild_msg = IncomingDiscordMessage {
            guild_id: Some("g1".into()),
            ..dm_msg
        };
        let incoming = DiscordAdapter::to_incoming(&guild_msg);
        assert!(!incoming.is_dm);
    }

    fn mention_test_adapter() -> DiscordAdapter {
        let config = DiscordConfig {
            token: "test-token".into(),
            application_id: Some("123456".into()),
            proxy: AdapterProxyConfig::default(),
            require_mention: true,
            intents: default_intents(),
        };
        DiscordAdapter::new(config).unwrap()
    }

    #[test]
    fn mentioned_self_detects_mention_forms() {
        let adapter = mention_test_adapter();
        assert!(adapter.mentioned_self("hey <@123456> do this"));
        assert!(adapter.mentioned_self("hey <@!123456> do this"));
        assert!(!adapter.mentioned_self("hey <@999999> do this"));
        assert!(!adapter.mentioned_self("no mention here"));

        // Without an application_id nothing can be mentioned.
        let config = DiscordConfig {
            token: "test-token".into(),
            application_id: None,
            proxy: AdapterProxyConfig::default(),
            require_mention: true,
            intents: default_intents(),
        };
        let adapter = DiscordAdapter::new(config).unwrap();
        assert!(!adapter.mentioned_self("<@123456> hi"));
    }

    // -- GatewaySession tests -----------------------------------------------

    #[test]
    fn session_handles_hello() {
        let mut session = GatewaySession::new();
        let payload = GatewayPayload {
            op: opcodes::HELLO,
            d: Some(serde_json::json!({ "heartbeat_interval": 41250 })),
            s: None,
            t: None,
        };

        let actions = session.handle_gateway_event(&payload);
        assert_eq!(session.heartbeat_interval_ms, Some(41250));
        assert!(actions.contains(&GatewayAction::SendHeartbeat));
        assert!(actions.contains(&GatewayAction::SendIdentify));
    }

    #[test]
    fn session_handles_hello_with_resume() {
        let mut session = GatewaySession::new();
        session.session_id = Some("sess123".into());
        session.sequence = Some(42);

        let payload = GatewayPayload {
            op: opcodes::HELLO,
            d: Some(serde_json::json!({ "heartbeat_interval": 30000 })),
            s: None,
            t: None,
        };

        let actions = session.handle_gateway_event(&payload);
        assert!(actions.contains(&GatewayAction::SendResume));
        assert!(!actions.contains(&GatewayAction::SendIdentify));
    }

    #[test]
    fn session_handles_heartbeat_ack() {
        let mut session = GatewaySession::new();
        session.heartbeat_acknowledged = false;

        let payload = GatewayPayload {
            op: opcodes::HEARTBEAT_ACK,
            d: None,
            s: None,
            t: None,
        };

        session.handle_gateway_event(&payload);
        assert!(session.heartbeat_acknowledged);
    }

    #[test]
    fn session_handles_reconnect() {
        let mut session = GatewaySession::new();
        let payload = GatewayPayload {
            op: opcodes::RECONNECT,
            d: None,
            s: None,
            t: None,
        };

        let actions = session.handle_gateway_event(&payload);
        assert_eq!(actions, vec![GatewayAction::Reconnect]);
    }

    #[test]
    fn session_handles_invalid_session_resumable() {
        let mut session = GatewaySession::new();
        session.session_id = Some("sess".into());
        session.sequence = Some(10);

        let payload = GatewayPayload {
            op: opcodes::INVALID_SESSION,
            d: Some(serde_json::Value::Bool(true)),
            s: None,
            t: None,
        };

        let actions = session.handle_gateway_event(&payload);
        assert_eq!(actions, vec![GatewayAction::InvalidSession(true)]);
        assert!(session.session_id.is_some());
    }

    #[test]
    fn session_handles_invalid_session_not_resumable() {
        let mut session = GatewaySession::new();
        session.session_id = Some("sess".into());
        session.sequence = Some(10);

        let payload = GatewayPayload {
            op: opcodes::INVALID_SESSION,
            d: Some(serde_json::Value::Bool(false)),
            s: None,
            t: None,
        };

        let actions = session.handle_gateway_event(&payload);
        assert_eq!(actions, vec![GatewayAction::InvalidSession(false)]);
        assert!(session.session_id.is_none());
        assert!(session.sequence.is_none());
    }

    #[test]
    fn session_handles_ready_dispatch() {
        let mut session = GatewaySession::new();
        let payload = GatewayPayload {
            op: opcodes::DISPATCH,
            d: Some(serde_json::json!({
                "session_id": "abc123",
                "resume_gateway_url": "wss://resume.discord.gg",
                "user": { "id": "12345", "username": "testbot" }
            })),
            s: Some(1),
            t: Some("READY".into()),
        };

        let actions = session.handle_gateway_event(&payload);
        assert_eq!(session.session_id, Some("abc123".into()));
        assert_eq!(
            session.resume_gateway_url,
            Some("wss://resume.discord.gg".into())
        );
        assert_eq!(session.sequence, Some(1));
        assert!(session.identified);

        assert_eq!(actions.len(), 1);
        match &actions[0] {
            GatewayAction::Dispatch(name, _) => assert_eq!(name, "READY"),
            other => panic!("expected Dispatch, got {:?}", other),
        }
    }

    #[test]
    fn session_tracks_sequence() {
        let mut session = GatewaySession::new();
        let payload = GatewayPayload {
            op: opcodes::DISPATCH,
            d: Some(serde_json::json!({})),
            s: Some(42),
            t: Some("GUILD_CREATE".into()),
        };

        session.handle_gateway_event(&payload);
        assert_eq!(session.sequence, Some(42));
    }

    #[test]
    fn session_zombie_detection() {
        let mut session = GatewaySession::new();
        assert!(!session.is_zombie());

        session.heartbeat_sent();
        assert!(session.is_zombie());

        session.heartbeat_acknowledged = true;
        assert!(!session.is_zombie());
    }

    #[test]
    fn session_reset() {
        let mut session = GatewaySession::new();
        session.session_id = Some("s".into());
        session.sequence = Some(99);
        session.heartbeat_interval_ms = Some(5000);
        session.identified = true;

        session.reset();
        assert!(session.session_id.is_none());
        assert!(session.sequence.is_none());
        assert!(session.heartbeat_interval_ms.is_none());
        assert!(!session.identified);
    }

    #[test]
    fn session_heartbeat_request() {
        let mut session = GatewaySession::new();
        let payload = GatewayPayload {
            op: opcodes::HEARTBEAT,
            d: None,
            s: None,
            t: None,
        };

        let actions = session.handle_gateway_event(&payload);
        assert_eq!(actions, vec![GatewayAction::SendHeartbeat]);
    }

    // -- Event parsing tests ------------------------------------------------

    #[test]
    fn parse_message_update_full() {
        let data = serde_json::json!({
            "id": "msg100",
            "channel_id": "ch200",
            "content": "edited content",
            "author": { "id": "user300" },
            "guild_id": "guild400"
        });

        let evt = DiscordAdapter::parse_message_update(&data).unwrap();
        assert_eq!(evt.message_id, "msg100");
        assert_eq!(evt.channel_id, "ch200");
        assert_eq!(evt.content, Some("edited content".into()));
        assert_eq!(evt.author_id, Some("user300".into()));
        assert_eq!(evt.guild_id, Some("guild400".into()));
    }

    #[test]
    fn parse_message_update_partial() {
        let data = serde_json::json!({
            "id": "msg100",
            "channel_id": "ch200"
        });

        let evt = DiscordAdapter::parse_message_update(&data).unwrap();
        assert!(evt.content.is_none());
        assert!(evt.author_id.is_none());
    }

    #[test]
    fn parse_interaction_create_slash_command() {
        let data = serde_json::json!({
            "id": "int1",
            "application_id": "app1",
            "type": 2,
            "token": "tok1",
            "channel_id": "ch1",
            "guild_id": "g1",
            "member": {
                "user": { "id": "u1" }
            },
            "data": {
                "name": "hello",
                "options": [
                    { "name": "target", "value": "world" },
                    { "name": "count", "value": 3 }
                ]
            }
        });

        let interaction = DiscordAdapter::parse_interaction_create(&data).unwrap();
        assert_eq!(interaction.id, "int1");
        assert_eq!(interaction.interaction_type, 2);
        assert_eq!(interaction.command_name, Some("hello".into()));
        assert_eq!(interaction.user_id, Some("u1".into()));
        assert_eq!(interaction.command_options.len(), 2);
        assert_eq!(interaction.command_options[0].name, "target");
        assert_eq!(
            interaction.command_options[0].value,
            serde_json::json!("world")
        );
        assert_eq!(interaction.command_options[1].name, "count");
        assert_eq!(interaction.command_options[1].value, serde_json::json!(3));
    }

    #[test]
    fn parse_interaction_create_dm() {
        let data = serde_json::json!({
            "id": "int2",
            "application_id": "app2",
            "type": 2,
            "token": "tok2",
            "user": { "id": "dm_user" },
            "data": { "name": "ping" }
        });

        let interaction = DiscordAdapter::parse_interaction_create(&data).unwrap();
        assert_eq!(interaction.user_id, Some("dm_user".into()));
        assert!(interaction.guild_id.is_none());
        assert!(interaction.command_options.is_empty());
    }

    #[test]
    fn parse_reaction_add_event() {
        let data = serde_json::json!({
            "user_id": "u1",
            "channel_id": "ch1",
            "message_id": "msg1",
            "guild_id": "g1",
            "emoji": {
                "name": "\u{1f44d}",
                "id": null
            }
        });

        let evt = DiscordAdapter::parse_reaction_event(&data).unwrap();
        assert_eq!(evt.user_id, "u1");
        assert_eq!(evt.channel_id, "ch1");
        assert_eq!(evt.message_id, "msg1");
        assert_eq!(evt.guild_id, Some("g1".into()));
        assert_eq!(evt.emoji_name, Some("\u{1f44d}".into()));
        assert!(evt.emoji_id.is_none());
    }

    #[test]
    fn parse_reaction_custom_emoji() {
        let data = serde_json::json!({
            "user_id": "u2",
            "channel_id": "ch2",
            "message_id": "msg2",
            "emoji": {
                "name": "custom_emote",
                "id": "12345678"
            }
        });

        let evt = DiscordAdapter::parse_reaction_event(&data).unwrap();
        assert_eq!(evt.emoji_name, Some("custom_emote".into()));
        assert_eq!(evt.emoji_id, Some("12345678".into()));
    }

    #[test]
    fn parse_voice_state_update_event() {
        let data = serde_json::json!({
            "guild_id": "g1",
            "channel_id": "vc1",
            "user_id": "u1",
            "session_id": "sess1",
            "deaf": false,
            "mute": false,
            "self_deaf": true,
            "self_mute": true,
            "suppress": false
        });

        let vs = DiscordAdapter::parse_voice_state_update(&data).unwrap();
        assert_eq!(vs.guild_id, Some("g1".into()));
        assert_eq!(vs.channel_id, Some("vc1".into()));
        assert_eq!(vs.user_id, "u1");
        assert!(!vs.deaf);
        assert!(!vs.mute);
        assert!(vs.self_deaf);
        assert!(vs.self_mute);
        assert!(!vs.suppress);
    }

    #[test]
    fn parse_voice_state_leave() {
        let data = serde_json::json!({
            "guild_id": "g1",
            "channel_id": null,
            "user_id": "u1",
            "session_id": "sess2",
            "deaf": false,
            "mute": false,
            "self_deaf": false,
            "self_mute": false,
            "suppress": false
        });

        let vs = DiscordAdapter::parse_voice_state_update(&data).unwrap();
        assert!(vs.channel_id.is_none());
    }

    // -- Dispatch routing tests ---------------------------------------------

    #[test]
    fn dispatch_routes_message_create() {
        let data = serde_json::json!({
            "id": "m1",
            "channel_id": "c1",
            "content": "hi",
            "author": { "id": "u1", "username": "a", "bot": false }
        });

        let evt = DiscordAdapter::parse_dispatch("MESSAGE_CREATE", &data);
        assert!(matches!(evt, Some(DispatchEvent::MessageCreate(_))));
    }

    #[test]
    fn dispatch_routes_message_update() {
        let data = serde_json::json!({ "id": "m1", "channel_id": "c1" });
        let evt = DiscordAdapter::parse_dispatch("MESSAGE_UPDATE", &data);
        assert!(matches!(evt, Some(DispatchEvent::MessageUpdate(_))));
    }

    #[test]
    fn dispatch_routes_interaction_create() {
        let data = serde_json::json!({
            "id": "i1",
            "application_id": "a1",
            "type": 2,
            "token": "t1",
            "data": { "name": "test" }
        });
        let evt = DiscordAdapter::parse_dispatch("INTERACTION_CREATE", &data);
        assert!(matches!(evt, Some(DispatchEvent::InteractionCreate(_))));
    }

    #[test]
    fn dispatch_routes_reaction_add() {
        let data = serde_json::json!({
            "user_id": "u1",
            "channel_id": "c1",
            "message_id": "m1",
            "emoji": { "name": "x" }
        });
        let evt = DiscordAdapter::parse_dispatch("MESSAGE_REACTION_ADD", &data);
        assert!(matches!(evt, Some(DispatchEvent::ReactionAdd(_))));
    }

    #[test]
    fn dispatch_routes_reaction_remove() {
        let data = serde_json::json!({
            "user_id": "u1",
            "channel_id": "c1",
            "message_id": "m1",
            "emoji": { "name": "x" }
        });
        let evt = DiscordAdapter::parse_dispatch("MESSAGE_REACTION_REMOVE", &data);
        assert!(matches!(evt, Some(DispatchEvent::ReactionRemove(_))));
    }

    #[test]
    fn dispatch_routes_voice_state() {
        let data = serde_json::json!({
            "user_id": "u1",
            "session_id": "s1",
            "deaf": false,
            "mute": false,
            "self_deaf": false,
            "self_mute": false,
            "suppress": false
        });
        let evt = DiscordAdapter::parse_dispatch("VOICE_STATE_UPDATE", &data);
        assert!(matches!(evt, Some(DispatchEvent::VoiceStateUpdate(_))));
    }

    #[test]
    fn dispatch_unknown_event_returns_none() {
        let data = serde_json::json!({});
        let evt = DiscordAdapter::parse_dispatch("UNKNOWN_EVENT", &data);
        assert!(evt.is_none());
    }

    // -- Embed builder tests ------------------------------------------------

    #[test]
    fn embed_builder() {
        let embed = DiscordEmbed::new()
            .with_title("Test Embed")
            .with_description("A description")
            .with_color(0xFF5733)
            .with_footer("footer text")
            .with_timestamp("2026-01-01T00:00:00Z")
            .add_field("Field 1", "Value 1", true)
            .add_field("Field 2", "Value 2", false);

        assert_eq!(embed.title, Some("Test Embed".into()));
        assert_eq!(embed.description, Some("A description".into()));
        assert_eq!(embed.color, Some(0xFF5733));
        assert_eq!(embed.footer.as_ref().unwrap().text, "footer text");
        assert_eq!(embed.timestamp, Some("2026-01-01T00:00:00Z".into()));
        assert_eq!(embed.fields.len(), 2);
        assert_eq!(embed.fields[0].name, "Field 1");
        assert_eq!(embed.fields[0].inline, Some(true));
        assert_eq!(embed.fields[1].inline, Some(false));
    }

    #[test]
    fn embed_serialization() {
        let embed = DiscordEmbed::new().with_title("Hello").with_color(0x00FF00);

        let json = serde_json::to_value(&embed).unwrap();
        assert_eq!(json["title"], "Hello");
        assert_eq!(json["color"], 0x00FF00);
        assert!(json.get("description").is_none());
        assert!(json.get("footer").is_none());
    }

    // -- Slash command serialization tests ----------------------------------

    #[test]
    fn slash_command_serialization() {
        let cmd = SlashCommand {
            name: "greet".into(),
            description: "Say hello".into(),
            command_type: 1,
            options: Some(vec![
                SlashCommandOption {
                    name: "name".into(),
                    description: "Who to greet".into(),
                    option_type: 3, // STRING
                    required: Some(true),
                    choices: None,
                },
                SlashCommandOption {
                    name: "style".into(),
                    description: "Greeting style".into(),
                    option_type: 3,
                    required: Some(false),
                    choices: Some(vec![
                        SlashCommandChoice {
                            name: "Formal".into(),
                            value: serde_json::json!("formal"),
                        },
                        SlashCommandChoice {
                            name: "Casual".into(),
                            value: serde_json::json!("casual"),
                        },
                    ]),
                },
            ]),
        };

        let json = serde_json::to_value(&cmd).unwrap();
        assert_eq!(json["name"], "greet");
        assert_eq!(json["type"], 1);
        let options = json["options"].as_array().unwrap();
        assert_eq!(options.len(), 2);
        assert_eq!(options[0]["required"], true);
        let choices = options[1]["choices"].as_array().unwrap();
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0]["name"], "Formal");
    }

    // -- Emoji encoding tests -----------------------------------------------

    #[test]
    fn encode_emoji_unicode() {
        let encoded = encode_emoji("\u{1f44d}");
        assert_eq!(encoded, "%F0%9F%91%8D");
    }

    #[test]
    fn encode_emoji_custom() {
        let encoded = encode_emoji("custom_emote:12345");
        assert_eq!(encoded, "custom_emote:12345");
    }

    // -- Default trait impls ------------------------------------------------

    #[test]
    fn gateway_session_default() {
        let session = GatewaySession::default();
        assert!(session.sequence.is_none());
        assert!(session.session_id.is_none());
        assert!(!session.identified);
        assert!(session.heartbeat_acknowledged);
    }

    #[test]
    fn embed_default() {
        let embed = DiscordEmbed::default();
        assert!(embed.title.is_none());
        assert!(embed.fields.is_empty());
    }

    #[test]
    fn default_intents_include_direct_messages() {
        let intents = default_intents();
        // GUILDS (1<<0)
        assert_ne!(intents & (1 << 0), 0);
        // GUILD_MESSAGES (1<<9)
        assert_ne!(intents & (1 << 9), 0);
        // DIRECT_MESSAGES (1<<12) — required to receive DM events; not privileged.
        assert_ne!(intents & (1 << 12), 0);
        // MESSAGE_CONTENT (1<<15, privileged)
        assert_ne!(intents & (1 << 15), 0);
        // serde default on DiscordConfig picks the same mask.
        let config: DiscordConfig =
            serde_json::from_value(serde_json::json!({"token": "x"})).unwrap();
        assert_eq!(config.intents, intents);
    }

    // -- Reconnect backoff ---------------------------------------------------

    #[test]
    fn reconnect_base_delay_progression() {
        // 1s, 2s, 4s, … capped at 60s.
        let expected_secs = [1u64, 2, 4, 8, 16, 32, 60, 60, 60];
        for (attempt, &expected) in expected_secs.iter().enumerate() {
            assert_eq!(
                reconnect_base_delay(attempt as u32),
                Duration::from_secs(expected),
                "attempt {}",
                attempt
            );
        }
    }

    #[test]
    fn reconnect_delay_within_jitter_bounds() {
        // Jitter is ±25% of the base delay.
        for attempt in 0..8u32 {
            let base_ms = reconnect_base_delay(attempt).as_millis() as f64;
            let delay_ms = reconnect_delay(attempt).as_millis() as f64;
            assert!(
                delay_ms >= base_ms * 0.75 - 1.0,
                "attempt {}: {:?} below lower jitter bound",
                attempt,
                reconnect_delay(attempt)
            );
            assert!(
                delay_ms <= base_ms * 1.25 + 1.0,
                "attempt {}: {:?} above upper jitter bound",
                attempt,
                reconnect_delay(attempt)
            );
        }
    }

    // -- Inbound message dedup -----------------------------------------------

    #[test]
    fn message_dedup_insert_and_duplicate() {
        let mut dedup = MessageDedup::new();
        assert!(dedup.is_empty());
        assert!(dedup.insert("msg1".to_string()));
        assert_eq!(dedup.len(), 1);
        // Second insert of the same ID reports a duplicate.
        assert!(!dedup.insert("msg1".to_string()));
        assert_eq!(dedup.len(), 1);
        assert!(dedup.insert("msg2".to_string()));
        assert_eq!(dedup.len(), 2);
    }

    #[test]
    fn message_dedup_evicts_oldest_at_cap() {
        let mut dedup = MessageDedup::new();
        for i in 0..(MAX_SEEN_MESSAGE_IDS + 10) {
            assert!(dedup.insert(format!("msg{}", i)));
        }
        assert_eq!(dedup.len(), MAX_SEEN_MESSAGE_IDS);
        // The oldest IDs were evicted, so they insert as "new" again…
        assert!(dedup.insert("msg0".to_string()));
        // …while the newest ID is still tracked as a duplicate.
        assert!(!dedup.insert(format!("msg{}", MAX_SEEN_MESSAGE_IDS + 9)));
        assert_eq!(dedup.len(), MAX_SEEN_MESSAGE_IDS);
    }

    #[test]
    fn message_dedup_trim_enforces_cap() {
        let mut dedup = MessageDedup::new();
        for i in 0..(MAX_SEEN_MESSAGE_IDS + 5) {
            // bypass the in-insert trim by pushing directly, then trim once
            dedup.set.insert(format!("msg{}", i));
            dedup.order.push_back(format!("msg{}", i));
        }
        dedup.trim();
        assert_eq!(dedup.len(), MAX_SEEN_MESSAGE_IDS);
    }

    fn test_adapter() -> DiscordAdapter {
        let config = DiscordConfig {
            token: "test-token".into(),
            application_id: None,
            proxy: AdapterProxyConfig::default(),
            require_mention: false,
            intents: default_intents(),
        };
        DiscordAdapter::new(config).unwrap()
    }

    fn message_create_data(id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "channel_id": "ch1",
            "content": "hello",
            "author": { "id": "u1", "username": "user" },
        })
    }

    #[tokio::test]
    async fn dispatch_event_dedups_message_create() {
        let adapter = test_adapter();
        let data = message_create_data("dup-msg");

        let first = adapter.dispatch_event("MESSAGE_CREATE", &data).await;
        assert!(matches!(first, Some(DispatchEvent::MessageCreate(_))));

        // Same message ID arriving again (e.g. replayed after reconnect) is skipped.
        let second = adapter.dispatch_event("MESSAGE_CREATE", &data).await;
        assert!(second.is_none());

        // A different message ID still passes through.
        let other = adapter
            .dispatch_event("MESSAGE_CREATE", &message_create_data("other-msg"))
            .await;
        assert!(matches!(other, Some(DispatchEvent::MessageCreate(_))));
    }

    #[tokio::test]
    async fn dispatch_event_dedups_message_update() {
        let adapter = test_adapter();
        let data = serde_json::json!({
            "id": "upd-msg",
            "channel_id": "ch1",
            "content": "edited",
        });

        assert!(adapter
            .dispatch_event("MESSAGE_UPDATE", &data)
            .await
            .is_some());
        assert!(adapter
            .dispatch_event("MESSAGE_UPDATE", &data)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn dispatch_event_non_message_events_pass_through() {
        let adapter = test_adapter();
        let data = serde_json::json!({
            "id": "react1",
            "channel_id": "ch1",
            "message_id": "msg1",
            "user_id": "u1",
            "emoji": { "name": "👍" },
        });
        // Reaction events carry no deduped message ID of their own: they are
        // never dropped by the dedup set.
        assert!(adapter
            .dispatch_event("MESSAGE_REACTION_ADD", &data)
            .await
            .is_some());
        assert!(adapter
            .dispatch_event("MESSAGE_REACTION_ADD", &data)
            .await
            .is_some());
    }

    // -- allowed_mentions bodies ------------------------------------------------

    #[test]
    fn text_message_body_has_users_only_allowed_mentions() {
        let body = DiscordAdapter::text_message_body("hello");
        assert_eq!(body["content"], serde_json::json!("hello"));
        assert_eq!(
            body["allowed_mentions"],
            serde_json::json!({ "parse": ["users"] })
        );
    }

    #[test]
    fn embed_message_body_has_users_only_allowed_mentions() {
        let embed = DiscordEmbed::default().with_title("t");
        let with_content =
            DiscordAdapter::embed_message_body(Some("hi"), std::slice::from_ref(&embed));
        assert_eq!(with_content["content"], serde_json::json!("hi"));
        assert_eq!(
            with_content["allowed_mentions"],
            serde_json::json!({ "parse": ["users"] })
        );
        assert!(with_content["embeds"].is_array());

        let without_content = DiscordAdapter::embed_message_body(None, &[embed]);
        assert!(without_content.get("content").is_none());
        assert_eq!(
            without_content["allowed_mentions"],
            serde_json::json!({ "parse": ["users"] })
        );
    }
}
