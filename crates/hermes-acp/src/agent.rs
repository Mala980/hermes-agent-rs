//! Real per-session agent runtime for ACP.
//!
//! Each ACP session gets its own [`AgentLoop`] built with a per-session
//! [`InterruptController`], so `session/cancel` genuinely aborts the running
//! turn (the loop checks the controller every iteration) instead of merely
//! acknowledging the request. The provider and tool registry are shared
//! across sessions; only the loop (config + interrupt state) is per-session.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use hermes_agent::agent_loop::ToolRegistry as AgentToolRegistry;
use hermes_agent::{AgentConfig, AgentLoop, InterruptController};
use hermes_core::LlmProvider;

/// Live execution handle for one ACP session.
pub struct SessionRuntime {
    /// The agent loop serving this session's turns.
    pub agent: Arc<AgentLoop>,
    /// Signalled by `session/cancel`; checked by the agent loop every iteration.
    pub interrupt: InterruptController,
    /// True while a `session/prompt` turn is in flight for this session.
    pub busy: AtomicBool,
}

/// Builds per-session [`AgentLoop`]s over one shared provider + tool registry.
pub struct AgentRuntime {
    provider: Arc<dyn LlmProvider>,
    tools: Arc<AgentToolRegistry>,
    base_config: AgentConfig,
    provider_id: String,
    runtimes: Mutex<HashMap<String, Arc<SessionRuntime>>>,
}

impl AgentRuntime {
    /// Create a runtime sharing `provider` + `tools` across sessions.
    ///
    /// `base_config` supplies model, system prompt, budgets, etc.; the
    /// session id is stamped per session. `provider_id` is the id advertised
    /// in `initialize` → `authMethods` (e.g. `"openai"`).
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tools: Arc<AgentToolRegistry>,
        base_config: AgentConfig,
        provider_id: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            tools,
            base_config,
            provider_id: provider_id.into(),
            runtimes: Mutex::new(HashMap::new()),
        }
    }

    /// Provider id advertised in `initialize` and the only provider id that
    /// `authenticate` acknowledges (besides `hermes-setup`).
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// Default model for sessions that never called `session/set_model`.
    pub fn default_model(&self) -> &str {
        &self.base_config.model
    }

    fn build(&self, session_id: &str, model: &str) -> Arc<SessionRuntime> {
        let mut cfg = self.base_config.clone();
        cfg.model = model.to_string();
        cfg.session_id = Some(session_id.to_string());
        let interrupt = InterruptController::new();
        let agent = Arc::new(AgentLoop::with_interrupt(
            cfg,
            self.tools.clone(),
            self.provider.clone(),
            interrupt.clone(),
        ));
        Arc::new(SessionRuntime {
            agent,
            interrupt,
            busy: AtomicBool::new(false),
        })
    }

    /// Get the runtime for a session, creating it on first use.
    ///
    /// When `model` is `Some` and differs from the session's current model,
    /// the agent loop is rebuilt with the new model (the interrupt
    /// controller is preserved).
    pub fn runtime_for(&self, session_id: &str, model: Option<&str>) -> Arc<SessionRuntime> {
        let mut guard = self.runtimes.lock().unwrap();
        if let Some(rt) = guard.get(session_id) {
            if let Some(m) = model {
                if rt.agent.config.model != m {
                    let rebuilt = self.build(session_id, m);
                    guard.insert(session_id.to_string(), rebuilt.clone());
                    return rebuilt;
                }
            }
            return rt.clone();
        }
        let model = model.unwrap_or(&self.base_config.model).to_string();
        let rt = self.build(session_id, &model);
        guard.insert(session_id.to_string(), rt.clone());
        rt
    }

    /// Look up a session's runtime without creating one.
    pub fn get(&self, session_id: &str) -> Option<Arc<SessionRuntime>> {
        self.runtimes.lock().unwrap().get(session_id).cloned()
    }

    /// Rebuild the session's agent loop with a new model, preserving its
    /// interrupt controller. Returns false when the session has no runtime
    /// yet (the model is then picked up lazily on first prompt).
    pub fn set_model(&self, session_id: &str, model: &str) -> bool {
        let (interrupt, cfg) = {
            let guard = self.runtimes.lock().unwrap();
            let Some(rt) = guard.get(session_id) else {
                return false;
            };
            if rt.agent.config.model == model {
                return true;
            }
            let mut cfg = rt.agent.config.clone();
            cfg.model = model.to_string();
            (rt.interrupt.clone(), cfg)
        };
        let agent = Arc::new(AgentLoop::with_interrupt(
            cfg,
            self.tools.clone(),
            self.provider.clone(),
            interrupt.clone(),
        ));
        self.runtimes.lock().unwrap().insert(
            session_id.to_string(),
            Arc::new(SessionRuntime {
                agent,
                interrupt,
                busy: AtomicBool::new(false),
            }),
        );
        true
    }

    /// Signal cancellation for the session's running turn.
    ///
    /// Idempotent: unknown sessions report `false` (the caller still answers
    /// success, matching the reference behavior).
    pub fn cancel(&self, session_id: &str) -> bool {
        let guard = self.runtimes.lock().unwrap();
        match guard.get(session_id) {
            Some(rt) => {
                rt.interrupt.interrupt(None);
                true
            }
            None => false,
        }
    }

    /// Drop a session's runtime (e.g. when the session is removed).
    pub fn remove(&self, session_id: &str) {
        self.runtimes.lock().unwrap().remove(session_id);
    }

    /// True while the session has a turn in flight.
    pub fn is_busy(&self, session_id: &str) -> bool {
        self.runtimes
            .lock()
            .unwrap()
            .get(session_id)
            .map(|rt| rt.busy.load(Ordering::SeqCst))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod mock {
    use super::*;
    use futures::stream::{self, BoxStream};
    use futures::StreamExt as _;
    use hermes_core::{AgentError, LlmResponse, Message, StreamChunk, StreamDelta};

    /// Canned provider for unit tests — no network, instant reply.
    pub struct MockProvider {
        pub reply: String,
    }

    #[async_trait::async_trait]
    impl LlmProvider for MockProvider {
        async fn chat_completion(
            &self,
            _messages: &[Message],
            _tools: &[hermes_core::ToolSchema],
            _max_tokens: Option<u32>,
            _temperature: Option<f64>,
            _model: Option<&str>,
            _extra_body: Option<&serde_json::Value>,
        ) -> Result<LlmResponse, AgentError> {
            Ok(LlmResponse {
                message: Message::assistant(self.reply.clone()),
                usage: None,
                model: "mock".to_string(),
                finish_reason: Some("stop".to_string()),
            })
        }

        fn chat_completion_stream(
            &self,
            _messages: &[Message],
            _tools: &[hermes_core::ToolSchema],
            _max_tokens: Option<u32>,
            _temperature: Option<f64>,
            _model: Option<&str>,
            _extra_body: Option<&serde_json::Value>,
        ) -> BoxStream<'static, Result<StreamChunk, AgentError>> {
            let reply = self.reply.clone();
            stream::iter(vec![
                Ok(StreamChunk {
                    delta: Some(StreamDelta {
                        content: Some(reply),
                        tool_calls: None,
                        extra: None,
                    }),
                    finish_reason: None,
                    usage: None,
                }),
                Ok(StreamChunk {
                    delta: None,
                    finish_reason: Some("stop".to_string()),
                    usage: None,
                }),
            ])
            .boxed()
        }
    }

    /// Build an [`AgentRuntime`] backed by the canned mock provider.
    pub fn mock_runtime() -> AgentRuntime {
        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider {
            reply: "mock reply".to_string(),
        });
        AgentRuntime::new(
            provider,
            Arc::new(AgentToolRegistry::new()),
            AgentConfig::default(),
            "mock",
        )
    }

    /// Slow streaming provider: N text chunks with a delay between them, so
    /// `session/cancel` can land mid-turn in tests.
    pub struct SlowMockProvider {
        pub chunks: usize,
        pub delay_ms: u64,
    }

    #[async_trait::async_trait]
    impl LlmProvider for SlowMockProvider {
        async fn chat_completion(
            &self,
            _messages: &[Message],
            _tools: &[hermes_core::ToolSchema],
            _max_tokens: Option<u32>,
            _temperature: Option<f64>,
            _model: Option<&str>,
            _extra_body: Option<&serde_json::Value>,
        ) -> Result<LlmResponse, AgentError> {
            Ok(LlmResponse {
                message: Message::assistant("slow reply".to_string()),
                usage: None,
                model: "mock".to_string(),
                finish_reason: Some("stop".to_string()),
            })
        }

        fn chat_completion_stream(
            &self,
            _messages: &[Message],
            _tools: &[hermes_core::ToolSchema],
            _max_tokens: Option<u32>,
            _temperature: Option<f64>,
            _model: Option<&str>,
            _extra_body: Option<&serde_json::Value>,
        ) -> BoxStream<'static, Result<StreamChunk, AgentError>> {
            let chunks = self.chunks;
            let delay_ms = self.delay_ms;
            stream::unfold(0usize, move |i| async move {
                if i >= chunks {
                    return None;
                }
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                let chunk = if i + 1 == chunks {
                    StreamChunk {
                        delta: None,
                        finish_reason: Some("stop".to_string()),
                        usage: None,
                    }
                } else {
                    StreamChunk {
                        delta: Some(StreamDelta {
                            content: Some(format!("chunk{i} ")),
                            tool_calls: None,
                            extra: None,
                        }),
                        finish_reason: None,
                        usage: None,
                    }
                };
                Some((Ok(chunk), i + 1))
            })
            .boxed()
        }
    }

    /// Build an [`AgentRuntime`] backed by the slow streaming mock provider
    /// (100 chunks × 20ms ≈ 2s per turn).
    pub fn mock_slow_runtime() -> AgentRuntime {
        let provider: Arc<dyn LlmProvider> = Arc::new(SlowMockProvider {
            chunks: 100,
            delay_ms: 20,
        });
        AgentRuntime::new(
            provider,
            Arc::new(AgentToolRegistry::new()),
            AgentConfig::default(),
            "mock",
        )
    }
}

#[cfg(test)]
pub(crate) use mock::{mock_runtime, mock_slow_runtime};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_for_creates_and_reuses() {
        let rt = mock_runtime();
        let a = rt.runtime_for("s1", None);
        let b = rt.runtime_for("s1", None);
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(a.agent.config.model, AgentConfig::default().model);
    }

    #[test]
    fn test_set_model_rebuilds_loop() {
        let rt = mock_runtime();
        let before = rt.runtime_for("s1", None);
        assert!(rt.set_model("s1", "other-model"));
        let after = rt.get("s1").unwrap();
        assert!(!Arc::ptr_eq(&before, &after));
        assert_eq!(after.agent.config.model, "other-model");
        // Interrupt controller is preserved across the rebuild.
        assert_eq!(
            before.interrupt.is_interrupted(),
            after.interrupt.is_interrupted()
        );
    }

    #[test]
    fn test_cancel_signals_interrupt() {
        let rt = mock_runtime();
        assert!(!rt.cancel("unknown"));
        let handle = rt.runtime_for("s1", None);
        assert!(!handle.interrupt.is_interrupted());
        assert!(rt.cancel("s1"));
        assert!(handle.interrupt.is_interrupted());
    }
}
