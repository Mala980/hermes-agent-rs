//! `hermes gateway` handler, setup wizard, and adapter registration.

use super::auth::run_auth;
use super::common::{hermes_state_root, prompt_line};
use hermes_cli::cli::Cli;
#[cfg(test)]
use hermes_config::platform_token_or_extra;
use hermes_config::{
    gateway_pid_path_in, hermes_home, load_config, load_user_config_file, save_config_yaml,
    validate_config, PlatformConfig,
};
use hermes_core::AgentError;
#[cfg(test)]
use hermes_core::PlatformAdapter;
use hermes_gateway::Gateway;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

fn gateway_pid_path_for_cli(cli: &Cli) -> PathBuf {
    gateway_pid_path_in(hermes_state_root(cli))
}

pub(crate) fn serve_pid_path_for_cli(cli: &Cli) -> PathBuf {
    hermes_state_root(cli).join("serve.pid")
}

pub(crate) fn read_gateway_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(unix)]
pub(crate) fn gateway_pid_is_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
pub(crate) fn gateway_pid_is_alive(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
pub(crate) fn gateway_pid_terminate(pid: u32) -> std::io::Result<()> {
    let r = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
pub(crate) fn gateway_pid_terminate(_pid: u32) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "gateway stop is not supported on this platform",
    ))
}

/// Sidecar: forward cron completion events to every URL in `webhooks.json`.
async fn run_cron_webhook_delivery_loop(
    mut rx: tokio::sync::broadcast::Receiver<hermes_cron::CronCompletionEvent>,
    webhooks_json: PathBuf,
) {
    use tokio::sync::broadcast::error::RecvError;

    let client = match hermes_cli::webhook_delivery::webhook_http_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("cron webhooks: HTTP client build failed: {e}");
            return;
        }
    };

    loop {
        let ev = match rx.recv().await {
            Ok(ev) => ev,
            Err(RecvError::Lagged(n)) => {
                tracing::debug!(n, "cron webhook receiver lagged; skipped messages");
                continue;
            }
            Err(RecvError::Closed) => break,
        };

        if let Err(e) = hermes_cli::webhook_delivery::deliver_cron_completion_to_webhooks(
            &webhooks_json,
            &ev,
            &client,
        )
        .await
        {
            tracing::warn!("cron webhook delivery: {e}");
        }
    }
}

/// Handle `hermes gateway [action]`.
pub(crate) async fn run_gateway(
    cli: Cli,
    action: Option<String>,
    readonly: bool,
    yes: bool,
) -> Result<(), AgentError> {
    let config =
        load_config(cli.config_dir.as_deref()).map_err(|e| AgentError::Config(e.to_string()))?;

    match action.as_deref() {
        Some("setup") => {
            run_gateway_setup(&cli).await?;
        }
        None | Some("start") => {
            println!("Starting Hermes Gateway...");

            // List enabled platforms
            let enabled: Vec<&String> = config
                .platforms
                .iter()
                .filter(|(_, pc)| pc.enabled)
                .map(|(name, _)| name)
                .collect();

            if enabled.is_empty() {
                println!(
                    "Note: no chat platforms enabled in config.yaml — gateway still runs cron + HTTP webhooks."
                );
            }
            let requirement_issues = hermes_gateway::gateway_requirement_issues(&config);
            if !requirement_issues.is_empty() {
                let mut msg = String::from("Gateway requirement check failed:\n");
                for issue in requirement_issues {
                    msg.push_str("  - ");
                    msg.push_str(&issue);
                    msg.push('\n');
                }
                return Err(AgentError::Config(msg));
            }

            let pid_path = gateway_pid_path_for_cli(&cli);
            if let Some(pid) = read_gateway_pid(&pid_path) {
                if gateway_pid_is_alive(pid) {
                    println!(
                        "Gateway already appears to be running (PID {}, file {}). Stop it first or remove a stale PID file.",
                        pid,
                        pid_path.display()
                    );
                    return Ok(());
                }
                let _ = std::fs::remove_file(&pid_path);
            }

            if !enabled.is_empty() {
                println!(
                    "Enabled platforms: {}",
                    enabled
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }

            // Build the gateway runtime (session manager, DM policy, hooks).
            // DmManager is built from platform configs: per-platform
            // unauthorized_dm_behavior, admin_users, and allowed_users
            // (pre-authorized, since the operator explicitly vetted them).
            let session_manager =
                Arc::new(hermes_gateway::SessionManager::new(config.session.clone()));
            let dm_manager = hermes_gateway::DmManager::from_platform_configs(&config.platforms);
            let gateway = Arc::new(Gateway::new(
                session_manager,
                dm_manager,
                hermes_gateway::gateway::GatewayConfig::default(),
            ));
            let mut hook_registry = hermes_gateway::hooks::HookRegistry::new();
            hook_registry.register_builtins();
            hook_registry.discover_and_load(&hermes_home().join("hooks"));
            hook_registry.set_execution_limits(Some(16));
            gateway.set_hook_registry(Arc::new(hook_registry)).await;
            let enabled_refs: Vec<&str> = enabled.iter().map(|s| s.as_str()).collect();
            gateway
                .emit_hook_event(
                    "gateway:startup",
                    hermes_gateway::hook_payloads::gateway_startup(&enabled_refs),
                )
                .await;

            // Tool registry shared by the gateway agent and the cron runner.
            let tool_registry = Arc::new(hermes_tools::ToolRegistry::new());
            let terminal_backend: Arc<dyn hermes_core::TerminalBackend> =
                Arc::new(hermes_environments::LocalBackend::default());
            let skill_store = Arc::new(hermes_skills::FileSkillStore::new(
                hermes_skills::FileSkillStore::default_dir(),
            ));
            let skill_provider: Arc<dyn hermes_core::SkillProvider> =
                Arc::new(hermes_skills::SkillManager::new(skill_store));
            // Unattended approval: explicit CLI flags beat the env default.
            let approval_mode = hermes_tools::ApprovalMode::from_flags(readonly, yes);
            if readonly {
                println!("Gateway running in --readonly mode: confirmation-required commands are denied.");
            }
            hermes_tools::register_builtin_tools_with_approval_mode(
                &tool_registry,
                terminal_backend,
                skill_provider,
                approval_mode,
            );
            let agent_tools = Arc::new(hermes_agent::agent_builder::bridge_tool_registry(
                &tool_registry,
            ));

            // Incoming chat messages run through the agent loop.
            let config_arc = Arc::new(config.clone());
            let handler_tools = agent_tools.clone();
            gateway
                .set_message_handler_with_context(Arc::new(move |messages, ctx| {
                    let config = config_arc.clone();
                    let agent_tools = handler_tools.clone();
                    Box::pin(async move {
                        let model = ctx
                            .model
                            .clone()
                            .or_else(|| config.model.clone())
                            .unwrap_or_else(|| "gpt-4o".to_string());
                        let provider = hermes_agent::agent_builder::build_provider(&config, &model);
                        let mut agent_config = hermes_agent::AgentConfig::default();
                        agent_config.model = model;
                        let schemas = agent_tools.schemas();
                        let agent_loop =
                            hermes_agent::AgentLoop::new(agent_config, agent_tools, provider);
                        let result = agent_loop
                            .run(messages, Some(schemas))
                            .await
                            .map_err(|e| hermes_core::GatewayError::Platform(e.to_string()))?;
                        let reply = result
                            .messages
                            .iter()
                            .rev()
                            .find(|m| matches!(m.role, hermes_core::MessageRole::Assistant))
                            .and_then(|m| m.content.clone())
                            .unwrap_or_default();
                        Ok(reply)
                    })
                }))
                .await;

            // Cron: same on-disk dir as `hermes cron`, with the gateway's LLM/tools.
            let cron_dir = hermes_state_root(&cli).join("cron");
            std::fs::create_dir_all(&cron_dir)
                .map_err(|e| AgentError::Io(format!("cron dir {}: {}", cron_dir.display(), e)))?;
            let default_model = config.model.clone().unwrap_or_else(|| "gpt-4o".to_string());
            let cron_persistence =
                Arc::new(hermes_cron::FileJobPersistence::with_dir(cron_dir.clone()));
            let cron_llm = hermes_agent::agent_builder::build_provider(&config, &default_model);
            let cron_runner = Arc::new(hermes_cron::CronRunner::new(cron_llm, agent_tools.clone()));
            let mut cron_scheduler = hermes_cron::CronScheduler::new(cron_persistence, cron_runner);
            let (cron_tx, cron_rx) =
                tokio::sync::broadcast::channel::<hermes_cron::CronCompletionEvent>(64);
            cron_scheduler.set_completion_broadcast(cron_tx);
            cron_scheduler
                .load_persisted_jobs()
                .await
                .map_err(|e| AgentError::Config(format!("cron load: {e}")))?;
            cron_scheduler.start().await;
            let cron_scheduler = Arc::new(cron_scheduler);
            let webhooks_path = hermes_state_root(&cli).join("webhooks.json");
            println!(
                "Cron jobs: {}  |  Webhook registry: {}",
                cron_dir.display(),
                webhooks_path.display()
            );

            let mut sidecar_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
            sidecar_tasks.push(tokio::spawn(run_cron_webhook_delivery_loop(
                cron_rx,
                webhooks_path,
            )));

            register_gateway_adapters(&config, gateway.clone(), &mut sidecar_tasks).await?;

            if gateway.adapter_names().await.is_empty() {
                if enabled.is_empty() {
                    println!("No chat adapters enabled; cron + webhooks still active.");
                } else {
                    return Err(AgentError::Config(
                        "Gateway startup failed: platforms are enabled but no adapters registered."
                            .to_string(),
                    ));
                }
            }

            gateway.start_all().await?;
            {
                let gw_reconnect = gateway.clone();
                sidecar_tasks.push(tokio::spawn(async move {
                    gw_reconnect.platform_reconnect_watcher(20).await;
                }));
                let gw_expiry = gateway.clone();
                sidecar_tasks.push(tokio::spawn(async move {
                    gw_expiry.session_expiry_watcher(300).await;
                }));
                let gw_cleanup = gateway.clone();
                sidecar_tasks.push(tokio::spawn(async move {
                    gw_cleanup.gateway_cleanup_watcher(300).await;
                }));
            }

            let own_pid = std::process::id();
            std::fs::write(&pid_path, format!("{own_pid}\n")).map_err(|e| {
                AgentError::Io(format!("failed to write {}: {}", pid_path.display(), e))
            })?;
            println!("Gateway is ready. Press Ctrl+C to stop.");
            // Keep the gateway alive until Ctrl+C / SIGINT.
            tokio::signal::ctrl_c()
                .await
                .map_err(|e| AgentError::Io(format!("Failed to listen for Ctrl+C: {e}")))?;

            println!("\nShutting down gateway...");
            cron_scheduler.stop().await;
            gateway.stop_all().await?;
            let _ = std::fs::remove_file(&pid_path);
            for task in sidecar_tasks {
                task.abort();
            }
            println!("Gateway stopped.");
        }
        Some("status") => {
            let pid_path = gateway_pid_path_for_cli(&cli);
            match std::fs::read_to_string(&pid_path) {
                Ok(raw) => match raw.trim().parse::<u32>() {
                    Ok(pid) if gateway_pid_is_alive(pid) => {
                        println!(
                            "Gateway status: running (PID {}, file {})",
                            pid,
                            pid_path.display()
                        );
                    }
                    Ok(pid) => {
                        println!(
                            "Gateway status: not running (stale PID {} in {})",
                            pid,
                            pid_path.display()
                        );
                    }
                    Err(_) => {
                        println!("Gateway status: invalid PID file at {}", pid_path.display());
                    }
                },
                Err(_) => {
                    println!("Gateway status: not running (no PID file; start with `hermes gateway start`)");
                }
            }
        }
        Some("stop") => {
            let pid_path = gateway_pid_path_for_cli(&cli);
            let Some(pid) = read_gateway_pid(&pid_path) else {
                println!("Gateway stop: no PID file (nothing to stop).");
                return Ok(());
            };
            if !gateway_pid_is_alive(pid) {
                let _ = std::fs::remove_file(&pid_path);
                println!(
                    "Gateway stop: process {} not running; removed stale PID file {}.",
                    pid,
                    pid_path.display()
                );
                return Ok(());
            }
            match gateway_pid_terminate(pid) {
                Ok(()) => {
                    println!("Sent SIGTERM to gateway PID {}.", pid);
                    let _ = std::fs::remove_file(&pid_path);
                    println!("Removed {}.", pid_path.display());
                }
                Err(e) => println!("Gateway stop: failed to signal PID {}: {}", pid, e),
            }
        }
        Some(other) => {
            println!(
                "Unknown gateway action: {}. Use 'start', 'stop', or 'status'.",
                other
            );
        }
    }
    Ok(())
}

async fn prompt_yes_no(question: &str, default_yes: bool) -> Result<bool, AgentError> {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    let ans = prompt_line(format!("{question} {hint}: ")).await?;
    if ans.trim().is_empty() {
        return Ok(default_yes);
    }
    let v = ans.trim().to_ascii_lowercase();
    Ok(matches!(v.as_str(), "y" | "yes" | "1" | "true" | "on"))
}

fn parse_csv_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn normalize_gateway_platform_key(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "telegram" | "tg" => Some("telegram"),
        "weixin" | "wechat" | "wx" => Some("weixin"),
        "qq" | "qqbot" => Some("qqbot"),
        "discord" => Some("discord"),
        "slack" => Some("slack"),
        "matrix" => Some("matrix"),
        "mattermost" | "mm" => Some("mattermost"),
        "signal" => Some("signal"),
        "whatsapp" | "wa" => Some("whatsapp"),
        "dingtalk" => Some("dingtalk"),
        "feishu" | "lark" => Some("feishu"),
        "wecom" => Some("wecom"),
        "wecom_callback" | "wecom-callback" => Some("wecom_callback"),
        "bluebubbles" | "imessage" => Some("bluebubbles"),
        "email" => Some("email"),
        "sms" => Some("sms"),
        "homeassistant" | "ha" => Some("homeassistant"),
        "webhook" => Some("webhook"),
        "api_server" | "api-server" | "api" => Some("api_server"),
        _ => None,
    }
}

fn enabled_flag(platform: Option<&PlatformConfig>) -> &'static str {
    if platform.map(|p| p.enabled).unwrap_or(false) {
        "enabled"
    } else {
        "disabled"
    }
}

fn set_extra_string_if_nonempty(platform: &mut PlatformConfig, key: &str, value: &str) {
    let v = value.trim();
    if !v.is_empty() {
        platform
            .extra
            .insert(key.to_string(), serde_json::Value::String(v.to_string()));
    }
}

pub(crate) async fn configure_platform_basic_prompts(
    disk: &mut hermes_config::GatewayConfig,
    key: &str,
) -> Result<(), AgentError> {
    let p = disk
        .platforms
        .entry(key.to_string())
        .or_insert_with(PlatformConfig::default);
    p.enabled = true;

    match key {
        "discord" => {
            let token = prompt_line("Discord bot token: ").await?;
            if !token.trim().is_empty() {
                p.token = Some(token.trim().to_string());
            }
            let app_id = prompt_line("Discord application_id (optional): ").await?;
            set_extra_string_if_nonempty(p, "application_id", &app_id);
            let allowed =
                prompt_line("Discord allowed users (comma-separated, optional): ").await?;
            if !allowed.trim().is_empty() {
                p.allowed_users = parse_csv_list(&allowed);
            }
            let home = prompt_line("Discord home channel (optional): ").await?;
            if !home.trim().is_empty() {
                p.home_channel = Some(home.trim().to_string());
            }
        }
        "slack" => {
            let token = prompt_line("Slack bot token (xoxb-...): ").await?;
            if !token.trim().is_empty() {
                p.token = Some(token.trim().to_string());
            }
            let app_token = prompt_line("Slack app token (xapp-..., optional): ").await?;
            set_extra_string_if_nonempty(p, "app_token", &app_token);
            let socket_mode = prompt_yes_no("Slack use socket_mode?", true).await?;
            p.extra.insert(
                "socket_mode".to_string(),
                serde_json::Value::Bool(socket_mode),
            );
        }
        "matrix" => {
            let homeserver =
                prompt_line("Matrix homeserver_url (e.g. https://matrix.org): ").await?;
            set_extra_string_if_nonempty(p, "homeserver_url", &homeserver);
            let user_id = prompt_line("Matrix user_id (e.g. @bot:matrix.org): ").await?;
            set_extra_string_if_nonempty(p, "user_id", &user_id);
            let token = prompt_line("Matrix access token: ").await?;
            if !token.trim().is_empty() {
                p.token = Some(token.trim().to_string());
            }
            let room = prompt_line("Matrix home room_id (optional): ").await?;
            set_extra_string_if_nonempty(p, "room_id", &room);
        }
        "mattermost" => {
            let server_url = prompt_line("Mattermost server_url: ").await?;
            set_extra_string_if_nonempty(p, "server_url", &server_url);
            let token = prompt_line("Mattermost bot token: ").await?;
            if !token.trim().is_empty() {
                p.token = Some(token.trim().to_string());
            }
            let team_id = prompt_line("Mattermost team_id (optional): ").await?;
            set_extra_string_if_nonempty(p, "team_id", &team_id);
            let home = prompt_line("Mattermost home channel (optional): ").await?;
            if !home.trim().is_empty() {
                p.home_channel = Some(home.trim().to_string());
            }
        }
        "signal" => {
            let account = prompt_line("Signal phone_number/account (e.g. +15551234567): ").await?;
            set_extra_string_if_nonempty(p, "phone_number", &account);
            let api_url = prompt_line("Signal api_url (default http://localhost:8080): ").await?;
            set_extra_string_if_nonempty(p, "api_url", &api_url);
        }
        "whatsapp" => {
            let token = prompt_line("WhatsApp Cloud API token: ").await?;
            if !token.trim().is_empty() {
                p.token = Some(token.trim().to_string());
            }
            let phone_id = prompt_line("WhatsApp phone_number_id: ").await?;
            set_extra_string_if_nonempty(p, "phone_number_id", &phone_id);
            let verify = prompt_line("WhatsApp verify_token (optional): ").await?;
            set_extra_string_if_nonempty(p, "verify_token", &verify);
            let home = prompt_line("WhatsApp home channel (optional): ").await?;
            if !home.trim().is_empty() {
                p.home_channel = Some(home.trim().to_string());
            }
        }
        "dingtalk" => {
            let client_id = prompt_line("DingTalk client_id/appkey: ").await?;
            set_extra_string_if_nonempty(p, "client_id", &client_id);
            let client_secret = prompt_line("DingTalk client_secret: ").await?;
            set_extra_string_if_nonempty(p, "client_secret", &client_secret);
        }
        "feishu" => {
            let app_id = prompt_line("Feishu/Lark app_id: ").await?;
            set_extra_string_if_nonempty(p, "app_id", &app_id);
            let app_secret = prompt_line("Feishu/Lark app_secret: ").await?;
            set_extra_string_if_nonempty(p, "app_secret", &app_secret);
            let verify = prompt_line("Feishu verification_token (optional): ").await?;
            set_extra_string_if_nonempty(p, "verification_token", &verify);
            let encrypt_key = prompt_line("Feishu encrypt_key (optional): ").await?;
            set_extra_string_if_nonempty(p, "encrypt_key", &encrypt_key);
        }
        "wecom" => {
            let corp_id = prompt_line("WeCom corp_id: ").await?;
            set_extra_string_if_nonempty(p, "corp_id", &corp_id);
            let agent_id = prompt_line("WeCom agent_id: ").await?;
            set_extra_string_if_nonempty(p, "agent_id", &agent_id);
            let secret = prompt_line("WeCom secret: ").await?;
            set_extra_string_if_nonempty(p, "secret", &secret);
        }
        "wecom_callback" => {
            let corp_id = prompt_line("WeCom callback corp_id: ").await?;
            set_extra_string_if_nonempty(p, "corp_id", &corp_id);
            let corp_secret = prompt_line("WeCom callback corp_secret: ").await?;
            set_extra_string_if_nonempty(p, "corp_secret", &corp_secret);
            let agent_id = prompt_line("WeCom callback agent_id: ").await?;
            set_extra_string_if_nonempty(p, "agent_id", &agent_id);
            let token = prompt_line("WeCom callback token: ").await?;
            set_extra_string_if_nonempty(p, "token", &token);
            let aes = prompt_line("WeCom callback encoding_aes_key: ").await?;
            set_extra_string_if_nonempty(p, "encoding_aes_key", &aes);
            let host = prompt_line("WeCom callback host (default 0.0.0.0): ").await?;
            set_extra_string_if_nonempty(p, "host", &host);
            let port = prompt_line("WeCom callback port (default 8645): ").await?;
            if let Ok(v) = port.trim().parse::<u16>() {
                p.extra
                    .insert("port".to_string(), serde_json::Value::from(v));
            }
            let path = prompt_line("WeCom callback path (default /wecom/callback): ").await?;
            set_extra_string_if_nonempty(p, "path", &path);
        }
        "qqbot" => {
            let app_id = prompt_line("QQBot app_id: ").await?;
            set_extra_string_if_nonempty(p, "app_id", &app_id);
            let secret = prompt_line("QQBot client_secret: ").await?;
            set_extra_string_if_nonempty(p, "client_secret", &secret);
            let markdown = prompt_yes_no("QQBot markdown_support?", true).await?;
            p.extra.insert(
                "markdown_support".to_string(),
                serde_json::Value::Bool(markdown),
            );
        }
        "bluebubbles" => {
            let server_url = prompt_line("BlueBubbles server_url: ").await?;
            set_extra_string_if_nonempty(p, "server_url", &server_url);
            let password = prompt_line("BlueBubbles password: ").await?;
            set_extra_string_if_nonempty(p, "password", &password);
        }
        "email" => {
            let username = prompt_line("Email username/address: ").await?;
            set_extra_string_if_nonempty(p, "username", &username);
            let password = prompt_line("Email password/app password: ").await?;
            set_extra_string_if_nonempty(p, "password", &password);
            let imap_host = prompt_line("Email imap_host: ").await?;
            set_extra_string_if_nonempty(p, "imap_host", &imap_host);
            let smtp_host = prompt_line("Email smtp_host: ").await?;
            set_extra_string_if_nonempty(p, "smtp_host", &smtp_host);
            let imap_port = prompt_line("Email imap_port (default 993): ").await?;
            if let Ok(v) = imap_port.trim().parse::<u16>() {
                p.extra
                    .insert("imap_port".to_string(), serde_json::Value::from(v));
            }
            let smtp_port = prompt_line("Email smtp_port (default 587): ").await?;
            if let Ok(v) = smtp_port.trim().parse::<u16>() {
                p.extra
                    .insert("smtp_port".to_string(), serde_json::Value::from(v));
            }
        }
        "sms" => {
            let sid = prompt_line("Twilio account_sid: ").await?;
            set_extra_string_if_nonempty(p, "account_sid", &sid);
            let auth = prompt_line("Twilio auth_token: ").await?;
            set_extra_string_if_nonempty(p, "auth_token", &auth);
            let from = prompt_line("Twilio from_number (E.164): ").await?;
            set_extra_string_if_nonempty(p, "from_number", &from);
        }
        "homeassistant" => {
            let base_url =
                prompt_line("HomeAssistant base_url (e.g. http://127.0.0.1:8123): ").await?;
            set_extra_string_if_nonempty(p, "base_url", &base_url);
            let token = prompt_line("HomeAssistant long_lived_token: ").await?;
            if !token.trim().is_empty() {
                p.token = Some(token.trim().to_string());
            }
            let webhook_id = prompt_line("HomeAssistant webhook_id (optional): ").await?;
            set_extra_string_if_nonempty(p, "webhook_id", &webhook_id);
        }
        "webhook" => {
            let secret = prompt_line("Webhook secret: ").await?;
            set_extra_string_if_nonempty(p, "secret", &secret);
            let port = prompt_line("Webhook port (default 9000): ").await?;
            if let Ok(v) = port.trim().parse::<u16>() {
                p.extra
                    .insert("port".to_string(), serde_json::Value::from(v));
            }
            let path = prompt_line("Webhook path (default /webhook): ").await?;
            set_extra_string_if_nonempty(p, "path", &path);
        }
        "api_server" => {
            let host = prompt_line("API server host (default 0.0.0.0): ").await?;
            set_extra_string_if_nonempty(p, "host", &host);
            let port = prompt_line("API server port (default 8090): ").await?;
            if let Ok(v) = port.trim().parse::<u16>() {
                p.extra
                    .insert("port".to_string(), serde_json::Value::from(v));
            }
            let token = prompt_line("API server auth_token (optional): ").await?;
            set_extra_string_if_nonempty(p, "auth_token", &token);
        }
        _ => {}
    }
    Ok(())
}

async fn run_gateway_setup(cli: &Cli) -> Result<(), AgentError> {
    println!("Gateway setup wizard");
    println!("--------------------");
    let cfg_path = hermes_state_root(cli).join("config.yaml");
    let mut disk =
        load_user_config_file(&cfg_path).map_err(|e| AgentError::Config(e.to_string()))?;
    println!("This wizard configures messaging platforms in config.yaml.");
    println!("Current platform status:");
    for (k, label) in [
        ("weixin", "Weixin"),
        ("qqbot", "QQBot"),
        ("telegram", "Telegram"),
        ("discord", "Discord"),
        ("slack", "Slack"),
        ("matrix", "Matrix"),
        ("mattermost", "Mattermost"),
        ("whatsapp", "WhatsApp"),
        ("signal", "Signal"),
        ("dingtalk", "DingTalk"),
        ("feishu", "Feishu"),
        ("wecom", "WeCom"),
        ("wecom_callback", "WeCom Callback"),
        ("bluebubbles", "BlueBubbles"),
        ("email", "Email"),
        ("sms", "SMS"),
        ("homeassistant", "HomeAssistant"),
        ("webhook", "Webhook"),
        ("api_server", "API Server"),
    ] {
        println!("  - {:<13} {}", label, enabled_flag(disk.platforms.get(k)));
    }
    println!();
    println!("Examples: weixin,telegram   or   discord,slack,matrix");
    let raw = prompt_line(
        "Platforms to configure (comma-separated, empty defaults to weixin,telegram): ",
    )
    .await?;
    let mut selected: Vec<String> = if raw.trim().is_empty() {
        vec!["weixin".to_string(), "telegram".to_string()]
    } else {
        parse_csv_list(&raw)
            .into_iter()
            .filter_map(|k| normalize_gateway_platform_key(&k).map(|v| v.to_string()))
            .collect()
    };
    selected.sort();
    selected.dedup();
    if selected.is_empty() {
        println!("No valid platforms selected.");
        return Ok(());
    }

    for key in selected {
        println!();
        println!("Configuring {}...", key);
        match key.as_str() {
            "weixin" => {
                run_auth(
                    cli.clone(),
                    Some("login".to_string()),
                    Some("weixin".to_string()),
                    true,
                )
                .await?;
                disk = load_user_config_file(&cfg_path)
                    .map_err(|e| AgentError::Config(e.to_string()))?;
                let wx = disk
                    .platforms
                    .entry("weixin".to_string())
                    .or_insert_with(PlatformConfig::default);
                wx.enabled = true;
                println!("Direct message policy: 1)pairing 2)open 3)allowlist 4)disabled");
                let dm_choice = prompt_line("Choose [1-4] (default 1): ").await?;
                match dm_choice.trim() {
                    "2" => {
                        wx.extra
                            .insert("dm_policy".to_string(), serde_json::json!("open"));
                        wx.extra
                            .insert("allow_from".to_string(), serde_json::json!([]));
                    }
                    "3" => {
                        let ids = parse_csv_list(
                            &prompt_line("Allowed Weixin user IDs (comma-separated): ").await?,
                        );
                        wx.extra
                            .insert("dm_policy".to_string(), serde_json::json!("allowlist"));
                        wx.extra.insert(
                            "allow_from".to_string(),
                            serde_json::Value::Array(
                                ids.into_iter().map(serde_json::Value::String).collect(),
                            ),
                        );
                    }
                    "4" => {
                        wx.extra
                            .insert("dm_policy".to_string(), serde_json::json!("disabled"));
                        wx.extra
                            .insert("allow_from".to_string(), serde_json::json!([]));
                    }
                    _ => {
                        wx.extra
                            .insert("dm_policy".to_string(), serde_json::json!("pairing"));
                        wx.extra
                            .insert("allow_from".to_string(), serde_json::json!([]));
                    }
                }
                println!("Group policy: 1)disabled 2)open 3)allowlist");
                let group_choice = prompt_line("Choose [1-3] (default 1): ").await?;
                match group_choice.trim() {
                    "2" => {
                        wx.extra
                            .insert("group_policy".to_string(), serde_json::json!("open"));
                        wx.extra
                            .insert("group_allow_from".to_string(), serde_json::json!([]));
                    }
                    "3" => {
                        let ids = parse_csv_list(
                            &prompt_line("Allowed Weixin group IDs (comma-separated): ").await?,
                        );
                        wx.extra
                            .insert("group_policy".to_string(), serde_json::json!("allowlist"));
                        wx.extra.insert(
                            "group_allow_from".to_string(),
                            serde_json::Value::Array(
                                ids.into_iter().map(serde_json::Value::String).collect(),
                            ),
                        );
                    }
                    _ => {
                        wx.extra
                            .insert("group_policy".to_string(), serde_json::json!("disabled"));
                        wx.extra
                            .insert("group_allow_from".to_string(), serde_json::json!([]));
                    }
                }
                let home = prompt_line("Weixin home channel (optional): ").await?;
                if !home.trim().is_empty() {
                    wx.home_channel = Some(home.trim().to_string());
                }
            }
            "telegram" => {
                run_auth(
                    cli.clone(),
                    Some("login".to_string()),
                    Some("telegram".to_string()),
                    false,
                )
                .await?;
                disk = load_user_config_file(&cfg_path)
                    .map_err(|e| AgentError::Config(e.to_string()))?;
                let tg = disk
                    .platforms
                    .entry("telegram".to_string())
                    .or_insert_with(PlatformConfig::default);
                tg.enabled = true;
                let polling = prompt_yes_no("Telegram use polling mode?", true).await?;
                tg.extra
                    .insert("polling".to_string(), serde_json::Value::Bool(polling));
                if !polling {
                    let webhook_url = prompt_line("Telegram webhook URL: ").await?;
                    if !webhook_url.trim().is_empty() {
                        tg.webhook_url = Some(webhook_url.trim().to_string());
                    }
                }
                let home = prompt_line("Telegram home channel (optional): ").await?;
                if !home.trim().is_empty() {
                    tg.home_channel = Some(home.trim().to_string());
                }
                let allowed = prompt_line(
                    "Allowed Telegram user IDs (comma-separated, blank = use env / deny-all): ",
                )
                .await?;
                if !allowed.trim().is_empty() {
                    tg.allowed_users = parse_csv_list(&allowed);
                }
            }
            other => configure_platform_basic_prompts(&mut disk, other).await?,
        }
    }

    validate_config(&disk).map_err(|e| AgentError::Config(e.to_string()))?;
    save_config_yaml(&cfg_path, &disk).map_err(|e| AgentError::Config(e.to_string()))?;

    println!();
    println!("Gateway setup complete.");
    println!("Config saved: {}", cfg_path.display());
    println!("Next step: `hermes gateway start`");
    Ok(())
}

pub(crate) async fn register_gateway_adapters(
    config: &hermes_config::GatewayConfig,
    gateway: Arc<Gateway>,
    sidecar_tasks: &mut Vec<tokio::task::JoinHandle<()>>,
) -> Result<hermes_gateway::platform_registry::RegistrationSummary, AgentError> {
    let summary =
        hermes_gateway::platform_registry::register_platforms(&gateway, config, sidecar_tasks)
            .await?;

    #[cfg(test)]
    {
        use async_trait::async_trait;
        use hermes_core::{GatewayError, ParseMode};

        struct NoopAdapter {
            name: &'static str,
        }

        #[async_trait]
        impl PlatformAdapter for NoopAdapter {
            async fn start(&self) -> Result<(), GatewayError> {
                Ok(())
            }
            async fn stop(&self) -> Result<(), GatewayError> {
                Ok(())
            }
            async fn send_message(
                &self,
                _chat_id: &str,
                _text: &str,
                _parse_mode: Option<ParseMode>,
            ) -> Result<(), GatewayError> {
                Ok(())
            }
            async fn edit_message(
                &self,
                _chat_id: &str,
                _message_id: &str,
                _text: &str,
            ) -> Result<(), GatewayError> {
                Ok(())
            }
            async fn send_file(
                &self,
                _chat_id: &str,
                _file_path: &str,
                _caption: Option<&str>,
            ) -> Result<(), GatewayError> {
                Ok(())
            }
            fn is_running(&self) -> bool {
                true
            }
            fn platform_name(&self) -> &str {
                self.name
            }
        }

        if let Some(qqbot) = config.platforms.get("qqbot") {
            let ready = qqbot.enabled
                && hermes_config::extra_string(qqbot, "app_id").is_some()
                && hermes_config::extra_string(qqbot, "client_secret").is_some();
            if ready && !summary.registered.iter().any(|n| n == "qqbot") {
                gateway
                    .register_adapter("qqbot", Arc::new(NoopAdapter { name: "qqbot" }))
                    .await;
            }
        }

        if let Some(wecom_cb) = config.platforms.get("wecom_callback") {
            let ready = wecom_cb.enabled
                && hermes_config::extra_string(wecom_cb, "corp_id").is_some()
                && hermes_config::extra_string(wecom_cb, "corp_secret").is_some()
                && hermes_config::extra_string(wecom_cb, "agent_id").is_some()
                && platform_token_or_extra(wecom_cb).is_some()
                && hermes_config::extra_string(wecom_cb, "encoding_aes_key").is_some();
            if ready && !summary.registered.iter().any(|n| n == "wecom_callback") {
                gateway
                    .register_adapter(
                        "wecom_callback",
                        Arc::new(NoopAdapter {
                            name: "wecom_callback",
                        }),
                    )
                    .await;
            }
        }
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_config::session::SessionConfig;
    use hermes_config::PlatformConfig;
    use hermes_gateway::dm::DmManager;
    use hermes_gateway::{Gateway, SessionManager};

    fn make_platform(enabled: bool, token: Option<&str>) -> PlatformConfig {
        let mut cfg = PlatformConfig {
            enabled,
            ..Default::default()
        };
        if let Some(t) = token {
            cfg.token = Some(t.to_string());
        }
        cfg
    }

    fn make_gateway() -> Arc<Gateway> {
        Arc::new(Gateway::new(
            Arc::new(SessionManager::new(SessionConfig::default())),
            DmManager::with_pair_behavior(),
            hermes_gateway::gateway::GatewayConfig::default(),
        ))
    }

    #[test]
    fn parse_csv_list_parses_allowed_user_ids() {
        // Wizard parsing for telegram `allowed_users`: trims, drops empties.
        assert_eq!(
            parse_csv_list("123, 456 ,789"),
            vec!["123", "456", "789"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
        assert!(parse_csv_list("").is_empty());
        assert!(parse_csv_list("  , , ").is_empty());
    }

    #[test]
    fn gateway_requirement_check_flags_missing_required_fields() {
        let mut config = hermes_config::GatewayConfig::default();
        config
            .platforms
            .insert("telegram".to_string(), make_platform(true, None));
        config
            .platforms
            .insert("qqbot".to_string(), make_platform(true, None));
        let issues = hermes_gateway::gateway_requirement_issues(&config);
        assert!(issues.iter().any(|s| s.contains("telegram")));
        assert!(issues.iter().any(|s| s.contains("qqbot")));
    }

    #[test]
    fn gateway_requirement_check_accepts_complete_qqbot_and_wecom_callback() {
        let mut config = hermes_config::GatewayConfig::default();

        let mut qqbot = make_platform(true, None);
        qqbot
            .extra
            .insert("app_id".to_string(), serde_json::json!("qq-app"));
        qqbot
            .extra
            .insert("client_secret".to_string(), serde_json::json!("qq-secret"));
        config.platforms.insert("qqbot".to_string(), qqbot);

        let mut wecom_cb = make_platform(true, Some("cb-token"));
        wecom_cb
            .extra
            .insert("corp_id".to_string(), serde_json::json!("wwcorp"));
        wecom_cb
            .extra
            .insert("corp_secret".to_string(), serde_json::json!("corp-secret"));
        wecom_cb
            .extra
            .insert("agent_id".to_string(), serde_json::json!("1000002"));
        wecom_cb.extra.insert(
            "encoding_aes_key".to_string(),
            serde_json::json!("abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG"),
        );
        config
            .platforms
            .insert("wecom_callback".to_string(), wecom_cb);

        assert!(hermes_gateway::gateway_requirement_issues(&config).is_empty());
    }

    #[tokio::test]
    async fn register_gateway_adapters_registers_primary_platforms_when_config_is_complete() {
        let mut config = hermes_config::GatewayConfig::default();

        let mut telegram = make_platform(true, Some("tg-token"));
        telegram
            .extra
            .insert("polling".to_string(), serde_json::json!(false));
        config.platforms.insert("telegram".to_string(), telegram);

        let mut weixin = make_platform(true, Some("wx-token"));
        weixin
            .extra
            .insert("account_id".to_string(), serde_json::json!("wxid_abc"));
        config.platforms.insert("weixin".to_string(), weixin);

        config.platforms.insert(
            "discord".to_string(),
            make_platform(true, Some("discord-token")),
        );
        config
            .platforms
            .insert("slack".to_string(), make_platform(true, Some("xoxb-slack")));

        let gateway = make_gateway();
        let mut sidecar_tasks = Vec::new();
        register_gateway_adapters(&config, gateway.clone(), &mut sidecar_tasks)
            .await
            .expect("primary platform registration should succeed");

        let mut names = gateway.adapter_names().await;
        names.sort();
        assert!(names.contains(&"telegram".to_string()));
        assert!(names.contains(&"weixin".to_string()));
        assert!(names.contains(&"discord".to_string()));
        assert!(names.contains(&"slack".to_string()));

        for task in sidecar_tasks {
            task.abort();
        }
    }

    #[tokio::test]
    async fn register_gateway_adapters_skips_primary_platforms_when_required_credentials_missing() {
        let mut config = hermes_config::GatewayConfig::default();
        config
            .platforms
            .insert("telegram".to_string(), make_platform(true, None));
        config
            .platforms
            .insert("weixin".to_string(), make_platform(true, None));
        config
            .platforms
            .insert("discord".to_string(), make_platform(true, None));
        config
            .platforms
            .insert("slack".to_string(), make_platform(true, None));

        let gateway = make_gateway();
        let mut sidecar_tasks = Vec::new();
        register_gateway_adapters(&config, gateway.clone(), &mut sidecar_tasks)
            .await
            .expect("missing credentials should be handled gracefully");

        assert!(
            gateway.adapter_names().await.is_empty(),
            "no primary adapter should register when required credentials are missing"
        );
        for task in sidecar_tasks {
            task.abort();
        }
    }

    #[tokio::test]
    async fn register_gateway_adapters_registers_qqbot_and_wecom_callback() {
        let mut config = hermes_config::GatewayConfig::default();

        let mut qqbot = make_platform(true, None);
        qqbot
            .extra
            .insert("app_id".to_string(), serde_json::json!("qq-app"));
        qqbot
            .extra
            .insert("client_secret".to_string(), serde_json::json!("qq-secret"));
        config.platforms.insert("qqbot".to_string(), qqbot);

        let mut wecom_cb = make_platform(true, None);
        wecom_cb
            .extra
            .insert("corp_id".to_string(), serde_json::json!("wwcorp"));
        wecom_cb
            .extra
            .insert("corp_secret".to_string(), serde_json::json!("corp-secret"));
        wecom_cb
            .extra
            .insert("agent_id".to_string(), serde_json::json!("1000002"));
        wecom_cb
            .extra
            .insert("token".to_string(), serde_json::json!("cb-token"));
        wecom_cb.extra.insert(
            "encoding_aes_key".to_string(),
            serde_json::json!("abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG"),
        );
        config
            .platforms
            .insert("wecom_callback".to_string(), wecom_cb);

        let gateway = make_gateway();
        let mut sidecar_tasks = Vec::new();
        register_gateway_adapters(&config, gateway.clone(), &mut sidecar_tasks)
            .await
            .expect("qqbot and wecom_callback should register");

        let names = gateway.adapter_names().await;
        assert!(names.contains(&"qqbot".to_string()));
        assert!(names.contains(&"wecom_callback".to_string()));
    }
}
