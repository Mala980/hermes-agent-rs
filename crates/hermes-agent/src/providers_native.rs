//! Native LLM providers: Google Gemini, Azure OpenAI, AWS Bedrock.
//!
//! These cover APIs that are *not* OpenAI-compatible and therefore cannot go
//! through [`crate::provider::GenericProvider`]:
//!
//! - [`GeminiProvider`]: Google AI Studio `streamGenerateContent` (SSE), auth
//!   via the `x-goog-api-key` header.
//! - [`AzureOpenAiProvider`]: Azure OpenAI chat completions, auth via the
//!   `api-key` header (not Bearer) against a deployment URL.
//! - [`BedrockProvider`]: AWS Bedrock Converse API with SigV4 request signing.
//!
//! Each provider exposes `new(...)`, `with_model(...)` and an
//! `api_key_from_env()`/`from_env(...)` helper following the style of the
//! other providers in this crate.

use std::collections::HashMap;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use hermes_core::tool_schema::ToolSchema;
use hermes_core::{
    AgentError, FunctionCall, FunctionCallDelta, LlmProvider, LlmResponse, Message, MessageRole,
    StreamChunk, StreamDelta, ToolCall, ToolCallDelta, UsageStats,
};
use reqwest::Client;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Shared aggregation helper
// ---------------------------------------------------------------------------

/// Collect a stream of chunks into a single [`LlmResponse`].
///
/// Used by [`LlmProvider::chat_completion`] implementations that are built on
/// top of their own streaming path.
async fn collect_stream(
    mut stream: BoxStream<'static, Result<StreamChunk, AgentError>>,
    model: String,
) -> Result<LlmResponse, AgentError> {
    let mut text = String::new();
    let mut calls: HashMap<u32, ToolCall> = HashMap::new();
    let mut usage: Option<UsageStats> = None;
    let mut finish_reason: Option<String> = None;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if let Some(delta) = chunk.delta {
            if let Some(c) = delta.content {
                text.push_str(&c);
            }
            if let Some(tcs) = delta.tool_calls {
                for tc in tcs {
                    let entry = calls.entry(tc.index).or_insert_with(|| ToolCall {
                        id: tc.id.clone().unwrap_or_default(),
                        function: FunctionCall {
                            name: String::new(),
                            arguments: String::new(),
                        },
                    });
                    if let Some(id) = tc.id {
                        if !id.is_empty() {
                            entry.id = id;
                        }
                    }
                    if let Some(f) = tc.function {
                        if let Some(name) = f.name {
                            entry.function.name.push_str(&name);
                        }
                        if let Some(args) = f.arguments {
                            entry.function.arguments.push_str(&args);
                        }
                    }
                }
            }
        }
        if chunk.finish_reason.is_some() {
            finish_reason = chunk.finish_reason;
        }
        if chunk.usage.is_some() {
            usage = chunk.usage;
        }
    }

    let mut ordered: Vec<(u32, ToolCall)> = calls.into_iter().collect();
    ordered.sort_by_key(|(i, _)| *i);
    let tool_calls: Vec<ToolCall> = ordered.into_iter().map(|(_, tc)| tc).collect();

    Ok(LlmResponse {
        message: Message {
            role: MessageRole::Assistant,
            content: Some(text),
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            tool_call_id: None,
            name: None,
            reasoning_content: None,
            cache_control: None,
        },
        usage,
        model,
        finish_reason,
    })
}

// ---------------------------------------------------------------------------
// GeminiProvider
// ---------------------------------------------------------------------------

/// Google Gemini via the native `generativelanguage.googleapis.com` REST API.
///
/// Streams with `models/{model}:streamGenerateContent?alt=sse`, authenticated
/// with the `x-goog-api-key` header. Tool calls are mapped to Gemini
/// `functionCall`/`functionResponse` parts.
#[derive(Debug, Clone)]
pub struct GeminiProvider {
    pub api_key: String,
    pub model: String,
    pub base_url: String,
    client: Client,
}

impl GeminiProvider {
    /// Create a provider with an explicit API key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: "gemini-2.0-flash".to_string(),
            base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
            client: Client::new(),
        }
    }

    /// Look up an API key from `GEMINI_API_KEY` or `GOOGLE_API_KEY`.
    pub fn api_key_from_env() -> Option<String> {
        for var in ["GEMINI_API_KEY", "GOOGLE_API_KEY"] {
            if let Ok(k) = std::env::var(var) {
                if !k.trim().is_empty() {
                    return Some(k);
                }
            }
        }
        None
    }

    /// Set the default model.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Override the API base URL (e.g. a proxy).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    /// Convert messages to Gemini `contents` + `systemInstruction`.
    fn convert(messages: &[Message]) -> (Option<Value>, Vec<Value>) {
        let mut system_parts = Vec::new();
        let mut contents = Vec::new();
        for m in messages {
            match m.role {
                MessageRole::System => {
                    if let Some(t) = m.content.as_deref() {
                        if !t.is_empty() {
                            system_parts.push(json!({ "text": t }));
                        }
                    }
                }
                MessageRole::User => {
                    let text = m.content.as_deref().unwrap_or("");
                    contents.push(json!({ "role": "user", "parts": [{ "text": text }] }));
                }
                MessageRole::Assistant => {
                    let mut parts = Vec::new();
                    if let Some(t) = m.content.as_deref() {
                        if !t.is_empty() {
                            parts.push(json!({ "text": t }));
                        }
                    }
                    if let Some(tcs) = m.tool_calls.as_deref() {
                        for tc in tcs {
                            let args: Value =
                                serde_json::from_str(&tc.function.arguments).unwrap_or(Value::Null);
                            parts.push(json!({
                                "functionCall": { "name": tc.function.name, "args": args }
                            }));
                        }
                    }
                    if parts.is_empty() {
                        parts.push(json!({ "text": "" }));
                    }
                    contents.push(json!({ "role": "model", "parts": parts }));
                }
                MessageRole::Tool => {
                    // Tool results become functionResponse parts in a user turn.
                    let text = m.content.as_deref().unwrap_or("");
                    let name = m.name.as_deref().unwrap_or("tool");
                    contents.push(json!({
                        "role": "user",
                        "parts": [{
                            "functionResponse": {
                                "name": name,
                                "response": { "result": text },
                            }
                        }],
                    }));
                }
            }
        }
        let system = if system_parts.is_empty() {
            None
        } else {
            Some(json!({ "parts": system_parts }))
        };
        (system, contents)
    }

    /// Convert tool schemas to Gemini `tools` (`functionDeclarations`).
    fn convert_tools(tools: &[ToolSchema]) -> Vec<Value> {
        if tools.is_empty() {
            return Vec::new();
        }
        let decls: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                })
            })
            .collect();
        vec![json!({ "functionDeclarations": decls })]
    }

    fn stream_url(&self, model: &str) -> String {
        format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            self.base_url, model
        )
    }
}

#[async_trait]
impl LlmProvider for GeminiProvider {
    async fn chat_completion(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra_body: Option<&Value>,
    ) -> Result<LlmResponse, AgentError> {
        let stream = self.chat_completion_stream(
            messages,
            tools,
            max_tokens,
            temperature,
            model,
            extra_body,
        );
        let effective = model.unwrap_or(&self.model).to_string();
        collect_stream(stream, effective).await
    }

    fn chat_completion_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        _extra_body: Option<&Value>,
    ) -> BoxStream<'static, Result<StreamChunk, AgentError>> {
        let provider = self.clone();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        let model = model.unwrap_or(&provider.model).to_string();

        async_stream::stream! {
            let (system_instruction, contents) = GeminiProvider::convert(&messages);
            let mut body = json!({
                "contents": contents,
                "tools": GeminiProvider::convert_tools(&tools),
                "generationConfig": {
                    "maxOutputTokens": max_tokens,
                    "temperature": temperature,
                },
            });
            if let Some(s) = system_instruction {
                body["systemInstruction"] = s;
            }

            let url = provider.stream_url(&model);
            let resp = match provider
                .client
                .post(&url)
                .header("x-goog-api-key", &provider.api_key)
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    yield Err(AgentError::LlmApi(format!("Gemini request failed: {e}")));
                    return;
                }
            };

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                let short: String = text.chars().take(500).collect();
                yield Err(AgentError::LlmApi(format!("Gemini HTTP {status}: {short}")));
                return;
            }

            let mut byte_stream = resp.bytes_stream();
            let mut buffer = String::new();
            let mut tool_index: u32 = 0;
            let mut usage: Option<UsageStats> = None;

            while let Some(chunk_result) = byte_stream.next().await {
                let chunk_bytes = match chunk_result {
                    Ok(b) => b,
                    Err(e) => {
                        yield Err(AgentError::LlmApi(format!("Gemini stream error: {e}")));
                        return;
                    }
                };
                buffer.push_str(&String::from_utf8_lossy(&chunk_bytes));

                while let Some(pos) = buffer.find('\n') {
                    let line = buffer[..pos].trim().to_string();
                    buffer.drain(..=pos);
                    let Some(data) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let data = data.trim();
                    if data.is_empty() || data == "[DONE]" {
                        continue;
                    }
                    let v: Value = match serde_json::from_str(data) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if let Some(err) = v.get("error") {
                        yield Err(AgentError::LlmApi(format!("Gemini error: {err}")));
                        return;
                    }
                    if let Some(u) = v.get("usageMetadata") {
                        let prompt = u.get("promptTokenCount").and_then(|n| n.as_u64()).unwrap_or(0);
                        let completion = u
                            .get("candidatesTokenCount")
                            .and_then(|n| n.as_u64())
                            .unwrap_or(0);
                        usage = Some(UsageStats {
                            prompt_tokens: prompt,
                            completion_tokens: completion,
                            total_tokens: prompt + completion,
                            estimated_cost: None,
                        });
                    }
                    let parts = v
                        .pointer("/candidates/0/content/parts")
                        .and_then(|p| p.as_array())
                        .cloned()
                        .unwrap_or_default();
                    for part in parts {
                        let mut delta = StreamDelta {
                            content: None,
                            tool_calls: None,
                            extra: None,
                        };
                        let mut has = false;
                        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                            if !t.is_empty() {
                                delta.content = Some(t.to_string());
                                has = true;
                            }
                        }
                        if let Some(fc) = part.get("functionCall") {
                            let name = fc
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            let args = fc
                                .get("args")
                                .map(|a| a.to_string())
                                .unwrap_or_else(|| "{}".to_string());
                            delta.tool_calls = Some(vec![ToolCallDelta {
                                index: tool_index,
                                id: Some(format!("gemini-call-{tool_index}")),
                                function: Some(FunctionCallDelta {
                                    name: Some(name),
                                    arguments: Some(args),
                                }),
                            }]);
                            tool_index += 1;
                            has = true;
                        }
                        if has {
                            yield Ok(StreamChunk {
                                delta: Some(delta),
                                finish_reason: None,
                                usage: None,
                            });
                        }
                    }
                }
            }

            // Surface usage observed during the stream as a final chunk.
            if let Some(u) = usage {
                yield Ok(StreamChunk {
                    delta: None,
                    finish_reason: Some("stop".to_string()),
                    usage: Some(u),
                });
            }
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// AzureOpenAiProvider
// ---------------------------------------------------------------------------

/// Azure OpenAI chat completions.
///
/// Unlike the public OpenAI API, Azure authenticates with the `api-key` header
/// (not `Authorization: Bearer`) and addresses a deployment URL of the form
/// `https://{resource}.openai.azure.com/openai/deployments/{deployment}/chat/completions?api-version={ver}`.
#[derive(Debug, Clone)]
pub struct AzureOpenAiProvider {
    pub api_key: String,
    /// Full deployment URL, including the `?api-version=` query.
    pub deployment_url: String,
    pub model: String,
    client: Client,
}

impl AzureOpenAiProvider {
    /// Create a provider for an Azure OpenAI deployment.
    ///
    /// `api_version` e.g. `"2024-12-01-preview"`. Use [`Self::with_api_version`]
    /// or [`Self::api_key_from_env`] companions for env-driven setup.
    pub fn new(
        api_key: impl Into<String>,
        resource: impl Into<String>,
        deployment: impl Into<String>,
    ) -> Self {
        Self::with_api_version(api_key, resource, deployment, "2024-12-01-preview")
    }

    /// Create a provider with an explicit API version.
    pub fn with_api_version(
        api_key: impl Into<String>,
        resource: impl Into<String>,
        deployment: impl Into<String>,
        api_version: impl Into<String>,
    ) -> Self {
        let deployment = deployment.into();
        let deployment_url = format!(
            "https://{}.openai.azure.com/openai/deployments/{}/chat/completions?api-version={}",
            resource.into(),
            deployment,
            api_version.into(),
        );
        Self {
            api_key: api_key.into(),
            deployment_url,
            model: deployment.clone(),
            client: Client::new(),
        }
    }

    /// Look up an API key from `AZURE_OPENAI_API_KEY`.
    pub fn api_key_from_env() -> Option<String> {
        std::env::var("AZURE_OPENAI_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
    }

    /// Deployment resource name from `AZURE_OPENAI_RESOURCE`.
    pub fn resource_from_env() -> Option<String> {
        std::env::var("AZURE_OPENAI_RESOURCE")
            .ok()
            .filter(|s| !s.trim().is_empty())
    }

    /// Deployment name from `AZURE_OPENAI_DEPLOYMENT`.
    pub fn deployment_from_env() -> Option<String> {
        std::env::var("AZURE_OPENAI_DEPLOYMENT")
            .ok()
            .filter(|s| !s.trim().is_empty())
    }

    /// Set the default model label (Azure routes by deployment, the body model
    /// is informational).
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Convert tool schemas to the OpenAI `tools` payload.
    fn convert_tools(tools: &[ToolSchema]) -> Value {
        let arr: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        Value::Array(arr)
    }

    fn request_body(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        stream: bool,
        extra_body: Option<&Value>,
    ) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": crate::provider::GenericProvider::sanitize_messages_for_strict_api(messages, false),
            "stream": stream,
        });
        if stream {
            body["stream_options"] = json!({ "include_usage": true });
        }
        if let Some(mt) = max_tokens {
            body["max_tokens"] = json!(mt);
        }
        if let Some(t) = temperature {
            body["temperature"] = json!(t);
        }
        if !tools.is_empty() {
            body["tools"] = Self::convert_tools(tools);
        }
        if let Some(Value::Object(map)) = extra_body {
            for (k, v) in map {
                body[k] = v.clone();
            }
        }
        body
    }

    async fn post(&self, body: &Value) -> Result<reqwest::Response, AgentError> {
        self.client
            .post(&self.deployment_url)
            .header("api-key", &self.api_key)
            .json(body)
            .send()
            .await
            .map_err(|e| AgentError::LlmApi(format!("Azure OpenAI request failed: {e}")))
    }
}

#[async_trait]
impl LlmProvider for AzureOpenAiProvider {
    async fn chat_completion(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra_body: Option<&Value>,
    ) -> Result<LlmResponse, AgentError> {
        let _ = model;
        let body = self.request_body(messages, tools, max_tokens, temperature, false, extra_body);
        let resp = self.post(&body).await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(AgentError::LlmApi(format!(
                "Azure OpenAI HTTP {status}: {text}"
            )));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| AgentError::LlmApi(format!("Azure parse failed: {e}")))?;
        crate::provider::parse_openai_response(&v)
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
        let _ = model;
        let extra_body = extra_body.cloned();

        async_stream::stream! {
            let body = provider.request_body(
                &messages,
                &tools,
                max_tokens,
                temperature,
                true,
                extra_body.as_ref(),
            );
            let resp = match provider.post(&body).await {
                Ok(r) => r,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                yield Err(AgentError::LlmApi(format!(
                    "Azure OpenAI HTTP {status}: {text}"
                )));
                return;
            }

            let mut byte_stream = resp.bytes_stream();
            let mut buffer = String::new();
            while let Some(chunk_result) = byte_stream.next().await {
                let chunk_bytes = match chunk_result {
                    Ok(b) => b,
                    Err(e) => {
                        yield Err(AgentError::LlmApi(format!("Azure stream error: {e}")));
                        return;
                    }
                };
                buffer.push_str(&String::from_utf8_lossy(&chunk_bytes));
                while let Some(event_end) = buffer.find("\n\n") {
                    let event_block = buffer[..event_end].to_string();
                    buffer = buffer[event_end + 2..].to_string();
                    for line in event_block.lines() {
                        let line = line.trim();
                        if line.is_empty() || line.starts_with(':') {
                            continue;
                        }
                        if let Some(data) = line.strip_prefix("data: ") {
                            let data = data.trim();
                            if data == "[DONE]" {
                                return;
                            }
                            match serde_json::from_str::<Value>(data) {
                                Ok(json) => {
                                    if let Some(chunk) =
                                        crate::provider::parse_sse_chunk(&json)
                                    {
                                        yield Ok(chunk);
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("Azure: failed to parse SSE data: {e}");
                                }
                            }
                        }
                    }
                }
            }
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// BedrockProvider
// ---------------------------------------------------------------------------

/// AWS Bedrock via the Converse API with SigV4 request signing.
///
/// Credentials come from the standard AWS environment variables
/// (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, optional
/// `AWS_SESSION_TOKEN`, `AWS_REGION` defaulting to `us-east-1`).
///
/// Streaming is implemented as a single non-streaming Converse call whose
/// result is yielded as one chunk: Bedrock's `converse-stream` uses a binary
/// event-stream framing that is out of scope here.
#[derive(Debug, Clone)]
pub struct BedrockProvider {
    pub region: String,
    pub model: String,
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
    client: Client,
}

impl BedrockProvider {
    fn http_client() -> Client {
        Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .unwrap_or_else(|_| Client::new())
    }

    /// Build from explicit credentials.
    pub fn new(
        region: impl Into<String>,
        model: impl Into<String>,
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
    ) -> Self {
        Self {
            region: region.into(),
            model: model.into(),
            access_key: access_key.into(),
            secret_key: secret_key.into(),
            session_token: None,
            client: Self::http_client(),
        }
    }

    /// Build from the standard AWS environment variables.
    pub fn from_env(model: impl Into<String>) -> Result<Self, AgentError> {
        let access_key = std::env::var("AWS_ACCESS_KEY_ID")
            .map_err(|_| AgentError::Config("set AWS_ACCESS_KEY_ID".to_string()))?;
        let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY")
            .map_err(|_| AgentError::Config("set AWS_SECRET_ACCESS_KEY".to_string()))?;
        if access_key.trim().is_empty() || secret_key.trim().is_empty() {
            return Err(AgentError::Config(
                "AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY must not be empty".to_string(),
            ));
        }
        let region = std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".to_string());
        let session_token = std::env::var("AWS_SESSION_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        Ok(Self {
            region,
            model: model.into(),
            access_key,
            secret_key,
            session_token,
            client: Self::http_client(),
        })
    }

    /// Set the default model.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Attach a session token (temporary credentials).
    pub fn with_session_token(mut self, token: impl Into<String>) -> Self {
        let t = token.into();
        self.session_token = if t.is_empty() { None } else { Some(t) };
        self
    }

    /// Convert messages to Bedrock Converse `system` + `messages`.
    fn convert(messages: &[Message]) -> (Vec<Value>, Vec<Value>) {
        let mut system = Vec::new();
        let mut out = Vec::new();
        for m in messages {
            match m.role {
                MessageRole::System => {
                    if let Some(t) = m.content.as_deref() {
                        if !t.is_empty() {
                            system.push(json!({ "text": t }));
                        }
                    }
                }
                MessageRole::User => {
                    let text = m.content.as_deref().unwrap_or("");
                    let mut blocks = Vec::new();
                    if !text.is_empty() {
                        blocks.push(json!({ "text": text }));
                    }
                    if blocks.is_empty() {
                        blocks.push(json!({ "text": "" }));
                    }
                    out.push(json!({ "role": "user", "content": blocks }));
                }
                MessageRole::Assistant => {
                    let mut blocks = Vec::new();
                    if let Some(t) = m.content.as_deref() {
                        if !t.is_empty() {
                            blocks.push(json!({ "text": t }));
                        }
                    }
                    if let Some(tcs) = m.tool_calls.as_deref() {
                        for tc in tcs {
                            let input: Value =
                                serde_json::from_str(&tc.function.arguments).unwrap_or(json!({}));
                            blocks.push(json!({
                                "toolUse": {
                                    "toolUseId": tc.id,
                                    "name": tc.function.name,
                                    "input": input,
                                }
                            }));
                        }
                    }
                    if blocks.is_empty() {
                        blocks.push(json!({ "text": "" }));
                    }
                    out.push(json!({ "role": "assistant", "content": blocks }));
                }
                MessageRole::Tool => {
                    let text = m.content.as_deref().unwrap_or("");
                    let id = m.tool_call_id.as_deref().unwrap_or("");
                    out.push(json!({
                        "role": "user",
                        "content": [{
                            "toolResult": {
                                "toolUseId": id,
                                "content": { "text": text },
                            }
                        }],
                    }));
                }
            }
        }
        (system, out)
    }

    /// Convert tool schemas to Bedrock `toolConfig`.
    fn convert_tools(tools: &[ToolSchema]) -> Option<Value> {
        if tools.is_empty() {
            return None;
        }
        let specs: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "toolSpec": {
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": { "json": t.parameters },
                    }
                })
            })
            .collect();
        Some(json!({ "tools": specs }))
    }

    fn request_body(
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
    ) -> Value {
        let (system, converted) = Self::convert(messages);
        let mut body = json!({
            "messages": converted,
            "inferenceConfig": {
                "maxTokens": max_tokens,
                "temperature": temperature,
            },
        });
        if !system.is_empty() {
            body["system"] = Value::Array(system);
        }
        if let Some(tc) = Self::convert_tools(tools) {
            body["toolConfig"] = tc;
        }
        body
    }

    /// Non-streaming Converse call. Returns (text, tool_uses, usage).
    async fn converse(
        &self,
        model: &str,
        body: &Value,
    ) -> Result<(String, Vec<(String, String, Value)>, UsageStats), AgentError> {
        let body_str = body.to_string();
        let host = format!("bedrock-runtime.{}.amazonaws.com", self.region);
        let path = format!("/model/{model}/converse");
        let url = format!("https://{host}{path}");
        let datetime = utc_now();
        let date = datetime[..8].to_string();

        let payload_hash = hex_sha256(body_str.as_bytes());
        let mut canonical_headers =
            format!("content-type:application/json\nhost:{host}\nx-amz-date:{datetime}\n");
        let mut signed_headers = "content-type;host;x-amz-date".to_string();
        if let Some(tok) = &self.session_token {
            canonical_headers.push_str(&format!("x-amz-security-token:{tok}\n"));
            signed_headers.push_str(";x-amz-security-token");
        }
        let canonical_request =
            format!("POST\n{path}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
        let scope = format!("{date}/{}/bedrock/aws4_request", self.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{datetime}\n{scope}\n{}",
            hex_sha256(canonical_request.as_bytes())
        );
        let signing_key = sig_key(&self.secret_key, &date, &self.region);
        let signature = hex_hmac(&signing_key, string_to_sign.as_bytes());
        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.access_key
        );

        let mut req = self
            .client
            .post(&url)
            .header("x-amz-date", &datetime)
            .header("authorization", auth)
            .header("content-type", "application/json")
            .body(body_str);
        if let Some(tok) = &self.session_token {
            req = req.header("x-amz-security-token", tok);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| AgentError::LlmApi(format!("Bedrock request failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let short: String = text.chars().take(600).collect();
            return Err(AgentError::LlmApi(format!(
                "Bedrock HTTP {status}: {short}"
            )));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| AgentError::LlmApi(format!("Bedrock parse failed: {e}")))?;

        let mut text = String::new();
        let mut tool_uses = Vec::new();
        for block in v
            .pointer("/output/message/content")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default()
        {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                text.push_str(t);
            }
            if let Some(tu) = block.get("toolUse") {
                tool_uses.push((
                    tu.get("toolUseId")
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string(),
                    tu.get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string(),
                    tu.get("input").cloned().unwrap_or(Value::Null),
                ));
            }
        }
        let usage = UsageStats {
            prompt_tokens: v
                .pointer("/usage/inputTokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0),
            completion_tokens: v
                .pointer("/usage/outputTokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0),
            total_tokens: 0,
            estimated_cost: None,
        };
        let usage = UsageStats {
            total_tokens: usage.prompt_tokens + usage.completion_tokens,
            ..usage
        };
        Ok((text, tool_uses, usage))
    }
}

#[async_trait]
impl LlmProvider for BedrockProvider {
    async fn chat_completion(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        _extra_body: Option<&Value>,
    ) -> Result<LlmResponse, AgentError> {
        let effective = model.unwrap_or(&self.model);
        let body = BedrockProvider::request_body(messages, tools, max_tokens, temperature);
        let (text, tool_uses, usage) = self.converse(effective, &body).await?;
        let tool_calls: Vec<ToolCall> = tool_uses
            .into_iter()
            .map(|(id, name, input)| ToolCall {
                id,
                function: FunctionCall {
                    name,
                    arguments: input.to_string(),
                },
            })
            .collect();
        Ok(LlmResponse {
            message: Message {
                role: MessageRole::Assistant,
                content: Some(text),
                tool_calls: if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls)
                },
                tool_call_id: None,
                name: None,
                reasoning_content: None,
                cache_control: None,
            },
            usage: Some(usage),
            model: effective.to_string(),
            finish_reason: None,
        })
    }

    fn chat_completion_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        _extra_body: Option<&Value>,
    ) -> BoxStream<'static, Result<StreamChunk, AgentError>> {
        // Bedrock converse-stream uses binary event-stream framing; emulate the
        // stream with a single non-streaming call yielded as one chunk.
        let provider = self.clone();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        let model = model.unwrap_or(&provider.model).to_string();

        async_stream::stream! {
            match provider
                .chat_completion(&messages, &tools, max_tokens, temperature, Some(&model), None)
                .await
            {
                Ok(resp) => {
                    let mut delta = StreamDelta {
                        content: resp.message.content.clone(),
                        tool_calls: None,
                        extra: None,
                    };
                    if let Some(tcs) = resp.message.tool_calls.as_deref() {
                        delta.tool_calls = Some(
                            tcs.iter()
                                .enumerate()
                                .map(|(i, tc)| ToolCallDelta {
                                    index: i as u32,
                                    id: Some(tc.id.clone()),
                                    function: Some(FunctionCallDelta {
                                        name: Some(tc.function.name.clone()),
                                        arguments: Some(tc.function.arguments.clone()),
                                    }),
                                })
                                .collect(),
                        );
                    }
                    yield Ok(StreamChunk {
                        delta: Some(delta),
                        finish_reason: Some("stop".to_string()),
                        usage: resp.usage,
                    });
                }
                Err(e) => {
                    yield Err(e);
                }
            }
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// SigV4 helpers (AWS Signature Version 4)
// ---------------------------------------------------------------------------

fn utc_now() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn hex_sha256(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    hex_encode(&h.finalize())
}

fn hex_hmac(key: &[u8], data: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key).expect("hmac key");
    mac.update(data);
    hex_encode(&mac.finalize().into_bytes())
}

/// Derive the SigV4 signing key: `AWS4{secret} -> date -> region -> bedrock -> aws4_request`.
fn sig_key(secret: &str, date: &str, region: &str) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    let mut mac =
        Hmac::<sha2::Sha256>::new_from_slice(format!("AWS4{secret}").as_bytes()).expect("hmac key");
    mac.update(date.as_bytes());
    let k_date = mac.finalize().into_bytes().to_vec();
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&k_date).expect("hmac key");
    mac.update(region.as_bytes());
    let k_region = mac.finalize().into_bytes().to_vec();
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&k_region).expect("hmac key");
    mac.update(b"bedrock");
    let k_service = mac.finalize().into_bytes().to_vec();
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&k_service).expect("hmac key");
    mac.update(b"aws4_request");
    mac.finalize().into_bytes().to_vec()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::tool_schema::JsonSchema;

    fn msg(role: MessageRole, content: &str) -> Message {
        Message {
            role,
            content: Some(content.to_string()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning_content: None,
            cache_control: None,
        }
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            hex_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hmac_sha256_rfc4231_test_case_1() {
        // RFC 4231 Test Case 1: key = 20x 0x0b, data = "Hi There".
        let key = vec![0x0bu8; 20];
        assert_eq!(
            hex_hmac(&key, b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn sigv4_key_derivation_is_deterministic_32_bytes() {
        let k1 = sig_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
        );
        let k2 = sig_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
        );
        assert_eq!(k1.len(), 32);
        assert_eq!(k1, k2);
    }

    #[test]
    fn gemini_converts_roles_correctly() {
        let msgs = vec![
            msg(MessageRole::System, "sys"),
            msg(MessageRole::User, "hi"),
            Message {
                role: MessageRole::Assistant,
                content: None,
                tool_calls: Some(vec![ToolCall {
                    id: "c1".to_string(),
                    function: FunctionCall {
                        name: "exec".to_string(),
                        arguments: r#"{"command":"ls"}"#.to_string(),
                    },
                }]),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
                cache_control: None,
            },
        ];
        let (system, contents) = GeminiProvider::convert(&msgs);
        assert!(system.is_some());
        assert_eq!(system.unwrap()["parts"][0]["text"], "sys");
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
        assert!(contents[1]["parts"][0].get("functionCall").is_some());
        assert_eq!(contents[1]["parts"][0]["functionCall"]["name"], "exec");
    }

    #[test]
    fn gemini_tool_message_becomes_function_response() {
        let msgs = vec![Message {
            role: MessageRole::Tool,
            content: Some("ok".to_string()),
            tool_calls: None,
            tool_call_id: Some("c1".to_string()),
            name: Some("exec".to_string()),
            reasoning_content: None,
            cache_control: None,
        }];
        let (_, contents) = GeminiProvider::convert(&msgs);
        assert_eq!(contents.len(), 1);
        let fr = &contents[0]["parts"][0]["functionResponse"];
        assert_eq!(fr["name"], "exec");
        assert_eq!(fr["response"]["result"], "ok");
    }

    #[test]
    fn gemini_converts_tool_schemas() {
        let tools = vec![ToolSchema::new(
            "read_file",
            "Read a file",
            JsonSchema::new("object"),
        )];
        let converted = GeminiProvider::convert_tools(&tools);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["functionDeclarations"][0]["name"], "read_file");
    }

    #[test]
    fn azure_builds_deployment_url() {
        let p = AzureOpenAiProvider::with_api_version("k", "myres", "mydep", "2024-10-21");
        assert_eq!(
            p.deployment_url,
            "https://myres.openai.azure.com/openai/deployments/mydep/chat/completions?api-version=2024-10-21"
        );
    }

    #[test]
    fn bedrock_converts_tool_result() {
        let msgs = vec![Message {
            role: MessageRole::Tool,
            content: Some("done".to_string()),
            tool_calls: None,
            tool_call_id: Some("tu-1".to_string()),
            name: None,
            reasoning_content: None,
            cache_control: None,
        }];
        let (_, out) = BedrockProvider::convert(&msgs);
        assert_eq!(out.len(), 1);
        let tr = &out[0]["content"][0]["toolResult"];
        assert_eq!(tr["toolUseId"], "tu-1");
        assert_eq!(tr["content"]["text"], "done");
    }
}
