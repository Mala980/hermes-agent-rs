//! `hermes slack manifest` — generate the Slack app manifest JSON.
//!
//! Ported from NousResearch/hermes-agent's `hermes_cli/slack_cli.py`.
//!
//! The slash command list comes from the fork's own `SLASH_COMMANDS`
//! registry (the original built it from the gateway command registry).

use std::path::PathBuf;

use hermes_cli::cli::Cli;
use hermes_cli::commands::SLASH_COMMANDS;
use hermes_config::hermes_home;
use hermes_core::AgentError;

/// Slack long-description length bounds (mirrors the original).
const LONG_DESC_MIN: usize = 175;
const LONG_DESC_MAX: usize = 4000;

/// Schema-required request URL (ignored in Socket Mode), mirrors the original.
const SLASH_REQUEST_URL: &str = "https://hermes-agent.local/slack/commands";

/// Build the `slash_commands` manifest section from the fork's slash registry.
fn slash_commands_section() -> Vec<serde_json::Value> {
    let mut slashes: Vec<serde_json::Value> = Vec::new();
    // `/hermes` is always first (mirrors the original).
    slashes.push(serde_json::json!({
        "command": "/hermes",
        "description": "Talk to your Hermes agent",
        "should_escape": false,
        "url": SLASH_REQUEST_URL,
        "usage_hint": "[subcommand] [args]",
    }));
    for (name, desc) in SLASH_COMMANDS {
        let slack_name = name.trim_start_matches('/').replace('_', "-");
        if slack_name == "hermes" || slack_name.is_empty() {
            continue;
        }
        slashes.push(serde_json::json!({
            "command": format!("/{}", slack_name),
            "description": desc,
            "should_escape": false,
            "url": SLASH_REQUEST_URL,
        }));
        if slashes.len() >= 50 {
            break; // Slack's 50-command cap (mirrors the original).
        }
    }
    slashes
}

/// Build the full Slack manifest (mirrors `_build_full_manifest`).
fn build_full_manifest(
    bot_name: &str,
    bot_description: &str,
    messaging_experience: &str,
    long_description: Option<&str>,
) -> serde_json::Value {
    let mut features = serde_json::json!({
        "app_home": {
            "home_tab_enabled": false,
            "messages_tab_enabled": true,
            "messages_tab_read_only_enabled": false
        },
        "bot_user": {
            "display_name": bot_name.chars().take(80).collect::<String>(),
            "always_online": true
        },
        "slash_commands": slash_commands_section()
    });

    let mut bot_scopes = vec![
        "app_mentions:read",
        "channels:history",
        "channels:read",
        "chat:write",
        "commands",
        "files:read",
        "files:write",
        "groups:history",
        "groups:read",
        "im:history",
        "im:read",
        "im:write",
        "mpim:history",
        "mpim:read",
        "reactions:read",
        "users:read",
    ];
    let mut bot_events = vec![
        "app_mention",
        "message.channels",
        "message.groups",
        "message.im",
        "message.mpim",
        "reaction_added",
        "reaction_removed",
    ];

    match messaging_experience {
        "assistant" => {
            features["assistant_view"] = serde_json::json!({
                "assistant_description": "Chat with Hermes in threads and DMs."
            });
            bot_scopes.push("assistant:write");
            bot_events.push("assistant_thread_context_changed");
            bot_events.push("assistant_thread_started");
        }
        "agent" => {
            features["agent_view"] = serde_json::json!({
                "agent_description": "Chat with Hermes in Slack Messages."
            });
            bot_scopes.push("assistant:write");
            bot_events.push("app_context_changed");
            bot_events.push("app_home_opened");
        }
        _ => {}
    }
    bot_scopes.sort_unstable();
    bot_events.sort_unstable();

    let mut display_information = serde_json::json!({
        "name": bot_name.chars().take(35).collect::<String>(),
        "description": bot_description.chars().take(140).collect::<String>(),
        "background_color": "#1a1a2e"
    });
    if let Some(long) = long_description {
        display_information["long_description"] = serde_json::Value::String(long.to_string());
    }

    serde_json::json!({
        "_metadata": {"major_version": 1, "minor_version": 1},
        "display_information": display_information,
        "features": features,
        "oauth_config": {"scopes": {"bot": bot_scopes}},
        "settings": {
            "event_subscriptions": {"bot_events": bot_events},
            "interactivity": {"is_enabled": true},
            "org_deploy_enabled": false,
            "socket_mode_enabled": true,
            "token_rotation_enabled": false
        }
    })
}

/// Handle `hermes slack [manifest]`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_slack(
    _cli: Cli,
    action: Option<String>,
    name: Option<String>,
    description: Option<String>,
    long_description: Option<String>,
    long_description_file: Option<String>,
    slashes_only: bool,
    agent_view: bool,
    no_assistant: bool,
    write: Option<String>,
) -> Result<(), AgentError> {
    match action.as_deref().unwrap_or("manifest") {
        "manifest" => cmd_manifest(
            name,
            description,
            long_description,
            long_description_file,
            slashes_only,
            agent_view,
            no_assistant,
            write,
        ),
        other => Err(AgentError::Config(format!(
            "Unknown slack action: '{}'. Available: manifest",
            other
        ))),
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_manifest(
    name: Option<String>,
    description: Option<String>,
    long_description: Option<String>,
    long_description_file: Option<String>,
    slashes_only: bool,
    agent_view: bool,
    no_assistant: bool,
    write: Option<String>,
) -> Result<(), AgentError> {
    let fail = |msg: String| AgentError::Config(format!("hermes slack manifest: {}", msg));

    let name = name.unwrap_or_else(|| "Hermes".to_string());
    let description = description.unwrap_or_else(|| "Your Hermes agent on Slack".to_string());
    let mut long_description = long_description;

    if slashes_only && (long_description.is_some() || long_description_file.is_some()) {
        return Err(fail(
            "long description options cannot be used with --slashes-only".into(),
        ));
    }
    if let Some(file) = long_description_file {
        let source = PathBuf::from(shellexpand_home(&file));
        long_description = Some(
            std::fs::read_to_string(&source)
                .map_err(|e| fail(format!("cannot read long description from {}: {}", file, e)))?,
        );
    }
    if let Some(long) = &long_description {
        let n = long.chars().count();
        if n < LONG_DESC_MIN {
            return Err(fail(format!(
                "long description must be at least {} characters (got {})",
                LONG_DESC_MIN, n
            )));
        }
        if n > LONG_DESC_MAX {
            return Err(fail(format!(
                "long description must be at most {} characters (got {})",
                LONG_DESC_MAX, n
            )));
        }
    }

    let messaging_experience = if agent_view {
        "agent"
    } else if no_assistant {
        "none"
    } else {
        "assistant"
    };

    let manifest = if slashes_only {
        serde_json::json!({ "slash_commands": slash_commands_section() })
    } else {
        build_full_manifest(
            &name,
            &description,
            messaging_experience,
            long_description.as_deref(),
        )
    };
    let payload = serde_json::to_string_pretty(&manifest)
        .map_err(|e| AgentError::Config(e.to_string()))?
        + "\n";

    let Some(write) = write else {
        print!("{}", payload);
        return Ok(());
    };
    // Bare `--write` (empty default_missing_value) → default location.
    let target: PathBuf = if write.is_empty() {
        hermes_home().join("slack-manifest.json")
    } else {
        PathBuf::from(shellexpand_home(&write))
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AgentError::Io(format!("Failed to create manifest dir: {}", e)))?;
    }
    std::fs::write(&target, &payload)
        .map_err(|e| AgentError::Io(format!("Failed to write manifest: {}", e)))?;
    eprintln!("Slack manifest written to: {}", target.display());
    eprintln!(
        "\nNext steps:\n\
         \x20  1. Open https://api.slack.com/apps and pick your Hermes app\n\
         \x20     (or create a new one: Create New App → From an app manifest).\n\
         \x20  2. Features → App Manifest → paste the contents of\n\
         \x20     {}\n\
         \x20  3. Save; Slack will prompt to reinstall the app if scopes or\n\
         \x20     slash commands changed.\n\
         \x20  4. Make sure Socket Mode is enabled and you have a bot token\n\
         \x20     (xoxb-...) and app token (xapp-...) configured via\n\
         \x20     `hermes setup`.",
        target.display()
    );
    Ok(())
}

/// Expand a leading `~` to the user's home directory.
fn shellexpand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    path.to_string()
}
