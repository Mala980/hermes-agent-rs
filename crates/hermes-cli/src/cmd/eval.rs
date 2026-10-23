//! `hermes eval` handler: run evaluation suites against the agent.
//!
//! Ported from hermes-rs (`crates/hermes-cli/src/cmd/eval.rs`). The JSONL
//! check engine is ported directly rather than driven through
//! `hermes-eval`'s `BatchRunner`, which targets benchmark-style runs
//! (`BenchmarkAdapter`, run records, pass@1, checkpoint resume) — a poor
//! fit for the simple prompt → reply → string/file-checks contract here.
//! The pass/fail verdict helpers (`eval_passes`, `clamp_threshold`) are
//! defined locally: `hermes-eval` exposes no such helpers.
//!
//! Suite format (one JSON object per line):
//! `{"name": "...", "prompt": "...", "checks": ["contains:teks",
//!  "not_contains:teks", "regex:pola", "file_exists:path",
//!  "file_contains:path:teks"]}`
//! `suite` may be `builtin:tools` for the built-in tool smoke suite.

use std::sync::Arc;

use crate::cli::{Cli, EvalCmd};
use hermes_agent::agent_builder::{bridge_tool_registry, build_agent_config, build_provider};
use hermes_config::load_config;
use hermes_core::{AgentError, Message, MessageRole};
use hermes_environments::LocalBackend;
use hermes_skills::{FileSkillStore, SkillManager};
use hermes_tools::ToolRegistry;

/// Default pass threshold (percent) when `--threshold` is not given.
/// hermes-rs reads `[evals].threshold` from config; the fork has no evals
/// config section, so the strict default (100%) applies.
const DEFAULT_EVAL_THRESHOLD: f64 = 100.0;

/// Clamp a pass threshold to the 0–100 percent range.
fn clamp_threshold(threshold: f64) -> f64 {
    threshold.clamp(0.0, 100.0)
}

/// Epsilon-tolerant pass verdict: `score` meets or exceeds `threshold`.
fn eval_passes(score: f64, threshold: f64) -> bool {
    score + 1e-9 >= threshold
}

/// Handle `hermes eval list|run`.
pub async fn cmd_eval(cli: &Cli, cmd: &EvalCmd) -> Result<(), AgentError> {
    match cmd {
        EvalCmd::List => {
            let tools = builtin_tool_suite();
            println!("Built-in suites:");
            println!(
                "  builtin:tools — {} cases (file/exec/memory/todo tool smoke tests)",
                tools.len()
            );
            println!("\nRun: hermes eval run builtin:tools");
            Ok(())
        }
        EvalCmd::Run {
            suite,
            out,
            threshold,
        } => cmd_eval_run(cli, suite, out.as_deref(), *threshold).await,
    }
}

/// Run an eval suite and score the results.
///
/// `threshold_override`: pass threshold percent from `--threshold`;
/// when `None`, the strict default (100%) is used.
pub async fn cmd_eval_run(
    cli: &Cli,
    suite: &str,
    out: Option<&str>,
    threshold_override: Option<f64>,
) -> Result<(), AgentError> {
    let cases: Vec<serde_json::Value> = if suite == "builtin:tools" {
        builtin_tool_suite()
    } else {
        let text =
            std::fs::read_to_string(suite).map_err(|e| AgentError::Io(format!("{suite}: {e}")))?;
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| AgentError::Config(format!("invalid suite JSON: {e}")))?
    };
    if cases.is_empty() {
        return Err(AgentError::Config(
            "empty suite: no cases to run".to_string(),
        ));
    }

    println!("Running {} eval case(s)...", cases.len());
    tracing::debug!(suite = %suite, "eval run starting");

    // Build the agent once (same one-shot wiring as `hermes chat --query`).
    // Eval runs are non-interactive: never block on tool approval prompts.
    let mut config =
        load_config(cli.config_dir.as_deref()).map_err(|e| AgentError::Config(e.to_string()))?;
    config.approval.require_approval = false;
    let model = config.model.clone().unwrap_or_else(|| "gpt-4o".to_string());

    let tool_registry = Arc::new(ToolRegistry::new());
    let terminal_backend: Arc<dyn hermes_core::TerminalBackend> = Arc::new(LocalBackend::default());
    let skill_store = Arc::new(FileSkillStore::new(FileSkillStore::default_dir()));
    let skill_provider: Arc<dyn hermes_core::SkillProvider> =
        Arc::new(SkillManager::new(skill_store));
    hermes_tools::register_builtin_tools(&tool_registry, terminal_backend, skill_provider);
    let agent_tool_registry = Arc::new(bridge_tool_registry(&tool_registry));
    let agent_config = build_agent_config(&config, &model, Some("cli"));
    let provider = build_provider(&config, &model);
    let agent = hermes_agent::AgentLoop::new(agent_config, agent_tool_registry, provider);

    let mut results = Vec::new();
    let mut passed = 0usize;
    for (i, case) in cases.iter().enumerate() {
        let name = case.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let prompt = case.get("prompt").and_then(|p| p.as_str()).unwrap_or("");
        let checks: Vec<String> = case
            .get("checks")
            .and_then(|c| c.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let reply = match agent.run(vec![Message::user(prompt)], None).await {
            Ok(result) => result
                .messages
                .iter()
                .rev()
                .find_map(|m| {
                    if m.role == MessageRole::Assistant {
                        m.content.clone()
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| "(no assistant reply)".to_string()),
            Err(e) => format!("[error: {e}]"),
        };

        let mut failures = Vec::new();
        for check in &checks {
            if let Err(reason) = eval_check(check, &reply) {
                failures.push(format!("{check} → {reason}"));
            }
        }
        let ok = failures.is_empty();
        if ok {
            passed += 1;
        }
        println!(
            "[{}/{}] {} — {}",
            i + 1,
            cases.len(),
            name,
            if ok { "PASS" } else { "FAIL" }
        );
        for f in &failures {
            println!("    - {f}");
        }
        results.push(serde_json::json!({
            "name": name, "passed": ok, "failures": failures,
            "reply_preview": reply.chars().take(300).collect::<String>(),
        }));
    }

    let score = passed as f64 / cases.len() as f64 * 100.0;
    let threshold = clamp_threshold(threshold_override.unwrap_or(DEFAULT_EVAL_THRESHOLD));
    let suite_passed = eval_passes(score, threshold);
    println!(
        "\nScore: {passed}/{} ({score:.1}%) — threshold {threshold:.1}% → {}",
        cases.len(),
        if suite_passed { "PASS" } else { "FAIL" }
    );

    if let Some(out_path) = out {
        let mut text = String::new();
        for r in &results {
            text.push_str(
                &serde_json::to_string(r)
                    .map_err(|e| AgentError::Io(format!("serialize report: {e}")))?,
            );
            text.push('\n');
        }
        text.push_str(
            &serde_json::to_string(&serde_json::json!({
                "summary": {
                    "passed": passed, "total": cases.len(), "score": score,
                    "threshold": threshold, "suite_passed": suite_passed,
                }
            }))
            .map_err(|e| AgentError::Io(format!("serialize summary: {e}")))?,
        );
        text.push('\n');
        std::fs::write(out_path, text)
            .map_err(|e| AgentError::Io(format!("write {out_path}: {e}")))?;
        println!("Report written to {out_path}");
    }

    if !suite_passed {
        return Err(AgentError::ToolExecution(format!(
            "evaluation FAILED: score {score:.1}% below threshold {threshold:.1}%"
        )));
    }
    Ok(())
}

/// Evaluate a single check string against the agent's reply.
pub(crate) fn eval_check(check: &str, reply: &str) -> std::result::Result<(), String> {
    if let Some(needle) = check.strip_prefix("contains:") {
        if reply.contains(needle) {
            Ok(())
        } else {
            Err("reply does not contain text".into())
        }
    } else if let Some(needle) = check.strip_prefix("not_contains:") {
        if !reply.contains(needle) {
            Ok(())
        } else {
            Err("reply contains forbidden text".into())
        }
    } else if let Some(pattern) = check.strip_prefix("regex:") {
        // Minimal regex: literal matching with `.*` as wildcard.
        if simple_regex_match(pattern, reply) {
            Ok(())
        } else {
            Err("regex did not match".into())
        }
    } else if let Some(path) = check.strip_prefix("file_exists:") {
        if std::path::Path::new(path).exists() {
            Ok(())
        } else {
            Err("file does not exist".into())
        }
    } else if let Some(rest) = check.strip_prefix("file_contains:") {
        let (path, needle) = rest
            .split_once(':')
            .ok_or("format file_contains:path:text")?;
        let content =
            std::fs::read_to_string(path).map_err(|_| "file could not be read".to_string())?;
        if content.contains(needle) {
            Ok(())
        } else {
            Err("file does not contain text".into())
        }
    } else {
        Err("unknown check".to_string())
    }
}

/// Minimal regex matching without extra handling: literal text with `.*`
/// acting as a wildcard (ported from hermes-rs; never fails on the pattern).
pub(crate) fn simple_regex_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split(".*").collect();
    if parts.len() == 1 {
        return text.contains(pattern);
    }
    let mut rest = text;
    for part in &parts {
        if part.is_empty() {
            continue;
        }
        match rest.find(part) {
            Some(pos) => rest = &rest[pos + part.len()..],
            None => return false,
        }
    }
    true
}

/// Built-in suite: basic tool capabilities (file, exec, todo, memory).
pub(crate) fn builtin_tool_suite() -> Vec<serde_json::Value> {
    let dir = std::env::temp_dir().join("hermes-eval-tools");
    let f1 = dir.join("eval_test.txt");
    vec![
        serde_json::json!({
            "name": "write_and_read_file",
            "prompt": format!("Write a file at {} containing exactly the sentence HELLO EVAL, then read it back and report its contents.", f1.display()),
            "checks": [
                format!("file_contains:{}:HELLO EVAL", f1.display()),
                "contains:HELLO EVAL",
            ],
        }),
        serde_json::json!({
            "name": "exec_echo",
            "prompt": "Run the shell command: echo EVAL_OK_123. Report its output verbatim.",
            "checks": ["contains:EVAL_OK_123"],
        }),
        serde_json::json!({
            "name": "todo_list_track",
            "prompt": "Create a todo_list with 2 tasks: 'eval task A' and 'eval task B', mark the first one in_progress, then show the list.",
            "checks": ["contains:eval task A", "contains:in_progress"],
        }),
        serde_json::json!({
            "name": "memory_remember",
            "prompt": "Remember this fact with memory: 'the eval favorite color is sky blue'. Then answer: what is the eval favorite color?",
            "checks": ["contains:sky blue"],
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn contains_check() {
        assert!(eval_check("contains:hello", "say hello world").is_ok());
        assert!(eval_check("contains:bye", "say hello world").is_err());
        // Empty needle always matches (substring semantics).
        assert!(eval_check("contains:", "anything").is_ok());
    }

    #[test]
    fn not_contains_check() {
        assert!(eval_check("not_contains:bye", "say hello world").is_ok());
        assert!(eval_check("not_contains:hello", "say hello world").is_err());
    }

    #[test]
    fn regex_literal_match() {
        assert!(simple_regex_match("hello", "say hello world"));
        assert!(!simple_regex_match("bye", "say hello world"));
        assert!(eval_check("regex:hello", "say hello world").is_ok());
        assert!(eval_check("regex:bye", "say hello world").is_err());
    }

    #[test]
    fn regex_wildcard_match() {
        assert!(simple_regex_match("hello.*world", "say hello brave world"));
        assert!(simple_regex_match(".*world", "say hello world"));
        assert!(simple_regex_match("hello.*", "say hello world"));
        assert!(!simple_regex_match("hello.*bye", "say hello world"));
        // Wildcards are ordered: reversed parts must not match.
        assert!(!simple_regex_match("world.*hello", "say hello world"));
        assert!(eval_check("regex:EVAL.*123", "code EVAL_OK_123").is_ok());
    }

    #[test]
    fn file_exists_check() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("present.txt");
        std::fs::write(&present, "x").unwrap();
        assert!(eval_check(&format!("file_exists:{}", present.display()), "irrelevant").is_ok());
        let missing = dir.path().join("missing.txt");
        assert!(eval_check(&format!("file_exists:{}", missing.display()), "irrelevant").is_err());
    }

    #[test]
    fn file_contains_check() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.txt");
        std::fs::write(&file, "the quick brown fox").unwrap();
        assert!(eval_check(
            &format!("file_contains:{}:brown", file.display()),
            "irrelevant"
        )
        .is_ok());
        assert!(eval_check(
            &format!("file_contains:{}:purple", file.display()),
            "irrelevant"
        )
        .is_err());
        // Unreadable / missing file.
        let missing = dir.path().join("nope.txt");
        assert!(eval_check(&format!("file_contains:{}:x", missing.display()), "").is_err());
        // Malformed check (no needle separator).
        assert!(eval_check("file_contains:/tmp/x", "").is_err());
    }

    #[test]
    fn unknown_check_is_error() {
        assert!(eval_check("bogus:whatever", "reply").is_err());
        assert!(eval_check("", "reply").is_err());
    }

    #[test]
    fn threshold_helpers() {
        // Epsilon-tolerant pass verdict.
        assert!(eval_passes(100.0, 100.0));
        assert!(eval_passes(80.0, 75.0));
        assert!(!eval_passes(74.9, 75.0));
        assert!(eval_passes(2.0 / 3.0 * 100.0, 2.0 / 3.0 * 100.0));
        // Clamp to 0–100.
        assert_eq!(clamp_threshold(75.0), 75.0);
        assert_eq!(clamp_threshold(-5.0), 0.0);
        assert_eq!(clamp_threshold(150.0), 100.0);
    }

    #[test]
    fn builtin_suite_shape() {
        let suite = builtin_tool_suite();
        assert_eq!(suite.len(), 4);
        let names: Vec<&str> = suite
            .iter()
            .map(|c| c.get("name").and_then(|n| n.as_str()).unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "write_and_read_file",
                "exec_echo",
                "todo_list_track",
                "memory_remember"
            ]
        );
        for case in &suite {
            assert!(case.get("prompt").and_then(|p| p.as_str()).is_some());
            let checks = case.get("checks").and_then(|c| c.as_array()).unwrap();
            assert!(!checks.is_empty());
            for check in checks {
                let s = check.as_str().unwrap();
                assert!(
                    s.starts_with("contains:")
                        || s.starts_with("not_contains:")
                        || s.starts_with("regex:")
                        || s.starts_with("file_exists:")
                        || s.starts_with("file_contains:"),
                    "unexpected check prefix: {s}"
                );
            }
        }
    }

    #[tokio::test]
    async fn eval_list_does_not_need_agent() {
        // NOTE: intentionally does not go through `CliCommand::Eval` parsing —
        // the `eval` subcommand wiring in cli.rs is owned by the coordinator.
        let cli = Cli::try_parse_from(["hermes"]).unwrap();
        cmd_eval(&cli, &EvalCmd::List).await.unwrap();
    }
}
