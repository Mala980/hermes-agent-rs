//! Browser automation tools
//!
//! All browser tools delegate to an injectable `BrowserBackend` trait,
//! allowing different browser implementations (Playwright, Chromium, etc.)
//! to be plugged in by the caller.

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Value};

use hermes_core::{tool_schema, JsonSchema, ToolError, ToolHandler, ToolSchema};

// ---------------------------------------------------------------------------
// BrowserBackend trait
// ---------------------------------------------------------------------------

/// Injected backend for browser automation operations.
#[async_trait]
pub trait BrowserBackend: Send + Sync {
    async fn navigate(&self, url: &str) -> Result<String, ToolError>;
    async fn snapshot(&self) -> Result<String, ToolError>;
    async fn click(&self, selector: &str) -> Result<String, ToolError>;
    async fn r#type(&self, selector: &str, text: &str) -> Result<String, ToolError>;
    async fn scroll(&self, direction: &str, amount: Option<u32>) -> Result<String, ToolError>;
    async fn go_back(&self) -> Result<String, ToolError>;
    async fn press(&self, key: &str) -> Result<String, ToolError>;
    async fn get_images(&self, selector: Option<&str>) -> Result<String, ToolError>;
    async fn vision(&self, instruction: &str) -> Result<String, ToolError>;
    async fn console(&self, action: &str) -> Result<String, ToolError>;

    /// Send a raw Chrome DevTools Protocol command.
    ///
    /// `target_id` optionally attaches to a specific tab via
    /// `Target.attachToTarget` (flattened session); without it the method runs
    /// against the default target session. The default implementation reports
    /// the backend as CDP-incapable; CDP-backed backends override this.
    async fn cdp_raw(
        &self,
        method: &str,
        params: Value,
        target_id: Option<&str>,
    ) -> Result<Value, ToolError> {
        let _ = (method, params, target_id);
        Err(ToolError::ExecutionFailed(
            "Raw CDP commands are not supported by this browser backend".into(),
        ))
    }

    /// Evaluate a JavaScript expression in the current page.
    ///
    /// Default implementation routes through [`BrowserBackend::cdp_raw`]
    /// (`Runtime.evaluate`).
    async fn evaluate_js(
        &self,
        expression: &str,
        return_by_value: bool,
    ) -> Result<Value, ToolError> {
        self.cdp_raw(
            "Runtime.evaluate",
            json!({"expression": expression, "returnByValue": return_by_value}),
            None,
        )
        .await
    }

    /// Respond to a native JavaScript dialog blocking the page.
    ///
    /// Default implementation routes through [`BrowserBackend::cdp_raw`]
    /// (`Page.handleJavaScriptDialog`).
    async fn handle_dialog(
        &self,
        accept: bool,
        prompt_text: Option<&str>,
    ) -> Result<Value, ToolError> {
        self.cdp_raw(
            "Page.handleJavaScriptDialog",
            json!({"accept": accept, "promptText": prompt_text.unwrap_or("")}),
            None,
        )
        .await
    }

    /// Insert text into the focused element without embedding it in a JS
    /// expression. Used for secrets: the value travels only as a CDP
    /// parameter over the WebSocket, never in page JS, argv, or logs.
    ///
    /// Default implementation routes through [`BrowserBackend::cdp_raw`]
    /// (`Input.insertText`).
    async fn insert_text(&self, text: &str) -> Result<Value, ToolError> {
        self.cdp_raw("Input.insertText", json!({"text": text}), None)
            .await
    }
}

// ---------------------------------------------------------------------------
// BrowserNavigateHandler
// ---------------------------------------------------------------------------

pub struct BrowserNavigateHandler {
    backend: std::sync::Arc<dyn BrowserBackend>,
}

impl BrowserNavigateHandler {
    pub fn new(backend: std::sync::Arc<dyn BrowserBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl ToolHandler for BrowserNavigateHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let url = params
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("Missing 'url' parameter".into()))?;
        self.backend.navigate(url).await
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "url".into(),
            json!({
                "type": "string",
                "description": "The URL to navigate to"
            }),
        );
        tool_schema(
            "browser_navigate",
            "Navigate the browser to a URL.",
            JsonSchema::object(props, vec!["url".into()]),
        )
    }
}

// ---------------------------------------------------------------------------
// BrowserSnapshotHandler
// ---------------------------------------------------------------------------

pub struct BrowserSnapshotHandler {
    backend: std::sync::Arc<dyn BrowserBackend>,
}

impl BrowserSnapshotHandler {
    pub fn new(backend: std::sync::Arc<dyn BrowserBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl ToolHandler for BrowserSnapshotHandler {
    async fn execute(&self, _params: Value) -> Result<String, ToolError> {
        self.backend.snapshot().await
    }

    fn schema(&self) -> ToolSchema {
        tool_schema(
            "browser_snapshot",
            "Take a snapshot of the current page state (accessibility tree).",
            JsonSchema::new("object"),
        )
    }
}

// ---------------------------------------------------------------------------
// BrowserClickHandler
// ---------------------------------------------------------------------------

pub struct BrowserClickHandler {
    backend: std::sync::Arc<dyn BrowserBackend>,
}

impl BrowserClickHandler {
    pub fn new(backend: std::sync::Arc<dyn BrowserBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl ToolHandler for BrowserClickHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let selector = params
            .get("selector")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("Missing 'selector' parameter".into()))?;
        self.backend.click(selector).await
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "selector".into(),
            json!({
                "type": "string",
                "description": "CSS selector or accessibility label to click"
            }),
        );
        tool_schema(
            "browser_click",
            "Click an element on the page.",
            JsonSchema::object(props, vec!["selector".into()]),
        )
    }
}

// ---------------------------------------------------------------------------
// BrowserTypeHandler
// ---------------------------------------------------------------------------

pub struct BrowserTypeHandler {
    backend: std::sync::Arc<dyn BrowserBackend>,
}

impl BrowserTypeHandler {
    pub fn new(backend: std::sync::Arc<dyn BrowserBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl ToolHandler for BrowserTypeHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let selector = params
            .get("selector")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("Missing 'selector' parameter".into()))?;
        let text = params
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("Missing 'text' parameter".into()))?;
        self.backend.r#type(selector, text).await
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "selector".into(),
            json!({
                "type": "string",
                "description": "CSS selector or accessibility label of the input field"
            }),
        );
        props.insert(
            "text".into(),
            json!({
                "type": "string",
                "description": "Text to type into the element"
            }),
        );
        tool_schema(
            "browser_type",
            "Type text into an element on the page.",
            JsonSchema::object(props, vec!["selector".into(), "text".into()]),
        )
    }
}

// ---------------------------------------------------------------------------
// BrowserScrollHandler
// ---------------------------------------------------------------------------

pub struct BrowserScrollHandler {
    backend: std::sync::Arc<dyn BrowserBackend>,
}

impl BrowserScrollHandler {
    pub fn new(backend: std::sync::Arc<dyn BrowserBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl ToolHandler for BrowserScrollHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let direction = params
            .get("direction")
            .and_then(|v| v.as_str())
            .unwrap_or("down");
        let amount = params
            .get("amount")
            .and_then(|v| v.as_u64())
            .map(|n| n as u32);
        self.backend.scroll(direction, amount).await
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "direction".into(),
            json!({
                "type": "string",
                "description": "Scroll direction: up, down, left, right",
                "enum": ["up", "down", "left", "right"],
                "default": "down"
            }),
        );
        props.insert(
            "amount".into(),
            json!({
                "type": "integer",
                "description": "Number of pixels to scroll (default: 500)"
            }),
        );
        tool_schema(
            "browser_scroll",
            "Scroll the page in a direction.",
            JsonSchema::object(props, vec![]),
        )
    }
}

// ---------------------------------------------------------------------------
// BrowserBackHandler
// ---------------------------------------------------------------------------

pub struct BrowserBackHandler {
    backend: std::sync::Arc<dyn BrowserBackend>,
}

impl BrowserBackHandler {
    pub fn new(backend: std::sync::Arc<dyn BrowserBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl ToolHandler for BrowserBackHandler {
    async fn execute(&self, _params: Value) -> Result<String, ToolError> {
        self.backend.go_back().await
    }

    fn schema(&self) -> ToolSchema {
        tool_schema(
            "browser_back",
            "Navigate back in browser history.",
            JsonSchema::new("object"),
        )
    }
}

// ---------------------------------------------------------------------------
// BrowserPressHandler
// ---------------------------------------------------------------------------

pub struct BrowserPressHandler {
    backend: std::sync::Arc<dyn BrowserBackend>,
}

impl BrowserPressHandler {
    pub fn new(backend: std::sync::Arc<dyn BrowserBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl ToolHandler for BrowserPressHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let key = params
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("Missing 'key' parameter".into()))?;
        self.backend.press(key).await
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "key".into(),
            json!({
                "type": "string",
                "description": "Key to press (e.g. 'Enter', 'Tab', 'Escape', 'ArrowDown')"
            }),
        );
        tool_schema(
            "browser_press",
            "Press a keyboard key.",
            JsonSchema::object(props, vec!["key".into()]),
        )
    }
}

// ---------------------------------------------------------------------------
// BrowserGetImagesHandler
// ---------------------------------------------------------------------------

pub struct BrowserGetImagesHandler {
    backend: std::sync::Arc<dyn BrowserBackend>,
}

impl BrowserGetImagesHandler {
    pub fn new(backend: std::sync::Arc<dyn BrowserBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl ToolHandler for BrowserGetImagesHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let selector = params.get("selector").and_then(|v| v.as_str());
        self.backend.get_images(selector).await
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "selector".into(),
            json!({
                "type": "string",
                "description": "Optional CSS selector to filter images"
            }),
        );
        tool_schema(
            "browser_get_images",
            "Get images from the current page.",
            JsonSchema::object(props, vec![]),
        )
    }
}

// ---------------------------------------------------------------------------
// BrowserVisionHandler
// ---------------------------------------------------------------------------

pub struct BrowserVisionHandler {
    backend: std::sync::Arc<dyn BrowserBackend>,
}

impl BrowserVisionHandler {
    pub fn new(backend: std::sync::Arc<dyn BrowserBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl ToolHandler for BrowserVisionHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let instruction = params
            .get("instruction")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("Missing 'instruction' parameter".into()))?;
        self.backend.vision(instruction).await
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "instruction".into(),
            json!({
                "type": "string",
                "description": "What to look for or analyze in the current page screenshot"
            }),
        );
        tool_schema(
            "browser_vision",
            "Use vision to analyze the current browser page.",
            JsonSchema::object(props, vec!["instruction".into()]),
        )
    }
}

// ---------------------------------------------------------------------------
// BrowserConsoleHandler
// ---------------------------------------------------------------------------

pub struct BrowserConsoleHandler {
    backend: std::sync::Arc<dyn BrowserBackend>,
}

impl BrowserConsoleHandler {
    pub fn new(backend: std::sync::Arc<dyn BrowserBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl ToolHandler for BrowserConsoleHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("read");
        self.backend.console(action).await
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert("action".into(), json!({
            "type": "string",
            "description": "Console action: 'read' to get console output, 'clear' to clear console",
            "enum": ["read", "clear"],
            "default": "read"
        }));
        tool_schema(
            "browser_console",
            "Read or clear the browser console.",
            JsonSchema::object(props, vec![]),
        )
    }
}

// ---------------------------------------------------------------------------
// BrowserCdpHandler — raw CDP passthrough (`browser_cdp`)
// ---------------------------------------------------------------------------

/// Clamp a CDP timeout to the 1..=300s range (mirrors Python `browser_cdp`).
fn clamp_cdp_timeout(timeout: Option<f64>) -> f64 {
    match timeout {
        Some(t) if t.is_finite() => t.clamp(1.0, 300.0),
        _ => 30.0,
    }
}

pub struct BrowserCdpHandler {
    backend: std::sync::Arc<dyn BrowserBackend>,
}

impl BrowserCdpHandler {
    pub fn new(backend: std::sync::Arc<dyn BrowserBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl ToolHandler for BrowserCdpHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let method = params
            .get("method")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidParams("Missing 'method' parameter".into()))?;
        let cdp_params = params.get("params").cloned().unwrap_or(json!({}));
        if !cdp_params.is_object() {
            return Err(ToolError::InvalidParams(
                "'params' must be a JSON object".into(),
            ));
        }
        let target_id = params.get("target_id").and_then(|v| v.as_str());
        let timeout = clamp_cdp_timeout(params.get("timeout").and_then(|v| v.as_f64()));

        let call = self.backend.cdp_raw(method, cdp_params, target_id);
        let result = tokio::time::timeout(std::time::Duration::from_secs_f64(timeout), call)
            .await
            .map_err(|_| ToolError::Timeout(format!("CDP call timed out after {timeout}s")))?;
        match result {
            Ok(value) => Ok(json!({
                "success": true,
                "method": method,
                "result": value,
            })
            .to_string()),
            Err(e) => Ok(json!({
                "success": false,
                "method": method,
                "error": e.to_string(),
            })
            .to_string()),
        }
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "method".into(),
            json!({
                "type": "string",
                "description": "CDP method name, e.g. 'Target.getTargets', 'Runtime.evaluate', 'Page.handleJavaScriptDialog'"
            }),
        );
        props.insert(
            "params".into(),
            json!({
                "type": "object",
                "description": "Method-specific parameters as a JSON object",
                "additionalProperties": true
            }),
        );
        props.insert(
            "target_id".into(),
            json!({
                "type": "string",
                "description": "Optional target/tab ID from Target.getTargets for page-level methods"
            }),
        );
        props.insert(
            "timeout".into(),
            json!({
                "type": "number",
                "default": 30,
                "description": "Timeout in seconds (default 30, clamped to 1..=300)"
            }),
        );
        tool_schema(
            "browser_cdp",
            "Send a raw Chrome DevTools Protocol command. Escape hatch for browser operations not covered by the other browser tools. Method reference: https://chromedevtools.github.io/devtools-protocol/",
            JsonSchema::object(props, vec!["method".into()]),
        )
    }
}

// ---------------------------------------------------------------------------
// BrowserDialogHandler (`browser_dialog`)
// ---------------------------------------------------------------------------

pub struct BrowserDialogHandler {
    backend: std::sync::Arc<dyn BrowserBackend>,
}

impl BrowserDialogHandler {
    pub fn new(backend: std::sync::Arc<dyn BrowserBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl ToolHandler for BrowserDialogHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ToolError::InvalidParams("Missing 'action' parameter (accept|dismiss)".into())
            })?;
        let accept = match action {
            "accept" => true,
            "dismiss" => false,
            _ => {
                return Err(ToolError::InvalidParams(
                    "action must be 'accept' or 'dismiss'".into(),
                ))
            }
        };
        let prompt_text = params.get("prompt_text").and_then(|v| v.as_str());
        let dialog_id = params.get("dialog_id").and_then(|v| v.as_str());

        match self.backend.handle_dialog(accept, prompt_text).await {
            Ok(result) => Ok(json!({
                "success": true,
                "action": action,
                "dialog_id": dialog_id,
                "result": result,
            })
            .to_string()),
            Err(e) => Ok(json!({
                "success": false,
                "action": action,
                "error": e.to_string(),
            })
            .to_string()),
        }
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "action".into(),
            json!({
                "type": "string",
                "enum": ["accept", "dismiss"],
                "description": "'accept' clicks OK / returns the prompt text; 'dismiss' clicks Cancel"
            }),
        );
        props.insert(
            "prompt_text".into(),
            json!({
                "type": "string",
                "description": "Response string for a prompt() dialog. Ignored for other dialog types."
            }),
        );
        props.insert(
            "dialog_id".into(),
            json!({
                "type": "string",
                "description": "Specific dialog to respond to when several are queued"
            }),
        );
        tool_schema(
            "browser_dialog",
            "Respond to a native JavaScript dialog (alert/confirm/prompt/beforeunload) blocking the page.",
            JsonSchema::object(props, vec!["action".into()]),
        )
    }
}

// ---------------------------------------------------------------------------
// BrowserExecHandler (`browser_exec`)
// ---------------------------------------------------------------------------

pub struct BrowserExecHandler {
    backend: std::sync::Arc<dyn BrowserBackend>,
}

impl BrowserExecHandler {
    pub fn new(backend: std::sync::Arc<dyn BrowserBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl ToolHandler for BrowserExecHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let code = params
            .get("code")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidParams("Missing 'code' parameter".into()))?;
        let return_by_value = params
            .get("return_by_value")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        match self.backend.evaluate_js(code, return_by_value).await {
            Ok(value) => Ok(json!({"success": true, "result": value}).to_string()),
            Err(e) => Ok(json!({"success": false, "error": e.to_string()}).to_string()),
        }
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "code".into(),
            json!({
                "type": "string",
                "description": "JavaScript expression to evaluate in the current page"
            }),
        );
        props.insert(
            "return_by_value".into(),
            json!({
                "type": "boolean",
                "default": true,
                "description": "Return the result by value instead of by object reference"
            }),
        );
        tool_schema(
            "browser_exec",
            "Evaluate JavaScript in the current browser page via CDP Runtime.evaluate and return the result.",
            JsonSchema::object(props, vec!["code".into()]),
        )
    }
}

// ---------------------------------------------------------------------------
// Credential vault (`browser_vault`)
// ---------------------------------------------------------------------------

/// Kind of vault item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultItemKind {
    Login,
    Card,
    Address,
}

/// Non-secret metadata for a vault item. Safe to show the model; the
/// identifier (username) is not a secret, passwords never leave the vault.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VaultItemMeta {
    pub handle: String,
    pub backend: String,
    pub label: String,
    pub kind: VaultItemKind,
    pub origin: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    #[serde(default)]
    pub has_otp: bool,
}

/// A vault item together with its secret. The secret must never be serialized
/// into tool output, logs, or the session DB.
#[derive(Debug, Clone)]
pub struct VaultItem {
    pub meta: VaultItemMeta,
    pub secret: String,
}

/// Check whether `page_origin` is authorized for the item: exact match against
/// the bound origin or any entry of `allowed_origins` (mirrors the Python
/// vault's origin gate; trailing slashes are ignored for comparison).
pub fn vault_origin_allowed(meta: &VaultItemMeta, page_origin: &str) -> bool {
    fn norm(origin: &str) -> &str {
        origin.trim().trim_end_matches('/')
    }
    let page = norm(page_origin);
    if page.is_empty() {
        return false;
    }
    if norm(&meta.origin) == page {
        return true;
    }
    meta.allowed_origins.iter().any(|o| norm(o) == page)
}

/// Pluggable credential store backing `browser_vault`.
#[async_trait]
pub trait VaultBackend: Send + Sync {
    fn backend_name(&self) -> &'static str;
    async fn list_items(&self) -> Result<Vec<VaultItemMeta>, ToolError>;
    async fn get_item(&self, handle: &str) -> Result<VaultItem, ToolError>;
    async fn save_login(
        &self,
        label: &str,
        identifier: &str,
        origin: &str,
        secret: &str,
    ) -> Result<String, ToolError>;
    fn needs_unlock(&self) -> bool {
        false
    }
    fn is_unlocked(&self) -> bool {
        true
    }
    async fn unlock(&self) -> Result<(), ToolError> {
        Err(ToolError::ExecutionFailed(
            "This vault backend does not support unlock".into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Encrypted file vault: crypto primitives
// ---------------------------------------------------------------------------

/// Encrypt-then-MAC construction for the file vault.
///
/// - Confidentiality: HMAC-SHA256-CTR stream cipher —
///   `keystream block_i = HMAC(k_enc, nonce || le64(i))`, XORed with plaintext.
/// - Integrity: `tag = HMAC-SHA256(k_mac, nonce || ciphertext)`, verified with
///   a constant-time comparison before decryption.
///
/// Only `hmac 0.12` + `sha2 0.10` are used (already workspace deps); no AES
/// crate is needed. This mirrors the reference port's construction for the
/// local-file threat model. It is NOT hardware-backed and there is no
/// passphrase: whoever can read `vault.key` (0600) can decrypt the vault.
mod vault_crypto {
    /// HMAC-SHA256.
    pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let mut mac =
            Hmac::<Sha256>::new_from_slice(key).expect("HMAC-SHA256 accepts keys of any length");
        mac.update(msg);
        mac.finalize().into_bytes().into()
    }

    /// OS entropy for keys, nonces and temp-file names.
    pub fn random_bytes(n: usize) -> std::io::Result<Vec<u8>> {
        use std::io::Read;
        let mut f = std::fs::File::open("/dev/urandom")?;
        let mut buf = vec![0u8; n];
        f.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Constant-time equality for MAC tags.
    pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let mut acc = 0u8;
        for (x, y) in a.iter().zip(b.iter()) {
            acc |= x ^ y;
        }
        acc == 0
    }

    /// Keystream blocks: `block_i = HMAC(k_enc, nonce || le64(i))`.
    pub fn keystream(enc_key: &[u8], nonce: &[u8; 16], len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut ctr: u64 = 0;
        while out.len() < len {
            let mut msg = Vec::with_capacity(24);
            msg.extend_from_slice(nonce);
            msg.extend_from_slice(&ctr.to_le_bytes());
            out.extend_from_slice(&hmac_sha256(enc_key, &msg));
            ctr += 1;
        }
        out.truncate(len);
        out
    }
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

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn hex_decode(s: &str) -> Result<Vec<u8>, ToolError> {
    let bad = || ToolError::ExecutionFailed("vault: invalid hex encoding".into());
    if !s.len().is_multiple_of(2) {
        return Err(bad());
    }
    let raw = s.as_bytes();
    let mut out = Vec::with_capacity(raw.len() / 2);
    let mut i = 0;
    while i < raw.len() {
        let hi = hex_val(raw[i]).ok_or_else(bad)?;
        let lo = hex_val(raw[i + 1]).ok_or_else(bad)?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

/// On-disk envelope for `vault.json.enc`:
/// `{"format":"hermes-vault/1","nonce":"<32 hex>","payload":"<hex>"}`,
/// where `payload = ciphertext || tag` (tag = 32 raw bytes),
/// `ciphertext = plaintext XOR keystream(k_enc, nonce)`,
/// `tag = HMAC-SHA256(k_mac, nonce || ciphertext)`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct VaultEnvelope {
    format: String,
    nonce: String,
    payload: String,
}

/// Serializes read-modify-write cycles so concurrent in-process saves cannot
/// interleave. Writes are atomic (temp file + rename); cross-process is
/// last-writer-wins (documented).
static FILE_VAULT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredVaultItem {
    handle: String,
    label: String,
    kind: VaultItemKind,
    origin: String,
    #[serde(default)]
    allowed_origins: Vec<String>,
    #[serde(default)]
    identifier: Option<String>,
    secret: String,
    #[serde(default)]
    has_otp: bool,
}

/// Non-secret metadata projection for a stored item.
fn vault_meta_of(item: &StoredVaultItem) -> VaultItemMeta {
    VaultItemMeta {
        handle: item.handle.clone(),
        backend: "local".into(),
        label: item.label.clone(),
        kind: item.kind,
        origin: item.origin.clone(),
        allowed_origins: item.allowed_origins.clone(),
        identifier: item.identifier.clone(),
        has_otp: item.has_otp,
    }
}

/// Local file-backed vault with at-rest encryption.
///
/// Layout — everything lives in the directory containing `path`:
/// - `vault.key` — 0600, 64 random bytes (32-byte encryption key +
///   32-byte MAC key), created on first use with `create_new` (race-safe).
/// - `vault.json.enc` — 0600, the encrypted item list (see [`VaultEnvelope`]).
/// - legacy `browser_vault.json` — plaintext from older builds. On the first
///   load where it exists it is migrated into the encrypted store and renamed
///   to `browser_vault.json.migrated`. The old file is never deleted
///   silently; re-migration is idempotent (dedup by handle).
///
/// The directory gets best-effort 0700 (a warning is logged on failure).
///
/// Item records keep the legacy shape
/// (handle/label/kind/origin/allowed_origins/identifier/has_otp); the `secret`
/// field rides inside the encrypted payload and never appears in `list`
/// output.
///
/// This backend is **key-file based, not passphrase locked**: `needs_unlock`
/// stays `false` and there is no unlock prompt — possession of `vault.key`
/// (0600) is the unlock. Saving a new secret interactively goes through
/// [`read_masked_password`], which fails closed when stdin is not a TTY.
pub struct FileVaultBackend {
    /// Legacy plaintext path; doubles as the migration source. The key and
    /// the encrypted store live in its parent directory.
    path: std::path::PathBuf,
}

impl FileVaultBackend {
    pub fn new(path: std::path::PathBuf) -> Self {
        Self { path }
    }

    /// Legacy default location (`<hermes_home>/browser_vault.json`).
    ///
    /// Kept stable for migration: an existing plaintext file here is moved
    /// into the encrypted store on first use. The encrypted files live
    /// beside it as `<hermes_home>/vault.key` and `<hermes_home>/vault.json.enc`.
    pub fn default_path() -> std::path::PathBuf {
        hermes_config::hermes_home().join("browser_vault.json")
    }

    fn vault_dir(&self) -> Result<std::path::PathBuf, ToolError> {
        self.path
            .parent()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| ToolError::ExecutionFailed("vault path has no parent directory".into()))
    }

    fn key_path(&self) -> Result<std::path::PathBuf, ToolError> {
        Ok(self.vault_dir()?.join("vault.key"))
    }

    fn enc_path(&self) -> Result<std::path::PathBuf, ToolError> {
        Ok(self.vault_dir()?.join("vault.json.enc"))
    }

    fn migrated_path(&self) -> std::path::PathBuf {
        self.path.with_extension("json.migrated")
    }

    fn ensure_dir(&self) -> Result<std::path::PathBuf, ToolError> {
        let dir = self.vault_dir()?;
        std::fs::create_dir_all(&dir)
            .map_err(|e| ToolError::ExecutionFailed(format!("vault dir failed: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)) {
                tracing::warn!(
                    "browser_vault: could not set 0700 on vault dir {}: {e}",
                    dir.display()
                );
            }
        }
        Ok(dir)
    }

    /// Load (or create) the 64-byte key: 32-byte encryption key ||
    /// 32-byte MAC key.
    fn load_key(&self) -> Result<([u8; 32], [u8; 32]), ToolError> {
        self.ensure_dir()?;
        let path = self.key_path()?;
        let raw: Vec<u8> = if path.exists() {
            std::fs::read(&path)
                .map_err(|e| ToolError::ExecutionFailed(format!("vault key read failed: {e}")))?
        } else {
            let fresh = vault_crypto::random_bytes(64)
                .map_err(|e| ToolError::ExecutionFailed(format!("vault key gen failed: {e}")))?;
            use std::io::Write;
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    f.write_all(&fresh).map_err(|e| {
                        ToolError::ExecutionFailed(format!("vault key write failed: {e}"))
                    })?;
                    fresh
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Lost a creation race; use the winner's key.
                    std::fs::read(&path).map_err(|e| {
                        ToolError::ExecutionFailed(format!("vault key read failed: {e}"))
                    })?
                }
                Err(e) => {
                    return Err(ToolError::ExecutionFailed(format!(
                        "vault key write failed: {e}"
                    )))
                }
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            {
                tracing::warn!(
                    "browser_vault: could not set 0600 on {}: {e}",
                    path.display()
                );
            }
        }
        if raw.len() != 64 {
            return Err(ToolError::ExecutionFailed(
                "vault key file is corrupt (expected 64 bytes)".into(),
            ));
        }
        let mut k_enc = [0u8; 32];
        let mut k_mac = [0u8; 32];
        k_enc.copy_from_slice(&raw[..32]);
        k_mac.copy_from_slice(&raw[32..]);
        Ok((k_enc, k_mac))
    }

    fn encrypt(&self, plaintext: &[u8]) -> Result<VaultEnvelope, ToolError> {
        let (k_enc, k_mac) = self.load_key()?;
        let nonce_raw = vault_crypto::random_bytes(16)
            .map_err(|e| ToolError::ExecutionFailed(format!("vault nonce failed: {e}")))?;
        let mut nonce = [0u8; 16];
        nonce.copy_from_slice(&nonce_raw);
        let ks = vault_crypto::keystream(&k_enc, &nonce, plaintext.len());
        let ciphertext: Vec<u8> = plaintext
            .iter()
            .zip(ks.iter())
            .map(|(p, k)| p ^ k)
            .collect();
        let mut mac_msg = Vec::with_capacity(16 + ciphertext.len());
        mac_msg.extend_from_slice(&nonce);
        mac_msg.extend_from_slice(&ciphertext);
        let tag = vault_crypto::hmac_sha256(&k_mac, &mac_msg);
        let mut payload = ciphertext;
        payload.extend_from_slice(&tag);
        Ok(VaultEnvelope {
            format: "hermes-vault/1".to_string(),
            nonce: hex_encode(&nonce),
            payload: hex_encode(&payload),
        })
    }

    fn decrypt(&self, env: &VaultEnvelope) -> Result<Vec<u8>, ToolError> {
        let bad = || {
            ToolError::ExecutionFailed(
                "vault decryption failed (wrong key or corrupted file)".into(),
            )
        };
        if env.format != "hermes-vault/1" {
            return Err(ToolError::ExecutionFailed(format!(
                "unknown vault format: {}",
                env.format
            )));
        }
        let (k_enc, k_mac) = self.load_key()?;
        let nonce_raw = hex_decode(&env.nonce).map_err(|_| bad())?;
        let payload = hex_decode(&env.payload).map_err(|_| bad())?;
        if nonce_raw.len() != 16 || payload.len() < 32 {
            return Err(bad());
        }
        let mut nonce = [0u8; 16];
        nonce.copy_from_slice(&nonce_raw);
        let (ciphertext, tag) = payload.split_at(payload.len() - 32);
        let mut mac_msg = Vec::with_capacity(16 + ciphertext.len());
        mac_msg.extend_from_slice(&nonce);
        mac_msg.extend_from_slice(ciphertext);
        let expect = vault_crypto::hmac_sha256(&k_mac, &mac_msg);
        if !vault_crypto::ct_eq(&expect, tag) {
            return Err(bad());
        }
        let ks = vault_crypto::keystream(&k_enc, &nonce, ciphertext.len());
        Ok(ciphertext
            .iter()
            .zip(ks.iter())
            .map(|(c, k)| c ^ k)
            .collect())
    }

    /// Load items. The caller must hold [`FILE_VAULT_LOCK`].
    fn load_locked(&self) -> Result<Vec<StoredVaultItem>, ToolError> {
        let enc = self.enc_path()?;
        let mut items: Vec<StoredVaultItem> = if enc.exists() {
            let raw = std::fs::read(&enc)
                .map_err(|e| ToolError::ExecutionFailed(format!("vault read failed: {e}")))?;
            if raw.is_empty() {
                Vec::new()
            } else {
                let env: VaultEnvelope = serde_json::from_slice(&raw).map_err(|_| {
                    ToolError::ExecutionFailed("vault file is corrupt (bad envelope)".into())
                })?;
                let plain = self.decrypt(&env)?;
                serde_json::from_slice(&plain).map_err(|_| {
                    ToolError::ExecutionFailed("vault file is corrupt (bad payload)".into())
                })?
            }
        } else {
            Vec::new()
        };
        self.migrate_legacy_locked(&mut items)?;
        Ok(items)
    }

    /// Migrate a legacy plaintext `browser_vault.json` into the encrypted
    /// store (dedup by handle), then rename it to `.migrated`. Idempotent:
    /// if the rename failed on a previous run, merging again changes nothing.
    /// The caller must hold [`FILE_VAULT_LOCK`].
    fn migrate_legacy_locked(&self, items: &mut Vec<StoredVaultItem>) -> Result<(), ToolError> {
        if !self.path.exists() {
            return Ok(());
        }
        let data = std::fs::read(&self.path)
            .map_err(|e| ToolError::ExecutionFailed(format!("legacy vault read failed: {e}")))?;
        let legacy: Vec<StoredVaultItem> = serde_json::from_slice(&data)
            .map_err(|e| ToolError::ExecutionFailed(format!("legacy vault parse failed: {e}")))?;
        for item in legacy {
            if !items.iter().any(|i| i.handle == item.handle) {
                items.push(item);
            }
        }
        // Persist before renaming so a crash cannot lose items.
        self.save_locked(items)?;
        let target = self.migrated_path();
        match std::fs::rename(&self.path, &target) {
            Ok(()) => tracing::info!(
                "browser_vault: migrated legacy plaintext vault into the encrypted store; \
                 old file renamed to {}",
                target.display()
            ),
            Err(e) => tracing::warn!(
                "browser_vault: migrated legacy vault but could not rename {}: {e} \
                 (will re-migrate idempotently on next load)",
                self.path.display()
            ),
        }
        Ok(())
    }

    /// Write items atomically (temp file + rename). The caller must hold
    /// [`FILE_VAULT_LOCK`].
    fn save_locked(&self, items: &[StoredVaultItem]) -> Result<(), ToolError> {
        let dir = self.ensure_dir()?;
        let data = serde_json::to_vec(items)
            .map_err(|e| ToolError::ExecutionFailed(format!("vault encode failed: {e}")))?;
        let env = self.encrypt(&data)?;
        let out = serde_json::to_vec(&env)
            .map_err(|e| ToolError::ExecutionFailed(format!("vault encode failed: {e}")))?;
        let tmp_name = format!(
            ".vault.{}.tmp",
            hex_encode(
                &vault_crypto::random_bytes(8)
                    .map_err(|e| ToolError::ExecutionFailed(format!("vault nonce failed: {e}")))?
            )
        );
        let tmp = dir.join(tmp_name);
        std::fs::write(&tmp, &out)
            .map_err(|e| ToolError::ExecutionFailed(format!("vault write failed: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
                tracing::warn!("browser_vault: could not set 0600 on temp vault file: {e}");
            }
        }
        std::fs::rename(&tmp, self.enc_path()?)
            .map_err(|e| ToolError::ExecutionFailed(format!("vault write failed: {e}")))?;
        Ok(())
    }
}

#[async_trait]
impl VaultBackend for FileVaultBackend {
    fn backend_name(&self) -> &'static str {
        "local"
    }

    async fn list_items(&self) -> Result<Vec<VaultItemMeta>, ToolError> {
        let _guard = FILE_VAULT_LOCK.lock().unwrap();
        Ok(self.load_locked()?.iter().map(vault_meta_of).collect())
    }

    async fn get_item(&self, handle: &str) -> Result<VaultItem, ToolError> {
        let _guard = FILE_VAULT_LOCK.lock().unwrap();
        self.load_locked()?
            .into_iter()
            .find(|i| i.handle == handle)
            .map(|i| VaultItem {
                meta: vault_meta_of(&i),
                secret: i.secret,
            })
            .ok_or_else(|| ToolError::NotFound(format!("vault item {handle:?} not found")))
    }

    async fn save_login(
        &self,
        label: &str,
        identifier: &str,
        origin: &str,
        secret: &str,
    ) -> Result<String, ToolError> {
        if secret.is_empty() {
            return Err(ToolError::InvalidParams(
                "Cannot save an empty secret".into(),
            ));
        }
        let _guard = FILE_VAULT_LOCK.lock().unwrap();
        let mut items = self.load_locked()?;
        let handle = uuid::Uuid::new_v4().to_string();
        // Replace an existing login for the same origin+identifier instead of
        // duplicating it.
        items.retain(|i| !(i.origin == origin && i.identifier.as_deref() == Some(identifier)));
        items.push(StoredVaultItem {
            handle: handle.clone(),
            label: label.to_string(),
            kind: VaultItemKind::Login,
            origin: origin.to_string(),
            allowed_origins: Vec::new(),
            identifier: (!identifier.is_empty()).then(|| identifier.to_string()),
            secret: secret.to_string(),
            has_otp: false,
        });
        self.save_locked(&items)?;
        Ok(handle)
    }
}

/// Read a password from the terminal with echo disabled (Unix).
/// Fails when stdin is not an interactive terminal.
#[cfg(unix)]
fn read_masked_password(prompt: &str) -> Result<String, ToolError> {
    use std::io::{BufRead, Write};

    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        return Err(ToolError::ExecutionFailed(
            "save_login needs an interactive terminal to read the password securely; \
             run it from an interactive Hermes session."
                .into(),
        ));
    }
    eprint!("{prompt}");
    std::io::stderr()
        .flush()
        .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

    let fd = libc::STDIN_FILENO;
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut termios) } != 0 {
        return Err(ToolError::ExecutionFailed("tcgetattr failed".into()));
    }
    let original = termios;
    termios.c_lflag &= !(libc::ECHO | libc::ECHONL);
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) } != 0 {
        return Err(ToolError::ExecutionFailed("tcsetattr failed".into()));
    }
    let mut line = String::new();
    let read_res = std::io::stdin().lock().read_line(&mut line);
    unsafe {
        libc::tcsetattr(fd, libc::TCSANOW, &original);
    }
    eprintln!();
    read_res.map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

#[cfg(not(unix))]
fn read_masked_password(_prompt: &str) -> Result<String, ToolError> {
    Err(ToolError::ExecutionFailed(
        "Masked password input is only supported on Unix.".into(),
    ))
}

pub struct BrowserVaultHandler {
    browser: std::sync::Arc<dyn BrowserBackend>,
    vault: std::sync::Arc<dyn VaultBackend>,
}

impl BrowserVaultHandler {
    pub fn new(
        browser: std::sync::Arc<dyn BrowserBackend>,
        vault: std::sync::Arc<dyn VaultBackend>,
    ) -> Self {
        Self { browser, vault }
    }

    /// File-backed vault at the default Hermes home location.
    pub fn with_default_vault(browser: std::sync::Arc<dyn BrowserBackend>) -> Self {
        Self::new(
            browser,
            std::sync::Arc::new(FileVaultBackend::new(FileVaultBackend::default_path())),
        )
    }

    async fn current_page_origin(&self) -> Result<String, ToolError> {
        let value = self
            .browser
            .evaluate_js("window.location.origin", true)
            .await?;
        value
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .or_else(|| value.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| ToolError::ExecutionFailed("Could not read page origin".into()))
    }

    async fn list(&self) -> Result<String, ToolError> {
        let items = self.vault.list_items().await?;
        let mut out = json!({"success": true, "items": items});
        if items.is_empty() {
            out["hint"] = json!(
                "No saved logins. On a login page, call browser_vault action=save_login to ask the user to save one. \
                 Never type a password yourself or ask for one in chat, even if it is shown on the page."
            );
        }
        Ok(out.to_string())
    }

    async fn fill(&self, handle: &str) -> Result<String, ToolError> {
        if handle.is_empty() {
            return Err(ToolError::InvalidParams(
                "fill requires a 'handle' from list".into(),
            ));
        }
        let item = self.vault.get_item(handle).await?;
        let page_origin = self.current_page_origin().await?;
        if !vault_origin_allowed(&item.meta, &page_origin) {
            return Ok(json!({
                "success": false,
                "error": format!(
                    "Refused: page origin {page_origin:?} does not match the vault item's bound origin {:?}.",
                    item.meta.origin,
                ),
            })
            .to_string());
        }
        // Focus the first password field without embedding any secret in JS.
        let focus = self
            .browser
            .evaluate_js(
                "(() => { const el = document.querySelector('input[type=password]'); \
                          if (!el) return 'no-password-field'; el.focus(); el.select(); return 'focused'; })()",
                true,
            )
            .await;
        let focused = focus
            .as_ref()
            .ok()
            .and_then(|v| v.get("result"))
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            == Some("focused");
        if !focused {
            return Ok(json!({
                "success": false,
                "error": "No password field found on the current page.",
            })
            .to_string());
        }
        // Type the secret via Input.insertText: it travels only as a CDP
        // parameter, never inside page JS, argv, or logs.
        if let Err(e) = self.browser.insert_text(&item.secret).await {
            return Ok(json!({"success": false, "error": e.to_string()}).to_string());
        }
        Ok(json!({
            "success": true,
            "filled_fields": 1,
            "kind": "login",
            "origin": page_origin,
        })
        .to_string())
    }

    async fn save_login(&self, params: &Value) -> Result<String, ToolError> {
        let page_origin = self.current_page_origin().await?;
        let label = params
            .get("label")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                page_origin
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
            });
        let identifier = params
            .get("identifier")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // The secret is read via a masked terminal prompt — it never comes
        // from the model or the chat transcript.
        let secret = read_masked_password(&format!("Password for {label} ({page_origin}): "))?;
        if secret.is_empty() {
            return Ok(json!({"success": false, "error": "save_declined"}).to_string());
        }
        let handle = self
            .vault
            .save_login(label, identifier, &page_origin, &secret)
            .await?;
        Ok(json!({
            "success": true,
            "handle": handle,
            "label": label,
            "origin": page_origin,
        })
        .to_string())
    }
}

#[async_trait]
impl ToolHandler for BrowserVaultHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("Missing 'action' parameter".into()))?;
        match action {
            "list" => self.list().await,
            "fill" => {
                self.fill(params.get("handle").and_then(|v| v.as_str()).unwrap_or(""))
                    .await
            }
            "save_login" => self.save_login(&params).await,
            "enter_code" => Ok(json!({
                "success": false,
                "error": "Automatic 2FA code entry is not supported in this build; approve the code on your device.",
            })
            .to_string()),
            "unlock" => {
                if self.vault.needs_unlock() {
                    match self.vault.unlock().await {
                        Ok(()) => Ok(json!({"success": true}).to_string()),
                        Err(e) => Ok(json!({"success": false, "error": e.to_string()}).to_string()),
                    }
                } else {
                    Ok(json!({"success": true, "already_unlocked": true}).to_string())
                }
            }
            other => Ok(json!({
                "success": false,
                "error": format!("Unknown action {other:?}; use list, fill, save_login, enter_code or unlock."),
            })
            .to_string()),
        }
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "action".into(),
            json!({
                "type": "string",
                "enum": ["list", "fill", "save_login", "enter_code", "unlock"],
                "description": "Vault action to perform"
            }),
        );
        props.insert(
            "handle".into(),
            json!({
                "type": "string",
                "description": "fill: a handle from list"
            }),
        );
        props.insert(
            "label".into(),
            json!({
                "type": "string",
                "description": "save_login: short site name (default: the page host)"
            }),
        );
        props.insert(
            "identifier".into(),
            json!({
                "type": "string",
                "description": "save_login: username/login identifier (not a secret)"
            }),
        );
        tool_schema(
            "browser_vault",
            "Use saved logins on the current browser page without secrets entering the conversation. \
             Actions: list (saved logins as handles; passwords are never returned), fill (types the password \
             for a handle; refused unless the page origin matches the item), save_login (asks the user for \
             the login via a masked prompt and stores it). Never type, ask for or repeat a password yourself.",
            JsonSchema::object(props, vec!["action".into()]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockBrowserBackend;
    #[async_trait]
    impl BrowserBackend for MockBrowserBackend {
        async fn navigate(&self, url: &str) -> Result<String, ToolError> {
            Ok(format!("Navigated to {}", url))
        }
        async fn snapshot(&self) -> Result<String, ToolError> {
            Ok("Page snapshot".into())
        }
        async fn click(&self, sel: &str) -> Result<String, ToolError> {
            Ok(format!("Clicked {}", sel))
        }
        async fn r#type(&self, sel: &str, text: &str) -> Result<String, ToolError> {
            Ok(format!("Typed '{}' into {}", text, sel))
        }
        async fn scroll(&self, dir: &str, _amt: Option<u32>) -> Result<String, ToolError> {
            Ok(format!("Scrolled {}", dir))
        }
        async fn go_back(&self) -> Result<String, ToolError> {
            Ok("Went back".into())
        }
        async fn press(&self, key: &str) -> Result<String, ToolError> {
            Ok(format!("Pressed {}", key))
        }
        async fn get_images(&self, sel: Option<&str>) -> Result<String, ToolError> {
            Ok(format!("Images: {:?}", sel))
        }
        async fn vision(&self, inst: &str) -> Result<String, ToolError> {
            Ok(format!("Vision: {}", inst))
        }
        async fn console(&self, action: &str) -> Result<String, ToolError> {
            Ok(format!("Console: {}", action))
        }
    }

    fn backend() -> std::sync::Arc<dyn BrowserBackend> {
        std::sync::Arc::new(MockBrowserBackend)
    }

    #[tokio::test]
    async fn test_browser_navigate() {
        let handler = BrowserNavigateHandler::new(backend());
        let result = handler
            .execute(json!({"url": "https://example.com"}))
            .await
            .unwrap();
        assert!(result.contains("example.com"));
    }

    #[tokio::test]
    async fn test_browser_click() {
        let handler = BrowserClickHandler::new(backend());
        let result = handler.execute(json!({"selector": "#btn"})).await.unwrap();
        assert!(result.contains("#btn"));
    }

    #[tokio::test]
    async fn test_browser_snapshot() {
        let handler = BrowserSnapshotHandler::new(backend());
        let result = handler.execute(json!({})).await.unwrap();
        assert!(result.contains("snapshot"));
    }

    // ------------------------------------------------------------------
    // browser_cdp / browser_dialog / browser_exec / browser_vault
    // ------------------------------------------------------------------

    struct CdpMockBackend {
        calls: std::sync::Mutex<Vec<(String, Value, Option<String>)>>,
    }

    impl CdpMockBackend {
        fn new() -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                calls: std::sync::Mutex::new(Vec::new()),
            })
        }
        fn last_call(&self) -> (String, Value, Option<String>) {
            self.calls.lock().unwrap().last().cloned().unwrap()
        }
    }

    #[async_trait]
    impl BrowserBackend for CdpMockBackend {
        async fn navigate(&self, _url: &str) -> Result<String, ToolError> {
            Ok("navigated".into())
        }
        async fn snapshot(&self) -> Result<String, ToolError> {
            Ok("snapshot".into())
        }
        async fn click(&self, _sel: &str) -> Result<String, ToolError> {
            Ok("clicked".into())
        }
        async fn r#type(&self, _sel: &str, _text: &str) -> Result<String, ToolError> {
            Ok("typed".into())
        }
        async fn scroll(&self, _dir: &str, _amt: Option<u32>) -> Result<String, ToolError> {
            Ok("scrolled".into())
        }
        async fn go_back(&self) -> Result<String, ToolError> {
            Ok("back".into())
        }
        async fn press(&self, _key: &str) -> Result<String, ToolError> {
            Ok("pressed".into())
        }
        async fn get_images(&self, _sel: Option<&str>) -> Result<String, ToolError> {
            Ok("images".into())
        }
        async fn vision(&self, _inst: &str) -> Result<String, ToolError> {
            Ok("vision".into())
        }
        async fn console(&self, _action: &str) -> Result<String, ToolError> {
            Ok("console".into())
        }
        async fn cdp_raw(
            &self,
            method: &str,
            params: Value,
            target_id: Option<&str>,
        ) -> Result<Value, ToolError> {
            self.calls.lock().unwrap().push((
                method.to_string(),
                params,
                target_id.map(|s| s.to_string()),
            ));
            if method == "Runtime.evaluate" {
                return Ok(json!({"result": {"type": "string", "value": "eval-ok"}}));
            }
            Ok(json!({"ok": true, "method": method}))
        }
    }

    struct MockVaultBackend {
        items: Vec<StoredVaultItem>,
    }

    impl MockVaultBackend {
        fn with_item(item: StoredVaultItem) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self { items: vec![item] })
        }
        fn sample_item() -> StoredVaultItem {
            StoredVaultItem {
                handle: "h1".into(),
                label: "Example".into(),
                kind: VaultItemKind::Login,
                origin: "https://example.com".into(),
                allowed_origins: vec![],
                identifier: Some("user@example.com".into()),
                secret: "s3cr3t".into(),
                has_otp: false,
            }
        }
    }

    #[async_trait]
    impl VaultBackend for MockVaultBackend {
        fn backend_name(&self) -> &'static str {
            "mock"
        }
        async fn list_items(&self) -> Result<Vec<VaultItemMeta>, ToolError> {
            Ok(self
                .items
                .iter()
                .map(|i| VaultItemMeta {
                    handle: i.handle.clone(),
                    backend: "mock".into(),
                    label: i.label.clone(),
                    kind: i.kind,
                    origin: i.origin.clone(),
                    allowed_origins: i.allowed_origins.clone(),
                    identifier: i.identifier.clone(),
                    has_otp: i.has_otp,
                })
                .collect())
        }
        async fn get_item(&self, handle: &str) -> Result<VaultItem, ToolError> {
            self.items
                .iter()
                .find(|i| i.handle == handle)
                .map(|i| VaultItem {
                    meta: VaultItemMeta {
                        handle: i.handle.clone(),
                        backend: "mock".into(),
                        label: i.label.clone(),
                        kind: i.kind,
                        origin: i.origin.clone(),
                        allowed_origins: i.allowed_origins.clone(),
                        identifier: i.identifier.clone(),
                        has_otp: i.has_otp,
                    },
                    secret: i.secret.clone(),
                })
                .ok_or_else(|| ToolError::NotFound(format!("no such handle: {handle}")))
        }
        async fn save_login(
            &self,
            _label: &str,
            _identifier: &str,
            _origin: &str,
            _secret: &str,
        ) -> Result<String, ToolError> {
            Ok("new-handle".into())
        }
    }

    /// Backend whose `evaluate_js` reports a fixed page origin (for vault fill tests).
    struct OriginMockBackend {
        inner: std::sync::Arc<CdpMockBackend>,
        origin: String,
    }

    #[async_trait]
    impl BrowserBackend for OriginMockBackend {
        async fn navigate(&self, _url: &str) -> Result<String, ToolError> {
            Ok("navigated".into())
        }
        async fn snapshot(&self) -> Result<String, ToolError> {
            Ok("snapshot".into())
        }
        async fn click(&self, _sel: &str) -> Result<String, ToolError> {
            Ok("clicked".into())
        }
        async fn r#type(&self, _sel: &str, _text: &str) -> Result<String, ToolError> {
            Ok("typed".into())
        }
        async fn scroll(&self, _dir: &str, _amt: Option<u32>) -> Result<String, ToolError> {
            Ok("scrolled".into())
        }
        async fn go_back(&self) -> Result<String, ToolError> {
            Ok("back".into())
        }
        async fn press(&self, _key: &str) -> Result<String, ToolError> {
            Ok("pressed".into())
        }
        async fn get_images(&self, _sel: Option<&str>) -> Result<String, ToolError> {
            Ok("images".into())
        }
        async fn vision(&self, _inst: &str) -> Result<String, ToolError> {
            Ok("vision".into())
        }
        async fn console(&self, _action: &str) -> Result<String, ToolError> {
            Ok("console".into())
        }
        async fn cdp_raw(
            &self,
            method: &str,
            params: Value,
            target_id: Option<&str>,
        ) -> Result<Value, ToolError> {
            // Simulate Runtime.evaluate results for origin reads and the
            // password-field focus probe used by vault fill.
            if method == "Runtime.evaluate" {
                let expr = params
                    .get("expression")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if expr.contains("window.location.origin") {
                    return Ok(json!({"result": {"type": "string", "value": self.origin.clone()}}));
                }
                if expr.contains("input[type=password]") {
                    return Ok(json!({"result": {"type": "string", "value": "focused"}}));
                }
            }
            self.inner.cdp_raw(method, params, target_id).await
        }
    }

    #[test]
    fn test_clamp_cdp_timeout() {
        assert_eq!(clamp_cdp_timeout(None), 30.0);
        assert_eq!(clamp_cdp_timeout(Some(60.0)), 60.0);
        assert_eq!(clamp_cdp_timeout(Some(0.5)), 1.0);
        assert_eq!(clamp_cdp_timeout(Some(500.0)), 300.0);
        assert_eq!(clamp_cdp_timeout(Some(f64::NAN)), 30.0);
        assert_eq!(clamp_cdp_timeout(Some(f64::INFINITY)), 30.0);
    }

    #[test]
    fn test_vault_origin_allowed() {
        let meta = VaultItemMeta {
            handle: "h".into(),
            backend: "local".into(),
            label: "Example".into(),
            kind: VaultItemKind::Login,
            origin: "https://example.com".into(),
            allowed_origins: vec!["https://login.example.com".into()],
            identifier: None,
            has_otp: false,
        };
        assert!(vault_origin_allowed(&meta, "https://example.com"));
        assert!(vault_origin_allowed(&meta, "https://example.com/"));
        assert!(vault_origin_allowed(&meta, "https://login.example.com"));
        assert!(!vault_origin_allowed(&meta, "https://evil.com"));
        assert!(!vault_origin_allowed(&meta, "https://example.com.evil.com"));
        assert!(!vault_origin_allowed(&meta, ""));
        assert!(!vault_origin_allowed(&meta, "http://example.com"));
    }

    #[tokio::test]
    async fn test_browser_cdp_success_envelope() {
        let backend = CdpMockBackend::new();
        let handler = BrowserCdpHandler::new(backend.clone());
        let out = handler
            .execute(json!({"method": "Target.getTargets", "params": {}}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], true);
        assert_eq!(v["method"], "Target.getTargets");
        let (method, _params, target) = backend.last_call();
        assert_eq!(method, "Target.getTargets");
        assert_eq!(target, None);
    }

    #[tokio::test]
    async fn test_browser_cdp_target_id_passthrough() {
        let backend = CdpMockBackend::new();
        let handler = BrowserCdpHandler::new(backend.clone());
        handler
            .execute(json!({
                "method": "Page.navigate",
                "params": {"url": "https://example.com"},
                "target_id": "ABC123",
            }))
            .await
            .unwrap();
        let (method, params, target) = backend.last_call();
        assert_eq!(method, "Page.navigate");
        assert_eq!(target, Some("ABC123".to_string()));
        assert_eq!(params["url"], "https://example.com");
    }

    #[tokio::test]
    async fn test_browser_cdp_missing_method() {
        let handler = BrowserCdpHandler::new(CdpMockBackend::new());
        let err = handler.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn test_browser_cdp_non_object_params() {
        let handler = BrowserCdpHandler::new(CdpMockBackend::new());
        let err = handler
            .execute(json!({"method": "Target.getTargets", "params": [1, 2]}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn test_browser_cdp_unsupported_backend_envelope() {
        // Default cdp_raw impl -> JSON error envelope, not a raised error.
        let handler = BrowserCdpHandler::new(backend());
        let out = handler
            .execute(json!({"method": "Target.getTargets"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], false);
        assert!(v["error"].as_str().unwrap().contains("not supported"));
    }

    #[tokio::test]
    async fn test_browser_dialog_accept_dismiss() {
        let backend = CdpMockBackend::new();
        let handler = BrowserDialogHandler::new(backend.clone());
        let out = handler
            .execute(json!({"action": "accept", "prompt_text": "hello"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], true);
        assert_eq!(v["action"], "accept");
        let (method, params, _) = backend.last_call();
        assert_eq!(method, "Page.handleJavaScriptDialog");
        assert_eq!(params["accept"], true);
        assert_eq!(params["promptText"], "hello");

        let out = handler.execute(json!({"action": "dismiss"})).await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], true);
        let (method, params, _) = backend.last_call();
        assert_eq!(method, "Page.handleJavaScriptDialog");
        assert_eq!(params["accept"], false);
    }

    #[tokio::test]
    async fn test_browser_dialog_bad_action() {
        let handler = BrowserDialogHandler::new(CdpMockBackend::new());
        let err = handler
            .execute(json!({"action": "maybe"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn test_browser_exec_evaluates_js() {
        let backend = CdpMockBackend::new();
        let handler = BrowserExecHandler::new(backend.clone());
        let out = handler
            .execute(json!({"code": "document.title"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], true);
        let (method, params, _) = backend.last_call();
        assert_eq!(method, "Runtime.evaluate");
        assert_eq!(params["expression"], "document.title");
        assert_eq!(params["returnByValue"], true);
    }

    #[tokio::test]
    async fn test_browser_exec_missing_code() {
        let handler = BrowserExecHandler::new(CdpMockBackend::new());
        let err = handler.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn test_vault_list_hides_secrets() {
        let vault = MockVaultBackend::with_item(MockVaultBackend::sample_item());
        let handler = BrowserVaultHandler::new(CdpMockBackend::new(), vault);
        let out = handler.execute(json!({"action": "list"})).await.unwrap();
        assert!(!out.contains("s3cr3t"), "secret leaked into list output");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], true);
        assert_eq!(v["items"][0]["handle"], "h1");
        assert_eq!(v["items"][0]["identifier"], "user@example.com");
    }

    #[tokio::test]
    async fn test_vault_fill_origin_mismatch_refused() {
        let vault = MockVaultBackend::with_item(MockVaultBackend::sample_item());
        let browser = std::sync::Arc::new(OriginMockBackend {
            inner: CdpMockBackend::new(),
            origin: "https://evil.com".into(),
        });
        let handler = BrowserVaultHandler::new(browser, vault);
        let out = handler
            .execute(json!({"action": "fill", "handle": "h1"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], false);
        assert!(v["error"].as_str().unwrap().contains("does not match"));
        assert!(!out.contains("s3cr3t"));
    }

    #[tokio::test]
    async fn test_vault_fill_success_no_secret_leak() {
        let vault = MockVaultBackend::with_item(MockVaultBackend::sample_item());
        let inner = CdpMockBackend::new();
        let browser = std::sync::Arc::new(OriginMockBackend {
            inner: inner.clone(),
            origin: "https://example.com".into(),
        });
        let handler = BrowserVaultHandler::new(browser, vault);
        let out = handler
            .execute(json!({"action": "fill", "handle": "h1"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], true);
        assert_eq!(v["filled_fields"], 1);
        assert_eq!(v["origin"], "https://example.com");
        assert!(!out.contains("s3cr3t"), "secret leaked into fill output");
        // The secret must travel via Input.insertText, never inside a JS expression.
        let (method, params, _) = inner.last_call();
        assert_eq!(method, "Input.insertText");
        assert_eq!(params["text"], "s3cr3t");
    }

    #[tokio::test]
    async fn test_vault_fill_unknown_handle() {
        let vault = MockVaultBackend::with_item(MockVaultBackend::sample_item());
        let browser = std::sync::Arc::new(OriginMockBackend {
            inner: CdpMockBackend::new(),
            origin: "https://example.com".into(),
        });
        let handler = BrowserVaultHandler::new(browser, vault);
        let err = handler
            .execute(json!({"action": "fill", "handle": "nope"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_vault_unknown_action() {
        let vault = MockVaultBackend::with_item(MockVaultBackend::sample_item());
        let handler = BrowserVaultHandler::new(CdpMockBackend::new(), vault);
        let out = handler.execute(json!({"action": "explode"})).await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], false);
    }

    #[tokio::test]
    async fn test_vault_enter_code_not_supported() {
        let vault = MockVaultBackend::with_item(MockVaultBackend::sample_item());
        let handler = BrowserVaultHandler::new(CdpMockBackend::new(), vault);
        let out = handler
            .execute(json!({"action": "enter_code", "handle": "h1"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], false);
    }

    #[test]
    fn test_file_vault_roundtrip_no_secret_in_list() {
        let dir = tempfile::tempdir().unwrap();
        // `path` is the legacy plaintext location; the encrypted files live
        // in its parent directory.
        let legacy_path = dir.path().join("vault.json");
        let backend = FileVaultBackend::new(legacy_path.clone());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt
            .block_on(backend.save_login(
                "Example",
                "user@example.com",
                "https://example.com",
                "s3cr3t",
            ))
            .unwrap();
        assert!(!handle.is_empty());
        let items = rt.block_on(backend.list_items()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].identifier.as_deref(), Some("user@example.com"));
        // The list output never includes the secret.
        let list_json = serde_json::to_string(&items).unwrap();
        assert!(!list_json.contains("s3cr3t"));
        // But the secret round-trips for fill.
        let item = rt.block_on(backend.get_item(&handle)).unwrap();
        assert_eq!(item.secret, "s3cr3t");
        // Saving again for the same origin+identifier replaces instead of duplicating.
        rt.block_on(backend.save_login(
            "Example",
            "user@example.com",
            "https://example.com",
            "n3w",
        ))
        .unwrap();
        let items = rt.block_on(backend.list_items()).unwrap();
        assert_eq!(items.len(), 1);
        // The encrypted store holds no plaintext secret, and no legacy
        // plaintext file was ever created.
        let enc_path = dir.path().join("vault.json.enc");
        assert!(enc_path.exists());
        let enc_bytes = std::fs::read(&enc_path).unwrap();
        assert!(!enc_bytes.windows(6).any(|w| w == b"s3cr3t"));
        assert!(!legacy_path.exists());
        // Key + encrypted file are owner-only on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode =
                |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&enc_path), 0o600);
            assert_eq!(mode(&dir.path().join("vault.key")), 0o600);
        }
    }

    #[test]
    fn test_vault_crypto_keystream_deterministic() {
        let a = vault_crypto::keystream(&[9u8; 32], &[7u8; 16], 100);
        let b = vault_crypto::keystream(&[9u8; 32], &[7u8; 16], 100);
        assert_eq!(a, b);
        assert_eq!(a.len(), 100);
        // Different nonce -> different keystream.
        assert_ne!(a, vault_crypto::keystream(&[9u8; 32], &[8u8; 16], 100));
        // Different key -> different keystream.
        assert_ne!(a, vault_crypto::keystream(&[10u8; 32], &[7u8; 16], 100));
    }

    #[test]
    fn test_vault_crypto_ct_eq() {
        assert!(vault_crypto::ct_eq(&[1, 2, 3], &[1, 2, 3]));
        assert!(!vault_crypto::ct_eq(&[1, 2, 3], &[1, 2, 4]));
        assert!(!vault_crypto::ct_eq(&[1, 2], &[1, 2, 3]));
    }

    #[test]
    fn test_vault_envelope_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FileVaultBackend::new(dir.path().join("vault.json"));
        let env = backend.encrypt(b"hello vault").unwrap();
        assert_eq!(env.format, "hermes-vault/1");
        assert_eq!(env.nonce.len(), 32);
        assert_eq!(backend.decrypt(&env).unwrap(), b"hello vault");
        // No plaintext in the stored payload.
        let payload = hex_decode(&env.payload).unwrap();
        assert!(!payload.windows(5).any(|w| w == b"hello"));
        // Unknown envelope format is rejected.
        let mut bad_env = backend.encrypt(b"x").unwrap();
        bad_env.format = "hermes-vault/99".into();
        assert!(backend.decrypt(&bad_env).is_err());
    }

    #[test]
    fn test_vault_tamper_detection() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FileVaultBackend::new(dir.path().join("vault.json"));
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(backend.save_login("Example", "u", "https://example.com", "s3cr3t"))
            .unwrap();
        let enc_path = dir.path().join("vault.json.enc");

        // Tamper with the ciphertext region and, separately, with the tag.
        let pristine = std::fs::read(&enc_path).unwrap();
        for (tamper, idx_fn) in [
            ("ciphertext", 0usize),
            ("tag", usize::MAX), // resolved below to the last hex char
        ] {
            let mut env: VaultEnvelope = serde_json::from_slice(&pristine).unwrap();
            let mut chars: Vec<char> = env.payload.chars().collect();
            let idx = if idx_fn == usize::MAX {
                chars.len() - 1
            } else {
                idx_fn
            };
            chars[idx] = if chars[idx] == 'a' { 'b' } else { 'a' };
            env.payload = chars.into_iter().collect();
            std::fs::write(&enc_path, serde_json::to_vec(&env).unwrap()).unwrap();
            let err = rt.block_on(backend.list_items()).unwrap_err();
            assert!(
                err.to_string().contains("decryption failed"),
                "tamper {tamper} was not detected: {err}"
            );
            // Restore the pristine store for the next iteration.
            std::fs::write(&enc_path, &pristine).unwrap();
        }
        // Store is usable again after re-saving.
        assert_eq!(rt.block_on(backend.list_items()).unwrap().len(), 1);
    }

    #[test]
    fn test_vault_permissions_enforced() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let backend = FileVaultBackend::new(dir.path().join("vault.json"));
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(backend.save_login("Example", "u", "https://example.com", "pw"))
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode =
                |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(dir.path()), 0o700, "vault dir");
            assert_eq!(mode(&dir.path().join("vault.key")), 0o600, "vault.key");
            assert_eq!(
                mode(&dir.path().join("vault.json.enc")),
                0o600,
                "vault.json.enc"
            );
        }
    }

    #[test]
    fn test_vault_key_file_reused_across_instances() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let backend = FileVaultBackend::new(dir.path().join("vault.json"));
        rt.block_on(backend.save_login("Example", "u", "https://example.com", "pw"))
            .unwrap();
        let key_before = std::fs::read(dir.path().join("vault.key")).unwrap();
        assert_eq!(key_before.len(), 64);
        // A fresh backend instance reuses the on-disk key: items still decrypt.
        let backend2 = FileVaultBackend::new(dir.path().join("vault.json"));
        let items = rt.block_on(backend2.list_items()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(
            std::fs::read(dir.path().join("vault.key")).unwrap(),
            key_before
        );
    }

    #[test]
    fn test_vault_migration_from_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("vault.json");
        let backend = FileVaultBackend::new(legacy_path.clone());
        // Simulate an old plaintext vault (cleartext `secret`, legacy shape).
        let legacy = vec![StoredVaultItem {
            handle: "legacy1".into(),
            label: "Old".into(),
            kind: VaultItemKind::Login,
            origin: "https://old.example".into(),
            allowed_origins: vec![],
            identifier: Some("olduser".into()),
            secret: "oldsecret".into(),
            has_otp: false,
        }];
        std::fs::write(&legacy_path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let items = rt.block_on(backend.list_items()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].handle, "legacy1");
        // The secret survived migration and is now stored encrypted.
        let item = rt.block_on(backend.get_item("legacy1")).unwrap();
        assert_eq!(item.secret, "oldsecret");
        // The legacy file was renamed (never deleted silently).
        assert!(!legacy_path.exists());
        assert!(dir.path().join("vault.json.migrated").exists());
        let enc = std::fs::read(dir.path().join("vault.json.enc")).unwrap();
        assert!(!enc.windows(9).any(|w| w == b"oldsecret"));
        // Second load is idempotent: no duplicates, no errors.
        let items = rt.block_on(backend.list_items()).unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn test_vault_migration_merges_with_encrypted_store() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("vault.json");
        let backend = FileVaultBackend::new(legacy_path.clone());
        let rt = tokio::runtime::Runtime::new().unwrap();
        // An item already in the encrypted store...
        let new_handle = rt
            .block_on(backend.save_login("New", "n", "https://new.example", "newsecret"))
            .unwrap();
        // ...plus a legacy plaintext file with the same handle (dedup) and a
        // genuinely old one.
        let legacy = vec![
            StoredVaultItem {
                handle: new_handle.clone(),
                label: "New".into(),
                kind: VaultItemKind::Login,
                origin: "https://new.example".into(),
                allowed_origins: vec![],
                identifier: Some("n".into()),
                secret: "stale".into(),
                has_otp: false,
            },
            StoredVaultItem {
                handle: "legacy2".into(),
                label: "Old".into(),
                kind: VaultItemKind::Login,
                origin: "https://old.example".into(),
                allowed_origins: vec![],
                identifier: Some("o".into()),
                secret: "oldsecret".into(),
                has_otp: false,
            },
        ];
        std::fs::write(&legacy_path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let items = rt.block_on(backend.list_items()).unwrap();
        assert_eq!(items.len(), 2);
        // The encrypted copy wins for the duplicated handle.
        assert_eq!(
            rt.block_on(backend.get_item(&new_handle)).unwrap().secret,
            "newsecret"
        );
    }

    #[test]
    fn test_vault_masked_prompt_fail_closed_without_tty() {
        // The test harness stdin is not a TTY: reading a masked password
        // must fail closed instead of echoing or blocking.
        let err = read_masked_password("Password: ").unwrap_err();
        assert!(
            matches!(err, ToolError::ExecutionFailed(_)),
            "expected fail-closed ExecutionFailed, got {err}"
        );
    }

    #[tokio::test]
    async fn test_vault_file_backend_keyfile_based_no_unlock() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FileVaultBackend::new(dir.path().join("vault.json"));
        // Key-file based: no unlock gate (unlike external manager backends).
        assert!(!backend.needs_unlock());
        assert!(backend.is_unlocked());
        assert!(backend.unlock().await.is_err());
    }

    #[test]
    fn test_browser_cdp_schema() {
        let handler = BrowserCdpHandler::new(CdpMockBackend::new());
        let schema = handler.schema();
        assert_eq!(schema.name, "browser_cdp");
    }

    #[test]
    fn test_browser_dialog_schema() {
        let handler = BrowserDialogHandler::new(CdpMockBackend::new());
        let schema = handler.schema();
        assert_eq!(schema.name, "browser_dialog");
    }

    #[test]
    fn test_browser_exec_schema() {
        let handler = BrowserExecHandler::new(CdpMockBackend::new());
        let schema = handler.schema();
        assert_eq!(schema.name, "browser_exec");
    }

    #[test]
    fn test_browser_vault_schema() {
        let vault = MockVaultBackend::with_item(MockVaultBackend::sample_item());
        let handler = BrowserVaultHandler::new(CdpMockBackend::new(), vault);
        let schema = handler.schema();
        assert_eq!(schema.name, "browser_vault");
    }
}
