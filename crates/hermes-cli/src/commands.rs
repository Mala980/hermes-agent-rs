//! CLI subcommand handlers (dispatched from main.rs).
//!
//! Split by area into submodules; everything is re-exported here so
//! existing paths like `commands::handle_cli_memory` keep working.

pub mod auth;
pub mod chat;
pub mod claw;
pub mod mcp;
pub mod memory;
pub mod misc;
pub mod plugins;
pub mod sessions;
pub mod skills;
pub mod slash;
pub mod whatsapp;

pub use auth::{
    handle_cli_login, handle_cli_logout, handle_cli_pairing, handle_cli_runtime_provider,
};
pub use chat::handle_cli_chat;
pub use claw::handle_cli_claw;
pub use mcp::handle_cli_mcp;
pub use memory::{handle_cli_memory, handle_cli_memory_setup};
pub use misc::{
    handle_cli_acp, handle_cli_backup, handle_cli_clipboard, handle_cli_codex_models,
    handle_cli_import, handle_cli_region, handle_cli_subscription, handle_cli_version,
};
pub use plugins::handle_cli_plugins;
pub use sessions::{handle_cli_insights, handle_cli_sessions};
pub use skills::handle_cli_skills;
pub use slash::{autocomplete, handle_slash_command, help_for, CommandResult, SLASH_COMMANDS};
pub use whatsapp::handle_cli_whatsapp;
