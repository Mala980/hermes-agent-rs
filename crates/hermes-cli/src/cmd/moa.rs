//! `hermes moa` — Mixture-of-Agents model presets.
//!
//! Ported from NousResearch/hermes-agent's `hermes_cli/moa_cmd.py`.
//!
//! NOT AVAILABLE in this build. This fork's `GatewayConfig` has no `moa`
//! section and the agent loop has no MoA aggregation stage, so there is
//! nothing for presets to configure and no runtime would honor them.
//!
//! The command is kept as a stub (same `cmd_moa` signature) so the CLI
//! surface stays stable; it explains the situation instead of pretending
//! to manage presets that nothing would read.

use crate::cli::Cli;
use hermes_core::AgentError;

/// Print the unavailability notice shared by every `moa` invocation.
fn explain() {
    println!();
    println!("  Mixture-of-Agents (MoA) is not available in this build.");
    println!();
    println!("  This fork's config schema has no `moa` section and the agent");
    println!("  loop has no MoA aggregation stage, so no presets can be defined");
    println!("  or used. To run multiple models, switch models directly with");
    println!("  `hermes model <provider>:<model>` or per-session overrides.");
    println!();
}

/// Handle `hermes moa [list|configure|delete]`.
pub async fn cmd_moa(
    _cli: Cli,
    action: Option<String>,
    _name: Option<String>,
    _reference: Vec<String>,
    _aggregator: Option<String>,
) -> Result<(), AgentError> {
    match action.as_deref().unwrap_or("list") {
        "list" | "ls" => {
            explain();
            Ok(())
        }
        other => Err(AgentError::Config(format!(
            "MoA is not available in this build, so '{}' cannot be performed. \
             Run `hermes moa` for details.",
            other
        ))),
    }
}
