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
use hermes_telemetry::init_telemetry_from_env;

mod cmd;
mod oauth_store;

use cmd::auth::run_auth;
use cmd::cloud::run_cloud;
use cmd::config::run_config;
use cmd::cron::run_cron;
use cmd::gateway::run_gateway;
use cmd::interactive::{run_interactive, run_model};
use cmd::misc::{
    run_completion, run_doctor, run_dump, run_logs, run_lumio, run_profile, run_setup, run_status,
    run_uninstall, run_update,
};
use cmd::serve::{run_serve, ServeOptions};
use cmd::tools::run_tools;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Initialize tracing
    init_tracing(cli.verbose);

    tracing::debug!("Hermes Agent starting");

    let result = match cli.effective_command() {
        CliCommand::Hermes => run_interactive(cli).await,
        CliCommand::Chat {
            query,
            preload_skill,
            yolo,
        } => hermes_cli::commands::handle_cli_chat(query, preload_skill, yolo).await,
        CliCommand::Model {
            provider_model,
            sub_action,
            sub_arg,
        } => {
            // "hermes model provider list" → delegate to runtime-provider handler
            if provider_model.as_deref() == Some("provider") {
                hermes_cli::commands::handle_cli_runtime_provider(sub_action, sub_arg).await
            } else {
                run_model(cli, provider_model).await
            }
        }
        CliCommand::Providers { action } => {
            hermes_cli::commands::handle_cli_runtime_provider(action, None).await
        }
        CliCommand::Tools { action, name } => run_tools(cli, action, name).await,
        CliCommand::Config { action, key, value } => {
            // "hermes config set region <value>" → delegate to region handler
            if action.as_deref() == Some("set") && key.as_deref() == Some("region") {
                hermes_cli::commands::handle_cli_region(Some("set".to_string()), value).await
            } else {
                run_config(cli, action, key, value).await
            }
        }
        CliCommand::Gateway {
            action,
            platform,
            readonly,
            yes,
        } => {
            // "hermes gateway setup whatsapp" → delegate to whatsapp handler
            if let (Some("setup"), Some(plat)) = (action.as_deref(), platform.as_deref()) {
                match plat {
                    "whatsapp" | "wa" => {
                        hermes_cli::commands::handle_cli_whatsapp(Some("setup".to_string())).await
                    }
                    _ => run_gateway(cli, action, readonly, yes).await,
                }
            } else {
                run_gateway(cli, action, readonly, yes).await
            }
        }
        CliCommand::Setup => run_setup().await,
        CliCommand::Doctor => run_doctor(cli).await,
        CliCommand::Update => run_update().await,
        CliCommand::Status { full, deep } => run_status(cli, full, deep).await,
        CliCommand::Logs {
            lines,
            follow,
            list,
            name,
        } => run_logs(cli, lines, follow, list, name).await,
        CliCommand::Profile { action, name } => run_profile(cli, action, name).await,
        CliCommand::Auth {
            action,
            provider,
            qr,
            model,
        } => {
            // "hermes auth login lumio" → delegate to lumio handler
            if action.as_deref() == Some("login") && provider.as_deref() == Some("lumio") {
                run_lumio(Some("login".to_string()), model).await
            } else {
                run_auth(cli, action, provider, qr).await
            }
        }
        CliCommand::Skills {
            action,
            name,
            extra,
        } => hermes_cli::commands::handle_cli_skills(action, name, extra).await,
        CliCommand::Plugins {
            action,
            name,
            git_ref,
            allow_untrusted_git_host,
        } => {
            hermes_cli::commands::handle_cli_plugins(
                action,
                name,
                git_ref,
                allow_untrusted_git_host,
            )
            .await
        }
        CliCommand::Memory {
            action,
            arg,
            target,
        } => {
            // "hermes memory setup redis" → delegate to memory-setup handler.
            // Legacy "hermes memory <provider>" (no action) also goes there.
            if action.as_deref() == Some("setup") || (action.is_none() && arg.is_some()) {
                hermes_cli::commands::handle_cli_memory_setup(action, arg).await
            } else {
                hermes_cli::commands::handle_cli_memory(action, arg, target).await
            }
        }
        CliCommand::Mcp { action, server } => {
            hermes_cli::commands::handle_cli_mcp(action, server).await
        }
        CliCommand::Sessions {
            action,
            id,
            name,
            output,
        } => {
            // "hermes sessions dump" → delegate to dump
            if action.as_deref() == Some("dump") {
                run_dump(cli, id, output).await
            } else if action.as_deref() == Some("stats") {
                hermes_cli::commands::handle_cli_insights(30, None).await
            } else {
                hermes_cli::commands::handle_cli_sessions(action, id, name).await
            }
        }
        CliCommand::Acp { action } => hermes_cli::commands::handle_cli_acp(action).await,
        CliCommand::Backup { output } => hermes_cli::commands::handle_cli_backup(output).await,
        CliCommand::Import { path } => hermes_cli::commands::handle_cli_import(path).await,
        CliCommand::Version => hermes_cli::commands::handle_cli_version(),
        CliCommand::Cron {
            action,
            id,
            schedule,
            prompt,
            timezone,
            overlap,
        } => run_cron(cli, action, id, schedule, prompt, timezone, overlap).await,
        CliCommand::Serve {
            action,
            host,
            port,
            no_gateway,
            no_cron,
            readonly,
            yes,
        } => {
            run_serve(
                cli,
                ServeOptions {
                    action,
                    host,
                    port,
                    no_gateway,
                    no_cron,
                    readonly,
                    yes,
                },
            )
            .await
        }
        CliCommand::Cloud {
            action,
            agent_id,
            prompt,
            env,
            attempts,
            email,
            password,
            url_override,
            register,
            once,
            poll_interval_secs,
            limit,
            json,
        } => {
            run_cloud(
                action,
                agent_id,
                prompt,
                env,
                attempts,
                email,
                password,
                url_override,
                register,
                once,
                poll_interval_secs,
                limit,
                json,
            )
            .await
        }
        CliCommand::Completion { shell } => run_completion(shell),
        CliCommand::Uninstall { yes } => run_uninstall(yes).await,
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

/// Initialize the tracing subscriber with env filter.
fn init_tracing(verbose: bool) {
    let default = if verbose { "debug" } else { "info" };
    init_telemetry_from_env("hermes-cli", default);
}
