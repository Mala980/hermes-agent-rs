#![allow(
    clippy::type_complexity,
    clippy::print_literal,
    clippy::field_reassign_with_default,
    clippy::useless_format,
    clippy::needless_borrow,
    clippy::unwrap_or_default,
    unused_variables
)]
//! Hermes Agent — binary entry point.
//!
//! Initializes logging, parses CLI arguments, and dispatches to the
//! appropriate subcommand handler (see the `cmd` module).

use clap::Parser;
use hermes_cli::cli::{Cli, CliCommand};
use hermes_cli::cmd;
use hermes_telemetry::init_telemetry_from_env;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // In TUI mode, redirect tracing logs to a file — writing to stdout/stderr
    // corrupts the alternate-screen rendering (log lines overwrite TUI widgets).
    let is_tui = matches!(
        cli.effective_command(),
        CliCommand::Hermes | CliCommand::Tui | CliCommand::Chat { .. }
    );
    init_tracing(cli.verbose, is_tui);

    tracing::debug!("Hermes Agent starting");

    let result: Result<(), String> = match cli.effective_command() {
        CliCommand::Hermes => {
            eprintln!("Interactive mode is temporarily unavailable in this build.");
            eprintln!("Use `hermes chat --query \"...\"` for one-shot queries.");
            Ok(())
        }
        CliCommand::Chat {
            query,
            preload_skill,
            yolo,
        } => hermes_cli::commands::handle_cli_chat(query, preload_skill, yolo)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Model {
            provider_model,
            sub_action,
            sub_arg,
        } => {
            // "hermes model provider list" → delegate to runtime-provider handler
            if provider_model.as_deref() == Some("provider") {
                hermes_cli::commands::handle_cli_runtime_provider(sub_action, sub_arg)
                    .await
                    .map_err(|e| e.to_string())
            } else {
                eprintln!("Model switching via CLI is temporarily unavailable.");
                Ok(())
            }
        }
        CliCommand::Providers { action } => {
            hermes_cli::commands::handle_cli_runtime_provider(action, None)
                .await
                .map_err(|e| e.to_string())
        }
        CliCommand::Skills {
            action,
            name,
            extra,
        } => hermes_cli::commands::handle_cli_skills(action, name, extra)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Plugins {
            action,
            name,
            git_ref,
            allow_untrusted_git_host,
        } => hermes_cli::commands::handle_cli_plugins(
            action,
            name,
            git_ref,
            allow_untrusted_git_host,
        )
        .await
        .map_err(|e| e.to_string()),
        CliCommand::Memory {
            action,
            arg,
            target,
        } => {
            if action.as_deref() == Some("setup") || (action.is_none() && arg.is_some()) {
                hermes_cli::commands::handle_cli_memory_setup(action, arg)
                    .await
                    .map_err(|e| e.to_string())
            } else {
                hermes_cli::commands::handle_cli_memory(action, arg, target)
                    .await
                    .map_err(|e| e.to_string())
            }
        }
        CliCommand::Mcp { action, server } => hermes_cli::commands::handle_cli_mcp(action, server)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Sessions {
            action,
            id,
            name,
            output,
        } => hermes_cli::commands::handle_cli_sessions(action, id, name)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Acp { action } => hermes_cli::commands::handle_cli_acp(action)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Backup { output } => hermes_cli::commands::handle_cli_backup(output)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Import { path } => hermes_cli::commands::handle_cli_import(path)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Version => {
            hermes_cli::commands::handle_cli_version().map_err(|e| e.to_string())
        }
        CliCommand::Setup => cmd::misc::run_setup().await.map_err(|e| e.to_string()),
        CliCommand::Completion { shell } => {
            cmd::misc::run_completion(shell).map_err(|e| e.to_string())
        }
        CliCommand::Doctor => cmd::misc::run_doctor(cli).await.map_err(|e| e.to_string()),
        CliCommand::Lumio { action, model } => cmd::misc::run_lumio(action, model)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Status { full, deep } => cmd::misc::run_status(cli, full, deep)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Uninstall { yes } => cmd::misc::run_uninstall(yes)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Update => cmd::misc::run_update().await.map_err(|e| e.to_string()),
        // ── Ported from hermes-rs (working) ─────────────────────────
        CliCommand::Voice { cmd } => cmd::voice::cmd_voice(&cli, &cmd)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Webhook { port } => cmd::webhook::cmd_webhook(&cli, port)
            .await
            .map_err(|e| e.to_string()),
        // ── Quick-wins (working) ────────────────────────────────────
        CliCommand::Claw { action } => hermes_cli::commands::handle_cli_claw(action)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Pairing { action, device_id } => {
            hermes_cli::commands::handle_cli_pairing(action, device_id)
                .await
                .map_err(|e| e.to_string())
        }
        CliCommand::Pause { reason } => cmd::estop::cmd_pause(reason)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Resume => cmd::estop::cmd_resume().await.map_err(|e| e.to_string()),
        CliCommand::Browser { action, browser } => cmd::browser::cmd_browser(cli, action, browser)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Slack {
            action,
            name,
            description,
            long_description,
            long_description_file,
            slashes_only,
            agent_view,
            no_assistant,
            write,
        } => cmd::slack::cmd_slack(
            cli,
            action,
            name,
            description,
            long_description,
            long_description_file,
            slashes_only,
            agent_view,
            no_assistant,
            write,
        )
        .await
        .map_err(|e| e.to_string()),
        // ── Phase 5: new utilities ──────────────────────────────────
        CliCommand::Logs(args) => cmd::logs::run(&args).await.map_err(|e| e.to_string()),
        CliCommand::Profile(args) => cmd::profile::run(&args).await.map_err(|e| e.to_string()),
        CliCommand::Kanban(args) => cmd::kanban::run(&args).await.map_err(|e| e.to_string()),
        CliCommand::Checkpoints(args) => cmd::checkpoints::run(&args)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Usage(args) => cmd::usage_cmd::run(&args).await.map_err(|e| e.to_string()),
        CliCommand::PromptSize(args) => cmd::prompt_size::run(&args)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Worktree(args) => cmd::worktree::run(&args).await.map_err(|e| e.to_string()),
        CliCommand::Monitoring(args) => {
            cmd::monitoring::run(&args).await.map_err(|e| e.to_string())
        }
        CliCommand::Insights(args) => cmd::insights::run(&args).await.map_err(|e| e.to_string()),
        CliCommand::Debug(args) => cmd::debug_cmd::run(&args).await.map_err(|e| e.to_string()),
        CliCommand::Dump(args) => cmd::dump_cmd::run(&args).await.map_err(|e| e.to_string()),
        CliCommand::Dashboard(args) => cmd::advanced::run_dashboard(&args)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Hooks(args) => cmd::advanced::run_hooks(&args)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Bundles(args) => cmd::advanced::run_bundles(&args)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Vault(args) => cmd::advanced::run_vault(&args)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Security(args) => cmd::advanced::run_security(&args)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Connectors(args) => cmd::advanced::run_connectors(&args)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Verify(args) => cmd::advanced::run_verify(&args)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Console(args) => cmd::advanced::run_console(&args)
            .await
            .map_err(|e| e.to_string()),
        // ── Restored fixed modules ──
        CliCommand::Auth { action, provider } => cmd::auth::run_auth(cli, action, provider, false)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Batch { input, out } => cmd::batch::cmd_batch(&cli, &input, out.as_deref())
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Cloud { action } => cmd::cloud::run_cloud(
            action, None, None, None, None, None, None, None, false, false, 30, 10, false,
        )
        .await
        .map_err(|e| e.to_string()),
        CliCommand::Config { action, key, value } => {
            cmd::config::run_config(cli, action, key, value)
                .await
                .map_err(|e| e.to_string())
        }
        CliCommand::Cron { action } => {
            cmd::cron::run_cron(cli, action, None, None, None, None, None)
                .await
                .map_err(|e| e.to_string())
        }
        CliCommand::Eval(cmd) => cmd::eval::cmd_eval(&cli, &cmd)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Fallback { action } => {
            cmd::fallback::cmd_fallback(cli, action, None, None, None, None)
                .await
                .map_err(|e| e.to_string())
        }
        CliCommand::Gateway { action } => cmd::gateway::run_gateway(cli, action, false, false)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Interactive => cmd::interactive::run_interactive(cli)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Misc { action } => {
            eprintln!("misc: action {:?} not yet wired", action);
            Ok(())
        }
        CliCommand::Moa { action } => cmd::moa::cmd_moa(cli, action, None, vec![], None)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Serve { action } => {
            let opts = cmd::serve::ServeOptions {
                action,
                host: "127.0.0.1".to_string(),
                port: 8080,
                no_gateway: false,
                no_cron: false,
                readonly: false,
                yes: false,
            };
            cmd::serve::run_serve(cli, opts)
                .await
                .map_err(|e| e.to_string())
        }
        CliCommand::Skin { action } => cmd::skin::cmd_skin(cli, action, None, None, None)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Tools { action } => cmd::tools::run_tools(cli, action, None)
            .await
            .map_err(|e| e.to_string()),
        CliCommand::Tui => cmd::tui_cmd::cmd_tui(cli).await.map_err(|e| e.to_string()),
        CliCommand::Run { prompt } => cmd::run::cmd_run(cli, &prompt)
            .await
            .map_err(|e| e.to_string()),
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

/// Initialize the tracing subscriber with env filter.
fn init_tracing(verbose: bool, tui_mode: bool) {
    if tui_mode {
        // Redirect logs to a file so they don't corrupt the TUI.
        let log_dir = hermes_config::hermes_home().join("logs");
        let _ = std::fs::create_dir_all(&log_dir);
        let log_file = log_dir.join("tui.log");
        let level = if verbose { "debug" } else { "info" };
        if hermes_telemetry::init_telemetry_to_file(&log_file, level).is_ok() {
            return;
        }
        // Fallback: disable logging rather than corrupting the TUI.
        let _ = hermes_telemetry::init_telemetry_to_file(std::path::Path::new("/dev/null"), "off");
        return;
    }
    let default = if verbose { "debug" } else { "info" };
    init_telemetry_from_env("hermes-cli", default);
}
