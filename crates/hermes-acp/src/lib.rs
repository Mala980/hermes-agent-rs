#![allow(clippy::type_complexity, clippy::double_must_use)]
//! Agent Communication Protocol (ACP) adapter.
//!
//! Implements the ACP JSON-RPC interface so that Hermes can be controlled
//! by external agent orchestrators. Full protocol surface includes:
//!
//! - **Lifecycle**: initialize, authenticate
//! - **Sessions**: new, load, resume, fork, list, cancel
//! - **Prompts**: prompt with streaming events
//! - **Configuration**: set model, mode, config options
//! - **Events**: thinking, tool progress, message streaming
//! - **Permissions**: approval callback bridging

pub mod agent;
pub mod events;
pub mod handler;
pub mod permissions;
pub mod protocol;
pub mod server;
pub mod session;

pub use agent::{AgentRuntime, SessionRuntime};
pub use events::{AcpEvent, AcpEventKind, EventSink, ToolCallIdTracker};
pub use handler::{AcpHandler, DefaultAcpHandler, HandlerOutcome, HermesAcpHandler};
pub use permissions::{
    ApprovalCallback, PermissionKind, PermissionOption, PermissionOutcome, PermissionRequest,
    PermissionStore,
};
pub use protocol::{
    AcpError, AcpMethod, AcpRequest, AcpResponse, AgentCapabilities, AuthMethod, AvailableCommand,
    ClientCapabilities, ContentBlock, Implementation, InitializeResponse, McpServerConfig,
    PromptCapabilities, PromptResponse, SessionCapabilities, SessionUpdate, StopReason, Supported,
    Usage,
};
pub use server::AcpServer;
pub use session::{SessionInfo, SessionManager, SessionPhase, SessionState};

use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// CLI integration
// ---------------------------------------------------------------------------

/// Configuration for starting the ACP server from CLI context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpConfig {
    /// Model to use for the agent behind this ACP server.
    pub model: String,
    /// Optional personality / system prompt override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personality: Option<String>,
    /// List of tool names to enable.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Maximum conversation turns before the server auto-closes.
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
}

fn default_max_turns() -> usize {
    100
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            model: "default".to_string(),
            personality: None,
            tools: vec![],
            max_turns: default_max_turns(),
        }
    }
}

/// Start the ACP server from CLI context.
///
/// Builds the real agent runtime (LLM provider + tool registry from the
/// user's config, per-session `AgentLoop`s with per-session interrupt
/// controllers) and serves JSON-RPC on stdio. When no LLM backend is
/// configured the provider falls back to an honest erroring stub, so
/// `session/prompt` fails with a clear message instead of a fake reply.
pub async fn start_acp_server(config: AcpConfig) -> Result<(), Box<dyn std::error::Error>> {
    let session_manager = Arc::new(SessionManager::new());
    let event_sink = Arc::new(EventSink::default());
    let permission_store = Arc::new(PermissionStore::new());

    let agent_runtime = Arc::new(build_agent_runtime(&config));
    tracing::info!(
        "ACP agent runtime ready: provider_id={}, model={}",
        agent_runtime.provider_id(),
        config.model,
    );

    let handler: Arc<dyn AcpHandler> = Arc::new(
        HermesAcpHandler::new(
            session_manager.clone(),
            event_sink.clone(),
            permission_store.clone(),
        )
        .with_agent_runtime(agent_runtime),
    );

    tracing::info!(
        "Starting ACP server: model={}, tools={:?}, max_turns={}",
        config.model,
        config.tools,
        config.max_turns,
    );

    // NB: share the handler's components with the server so prompt streaming
    // events actually reach the client (AcpServer::new would isolate them).
    let server = AcpServer::with_components(handler, session_manager, event_sink, permission_store);
    server.run().await
}

/// Build the shared agent runtime: provider + tool registry + base config.
///
/// Mirrors the CLI `App` wiring (`hermes-cli/src/app.rs`): provider from the
/// model string (honest stub fallback when unconfigured), full builtin tool
/// registry, agent config tagged `platform = "acp"`.
fn build_agent_runtime(config: &AcpConfig) -> AgentRuntime {
    let gateway_config = hermes_config::load_config(None).unwrap_or_default();
    let model = if config.model.is_empty() || config.model == "default" {
        gateway_config
            .model
            .clone()
            .unwrap_or_else(|| "gpt-4o".to_string())
    } else {
        config.model.clone()
    };

    let provider = hermes_agent::agent_builder::build_provider(&gateway_config, &model);
    let (provider_id, _) = hermes_agent::agent_builder::parse_model_string(&model);

    let tools = Arc::new(hermes_tools::ToolRegistry::new());
    let terminal_backend: Arc<dyn hermes_core::TerminalBackend> =
        Arc::new(hermes_environments::LocalBackend::default());
    let skill_store = Arc::new(hermes_skills::FileSkillStore::new(
        hermes_skills::FileSkillStore::default_dir(),
    ));
    let skill_provider: Arc<dyn hermes_core::SkillProvider> =
        Arc::new(hermes_skills::SkillManager::new(skill_store));
    hermes_tools::register_builtin_tools(&tools, terminal_backend, skill_provider);
    let agent_tools = Arc::new(hermes_agent::agent_builder::bridge_tool_registry(&tools));

    let mut agent_config =
        hermes_agent::agent_builder::build_agent_config(&gateway_config, &model, Some("acp"));
    agent_config.max_turns = config.max_turns as u32;
    if let Some(personality) = &config.personality {
        agent_config.personality = Some(personality.clone());
    }

    AgentRuntime::new(provider, agent_tools, agent_config, provider_id)
}

/// Start the ACP server with a custom handler implementation.
pub async fn start_acp_server_with_handler(
    config: AcpConfig,
    handler: Arc<dyn AcpHandler>,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!(
        "Starting ACP server: model={}, tools={:?}, max_turns={}",
        config.model,
        config.tools,
        config.max_turns,
    );

    let server = AcpServer::new(handler);
    server.run().await
}
