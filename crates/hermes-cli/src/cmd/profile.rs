//! `hermes profile` — manage named configuration profiles.
//!
//! Profiles are `<home>/profiles/<name>.yaml` files (the fork's layout,
//! matching the interactive `run_profile` handler). The sticky active profile
//! is tracked in `<home>/active_profile` — an atomic write, removed when
//! switching back to `default` — mirroring the original's
//! `profiles.set_active_profile` semantics from `hermes_cli/profiles.py`.
//!
//! Actions: `show` (default), `list`, `create <name>`, `switch <name>`
//! (`use` is an alias), `delete <name>`.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// File inside the hermes home that pins the sticky active profile.
pub const ACTIVE_PROFILE_FILE: &str = "active_profile";

#[derive(clap::Args, Debug, Clone)]
pub struct ProfileArgs {
    /// Action: show (default), list, create, switch, delete.
    pub action: Option<String>,

    /// Profile name (for create/switch/delete/show).
    pub name: Option<String>,

    /// Skip the delete confirmation prompt.
    #[arg(short = 'y', long)]
    pub yes: bool,

    /// Config dir override (wired by the parent from the global --config-dir).
    #[arg(skip)]
    pub config_dir: Option<PathBuf>,
}

/// Canonicalize + validate a profile id: lowercase, starts with an
/// alphanumeric, then alphanumerics, `-` or `_`, max 64 chars. Mirrors the
/// original's `normalize_profile_name` / `validate_profile_name` rules.
pub fn normalize_profile_name(name: &str) -> Result<String, String> {
    let canon = name.trim().to_lowercase();
    if canon.is_empty() {
        return Err("profile name must not be empty".to_string());
    }
    if canon.len() > 64 {
        return Err("profile name must be at most 64 characters".to_string());
    }
    let mut chars = canon.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphanumeric() {
        return Err(format!(
            "profile name must start with a letter or digit, got {name:?}"
        ));
    }
    if !canon
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "profile name may only contain lowercase letters, digits, '-' and '_', got {name:?}"
        ));
    }
    Ok(canon)
}

pub fn profiles_dir(home: &Path) -> PathBuf {
    home.join("profiles")
}

pub fn profile_path(home: &Path, name: &str) -> PathBuf {
    profiles_dir(home).join(format!("{name}.yaml"))
}

pub fn active_profile_path(home: &Path) -> PathBuf {
    home.join(ACTIVE_PROFILE_FILE)
}

/// Name of the sticky active profile: the `active_profile` file's content,
/// or `"default"` when unset, empty, or invalid.
pub fn get_active_profile(home: &Path) -> String {
    std::fs::read_to_string(active_profile_path(home))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| normalize_profile_name(s).is_ok())
        .unwrap_or_else(|| "default".to_string())
}

/// Pin the sticky active profile. `"default"` removes the pin file.
/// The write is atomic (tmp file + rename), like the original.
pub fn set_active_profile(home: &Path, name: &str) -> Result<()> {
    let canon = normalize_profile_name(name).map_err(|e| anyhow::anyhow!(e))?;
    let path = active_profile_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    if canon == "default" {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    if !profile_path(home, &canon).exists() {
        // Also accept .yml.
        let alt = profiles_dir(home).join(format!("{canon}.yml"));
        if !alt.exists() {
            bail!("Profile '{canon}' not found. Create it with `hermes profile create {canon}`.");
        }
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{canon}\n"))
        .with_context(|| format!("Failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("Failed to activate profile '{canon}'"))?;
    Ok(())
}

/// Sorted profile names from `*.yaml` / `*.yml` files.
pub fn list_profile_names(home: &Path) -> Vec<String> {
    let dir = profiles_dir(home);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x == "yaml" || x == "yml")
        })
        .filter_map(|e| {
            e.path()
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Create a new profile file from the default template.
pub fn create_profile(home: &Path, name: &str) -> Result<PathBuf> {
    let canon = normalize_profile_name(name).map_err(|e| anyhow::anyhow!(e))?;
    let dir = profiles_dir(home);
    std::fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    let path = profile_path(home, &canon);
    if path.exists() {
        bail!("Profile '{canon}' already exists at {}", path.display());
    }
    let content = format!(
        "# Hermes Profile: {canon}\nname: {canon}\nmodel: openai:gpt-4o\npersonality: default\nmax_turns: 50\n"
    );
    std::fs::write(&path, content)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(path)
}

/// Delete a profile file. When it was the active profile, the pin is cleared.
pub fn delete_profile(home: &Path, name: &str) -> Result<()> {
    let canon = normalize_profile_name(name).map_err(|e| anyhow::anyhow!(e))?;
    if canon == "default" {
        bail!("The 'default' profile cannot be deleted.");
    }
    let path = profile_path(home, &canon);
    let alt = profiles_dir(home).join(format!("{canon}.yml"));
    let target = if path.exists() {
        path
    } else if alt.exists() {
        alt
    } else {
        bail!("Profile '{canon}' not found.");
    };
    std::fs::remove_file(&target)
        .with_context(|| format!("Failed to delete {}", target.display()))?;
    if get_active_profile(home) == canon {
        let _ = std::fs::remove_file(active_profile_path(home));
        println!("(It was the active profile — the pin was cleared.)");
    }
    Ok(())
}

async fn confirm(prompt: &str) -> Result<bool> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut out = tokio::io::stdout();
    out.write_all(format!("{prompt} [y/N] ").as_bytes()).await?;
    out.flush().await?;
    let mut line = String::new();
    tokio::io::BufReader::new(tokio::io::stdin())
        .read_line(&mut line)
        .await?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn show_current(home: &Path, config_dir_arg: Option<&str>) -> Result<()> {
    let active = get_active_profile(home);
    println!("Active profile: {active}");
    let path = profile_path(home, &active);
    if path.exists() {
        println!("Profile file: {}", path.display());
    } else if active != "default" {
        println!("(profile file {} not found)", path.display());
    }
    // Effective settings come from the merged config, as in the original's
    // `profile show` summary.
    match hermes_config::load_config(config_dir_arg) {
        Ok(config) => {
            println!(
                "  Model:       {}",
                config.model.as_deref().unwrap_or("gpt-4o")
            );
            println!(
                "  Personality: {}",
                config.personality.as_deref().unwrap_or("default")
            );
            println!("  Max turns:   {}", config.max_turns);
        }
        Err(e) => println!("  (could not load config: {e})"),
    }
    println!("\nUse `hermes profile list` to see all profiles.");
    Ok(())
}

/// Handle `hermes profile [action] [name]`.
pub async fn run(args: &ProfileArgs) -> Result<()> {
    let home = args
        .config_dir
        .clone()
        .unwrap_or_else(hermes_config::hermes_home);
    let config_dir_arg = args.config_dir.as_deref().and_then(|p| p.to_str());

    match args.action.as_deref() {
        None | Some("show") => {
            if let Some(name) = args.name.as_deref() {
                let canon = normalize_profile_name(name).map_err(|e| anyhow::anyhow!(e))?;
                let path = profile_path(&home, &canon);
                let alt = profiles_dir(&home).join(format!("{canon}.yml"));
                let target = if path.exists() {
                    path
                } else if alt.exists() {
                    alt
                } else {
                    bail!("Profile '{canon}' not found.");
                };
                let content = std::fs::read_to_string(&target)
                    .with_context(|| format!("Failed to read {}", target.display()))?;
                println!("--- {} ---\n{content}", target.display());
                if get_active_profile(&home) == canon {
                    println!("(this is the active profile)");
                }
            } else {
                show_current(&home, config_dir_arg)?;
            }
        }
        Some("list") => {
            let names = list_profile_names(&home);
            let active = get_active_profile(&home);
            if names.is_empty() {
                println!("No profiles found. Create one with `hermes profile create <name>`.");
            } else {
                println!("Available profiles:");
                for name in &names {
                    let marker = if *name == active {
                        "  • * "
                    } else {
                        "  •   "
                    };
                    println!("{marker}{name}");
                }
                if active == "default" || !names.contains(&active) {
                    println!("\nActive profile: {active}");
                }
            }
        }
        Some("create") => {
            let name = args.name.as_deref().with_context(|| {
                "Missing profile name. Usage: hermes profile create <name>".to_string()
            })?;
            let path = create_profile(&home, name)?;
            let canon = normalize_profile_name(name).map_err(|e| anyhow::anyhow!(e))?;
            println!("Created profile '{canon}' at {}", path.display());
            println!("Edit it to customize, then switch with `hermes profile switch {canon}`.");
        }
        Some("switch") | Some("use") => {
            let name = args.name.as_deref().with_context(|| {
                "Missing profile name. Usage: hermes profile switch <name>".to_string()
            })?;
            set_active_profile(&home, name)?;
            let canon = normalize_profile_name(name).map_err(|e| anyhow::anyhow!(e))?;
            println!("Switched to profile: {canon}");
            println!("(Profile settings apply on the next `hermes` session)");
        }
        Some("delete") | Some("remove") => {
            let name = args.name.as_deref().with_context(|| {
                "Missing profile name. Usage: hermes profile delete <name>".to_string()
            })?;
            let canon = normalize_profile_name(name).map_err(|e| anyhow::anyhow!(e))?;
            if !args.yes && !confirm(&format!("Delete profile '{canon}'?")).await? {
                println!("Cancelled.");
                return Ok(());
            }
            delete_profile(&home, &canon)?;
            println!("Deleted profile '{canon}'.");
        }
        Some(other) => {
            bail!(
                "Unknown profile action: '{other}'. Use 'show', 'list', 'create', 'switch', or 'delete'."
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_profile_name_rules() {
        assert_eq!(normalize_profile_name("Work").unwrap(), "work");
        assert_eq!(normalize_profile_name("  dev-2_x ").unwrap(), "dev-2_x");
        assert_eq!(normalize_profile_name("default").unwrap(), "default");
        assert!(normalize_profile_name("").is_err());
        assert!(normalize_profile_name("has space").is_err());
        assert!(normalize_profile_name("-lead").is_err());
        assert!(normalize_profile_name("UPPER").is_ok());
        assert!(normalize_profile_name(&"a".repeat(65)).is_err());
    }

    fn test_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hermes-profile-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn active_profile_roundtrip() {
        let home = test_home("active");
        assert_eq!(get_active_profile(&home), "default");
        create_profile(&home, "work").unwrap();
        set_active_profile(&home, "work").unwrap();
        assert_eq!(get_active_profile(&home), "work");
        // Atomic write leaves no tmp file behind.
        assert!(!active_profile_path(&home).with_extension("tmp").exists());
        set_active_profile(&home, "default").unwrap();
        assert_eq!(get_active_profile(&home), "default");
        assert!(!active_profile_path(&home).exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn switch_rejects_unknown_profile() {
        let home = test_home("unknown");
        assert!(set_active_profile(&home, "ghost").is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn create_lists_and_deletes() {
        let home = test_home("crud");
        assert!(list_profile_names(&home).is_empty());
        let path = create_profile(&home, "dev").unwrap();
        assert!(path.exists());
        // Duplicate create fails.
        assert!(create_profile(&home, "dev").is_err());
        // .yml files are listed too.
        std::fs::write(profiles_dir(&home).join("ops.yml"), "name: ops\n").unwrap();
        let names = list_profile_names(&home);
        assert_eq!(names, vec!["dev".to_string(), "ops".to_string()]);

        // Deleting the active profile clears the pin.
        set_active_profile(&home, "dev").unwrap();
        delete_profile(&home, "dev").unwrap();
        assert_eq!(get_active_profile(&home), "default");
        assert!(!profile_path(&home, "dev").exists());

        // The built-in default profile cannot be deleted.
        assert!(delete_profile(&home, "default").is_err());
        let _ = std::fs::remove_dir_all(&home);
    }
}
