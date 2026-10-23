//! Small helpers shared by several `hermes` subcommand handlers.

use crate::cli::Cli;
use hermes_config::state_dir;
use hermes_core::AgentError;
use std::path::Path;
use std::path::PathBuf;

/// Config/state root shared by CLI, `hermes gateway`, cron, and `webhooks.json`.
pub(crate) fn hermes_state_root(cli: &Cli) -> PathBuf {
    state_dir(cli.config_dir.as_deref().map(Path::new))
}

pub(crate) fn is_truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

pub(crate) async fn prompt_line(prompt: impl Into<String>) -> Result<String, AgentError> {
    let prompt = prompt.into();
    let line = tokio::task::spawn_blocking(move || {
        use std::io::{self, Write};
        print!("{}", prompt);
        let _ = io::stdout().flush();
        let mut buf = String::new();
        io::stdin().read_line(&mut buf).map(|_| buf)
    })
    .await
    .map_err(|e| AgentError::Io(format!("stdin task: {}", e)))?
    .map_err(|e| AgentError::Io(format!("stdin: {}", e)))?;
    Ok(line.trim().to_string())
}
