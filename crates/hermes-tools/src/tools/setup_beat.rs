//! setup_beat: the onboarding-only tool, simplified.
//!
//! Port of `tools/setup_beat_tool.py`. The Python version drives interactive
//! desktop cards through a callback and persists progress in a session record.
//! This port is stateless and UI-free: each call carries everything it needs,
//! the "card" is returned as JSON text, and answers come back via `value`.
//!
//! Beats: `name`, `theme`, `layout`, `connectors`, `plugins`, `tour`, `build`,
//! plus the pseudo-beats `next` and `done`.

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Value};

use hermes_core::{tool_schema, JsonSchema, ToolError, ToolHandler, ToolSchema};

/// Onboarding beats in order.
const BEATS: &[&str] = &[
    "name",
    "theme",
    "layout",
    "connectors",
    "plugins",
    "tour",
    "build",
];

fn beat_question(beat: &str) -> Option<&'static str> {
    Some(match beat {
        "name" => "What should I call you? Tell me your name.",
        "theme" => "Pick an accent color and light or dark mode.",
        "layout" => "Choose a layout for the app: comfortable or compact.",
        "connectors" => {
            "Which apps should I connect? Name them by slug (e.g. github, gmail, slack)."
        }
        "plugins" => "Which plugins should I install? Name them, or say none.",
        "tour" => "Want a quick tour of the app? Answer yes or skip.",
        "build" => "What should we build first? Describe your first task.",
        _ => return None,
    })
}

/// First beat in [`BEATS`] not present in `completed`.
fn next_beat(completed: &[String]) -> Option<&'static str> {
    BEATS
        .iter()
        .find(|b| !completed.iter().any(|c| c == **b))
        .copied()
}

/// The one onboarding-only tool.
pub struct SetupBeatHandler;

impl SetupBeatHandler {
    pub fn new() -> Self {
        Self
    }

    fn present(beat: &str) -> Value {
        json!({
            "beat": beat,
            "question": beat_question(beat),
        })
    }
}

impl Default for SetupBeatHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolHandler for SetupBeatHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let beat = params
            .get("beat")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .ok_or_else(|| ToolError::InvalidParams("beat is required".into()))?;

        let valid: Vec<&str> = BEATS.iter().copied().chain(["next", "done"]).collect();
        if !valid.contains(&beat) {
            return Err(ToolError::InvalidParams(format!(
                "beat must be one of: {}.",
                valid.join(", ")
            )));
        }

        let completed: Vec<String> = params
            .get("completed")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        if beat == "done" {
            return Ok(json!({
                "done": true,
                "message": "Onboarding complete. This is a normal chat now.",
            })
            .to_string());
        }

        if beat == "next" {
            return match next_beat(&completed) {
                Some(b) => Ok(json!({"next": b, "card": Self::present(b)}).to_string()),
                None => Ok(json!({
                    "next": "done",
                    "message": "Onboarding complete. This is a normal chat now.",
                })
                .to_string()),
            };
        }

        // A concrete beat: record a spoken answer, a skip, or present the card.
        let value = params
            .get("value")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty());
        let skip = params.get("skip").and_then(|v| v.as_bool()).unwrap_or(false);

        if skip {
            let mut done = completed;
            if !done.iter().any(|c| c == beat) {
                done.push(beat.to_string());
            }
            return Ok(json!({
                "beat": beat,
                "skipped": true,
                "next": next_beat(&done),
            })
            .to_string());
        }

        if let Some(v) = value {
            let mut done = completed;
            if !done.iter().any(|c| c == beat) {
                done.push(beat.to_string());
            }
            return Ok(json!({
                "beat": beat,
                "recorded": v,
                "next": next_beat(&done),
            })
            .to_string());
        }

        // No answer yet: present the card.
        Ok(json!({"beat": beat, "card": Self::present(beat)}).to_string())
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "beat".into(),
            json!({
                "type": "string",
                "description": "Which onboarding beat: next, done, or a beat id (name, theme, layout, connectors, plugins, tour, build).",
                "enum": ["next", "done", "name", "theme", "layout", "connectors", "plugins", "tour", "build"]
            }),
        );
        props.insert(
            "value".into(),
            json!({
                "type": "string",
                "description": "Record an answer given in words (a name, or an option id). No card."
            }),
        );
        props.insert(
            "skip".into(),
            json!({
                "type": "boolean",
                "description": "They skipped this beat in words. No card.",
                "default": false
            }),
        );
        props.insert(
            "completed".into(),
            json!({
                "type": "array",
                "items": {"type": "string"},
                "description": "Beat ids already finished; used by beat=next to advance."
            }),
        );
        props.insert(
            "question".into(),
            json!({
                "type": "string",
                "description": "Your line above the card."
            }),
        );
        tool_schema(
            "setup_beat",
            "Run the first-run onboarding in this chat. Shows the card for a beat and waits for the user, then returns their answer and what to do next. beat=next shows the next beat.",
            JsonSchema::object(props, vec!["beat".into()]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beat_ids_in_order() {
        assert_eq!(
            BEATS,
            &["name", "theme", "layout", "connectors", "plugins", "tour", "build"]
        );
    }

    #[test]
    fn every_beat_has_a_question() {
        for b in BEATS {
            assert!(beat_question(b).is_some(), "beat {b} has no question");
        }
        assert!(beat_question("nope").is_none());
    }

    #[test]
    fn next_beat_advances_over_completed() {
        assert_eq!(next_beat(&[]), Some("name"));
        assert_eq!(next_beat(&["name".to_string()]), Some("theme"));
        assert_eq!(
            next_beat(&["name".to_string(), "theme".to_string(), "layout".to_string()]),
            Some("connectors")
        );
        let all: Vec<String> = BEATS.iter().map(|s| s.to_string()).collect();
        assert_eq!(next_beat(&all), None);
    }

    #[tokio::test]
    async fn execute_rejects_unknown_beat() {
        let h = SetupBeatHandler::new();
        let err = h.execute(json!({"beat": "bogus"})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn execute_requires_beat() {
        let h = SetupBeatHandler::new();
        let err = h.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn execute_done() {
        let h = SetupBeatHandler::new();
        let out: Value =
            serde_json::from_str(&h.execute(json!({"beat": "done"})).await.unwrap()).unwrap();
        assert_eq!(out["done"], true);
    }

    #[tokio::test]
    async fn execute_next_returns_first_incomplete() {
        let h = SetupBeatHandler::new();
        let out: Value = serde_json::from_str(
            &h.execute(json!({"beat": "next", "completed": ["name"]}))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["next"], "theme");
        assert_eq!(out["card"]["beat"], "theme");
    }

    #[tokio::test]
    async fn execute_next_when_all_done() {
        let h = SetupBeatHandler::new();
        let all: Vec<String> = BEATS.iter().map(|s| s.to_string()).collect();
        let out: Value = serde_json::from_str(
            &h.execute(json!({"beat": "next", "completed": all}))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["next"], "done");
    }

    #[tokio::test]
    async fn execute_records_value_and_advances() {
        let h = SetupBeatHandler::new();
        let out: Value = serde_json::from_str(
            &h.execute(json!({"beat": "name", "value": "Chewy"}))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["recorded"], "Chewy");
        assert_eq!(out["next"], "theme");
    }

    #[tokio::test]
    async fn execute_skip_advances() {
        let h = SetupBeatHandler::new();
        let out: Value = serde_json::from_str(
            &h.execute(
                json!({"beat": "tour", "skip": true, "completed": ["name", "theme", "layout", "connectors", "plugins"]}),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(out["skipped"], true);
        assert_eq!(out["next"], "build");
    }

    #[tokio::test]
    async fn execute_presents_card_without_answer() {
        let h = SetupBeatHandler::new();
        let out: Value = serde_json::from_str(
            &h.execute(json!({"beat": "connectors"})).await.unwrap(),
        )
        .unwrap();
        assert_eq!(out["card"]["beat"], "connectors");
        assert!(out["card"]["question"].as_str().unwrap().len() > 0);
    }

    #[test]
    fn schema_has_expected_shape() {
        let s = SetupBeatHandler::new().schema();
        assert_eq!(s.name, "setup_beat");
    }
}
