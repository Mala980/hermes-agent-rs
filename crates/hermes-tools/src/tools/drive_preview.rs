//! `drive_preview`: drive the in-app browser / preview pane in the Hermes desktop GUI.
//!
//! NOTE on naming: despite the name, the reference implementation's
//! `drive_preview` is NOT a file-drive preview — it dispatches interactions
//! (click, type, scroll, …) to the web page open in the desktop preview pane
//! via an injected renderer callback. This port preserves that behavior.
//!
//! The bridge is injected by the desktop gateway (`preview.act.request` /
//! `preview.act.respond` round-trip). Without it every call fails with a
//! clear error. All argument validation below is pure and unit-tested.

use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Value};

use hermes_core::{tool_schema, JsonSchema, ToolError, ToolHandler, ToolSchema};

/// Valid actions, in the reference implementation's order.
pub const ACTIONS: &[&str] = &[
    "elements", "click", "hover", "type", "scroll", "press", "strobe", "back", "forward",
    "reload",
];

/// Valid `to` targets for `scroll`.
pub const SCROLL_TO: &[&str] = &["top", "bottom"];

/// Verbs that need something to act on — a ref from the last inventory or a
/// raw CSS selector. `scroll` is deliberately absent: bare, it scrolls the page.
const NEEDS_TARGET: &[&str] = &["click", "hover", "type", "press"];

// ---------------------------------------------------------------------------
// Bridge (injected by the desktop gateway)
// ---------------------------------------------------------------------------

/// Bridge to the desktop renderer's preview pane.
#[async_trait]
pub trait PreviewBridge: Send + Sync {
    /// Dispatch one interaction request; returns the renderer's raw JSON outcome.
    async fn act(&self, request: Value) -> Result<String, ToolError>;
}

// ---------------------------------------------------------------------------
// Pure validation (mirrors drive_preview_tool.py)
// ---------------------------------------------------------------------------

/// Validated, normalized request fields for one preview interaction.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewRequest {
    pub action: String,
    pub target_ref: Option<String>,
    pub selector: Option<String>,
    pub text: Option<String>,
    pub key: Option<String>,
    pub submit: Option<bool>,
    pub amount: Option<i64>,
    pub to: Option<String>,
    pub max: Option<i64>,
    pub full: Option<bool>,
    pub allow_shortcut: Option<bool>,
}

impl PreviewRequest {
    /// Serialize to the JSON object the renderer callback expects.
    /// `None` fields are omitted, mirroring the Python dispatcher.
    pub fn to_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("action".into(), json!(self.action));
        let mut opt = |k: &str, v: Option<Value>| {
            if let Some(v) = v {
                map.insert(k.into(), v);
            }
        };
        opt("ref", self.target_ref.clone().map(Value::String));
        opt("selector", self.selector.clone().map(Value::String));
        opt("text", self.text.clone().map(Value::String));
        opt("key", self.key.clone().map(Value::String));
        opt("submit", self.submit.map(Value::Bool));
        opt("full", self.full.map(Value::Bool));
        opt("to", self.to.clone().map(Value::String));
        opt(
            "allow_shortcut",
            self.allow_shortcut.map(Value::Bool),
        );
        opt("amount", self.amount.map(|n| json!(n)));
        opt("max", self.max.map(|n| json!(n)));
        Value::Object(map)
    }
}

/// Validate raw string params into a [`PreviewRequest`].
///
/// `get_int` parses the integer-ish params (`amount`, `max`); anything else is
/// passed through as an optional string/bool.
pub fn validate_request(
    action: &str,
    target_ref: Option<String>,
    selector: Option<String>,
    text: Option<String>,
    key: Option<String>,
    submit: Option<bool>,
    amount: Option<Value>,
    to: Option<String>,
    max: Option<Value>,
    full: Option<bool>,
    allow_shortcut: Option<bool>,
) -> Result<PreviewRequest, ToolError> {
    let verb = action.trim().to_lowercase();
    if !ACTIONS.contains(&verb.as_str()) {
        return Err(ToolError::InvalidParams(format!(
            "action must be one of: {}.",
            ACTIONS.join(", ")
        )));
    }
    if NEEDS_TARGET.contains(&verb.as_str()) && target_ref.is_none() && selector.is_none() {
        return Err(ToolError::InvalidParams(format!(
            "{verb} needs a ref from action='elements' (e.g. 'btn-sign-in') or a CSS selector."
        )));
    }
    if verb == "type" && text.is_none() {
        return Err(ToolError::InvalidParams("type needs the text to enter.".into()));
    }
    if verb == "press" && key.as_deref().map(str::is_empty).unwrap_or(true) {
        return Err(ToolError::InvalidParams(
            "press needs a key, e.g. 'Enter' or 'Escape'.".into(),
        ));
    }
    if let Some(t) = &to {
        if !SCROLL_TO.contains(&t.as_str()) {
            return Err(ToolError::InvalidParams(format!(
                "to must be one of: {}.",
                SCROLL_TO.join(", ")
            )));
        }
    }
    let parse_int = |v: Option<Value>, name: &str| -> Result<Option<i64>, ToolError> {
        match v {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(n)) => n.as_i64().ok_or_else(|| {
                ToolError::InvalidParams(format!("{name} must be an integer."))
            }).map(Some),
            Some(_) => Err(ToolError::InvalidParams(format!("{name} must be an integer."))),
        }
    };
    let amount = parse_int(amount, "amount")?;
    let max = parse_int(max, "max")?;
    Ok(PreviewRequest {
        action: verb,
        target_ref,
        selector,
        text,
        key,
        submit,
        amount,
        to,
        max,
        full,
        allow_shortcut,
    })
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// Drive the desktop preview pane. Requires an injected [`PreviewBridge`].
pub struct DrivePreviewHandler {
    bridge: Option<Arc<dyn PreviewBridge>>,
}

impl DrivePreviewHandler {
    pub fn new(bridge: Option<Arc<dyn PreviewBridge>>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl ToolHandler for DrivePreviewHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let req = validate_request(
            params.get("action").and_then(|v| v.as_str()).unwrap_or(""),
            params.get("ref").and_then(|v| v.as_str()).map(str::to_string),
            params.get("selector").and_then(|v| v.as_str()).map(str::to_string),
            params.get("text").and_then(|v| v.as_str()).map(str::to_string),
            params.get("key").and_then(|v| v.as_str()).map(str::to_string),
            params.get("submit").and_then(|v| v.as_bool()),
            params.get("amount").cloned(),
            params.get("to").and_then(|v| v.as_str()).map(str::to_string),
            params.get("max").cloned(),
            params.get("full").and_then(|v| v.as_bool()),
            params.get("allow_shortcut").and_then(|v| v.as_bool()),
        )?;
        let bridge = self.bridge.as_ref().ok_or_else(|| {
            ToolError::ExecutionFailed(
                "drive_preview is only available in the Hermes desktop app: no preview bridge is wired.".into(),
            )
        })?;
        let raw = bridge.act(req.to_json()).await?;
        if raw.trim().is_empty() {
            return Err(ToolError::ExecutionFailed(
                "No GUI window answered with a page: no preview tab is open. \
                 Open a page with desktop_preview first."
                    .into(),
            ));
        }
        Ok(raw)
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "action".into(),
            json!({
                "type": "string",
                "enum": ACTIONS,
                "description": "Start with 'elements'."
            }),
        );
        props.insert(
            "ref".into(),
            json!({"type": "string", "description": "Element ref from an earlier elements call."}),
        );
        props.insert(
            "selector".into(),
            json!({"type": "string", "description": "CSS selector fallback. Prefer ref."}),
        );
        props.insert(
            "text".into(),
            json!({"type": "string", "description": "type: the text."}),
        );
        props.insert(
            "submit".into(),
            json!({"type": "boolean", "description": "type: press Enter + submit the form after."}),
        );
        props.insert(
            "key".into(),
            json!({"type": "string", "description": "press: key name ('Enter', 'Escape', 'ArrowDown')."}),
        );
        props.insert(
            "allow_shortcut".into(),
            json!({"type": "boolean", "description": "press: allow a printable key on body/html. Off by default."}),
        );
        props.insert(
            "amount".into(),
            json!({"type": "integer", "description": "scroll: pixels (negative = up; default ~one screen)."}),
        );
        props.insert(
            "to".into(),
            json!({"type": "string", "enum": SCROLL_TO, "description": "scroll: jump to top/bottom instead."}),
        );
        props.insert(
            "max".into(),
            json!({"type": "integer", "description": "elements: cap the inventory."}),
        );
        props.insert(
            "full".into(),
            json!({"type": "boolean", "description": "elements: full re-read instead of a delta. Rarely needed."}),
        );
        tool_schema(
            "drive_preview",
            "Use the web page open in the desktop preview pane: log in, fill forms, click through flows. ALWAYS start with action='elements' — it inventories clickable/typable things as refs; act by ref, not guessed selectors. Actions: elements, click, hover, type, scroll, press, strobe, back/forward/reload.",
            JsonSchema::object(props, vec!["action".into()]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(action: &str) -> Result<PreviewRequest, ToolError> {
        validate_request(action, None, None, None, None, None, None, None, None, None, None)
    }

    #[test]
    fn rejects_unknown_action() {
        assert!(req("fly").is_err());
    }

    #[test]
    fn action_is_case_insensitive_and_trimmed() {
        let r = validate_request(
            "  CLICK ",
            Some("btn-x".into()),
            None, None, None, None, None, None, None, None, None,
        )
        .unwrap();
        assert_eq!(r.action, "click");
    }

    #[test]
    fn click_needs_ref_or_selector() {
        assert!(req("click").is_err());
        assert!(validate_request(
            "click", None, Some("#x".into()), None, None, None, None, None, None, None, None
        )
        .is_ok());
    }

    #[test]
    fn type_needs_text() {
        assert!(validate_request(
            "type", Some("f".into()), None, None, None, None, None, None, None, None, None
        )
        .is_err());
    }

    #[test]
    fn press_needs_key() {
        assert!(validate_request(
            "press", Some("b".into()), None, None, None, None, None, None, None, None, None
        )
        .is_err());
    }

    #[test]
    fn scroll_to_validated() {
        assert!(validate_request(
            "scroll", None, None, None, None, None, None, Some("sideways".into()), None, None, None
        )
        .is_err());
        assert!(validate_request(
            "scroll", None, None, None, None, None, None, Some("top".into()), None, None, None
        )
        .is_ok());
    }

    #[test]
    fn amount_must_be_integer() {
        assert!(validate_request(
            "scroll", None, None, None, None, None, Some(json!("far")), None, None, None, None
        )
        .is_err());
    }

    #[test]
    fn to_json_omits_nones() {
        let r = req("elements").unwrap();
        let v = r.to_json();
        assert_eq!(v["action"], "elements");
        assert!(v.get("ref").is_none());
        assert!(v.get("amount").is_none());
    }

    #[test]
    fn schema_name_matches_original() {
        assert_eq!(DrivePreviewHandler::new(None).schema().name, "drive_preview");
    }
}
