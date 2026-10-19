//! Shared agent construction functions.
//!
//! These helpers build an [`AgentConfig`], LLM provider, and tool-registry
//! bridge from a [`GatewayConfig`] + model string.  Both the CLI (`app.rs`)
//! and the Dashboard (`lib.rs`) import from here so the logic lives in one
//! place.

use std::sync::Arc;

use futures::StreamExt;
use serde_json::Value;

use hermes_config::GatewayConfig;
use hermes_core::{AgentError, LlmProvider};
use hermes_tools::ToolRegistry;

use crate::agent_loop::{
    CheapModelRouteConfig, RuntimeProviderConfig, SmartModelRoutingConfig,
    ToolRegistry as AgentToolRegistry,
};
use crate::provider::{AnthropicProvider, GenericProvider, OpenAiProvider, OpenRouterProvider};
use crate::providers_extra::{
    CopilotProvider, KimiProvider, MiniMaxProvider, NousProvider, QwenProvider,
};
use crate::providers_native::{AzureOpenAiProvider, BedrockProvider, GeminiProvider};
use crate::AgentConfig;

// ---------------------------------------------------------------------------
// parse_model_string
// ---------------------------------------------------------------------------

/// Split a `"provider:model"` string into `(provider, model)`.
///
/// If no colon is present the provider defaults to `"openai"`.
pub fn parse_model_string(model: &str) -> (&str, &str) {
    model.split_once(':').unwrap_or(("openai", model))
}

// ---------------------------------------------------------------------------
// build_agent_config
// ---------------------------------------------------------------------------

/// Build an [`AgentConfig`] from a [`GatewayConfig`] and a model string.
///
/// `platform` lets callers tag the config with their origin (`"cli"`,
/// `"http"`, `"gateway"`, …).  When `None` the field is left unset.
pub fn build_agent_config(
    config: &GatewayConfig,
    model: &str,
    platform: Option<&str>,
) -> AgentConfig {
    let provider_from_model = model.split_once(':').map(|(p, _)| p.to_string());
    AgentConfig {
        max_turns: config.max_turns,
        budget: config.budget.clone(),
        model: model.to_string(),
        system_prompt: config.system_prompt.clone(),
        personality: config.personality.clone(),
        hermes_home: config.home_dir.clone(),
        provider: provider_from_model,
        stream: config.streaming.enabled,
        platform: platform.map(|s| s.to_string()),
        pass_session_id: true,
        runtime_providers: config
            .llm_providers
            .iter()
            .map(|(name, cfg)| {
                (
                    name.clone(),
                    RuntimeProviderConfig {
                        api_key: cfg.api_key.clone(),
                        base_url: cfg.base_url.clone(),
                        command: cfg.command.clone(),
                        args: cfg.args.clone(),
                        oauth_token_url: cfg.oauth_token_url.clone(),
                        oauth_client_id: cfg.oauth_client_id.clone(),
                    },
                )
            })
            .collect(),
        smart_model_routing: SmartModelRoutingConfig {
            enabled: config.smart_model_routing.enabled,
            max_simple_chars: config.smart_model_routing.max_simple_chars,
            max_simple_words: config.smart_model_routing.max_simple_words,
            cheap_model: config.smart_model_routing.cheap_model.as_ref().map(|m| {
                CheapModelRouteConfig {
                    provider: m.provider.clone(),
                    model: m.model.clone(),
                    base_url: m.base_url.clone(),
                    api_key_env: m.api_key_env.clone(),
                }
            }),
        },
        memory_nudge_interval: config.agent.memory_nudge_interval,
        skill_creation_nudge_interval: config.agent.skill_creation_nudge_interval,
        background_review_enabled: config.agent.background_review_enabled,
        ..AgentConfig::default()
    }
}

// ---------------------------------------------------------------------------
// bridge_tool_registry
// ---------------------------------------------------------------------------

/// Bridge a full [`hermes_tools::ToolRegistry`] into the minimal
/// [`AgentToolRegistry`](crate::agent_loop::ToolRegistry) consumed by
/// [`AgentLoop`](crate::AgentLoop).
pub fn bridge_tool_registry(tools: &ToolRegistry) -> AgentToolRegistry {
    let mut agent_registry = AgentToolRegistry::new();
    for schema in tools.get_definitions() {
        let name = schema.name.clone();
        let tools_clone = tools.clone();
        agent_registry.register(
            name.clone(),
            schema,
            Arc::new(
                move |params: Value| -> Result<String, hermes_core::ToolError> {
                    Ok(tools_clone.dispatch(&name, params))
                },
            ),
        );
    }
    agent_registry
}

// ---------------------------------------------------------------------------
// provider_api_key_from_env
// ---------------------------------------------------------------------------

/// Resolve an API key / token for a named LLM provider from well-known
/// environment variables.
pub fn provider_api_key_from_env(provider: &str) -> Option<String> {
    // Gemini accepts two env var names; Bedrock uses the standard AWS vars and
    // is handled separately in `build_provider`.
    if provider == "gemini" {
        return GeminiProvider::api_key_from_env();
    }
    let var = match provider {
        "openai" => "OPENAI_API_KEY",
        "anthropic" => "ANTHROPIC_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        "qwen" => "DASHSCOPE_API_KEY",
        "kimi" | "moonshot" => "MOONSHOT_API_KEY",
        "minimax" => "MINIMAX_API_KEY",
        "nous" => "NOUS_API_KEY",
        "copilot" => "GITHUB_COPILOT_TOKEN",
        "azure" => "AZURE_OPENAI_API_KEY",
        _ => return None,
    };
    std::env::var(var).ok().filter(|s| !s.is_empty())
}

/// Parse an Azure OpenAI target from a model string.
///
/// Accepted model-string form: `azure:<resource>/<deployment>[/<model>]`.
/// Falls back to `AZURE_OPENAI_RESOURCE` / `AZURE_OPENAI_DEPLOYMENT` env vars.
/// Returns `(resource, deployment, model_label)`.
fn parse_azure_target(model_name: &str) -> Option<(String, String, String)> {
    let mut parts = model_name.splitn(3, '/');
    let resource = parts.next().filter(|s| !s.is_empty());
    let deployment = parts.next().filter(|s| !s.is_empty());
    match (resource, deployment) {
        (Some(r), Some(d)) => {
            let label = parts.next().filter(|s| !s.is_empty()).unwrap_or(d);
            Some((r.to_string(), d.to_string(), label.to_string()))
        }
        _ => {
            let r = AzureOpenAiProvider::resource_from_env()?;
            let d = AzureOpenAiProvider::deployment_from_env()?;
            Some((r, d.clone(), d))
        }
    }
}

// ---------------------------------------------------------------------------
// build_provider
// ---------------------------------------------------------------------------

/// Build an `Arc<dyn LlmProvider>` from a [`GatewayConfig`] and model string.
///
/// Falls back to [`StubProvider`] when no API key can be resolved.
pub fn build_provider(config: &GatewayConfig, model: &str) -> Arc<dyn LlmProvider> {
    let (provider_name, model_name) = parse_model_string(model);

    // AWS Bedrock: credentials come from the standard AWS env vars
    // (AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY / AWS_REGION), not a single
    // API key, so it is resolved before the generic key lookup below.
    if provider_name == "bedrock" {
        match BedrockProvider::from_env(model_name) {
            Ok(p) => return Arc::new(p),
            Err(e) => {
                tracing::warn!(
                    "Bedrock credentials not configured ({e}); falling back to StubProvider"
                );
                return Arc::new(StubProvider {
                    model: model.to_string(),
                });
            }
        }
    }

    let provider_config = config.llm_providers.get(provider_name);

    let api_key = provider_config
        .and_then(|c| c.api_key.clone())
        .or_else(|| provider_api_key_from_env(provider_name));

    let api_key = match api_key {
        Some(k) => k,
        None => {
            tracing::warn!(
                "No API key for provider '{provider_name}'; falling back to StubProvider"
            );
            return Arc::new(StubProvider {
                model: model.to_string(),
            });
        }
    };

    let base_url = provider_config.and_then(|c| c.base_url.clone());

    match provider_name {
        "openai" => {
            let mut p = OpenAiProvider::new(&api_key).with_model(model_name);
            if let Some(url) = base_url {
                p = p.with_base_url(url);
            }
            Arc::new(p)
        }
        "anthropic" => {
            let mut p = AnthropicProvider::new(&api_key).with_model(model_name);
            if let Some(url) = base_url {
                p = p.with_base_url(url);
            }
            Arc::new(p)
        }
        "openrouter" => {
            let mut p = OpenRouterProvider::new(&api_key).with_model(model_name);
            if let Some(url) = base_url {
                p = p.with_base_url(url);
            }
            if let Some(cfg) = provider_config {
                if !cfg.provider_order.is_empty() {
                    p = p.with_provider_order(cfg.provider_order.clone());
                }
            }
            Arc::new(p)
        }
        "qwen" => {
            let mut p = QwenProvider::new(&api_key).with_model(model_name);
            if let Some(url) = base_url {
                p = p.with_base_url(url);
            }
            Arc::new(p)
        }
        "kimi" | "moonshot" => {
            let mut p = KimiProvider::new(&api_key).with_model(model_name);
            if let Some(url) = base_url {
                p = p.with_base_url(url);
            }
            Arc::new(p)
        }
        "minimax" => {
            let mut p = MiniMaxProvider::new(&api_key).with_model(model_name);
            if let Some(url) = base_url {
                p = p.with_base_url(url);
            }
            Arc::new(p)
        }
        "nous" => {
            let mut p = NousProvider::new(&api_key).with_model(model_name);
            if let Some(url) = base_url {
                p = p.with_base_url(url);
            }
            Arc::new(p)
        }
        "copilot" => {
            let p = CopilotProvider::new(
                base_url.unwrap_or_else(|| "https://api.github.com/copilot".to_string()),
                &api_key,
            )
            .with_model(model_name);
            Arc::new(p)
        }
        "deepseek" => {
            let url = base_url.unwrap_or_else(|| "https://api.deepseek.com/v1".to_string());
            Arc::new(GenericProvider::new(url, &api_key, model_name))
        }
        "gemini" => {
            let mut p = GeminiProvider::new(&api_key).with_model(model_name);
            if let Some(url) = base_url {
                p = p.with_base_url(url);
            }
            Arc::new(p)
        }
        "azure" => match parse_azure_target(model_name) {
            Some((resource, deployment, label)) => {
                let api_version = std::env::var("AZURE_OPENAI_API_VERSION")
                    .unwrap_or_else(|_| "2024-12-01-preview".to_string());
                let p = AzureOpenAiProvider::with_api_version(
                    &api_key,
                    resource,
                    deployment,
                    api_version,
                )
                .with_model(label);
                Arc::new(p)
            }
            None => {
                tracing::warn!(
                    "Azure OpenAI needs resource/deployment \
                     (model 'azure:<resource>/<deployment>' or \
                     AZURE_OPENAI_RESOURCE/AZURE_OPENAI_DEPLOYMENT); \
                     falling back to StubProvider"
                );
                Arc::new(StubProvider {
                    model: model.to_string(),
                })
            }
        },
        _ => {
            let url = base_url.unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            Arc::new(GenericProvider::new(url, &api_key, model_name))
        }
    }
}

// ---------------------------------------------------------------------------
// StubProvider — fallback when no API key is configured
// ---------------------------------------------------------------------------

/// Fallback LLM provider returned by [`build_provider`] when no API key
/// can be resolved for the requested provider.  Every call returns an error
/// directing the user to configure credentials.
pub struct StubProvider {
    pub model: String,
}

#[async_trait::async_trait]
impl LlmProvider for StubProvider {
    async fn chat_completion(
        &self,
        _messages: &[hermes_core::Message],
        _tools: &[hermes_core::ToolSchema],
        _max_tokens: Option<u32>,
        _temperature: Option<f64>,
        _model: Option<&str>,
        _extra_body: Option<&Value>,
    ) -> Result<hermes_core::LlmResponse, AgentError> {
        Err(AgentError::LlmApi(format!(
            "StubProvider: no LLM backend configured for model '{}'. \
             Configure an API key and provider in the config file.",
            self.model
        )))
    }

    fn chat_completion_stream(
        &self,
        _messages: &[hermes_core::Message],
        _tools: &[hermes_core::ToolSchema],
        _max_tokens: Option<u32>,
        _temperature: Option<f64>,
        _model: Option<&str>,
        _extra_body: Option<&Value>,
    ) -> futures::stream::BoxStream<'static, Result<hermes_core::StreamChunk, AgentError>> {
        futures::stream::once(async move {
            Err(AgentError::LlmApi(
                "StubProvider: no LLM backend configured for streaming.".to_string(),
            ))
        })
        .boxed()
    }
}

#[cfg(test)]
mod build_provider_tests {
    use super::*;

    #[test]
    fn bedrock_without_creds_falls_back_to_stub() {
        // Deterministic: no network involved.
        let saved_key = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let saved_secret = std::env::var("AWS_SECRET_ACCESS_KEY").ok();
        std::env::remove_var("AWS_ACCESS_KEY_ID");
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");

        let cfg = GatewayConfig::default();
        let p = build_provider(&cfg, "bedrock:us.anthropic.claude-3-5-sonnet-20241022-v2:0");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(async { p.chat_completion(&[], &[], None, None, None, None).await })
            .expect_err("bedrock without creds should error");
        assert!(
            err.to_string().contains("StubProvider"),
            "unexpected: {err}"
        );

        if let Some(k) = saved_key {
            std::env::set_var("AWS_ACCESS_KEY_ID", k);
        }
        if let Some(s) = saved_secret {
            std::env::set_var("AWS_SECRET_ACCESS_KEY", s);
        }
    }

    #[test]
    fn bedrock_from_env_builds_real_provider() {
        // from_env is pure config parsing: no network involved.
        let saved_key = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let saved_secret = std::env::var("AWS_SECRET_ACCESS_KEY").ok();
        let saved_region = std::env::var("AWS_REGION").ok();
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "secret");
        std::env::set_var("AWS_REGION", "eu-west-1");

        let p = BedrockProvider::from_env("anthropic.claude-3-7-sonnet-20250219-v1:0")
            .expect("from_env should succeed");
        assert_eq!(p.region, "eu-west-1");

        // And build_provider must route to it instead of the old gap placeholder.
        let cfg = GatewayConfig::default();
        let p = build_provider(&cfg, "bedrock:anthropic.claude-3-7-sonnet-20250219-v1:0");
        let rt = tokio::runtime::Runtime::new().unwrap();
        // The real provider attempts a signed HTTPS request; without network it
        // fails with a Bedrock transport error — never "not implemented".
        let err = rt
            .block_on(async { p.chat_completion(&[], &[], None, None, None, None).await })
            .expect_err("no network in test");
        let s = err.to_string();
        assert!(
            s.contains("Bedrock") && !s.contains("not implemented"),
            "unexpected: {s}"
        );

        match saved_key {
            Some(k) => std::env::set_var("AWS_ACCESS_KEY_ID", k),
            None => std::env::remove_var("AWS_ACCESS_KEY_ID"),
        }
        match saved_secret {
            Some(s) => std::env::set_var("AWS_SECRET_ACCESS_KEY", s),
            None => std::env::remove_var("AWS_SECRET_ACCESS_KEY"),
        }
        match saved_region {
            Some(r) => std::env::set_var("AWS_REGION", r),
            None => std::env::remove_var("AWS_REGION"),
        }
    }

    #[test]
    fn azure_target_parsing() {
        // Model-string form wins.
        let (r, d, m) = parse_azure_target("myres/mydep/gpt-4o").unwrap();
        assert_eq!(
            (r.as_str(), d.as_str(), m.as_str()),
            ("myres", "mydep", "gpt-4o")
        );
        // Deployment doubles as the model label when omitted.
        let (r, d, m) = parse_azure_target("myres/mydep").unwrap();
        assert_eq!(
            (r.as_str(), d.as_str(), m.as_str()),
            ("myres", "mydep", "mydep")
        );

        // Env fallback.
        let saved_r = std::env::var("AZURE_OPENAI_RESOURCE").ok();
        let saved_d = std::env::var("AZURE_OPENAI_DEPLOYMENT").ok();
        std::env::set_var("AZURE_OPENAI_RESOURCE", "envres");
        std::env::set_var("AZURE_OPENAI_DEPLOYMENT", "envdep");
        let (r, d, m) = parse_azure_target("").unwrap();
        assert_eq!(
            (r.as_str(), d.as_str(), m.as_str()),
            ("envres", "envdep", "envdep")
        );
        std::env::remove_var("AZURE_OPENAI_RESOURCE");
        std::env::remove_var("AZURE_OPENAI_DEPLOYMENT");
        assert!(parse_azure_target("").is_none());
        match saved_r {
            Some(v) => std::env::set_var("AZURE_OPENAI_RESOURCE", v),
            None => std::env::remove_var("AZURE_OPENAI_RESOURCE"),
        }
        match saved_d {
            Some(v) => std::env::set_var("AZURE_OPENAI_DEPLOYMENT", v),
            None => std::env::remove_var("AZURE_OPENAI_DEPLOYMENT"),
        }
    }

    #[test]
    fn provider_api_key_from_env_covers_new_providers() {
        let saved_g = std::env::var("GEMINI_API_KEY").ok();
        let saved_a = std::env::var("AZURE_OPENAI_API_KEY").ok();
        std::env::set_var("GEMINI_API_KEY", "g-key");
        std::env::set_var("AZURE_OPENAI_API_KEY", "a-key");
        assert_eq!(
            provider_api_key_from_env("gemini").as_deref(),
            Some("g-key")
        );
        assert_eq!(provider_api_key_from_env("azure").as_deref(), Some("a-key"));
        match saved_g {
            Some(v) => std::env::set_var("GEMINI_API_KEY", v),
            None => std::env::remove_var("GEMINI_API_KEY"),
        }
        match saved_a {
            Some(v) => std::env::set_var("AZURE_OPENAI_API_KEY", v),
            None => std::env::remove_var("AZURE_OPENAI_API_KEY"),
        }
    }
}
