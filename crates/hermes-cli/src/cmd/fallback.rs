//! `hermes fallback` — manage the fallback provider chain.
//!
//! Ported from NousResearch/hermes-agent's `hermes_cli/fallback_cmd.py`.
//!
//! The chain is stored in config as `fallback_providers` (tried in order when
//! the primary route fails: rate-limit, 5xx, connection errors).
//!
//! Divergence from the original: `add` and `remove` are non-interactive in
//! this port (the original launched the `hermes model` picker / a curses
//! menu). Use explicit flags instead:
//!
//!   hermes fallback add --provider openai --model gpt-4o [--base-url URL]
//!   hermes fallback remove 1            # 1-based index into the listed chain
//!   hermes fallback remove openai/gpt-4o

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use hermes_cli::cli::Cli;
use hermes_config::{
    hermes_home, load_user_config_file, save_config_yaml, FallbackProviderEntry, GatewayConfig,
};
use hermes_core::AgentError;

fn config_file(cli: &Cli) -> PathBuf {
    cli.config_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(hermes_home)
        .join("config.yaml")
}

fn load_disk(cli: &Cli) -> Result<(PathBuf, GatewayConfig), AgentError> {
    let path = config_file(cli);
    let cfg = load_user_config_file(&path).map_err(|e| AgentError::Config(e.to_string()))?;
    Ok((path, cfg))
}

fn save_disk(path: &std::path::Path, cfg: &GatewayConfig) -> Result<(), AgentError> {
    save_config_yaml(path, cfg).map_err(|e| AgentError::Config(e.to_string()))
}

fn entries(n: usize) -> String {
    format!("{} {}", n, if n == 1 { "entry" } else { "entries" })
}

fn format_entry(entry: &FallbackProviderEntry) -> String {
    let base = entry
        .base_url
        .as_deref()
        .filter(|u| !u.is_empty())
        .map(|u| format!("  [{}]", u))
        .unwrap_or_default();
    let model = if entry.model.is_empty() {
        "?"
    } else {
        &entry.model
    };
    let provider = if entry.provider.is_empty() {
        "?"
    } else {
        &entry.provider
    };
    format!("{}  (via {}){}", model, provider, base)
}

fn print_chain(heading: &str, chain: &[FallbackProviderEntry]) {
    println!("  {} ({}):", heading, entries(chain.len()));
    for (i, entry) in chain.iter().enumerate() {
        println!("    {}. {}", i + 1, format_entry(entry));
    }
}

fn describe_primary(cfg: &GatewayConfig) -> Option<String> {
    cfg.model.as_deref().filter(|m| !m.is_empty()).map(|m| {
        // `model` may be "provider:model" or a bare model name.
        match m.split_once(':') {
            Some((provider, model)) => format!("{}  (via {})", model, provider),
            None => m.to_string(),
        }
    })
}

fn cmd_list(cfg: &GatewayConfig) {
    let chain = &cfg.fallback_providers;
    if chain.is_empty() {
        println!("\n  No fallback providers configured.\n");
        println!("  Add one with:  hermes fallback add --provider <id> --model <name>\n");
        return;
    }
    println!();
    if let Some(primary) = describe_primary(cfg) {
        println!("  Primary:   {}\n", primary);
    }
    print_chain("Fallback chain", chain);
    println!("  Tried in order when the primary fails (rate-limit, 5xx, connection errors).\n");
}

fn cmd_add(
    cli: &Cli,
    provider: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
) -> Result<(), AgentError> {
    let provider = provider.ok_or_else(|| {
        AgentError::Config(
            "Missing --provider. Usage: hermes fallback add --provider <id> --model <name> [--base-url <url>]"
                .into(),
        )
    })?;
    let model = model.ok_or_else(|| {
        AgentError::Config(
            "Missing --model. Usage: hermes fallback add --provider <id> --model <name> [--base-url <url>]"
                .into(),
        )
    })?;

    let (path, mut cfg) = load_disk(cli)?;
    let new_entry = FallbackProviderEntry {
        provider: provider.clone(),
        model: model.clone(),
        base_url: base_url.filter(|u| !u.is_empty()),
    };

    if cfg.fallback_providers.iter().any(|e| {
        e.provider == new_entry.provider
            && e.model == new_entry.model
            && e.base_url == new_entry.base_url
    }) {
        println!(
            "\n  {} is already in the fallback chain — skipped.\n",
            format_entry(&new_entry)
        );
        return Ok(());
    }

    cfg.fallback_providers.push(new_entry.clone());
    save_disk(&path, &cfg)?;
    println!("\n  Added fallback: {}", format_entry(&new_entry));
    println!(
        "  Chain is now {} long.\n",
        entries(cfg.fallback_providers.len())
    );
    println!("  Run `hermes fallback list` to view, or `hermes fallback remove` to delete.");
    Ok(())
}

fn cmd_remove(cli: &Cli, target: Option<String>) -> Result<(), AgentError> {
    let (path, mut cfg) = load_disk(cli)?;
    let chain = &cfg.fallback_providers;
    if chain.is_empty() {
        println!("\n  No fallback providers configured — nothing to remove.\n");
        return Ok(());
    }
    let target = target.ok_or_else(|| {
        let mut msg =
            String::from("Missing entry. Usage: hermes fallback remove <n>\n\n  Current chain:\n");
        for (i, entry) in chain.iter().enumerate() {
            msg.push_str(&format!("    {}. {}\n", i + 1, format_entry(entry)));
        }
        AgentError::Config(msg)
    })?;

    // 1-based index first, then provider/model substring match.
    let idx = if let Ok(n) = target.parse::<usize>() {
        if n >= 1 && n <= chain.len() {
            Some(n - 1)
        } else {
            None
        }
    } else {
        chain.iter().position(|e| {
            let hay = format!("{}/{}", e.provider, e.model).to_lowercase();
            hay.contains(&target.to_lowercase())
        })
    };

    let idx = idx.ok_or_else(|| {
        AgentError::Config(format!(
            "No fallback entry matches '{}'. Run `hermes fallback list` to see the chain.",
            target
        ))
    })?;

    let removed = cfg.fallback_providers.remove(idx);
    save_disk(&path, &cfg)?;
    println!("\n  Removed fallback: {}", format_entry(&removed));
    if cfg.fallback_providers.is_empty() {
        println!("  Fallback chain is now empty.\n");
    } else {
        println!(
            "  Chain is now {} long.\n",
            entries(cfg.fallback_providers.len())
        );
    }
    Ok(())
}

fn cmd_clear(cli: &Cli) -> Result<(), AgentError> {
    let (path, mut cfg) = load_disk(cli)?;
    if cfg.fallback_providers.is_empty() {
        println!("\n  No fallback providers configured — nothing to clear.\n");
        return Ok(());
    }
    println!();
    print_chain("Current fallback chain", &cfg.fallback_providers);
    print!("  Clear all entries? [y/N]: ");
    io::stdout().flush().ok();
    let answer = io::stdin()
        .lock()
        .lines()
        .next()
        .and_then(|l| l.ok())
        .unwrap_or_default();
    if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
        println!("  Cancelled — no change.");
        return Ok(());
    }
    cfg.fallback_providers.clear();
    save_disk(&path, &cfg)?;
    println!("\n  Fallback chain cleared.\n");
    Ok(())
}

/// Handle `hermes fallback [list|add|remove|clear]`.
pub(crate) async fn cmd_fallback(
    cli: Cli,
    action: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    target: Option<String>,
) -> Result<(), AgentError> {
    match action.as_deref().unwrap_or("list") {
        "list" | "ls" => {
            let (_, cfg) = load_disk(&cli)?;
            cmd_list(&cfg);
            Ok(())
        }
        "add" => cmd_add(&cli, provider, model, base_url),
        "remove" | "rm" => cmd_remove(&cli, target),
        "clear" => cmd_clear(&cli),
        other => Err(AgentError::Config(format!(
            "Unknown fallback subcommand: '{}'. Use one of: list, add, remove, clear",
            other
        ))),
    }
}
