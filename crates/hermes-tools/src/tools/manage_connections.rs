//! `manage_connections`: connection lifecycle for hosted connector accounts and
//! local MCP servers.
//!
//! The reference implementation's handler is deeply wired into the desktop
//! approval-card flow, the Nous Portal, and the gateway's connector client.
//! This port keeps the parts that work without that machinery:
//!
//! - `status` reads the MCP server list from the local config
//!   (`mcp_servers` entries) and reports it.
//! - Every other action runs the same argument validation as the original
//!   (`normalize_targets` / `validate_action`) and then returns a clear
//!   error explaining which surface performs it (desktop app / Nous Portal).
//!
//! LIMITATIONS vs the Python original:
//! - No approval cards, no OAuth flows, no connect/reconnect/rename/install
//!   execution — those need the desktop bridge or the portal.
//! - Hosted connector accounts (Gmail, Notion, …) are not listed: only the
//!   local `mcp_servers` config is visible here.

use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Value};

use hermes_core::{tool_schema, JsonSchema, ToolError, ToolHandler, ToolSchema};

/// Actions on hosted connector accounts.
const CONNECTOR_ACTIONS: &[&str] = &["display", "status", "connect", "reconnect", "rename"];
/// Actions on local MCP servers.
const MCP_ACTIONS: &[&str] = &["install", "enable", "authorize"];

/// One hosted-connector target: a slug plus an optional account alias / rename.
#[derive(Debug, Clone, PartialEq)]
pub struct HostedTarget {
    pub name: String,
    pub alias: Option<String>,
    pub to: Option<String>,
}

/// Split raw `connectors` into hosted targets and MCP server names.
///
/// Mirrors `normalize_targets` in the reference implementation. Returns
/// `(hosted, mcp)` or a human-readable error.
pub fn normalize_targets(raw: Option<&Value>) -> Result<(Vec<HostedTarget>, Vec<String>), String> {
    let items: Vec<&Value> = match raw {
        None | Some(Value::Null) => return Ok((vec![], vec![])),
        Some(Value::Array(a)) => a.iter().collect(),
        Some(Value::String(_)) | Some(Value::Object(_)) => vec![raw.unwrap()],
        Some(_) => {
            return Err("'connectors' must be a list of names or {name, mcp} objects.".into())
        }
    };
    let mut managed: Vec<HostedTarget> = vec![];
    let mut mcp: Vec<String> = vec![];
    for item in items {
        let (name, is_mcp, alias, to) = match item {
            Value::Object(map) => {
                let known = ["name", "mcp", "alias", "to"];
                let unknown: Vec<&String> =
                    map.keys().filter(|k| !known.contains(&k.as_str())).collect();
                if !unknown.is_empty() {
                    return Err(format!(
                        "unknown target field(s) {}: a target is {{\"name\": \"<slug>\"}} or {{\"name\": \"<server>\", \"mcp\": true}}.",
                        unknown
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                let name = map
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_lowercase();
                let is_mcp = map.get("mcp").and_then(|v| v.as_bool()).unwrap_or(false);
                if is_mcp && (map.contains_key("alias") || map.contains_key("to")) {
                    return Err(
                        "'alias' and 'to' name hosted connector accounts; an MCP target takes neither."
                            .into(),
                    );
                }
                let alias = map
                    .get("alias")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let to = map
                    .get("to")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                (name, is_mcp, alias, to)
            }
            Value::String(s) => (s.trim().to_lowercase(), false, None, None),
            _ => {
                return Err(
                    "every target is a name string or a {name, mcp} object.".into(),
                )
            }
        };
        if name.is_empty() {
            return Err("every target needs a non-empty 'name'.".into());
        }
        if is_mcp {
            if !mcp.contains(&name) {
                mcp.push(name);
            }
            continue;
        }
        let target = HostedTarget { name, alias, to };
        if !managed.iter().any(|t| t.name == target.name && t.alias == target.alias) {
            managed.push(target);
        }
    }
    Ok((managed, mcp))
}

/// Validate the action against the parsed targets.
/// Mirrors `validate_action` in the reference implementation.
pub fn validate_action(
    action: &str,
    managed: &[HostedTarget],
    mcp: &[String],
) -> Option<String> {
    let all: Vec<&str> = CONNECTOR_ACTIONS
        .iter()
        .chain(MCP_ACTIONS.iter())
        .copied()
        .collect();
    if !all.contains(&action) {
        return Some(format!(
            "action must be one of {}. {} apply to local MCP servers (targets {{\"name\": ..., \"mcp\": true}}); the rest apply to managed connectors.",
            all.join(", "),
            MCP_ACTIONS.join(", ")
        ));
    }
    if MCP_ACTIONS.contains(&action) {
        if !managed.is_empty() {
            return Some(format!(
                "'{action}' is an MCP action: every target must carry \"mcp\": true (got managed connector(s) {}).",
                managed.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        if mcp.is_empty() {
            return Some(format!(
                "'{action}' requires 'connectors': the MCP server name(s), e.g. [{{\"name\": \"linear\", \"mcp\": true}}]."
            ));
        }
    } else if !mcp.is_empty() {
        return Some(format!(
            "'{action}' is a managed-connector action; MCP targets ({}) use {}.",
            mcp.join(", "),
            MCP_ACTIONS.join(", ")
        ));
    }
    if action != "rename" && managed.iter().any(|t| t.to.is_some()) {
        return Some("'to' is the new name of an account and applies to action rename only.".into());
    }
    None
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// Read-only view of the locally configured MCP servers, for `status`.
#[derive(Debug, Clone)]
pub struct LocalMcpServer {
    pub name: String,
    pub transport: String,
}

impl From<&hermes_config::McpServerEntry> for LocalMcpServer {
    fn from(e: &hermes_config::McpServerEntry) -> Self {
        let transport = if e.url.is_some() {
            "http"
        } else if e.command.is_some() {
            "stdio"
        } else {
            "unknown"
        }
        .to_string();
        Self {
            name: e.name.clone(),
            transport,
        }
    }
}

/// `manage_connections` tool handler.
pub struct ManageConnectionsHandler {
    mcp_servers: Arc<Vec<LocalMcpServer>>,
}

impl ManageConnectionsHandler {
    pub fn new(mcp_servers: Vec<LocalMcpServer>) -> Self {
        Self {
            mcp_servers: Arc::new(mcp_servers),
        }
    }
}

#[async_trait]
impl ToolHandler for ManageConnectionsHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "status".to_string());

        let (managed, mcp) = normalize_targets(params.get("connectors"))
            .map_err(ToolError::InvalidParams)?;
        if let Some(err) = validate_action(&action, &managed, &mcp) {
            return Err(ToolError::InvalidParams(err));
        }

        match action.as_str() {
            "status" => {
                let servers: Vec<Value> = self
                    .mcp_servers
                    .iter()
                    .map(|s| json!({"name": s.name, "transport": s.transport, "kind": "mcp"}))
                    .collect();
                Ok(json!({
                    "mcp_servers": servers,
                    "note": "Hosted connector accounts (Gmail, Notion, …) are managed in the Nous Portal or the desktop app; only local mcp_servers config is visible here.",
                })
                .to_string())
            }
            other => Err(ToolError::ExecutionFailed(format!(
                "action '{other}' needs an interactive surface: use the Hermes desktop app \
                 (approval card) or the Nous Portal for hosted connectors, and `hermes mcp` \
                 for local MCP servers. This tool only implements 'status'."
            ))),
        }
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        let mut all: Vec<&str> = CONNECTOR_ACTIONS.to_vec();
        all.extend_from_slice(MCP_ACTIONS);
        props.insert(
            "action".into(),
            json!({
                "type": "string",
                "enum": all,
                "description": "Defaults to status. Only 'status' is implemented here; other actions need the desktop app or portal."
            }),
        );
        props.insert(
            "connectors".into(),
            json!({
                "type": "array",
                "description": "Targets: a bare slug is a hosted connector; {\"name\": \"x\", \"mcp\": true} is a local MCP server.",
                "items": {"anyOf": [{"type": "string"}, {"type": "object"}]},
            }),
        );
        tool_schema(
            "manage_connections",
            "List locally configured MCP servers ('status'). Other connection actions (connect, install, …) require the desktop app or Nous Portal and are not executed here.",
            JsonSchema::object(props, vec![]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_none_gives_empty() {
        assert_eq!(normalize_targets(None).unwrap(), (vec![], vec![]));
    }

    #[test]
    fn normalize_bare_string_is_hosted() {
        let (managed, mcp) = normalize_targets(Some(&json!("gmail"))).unwrap();
        assert_eq!(managed.len(), 1);
        assert_eq!(managed[0].name, "gmail");
        assert!(mcp.is_empty());
    }

    #[test]
    fn normalize_mcp_object() {
        let (managed, mcp) =
            normalize_targets(Some(&json!([{"name": "linear", "mcp": true}]))).unwrap();
        assert!(managed.is_empty());
        assert_eq!(mcp, vec!["linear"]);
    }

    #[test]
    fn normalize_rejects_unknown_fields() {
        assert!(normalize_targets(Some(&json!([{"name": "x", "bogus": 1}]))).is_err());
    }

    #[test]
    fn normalize_rejects_alias_on_mcp() {
        assert!(normalize_targets(Some(&json!([{"name": "x", "mcp": true, "alias": "a"}]))).is_err());
    }

    #[test]
    fn validate_unknown_action() {
        assert!(validate_action("explode", &[], &[]).is_some());
    }

    #[test]
    fn validate_mcp_action_needs_mcp_targets() {
        // install with a hosted target -> error
        let managed = vec![HostedTarget { name: "gmail".into(), alias: None, to: None }];
        assert!(validate_action("install", &managed, &[]).is_some());
        // install with no targets -> error
        assert!(validate_action("install", &[], &[]).is_some());
        // install with mcp target -> ok
        assert!(validate_action("install", &[], &["linear".to_string()]).is_none());
    }

    #[test]
    fn validate_hosted_action_rejects_mcp_targets() {
        assert!(validate_action("connect", &[], &["linear".to_string()]).is_some());
    }

    #[test]
    fn validate_to_only_for_rename() {
        let managed = vec![HostedTarget {
            name: "gmail".into(),
            alias: None,
            to: Some("work".into()),
        }];
        assert!(validate_action("connect", &managed, &[]).is_some());
        assert!(validate_action("rename", &managed, &[]).is_none());
    }

    #[test]
    fn schema_name_matches_original() {
        assert_eq!(
            ManageConnectionsHandler::new(vec![]).schema().name,
            "manage_connections"
        );
    }
}
