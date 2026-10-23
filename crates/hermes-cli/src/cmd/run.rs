//! `hermes run` subcommand handler.
//!
//! One-shot non-interactive run — equivalent to `hermes chat --query`.

use crate::cli::Cli;
use hermes_core::AgentError;

/// Run a single prompt non-interactively and print the assistant's reply.
///
/// Delegates to the same one-shot path that `hermes chat --query` uses, so
/// `run` behaves exactly like `chat --query <prompt>`.
pub async fn cmd_run(cli: Cli, prompt: &str) -> Result<(), AgentError> {
    // `handle_cli_chat` ignores global CLI overrides today (same as the
    // `Chat` dispatch in main.rs); keep the parameter for a future pass-through.
    let _ = &cli;
    crate::commands::handle_cli_chat(Some(prompt.to_owned()), None, false).await
}
