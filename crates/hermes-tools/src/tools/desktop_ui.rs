//! Desktop-UI bridge tools: `show_tip`, `annotate_preview`, `focus_pane`,
//! `gui_tour`, `set_theme`, `desktop_preview`, `read_window_below`.
//!
//! All seven live in the `desktop_ui` toolset of the reference implementation:
//! they round-trip to the Electron desktop renderer through a bridge that the
//! desktop gateway injects. Without a wired bridge every call fails with a
//! clear "desktop app only" error instead of silently doing nothing.
//!
//! LIMITATIONS vs the Python original:
//! - Python splits the transport in two: `desktop_ui.emit` (fire-and-forget
//!   events like `tip.show` / `pane.reveal`) and per-tool callbacks injected
//!   as `kw["callback"]` (blocking round-trips). Here both collapse into one
//!   injected trait, [`DesktopUiBridge::call`], whose `method` names mirror
//!   the Python wire verbs (`tip.show`, `pane.reveal`, `tour.request`, …).
//! - `show_tip`'s `check_tips_enabled` / `gui_tour`'s `check_tours_enabled`
//!   (the Settings → Appearance switches that hide the tools from the model)
//!   are registry-layer concerns, not ported here.
//! - `desktop_preview` action `read` is dispatched at the agent level in
//!   Python (the handler itself errors out); this port keeps that behavior.
//! - `set_theme` re-validates swatch/layout ids in the renderer; only the
//!   client-side checks (accent hex, mode enum, picker exclusivity) are
//!   ported — the renderer remains the authority on saved layouts.

use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{json, Value};

use hermes_core::{tool_schema, JsonSchema, ToolError, ToolHandler, ToolSchema};

// ---------------------------------------------------------------------------
// Bridge (injected by the desktop gateway)
// ---------------------------------------------------------------------------

/// Bridge to the Hermes desktop renderer.
///
/// The gateway implements this against its event / blocking-prompt bridge.
/// Method names mirror the Python wire verbs:
/// `tip.show`, `preview.annotate`, `pane.reveal`, `tour.request`,
/// `theme.set`, `preview.open`, `preview.close`, `window.read`.
#[async_trait]
pub trait DesktopUiBridge: Send + Sync {
    /// Dispatch one desktop-UI request; return the renderer's answer as JSON.
    async fn call(&self, method: &str, params: Value) -> Result<Value, ToolError>;
}

fn no_bridge(tool: &str) -> ToolError {
    ToolError::ExecutionFailed(format!(
        "{tool} is only available in the Hermes desktop app: no desktop bridge is wired."
    ))
}

/// Pass a renderer JSON answer through; wrap non-objects like Python does.
fn passthrough(raw: Value) -> String {
    if raw.is_object() {
        raw.to_string()
    } else {
        json!({ "text": raw.to_string() }).to_string()
    }
}

fn get_str(params: &Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// show_tip
// ---------------------------------------------------------------------------

const TIP_SIDES: &[&str] = &["top", "right", "bottom", "left"];

/// Pure validation for `show_tip` (mirrors `tip_tool`).
fn validate_tip(text: &str, selector: &str, side: &str) -> Result<(String, String, Option<String>), String> {
    let text = text.trim().to_string();
    let selector = selector.trim().to_string();
    if text.is_empty() {
        return Err("tip needs text — the one line the bubble says.".into());
    }
    if selector.is_empty() {
        return Err("tip needs a selector to point at. Call tour(action='targets') to see \
            what's on screen and prefer a target reporting stable: true."
            .into());
    }
    let side = side.trim();
    if !side.is_empty() && !TIP_SIDES.contains(&side) {
        return Err(format!("side must be one of: {}.", TIP_SIDES.join(", ")));
    }
    Ok((text, selector, (!side.is_empty()).then(|| side.to_string())))
}

/// Point at one thing in the desktop UI with a small arrow bubble.
pub struct ShowTipHandler {
    bridge: Option<Arc<dyn DesktopUiBridge>>,
}

impl ShowTipHandler {
    pub fn new(bridge: Option<Arc<dyn DesktopUiBridge>>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl ToolHandler for ShowTipHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let selector = params.get("selector").and_then(|v| v.as_str()).unwrap_or("");
        let side = params.get("side").and_then(|v| v.as_str()).unwrap_or("");
        let title = get_str(&params, "title");
        let (text, selector, side) =
            validate_tip(text, selector, side).map_err(ToolError::InvalidParams)?;
        let bridge = self.bridge.as_ref().ok_or_else(|| no_bridge("show_tip"))?;
        let mut payload = json!({ "selector": selector, "text": text });
        if let Some(t) = title {
            payload["title"] = json!(t);
        }
        if let Some(s) = side {
            payload["side"] = json!(s);
        }
        let answer = bridge.call("tip.show", payload).await?;
        let ok = answer.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
        if !ok {
            return Err(no_bridge("show_tip"));
        }
        Ok(json!({ "success": true, "selector": selector }).to_string())
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "text".into(),
            json!({"type": "string", "description": "The one-sentence bubble text."}),
        );
        props.insert(
            "selector".into(),
            json!({"type": "string", "description": "Selector from tour targets."}),
        );
        props.insert(
            "title".into(),
            json!({"type": "string", "description": "Optional heading."}),
        );
        props.insert(
            "side".into(),
            json!({"type": "string", "enum": TIP_SIDES, "description": "Omit for 'top'; flips at screen edges."}),
        );
        tool_schema(
            "show_tip",
            "Point at one thing in the desktop UI with a small arrow bubble (no dimming, no tour chrome) — for when a sentence is clearer with a finger on its subject. Get selectors from tour(action='targets'), prefer stable:true, never guess. One tip at a time (new replaces last); say the same thing in chat too — the bubble is a pointer, not the message. Sparingly: a bubble every turn stops being read.",
            JsonSchema::object(props, vec!["text".into(), "selector".into()]),
        )
    }
}

// ---------------------------------------------------------------------------
// annotate_preview
// ---------------------------------------------------------------------------

const ANNOTATE_ACTIONS: &[&str] = &["add", "hold", "remove", "clear"];

/// Map our action verbs to the renderer's wire verbs (`clear` is `unpin`
/// with no target = "all of them").
fn annotate_wire_verb(action: &str) -> &'static str {
    match action {
        "add" => "pin",
        "hold" => "hold",
        _ => "unpin",
    }
}

/// Pure validation for `annotate_preview` (mirrors `annotate_preview_tool`).
fn validate_annotate(
    action: &str,
    reference: Option<&str>,
    selector: Option<&str>,
) -> Result<(String, Option<String>, Option<String>), String> {
    let verb = action.trim().to_lowercase();
    if verb.is_empty() || !ANNOTATE_ACTIONS.contains(&verb.as_str()) {
        return Err(format!(
            "action must be one of: {}.",
            ANNOTATE_ACTIONS.join(", ")
        ));
    }
    let reference = reference.map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let selector = selector.map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    if (verb == "add" || verb == "remove") && reference.is_none() && selector.is_none() {
        return Err(format!(
            "{verb} needs a ref from drive_preview action='elements' (e.g. 'btn-sign-in') or a CSS selector."
        ));
    }
    Ok((verb, reference, selector))
}

/// Highlight elements on the preview-pane page, lastingly.
pub struct AnnotatePreviewHandler {
    bridge: Option<Arc<dyn DesktopUiBridge>>,
}

impl AnnotatePreviewHandler {
    pub fn new(bridge: Option<Arc<dyn DesktopUiBridge>>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl ToolHandler for AnnotatePreviewHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let action = params.get("action").and_then(|v| v.as_str()).unwrap_or("add");
        let reference = params.get("ref").and_then(|v| v.as_str());
        let selector = params.get("selector").and_then(|v| v.as_str());
        let label = get_str(&params, "label");
        let (verb, reference, selector) =
            validate_annotate(action, reference, selector).map_err(ToolError::InvalidParams)?;
        let bridge = self
            .bridge
            .as_ref()
            .ok_or_else(|| no_bridge("annotate_preview"))?;
        // `hold`/`clear` address the whole field; add/remove target one element.
        let targeted = verb != "clear" && verb != "hold";
        let mut payload = json!({ "action": annotate_wire_verb(&verb) });
        if targeted {
            if let Some(r) = reference {
                payload["ref"] = json!(r);
            }
            if let Some(s) = selector {
                payload["selector"] = json!(s);
            }
        }
        if let Some(l) = label {
            payload["text"] = json!(l);
        }
        let answer = bridge.call("preview.annotate", payload).await.map_err(|e| {
            ToolError::ExecutionFailed(format!("Failed to annotate the in-app browser: {e}"))
        })?;
        if answer.is_null() {
            return Err(ToolError::ExecutionFailed(
                "No GUI window answered with a page: no preview tab is open. \
                 Open a page with desktop_preview first."
                    .into(),
            ));
        }
        Ok(passthrough(answer))
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "action".into(),
            json!({"type": "string", "enum": ANNOTATE_ACTIONS, "description": "Defaults to 'add'."}),
        );
        props.insert(
            "ref".into(),
            json!({"type": "string", "description": "Ref from drive_preview elements."}),
        );
        props.insert(
            "selector".into(),
            json!({"type": "string", "description": "CSS selector fallback. Prefer ref."}),
        );
        props.insert(
            "label".into(),
            json!({"type": "string", "description": "Optional caption, e.g. 'cheapest'."}),
        );
        tool_schema(
            "annotate_preview",
            "Highlight elements on the preview-pane page, lastingly (drive_preview's own marks fade; annotations stay until removed) — point at findings, flag what you're about to change, keep your place. Use the refs from drive_preview action='elements'. add: outline one element (optional short label — a word or two, drawn on the page). hold: freeze the whole visible field, every element outlined and named. remove/clear: take one/all down. Marks follow their element on scroll; navigation clears them.",
            JsonSchema::object(props, vec![]),
        )
    }
}

// ---------------------------------------------------------------------------
// focus_pane
// ---------------------------------------------------------------------------

const PANES: &[&str] = &["chat", "files", "terminal", "review", "sessions"];

/// Pure validation for `focus_pane`.
fn validate_focus_pane(pane: &str) -> Result<String, String> {
    let name = pane.trim().to_lowercase();
    if !PANES.contains(&name.as_str()) {
        return Err(format!("pane must be one of: {}.", PANES.join(", ")));
    }
    Ok(name)
}

/// Reveal and focus a Hermes desktop pane.
pub struct FocusPaneHandler {
    bridge: Option<Arc<dyn DesktopUiBridge>>,
}

impl FocusPaneHandler {
    pub fn new(bridge: Option<Arc<dyn DesktopUiBridge>>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl ToolHandler for FocusPaneHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let pane = params.get("pane").and_then(|v| v.as_str()).unwrap_or("");
        let name = validate_focus_pane(pane).map_err(ToolError::InvalidParams)?;
        let bridge = self.bridge.as_ref().ok_or_else(|| no_bridge("focus_pane"))?;
        bridge
            .call("pane.reveal", json!({ "pane": name }))
            .await
            .map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to focus the {name} pane: {e}"))
            })?;
        Ok(json!({ "success": true, "pane": name }).to_string())
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "pane".into(),
            json!({"type": "string", "enum": PANES, "description": "Which pane to reveal."}),
        );
        tool_schema(
            "focus_pane",
            "Reveal and focus a Hermes desktop pane when the user asks to see it: chat, files, terminal, review (git diff), or sessions. For URLs/files use the desktop_preview tool instead.",
            JsonSchema::object(props, vec!["pane".into()]),
        )
    }
}

// ---------------------------------------------------------------------------
// gui_tour
// ---------------------------------------------------------------------------

const TOUR_ACTIONS: &[&str] = &["targets", "show", "start", "next", "prev", "stop"];
const TOUR_SURFACES: &[&str] = &["app", "preview"];
const TOUR_PRESETS: &[&str] = &["quick", "full"];

/// One step of a custom tour (mirrors `_STEP_SCHEMA`).
fn validate_tour_step(step: &Value, index: usize) -> Result<(), String> {
    let obj = step.as_object().ok_or_else(|| format!("steps[{index}] must be an object."))?;
    let has_target = ["selector", "title", "text"]
        .iter()
        .any(|k| obj.get(*k).and_then(|v| v.as_str()).map(|s| !s.trim().is_empty()).unwrap_or(false));
    if !has_target {
        return Err(format!("steps[{index}] needs a selector and/or title/text."));
    }
    Ok(())
}

/// Pure validation for `gui_tour` (mirrors `tour_tool`).
#[allow(clippy::too_many_arguments)]
fn validate_tour(
    action: &str,
    surface: Option<&str>,
    selector: Option<&str>,
    title: Option<&str>,
    text: Option<&str>,
    side: Option<&str>,
    steps: Option<&Vec<Value>>,
    preset: Option<&str>,
) -> Result<Value, String> {
    let verb = action.trim().to_lowercase();
    if !TOUR_ACTIONS.contains(&verb.as_str()) {
        return Err(format!("action must be one of: {}.", TOUR_ACTIONS.join(", ")));
    }
    let surface = surface.unwrap_or("app").trim().to_lowercase();
    if !TOUR_SURFACES.contains(&surface.as_str()) {
        return Err(format!("surface must be one of: {}.", TOUR_SURFACES.join(", ")));
    }
    if let Some(s) = side {
        if !TIP_SIDES.contains(&s) {
            return Err(format!("side must be one of: {}.", TIP_SIDES.join(", ")));
        }
    }
    if let Some(p) = preset {
        if !TOUR_PRESETS.contains(&p) {
            return Err(format!("preset must be one of: {}.", TOUR_PRESETS.join(", ")));
        }
        if steps.is_some() {
            return Err("preset picks a built-in tour; pass steps or preset, not both.".into());
        }
    }
    let has_point = [selector, title, text]
        .iter()
        .any(|v| v.map(|s| !s.trim().is_empty()).unwrap_or(false));
    if verb == "show" && !has_point {
        return Err("show needs a selector (and/or title/text for the popover).".into());
    }
    if verb == "start" && steps.is_none() && surface != "app" {
        return Err("The built-in tour runs on surface='app'; pass steps for a preview tour.".into());
    }
    if let Some(steps) = steps {
        if verb == "start" {
            if steps.is_empty() {
                return Err("start needs a non-empty steps array, or no steps for the built-in tour.".into());
            }
            for (i, step) in steps.iter().enumerate() {
                validate_tour_step(step, i)?;
            }
        }
    }
    let mut payload = json!({ "action": verb, "surface": surface });
    for (key, val) in [
        ("selector", selector),
        ("title", title),
        ("text", text),
        ("side", side),
        ("preset", preset),
    ] {
        if let Some(v) = val {
            let v = v.trim();
            if !v.is_empty() {
                payload[key] = json!(v);
            }
        }
    }
    if let Some(steps) = steps {
        payload["steps"] = json!(steps);
    }
    Ok(payload)
}

/// Guided tour (highlight + narrate UI elements) in the desktop GUI.
pub struct GuiTourHandler {
    bridge: Option<Arc<dyn DesktopUiBridge>>,
}

impl GuiTourHandler {
    pub fn new(bridge: Option<Arc<dyn DesktopUiBridge>>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl ToolHandler for GuiTourHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let action = params.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let payload = validate_tour(
            action,
            params.get("surface").and_then(|v| v.as_str()),
            params.get("selector").and_then(|v| v.as_str()),
            params.get("title").and_then(|v| v.as_str()),
            params.get("text").and_then(|v| v.as_str()),
            params.get("side").and_then(|v| v.as_str()),
            params.get("steps").and_then(|v| v.as_array()),
            params.get("preset").and_then(|v| v.as_str()),
        )
        .map_err(ToolError::InvalidParams)?;
        let mut payload = payload;
        if let Some(idx) = params.get("step_index").and_then(|v| v.as_i64()) {
            payload["step_index"] = json!(idx);
        }
        let bridge = self.bridge.as_ref().ok_or_else(|| no_bridge("gui_tour"))?;
        let answer = bridge.call("tour.request", payload).await.map_err(|e| {
            ToolError::ExecutionFailed(format!("Tour action failed: {e}"))
        })?;
        if answer.is_null() {
            return Err(ToolError::ExecutionFailed(
                "The tour request timed out, or no GUI window answered. \
                 For surface='preview' open a page in the preview pane first."
                    .into(),
            ));
        }
        Ok(passthrough(answer))
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "action".into(),
            json!({"type": "string", "enum": TOUR_ACTIONS, "description": "start alone runs the built-in tour; targets first for custom steps."}),
        );
        props.insert(
            "surface".into(),
            json!({"type": "string", "enum": TOUR_SURFACES, "description": "'app' (default) or 'preview'."}),
        );
        props.insert(
            "selector".into(),
            json!({"type": "string", "description": "show: selector from targets (prefer stable). Omit = centered narration."}),
        );
        props.insert(
            "title".into(),
            json!({"type": "string", "description": "show: popover title."}),
        );
        props.insert(
            "text".into(),
            json!({"type": "string", "description": "show: popover body."}),
        );
        props.insert(
            "side".into(),
            json!({"type": "string", "enum": TIP_SIDES, "description": "show: popover side; omit to auto-place."}),
        );
        let mut step_props: IndexMap<String, Value> = IndexMap::new();
        step_props.insert(
            "selector".into(),
            json!({"type": "string", "description": "Element to highlight; omit = centered narration."}),
        );
        step_props.insert("title".into(), json!({"type": "string", "description": "Popover title."}));
        step_props.insert("text".into(), json!({"type": "string", "description": "Popover body."}));
        step_props.insert(
            "side".into(),
            json!({"type": "string", "enum": TIP_SIDES, "description": "Popover side; omit to auto-place."}),
        );
        props.insert(
            "steps".into(),
            json!({"type": "array", "items": {"type": "object", "properties": step_props}, "description": "start: ordered steps; omit for the built-in tour."}),
        );
        props.insert(
            "preset".into(),
            json!({"type": "string", "enum": TOUR_PRESETS, "description": "start without steps: 'quick' (four essentials) or 'full'."}),
        );
        props.insert(
            "step_index".into(),
            json!({"type": "integer", "description": "start: 0-indexed first step."}),
        );
        tool_schema(
            "gui_tour",
            "Guided tour in the desktop GUI: dim the screen, highlight an element, attach a titled popover. Surfaces: 'app' (Hermes itself) or 'preview' (the page in the preview pane). A general look around the app is ONE call: action='start' with no steps runs the app's built-in tour (preset 'quick' or 'full', default full). For a custom tour, call action='targets' first — prefer targets marked stable:true (their selectors survive re-renders); re-scan if one stops matching. Then narrate with action='show' (one highlight per call, replaces the last — pair each with a chat message) or hand over with action='start' + steps (user gets Next/Prev; 'next'/'prev' also page it). 'stop' clears. Use for how-does-X-work / where-is-Y walkthroughs.",
            JsonSchema::object(props, vec!["action".into()]),
        )
    }
}

// ---------------------------------------------------------------------------
// set_theme
// ---------------------------------------------------------------------------

const ACCENT_SWATCHES: &[&str] = &["mono", "green", "cyan", "nous", "violet", "pink", "red", "orange"];
const THEME_ACCENT: &str = "theme";
const THEME_MODES: &[&str] = &["light", "dark", "system"];
const THEME_PICKERS: &[&str] = &["accent", "layout"];
const SIMPLE_LAYOUTS: &[&str] = &["sidebar-left", "sidebar-right"];
const ADVANCED_LAYOUTS: &[&str] = &["default", "basic", "focus", "terminal-deck", "quad"];

/// Pure validation for `set_theme` (mirrors `set_theme_tool`).
/// Returns the payload to send to the renderer.
fn validate_set_theme(
    accent: Option<&str>,
    mode: Option<&str>,
    layout: Option<&str>,
    picker: Option<&str>,
) -> Result<Value, String> {
    let clean = |v: Option<&str>| v.map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let accent = clean(accent);
    let mode = clean(mode);
    let layout = clean(layout);
    let picker = clean(picker);

    if let Some(p) = &picker {
        if !THEME_PICKERS.contains(&p.as_str()) {
            return Err(format!("picker must be one of: {}.", THEME_PICKERS.join(", ")));
        }
        if accent.is_some() || mode.is_some() || layout.is_some() {
            return Err(
                "picker shows a card for the user to choose; pass picker alone, without values.".into(),
            );
        }
        return Ok(json!({ "picker": p }));
    }
    if accent.is_none() && mode.is_none() && layout.is_none() {
        return Err(
            "Pass at least one of accent, mode or layout, or picker to let the user choose.".into(),
        );
    }
    if let Some(m) = &mode {
        if !THEME_MODES.contains(&m.as_str()) {
            return Err(format!(
                "Unknown mode {m:?}. Available: {}.",
                THEME_MODES.join(", ")
            ));
        }
    }
    if let Some(a) = &accent {
        let is_hex = Regex::new(r"^#[0-9a-fA-F]{6}$").map(|re| re.is_match(a)).unwrap_or(false);
        if !ACCENT_SWATCHES.contains(&a.as_str()) && a != THEME_ACCENT && !is_hex {
            let mut available: Vec<&str> = ACCENT_SWATCHES.to_vec();
            available.push(THEME_ACCENT);
            return Err(format!(
                "Unknown accent {a:?}: use a swatch id, 'theme' or #rrggbb. Available: {}.",
                available.join(", ")
            ));
        }
    }
    let mut payload = json!({});
    if let Some(a) = accent {
        payload["accent"] = json!(a);
    }
    if let Some(m) = mode {
        payload["mode"] = json!(m);
    }
    if let Some(l) = layout {
        payload["layout"] = json!(l);
    }
    Ok(payload)
}

/// Change how the Hermes desktop app looks.
pub struct SetThemeHandler {
    bridge: Option<Arc<dyn DesktopUiBridge>>,
}

impl SetThemeHandler {
    pub fn new(bridge: Option<Arc<dyn DesktopUiBridge>>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl ToolHandler for SetThemeHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let payload = validate_set_theme(
            params.get("accent").and_then(|v| v.as_str()),
            params.get("mode").and_then(|v| v.as_str()),
            params.get("layout").and_then(|v| v.as_str()),
            params.get("picker").and_then(|v| v.as_str()),
        )
        .map_err(ToolError::InvalidParams)?;
        let bridge = self.bridge.as_ref().ok_or_else(|| no_bridge("set_theme"))?;
        let answer = bridge.call("theme.set", payload).await?;
        Ok(passthrough(answer))
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "accent".into(),
            json!({"type": "string", "description": format!("A swatch id ({}), 'theme' for the theme's own accent, or a hex colour '#rrggbb'.", ACCENT_SWATCHES.join(", "))}),
        );
        props.insert(
            "mode".into(),
            json!({"type": "string", "enum": THEME_MODES}),
        );
        props.insert(
            "layout".into(),
            json!({"type": "string", "description": format!("Layout preset id. Simple mode: {}. Advanced mode: {}. The user's saved layouts by id.", SIMPLE_LAYOUTS.join(", "), ADVANCED_LAYOUTS.join(", "))}),
        );
        props.insert(
            "picker".into(),
            json!({"type": "string", "enum": THEME_PICKERS, "description": "Show that picker card and wait for the user (up to 5 minutes). Pass it alone."}),
        );
        tool_schema(
            "set_theme",
            "Change how the Hermes desktop app looks: accent colour, light/dark mode, layout. Pass the values to apply them now; the result says what was applied. Pass picker instead to show the user an accent or layout picker card in the chat and wait for their pick. To reveal ONE pane, use focus_pane instead.",
            JsonSchema::object(props, vec![]),
        )
    }
}

// ---------------------------------------------------------------------------
// desktop_preview
// ---------------------------------------------------------------------------

const PREVIEW_ACTIONS: &[&str] = &["open", "close", "read"];

/// Coax a bare host/domain into a fetchable URL; leave paths + schemes alone.
/// Port of `open_preview_tool._normalize_target`.
fn normalize_preview_target(raw: &str) -> String {
    let v = raw.trim().trim_matches('`').trim().to_string();
    if v.is_empty() || v.contains("://") || v.starts_with('/') || v.starts_with("./")
        || v.starts_with("../") || v.starts_with('~') || v.starts_with("file:")
    {
        return v;
    }
    let localhost = Regex::new(r"(?i)^(localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1\])(:\d+)?(/|$)").unwrap();
    if localhost.is_match(&v) {
        return format!("http://{v}");
    }
    let domain = Regex::new(r"(?i)^[\w.-]+\.[a-z]{2,}(:\d+)?(/.*)?$").unwrap();
    if domain.is_match(&v) {
        return format!("https://{v}");
    }
    v
}

/// The preview pane beside the chat, as ONE tool.
pub struct DesktopPreviewHandler {
    bridge: Option<Arc<dyn DesktopUiBridge>>,
}

impl DesktopPreviewHandler {
    pub fn new(bridge: Option<Arc<dyn DesktopUiBridge>>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl ToolHandler for DesktopPreviewHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_lowercase();
        if !PREVIEW_ACTIONS.contains(&action.as_str()) {
            return Err(ToolError::InvalidParams(
                "action must be one of: open, close, read.".into(),
            ));
        }
        if action == "read" {
            // Dispatched at the agent level in the reference implementation.
            return Err(ToolError::ExecutionFailed(
                "preview read must run inside a desktop session (no GUI callback here).".into(),
            ));
        }
        let bridge = self
            .bridge
            .as_ref()
            .ok_or_else(|| no_bridge("desktop_preview"))?;
        let url = params.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let target = normalize_preview_target(url);
        if action == "open" {
            if target.is_empty() {
                return Err(ToolError::InvalidParams(
                    "url is required — a web URL (https://…), a localhost dev server, or a \
                     file path to show in the preview pane."
                        .into(),
                ));
            }
            let mut payload = json!({ "url": target });
            if let Some(label) = get_str(&params, "label") {
                payload["label"] = json!(label);
            }
            let answer = bridge.call("preview.open", payload).await?;
            Ok(passthrough(answer))
        } else {
            let answer = bridge.call("preview.close", json!({ "url": target })).await.map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to close the preview: {e}"))
            })?;
            let _ = answer;
            let closed = if target.is_empty() { "all".to_string() } else { target };
            Ok(json!({ "success": true, "closed": closed }).to_string())
        }
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "action".into(),
            json!({"type": "string", "enum": PREVIEW_ACTIONS}),
        );
        props.insert(
            "url".into(),
            json!({"type": "string", "description": "open: the target. close: one tab (omit for the whole pane)."}),
        );
        props.insert(
            "label".into(),
            json!({"type": "string", "description": "open: optional tab label."}),
        );
        props.insert(
            "start".into(),
            json!({"type": "integer", "description": "read: 0-indexed char offset."}),
        );
        props.insert(
            "count".into(),
            json!({"type": "integer", "description": "read: chars to return (capped per read)."}),
        );
        tool_schema(
            "desktop_preview",
            "Open, close, or read the preview pane beside the chat. open: show a web URL (bare domains fine), a localhost dev server, or a file path (HTML renders live) — opens for the current window only. close: dismiss the whole pane, or one tab via url. read: what the pane currently shows — returns {kind, url, title, text, start, end, total_chars}; a Browser tab's text is the rendered page's visible text, paged with start/count (char offsets); a file tab answers identity only (read the file with read_file).",
            JsonSchema::object(props, vec!["action".into()]),
        )
    }
}

// ---------------------------------------------------------------------------
// read_window_below
// ---------------------------------------------------------------------------

/// Identify the app window directly behind the Hermes desktop window.
pub struct ReadWindowBelowHandler {
    bridge: Option<Arc<dyn DesktopUiBridge>>,
}

impl ReadWindowBelowHandler {
    pub fn new(bridge: Option<Arc<dyn DesktopUiBridge>>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl ToolHandler for ReadWindowBelowHandler {
    async fn execute(&self, _params: Value) -> Result<String, ToolError> {
        let bridge = self
            .bridge
            .as_ref()
            .ok_or_else(|| no_bridge("read_window_below"))?;
        let answer = bridge.call("window.read", json!({})).await.map_err(|e| {
            ToolError::ExecutionFailed(format!("Failed to read the window below: {e}"))
        })?;
        if answer.is_null() {
            return Err(ToolError::ExecutionFailed(
                "Could not determine the window underneath (the desktop app did not answer, \
                 or window enumeration is unavailable on this system)."
                    .into(),
            ));
        }
        Ok(passthrough(answer))
    }

    fn schema(&self) -> ToolSchema {
        tool_schema(
            "read_window_below",
            "Identify the app window directly behind the Hermes desktop window (what the user is working in). JSON: {window: {app, title, bounds, id}, frontmost, platform}. title may be empty when the OS withholds it (noted in `note`); where windows cannot be enumerated at all, {error, platform} says what would fix it — relay that instead of retrying. Metadata only; never captures pixels.",
            JsonSchema::object(IndexMap::new(), vec![]),
        )
    }
}

// ---------------------------------------------------------------------------
// Tests (pure logic only — no bridge, no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- validate_tip --

    #[test]
    fn tip_rejects_empty_text() {
        let err = validate_tip("", "[data-tour=x]", "").unwrap_err();
        assert!(err.contains("tip needs text"));
    }

    #[test]
    fn tip_rejects_empty_selector() {
        let err = validate_tip("hello", "   ", "").unwrap_err();
        assert!(err.contains("selector"));
    }

    #[test]
    fn tip_rejects_bad_side() {
        let err = validate_tip("hello", "[data-tour=x]", "up").unwrap_err();
        assert!(err.contains("side must be one of"));
    }

    #[test]
    fn tip_accepts_all_sides_and_none() {
        for side in ["top", "right", "bottom", "left", ""] {
            let (text, sel, s) = validate_tip(" t ", " [data-tour=x] ", side).unwrap();
            assert_eq!(text, "t");
            assert_eq!(sel, "[data-tour=x]");
            assert_eq!(s, (!side.is_empty()).then(|| side.to_string()));
        }
    }

    // -- validate_annotate --

    #[test]
    fn annotate_rejects_unknown_action() {
        let err = validate_annotate("draw", None, None).unwrap_err();
        assert!(err.contains("action must be one of"));
    }

    #[test]
    fn annotate_add_needs_ref_or_selector() {
        let err = validate_annotate("add", None, None).unwrap_err();
        assert!(err.contains("needs a ref"));
        assert!(validate_annotate("add", Some("btn-1"), None).is_ok());
        assert!(validate_annotate("remove", None, Some(".cls")).is_ok());
    }

    #[test]
    fn annotate_hold_and_clear_need_no_target() {
        assert!(validate_annotate("hold", None, None).is_ok());
        assert!(validate_annotate("clear", None, None).is_ok());
    }

    #[test]
    fn annotate_wire_verbs_match_python() {
        assert_eq!(annotate_wire_verb("add"), "pin");
        assert_eq!(annotate_wire_verb("hold"), "hold");
        assert_eq!(annotate_wire_verb("remove"), "unpin");
        assert_eq!(annotate_wire_verb("clear"), "unpin");
    }

    // -- validate_focus_pane --

    #[test]
    fn focus_pane_accepts_all_panes_case_insensitive() {
        for pane in ["chat", "files", "terminal", "review", "sessions", "CHAT", " Review "] {
            assert!(validate_focus_pane(pane).is_ok(), "{pane}");
        }
    }

    #[test]
    fn focus_pane_rejects_unknown() {
        let err = validate_focus_pane("settings").unwrap_err();
        assert!(err.contains("pane must be one of"));
    }

    // -- validate_tour --

    #[test]
    fn tour_rejects_unknown_action() {
        let err = validate_tour("dance", None, None, None, None, None, None, None).unwrap_err();
        assert!(err.contains("action must be one of"));
    }

    #[test]
    fn tour_show_needs_point_or_text() {
        let err = validate_tour("show", None, None, None, None, None, None, None).unwrap_err();
        assert!(err.contains("show needs a selector"));
        assert!(validate_tour("show", None, Some("[data-tour=x]"), None, None, None, None, None).is_ok());
        assert!(validate_tour("show", None, None, Some("T"), None, None, None, None).is_ok());
    }

    #[test]
    fn tour_start_without_steps_requires_app_surface() {
        let err = validate_tour("start", Some("preview"), None, None, None, None, None, None).unwrap_err();
        assert!(err.contains("surface='app'"));
    }

    #[test]
    fn tour_preset_and_steps_conflict() {
        let err = validate_tour(
            "start", None, None, None, None, None,
            Some(&vec![json!({"selector": "x"})]), Some("quick"),
        )
        .unwrap_err();
        assert!(err.contains("not both"));
    }

    #[test]
    fn tour_steps_must_be_valid_objects() {
        let bad = vec![json!({"title": ""})];
        let err = validate_tour("start", None, None, None, None, None, Some(&bad), None).unwrap_err();
        assert!(err.contains("needs a selector"));
        let not_obj = vec![json!("nope")];
        let err = validate_tour("start", None, None, None, None, None, Some(&not_obj), None).unwrap_err();
        assert!(err.contains("must be an object"));
        let good = vec![json!({"selector": "[data-tour=a]", "title": "A"})];
        assert!(validate_tour("start", None, None, None, None, None, Some(&good), None).is_ok());
    }

    #[test]
    fn tour_targets_and_stop_need_nothing() {
        assert!(validate_tour("targets", None, None, None, None, None, None, None).is_ok());
        assert!(validate_tour("stop", None, None, None, None, None, None, None).is_ok());
    }

    // -- validate_set_theme --

    #[test]
    fn set_theme_needs_something() {
        let err = validate_set_theme(None, None, None, None).unwrap_err();
        assert!(err.contains("at least one"));
    }

    #[test]
    fn set_theme_picker_alone() {
        assert_eq!(validate_set_theme(None, None, None, Some("accent")).unwrap(), json!({"picker": "accent"}));
        let err = validate_set_theme(None, Some("dark"), None, Some("accent")).unwrap_err();
        assert!(err.contains("alone"));
        let err = validate_set_theme(None, None, None, Some("bogus")).unwrap_err();
        assert!(err.contains("picker must be one of"));
    }

    #[test]
    fn set_theme_validates_mode_and_accent() {
        assert!(validate_set_theme(None, Some("dark"), None, None).is_ok());
        let err = validate_set_theme(None, Some("neon"), None, None).unwrap_err();
        assert!(err.contains("Unknown mode"));
        for good in ["mono", "green", "cyan", "nous", "violet", "pink", "red", "orange", "theme", "#a1b2c3", "#A1B2C3"] {
            assert!(validate_set_theme(Some(good), None, None, None).is_ok(), "{good}");
        }
        let err = validate_set_theme(Some("puce"), None, None, None).unwrap_err();
        assert!(err.contains("Unknown accent"));
        let err = validate_set_theme(Some("#abc"), None, None, None).unwrap_err();
        assert!(err.contains("Unknown accent"));
    }

    // -- normalize_preview_target --

    #[test]
    fn preview_target_normalization_matches_python() {
        assert_eq!(normalize_preview_target("www.cnn.com"), "https://www.cnn.com");
        assert_eq!(normalize_preview_target("localhost:3000"), "http://localhost:3000");
        assert_eq!(normalize_preview_target("127.0.0.1:8080/x"), "http://127.0.0.1:8080/x");
        assert_eq!(normalize_preview_target("https://a.com"), "https://a.com");
        assert_eq!(normalize_preview_target("/tmp/x.html"), "/tmp/x.html");
        assert_eq!(normalize_preview_target("~/x.html"), "~/x.html");
        assert_eq!(normalize_preview_target(""), "");
        assert_eq!(normalize_preview_target("`example.com`"), "https://example.com");
    }

    // -- schemas --

    #[test]
    fn schema_names_match_original() {
        assert_eq!(ShowTipHandler::new(None).schema().name, "show_tip");
        assert_eq!(AnnotatePreviewHandler::new(None).schema().name, "annotate_preview");
        assert_eq!(FocusPaneHandler::new(None).schema().name, "focus_pane");
        assert_eq!(GuiTourHandler::new(None).schema().name, "gui_tour");
        assert_eq!(SetThemeHandler::new(None).schema().name, "set_theme");
        assert_eq!(DesktopPreviewHandler::new(None).schema().name, "desktop_preview");
        assert_eq!(ReadWindowBelowHandler::new(None).schema().name, "read_window_below");
    }

    #[test]
    fn schema_required_fields_match_python() {
        let req = |h: &dyn ToolHandler| {
            h.schema().parameters.required.clone().unwrap_or_default()
        };
        assert_eq!(req(&ShowTipHandler::new(None)), vec!["text", "selector"]);
        assert_eq!(req(&FocusPaneHandler::new(None)), vec!["pane"]);
        assert_eq!(req(&GuiTourHandler::new(None)), vec!["action"]);
        assert_eq!(req(&DesktopPreviewHandler::new(None)), vec!["action"]);
        assert!(req(&AnnotatePreviewHandler::new(None)).is_empty());
        assert!(req(&SetThemeHandler::new(None)).is_empty());
        assert!(req(&ReadWindowBelowHandler::new(None)).is_empty());
    }
}
