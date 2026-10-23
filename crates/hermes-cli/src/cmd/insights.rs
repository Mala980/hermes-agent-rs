//! `hermes insights` — show session insights.
//!
//! Ported from NousResearch/hermes-agent's `hermes_cli/main_agent_cmds.py::
//! cmd_insights`, which read the SQLite state db and ran `InsightsEngine`.
//! This fork persists sessions as JSON files under `~/.hermes/sessions/`,
//! so this command adapts those files into the
//! `hermes-intelligence::session_insights` data model
//! (`SessionDataSource` → `SessionInsightsEngine` → `format_terminal`),
//! exactly the same engine the gateway's `/insights` renders.

use chrono::Utc;
use hermes_config::hermes_home;
use hermes_intelligence::session_insights::{
    format_terminal, AssistantToolCallRow, InMemorySessionData, SessionInsightsEngine, SessionRow,
    ToolMessageRow,
};
use std::time::{SystemTime, UNIX_EPOCH};

/// CLI args for `hermes insights`.
#[derive(clap::Args, Debug, Clone)]
pub struct InsightsArgs {
    /// Only include sessions from the last N days.
    #[arg(long, default_value_t = 30)]
    pub days: u32,
    /// Only include sessions from this source (e.g. "cli", "telegram").
    #[arg(long)]
    pub source: Option<String>,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Build an in-memory [`SessionDataSource`] from the session files on disk.
///
/// Sessions lacking token data still contribute message/tool counts; cost
/// columns stay `None` so the engine's pricing logic decides what to show.
pub fn load_session_data(
    sessions_dir: &std::path::Path,
    days: u32,
    source_filter: Option<&str>,
) -> InMemorySessionData {
    let mut data = InMemorySessionData::default();
    let cutoff = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
        - f64::from(days) * 86400.0;

    let Ok(rd) = std::fs::read_dir(sessions_dir) else {
        return data;
    };
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().map(|e| e != "json").unwrap_or(true) {
            continue;
        }
        let id = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        if mtime < cutoff {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let source = value
            .get("source")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());
        if let Some(filter) = source_filter {
            if source.as_deref() != Some(filter) {
                continue;
            }
        }

        let mut user = 0u64;
        let mut assistant = 0u64;
        let mut tool = 0u64;
        let mut tool_call_count = 0u64;
        if let Some(msgs) = value.get("messages").and_then(|m| m.as_array()) {
            for msg in msgs {
                match msg.get("role").and_then(|r| r.as_str()) {
                    Some("user") => user += 1,
                    Some("assistant") => {
                        assistant += 1;
                        let mut names: Vec<String> = Vec::new();
                        if let Some(calls) = msg.get("tool_calls").and_then(|c| c.as_array()) {
                            for call in calls {
                                tool_call_count += 1;
                                let name = call
                                    .get("name")
                                    .or_else(|| call.get("function").and_then(|f| f.get("name")))
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("(unknown)")
                                    .to_string();
                                names.push(name);
                            }
                        }
                        if !names.is_empty() {
                            data.assistant_tool_calls.push(AssistantToolCallRow {
                                session_id: id.clone(),
                                source: source.clone(),
                                tool_names: names,
                            });
                        }
                    }
                    Some("tool") => {
                        tool += 1;
                        if let Some(name) = msg.get("tool_name").and_then(|n| n.as_str()) {
                            let row = data
                                .tool_messages
                                .iter_mut()
                                .find(|r| r.session_id == id && r.tool_name == name);
                            match row {
                                Some(r) => r.count += 1,
                                None => data.tool_messages.push(ToolMessageRow {
                                    session_id: id.clone(),
                                    source: source.clone(),
                                    tool_name: name.to_string(),
                                    count: 1,
                                }),
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        let usage = value.get("usage");
        data.sessions.push(SessionRow {
            id: id.clone(),
            source: source.clone(),
            model: value
                .get("model")
                .and_then(|m| m.as_str())
                .map(|s| s.to_string()),
            started_at: value
                .get("created_at")
                .and_then(|v| v.as_f64())
                .or(Some(mtime)),
            ended_at: Some(mtime),
            message_count: Some(user + assistant + tool),
            tool_call_count: Some(tool_call_count),
            input_tokens: usage
                .and_then(|u| u.get("input_tokens"))
                .and_then(|v| v.as_u64()),
            output_tokens: usage
                .and_then(|u| u.get("output_tokens"))
                .and_then(|v| v.as_u64()),
            cache_read_tokens: None,
            cache_write_tokens: None,
            billing_provider: None,
            billing_base_url: None,
            billing_mode: None,
            estimated_cost_usd: None,
            actual_cost_usd: usage.and_then(|u| u.get("cost")).and_then(|v| v.as_f64()),
            cost_status: None,
            cost_source: None,
        });
        data.message_counts.total_messages += user + assistant + tool;
        data.message_counts.user_messages += user;
        data.message_counts.assistant_messages += assistant;
        data.message_counts.tool_messages += tool;
    }
    data
}

/// Print session insights for the last `days` days.
pub async fn run(args: &InsightsArgs) -> anyhow::Result<()> {
    let sessions_dir = hermes_home().join("sessions");
    if !sessions_dir.exists() {
        println!("No session data found (no ~/.hermes/sessions directory).");
        return Ok(());
    }
    let data = load_session_data(&sessions_dir, args.days, args.source.as_deref());
    let engine = SessionInsightsEngine::new(&data);
    let now = Utc::now().timestamp() as f64;
    let report = engine.generate(args.days, args.source.as_deref(), now);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{}", format_terminal(&report));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_intelligence::session_insights::SessionDataSource;

    fn write_session(dir: &std::path::Path, name: &str, source: &str) {
        let doc = serde_json::json!({
            "model": "gpt-4o",
            "source": source,
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello",
                 "tool_calls": [{"name": "read_file"}, {"name": "read_file"}]},
                {"role": "tool", "tool_name": "read_file", "content": "ok"},
            ],
            "usage": {"input_tokens": 100, "output_tokens": 50, "cost": 0.001},
        });
        std::fs::write(dir.join(format!("{name}.json")), doc.to_string()).unwrap();
    }

    #[test]
    fn load_session_data_counts_messages_and_tools() {
        let dir = std::env::temp_dir().join("hermes-insights-test");
        std::fs::create_dir_all(&dir).unwrap();
        write_session(&dir, "a", "cli");
        write_session(&dir, "b", "telegram");

        let data = load_session_data(&dir, 30, None);
        assert_eq!(data.sessions.len(), 2);
        assert_eq!(data.message_counts.total_messages, 6);
        assert_eq!(data.message_counts.user_messages, 2);
        assert_eq!(data.message_counts.assistant_messages, 2);
        assert_eq!(data.message_counts.tool_messages, 2);
        assert_eq!(data.assistant_tool_calls.len(), 2);
        // Two assistant messages each with 2 read_file calls; the engine
        // merges by taking max per tool, so fetch and check the rows exist.
        let usage = data.fetch_tool_usage(0.0, None);
        assert!(usage.iter().any(|r| r.tool_name == "read_file"));

        // Source filter.
        let cli_only = load_session_data(&dir, 30, Some("cli"));
        assert_eq!(cli_only.sessions.len(), 1);
        assert_eq!(cli_only.sessions[0].source.as_deref(), Some("cli"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn engine_generates_nonempty_report() {
        let dir = std::env::temp_dir().join("hermes-insights-test-engine");
        std::fs::create_dir_all(&dir).unwrap();
        write_session(&dir, "a", "cli");
        let data = load_session_data(&dir, 30, None);
        let engine = SessionInsightsEngine::new(&data);
        let report = engine.generate(30, None, Utc::now().timestamp() as f64);
        assert!(!report.empty);
        assert_eq!(report.days, 30);
        assert_eq!(report.overview.total_sessions, 1);
        let text = format_terminal(&report);
        assert!(!text.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_dir_yields_empty_report() {
        let dir = std::env::temp_dir().join("hermes-insights-test-empty");
        std::fs::create_dir_all(&dir).unwrap();
        let data = load_session_data(&dir, 30, None);
        let engine = SessionInsightsEngine::new(&data);
        let report = engine.generate(30, None, Utc::now().timestamp() as f64);
        assert!(report.empty);
        std::fs::remove_dir_all(&dir).ok();
    }
}
