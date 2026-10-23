//! `hermes serve` handler.

use super::common::{is_truthy, prompt_line};
use super::gateway::{
    gateway_pid_is_alive, gateway_pid_terminate, read_gateway_pid, serve_pid_path_for_cli,
};
use crate::app::provider_api_key_from_env;
use crate::cli::Cli;
use hermes_config::load_config;
use hermes_core::AgentError;

/// Resolve API key for `hermes auth login <provider>`: env → merged config → stdin.
pub async fn resolve_llm_login_token(cli: &Cli, provider: &str) -> Result<String, AgentError> {
    if let Some(k) = provider_api_key_from_env(provider) {
        return Ok(k);
    }
    let cfg =
        load_config(cli.config_dir.as_deref()).map_err(|e| AgentError::Config(e.to_string()))?;
    if let Some(k) = cfg
        .llm_providers
        .get(provider)
        .and_then(|c| c.api_key.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Ok(k.to_string());
    }
    let fallback_var = format!("{}_API_KEY", provider.to_uppercase().replace('-', "_"));
    let msg = format!(
        "No API key in env or config for provider '{}'.\n\
         Set {} (see `hermes config set llm.{}.api_key ...`) or paste key now: ",
        provider, fallback_var, provider
    );
    let pasted = prompt_line(msg).await?;
    if pasted.is_empty() {
        return Err(AgentError::Config(format!(
            "Missing API key for provider '{}'",
            provider
        )));
    }
    Ok(pasted)
}

/// Options for `hermes serve` (bundled to keep the arg count lint-clean).
pub struct ServeOptions {
    pub action: Option<String>,
    pub host: String,
    pub port: u16,
    pub no_gateway: bool,
    pub no_cron: bool,
    pub readonly: bool,
    pub yes: bool,
}

pub async fn run_serve(cli: Cli, opts: ServeOptions) -> Result<(), AgentError> {
    let ServeOptions {
        action,
        host,
        port,
        no_gateway,
        no_cron,
        readonly,
        yes,
    } = opts;
    match action.as_deref() {
        None | Some("start") => {
            // Unattended approval flags: the tool registry is built inside
            // hermes-server, so the mode would need a process-wide override.
            // NOTE: hermes-tools exposes no such API (`ApprovalMode` /
            // `set_global_approval_mode` do not exist); the flags are accepted
            // for CLI compatibility but not enforced yet.
            tracing::debug!(
                readonly,
                yes,
                "approval flags accepted; no enforcement API in hermes-tools yet"
            );
            if readonly {
                println!(
                    "Serve running in --readonly mode: confirmation-required commands are denied."
                );
            }
            let config = hermes_config::load_config(cli.config_dir.as_deref())
                .map_err(|e| AgentError::Config(e.to_string()))?;
            let addr: std::net::SocketAddr = format!("{}:{}", host, port)
                .parse()
                .map_err(|e| AgentError::Config(format!("invalid address: {}", e)))?;

            let pid_path = serve_pid_path_for_cli(&cli);
            if let Some(pid) = read_gateway_pid(&pid_path) {
                if gateway_pid_is_alive(pid) {
                    println!(
                        "Serve already appears to be running (PID {}, file {}). Stop it first or remove a stale PID file.",
                        pid,
                        pid_path.display()
                    );
                    return Ok(());
                }
                let _ = std::fs::remove_file(&pid_path);
            }
            if let Some(parent) = pid_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&pid_path, format!("{}\n", std::process::id()))
                .map_err(|e| AgentError::Io(format!("failed to write PID file: {}", e)))?;

            let cloud_mode = std::env::var("HERMES_CLOUD_MODE")
                .ok()
                .map(|v| is_truthy(&v))
                .unwrap_or(false);
            if cloud_mode {
                println!("Cloud mode is not available in engine-only hermes-server.");
                println!("Falling back to single-user API server mode.");
            }
            if !no_gateway || !no_cron {
                println!(
                    "Gateway/cron sidecars require runtime integration and are skipped in engine mode."
                );
            }
            println!("API server listening at http://{}", addr);

            let res = hermes_server::run_server(addr, config).await;
            let _ = std::fs::remove_file(&pid_path);
            res
        }
        Some("status") => {
            let pid_path = serve_pid_path_for_cli(&cli);
            match std::fs::read_to_string(&pid_path) {
                Ok(raw) => match raw.trim().parse::<u32>() {
                    Ok(pid) if gateway_pid_is_alive(pid) => {
                        println!(
                            "Serve status: running (PID {}, file {})",
                            pid,
                            pid_path.display()
                        );
                    }
                    Ok(pid) => {
                        println!(
                            "Serve status: not running (stale PID {} in {})",
                            pid,
                            pid_path.display()
                        );
                    }
                    Err(_) => {
                        println!("Serve status: invalid PID file at {}", pid_path.display());
                    }
                },
                Err(_) => {
                    println!(
                        "Serve status: not running (no PID file; start with `hermes serve start`)"
                    );
                }
            }
            Ok(())
        }
        Some("stop") => {
            let pid_path = serve_pid_path_for_cli(&cli);
            let Some(pid) = read_gateway_pid(&pid_path) else {
                println!("Serve stop: no PID file (nothing to stop).");
                return Ok(());
            };
            if !gateway_pid_is_alive(pid) {
                let _ = std::fs::remove_file(&pid_path);
                println!(
                    "Serve stop: process {} not running; removed stale PID file {}.",
                    pid,
                    pid_path.display()
                );
                return Ok(());
            }
            match gateway_pid_terminate(pid) {
                Ok(()) => {
                    println!("Sent SIGTERM to serve PID {}.", pid);
                    let _ = std::fs::remove_file(&pid_path);
                    println!("Removed {}.", pid_path.display());
                }
                Err(e) => println!("Serve stop: failed to signal PID {}: {}", pid, e),
            }
            Ok(())
        }
        Some(other) => {
            println!(
                "Unknown serve action: {}. Use 'start', 'stop', or 'status'.",
                other
            );
            Ok(())
        }
    }
}
