//! `hermes moa` — manage Mixture-of-Agents model presets.
//!
//! Ported from NousResearch/hermes-agent's `hermes_cli/moa_cmd.py` (defaults
//! from `hermes_cli/moa_config.py`).
//!
//! Presets live in config under `moa`:
//! `{presets: {name: {reference_models: [...], aggregator: {...}}},
//!   default_preset, active_preset}`.
//!
//! Divergence from the original: `configure` is non-interactive in this port
//! (the original walked an interactive model picker). Pass explicit slots:
//!
//!   hermes moa configure [name] \
//!     --reference openai:gpt-4o --reference anthropic:claude-opus-4-6 \
//!     --aggregator openai:gpt-4o

use std::path::PathBuf;

use hermes_cli::cli::Cli;
use hermes_config::{
    hermes_home, load_user_config_file, save_config_yaml, GatewayConfig, MoaConfig, MoaModelSlot,
    MoaPreset,
};
use hermes_core::AgentError;

/// Default preset name (mirrors `DEFAULT_MOA_PRESET_NAME`).
const DEFAULT_PRESET_NAME: &str = "default";

/// Default reference slots (mirrors `DEFAULT_MOA_REFERENCE_MODELS`).
fn default_reference_models() -> Vec<MoaModelSlot> {
    vec![
        MoaModelSlot {
            provider: "openai-codex".into(),
            model: "gpt-5.5".into(),
        },
        MoaModelSlot {
            provider: "openrouter".into(),
            model: "deepseek/deepseek-v4-pro".into(),
        },
    ]
}

/// Default aggregator slot (mirrors `DEFAULT_MOA_AGGREGATOR`).
fn default_aggregator() -> MoaModelSlot {
    MoaModelSlot {
        provider: "openrouter".into(),
        model: "anthropic/claude-opus-4.8".into(),
    }
}

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

/// Parse a `provider:model` slot (mirrors the original's slot dicts).
fn parse_slot(raw: &str) -> Result<MoaModelSlot, AgentError> {
    match raw.split_once(':') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => Ok(MoaModelSlot {
            provider: provider.trim().to_string(),
            model: model.trim().to_string(),
        }),
        _ => Err(AgentError::Config(format!(
            "Invalid model slot '{}'. Use provider:model, e.g. openai:gpt-4o.",
            raw
        ))),
    }
}

fn format_slot(slot: &MoaModelSlot) -> String {
    format!("{}:{}", slot.provider, slot.model)
}

fn print_config(moa: &MoaConfig) {
    if moa.presets.is_empty() {
        println!("\n  No MoA presets configured.\n");
        println!("  Create one with: hermes moa configure [name] --reference <provider:model> --aggregator <provider:model>\n");
        return;
    }
    println!();
    let mut names: Vec<&String> = moa.presets.keys().collect();
    names.sort();
    for name in names {
        let preset = &moa.presets[name];
        let marks = [
            (
                moa.default_preset.as_deref() == Some(name.as_str()),
                "default",
            ),
            (
                moa.active_preset.as_deref() == Some(name.as_str()),
                "active",
            ),
        ];
        let tag: Vec<&str> = marks
            .iter()
            .filter(|(on, _)| *on)
            .map(|(_, label)| *label)
            .collect();
        let tag = if tag.is_empty() {
            String::new()
        } else {
            format!(" [{}]", tag.join(", "))
        };
        println!("  {}{}", name, tag);
        let refs: Vec<String> = preset.reference_models.iter().map(format_slot).collect();
        println!(
            "    reference models: {}",
            if refs.is_empty() {
                "(none)".to_string()
            } else {
                refs.join(", ")
            }
        );
        println!(
            "    aggregator:       {}",
            preset
                .aggregator
                .as_ref()
                .map(format_slot)
                .unwrap_or_else(|| "(none)".to_string())
        );
    }
    println!();
}

fn cmd_configure(
    cli: &Cli,
    name: Option<String>,
    reference: Vec<String>,
    aggregator: Option<String>,
) -> Result<(), AgentError> {
    let (path, mut cfg) = load_disk(cli)?;
    let mut moa = std::mem::take(&mut cfg.moa);
    let preset_name = name
        .or_else(|| moa.default_preset.clone())
        .unwrap_or_else(|| DEFAULT_PRESET_NAME.to_string());
    let preset_name = preset_name.trim().to_string();

    let current = moa.presets.get(&preset_name).cloned().unwrap_or(MoaPreset {
        reference_models: default_reference_models(),
        aggregator: Some(default_aggregator()),
    });

    let refs: Vec<MoaModelSlot> = if reference.is_empty() {
        current.reference_models.clone()
    } else {
        reference
            .iter()
            .map(|r| parse_slot(r))
            .collect::<Result<_, _>>()?
    };
    if refs.is_empty() {
        return Err(AgentError::Config(
            "Pick at least one reference model: --reference <provider:model>".into(),
        ));
    }
    let agg = match aggregator {
        Some(a) => Some(parse_slot(&a)?),
        None => current.aggregator.clone(),
    };

    moa.presets.insert(
        preset_name.clone(),
        MoaPreset {
            reference_models: refs,
            aggregator: agg,
        },
    );
    if moa.default_preset.is_none() {
        moa.default_preset = Some(preset_name.clone());
    }
    cfg.moa = moa;
    save_disk(&path, &cfg)?;
    println!("Saved MoA preset: {}", preset_name);
    print_config(&cfg.moa);
    Ok(())
}

fn cmd_delete(cli: &Cli, name: Option<String>) -> Result<(), AgentError> {
    let preset_name = name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| AgentError::Config("Usage: hermes moa delete <name>".into()))?;

    let (path, mut cfg) = load_disk(cli)?;
    let mut moa = std::mem::take(&mut cfg.moa);
    if !moa.presets.contains_key(&preset_name) {
        return Err(AgentError::Config(format!(
            "Unknown MoA preset: {}",
            preset_name
        )));
    }
    if moa.presets.len() <= 1 {
        return Err(AgentError::Config(
            "Cannot delete the only MoA preset".into(),
        ));
    }
    moa.presets.remove(&preset_name);
    if moa.default_preset.as_deref() == Some(preset_name.as_str()) {
        moa.default_preset = moa.presets.keys().next().cloned();
    }
    if moa.active_preset.as_deref() == Some(preset_name.as_str()) {
        moa.active_preset = None;
    }
    cfg.moa = moa;
    save_disk(&path, &cfg)?;
    println!("Deleted MoA preset: {}", preset_name);
    Ok(())
}

/// Handle `hermes moa [list|configure|delete]`.
pub(crate) async fn cmd_moa(
    cli: Cli,
    action: Option<String>,
    name: Option<String>,
    reference: Vec<String>,
    aggregator: Option<String>,
) -> Result<(), AgentError> {
    match action.as_deref().unwrap_or("list") {
        "list" | "ls" => {
            let (_, cfg) = load_disk(&cli)?;
            print_config(&cfg.moa);
            Ok(())
        }
        "config" | "configure" => cmd_configure(&cli, name, reference, aggregator),
        "delete" => cmd_delete(&cli, name),
        other => Err(AgentError::Config(format!(
            "Unknown moa subcommand: '{}'. Use one of: list, configure, delete",
            other
        ))),
    }
}
