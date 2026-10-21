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
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// Default Gemini API base URL (parity `DEFAULT_GEMINI_BASE_URL`).
pub const DEFAULT_GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
/// Default model (parity with the original adapter's default).
pub const DEFAULT_GEMINI_MODEL: &str = "gemini-3.7-flash";
/// Placeholder model turn inserted between user contents that must not be
/// merged, so role alternation is preserved (parity
/// `_INTERRUPTED_RESPONSE_PLACEHOLDER`).
const INTERRUPTED_RESPONSE_PLACEHOLDER: &str =
    "[The previous response was interrupted before it completed.]";

/// True when `seg` looks like a Gemini API version segment (`v1`, `v1beta`,
/// `v1alpha2`, ...), case-insensitive.
fn is_gemini_api_version_segment(seg: &str) -> bool {
    let rest = match seg.strip_prefix('v').or_else(|| seg.strip_prefix('V')) {
        Some(r) if !r.is_empty() => r,
        _ => return false,
    };
    let mut chars = rest.chars().peekable();
    let mut saw_digit = false;
    while matches!(chars.peek(), Some(c) if c.is_ascii_digit()) {
        chars.next();
        saw_digit = true;
    }
    if !saw_digit {
        return false;
    }
    let tail: String = chars.collect();
    if tail.is_empty() {
        return true;
    }
    let lower = tail.to_lowercase();
    let tail = if let Some(t) = lower.strip_prefix("alpha") {
        t
    } else if let Some(t) = lower.strip_prefix("beta") {
        t
    } else {
        return false;
    };
    tail.chars().all(|c| c.is_ascii_digit())
}

/// Normalize a Gemini base URL: trim it, drop a trailing `/openai` suffix,
/// and ensure an API version segment is present; empty input yields the
/// default (parity `normalize_gemini_base_url`).
fn normalize_gemini_base_url(base_url: Option<&str>) -> String {
    let mut trimmed = base_url.unwrap_or("").trim().to_string();
    while trimmed.ends_with('/') {
        trimmed.pop();
    }
    if trimmed.len() >= 7 && trimmed[trimmed.len() - 7..].eq_ignore_ascii_case("/openai") {
        trimmed.truncate(trimmed.len() - 7);
        while trimmed.ends_with('/') {
            trimmed.pop();
        }
    }
    if trimmed.is_empty() {
        return DEFAULT_GEMINI_BASE_URL.to_string();
    }
    let last = trimmed.rsplit('/').next().unwrap_or("");
    if !is_gemini_api_version_segment(last) {
        trimmed.push_str("/v1beta");
    }
    trimmed
}

/// Resolve the effective base URL from an optional explicit value and an
/// optional env override (pure; no process-env access, so it is unit
/// testable). Precedence: env > explicit > default; empty values and values
/// equal to the default are ignored.
fn resolve_gemini_base_url_with(env: Option<&str>, explicit: Option<&str>) -> String {
    fn useful(v: &str) -> Option<&str> {
        let t = v.trim().trim_end_matches('/');
        if t.is_empty() || t == DEFAULT_GEMINI_BASE_URL {
            None
        } else {
            Some(t)
        }
    }
    let env_base = env.and_then(useful);
    let cfg_base = explicit.and_then(useful);
    normalize_gemini_base_url(env_base.or(cfg_base))
}

/// Resolve the effective base URL. Precedence: `GEMINI_BASE_URL` env >
/// explicit base URL > default; an env value equal to the default is
/// ignored.
fn resolve_gemini_base_url(explicit: Option<&str>) -> String {
    let env = std::env::var("GEMINI_BASE_URL").ok();
    resolve_gemini_base_url_with(env.as_deref(), explicit)
}

/// Strip aggregator prefixes (`google/`, `gemini/`) from a model id.
fn bare_gemini_model_id(model: &str) -> String {
    let name = model.trim();
    for prefix in ["google/", "gemini/"] {
        if name.len() > prefix.len() && name[..prefix.len()].eq_ignore_ascii_case(prefix) {
            let rest = name[prefix.len()..].trim();
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    name.to_string()
}

/// `(major, minor)` parsed from a model id, e.g. `gemini-3.7-flash` →
/// `(3, 7)`; `(0, 0)` for ids without a version.
fn gemini_version(model: &str) -> (u32, u32) {
    let bare = bare_gemini_model_id(model).to_lowercase();
    let rest = match bare.strip_prefix("gemini-") {
        Some(r) => r,
        None => return (0, 0),
    };
    let mut chars = rest.chars().peekable();
    let mut major = String::new();
    while let Some(c) = chars.peek() {
        if c.is_ascii_digit() {
            major.push(*c);
            chars.next();
        } else {
            break;
        }
    }
    if major.is_empty() {
        return (0, 0);
    }
    let major: u32 = major.parse().unwrap_or(0);
    let tail: String = chars.collect();
    let minor = tail
        .strip_prefix('.')
        .map(|m| {
            m.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    (major, minor)
}

/// Gemini 3+ requires an explicit `id` on `functionCall`/`functionResponse`
/// so parallel calls pair with their responses; 2.x rejects the field.
fn gemini_requires_tool_call_ids(model: &str) -> bool {
    gemini_version(model).0 >= 3
}

/// True when a JSON value looks like a JSON Schema (contains `$ref:
/// "#/..."`). Gemini resolves `$ref` pointers inside
/// `functionResponse.response` and rejects unknown ones with 400, so such
/// payloads must travel as opaque text instead.
fn looks_like_json_schema(node: &Value) -> bool {
    match node {
        Value::Object(map) => map.iter().any(|(k, v)| {
            (k == "$ref" && v.as_str().map(|s| s.starts_with("#/")).unwrap_or(false))
                || looks_like_json_schema(v)
        }),
        Value::Array(arr) => arr.iter().any(looks_like_json_schema),
        _ => false,
    }
}

/// Build `functionResponse.response`: a JSON object payload is sent as-is
/// (unless it looks like a JSON Schema — then it must travel as opaque
/// text); anything else is wrapped as `{"output": <text>}`.
fn gemini_function_response_payload(text: &str) -> Value {
    let trimmed = text.trim_start();
    let parsed = if trimmed.starts_with('{') || trimmed.starts_with('[') {
        serde_json::from_str::<Value>(text).ok()
    } else {
        None
    };
    match parsed {
        Some(obj @ Value::Object(_)) if !looks_like_json_schema(&obj) => obj,
        _ => json!({ "output": text }),
    }
}

fn gemini_same_role(a: Option<&Value>, b: &Value) -> bool {
    a.and_then(|p| p.get("role"))
        .zip(b.get("role"))
        .map(|(r1, r2)| r1 == r2)
        .unwrap_or(false)
}

fn gemini_has_function_response(content: &Value) -> bool {
    content
        .get("parts")
        .and_then(|p| p.as_array())
        .map(|parts| {
            parts
                .iter()
                .any(|part| part.get("functionResponse").is_some())
        })
        .unwrap_or(false)
}

/// Merge consecutive contents with the same role (parity
/// `_merge_alternating`): Gemini rejects multi-turn requests that break
/// user/model alternation with HTTP 400.
///
/// As in the original, a user text turn is never merged into a preceding
/// user content that carries only `functionResponse` parts (or vice versa)
/// — the model reads trailing text as a continuation of the tool result and
/// returns an empty candidate. Such pairs stay valid via an INTERRUPTED
/// placeholder model turn inserted between them.
fn gemini_merge_alternating(contents: Vec<Value>) -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::with_capacity(contents.len());
    for content in contents {
        if gemini_same_role(merged.last(), &content)
            && content.get("role").and_then(Value::as_str) == Some("user")
            && gemini_has_function_response(merged.last().unwrap())
                != gemini_has_function_response(&content)
        {
            merged.push(json!({
                "role": "model",
                "parts": [{"text": INTERRUPTED_RESPONSE_PLACEHOLDER}],
            }));
        }
        if gemini_same_role(merged.last(), &content) {
            let incoming: Vec<Value> = content
                .get("parts")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            merged.last_mut().unwrap()["parts"]
                .as_array_mut()
                .unwrap()
                .extend(incoming);
        } else {
            merged.push(content);
        }
    }
    merged
}

/// Keys accepted in a Gemini `FunctionDeclaration.parameters` schema (OpenAPI
/// subset); anything else — e.g. `$schema`, `additionalProperties` — makes
/// the whole request 400.
const GEMINI_SCHEMA_ALLOWED_KEYS: &[&str] = &[
    "type",
    "format",
    "title",
    "description",
    "nullable",
    "enum",
    "maxItems",
    "minItems",
    "properties",
    "required",
    "minProperties",
    "maxProperties",
    "minLength",
    "maxLength",
    "pattern",
    "example",
    "anyOf",
    "propertyOrdering",
    "default",
    "items",
    "minimum",
    "maximum",
];

/// Normalize a `type: [...]` array into a single `type` (or `anyOf` for real
/// unions), setting `nullable` when `null` is among the options.
fn gemini_normalize_type_array(value: &[Value], out: &mut serde_json::Map<String, Value>) {
    let has_null = value.iter().any(|t| t.as_str() == Some("null"));
    let non_null: Vec<&str> = value
        .iter()
        .filter_map(|t| t.as_str())
        .filter(|t| *t != "null")
        .collect();
    if non_null.is_empty() {
        out.insert(
            "type".to_string(),
            Value::String(if has_null { "null" } else { "object" }.to_string()),
        );
        return;
    }
    if non_null.len() == 1 {
        out.insert("type".to_string(), Value::String(non_null[0].to_string()));
    } else {
        out.insert(
            "anyOf".to_string(),
            Value::Array(non_null.iter().map(|t| json!({ "type": t })).collect()),
        );
    }
    if has_null {
        out.entry("nullable".to_string())
            .or_insert(Value::Bool(true));
    }
}

/// A Gemini-safe enum entry (must be a string), or `None` to drop it.
fn gemini_stringify_enum_value(item: &Value) -> Option<String> {
    match item {
        Value::Bool(b) => Some(if *b { "true" } else { "false" }.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// Copy of a tool parameter schema keeping only Gemini-compatible keys,
/// recursing into `properties`/`items`/`anyOf`.
fn sanitize_gemini_schema(schema: &Value) -> serde_json::Map<String, Value> {
    let mut cleaned = serde_json::Map::new();
    let obj = match schema.as_object() {
        Some(o) => o,
        None => return cleaned,
    };
    for (key, value) in obj {
        if !GEMINI_SCHEMA_ALLOWED_KEYS.contains(&key.as_str()) {
            continue;
        }
        match key.as_str() {
            "properties" => {
                if let Some(props) = value.as_object() {
                    let mut map = serde_json::Map::new();
                    for (name, sub) in props {
                        map.insert(name.clone(), Value::Object(sanitize_gemini_schema(sub)));
                    }
                    cleaned.insert("properties".to_string(), Value::Object(map));
                }
            }
            "items" => {
                cleaned.insert(
                    "items".to_string(),
                    Value::Object(sanitize_gemini_schema(value)),
                );
            }
            "anyOf" => {
                if let Some(arr) = value.as_array() {
                    cleaned.insert(
                        "anyOf".to_string(),
                        Value::Array(
                            arr.iter()
                                .filter(|i| i.is_object())
                                .map(|i| Value::Object(sanitize_gemini_schema(i)))
                                .collect(),
                        ),
                    );
                }
            }
            _ => {
                cleaned.insert(key.clone(), value.clone());
            }
        }
    }

    let type_was_array = cleaned.get("type").map(|t| t.is_array()).unwrap_or(false);
    if let Some(type_val) = cleaned.remove("type") {
        if let Some(arr) = type_val.as_array() {
            let mut derived = serde_json::Map::new();
            gemini_normalize_type_array(arr, &mut derived);
            if let Some(t) = derived.remove("type") {
                cleaned.insert("type".to_string(), t);
            }
            if let Some(any_of) = derived.remove("anyOf") {
                cleaned.insert("anyOf".to_string(), any_of);
            }
            if derived.get("nullable").and_then(|v| v.as_bool()) == Some(true) {
                cleaned.insert("nullable".to_string(), Value::Bool(true));
            }
        } else {
            cleaned.insert("type".to_string(), type_val);
        }
    }

    // Gemini requires every `enum` entry to be a string.
    if let Some(enum_val) = cleaned.get("enum").and_then(|v| v.as_array()).cloned() {
        let normalized_type = cleaned.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if type_was_array || ["integer", "number", "boolean"].contains(&normalized_type) {
            let mut seen = std::collections::HashSet::new();
            let stringified: Vec<Value> = enum_val
                .iter()
                .filter_map(gemini_stringify_enum_value)
                .filter(|s| seen.insert(s.clone()))
                .map(Value::String)
                .collect();
            if stringified.is_empty() {
                cleaned.remove("enum");
            } else {
                cleaned.insert("enum".to_string(), Value::Array(stringified));
            }
        }
    }

    // `required` may only name properties that exist (one bad schema fails
    // the entire request with 400).
    if let Some(required_val) = cleaned.get("required").and_then(|v| v.as_array()).cloned() {
        let prop_names: std::collections::HashSet<&str> = cleaned
            .get("properties")
            .and_then(|p| p.as_object())
            .map(|m| m.keys().map(|k| k.as_str()).collect())
            .unwrap_or_default();
        let valid: Vec<&str> = required_val
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|n| prop_names.contains(n))
            .collect();
        if valid.is_empty() {
            cleaned.remove("required");
        } else if valid.len() != required_val.len() {
            cleaned.insert(
                "required".to_string(),
                Value::Array(valid.into_iter().map(|s| json!(s)).collect()),
            );
        }
    }

    cleaned
}

/// Normalize tool parameters into a Gemini-valid object schema (parity
/// `sanitize_gemini_tool_parameters`).
fn sanitize_gemini_tool_parameters(parameters: &Value) -> Value {
    let cleaned = sanitize_gemini_schema(parameters);
    if cleaned.is_empty() {
        json!({ "type": "object", "properties": {} })
    } else {
        Value::Object(cleaned)
    }
}

impl GeminiProvider {
    /// Create a provider with an explicit API key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: DEFAULT_GEMINI_MODEL.to_string(),
            base_url: resolve_gemini_base_url(None),
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
    ///
    /// Precedence: `GEMINI_BASE_URL` env > this explicit value > default;
    /// an env value equal to the default is ignored. The result is
    /// normalized (an API version segment is ensured).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        let base_url = base_url.into();
        self.base_url = resolve_gemini_base_url(Some(&base_url));
        self
    }

    /// Convert messages to Gemini `contents` + `systemInstruction`.
    ///
    /// - System messages are joined into a single `systemInstruction` with
    ///   `role: "system"` (never a `role: "system"` content entry).
    /// - Tool results become `functionResponse` parts in a user turn; a
    ///   JSON object payload is sent as-is (JSON-Schema-looking payloads
    ///   travel as opaque text), anything else is wrapped as
    ///   `{"output": <text>}`.
    /// - On Gemini 3+ models, `functionCall`/`functionResponse` carry an
    ///   explicit `id` so parallel calls pair with their responses.
    /// - Consecutive same-role contents are merged so the request keeps
    ///   strict user/model alternation; non-mergeable pairs are separated
    ///   by an INTERRUPTED placeholder model turn.
    fn convert(messages: &[Message], model: &str) -> (Option<Value>, Vec<Value>) {
        let include_ids = gemini_requires_tool_call_ids(model);
        let mut system_texts: Vec<&str> = Vec::new();
        let mut contents = Vec::new();
        for m in messages {
            match m.role {
                MessageRole::System => {
                    if let Some(t) = m.content.as_deref() {
                        if !t.is_empty() {
                            system_texts.push(t);
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
                            let mut fc = serde_json::Map::new();
                            fc.insert("name".to_string(), json!(tc.function.name));
                            fc.insert("args".to_string(), args);
                            if include_ids && !tc.id.is_empty() {
                                fc.insert("id".to_string(), json!(tc.id));
                            }
                            parts.push(json!({ "functionCall": Value::Object(fc) }));
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
                    let mut fr = serde_json::Map::new();
                    fr.insert("name".to_string(), json!(name));
                    fr.insert(
                        "response".to_string(),
                        gemini_function_response_payload(text),
                    );
                    if include_ids {
                        if let Some(id) = m.tool_call_id.as_deref() {
                            if !id.is_empty() {
                                fr.insert("id".to_string(), json!(id));
                            }
                        }
                    }
                    contents.push(json!({
                        "role": "user",
                        "parts": [{ "functionResponse": Value::Object(fr) }],
                    }));
                }
            }
        }
        let joined = system_texts.join("\n");
        let system = if joined.trim().is_empty() {
            None
        } else {
            Some(json!({ "role": "system", "parts": [{ "text": joined }] }))
        };
        (system, gemini_merge_alternating(contents))
    }

    /// Convert tool schemas to Gemini `tools` (`functionDeclarations`).
    ///
    /// Nameless declarations are skipped, duplicate names are dropped, and
    /// `parameters` are sanitized to the Gemini-compatible OpenAPI subset.
    fn convert_tools(tools: &[ToolSchema]) -> Vec<Value> {
        let mut seen = std::collections::HashSet::new();
        let mut decls = Vec::new();
        for t in tools {
            if t.name.is_empty() || !seen.insert(t.name.clone()) {
                continue;
            }
            let mut decl = serde_json::Map::new();
            decl.insert("name".to_string(), json!(t.name));
            if !t.description.is_empty() {
                decl.insert("description".to_string(), json!(t.description));
            }
            let params = serde_json::to_value(&t.parameters).unwrap_or(Value::Null);
            if params.is_object() {
                decl.insert(
                    "parameters".to_string(),
                    sanitize_gemini_tool_parameters(&params),
                );
            }
            decls.push(Value::Object(decl));
        }
        if decls.is_empty() {
            Vec::new()
        } else {
            vec![json!({ "functionDeclarations": decls })]
        }
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
            let (system_instruction, contents) = GeminiProvider::convert(&messages, &model);
            // Gemini caps output via maxOutputTokens; default to the
            // per-model ceiling instead of sending null (parity with
            // hermes-rs `GEMINI_DEFAULT_MAX_OUTPUT_TOKENS`).
            let max_output_tokens =
                crate::provider::effective_max_tokens_for_model(&model, max_tokens);
            let mut body = json!({
                "contents": contents,
                "generationConfig": {
                    "maxOutputTokens": max_output_tokens,
                    "temperature": temperature,
                },
            });
            let tools = GeminiProvider::convert_tools(&tools);
            if !tools.is_empty() {
                body["tools"] = Value::Array(tools);
            }
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
    /// Full deployment URL, including the `?api-version=` query (legacy
    /// mode), or the verbatim user-supplied base URL (Foundry mode).
    pub deployment_url: String,
    pub model: String,
    /// Azure Foundry mode: `deployment_url` is a full user-supplied base URL
    /// used verbatim — no deployment path is appended and no `api-version`
    /// query param is added. Auth stays via the `api-key` header.
    pub foundry: bool,
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
            foundry: false,
            client: Client::new(),
        }
    }

    /// Create a provider for Azure Foundry: `base_url` is a full
    /// user-supplied chat-completions URL used VERBATIM — no deployment path
    /// is appended and no `api-version` query param is added (unlike regular
    /// Azure OpenAI). Auth stays via the `api-key` header, like legacy mode.
    pub fn new_foundry(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            deployment_url: base_url.into().trim_end_matches('/').to_string(),
            model: String::new(),
            foundry: true,
            client: Client::new(),
        }
    }

    /// Foundry base URL from `AZURE_FOUNDRY_BASE_URL`.
    pub fn foundry_base_url_from_env() -> Option<String> {
        std::env::var("AZURE_FOUNDRY_BASE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
    }

    /// Foundry API key from `AZURE_FOUNDRY_API_KEY`.
    pub fn foundry_api_key_from_env() -> Option<String> {
        std::env::var("AZURE_FOUNDRY_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
    }

    /// Effective chat-completions URL. In Foundry mode the configured base
    /// URL is used verbatim; in legacy mode the deployment URL (with
    /// deployment path + `api-version`) was already built at construction.
    pub fn chat_url(&self) -> &str {
        &self.deployment_url
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
    ///
    /// Duplicate tool names are deduplicated (first wins), matching
    /// hermes-rs `_dedupe_tool_names`.
    fn convert_tools(tools: &[ToolSchema]) -> Value {
        let arr: Vec<Value> = crate::provider::dedupe_tools_by_name(tools)
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
        // OpenAI-compatible: per-model ceiling + max_completion_tokens for
        // model families that reject max_tokens (hermes-rs openai.rs parity).
        crate::provider::apply_max_tokens(&mut body, &self.model, max_tokens);
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
            .post(self.chat_url())
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
                        match crate::provider::handle_sse_line(line) {
                            crate::provider::SseLineAction::Emit(chunk) => yield Ok(chunk),
                            crate::provider::SseLineAction::Done => return,
                            crate::provider::SseLineAction::Ignore => {}
                            crate::provider::SseLineAction::Error(msg) => {
                                yield Err(AgentError::LlmApi(format!(
                                    "Azure provider stream error: {msg}"
                                )));
                                return;
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
/// Credentials come from the standard AWS credential chain:
/// `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` (optional `AWS_SESSION_TOKEN`)
/// env vars, then the `[default]` profile of `~/.aws/credentials`, then
/// IMDSv2 instance metadata on EC2. Region resolves from `AWS_REGION`,
/// then `AWS_DEFAULT_REGION`, defaulting to `us-east-1`.
///
/// Streaming uses the real `converse-stream` endpoint with AWS binary
/// event-stream framing. If the stream endpoint fails at connect time
/// (4xx/5xx — e.g. the model/region does not support streaming), the
/// provider falls back to a non-streaming `converse` call replayed as
/// chunks.
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

    /// Build from the AWS credential chain:
    /// 1. `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` env vars
    ///    (optional `AWS_SESSION_TOKEN`),
    /// 2. the `[default]` profile of `~/.aws/credentials`,
    /// 3. IMDSv2 instance metadata (EC2 only, fails fast off-EC2).
    ///
    /// Region: `AWS_REGION` -> `AWS_DEFAULT_REGION` -> `us-east-1`.
    pub fn from_env(model: impl Into<String>) -> Result<Self, AgentError> {
        let env: HashMap<String, String> = std::env::vars().collect();
        let region = resolve_bedrock_region_from(&env);
        let creds = credentials_from_env_vars(&env)
            .or_else(|| credentials_from_aws_credentials_file(aws_home_dir().as_deref()))
            .or_else(|| credentials_from_imds(IMDS_BASE))
            .ok_or_else(|| {
                AgentError::Config(
                    "no AWS credentials: set AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY, \
                     add a [default] profile to ~/.aws/credentials, or run on EC2 (IMDS)"
                        .to_string(),
                )
            })?;
        Ok(Self {
            region,
            model: model.into(),
            access_key: creds.access_key,
            secret_key: creds.secret_key,
            session_token: creds.session_token,
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

    /// Placeholder sent in place of empty text: Bedrock Converse rejects
    /// text blocks that are empty or whitespace-only.
    const EMPTY_TEXT_PLACEHOLDER: &'static str = "(empty)";

    /// `text` when it has non-whitespace content, else the placeholder.
    fn safe_text(text: &str) -> &str {
        if text.trim().is_empty() {
            Self::EMPTY_TEXT_PLACEHOLDER
        } else {
            text
        }
    }

    /// Convert messages to Bedrock Converse `system` + `messages`.
    ///
    /// Converse requires strict user/assistant alternation: adjacent
    /// same-role turns are merged, and the ends are padded with a user
    /// placeholder turn when the stream would start/end with an assistant
    /// turn. `toolResult.content` is a *list* of blocks (no `status` field).
    fn convert(messages: &[Message]) -> (Vec<Value>, Vec<Value>) {
        let mut system = Vec::new();
        let mut out: Vec<Value> = Vec::new();
        // `append_turn`: merge into the last turn when it has the same role.
        let mut append_turn = |role: &str, blocks: Vec<Value>| {
            if blocks.is_empty() {
                return;
            }
            let same_role = out
                .last()
                .and_then(|t| t.get("role"))
                .and_then(|r| r.as_str())
                == Some(role);
            if same_role {
                if let Some(content) = out
                    .last_mut()
                    .and_then(|t| t.get_mut("content"))
                    .and_then(|c| c.as_array_mut())
                {
                    content.extend(blocks);
                }
                return;
            }
            out.push(json!({"role": role, "content": blocks}));
        };
        for m in messages {
            match m.role {
                MessageRole::System => {
                    // Blank/whitespace-only system text is dropped, not
                    // replaced with a placeholder.
                    if let Some(t) = m.content.as_deref() {
                        if !t.trim().is_empty() {
                            system.push(json!({ "text": t }));
                        }
                    }
                }
                MessageRole::User => {
                    let text = m.content.as_deref().unwrap_or("");
                    append_turn("user", vec![json!({ "text": Self::safe_text(text) })]);
                }
                MessageRole::Assistant => {
                    let mut blocks = Vec::new();
                    if let Some(t) = m.content.as_deref() {
                        blocks.push(json!({ "text": Self::safe_text(t) }));
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
                        blocks.push(json!({ "text": Self::EMPTY_TEXT_PLACEHOLDER }));
                    }
                    append_turn("assistant", blocks);
                }
                MessageRole::Tool => {
                    // toolResult: {"toolUseId", "content": [{"text"}]} —
                    // content is a list of blocks, with no `status` field.
                    let text = m.content.as_deref().unwrap_or("");
                    let id = m.tool_call_id.as_deref().unwrap_or("");
                    append_turn(
                        "user",
                        vec![json!({
                            "toolResult": {
                                "toolUseId": id,
                                "content": [{ "text": Self::safe_text(text) }],
                            }
                        })],
                    );
                }
            }
        }
        // Pad the ends with a user placeholder turn so the stream is
        // user-first and user-last, as Converse requires.
        if !out.is_empty() {
            let first_is_user = out
                .first()
                .and_then(|t| t.get("role"))
                .and_then(|r| r.as_str())
                == Some("user");
            if !first_is_user {
                out.insert(
                    0,
                    json!({"role": "user", "content": [{"text": Self::EMPTY_TEXT_PLACEHOLDER}]}),
                );
            }
            let last_is_user = out
                .last()
                .and_then(|t| t.get("role"))
                .and_then(|r| r.as_str())
                == Some("user");
            if !last_is_user {
                out.push(
                    json!({"role": "user", "content": [{"text": Self::EMPTY_TEXT_PLACEHOLDER}]}),
                );
            }
        }
        (system, out)
    }

    /// Convert tool schemas to Bedrock `toolConfig`.
    ///
    /// Duplicate tool names are dropped (first wins): Converse returns 400
    /// on duplicate names.
    fn convert_tools(tools: &[ToolSchema]) -> Option<Value> {
        if tools.is_empty() {
            return None;
        }
        let mut seen = std::collections::HashSet::new();
        let specs: Vec<Value> = tools
            .iter()
            .filter(|t| {
                if t.name.is_empty() {
                    return true;
                }
                if !seen.insert(t.name.clone()) {
                    tracing::warn!("dropping duplicate Bedrock tool name '{}'", t.name);
                    return false;
                }
                true
            })
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
        if specs.is_empty() {
            return None;
        }
        Some(json!({ "tools": specs }))
    }

    fn request_body(
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
    ) -> Value {
        let (system, converted) = Self::convert(messages);
        // `inferenceConfig` only carries keys that are set (never null),
        // and is omitted entirely when empty.
        let mut body = json!({ "messages": converted });
        let mut inference_config = serde_json::Map::new();
        if let Some(mt) = max_tokens {
            inference_config.insert("maxTokens".to_string(), json!(mt));
        }
        if let Some(t) = temperature {
            inference_config.insert("temperature".to_string(), json!(t));
        }
        if !inference_config.is_empty() {
            body["inferenceConfig"] = Value::Object(inference_config);
        }
        if !system.is_empty() {
            body["system"] = Value::Array(system);
        }
        if let Some(tc) = Self::convert_tools(tools) {
            body["toolConfig"] = tc;
        }
        body
    }

    /// Sign a POST to bedrock-runtime.
    ///
    /// Returns `(url, x-amz-date, authorization)`. Shared by `converse`
    /// and the `converse-stream` request.
    fn sign_post(&self, path: &str, body: &str) -> (String, String, String) {
        let host = format!("bedrock-runtime.{}.amazonaws.com", self.region);
        let url = format!("https://{host}{path}");
        let datetime = utc_now();
        let date = datetime[..8].to_string();

        let payload_hash = hex_sha256(body.as_bytes());
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
        (url, datetime, auth)
    }

    /// Build the request to the `converse-stream` endpoint.
    ///
    /// The response (including non-success statuses) is returned to the
    /// caller: a 4xx/5xx at connect time means the endpoint is not
    /// supported and the caller falls back to non-streaming `converse`.
    async fn stream_request(
        &self,
        model: &str,
        body: &Value,
    ) -> Result<reqwest::Response, AgentError> {
        let body_str = body.to_string();
        let path = format!("/model/{model}/converse-stream");
        let (url, datetime, auth) = self.sign_post(&path, &body_str);
        let mut req = self
            .client
            .post(&url)
            .header("x-amz-date", &datetime)
            .header("authorization", auth)
            .header("content-type", "application/json")
            .header("accept", "application/vnd.amazon.eventstream")
            .body(body_str);
        if let Some(tok) = &self.session_token {
            req = req.header("x-amz-security-token", tok);
        }
        req.send()
            .await
            .map_err(|e| AgentError::LlmApi(format!("Bedrock stream request failed: {e}")))
    }

    /// Non-streaming Converse call. Returns (text, tool_uses, usage).
    ///
    /// Also used as the fallback when the `converse-stream` endpoint fails
    /// at connect time (4xx/5xx).
    async fn converse(
        &self,
        model: &str,
        body: &Value,
    ) -> Result<(String, Vec<(String, String, Value)>, UsageStats), AgentError> {
        let body_str = body.to_string();
        let path = format!("/model/{model}/converse");
        let (url, datetime, auth) = self.sign_post(&path, &body_str);

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
        let usage = bedrock_usage(
            v.pointer("/usage/inputTokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0),
            v.pointer("/usage/outputTokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0),
        );
        Ok((text, tool_uses, usage))
    }
}

// ---------------------------------------------------------------------------
// Bedrock converse-stream: AWS binary event-stream parsing
// ---------------------------------------------------------------------------

/// One parsed event-stream frame: the `:message-type`/`:event-type`
/// header values plus the raw payload.
struct BedrockEventFrame {
    message_type: String,
    event_type: String,
    payload: Vec<u8>,
}

fn bedrock_frame_error() -> AgentError {
    AgentError::LlmApi("Bedrock event-stream: malformed framing".to_string())
}

fn bedrock_read_u16_at(buf: &[u8], pos: usize, limit: usize) -> Result<u16, AgentError> {
    if pos + 2 > limit || pos + 2 > buf.len() {
        return Err(bedrock_frame_error());
    }
    Ok(u16::from_be_bytes([buf[pos], buf[pos + 1]]))
}

/// Parse one frame from the start of `buf`.
///
/// AWS framing: u32 total length + u32 headers length + prelude CRC (4B) +
/// headers + payload + message CRC (4B). Both CRCs are skipped without
/// verification (transport is TLS; the parser's job is framing).
///
/// `Ok(None)` = not enough data yet, wait for the next chunk.
/// `Err` = malformed framing (inconsistent lengths / truncated header).
fn parse_bedrock_event_frame(buf: &[u8]) -> Result<Option<(BedrockEventFrame, usize)>, AgentError> {
    if buf.len() < 12 {
        return Ok(None);
    }
    let total = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let headers_len = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    if total < 16 || headers_len > total - 16 {
        return Err(bedrock_frame_error());
    }
    if buf.len() < total {
        return Ok(None);
    }
    let headers_end = 12 + headers_len;
    let mut message_type = String::new();
    let mut event_type = String::new();
    let mut pos = 12usize;
    while pos < headers_end {
        let name_len = *buf.get(pos).ok_or_else(bedrock_frame_error)? as usize;
        pos += 1;
        let name_end = pos.checked_add(name_len).ok_or_else(bedrock_frame_error)?;
        if name_end > headers_end {
            return Err(bedrock_frame_error());
        }
        let name = &buf[pos..name_end];
        pos = name_end;
        let value_type = *buf.get(pos).ok_or_else(bedrock_frame_error)?;
        pos += 1;
        match value_type {
            0 | 1 => {}        // boolean true/false: no payload
            2 => pos += 1,     // byte
            3 => pos += 2,     // short
            4 => pos += 4,     // integer
            5 | 8 => pos += 8, // long / timestamp
            9 => pos += 16,    // uuid
            6 | 7 => {
                // bytes / string: u16 length + data
                let len = bedrock_read_u16_at(buf, pos, headers_end)? as usize;
                pos += 2;
                if pos.checked_add(len).ok_or_else(bedrock_frame_error)? > headers_end {
                    return Err(bedrock_frame_error());
                }
                if value_type == 7 {
                    let s = String::from_utf8_lossy(&buf[pos..pos + len]).into_owned();
                    if name == b":message-type" {
                        message_type = s;
                    } else if name == b":event-type" {
                        event_type = s;
                    }
                }
                pos += len;
            }
            _ => return Err(bedrock_frame_error()),
        }
        if pos > headers_end {
            return Err(bedrock_frame_error());
        }
    }
    let payload_end = total - 4;
    let payload = buf[12 + headers_len..payload_end].to_vec();
    Ok(Some((
        BedrockEventFrame {
            message_type,
            event_type,
            payload,
        },
        total,
    )))
}

/// Action resulting from mapping one event-stream frame to public chunks.
enum BedrockFrameAction {
    /// Assistant text piece (`contentBlockDelta` / `delta.text`).
    Text(String),
    /// Start of a toolUse block: id + name set, arguments empty.
    ToolStart {
        index: u32,
        id: String,
        name: String,
    },
    /// Partial toolUse JSON arguments string.
    ToolInput { index: u32, delta: String },
    /// `messageStop` received; carries the stop reason.
    Stop(String),
    /// Token usage from a `metadata` event (inputTokens/outputTokens).
    Usage(UsageStats),
    /// Exception frame (`:message-type` == "exception"), e.g.
    /// `modelStreamErrorException`.
    StreamError(String),
    /// Unmapped frame (text contentBlockStart, contentBlockStop, ...).
    Ignore,
}

/// Map one frame to an action. converse-stream event payloads are JSON.
fn map_bedrock_frame(frame: &BedrockEventFrame) -> BedrockFrameAction {
    if frame.message_type == "exception" {
        let msg = serde_json::from_slice::<Value>(&frame.payload)
            .ok()
            .and_then(|v| {
                v.get("message")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| String::from_utf8_lossy(&frame.payload).into_owned());
        return BedrockFrameAction::StreamError(format!("{}: {msg}", frame.event_type));
    }
    let v: Value = match serde_json::from_slice(&frame.payload) {
        Ok(v) => v,
        Err(_) => return BedrockFrameAction::Ignore,
    };
    let index = v
        .get("contentBlockIndex")
        .and_then(|i| i.as_u64())
        .unwrap_or(0) as u32;
    match frame.event_type.as_str() {
        "contentBlockStart" => {
            if let Some(tu) = v.pointer("/start/toolUse") {
                BedrockFrameAction::ToolStart {
                    index,
                    id: tu
                        .get("toolUseId")
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string(),
                    name: tu
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string(),
                }
            } else {
                BedrockFrameAction::Ignore
            }
        }
        "contentBlockDelta" => {
            if let Some(t) = v.pointer("/delta/text").and_then(|t| t.as_str()) {
                BedrockFrameAction::Text(t.to_string())
            } else if let Some(inp) = v.pointer("/delta/toolUse/input").and_then(|i| i.as_str()) {
                BedrockFrameAction::ToolInput {
                    index,
                    delta: inp.to_string(),
                }
            } else {
                BedrockFrameAction::Ignore
            }
        }
        "messageStop" => BedrockFrameAction::Stop(
            v.get("stopReason")
                .and_then(|s| s.as_str())
                .unwrap_or("stop")
                .to_string(),
        ),
        "metadata" => BedrockFrameAction::Usage(bedrock_usage(
            v.pointer("/usage/inputTokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0),
            v.pointer("/usage/outputTokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0),
        )),
        _ => BedrockFrameAction::Ignore,
    }
}

fn bedrock_usage(prompt_tokens: u64, completion_tokens: u64) -> UsageStats {
    UsageStats {
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens + completion_tokens,
        estimated_cost: None,
    }
}

fn bedrock_text_chunk(text: String) -> StreamChunk {
    StreamChunk {
        delta: Some(StreamDelta {
            content: Some(text),
            tool_calls: None,
            extra: None,
        }),
        finish_reason: None,
        usage: None,
    }
}

fn bedrock_tool_chunk(
    index: u32,
    id: Option<String>,
    name: Option<String>,
    arguments_delta: String,
) -> StreamChunk {
    StreamChunk {
        delta: Some(StreamDelta {
            content: None,
            tool_calls: Some(vec![ToolCallDelta {
                index,
                id,
                function: Some(FunctionCallDelta {
                    name,
                    arguments: Some(arguments_delta),
                }),
            }]),
            extra: None,
        }),
        finish_reason: None,
        usage: None,
    }
}

/// Feed one raw byte chunk through the event-stream parser: parse complete
/// frames, map them to stream chunks, and accumulate state.
///
/// Kept separate from the I/O loop so it can be unit-tested with canned
/// byte frames. `pending_tool` tracks the running toolUse block
/// (contentBlockIndex, id, name); it is cleared on `contentBlockStop`.
#[allow(clippy::too_many_arguments)]
fn pump_event_stream_bytes(
    buf: &mut Vec<u8>,
    chunk: &[u8],
    pending_tool: &mut Option<(u32, String, String)>,
    usage: &mut UsageStats,
    stop_reason: &mut Option<String>,
) -> Result<Vec<StreamChunk>, AgentError> {
    let mut out = Vec::new();
    buf.extend_from_slice(chunk);
    loop {
        let (frame, used) = match parse_bedrock_event_frame(buf)? {
            None => break,
            Some(f) => f,
        };
        buf.drain(..used);
        match map_bedrock_frame(&frame) {
            BedrockFrameAction::Text(t) => out.push(bedrock_text_chunk(t)),
            BedrockFrameAction::ToolStart { index, id, name } => {
                *pending_tool = Some((index, id.clone(), name.clone()));
                out.push(bedrock_tool_chunk(
                    index,
                    Some(id),
                    Some(name),
                    String::new(),
                ));
            }
            BedrockFrameAction::ToolInput { index, delta } => {
                let (id, name) = match pending_tool {
                    Some((i, id, name)) if *i == index => (Some(id.clone()), Some(name.clone())),
                    _ => (None, None),
                };
                out.push(bedrock_tool_chunk(index, id, name, delta));
            }
            BedrockFrameAction::Stop(reason) => {
                *stop_reason = Some(reason);
            }
            BedrockFrameAction::Usage(u) => {
                *usage = u;
            }
            BedrockFrameAction::StreamError(e) => {
                return Err(AgentError::LlmApi(format!("Bedrock stream error: {e}")))
            }
            BedrockFrameAction::Ignore => {}
        }
        if frame.event_type == "contentBlockStop" {
            *pending_tool = None;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Bedrock AWS region & credential chain
// ---------------------------------------------------------------------------

/// IAM credentials produced by the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BedrockCredentials {
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
}

/// Default IMDS endpoint (EC2 link-local).
const IMDS_BASE: &str = "http://169.254.169.254";

/// TCP connect timeout for IMDS. On non-EC2 machines the link-local address
/// can blackhole SYN packets until the TCP timeout (~2 minutes); this bound
/// keeps the chain fail-fast.
const IMDS_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// Read/write timeout per IMDS request.
const IMDS_IO_TIMEOUT: Duration = Duration::from_secs(1);

/// Region chain: `AWS_REGION` -> `AWS_DEFAULT_REGION` -> `us-east-1`.
/// Empty/whitespace values are treated as unset.
fn resolve_bedrock_region_from(env: &HashMap<String, String>) -> String {
    for key in ["AWS_REGION", "AWS_DEFAULT_REGION"] {
        if let Some(v) = env.get(key).map(|s| s.trim()).filter(|s| !s.is_empty()) {
            return v.to_string();
        }
    }
    "us-east-1".to_string()
}

/// Chain level 1: explicit env pair (`AWS_ACCESS_KEY_ID` +
/// `AWS_SECRET_ACCESS_KEY`, optional `AWS_SESSION_TOKEN`).
fn credentials_from_env_vars(env: &HashMap<String, String>) -> Option<BedrockCredentials> {
    let access_key = nonempty_env_var(env, "AWS_ACCESS_KEY_ID")?;
    let secret_key = nonempty_env_var(env, "AWS_SECRET_ACCESS_KEY")?;
    let session_token = nonempty_env_var(env, "AWS_SESSION_TOKEN").map(str::to_string);
    Some(BedrockCredentials {
        access_key: access_key.to_string(),
        secret_key: secret_key.to_string(),
        session_token,
    })
}

fn nonempty_env_var<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    env.get(key).map(|s| s.trim()).filter(|s| !s.is_empty())
}

/// Chain level 2: the `[default]` profile of `~/.aws/credentials`.
fn credentials_from_aws_credentials_file(home: Option<&Path>) -> Option<BedrockCredentials> {
    let path = home?.join(".aws").join("credentials");
    let text = std::fs::read_to_string(&path).ok()?;
    parse_aws_credentials_ini(&text)
}

/// Lightweight INI parser for `~/.aws/credentials`: `[section]` headers,
/// `key = value` pairs, `#`/`;` comments and blank lines skipped, keys
/// case-insensitive. Only the `[default]` section is read; named
/// profiles/SSO are out of scope.
fn parse_aws_credentials_ini(text: &str) -> Option<BedrockCredentials> {
    let mut in_default = false;
    let mut access_key: Option<String> = None;
    let mut secret_key: Option<String> = None;
    let mut session_token: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_default = section.trim().eq_ignore_ascii_case("default");
            continue;
        }
        if !in_default {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim().to_ascii_lowercase().as_str() {
            "aws_access_key_id" => access_key = Some(value.to_string()),
            "aws_secret_access_key" => secret_key = Some(value.to_string()),
            "aws_session_token" => session_token = Some(value.to_string()),
            _ => {}
        }
    }
    Some(BedrockCredentials {
        access_key: access_key?,
        secret_key: secret_key?,
        session_token,
    })
}

/// Home directory for `~/.aws/credentials` (`HOME`, falling back to
/// `USERPROFILE` on Windows).
fn aws_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

/// Chain level 3: IMDSv2 instance metadata.
///
/// Mandatory handshake: `PUT /latest/api/token` (header
/// `X-aws-ec2-metadata-token-ttl-seconds`), then `GET
/// /latest/meta-data/iam/security-credentials/` for the role name, then
/// `GET .../security-credentials/{role}` for the JSON credentials document
/// (`AccessKeyId`, `SecretAccessKey`, `Token`).
///
/// Deliberately synchronous over `TcpStream` with a 1s connect timeout and
/// 1s I/O timeout per request (not reqwest async): `from_env` runs in a
/// synchronous context and this step must fail fast on non-EC2 machines.
fn credentials_from_imds(imds_base: &str) -> Option<BedrockCredentials> {
    let token = imds_request(imds_base, "PUT", "/latest/api/token", None)?;
    if token.trim().is_empty() {
        return None;
    }
    let role = imds_request(
        imds_base,
        "GET",
        "/latest/meta-data/iam/security-credentials/",
        Some(&token),
    )?;
    let role = role.trim();
    if role.is_empty() {
        return None;
    }
    let doc = imds_request(
        imds_base,
        "GET",
        &format!("/latest/meta-data/iam/security-credentials/{role}"),
        Some(&token),
    )?;
    let doc: Value = serde_json::from_str(&doc).ok()?;
    let access_key = doc
        .get("AccessKeyId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let secret_key = doc
        .get("SecretAccessKey")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let session_token = doc
        .get("Token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(BedrockCredentials {
        access_key: access_key.to_string(),
        secret_key: secret_key.to_string(),
        session_token,
    })
}

/// One minimal HTTP/1.0 request to IMDS; returns the body on 2xx status.
fn imds_request(imds_base: &str, method: &str, path: &str, token: Option<&str>) -> Option<String> {
    let (host, port) = imds_host_port(imds_base)?;
    // `to_socket_addrs` on an IP literal does not touch DNS — fast.
    let addr = (host.as_str(), port).to_socket_addrs().ok()?.next()?;
    let mut stream = TcpStream::connect_timeout(&addr, IMDS_CONNECT_TIMEOUT).ok()?;
    stream.set_read_timeout(Some(IMDS_IO_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(IMDS_IO_TIMEOUT)).ok()?;
    let mut request = format!("{method} {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n");
    if method == "PUT" {
        request.push_str("X-aws-ec2-metadata-token-ttl-seconds: 21600\r\nContent-Length: 0\r\n");
    }
    if let Some(token) = token {
        request.push_str(&format!("X-aws-ec2-metadata-token: {token}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let (head, body) = response.split_once("\r\n\r\n")?;
    let status: u16 = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    if !(200..300).contains(&status) {
        return None;
    }
    Some(body.to_string())
}

/// Parse `http://host[:port]` -> (host, port). Only the `http` scheme
/// (IMDS) is supported.
fn imds_host_port(imds_base: &str) -> Option<(String, u16)> {
    let hostport = imds_base.strip_prefix("http://")?.split('/').next()?;
    if hostport.is_empty() {
        return None;
    }
    if let Some((host, port)) = hostport.rsplit_once(':') {
        Some((host.to_string(), port.parse().ok()?))
    } else {
        Some((hostport.to_string(), 80))
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
        let provider = self.clone();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        let model = model.unwrap_or(&provider.model).to_string();

        async_stream::stream! {
            let body = BedrockProvider::request_body(&messages, &tools, max_tokens, temperature);

            // 1. Try the real converse-stream endpoint.
            let streamed = match provider.stream_request(&model, &body).await {
                Ok(resp) if resp.status().is_success() => Some(resp),
                // Non-success status (4xx/5xx) or transport failure:
                // fall back to non-streaming converse below.
                _ => None,
            };
            if let Some(resp) = streamed {
                let mut usage = bedrock_usage(0, 0);
                let mut buf: Vec<u8> = Vec::new();
                let mut pending_tool: Option<(u32, String, String)> = None;
                let mut stop_reason: Option<String> = None;
                let mut failed = false;
                let mut byte_stream = resp.bytes_stream();
                while let Some(chunk_result) = byte_stream.next().await {
                    let chunk_bytes = match chunk_result {
                        Ok(b) => b,
                        Err(e) => {
                            yield Err(AgentError::LlmApi(format!("Bedrock stream error: {e}")));
                            failed = true;
                            break;
                        }
                    };
                    match pump_event_stream_bytes(
                        &mut buf,
                        &chunk_bytes,
                        &mut pending_tool,
                        &mut usage,
                        &mut stop_reason,
                    ) {
                        Ok(chunks) => {
                            for chunk in chunks {
                                yield Ok(chunk);
                            }
                        }
                        Err(e) => {
                            yield Err(e);
                            failed = true;
                            break;
                        }
                    }
                }
                if !failed {
                    usage.total_tokens = usage.prompt_tokens + usage.completion_tokens;
                    yield Ok(StreamChunk {
                        delta: None,
                        finish_reason: Some(stop_reason.unwrap_or_else(|| "stop".to_string())),
                        usage: Some(usage),
                    });
                }
                return;
            }

            // 2. Fallback: the converse-stream endpoint failed at connect
            // time (e.g. the model/region does not support streaming).
            // Replay a non-streaming converse call as stream chunks.
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
        let (system, contents) = GeminiProvider::convert(&msgs, "gemini-2.0-flash");
        let system = system.unwrap();
        assert_eq!(system["role"], "system");
        assert_eq!(system["parts"][0]["text"], "sys");
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
        assert!(contents[1]["parts"][0].get("functionCall").is_some());
        assert_eq!(contents[1]["parts"][0]["functionCall"]["name"], "exec");
        // 2.x models reject explicit ids.
        assert!(contents[1]["parts"][0]["functionCall"].get("id").is_none());
    }

    #[test]
    fn gemini_system_messages_join_into_one_system_instruction() {
        let msgs = vec![
            msg(MessageRole::System, "first"),
            msg(MessageRole::System, "second"),
            msg(MessageRole::User, "hi"),
        ];
        let (system, contents) = GeminiProvider::convert(&msgs, "gemini-2.0-flash");
        let system = system.unwrap();
        assert_eq!(system["role"], "system");
        assert_eq!(system["parts"][0]["text"], "first\nsecond");
        // No `role: "system"` content entry may leak into contents.
        assert!(contents.iter().all(|c| c["role"] != "system"));
    }

    fn tool_msg(name: &str, content: &str, tool_call_id: &str) -> Message {
        Message {
            role: MessageRole::Tool,
            content: Some(content.to_string()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.to_string()),
            name: Some(name.to_string()),
            reasoning_content: None,
            cache_control: None,
        }
    }

    #[test]
    fn gemini_tool_message_becomes_function_response() {
        let msgs = vec![tool_msg("exec", "ok", "c1")];
        let (_, contents) = GeminiProvider::convert(&msgs, "gemini-2.0-flash");
        assert_eq!(contents.len(), 1);
        let fr = &contents[0]["parts"][0]["functionResponse"];
        assert_eq!(fr["name"], "exec");
        assert_eq!(fr["response"]["output"], "ok");
        assert!(fr.get("id").is_none());
    }

    #[test]
    fn gemini_function_response_json_object_sent_as_is() {
        let msgs = vec![tool_msg("exec", r#"{"files": ["a.rs"]}"#, "c1")];
        let (_, contents) = GeminiProvider::convert(&msgs, "gemini-2.0-flash");
        let fr = &contents[0]["parts"][0]["functionResponse"];
        assert_eq!(fr["response"], json!({ "files": ["a.rs"] }));
    }

    #[test]
    fn gemini_function_response_schema_like_payload_stays_opaque() {
        // `$ref: "#/..."` inside the payload must NOT be sent as JSON —
        // Gemini resolves the pointer and 400s on unknown ones.
        let payload = r##"{"$ref": "#/defs/thing"}"##;
        let msgs = vec![tool_msg("exec", payload, "c1")];
        let (_, contents) = GeminiProvider::convert(&msgs, "gemini-2.0-flash");
        let fr = &contents[0]["parts"][0]["functionResponse"];
        assert_eq!(fr["response"], json!({ "output": payload }));
    }

    #[test]
    fn gemini_function_call_ids_only_for_v3_models() {
        let assistant = Message {
            role: MessageRole::Assistant,
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call-9".to_string(),
                function: FunctionCall {
                    name: "exec".to_string(),
                    arguments: "{}".to_string(),
                },
            }]),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
            cache_control: None,
        };
        let msgs = vec![assistant.clone(), tool_msg("exec", "done", "call-9")];

        let (_, v2) = GeminiProvider::convert(&msgs, "gemini-2.0-flash");
        assert!(v2[0]["parts"][0]["functionCall"].get("id").is_none());
        // Two tool-bearing user contents merge into one.
        assert_eq!(v2.len(), 2);
        assert!(v2[1]["parts"][0]["functionResponse"].get("id").is_none());

        let (_, v3) = GeminiProvider::convert(&msgs, "gemini-3.7-flash");
        assert_eq!(v3[0]["parts"][0]["functionCall"]["id"], "call-9");
        assert_eq!(v3[1]["parts"][0]["functionResponse"]["id"], "call-9");
    }

    #[test]
    fn gemini_merge_alternating_merges_consecutive_tool_results() {
        let msgs = vec![
            msg(MessageRole::User, "run both"),
            Message {
                role: MessageRole::Assistant,
                content: None,
                tool_calls: Some(vec![
                    ToolCall {
                        id: "a".to_string(),
                        function: FunctionCall {
                            name: "exec".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                    ToolCall {
                        id: "b".to_string(),
                        function: FunctionCall {
                            name: "read".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                ]),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
                cache_control: None,
            },
            tool_msg("exec", "out-a", "a"),
            tool_msg("read", "out-b", "b"),
            msg(MessageRole::User, "thanks"),
        ];
        let (_, contents) = GeminiProvider::convert(&msgs, "gemini-2.0-flash");
        // user, model, merged user (2 functionResponses), model placeholder, user
        assert_eq!(contents.len(), 5);
        assert_eq!(contents[2]["role"], "user");
        assert_eq!(contents[2]["parts"].as_array().unwrap().len(), 2);
        assert_eq!(contents[3]["role"], "model");
        assert!(contents[3]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("interrupted"));
        assert_eq!(contents[4]["parts"][0]["text"], "thanks");
        // Strict alternation holds.
        let roles: Vec<&str> = contents
            .iter()
            .map(|c| c["role"].as_str().unwrap())
            .collect();
        assert!(roles.windows(2).all(|w| w[0] != w[1]));
    }

    #[test]
    fn gemini_merge_alternating_keeps_tool_result_and_text_apart() {
        // A functionResponse content and a human text content are never
        // merged; a placeholder model turn is inserted so roles alternate.
        let msgs = vec![msg(MessageRole::User, "hi"), tool_msg("exec", "ok", "c1")];
        let (_, contents) = GeminiProvider::convert(&msgs, "gemini-2.0-flash");
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
        assert!(contents[1]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("interrupted"));
        assert_eq!(contents[2]["role"], "user");
        assert!(contents[2]["parts"][0].get("functionResponse").is_some());
    }

    #[test]
    fn gemini_version_parsing() {
        assert_eq!(gemini_version("gemini-2.0-flash"), (2, 0));
        assert_eq!(gemini_version("gemini-3.7-flash"), (3, 7));
        assert_eq!(gemini_version("google/gemini-3.8-flash"), (3, 8));
        assert_eq!(gemini_version("gemini/gemini-2.5-pro"), (2, 5));
        assert_eq!(gemini_version("something-else"), (0, 0));
        assert!(gemini_requires_tool_call_ids("gemini-3.7-flash"));
        assert!(!gemini_requires_tool_call_ids("gemini-2.0-flash"));
    }

    #[test]
    fn gemini_convert_tools_sanitizes_parameters() {
        let tools = vec![ToolSchema::new(
            "read_file",
            "Read a file",
            JsonSchema::new("object"),
        )];
        let converted = GeminiProvider::convert_tools(&tools);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["functionDeclarations"][0]["name"], "read_file");

        // Nameless declarations are skipped, duplicate names dropped.
        let dupes = vec![
            ToolSchema::new("", "no name", JsonSchema::new("object")),
            ToolSchema::new("a", "first", JsonSchema::new("object")),
            ToolSchema::new("a", "second", JsonSchema::new("object")),
        ];
        let converted = GeminiProvider::convert_tools(&dupes);
        assert_eq!(
            converted[0]["functionDeclarations"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        // Unknown schema keys ($schema, additionalProperties, const) are
        // stripped; type arrays normalize; bad `required` entries are pruned.
        let raw = json!({
            "type": ["object", "null"],
            "$schema": "http://json-schema.org/draft-07/schema#",
            "additionalProperties": false,
            "properties": {
                "path": { "type": "string", "const": "x" },
                "count": { "type": "integer", "enum": [1, 2] },
            },
            "required": ["path", "missing"],
        });
        let cleaned = sanitize_gemini_tool_parameters(&raw);
        assert!(cleaned.get("$schema").is_none());
        assert!(cleaned.get("additionalProperties").is_none());
        assert_eq!(cleaned["type"], "object");
        assert_eq!(cleaned["nullable"], true);
        assert!(cleaned["properties"]["path"].get("const").is_none());
        assert_eq!(cleaned["properties"]["count"]["enum"], json!(["1", "2"]));
        assert_eq!(cleaned["required"], json!(["path"]));
    }

    #[test]
    fn gemini_base_url_resolution_precedence() {
        let d = DEFAULT_GEMINI_BASE_URL;
        // Default when nothing is given.
        assert_eq!(resolve_gemini_base_url_with(None, None), d);
        // Explicit base URL is used and normalized with a version segment.
        assert_eq!(
            resolve_gemini_base_url_with(None, Some("https://proxy.example.com")),
            "https://proxy.example.com/v1beta"
        );
        // Existing version segment is kept.
        assert_eq!(
            resolve_gemini_base_url_with(None, Some("https://proxy.example.com/v1")),
            "https://proxy.example.com/v1"
        );
        // `/openai` suffix is stripped.
        assert_eq!(
            resolve_gemini_base_url_with(None, Some("https://proxy.example.com/openai")),
            "https://proxy.example.com/v1beta"
        );
        // Env wins over explicit.
        assert_eq!(
            resolve_gemini_base_url_with(
                Some("https://env-proxy.example.com"),
                Some("https://proxy.example.com")
            ),
            "https://env-proxy.example.com/v1beta"
        );
        // Env equal to the default is ignored: explicit wins.
        assert_eq!(
            resolve_gemini_base_url_with(Some(d), Some("https://proxy.example.com")),
            "https://proxy.example.com/v1beta"
        );
        // Explicit equal to the default is ignored: default stands.
        assert_eq!(resolve_gemini_base_url_with(None, Some(d)), d);
        // Empty values are ignored.
        assert_eq!(resolve_gemini_base_url_with(Some("  "), Some("")), d);
    }

    #[test]
    fn gemini_new_uses_current_defaults() {
        let p = GeminiProvider::new("k");
        assert_eq!(p.model, DEFAULT_GEMINI_MODEL);
        assert_eq!(p.model, "gemini-3.7-flash");
    }

    #[test]
    fn azure_builds_deployment_url() {
        let p = AzureOpenAiProvider::with_api_version("k", "myres", "mydep", "2024-10-21");
        assert_eq!(
            p.deployment_url,
            "https://myres.openai.azure.com/openai/deployments/mydep/chat/completions?api-version=2024-10-21"
        );
        assert!(!p.foundry);
    }

    // ------------------------------------------------------------------
    // Wave 4 / Worker 5: Azure Foundry verbatim-URL mode + request minors
    // ------------------------------------------------------------------

    #[test]
    fn azure_foundry_uses_base_url_verbatim() {
        let base = "https://myres.services.ai.azure.com/openai/v1/chat/completions";
        // Trailing slashes are trimmed, but nothing else is appended.
        let p = AzureOpenAiProvider::new_foundry("k", format!("{base}//"));
        assert!(p.foundry);
        assert_eq!(p.deployment_url, base);
        assert_eq!(p.chat_url(), base);
        assert!(
            !p.chat_url().contains("api-version"),
            "Foundry URL must not gain an api-version query param"
        );
        assert!(
            !p.chat_url().contains("/deployments/"),
            "Foundry URL must not gain a deployment path"
        );
    }

    #[test]
    fn azure_foundry_env_helpers() {
        // Save/restore: these tests run in the same process as other tests.
        let saved_url = std::env::var("AZURE_FOUNDRY_BASE_URL").ok();
        let saved_key = std::env::var("AZURE_FOUNDRY_API_KEY").ok();
        std::env::set_var("AZURE_FOUNDRY_BASE_URL", "https://f.example.com/v1");
        std::env::set_var("AZURE_FOUNDRY_API_KEY", "f-key");
        assert_eq!(
            AzureOpenAiProvider::foundry_base_url_from_env().as_deref(),
            Some("https://f.example.com/v1")
        );
        assert_eq!(
            AzureOpenAiProvider::foundry_api_key_from_env().as_deref(),
            Some("f-key")
        );
        std::env::set_var("AZURE_FOUNDRY_BASE_URL", "   ");
        assert!(AzureOpenAiProvider::foundry_base_url_from_env().is_none());
        match saved_url {
            Some(v) => std::env::set_var("AZURE_FOUNDRY_BASE_URL", v),
            None => std::env::remove_var("AZURE_FOUNDRY_BASE_URL"),
        }
        match saved_key {
            Some(v) => std::env::set_var("AZURE_FOUNDRY_API_KEY", v),
            None => std::env::remove_var("AZURE_FOUNDRY_API_KEY"),
        }
    }

    #[test]
    fn azure_convert_tools_dedups_duplicate_names() {
        let mk = |name: &str| ToolSchema::new(name, "d", JsonSchema::new("object"));
        let tools = vec![mk("a"), mk("b"), mk("a"), mk("")];
        let v = AzureOpenAiProvider::convert_tools(&tools);
        let arr = v.as_array().unwrap();
        let names: Vec<&str> = arr
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        // First occurrence wins; empty names are exempt from dedup.
        assert_eq!(names, vec!["a", "b", ""]);
    }

    #[test]
    fn azure_request_body_omits_empty_tools_and_unset_temperature() {
        // Parity with hermes-rs build_chat_body: `tools` is omitted when
        // empty (never `[]`/`null`), `temperature` is omitted when None
        // (never null).
        let p = AzureOpenAiProvider::with_api_version("k", "r", "d", "v");
        let msgs = vec![msg(MessageRole::User, "hi")];
        let body = p.request_body(&msgs, &[], None, None, false, None);
        let obj = body.as_object().unwrap();
        assert!(!obj.contains_key("tools"), "empty tools must be omitted");
        assert!(
            !obj.contains_key("temperature"),
            "unset temperature must be omitted, not null"
        );
        assert!(!obj.contains_key("stream_options"));
    }

    #[test]
    fn azure_request_body_includes_tools_and_temperature_when_set() {
        let p = AzureOpenAiProvider::new_foundry("k", "https://f.example.com/v1");
        let msgs = vec![msg(MessageRole::User, "hi")];
        let tools = vec![
            ToolSchema::new("exec", "run", JsonSchema::new("object")),
            ToolSchema::new("exec", "dup", JsonSchema::new("object")),
        ];
        let body = p.request_body(&msgs, &tools, Some(50), Some(0.7), true, None);
        let obj = body.as_object().unwrap();
        assert_eq!(obj["temperature"].as_f64().unwrap(), 0.7);
        assert_eq!(obj["max_tokens"], 50);
        assert_eq!(obj["stream"], true);
        assert!(obj["stream_options"].is_object());
        // Duplicate tool names deduplicated: first wins.
        let arr = obj["tools"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["function"]["description"], "run");
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
        // toolResult.content is a *list* of blocks (no `status` field).
        assert!(tr.get("status").is_none());
        assert!(tr["content"].is_array());
        assert_eq!(tr["content"][0]["text"], "done");
    }

    #[test]
    fn bedrock_empty_text_becomes_placeholder() {
        // Converse rejects empty / whitespace-only text blocks; they are
        // sent as "(empty)".
        for content in ["", "   \n\t "] {
            let msgs = vec![msg(MessageRole::User, content)];
            let (_, out) = BedrockProvider::convert(&msgs);
            assert_eq!(out.len(), 1);
            assert_eq!(out[0]["content"][0]["text"], "(empty)");
        }
    }

    #[test]
    fn bedrock_system_blank_text_dropped_not_placeholder() {
        // Blank system text is dropped (not replaced with a placeholder).
        let msgs = vec![
            msg(MessageRole::System, "   "),
            msg(MessageRole::User, "hi"),
        ];
        let (system, _) = BedrockProvider::convert(&msgs);
        assert!(system.is_empty());
    }

    #[test]
    fn bedrock_convert_merges_same_role_and_pads_ends() {
        // Adjacent same-role turns merge; a stream starting/ending with an
        // assistant turn is padded with user placeholder turns.
        let msgs = vec![
            msg(MessageRole::Assistant, "a1"),
            msg(MessageRole::Assistant, "a2"),
            msg(MessageRole::User, "u1"),
        ];
        let (_, out) = BedrockProvider::convert(&msgs);
        // [pad user, assistant(a1+a2), user(u1)] — the tail is already user.
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[0]["content"][0]["text"], "(empty)");
        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[1]["content"].as_array().unwrap().len(), 2);
        assert_eq!(out[2]["role"], "user");
        assert_eq!(out[2]["content"][0]["text"], "u1");
    }

    #[test]
    fn bedrock_convert_tool_results_merge_into_user_turn() {
        let tool_msg = |id: &str| Message {
            role: MessageRole::Tool,
            content: Some("ok".to_string()),
            tool_calls: None,
            tool_call_id: Some(id.to_string()),
            name: None,
            reasoning_content: None,
            cache_control: None,
        };
        let msgs = vec![tool_msg("c1"), tool_msg("c2")];
        let (_, out) = BedrockProvider::convert(&msgs);
        // Two consecutive tool results merge into one user turn.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(out[0]["content"][0]["toolResult"]["toolUseId"], "c1");
        assert_eq!(out[0]["content"][1]["toolResult"]["toolUseId"], "c2");
    }

    #[test]
    fn bedrock_empty_tool_result_text_becomes_placeholder() {
        let msgs = vec![Message {
            role: MessageRole::Tool,
            content: Some("".to_string()),
            tool_calls: None,
            tool_call_id: Some("c1".to_string()),
            name: None,
            reasoning_content: None,
            cache_control: None,
        }];
        let (_, out) = BedrockProvider::convert(&msgs);
        assert_eq!(
            out[0]["content"][0]["toolResult"]["content"][0]["text"],
            "(empty)"
        );
    }

    #[test]
    fn bedrock_request_body_omits_unset_inference_config_keys() {
        let msgs = vec![msg(MessageRole::User, "hi")];
        let body = BedrockProvider::request_body(&msgs, &[], None, None);
        assert!(body.get("inferenceConfig").is_none());
        let body = BedrockProvider::request_body(&msgs, &[], Some(100), None);
        assert_eq!(body["inferenceConfig"]["maxTokens"], 100);
        assert!(body["inferenceConfig"].get("temperature").is_none());
    }

    #[test]
    fn bedrock_convert_tools_dedupes_duplicate_names() {
        let tools = vec![
            ToolSchema::new("read_file", "Read a file", JsonSchema::new("object")),
            ToolSchema::new("read_file", "Read a file (dup)", JsonSchema::new("object")),
        ];
        let tc = BedrockProvider::convert_tools(&tools).expect("toolConfig");
        assert_eq!(tc["tools"].as_array().unwrap().len(), 1);
        assert_eq!(tc["tools"][0]["toolSpec"]["name"], "read_file");
    }

    // ------------------------------------------------------------------
    // converse-stream event-stream framing tests (canned binary frames)
    // ------------------------------------------------------------------

    /// Build one canned event-stream frame. CRCs are zeroed: the parser
    /// skips CRC verification (framing is what's under test).
    fn canned_frame(message_type: &str, event_type: &str, payload: &[u8]) -> Vec<u8> {
        let mut headers = Vec::new();
        for (name, value) in [
            (":message-type", message_type),
            (":event-type", event_type),
            (":content-type", "application/json"),
        ] {
            headers.push(name.len() as u8);
            headers.extend_from_slice(name.as_bytes());
            headers.push(7u8); // value type: string
            headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            headers.extend_from_slice(value.as_bytes());
        }
        let total = (12 + headers.len() + payload.len() + 4) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        out.extend_from_slice(&[0u8; 4]); // prelude CRC (skipped)
        out.extend_from_slice(&headers);
        out.extend_from_slice(payload);
        out.extend_from_slice(&[0u8; 4]); // message CRC (skipped)
        out
    }

    fn fresh_pump_state() -> (
        Vec<u8>,
        Option<(u32, String, String)>,
        UsageStats,
        Option<String>,
    ) {
        (Vec::new(), None, bedrock_usage(0, 0), None)
    }

    #[test]
    fn bedrock_eventstream_frame_parse_partial_waits() {
        let full = canned_frame("event", "messageStop", b"{\"stopReason\":\"end_turn\"}");
        // Less than one full frame -> None (wait for more chunks).
        assert!(parse_bedrock_event_frame(&full[..7]).unwrap().is_none());
        assert!(parse_bedrock_event_frame(&full[..full.len() - 1])
            .unwrap()
            .is_none());
        let (frame, used) = parse_bedrock_event_frame(&full).unwrap().unwrap();
        assert_eq!(used, full.len());
        assert_eq!(frame.message_type, "event");
        assert_eq!(frame.event_type, "messageStop");
        assert_eq!(frame.payload, b"{\"stopReason\":\"end_turn\"}".to_vec());
    }

    #[test]
    fn bedrock_eventstream_frame_bad_length_errors() {
        let mut bad = canned_frame("event", "messageStop", b"{}");
        bad[0] = 0; // total_len < 16: impossible
        bad[3] = 5;
        assert!(parse_bedrock_event_frame(&bad).is_err());
    }

    #[test]
    fn bedrock_pump_canned_text_sequence() {
        let (mut buf, mut pending, mut usage, mut stop) = fresh_pump_state();
        let seq = [
            canned_frame(
                "event",
                "contentBlockDelta",
                b"{\"contentBlockIndex\":0,\"delta\":{\"text\":\"Halo\"}}",
            ),
            canned_frame(
                "event",
                "contentBlockDelta",
                b"{\"contentBlockIndex\":0,\"delta\":{\"text\":\" dunia\"}}",
            ),
            canned_frame("event", "messageStop", b"{\"stopReason\":\"end_turn\"}"),
            canned_frame(
                "event",
                "metadata",
                b"{\"usage\":{\"inputTokens\":10,\"outputTokens\":5}}",
            ),
        ]
        .concat();
        let mut chunks = Vec::new();
        // Feed in odd-sized pieces to exercise buffering across chunks.
        for piece in seq.chunks(7) {
            chunks.extend(
                pump_event_stream_bytes(&mut buf, piece, &mut pending, &mut usage, &mut stop)
                    .unwrap(),
            );
        }
        let texts: Vec<&str> = chunks
            .iter()
            .filter_map(|c| c.delta.as_ref().and_then(|d| d.content.as_deref()))
            .collect();
        assert_eq!(texts.concat(), "Halo dunia");
        assert!(chunks.iter().all(|c| c
            .delta
            .as_ref()
            .map(|d| d.tool_calls.is_none())
            .unwrap_or(true)));
        assert_eq!(stop.as_deref(), Some("end_turn"));
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
    }

    #[test]
    fn bedrock_pump_tool_use_deltas() {
        let (mut buf, mut pending, mut usage, mut stop) = fresh_pump_state();
        let delta_payload = serde_json::json!({
            "contentBlockIndex": 1,
            "delta": {"toolUse": {"input": "{\"cmd\":\"ls\"}"}},
        })
        .to_string();
        let seq = [
            canned_frame(
                "event",
                "contentBlockStart",
                b"{\"contentBlockIndex\":1,\"start\":{\"toolUse\":{\"toolUseId\":\"tu-1\",\"name\":\"exec\"}}}",
            ),
            canned_frame("event", "contentBlockDelta", delta_payload.as_bytes()),
            canned_frame("event", "contentBlockStop", b"{\"contentBlockIndex\":1}"),
        ]
        .concat();
        let chunks =
            pump_event_stream_bytes(&mut buf, &seq, &mut pending, &mut usage, &mut stop).unwrap();
        let tcs: Vec<&ToolCallDelta> = chunks
            .iter()
            .filter_map(|c| {
                c.delta
                    .as_ref()
                    .and_then(|d| d.tool_calls.as_deref())
                    .and_then(|t| t.first())
            })
            .collect();
        assert_eq!(tcs.len(), 2);
        assert_eq!(tcs[0].index, 1);
        assert_eq!(tcs[0].id.as_deref(), Some("tu-1"));
        assert_eq!(
            tcs[0].function.as_ref().and_then(|f| f.name.as_deref()),
            Some("exec")
        );
        assert_eq!(
            tcs[1]
                .function
                .as_ref()
                .and_then(|f| f.arguments.as_deref()),
            Some(r#"{"cmd":"ls"}"#)
        );
        // contentBlockStop clears the pending tool.
        assert!(pending.is_none());
    }

    #[test]
    fn bedrock_pump_exception_becomes_stream_error() {
        let (mut buf, mut pending, mut usage, mut stop) = fresh_pump_state();
        let frame = canned_frame(
            "exception",
            "modelStreamErrorException",
            b"{\"message\":\"model bermasalah\"}",
        );
        let err = pump_event_stream_bytes(&mut buf, &frame, &mut pending, &mut usage, &mut stop)
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("modelStreamErrorException"), "{msg}");
        assert!(msg.contains("model bermasalah"), "{msg}");
    }

    // ------------------------------------------------------------------
    // AWS credential / region chain tests (no live network)
    // ------------------------------------------------------------------

    fn env_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn bedrock_region_chain() {
        assert_eq!(resolve_bedrock_region_from(&env_of(&[])), "us-east-1");
        assert_eq!(
            resolve_bedrock_region_from(&env_of(&[("AWS_REGION", "  ")])), // blank ignored
            "us-east-1"
        );
        assert_eq!(
            resolve_bedrock_region_from(&env_of(&[("AWS_DEFAULT_REGION", "eu-west-1")])),
            "eu-west-1"
        );
        assert_eq!(
            resolve_bedrock_region_from(&env_of(&[
                ("AWS_REGION", "ap-southeast-1"),
                ("AWS_DEFAULT_REGION", "eu-west-1"),
            ])),
            "ap-southeast-1"
        );
    }

    #[test]
    fn bedrock_env_creds_chain_level_1() {
        // Both keys required; session token optional.
        assert!(credentials_from_env_vars(&env_of(&[])).is_none());
        assert!(credentials_from_env_vars(&env_of(&[("AWS_ACCESS_KEY_ID", "AKID")])).is_none());
        assert!(credentials_from_env_vars(&env_of(&[
            ("AWS_ACCESS_KEY_ID", "AKID"),
            ("AWS_SECRET_ACCESS_KEY", "  "),
        ]))
        .is_none());
        let creds = credentials_from_env_vars(&env_of(&[
            ("AWS_ACCESS_KEY_ID", " AKID "),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
            ("AWS_SESSION_TOKEN", "tok"),
        ]))
        .expect("creds");
        assert_eq!(creds.access_key, "AKID");
        assert_eq!(creds.secret_key, "secret");
        assert_eq!(creds.session_token.as_deref(), Some("tok"));
    }

    #[test]
    fn bedrock_ini_parser_reads_default_profile_only() {
        let ini = "# comment\n[default]\naws_access_key_id = AKIDF\naws_secret_access_key = secretf\n\n[other]\naws_access_key_id = NOPE\n";
        let creds = parse_aws_credentials_ini(ini).expect("creds");
        assert_eq!(creds.access_key, "AKIDF");
        assert_eq!(creds.secret_key, "secretf");
        assert!(creds.session_token.is_none());

        // Missing secret -> None.
        assert!(parse_aws_credentials_ini("[default]\naws_access_key_id = AKID\n").is_none());
        // Only a named profile -> None.
        assert!(parse_aws_credentials_ini(
            "[work]\naws_access_key_id = AKID\naws_secret_access_key = s\n"
        )
        .is_none());
        // Session token is picked up when present.
        let ini =
            "[default]\naws_access_key_id = A\naws_secret_access_key = B\naws_session_token = T\n";
        assert_eq!(
            parse_aws_credentials_ini(ini)
                .unwrap()
                .session_token
                .as_deref(),
            Some("T")
        );
    }

    #[test]
    fn bedrock_credentials_file_chain_level_2() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".aws")).unwrap();
        std::fs::write(
            tmp.path().join(".aws").join("credentials"),
            "[default]\naws_access_key_id = AKID2\naws_secret_access_key = secret2\n",
        )
        .unwrap();
        let creds = credentials_from_aws_credentials_file(Some(tmp.path())).expect("creds");
        assert_eq!(creds.access_key, "AKID2");
        // No file -> None.
        assert!(
            credentials_from_aws_credentials_file(Some(tmp.path().join("nope").as_path()))
                .is_none()
        );
        assert!(credentials_from_aws_credentials_file(None).is_none());
    }

    #[test]
    fn bedrock_imds_fail_fast_off_ec2() {
        // Never touch the real 169.254.169.254 in tests: point the IMDS
        // client at a dead loopback port and expect a fast None.
        assert!(credentials_from_imds("http://127.0.0.1:9").is_none());
    }

    #[test]
    fn bedrock_imds_host_port_parse() {
        assert_eq!(
            imds_host_port("http://169.254.169.254"),
            Some(("169.254.169.254".to_string(), 80))
        );
        assert_eq!(
            imds_host_port("http://127.0.0.1:9/latest"),
            Some(("127.0.0.1".to_string(), 9))
        );
        assert!(imds_host_port("https://169.254.169.254").is_none());
        assert!(imds_host_port("http://").is_none());
    }
}
