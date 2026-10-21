//! Cross-process tick lock for the cron scheduler.
//!
//! Two scheduler processes (e.g. after a crash + watchdog relaunch) could
//! fire the same job twice. Before processing one tick, the scheduler tries
//! to acquire this exclusive lock; on failure the tick is skipped with a
//! warning log (not an error).
//!
//! Mechanism: a lock file in the cron data directory ([`TICK_LOCK_FILE`]),
//! created atomically via `create_new` (exactly one process wins). The file
//! content is the lock holder's PID; when the file already exists, the PID is
//! read and its liveness is checked via `kill(pid, 0)`. A lock from a dead
//! process is considered stale and reclaimed.
//!
//! Documented limitations:
//! - Cross-process atomicity is only guaranteed on local filesystems; on NFS
//!   or other network filesystems `create_new` semantics may not be atomic.
//! - PIDs can be reused by the OS: a stale lock whose PID happens to belong
//!   to another live process is treated as valid until that process dies.
//!   For the watchdog-relaunch scenario (the old process is truly dead) this
//!   is safe.
//! - On non-Unix platforms there is no `kill(2)`; an existing lock is always
//!   treated as held by a live process (fail-closed) so no double-fire occurs.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::scheduler::CronError;

/// Name of the lock file inside the cron data directory.
pub const TICK_LOCK_FILE: &str = "cron.tick.lock";

/// Stale-lock reclaim attempts before giving up (treated as contended).
const MAX_RECLAIM_ATTEMPTS: u32 = 3;

/// Cross-process tick-lock guard. The lock file is removed when the guard is
/// dropped; if the process crashes, the file is left behind and reclaimed by
/// the next acquisition via the PID check.
pub struct TickLock {
    path: PathBuf,
}

impl TickLock {
    /// Try to acquire the lock in `dir`.
    ///
    /// - `Ok(Some(lock))`: lock acquired; hold the guard while the tick runs.
    /// - `Ok(None)`: the lock is held by another live process → the caller
    ///   should skip the tick with a warning log (not an error).
    /// - `Err`: I/O failure beyond contention (e.g. permissions) — treat like
    ///   an unacquired lock: warn + skip the tick.
    pub fn acquire(dir: &Path) -> Result<Option<Self>, CronError> {
        std::fs::create_dir_all(dir)
            .map_err(|e| CronError::Scheduler(format!("failed to create cron data dir: {e}")))?;
        let path = dir.join(TICK_LOCK_FILE);
        for _ in 0..MAX_RECLAIM_ATTEMPTS {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    // Write the PID so other processes can detect a stale lock.
                    let _ = writeln!(f, "{}", std::process::id());
                    let _ = f.sync_all();
                    return Ok(Some(Self { path }));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    match read_lock_pid(&path) {
                        Some(pid) if pid_alive(pid) => return Ok(None),
                        // Stale lock (dead PID / unreadable content): remove it
                        // and try again. A race between two reclaiming
                        // processes is settled by the atomic create_new:
                        // exactly one wins; the loser sees the winner's PID
                        // alive and returns None.
                        _ => {
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
                Err(e) => {
                    return Err(CronError::Scheduler(format!(
                        "failed to create cron tick lockfile: {e}"
                    )))
                }
            }
        }
        // Could not reclaim after several attempts: treat as contended.
        Ok(None)
    }
}

impl Drop for TickLock {
    fn drop(&mut self) {
        // Best-effort: on a crash the file is left behind and reclaimed by
        // the next acquisition via the PID check.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Read the PID from the lockfile content; `None` when unreadable / not a
/// valid number.
fn read_lock_pid(path: &Path) -> Option<u32> {
    let content = std::fs::read_to_string(path).ok()?;
    let pid: u32 = content.trim().parse().ok()?;
    if pid == 0 {
        None
    } else {
        Some(pid)
    }
}

/// True when process `pid` is still alive.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 sends nothing; it only checks the error.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    // EPERM = the process exists but may not be signaled; ESRCH = no such process.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Without kill(2) (non-Unix): conservative, always treat the lock as valid
/// (fail-closed) so no double-fire occurs.
#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("hermes-cron-lock-test")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn second_acquire_skips_while_held() {
        let dir = test_dir("contended");
        let first = TickLock::acquire(&dir).unwrap();
        assert!(first.is_some(), "first acquisition must succeed");
        assert_eq!(
            std::fs::read_to_string(dir.join(TICK_LOCK_FILE))
                .unwrap()
                .trim()
                .parse::<u32>()
                .unwrap(),
            std::process::id(),
            "lockfile must contain our own PID"
        );
        // A second attempt while the lock is held → None (skip the tick).
        let second = TickLock::acquire(&dir).unwrap();
        assert!(second.is_none(), "second acquisition must be refused");
        drop(first);
        // After release, acquisition succeeds again and the file is cleaned up.
        assert!(
            TickLock::acquire(&dir).unwrap().is_some(),
            "lock must be acquirable again after release"
        );
    }

    #[test]
    fn stale_lock_reclaimed() {
        let dir = test_dir("stale");
        // Stale lock: PID of an already-dead process (spawned and waited).
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(
            !pid_alive(pid),
            "precondition: waited child PID must be dead"
        );
        std::fs::write(dir.join(TICK_LOCK_FILE), format!("{pid}\n")).unwrap();
        let lock = TickLock::acquire(&dir).unwrap();
        assert!(lock.is_some(), "stale lock must be reclaimed");
    }

    #[test]
    fn unreadable_lockfile_treated_as_stale() {
        let dir = test_dir("unreadable");
        std::fs::write(dir.join(TICK_LOCK_FILE), "not-a-pid\n").unwrap();
        let lock = TickLock::acquire(&dir).unwrap();
        assert!(
            lock.is_some(),
            "unreadable lockfile must be treated as stale"
        );
    }
}
