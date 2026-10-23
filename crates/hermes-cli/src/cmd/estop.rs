//! `hermes pause` / `hermes resume` — the global emergency stop.
//!
//! Ported from NousResearch/hermes-agent's `hermes_cli/subcommands/pause.py`
//! (semantics from `agent/estop.py`):
//!
//! `pause` writes the ESTOP sentinel at `$HERMES_HOME/ESTOP`; cron, kanban and
//! new gateway turns halt on their next check (in-flight work is never
//! killed). The body is JSON `{"engaged_at", "reason"}`; a corrupt/empty file
//! still counts as engaged (fail safe, e.g. `touch ~/.hermes/ESTOP`).
//! `resume` removes it and operation resumes on the next tick — no restart.

use chrono::Utc;
use hermes_config::hermes_home;
use hermes_core::AgentError;

/// Name of the ESTOP sentinel file, relative to the hermes home.
pub const SENTINEL_NAME: &str = "ESTOP";

/// Path of the ESTOP sentinel this process writes on `hermes pause`.
pub fn sentinel_path() -> std::path::PathBuf {
    hermes_home().join(SENTINEL_NAME)
}

/// True when the sentinel exists (fail safe: a corrupt/empty file counts).
pub fn is_engaged() -> bool {
    sentinel_path().exists()
}

/// Create the ESTOP sentinel. Idempotent; re-engaging updates the file.
fn engage(reason: Option<&str>) -> Result<std::path::PathBuf, AgentError> {
    let path = sentinel_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AgentError::Io(format!("Failed to create hermes home: {}", e)))?;
    }
    let payload = serde_json::json!({
        "engaged_at": Utc::now().to_rfc3339(),
        "reason": reason,
    });
    // Best effort: an empty/partial sentinel still pauses (fail safe).
    if std::fs::write(
        &path,
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
    )
    .is_err()
    {
        let _ = std::fs::File::create(&path);
    }
    Ok(path)
}

/// Remove the sentinel. Returns true when something was lifted.
fn disengage() -> bool {
    let path = sentinel_path();
    match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(_) => false,
    }
}

/// Engage the global emergency stop.
pub async fn cmd_pause(reason: Option<String>) -> Result<(), AgentError> {
    let already = is_engaged();
    let path = engage(reason.as_deref())?;
    let detail = reason
        .as_deref()
        .filter(|r| !r.is_empty())
        .map(|r| format!(" — reason: {}", r))
        .unwrap_or_default();
    let verb = if already {
        "Still paused"
    } else {
        "Hermes paused"
    };
    println!("⏸️  {}{}", verb, detail);
    println!("    sentinel: {}", path.display());
    println!(
        "    Cron dispatch, kanban dispatch, and new gateway turns (messaging and interactive\n\
         \x20   CLI, TUI and Desktop turns alike) are on hold. In-flight work keeps running.\n\
         \x20   Run `hermes resume` to lift the pause."
    );
    Ok(())
}

/// Disengage the global emergency stop.
pub async fn cmd_resume() -> Result<(), AgentError> {
    if disengage() {
        println!("▶️  Hermes resumed — dispatch picks up on the next tick.");
    } else {
        println!(
            "Hermes is not paused (no sentinel at {}).",
            sentinel_path().display()
        );
    }
    Ok(())
}
