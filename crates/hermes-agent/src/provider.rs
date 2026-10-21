//! LLM provider implementations.
//!
//! Provides concrete implementations of the `LlmProvider` trait for
//! OpenAI, Anthropic, and OpenRouter APIs.

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hermes_core::{
    AgentError, FunctionCall, FunctionCallDelta, LlmProvider, LlmResponse, Message, MessageRole,
    StreamChunk, StreamDelta, ToolCall, ToolCallDelta, ToolSchema, UsageStats,
};

use crate::credential_pool::CredentialPool;
use crate::rate_limit::RateLimitTracker;

// ---------------------------------------------------------------------------
// Request-shape parity helpers (hermes-rs W3/W4, original `agent/` adapters)
// ---------------------------------------------------------------------------

/// Canonical base model name for per-model request rules: trimmed,
/// lowercased, with any `provider/` (OpenRouter-style) or `provider:`
/// (fork factory-style) prefix stripped.
pub fn model_base_name(model: &str) -> String {
    let m = model.trim().to_lowercase();
    let no_slash = m.rsplit('/').next().unwrap_or(&m);
    no_slash
        .split_once(':')
        .map(|(_, rest)| rest)
        .unwrap_or(no_slash)
        .to_string()
}

/// Mirrors `utils.model_forces_max_completion_tokens` in the original
/// (hermes-rs `openai.rs`): `true` for the OpenAI model families that reject
/// `max_tokens` (HTTP 400 `unsupported_parameter`) and require
/// `max_completion_tokens` instead.
pub fn model_forces_max_completion_tokens(model: &str) -> bool {
    let name = model_base_name(model);
    ["gpt-4o", "gpt-4.1", "gpt-5", "o1", "o3", "o4"]
        .iter()
        .any(|p| name.starts_with(p))
}

/// Per-model output-token ceiling for `max_tokens` (parity with hermes-rs
/// `AgentProfile::max_output_ceiling_for_model`). Unknown models fall back
/// to 8192, the historical hardcoded default.
pub fn max_output_ceiling_for_model(model: &str) -> u32 {
    let name = model_base_name(model);
    if name.starts_with("claude-") {
        64_000
    } else if name.starts_with("gpt-4o") || name.starts_with("gpt-4.1") {
        128_000
    } else if name.starts_with("gemini-") {
        65_536
    } else if name.starts_with("deepseek") {
        64_000
    } else {
        8_192
    }
}

/// Effective `max_tokens` for a turn: an explicit value always wins but is
/// clamped to the per-model ceiling; unset means the ceiling itself
/// (parity with hermes-rs `AgentProfile::effective_max_tokens`).
pub fn effective_max_tokens_for_model(model: &str, max_tokens: Option<u32>) -> Option<u32> {
    let ceiling = max_output_ceiling_for_model(model);
    Some(match max_tokens {
        Some(v) => v.min(ceiling),
        None => ceiling,
    })
}

/// Apply the output-token budget to an OpenAI-compatible request body:
/// clamps to the per-model ceiling and selects `max_completion_tokens` for
/// model families that reject `max_tokens`.
pub fn apply_max_tokens(body: &mut Value, model: &str, max_tokens: Option<u32>) {
    let key = if model_forces_max_completion_tokens(model) {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };
    if let Some(mt) = effective_max_tokens_for_model(model, max_tokens) {
        body[key] = serde_json::json!(mt);
    }
}

// ---------------------------------------------------------------------------
// GenericProvider — a flexible, config-driven provider
// ---------------------------------------------------------------------------

/// A generic LLM provider that can be configured for any OpenAI-compatible API.
///
/// This is the primary provider used by the agent loop. It supports
/// OpenAI-compatible APIs via configuration.
#[derive(Debug, Clone)]
pub struct GenericProvider {
    /// Base URL for the API endpoint.
    pub base_url: String,
    /// API key for authentication.
    pub api_key: String,
    /// Default model identifier.
    pub model: String,
    /// HTTP client.
    client: Arc<Mutex<Client>>,
    /// Last time we rebuilt the client transport.
    client_refreshed_at: Arc<Mutex<Instant>>,
    /// Optional custom headers to send with every request.
    pub extra_headers: Vec<(String, String)>,
    /// Optional rate limit tracker.
    pub rate_limiter: Option<Arc<RateLimitTracker>>,
    /// Optional credential pool for key rotation.
    pub credential_pool: Option<Arc<CredentialPool>>,
}

impl GenericProvider {
    fn openrouter_tool_limit(extra_body: Option<&Value>) -> usize {
        if let Some(limit) = extra_body
            .and_then(|v| v.get("openrouter_max_tools"))
            .and_then(|v| v.as_u64())
        {
            return limit as usize;
        }
        if let Ok(raw) = std::env::var("HERMES_OPENROUTER_MAX_TOOLS") {
            if let Ok(parsed) = raw.parse::<usize>() {
                return parsed;
            }
        }
        24
    }

    fn tool_priority(name: &str) -> usize {
        // Prefer filesystem/search primitives first when we must trim toolset size.
        // These are the most common "remote mode" requests and preserve core utility.
        if name.starts_with("read_")
            || name.starts_with("write_")
            || name.starts_with("list_")
            || name.starts_with("glob")
            || name.starts_with("search")
            || name.starts_with("grep")
            || name.contains("file")
            || name.contains("dir")
            || name.contains("path")
        {
            return 0;
        }
        if name.starts_with("exec") || name.contains("terminal") || name.contains("shell") {
            return 1;
        }
        if name.starts_with("browser_") {
            return 2;
        }
        3
    }

    fn normalize_tools_payload(&self, tools: &[ToolSchema], extra_body: Option<&Value>) -> Value {
        let tools = dedupe_tools_by_name(tools);
        let mut normalized: Vec<Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();

        if self.base_url.contains("openrouter.ai") {
            // OpenRouter may route to stricter downstream providers. Trim and prioritize
            // high-signal tools to avoid provider-specific payload validation failures.
            let mut ranked: Vec<(usize, Value)> = normalized
                .into_iter()
                .map(|v| {
                    let name = v
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or_default();
                    (Self::tool_priority(name), v)
                })
                .collect();
            ranked.sort_by_key(|(priority, _)| *priority);
            let limit = Self::openrouter_tool_limit(extra_body);
            normalized = ranked.into_iter().take(limit).map(|(_, v)| v).collect();
        }

        Value::Array(normalized)
    }

    /// Create a new generic provider.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            client: Arc::new(Mutex::new(Client::new())),
            client_refreshed_at: Arc::new(Mutex::new(Instant::now())),
            extra_headers: Vec::new(),
            rate_limiter: None,
            credential_pool: None,
        }
    }

    /// Add a custom header to be sent with every request.
    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((key.into(), value.into()));
        self
    }

    /// Set a custom base URL.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Set the default model.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Attach a rate limit tracker.
    pub fn with_rate_limiter(mut self, tracker: Arc<RateLimitTracker>) -> Self {
        self.rate_limiter = Some(tracker);
        self
    }

    /// Attach a credential pool for API key rotation.
    pub fn with_credential_pool(mut self, pool: Arc<CredentialPool>) -> Self {
        self.credential_pool = Some(pool);
        self
    }

    /// Get the effective API key, using the credential pool if available.
    fn effective_api_key(&self) -> String {
        if let Some(ref pool) = self.credential_pool {
            pool.get_key()
        } else {
            self.api_key.clone()
        }
    }

    /// Check rate limits before making a request. Waits if needed.
    async fn check_rate_limit(&self) {
        if let Some(ref tracker) = self.rate_limiter {
            if let Some(wait_duration) = tracker.should_wait() {
                tracing::info!(
                    "Rate limited, waiting {:?} before next request",
                    wait_duration
                );
                tokio::time::sleep(wait_duration).await;
            }
        }
    }

    /// Update rate limit state from response headers.
    fn update_rate_limit(&self, headers: &reqwest::header::HeaderMap) {
        if let Some(ref tracker) = self.rate_limiter {
            tracker.update_from_headers(headers);
        }
    }

    /// Inject optional runtime hints: reasoning effort, vision preprocessing,
    /// and service tier.
    fn apply_runtime_hints(
        &self,
        body: &mut Value,
        messages: &[Message],
        extra_body: Option<&Value>,
    ) {
        // Reasoning effort passthrough (`low|medium|high`) using extra_body.reasoning_effort.
        if let Some(eb) = extra_body
            .and_then(|v| v.get("reasoning_effort"))
            .and_then(|v| v.as_str())
        {
            body["reasoning_effort"] = serde_json::json!(eb);
        }

        // OpenAI service tier passthrough.
        if let Some(st) = extra_body
            .and_then(|v| v.get("service_tier"))
            .and_then(|v| v.as_str())
        {
            body["service_tier"] = serde_json::json!(st);
        }

        // Vision preprocessing: if user content contains local file-like paths,
        // add a hint field used by downstream adapters.
        let needs_vision_preprocess = messages.iter().any(|m| {
            m.content
                .as_deref()
                .map(|c| c.contains(".png") || c.contains(".jpg") || c.contains("data:image/"))
                .unwrap_or(false)
        });
        if needs_vision_preprocess {
            body["vision_preprocessed"] = serde_json::json!(true);
        }
    }

    /// Force-close helper for future explicit TCP cleanup hooks.
    pub fn force_close_tcp_sockets(&self) {
        // reqwest handles connection pooling internally; dropping clones and relying
        // on idle timeout is currently sufficient for our runtime.
    }

    fn current_client(&self) -> Client {
        self.client
            .lock()
            .map(|c| c.clone())
            .unwrap_or_else(|_| Client::new())
    }

    fn refresh_client(&self, reason: &str) {
        tracing::warn!("rebuilding primary HTTP client: {}", reason);
        if let Ok(mut c) = self.client.lock() {
            *c = Client::new();
        }
        if let Ok(mut t) = self.client_refreshed_at.lock() {
            *t = Instant::now();
        }
    }

    async fn maybe_refresh_stale_client(&self, probe_url: &str) {
        const STALE_CLIENT_REFRESH_SECS: u64 = 300;
        let stale_after = Duration::from_secs(STALE_CLIENT_REFRESH_SECS);
        let should_refresh = self
            .client_refreshed_at
            .lock()
            .map(|t| t.elapsed() >= stale_after)
            .unwrap_or(false);
        if !should_refresh {
            return;
        }
        let probe_client = self.current_client();
        match probe_client
            .get(probe_url)
            .timeout(Duration::from_secs(3))
            .send()
            .await
        {
            Ok(_) => {
                if let Ok(mut t) = self.client_refreshed_at.lock() {
                    *t = Instant::now();
                }
            }
            Err(e) => {
                if Self::is_connection_recoverable(&e) {
                    self.refresh_client(&format!("stale connection probe failed: {e}"));
                } else if let Ok(mut t) = self.client_refreshed_at.lock() {
                    *t = Instant::now();
                }
            }
        }
    }

    fn is_connection_recoverable(err: &reqwest::Error) -> bool {
        if err.is_connect() || err.is_timeout() || err.is_request() {
            return true;
        }
        let msg = err.to_string().to_lowercase();
        msg.contains("connection reset")
            || msg.contains("connection closed")
            || msg.contains("broken pipe")
            || msg.contains("pool")
            || msg.contains("eof")
    }

    fn should_sanitize_tool_calls(extra_body: Option<&Value>) -> bool {
        extra_body
            .and_then(|v| {
                v.get("strict_tool_calls")
                    .or_else(|| v.get("strict_api"))
                    .or_else(|| v.get("provider_strict"))
            })
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    pub(crate) fn sanitize_messages_for_strict_api(messages: &[Message], enabled: bool) -> Value {
        let mut out = Vec::with_capacity(messages.len());
        for msg in messages {
            let mut api_msg = serde_json::to_value(msg).unwrap_or_else(|_| serde_json::json!({}));
            if let Some(tool_calls) = api_msg.get_mut("tool_calls").and_then(|v| v.as_array_mut()) {
                let mut rebuilt_tool_calls: Vec<Value> = Vec::with_capacity(tool_calls.len());
                for tc in tool_calls.iter() {
                    if let Some(obj) = tc.as_object() {
                        let id = obj.get("id").cloned();
                        let function = obj.get("function").cloned().or_else(|| {
                            let name = obj.get("name").and_then(|v| v.as_str())?;
                            let arguments = obj
                                .get("arguments")
                                .cloned()
                                .map(|v| {
                                    if let Some(s) = v.as_str() {
                                        Value::String(s.to_string())
                                    } else {
                                        Value::String(v.to_string())
                                    }
                                })
                                .unwrap_or_else(|| Value::String("{}".to_string()));
                            Some(serde_json::json!({
                                "name": name,
                                "arguments": arguments,
                            }))
                        });
                        // Defensive: drop malformed legacy tool_call items that cannot be
                        // converted into OpenAI-compatible {"function": {...}} shape.
                        if function.is_none() {
                            continue;
                        }
                        let mut stripped = serde_json::Map::new();
                        if let Some(v) = id {
                            stripped.insert("id".to_string(), v);
                        }
                        stripped.insert(
                            "type".to_string(),
                            obj.get("type")
                                .cloned()
                                .unwrap_or_else(|| Value::String("function".to_string())),
                        );
                        if let Some(v) = function {
                            stripped.insert("function".to_string(), v);
                        }
                        if enabled {
                            rebuilt_tool_calls.push(Value::Object(stripped));
                        } else {
                            // Always keep an OpenAI-compatible representation to avoid provider
                            // deserialization errors when replaying tool call messages.
                            let mut merged = obj.clone();
                            if let Some(v) = stripped.get("function").cloned() {
                                merged.insert("function".to_string(), v);
                            }
                            merged.insert(
                                "type".to_string(),
                                stripped
                                    .get("type")
                                    .cloned()
                                    .unwrap_or_else(|| Value::String("function".to_string())),
                            );
                            rebuilt_tool_calls.push(Value::Object(merged));
                        }
                    }
                }
                if let Some(api_obj) = api_msg.as_object_mut() {
                    if rebuilt_tool_calls.is_empty() {
                        api_obj.remove("tool_calls");
                    } else {
                        api_obj.insert("tool_calls".to_string(), Value::Array(rebuilt_tool_calls));
                    }
                }
            }
            out.push(api_msg);
        }
        Value::Array(out)
    }

    fn build_request(
        &self,
        client: &Client,
        url: &str,
        api_key: &str,
        body: &Value,
    ) -> reqwest::RequestBuilder {
        let mut req = client
            .post(url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .json(body);
        for (key, value) in &self.extra_headers {
            req = req.header(key.as_str(), value.as_str());
        }
        req
    }

    async fn send_with_dead_connection_recovery(
        &self,
        url: &str,
        api_key: &str,
        body: &Value,
    ) -> Result<reqwest::Response, AgentError> {
        self.maybe_refresh_stale_client(url).await;
        let client = self.current_client();
        match self.build_request(&client, url, api_key, body).send().await {
            Ok(resp) => Ok(resp),
            Err(e) => {
                if !Self::is_connection_recoverable(&e) {
                    return Err(AgentError::LlmApi(format!("HTTP request failed: {e}")));
                }
                self.refresh_client(&format!("recoverable transport error: {e}"));
                let retry_client = self.current_client();
                self.build_request(&retry_client, url, api_key, body)
                    .send()
                    .await
                    .map_err(|e2| {
                        AgentError::LlmApi(format!(
                            "HTTP request failed after reconnect retry: {e2}"
                        ))
                    })
            }
        }
    }
}

#[async_trait]
impl LlmProvider for GenericProvider {
    async fn chat_completion(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra_body: Option<&Value>,
    ) -> Result<LlmResponse, AgentError> {
        self.check_rate_limit().await;

        let effective_model = model.unwrap_or(&self.model);
        let api_key = self.effective_api_key();
        let strict_tool_sanitize = Self::should_sanitize_tool_calls(extra_body);
        let api_messages = Self::sanitize_messages_for_strict_api(messages, strict_tool_sanitize);

        let mut body = serde_json::json!({
            "model": effective_model,
            "messages": api_messages,
        });

        apply_max_tokens(&mut body, effective_model, max_tokens);
        if let Some(temp) = temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if !tools.is_empty() {
            body["tools"] = self.normalize_tools_payload(tools, extra_body);
        }
        if let Some(eb) = extra_body {
            if let Value::Object(map) = eb {
                for (k, v) in map {
                    body[k] = v.clone();
                }
            }
        }
        self.apply_runtime_hints(&mut body, messages, extra_body);

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let resp = self
            .send_with_dead_connection_recovery(&url, &api_key, &body)
            .await?;

        self.update_rate_limit(resp.headers());

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            return Err(AgentError::LlmApi(format!(
                "API error {status}: {body_text}"
            )));
        }

        let resp_json: Value = resp
            .json()
            .await
            .map_err(|e| AgentError::LlmApi(format!("Failed to parse response: {e}")))?;

        parse_openai_response(&resp_json)
    }

    fn chat_completion_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra_body: Option<&Value>,
    ) -> BoxStream<'static, Result<StreamChunk, AgentError>> {
        let provider = self.clone();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        let model = model.map(|s| s.to_string());
        let extra_body = extra_body.cloned();

        async_stream::stream! {
            provider.check_rate_limit().await;

            let effective_model = model.as_deref().unwrap_or(&provider.model);
            let api_key = provider.effective_api_key();
            let strict_tool_sanitize = GenericProvider::should_sanitize_tool_calls(extra_body.as_ref());
            let api_messages =
                GenericProvider::sanitize_messages_for_strict_api(&messages, strict_tool_sanitize);

            let mut body = serde_json::json!({
                "model": effective_model,
                "messages": api_messages,
                "stream": true,
            });

            apply_max_tokens(&mut body, effective_model, max_tokens);
            if let Some(temp) = temperature {
                body["temperature"] = serde_json::json!(temp);
            }
            if !tools.is_empty() {
                body["tools"] = provider.normalize_tools_payload(&tools, extra_body.as_ref());
            }
            if let Some(ref eb) = extra_body {
                if let Value::Object(map) = eb {
                    for (k, v) in map {
                        body[k] = v.clone();
                    }
                }
            }
            provider.apply_runtime_hints(&mut body, &messages, extra_body.as_ref());
            // Request usage in the final streaming chunk
            body["stream_options"] = serde_json::json!({"include_usage": true});

            let url = format!("{}/chat/completions", provider.base_url.trim_end_matches('/'));

            let resp = match provider
                .send_with_dead_connection_recovery(&url, &api_key, &body)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };

            provider.update_rate_limit(resp.headers());

            if !resp.status().is_success() {
                let status = resp.status();
                let body_text = resp.text().await.unwrap_or_else(|_| "<no body>".to_string());
                yield Err(AgentError::LlmApi(format!("API error {status}: {body_text}")));
                return;
            }

            // Read the SSE byte stream line by line
            let mut byte_stream = resp.bytes_stream();
            let mut buffer = String::new();

            while let Some(chunk_result) = byte_stream.next().await {
                let chunk_bytes = match chunk_result {
                    Ok(b) => b,
                    Err(e) => {
                        yield Err(AgentError::LlmApi(format!("Stream read error: {e}")));
                        return;
                    }
                };

                buffer.push_str(&String::from_utf8_lossy(&chunk_bytes));

                // Process complete SSE events (separated by double newlines)
                while let Some(event_end) = buffer.find("\n\n") {
                    let event_block = buffer[..event_end].to_string();
                    buffer = buffer[event_end + 2..].to_string();

                    for line in event_block.lines() {
                        match handle_sse_line(line) {
                            SseLineAction::Emit(chunk) => yield Ok(chunk),
                            SseLineAction::Done => return,
                            SseLineAction::Ignore => {}
                            SseLineAction::Error(msg) => {
                                yield Err(AgentError::LlmApi(format!(
                                    "provider stream error: {msg}"
                                )));
                                return;
                            }
                        }
                    }
                }
            }

            // Process any remaining data in the buffer
            if !buffer.trim().is_empty() {
                for line in buffer.lines() {
                    match handle_sse_line(line) {
                        SseLineAction::Emit(chunk) => yield Ok(chunk),
                        SseLineAction::Done => return,
                        SseLineAction::Ignore => {}
                        SseLineAction::Error(msg) => {
                            yield Err(AgentError::LlmApi(format!(
                                "provider stream error: {msg}"
                            )));
                            return;
                        }
                    }
                }
            }
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// OpenAiProvider
// ---------------------------------------------------------------------------

/// OpenAI API provider.
#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    inner: GenericProvider,
}

impl OpenAiProvider {
    /// Create a new OpenAI provider with the given API key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            inner: GenericProvider::new("https://api.openai.com/v1", api_key, "gpt-4o"),
        }
    }

    /// Use a custom base URL (e.g., for Azure OpenAI).
    pub fn with_base_url(self, base_url: impl Into<String>) -> Self {
        Self {
            inner: self.inner.with_base_url(base_url),
        }
    }

    /// Set the default model.
    pub fn with_model(self, model: impl Into<String>) -> Self {
        Self {
            inner: self.inner.with_model(model),
        }
    }

    /// Attach a credential pool for API key rotation.
    pub fn with_credential_pool(self, pool: Arc<CredentialPool>) -> Self {
        Self {
            inner: self.inner.with_credential_pool(pool),
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn chat_completion(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra_body: Option<&Value>,
    ) -> Result<LlmResponse, AgentError> {
        self.inner
            .chat_completion(messages, tools, max_tokens, temperature, model, extra_body)
            .await
    }

    fn chat_completion_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra_body: Option<&Value>,
    ) -> BoxStream<'static, Result<StreamChunk, AgentError>> {
        self.inner.chat_completion_stream(
            messages,
            tools,
            max_tokens,
            temperature,
            model,
            extra_body,
        )
    }
}

// ---------------------------------------------------------------------------
// AnthropicProvider
// ---------------------------------------------------------------------------

/// Credential env vars for Anthropic, in precedence order: `ANTHROPIC_TOKEN` /
/// `CLAUDE_CODE_OAUTH_TOKEN` (OAuth/subscription tokens) first, then
/// `ANTHROPIC_API_KEY` (Console API key). Only the first set (non-empty) var
/// in the tuple is read; values from the other vars are ignored so no stray
/// credential is ever sent.
pub const ANTHROPIC_CREDENTIAL_ENV_VARS: [&str; 3] = [
    "ANTHROPIC_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_API_KEY",
];

/// Beta headers safe for ordinary native requests.
const ANTHROPIC_COMMON_BETAS: &str =
    "interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14";
/// Extra betas required for the OAuth/subscription auth path.
const ANTHROPIC_OAUTH_ONLY_BETAS: &str = "claude-code-20250219,oauth-2025-04-20";
/// Fallback Claude Code version used for the identity `user-agent` header when
/// the `claude` CLI is not installed.
const CLAUDE_CODE_VERSION_FALLBACK: &str = "2.1.74";

/// Per-request auth style for the Anthropic Messages API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnthropicAuthStyle {
    /// Console API key: `x-api-key` header, no Bearer, no identity headers.
    ApiKey,
    /// OAuth/subscription token: `Authorization: Bearer` plus Claude Code
    /// identity headers — only ever on the native Anthropic host.
    OAuth,
}

/// Classify a credential as an OAuth token (`true`) or a Console API key
/// (`false`). Console key families (`sk-ant-api*`, `sk-ant-usr*`,
/// `sk-ant-svc*`, `sk-ant-admin*`) are billed per token and authenticate as
/// `x-api-key`, so they must never take the OAuth path — even when found in
/// an OAuth env var. Unknown `sk-ant-*` prefixes default to OAuth so new
/// Anthropic-issued OAuth prefixes keep working; `eyJ*` (JWT) and `cc-*`
/// are OAuth shapes too.
fn is_anthropic_oauth_token(key: &str) -> bool {
    if key.is_empty()
        || key.starts_with("sk-ant-api")
        || key.starts_with("sk-ant-usr")
        || key.starts_with("sk-ant-svc")
        || key.starts_with("sk-ant-admin")
    {
        return false;
    }
    key.starts_with("sk-ant-") || key.starts_with("eyJ") || key.starts_with("cc-")
}

/// True when `base_url` is the native Anthropic API host (`api.anthropic.com`;
/// an empty base URL means the native default). Identity headers must never be
/// sent to proxies or custom base URLs — third-party endpoints reject them.
fn is_native_anthropic_host(base_url: &str) -> bool {
    let text = base_url.trim();
    if text.is_empty() {
        return true;
    }
    let after_scheme = text.split("://").nth(1).unwrap_or(text);
    let authority = after_scheme.split('/').next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    host.eq_ignore_ascii_case("api.anthropic.com")
}

/// Resolve the auth style for one request: OAuth identity (Bearer + Claude
/// Code headers) only on the native host with an OAuth-shaped token;
/// everything else — including OAuth-shaped tokens aimed at proxies — uses
/// the `x-api-key` style.
fn anthropic_auth_style(base_url: &str, key: &str) -> AnthropicAuthStyle {
    if is_native_anthropic_host(base_url) && is_anthropic_oauth_token(key) {
        AnthropicAuthStyle::OAuth
    } else {
        AnthropicAuthStyle::ApiKey
    }
}

/// Claude Code identity `user-agent` header value.
fn claude_code_user_agent(version: &str) -> String {
    format!("claude-code/{version} (external, cli)")
}

/// Try `claude --version`; `None` when the CLI is missing or fails. Bounded
/// (~2s) so a hanging CLI cannot stall request setup.
fn detect_claude_code_version() -> Option<String> {
    let mut child = std::process::Command::new("claude")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let out = child.wait_with_output().ok()?;
                // Output looks like "2.1.74 (Claude Code)"; take the first token.
                let token = String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .next()?
                    .trim_start_matches('v')
                    .to_string();
                return token
                    .chars()
                    .next()
                    .filter(|c| c.is_ascii_digit())
                    .map(|_| token);
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

/// Claude Code version for the identity headers, probed once per process.
fn claude_code_version() -> String {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            detect_claude_code_version().unwrap_or_else(|| CLAUDE_CODE_VERSION_FALLBACK.to_string())
        })
        .clone()
}

/// Build the exact header set for one request. Always exactly ONE credential:
/// OAuth sends `Authorization: Bearer` (no `x-api-key`) plus the beta set and
/// Claude Code identity headers; API key sends `x-api-key` (no `Authorization`)
/// with the common betas only. Identity headers are gated by
/// [`anthropic_auth_style`], so proxies never receive them.
fn anthropic_request_headers(
    base_url: &str,
    key: &str,
    claude_version: &str,
) -> Vec<(String, String)> {
    match anthropic_auth_style(base_url, key) {
        AnthropicAuthStyle::OAuth => vec![
            ("authorization".to_string(), format!("Bearer {key}")),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
            (
                "anthropic-beta".to_string(),
                format!("{ANTHROPIC_COMMON_BETAS},{ANTHROPIC_OAUTH_ONLY_BETAS}"),
            ),
            (
                "user-agent".to_string(),
                claude_code_user_agent(claude_version),
            ),
            ("x-app".to_string(), "cli".to_string()),
        ],
        AnthropicAuthStyle::ApiKey => vec![
            ("x-api-key".to_string(), key.to_string()),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
            (
                "anthropic-beta".to_string(),
                ANTHROPIC_COMMON_BETAS.to_string(),
            ),
        ],
    }
}

/// Read Anthropic credentials from the env tuple: the first set (non-empty)
/// var wins, and a single var may hold several comma-separated keys.
fn anthropic_env_keys() -> Vec<String> {
    ANTHROPIC_CREDENTIAL_ENV_VARS
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .map(|v| v.trim().to_string())
        .find(|v| !v.is_empty())
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Running usage accumulator for one Anthropic SSE stream.
///
/// `message_start` carries `input_tokens` (+ cache counters) nested under
/// `message.usage`; `message_delta` carries `output_tokens` under `usage`.
/// Fields absent from a payload never overwrite already-accumulated values,
/// so the final usage is `message_start` input/cache + `message_delta` output.
#[derive(Debug, Default, Clone)]
struct AnthropicStreamUsage {
    input_tokens: u64,
    output_tokens: u64,
    // Parsed and accumulated, but not yet surfaced: hermes-core's UsageStats
    // has no cache fields (they arrive with the wave 1-3 types.rs change).
    // Wire into to_stats() once the fields exist.
    #[allow(dead_code)]
    cache_read_input_tokens: Option<u64>,
    #[allow(dead_code)]
    cache_creation_input_tokens: Option<u64>,
}

/// Merge one Anthropic `usage` object into the accumulator: fields present in
/// the payload overwrite, absent fields leave the accumulator untouched.
fn merge_anthropic_usage(acc: &mut AnthropicStreamUsage, u: &Value) {
    if let Some(n) = u.get("input_tokens").and_then(Value::as_u64) {
        acc.input_tokens = n;
    }
    if let Some(n) = u.get("output_tokens").and_then(Value::as_u64) {
        acc.output_tokens = n;
    }
    if let Some(n) = u.get("cache_read_input_tokens").and_then(Value::as_u64) {
        acc.cache_read_input_tokens = Some(n);
    }
    if let Some(n) = u.get("cache_creation_input_tokens").and_then(Value::as_u64) {
        acc.cache_creation_input_tokens = Some(n);
    }
}

impl AnthropicStreamUsage {
    fn to_stats(&self) -> UsageStats {
        UsageStats {
            prompt_tokens: self.input_tokens,
            completion_tokens: self.output_tokens,
            total_tokens: self.input_tokens + self.output_tokens,
            estimated_cost: None,
            // NOTE: cache_read/cache_creation counters are parsed into the
            // accumulator above but dropped here until hermes-core's
            // UsageStats gains the corresponding fields.
        }
    }
}

/// Outcome of processing one parsed SSE event block.
#[derive(Debug)]
enum AnthropicSseOutcome {
    /// Yield this chunk to the stream consumer.
    Chunk(StreamChunk),
    /// `message_stop` seen: the stream completed normally.
    Done,
    /// Nothing worth emitting for this event.
    Ignore,
    /// Yield this error and terminate the stream.
    Error(AgentError),
}

/// Extract an SSE field value (`event` / `data`), tolerating a missing space
/// after the colon: both `event:foo` and `event: foo` parse.
fn sse_field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(name)?.strip_prefix(':')?;
    let rest = rest.strip_prefix(' ').unwrap_or(rest);
    Some(rest.trim_end())
}

/// Pure SSE event handler for the Anthropic Messages stream (no I/O), so
/// canned event payloads can be unit-tested without a network connection.
///
/// - `message_start` merges `message.usage` (input + cache counters) and
///   emits a usage chunk.
/// - `message_delta` merges `usage` (output tokens) and emits the stop
///   reason together with the fully merged usage.
/// - `error` events (or `{"type":"error"}` payloads) surface as errors,
///   never as content.
/// - Malformed `data:` payloads are skipped, never fatal; unknown events
///   (e.g. `ping`) are ignored.
fn handle_anthropic_sse_event(
    event_type: &str,
    json: &Value,
    usage: &mut AnthropicStreamUsage,
    tool_index: &mut u32,
) -> AnthropicSseOutcome {
    // SSE error shape surfaces as an error, not as content.
    let payload_is_error = json.get("type").and_then(|t| t.as_str()) == Some("error");
    if event_type == "error" || payload_is_error {
        let message = json
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("unknown stream error");
        return AnthropicSseOutcome::Error(AgentError::LlmApi(format!(
            "Anthropic stream error: {message}"
        )));
    }
    match event_type {
        "content_block_start" => {
            let block = json.get("content_block").unwrap_or(json);
            let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if block_type == "tool_use" {
                let id = block
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let idx = json
                    .get("index")
                    .and_then(|i| i.as_u64())
                    .unwrap_or(*tool_index as u64) as u32;
                *tool_index = idx;
                AnthropicSseOutcome::Chunk(StreamChunk {
                    delta: Some(StreamDelta {
                        content: None,
                        tool_calls: Some(vec![ToolCallDelta {
                            index: idx,
                            id: Some(id),
                            function: Some(FunctionCallDelta {
                                name: Some(name),
                                arguments: None,
                            }),
                        }]),
                        extra: None,
                    }),
                    finish_reason: None,
                    usage: None,
                })
            } else {
                AnthropicSseOutcome::Ignore
            }
        }
        "content_block_delta" => {
            let delta = json.get("delta").unwrap_or(json);
            let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match delta_type {
                "text_delta" => {
                    let text = delta
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    AnthropicSseOutcome::Chunk(StreamChunk {
                        delta: Some(StreamDelta {
                            content: Some(text),
                            tool_calls: None,
                            extra: None,
                        }),
                        finish_reason: None,
                        usage: None,
                    })
                }
                "input_json_delta" => {
                    let partial = delta
                        .get("partial_json")
                        .and_then(|p| p.as_str())
                        .unwrap_or("")
                        .to_string();
                    AnthropicSseOutcome::Chunk(StreamChunk {
                        delta: Some(StreamDelta {
                            content: None,
                            tool_calls: Some(vec![ToolCallDelta {
                                index: *tool_index,
                                id: None,
                                function: Some(FunctionCallDelta {
                                    name: None,
                                    arguments: Some(partial),
                                }),
                            }]),
                            extra: None,
                        }),
                        finish_reason: None,
                        usage: None,
                    })
                }
                _ => AnthropicSseOutcome::Ignore,
            }
        }
        "message_delta" => {
            let stop_reason = json
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(|s| s.as_str())
                .map(|s| match s {
                    "end_turn" => "stop".to_string(),
                    "tool_use" => "tool_calls".to_string(),
                    other => other.to_string(),
                });
            if let Some(u) = json.get("usage") {
                merge_anthropic_usage(usage, u);
            }
            AnthropicSseOutcome::Chunk(StreamChunk {
                delta: None,
                finish_reason: stop_reason,
                usage: Some(usage.to_stats()),
            })
        }
        "message_start" => {
            // The usage object is nested under `message`, not top-level.
            if let Some(u) = json.get("message").and_then(|m| m.get("usage")) {
                merge_anthropic_usage(usage, u);
                AnthropicSseOutcome::Chunk(StreamChunk {
                    delta: None,
                    finish_reason: None,
                    usage: Some(usage.to_stats()),
                })
            } else {
                AnthropicSseOutcome::Ignore
            }
        }
        "message_stop" => AnthropicSseOutcome::Done,
        // "ping" and any unknown event types are ignored.
        _ => AnthropicSseOutcome::Ignore,
    }
}

/// Process one complete SSE event block (the text between blank lines) into
/// an outcome. Malformed `data:` payloads are skipped, never fatal.
fn process_anthropic_event_block(
    event_block: &str,
    usage: &mut AnthropicStreamUsage,
    tool_index: &mut u32,
) -> AnthropicSseOutcome {
    let mut event_type = String::new();
    let mut event_data = String::new();

    for line in event_block.lines() {
        let line = line.trim();
        if let Some(et) = sse_field(line, "event") {
            event_type = et.to_string();
        } else if let Some(d) = sse_field(line, "data") {
            if !event_data.is_empty() {
                event_data.push('\n');
            }
            event_data.push_str(d);
        }
    }

    if event_data.trim().is_empty() {
        return AnthropicSseOutcome::Ignore;
    }

    let json: Value = match serde_json::from_str(event_data.trim()) {
        Ok(v) => v,
        Err(_) => return AnthropicSseOutcome::Ignore,
    };

    handle_anthropic_sse_event(&event_type, &json, usage, tool_index)
}

/// Anthropic API provider with native Messages API support.
///
/// Uses Anthropic's own message format rather than OpenAI-compatible format:
/// - System message goes in `system` parameter, not in messages array
/// - Console API keys use the `x-api-key` header; OAuth/subscription tokens
///   (from `ANTHROPIC_TOKEN` / `CLAUDE_CODE_OAUTH_TOKEN`) use
///   `Authorization: Bearer` plus Claude Code identity headers — but only on
///   the native `api.anthropic.com` host, never on proxies/custom base URLs
/// - Content blocks use array format with typed blocks
/// - Tool use returns `type: "tool_use"` content blocks
#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    /// Base URL for the Anthropic API.
    pub base_url: String,
    /// API key for authentication.
    pub api_key: String,
    /// Default model identifier.
    pub model: String,
    /// HTTP client.
    client: Arc<Mutex<Client>>,
    /// Last time we rebuilt the client transport.
    client_refreshed_at: Arc<Mutex<Instant>>,
    /// Anthropic API version header.
    pub api_version: String,
    /// Optional rate limit tracker.
    pub rate_limiter: Option<Arc<RateLimitTracker>>,
    /// Optional credential pool.
    pub credential_pool: Option<Arc<CredentialPool>>,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider with the given API key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            base_url: Self::normalize_base_url("https://api.anthropic.com"),
            api_key: api_key.into(),
            model: "claude-3-5-sonnet-20241022".to_string(),
            client: Arc::new(Mutex::new(Client::new())),
            client_refreshed_at: Arc::new(Mutex::new(Instant::now())),
            api_version: "2023-06-01".to_string(),
            rate_limiter: None,
            credential_pool: None,
        }
    }

    /// Set the default model.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Set a custom base URL.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Self::normalize_base_url(&url.into());
        self
    }

    /// Normalize a base URL: drop trailing slashes and a trailing `/v1`
    /// segment, since `/v1/messages` is appended per request. Without this a
    /// base like `https://host/v1` would produce `/v1/v1/messages`.
    fn normalize_base_url(base_url: &str) -> String {
        let trimmed = base_url.trim_end_matches('/');
        trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string()
    }

    /// Attach a rate limit tracker.
    pub fn with_rate_limiter(mut self, tracker: Arc<RateLimitTracker>) -> Self {
        self.rate_limiter = Some(tracker);
        self
    }

    /// Attach a credential pool.
    pub fn with_credential_pool(mut self, pool: Arc<CredentialPool>) -> Self {
        self.credential_pool = Some(pool);
        self
    }

    fn effective_api_key(&self) -> String {
        let configured = if let Some(ref pool) = self.credential_pool {
            pool.get_key()
        } else {
            self.api_key.clone()
        };
        if configured.trim().is_empty() {
            // No key configured: fall back to the credential env tuple
            // (ANTHROPIC_TOKEN / CLAUDE_CODE_OAUTH_TOKEN first, then
            // ANTHROPIC_API_KEY). The auth style is resolved per request
            // from the key's shape, so an OAuth-shaped value here takes the
            // Bearer path while an sk-ant-api* value takes the x-api-key path.
            return anthropic_env_keys().into_iter().next().unwrap_or_default();
        }
        configured
    }

    async fn check_rate_limit(&self) {
        if let Some(ref tracker) = self.rate_limiter {
            if let Some(wait_duration) = tracker.should_wait() {
                tracing::info!("Rate limited, waiting {:?}", wait_duration);
                tokio::time::sleep(wait_duration).await;
            }
        }
    }

    fn update_rate_limit(&self, headers: &reqwest::header::HeaderMap) {
        if let Some(ref tracker) = self.rate_limiter {
            tracker.update_from_headers(headers);
        }
    }

    fn current_client(&self) -> Client {
        self.client
            .lock()
            .map(|c| c.clone())
            .unwrap_or_else(|_| Client::new())
    }

    fn refresh_client(&self, reason: &str) {
        tracing::warn!("rebuilding anthropic HTTP client: {}", reason);
        if let Ok(mut c) = self.client.lock() {
            *c = Client::new();
        }
        if let Ok(mut t) = self.client_refreshed_at.lock() {
            *t = Instant::now();
        }
    }

    async fn maybe_refresh_stale_client(&self, probe_url: &str) {
        const STALE_CLIENT_REFRESH_SECS: u64 = 300;
        let stale_after = Duration::from_secs(STALE_CLIENT_REFRESH_SECS);
        let should_refresh = self
            .client_refreshed_at
            .lock()
            .map(|t| t.elapsed() >= stale_after)
            .unwrap_or(false);
        if !should_refresh {
            return;
        }
        let probe_client = self.current_client();
        match probe_client
            .get(probe_url)
            .timeout(Duration::from_secs(3))
            .send()
            .await
        {
            Ok(_) => {
                if let Ok(mut t) = self.client_refreshed_at.lock() {
                    *t = Instant::now();
                }
            }
            Err(e) => {
                if GenericProvider::is_connection_recoverable(&e) {
                    self.refresh_client(&format!("stale connection probe failed: {e}"));
                } else if let Ok(mut t) = self.client_refreshed_at.lock() {
                    *t = Instant::now();
                }
            }
        }
    }

    fn build_request(
        &self,
        client: &Client,
        url: &str,
        api_key: &str,
        body: &Value,
    ) -> reqwest::RequestBuilder {
        // Auth style is resolved per request from the base URL + key shape:
        // OAuth (Bearer + Claude Code identity headers) only on the native
        // host, x-api-key everywhere else. The claude CLI probe runs at most
        // once per process and only on the OAuth path.
        let claude_version;
        let version_arg =
            if anthropic_auth_style(&self.base_url, api_key) == AnthropicAuthStyle::OAuth {
                claude_version = claude_code_version();
                claude_version.as_str()
            } else {
                ""
            };
        let mut builder = client.post(url);
        for (name, value) in anthropic_request_headers(&self.base_url, api_key, version_arg) {
            builder = builder.header(name, value);
        }
        builder
            .header("Content-Type", "application/json")
            .json(body)
    }

    async fn send_with_dead_connection_recovery(
        &self,
        url: &str,
        api_key: &str,
        body: &Value,
    ) -> Result<reqwest::Response, AgentError> {
        self.maybe_refresh_stale_client(url).await;
        let client = self.current_client();
        match self.build_request(&client, url, api_key, body).send().await {
            Ok(resp) => Ok(resp),
            Err(e) => {
                if !GenericProvider::is_connection_recoverable(&e) {
                    return Err(AgentError::LlmApi(format!("HTTP request failed: {e}")));
                }
                self.refresh_client(&format!("recoverable transport error: {e}"));
                let retry_client = self.current_client();
                self.build_request(&retry_client, url, api_key, body)
                    .send()
                    .await
                    .map_err(|e2| {
                        AgentError::LlmApi(format!(
                            "HTTP request failed after reconnect retry: {e2}"
                        ))
                    })
            }
        }
    }

    /// Convert internal messages to Anthropic format, extracting system message.
    fn convert_messages(messages: &[Message]) -> (Option<String>, Vec<Value>) {
        let mut system_text: Option<String> = None;
        let mut anthropic_messages: Vec<Value> = Vec::new();

        for msg in messages {
            match msg.role {
                MessageRole::System => {
                    // Anthropic: system goes in a separate `system` parameter
                    let content = msg.content.as_deref().unwrap_or("");
                    system_text = Some(match system_text {
                        Some(existing) => format!("{existing}\n\n{content}"),
                        None => content.to_string(),
                    });
                }
                MessageRole::User => {
                    let mut content_blocks = Vec::new();
                    if let Some(ref text) = msg.content {
                        let mut block = serde_json::json!({"type": "text", "text": text});
                        if let Some(ref cc) = msg.cache_control {
                            block["cache_control"] = serde_json::json!({"type": format!("{:?}", cc.cache_type).to_lowercase()});
                        }
                        content_blocks.push(block);
                    }
                    anthropic_messages.push(serde_json::json!({
                        "role": "user",
                        "content": content_blocks,
                    }));
                }
                MessageRole::Assistant => {
                    let mut content_blocks = Vec::new();
                    if let Some(ref text) = msg.content {
                        if !text.is_empty() {
                            content_blocks.push(serde_json::json!({"type": "text", "text": text}));
                        }
                    }
                    // Convert tool_calls to Anthropic tool_use blocks
                    if let Some(ref tool_calls) = msg.tool_calls {
                        for tc in tool_calls {
                            let input: Value = serde_json::from_str(&tc.function.arguments)
                                .unwrap_or(serde_json::json!({}));
                            content_blocks.push(serde_json::json!({
                                "type": "tool_use",
                                "id": tc.id,
                                "name": tc.function.name,
                                "input": input,
                            }));
                        }
                    }
                    if !content_blocks.is_empty() {
                        anthropic_messages.push(serde_json::json!({
                            "role": "assistant",
                            "content": content_blocks,
                        }));
                    }
                }
                MessageRole::Tool => {
                    // Anthropic: tool results go as user messages with tool_result content blocks
                    let content_blocks = vec![serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": msg.tool_call_id.as_deref().unwrap_or(""),
                        "content": msg.content.as_deref().unwrap_or(""),
                    })];
                    anthropic_messages.push(serde_json::json!({
                        "role": "user",
                        "content": content_blocks,
                    }));
                }
            }
        }

        (system_text, anthropic_messages)
    }

    /// Convert tool schemas to Anthropic tool format.
    fn convert_tools(tools: &[ToolSchema]) -> Vec<Value> {
        tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect()
    }

    /// Parse an Anthropic Messages API response into LlmResponse.
    fn parse_response(json: &Value) -> Result<LlmResponse, AgentError> {
        let mut content_text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        if let Some(content_arr) = json.get("content").and_then(|c| c.as_array()) {
            for block in content_arr {
                let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match block_type {
                    "text" => {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            if !content_text.is_empty() {
                                content_text.push('\n');
                            }
                            content_text.push_str(text);
                        }
                    }
                    "tool_use" => {
                        let id = block
                            .get("id")
                            .and_then(|i| i.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();
                        let input = block.get("input").cloned().unwrap_or(serde_json::json!({}));
                        let arguments =
                            serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                        tool_calls.push(ToolCall {
                            id,
                            function: FunctionCall { name, arguments },
                        });
                    }
                    _ => {}
                }
            }
        }

        let usage = json.get("usage").map(|u| {
            let input = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let output = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            UsageStats {
                prompt_tokens: input,
                completion_tokens: output,
                total_tokens: input + output,
                estimated_cost: None,
            }
        });

        let stop_reason = json
            .get("stop_reason")
            .and_then(|s| s.as_str())
            .map(|s| match s {
                "end_turn" => "stop".to_string(),
                "tool_use" => "tool_calls".to_string(),
                other => other.to_string(),
            });

        let model = json
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown")
            .to_string();

        let message = Message {
            role: MessageRole::Assistant,
            content: Some(content_text),
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            tool_call_id: None,
            name: None,
            reasoning_content: None,
            cache_control: None,
        };

        Ok(LlmResponse {
            message,
            usage,
            model,
            finish_reason: stop_reason,
        })
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn chat_completion(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra_body: Option<&Value>,
    ) -> Result<LlmResponse, AgentError> {
        self.check_rate_limit().await;

        let effective_model = model.unwrap_or(&self.model);
        // Anthropic requires max_tokens; default to the per-model ceiling
        // rather than a hardcoded 4096 (parity with hermes-rs
        // `effective_max_tokens`).
        let max_tokens =
            effective_max_tokens_for_model(effective_model, max_tokens).unwrap_or(4096);
        let api_key = self.effective_api_key();
        let (system_text, anthropic_messages) = Self::convert_messages(messages);

        let mut body = serde_json::json!({
            "model": effective_model,
            "messages": anthropic_messages,
            "max_tokens": max_tokens,
        });

        if let Some(ref sys) = system_text {
            body["system"] = serde_json::json!(sys);
        }
        if let Some(temp) = temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if !tools.is_empty() {
            body["tools"] = serde_json::json!(Self::convert_tools(tools));
        }
        if let Some(eb) = extra_body {
            if let Value::Object(map) = eb {
                for (k, v) in map {
                    body[k] = v.clone();
                }
            }
        }

        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let resp = self
            .send_with_dead_connection_recovery(&url, &api_key, &body)
            .await?;

        self.update_rate_limit(resp.headers());

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            return Err(AgentError::LlmApi(format!(
                "API error {status}: {body_text}"
            )));
        }

        let resp_json: Value = resp
            .json()
            .await
            .map_err(|e| AgentError::LlmApi(format!("Failed to parse response: {e}")))?;

        Self::parse_response(&resp_json)
    }

    fn chat_completion_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra_body: Option<&Value>,
    ) -> BoxStream<'static, Result<StreamChunk, AgentError>> {
        let provider = self.clone();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        let model = model.map(|s| s.to_string());
        let extra_body = extra_body.cloned();

        async_stream::stream! {
            provider.check_rate_limit().await;

            let effective_model = model.as_deref().unwrap_or(&provider.model);
            let max_tokens =
                effective_max_tokens_for_model(effective_model, max_tokens).unwrap_or(4096);
            let api_key = provider.effective_api_key();
            let (system_text, anthropic_messages) = AnthropicProvider::convert_messages(&messages);

            let mut body = serde_json::json!({
                "model": effective_model,
                "messages": anthropic_messages,
                "max_tokens": max_tokens,
                "stream": true,
            });

            if let Some(ref sys) = system_text {
                body["system"] = serde_json::json!(sys);
            }
            if let Some(temp) = temperature {
                body["temperature"] = serde_json::json!(temp);
            }
            if !tools.is_empty() {
                body["tools"] = serde_json::json!(AnthropicProvider::convert_tools(&tools));
            }
            if let Some(ref eb) = extra_body {
                if let Value::Object(map) = eb {
                    for (k, v) in map {
                        body[k] = v.clone();
                    }
                }
            }

            let url = format!("{}/v1/messages", provider.base_url.trim_end_matches('/'));

            let resp = match provider.send_with_dead_connection_recovery(&url, &api_key, &body).await {
                Ok(r) => r,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };

            provider.update_rate_limit(resp.headers());

            if !resp.status().is_success() {
                let status = resp.status();
                let body_text = resp.text().await.unwrap_or_else(|_| "<no body>".to_string());
                yield Err(AgentError::LlmApi(format!("API error {status}: {body_text}")));
                return;
            }

            let mut byte_stream = resp.bytes_stream();
            let mut buffer = String::new();
            // Track current tool_use block index for delta accumulation
            let mut current_tool_index: u32 = 0;
            // Usage merged across message_start (input + cache) and
            // message_delta (output) events for this stream.
            let mut stream_usage = AnthropicStreamUsage::default();

            while let Some(chunk_result) = byte_stream.next().await {
                let chunk_bytes = match chunk_result {
                    Ok(b) => b,
                    Err(e) => {
                        yield Err(AgentError::LlmApi(format!("Stream read error: {e}")));
                        return;
                    }
                };

                buffer.push_str(&String::from_utf8_lossy(&chunk_bytes));

                while let Some(event_end) = buffer.find("\n\n") {
                    let event_block = buffer[..event_end].to_string();
                    buffer = buffer[event_end + 2..].to_string();

                    match process_anthropic_event_block(
                        &event_block,
                        &mut stream_usage,
                        &mut current_tool_index,
                    ) {
                        AnthropicSseOutcome::Chunk(chunk) => {
                            yield Ok(chunk);
                        }
                        AnthropicSseOutcome::Done => return,
                        AnthropicSseOutcome::Ignore => {}
                        AnthropicSseOutcome::Error(e) => {
                            yield Err(e);
                            return;
                        }
                    }
                }
            }
            // A final event block without a trailing blank line is still
            // processed. A stream that ends without `message_stop` is treated
            // as complete, not as an error.
            if !buffer.trim().is_empty() {
                match process_anthropic_event_block(
                    &buffer,
                    &mut stream_usage,
                    &mut current_tool_index,
                ) {
                    AnthropicSseOutcome::Chunk(chunk) => {
                        yield Ok(chunk);
                    }
                    AnthropicSseOutcome::Error(e) => {
                        yield Err(e);
                    }
                    AnthropicSseOutcome::Done | AnthropicSseOutcome::Ignore => {}
                }
            }
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// OpenRouterProvider
// ---------------------------------------------------------------------------

/// OpenRouter API provider with support for OpenRouter-specific parameters.
///
/// Adds:
/// - `HTTP-Referer` and `X-Title` headers (required by OpenRouter)
/// - Support for `transforms`, `provider` preferences, `route` in extra_body
/// - Parsing of `reasoning_details` array from responses
/// - `reasoning_content` extraction
#[derive(Debug, Clone)]
pub struct OpenRouterProvider {
    inner: GenericProvider,
    /// HTTP-Referer header value (required by OpenRouter).
    pub http_referer: Option<String>,
    /// X-Title header value (required by OpenRouter).
    pub x_title: Option<String>,
    /// Preferred provider order (e.g. ["DeepInfra", "Together"]).
    pub provider_order: Option<Vec<String>>,
}

impl OpenRouterProvider {
    /// Create a new OpenRouter provider with the given API key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            inner: GenericProvider::new("https://openrouter.ai/api/v1", api_key, "openai/gpt-4o"),
            http_referer: None,
            x_title: None,
            provider_order: None,
        }
    }

    /// Set the default model.
    pub fn with_model(self, model: impl Into<String>) -> Self {
        Self {
            inner: self.inner.with_model(model),
            ..self
        }
    }

    /// Set a custom base URL (OpenAI-compatible OpenRouter proxy).
    pub fn with_base_url(self, base_url: impl Into<String>) -> Self {
        Self {
            inner: self.inner.with_base_url(base_url),
            ..self
        }
    }

    /// Set the HTTP-Referer header (required by OpenRouter).
    pub fn with_http_referer(mut self, referer: impl Into<String>) -> Self {
        self.http_referer = Some(referer.into());
        self
    }

    /// Set the X-Title header (required by OpenRouter).
    pub fn with_x_title(mut self, title: impl Into<String>) -> Self {
        self.x_title = Some(title.into());
        self
    }

    /// Set preferred provider order for OpenRouter routing.
    /// e.g. `vec!["DeepInfra", "Together"]`
    pub fn with_provider_order(mut self, order: Vec<String>) -> Self {
        self.provider_order = Some(order);
        self
    }

    /// Attach a credential pool for API key rotation.
    pub fn with_credential_pool(self, pool: Arc<CredentialPool>) -> Self {
        Self {
            inner: self.inner.with_credential_pool(pool),
            ..self
        }
    }

    /// Build the extra headers including OpenRouter-specific ones.
    fn build_headers(&self) -> Vec<(String, String)> {
        let mut headers = self.inner.extra_headers.clone();
        if let Some(ref referer) = self.http_referer {
            headers.push(("HTTP-Referer".to_string(), referer.clone()));
        }
        if let Some(ref title) = self.x_title {
            headers.push(("X-Title".to_string(), title.clone()));
        }
        headers
    }

    /// Merge OpenRouter-specific parameters into extra_body.
    fn merge_extra_body(extra_body: Option<&Value>) -> Option<Value> {
        let mut merged = extra_body.cloned()?;
        // Internal compatibility tuning flag; don't forward unknown key upstream.
        if let Some(obj) = merged.as_object_mut() {
            obj.remove("openrouter_max_tools");
        }
        Some(merged)
    }

    /// Parse an OpenRouter response, extracting reasoning_details if present.
    fn parse_openrouter_response(json: &Value) -> Result<LlmResponse, AgentError> {
        let mut response = parse_openai_response(json)?;

        // Extract reasoning_content from various locations
        if let Some(reasoning) = crate::reasoning::parse_reasoning(json) {
            response.message.reasoning_content = Some(reasoning);
        }

        Ok(response)
    }
}

#[async_trait]
impl LlmProvider for OpenRouterProvider {
    async fn chat_completion(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra_body: Option<&Value>,
    ) -> Result<LlmResponse, AgentError> {
        // Build a provider clone with OpenRouter headers
        let mut provider = self.inner.clone();
        provider.extra_headers = self.build_headers();

        let merged_extra = Self::merge_extra_body(extra_body);

        // Use GenericProvider for the actual request
        let effective_model = model.unwrap_or(&self.inner.model);
        provider.check_rate_limit().await;

        let api_key = provider.effective_api_key();
        let strict_tool_sanitize =
            GenericProvider::should_sanitize_tool_calls(merged_extra.as_ref());
        let api_messages =
            GenericProvider::sanitize_messages_for_strict_api(messages, strict_tool_sanitize);

        let mut body = serde_json::json!({
            "model": effective_model,
            "messages": api_messages,
        });

        apply_max_tokens(&mut body, effective_model, max_tokens);
        if let Some(temp) = temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if !tools.is_empty() {
            body["tools"] = provider.normalize_tools_payload(tools, merged_extra.as_ref());
        }
        if let Some(ref eb) = merged_extra {
            if let Value::Object(map) = eb {
                for (k, v) in map {
                    body[k] = v.clone();
                }
            }
        }

        // Inject OpenRouter provider preferences
        if let Some(ref order) = self.provider_order {
            if !order.is_empty() {
                body["provider"] = serde_json::json!({
                    "order": order,
                    "allow_fallbacks": false,
                });
            }
        }

        let url = format!(
            "{}/chat/completions",
            provider.base_url.trim_end_matches('/')
        );

        let resp = provider
            .send_with_dead_connection_recovery(&url, &api_key, &body)
            .await?;

        provider.update_rate_limit(resp.headers());

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            return Err(AgentError::LlmApi(format!(
                "API error {status}: {body_text}"
            )));
        }

        let resp_json: Value = resp
            .json()
            .await
            .map_err(|e| AgentError::LlmApi(format!("Failed to parse response: {e}")))?;

        Self::parse_openrouter_response(&resp_json)
    }

    fn chat_completion_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra_body: Option<&Value>,
    ) -> BoxStream<'static, Result<StreamChunk, AgentError>> {
        // Use GenericProvider's streaming with OpenRouter headers
        let mut provider = self.inner.clone();
        provider.extra_headers = self.build_headers();
        let merged_extra = Self::merge_extra_body(extra_body);

        // Inject provider order into extra_body for streaming
        let final_extra = if let Some(ref order) = self.provider_order {
            if !order.is_empty() {
                let mut eb = merged_extra.unwrap_or_else(|| serde_json::json!({}));
                eb["provider"] = serde_json::json!({
                    "order": order,
                    "allow_fallbacks": false,
                });
                Some(eb)
            } else {
                merged_extra
            }
        } else {
            merged_extra
        };

        provider.chat_completion_stream(
            messages,
            tools,
            max_tokens,
            temperature,
            model,
            final_extra.as_ref(),
        )
    }
}

// ---------------------------------------------------------------------------
// SSE chunk parsing helpers
// ---------------------------------------------------------------------------

/// Deduplicate tool definitions by name; the first occurrence wins.
///
/// Parity with hermes-rs `_dedupe_tool_names` (openai.rs `convert_tools`):
/// duplicate names are dropped with a warning, and empty names are exempt
/// from dedup (kept as-is, like the reference).
pub(crate) fn dedupe_tools_by_name(tools: &[ToolSchema]) -> Vec<&ToolSchema> {
    let mut seen = std::collections::HashSet::new();
    tools
        .iter()
        .filter(|t| {
            if t.name.is_empty() {
                return true;
            }
            if !seen.insert(t.name.clone()) {
                tracing::warn!("duplicate tool name '{}' dropped (first wins)", t.name);
                return false;
            }
            true
        })
        .collect()
}

/// Detect provider error shapes in an SSE data payload.
///
/// Adapted from hermes-rs `stream_error_shape` (openai.rs): some providers
/// deliver errors as in-stream `data:` payloads instead of HTTP errors —
/// `{"error": ...}`, DeepInfra-style `error_type`/`error_message`, or a
/// generic `message` + `code` shape. These must surface as provider errors,
/// never be silently swallowed.
pub(crate) fn sse_error_shape(json: &Value) -> Option<String> {
    if let Some(err) = json.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .or_else(|| err.as_str())
            .unwrap_or("provider stream error");
        let code = err.get("code").and_then(|c| c.as_str()).unwrap_or("");
        return Some(if code.is_empty() {
            msg.to_string()
        } else {
            format!("{code}: {msg}")
        });
    }
    // DeepInfra: choices=null + error_type/error_message.
    let err_type = json.get("error_type").and_then(|s| s.as_str());
    let err_msg = json.get("error_message").and_then(|s| s.as_str());
    if err_type.is_some() || err_msg.is_some() {
        return Some(format!(
            "{}: {}",
            err_type.unwrap_or("provider_in_stream_error"),
            err_msg.unwrap_or("")
        ));
    }
    // Generic shape: `message` + `code`/`error_code`.
    if let Some(msg) = json.get("message").and_then(|m| m.as_str()) {
        if json.get("code").is_some() || json.get("error_code").is_some() {
            return Some(msg.to_string());
        }
    }
    None
}

/// Outcome of processing one raw SSE line. Pure (no I/O) so canned
/// streams can be unit-tested without HTTP.
#[derive(Debug, PartialEq)]
pub(crate) enum SseLineAction {
    /// A chunk to yield downstream.
    Emit(StreamChunk),
    /// `data: [DONE]`: the stream ended normally.
    Done,
    /// Not a relevant event (comment, blank line, non-SSE line) or a
    /// malformed payload: skip it — never abort the stream for these.
    Ignore,
    /// Provider error shape in the payload: abort the stream with this error.
    Error(String),
}

/// Process one raw SSE line into an action.
///
/// - `data: [DONE]` ends the stream.
/// - Malformed `data:` payloads (non-JSON, empty) are skipped, not fatal.
/// - `{"error": ...}` payloads (and DeepInfra-style `error_type` /
///   `error_message` shapes) abort the stream as provider errors.
/// - Only `choices[0]` is read; extra choices are ignored.
pub(crate) fn handle_sse_line(line: &str) -> SseLineAction {
    let line = line.trim();
    let data = match line.strip_prefix("data:") {
        Some(d) => d.trim(),
        None => return SseLineAction::Ignore,
    };
    if data == "[DONE]" {
        return SseLineAction::Done;
    }
    let json: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return SseLineAction::Ignore,
    };
    if let Some(msg) = sse_error_shape(&json) {
        return SseLineAction::Error(msg);
    }
    match parse_sse_chunk(&json) {
        Some(chunk) => SseLineAction::Emit(chunk),
        None => SseLineAction::Ignore,
    }
}

/// Parse a single SSE data JSON object into a StreamChunk (OpenAI format).
pub(crate) fn parse_sse_chunk(json: &Value) -> Option<StreamChunk> {
    let choices = json.get("choices").and_then(|c| c.as_array())?;
    let choice = choices.first()?;

    let delta_obj = choice.get("delta")?;

    let content = delta_obj
        .get("content")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string());
    let reasoning = delta_obj
        .get("reasoning_content")
        .or_else(|| delta_obj.get("reasoning"))
        .and_then(|r| r.as_str())
        .map(|s| s.to_string());

    let tool_calls = delta_obj
        .get("tool_calls")
        .and_then(|tc| tc.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|tc| {
                    let index = tc.get("index").and_then(|i| i.as_u64())? as u32;
                    let id = tc.get("id").and_then(|i| i.as_str()).map(|s| s.to_string());
                    let function = tc.get("function").map(|f| FunctionCallDelta {
                        name: f
                            .get("name")
                            .and_then(|n| n.as_str())
                            .map(|s| s.to_string()),
                        arguments: f
                            .get("arguments")
                            .and_then(|a| a.as_str())
                            .map(|s| s.to_string()),
                    });
                    Some(ToolCallDelta {
                        index,
                        id,
                        function,
                    })
                })
                .collect::<Vec<_>>()
        });

    let delta = if content.is_some() || tool_calls.is_some() || reasoning.is_some() {
        Some(StreamDelta {
            content,
            tool_calls,
            extra: reasoning.map(|t| serde_json::json!({ "thinking": t })),
        })
    } else {
        None
    };

    let finish_reason = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .map(|s| s.to_string());

    // Usage may appear in the final chunk
    let usage = json.get("usage").map(|u| UsageStats {
        prompt_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        completion_tokens: u
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        total_tokens: u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        estimated_cost: None,
    });

    Some(StreamChunk {
        delta,
        finish_reason,
        usage,
    })
}

// ---------------------------------------------------------------------------
// Response parsing helpers
// ---------------------------------------------------------------------------

/// Parse an OpenAI-style chat completion response.
pub(crate) fn parse_openai_response(json: &Value) -> Result<LlmResponse, AgentError> {
    let choices = json
        .get("choices")
        .and_then(|c| c.as_array())
        .ok_or_else(|| AgentError::LlmApi("No choices in response".to_string()))?;

    let choice = choices
        .first()
        .ok_or_else(|| AgentError::LlmApi("Empty choices array".to_string()))?;

    let message_obj = choice
        .get("message")
        .ok_or_else(|| AgentError::LlmApi("No message in choice".to_string()))?;

    // Parse content
    let content = message_obj
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();

    // Parse tool calls
    let tool_calls = message_obj
        .get("tool_calls")
        .and_then(|tc| tc.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|tc| {
                    let id = tc.get("id")?.as_str()?.to_string();
                    let function = tc.get("function")?;
                    let name = function.get("name")?.as_str()?.to_string();
                    let arguments = function
                        .get("arguments")
                        .and_then(|a| a.as_str())
                        .unwrap_or("{}")
                        .to_string();

                    Some(hermes_core::ToolCall {
                        id,
                        function: hermes_core::FunctionCall { name, arguments },
                    })
                })
                .collect::<Vec<_>>()
        });

    // Parse usage
    let usage = json.get("usage").and_then(|u| {
        Some(UsageStats {
            prompt_tokens: u.get("prompt_tokens")?.as_u64()?,
            completion_tokens: u.get("completion_tokens")?.as_u64()?,
            total_tokens: u.get("total_tokens")?.as_u64()?,
            estimated_cost: None,
        })
    });

    let role = message_obj
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or("assistant");

    // Extract reasoning content
    let reasoning_content = message_obj
        .get("reasoning_content")
        .and_then(|r| r.as_str())
        .map(|s| s.to_string());

    let message = Message {
        role: match role {
            "user" => MessageRole::User,
            "system" => MessageRole::System,
            "tool" => MessageRole::Tool,
            _ => MessageRole::Assistant,
        },
        content: Some(content),
        tool_calls,
        tool_call_id: None,
        name: None,
        reasoning_content,
        cache_control: None,
    };

    let finish_reason = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .map(|s| s.to_string());

    let model = json
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown")
        .to_string();

    Ok(LlmResponse {
        message,
        usage,
        model,
        finish_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_openai_response_basic() {
        let json = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Hello!"
                },
                "finish_reason": "stop"
            }],
            "model": "gpt-4o",
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        });
        let resp = parse_openai_response(&json).unwrap();
        assert_eq!(resp.message.content.as_deref(), Some("Hello!"));
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
        assert_eq!(resp.usage.as_ref().unwrap().total_tokens, 15);
    }

    #[test]
    fn test_parse_openai_response_with_tool_calls() {
        let json = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\": \"test.txt\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "model": "gpt-4o"
        });
        let resp = parse_openai_response(&json).unwrap();
        let tc = resp.message.tool_calls.as_ref().unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].function.name, "read_file");
    }

    #[test]
    fn test_parse_sse_chunk_content() {
        let json = serde_json::json!({
            "choices": [{
                "delta": {
                    "content": "Hello"
                },
                "finish_reason": null
            }]
        });
        let chunk = parse_sse_chunk(&json).unwrap();
        assert_eq!(
            chunk.delta.as_ref().unwrap().content.as_deref(),
            Some("Hello")
        );
        assert!(chunk.finish_reason.is_none());
    }

    #[test]
    fn test_parse_sse_chunk_tool_call() {
        let json = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_abc",
                        "function": {
                            "name": "search",
                            "arguments": ""
                        }
                    }]
                },
                "finish_reason": null
            }]
        });
        let chunk = parse_sse_chunk(&json).unwrap();
        let tc = chunk.delta.as_ref().unwrap().tool_calls.as_ref().unwrap();
        assert_eq!(tc[0].index, 0);
        assert_eq!(tc[0].id.as_deref(), Some("call_abc"));
    }

    #[test]
    fn test_parse_sse_chunk_finish() {
        let json = serde_json::json!({
            "choices": [{
                "delta": {},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150
            }
        });
        let chunk = parse_sse_chunk(&json).unwrap();
        assert_eq!(chunk.finish_reason.as_deref(), Some("stop"));
        assert_eq!(chunk.usage.as_ref().unwrap().total_tokens, 150);
    }

    #[test]
    fn test_anthropic_convert_messages() {
        let messages = vec![
            Message::system("You are helpful"),
            Message::user("Hello"),
            Message::assistant("Hi there!"),
        ];
        let (system, msgs) = AnthropicProvider::convert_messages(&messages);
        assert_eq!(system.as_deref(), Some("You are helpful"));
        assert_eq!(msgs.len(), 2); // user + assistant, system extracted
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
    }

    #[test]
    fn test_anthropic_convert_messages_with_tool_result() {
        let messages = vec![
            Message::system("System"),
            Message::user("Do something"),
            Message {
                role: MessageRole::Assistant,
                content: None,
                tool_calls: Some(vec![ToolCall {
                    id: "tc_1".to_string(),
                    function: FunctionCall {
                        name: "read_file".to_string(),
                        arguments: r#"{"path":"test.txt"}"#.to_string(),
                    },
                }]),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
                cache_control: None,
            },
            Message::tool_result("tc_1", "file contents here"),
        ];
        let (system, msgs) = AnthropicProvider::convert_messages(&messages);
        assert_eq!(system.as_deref(), Some("System"));
        assert_eq!(msgs.len(), 3); // user, assistant with tool_use, user with tool_result
                                   // Assistant message should have tool_use block
        let assistant_content = msgs[1]["content"].as_array().unwrap();
        assert_eq!(assistant_content[0]["type"], "tool_use");
        assert_eq!(assistant_content[0]["name"], "read_file");
        // Tool result should be a user message with tool_result block
        let tool_content = msgs[2]["content"].as_array().unwrap();
        assert_eq!(tool_content[0]["type"], "tool_result");
        assert_eq!(tool_content[0]["tool_use_id"], "tc_1");
    }

    #[test]
    fn test_anthropic_parse_response() {
        let json = serde_json::json!({
            "content": [
                {"type": "text", "text": "Here is the answer."}
            ],
            "model": "claude-3-5-sonnet-20241022",
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50
            }
        });
        let resp = AnthropicProvider::parse_response(&json).unwrap();
        assert_eq!(resp.message.content.as_deref(), Some("Here is the answer."));
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
        assert_eq!(resp.usage.as_ref().unwrap().prompt_tokens, 100);
        assert_eq!(resp.usage.as_ref().unwrap().completion_tokens, 50);
    }

    #[test]
    fn test_anthropic_parse_response_with_tool_use() {
        let json = serde_json::json!({
            "content": [
                {"type": "text", "text": "Let me read that file."},
                {
                    "type": "tool_use",
                    "id": "toolu_123",
                    "name": "read_file",
                    "input": {"path": "test.txt"}
                }
            ],
            "model": "claude-3-5-sonnet-20241022",
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 200,
                "output_tokens": 80
            }
        });
        let resp = AnthropicProvider::parse_response(&json).unwrap();
        assert_eq!(resp.finish_reason.as_deref(), Some("tool_calls"));
        let tc = resp.message.tool_calls.as_ref().unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].id, "toolu_123");
        assert_eq!(tc[0].function.name, "read_file");
    }

    #[test]
    fn test_openrouter_parse_response_with_reasoning() {
        let json = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "The answer is 42.",
                    "reasoning_content": "Let me think step by step..."
                },
                "finish_reason": "stop"
            }],
            "model": "deepseek/deepseek-r1",
            "usage": {
                "prompt_tokens": 50,
                "completion_tokens": 30,
                "total_tokens": 80
            }
        });
        let resp = OpenRouterProvider::parse_openrouter_response(&json).unwrap();
        assert_eq!(resp.message.content.as_deref(), Some("The answer is 42."));
        assert_eq!(
            resp.message.reasoning_content.as_deref(),
            Some("Let me think step by step...")
        );
    }

    #[test]
    fn test_openrouter_parse_response_with_reasoning_details() {
        let json = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Final answer.",
                    "reasoning_details": [
                        {"type": "text", "text": "Step 1"},
                        {"type": "text", "text": "Step 2"}
                    ]
                },
                "finish_reason": "stop"
            }],
            "model": "openai/o1-preview"
        });
        let resp = OpenRouterProvider::parse_openrouter_response(&json).unwrap();
        let reasoning = resp.message.reasoning_content.as_deref().unwrap();
        assert!(reasoning.contains("Step 1"));
        assert!(reasoning.contains("Step 2"));
    }

    #[test]
    fn test_openrouter_build_headers() {
        let provider = OpenRouterProvider::new("key")
            .with_http_referer("https://example.com")
            .with_x_title("My App");
        let headers = provider.build_headers();
        assert!(headers
            .iter()
            .any(|(k, v)| k == "HTTP-Referer" && v == "https://example.com"));
        assert!(headers.iter().any(|(k, v)| k == "X-Title" && v == "My App"));
    }

    #[test]
    fn test_anthropic_convert_tools() {
        let tools = vec![ToolSchema::new(
            "read_file",
            "Read a file",
            hermes_core::JsonSchema::new("object"),
        )];
        let converted = AnthropicProvider::convert_tools(&tools);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["name"], "read_file");
        assert_eq!(converted[0]["description"], "Read a file");
        assert!(converted[0].get("input_schema").is_some());
    }

    // ------------------------------------------------------------------
    // Wave 4 / Worker 4: Anthropic auth styles, beta headers, usage merge,
    // SSE minors. All payloads canned — no network.
    // ------------------------------------------------------------------

    /// Env vars are process-global; serialize tests that touch them.
    static ANTHROPIC_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_anthropic_env(vars: &[(&str, &str)], f: impl FnOnce()) {
        let _guard = ANTHROPIC_ENV_LOCK.lock().unwrap();
        for v in ANTHROPIC_CREDENTIAL_ENV_VARS {
            std::env::remove_var(v);
        }
        for (k, val) in vars {
            std::env::set_var(k, val);
        }
        f();
        for v in ANTHROPIC_CREDENTIAL_ENV_VARS {
            std::env::remove_var(v);
        }
    }

    fn anthropic_header_map(headers: &[(String, String)]) -> std::collections::HashMap<&str, &str> {
        headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    #[test]
    fn test_anthropic_oauth_token_classification() {
        // OAuth shapes (fake values; only the shape is classified).
        for k in [
            "sk-ant-oat01-FAKE",
            "sk-ant-FAKE-unknown-prefix",
            "eyJhbGciOiJIUzI1NiJ9.FAKE",
            "cc-FAKE",
        ] {
            assert!(is_anthropic_oauth_token(k), "{k} should be OAuth");
        }
        // Console API key families are billed per token and authenticate as
        // x-api-key — even when found in an OAuth env var.
        for k in [
            "sk-ant-api03-FAKE",
            "sk-ant-usr01-FAKE",
            "sk-ant-svc01-FAKE",
            "sk-ant-admin01-FAKE",
            "",
            "random-key-without-prefix",
        ] {
            assert!(!is_anthropic_oauth_token(k), "{k:?} should be API key");
        }
    }

    #[test]
    fn test_anthropic_auth_style_native_host_gating() {
        // Native host + OAuth-shaped token -> Bearer + identity headers.
        assert_eq!(
            anthropic_auth_style("https://api.anthropic.com", "sk-ant-oat01-FAKE"),
            AnthropicAuthStyle::OAuth
        );
        assert_eq!(
            anthropic_auth_style("https://api.anthropic.com/", "eyJhbGciOiJIUzI1NiJ9.FAKE"),
            AnthropicAuthStyle::OAuth
        );
        // Native host + Console key -> x-api-key.
        assert_eq!(
            anthropic_auth_style("https://api.anthropic.com", "sk-ant-api03-FAKE"),
            AnthropicAuthStyle::ApiKey
        );
        // Third-party endpoints NEVER get OAuth identity (they 401/403 on
        // it), even with an OAuth-shaped token.
        assert_eq!(
            anthropic_auth_style("https://proxy.example.com/anthropic", "sk-ant-oat01-FAKE"),
            AnthropicAuthStyle::ApiKey
        );
        assert_eq!(
            anthropic_auth_style("https://proxy.example.com:8443", "cc-FAKE"),
            AnthropicAuthStyle::ApiKey
        );
        assert_eq!(
            anthropic_auth_style("https://proxy.example.com", "sk-ant-api03-FAKE"),
            AnthropicAuthStyle::ApiKey
        );
    }

    #[test]
    fn test_anthropic_native_host_detection() {
        assert!(is_native_anthropic_host(""));
        assert!(is_native_anthropic_host("https://api.anthropic.com"));
        assert!(is_native_anthropic_host("https://api.anthropic.com/v1"));
        assert!(is_native_anthropic_host("https://API.ANTHROPIC.COM"));
        assert!(is_native_anthropic_host(
            "https://user:pass@api.anthropic.com/v1"
        ));
        assert!(!is_native_anthropic_host("https://proxy.example.com"));
        assert!(!is_native_anthropic_host(
            "https://api.anthropic.com.evil.example"
        ));
        assert!(!is_native_anthropic_host(
            "https://api.anthropic.com:443.evil.example"
        ));
    }

    #[test]
    fn test_anthropic_request_headers_api_key_style() {
        let headers = anthropic_request_headers(
            "https://api.anthropic.com",
            "sk-ant-api03-FAKEKEY",
            "2.1.74",
        );
        let map = anthropic_header_map(&headers);
        assert_eq!(map.get("x-api-key"), Some(&"sk-ant-api03-FAKEKEY"));
        assert!(
            !map.contains_key("authorization"),
            "API-key path must not send Authorization"
        );
        assert_eq!(
            map.get("anthropic-beta"),
            Some(&"interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14")
        );
        assert_eq!(map.get("anthropic-version"), Some(&"2023-06-01"));
        assert!(
            !map.contains_key("x-app") && !map.contains_key("user-agent"),
            "API-key path must not send Claude Code identity headers"
        );
    }

    #[test]
    fn test_anthropic_request_headers_oauth_style() {
        let headers = anthropic_request_headers(
            "https://api.anthropic.com",
            "sk-ant-oat01-FAKEKEY",
            "2.1.74",
        );
        let map = anthropic_header_map(&headers);
        assert_eq!(
            map.get("authorization"),
            Some(&"Bearer sk-ant-oat01-FAKEKEY")
        );
        assert!(
            !map.contains_key("x-api-key"),
            "OAuth path must send exactly one credential (no x-api-key)"
        );
        assert_eq!(
            map.get("user-agent"),
            Some(&"claude-code/2.1.74 (external, cli)")
        );
        assert_eq!(map.get("x-app"), Some(&"cli"));
        assert_eq!(
            map.get("anthropic-beta"),
            Some(
                &"interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14,claude-code-20250219,oauth-2025-04-20"
            )
        );
        assert_eq!(map.get("anthropic-version"), Some(&"2023-06-01"));
    }

    #[test]
    fn test_anthropic_request_headers_proxy_never_identity() {
        // OAuth-shaped token aimed at a proxy: x-api-key style, no Bearer,
        // no identity headers, common betas only.
        let headers = anthropic_request_headers(
            "https://proxy.example.com/anthropic",
            "sk-ant-oat01-FAKEKEY",
            "2.1.74",
        );
        let map = anthropic_header_map(&headers);
        assert_eq!(map.get("x-api-key"), Some(&"sk-ant-oat01-FAKEKEY"));
        assert!(!map.contains_key("authorization"));
        assert!(!map.contains_key("x-app") && !map.contains_key("user-agent"));
        assert_eq!(
            map.get("anthropic-beta"),
            Some(&"interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14")
        );
    }

    #[test]
    fn test_anthropic_normalize_base_url() {
        let cases = [
            ("https://api.anthropic.com/v1", "https://api.anthropic.com"),
            ("https://api.anthropic.com/v1/", "https://api.anthropic.com"),
            ("https://api.anthropic.com", "https://api.anthropic.com"),
            ("https://api.anthropic.com/", "https://api.anthropic.com"),
            // Not an exact "/v1" suffix: untouched.
            (
                "https://proxy.example.com/v12",
                "https://proxy.example.com/v12",
            ),
            (
                "https://proxy.example.com/apiv1",
                "https://proxy.example.com/apiv1",
            ),
            (
                "https://proxy.example.com/foo/v1",
                "https://proxy.example.com/foo",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(
                AnthropicProvider::normalize_base_url(input),
                expected,
                "input {input}"
            );
        }
        // The provider never builds a /v1/v1/messages URL.
        let p = AnthropicProvider::new("k").with_base_url("https://api.anthropic.com/v1/");
        assert_eq!(
            format!("{}/v1/messages", p.base_url),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn test_anthropic_env_keys_first_set_wins() {
        // All set -> ANTHROPIC_TOKEN wins.
        with_anthropic_env(
            &[
                ("ANTHROPIC_TOKEN", "FAKE-TOKEN"),
                ("CLAUDE_CODE_OAUTH_TOKEN", "FAKE-OAUTH"),
                ("ANTHROPIC_API_KEY", "FAKE-KEY"),
            ],
            || {
                assert_eq!(anthropic_env_keys(), vec!["FAKE-TOKEN".to_string()]);
            },
        );
        // ANTHROPIC_TOKEN empty -> CLAUDE_CODE_OAUTH_TOKEN next.
        with_anthropic_env(
            &[
                ("ANTHROPIC_TOKEN", "   "),
                ("CLAUDE_CODE_OAUTH_TOKEN", "FAKE-OAUTH"),
                ("ANTHROPIC_API_KEY", "FAKE-KEY"),
            ],
            || {
                assert_eq!(anthropic_env_keys(), vec!["FAKE-OAUTH".to_string()]);
            },
        );
        // Comma-separated keys in one var are split.
        with_anthropic_env(
            &[("ANTHROPIC_API_KEY", "sk-ant-api03-A, sk-ant-api03-B")],
            || {
                assert_eq!(
                    anthropic_env_keys(),
                    vec!["sk-ant-api03-A".to_string(), "sk-ant-api03-B".to_string()]
                );
            },
        );
        // Nothing set -> empty.
        with_anthropic_env(&[], || {
            assert!(anthropic_env_keys().is_empty());
        });
    }

    #[test]
    fn test_anthropic_sse_usage_merges_start_and_delta() {
        // Canned SSE sequence: message_start carries input_tokens (+ cache
        // counters) nested under message.usage; message_delta carries
        // output_tokens. Final usage must combine both; absent fields must
        // not clobber accumulated ones.
        let mut usage = AnthropicStreamUsage::default();
        let mut tool_index = 0u32;

        let start: Value = serde_json::from_str(
            r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-x","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"cache_creation_input_tokens":4,"cache_read_input_tokens":2}}}"#,
        )
        .unwrap();
        match handle_anthropic_sse_event("message_start", &start, &mut usage, &mut tool_index) {
            AnthropicSseOutcome::Chunk(c) => {
                let u = c.usage.as_ref().expect("usage chunk");
                assert_eq!(u.prompt_tokens, 10);
                assert_eq!(u.completion_tokens, 0);
                // Cache counters are parsed into the accumulator (surfaced
                // into UsageStats once hermes-core gains the fields).
                assert_eq!(usage.cache_creation_input_tokens, Some(4));
                assert_eq!(usage.cache_read_input_tokens, Some(2));
            }
            other => panic!("expected Chunk, got {other:?}"),
        }

        let delta: Value = serde_json::from_str(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}"#,
        )
        .unwrap();
        match handle_anthropic_sse_event("message_delta", &delta, &mut usage, &mut tool_index) {
            AnthropicSseOutcome::Chunk(c) => {
                assert_eq!(c.finish_reason.as_deref(), Some("stop"));
                let u = c.usage.as_ref().expect("usage chunk");
                assert_eq!(u.prompt_tokens, 10);
                assert_eq!(u.completion_tokens, 5);
                assert_eq!(u.total_tokens, 15);
                // Cache counters from message_start survive the merge even
                // though message_delta carries no cache fields.
                assert_eq!(usage.cache_creation_input_tokens, Some(4));
                assert_eq!(usage.cache_read_input_tokens, Some(2));
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[test]
    fn test_anthropic_sse_usage_delta_only_without_start() {
        // Regression: without message_start, only the delta's output counts.
        let mut usage = AnthropicStreamUsage::default();
        let mut tool_index = 0u32;
        let delta: Value = serde_json::from_str(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
        )
        .unwrap();
        match handle_anthropic_sse_event("message_delta", &delta, &mut usage, &mut tool_index) {
            AnthropicSseOutcome::Chunk(c) => {
                let u = c.usage.as_ref().unwrap();
                assert_eq!(u.prompt_tokens, 0);
                assert_eq!(u.completion_tokens, 7);
                assert_eq!(usage.cache_creation_input_tokens, None);
                assert_eq!(usage.cache_read_input_tokens, None);
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[test]
    fn test_anthropic_merge_usage_does_not_clobber_absent_fields() {
        let mut acc = AnthropicStreamUsage {
            input_tokens: 10,
            cache_read_input_tokens: Some(2),
            ..Default::default()
        };
        let u: Value = serde_json::from_str(r#"{"output_tokens":5}"#).unwrap();
        merge_anthropic_usage(&mut acc, &u);
        assert_eq!(acc.input_tokens, 10);
        assert_eq!(acc.output_tokens, 5);
        assert_eq!(acc.cache_read_input_tokens, Some(2));
        assert_eq!(acc.cache_creation_input_tokens, None);
    }

    #[test]
    fn test_anthropic_sse_error_event_surfaces_as_error() {
        let mut usage = AnthropicStreamUsage::default();
        let mut tool_index = 0u32;
        // Named error event.
        let v: Value = serde_json::from_str(
            r#"{"type":"error","error":{"type":"api_error","message":"boom"}}"#,
        )
        .unwrap();
        match handle_anthropic_sse_event("error", &v, &mut usage, &mut tool_index) {
            AnthropicSseOutcome::Error(AgentError::LlmApi(msg)) => {
                assert!(msg.contains("boom"), "unexpected message: {msg}")
            }
            other => panic!("expected Error, got {other:?}"),
        }
        // {"type":"error"} payload without an event name is also an error,
        // never content.
        match handle_anthropic_sse_event("", &v, &mut usage, &mut tool_index) {
            AnthropicSseOutcome::Error(_) => {}
            other => panic!("expected Error, got {other:?}"),
        }
        // process_anthropic_event_block end-to-end with a canned block.
        let block = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded\",\"message\":\"overloaded\"}}";
        match process_anthropic_event_block(block, &mut usage, &mut tool_index) {
            AnthropicSseOutcome::Error(AgentError::LlmApi(msg)) => {
                assert!(msg.contains("overloaded"), "unexpected message: {msg}")
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn test_anthropic_sse_malformed_data_is_skipped_not_fatal() {
        let mut usage = AnthropicStreamUsage::default();
        let mut tool_index = 0u32;
        // Non-JSON data: skipped, stream continues.
        assert!(matches!(
            process_anthropic_event_block(
                "event: ping\ndata: not json {{{",
                &mut usage,
                &mut tool_index
            ),
            AnthropicSseOutcome::Ignore
        ));
        // Missing space after the colon still parses.
        assert!(matches!(
            process_anthropic_event_block(
                "event:message_stop\ndata:{}",
                &mut usage,
                &mut tool_index
            ),
            AnthropicSseOutcome::Done
        ));
        // Unknown events (ping) are ignored.
        let ping: Value = serde_json::json!({"type": "ping"});
        assert!(matches!(
            handle_anthropic_sse_event("ping", &ping, &mut usage, &mut tool_index),
            AnthropicSseOutcome::Ignore
        ));
    }

    #[test]
    fn test_anthropic_parse_response_keeps_cache_fields() {
        let json = serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-x",
            "content": [{"type": "text", "text": "Hi"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 12,
                "output_tokens": 3,
                "cache_creation_input_tokens": 6,
                "cache_read_input_tokens": 1
            }
        });
        let resp = AnthropicProvider::parse_response(&json).unwrap();
        let u = resp.usage.as_ref().unwrap();
        assert_eq!(u.prompt_tokens, 12);
        assert_eq!(u.completion_tokens, 3);
        assert_eq!(u.total_tokens, 15);
        // NOTE: cache_creation/cache_read counters are parsed by the stream
        // accumulator but not surfaced in UsageStats until hermes-core gains
        // the fields (see AnthropicStreamUsage).
    }

    // ------------------------------------------------------------------
    // Wave 4 / Worker 5: SSE minors + request-shape minors (OpenAI-compat)
    // ------------------------------------------------------------------

    #[test]
    fn test_sse_error_shape_variants() {
        // {"error": {"message", "code"}}
        let v = serde_json::json!({"error": {"message": "boom", "code": "bad"}});
        assert_eq!(sse_error_shape(&v).as_deref(), Some("bad: boom"));
        // {"error": "plain string"}
        let v = serde_json::json!({"error": "plain boom"});
        assert_eq!(sse_error_shape(&v).as_deref(), Some("plain boom"));
        // {"error": {"message"}} without code
        let v = serde_json::json!({"error": {"message": "no code"}});
        assert_eq!(sse_error_shape(&v).as_deref(), Some("no code"));
        // DeepInfra shape: choices=null + error_type/error_message
        let v = serde_json::json!({
            "choices": null,
            "error_type": "bad_request",
            "error_message": "nope"
        });
        assert_eq!(sse_error_shape(&v).as_deref(), Some("bad_request: nope"));
        // generic message + code shape
        let v = serde_json::json!({"message": "kaput", "code": "x"});
        assert_eq!(sse_error_shape(&v).as_deref(), Some("kaput"));
        // generic message + error_code shape
        let v = serde_json::json!({"message": "kaput2", "error_code": 500});
        assert_eq!(sse_error_shape(&v).as_deref(), Some("kaput2"));
        // normal chunk: no error
        let v = serde_json::json!({"choices": [{"delta": {"content": "hi"}}]});
        assert!(sse_error_shape(&v).is_none());
        // message without a code: not an error shape
        let v = serde_json::json!({"message": "just info"});
        assert!(sse_error_shape(&v).is_none());
    }

    #[test]
    fn test_handle_sse_line_text_delta() {
        match handle_sse_line(r#"data: {"choices":[{"delta":{"content":"halo"}}]}"#) {
            SseLineAction::Emit(chunk) => {
                assert_eq!(
                    chunk.delta.as_ref().unwrap().content.as_deref(),
                    Some("halo")
                );
            }
            other => panic!("expected Emit, got {other:?}"),
        }
    }

    #[test]
    fn test_handle_sse_line_tool_call_delta() {
        match handle_sse_line(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"exec","arguments":"{}"}}]}}]}"#,
        ) {
            SseLineAction::Emit(chunk) => {
                let tcs = chunk.delta.as_ref().unwrap().tool_calls.as_ref().unwrap();
                assert_eq!(tcs[0].id.as_deref(), Some("c1"));
            }
            other => panic!("expected Emit, got {other:?}"),
        }
    }

    #[test]
    fn test_handle_sse_line_done() {
        assert!(matches!(
            handle_sse_line("data: [DONE]"),
            SseLineAction::Done
        ));
        assert!(matches!(
            handle_sse_line("data:[DONE]"),
            SseLineAction::Done
        ));
    }

    #[test]
    fn test_handle_sse_line_malformed_is_skipped_not_fatal() {
        // Malformed chunks are tolerated: skip, don't abort the stream.
        assert!(matches!(
            handle_sse_line("data: ini bukan json"),
            SseLineAction::Ignore
        ));
        assert!(matches!(handle_sse_line("data:"), SseLineAction::Ignore));
        assert!(matches!(handle_sse_line("data: "), SseLineAction::Ignore));
        assert!(matches!(handle_sse_line(""), SseLineAction::Ignore));
        assert!(matches!(handle_sse_line(": ping"), SseLineAction::Ignore));
        assert!(matches!(
            handle_sse_line("event: message"),
            SseLineAction::Ignore
        ));
    }

    #[test]
    fn test_handle_sse_line_without_space_after_colon() {
        // `data:{...}` (no space) is valid SSE and must be parsed.
        match handle_sse_line(r#"data:{"choices":[{"delta":{"content":"x"}}]}"#) {
            SseLineAction::Emit(chunk) => {
                assert_eq!(chunk.delta.as_ref().unwrap().content.as_deref(), Some("x"));
            }
            other => panic!("expected Emit, got {other:?}"),
        }
    }

    #[test]
    fn test_handle_sse_line_error_shape_aborts() {
        for line in [
            r#"data: {"error":{"message":"boom","code":"bad"}}"#,
            r#"data: {"error":"plain boom"}"#,
            r#"data: {"choices":null,"error_type":"bad_request","error_message":"nope"}"#,
        ] {
            match handle_sse_line(line) {
                SseLineAction::Error(msg) => {
                    assert!(!msg.is_empty(), "error message must not be empty")
                }
                other => panic!("expected Error for {line}, got {other:?}"),
            }
        }
    }

    #[test]
    fn test_handle_sse_line_reads_only_first_choice() {
        match handle_sse_line(
            r#"data: {"choices":[{"delta":{"content":"satu"}},{"delta":{"content":"dua"}}]}"#,
        ) {
            SseLineAction::Emit(chunk) => {
                assert_eq!(
                    chunk.delta.as_ref().unwrap().content.as_deref(),
                    Some("satu")
                );
            }
            other => panic!("expected Emit, got {other:?}"),
        }
    }

    #[test]
    fn test_dedupe_tools_by_name() {
        let mk = |name: &str| ToolSchema::new(name, "desc", hermes_core::JsonSchema::new("object"));
        let tools = vec![mk("a"), mk("b"), mk("a"), mk(""), mk("")];
        let deduped = dedupe_tools_by_name(&tools);
        let names: Vec<&str> = deduped.iter().map(|t| t.name.as_str()).collect();
        // First occurrence wins; empty names are exempt from dedup.
        assert_eq!(names, vec!["a", "b", "", ""]);
    }

    #[test]
    fn test_normalize_tools_payload_dedups() {
        let mk = |name: &str| ToolSchema::new(name, "desc", hermes_core::JsonSchema::new("object"));
        let provider = GenericProvider::new("https://example.com/v1", "k", "m");
        let payload = provider.normalize_tools_payload(&[mk("a"), mk("b"), mk("a")], None);
        let arr = payload.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["function"]["name"], "a");
        assert_eq!(arr[1]["function"]["name"], "b");
    }

    // --- Wave 4: request-shape parity ------------------------------------

    #[test]
    fn anthropic_normalize_base_url_strips_trailing_v1() {
        assert_eq!(
            AnthropicProvider::normalize_base_url("https://api.anthropic.com/v1"),
            "https://api.anthropic.com"
        );
        assert_eq!(
            AnthropicProvider::normalize_base_url("https://api.anthropic.com/v1/"),
            "https://api.anthropic.com"
        );
        assert_eq!(
            AnthropicProvider::normalize_base_url("https://api.anthropic.com/"),
            "https://api.anthropic.com"
        );
        assert_eq!(
            AnthropicProvider::normalize_base_url("https://api.anthropic.com"),
            "https://api.anthropic.com"
        );
        // Must not strip a `/v1` that is not the trailing segment.
        assert_eq!(
            AnthropicProvider::normalize_base_url("https://proxy.local/v1/anthropic"),
            "https://proxy.local/v1/anthropic"
        );
    }

    #[test]
    fn anthropic_with_base_url_never_produces_double_v1() {
        let p = AnthropicProvider::new("k").with_base_url("https://proxy.local/v1");
        assert_eq!(p.base_url, "https://proxy.local");
        // Request URL building appends /v1/messages itself.
        let url = format!("{}/v1/messages", p.base_url.trim_end_matches('/'));
        assert_eq!(url, "https://proxy.local/v1/messages");
        assert!(!url.contains("/v1/v1/"));
    }

    #[test]
    fn model_forces_max_completion_tokens_families() {
        for m in [
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-4.1",
            "gpt-4.1-nano",
            "gpt-5",
            " GPT-5 ",
            "o1",
            "o1-mini",
            "o3",
            "o4-mini",
            "openai/gpt-4o",
            "openai:o1-preview",
        ] {
            assert!(
                model_forces_max_completion_tokens(m),
                "expected true for {m}"
            );
        }
        for m in [
            "gpt-4-turbo",
            "gpt-3.5-turbo",
            "claude-3-5-sonnet-20241022",
            "deepseek-chat",
            "gemini-2.0-flash",
            "llama-3.1-70b",
        ] {
            assert!(
                !model_forces_max_completion_tokens(m),
                "expected false for {m}"
            );
        }
    }

    #[test]
    fn max_output_ceiling_for_model_table() {
        assert_eq!(
            max_output_ceiling_for_model("claude-3-5-sonnet-20241022"),
            64_000
        );
        assert_eq!(
            max_output_ceiling_for_model("anthropic:claude-opus-4"),
            64_000
        );
        assert_eq!(max_output_ceiling_for_model("gpt-4o"), 128_000);
        assert_eq!(max_output_ceiling_for_model("gpt-4.1-mini"), 128_000);
        assert_eq!(max_output_ceiling_for_model("gemini-2.0-flash"), 65_536);
        assert_eq!(max_output_ceiling_for_model("deepseek-chat"), 64_000);
        assert_eq!(max_output_ceiling_for_model("deepseek-reasoner"), 64_000);
        assert_eq!(max_output_ceiling_for_model("some-unknown-model"), 8_192);
    }

    #[test]
    fn effective_max_tokens_clamps_to_ceiling() {
        // Explicit wins, but never exceeds the ceiling.
        assert_eq!(
            effective_max_tokens_for_model("claude-3-5-sonnet-20241022", Some(100)),
            Some(100)
        );
        assert_eq!(
            effective_max_tokens_for_model("claude-3-5-sonnet-20241022", Some(200_000)),
            Some(64_000)
        );
        // Unset means the ceiling itself.
        assert_eq!(
            effective_max_tokens_for_model("gpt-4o", None),
            Some(128_000)
        );
        assert_eq!(
            effective_max_tokens_for_model("unknown-model", None),
            Some(8_192)
        );
    }

    #[test]
    fn apply_max_tokens_picks_key_and_clamps() {
        let mut body = serde_json::json!({"model": "o1"});
        apply_max_tokens(&mut body, "o1", Some(50));
        assert_eq!(body["max_completion_tokens"], 50);
        assert!(body.get("max_tokens").is_none());

        let mut body = serde_json::json!({"model": "gpt-4o"});
        apply_max_tokens(&mut body, "gpt-4o", Some(300_000));
        assert_eq!(body["max_completion_tokens"], 128_000);
        assert!(body.get("max_tokens").is_none());

        let mut body = serde_json::json!({"model": "gpt-4-turbo"});
        apply_max_tokens(&mut body, "gpt-4-turbo", None);
        assert_eq!(body["max_tokens"], 8_192);
        assert!(body.get("max_completion_tokens").is_none());
    }
}
