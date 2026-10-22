//! `hermes tui` — launch the interactive TUI directly.

use hermes_cli::cli::Cli;
use hermes_core::AgentError;

pub(crate) async fn cmd_tui(cli: Cli) -> Result<(), AgentError> {
    super::interactive::run_interactive(cli).await
}
