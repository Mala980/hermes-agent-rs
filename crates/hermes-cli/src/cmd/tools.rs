//! `hermes tools` handler.

use crate::cli::Cli;
use hermes_config::{hermes_home, load_user_config_file, save_config_yaml};
use hermes_core::AgentError;
use std::path::PathBuf;

/// Handle `hermes tools [action] [name]`.
///
/// `enable`/`disable` persist the tool name in `tools_config.enabled` /
/// `tools_config.disabled` inside config.yaml (mirrors the agent runtime's
/// tool gating). `list` marks tools that are currently disabled.
pub async fn run_tools(
    cli: Cli,
    action: Option<String>,
    name: Option<String>,
) -> Result<(), AgentError> {
    let base: PathBuf = cli
        .config_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(hermes_home);
    let cfg_path = base.join("config.yaml");

    match action.as_deref() {
        Some("enable") | Some("disable") => {
            let tool_name = name.ok_or_else(|| {
                AgentError::Config(format!(
                    "Missing tool name. Usage: hermes tools {} <name>",
                    action.as_deref().unwrap_or("enable")
                ))
            })?;
            let mut disk =
                load_user_config_file(&cfg_path).map_err(|e| AgentError::Config(e.to_string()))?;
            if action.as_deref() == Some("disable") {
                if !disk.tools_config.disabled.iter().any(|t| t == &tool_name) {
                    disk.tools_config.disabled.push(tool_name.clone());
                }
                disk.tools_config.enabled.retain(|t| t != &tool_name);
                save_config_yaml(&cfg_path, &disk)
                    .map_err(|e| AgentError::Config(e.to_string()))?;
                println!(
                    "Tool '{}' disabled (persisted to {}).",
                    tool_name,
                    cfg_path.display()
                );
            } else {
                if !disk.tools_config.enabled.iter().any(|t| t == &tool_name) {
                    disk.tools_config.enabled.push(tool_name.clone());
                }
                disk.tools_config.disabled.retain(|t| t != &tool_name);
                save_config_yaml(&cfg_path, &disk)
                    .map_err(|e| AgentError::Config(e.to_string()))?;
                println!(
                    "Tool '{}' enabled (persisted to {}).",
                    tool_name,
                    cfg_path.display()
                );
            }
            return Ok(());
        }
        _ => {}
    }

    let disabled: Vec<String> = load_user_config_file(&cfg_path)
        .map(|c| c.tools_config.disabled)
        .unwrap_or_default();

    let registry = hermes_tools::ToolRegistry::new();
    let tools = registry.list_tools();

    match action.as_deref() {
        None | Some("list") => {
            if tools.is_empty() {
                println!("No tools registered (tools are loaded at runtime).");
                println!("\nBuilt-in tool categories:");
                let categories = [
                    "web",
                    "terminal",
                    "file",
                    "browser",
                    "vision",
                    "image_gen",
                    "skills",
                    "memory",
                    "session_search",
                    "todo",
                    "clarify",
                    "code_execution",
                    "delegation",
                    "cronjob",
                    "messaging",
                    "homeassistant",
                ];
                for cat in &categories {
                    let mark = if disabled.iter().any(|d| d == cat) {
                        " (disabled)"
                    } else {
                        ""
                    };
                    println!("  • {}{}", cat, mark);
                }
            } else {
                println!("Registered tools ({}):", tools.len());
                for tool in &tools {
                    let mark = if disabled.iter().any(|d| d == &tool.name) {
                        " [disabled]"
                    } else {
                        ""
                    };
                    println!("  • {} — {}{}", tool.name, tool.description, mark);
                }
            }
            if !disabled.is_empty() {
                println!(
                    "\nDisabled tools ({}): {}",
                    disabled.len(),
                    disabled.join(", ")
                );
            }
        }
        Some(other) => {
            println!(
                "Unknown tools action: {}. Use 'list', 'enable <name>', or 'disable <name>'.",
                other
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cli_with_config_dir(dir: &std::path::Path) -> Cli {
        Cli {
            command: None,
            verbose: false,
            config_dir: Some(dir.to_string_lossy().into_owned()),
            model: None,
            personality: None,
        }
    }

    fn read_tools_config(dir: &std::path::Path) -> hermes_config::GatewayConfig {
        let path = dir.join("config.yaml");
        hermes_config::load_user_config_file(&path).expect("config should load")
    }

    #[tokio::test]
    async fn tools_disable_persists_to_config() {
        let tmp = tempfile::tempdir().unwrap();
        let cli = cli_with_config_dir(tmp.path());

        run_tools(
            cli,
            Some("disable".to_string()),
            Some("web_search".to_string()),
        )
        .await
        .expect("disable should succeed");
        let cfg = read_tools_config(tmp.path());
        assert!(cfg
            .tools_config
            .disabled
            .contains(&"web_search".to_string()));
        assert!(!cfg.tools_config.enabled.contains(&"web_search".to_string()));

        // Disabling twice is idempotent (no duplicates).
        let cli = cli_with_config_dir(tmp.path());
        run_tools(
            cli,
            Some("disable".to_string()),
            Some("web_search".to_string()),
        )
        .await
        .expect("disable should succeed");
        let cfg = read_tools_config(tmp.path());
        assert_eq!(
            cfg.tools_config
                .disabled
                .iter()
                .filter(|t| *t == "web_search")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn tools_enable_moves_name_to_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let cli = cli_with_config_dir(tmp.path());
        run_tools(
            cli,
            Some("disable".to_string()),
            Some("web_search".to_string()),
        )
        .await
        .expect("disable should succeed");

        let cli = cli_with_config_dir(tmp.path());
        run_tools(
            cli,
            Some("enable".to_string()),
            Some("web_search".to_string()),
        )
        .await
        .expect("enable should succeed");
        let cfg = read_tools_config(tmp.path());
        assert!(cfg.tools_config.enabled.contains(&"web_search".to_string()));
        assert!(!cfg
            .tools_config
            .disabled
            .contains(&"web_search".to_string()));
    }

    #[tokio::test]
    async fn tools_enable_without_name_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let cli = cli_with_config_dir(tmp.path());
        let err = run_tools(cli, Some("enable".to_string()), None)
            .await
            .expect_err("enable without a name should fail");
        assert!(err.to_string().contains("Missing tool name"));
    }
}
