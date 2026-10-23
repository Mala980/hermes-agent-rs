//! `hermes checkpoints` — list, create, restore and prune conversation checkpoints.
//!
//! Ported from NousResearch/hermes-agent's `hermes_cli/checkpoints.py`.
//! The original maintained the git-shadow checkpoint store behind
//! `/rollback` (`status`/`prune`/`clear`/`clear-legacy`). This fork keeps
//! conversations as JSON session files, so a checkpoint here is a named
//! snapshot of a session stored under `$HERMES_HOME/checkpoints/`:
//! `create` snapshots the live session, `restore` writes the snapshot back
//! over it, `list` shows the store, and `prune` reclaims old snapshots.
//!
//! None of these require the agent to be running. Safe to call any time.

use chrono::{DateTime, Utc};
use hermes_config::hermes_home;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A checkpoint: a snapshot of one conversation session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub name: String,
    pub session_id: String,
    #[serde(default)]
    pub created_at: String,
    pub session: serde_json::Value,
}

/// Listing metadata for one checkpoint file.
#[derive(Debug, Clone)]
pub struct CheckpointMeta {
    pub name: String,
    pub session_id: String,
    pub created: Option<DateTime<Utc>>,
    pub size_bytes: u64,
}

/// CLI args for `hermes checkpoints`.
#[derive(clap::Args, Debug, Clone)]
pub struct CheckpointsArgs {
    /// Action: list (default), create, restore, delete, prune.
    pub action: Option<String>,
    /// Session id for `create` (defaults to the most recently modified session).
    #[arg(long)]
    pub id: Option<String>,
    /// Checkpoint name for create/restore/delete.
    #[arg(long)]
    pub name: Option<String>,
    /// Skip the confirmation prompt (restore/delete/prune).
    #[arg(long, short = 'f')]
    pub force: bool,
    /// Prune checkpoints older than N days (default 30).
    #[arg(long, default_value_t = 30.0)]
    pub older_than_days: f64,
    /// Machine-readable output for `list`.
    #[arg(long)]
    pub json: bool,
}

/// Directory holding checkpoint snapshots.
pub fn checkpoint_dir() -> PathBuf {
    hermes_home().join("checkpoints")
}

fn sessions_dir() -> PathBuf {
    hermes_home().join("sessions")
}

/// Human age string mirroring `_fmt_age` in checkpoints.py.
pub fn fmt_age(ts: f64) -> String {
    fmt_age_at(ts, Utc::now().timestamp() as f64)
}

/// [`fmt_age`] with an explicit "now" (for deterministic tests).
pub fn fmt_age_at(ts: f64, now: f64) -> String {
    let age = now - ts;
    if age < 0.0 {
        return "now".to_string();
    }
    if age < 60.0 {
        return format!("{}s ago", age as u64);
    }
    if age < 3600.0 {
        return format!("{}m ago", (age / 60.0) as u64);
    }
    if age < 86400.0 {
        return format!("{}h ago", (age / 3600.0) as u64);
    }
    format!("{}d ago", (age / 86400.0) as u64)
}

pub fn format_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut unit = UNITS[0];
    for u in &UNITS[1..] {
        if v < 1024.0 {
            break;
        }
        v /= 1024.0;
        unit = u;
    }
    if unit == "B" {
        format!("{n} B")
    } else {
        format!("{v:.1} {unit}")
    }
}

fn checkpoint_path(dir: &std::path::Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.json"))
}

fn read_checkpoint(path: &std::path::Path) -> Option<Checkpoint> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// List all checkpoints, newest first.
pub fn list_checkpoints(dir: &std::path::Path) -> Vec<CheckpointMeta> {
    let mut metas = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return metas;
    };
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().map(|e| e != "json").unwrap_or(true) {
            continue;
        }
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let size_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let (session_id, created) = match read_checkpoint(&path) {
            Some(cp) => (cp.session_id, cp.created_at.parse::<DateTime<Utc>>().ok()),
            None => ("(unreadable)".to_string(), None),
        };
        metas.push(CheckpointMeta {
            name,
            session_id,
            created,
            size_bytes,
        });
    }
    metas.sort_by_key(|a| std::cmp::Reverse(a.created));
    metas
}

/// Id of the most recently modified session, if any.
fn newest_session_id(dir: &std::path::Path) -> Option<String> {
    let rd = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().map(|e| e != "json").unwrap_or(true) {
            continue;
        }
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
        let id = path.file_stem().map(|s| s.to_string_lossy().into_owned())?;
        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, id));
        }
    }
    best.map(|(_, id)| id)
}

fn confirm(prompt: &str) -> bool {
    print!("{prompt} [y/N]: ");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    match std::io::stdin().read_line(&mut input) {
        Ok(_) => matches!(input.trim().to_lowercase().as_str(), "y" | "yes"),
        Err(_) => false,
    }
}

fn confirmed(force: bool, prompt: &str) -> bool {
    if force || confirm(prompt) {
        return true;
    }
    println!("Aborted.");
    false
}

fn run_list(dir: &std::path::Path, as_json: bool) -> anyhow::Result<()> {
    let metas = list_checkpoints(dir);
    let total: u64 = metas.iter().map(|m| m.size_bytes).sum();
    if as_json {
        let docs: Vec<serde_json::Value> = metas
            .iter()
            .map(|m| {
                serde_json::json!({
                    "name": m.name,
                    "session_id": m.session_id,
                    "created_at": m.created.map(|d| d.to_rfc3339()),
                    "size_bytes": m.size_bytes,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&docs)?);
        return Ok(());
    }
    println!("Checkpoint base: {}", dir.display());
    println!("Total size:      {}", format_bytes(total));
    println!("Checkpoints:     {}", metas.len());
    if !metas.is_empty() {
        println!();
        println!(
            "  {:<40}  {:>12}  {:>10}  SESSION",
            "NAME", "CREATED", "SIZE"
        );
        for m in &metas {
            let created = m
                .created
                .map(|d| fmt_age(d.timestamp() as f64))
                .unwrap_or_else(|| "—".to_string());
            println!(
                "  {:<40}  {:>12}  {:>10}  {}",
                m.name,
                created,
                format_bytes(m.size_bytes),
                m.session_id
            );
        }
    } else {
        println!(
            "\nNo checkpoints yet. Create one with `hermes checkpoints create --id <session>`."
        );
    }
    Ok(())
}

fn run_create(dir: &std::path::Path, args: &CheckpointsArgs) -> anyhow::Result<()> {
    let session_id = match &args.id {
        Some(id) => id.clone(),
        None => newest_session_id(&sessions_dir())
            .ok_or_else(|| anyhow::anyhow!("No sessions found. Pass --id <session> explicitly."))?,
    };
    let session_path = sessions_dir().join(format!("{session_id}.json"));
    let session_text = std::fs::read_to_string(&session_path).map_err(|_| {
        anyhow::anyhow!(
            "Session '{session_id}' not found at {}",
            session_path.display()
        )
    })?;
    let session: serde_json::Value = serde_json::from_str(&session_text)
        .map_err(|e| anyhow::anyhow!("Session file is not valid JSON: {e}"))?;
    let name = args
        .name
        .clone()
        .unwrap_or_else(|| format!("ckpt-{}", Utc::now().format("%Y%m%d-%H%M%S")));
    if name.contains('/') || name.contains('\\') || name.is_empty() {
        return Err(anyhow::anyhow!("Invalid checkpoint name: {name}"));
    }
    let path = checkpoint_path(dir, &name);
    if path.exists() && !args.force {
        return Err(anyhow::anyhow!(
            "Checkpoint '{name}' already exists. Re-run with --force to overwrite."
        ));
    }
    std::fs::create_dir_all(dir)?;
    let checkpoint = Checkpoint {
        name: name.clone(),
        session_id: session_id.clone(),
        created_at: Utc::now().to_rfc3339(),
        session,
    };
    std::fs::write(&path, serde_json::to_string_pretty(&checkpoint)?)?;
    println!("Created checkpoint '{name}' from session '{session_id}'.");
    println!("  Restore with: hermes checkpoints restore --name {name}");
    Ok(())
}

fn run_restore(dir: &std::path::Path, args: &CheckpointsArgs) -> anyhow::Result<()> {
    let name = args.name.as_deref().ok_or_else(|| {
        anyhow::anyhow!("Missing checkpoint name. Usage: hermes checkpoints restore --name <name>")
    })?;
    let path = checkpoint_path(dir, name);
    let checkpoint = read_checkpoint(&path)
        .ok_or_else(|| anyhow::anyhow!("Checkpoint '{name}' not found or unreadable."))?;
    let session_path = sessions_dir().join(format!("{}.json", checkpoint.session_id));
    println!(
        "This will overwrite session '{}' with checkpoint '{name}' (created {}).",
        checkpoint.session_id,
        checkpoint
            .created_at
            .parse::<DateTime<Utc>>()
            .map(|d| fmt_age(d.timestamp() as f64))
            .unwrap_or_else(|_| checkpoint.created_at.clone())
    );
    if !confirmed(args.force, "Proceed?") {
        return Ok(());
    }
    std::fs::create_dir_all(sessions_dir())?;
    std::fs::write(
        &session_path,
        serde_json::to_string_pretty(&checkpoint.session)?,
    )?;
    println!(
        "Restored session '{}' from checkpoint '{name}'.",
        checkpoint.session_id
    );
    Ok(())
}

fn run_delete(dir: &std::path::Path, args: &CheckpointsArgs) -> anyhow::Result<()> {
    let name = args.name.as_deref().ok_or_else(|| {
        anyhow::anyhow!("Missing checkpoint name. Usage: hermes checkpoints delete --name <name>")
    })?;
    let path = checkpoint_path(dir, name);
    if !path.exists() {
        println!("Checkpoint '{name}' does not exist.");
        return Ok(());
    }
    if !confirmed(args.force, &format!("Delete checkpoint '{name}'?")) {
        return Ok(());
    }
    std::fs::remove_file(&path)?;
    println!("Deleted checkpoint '{name}'.");
    Ok(())
}

fn run_prune(dir: &std::path::Path, args: &CheckpointsArgs) -> anyhow::Result<()> {
    let cutoff = Utc::now().timestamp() as f64 - args.older_than_days * 86400.0;
    let stale: Vec<CheckpointMeta> = list_checkpoints(dir)
        .into_iter()
        .filter(|m| {
            m.created
                .map(|d| (d.timestamp() as f64) < cutoff)
                .unwrap_or(false)
        })
        .collect();
    if stale.is_empty() {
        println!(
            "Nothing to prune — no checkpoints older than {:.1} days.",
            args.older_than_days
        );
        return Ok(());
    }
    let reclaimed: u64 = stale.iter().map(|m| m.size_bytes).sum();
    println!(
        "This will permanently delete {} checkpoint(s) older than {:.1} days:",
        stale.len(),
        args.older_than_days
    );
    for m in &stale {
        println!("  {}  ({})", m.name, format_bytes(m.size_bytes));
    }
    if !confirmed(args.force, "Delete these checkpoints?") {
        return Ok(());
    }
    let mut errors = 0;
    for m in &stale {
        if std::fs::remove_file(checkpoint_path(dir, &m.name)).is_err() {
            errors += 1;
        }
    }
    println!(
        "Pruned {} checkpoint(s), reclaimed {}.{}",
        stale.len() - errors,
        format_bytes(reclaimed),
        if errors > 0 {
            format!(" {errors} error(s) — see logs.")
        } else {
            String::new()
        }
    );
    Ok(())
}

/// Dispatch `hermes checkpoints`.
pub async fn run(args: &CheckpointsArgs) -> anyhow::Result<()> {
    let dir = checkpoint_dir();
    match args.action.as_deref().unwrap_or("list") {
        "list" | "status" => run_list(&dir, args.json),
        "create" => run_create(&dir, args),
        "restore" => run_restore(&dir, args),
        "delete" | "rm" => run_delete(&dir, args),
        "prune" => run_prune(&dir, args),
        other => {
            println!(
                "Unknown checkpoints action: '{other}'. Use: list, create, restore, delete, prune."
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_checkpoint(dir: &std::path::Path, name: &str, created_at: &str) {
        let cp = Checkpoint {
            name: name.to_string(),
            session_id: "sess1".to_string(),
            created_at: created_at.to_string(),
            session: serde_json::json!({"messages": []}),
        };
        std::fs::write(
            checkpoint_path(dir, name),
            serde_json::to_string_pretty(&cp).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn fmt_age_formats_buckets() {
        let now = 1_000_000.0;
        assert_eq!(fmt_age_at(now - 30.0, now), "30s ago");
        assert_eq!(fmt_age_at(now - 90.0, now), "1m ago");
        assert_eq!(fmt_age_at(now - 7200.0, now), "2h ago");
        assert_eq!(fmt_age_at(now - 90000.0, now), "1d ago");
        assert_eq!(fmt_age_at(now + 10.0, now), "now");
    }

    #[test]
    fn format_bytes_scales() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn list_checkpoints_reads_and_sorts_newest_first() {
        let dir = temp_dir("hermes-checkpoints-test-list");
        write_checkpoint(&dir, "old", "2020-01-01T00:00:00+00:00");
        write_checkpoint(&dir, "new", "2024-06-01T00:00:00+00:00");
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let metas = list_checkpoints(&dir);
        assert_eq!(metas.len(), 2);
        assert_eq!(metas[0].name, "new");
        assert_eq!(metas[1].name, "old");
        assert_eq!(metas[0].session_id, "sess1");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prune_keeps_recent_checkpoints() {
        let dir = temp_dir("hermes-checkpoints-test-prune");
        let recent = Utc::now().to_rfc3339();
        write_checkpoint(&dir, "recent", &recent);
        write_checkpoint(&dir, "ancient", "2020-01-01T00:00:00+00:00");
        let cutoff = Utc::now().timestamp() as f64 - 30.0 * 86400.0;
        let stale: Vec<_> = list_checkpoints(&dir)
            .into_iter()
            .filter(|m| {
                m.created
                    .map(|d| (d.timestamp() as f64) < cutoff)
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].name, "ancient");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn checkpoint_roundtrip_preserves_session() {
        let dir = temp_dir("hermes-checkpoints-test-rt");
        let session =
            serde_json::json!({"messages": [{"role": "user", "content": "hi"}], "model": "x"});
        let cp = Checkpoint {
            name: "rt".to_string(),
            session_id: "s1".to_string(),
            created_at: Utc::now().to_rfc3339(),
            session: session.clone(),
        };
        std::fs::write(
            checkpoint_path(&dir, "rt"),
            serde_json::to_string_pretty(&cp).unwrap(),
        )
        .unwrap();
        let back = read_checkpoint(&checkpoint_path(&dir, "rt")).unwrap();
        assert_eq!(back.session, session);
        std::fs::remove_dir_all(&dir).ok();
    }
}
