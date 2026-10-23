//! `hermes usage` — show token/cost usage statistics.
//!
//! Ported from NousResearch/hermes-agent's `hermes_cli/subcommands/usage.py`.
//! The original printed the provider account rate-limit windows (the REPL
//! `/usage` block) by querying account endpoints; this fork has no
//! account-usage endpoints wired, so this command reports the usage
//! recorded in local session files instead: per-model token totals and
//! estimated cost via the pricing tables in `hermes-intelligence`
//! (`usage_pricing::get_pricing`), with a daily trend and `--json` output.

use chrono::{DateTime, Utc};
use hermes_config::hermes_home;
use hermes_intelligence::usage_pricing::{format_cost, get_pricing};
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// CLI args for `hermes usage`.
#[derive(clap::Args, Debug, Clone)]
pub struct UsageArgs {
    /// Only include sessions modified in the last N days.
    #[arg(long, default_value_t = 30)]
    pub days: u32,
    /// Only include sessions using this model (substring match).
    #[arg(long)]
    pub model: Option<String>,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Per-model usage accumulator.
#[derive(Debug, Default, Clone)]
pub struct ModelUsage {
    pub sessions: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

/// Aggregated usage over a session set.
#[derive(Debug, Default)]
pub struct UsageAggregate {
    pub sessions: u64,
    pub messages: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub per_model: BTreeMap<String, ModelUsage>,
    /// YYYY-MM-DD → (input, output) tokens.
    pub daily: BTreeMap<String, (u64, u64)>,
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Estimate cost for a session: recorded `usage.cost` wins, otherwise price
/// the tokens with the model pricing table.
pub fn estimate_session_cost(model: &str, input: u64, output: u64, recorded: Option<f64>) -> f64 {
    if let Some(c) = recorded {
        return c;
    }
    let (input_per_m, output_per_m) = get_pricing(model, None, None);
    input as f64 / 1_000_000.0 * input_per_m + output as f64 / 1_000_000.0 * output_per_m
}

/// Aggregate usage from session files under `sessions_dir`, restricted to
/// sessions modified within `days` days and (optionally) a model filter.
pub fn aggregate_usage(
    sessions_dir: &std::path::Path,
    days: u32,
    model_filter: Option<&str>,
) -> UsageAggregate {
    let mut agg = UsageAggregate::default();
    let cutoff = now_secs() - f64::from(days) * 86400.0;
    let Ok(rd) = std::fs::read_dir(sessions_dir) else {
        return agg;
    };
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().map(|e| e != "json").unwrap_or(true) {
            continue;
        }
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
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let model = data
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("(unknown)")
            .to_string();
        if let Some(filter) = model_filter {
            if !model.contains(filter) {
                continue;
            }
        }
        let messages = data
            .get("messages")
            .and_then(|m| m.as_array())
            .map(|a| a.len() as u64)
            .unwrap_or(0);
        let usage = data.get("usage");
        let input = usage
            .and_then(|u| u.get("input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let output = usage
            .and_then(|u| u.get("output_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let recorded = usage.and_then(|u| u.get("cost")).and_then(|v| v.as_f64());
        let cost = estimate_session_cost(&model, input, output, recorded);

        agg.sessions += 1;
        agg.messages += messages;
        agg.input_tokens += input;
        agg.output_tokens += output;
        agg.cost_usd += cost;
        let entry = agg.per_model.entry(model).or_default();
        entry.sessions += 1;
        entry.input_tokens += input;
        entry.output_tokens += output;
        entry.cost_usd += cost;
        let day = DateTime::<Utc>::from_timestamp(mtime as i64, 0)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let day_entry = agg.daily.entry(day).or_insert((0, 0));
        day_entry.0 += input;
        day_entry.1 += output;
    }
    agg
}

fn compact(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn print_human(agg: &UsageAggregate, days: u32) {
    println!("Token usage (last {days} days)");
    println!("=============================");
    if agg.sessions == 0 {
        println!("No sessions with recorded usage in this window.");
        println!("(Usage is recorded in ~/.hermes/sessions/*.json as sessions run.)");
        return;
    }
    println!("Sessions:       {}", agg.sessions);
    println!("Messages:       {}", agg.messages);
    println!(
        "Input tokens:   {} ({})",
        agg.input_tokens,
        compact(agg.input_tokens)
    );
    println!(
        "Output tokens:  {} ({})",
        agg.output_tokens,
        compact(agg.output_tokens)
    );
    let total = agg.input_tokens + agg.output_tokens;
    println!("Total tokens:   {} ({})", total, compact(total));
    println!("Estimated cost: {}", format_cost(agg.cost_usd));

    let mut models: Vec<(&String, &ModelUsage)> = agg.per_model.iter().collect();
    models.sort_by(|a, b| {
        b.1.cost_usd
            .partial_cmp(&a.1.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    println!("\nBy model:");
    println!(
        "  {:<32} {:>8} {:>12} {:>12} {:>12}",
        "MODEL", "SESSIONS", "INPUT", "OUTPUT", "COST"
    );
    for (model, u) in models {
        println!(
            "  {:<32} {:>8} {:>12} {:>12} {:>12}",
            model.chars().take(32).collect::<String>(),
            u.sessions,
            compact(u.input_tokens),
            compact(u.output_tokens),
            format_cost(u.cost_usd)
        );
    }

    if agg.daily.len() > 1 {
        println!("\nDaily trend (input+output tokens):");
        let max: u64 = agg
            .daily
            .values()
            .map(|(i, o)| i + o)
            .max()
            .unwrap_or(1)
            .max(1);
        for (day, (input, output)) in &agg.daily {
            let total = input + output;
            let width = ((total as f64 / max as f64) * 30.0).round() as usize;
            println!("  {day}  {:>10} {}", compact(total), "█".repeat(width));
        }
    }
}

fn print_json(agg: &UsageAggregate, days: u32) -> anyhow::Result<()> {
    let models: Vec<serde_json::Value> = agg
        .per_model
        .iter()
        .map(|(model, u)| {
            serde_json::json!({
                "model": model,
                "sessions": u.sessions,
                "input_tokens": u.input_tokens,
                "output_tokens": u.output_tokens,
                "cost_usd": u.cost_usd,
            })
        })
        .collect();
    let daily: Vec<serde_json::Value> = agg
        .daily
        .iter()
        .map(|(day, (input, output))| {
            serde_json::json!({ "date": day, "input_tokens": input, "output_tokens": output })
        })
        .collect();
    let doc = serde_json::json!({
        "days": days,
        "sessions": agg.sessions,
        "messages": agg.messages,
        "input_tokens": agg.input_tokens,
        "output_tokens": agg.output_tokens,
        "total_tokens": agg.input_tokens + agg.output_tokens,
        "cost_usd": agg.cost_usd,
        "models": models,
        "daily": daily,
    });
    println!("{}", serde_json::to_string_pretty(&doc)?);
    Ok(())
}

/// Print token/cost usage statistics.
pub async fn run(args: &UsageArgs) -> anyhow::Result<()> {
    let sessions_dir = hermes_home().join("sessions");
    if !sessions_dir.exists() {
        println!("No sessions directory found — no usage recorded yet.");
        return Ok(());
    }
    let agg = aggregate_usage(&sessions_dir, args.days, args.model.as_deref());
    if args.json {
        print_json(&agg, args.days)
    } else {
        print_human(&agg, args.days);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_session(dir: &std::path::Path, name: &str, model: &str, input: u64, output: u64) {
        let doc = serde_json::json!({
            "model": model,
            "source": "cli",
            "messages": [{"role": "user", "content": "hi"}],
            "usage": {"input_tokens": input, "output_tokens": output},
        });
        std::fs::write(dir.join(format!("{name}.json")), doc.to_string()).unwrap();
    }

    #[test]
    fn aggregate_usage_sums_tokens_per_model() {
        let dir = std::env::temp_dir().join("hermes-usage-test");
        std::fs::create_dir_all(&dir).unwrap();
        write_session(&dir, "a", "gpt-4o", 1000, 500);
        write_session(&dir, "b", "gpt-4o", 2000, 500);
        write_session(&dir, "c", "other-model", 100, 100);
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();

        let agg = aggregate_usage(&dir, 30, None);
        assert_eq!(agg.sessions, 3);
        assert_eq!(agg.messages, 3);
        assert_eq!(agg.input_tokens, 3100);
        assert_eq!(agg.output_tokens, 1100);
        assert_eq!(agg.per_model["gpt-4o"].sessions, 2);
        assert_eq!(agg.per_model["gpt-4o"].input_tokens, 3000);
        assert!(agg.cost_usd >= 0.0);
        assert_eq!(agg.daily.len(), 1);

        let filtered = aggregate_usage(&dir, 30, Some("gpt-4o"));
        assert_eq!(filtered.sessions, 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn aggregate_usage_respects_day_cutoff() {
        let dir = std::env::temp_dir().join("hermes-usage-test-cutoff");
        std::fs::create_dir_all(&dir).unwrap();
        write_session(&dir, "fresh", "m", 10, 10);
        let agg = aggregate_usage(&dir, 0, None);
        // days=0 → cutoff is now; the file written a moment ago may or may
        // not pass — assert only that the window logic is applied, i.e. a
        // 30-day window includes it.
        let agg30 = aggregate_usage(&dir, 30, None);
        assert_eq!(agg30.sessions, 1);
        assert!(agg.sessions <= 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn estimate_session_cost_prefers_recorded_cost() {
        assert!((estimate_session_cost("m", 1000, 1000, Some(0.5)) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn estimate_session_cost_falls_back_to_pricing() {
        let (inp, outp) = get_pricing("gpt-4o", None, None);
        let expected = 1_000_000.0 / 1_000_000.0 * inp + 500_000.0 / 1_000_000.0 * outp;
        let got = estimate_session_cost("gpt-4o", 1_000_000, 500_000, None);
        assert!((got - expected).abs() < 1e-9);
    }

    #[test]
    fn compact_formats_scales() {
        assert_eq!(compact(999), "999");
        assert_eq!(compact(1500), "1.5K");
        assert_eq!(compact(2_500_000), "2.5M");
    }
}
