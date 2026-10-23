//! Default interactive REPL and `hermes model` handlers.

use crate::cli::Cli;
use crate::App;
use hermes_config::load_config;
use hermes_core::AgentError;

/// Run the interactive REPL (default command).
pub async fn run_interactive(cli: Cli) -> Result<(), AgentError> {
    let app = App::new(cli).await?;
    crate::tui::run(app).await
}

/// Handle `hermes model [provider:model]`.
pub async fn run_model(cli: Cli, provider_model: Option<String>) -> Result<(), AgentError> {
    let config =
        load_config(cli.config_dir.as_deref()).map_err(|e| AgentError::Config(e.to_string()))?;

    match provider_model {
        Some(pm) => {
            println!("Model switched to: {}", pm);
            println!("(To persist, run: hermes config set model {})", pm);
        }
        None => {
            let current = config.model.as_deref().unwrap_or("gpt-4o");
            println!("Current model: {}", current);

            // List known providers
            println!("\nAvailable providers:");
            println!("  openai       — OpenAI (gpt-4o, gpt-4o-mini, ...)");
            println!("  anthropic    — Anthropic (claude-3-5-sonnet, claude-3-opus, ...)");
            println!("  openrouter   — OpenRouter (multi-provider routing)");
            println!("\nUsage: hermes model <provider>:<model>");
        }
    }
    Ok(())
}
