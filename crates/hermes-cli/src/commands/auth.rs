//! Auth-related CLI subcommand handlers: login/logout, pairing, runtime-provider.

/// Handle `hermes login [provider]`.
pub async fn handle_cli_login(provider: Option<String>) -> Result<(), hermes_core::AgentError> {
    let provider = provider.unwrap_or_else(|| "openai".to_string());
    let creds_dir = hermes_config::hermes_home().join("credentials");
    std::fs::create_dir_all(&creds_dir).map_err(|e| hermes_core::AgentError::Io(e.to_string()))?;

    println!("Login to: {}", provider);
    println!("----------{}", "-".repeat(provider.len()));

    match provider.as_str() {
        "openai" => {
            let env_key = std::env::var("OPENAI_API_KEY").ok();
            if let Some(key) = env_key {
                let masked = if key.len() > 8 {
                    format!("{}...{}", &key[..4], &key[key.len() - 4..])
                } else {
                    "****".to_string()
                };
                println!("Found OPENAI_API_KEY in environment: {}", masked);
                let cred_file = creds_dir.join("openai.json");
                let cred = serde_json::json!({
                    "provider": "openai",
                    "api_key_masked": masked,
                    "stored_at": chrono::Utc::now().to_rfc3339(),
                    "source": "env",
                });
                std::fs::write(
                    &cred_file,
                    serde_json::to_string_pretty(&cred).unwrap_or_default(),
                )
                .map_err(|e| hermes_core::AgentError::Io(e.to_string()))?;
                println!("Credential reference stored at {}", cred_file.display());
            } else {
                println!("No OPENAI_API_KEY found in environment.");
                println!("Set it with: export OPENAI_API_KEY=sk-...");
                println!("Or use: hermes config set openai_api_key <key>");
            }
        }
        "anthropic" => {
            let env_key = std::env::var("ANTHROPIC_API_KEY").ok();
            if let Some(key) = env_key {
                let masked = if key.len() > 8 {
                    format!("{}...{}", &key[..4], &key[key.len() - 4..])
                } else {
                    "****".to_string()
                };
                println!("Found ANTHROPIC_API_KEY in environment: {}", masked);
                let cred_file = creds_dir.join("anthropic.json");
                let cred = serde_json::json!({
                    "provider": "anthropic",
                    "api_key_masked": masked,
                    "stored_at": chrono::Utc::now().to_rfc3339(),
                    "source": "env",
                });
                std::fs::write(
                    &cred_file,
                    serde_json::to_string_pretty(&cred).unwrap_or_default(),
                )
                .map_err(|e| hermes_core::AgentError::Io(e.to_string()))?;
                println!("Credential reference stored at {}", cred_file.display());
            } else {
                println!("No ANTHROPIC_API_KEY found in environment.");
                println!("Set it with: export ANTHROPIC_API_KEY=sk-ant-...");
            }
        }
        other => {
            let env_var = format!("{}_API_KEY", other.to_uppercase().replace('-', "_"));
            let env_key = std::env::var(&env_var).ok();
            if let Some(key) = env_key {
                let masked = if key.len() > 8 {
                    format!("{}...{}", &key[..4], &key[key.len() - 4..])
                } else {
                    "****".to_string()
                };
                println!("Found {} in environment: {}", env_var, masked);
                let cred_file = creds_dir.join(format!("{}.json", other));
                let cred = serde_json::json!({
                    "provider": other,
                    "api_key_masked": masked,
                    "stored_at": chrono::Utc::now().to_rfc3339(),
                    "source": "env",
                });
                std::fs::write(
                    &cred_file,
                    serde_json::to_string_pretty(&cred).unwrap_or_default(),
                )
                .map_err(|e| hermes_core::AgentError::Io(e.to_string()))?;
                println!("Credential reference stored.");
            } else {
                println!("No {} found in environment.", env_var);
                println!("Set it with: export {}=<your-key>", env_var);
            }
        }
    }
    Ok(())
}

/// Handle `hermes logout [provider]`.
pub async fn handle_cli_logout(provider: Option<String>) -> Result<(), hermes_core::AgentError> {
    let creds_dir = hermes_config::hermes_home().join("credentials");

    match provider.as_deref() {
        Some(p) => {
            let cred_file = creds_dir.join(format!("{}.json", p));
            if cred_file.exists() {
                std::fs::remove_file(&cred_file)
                    .map_err(|e| hermes_core::AgentError::Io(e.to_string()))?;
                println!("Logged out from '{}'. Credential reference removed.", p);
            } else {
                println!("No stored credentials for '{}'.", p);
            }
            println!(
                "Note: Environment variables (e.g. {}_API_KEY) are not affected.",
                p.to_uppercase().replace('-', "_")
            );
        }
        None => {
            if creds_dir.exists() {
                let mut removed = 0u32;
                if let Ok(rd) = std::fs::read_dir(&creds_dir) {
                    for entry in rd.filter_map(|e| e.ok()) {
                        let path = entry.path();
                        if path.extension().map(|e| e == "json").unwrap_or(false) {
                            if std::fs::remove_file(&path).is_ok() {
                                let name = path.file_stem().unwrap_or_default().to_string_lossy();
                                println!("  Removed credential: {}", name);
                                removed += 1;
                            }
                        }
                    }
                }
                if removed == 0 {
                    println!("No stored credentials to remove.");
                } else {
                    println!("Logged out from {} provider(s).", removed);
                }
            } else {
                println!("No credentials directory found.");
            }
            println!("Note: Environment variables are not affected.");
        }
    }
    Ok(())
}

/// Handle `hermes pairing [action] [--device-id ...]`.
pub async fn handle_cli_pairing(
    action: Option<String>,
    device_id: Option<String>,
) -> Result<(), hermes_core::AgentError> {
    use crate::pairing_store::{PairingStatus, PairingStore};

    let store = PairingStore::open_default();

    match action.as_deref().unwrap_or("list") {
        "list" => {
            let devices = store.list().map_err(|e| hermes_core::AgentError::Io(e))?;
            if devices.is_empty() {
                println!("No paired devices.");
                println!("  Store: {}", PairingStore::default_path().display());
            } else {
                println!("Paired devices ({}):", devices.len());
                println!(
                    "  {:20} {:10} {:12} {}",
                    "Device ID", "Status", "Last Seen", "Name"
                );
                println!("  {}", "-".repeat(60));
                for d in &devices {
                    let last_seen = d.last_seen.as_deref().unwrap_or("never");
                    let name = d.name.as_deref().unwrap_or("(unnamed)");
                    let status_icon = match d.status {
                        PairingStatus::Pending => "⏳",
                        PairingStatus::Approved => "✓",
                        PairingStatus::Revoked => "✗",
                    };
                    println!(
                        "  {:20} {} {:8} {:12} {}",
                        d.device_id, status_icon, d.status, last_seen, name
                    );
                }
            }
        }
        "approve" => {
            let did = device_id.ok_or_else(|| {
                hermes_core::AgentError::Config(
                    "Missing --device-id. Usage: hermes pairing approve --device-id <id>".into(),
                )
            })?;
            match store.approve(&did) {
                Ok(dev) => {
                    println!("Device '{}' approved.", dev.device_id);
                    if let Some(secret) = &dev.shared_secret {
                        println!("  Shared secret: {}", secret);
                        println!("  (Store this securely — it will not be shown again)");
                    }
                }
                Err(e) => println!("Failed to approve device: {}", e),
            }
        }
        "revoke" => {
            let did = device_id.ok_or_else(|| {
                hermes_core::AgentError::Config(
                    "Missing --device-id. Usage: hermes pairing revoke --device-id <id>".into(),
                )
            })?;
            match store.revoke(&did) {
                Ok(dev) => {
                    println!("Device '{}' revoked.", dev.device_id);
                    println!("  The device will no longer be able to connect.");
                }
                Err(e) => println!("Failed to revoke device: {}", e),
            }
        }
        "clear-pending" => match store.clear_pending() {
            Ok(count) => {
                if count == 0 {
                    println!("No pending pairing requests to clear.");
                } else {
                    println!("Cleared {} pending pairing request(s).", count);
                }
            }
            Err(e) => println!("Failed to clear pending requests: {}", e),
        },
        other => {
            println!("Pairing action '{}' is not recognized.", other);
            println!("Available actions: list, approve, revoke, clear-pending");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// RuntimeProvider
// ---------------------------------------------------------------------------

/// Handle `hermes runtime-provider [action] [provider]`.
pub async fn handle_cli_runtime_provider(
    action: Option<String>,
    provider: Option<String>,
) -> Result<(), hermes_core::AgentError> {
    match action.as_deref().unwrap_or("status") {
        "list" => {
            // Configured providers (from config.yaml) first, with the active
            // one marked; then the built-in known provider backends.
            let config = hermes_config::load_config(None).ok();
            let active_name: Option<String> = config.as_ref().and_then(|cfg| {
                let model_str = cfg.model.as_deref().unwrap_or("gpt-4o");
                model_str.find(':').map(|idx| model_str[..idx].to_string())
            });
            if let Some(cfg) = &config {
                if !cfg.llm_providers.is_empty() {
                    println!("Configured providers:");
                    let mut names: Vec<&String> = cfg.llm_providers.keys().collect();
                    names.sort();
                    for name in names {
                        let entry = &cfg.llm_providers[name];
                        let mark = if active_name.as_deref() == Some(name.as_str()) {
                            "●"
                        } else {
                            " "
                        };
                        let model = entry.model.as_deref().unwrap_or("-");
                        let base = entry.base_url.as_deref().unwrap_or("-");
                        println!("  {} {:<12} model={:<24} {}", mark, name, model, base);
                    }
                    println!();
                }
            }
            println!("Available runtime providers:");
            println!("  openai       — OpenAI API (gpt-4o, gpt-4o-mini, o1, ...)");
            println!("  anthropic    — Anthropic API (claude-3.5-sonnet, claude-3-opus, ...)");
            println!("  openrouter   — OpenRouter (multi-provider routing)");
            println!("  nous         — Nous Research API");
            println!("  generic      — Generic OpenAI-compatible endpoint");
            println!("  copilot      — GitHub Copilot backend");
            println!("  codex        — OpenAI Codex backend");
            println!("  qwen         — Alibaba Qwen API");
            println!("  kimi         — Moonshot Kimi API");
            println!("  minimax      — MiniMax API");
            println!("\nUsage: hermes runtime-provider set <provider>");
        }
        "set" => {
            let target = provider.ok_or_else(|| {
                hermes_core::AgentError::Config(
                    "Missing provider. Usage: hermes runtime-provider set <provider>".into(),
                )
            })?;
            let known = [
                "openai",
                "anthropic",
                "openrouter",
                "nous",
                "generic",
                "copilot",
                "codex",
                "qwen",
                "kimi",
                "minimax",
            ];
            if known.contains(&target.as_str()) {
                println!("Runtime provider set to: {}", target);
                println!(
                    "(To persist, run: hermes config set model {}:<model-name>)",
                    target
                );
            } else {
                println!(
                    "Unknown provider: '{}'. Use `hermes runtime-provider list` to see options.",
                    target
                );
            }
        }
        "status" => {
            let config = hermes_config::load_config(None)
                .map_err(|e| hermes_core::AgentError::Config(e.to_string()))?;
            let model_str = config.model.as_deref().unwrap_or("gpt-4o");
            let (provider_name, model_name) = if let Some(idx) = model_str.find(':') {
                (&model_str[..idx], &model_str[idx + 1..])
            } else {
                ("openai", model_str)
            };
            println!("Runtime Provider Status");
            println!("  Provider: {}", provider_name);
            println!("  Model:    {}", model_name);
            println!(
                "  Endpoint: {}",
                match provider_name {
                    "openai" => "https://api.openai.com/v1",
                    "anthropic" => "https://api.anthropic.com/v1",
                    "openrouter" => "https://openrouter.ai/api/v1",
                    "nous" => "https://inference.nous.hermes.dev/v1",
                    _ => "(custom endpoint)",
                }
            );

            // Check for API key availability
            let env_key = format!("{}_API_KEY", provider_name.to_uppercase());
            let has_key =
                std::env::var(&env_key).is_ok() || std::env::var("OPENAI_API_KEY").is_ok();
            println!(
                "  Auth:     {}",
                if has_key {
                    "configured ✓"
                } else {
                    "not configured ✗"
                }
            );
        }
        other => {
            println!("Unknown runtime-provider action: '{}'.", other);
            println!("Available actions: list, set, status");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Subscription
// ---------------------------------------------------------------------------
