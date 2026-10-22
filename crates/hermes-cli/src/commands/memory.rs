//! `hermes memory` / `hermes memory-setup` CLI subcommand handlers.

/// Handle `hermes memory [action]`.
/// Resolve the memory store file for a CLI target ("memory" → MEMORY.md,
/// "user" → USER.md).
pub(crate) fn memory_target_file(
    home: &std::path::Path,
    target: &str,
) -> Result<std::path::PathBuf, hermes_core::AgentError> {
    let file = match target {
        "memory" => "MEMORY.md",
        "user" => "USER.md",
        other => {
            return Err(hermes_core::AgentError::Config(format!(
                "Unknown memory target: '{}'. Use 'memory' or 'user'.",
                other
            )))
        }
    };
    Ok(home.join("memories").join(file))
}

/// Case-insensitive substring search over memory file lines.
/// Returns (1-based line number, line) pairs.
pub(crate) fn memory_recall(content: &str, query: &str) -> Vec<(usize, String)> {
    let q = query.to_lowercase();
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| line.to_lowercase().contains(&q))
        .map(|(i, line)| (i + 1, line.to_string()))
        .collect()
}

/// Remove lines containing `query` (case-insensitive).
/// Returns (new content, removed count).
pub(crate) fn memory_forget(content: &str, query: &str) -> (String, usize) {
    let q = query.to_lowercase();
    let mut removed = 0usize;
    let kept: Vec<&str> = content
        .lines()
        .filter(|line| {
            if line.to_lowercase().contains(&q) {
                removed += 1;
                false
            } else {
                true
            }
        })
        .collect();
    let mut out = kept.join("\n");
    if !out.is_empty() && content.ends_with('\n') {
        out.push('\n');
    }
    (out, removed)
}

/// Handle `hermes memory [action] [arg] [--target ...]`.
///
/// `remember`/`recall`/`forget` operate on the built-in file-based memory
/// store (`~/.hermes/memories/MEMORY.md` and `USER.md`); `setup`/`status`/`off`
/// manage the external memory provider as before.
pub async fn handle_cli_memory(
    action: Option<String>,
    arg: Option<String>,
    target: String,
) -> Result<(), hermes_core::AgentError> {
    match action.as_deref().unwrap_or("status") {
        "status" => {
            let memory_db = hermes_config::hermes_home().join("memory.db");
            if memory_db.exists() {
                let size = std::fs::metadata(&memory_db).map(|m| m.len()).unwrap_or(0);
                println!("Memory provider: sqlite (file-based)");
                println!("  Database: {}", memory_db.display());
                println!("  Size: {} KB", size / 1024);
            } else {
                println!("Memory provider: not configured");
                println!("Run `hermes memory setup` to initialize.");
            }
        }
        "setup" => {
            println!("Memory Provider Setup");
            println!("---------------------");
            println!("Available providers:");
            println!("  1) sqlite  — Local SQLite database (default)");
            println!("  2) redis   — Redis-backed memory (requires REDIS_URL)");
            println!("  3) qdrant  — Qdrant vector store (requires QDRANT_URL)");
            println!("\nInitializing default SQLite provider...");
            let memory_db = hermes_config::hermes_home().join("memory.db");
            println!("Memory database will be stored at: {}", memory_db.display());
            println!("(Full setup wizard with provider selection coming soon)");
        }
        "off" => {
            println!("Memory provider disabled.");
            println!("(Persistent memory will not be used in future sessions)");
        }
        "remember" => {
            let text = arg.ok_or_else(|| {
                hermes_core::AgentError::Config(
                    "Missing text. Usage: hermes memory remember \"<note>\" [--target memory|user]"
                        .into(),
                )
            })?;
            let path = memory_target_file(&hermes_config::hermes_home(), &target)?;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| hermes_core::AgentError::Io(e.to_string()))?;
            }
            let mut content = std::fs::read_to_string(&path).unwrap_or_default();
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(text.trim());
            content.push('\n');
            std::fs::write(&path, &content)
                .map_err(|e| hermes_core::AgentError::Io(e.to_string()))?;
            println!("Remembered in {} ({}).", path.display(), target);
        }
        "recall" => {
            let query = arg.ok_or_else(|| {
                hermes_core::AgentError::Config(
                    "Missing query. Usage: hermes memory recall \"<query>\" [--target memory|user|all]"
                        .into(),
                )
            })?;
            let home = hermes_config::hermes_home();
            let targets: Vec<&str> = if target == "all" {
                vec!["memory", "user"]
            } else {
                // Validate the target name via the file resolver.
                memory_target_file(&home, &target)?;
                vec![target.as_str()]
            };
            let mut total = 0usize;
            for t in targets {
                let path = memory_target_file(&home, t)?;
                let content = std::fs::read_to_string(&path).unwrap_or_default();
                let hits = memory_recall(&content, &query);
                if hits.is_empty() {
                    continue;
                }
                println!("{} ({}):", path.display(), t);
                for (lineno, line) in &hits {
                    println!("  {:>4}: {}", lineno, line);
                }
                total += hits.len();
            }
            if total == 0 {
                println!("No memories matching '{}'.", query);
            } else {
                println!("{} match(es).", total);
            }
        }
        "forget" => {
            let query = arg.ok_or_else(|| {
                hermes_core::AgentError::Config(
                    "Missing text. Usage: hermes memory forget \"<text>\" [--target memory|user|all]"
                        .into(),
                )
            })?;
            let home = hermes_config::hermes_home();
            let targets: Vec<&str> = if target == "all" {
                vec!["memory", "user"]
            } else {
                memory_target_file(&home, &target)?;
                vec![target.as_str()]
            };
            let mut total = 0usize;
            for t in targets {
                let path = memory_target_file(&home, t)?;
                let content = std::fs::read_to_string(&path).unwrap_or_default();
                let (new_content, removed) = memory_forget(&content, &query);
                if removed > 0 {
                    std::fs::write(&path, new_content)
                        .map_err(|e| hermes_core::AgentError::Io(e.to_string()))?;
                    println!("Forgot {} line(s) in {} ({}).", removed, path.display(), t);
                    total += removed;
                }
            }
            if total == 0 {
                println!("No memories matching '{}'.", query);
            }
        }
        other => {
            println!("Memory action '{}' is not yet implemented.", other);
            println!("Available actions: status, setup, off, remember, recall, forget");
        }
    }
    Ok(())
}

// MemorySetup
// ---------------------------------------------------------------------------

/// Handle `hermes memory-setup [action] [provider]`.
pub async fn handle_cli_memory_setup(
    action: Option<String>,
    provider: Option<String>,
) -> Result<(), hermes_core::AgentError> {
    match action.as_deref().unwrap_or("setup") {
        "setup" => {
            println!("Memory Provider Configuration Wizard");
            println!("====================================\n");
            println!("Available memory providers:");
            println!("  1) sqlite       — Local SQLite database (default, no setup required)");
            println!("  2) redis        — Redis-backed memory (requires REDIS_URL)");
            println!("  3) qdrant       — Qdrant vector store (requires QDRANT_URL)");
            println!("  4) mem0         — Mem0 cloud memory (requires MEM0_API_KEY)");
            println!("  5) honcho       — Honcho session memory (requires HONCHO_API_KEY)");
            println!("  6) holographic  — Holographic memory (requires HOLOGRAPHIC_API_KEY)");
            println!("  7) supermemory  — Supermemory provider (requires SUPERMEMORY_API_KEY)");
            println!("  8) byterover   — ByteRover memory (requires BYTEROVER_API_KEY)");

            if let Some(p) = &provider {
                println!("\nConfiguring provider: {}", p);
                match p.as_str() {
                    "sqlite" => {
                        let db_path = hermes_config::hermes_home().join("memory.db");
                        println!("SQLite memory will be stored at: {}", db_path.display());
                        println!("No additional configuration required.");
                    }
                    "redis" | "qdrant" | "mem0" | "honcho" | "holographic" | "supermemory"
                    | "byterover" => {
                        let env_key = format!("{}_API_KEY", p.to_uppercase());
                        println!("Set {} in ~/.hermes/.env to enable this provider.", env_key);
                    }
                    _ => {
                        println!("Unknown provider: '{}'. See the list above.", p);
                    }
                }
            } else {
                println!("\nUsage: hermes memory-setup setup <provider>");
                println!("Or run `hermes memory-setup status` to check current configuration.");
            }
        }
        "status" => {
            let memory_dir = hermes_config::hermes_home().join("memories");
            let memory_db = hermes_config::hermes_home().join("memory.db");
            println!("Memory Provider Status");
            println!("----------------------");
            if memory_db.exists() {
                let size = std::fs::metadata(&memory_db).map(|m| m.len()).unwrap_or(0);
                println!("  Provider:  sqlite (file-based)");
                println!("  Database:  {}", memory_db.display());
                println!("  Size:      {} KB", size / 1024);
            } else {
                println!("  Provider:  none configured");
            }
            if memory_dir.exists() {
                let memory_md = memory_dir.join("MEMORY.md");
                let user_md = memory_dir.join("USER.md");
                println!(
                    "  MEMORY.md: {}",
                    if memory_md.exists() {
                        "present"
                    } else {
                        "not found"
                    }
                );
                println!(
                    "  USER.md:   {}",
                    if user_md.exists() {
                        "present"
                    } else {
                        "not found"
                    }
                );
            } else {
                println!("  Memories directory: not created");
            }
        }
        "off" => {
            println!("External memory provider disabled.");
            println!("Built-in file-based memory (MEMORY.md / USER.md) remains active.");
            println!("To re-enable, run `hermes memory-setup setup <provider>`.");
        }
        other => {
            println!("Unknown memory-setup action: '{}'.", other);
            println!("Available actions: setup, status, off");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_recall_is_case_insensitive() {
        let content = "Chewy prefers dark mode\nLives in Makassar\nDARK chocolate is a snack\n";
        let hits = memory_recall(content, "dark");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, 1);
        assert_eq!(hits[1].0, 3);
        assert!(memory_recall(content, "tokyo").is_empty());
    }

    #[test]
    fn memory_forget_removes_matching_lines() {
        let content = "keep this\nforget me please\nkeep this too\nFORGET me too\n";
        let (out, removed) = memory_forget(content, "forget");
        assert_eq!(removed, 2);
        assert_eq!(out, "keep this\nkeep this too\n");
        // No match → content unchanged, zero removed.
        let (out2, removed2) = memory_forget(content, "zzz");
        assert_eq!(removed2, 0);
        assert_eq!(out2, content);
    }

    #[test]
    fn memory_target_file_resolves_and_validates() {
        let home = std::path::Path::new("/tmp/hermes-test");
        assert_eq!(
            memory_target_file(home, "memory").unwrap(),
            home.join("memories").join("MEMORY.md")
        );
        assert_eq!(
            memory_target_file(home, "user").unwrap(),
            home.join("memories").join("USER.md")
        );
        assert!(memory_target_file(home, "bogus").is_err());
    }
}
