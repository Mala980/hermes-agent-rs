//! `hermes skin` — list, switch, and tweak skins from the CLI.
//!
//! Ported from NousResearch/hermes-agent's `hermes_cli/skin_cmd.py`.
//!
//! - `skin` / `skin list` — list built-in + user-installed skins, marking the
//!   active one with `*`.
//! - `skin use <name>` — persist the choice as `display.skin` in config.
//! - `skin set <key> <value> [--skin <name>]` — change ONE color of the active
//!   skin in place (`<name>.yaml` under `$HERMES_HOME/skins/`). Editing a
//!   built-in skin forks it into `<name>-custom.yaml` so the built-in stays
//!   intact; the fork is then activated.
//!
//! Divergence from the original: the fork's skin engine (`skin_engine.rs`)
//! only resolves TUI themes, so a forked `<name>-custom.yaml` starts from the
//! overridden color alone instead of the built-in's full palette.

use std::path::PathBuf;

use hermes_cli::cli::Cli;
use hermes_config::{
    hermes_home, load_user_config_file, save_config_yaml, user_config_field_display,
};
use hermes_core::AgentError;

/// Built-in skins (names + descriptions, from the original `skin_engine.py`).
const BUILTIN_SKINS: &[(&str, &str)] = &[
    ("default", "Classic Hermes — gold and kawaii"),
    ("ares", "War-god theme — crimson and bronze"),
    ("mono", "Monochrome — clean grayscale"),
    ("slate", "Cool blue — developer-focused"),
    ("daylight", "Light theme — warm paper"),
    ("poseidon", "Ocean-god theme — deep blue and seafoam"),
    (
        "sisyphus",
        "Sisyphean theme — austere grayscale with persistence",
    ),
    ("charizard", "Volcanic theme — burnt orange and ember"),
];

/// One skin row for `skin list`.
struct SkinRow {
    name: String,
    description: String,
    source: &'static str,
}

/// List all available skins: built-ins first, then user YAML skins.
/// User skins never shadow built-ins (same rule as the original).
fn list_skins() -> Vec<SkinRow> {
    let mut rows: Vec<SkinRow> = BUILTIN_SKINS
        .iter()
        .map(|(name, desc)| SkinRow {
            name: name.to_string(),
            description: desc.to_string(),
            source: "builtin",
        })
        .collect();

    let skins_dir = hermes_home().join("skins");
    if let Ok(entries) = std::fs::read_dir(&skins_dir) {
        let mut yaml_files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "yaml" || e == "yml"))
            .collect();
        yaml_files.sort();
        for path in yaml_files {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(&text) {
                    let name = value
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| {
                            path.file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or("unknown")
                                .to_string()
                        });
                    if rows.iter().any(|r| r.name == name) {
                        continue; // user skins never shadow built-ins
                    }
                    let description = value
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    rows.push(SkinRow {
                        name,
                        description,
                        source: "user",
                    });
                }
            }
        }
    }
    rows
}

/// Read the active skin name from config (`display.skin`), default `"default"`.
fn active_skin_name(home: &std::path::Path) -> String {
    let cfg_path = home.join("config.yaml");
    load_user_config_file(&cfg_path)
        .ok()
        .and_then(|cfg| user_config_field_display(&cfg, "display.skin").ok())
        .map(|s| {
            if s == "(not set)" {
                "default".to_string()
            } else {
                s
            }
        })
        .unwrap_or_else(|| "default".to_string())
}

/// Persist `display.skin` in the user's config file.
fn persist_active_skin(home: &std::path::Path, name: &str) -> Result<(), AgentError> {
    let cfg_path = home.join("config.yaml");
    let mut cfg =
        load_user_config_file(&cfg_path).map_err(|e| AgentError::Config(e.to_string()))?;
    cfg.display.skin = Some(name.to_string());
    save_config_yaml(&cfg_path, &cfg).map_err(|e| AgentError::Config(e.to_string()))?;
    Ok(())
}

/// Atomically write a YAML skin file (write-then-rename, like the original's
/// `atomic_yaml_write`).
fn atomic_write_yaml(path: &std::path::Path, value: &serde_yaml::Value) -> Result<(), AgentError> {
    let yaml = serde_yaml::to_string(value)
        .map_err(|e| AgentError::Io(format!("Failed to serialize skin YAML: {}", e)))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AgentError::Io(format!("Failed to create skins dir: {}", e)))?;
    }
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, yaml)
        .map_err(|e| AgentError::Io(format!("Failed to write skin file: {}", e)))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| AgentError::Io(format!("Failed to install skin file: {}", e)))?;
    Ok(())
}

fn is_hex_color(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 7 && bytes[0] == b'#' && bytes[1..].iter().all(|b| b.is_ascii_hexdigit())
}

/// `skin set <key> <value> [--skin <name>]`: change one color in place.
fn cmd_set(
    home: &std::path::Path,
    key: &str,
    value: &str,
    skin: Option<&str>,
) -> Result<(), AgentError> {
    if !is_hex_color(value) {
        return Err(AgentError::Config(format!(
            "'{}' is not a #rrggbb hex color",
            value
        )));
    }

    let name = skin
        .map(|s| s.to_string())
        .unwrap_or_else(|| active_skin_name(home));
    let skins_dir = home.join("skins");
    let path = skins_dir.join(format!("{}.yaml", name));

    let (target, mut data): (String, serde_yaml::Value) = if path.exists() {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| AgentError::Io(format!("Failed to read skin file: {}", e)))?;
        let data: serde_yaml::Value =
            serde_yaml::from_str(&text).map_err(|e| AgentError::Config(e.to_string()))?;
        (name.clone(), data)
    } else {
        // Built-in (or missing): fork into an editable copy under a fresh
        // name so the built-in stays intact for revert.
        let target = format!("{}-custom", name);
        let fork_path = skins_dir.join(format!("{}.yaml", target));
        let data: serde_yaml::Value = if !fork_path.exists() {
            let builtin_desc = BUILTIN_SKINS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, d)| *d)
                .unwrap_or("Custom skin");
            let mut map = serde_yaml::Mapping::new();
            map.insert(
                serde_yaml::Value::String("name".into()),
                serde_yaml::Value::String(target.clone()),
            );
            map.insert(
                serde_yaml::Value::String("description".into()),
                serde_yaml::Value::String(format!("{} + custom {}", builtin_desc, key)),
            );
            map.insert(
                serde_yaml::Value::String("colors".into()),
                serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
            );
            serde_yaml::Value::Mapping(map)
        } else {
            let text = std::fs::read_to_string(&fork_path)
                .map_err(|e| AgentError::Io(format!("Failed to read skin file: {}", e)))?;
            serde_yaml::from_str(&text).map_err(|e| AgentError::Config(e.to_string()))?
        };
        (target, data)
    };

    let target_path = skins_dir.join(format!("{}.yaml", target));
    if !data.get("colors").is_some_and(|c| c.is_mapping()) {
        if let Some(map) = data.as_mapping_mut() {
            map.insert(
                serde_yaml::Value::String("colors".into()),
                serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
            );
        }
    }
    if let Some(colors) = data.get_mut("colors").and_then(|c| c.as_mapping_mut()) {
        colors.insert(
            serde_yaml::Value::String(key.to_string()),
            serde_yaml::Value::String(value.to_string()),
        );
    }
    if data.get("name").is_none() {
        if let Some(map) = data.as_mapping_mut() {
            map.insert(
                serde_yaml::Value::String("name".into()),
                serde_yaml::Value::String(target.clone()),
            );
        }
    }
    atomic_write_yaml(&target_path, &data)?;

    if target != name {
        persist_active_skin(home, &target)?;
    }
    println!(
        "✓ {} = {} in {}/skins/{}.yaml (live within ~1s)",
        key,
        value,
        home.display(),
        target
    );
    Ok(())
}

/// Handle `hermes skin [list|use|set]`.
pub(crate) async fn cmd_skin(
    cli: Cli,
    action: Option<String>,
    key: Option<String>,
    value: Option<String>,
    skin: Option<String>,
) -> Result<(), AgentError> {
    let home: PathBuf = cli
        .config_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(hermes_home);

    match action.as_deref().unwrap_or("list") {
        "list" => {
            let active = active_skin_name(&home);
            for row in list_skins() {
                let mark = if row.name == active { "*" } else { " " };
                println!(
                    "{} {:<16} {:<8} {}",
                    mark, row.name, row.source, row.description
                );
            }
            Ok(())
        }
        "use" => {
            let name = key.as_deref().or(skin.as_deref()).ok_or_else(|| {
                AgentError::Config("Missing skin name. Usage: hermes skin use <name>".into())
            })?;
            persist_active_skin(&home, name)?;
            println!("✓ active skin → {} (live within ~1s)", name);
            Ok(())
        }
        "set" => {
            let key = key.ok_or_else(|| {
                AgentError::Config("Missing key. Usage: hermes skin set <key> <value>".into())
            })?;
            let value = value.ok_or_else(|| {
                AgentError::Config("Missing value. Usage: hermes skin set <key> <value>".into())
            })?;
            cmd_set(&home, &key, &value, skin.as_deref())
        }
        other => Err(AgentError::Config(format!(
            "Unknown skin action: '{}'. Use one of: list, use, set",
            other
        ))),
    }
}
