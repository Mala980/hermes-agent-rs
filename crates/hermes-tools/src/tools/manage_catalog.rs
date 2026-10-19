//! `manage_catalog`: catalog search and install for plugins and hub skills.
//!
//! The reference implementation draws an approval card in the desktop app and
//! blocks until the user settles each row. This port simplifies without the
//! card: arguments are validated exactly like the original (`_check_args`),
//! `search` explains how to browse from the terminal, and `install` points at
//! the CLI commands that perform it.
//!
//! LIMITATIONS vs the Python original:
//! - No approval card, no blocking install flow (needs the desktop surface).
//! - `search` does not query the remote catalog/hub; it directs the user to
//!   the terminal equivalents.

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Value};

use hermes_core::{tool_schema, JsonSchema, ToolError, ToolHandler, ToolSchema};

/// Valid actions.
pub const ACTIONS: &[&str] = &["search", "install"];
/// Valid item kinds.
pub const ITEM_KINDS: &[&str] = &["plugin", "skill"];

const TOP_KEYS: &[&str] = &["action", "kind", "query", "items", "reason"];
const ITEM_KEYS: &[&str] = &["kind", "id"];

const NOT_HERE: &str = "manage_catalog install needs the Hermes desktop app, where the approval card can be drawn. From a terminal instead: `hermes plugins install <id>` for a plugin, `hermes skills install <id>` for a skill.";

/// One validated install item: (kind, id).
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogItem {
    pub kind: String,
    pub id: String,
}

/// Validated arguments for `manage_catalog`.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogArgs {
    pub action: String,
    pub kind: Option<String>,
    pub query: Option<String>,
    pub items: Vec<CatalogItem>,
    pub reason: Option<String>,
}

/// Validate raw args exactly like `_check_args` in the reference implementation.
/// Pure logic, unit-tested.
pub fn validate_catalog_args(args: &Value) -> Result<CatalogArgs, String> {
    let map = args.as_object().ok_or_else(|| {
        "manage_catalog takes an object with action, kind, query, items and reason.".to_string()
    })?;
    let unknown: Vec<&String> = map
        .keys()
        .filter(|k| !TOP_KEYS.contains(&k.as_str()))
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "unknown parameter(s) {}: manage_catalog takes action, kind, query, items and reason.",
            unknown
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let action = map
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !ACTIONS.contains(&action.as_str()) {
        return Err(format!("action must be one of {}.", ACTIONS.join(", ")));
    }
    let kind = map
        .get("kind")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(k) = &kind {
        if !ITEM_KINDS.contains(&k.as_str()) {
            return Err(format!("kind must be one of {}.", ITEM_KINDS.join(", ")));
        }
    }
    let query = map
        .get("query")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty());
    let reason = map
        .get("reason")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let mut items = vec![];
    if action == "install" {
        let raw_items = map.get("items").and_then(|v| v.as_array()).ok_or_else(|| {
            "install needs 'items': [{\"kind\": \"plugin\", \"id\": \"<catalog id>\"}, ...].".to_string()
        })?;
        if raw_items.is_empty() {
            return Err(
                "install needs 'items': [{\"kind\": \"plugin\", \"id\": \"<catalog id>\"}, ...]."
                    .to_string(),
            );
        }
        for item in raw_items {
            let obj = item
                .as_object()
                .ok_or_else(|| "every item is an object {kind, id}.".to_string())?;
            let extra: Vec<&String> = obj
                .keys()
                .filter(|k| !ITEM_KEYS.contains(&k.as_str()))
                .collect();
            if !extra.is_empty() {
                return Err(format!(
                    "unknown item field(s) {}: an item is {{kind, id}}.",
                    extra
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            let kind = obj
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let id = obj
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if !ITEM_KINDS.contains(&kind.as_str()) || id.is_empty() {
                return Err(
                    "every item needs kind (plugin or skill) and a non-empty id.".to_string(),
                );
            }
            items.push(CatalogItem { kind, id });
        }
    }
    Ok(CatalogArgs {
        action,
        kind,
        query,
        items,
        reason,
    })
}

/// `manage_catalog` tool handler (simplified: no approval card).
pub struct ManageCatalogHandler;

impl ManageCatalogHandler {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ManageCatalogHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolHandler for ManageCatalogHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let args = validate_catalog_args(&params).map_err(ToolError::InvalidParams)?;
        match args.action.as_str() {
            "search" => Ok(json!({
                "results": [],
                "note": "Catalog search needs the Hermes desktop app. From a terminal: `hermes plugins search <query>` for plugins, `hermes skills hub search <query>` for hub skills.",
                "query": args.query,
                "kind": args.kind,
            })
            .to_string()),
            "install" => Err(ToolError::ExecutionFailed(format!(
                "{NOT_HERE} Requested items: {}",
                args.items
                    .iter()
                    .map(|i| format!("{}:{}", i.kind, i.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
            _ => unreachable!("validated above"),
        }
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "action".into(),
            json!({"type": "string", "enum": ACTIONS}),
        );
        props.insert(
            "kind".into(),
            json!({
                "type": "string",
                "enum": ITEM_KINDS,
                "description": "search only: limit results to plugins or skills."
            }),
        );
        props.insert(
            "query".into(),
            json!({"type": "string", "description": "search only: words to match; empty lists plugins."}),
        );
        props.insert(
            "items".into(),
            json!({
                "type": "array",
                "description": "install only: the catalog items to offer.",
                "items": {
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string", "enum": ITEM_KINDS},
                        "id": {"type": "string"},
                    },
                    "required": ["kind", "id"],
                    "additionalProperties": false,
                },
            }),
        );
        props.insert(
            "reason".into(),
            json!({"type": "string", "description": "install only: one line on why these are offered."}),
        );
        tool_schema(
            "manage_catalog",
            "Find and install Hermes catalog plugins and hub skills. 'search' lists matches; 'install' takes catalog ids. Without the desktop approval card, install directs you to the terminal equivalents.",
            JsonSchema::object(props, vec!["action".into()]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_unknown_top_level_key() {
        assert!(validate_catalog_args(&json!({"action": "search", "bogus": 1})).is_err());
    }

    #[test]
    fn rejects_bad_action() {
        assert!(validate_catalog_args(&json!({"action": "delete"})).is_err());
    }

    #[test]
    fn rejects_bad_kind() {
        assert!(validate_catalog_args(&json!({"action": "search", "kind": "theme"})).is_err());
    }

    #[test]
    fn search_args_valid() {
        let a = validate_catalog_args(&json!({"action": "search", "query": "git"})).unwrap();
        assert_eq!(a.action, "search");
        assert_eq!(a.query.as_deref(), Some("git"));
        assert!(a.items.is_empty());
    }

    #[test]
    fn install_needs_items() {
        assert!(validate_catalog_args(&json!({"action": "install"})).is_err());
        assert!(validate_catalog_args(&json!({"action": "install", "items": []})).is_err());
    }

    #[test]
    fn install_validates_each_item() {
        let a = validate_catalog_args(&json!({
            "action": "install",
            "items": [{"kind": "plugin", "id": "github"}],
        }))
        .unwrap();
        assert_eq!(
            a.items,
            vec![CatalogItem { kind: "plugin".into(), id: "github".into() }]
        );
        // bad kind
        assert!(validate_catalog_args(&json!({
            "action": "install",
            "items": [{"kind": "theme", "id": "x"}],
        }))
        .is_err());
        // empty id
        assert!(validate_catalog_args(&json!({
            "action": "install",
            "items": [{"kind": "skill", "id": "  "}],
        }))
        .is_err());
        // extra field
        assert!(validate_catalog_args(&json!({
            "action": "install",
            "items": [{"kind": "skill", "id": "x", "version": "1"}],
        }))
        .is_err());
    }

    #[test]
    fn schema_name_matches_original() {
        assert_eq!(ManageCatalogHandler::new().schema().name, "manage_catalog");
    }
}
