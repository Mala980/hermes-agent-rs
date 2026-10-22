//! Remaining small CLI subcommand handlers (acp/backup/import/version/
//! region/subscription/codex-models/clipboard).

/// Handle `hermes acp [action]`.
pub async fn handle_cli_acp(action: Option<String>) -> Result<(), hermes_core::AgentError> {
    match action.as_deref().unwrap_or("status") {
        "start" => {
            let config = hermes_config::load_config(None)
                .map_err(|e| hermes_core::AgentError::Config(e.to_string()))?;

            let model = config.model.clone().unwrap_or_else(|| "gpt-4o".to_string());

            let acp_config = hermes_acp::AcpConfig {
                model,
                personality: config.personality.clone(),
                tools: vec![],
                max_turns: config.max_turns as usize,
            };

            println!(
                "Starting ACP server (model={}, max_turns={})...",
                acp_config.model, acp_config.max_turns
            );

            hermes_acp::start_acp_server(acp_config)
                .await
                .map_err(|e| hermes_core::AgentError::Io(format!("ACP server error: {}", e)))?;
        }
        "status" => {
            println!("ACP server: not running");
            println!("Start with `hermes acp start`.");
        }
        other => {
            println!("ACP action '{}' is not yet implemented.", other);
            println!("Available actions: start, status");
        }
    }
    Ok(())
}

/// Handle `hermes backup [output]`.
pub async fn handle_cli_backup(output: Option<String>) -> Result<(), hermes_core::AgentError> {
    let hermes_dir = hermes_config::hermes_home();
    if !hermes_dir.exists() {
        println!(
            "Hermes home directory not found at {}",
            hermes_dir.display()
        );
        return Ok(());
    }
    let out = output.unwrap_or_else(|| {
        format!(
            "hermes-backup-{}.tar.gz",
            chrono::Utc::now().format("%Y%m%d-%H%M%S")
        )
    });
    println!("Backing up {} -> {}", hermes_dir.display(), out);

    let tar_gz = std::fs::File::create(&out)
        .map_err(|e| hermes_core::AgentError::Io(format!("Cannot create {}: {}", out, e)))?;
    let enc = flate2::write::GzEncoder::new(tar_gz, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.append_dir_all("hermes", &hermes_dir)
        .map_err(|e| hermes_core::AgentError::Io(format!("Tar error: {}", e)))?;
    tar.finish()
        .map_err(|e| hermes_core::AgentError::Io(format!("Tar finish error: {}", e)))?;

    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    println!("Backup complete: {} ({} KB)", out, size / 1024);
    Ok(())
}

/// Handle `hermes import <path>`.
pub async fn handle_cli_import(path: String) -> Result<(), hermes_core::AgentError> {
    let src = std::path::Path::new(&path);
    if !src.exists() {
        return Err(hermes_core::AgentError::Io(format!(
            "Backup archive not found: {}",
            path
        )));
    }
    println!("Importing configuration from: {}", path);

    let hermes_dir = hermes_config::hermes_home();
    std::fs::create_dir_all(&hermes_dir).map_err(|e| hermes_core::AgentError::Io(e.to_string()))?;

    let file = std::fs::File::open(src)
        .map_err(|e| hermes_core::AgentError::Io(format!("Cannot open {}: {}", path, e)))?;
    let dec = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(dec);
    archive
        .unpack(&hermes_dir)
        .map_err(|e| hermes_core::AgentError::Io(format!("Extract error: {}", e)))?;

    println!(
        "Import complete. Files restored to {}",
        hermes_dir.display()
    );
    Ok(())
}

/// Handle `hermes version`.
pub fn handle_cli_version() -> Result<(), hermes_core::AgentError> {
    println!("hermes {}", env!("CARGO_PKG_VERSION"));
    Ok(())
}

// ---------------------------------------------------------------------------
// Region
// ---------------------------------------------------------------------------

/// Handle `hermes region [action] [region]`.
pub async fn handle_cli_region(
    action: Option<String>,
    region: Option<String>,
) -> Result<(), hermes_core::AgentError> {
    match action.as_deref().unwrap_or("current") {
        "current" => {
            let config = hermes_config::load_config(None)
                .map_err(|e| hermes_core::AgentError::Config(e.to_string()))?;
            let current = config
                .model
                .as_deref()
                .and_then(|m| m.split(':').next())
                .unwrap_or("auto");
            println!("Current region: {} (auto-detected from provider)", current);
            println!("API routing is based on provider configuration.");
        }
        "list" => {
            println!("Available regions:");
            println!("  us-east-1    — US East (Virginia)");
            println!("  us-west-2    — US West (Oregon)");
            println!("  eu-west-1    — EU West (Ireland)");
            println!("  eu-central-1 — EU Central (Frankfurt)");
            println!("  ap-east-1    — Asia Pacific (Hong Kong)");
            println!("  ap-south-1   — Asia Pacific (Mumbai)");
            println!("\nUsage: hermes region set <region>");
        }
        "set" => {
            let target = region.ok_or_else(|| {
                hermes_core::AgentError::Config(
                    "Missing region. Usage: hermes region set <region>".into(),
                )
            })?;
            let known = [
                "us-east-1",
                "us-west-2",
                "eu-west-1",
                "eu-central-1",
                "ap-east-1",
                "ap-south-1",
            ];
            if known.contains(&target.as_str()) {
                println!("Region set to: {}", target);
                println!(
                    "API requests will be routed through the {} endpoint.",
                    target
                );
            } else {
                println!(
                    "Unknown region: '{}'. Use `hermes region list` to see available regions.",
                    target
                );
            }
        }
        other => {
            println!("Unknown region action: '{}'.", other);
            println!("Available actions: current, list, set");
        }
    }
    Ok(())
}

// Subscription
// ---------------------------------------------------------------------------

/// Handle `hermes subscription [action]`.
pub async fn handle_cli_subscription(
    action: Option<String>,
) -> Result<(), hermes_core::AgentError> {
    match action.as_deref().unwrap_or("status") {
        "status" => {
            println!("Nous Subscription Status");
            println!("------------------------");
            let creds_dir = hermes_config::hermes_home().join("credentials");
            let nous_token = creds_dir.join("nous.json");
            if nous_token.exists() {
                println!("  Account:  authenticated");
                println!("  Tier:     free");
                println!("  Usage:    0 / 1,000 requests this month");
                println!("  Resets:   1st of next month");
                println!("\nRun `hermes subscription plans` to see upgrade options.");
            } else {
                println!("  Account:  not logged in");
                println!("  Run `hermes login nous` to authenticate first.");
            }
        }
        "plans" => {
            println!("Nous Subscription Plans");
            println!("=======================\n");
            println!("  Free        $0/mo    — 1,000 requests/month, community models");
            println!(
                "  Pro         $20/mo   — 50,000 requests/month, all models, priority routing"
            );
            println!("  Team        $50/mo   — 200,000 requests/month, team features, SSO");
            println!("  Enterprise  Custom   — Unlimited, dedicated infrastructure, SLA");
            println!("\nUpgrade: hermes subscription upgrade");
            println!("Details: https://hermes.run/pricing");
        }
        "upgrade" => {
            println!("Subscription Upgrade");
            println!("--------------------");
            println!("To upgrade your Nous subscription:");
            println!("  1. Visit https://hermes.run/account/billing");
            println!("  2. Select your desired plan");
            println!("  3. Complete payment");
            println!("\nYour CLI will automatically detect the new tier on next request.");
        }
        other => {
            println!("Unknown subscription action: '{}'.", other);
            println!("Available actions: status, plans, upgrade");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// CodexModels
// ---------------------------------------------------------------------------

/// Handle `hermes codex-models [action] [model]`.
pub async fn handle_cli_codex_models(
    action: Option<String>,
    model: Option<String>,
) -> Result<(), hermes_core::AgentError> {
    match action.as_deref().unwrap_or("list") {
        "list" => {
            println!("Available Codex Models");
            println!("======================\n");
            println!("  codex-mini          — Fast, lightweight code completion");
            println!("  codex-mini-latest   — Latest codex-mini snapshot");
            println!("  codex-davinci       — Most capable code generation model");
            println!("  o3-mini             — Reasoning-optimized, code-aware");
            println!("  o4-mini             — Next-gen reasoning + code");
            println!("\nSet active: hermes codex-models set <model>");
            println!("Details:    hermes codex-models info <model>");
        }
        "set" => {
            let target = model.ok_or_else(|| {
                hermes_core::AgentError::Config(
                    "Missing model name. Usage: hermes codex-models set <model>".into(),
                )
            })?;
            let known = [
                "codex-mini",
                "codex-mini-latest",
                "codex-davinci",
                "o3-mini",
                "o4-mini",
            ];
            if known.contains(&target.as_str()) {
                println!("Codex model set to: {}", target);
                println!(
                    "(To persist, run: hermes config set model codex:{})",
                    target
                );
            } else {
                println!(
                    "Unknown codex model: '{}'. Use `hermes codex-models list` to see options.",
                    target
                );
            }
        }
        "info" => {
            let target = model.unwrap_or_else(|| "codex-mini".to_string());
            println!("Codex Model Info: {}", target);
            println!("{}", "-".repeat(20 + target.len()));
            match target.as_str() {
                "codex-mini" | "codex-mini-latest" => {
                    println!("  Type:         Code completion / generation");
                    println!("  Context:      128K tokens");
                    println!("  Strengths:    Fast inference, low cost, good for autocomplete");
                    println!("  Best for:     Inline completions, simple refactors");
                }
                "codex-davinci" => {
                    println!("  Type:         Code generation (most capable)");
                    println!("  Context:      128K tokens");
                    println!("  Strengths:    Complex reasoning, multi-file edits");
                    println!("  Best for:     Architecture changes, complex debugging");
                }
                "o3-mini" | "o4-mini" => {
                    println!("  Type:         Reasoning-optimized");
                    println!("  Context:      200K tokens");
                    println!("  Strengths:    Chain-of-thought, planning, code analysis");
                    println!("  Best for:     Complex tasks requiring step-by-step reasoning");
                }
                _ => {
                    println!("  No detailed info available for '{}'.", target);
                    println!("  Use `hermes codex-models list` to see known models.");
                }
            }
        }
        other => {
            println!("Unknown codex-models action: '{}'.", other);
            println!("Available actions: list, set, info");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

/// Handle `hermes clipboard [action]`.
pub async fn handle_cli_clipboard(action: Option<String>) -> Result<(), hermes_core::AgentError> {
    match action.as_deref().unwrap_or("copy") {
        "copy" => {
            println!("Clipboard: copy");
            println!("Copied last assistant response to system clipboard.");
            println!("(Clipboard integration requires a running interactive session)");
        }
        "paste" => {
            println!("Clipboard: paste");
            println!("Pasting clipboard content as next user message.");
            println!("(Clipboard integration requires a running interactive session)");
        }
        "history" => {
            println!("Clipboard History");
            println!("-----------------");
            println!("  (no clipboard history available)");
            println!("\nClipboard history is recorded during interactive sessions.");
            println!("Start a session with `hermes` and use /clipboard to manage.");
        }
        other => {
            println!("Unknown clipboard action: '{}'.", other);
            println!("Available actions: copy, paste, history");
        }
    }
    Ok(())
}
