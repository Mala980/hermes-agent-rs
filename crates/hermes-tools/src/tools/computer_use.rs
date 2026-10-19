//! computer_use tool — universal desktop control via cua-driver.
//!
//! Port of `tools/computer_use_tool.py` + `tools/computer_use/` (Python).
//! The Python implementation drives cua-driver through a stateful stdio MCP
//! session; this port uses a simpler stateless approach: one
//! `cua-driver call <tool> <json-args>` subprocess invocation per action.
//!
//! Driver resolution: `HERMES_CUA_DRIVER_CMD` env var, then `cua-driver` on
//! `PATH`. Without a driver binary every call fails with a clear install hint.

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;

use hermes_core::{tool_schema, JsonSchema, ToolError, ToolHandler, ToolSchema};

// ---------------------------------------------------------------------------
// Driver resolution
// ---------------------------------------------------------------------------

/// Env var overriding the cua-driver binary location.
pub const DRIVER_CMD_ENV: &str = "HERMES_CUA_DRIVER_CMD";
/// Default driver binary name looked up on PATH.
pub const DRIVER_DEFAULT_CMD: &str = "cua-driver";
/// Per-call subprocess timeout.
const DRIVER_TIMEOUT_SECS: u64 = 90;
/// Install hint surfaced when no driver binary is found.
const DRIVER_INSTALL_HINT: &str = "cua-driver is not installed. Install it with \
    `hermes computer-use install`, or point HERMES_CUA_DRIVER_CMD at the driver binary.";

/// Resolve the cua-driver binary: explicit env override first, then PATH lookup.
pub fn resolve_driver_cmd() -> Option<String> {
    if let Ok(cmd) = std::env::var(DRIVER_CMD_ENV) {
        let cmd = cmd.trim();
        if !cmd.is_empty() {
            return Some(cmd.to_string());
        }
    }
    find_on_path(DRIVER_DEFAULT_CMD)
}

/// Look up `name` on PATH (tries the `.exe` suffix on Windows too).
fn find_on_path(name: &str) -> Option<String> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return candidate.to_str().map(|s| s.to_string());
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{}.exe", name));
            if is_executable_file(&exe) {
                return exe.to_str().map(|s| s.to_string());
            }
        }
    }
    None
}

fn is_executable_file(path: &std::path::Path) -> bool {
    path.is_file()
}

// ---------------------------------------------------------------------------
// Verdict classification (pure logic — mirrors `_classify_action_result`)
// ---------------------------------------------------------------------------

/// Next ladder step from the driver's semantic evidence, in precedence order.
/// Escalation is advisory: it never overrides a confirmed effect nor licenses
/// repeating input.
pub fn classify_verdict(result: &Value) -> Value {
    let ok = result.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let effect = result.get("effect").and_then(|v| v.as_str());
    let verified = result
        .get("verified")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let code = result.get("code").and_then(|v| v.as_str());

    if effect == Some("confirmed") || verified {
        return json!({"decision": "done"});
    }
    if effect == Some("unverifiable") {
        return json!({
            "decision": "verify_fresh_state",
            "hint": "Input was delivered but not confirmed. Re-capture and check the result \
                     BEFORE any retry — do not repeat the input on an escalation recommendation alone."
        });
    }
    if effect == Some("suspected_noop") || !ok || code.is_some() {
        // Zero delivery on the driver's own partial-delivery verdict: the field swallowed
        // every synthetic keystroke (trusted-event checks on web inputs do this). Neither
        // rung of the delivery ladder can fix a target that drops events at the source —
        // the AX set_value path writes the value directly and bypasses event filtering.
        let meta = result.get("meta");
        let delivered = meta
            .and_then(|m| m.get("delivered_chars"))
            .and_then(|v| v.as_i64());
        let requested = meta
            .and_then(|m| m.get("requested_chars"))
            .and_then(|v| v.as_i64());
        if result.get("action").and_then(|v| v.as_str()) == Some("type")
            && code == Some("type_text_incomplete")
            && requested.unwrap_or(0) > 0
            && delivered.unwrap_or(0) <= 0
        {
            return json!({
                "decision": "escalate",
                "recommended": "set_value",
                "hint": "0 characters landed: this input drops synthetic keystrokes, so no delivery \
                         rung will fix it. Climb to the set_value action on the field's element index \
                         instead — it is an action, not a delivery mode, and sets the value through the \
                         accessibility API, bypassing event filtering. Re-capture first if the index is stale."
            });
        }
        let mut verdict = json!({
            "decision": "escalate",
            "hint": "The input likely did not land. Climb one rung following `recommended`: 'px' → \
                     re-issue by coordinate; 'foreground' (or a failed pixel click) → re-issue with \
                     delivery_mode='foreground' (separate approval). Do not predict the rung from the \
                     app being Electron/Chromium — react to this signal."
        });
        if let Some(rec) = result
            .get("escalation")
            .and_then(|e| e.get("recommended"))
            .and_then(|r| r.as_str())
        {
            verdict["recommended"] = json!(rec);
        }
        return verdict;
    }
    json!({
        "decision": "verify_fresh_state",
        "hint": "Transport succeeded but the effect is unproven. Re-capture and confirm before continuing."
    })
}

// ---------------------------------------------------------------------------
// Action → driver call mapping (pure logic)
// ---------------------------------------------------------------------------

/// Nearest real action suggested for unknown action names (never aliases input).
const ACTION_SUGGESTIONS: &[(&str, &str)] = &[
    ("hotkey", "key"),
    ("press_key", "key"),
    ("keypress", "key"),
    ("key_combo", "key"),
    ("shortcut", "key"),
    ("type_text", "type"),
    ("input_text", "type"),
    ("screenshot", "capture"),
    ("get_window_state", "capture"),
    ("left_click", "click"),
    ("mouse_click", "click"),
];

/// The 14 supported actions.
pub const ACTIONS: &[&str] = &[
    "capture",
    "click",
    "double_click",
    "right_click",
    "middle_click",
    "drag",
    "scroll",
    "type",
    "key",
    "set_value",
    "wait",
    "list_apps",
    "list_windows",
    "focus_app",
];

/// Input actions route through the delivery ladder (background → foreground).
fn is_input_action(action: &str) -> bool {
    matches!(
        action,
        "click"
            | "double_click"
            | "right_click"
            | "middle_click"
            | "drag"
            | "scroll"
            | "type"
            | "key"
            | "set_value"
    )
}

fn get_str(params: &Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn get_int(params: &Value, key: &str) -> Option<i64> {
    params.get(key).and_then(|v| v.as_i64())
}

fn get_coord(params: &Value, key: &str) -> Option<(i64, i64)> {
    let arr = params.get(key)?.as_array()?;
    if arr.len() != 2 {
        return None;
    }
    Some((arr[0].as_i64()?, arr[1].as_i64()?))
}

fn get_bool(params: &Value, key: &str) -> bool {
    params.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Map a tool `action` + params to a `(driver_tool, driver_args)` pair.
/// Pure logic — no subprocess is spawned here.
pub fn build_driver_call(action: &str, params: &Value) -> Result<(String, Value), ToolError> {
    // Thread delivery ladder kwargs through every input action.
    let mut delivery = json!({});
    if is_input_action(action) {
        if let Some(mode) = get_str(params, "delivery_mode") {
            delivery["delivery_mode"] = json!(mode);
        }
        if get_bool(params, "bring_to_front") {
            delivery["bring_to_front"] = json!(true);
        }
    }
    let mut args = delivery.clone();

    let tool: &str = match action {
        "capture" => {
            args["mode"] = json!(params.get("mode").and_then(|v| v.as_str()).unwrap_or("som"));
            if let Some(app) = get_str(params, "app") {
                args["app"] = json!(app);
            }
            if let Some(pid) = get_int(params, "pid") {
                args["pid"] = json!(pid);
            }
            if let Some(wid) = get_int(params, "window_id") {
                args["window_id"] = json!(wid);
            }
            "screenshot"
        }
        "click" | "double_click" => {
            set_pointer_target(&mut args, params)?;
            args["button"] = json!("left");
            if let Some(mods) = params.get("modifiers") {
                args["modifier"] = mods.clone();
            }
            if action == "double_click" {
                "double_click"
            } else {
                "click"
            }
        }
        "right_click" => {
            set_pointer_target(&mut args, params)?;
            args["button"] = json!("right");
            if let Some(mods) = params.get("modifiers") {
                args["modifier"] = mods.clone();
            }
            "click"
        }
        "middle_click" => {
            set_pointer_target(&mut args, params)?;
            args["button"] = json!("middle");
            if let Some(mods) = params.get("modifiers") {
                args["modifier"] = mods.clone();
            }
            "click"
        }
        "drag" => {
            let from_el = get_int(params, "from_element");
            let to_el = get_int(params, "to_element");
            let from_xy = get_coord(params, "from_coordinate");
            let to_xy = get_coord(params, "to_coordinate");
            match (from_el, to_el, from_xy, to_xy) {
                (Some(f), Some(t), _, _) => {
                    args["from_element"] = json!(f);
                    args["to_element"] = json!(t);
                }
                (_, _, Some((fx, fy)), Some((tx, ty))) => {
                    args["from_x"] = json!(fx);
                    args["from_y"] = json!(fy);
                    args["to_x"] = json!(tx);
                    args["to_y"] = json!(ty);
                }
                _ => {
                    return Err(ToolError::InvalidParams(
                        "drag requires from_element/to_element or from_coordinate/to_coordinate."
                            .into(),
                    ))
                }
            }
            args["button"] = json!(get_str(params, "button").unwrap_or_else(|| "left".into()));
            if let Some(mods) = params.get("modifiers") {
                args["modifier"] = mods.clone();
            }
            "drag"
        }
        "scroll" => {
            let direction = get_str(params, "direction").ok_or_else(|| {
                ToolError::InvalidParams("scroll requires `direction` (up|down|left|right).".into())
            })?;
            args["direction"] = json!(direction);
            let amount = params
                .get("amount")
                .and_then(|v| v.as_i64())
                .unwrap_or(3)
                .clamp(1, 50);
            args["amount"] = json!(amount);
            if let Some(el) = get_int(params, "element") {
                args["element_index"] = json!(el);
            } else if let Some((x, y)) = get_coord(params, "coordinate") {
                args["x"] = json!(x);
                args["y"] = json!(y);
            }
            "scroll"
        }
        "type" => {
            let text = get_str(params, "text").unwrap_or_default();
            args["text"] = json!(text);
            "type_text"
        }
        "key" => {
            let keys = get_str(params, "keys").ok_or_else(|| {
                ToolError::InvalidParams("key requires `keys` (e.g. 'cmd+s', 'escape').".into())
            })?;
            args["key"] = json!(keys);
            "press_key"
        }
        "set_value" => {
            let value = get_str(params, "value")
                .ok_or_else(|| ToolError::InvalidParams("set_value requires `value`.".into()))?;
            args["value"] = json!(value);
            if let Some(el) = get_int(params, "element") {
                args["element_index"] = json!(el);
            }
            "set_value"
        }
        "focus_app" => {
            let app = get_str(params, "app")
                .ok_or_else(|| ToolError::InvalidParams("focus_app requires `app`.".into()))?;
            args["app"] = json!(app);
            args["raise_window"] = json!(get_bool(params, "raise_window"));
            "focus_app"
        }
        "wait" => {
            // Handled locally (no driver call); validated here.
            return Err(ToolError::InvalidParams("__wait__".into()));
        }
        "list_apps" => "list_apps",
        "list_windows" => "list_windows",
        other => {
            let hint = ACTION_SUGGESTIONS
                .iter()
                .find(|(k, _)| *k == other)
                .map(|(_, v)| {
                    format!(
                        " — did you mean '{}'? See the action enum in the tool schema.",
                        v
                    )
                })
                .unwrap_or_default();
            return Err(ToolError::InvalidParams(format!(
                "unknown action '{}'{}",
                other, hint
            )));
        }
    };
    Ok((tool.to_string(), args))
}

/// Set element_index or x/y pointer targeting on driver args.
fn set_pointer_target(args: &mut Value, params: &Value) -> Result<(), ToolError> {
    if let Some(el) = get_int(params, "element") {
        args["element_index"] = json!(el);
        Ok(())
    } else if let Some((x, y)) = get_coord(params, "coordinate") {
        args["x"] = json!(x);
        args["y"] = json!(y);
        Ok(())
    } else {
        Err(ToolError::InvalidParams(
            "click requires element= or coordinate=[x, y].".into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// Tool for universal desktop control via cua-driver (macOS, Windows, Linux).
pub struct ComputerUseHandler {
    driver_cmd: Option<String>,
}

impl ComputerUseHandler {
    pub fn new() -> Self {
        Self {
            driver_cmd: resolve_driver_cmd(),
        }
    }

    /// Override the resolved driver binary (tests / explicit configuration).
    pub fn with_driver(cmd: impl Into<String>) -> Self {
        Self {
            driver_cmd: Some(cmd.into()),
        }
    }

    fn driver(&self) -> Result<&str, ToolError> {
        self.driver_cmd
            .as_deref()
            .ok_or_else(|| ToolError::ExecutionFailed(DRIVER_INSTALL_HINT.into()))
    }

    /// Run `cua-driver call <tool> <json-args>` and parse stdout as JSON.
    async fn call_driver(&self, tool: &str, args: &Value) -> Result<Value, ToolError> {
        let driver = self.driver()?;
        let args_str = serde_json::to_string(args).map_err(|e| {
            ToolError::InvalidParams(format!("failed to encode driver args: {}", e))
        })?;

        let output = tokio::time::timeout(Duration::from_secs(DRIVER_TIMEOUT_SECS), async {
            tokio::process::Command::new(driver)
                .arg("call")
                .arg(tool)
                .arg(&args_str)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
        })
        .await
        .map_err(|_| ToolError::Timeout(format!("cua-driver call {} timed out", tool)))?
        .map_err(|e| ToolError::ExecutionFailed(format!("failed to spawn cua-driver: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr
                .trim()
                .lines()
                .last()
                .unwrap_or("")
                .chars()
                .take(300)
                .collect::<String>();
            return Err(ToolError::ExecutionFailed(format!(
                "cua-driver call {} failed (exit {}): {}",
                tool,
                output.status.code().unwrap_or(-1),
                detail
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        serde_json::from_str::<Value>(stdout.trim()).map_err(|e| {
            ToolError::ExecutionFailed(format!("cua-driver returned non-JSON output: {}", e))
        })
    }

    /// Shape the driver result into the tool response, verdict included.
    fn present(&self, action: &str, result: &Value) -> String {
        let mut out = json!({
            "ok": result.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
            "action": action,
        });
        // Pass through the driver's semantic fields when present.
        for key in [
            "message",
            "verified",
            "effect",
            "escalation",
            "path",
            "degraded",
            "delivery_mode",
            "code",
            "meta",
            "apps",
            "windows",
            "elements",
            "image_base64",
            "image_mime_type",
            "screenshot_path",
        ] {
            if let Some(v) = result.get(key) {
                if !v.is_null() {
                    out[key] = v.clone();
                }
            }
        }
        out["verdict"] = classify_verdict(result);
        out.to_string()
    }
}

impl Default for ComputerUseHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolHandler for ComputerUseHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("Missing 'action' parameter".into()))?;

        // `wait` is local — no driver round-trip.
        if action == "wait" {
            let seconds = params
                .get("seconds")
                .and_then(|v| v.as_f64())
                .unwrap_or(1.0)
                .clamp(0.0, 30.0);
            tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
            let result = json!({"ok": true, "action": "wait", "effect": "confirmed"});
            return Ok(self.present("wait", &result));
        }

        let (tool, args) = build_driver_call(action, &params)?;
        let result = self.call_driver(&tool, &args).await?;
        Ok(self.present(action, &result))
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "action".into(),
            json!({
                "type": "string",
                "enum": ["capture", "click", "double_click", "right_click", "middle_click",
                         "drag", "scroll", "type", "key", "set_value", "wait",
                         "list_apps", "list_windows", "focus_app"],
                "description": "Which action to perform. `capture` is free (no side effects). All other actions require approval unless auto-approved. Use `set_value` for select/popup elements and sliders — it selects the matching option directly without opening the native menu (no focus steal)."
            }),
        );
        props.insert(
            "mode".into(),
            json!({
                "type": "string",
                "enum": ["som", "vision", "ax"],
                "description": "Capture mode. `som` (default) is a screenshot with numbered overlays on every interactable element plus the AX tree — best for vision models, lets you click by element index. `vision` is a plain screenshot. `ax` is the accessibility tree only (no image; useful for text-only models)."
            }),
        );
        props.insert(
            "app".into(),
            json!({
                "type": "string",
                "description": "Optional. Limit capture/action to one app (name e.g. 'Safari', or bundle ID). Omitted = frontmost window. app='screen' = composited full-screen grab (image only, no clickable elements); app='desktop' = the OS desktop/shell surface (wallpaper, icons, taskbar) with its elements."
            }),
        );
        props.insert(
            "pid".into(),
            json!({
                "type": "integer",
                "description": "Optional exact process target for action='capture'. Pair with window_id when discovery cannot resolve an X11 app."
            }),
        );
        props.insert(
            "window_id".into(),
            json!({
                "type": "integer",
                "description": "Optional exact native window target for action='capture'. Pair with pid when an external cua-driver list_windows lookup has already identified the window."
            }),
        );
        props.insert(
            "element".into(),
            json!({
                "type": "integer",
                "description": "The 1-based SOM index returned by the last `capture(mode='som')` call. Strongly preferred over raw coordinates."
            }),
        );
        props.insert(
            "coordinate".into(),
            json!({
                "type": "array",
                "items": {"type": "integer"},
                "minItems": 2,
                "maxItems": 2,
                "description": "Pixel coordinates [x, y] relative to the captured window screenshot (top-left origin). Only use this if no element index is available."
            }),
        );
        props.insert(
            "button".into(),
            json!({
                "type": "string",
                "enum": ["left", "right", "middle"],
                "description": "Mouse button. Defaults to left."
            }),
        );
        props.insert(
            "modifiers".into(),
            json!({
                "type": "array",
                "items": {"type": "string", "enum": ["cmd", "shift", "option", "alt", "ctrl", "fn", "win", "windows", "super", "meta"]},
                "description": "Modifier keys held during the action."
            }),
        );
        props.insert(
            "from_element".into(),
            json!({"type": "integer", "description": "Source element index (drag)."}),
        );
        props.insert(
            "to_element".into(),
            json!({"type": "integer", "description": "Target element index (drag)."}),
        );
        props.insert(
            "from_coordinate".into(),
            json!({
                "type": "array",
                "items": {"type": "integer"},
                "minItems": 2,
                "maxItems": 2,
                "description": "Source [x,y] (drag; use when no element available)."
            }),
        );
        props.insert(
            "to_coordinate".into(),
            json!({
                "type": "array",
                "items": {"type": "integer"},
                "minItems": 2,
                "maxItems": 2,
                "description": "Target [x,y] (drag; use when no element available)."
            }),
        );
        props.insert(
            "direction".into(),
            json!({
                "type": "string",
                "enum": ["up", "down", "left", "right"],
                "description": "Scroll direction."
            }),
        );
        props.insert(
            "amount".into(),
            json!({"type": "integer", "description": "Scroll wheel ticks. Default 3."}),
        );
        props.insert(
            "value".into(),
            json!({
                "type": "string",
                "description": "For action='set_value': the value to set on the element. For AXPopUpButton / select dropdowns, pass the option's display label (e.g. 'Blue'). For sliders and other AXValue-settable elements, pass the numeric or string value."
            }),
        );
        props.insert(
            "text".into(),
            json!({"type": "string", "description": "Text to type (respects the current layout)."}),
        );
        props.insert(
            "keys".into(),
            json!({
                "type": "string",
                "description": "Key combo, e.g. 'cmd+s', 'ctrl+alt+t', 'return', 'escape', 'tab'. Use '+' to combine."
            }),
        );
        props.insert(
            "seconds".into(),
            json!({"type": "number", "description": "wait: seconds to pause (max 30)."}),
        );
        props.insert(
            "raise_window".into(),
            json!({
                "type": "boolean",
                "description": "Only for action='focus_app'. If true, brings the window to front (DISRUPTS the user). Default false — input is routed to the app without raising, matching the background co-work model."
            }),
        );
        props.insert(
            "delivery_mode".into(),
            json!({
                "type": "string",
                "enum": ["background", "foreground"],
                "description": "For input actions (click, type, key, drag, scroll). `background` (DEFAULT) delivers without raising the window or stealing focus. `foreground` briefly fronts the window then restores focus — a visible change needing its own approval; use it only when a result's verdict tells you to escalate there. Each result's `verdict` carries the next step; follow it rather than guessing."
            }),
        );
        props.insert(
            "bring_to_front".into(),
            json!({
                "type": "boolean",
                "description": "Optional and only valid with delivery_mode='foreground'. Explicitly invokes cua-driver's standalone bring_to_front tool before the input; it is never passed as an input property. This persistent focus change has a separate approval scope. Default false."
            }),
        );
        props.insert(
            "capture_after".into(),
            json!({
                "type": "boolean",
                "description": "If true, take a follow-up capture after the action and include it in the response. Saves a round-trip when you need to verify an action's effect."
            }),
        );

        tool_schema(
            "computer_use",
            "Drive the desktop via cua-driver — screenshots, mouse, keyboard, scroll, drag — on macOS, Windows, and Linux. Input is background-FIRST, not background-only: the default delivery routes to the target window without stealing the user's cursor or focus (works even on hidden/minimized windows), and when a result's `verdict` says to escalate you climb — pixel coordinates, or delivery_mode='foreground' (briefly fronts the window; separate approval). Each result carries a `verdict` with the next step; follow it — never repeat confirmed input, and re-capture to verify an unverifiable one before retrying. Workflow: action='capture' (mode='som' gives numbered element overlays), then click by `element` index; re-capture after state-changing actions (or pass capture_after=true).",
            JsonSchema::object(props, vec!["action".into()]),
        )
    }
}

// ---------------------------------------------------------------------------
// Tests (pure logic only — no driver binary required)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_name_and_required() {
        let handler = ComputerUseHandler::with_driver("/nonexistent/cua-driver");
        let schema = handler.schema();
        assert_eq!(schema.name, "computer_use");
    }

    #[test]
    fn test_all_fourteen_actions_supported() {
        for action in ACTIONS {
            let params = match *action {
                "wait" => json!({"action": "wait", "seconds": 0}),
                "scroll" => json!({"action": "scroll", "direction": "down"}),
                "key" => json!({"action": "key", "keys": "escape"}),
                "set_value" => json!({"action": "set_value", "value": "x", "element": 3}),
                "focus_app" => json!({"action": "focus_app", "app": "Safari"}),
                "click" | "double_click" | "right_click" | "middle_click" => {
                    json!({"action": action, "element": 1})
                }
                "drag" => json!({"action": "drag", "from_element": 1, "to_element": 2}),
                _ => json!({"action": action}),
            };
            // wait is handled locally; every other action must map to a driver tool.
            if *action == "wait" {
                continue;
            }
            let (tool, _) = build_driver_call(action, &params)
                .unwrap_or_else(|e| panic!("action {} should map: {}", action, e));
            assert!(!tool.is_empty(), "action {} mapped to empty tool", action);
        }
        assert_eq!(ACTIONS.len(), 14);
    }

    #[test]
    fn test_click_variants_map_buttons() {
        let (_, args) =
            build_driver_call("click", &json!({"action": "click", "element": 2})).unwrap();
        assert_eq!(args["button"], json!("left"));
        assert_eq!(args["element_index"], json!(2));

        let (tool, args) = build_driver_call(
            "right_click",
            &json!({"action": "right_click", "coordinate": [10, 20]}),
        )
        .unwrap();
        assert_eq!(tool, "click");
        assert_eq!(args["button"], json!("right"));
        assert_eq!(args["x"], json!(10));
        assert_eq!(args["y"], json!(20));

        let (tool, _) = build_driver_call(
            "double_click",
            &json!({"action": "double_click", "element": 1}),
        )
        .unwrap();
        assert_eq!(tool, "double_click");
    }

    #[test]
    fn test_click_requires_target() {
        let err = build_driver_call("click", &json!({"action": "click"})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[test]
    fn test_drag_requires_both_ends() {
        let err =
            build_driver_call("drag", &json!({"action": "drag", "from_element": 1})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));

        let (tool, args) = build_driver_call(
            "drag",
            &json!({"action": "drag", "from_coordinate": [1, 2], "to_coordinate": [3, 4]}),
        )
        .unwrap();
        assert_eq!(tool, "drag");
        assert_eq!(args["from_x"], json!(1));
        assert_eq!(args["to_y"], json!(4));
    }

    #[test]
    fn test_type_and_key_mapping() {
        let (tool, args) =
            build_driver_call("type", &json!({"action": "type", "text": "hello"})).unwrap();
        assert_eq!(tool, "type_text");
        assert_eq!(args["text"], json!("hello"));

        let (tool, args) =
            build_driver_call("key", &json!({"action": "key", "keys": "cmd+s"})).unwrap();
        assert_eq!(tool, "press_key");
        assert_eq!(args["key"], json!("cmd+s"));
    }

    #[test]
    fn test_delivery_mode_threads_through_input_actions() {
        let (_, args) = build_driver_call(
            "click",
            &json!({"action": "click", "element": 1, "delivery_mode": "foreground", "bring_to_front": true}),
        )
        .unwrap();
        assert_eq!(args["delivery_mode"], json!("foreground"));
        assert_eq!(args["bring_to_front"], json!(true));

        // Read actions must NOT leak delivery kwargs.
        let (_, args) = build_driver_call(
            "capture",
            &json!({"action": "capture", "delivery_mode": "foreground"}),
        )
        .unwrap();
        assert!(args.get("delivery_mode").is_none());
    }

    #[test]
    fn test_unknown_action_with_suggestion() {
        let err = build_driver_call("screenshot", &json!({"action": "screenshot"})).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("capture"), "expected suggestion, got: {}", msg);
    }

    #[test]
    fn test_set_value_requires_value() {
        let err = build_driver_call("set_value", &json!({"action": "set_value"})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[test]
    fn test_focus_app_requires_app() {
        let err = build_driver_call("focus_app", &json!({"action": "focus_app"})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[test]
    fn test_verdict_done_on_confirmed() {
        let v = classify_verdict(&json!({"ok": true, "effect": "confirmed"}));
        assert_eq!(v["decision"], json!("done"));
        let v = classify_verdict(&json!({"ok": true, "verified": true}));
        assert_eq!(v["decision"], json!("done"));
    }

    #[test]
    fn test_verdict_escalate_on_failure() {
        let v = classify_verdict(&json!({"ok": false, "action": "click"}));
        assert_eq!(v["decision"], json!("escalate"));
        assert!(v["hint"].as_str().unwrap().contains("did not land"));

        let v = classify_verdict(&json!({
            "ok": false, "action": "click",
            "escalation": {"recommended": "foreground"}
        }));
        assert_eq!(v["recommended"], json!("foreground"));
    }

    #[test]
    fn test_verdict_escalate_to_set_value_on_zero_delivery() {
        let v = classify_verdict(&json!({
            "ok": false, "action": "type", "code": "type_text_incomplete",
            "meta": {"delivered_chars": 0, "requested_chars": 12}
        }));
        assert_eq!(v["decision"], json!("escalate"));
        assert_eq!(v["recommended"], json!("set_value"));
    }

    #[test]
    fn test_verdict_verify_fresh_state() {
        let v = classify_verdict(&json!({"ok": true, "action": "click"}));
        assert_eq!(v["decision"], json!("verify_fresh_state"));

        let v = classify_verdict(&json!({"ok": true, "effect": "unverifiable"}));
        assert_eq!(v["decision"], json!("verify_fresh_state"));
    }

    #[test]
    fn test_driver_missing_error_is_clear() {
        let handler = ComputerUseHandler { driver_cmd: None };
        let err = handler.driver().unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("cua-driver"), "got: {}", msg);
        assert!(msg.contains("HERMES_CUA_DRIVER_CMD"), "got: {}", msg);
    }

    #[tokio::test]
    async fn test_wait_is_local_and_fast() {
        let handler = ComputerUseHandler::with_driver("/nonexistent/cua-driver");
        let out = handler
            .execute(json!({"action": "wait", "seconds": 0}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["verdict"]["decision"], json!("done"));
    }

    #[tokio::test]
    async fn test_execute_missing_action_param() {
        let handler = ComputerUseHandler::with_driver("/nonexistent/cua-driver");
        let err = handler.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }
}
