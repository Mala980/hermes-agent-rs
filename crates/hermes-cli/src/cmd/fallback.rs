//! `hermes fallback` — manage the fallback provider chain.
//!
//! Ported from NousResearch/hermes-agent's `hermes_cli/fallback_cmd.py`.
//!
//! The chain is tried in order when the primary route fails (rate-limit,
//! 5xx, connection errors).
//!
//! Divergence from the original: this fork's `GatewayConfig` has no
//! `fallback_providers` field, so the chain is kept in a sidecar file —
//! `$HERMES_HOME/fallback_chain.json` — instead of inside `config.yaml`.
//! The runtime agent loop does not consult this file yet; it records the
//! user's preferred order so it can be wired in later without losing data.
//!
//!   hermes fallback add --provider openai --model gpt-4o [--base-url URL]
//!   hermes fallback remove 1            # 1-based index into the listed chain
//!   hermes fallback remove openai/gpt-4o

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use crate::cli::Cli;
use hermes_config::{hermes_home, load_user_config_file, GatewayConfig};
use hermes_core::AgentError;

/// One fallback hop: provider + model, optional base URL override.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FallbackProviderEntry {
    provider: String,
    model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
}

fn chain_file(cli: &Cli) -> PathBuf {
    cli.config_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(hermes_home)
        .join("fallback_chain.json")
}

fn load_chain(cli: &Cli) -> Result<Vec<FallbackProviderEntry>, AgentError> {
    let path = chain_file(cli);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| AgentError::Io(format!("read {}: {}", path.display(), e)))?;
    serde_json::from_str(&raw)
        .map_err(|e| AgentError::Config(format!("parse {}: {}", path.display(), e)))
}

fn save_chain(cli: &Cli, chain: &[FallbackProviderEntry]) -> Result<(), AgentError> {
    let path = chain_file(cli);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AgentError::Io(format!("create {}: {}", parent.display(), e)))?;
    }
    let body = serde_json::to_string_pretty(chain)
        .map_err(|e| AgentError::Config(format!("serialize fallback chain: {}", e)))?;
    std::fs::write(&path, body)
        .map_err(|e| AgentError::Io(format!("write {}: {}", path.display(), e)))?;
    Ok(())
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

fn describe_primary(cli: &Cli) -> Option<String> {
    let path = cli
        .config_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(hermes_home)
        .join("config.yaml");
    let cfg: GatewayConfig = load_user_config_file(&path).ok()?;
    cfg.model.as_deref().filter(|m| !m.is_empty()).map(|m| {
        // `model` may be "provider:model" or a bare model name.
        match m.split_once(':') {
            Some((provider, model)) => format!("{}  (via {})", model, provider),
            None => m.to_string(),
        }
    })
}

fn cmd_list(cli: &Cli) {
    let chain = load_chain(cli).unwrap_or_default();
    if chain.is_empty() {
        println!("\n  No fallback providers configured.\n");
        println!("  Add one with:  hermes fallback add --provider <id> --model <name>\n");
        return;
    }
    println!();
    if let Some(primary) = describe_primary(cli) {
        println!("  Primary:   {}\n", primary);
    }
    print_chain("Fallback chain", &chain);
    println!("  Tried in order when the primary fails (rate-limit, 5xx, connection errors).");
    println!(
        "  (stored in {}; not yet consumed by the agent runtime)\n",
        chain_file(cli).display()
    );
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

    let mut chain = load_chain(cli)?;
    let new_entry = FallbackProviderEntry {
        provider,
        model,
        base_url: base_url.filter(|u| !u.is_empty()),
    };

    if chain.iter().any(|e| {
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

    chain.push(new_entry.clone());
    save_chain(cli, &chain)?;
    println!("\n  Added fallback: {}", format_entry(&new_entry));
    println!("  Chain is now {} long.\n", entries(chain.len()));
    println!("  Run `hermes fallback list` to view, or `hermes fallback remove` to delete.");
    Ok(())
}

fn cmd_remove(cli: &Cli, target: Option<String>) -> Result<(), AgentError> {
    let mut chain = load_chain(cli)?;
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

    let removed = chain.remove(idx);
    save_chain(cli, &chain)?;
    println!("\n  Removed fallback: {}", format_entry(&removed));
    if chain.is_empty() {
        println!("  Fallback chain is now empty.\n");
    } else {
        println!("  Chain is now {} long.\n", entries(chain.len()));
    }
    Ok(())
}

fn cmd_clear(cli: &Cli) -> Result<(), AgentError> {
    let chain = load_chain(cli)?;
    if chain.is_empty() {
        println!("\n  No fallback providers configured — nothing to clear.\n");
        return Ok(());
    }
    println!();
    print_chain("Current fallback chain", &chain);
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
    save_chain(cli, &[])?;
    println!("\n  Fallback chain cleared.\n");
    Ok(())
}

/// Handle `hermes fallback [list|add|remove|clear]`.
pub async fn cmd_fallback(
    cli: Cli,
    action: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    target: Option<String>,
) -> Result<(), AgentError> {
    match action.as_deref().unwrap_or("list") {
        "list" | "ls" => {
            cmd_list(&cli);
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
