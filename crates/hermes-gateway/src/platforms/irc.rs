//! IRC platform adapter — plain-text TCP IRC (RFC 1459 / 2812), no external IRC crate.
//!
//! Connects to an IRC server over `tokio::net::TcpStream`, registers with
//! `NICK`/`USER`, joins configured channels on `RPL_WELCOME` (001), answers
//! `PING` with `PONG` (required keepalive — servers disconnect idle clients),
//! and relays addressed `PRIVMSG`s to the gateway.
//!
//! Addressing rules (mirrors the original Python adapter):
//! * Channel messages are forwarded only when addressed to the bot:
//!   `nick:`, `nick,` or `nick ` prefix (case-insensitive).
//! * Direct messages to the bot's nick are always forwarded.
//! * The bot's own messages are ignored.
//!
//! TLS note: the `use_tls` config key is accepted for forward compatibility
//! with the original adapter, but **TLS is not implemented in v1** — there is
//! no `tokio-rustls` in the dependency tree. `IrcAdapter::new` fails closed
//! with an error when `use_tls` is true rather than silently downgrading to
//! plaintext.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use hermes_core::errors::GatewayError;
use hermes_core::traits::{ParseMode, PlatformAdapter};

use crate::adapter::BasePlatformAdapter;
use crate::gateway::IncomingMessage;

// ---------------------------------------------------------------------------
// Protocol constants
// ---------------------------------------------------------------------------

/// Hard IRC line limit (RFC 2812 §2.3): 512 bytes including CRLF.
const MAX_LINE_BYTES: usize = 512;
/// CRLF terminator bytes.
const CRLF_BYTES: usize = 2;
/// Backoff ladder for reconnect attempts (seconds).
const RECONNECT_BACKOFF_SECS: &[u64] = &[1, 2, 5, 10, 30, 60];
/// TCP connect timeout.
const CONNECT_TIMEOUT_SECS: u64 = 30;
/// Registration (`RPL_WELCOME`) timeout after sending NICK/USER.
const REGISTRATION_TIMEOUT_SECS: u64 = 30;
/// Read watchdog: if the server sends nothing (not even PING) for this long,
/// the connection is assumed dead and a reconnect is triggered.
const READ_TIMEOUT_SECS: u64 = 300;
/// Delay between consecutive PRIVMSGs (basic flood protection).
const FLOOD_DELAY_MS: u64 = 300;
/// Channel for lines queued for the socket writer.
const WRITER_CHANNEL_CAPACITY: usize = 128;

// ---------------------------------------------------------------------------
// Pure IRC protocol helpers (unit-testable, no I/O)
// ---------------------------------------------------------------------------

/// A parsed IRC protocol line: `[":" prefix SPACE] command [params]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrcLine {
    /// Optional prefix (usually `nick!user@host` or the server name).
    pub prefix: Option<String>,
    /// Command or numeric reply (e.g. `PRIVMSG`, `PING`, `001`).
    pub command: String,
    /// Parameters; the trailing `:text` parameter is appended as one entry.
    pub params: Vec<String>,
}

/// Parse a raw IRC line (without CRLF) into prefix / command / params.
///
/// Handles the trailing-parameter rule: the first parameter starting with
/// `:` consumes the rest of the line.
pub fn parse_irc_line(raw: &str) -> IrcLine {
    let mut rest = raw;
    let mut prefix = None;
    if let Some(after) = rest.strip_prefix(':') {
        let (p, r) = match after.find(' ') {
            Some(i) => (&after[..i], &after[i + 1..]),
            None => (after, ""),
        };
        prefix = Some(p.to_string());
        rest = r;
    }
    let (middle, trailing) = match rest.find(" :") {
        Some(i) => (&rest[..i], Some(&rest[i + 2..])),
        None => (rest, None),
    };
    let mut parts: Vec<&str> = middle.split_whitespace().collect();
    let command = parts.first().map(|s| s.to_string()).unwrap_or_default();
    let mut params: Vec<String> = parts.drain(1..).map(str::to_string).collect();
    if let Some(t) = trailing {
        params.push(t.to_string());
    }
    IrcLine {
        prefix,
        command,
        params,
    }
}

/// Extract the nickname from an IRC prefix (`nick!user@host` → `nick`).
pub fn nick_from_prefix(prefix: &str) -> &str {
    prefix.split('!').next().unwrap_or(prefix)
}

/// Whether an IRC target is a channel (`#`, `&`, `+`, `!` prefixes).
pub fn is_channel_target(target: &str) -> bool {
    matches!(target.chars().next(), Some('#' | '&' | '+' | '!'))
}

/// Strip the bot's address prefix from a channel message.
///
/// Accepts `nick:`, `nick,` or `nick ` (case-insensitive). Returns the
/// remaining message body, or `None` when the message is not addressed
/// to the bot.
pub fn strip_address_prefix<'a>(text: &'a str, nick: &str) -> Option<&'a str> {
    for sep in [":", ",", " "] {
        let prefix = format!("{nick}{sep}");
        if text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(&prefix) {
            return Some(text[prefix.len()..].trim_start());
        }
    }
    None
}

/// Whether the text is a CTCP message (wrapped in `\x01...\x01`).
pub fn is_ctcp(text: &str) -> bool {
    text.starts_with('\x01') && text.ends_with('\x01') && text.len() >= 2
}

/// Extract the body of a CTCP ACTION (`\x01ACTION waves\x01` → `waves`).
///
/// Returns `None` for non-ACTION CTCP messages.
pub fn parse_ctcp_action(text: &str) -> Option<&str> {
    if !is_ctcp(text) {
        return None;
    }
    let inner = &text[1..text.len() - 1];
    inner.strip_prefix("ACTION ")
}

/// Neutralise CR/LF/NUL in outbound text (IRC command-injection vector).
pub fn strip_irc_control_chars(text: &str) -> String {
    text.replace(['\r', '\n'], " ").replace('\x00', "")
}

/// Strip `_italic_` markers where the underscore is not adjacent to word
/// characters on its outer side (the `regex` crate has no look-around, so
/// this is a small manual scan instead).
fn strip_underscore_italics(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut candidates = Vec::new();
    for (i, &c) in chars.iter().enumerate() {
        if c != '_' {
            continue;
        }
        let outer_clear = i == 0
            || i + 1 >= chars.len()
            || !chars[i - 1].is_alphanumeric()
            || !chars[i + 1].is_alphanumeric();
        if outer_clear {
            candidates.push(i);
        }
    }
    let mut remove = vec![false; chars.len()];
    let mut i = 0;
    while i < candidates.len() {
        if i + 1 < candidates.len() && candidates[i + 1] > candidates[i] + 1 {
            remove[candidates[i]] = true;
            remove[candidates[i + 1]] = true;
            i += 2;
        } else {
            i += 1;
        }
    }
    chars
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !remove[*i])
        .map(|(_, c)| c)
        .collect()
}

/// Convert basic Markdown to plain text for IRC (no formatting support).
pub fn strip_markdown(text: &str) -> String {
    // Compiled once per call site via `Regex::new` caching is not available;
    // these are cheap enough for message-sized inputs, but keep a single
    // static set to avoid recompilation. Note: the `regex` crate does not
    // support look-around, so `_italic_` is handled by
    // `strip_underscore_italics` above instead of a regex rule.
    static RULES: std::sync::OnceLock<Vec<(Regex, &'static str)>> = std::sync::OnceLock::new();
    let rules = RULES.get_or_init(|| {
        [
            (r"\*\*(.+?)\*\*", "$1"),                // bold
            (r"__(.+?)__", "$1"),                    // bold
            (r"\*(.+?)\*", "$1"),                    // italic
            (r"```\w*\n?", ""),                      // code fences (before inline code!)
            (r"`(.+?)`", "$1"),                      // inline code
            (r"!\[([^\]]*)\]\(([^)]+)\)", "$2"),     // images → url
            (r"\[([^\]]+)\]\(([^)]+)\)", "$1 ($2)"), // links → text (url)
        ]
        .into_iter()
        .map(|(pat, repl)| (Regex::new(pat).expect("valid regex"), repl))
        .collect()
    });
    let mut out = text.to_string();
    for (re, repl) in rules {
        out = re.replace_all(&out, *repl).into_owned();
    }
    // Underscore italics run last so `__bold__` is consumed by the bold
    // regex above first.
    strip_underscore_italics(&out)
}

/// Payload bytes left in a 512-byte line after `PRIVMSG <target> :` and CRLF.
pub fn privmsg_line_budget(target: &str) -> usize {
    MAX_LINE_BYTES.saturating_sub("PRIVMSG ".len() + target.len() + " :".len() + CRLF_BYTES)
}

/// Split text into IRC-safe `PRIVMSG` payload chunks.
///
/// Chunks respect UTF-8 char boundaries and byte budgets, preferring to
/// break at spaces. `max_chars` caps chunk length in characters (the
/// original adapter's `max_message_length`); the per-line byte budget is
/// always enforced.
pub fn split_privmsg_text(text: &str, target: &str, max_chars: usize) -> Vec<String> {
    let byte_budget = privmsg_line_budget(target).max(1);
    let mut chunks = Vec::new();
    for paragraph in text.split('\n') {
        let paragraph = paragraph.trim();
        if paragraph.is_empty() {
            continue;
        }
        let mut current = String::new();
        let mut current_bytes = 0usize;
        let flush = |current: &mut String, current_bytes: &mut usize, chunks: &mut Vec<String>| {
            if !current.is_empty() {
                chunks.push(std::mem::take(current));
                *current_bytes = 0;
            }
        };
        for word in paragraph.split_whitespace() {
            let word_bytes = word.len();
            // A single word longer than the budget is hard-split at char boundaries.
            if word_bytes > byte_budget {
                flush(&mut current, &mut current_bytes, &mut chunks);
                let mut piece = String::new();
                let mut piece_bytes = 0usize;
                for ch in word.chars() {
                    if piece_bytes + ch.len_utf8() > byte_budget {
                        chunks.push(std::mem::take(&mut piece));
                        piece_bytes = 0;
                    }
                    piece_bytes += ch.len_utf8();
                    piece.push(ch);
                }
                if !piece.is_empty() {
                    chunks.push(piece);
                }
                continue;
            }
            let extra = if current.is_empty() { 0 } else { 1 } + word_bytes;
            let over_chars = max_chars > 0
                && current.chars().count() + extra_chars(word, current.is_empty()) > max_chars;
            if current_bytes + extra > byte_budget || over_chars {
                flush(&mut current, &mut current_bytes, &mut chunks);
            }
            if !current.is_empty() {
                current.push(' ');
                current_bytes += 1;
            }
            current.push_str(word);
            current_bytes += word_bytes;
        }
        flush(&mut current, &mut current_bytes, &mut chunks);
    }
    chunks
}

/// Character count a word (plus a leading space when `needs_space`) adds.
fn extra_chars(word: &str, needs_space: bool) -> usize {
    word.chars().count() + usize::from(needs_space)
}

/// Compute the next nick after a `433 ERR_NICKNAMEINUSE` collision.
///
/// Strips trailing `_`/digits from the *current* nick, then appends
/// `_N` with the attempt counter. Result is truncated to 16 chars
/// (common network NICKLEN).
pub fn next_nick_after_collision(current_nick: &str, attempt: u32) -> String {
    let suffix = format!("_{attempt}");
    let stem_max = 16usize.saturating_sub(suffix.len());
    let stem: String = current_nick
        .trim_end_matches(|c: char| c == '_' || c.is_ascii_digit())
        .chars()
        .take(stem_max)
        .collect();
    format!("{stem}{suffix}")
}

// ---------------------------------------------------------------------------
// IrcConfig
// ---------------------------------------------------------------------------

fn default_irc_port() -> u16 {
    6667
}
fn default_irc_nick() -> String {
    "hermes-bot".to_string()
}
fn default_irc_username() -> String {
    "hermes".to_string()
}
fn default_irc_realname() -> String {
    "Hermes Agent".to_string()
}
fn default_max_message_length() -> usize {
    450
}

/// Configuration for the IRC adapter.
///
/// Field names mirror the original Python adapter's `extra` keys
/// (`server`, `port`, `nickname`, `channels`, `server_password`,
/// `nickserv_password`, `allowed_users`, `max_message_length`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrcConfig {
    /// IRC server hostname (e.g. `irc.libera.chat`).
    #[serde(default)]
    pub server: String,
    /// Server port (default 6667, plaintext).
    #[serde(default = "default_irc_port")]
    pub port: u16,
    /// Bot nickname.
    #[serde(default = "default_irc_nick")]
    pub nick: String,
    /// USER command username.
    #[serde(default = "default_irc_username")]
    pub username: String,
    /// USER command realname (GECOS).
    #[serde(default = "default_irc_realname")]
    pub realname: String,
    /// Channels to join on connect (e.g. `["#hermes"]`).
    #[serde(default)]
    pub channels: Vec<String>,
    /// Optional server password (`PASS` command).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Accepted for forward compatibility; **TLS is not implemented in v1**.
    /// `IrcAdapter::new` fails closed when this is true.
    #[serde(default)]
    pub use_tls: bool,
    /// Optional NickServ password (IDENTIFY after registration).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickserv_password: Option<String>,
    /// IRC nicks allowed to talk to the bot (case-insensitive).
    /// Empty = allow everyone (matches the original adapter).
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// Per-line character cap before splitting (default 450).
    #[serde(default = "default_max_message_length")]
    pub max_message_length: usize,
}

impl IrcConfig {
    /// Build from a generic [`hermes_config::PlatformConfig`].
    ///
    /// Reads the canonical `allowed_users` field (union of the top-level
    /// list and `extra["allowed_users"]`) plus adapter-specific `extra`
    /// keys matching the original adapter's names.
    pub fn from_platform_config(p: &hermes_config::PlatformConfig) -> Self {
        let channels = hermes_config::platform::extra_string_list(p, "channels")
            .or_else(|| hermes_config::platform::extra_string_list(p, "channel"))
            .unwrap_or_default();
        let nick = hermes_config::extra_string(p, "nickname")
            .or_else(|| hermes_config::extra_string(p, "nick"))
            .unwrap_or_else(default_irc_nick);
        let password = hermes_config::extra_string(p, "server_password")
            .or_else(|| hermes_config::extra_string(p, "password"));
        Self {
            server: hermes_config::extra_string(p, "server").unwrap_or_default(),
            port: p
                .extra
                .get("port")
                .and_then(|v| v.as_u64())
                .map(|n| n as u16)
                .unwrap_or_else(default_irc_port),
            nick: nick.clone(),
            username: hermes_config::extra_string(p, "username")
                .unwrap_or_else(default_irc_username),
            realname: hermes_config::extra_string(p, "realname")
                .unwrap_or_else(default_irc_realname),
            channels,
            password,
            use_tls: p
                .extra
                .get("use_tls")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            nickserv_password: hermes_config::extra_string(p, "nickserv_password"),
            allowed_users: p.allowed_users_union(),
            max_message_length: p
                .extra
                .get("max_message_length")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or_else(default_max_message_length),
        }
    }
}

// ---------------------------------------------------------------------------
// IrcAdapter
// ---------------------------------------------------------------------------

struct IrcInner {
    base: BasePlatformAdapter,
    config: IrcConfig,
    /// Inbound messages are forwarded here (set by the gateway at registration).
    inbound_tx: Mutex<Option<mpsc::Sender<IncomingMessage>>>,
    /// Raw protocol lines queued for the socket writer (set while connected).
    write_tx: Mutex<Option<mpsc::Sender<String>>>,
    stop: Notify,
    run_task: Mutex<Option<JoinHandle<()>>>,
}

/// IRC platform adapter.
///
/// `start()` spawns the connection loop (connect → register → join →
/// read) with exponential-backoff reconnects; `stop()` shuts it down.
/// Inbound messages are pushed to the sender registered via
/// [`IrcAdapter::set_inbound_sender`].
pub struct IrcAdapter {
    inner: Arc<IrcInner>,
}

impl IrcAdapter {
    pub fn new(config: IrcConfig) -> Result<Self, GatewayError> {
        if config.server.trim().is_empty() {
            return Err(GatewayError::ConnectionFailed(
                "IRC server must be configured".to_string(),
            ));
        }
        if config.channels.is_empty() {
            return Err(GatewayError::ConnectionFailed(
                "IRC channels must be configured (at least one)".to_string(),
            ));
        }
        if config.nick.trim().is_empty() {
            return Err(GatewayError::ConnectionFailed(
                "IRC nick must be configured".to_string(),
            ));
        }
        if config.use_tls {
            return Err(GatewayError::ConnectionFailed(
                "IRC TLS is not supported in this build (v1); set use_tls=false or port 6667"
                    .to_string(),
            ));
        }
        Ok(Self {
            inner: Arc::new(IrcInner {
                base: BasePlatformAdapter::new(String::new()),
                config,
                inbound_tx: Mutex::new(None),
                write_tx: Mutex::new(None),
                stop: Notify::new(),
                run_task: Mutex::new(None),
            }),
        })
    }

    pub fn config(&self) -> &IrcConfig {
        &self.inner.config
    }

    /// Register the channel the gateway reads inbound messages from.
    pub async fn set_inbound_sender(&self, tx: mpsc::Sender<IncomingMessage>) {
        *self.inner.inbound_tx.lock().await = Some(tx);
    }

    /// Queue one raw protocol line for the socket writer.
    async fn send_raw(&self, line: &str) {
        if let Some(tx) = self.inner.write_tx.lock().await.clone() {
            if tx.send(line.to_string()).await.is_err() {
                debug!("IRC write channel closed; dropping line");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

/// Per-connection session state shared between the read loop and the
/// registration handshake.
struct IrcSession {
    inner: Arc<IrcInner>,
    write_tx: mpsc::Sender<String>,
    registered: Arc<Notify>,
    current_nick: Mutex<String>,
    nick_attempt: Mutex<u32>,
}

impl IrcSession {
    async fn send_raw(&self, line: String) {
        if self.write_tx.send(line).await.is_err() {
            debug!("IRC write channel closed; dropping line");
        }
    }

    async fn handle_line(&self, raw: &str) {
        let msg = parse_irc_line(raw);
        match msg.command.as_str() {
            "PING" => {
                let token = msg.params.first().cloned().unwrap_or_default();
                self.send_raw(format!("PONG :{token}")).await;
            }
            "001" => {
                // RPL_WELCOME — registration complete; params[0] is our nick.
                if let Some(nick) = msg.params.first() {
                    *self.current_nick.lock().await = nick.clone();
                }
                self.registered.notify_one();
            }
            "432" | "433" => {
                // Erroneous / in-use nickname — retry with a suffix.
                let mut attempt = self.nick_attempt.lock().await;
                *attempt += 1;
                let current = self.current_nick.lock().await.clone();
                let next = next_nick_after_collision(&current, *attempt);
                info!(old = %current, new = %next, "IRC nick in use, retrying");
                *self.current_nick.lock().await = next.clone();
                drop(attempt);
                self.send_raw(format!("NICK {next}")).await;
            }
            "NICK" => {
                // Track our own nick changes (e.g. forced by services).
                if let Some(prefix) = &msg.prefix {
                    let old = nick_from_prefix(prefix);
                    let current = self.current_nick.lock().await.clone();
                    if old.eq_ignore_ascii_case(&current) {
                        if let Some(new_nick) = msg.params.first() {
                            *self.current_nick.lock().await = new_nick.clone();
                        }
                    }
                }
            }
            "PRIVMSG" => {
                if msg.params.len() >= 2 {
                    let prefix = msg.prefix.as_deref().unwrap_or("");
                    self.handle_privmsg(prefix, &msg.params[0], &msg.params[1])
                        .await;
                }
            }
            "ERROR" => {
                warn!("IRC ERROR from server: {}", msg.params.join(" "));
            }
            _ => {
                debug!(command = %msg.command, "IRC: unhandled line");
            }
        }
    }

    async fn handle_privmsg(&self, prefix: &str, target: &str, text: &str) {
        let sender = nick_from_prefix(prefix).to_string();
        if sender.is_empty() {
            return;
        }
        let current_nick = self.current_nick.lock().await.clone();
        if sender.eq_ignore_ascii_case(&current_nick) {
            return; // our own message
        }

        // CTCP ACTION (/me) becomes "* nick text"; other CTCP is ignored.
        let owned;
        let text: &str = if is_ctcp(text) {
            match parse_ctcp_action(text) {
                Some(action) => {
                    owned = format!("* {sender} {action}");
                    &owned
                }
                None => return,
            }
        } else {
            text
        };

        let channel = is_channel_target(target);
        let (body, chat_id, is_dm) = if channel {
            match strip_address_prefix(text, &current_nick) {
                Some(body) => (body.to_string(), target.to_string(), false),
                None => return, // not addressed to the bot
            }
        } else {
            if !target.eq_ignore_ascii_case(&current_nick) {
                return; // PRIVMSG to someone else (shouldn't happen)
            }
            (text.to_string(), sender.clone(), true)
        };
        if body.trim().is_empty() {
            return;
        }

        // Allowlist (case-insensitive nicks); empty = allow everyone.
        let allowed: Vec<String> = self
            .inner
            .config
            .allowed_users
            .iter()
            .map(|u| u.to_lowercase())
            .collect();
        if !allowed.is_empty() && !allowed.iter().any(|u| u == &sender.to_lowercase()) {
            debug!(sender = %sender, "IRC: ignoring message from unauthorized nick");
            return;
        }

        let msg = IncomingMessage {
            platform: "irc".to_string(),
            chat_id,
            user_id: sender.clone(),
            text: body,
            message_id: None,
            is_dm,
        };
        if let Some(tx) = self.inner.inbound_tx.lock().await.clone() {
            if tx.send(msg).await.is_err() {
                warn!("IRC inbound receiver dropped; message from {sender} lost");
            }
        } else {
            debug!("IRC: no inbound sender registered; dropping message");
        }
    }

    /// Read loop: returns when the connection closes, errors, the read
    /// watchdog fires, or the adapter is stopped.
    async fn read_loop(self: Arc<Self>, read_half: tokio::net::tcp::OwnedReadHalf) {
        let mut reader = BufReader::new(read_half);
        loop {
            let mut buf = Vec::new();
            let n = tokio::select! {
                res = tokio::time::timeout(
                    Duration::from_secs(READ_TIMEOUT_SECS),
                    reader.read_until(b'\n', &mut buf),
                ) => match res {
                    Ok(Ok(n)) => n,
                    Ok(Err(e)) => {
                        debug!(error = %e, "IRC read error");
                        break;
                    }
                    Err(_) => {
                        warn!("IRC read timeout — no data from server, reconnecting");
                        break;
                    }
                },
                _ = self.inner.stop.notified() => {
                    debug!("IRC read loop: stop signalled");
                    break;
                }
            };
            if n == 0 {
                debug!("IRC server closed the connection");
                break;
            }
            if buf.len() > 8 * 1024 {
                warn!("IRC line exceeded 8KiB; truncating");
                buf.truncate(8 * 1024);
            }
            let line = String::from_utf8_lossy(&buf);
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                continue;
            }
            self.handle_line(line).await;
        }
    }
}

/// Connect once, register, join channels, and serve until disconnect.
async fn connect_and_serve(inner: Arc<IrcInner>) -> Result<(), GatewayError> {
    let cfg = &inner.config;
    info!(server = %cfg.server, port = cfg.port, nick = %cfg.nick, "IRC connecting");

    let stream = tokio::time::timeout(
        Duration::from_secs(CONNECT_TIMEOUT_SECS),
        TcpStream::connect((cfg.server.as_str(), cfg.port)),
    )
    .await
    .map_err(|_| {
        GatewayError::ConnectionFailed(format!(
            "IRC connect to {}:{} timed out",
            cfg.server, cfg.port
        ))
    })?
    .map_err(|e| {
        GatewayError::ConnectionFailed(format!(
            "IRC connect to {}:{} failed: {e}",
            cfg.server, cfg.port
        ))
    })?;

    let (read_half, write_half) = stream.into_split();
    let (line_tx, mut line_rx) = mpsc::channel::<String>(WRITER_CHANNEL_CAPACITY);
    *inner.write_tx.lock().await = Some(line_tx.clone());

    // Dedicated writer task: serialises all outbound protocol lines.
    let writer_task = tokio::spawn(async move {
        let mut w = write_half;
        while let Some(line) = line_rx.recv().await {
            let bytes = format!("{line}\r\n");
            if w.write_all(bytes.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let session = Arc::new(IrcSession {
        inner: inner.clone(),
        write_tx: line_tx,
        registered: Arc::new(Notify::new()),
        current_nick: Mutex::new(cfg.nick.clone()),
        nick_attempt: Mutex::new(0),
    });

    // Registration handshake.
    if let Some(pw) = &cfg.password {
        session
            .send_raw(format!("PASS {}", strip_irc_control_chars(pw)))
            .await;
    }
    session.send_raw(format!("NICK {}", cfg.nick)).await;
    session
        .send_raw(format!("USER {} 0 * :{}", cfg.username, cfg.realname))
        .await;

    // The read loop must already run to observe 001 / 433.
    let read_task = tokio::spawn({
        let session = session.clone();
        async move { session.read_loop(read_half).await }
    });

    let registered = tokio::time::timeout(
        Duration::from_secs(REGISTRATION_TIMEOUT_SECS),
        session.registered.notified(),
    )
    .await;
    if registered.is_err() {
        warn!("IRC registration timed out waiting for RPL_WELCOME");
        read_task.abort();
        writer_task.abort();
        *inner.write_tx.lock().await = None;
        return Err(GatewayError::ConnectionFailed(
            "IRC registration timed out (no RPL_WELCOME)".to_string(),
        ));
    }
    let current_nick = session.current_nick.lock().await.clone();
    info!(nick = %current_nick, "IRC registered");

    if let Some(ns_pw) = &cfg.nickserv_password {
        session
            .send_raw(format!(
                "PRIVMSG NickServ :IDENTIFY {}",
                strip_irc_control_chars(ns_pw)
            ))
            .await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    for channel in &cfg.channels {
        session.send_raw(format!("JOIN {channel}")).await;
        info!(channel = %channel, "IRC joined channel");
    }

    // Serve until the read loop ends (disconnect / stop).
    let _ = read_task.await;
    writer_task.abort();
    *inner.write_tx.lock().await = None;

    if !inner.base.is_running() {
        debug!("IRC connection closed after stop");
        Ok(())
    } else {
        Err(GatewayError::ConnectionFailed(
            "IRC connection closed unexpectedly".to_string(),
        ))
    }
}

/// Connection loop with exponential-backoff reconnects.
async fn run_loop(inner: Arc<IrcInner>) {
    let mut attempt: u32 = 0;
    loop {
        if !inner.base.is_running() {
            break;
        }
        match connect_and_serve(inner.clone()).await {
            Ok(()) => break,
            Err(e) => {
                if !inner.base.is_running() {
                    break;
                }
                let delay = RECONNECT_BACKOFF_SECS
                    [(attempt as usize).min(RECONNECT_BACKOFF_SECS.len() - 1)];
                warn!(error = %e, retry_in_secs = delay, "IRC disconnected, reconnecting");
                attempt = attempt.saturating_add(1);
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
                    _ = inner.stop.notified() => break,
                }
            }
        }
    }
    info!("IRC run loop exited");
}

#[async_trait]
impl PlatformAdapter for IrcAdapter {
    async fn start(&self) -> Result<(), GatewayError> {
        info!(
            server = %self.inner.config.server,
            channels = ?self.inner.config.channels,
            "IRC adapter starting"
        );
        self.inner.base.mark_running();
        let inner = self.inner.clone();
        let task = tokio::spawn(async move { run_loop(inner).await });
        *self.inner.run_task.lock().await = Some(task);
        Ok(())
    }

    async fn stop(&self) -> Result<(), GatewayError> {
        info!("IRC adapter stopping");
        self.inner.base.mark_stopped();
        self.inner.stop.notify_one();
        if let Some(task) = self.inner.run_task.lock().await.take() {
            task.abort();
        }
        *self.inner.write_tx.lock().await = None;
        Ok(())
    }

    async fn send_message(
        &self,
        chat_id: &str,
        text: &str,
        _parse_mode: Option<ParseMode>,
    ) -> Result<(), GatewayError> {
        let target = chat_id.trim();
        if target.is_empty()
            || target
                .chars()
                .any(|c| matches!(c, '\r' | '\n' | '\x00' | ' '))
        {
            return Err(GatewayError::SendFailed(
                "IRC: chat_id contains illegal characters".to_string(),
            ));
        }
        let tx = self.inner.write_tx.lock().await.clone();
        let Some(tx) = tx else {
            return Err(GatewayError::SendFailed(
                "IRC adapter is not connected".to_string(),
            ));
        };
        let plain = strip_irc_control_chars(&strip_markdown(text));
        let chunks = split_privmsg_text(&plain, target, self.inner.config.max_message_length);
        if chunks.is_empty() {
            return Err(GatewayError::SendFailed(
                "IRC: empty message after stripping".to_string(),
            ));
        }
        for chunk in chunks {
            tx.send(format!("PRIVMSG {target} :{chunk}"))
                .await
                .map_err(|_| GatewayError::SendFailed("IRC write channel closed".to_string()))?;
            // Basic flood protection between consecutive lines.
            tokio::time::sleep(Duration::from_millis(FLOOD_DELAY_MS)).await;
        }
        Ok(())
    }

    async fn edit_message(
        &self,
        _chat_id: &str,
        _message_id: &str,
        _text: &str,
    ) -> Result<(), GatewayError> {
        debug!("IRC does not support message editing");
        Ok(())
    }

    async fn send_file(
        &self,
        _chat_id: &str,
        _file_path: &str,
        _caption: Option<&str>,
    ) -> Result<(), GatewayError> {
        debug!("IRC adapter does not support file transfer");
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.inner.base.is_running()
    }

    fn platform_name(&self) -> &str {
        "irc"
    }
}

// ---------------------------------------------------------------------------
// Tests (no live network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> IrcConfig {
        IrcConfig {
            server: "irc.example.com".to_string(),
            port: 6667,
            nick: "hermes-bot".to_string(),
            username: "hermes".to_string(),
            realname: "Hermes Agent".to_string(),
            channels: vec!["#hermes".to_string()],
            password: None,
            use_tls: false,
            nickserv_password: None,
            allowed_users: vec![],
            max_message_length: 450,
        }
    }

    #[test]
    fn parse_irc_line_full_privmsg() {
        let line = parse_irc_line(":nick!user@host PRIVMSG #chan :hello world");
        assert_eq!(line.prefix.as_deref(), Some("nick!user@host"));
        assert_eq!(line.command, "PRIVMSG");
        assert_eq!(line.params, vec!["#chan", "hello world"]);
    }

    #[test]
    fn parse_irc_line_no_prefix() {
        let line = parse_irc_line("PING :irc.example.com");
        assert_eq!(line.prefix, None);
        assert_eq!(line.command, "PING");
        assert_eq!(line.params, vec!["irc.example.com"]);
    }

    #[test]
    fn parse_irc_line_middle_params_only() {
        let line = parse_irc_line(":srv 433 * hermes-bot :Nickname is already in use");
        assert_eq!(line.command, "433");
        assert_eq!(
            line.params,
            vec!["*", "hermes-bot", "Nickname is already in use"]
        );
    }

    #[test]
    fn parse_irc_line_trailing_with_colons() {
        let line = parse_irc_line(":n!u@h PRIVMSG #c :a:b :c");
        assert_eq!(line.params, vec!["#c", "a:b :c"]);
    }

    #[test]
    fn nick_from_prefix_variants() {
        assert_eq!(nick_from_prefix("nick!user@host"), "nick");
        assert_eq!(nick_from_prefix("irc.example.com"), "irc.example.com");
        assert_eq!(nick_from_prefix(""), "");
    }

    #[test]
    fn is_channel_target_prefixes() {
        assert!(is_channel_target("#hermes"));
        assert!(is_channel_target("&local"));
        assert!(is_channel_target("+modeless"));
        assert!(!is_channel_target("hermes-bot"));
        assert!(!is_channel_target(""));
    }

    #[test]
    fn strip_address_prefix_forms() {
        assert_eq!(
            strip_address_prefix("hermes-bot: hello there", "hermes-bot"),
            Some("hello there")
        );
        assert_eq!(
            strip_address_prefix("HERMES-BOT, hi", "hermes-bot"),
            Some("hi")
        );
        assert_eq!(
            strip_address_prefix("hermes-bot what time is it", "hermes-bot"),
            Some("what time is it")
        );
        assert_eq!(strip_address_prefix("hello everyone", "hermes-bot"), None);
        assert_eq!(strip_address_prefix("other-bot: hi", "hermes-bot"), None);
        // Bare nick without separator is NOT an address (avoids false triggers).
        assert_eq!(strip_address_prefix("hermes-bot", "hermes-bot"), None);
    }

    #[test]
    fn ctcp_detection_and_action() {
        assert!(is_ctcp("\x01ACTION waves\x01"));
        assert!(is_ctcp("\x01VERSION\x01"));
        assert!(!is_ctcp("plain text"));
        assert!(!is_ctcp("\x01unterminated"));
        assert_eq!(
            parse_ctcp_action("\x01ACTION waves hello\x01"),
            Some("waves hello")
        );
        assert_eq!(parse_ctcp_action("\x01VERSION\x01"), None);
        assert_eq!(parse_ctcp_action("plain"), None);
    }

    #[test]
    fn strip_irc_control_chars_neutralises_injection() {
        let out = strip_irc_control_chars("a\r\nb\x00c\nd");
        assert!(!out.contains(['\r', '\n', '\x00']));
        // CR/LF become spaces; NUL is dropped.
        assert_eq!(out, "a  bc d");
    }

    #[test]
    fn strip_markdown_converts_to_plain() {
        assert_eq!(strip_markdown("**bold** and *italic*"), "bold and italic");
        assert_eq!(strip_markdown("`code` here"), "code here");
        assert_eq!(strip_markdown("_italic_ text"), "italic text");
        // Underscores inside words are left alone.
        assert_eq!(strip_markdown("a_b_c"), "a_b_c");
        assert_eq!(
            strip_markdown("[Hermes](https://example.com)"),
            "Hermes (https://example.com)"
        );
        assert_eq!(strip_markdown("```rust\nlet x = 1;\n```"), "let x = 1;\n");
    }

    #[test]
    fn privmsg_line_budget_math() {
        // "PRIVMSG " (8) + "#chan" (5) + " :" (2) + CRLF (2) = 17 → 510 - 17 = 495
        assert_eq!(privmsg_line_budget("#chan"), 495);
    }

    #[test]
    fn split_privmsg_text_respects_byte_budget() {
        let target = "#chan";
        let text = "word ".repeat(300); // 1500 bytes
        let chunks = split_privmsg_text(&text, target, 450);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            let line = format!("PRIVMSG {target} :{chunk}\r\n");
            assert!(line.len() <= 512, "line too long: {} bytes", line.len());
        }
        // Words are not split mid-word.
        for chunk in &chunks {
            assert!(!chunk.ends_with("wo") || chunk == "word");
        }
        let rejoined = chunks.join(" ");
        assert_eq!(rejoined.split_whitespace().count(), 300);
    }

    #[test]
    fn split_privmsg_text_multibyte_safe() {
        // CJK chars are 3 bytes each: 200 × 3 = 600 bytes > 493 budget.
        let text: String = "漢".repeat(200);
        let chunks = split_privmsg_text(&text, "#chan", 450);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            let line = format!("PRIVMSG #chan :{chunk}\r\n");
            assert!(line.as_bytes().len() <= 512);
            assert!(std::str::from_utf8(line.as_bytes()).is_ok());
        }
        assert_eq!(chunks.concat().chars().count(), 200);
    }

    #[test]
    fn split_privmsg_text_empty_and_paragraphs() {
        assert!(split_privmsg_text("", "#c", 450).is_empty());
        assert!(split_privmsg_text("  \n  ", "#c", 450).is_empty());
        let chunks = split_privmsg_text("first\n\nsecond", "#c", 450);
        assert_eq!(chunks, vec!["first", "second"]);
    }

    #[test]
    fn next_nick_after_collision_sequence() {
        assert_eq!(next_nick_after_collision("hermes-bot", 1), "hermes-bot_1");
        assert_eq!(next_nick_after_collision("hermes-bot_1", 2), "hermes-bot_2");
        assert_eq!(next_nick_after_collision("hermes-bot_", 1), "hermes-bot_1");
        // Truncated to 16 chars.
        let long = next_nick_after_collision("averylongnickname", 12);
        assert!(long.chars().count() <= 16);
        assert!(long.ends_with("_12"));
    }

    #[test]
    fn irc_adapter_new_validates_config() {
        assert!(IrcAdapter::new(test_config()).is_ok());

        let mut bad = test_config();
        bad.server = String::new();
        assert!(IrcAdapter::new(bad).is_err());

        let mut bad = test_config();
        bad.channels = vec![];
        assert!(IrcAdapter::new(bad).is_err());

        let mut bad = test_config();
        bad.nick = "  ".to_string();
        assert!(IrcAdapter::new(bad).is_err());
    }

    #[test]
    fn irc_adapter_new_rejects_tls() {
        let mut cfg = test_config();
        cfg.use_tls = true;
        let err = match IrcAdapter::new(cfg) {
            Ok(_) => panic!("expected TLS rejection"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("TLS"), "unexpected error: {err}");
    }

    #[test]
    fn irc_config_from_platform_config() {
        let mut p = hermes_config::PlatformConfig::default();
        p.enabled = true;
        p.extra
            .insert("server".to_string(), serde_json::json!("irc.libera.chat"));
        p.extra.insert("port".to_string(), serde_json::json!(6667));
        p.extra
            .insert("nickname".to_string(), serde_json::json!("mybot"));
        p.extra
            .insert("channel".to_string(), serde_json::json!("#a, #b"));
        p.extra
            .insert("server_password".to_string(), serde_json::json!("s3cret"));
        p.allowed_users = vec!["Alice".to_string()];

        let cfg = IrcConfig::from_platform_config(&p);
        assert_eq!(cfg.server, "irc.libera.chat");
        assert_eq!(cfg.port, 6667);
        assert_eq!(cfg.nick, "mybot");
        assert_eq!(cfg.channels, vec!["#a", "#b"]);
        assert_eq!(cfg.password.as_deref(), Some("s3cret"));
        assert_eq!(cfg.allowed_users, vec!["Alice"]);
        assert!(!cfg.use_tls);
    }

    #[test]
    fn irc_config_defaults() {
        let cfg = IrcConfig::from_platform_config(&hermes_config::PlatformConfig::default());
        assert_eq!(cfg.port, 6667);
        assert_eq!(cfg.nick, "hermes-bot");
        assert_eq!(cfg.max_message_length, 450);
        assert!(cfg.channels.is_empty());
    }

    #[test]
    fn irc_config_serde_roundtrip() {
        let cfg = test_config();
        let json = serde_json::to_value(&cfg).unwrap();
        let back: IrcConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back.server, cfg.server);
        assert_eq!(back.port, cfg.port);
        assert_eq!(back.nick, cfg.nick);
        assert_eq!(back.channels, cfg.channels);
    }
}
