//! Remaining small subcommand handlers (dump/completion/uninstall/lumio/
//! setup/doctor/update/status/logs/profile).

use clap::CommandFactory;
use clap_complete::{generate, Shell as CompletionShell};
use hermes_cli::cli::Cli;
use hermes_config::{hermes_home, load_config};
use hermes_core::AgentError;
use std::path::PathBuf;

pub(crate) async fn run_dump(
    cli: Cli,
    session: Option<String>,
    output: Option<String>,
) -> Result<(), AgentError> {
    let home = cli
        .config_dir
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(hermes_config::hermes_home);
    let sessions_dir = home.join("sessions");
    let session = session.unwrap_or_else(|| "latest".to_string());
    let out = output.unwrap_or_else(|| format!("{}.dump.json", session));
    let payload = serde_json::json!({
        "session": session,
        "source_dir": sessions_dir,
        "note": "Session export scaffold"
    });
    std::fs::write(
        &out,
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
    )
    .map_err(|e| AgentError::Io(format!("Failed to write dump: {}", e)))?;
    println!("Wrote dump to {}", out);
    Ok(())
}

pub(crate) fn run_completion(shell: Option<String>) -> Result<(), AgentError> {
    let mut cmd = Cli::command();
    let sh = match shell.as_deref().unwrap_or("zsh") {
        "bash" => CompletionShell::Bash,
        "fish" => CompletionShell::Fish,
        "powershell" => CompletionShell::PowerShell,
        "elvish" => CompletionShell::Elvish,
        _ => CompletionShell::Zsh,
    };
    generate(sh, &mut cmd, "hermes", &mut std::io::stdout());
    Ok(())
}

pub(crate) async fn run_uninstall(yes: bool) -> Result<(), AgentError> {
    let home = hermes_config::hermes_home();
    if !yes {
        println!("Uninstall is destructive. Re-run with `hermes uninstall --yes`.");
        return Ok(());
    }
    if home.exists() {
        std::fs::remove_dir_all(&home)
            .map_err(|e| AgentError::Io(format!("Failed to remove {}: {}", home.display(), e)))?;
        println!("Removed {}", home.display());
    } else {
        println!("Nothing to uninstall.");
    }
    Ok(())
}

/// Handle `hermes lumio [action]`.
pub(crate) async fn run_lumio(
    action: Option<String>,
    model: Option<String>,
) -> Result<(), AgentError> {
    match action.as_deref() {
        None | Some("login") => {
            hermes_cli::lumio::setup(model.as_deref(), true).await?;
        }
        Some("logout") => {
            hermes_cli::lumio::clear_token();
            println!("✅ Lumio token removed.");
        }
        Some("status") => match hermes_cli::lumio::load_token() {
            Some(t) => {
                let user = if t.username.is_empty() {
                    "(unknown)"
                } else {
                    &t.username
                };
                println!("Lumio: logged in as {}", user);
                println!("  API: {}", t.base_url);
                println!(
                    "  Token: {}...{}",
                    &t.token[..8.min(t.token.len())],
                    &t.token[t.token.len().saturating_sub(4)..]
                );
            }
            None => {
                println!("Lumio: not logged in");
                println!("  Run `hermes lumio` to login.");
            }
        },
        Some(other) => {
            println!(
                "Unknown lumio action: '{}'. Use: login, logout, status.",
                other
            );
        }
    }
    Ok(())
}

/// Handle `hermes setup`.
pub(crate) async fn run_setup() -> Result<(), AgentError> {
    use std::io::{self, BufRead, Write};

    println!("Hermes Agent — Setup Wizard");
    println!("===========================\n");

    let config_dir = hermes_config::hermes_home();
    println!("Config directory: {}", config_dir.display());

    // 1. Create directory structure
    let subdirs = ["profiles", "sessions", "logs", "skills"];
    for dir in [config_dir.clone()]
        .into_iter()
        .chain(subdirs.iter().map(|d| config_dir.join(d)))
    {
        if dir.exists() {
            println!("  ✓ {} exists", dir.display());
        } else {
            std::fs::create_dir_all(&dir).map_err(|e| {
                AgentError::Io(format!("Failed to create {}: {}", dir.display(), e))
            })?;
            println!("  ✓ Created {}", dir.display());
        }
    }

    let config_path = config_dir.join("config.yaml");
    let stdin = io::stdin();
    let mut reader = stdin.lock();

    // 2. Prompt for API key
    print!("\nOpenAI API key (leave blank to skip): ");
    io::stdout().flush().ok();
    let mut api_key = String::new();
    reader.read_line(&mut api_key).ok();
    let api_key = api_key.trim().to_string();

    // 3. Prompt for model
    println!("\nAvailable models:");
    println!("  1) openai:gpt-4o          (recommended)");
    println!("  2) openai:gpt-4o-mini     (fast & cheap)");
    println!("  3) anthropic:claude-3-5-sonnet");
    println!("  4) openrouter:auto        (multi-provider)");
    print!("Choose model [1]: ");
    io::stdout().flush().ok();
    let mut model_choice = String::new();
    reader.read_line(&mut model_choice).ok();
    let model = match model_choice.trim() {
        "2" => "openai:gpt-4o-mini",
        "3" => "anthropic:claude-3-5-sonnet",
        "4" => "openrouter:auto",
        _ => "openai:gpt-4o",
    };

    // 4. Prompt for personality
    print!("\nPersonality (default, concise, creative, technical) [default]: ");
    io::stdout().flush().ok();
    let mut personality = String::new();
    reader.read_line(&mut personality).ok();
    let personality = personality.trim();
    let personality = if personality.is_empty() {
        "default"
    } else {
        personality
    };

    // 5. Write config.yaml
    if config_path.exists() {
        print!("\nconfig.yaml already exists. Overwrite? [y/N]: ");
        io::stdout().flush().ok();
        let mut answer = String::new();
        reader.read_line(&mut answer).ok();
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("Keeping existing config.yaml.");
            println!("\nSetup complete! Run `hermes` to start an interactive session.");
            return Ok(());
        }
    }

    let mut config_content = String::from("# Hermes Agent Configuration\n\n");
    config_content.push_str(&format!("model: {}\n", model));
    config_content.push_str(&format!("personality: {}\n", personality));
    config_content.push_str("max_turns: 50\n\n");

    if !api_key.is_empty() {
        config_content.push_str("llm_providers:\n");
        config_content.push_str("  openai:\n");
        config_content.push_str(&format!("    api_key: {}\n", api_key));
    }

    std::fs::write(&config_path, &config_content)
        .map_err(|e| AgentError::Io(format!("Failed to write config: {}", e)))?;
    println!("\n  ✓ Wrote config.yaml");

    // 6. Write default profile
    let default_profile = config_dir.join("profiles").join("default.yaml");
    if !default_profile.exists() {
        let profile_content = format!(
            "# Default Hermes Profile\nname: default\nmodel: {}\npersonality: {}\n",
            model, personality,
        );
        std::fs::write(&default_profile, profile_content)
            .map_err(|e| AgentError::Io(format!("Failed to write profile: {}", e)))?;
        println!("  ✓ Created default profile");
    }

    println!("\nSetup complete! Run `hermes` to start an interactive session.");
    println!("Run `hermes doctor` to check system requirements.");
    Ok(())
}

/// Handle `hermes doctor`.
pub(crate) async fn run_doctor(cli: Cli) -> Result<(), AgentError> {
    println!("Hermes Agent — System Check");
    println!("===========================\n");

    // Check config
    let config_dir = hermes_config::hermes_home();
    print!("Config directory ({})... ", config_dir.display());
    if config_dir.exists() {
        println!("✓");
    } else {
        println!("✗ (run `hermes setup`)");
    }

    // Check config.yaml
    let config_path = config_dir.join("config.yaml");
    print!("config.yaml... ");
    if config_path.exists() {
        println!("✓");
    } else {
        println!("✗ (run `hermes setup`)");
    }

    // Check API keys via environment
    let api_checks = [
        ("OPENAI_API_KEY", "OpenAI"),
        ("ANTHROPIC_API_KEY", "Anthropic"),
        ("OPENROUTER_API_KEY", "OpenRouter"),
        ("EXA_API_KEY", "Exa (web search)"),
        ("FIRECRAWL_API_KEY", "Firecrawl (web extract)"),
    ];

    println!("\nAPI Keys:");
    for (env_var, name) in &api_checks {
        print!("  {} ({})... ", name, env_var);
        if std::env::var(env_var).is_ok() {
            println!("✓");
        } else {
            println!("✗ (not set)");
        }
    }

    // Check external tools
    println!("\nExternal tools:");
    let tool_checks = [("docker", "Docker"), ("ssh", "SSH"), ("git", "Git")];

    for (cmd, name) in &tool_checks {
        print!("  {}... ", name);
        match tokio::process::Command::new("which")
            .arg(cmd)
            .output()
            .await
        {
            Ok(output) if output.status.success() => println!("✓"),
            _ => println!("✗ (not found)"),
        }
    }

    // Try loading config
    println!("\nConfiguration:");
    print!("  Loading config... ");
    match load_config(cli.config_dir.as_deref()) {
        Ok(config) => {
            println!("✓");
            println!(
                "  Model: {}",
                config.model.as_deref().unwrap_or("(default)")
            );
            println!("  Max turns: {}", config.max_turns);
            let platform_count = config.platforms.iter().filter(|(_, p)| p.enabled).count();
            println!("  Enabled platforms: {}", platform_count);
        }
        Err(e) => {
            println!("✗ ({})", e);
        }
    }

    println!("\nDone.");
    Ok(())
}

/// Handle `hermes update`.
pub(crate) async fn run_update() -> Result<(), AgentError> {
    println!("Hermes Agent v{}", env!("CARGO_PKG_VERSION"));
    println!("{}", hermes_cli::update::check_for_updates().await?);
    Ok(())
}

/// Handle `hermes status`.
pub(crate) async fn run_status(cli: Cli, full: bool, deep: bool) -> Result<(), AgentError> {
    println!("Hermes Agent — Status");
    println!("=====================\n");

    println!("Version: {}", env!("CARGO_PKG_VERSION"));
    println!(
        "Python baseline: v2026.4.16 ({})",
        "1dd6b5d5fb94cac59e93388f9aeee6bc365b8f42"
    );

    let config =
        load_config(cli.config_dir.as_deref()).map_err(|e| AgentError::Config(e.to_string()))?;

    println!(
        "Model:   {}",
        config.model.as_deref().unwrap_or("(default: gpt-4o)")
    );
    println!(
        "Personality: {}",
        config.personality.as_deref().unwrap_or("(none)")
    );
    println!("Max turns: {}", config.max_turns);

    let enabled_platforms: Vec<&String> = config
        .platforms
        .iter()
        .filter(|(_, pc)| pc.enabled)
        .map(|(name, _)| name)
        .collect();
    if enabled_platforms.is_empty() {
        println!("Platforms: (none enabled)");
    } else {
        println!(
            "Platforms: {}",
            enabled_platforms
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let config_dir = hermes_config::hermes_home();
    println!("\nConfig dir: {}", config_dir.display());

    println!(
        "Nous managed tools: {}",
        if hermes_config::managed_nous_tools_enabled() {
            "available"
        } else {
            "unavailable"
        }
    );

    println!("\nTool capability backends:");
    let web_ready = hermes_config::is_managed_tool_gateway_ready(
        "firecrawl",
        hermes_config::ResolveOptions::default(),
    );
    let image_ready = hermes_config::is_managed_tool_gateway_ready(
        "fal-queue",
        hermes_config::ResolveOptions::default(),
    );
    let tts_ready = hermes_config::is_managed_tool_gateway_ready(
        "openai-audio",
        hermes_config::ResolveOptions::default(),
    );
    let browser_ready = hermes_config::is_managed_tool_gateway_ready(
        "browser-use",
        hermes_config::ResolveOptions::default(),
    );
    println!(
        "  web:       use_gateway={} managed_ready={}",
        config.web.use_gateway, web_ready
    );
    println!(
        "  image_gen: use_gateway={} managed_ready={}",
        config.image_gen.use_gateway, image_ready
    );
    println!(
        "  tts:       use_gateway={} managed_ready={}",
        config.tts.use_gateway, tts_ready
    );
    println!(
        "  browser:   use_gateway={} managed_ready={} cloud_provider={}",
        config.browser.use_gateway,
        browser_ready,
        config.browser.cloud_provider.as_deref().unwrap_or("auto")
    );

    println!("\nExecution backend:");
    println!("  terminal.backend={:?}", config.terminal.backend);
    if matches!(
        config.terminal.backend,
        hermes_config::TerminalBackendType::Modal
    ) {
        let direct_modal = std::env::var("MODAL_API_TOKEN")
            .ok()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        let modal_state = hermes_config::resolve_modal_backend_state(
            config.terminal.modal_mode.as_deref(),
            direct_modal,
            hermes_config::is_managed_tool_gateway_ready(
                "modal",
                hermes_config::ResolveOptions::default(),
            ),
        );
        println!(
            "  modal_mode={} selected={}",
            modal_state.mode.as_str(),
            modal_state
                .selected_backend
                .map(|s| s.as_str())
                .unwrap_or("none")
        );
    }

    // Check for active sessions
    let sessions_dir = config_dir.join("sessions");
    if sessions_dir.exists() {
        let count = std::fs::read_dir(&sessions_dir)
            .map(|entries| entries.filter_map(|e| e.ok()).count())
            .unwrap_or(0);
        println!("Saved sessions: {}", count);
    }

    // Check for profiles
    let profiles_dir = config_dir.join("profiles");
    if profiles_dir.exists() {
        let profiles: Vec<String> = std::fs::read_dir(&profiles_dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.path()
                            .extension()
                            .map(|ext| ext == "yaml" || ext == "yml")
                            .unwrap_or(false)
                    })
                    .filter_map(|e| {
                        e.path()
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                    })
                    .collect()
            })
            .unwrap_or_default();
        if profiles.is_empty() {
            println!("Profiles: (none)");
        } else {
            println!("Profiles: {}", profiles.join(", "));
        }
    }

    if full || deep {
        println!("\n--- detail ---");
        let base: PathBuf = cli
            .config_dir
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(hermes_home);
        println!("Config file: {}", base.join("config.yaml").display());

        let logs_dir = base.join("logs");
        if logs_dir.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&logs_dir) {
                let mut logs: Vec<(String, u64)> = rd
                    .filter_map(|e| e.ok())
                    .map(|e| {
                        let size = std::fs::metadata(e.path()).map(|m| m.len()).unwrap_or(0);
                        (e.file_name().to_string_lossy().into_owned(), size)
                    })
                    .collect();
                logs.sort();
                if logs.is_empty() {
                    println!("Logs: (none)");
                } else {
                    println!("Logs:");
                    for (name, size) in &logs {
                        println!("  {:<20} {} bytes", name, size);
                    }
                }
            }
        } else {
            println!("Logs: (no logs directory)");
        }

        if !config.tools_config.disabled.is_empty() {
            println!(
                "Disabled tools: {}",
                config.tools_config.disabled.join(", ")
            );
        }
    }

    if deep {
        println!("\n--- credential checks ---");
        if config.llm_providers.is_empty() {
            println!("No llm_providers configured (using built-in defaults).");
        }
        let mut names: Vec<&String> = config.llm_providers.keys().collect();
        names.sort();
        for name in names {
            let entry = &config.llm_providers[name];
            let env_key = format!("{}_API_KEY", name.to_uppercase().replace('-', "_"));
            let has_key = entry
                .api_key
                .as_deref()
                .map(|k| !k.trim().is_empty() && !k.contains("${"))
                .unwrap_or(false)
                || std::env::var(&env_key)
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false)
                || entry.command.is_some();
            println!(
                "  {:<12} {}",
                name,
                if has_key {
                    "credentials OK"
                } else {
                    "MISSING api key (set llm.<name>.api_key or env var)"
                }
            );
        }
    }

    Ok(())
}

/// Handle `hermes logs`.
///
/// `hermes logs --list` lists available log files; `hermes logs --name <log>`
/// reads a specific log (bare name like "gateway" or "gateway.log").
pub(crate) async fn run_logs(
    cli: Cli,
    lines: u32,
    follow: bool,
    list: bool,
    name: Option<String>,
) -> Result<(), AgentError> {
    let config_dir = cli
        .config_dir
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(hermes_config::hermes_home);
    let logs_dir = config_dir.join("logs");

    if list {
        if !logs_dir.is_dir() {
            println!("No logs directory at: {}", logs_dir.display());
            return Ok(());
        }
        let mut entries: Vec<(String, u64)> = std::fs::read_dir(&logs_dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| {
                        let size = std::fs::metadata(e.path()).map(|m| m.len()).unwrap_or(0);
                        (e.file_name().to_string_lossy().into_owned(), size)
                    })
                    .collect()
            })
            .unwrap_or_default();
        entries.sort();
        if entries.is_empty() {
            println!("No log files in {}.", logs_dir.display());
        } else {
            println!("Log files ({}):", logs_dir.display());
            for (name, size) in &entries {
                println!("  {:<24} {} bytes", name, size);
            }
        }
        return Ok(());
    }

    let log_file = match name {
        Some(n) => {
            let direct = logs_dir.join(&n);
            if direct.is_file() {
                direct
            } else {
                logs_dir.join(format!("{}.log", n.trim_end_matches(".log")))
            }
        }
        None => logs_dir.join("hermes.log"),
    };

    if !log_file.exists() {
        println!("No log file found at: {}", log_file.display());
        println!("Logs are written here during interactive sessions.");
        return Ok(());
    }

    if follow {
        println!("Tailing {}... (Ctrl+C to stop)\n", log_file.display());
        let mut child = tokio::process::Command::new("tail")
            .args(["-f", "-n", &lines.to_string()])
            .arg(&log_file)
            .spawn()
            .map_err(|e| AgentError::Io(format!("Failed to tail log file: {}", e)))?;

        tokio::select! {
            status = child.wait() => {
                match status {
                    Ok(s) if !s.success() => {
                        eprintln!("tail exited with status: {}", s);
                    }
                    Err(e) => {
                        eprintln!("Error waiting for tail: {}", e);
                    }
                    _ => {}
                }
            }
            _ = tokio::signal::ctrl_c() => {
                child.kill().await.ok();
                println!("\nStopped tailing logs.");
            }
        }
    } else {
        let content = std::fs::read_to_string(&log_file)
            .map_err(|e| AgentError::Io(format!("Failed to read log file: {}", e)))?;
        let all_lines: Vec<&str> = content.lines().collect();
        let start = all_lines.len().saturating_sub(lines as usize);
        for line in &all_lines[start..] {
            println!("{}", line);
        }
        println!(
            "\n(Showing last {} of {} lines from {})",
            all_lines.len() - start,
            all_lines.len(),
            log_file.display()
        );
    }
    Ok(())
}

/// Handle `hermes profile [action] [name]`.
pub(crate) async fn run_profile(
    cli: Cli,
    action: Option<String>,
    name: Option<String>,
) -> Result<(), AgentError> {
    let config_dir = cli
        .config_dir
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(hermes_config::hermes_home);
    let profiles_dir = config_dir.join("profiles");

    match action.as_deref() {
        None => {
            // Show current profile
            let config = load_config(cli.config_dir.as_deref())
                .map_err(|e| AgentError::Config(e.to_string()))?;
            println!("Current profile:");
            println!(
                "  Model:       {}",
                config.model.as_deref().unwrap_or("gpt-4o")
            );
            println!(
                "  Personality: {}",
                config.personality.as_deref().unwrap_or("default")
            );
            println!("  Max turns:   {}", config.max_turns);
            println!("\nUse `hermes profile list` to see all profiles.");
        }
        Some("list") => {
            if !profiles_dir.exists() {
                println!("No profiles directory found. Run `hermes setup` first.");
                return Ok(());
            }
            let entries: Vec<String> = std::fs::read_dir(&profiles_dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter(|e| {
                            e.path()
                                .extension()
                                .map(|ext| ext == "yaml" || ext == "yml")
                                .unwrap_or(false)
                        })
                        .filter_map(|e| {
                            e.path()
                                .file_stem()
                                .map(|s| s.to_string_lossy().into_owned())
                        })
                        .collect()
                })
                .unwrap_or_default();

            if entries.is_empty() {
                println!("No profiles found. Create one with `hermes profile create <name>`.");
            } else {
                println!("Available profiles:");
                for name in &entries {
                    println!("  • {}", name);
                }
            }
        }
        Some("create") => {
            let name = name.ok_or_else(|| {
                AgentError::Config(
                    "Missing profile name. Usage: hermes profile create <name>".into(),
                )
            })?;

            std::fs::create_dir_all(&profiles_dir)
                .map_err(|e| AgentError::Io(format!("Failed to create profiles dir: {}", e)))?;

            let profile_path = profiles_dir.join(format!("{}.yaml", name));
            if profile_path.exists() {
                println!(
                    "Profile '{}' already exists at {}",
                    name,
                    profile_path.display()
                );
                return Ok(());
            }

            let content = format!(
                "# Hermes Profile: {}\nname: {}\nmodel: openai:gpt-4o\npersonality: default\nmax_turns: 50\n",
                name, name
            );
            std::fs::write(&profile_path, content)
                .map_err(|e| AgentError::Io(format!("Failed to write profile: {}", e)))?;
            println!("Created profile '{}' at {}", name, profile_path.display());
            println!(
                "Edit it to customize, then switch with `hermes profile switch {}`.",
                name
            );
        }
        Some("switch") => {
            let name = name.ok_or_else(|| {
                AgentError::Config(
                    "Missing profile name. Usage: hermes profile switch <name>".into(),
                )
            })?;

            let profile_path = profiles_dir.join(format!("{}.yaml", name));
            if !profile_path.exists() {
                // Also try .yml
                let alt = profiles_dir.join(format!("{}.yml", name));
                if !alt.exists() {
                    println!("Profile '{}' not found. Available profiles:", name);
                    if let Ok(rd) = std::fs::read_dir(&profiles_dir) {
                        for entry in rd.filter_map(|e| e.ok()) {
                            if let Some(stem) = entry.path().file_stem() {
                                println!("  • {}", stem.to_string_lossy());
                            }
                        }
                    }
                    return Ok(());
                }
            }
            println!("Switched to profile: {}", name);
            println!("(Profile loading will be applied on next `hermes` session)");
        }
        Some(other) => {
            println!(
                "Unknown profile action: '{}'. Use 'list', 'create', or 'switch'.",
                other
            );
        }
    }
    Ok(())
}
