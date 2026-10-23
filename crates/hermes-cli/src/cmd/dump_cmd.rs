//! `hermes dump` — compact, copy-pasteable setup summary for support/debugging.
//!
//! Ported from NousResearch/hermes-agent's `hermes_cli/dump.py::run_dump`
//! (the setup summary, not the session dump: that lives on as
//! `hermes sessions dump` in `cmd::misc::run_dump`).
//!
//! The report covers version/OS, model + provider, terminal backend, API key
//! presence (redacted by default), toolsets, MCP servers, gateway status,
//! configured platforms, cron jobs, skills and non-default config values.
//! [`collect_dump_summary`] is also reused by `hermes debug share`.

use hermes_config::loader::load_dotenv;
use hermes_config::{hermes_home, load_config};
use std::path::PathBuf;

/// Env-var → human label, in display order (mirrors `_API_KEYS` in dump.py).
const API_KEYS: &[(&str, &str)] = &[
    ("OPENROUTER_API_KEY", "openrouter"),
    ("OPENAI_API_KEY", "openai"),
    ("ANTHROPIC_API_KEY", "anthropic"),
    ("NOUS_API_KEY", "nous"),
    ("GOOGLE_API_KEY", "google/gemini"),
    ("GEMINI_API_KEY", "gemini"),
    ("GLM_API_KEY", "glm/zai"),
    ("ZAI_API_KEY", "zai"),
    ("KIMI_API_KEY", "kimi"),
    ("MINIMAX_API_KEY", "minimax"),
    ("DEEPSEEK_API_KEY", "deepseek"),
    ("DASHSCOPE_API_KEY", "dashscope"),
    ("HF_TOKEN", "huggingface"),
    ("NVIDIA_API_KEY", "nvidia"),
    ("AI_GATEWAY_API_KEY", "ai_gateway"),
    ("GITHUB_TOKEN", "github"),
];

/// Env-var → platform label for messaging platforms (mirrors `_PLATFORM_ENV_VARS`).
const PLATFORM_ENV_VARS: &[(&str, &str)] = &[
    ("TELEGRAM_BOT_TOKEN", "telegram"),
    ("DISCORD_BOT_TOKEN", "discord"),
    ("SLACK_BOT_TOKEN", "slack"),
    ("WHATSAPP_ENABLED", "whatsapp"),
    ("SIGNAL_HTTP_URL", "signal"),
    ("EMAIL_ADDRESS", "email"),
    ("TWILIO_ACCOUNT_SID", "sms"),
    ("MATRIX_HOMESERVER_URL", "matrix"),
    ("MATTERMOST_URL", "mattermost"),
    ("DINGTALK_CLIENT_ID", "dingtalk"),
    ("FEISHU_APP_ID", "feishu"),
    ("WECOM_BOT_ID", "wecom"),
    ("QQ_APP_ID", "qqbot"),
];

/// Redact a secret, showing only the first 4 and last 4 characters.
pub fn mask_secret(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 8 {
        return "••••••••".to_string();
    }
    format!(
        "{}…{}",
        chars[..4].iter().collect::<String>(),
        chars[chars.len() - 4..].iter().collect::<String>()
    )
}

/// Names assigned a non-empty value in `$HERMES_HOME/.env` — what the managed
/// backends (systemd / launchd / desktop `serve`) load, as opposed to the
/// shell exports `std::env` reflects here. A key exported in the shell but
/// absent from `.env` is flagged so support doesn't chase a phantom
/// "configured" (the trap behind #48504-style "no web_search" reports).
fn dotenv_key_names(home: &std::path::Path) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    let Ok(text) = std::fs::read_to_string(home.join(".env")) else {
        return names;
    };
    for raw in text.lines() {
        let mut line = raw.trim();
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        if line.to_lowercase().starts_with("export ") {
            line = line["export ".len()..].trim_start();
        }
        let (name, value) = match line.split_once('=') {
            Some(pair) => pair,
            None => continue,
        };
        let name = name.trim();
        let value = value.trim().trim_matches(|c| c == '\'' || c == '"');
        if !name.is_empty() && !value.is_empty() {
            names.insert(name.to_string());
        }
    }
    names
}

fn api_key_lines(show_keys: bool, dotenv_keys: &std::collections::HashSet<String>) -> Vec<String> {
    let mut lines = Vec::new();
    for (env_var, label) in API_KEYS {
        let val = std::env::var(env_var).unwrap_or_default();
        let mut display = if show_keys && !val.is_empty() {
            mask_secret(&val)
        } else if val.is_empty() {
            "not set".to_string()
        } else {
            "set".to_string()
        };
        if !val.is_empty() && !dotenv_keys.contains(*env_var) {
            display.push_str(" (shell only — not in .env; managed/desktop backend may not see it)");
        }
        lines.push(format!("  {label:<20} {display}"));
    }
    lines
}

fn gateway_status() -> String {
    // Match the spirit of the original: a live gateway means a supervising
    // service or the ESTOP sentinel. The fork can only check the sentinel and
    // a best-effort process scan here.
    if super::estop::is_engaged() {
        return "paused (ESTOP sentinel engaged)".to_string();
    }
    "unknown (no live process scan; gateway run state is managed by `hermes gateway`)".to_string()
}

fn count_skills(home: &std::path::Path) -> usize {
    fn visit(dir: &std::path::Path, count: &mut usize) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, count);
            } else if path.file_name().map(|n| n == "SKILL.md").unwrap_or(false) {
                *count += 1;
            }
        }
    }
    let mut count = 0;
    visit(&home.join("skills"), &mut count);
    count
}

fn cron_summary(home: &std::path::Path) -> String {
    let jobs_file = home.join("cron").join("jobs.json");
    if !jobs_file.exists() {
        return "0 (no jobs.json)".to_string();
    }
    match std::fs::read_to_string(&jobs_file)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    {
        Some(v) => {
            let jobs = v.get("jobs").and_then(|j| j.as_array());
            match jobs {
                Some(list) => {
                    let active = list
                        .iter()
                        .filter(|j| j.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true))
                        .count();
                    format!("{active} active / {} total", list.len())
                }
                None => "(unrecognized jobs.json shape)".to_string(),
            }
        }
        None => "(error reading)".to_string(),
    }
}

fn config_overrides_lines() -> Vec<String> {
    // Non-default values worth reporting, mirroring `_INTERESTING_PATHS`.
    let mut lines = Vec::new();
    let Ok(cfg) = load_config(None) else {
        return lines;
    };
    let defaults = hermes_config::GatewayConfig::default();
    let mut check = |path: &str, user: &str, default: &str| {
        if user != default && !user.is_empty() {
            lines.push(format!("  {path}: {user}"));
        }
    };
    check(
        "model",
        cfg.model.as_deref().unwrap_or(""),
        defaults.model.as_deref().unwrap_or(""),
    );
    check(
        "personality",
        cfg.personality.as_deref().unwrap_or(""),
        defaults.personality.as_deref().unwrap_or(""),
    );
    check(
        "max_turns",
        &cfg.max_turns.to_string(),
        &defaults.max_turns.to_string(),
    );
    check(
        "terminal.backend",
        &format!("{:?}", cfg.terminal.backend),
        &format!("{:?}", defaults.terminal.backend),
    );
    check(
        "system_prompt",
        if cfg.system_prompt.is_some() {
            "(custom set)"
        } else {
            ""
        },
        "",
    );
    if !cfg.tools.is_empty() && cfg.tools != defaults.tools {
        lines.push(format!("  tools: {}", cfg.tools.join(", ")));
    }
    lines
}

/// Build the full dump report text. `show_keys` unhides (masked) key values.
pub fn collect_dump_summary(show_keys: bool) -> String {
    load_dotenv();
    let home: PathBuf = hermes_home();
    let dotenv_keys = dotenv_key_names(&home);
    let config = load_config(None).ok();
    let (model, provider) = match config.as_ref().and_then(|c| c.model.as_deref()) {
        Some(m) if !m.is_empty() => (m.to_string(), "(auto)".to_string()),
        _ => ("(not set)".to_string(), "(auto)".to_string()),
    };
    let terminal_backend = config
        .as_ref()
        .map(|c| format!("{:?}", c.terminal.backend).to_lowercase())
        .unwrap_or_else(|| "local".to_string());
    let toolsets: Vec<String> = config.as_ref().map(|c| c.tools.clone()).unwrap_or_default();
    let mcp_servers = config.as_ref().map(|c| c.mcp_servers.len()).unwrap_or(0);
    let platforms: Vec<&str> = PLATFORM_ENV_VARS
        .iter()
        .filter(|(env, _)| std::env::var(env).map(|v| !v.is_empty()).unwrap_or(false))
        .map(|(_, label)| *label)
        .collect();
    let configured_platforms: Vec<String> = config
        .as_ref()
        .map(|c| c.platforms.keys().cloned().collect())
        .unwrap_or_default();

    let mut lines = vec![
        "--- hermes dump ---".to_string(),
        format!("version:          {}", env!("CARGO_PKG_VERSION")),
        format!(
            "os:               {} {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::env::consts::FAMILY
        ),
        "profile:          (default)".to_string(),
        format!("hermes_home:      {}", home.display()),
        format!("model:            {model}"),
        format!("provider:         {provider}"),
        format!("terminal:         {terminal_backend}"),
        String::new(),
        "api_keys:".to_string(),
    ];
    lines.extend(api_key_lines(show_keys, &dotenv_keys));
    lines.push(String::new());
    lines.push("features:".to_string());
    lines.push(format!(
        "  toolsets:           {}",
        if toolsets.is_empty() {
            "(default)".to_string()
        } else {
            toolsets.join(", ")
        }
    ));
    lines.push(format!("  mcp_servers:        {mcp_servers}"));
    lines.push("  memory_provider:    built-in".to_string());
    lines.push(format!("  gateway:            {}", gateway_status()));
    let mut all_platforms = platforms;
    for p in &configured_platforms {
        if !all_platforms.contains(&p.as_str()) {
            all_platforms.push(p);
        }
    }
    lines.push(format!(
        "  platforms:          {}",
        if all_platforms.is_empty() {
            "none".to_string()
        } else {
            all_platforms.join(", ")
        }
    ));
    lines.push(format!("  cron_jobs:          {}", cron_summary(&home)));
    lines.push(format!("  skills:             {}", count_skills(&home)));
    let overrides = config_overrides_lines();
    if !overrides.is_empty() {
        lines.push(String::new());
        lines.push("config_overrides:".to_string());
        lines.extend(overrides);
    }
    lines.push("--- end dump ---".to_string());
    lines.join("\n")
}

/// CLI args for `hermes dump`.
#[derive(clap::Args, Debug, Clone)]
pub struct DumpArgs {
    /// Show (masked) API key values instead of just presence.
    #[arg(long)]
    pub show_keys: bool,
}

/// Print the setup summary for support/debugging.
pub async fn run(args: &DumpArgs) -> anyhow::Result<()> {
    println!("{}", collect_dump_summary(args.show_keys));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_secret_keeps_first_and_last_four() {
        assert_eq!(mask_secret("sk-ant-abcdefghij1234"), "sk-a…1234");
    }

    #[test]
    fn mask_secret_short_values_fully_masked() {
        assert_eq!(mask_secret("short"), "••••••••");
        assert_eq!(mask_secret(""), "");
    }

    #[test]
    fn dump_summary_has_required_sections() {
        let report = collect_dump_summary(false);
        for section in [
            "--- hermes dump ---",
            "version:",
            "os:",
            "hermes_home:",
            "model:",
            "api_keys:",
            "features:",
            "--- end dump ---",
        ] {
            assert!(report.contains(section), "missing section {section}");
        }
    }

    #[test]
    fn dump_summary_never_leaks_raw_keys() {
        std::env::set_var("OPENAI_API_KEY", "sk-test-secret-value-12345");
        let report = collect_dump_summary(true);
        assert!(!report.contains("sk-test-secret-value-12345"));
        assert!(report.contains("sk-t…2345"));
        std::env::remove_var("OPENAI_API_KEY");
    }

    #[test]
    fn dotenv_key_names_parses_assignments() {
        let dir = std::env::temp_dir().join("hermes-dump-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".env"),
            "# comment\nOPENAI_API_KEY=sk-abc\nEMPTY=\nBARE\n export QUOTED=\"x y\" \n",
        )
        .unwrap();
        let names = dotenv_key_names(&dir);
        assert!(names.contains("OPENAI_API_KEY"));
        assert!(names.contains("QUOTED"));
        assert!(!names.contains("EMPTY"));
        assert!(!names.contains("BARE"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
