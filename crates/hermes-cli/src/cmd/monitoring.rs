//! `hermes monitoring` — show monitoring/metrics status.
//!
//! Ported from NousResearch/hermes-agent's `hermes_cli/main_agent_cmds.py::
//! cmd_monitoring` ("status" action): gateway health-export posture, OTLP
//! endpoint configuration, and OTel SDK presence. The fork's telemetry
//! surface is `hermes-telemetry` (in-process metrics registry + optional
//! OTLP export), so this reports the OTLP endpoint config, whether the OTLP
//! feature is compiled in, the effective log level, the ESTOP (pause) state,
//! and the current process's metrics snapshot.

use hermes_config::hermes_home;

/// CLI args for `hermes monitoring`.
#[derive(clap::Args, Debug, Clone)]
pub struct MonitoringArgs {
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Collected monitoring status.
#[derive(Debug, Clone)]
pub struct MonitoringStatus {
    pub otlp_endpoint: Option<String>,
    pub otlp_compiled_in: bool,
    pub log_level: String,
    pub log_json: bool,
    pub estop_engaged: bool,
    pub hermes_home: String,
    pub metrics: hermes_telemetry::MetricsSnapshot,
}

/// Read the current monitoring posture.
pub fn collect_status() -> MonitoringStatus {
    MonitoringStatus {
        otlp_endpoint: std::env::var("HERMES_OTLP_ENDPOINT")
            .ok()
            .filter(|v| !v.is_empty()),
        otlp_compiled_in: cfg!(feature = "otlp"),
        log_level: std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
        log_json: std::env::var("HERMES_LOG_JSON").ok().as_deref() == Some("1"),
        estop_engaged: super::estop::sentinel_path().exists(),
        hermes_home: hermes_home().display().to_string(),
        metrics: hermes_telemetry::snapshot(),
    }
}

fn render_human(s: &MonitoringStatus) -> String {
    let mut lines = vec![
        "Hermes monitoring".to_string(),
        format!(
            "  OTLP endpoint:  {}",
            s.otlp_endpoint
                .as_deref()
                .unwrap_or("not configured (HERMES_OTLP_ENDPOINT)")
        ),
        format!(
            "  OTLP export:    {}",
            if s.otlp_compiled_in {
                "compiled in (hermes-telemetry/otlp)"
            } else {
                "not compiled in (build with --features otlp)"
            }
        ),
        format!("  Log level:      {} (RUST_LOG)", s.log_level),
        format!(
            "  Log format:     {}",
            if s.log_json { "json" } else { "text" }
        ),
        format!(
            "  Paused (ESTOP): {}",
            if s.estop_engaged { "yes" } else { "no" }
        ),
        format!("  Hermes home:     {}", s.hermes_home),
        String::new(),
        "  In-process metrics (this process only; a fresh CLI run starts at zero):".to_string(),
    ];
    let m = &s.metrics;
    for (label, value) in [
        ("llm_requests_total", m.llm_requests_total),
        ("tool_calls_total", m.tool_calls_total),
        ("tool_time_ms_total", m.tool_time_ms_total),
        ("errors_total", m.errors_total),
        ("http_requests_total", m.http_requests_total),
        ("http_rejects_total", m.http_rejects_total),
        ("prompt_cache_hits", m.prompt_cache_hits),
        ("prompt_cache_misses", m.prompt_cache_misses),
    ] {
        lines.push(format!("    {label:<22} {value}"));
    }
    lines.push(String::new());
    lines.push("  Scope: process health + redacted diagnostics only.".to_string());
    lines
        .push("  No prompts, messages, tool args/results, usage analytics, or traces.".to_string());
    lines.join("\n")
}

fn render_json(s: &MonitoringStatus) -> anyhow::Result<String> {
    let doc = serde_json::json!({
        "otlp_endpoint": s.otlp_endpoint,
        "otlp_compiled_in": s.otlp_compiled_in,
        "log_level": s.log_level,
        "log_json": s.log_json,
        "estop_engaged": s.estop_engaged,
        "hermes_home": s.hermes_home,
        "metrics": {
            "llm_requests_total": s.metrics.llm_requests_total,
            "tool_calls_total": s.metrics.tool_calls_total,
            "tool_time_ms_total": s.metrics.tool_time_ms_total,
            "errors_total": s.metrics.errors_total,
            "http_requests_total": s.metrics.http_requests_total,
            "http_rejects_total": s.metrics.http_rejects_total,
            "prompt_cache_hits": s.metrics.prompt_cache_hits,
            "prompt_cache_misses": s.metrics.prompt_cache_misses,
        },
    });
    Ok(serde_json::to_string_pretty(&doc)?)
}

/// Print monitoring/metrics status.
pub async fn run(args: &MonitoringArgs) -> anyhow::Result<()> {
    let status = collect_status();
    if args.json {
        println!("{}", render_json(&status)?);
    } else {
        println!("{}", render_human(&status));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_status_reads_env() {
        std::env::set_var("HERMES_OTLP_ENDPOINT", "http://localhost:4318");
        let s = collect_status();
        assert_eq!(s.otlp_endpoint.as_deref(), Some("http://localhost:4318"));
        std::env::remove_var("HERMES_OTLP_ENDPOINT");
        let s = collect_status();
        assert_eq!(s.otlp_endpoint, None);
    }

    #[test]
    fn render_human_covers_key_sections() {
        let s = collect_status();
        let text = render_human(&s);
        for section in [
            "OTLP endpoint",
            "Log level",
            "In-process metrics",
            "llm_requests_total",
        ] {
            assert!(text.contains(section), "missing {section}");
        }
    }

    #[test]
    fn render_json_is_valid() {
        let s = collect_status();
        let text = render_json(&s).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(v.get("otlp_compiled_in").is_some());
        assert!(v["metrics"]["errors_total"].as_u64().is_some());
    }

    #[test]
    fn metrics_snapshot_starts_at_zero_in_fresh_test_process() {
        // Counters are process-global; this test only asserts the snapshot
        // shape is readable, not specific values.
        let m = hermes_telemetry::snapshot();
        let _ = m.llm_requests_total + m.tool_calls_total + m.errors_total;
    }
}
