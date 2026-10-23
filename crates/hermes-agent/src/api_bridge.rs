//! OpenAI Responses API (Codex) protocol implementation.
//!
//! The Responses API (`POST /v1/responses`) is used by OpenAI Codex and similar
//! models. It differs from chat completions in request/response format.

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;
use std::sync::Arc;

use hermes_core::{
    AgentError, FunctionCall, FunctionCallDelta, LlmProvider, LlmResponse, Message, MessageRole,
    StreamChunk, StreamDelta, ToolCall, ToolCallDelta, ToolSchema, UsageStats,
};

use crate::credential_pool::CredentialPool;
use crate::rate_limit::RateLimitTracker;

/// OpenAI Responses API provider for Codex models.
#[derive(Debug, Clone)]
pub struct CodexProvider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    client: Client,
    pub rate_limiter: Option<Arc<RateLimitTracker>>,
    pub credential_pool: Option<Arc<CredentialPool>>,
}

impl CodexProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: api_key.into(),
            model: "codex-mini-latest".to_string(),
            client: Client::new(),
            rate_limiter: None,
            credential_pool: None,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn with_rate_limiter(mut self, tracker: Arc<RateLimitTracker>) -> Self {
        self.rate_limiter = Some(tracker);
        self
    }

    pub fn with_credential_pool(mut self, pool: Arc<CredentialPool>) -> Self {
        self.credential_pool = Some(pool);
        self
    }

    fn effective_api_key(&self) -> String {
        if let Some(ref pool) = self.credential_pool {
            pool.get_key()
        } else {
            self.api_key.clone()
        }
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

    /// Convert internal messages to the Responses API input format.
    ///
    /// The Responses API uses a flat `input` array with items of different types:
    /// - `{ "role": "user", "content": "..." }`
    /// - `{ "role": "assistant", "content": "..." }`
    /// - `{ "type": "function_call", "name": "...", "arguments": "...", "call_id": "..." }`
    /// - `{ "type": "function_call_output", "call_id": "...", "output": "..." }`
    fn convert_input(messages: &[Message]) -> Vec<Value> {
        let mut input = Vec::new();
        for msg in messages {
            match msg.role {
                MessageRole::System => {
                    input.push(serde_json::json!({
                        "role": "system",
                        "content": msg.content.as_deref().unwrap_or("")
                    }));
                }
                MessageRole::User => {
                    input.push(serde_json::json!({
                        "role": "user",
                        "content": msg.content.as_deref().unwrap_or("")
                    }));
                }
                MessageRole::Assistant => {
                    if let Some(ref text) = msg.content {
                        if !text.is_empty() {
                            input.push(serde_json::json!({
                                "role": "assistant",
                                "content": text
                            }));
                        }
                    }
                    if let Some(ref tool_calls) = msg.tool_calls {
                        for tc in tool_calls {
                            input.push(serde_json::json!({
                                "type": "function_call",
                                "name": tc.function.name,
                                "arguments": tc.function.arguments,
                                "call_id": tc.id
                            }));
                        }
                    }
                }
                MessageRole::Tool => {
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": msg.tool_call_id.as_deref().unwrap_or(""),
                        "output": msg.content.as_deref().unwrap_or("")
                    }));
                }
            }
        }
        input
    }

    /// Convert tool schemas to Responses API function format.
    fn convert_tools(tools: &[ToolSchema]) -> Vec<Value> {
        tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters
                })
            })
            .collect()
    }

    /// Parse a Responses API response.
    fn parse_response(json: &Value) -> Result<LlmResponse, AgentError> {
        let mut content_text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        if let Some(output) = json.get("output").and_then(|o| o.as_array()) {
            for item in output {
                let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match item_type {
                    "message" => {
                        if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
                            for block in content {
                                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                    content_text.push_str(text);
                                }
                            }
                        }
                    }
                    "function_call" => {
                        let id = item
                            .get("call_id")
                            .and_then(|i| i.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = item
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();
                        let arguments = item
                            .get("arguments")
                            .and_then(|a| a.as_str())
                            .unwrap_or("{}")
                            .to_string();
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
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            }
        });

        let model = json
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown")
            .to_string();

        let stop_reason = json
            .get("status")
            .and_then(|s| s.as_str())
            .map(|s| match s {
                "completed" => "stop".to_string(),
                "incomplete" => "length".to_string(),
                other => other.to_string(),
            });

        let message = Message {
            role: MessageRole::Assistant,
            content: if content_text.is_empty() {
                None
            } else {
                Some(content_text)
            },
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
impl LlmProvider for CodexProvider {
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

        let mut body = serde_json::json!({
            "model": effective_model,
            "input": Self::convert_input(messages),
        });

        if let Some(mt) = max_tokens {
            body["max_output_tokens"] = serde_json::json!(mt);
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

        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| AgentError::LlmApi(format!("HTTP request failed: {e}")))?;

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
            let api_key = provider.effective_api_key();

            let mut body = serde_json::json!({
                "model": effective_model,
                "input": CodexProvider::convert_input(&messages),
                "stream": true,
            });

            if let Some(mt) = max_tokens {
                body["max_output_tokens"] = serde_json::json!(mt);
            }
            if let Some(temp) = temperature {
                body["temperature"] = serde_json::json!(temp);
            }
            if !tools.is_empty() {
                body["tools"] = serde_json::json!(CodexProvider::convert_tools(&tools));
            }
            if let Some(ref eb) = extra_body {
                if let Value::Object(map) = eb {
                    for (k, v) in map {
                        body[k] = v.clone();
                    }
                }
            }

            let url = format!("{}/responses", provider.base_url.trim_end_matches('/'));

            let resp = match provider.client
                .post(&url)
                .header("Authorization", format!("Bearer {}", api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    yield Err(AgentError::LlmApi(format!("HTTP request failed: {e}")));
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

                    let mut event_type = String::new();
                    let mut event_data = String::new();

                    for line in event_block.lines() {
                        let line = line.trim();
                        if let Some(et) = line.strip_prefix("event: ") {
                            event_type = et.trim().to_string();
                        } else if let Some(d) = line.strip_prefix("data: ") {
                            event_data = d.trim().to_string();
                        }
                    }

                    if event_data.is_empty() { continue; }

                    let json: Value = match serde_json::from_str(&event_data) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    match event_type.as_str() {
                        "response.output_text.delta" => {
                            if let Some(delta) = json.get("delta").and_then(|d| d.as_str()) {
                                yield Ok(StreamChunk {
                                    delta: Some(StreamDelta {
                                        content: Some(delta.to_string()),
                                        tool_calls: None,
                                        extra: None,
                                    }),
                                    finish_reason: None,
                                    usage: None,
                                });
                            }
                        }
                        "response.function_call_arguments.delta" => {
                            let call_id = json.get("call_id").and_then(|i| i.as_str()).map(|s| s.to_string());
                            let name = json.get("name").and_then(|n| n.as_str()).map(|s| s.to_string());
                            let args_delta = json.get("delta").and_then(|d| d.as_str()).map(|s| s.to_string());
                            yield Ok(StreamChunk {
                                delta: Some(StreamDelta {
                                    content: None,
                                    tool_calls: Some(vec![ToolCallDelta {
                                        index: 0,
                                        id: call_id,
                                        function: Some(FunctionCallDelta {
                                            name,
                                            arguments: args_delta,
                                        }),
                                    }]),
                                    extra: None,
                                }),
                                finish_reason: None,
                                usage: None,
                            });
                        }
                        "response.completed" => {
                            let usage = json.get("response").and_then(|r| r.get("usage")).map(|u| {
                                let input = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                                let output = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                                UsageStats {
                                    prompt_tokens: input,
                                    completion_tokens: output,
                                    total_tokens: input + output,
                                    estimated_cost: None,
                                    cache_read_input_tokens: None,
                                    cache_creation_input_tokens: None,
                                }
                            });
                            yield Ok(StreamChunk {
                                delta: None,
                                finish_reason: Some("stop".to_string()),
                                usage,
                            });
                            return;
                        }
                        _ => {}
                    }
                }
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn test_convert_input_basic() {
        let messages = vec![
            Message::system("You are helpful"),
            Message::user("Hello"),
            Message::assistant("Hi!"),
        ];
        let input = CodexProvider::convert_input(&messages);
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["role"], "system");
        assert_eq!(input[1]["role"], "user");
        assert_eq!(input[2]["role"], "assistant");
    }

    #[test]
    fn test_convert_input_with_tool_calls() {
        let messages = vec![
            Message::user("Read file"),
            Message {
                role: MessageRole::Assistant,
                content: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
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
            Message::tool_result("call_1", "file contents"),
        ];
        let input = CodexProvider::convert_input(&messages);
        assert_eq!(input.len(), 3);
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["name"], "read_file");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_1");
    }

    #[test]
    fn test_parse_response_text() {
        let json = serde_json::json!({
            "output": [
                {
                    "type": "message",
                    "content": [{"type": "output_text", "text": "Hello!"}]
                }
            ],
            "model": "codex-mini-latest",
            "status": "completed",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let resp = CodexProvider::parse_response(&json).unwrap();
        assert_eq!(resp.message.content.as_deref(), Some("Hello!"));
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn test_parse_response_tool_call() {
        let json = serde_json::json!({
            "output": [
                {
                    "type": "function_call",
                    "call_id": "fc_1",
                    "name": "read_file",
                    "arguments": "{\"path\":\"test.txt\"}"
                }
            ],
            "model": "codex-mini-latest",
            "status": "completed"
        });
        let resp = CodexProvider::parse_response(&json).unwrap();
        let tc = resp.message.tool_calls.as_ref().unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].id, "fc_1");
        assert_eq!(tc[0].function.name, "read_file");
    }

    #[test]
    fn test_builder_defaults_and_overrides() {
        let p = CodexProvider::new("k");
        assert_eq!(p.base_url, "https://api.openai.com/v1");
        assert_eq!(p.model, "codex-mini-latest");
        assert_eq!(p.api_key, "k");
        let p = p
            .with_model("gpt-5-codex")
            .with_base_url("https://proxy.example/v1/");
        assert_eq!(p.model, "gpt-5-codex");
        assert_eq!(p.base_url, "https://proxy.example/v1/");
    }

    // --- Mock-HTTP tests (no real tokens; local TCP loopback server) ---

    /// Index just past the `\r\n\r\n` header terminator, if present.
    fn find_headers_end(request: &[u8]) -> Option<usize> {
        request
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|pos| pos + 4)
    }

    /// Spin up a one-shot HTTP server on 127.0.0.1 that captures the raw
    /// request and replies with `response`. Returns the base URL to point
    /// the provider at plus a receiver for the captured request bytes.
    async fn serve_once(response: String) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request: Vec<u8> = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = socket.read(&mut buf).await.expect("read request");
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
                if let Some(headers_end) = find_headers_end(&request) {
                    let headers = String::from_utf8_lossy(&request[..headers_end]);
                    let content_len = headers
                        .lines()
                        .find_map(|l| {
                            let (name, value) = l.split_once(':')?;
                            if name.trim().eq_ignore_ascii_case("content-length") {
                                value.trim().parse::<usize>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    if request.len() >= headers_end + content_len {
                        break;
                    }
                }
                if request.len() > 4 * 1024 * 1024 {
                    break;
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&request).into_owned());
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
            // Give the client a moment to read before the socket drops.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        });
        (base_url, rx)
    }

    fn http_ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    }

    #[tokio::test]
    async fn test_chat_completion_against_mock_server() {
        let body = serde_json::json!({
            "id": "resp_123",
            "object": "response",
            "model": "codex-mini-latest",
            "status": "completed",
            "output": [
                {"type": "message",
                 "content": [{"type": "output_text", "text": "Hello from codex"}]},
                {"type": "function_call", "call_id": "call_1",
                 "name": "read_file", "arguments": "{\"path\":\"a.txt\"}"}
            ],
            "usage": {"input_tokens": 12, "output_tokens": 7}
        })
        .to_string();
        let (base_url, rx) = serve_once(http_ok(&body)).await;

        // Trailing slash must not produce a double slash in the path.
        let provider = CodexProvider::new("test-key")
            .with_base_url(format!("{base_url}/"))
            .with_model("test-model");

        let resp = provider
            .chat_completion(
                &[Message::user("Hi")],
                &[],
                Some(100),
                Some(0.5),
                None,
                None,
            )
            .await
            .expect("mock chat completion should succeed");

        assert_eq!(resp.message.content.as_deref(), Some("Hello from codex"));
        let tool_calls = resp.message.tool_calls.as_ref().expect("tool calls");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].function.name, "read_file");
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
        assert_eq!(resp.model, "codex-mini-latest");
        let usage = resp.usage.as_ref().expect("usage");
        assert_eq!(usage.total_tokens, 19);

        let request = rx.await.expect("request captured").to_lowercase();
        assert!(
            request.starts_with("post /responses http/1.1"),
            "request line: {request}"
        );
        assert!(
            request.contains("authorization: bearer test-key"),
            "auth header: {request}"
        );
        assert!(
            request.contains("\"model\":\"test-model\""),
            "model in body: {request}"
        );
        assert!(request.contains("\"input\""), "input in body: {request}");
        assert!(
            request.contains("\"max_output_tokens\":100"),
            "max tokens in body: {request}"
        );
    }

    #[tokio::test]
    async fn test_chat_completion_surfaces_http_error() {
        let body = r#"{"error":{"message":"invalid key","type":"invalid_request_error"}}"#;
        let response = format!(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let (base_url, _rx) = serve_once(response).await;
        let provider = CodexProvider::new("bad-key").with_base_url(base_url);
        let err = provider
            .chat_completion(&[Message::user("Hi")], &[], None, None, None, None)
            .await
            .expect_err("401 should surface as an error");
        match err {
            AgentError::LlmApi(msg) => {
                assert!(msg.contains("401"), "status in error: {msg}");
                assert!(msg.contains("invalid key"), "body in error: {msg}");
            }
            other => panic!("expected AgentError::LlmApi, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_chat_completion_stream_parses_sse() {
        let sse = concat!(
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"Hello \"}\n",
            "\n",
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"world\"}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n",
            "\n",
        );
        let (base_url, rx) = serve_once(http_ok(sse)).await;
        let provider = CodexProvider::new("test-key").with_base_url(base_url);

        let mut stream =
            provider.chat_completion_stream(&[Message::user("Hi")], &[], None, None, None, None);
        let mut text = String::new();
        let mut finish_reason = None;
        let mut total_tokens = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.expect("stream chunk should be Ok");
            if let Some(delta) = chunk.delta {
                if let Some(content) = delta.content {
                    text.push_str(&content);
                }
            }
            if chunk.finish_reason.is_some() {
                finish_reason = chunk.finish_reason;
            }
            if let Some(usage) = chunk.usage {
                total_tokens = usage.total_tokens;
            }
        }
        assert_eq!(text, "Hello world");
        assert_eq!(finish_reason.as_deref(), Some("stop"));
        assert_eq!(total_tokens, 8);

        let request = rx.await.expect("request captured").to_lowercase();
        assert!(
            request.contains("\"stream\":true"),
            "stream flag in body: {request}"
        );
    }
}
