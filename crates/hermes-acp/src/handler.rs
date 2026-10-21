//! Full ACP request handler — implements all ACP protocol methods.
//!
//! Every method does real work against [`SessionManager`] and, when an
//! [`AgentRuntime`] is attached, against a real per-session [`AgentLoop`]
//! (see `crate::agent`):
//!
//! - `session/prompt` runs a genuine agent turn via `AgentLoop::run_stream`;
//! - `session/cancel` signals the session's [`InterruptController`], aborting
//!   the in-flight turn (reported as `"cancelled"`, not an error);
//! - `session/set_model` rebuilds the session's agent loop with the new model;
//! - `session/set_config_option` is persisted on the session state;
//! - `session/set_mode` honestly returns `-32002`: this build has no
//!   per-session modes.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::agent::AgentRuntime;
use crate::events::{AcpEvent, EventSink};
use crate::permissions::PermissionStore;
use crate::protocol::*;
use crate::session::{SessionManager, SessionPhase, SessionState};

/// Auth method id for the interactive terminal setup flow.
const TERMINAL_SETUP_AUTH_METHOD_ID: &str = "hermes-setup";
/// Config option id that maps to per-session edit-approval modes, which this
/// build does not have (consistent with `session/set_mode` → -32002).
const EDIT_APPROVAL_POLICY_CONFIG_ID: &str = "edit_approval_policy";

/// Trait for handling ACP requests.
#[async_trait::async_trait]
pub trait AcpHandler: Send + Sync {
    async fn handle_request(&self, request: AcpRequest) -> AcpResponse;

    /// Full dispatch, including `session/update` notifications that must be
    /// written to the client *before* the response (history replay for
    /// `session/load` / `session/resume` — the ACP spec requires this order).
    ///
    /// The default implementation drops pre-response notifications for
    /// backward compatibility.
    async fn handle_request_full(&self, request: AcpRequest) -> HandlerOutcome {
        let response = self.handle_request(request).await;
        HandlerOutcome {
            response,
            pre_notifications: Vec::new(),
        }
    }
}

/// Result of dispatching one request: the JSON-RPC response plus any
/// notifications that must precede it on the wire.
pub struct HandlerOutcome {
    pub response: AcpResponse,
    pub pre_notifications: Vec<Value>,
}

impl HandlerOutcome {
    fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            response: AcpResponse::success(id, result),
            pre_notifications: Vec::new(),
        }
    }

    fn err(id: Option<Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            response: AcpResponse::error(id, code, message),
            pre_notifications: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Slash commands
// ---------------------------------------------------------------------------

#[allow(dead_code)]
struct SlashCommand {
    name: &'static str,
    description: &'static str,
    input_hint: Option<&'static str>,
}

const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "help",
        description: "Show available commands",
        input_hint: None,
    },
    SlashCommand {
        name: "model",
        description: "Show or change current model",
        input_hint: Some("model name to switch to"),
    },
    SlashCommand {
        name: "tools",
        description: "List available tools",
        input_hint: None,
    },
    SlashCommand {
        name: "context",
        description: "Show conversation context info",
        input_hint: None,
    },
    SlashCommand {
        name: "reset",
        description: "Clear conversation history",
        input_hint: None,
    },
    SlashCommand {
        name: "compact",
        description: "Compress conversation context",
        input_hint: None,
    },
    SlashCommand {
        name: "version",
        description: "Show Hermes version",
        input_hint: None,
    },
];

// ---------------------------------------------------------------------------
// HermesAcpHandler
// ---------------------------------------------------------------------------

/// Full ACP handler wrapping Hermes agent capabilities.
pub struct HermesAcpHandler {
    pub session_manager: Arc<SessionManager>,
    pub event_sink: Arc<EventSink>,
    pub permission_store: Arc<PermissionStore>,
    /// Real per-session agent loops. `None` = metadata-only handler:
    /// `session/prompt` then fails honestly with -32002 instead of echoing
    /// a placeholder.
    pub agent_runtime: Option<Arc<AgentRuntime>>,
    version: String,
}

impl HermesAcpHandler {
    pub fn new(
        session_manager: Arc<SessionManager>,
        event_sink: Arc<EventSink>,
        permission_store: Arc<PermissionStore>,
    ) -> Self {
        Self {
            session_manager,
            event_sink,
            permission_store,
            agent_runtime: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Attach the real agent runtime (per-session `AgentLoop`s). Builder style.
    pub fn with_agent_runtime(mut self, runtime: Arc<AgentRuntime>) -> Self {
        self.agent_runtime = Some(runtime);
        self
    }

    #[allow(dead_code)]
    fn available_commands(&self) -> Vec<Value> {
        SLASH_COMMANDS
            .iter()
            .map(|cmd| {
                let mut obj = json!({
                    "name": cmd.name,
                    "description": cmd.description,
                });
                if let Some(hint) = cmd.input_hint {
                    obj["input"] = json!({"hint": hint});
                }
                obj
            })
            .collect()
    }

    fn handle_slash_command(&self, text: &str, session_id: &str) -> Option<String> {
        let parts: Vec<&str> = text.splitn(2, ' ').collect();
        let cmd = parts[0].trim_start_matches('/').to_lowercase();
        let _args = if parts.len() > 1 { parts[1].trim() } else { "" };

        match cmd.as_str() {
            "help" => {
                let mut lines = vec!["Available commands:".to_string(), String::new()];
                for sc in SLASH_COMMANDS {
                    lines.push(format!("  /{:<10}  {}", sc.name, sc.description));
                }
                lines.push(String::new());
                lines.push(
                    "Unrecognized /commands are sent to the model as normal messages.".to_string(),
                );
                Some(lines.join("\n"))
            }
            "model" => {
                let state = self.session_manager.get_session(session_id)?;
                let model = state.model.as_deref().unwrap_or("unknown");
                let provider = state.provider.as_deref().unwrap_or("auto");
                Some(format!("Current model: {model}\nProvider: {provider}"))
            }
            "context" => {
                let state = self.session_manager.get_session(session_id)?;
                let n = state.history.len();
                if n == 0 {
                    return Some("Conversation is empty (no messages yet).".to_string());
                }
                let mut roles: HashMap<String, usize> = HashMap::new();
                for msg in &state.history {
                    let role = msg
                        .get("role")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    *roles.entry(role.to_string()).or_default() += 1;
                }
                Some(format!(
                    "Conversation: {} messages\n  user: {}, assistant: {}, tool: {}, system: {}",
                    n,
                    roles.get("user").unwrap_or(&0),
                    roles.get("assistant").unwrap_or(&0),
                    roles.get("tool").unwrap_or(&0),
                    roles.get("system").unwrap_or(&0),
                ))
            }
            "reset" => {
                self.session_manager.set_history(session_id, Vec::new());
                self.session_manager.save_session(session_id);
                Some("Conversation history cleared.".to_string())
            }
            "compact" => {
                Some("Context compression is not yet implemented in the Rust agent.".to_string())
            }
            "version" => Some(format!("Hermes Agent v{}", self.version)),
            "tools" => {
                Some("Use /tools to list tools (not yet implemented in Rust agent).".to_string())
            }
            _ => None,
        }
    }

    // -- helpers --------------------------------------------------------

    /// Auth methods advertised in `initialize`. `authenticate` acknowledges
    /// exactly these ids and nothing else.
    fn auth_methods(&self) -> Vec<AuthMethod> {
        let mut methods = Vec::new();
        if let Some(rt) = &self.agent_runtime {
            let id = rt.provider_id();
            if !id.is_empty() {
                methods.push(AuthMethod {
                    id: id.to_string(),
                    name: format!("{id} runtime credentials"),
                    description: Some(format!(
                        "Authenticate using the currently configured {id} credentials."
                    )),
                });
            }
        }
        methods.push(AuthMethod {
            id: TERMINAL_SETUP_AUTH_METHOD_ID.to_string(),
            name: "Configure Hermes provider".to_string(),
            description: Some(
                "Open Hermes' interactive model/provider setup in a terminal.".to_string(),
            ),
        });
        methods
    }

    /// `{"models": [...], "currentModelId": ...}` block for session responses.
    fn model_state(&self, current_model: &str) -> Value {
        json!({
            "models": [{ "modelId": current_model }],
            "currentModelId": current_model,
        })
    }

    fn session_model(&self, state: &SessionState) -> String {
        state.model.clone().unwrap_or_else(|| {
            self.agent_runtime
                .as_ref()
                .map(|rt| rt.default_model().to_string())
                .unwrap_or_else(|| "default".to_string())
        })
    }

    fn append_history(&self, session_id: &str, role: &str, content: &str) {
        if let Some(state) = self.session_manager.get_session(session_id) {
            let mut history = state.history;
            history.push(json!({"role": role, "content": content}));
            self.session_manager.set_history(session_id, history);
        }
    }

    // -- method implementations ------------------------------------------

    fn initialize(&self, id: Option<Value>) -> HandlerOutcome {
        let resp = InitializeResponse {
            protocol_version: 1,
            agent_info: Implementation {
                name: "hermes-agent".to_string(),
                version: self.version.clone(),
            },
            agent_capabilities: AgentCapabilities {
                load_session: true,
                session_capabilities: Some(SessionCapabilities {
                    fork: Some(Supported),
                    list: Some(Supported),
                    resume: Some(Supported),
                }),
                prompt_capabilities: Some(PromptCapabilities {
                    image: false,
                    audio: false,
                    embedded_context: true,
                }),
                streaming: true,
                ..Default::default()
            },
            auth_methods: Some(self.auth_methods()),
        };
        HandlerOutcome::ok(id, serde_json::to_value(&resp).unwrap())
    }

    fn authenticate(&self, id: Option<Value>, params: Option<&Value>) -> HandlerOutcome {
        // Only acknowledge method ids advertised in initialize(); anything
        // else yields a null result (not an error).
        let method_id = params
            .and_then(|p| p.as_object())
            .map(|p| {
                param_str_any(p, "methodId", "method_id")
                    .trim()
                    .to_lowercase()
            })
            .unwrap_or_default();
        let provider_ack = self
            .agent_runtime
            .as_ref()
            .map(|rt| !rt.provider_id().is_empty() && method_id == rt.provider_id().to_lowercase())
            .unwrap_or(false);
        if method_id == TERMINAL_SETUP_AUTH_METHOD_ID || provider_ack {
            tracing::info!("acp: authenticate acknowledged for method_id '{method_id}'");
            HandlerOutcome::ok(id, json!({}))
        } else {
            HandlerOutcome::ok(id, Value::Null)
        }
    }

    fn new_session(&self, id: Option<Value>, params: Option<&Value>) -> HandlerOutcome {
        let cwd = params
            .and_then(|p| p.as_object())
            .map(|p| param_str_any(p, "cwd", "cwd"))
            .unwrap_or(".");
        let state = self.session_manager.create_session(cwd);
        let model = self.session_model(&state);
        if let Some(rt) = &self.agent_runtime {
            rt.runtime_for(&state.session_id, Some(&model));
        }
        HandlerOutcome::ok(
            id,
            json!({ "sessionId": state.session_id, "models": self.model_state(&model) }),
        )
    }

    fn load_session(&self, id: Option<Value>, params: Option<&Value>) -> HandlerOutcome {
        let Some(p) = params.and_then(|v| v.as_object()) else {
            return HandlerOutcome::err(id, -32602, "Missing params");
        };
        let session_id = param_str_any(p, "sessionId", "session_id");
        if session_id.is_empty() {
            return HandlerOutcome::err(id, -32602, "sessionId is required");
        }
        let Some(state) = self.session_manager.get_session(session_id) else {
            return HandlerOutcome::err(id, -32000, format!("unknown session: {session_id}"));
        };
        let cwd = param_str_any(p, "cwd", "cwd");
        if !cwd.is_empty() {
            self.session_manager.update_cwd(session_id, cwd);
        }
        // Replay history BEFORE the response, as the ACP spec requires.
        let mut pre = Vec::new();
        replay_history(&state, &mut pre);
        let model = self.session_model(&state);
        if let Some(rt) = &self.agent_runtime {
            rt.runtime_for(session_id, Some(&model));
        }
        let mut outcome = HandlerOutcome::ok(
            id,
            json!({ "sessionId": state.session_id, "models": self.model_state(&model) }),
        );
        outcome.pre_notifications = pre;
        outcome
    }

    fn resume_session(&self, id: Option<Value>, params: Option<&Value>) -> HandlerOutcome {
        let Some(p) = params.and_then(|v| v.as_object()) else {
            return HandlerOutcome::err(id, -32602, "Missing params");
        };
        let session_id = param_str_any(p, "sessionId", "session_id");
        if session_id.is_empty() {
            return HandlerOutcome::err(id, -32602, "sessionId is required");
        }
        let cwd = param_str_any(p, "cwd", "cwd");
        let cwd = if cwd.is_empty() { "." } else { cwd };

        // Unknown id → brand-new session (not an error).
        let state = match self.session_manager.get_session(session_id) {
            Some(state) => {
                if !param_str_any(p, "cwd", "cwd").is_empty() {
                    self.session_manager.update_cwd(session_id, cwd);
                }
                state
            }
            None => self.session_manager.create_session(cwd),
        };
        let mut pre = Vec::new();
        replay_history(&state, &mut pre);
        let model = self.session_model(&state);
        if let Some(rt) = &self.agent_runtime {
            rt.runtime_for(&state.session_id, Some(&model));
        }
        let mut outcome = HandlerOutcome::ok(
            id,
            json!({ "sessionId": state.session_id, "models": self.model_state(&model) }),
        );
        outcome.pre_notifications = pre;
        outcome
    }

    fn fork_session(&self, id: Option<Value>, params: Option<&Value>) -> HandlerOutcome {
        let Some(p) = params.and_then(|v| v.as_object()) else {
            return HandlerOutcome::err(id, -32602, "Missing params");
        };
        let session_id = param_str_any(p, "sessionId", "session_id");
        if session_id.is_empty() {
            return HandlerOutcome::err(id, -32602, "sessionId is required");
        }
        let cwd = param_str_any(p, "cwd", "cwd");
        let cwd = if cwd.is_empty() { "." } else { cwd };
        match self.session_manager.fork_session(session_id, cwd) {
            Some(new_state) => {
                let model = self.session_model(&new_state);
                if let Some(rt) = &self.agent_runtime {
                    rt.runtime_for(&new_state.session_id, Some(&model));
                }
                HandlerOutcome::ok(
                    id,
                    json!({ "sessionId": new_state.session_id, "models": self.model_state(&model) }),
                )
            }
            None => HandlerOutcome::err(id, -32000, format!("unknown session: {session_id}")),
        }
    }

    fn list_sessions(&self, id: Option<Value>) -> HandlerOutcome {
        let sessions: Vec<Value> = self
            .session_manager
            .list_sessions()
            .iter()
            .map(|s| {
                json!({
                    "sessionId": s.session_id,
                    "cwd": s.cwd,
                    "model": s.model,
                    "updatedAt": s.updated_at,
                })
            })
            .collect();
        HandlerOutcome::ok(id, json!({ "sessions": sessions }))
    }

    /// Real cancellation: signal the session's interrupt controller so the
    /// in-flight turn aborts. Idempotent — unknown sessions still answer
    /// success with a null result.
    fn cancel_session(&self, id: Option<Value>, params: Option<&Value>) -> HandlerOutcome {
        let session_id = params
            .and_then(|v| v.as_object())
            .map(|p| param_str_any(p, "sessionId", "session_id"))
            .unwrap_or("");
        if let Some(rt) = &self.agent_runtime {
            rt.cancel(session_id);
        }
        if self.session_manager.get_session(session_id).is_some() {
            self.session_manager
                .set_phase(session_id, SessionPhase::Cancelled);
        }
        tracing::info!("acp: cancelled session {session_id}");
        HandlerOutcome::ok(id, Value::Null)
    }

    async fn prompt(&self, id: Option<Value>, params: Option<&Value>) -> HandlerOutcome {
        let Some(p) = params.and_then(|v| v.as_object()) else {
            return HandlerOutcome::err(id, -32602, "Missing params");
        };
        let session_id = param_str_any(p, "sessionId", "session_id").to_string();
        if session_id.is_empty() {
            return HandlerOutcome::err(id, -32602, "sessionId is required");
        }
        let Some(state) = self.session_manager.get_session(&session_id) else {
            return HandlerOutcome::err(id, -32602, format!("Session not found: {session_id}"));
        };

        let user_text = extract_prompt_text(p);
        if user_text.trim().is_empty() {
            return HandlerOutcome::ok(id, json!({"stopReason": "end_turn"}));
        }

        // Slash commands are local — they never need the agent runtime.
        if user_text.starts_with('/') {
            if let Some(response_text) = self.handle_slash_command(&user_text, &session_id) {
                self.event_sink
                    .push(AcpEvent::message_complete(&session_id, &response_text));
                return HandlerOutcome::ok(id, json!({"stopReason": "end_turn"}));
            }
        }

        let Some(agent_runtime) = self.agent_runtime.clone() else {
            return HandlerOutcome::err(
                id,
                -32002,
                "session/prompt requires an agent runtime; start the server via start_acp_server",
            );
        };

        let model = self.session_model(&state);
        let runtime = agent_runtime.runtime_for(&session_id, Some(&model));

        // Only one turn per session at a time.
        if runtime.busy.swap(true, Ordering::SeqCst) {
            return HandlerOutcome::err(
                id,
                -32603,
                "session is busy; wait for the running turn to finish",
            );
        }
        // A cancel from a previous turn must not freeze this one.
        runtime.interrupt.clear_interrupt();

        self.session_manager
            .set_phase(&session_id, SessionPhase::Active);
        self.append_history(&session_id, "user", &user_text);

        let messages = history_to_messages(
            &self
                .session_manager
                .get_session(&session_id)
                .map(|s| s.history)
                .unwrap_or_default(),
        );

        let sink = self.event_sink.clone();
        let sid = session_id.clone();
        let on_chunk = Box::new(move |chunk: hermes_core::StreamChunk| {
            if let Some(delta) = chunk.delta {
                if let Some(content) = delta.content {
                    if !content.is_empty() {
                        sink.push(AcpEvent::message_delta(&sid, &content));
                    }
                }
            }
        });

        self.event_sink
            .push(AcpEvent::thinking(&session_id, "Processing request…"));
        let result = runtime
            .agent
            .run_stream(messages, None, Some(on_chunk))
            .await;
        runtime.busy.store(false, Ordering::SeqCst);

        match result {
            Ok(agent_result) => {
                let reply = last_assistant_text(&agent_result.messages);
                if !reply.trim().is_empty() {
                    self.append_history(&session_id, "assistant", &reply);
                    self.event_sink
                        .push(AcpEvent::message_complete(&session_id, &reply));
                }
                if let Some(usage) = agent_result.usage {
                    self.session_manager.add_usage(
                        &session_id,
                        usage.prompt_tokens,
                        usage.completion_tokens,
                    );
                }
                let (stop_reason, phase) = if agent_result.interrupted {
                    ("cancelled", SessionPhase::Cancelled)
                } else {
                    ("end_turn", SessionPhase::Idle)
                };
                self.session_manager.set_phase(&session_id, phase);
                HandlerOutcome::ok(id, json!({ "stopReason": stop_reason }))
            }
            Err(hermes_core::AgentError::Interrupted { .. }) => {
                self.session_manager
                    .set_phase(&session_id, SessionPhase::Cancelled);
                HandlerOutcome::ok(id, json!({ "stopReason": "cancelled" }))
            }
            Err(e) => {
                self.session_manager
                    .set_phase(&session_id, SessionPhase::Failed);
                HandlerOutcome::err(id, -32001, format!("agent error: {e}"))
            }
        }
    }

    /// Real model switch: rebuilds the session's agent loop with the new
    /// model. Rejected while a turn is running.
    fn set_session_model(&self, id: Option<Value>, params: Option<&Value>) -> HandlerOutcome {
        let Some(p) = params.and_then(|v| v.as_object()) else {
            return HandlerOutcome::err(id, -32602, "Missing params");
        };
        let session_id = param_str_any(p, "sessionId", "session_id");
        if session_id.is_empty() {
            return HandlerOutcome::err(id, -32602, "sessionId is required");
        }
        let model_id = param_str_any(p, "modelId", "model_id");
        let model_id = if model_id.is_empty() {
            param_str_any(p, "model", "model")
        } else {
            model_id
        };
        if model_id.is_empty() {
            return HandlerOutcome::err(id, -32602, "modelId is required");
        }
        if self.session_manager.get_session(session_id).is_none() {
            return HandlerOutcome::err(id, -32602, format!("Session not found: {session_id}"));
        }
        if let Some(rt) = &self.agent_runtime {
            if rt.is_busy(session_id) {
                return HandlerOutcome::err(
                    id,
                    -32603,
                    "session is busy; switch models while the session is idle",
                );
            }
            rt.set_model(session_id, model_id);
        }
        self.session_manager.set_model(session_id, model_id);
        self.session_manager.save_session(session_id);
        tracing::info!("acp: session {session_id} model switched to {model_id}");
        HandlerOutcome::ok(id, json!({}))
    }

    /// This build has no per-session modes — approval policy is server-wide.
    /// Return -32002 instead of pretending to switch a mode.
    fn set_session_mode(&self, id: Option<Value>, params: Option<&Value>) -> HandlerOutcome {
        let session_id = params
            .and_then(|v| v.as_object())
            .map(|p| param_str_any(p, "sessionId", "session_id"))
            .unwrap_or("");
        if self.session_manager.get_session(session_id).is_none() {
            return HandlerOutcome::err(id, -32602, format!("Session not found: {session_id}"));
        }
        HandlerOutcome::err(
            id,
            -32002,
            "session/set_mode is not supported: this build has no per-session modes; \
             edit approval policy is controlled server-wide",
        )
    }

    /// Named config options are stored on the session state and persisted via
    /// the session manager's persist hook. Unknown sessions yield a null
    /// result (not an error).
    fn set_config_option(&self, id: Option<Value>, params: Option<&Value>) -> HandlerOutcome {
        let Some(p) = params.and_then(|v| v.as_object()) else {
            return HandlerOutcome::err(id, -32602, "Missing params");
        };
        let session_id = param_str_any(p, "sessionId", "session_id");
        if self.session_manager.get_session(session_id).is_none() {
            return HandlerOutcome::ok(id, Value::Null);
        }
        let config_id = param_str_any(p, "configId", "config_id");
        if config_id.is_empty() {
            return HandlerOutcome::err(id, -32602, "configId is required");
        }
        if config_id == EDIT_APPROVAL_POLICY_CONFIG_ID {
            return HandlerOutcome::err(
                id,
                -32002,
                "set_config_option for edit_approval_policy is not supported: \
                 this build has no per-session modes; edit approval policy is \
                 controlled server-wide",
            );
        }
        let Some(value) = p.get("value").and_then(|v| v.as_str()) else {
            return HandlerOutcome::err(id, -32602, "value is required (string)");
        };
        self.session_manager
            .set_config_option(session_id, config_id, value);
        self.session_manager.save_session(session_id);
        tracing::info!("acp: session {session_id} config option '{config_id}' updated");
        HandlerOutcome::ok(id, json!({ "configOptions": [] }))
    }

    /// Dispatch one request to its method implementation.
    async fn dispatch(&self, request: AcpRequest) -> HandlerOutcome {
        let id = request.id;
        let method = AcpMethod::from(request.method.as_str());
        let params = request.params;
        match method {
            // -- Lifecycle --------------------------------------------------
            AcpMethod::Initialize => self.initialize(id),
            AcpMethod::Authenticate => self.authenticate(id, params.as_ref()),

            // -- Session management -----------------------------------------
            AcpMethod::NewSession => self.new_session(id, params.as_ref()),
            AcpMethod::LoadSession => self.load_session(id, params.as_ref()),
            AcpMethod::ResumeSession => self.resume_session(id, params.as_ref()),
            AcpMethod::ForkSession => self.fork_session(id, params.as_ref()),
            AcpMethod::ListSessions => self.list_sessions(id),
            AcpMethod::Cancel => self.cancel_session(id, params.as_ref()),

            // -- Prompt (core) ----------------------------------------------
            AcpMethod::Prompt => self.prompt(id, params.as_ref()).await,

            // -- Session configuration --------------------------------------
            AcpMethod::SetSessionModel => self.set_session_model(id, params.as_ref()),
            AcpMethod::SetSessionMode => self.set_session_mode(id, params.as_ref()),
            AcpMethod::SetConfigOption => self.set_config_option(id, params.as_ref()),

            // -- Legacy methods ---------------------------------------------
            AcpMethod::CreateConversation => {
                let state = self.session_manager.create_session(".");
                HandlerOutcome::ok(id, json!({"conversation_id": state.session_id}))
            }

            AcpMethod::SendMessage => {
                let Some(p) = params.as_ref().and_then(|v| v.as_object()) else {
                    return HandlerOutcome::err(id, -32602, "message.send: missing params object");
                };
                let conv_id = param_str(p, "conversation_id").unwrap_or("");
                let text = param_str(p, "text")
                    .or_else(|| param_str(p, "content"))
                    .unwrap_or("");
                let msg_id = uuid::Uuid::new_v4().to_string();

                if let Some(state) = self.session_manager.get_session(conv_id) {
                    let mut history = state.history;
                    history.push(json!({
                        "id": msg_id,
                        "role": "user",
                        "content": text,
                    }));
                    self.session_manager.set_history(conv_id, history);
                    HandlerOutcome::ok(
                        id,
                        json!({"message_id": msg_id, "conversation_id": conv_id}),
                    )
                } else {
                    HandlerOutcome::err(id, -32602, format!("Unknown conversation_id '{conv_id}'"))
                }
            }

            AcpMethod::GetHistory => {
                let Some(p) = params.as_ref().and_then(|v| v.as_object()) else {
                    return HandlerOutcome::err(id, -32602, "history.get: missing params object");
                };
                let conv_id = param_str(p, "conversation_id").unwrap_or("");
                let messages = self
                    .session_manager
                    .get_session(conv_id)
                    .map(|s| s.history)
                    .unwrap_or_default();
                HandlerOutcome::ok(id, json!({"messages": messages}))
            }

            AcpMethod::ListTools => HandlerOutcome::ok(id, json!({"tools": []})),

            AcpMethod::ExecuteTool => {
                let Some(p) = params.as_ref().and_then(|v| v.as_object()) else {
                    return HandlerOutcome::err(id, -32602, "tools.execute: missing params object");
                };
                let name = p
                    .get("name")
                    .or_else(|| p.get("tool"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let arguments = p.get("arguments").cloned().unwrap_or(Value::Null);
                HandlerOutcome::ok(
                    id,
                    json!({
                        "tool": name,
                        "arguments": arguments,
                        "result": format!("ACP handler echo for tool '{}'", name),
                    }),
                )
            }

            AcpMethod::GetStatus => HandlerOutcome::ok(
                id,
                json!({
                    "status": "ready",
                    "version": self.version,
                }),
            ),

            AcpMethod::Unknown(method) => {
                HandlerOutcome::err(id, -32601, format!("Method not found: {}", method))
            }
        }
    }
}

fn param_str<'a>(p: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    p.get(key)?.as_str()
}

/// Read a param accepting both camelCase and snake_case spellings.
fn param_str_any<'a>(p: &'a serde_json::Map<String, Value>, camel: &str, snake: &str) -> &'a str {
    param_str(p, camel)
        .or_else(|| param_str(p, snake))
        .unwrap_or("")
}

/// Extract the user text from `prompt` content blocks (or a plain `text` param).
fn extract_prompt_text(p: &serde_json::Map<String, Value>) -> String {
    if let Some(prompt_val) = p.get("prompt") {
        if let Some(arr) = prompt_val.as_array() {
            return arr
                .iter()
                .filter_map(|b| {
                    let is_text =
                        b.get("type").and_then(|t| t.as_str()).unwrap_or("text") == "text";
                    if is_text {
                        b.get("text").and_then(|t| t.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
        } else if let Some(s) = prompt_val.as_str() {
            return s.to_string();
        }
    }
    p.get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// One `session/update` notification (full JSON-RPC object) for history replay.
fn session_update_notification(session_id: &str, update_kind: &str, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": update_kind,
                "content": { "type": "text", "text": text },
            }
        }
    })
}

/// Replay a session's stored history as `session/update` notifications.
/// User and assistant messages become text bubbles; anything else is skipped.
fn replay_history(state: &SessionState, out: &mut Vec<Value>) {
    for msg in &state.history {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let kind = match role {
            "user" => "user_message_chunk",
            "assistant" => "agent_message_chunk",
            _ => continue,
        };
        let text = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if text.trim().is_empty() {
            continue;
        }
        out.push(session_update_notification(&state.session_id, kind, text));
    }
}

/// Convert stored history entries into agent [`hermes_core::Message`]s.
fn history_to_messages(history: &[Value]) -> Vec<hermes_core::Message> {
    history
        .iter()
        .filter_map(|v| {
            let role = v.get("role")?.as_str()?;
            let content = v.get("content")?.as_str()?;
            match role {
                "user" => Some(hermes_core::Message::user(content)),
                "assistant" => Some(hermes_core::Message::assistant(content)),
                _ => None,
            }
        })
        .collect()
}

/// Text of the last assistant message in a turn result.
fn last_assistant_text(messages: &[hermes_core::Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, hermes_core::MessageRole::Assistant))
        .and_then(|m| m.content.clone())
        .unwrap_or_default()
}

#[async_trait::async_trait]
impl AcpHandler for HermesAcpHandler {
    async fn handle_request(&self, request: AcpRequest) -> AcpResponse {
        self.dispatch(request).await.response
    }

    async fn handle_request_full(&self, request: AcpRequest) -> HandlerOutcome {
        self.dispatch(request).await
    }
}

// ---------------------------------------------------------------------------
// Default handler (backward compat)
// ---------------------------------------------------------------------------

/// Minimal default ACP handler for backward compatibility.
pub struct DefaultAcpHandler {
    inner: HermesAcpHandler,
}

impl DefaultAcpHandler {
    pub fn new() -> Self {
        Self {
            inner: HermesAcpHandler::new(
                Arc::new(SessionManager::new()),
                Arc::new(EventSink::default()),
                Arc::new(PermissionStore::new()),
            ),
        }
    }
}

impl Default for DefaultAcpHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl AcpHandler for DefaultAcpHandler {
    async fn handle_request(&self, request: AcpRequest) -> AcpResponse {
        self.inner.handle_request(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::mock_runtime;
    use serde_json::json;
    use std::time::Duration;

    fn make_handler() -> HermesAcpHandler {
        HermesAcpHandler::new(
            Arc::new(SessionManager::new()),
            Arc::new(EventSink::default()),
            Arc::new(PermissionStore::new()),
        )
    }

    fn make_handler_with_runtime() -> HermesAcpHandler {
        make_handler().with_agent_runtime(Arc::new(mock_runtime()))
    }

    fn req(id: i32, method: &str, params: Option<Value>) -> AcpRequest {
        AcpRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(id)),
            method: method.into(),
            params,
        }
    }

    async fn new_session_id(handler: &HermesAcpHandler) -> String {
        let resp = handler
            .handle_request(req(1, "session/new", Some(json!({"cwd": "/tmp"}))))
            .await;
        resp.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn test_initialize() {
        let handler = make_handler_with_runtime();
        let resp = handler.handle_request(req(1, "initialize", None)).await;
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["agentInfo"]["name"], "hermes-agent");
        // camelCase capabilities per the ACP spec.
        assert_eq!(result["protocolVersion"], 1);
        assert_eq!(result["agentCapabilities"]["loadSession"], true);
        assert_eq!(
            result["agentCapabilities"]["sessionCapabilities"],
            json!({"fork": {}, "list": {}, "resume": {}})
        );
        assert_eq!(
            result["agentCapabilities"]["promptCapabilities"]["embeddedContext"],
            true
        );
        // Auth methods are advertised and honest.
        let methods = result["authMethods"].as_array().unwrap();
        let ids: Vec<&str> = methods.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&"mock"));
        assert!(ids.contains(&"hermes-setup"));
    }

    #[tokio::test]
    async fn test_authenticate_honest() {
        let handler = make_handler_with_runtime();
        // Advertised id → empty result.
        let resp = handler
            .handle_request(req(1, "authenticate", Some(json!({"methodId": "mock"}))))
            .await;
        assert_eq!(resp.result.unwrap(), json!({}));
        // Unknown id → null result, not an error.
        let resp = handler
            .handle_request(req(2, "authenticate", Some(json!({"methodId": "nope"}))))
            .await;
        assert!(resp.error.is_none());
        assert!(resp.result.unwrap().is_null());
        // Missing params → null as well.
        let resp = handler.handle_request(req(3, "authenticate", None)).await;
        assert!(resp.result.unwrap().is_null());
    }

    #[tokio::test]
    async fn test_session_lifecycle() {
        let handler = make_handler_with_runtime();

        let session_id = new_session_id(&handler).await;

        // List sessions
        let resp = handler.handle_request(req(2, "session/list", None)).await;
        let sessions = resp.result.unwrap()["sessions"].as_array().unwrap().clone();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["sessionId"], session_id);

        // Fork session
        let resp = handler
            .handle_request(req(
                3,
                "session/fork",
                Some(json!({"sessionId": session_id, "cwd": "/other"})),
            ))
            .await;
        let forked_id = resp.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ne!(forked_id, session_id);

        // Load unknown session → -32000.
        let resp = handler
            .handle_request(req(4, "session/load", Some(json!({"sessionId": "nope"}))))
            .await;
        assert_eq!(resp.error.unwrap().code, -32000);

        // Resume unknown session → brand-new session id.
        let resp = handler
            .handle_request(req(5, "session/resume", Some(json!({"sessionId": "nope"}))))
            .await;
        let resumed_id = resp.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!resumed_id.is_empty());
        assert_ne!(resumed_id, "nope");
    }

    #[tokio::test]
    async fn test_prompt_runs_real_turn() {
        let handler = make_handler_with_runtime();
        let session_id = new_session_id(&handler).await;

        let resp = handler
            .handle_request(req(
                2,
                "session/prompt",
                Some(
                    json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "hello"}]}),
                ),
            ))
            .await;
        assert!(resp.error.is_none());
        assert_eq!(resp.result.unwrap()["stopReason"], "end_turn");

        // The turn really ran: history holds the user message AND the
        // assistant reply (not an echo placeholder).
        let state = handler.session_manager.get_session(&session_id).unwrap();
        assert_eq!(state.history.len(), 2);
        assert_eq!(state.history[0]["role"], "user");
        assert_eq!(state.history[0]["content"], "hello");
        assert_eq!(state.history[1]["role"], "assistant");
        assert_eq!(state.history[1]["content"], "mock reply");

        // Streaming events reached the sink.
        let events = handler.event_sink.drain_for_session(&session_id);
        assert!(events
            .iter()
            .any(|e| e.kind == crate::events::AcpEventKind::MessageComplete));
    }

    #[tokio::test]
    async fn test_prompt_unknown_session_errors() {
        let handler = make_handler_with_runtime();
        let resp = handler
            .handle_request(req(
                1,
                "session/prompt",
                Some(json!({"sessionId": "ghost", "prompt": [{"type": "text", "text": "hi"}]})),
            ))
            .await;
        assert_eq!(resp.error.unwrap().code, -32602);
    }

    #[tokio::test]
    async fn test_prompt_without_runtime_is_honest_error() {
        // No agent runtime attached → -32002, never a fake echo.
        let handler = make_handler();
        let session_id = new_session_id(&handler).await;
        let resp = handler
            .handle_request(req(
                2,
                "session/prompt",
                Some(json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "hi"}]})),
            ))
            .await;
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32002);
    }

    #[tokio::test]
    async fn test_cancel_aborts_running_turn() {
        use crate::agent::mock_slow_runtime;

        let handler = Arc::new(make_handler().with_agent_runtime(Arc::new(mock_slow_runtime())));
        let session_id = new_session_id(&handler).await;

        // Start a slow turn in the background…
        let h2 = handler.clone();
        let sid2 = session_id.clone();
        let prompt_task = tokio::spawn(async move {
            h2.handle_request(req(
                2,
                "session/prompt",
                Some(json!({"sessionId": sid2, "prompt": [{"type": "text", "text": "slow"}]})),
            ))
            .await
        });
        // …let it get going, then cancel it for real.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let cancel_resp = handler
            .handle_request(req(
                3,
                "session/cancel",
                Some(json!({"sessionId": session_id})),
            ))
            .await;
        assert!(cancel_resp.error.is_none());

        let prompt_resp = prompt_task.await.unwrap();
        assert!(
            prompt_resp.error.is_none(),
            "cancel must not surface as an error"
        );
        assert_eq!(
            prompt_resp.result.unwrap()["stopReason"],
            "cancelled",
            "a cancelled turn reports stopReason=cancelled"
        );
        assert_eq!(
            handler
                .session_manager
                .get_session(&session_id)
                .unwrap()
                .phase,
            SessionPhase::Cancelled
        );
    }

    #[tokio::test]
    async fn test_cancel_unknown_session_is_idempotent() {
        let handler = make_handler_with_runtime();
        let resp = handler
            .handle_request(req(
                1,
                "session/cancel",
                Some(json!({"sessionId": "ghost"})),
            ))
            .await;
        assert!(resp.error.is_none());
        assert!(resp.result.unwrap().is_null());
    }

    #[tokio::test]
    async fn test_second_prompt_while_busy_rejected() {
        use crate::agent::mock_slow_runtime;

        let handler = Arc::new(make_handler().with_agent_runtime(Arc::new(mock_slow_runtime())));
        let session_id = new_session_id(&handler).await;

        let h2 = handler.clone();
        let sid2 = session_id.clone();
        let _bg = tokio::spawn(async move {
            h2.handle_request(req(
                2,
                "session/prompt",
                Some(json!({"sessionId": sid2, "prompt": [{"type": "text", "text": "slow"}]})),
            ))
            .await
        });
        // The first turn sets the busy flag synchronously before yielding.
        tokio::task::yield_now().await;
        let resp = handler
            .handle_request(req(
                3,
                "session/prompt",
                Some(json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "two"}]})),
            ))
            .await;
        assert_eq!(resp.error.unwrap().code, -32603);
        // Clean up: cancel the background turn.
        handler
            .handle_request(req(
                4,
                "session/cancel",
                Some(json!({"sessionId": session_id})),
            ))
            .await;
    }

    #[tokio::test]
    async fn test_load_replays_history_before_response() {
        let handler = make_handler_with_runtime();
        let session_id = new_session_id(&handler).await;
        handler
            .handle_request(req(
                2,
                "session/prompt",
                Some(
                    json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "hello"}]}),
                ),
            ))
            .await;

        let outcome = handler
            .handle_request_full(req(
                3,
                "session/load",
                Some(json!({"sessionId": session_id})),
            ))
            .await;
        assert!(outcome.response.error.is_none());
        // History replay notifications precede the response.
        assert_eq!(outcome.pre_notifications.len(), 2);
        assert_eq!(outcome.pre_notifications[0]["method"], "session/update");
        assert_eq!(
            outcome.pre_notifications[0]["params"]["update"]["sessionUpdate"],
            "user_message_chunk"
        );
        assert_eq!(
            outcome.pre_notifications[1]["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        assert_eq!(outcome.response.result.unwrap()["sessionId"], session_id);
    }

    #[tokio::test]
    async fn test_set_model_really_switches() {
        let handler = make_handler_with_runtime();
        let session_id = new_session_id(&handler).await;

        let resp = handler
            .handle_request(req(
                2,
                "session/set_model",
                Some(json!({"sessionId": session_id, "modelId": "other-model"})),
            ))
            .await;
        assert!(resp.error.is_none());

        // Session metadata…
        let state = handler.session_manager.get_session(&session_id).unwrap();
        assert_eq!(state.model.as_deref(), Some("other-model"));
        // …and the live agent loop both moved.
        let rt = handler.agent_runtime.as_ref().unwrap();
        assert_eq!(
            rt.get(&session_id).unwrap().agent.config.model,
            "other-model"
        );

        // Unknown session → error.
        let resp = handler
            .handle_request(req(
                3,
                "session/set_model",
                Some(json!({"sessionId": "ghost", "modelId": "x"})),
            ))
            .await;
        assert_eq!(resp.error.unwrap().code, -32602);

        // Missing modelId → -32602.
        let resp = handler
            .handle_request(req(
                4,
                "session/set_model",
                Some(json!({"sessionId": session_id})),
            ))
            .await;
        assert_eq!(resp.error.unwrap().code, -32602);
    }

    #[tokio::test]
    async fn test_set_mode_unsupported() {
        let handler = make_handler_with_runtime();
        let session_id = new_session_id(&handler).await;
        let resp = handler
            .handle_request(req(
                2,
                "session/set_mode",
                Some(json!({"sessionId": session_id, "modeId": "accept_edits"})),
            ))
            .await;
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32002);
    }

    #[tokio::test]
    async fn test_set_config_option_honored() {
        let handler = make_handler_with_runtime();
        let session_id = new_session_id(&handler).await;

        let resp = handler
            .handle_request(req(
                2,
                "session/set_config_option",
                Some(json!({"sessionId": session_id, "configId": "theme", "value": "dark"})),
            ))
            .await;
        assert!(resp.error.is_none());
        assert_eq!(resp.result.unwrap(), json!({"configOptions": []}));
        let state = handler.session_manager.get_session(&session_id).unwrap();
        assert_eq!(
            state.config_options.get("theme").map(String::as_str),
            Some("dark")
        );

        // edit_approval_policy maps to per-session modes → -32002.
        let resp = handler
            .handle_request(
                req(
                    3,
                    "session/set_config_option",
                    Some(json!({"sessionId": session_id, "configId": "edit_approval_policy", "value": "x"})),
                ),
            )
            .await;
        assert_eq!(resp.error.unwrap().code, -32002);

        // Unknown session → null result, not an error.
        let resp = handler
            .handle_request(req(
                4,
                "session/set_config_option",
                Some(json!({"sessionId": "ghost", "configId": "theme", "value": "dark"})),
            ))
            .await;
        assert!(resp.error.is_none());
        assert!(resp.result.unwrap().is_null());
    }

    #[tokio::test]
    async fn test_prompt_slash_command() {
        let handler = make_handler();

        let session_id = new_session_id(&handler).await;

        let resp = handler
            .handle_request(req(
                2,
                "prompt",
                Some(json!({
                    "session_id": session_id,
                    "text": "/help",
                })),
            ))
            .await;
        assert_eq!(
            resp.result.unwrap()["stopReason"].as_str().unwrap(),
            "end_turn"
        );
    }

    #[tokio::test]
    async fn test_unknown_method() {
        let handler = make_handler();
        let resp = handler.handle_request(req(1, "foo.bar", None)).await;
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[tokio::test]
    async fn test_legacy_create_conversation() {
        let handler = DefaultAcpHandler::default();
        let resp = handler
            .handle_request(req(1, "conversation.create", None))
            .await;
        assert!(resp.result.is_some());
        assert!(resp.result.unwrap().get("conversation_id").is_some());
    }
}
