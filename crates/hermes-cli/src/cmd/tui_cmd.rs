//! `hermes tui` — launch the interactive TUI directly.

use crate::cli::Cli;
use crate::App;
use hermes_core::AgentError;

/// Build the full [`App`] and hand it to the TUI runner.
///
/// (Previously this delegated to `cmd::interactive::run_interactive`; the
/// delegation is inlined here so `hermes tui` does not depend on that module.)
pub async fn cmd_tui(cli: Cli) -> Result<(), AgentError> {
    let app = App::new(cli).await?;
    crate::tui::run(app).await
}
