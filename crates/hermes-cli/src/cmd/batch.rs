//! `hermes batch` subcommand handler.
//!
//! Runs many prompts from a JSONL file non-interactively and writes the
//! results back as JSONL.
//!
//! Each input line: `{"prompt": "...", "session": "optional-id"}`.
//! Each output record: `{"prompt","session","response",
//! "usage":{"input_tokens","output_tokens"},"duration_ms"}`,
//! or `{"prompt","session","error","duration_ms"}` when a prompt fails.
//! Progress goes to stderr; records go to `--out` or stdout.

use std::collections::HashMap;
use std::io::Write as _;

use crate::cli::Cli;
use hermes_core::{AgentError, Message, MessageRole, UsageStats};

/// A single parsed batch input line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BatchItem {
    pub prompt: String,
    pub session: String,
}

/// Parse one input line into a [`BatchItem`].
///
/// Returns `Ok(None)` for blank lines (skipped). Malformed JSON or a missing
/// `prompt` field is a hard error that fails the whole batch, reporting the
/// 1-based line number like the reference implementation.
pub(crate) fn parse_batch_line(
    line: &str,
    line_no: usize,
) -> Result<Option<BatchItem>, AgentError> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let item: serde_json::Value = serde_json::from_str(line)
        .map_err(|e| AgentError::Io(format!("batch input line {line_no}: invalid JSON: {e}")))?;
    let prompt = item.get("prompt").and_then(|p| p.as_str()).ok_or_else(|| {
        AgentError::Io(format!(
            "batch input line {line_no}: missing string field `prompt`"
        ))
    })?;
    let session = item
        .get("session")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("batch-{line_no}"));
    Ok(Some(BatchItem {
        prompt: prompt.to_string(),
        session,
    }))
}

/// Build the success output record for a finished prompt.
pub(crate) fn success_record(
    item: &BatchItem,
    response: &str,
    usage: Option<&UsageStats>,
    duration_ms: u128,
) -> serde_json::Value {
    let (input_tokens, output_tokens) = usage
        .map(|u| (u.prompt_tokens, u.completion_tokens))
        .unwrap_or((0, 0));
    serde_json::json!({
        "prompt": item.prompt,
        "session": item.session,
        "response": response,
        "usage": { "input_tokens": input_tokens, "output_tokens": output_tokens },
        "duration_ms": duration_ms,
    })
}

/// Build the failure output record for a prompt that errored.
pub(crate) fn error_record(item: &BatchItem, error: &str, duration_ms: u128) -> serde_json::Value {
    serde_json::json!({
        "prompt": item.prompt,
        "session": item.session,
        "error": error,
        "duration_ms": duration_ms,
    })
}

/// Extract the last assistant reply from an agent result's messages.
fn last_assistant_reply(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Assistant)
        .and_then(|m| m.content.clone())
        .unwrap_or_default()
}

/// Run the prompts in `input` (a JSONL file) non-interactively.
///
/// Prompts that share a `session` id share one in-memory conversation
/// history, keyed by session id. Failing prompts produce an error record and
/// the batch continues with the next prompt.
pub async fn cmd_batch(cli: &Cli, input: &str, out: Option<&str>) -> Result<(), AgentError> {
    let data = std::fs::read_to_string(input)
        .map_err(|e| AgentError::Io(format!("cannot read batch input {input:?}: {e}")))?;

    let items: Vec<BatchItem> = data
        .lines()
        .enumerate()
        .filter_map(|(i, line)| parse_batch_line(line, i + 1).transpose())
        .collect::<Result<_, _>>()?;
    if items.is_empty() {
        eprintln!("batch: no prompts found in {input}");
        return Ok(());
    }

    // One App build for the whole batch; the agent loop is stateless across
    // runs, so per-session conversation history is tracked in `histories`.
    let app = crate::app::App::new(cli.clone()).await?;
    let mut histories: HashMap<String, Vec<Message>> = HashMap::new();
    let mut out_buf: Vec<String> = Vec::with_capacity(items.len());

    for (idx, item) in items.iter().enumerate() {
        eprintln!(
            "[{}/{}] session={} {}…",
            idx + 1,
            items.len(),
            item.session,
            item.prompt.chars().take(60).collect::<String>()
        );
        let history = histories.entry(item.session.clone()).or_default();
        history.push(Message::user(&item.prompt));
        let started = std::time::Instant::now();
        let result = app.agent.run(history.clone(), None).await;
        let duration_ms = started.elapsed().as_millis();
        let record = match result {
            Ok(agent_result) => {
                let reply = last_assistant_reply(&agent_result.messages);
                history.push(Message::assistant(&reply));
                success_record(item, &reply, agent_result.usage.as_ref(), duration_ms)
            }
            Err(e) => error_record(item, &e.to_string(), duration_ms),
        };
        out_buf.push(record.to_string());
    }

    let output = out_buf.join("\n");
    match out {
        Some(path) => {
            std::fs::write(path, output)?;
            println!("batch: {} prompt(s) → {path}", items.len());
        }
        None => {
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(output.as_bytes())?;
            if !output.is_empty() {
                stdout.write_all(b"\n")?;
            }
            stdout.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(prompt: &str, session: &str) -> BatchItem {
        BatchItem {
            prompt: prompt.to_string(),
            session: session.to_string(),
        }
    }

    #[test]
    fn parse_line_with_session() {
        let got = parse_batch_line(r#"{"prompt": "hi", "session": "s1"}"#, 3)
            .unwrap()
            .unwrap();
        assert_eq!(got, item("hi", "s1"));
    }

    #[test]
    fn parse_line_defaults_session_to_line_number() {
        let got = parse_batch_line(r#"{"prompt": "hi"}"#, 7).unwrap().unwrap();
        assert_eq!(got, item("hi", "batch-7"));
    }

    #[test]
    fn parse_line_skips_blank_lines() {
        assert!(parse_batch_line("   ", 1).unwrap().is_none());
        assert!(parse_batch_line("", 1).unwrap().is_none());
    }

    #[test]
    fn parse_line_rejects_invalid_json_with_line_number() {
        let err = parse_batch_line("{not json", 4).unwrap_err();
        assert!(err.to_string().contains("line 4"), "got: {err}");
    }

    #[test]
    fn parse_line_rejects_missing_prompt_with_line_number() {
        let err = parse_batch_line(r#"{"session": "s1"}"#, 2).unwrap_err();
        assert!(err.to_string().contains("line 2"), "got: {err}");
        assert!(err.to_string().contains("prompt"), "got: {err}");
    }

    #[test]
    fn parse_line_rejects_non_string_prompt() {
        let err = parse_batch_line(r#"{"prompt": 42}"#, 1).unwrap_err();
        assert!(err.to_string().contains("prompt"), "got: {err}");
    }

    #[test]
    fn success_record_shape() {
        let usage = UsageStats {
            prompt_tokens: 10,
            completion_tokens: 20,
            total_tokens: 30,
            estimated_cost: None,
        };
        let rec = success_record(&item("p", "s"), "hello", Some(&usage), 123);
        assert_eq!(rec["prompt"], "p");
        assert_eq!(rec["session"], "s");
        assert_eq!(rec["response"], "hello");
        assert_eq!(rec["usage"]["input_tokens"], 10);
        assert_eq!(rec["usage"]["output_tokens"], 20);
        assert_eq!(rec["duration_ms"], 123);
        assert!(rec.get("error").is_none());
    }

    #[test]
    fn success_record_without_usage_defaults_to_zero() {
        let rec = success_record(&item("p", "s"), "hi", None, 1);
        assert_eq!(rec["usage"]["input_tokens"], 0);
        assert_eq!(rec["usage"]["output_tokens"], 0);
    }

    #[test]
    fn error_record_shape() {
        let rec = error_record(&item("p", "s"), "boom", 5);
        assert_eq!(rec["prompt"], "p");
        assert_eq!(rec["session"], "s");
        assert_eq!(rec["error"], "boom");
        assert_eq!(rec["duration_ms"], 5);
        assert!(rec.get("response").is_none());
        assert!(rec.get("usage").is_none());
    }

    #[test]
    fn last_assistant_reply_picks_latest_assistant_message() {
        let messages = vec![
            Message::user("q"),
            Message::assistant("first"),
            Message::user("q2"),
            Message::assistant("second"),
        ];
        assert_eq!(last_assistant_reply(&messages), "second");
    }

    #[test]
    fn last_assistant_reply_empty_when_none() {
        assert_eq!(last_assistant_reply(&[Message::user("q")]), "");
    }
}
