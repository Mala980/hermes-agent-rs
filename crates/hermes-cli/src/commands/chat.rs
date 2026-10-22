//! `hermes chat` CLI subcommand handler.

use std::sync::Arc;

/// Handle `hermes chat [--query ...] [--preload-skill ...] [--yolo]`.
pub async fn handle_cli_chat(
    query: Option<String>,
    preload_skill: Option<String>,
    yolo: bool,
) -> Result<(), hermes_core::AgentError> {
    use hermes_config::load_config;
    use hermes_core::MessageRole;
    use hermes_environments::LocalBackend;
    use hermes_skills::{FileSkillStore, SkillManager};
    use hermes_tools::ToolRegistry;

    if let Some(skill) = &preload_skill {
        println!("[Preloading skill: {}]", skill);
    }
    if yolo {
        println!("[YOLO mode: tool confirmations disabled]");
    }

    let mut config =
        load_config(None).map_err(|e| hermes_core::AgentError::Config(e.to_string()))?;

    if yolo {
        config.approval.require_approval = false;
    }

    let current_model = config.model.clone().unwrap_or_else(|| "gpt-4o".to_string());

    let tool_registry = Arc::new(ToolRegistry::new());
    let terminal_backend: Arc<dyn hermes_core::TerminalBackend> = Arc::new(LocalBackend::default());
    let skill_store = Arc::new(FileSkillStore::new(FileSkillStore::default_dir()));
    let skill_provider: Arc<dyn hermes_core::SkillProvider> =
        Arc::new(SkillManager::new(skill_store));
    hermes_tools::register_builtin_tools(&tool_registry, terminal_backend, skill_provider);
    let live_count =
        crate::live_messaging::enable_live_messaging_tool(&config, &tool_registry).await;
    if live_count > 0 {
        println!(
            "[send_message live delivery enabled via {} configured adapter(s)]",
            live_count
        );
    }
    let agent_tool_registry = Arc::new(crate::app::bridge_tool_registry(&tool_registry));

    let agent_config = crate::app::build_agent_config(&config, &current_model, Some("cli"));
    let provider = crate::app::build_provider(&config, &current_model);

    let agent = hermes_agent::AgentLoop::new(agent_config, agent_tool_registry, provider);

    match query {
        Some(q) => {
            let messages = vec![hermes_core::Message::user(&q)];
            let result = agent.run(messages, None).await?;

            let reply = result
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
                .unwrap_or_else(|| "(no assistant reply)".to_string());
            println!("{}", reply);
        }
        None => {
            println!("Starting interactive chat session...");
            println!("(Use `hermes` for the default interactive TUI)");
        }
    }
    Ok(())
}
