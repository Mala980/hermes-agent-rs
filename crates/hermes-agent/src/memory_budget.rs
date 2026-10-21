//! Character budgets for memory injected into the system prompt.
//!
//! Parity with the reference port's batch-8 memory work (which mirrors the
//! original's `memory_char_limit` / `user_char_limit` semantics, adapted from
//! write-time rejection to injection-time budgeting):
//!
//! - Each entry is truncated to [`MemoryBudget::max_chars_per_entry`] chars
//!   (marked with [`TRUNCATED_MARKER`]).
//! - If the total still exceeds [`MemoryBudget::max_total_chars`], the
//!   **oldest** entries are dropped first until it fits. The fork stores
//!   entries in append order (`FileMemoryBackend::add` appends), so index 0
//!   is treated as the oldest entry.
//! - A budget value of 0 means "no limit" (safe against misconfiguration).
//! - At least the newest entry is always kept — a completely empty memory
//!   block is worse than one slightly over budget.

/// Marker appended when an entry was truncated or dropped by the budget.
pub const TRUNCATED_MARKER: &str = "[truncated]";

/// Character budgets applied when injecting memory into the system prompt.
#[derive(Debug, Clone)]
pub struct MemoryBudget {
    /// Max chars per injected entry. 0 = no limit.
    pub max_chars_per_entry: usize,
    /// Max total chars across all injected entries of one target. 0 = no limit.
    pub max_total_chars: usize,
}

impl Default for MemoryBudget {
    fn default() -> Self {
        Self {
            max_chars_per_entry: 2000,
            max_total_chars: 8000,
        }
    }
}

/// Result of applying a [`MemoryBudget`] to the entries about to be injected.
pub struct MemoryBudgetReport {
    /// Entries that fit the budget, in original order, with long entries
    /// already truncated.
    pub entries: Vec<String>,
    /// How many entries had their text truncated.
    pub truncated_entries: usize,
    /// How many oldest entries were dropped to fit the total budget.
    pub dropped_entries: usize,
}

impl MemoryBudgetReport {
    /// True when anything was truncated or dropped (caller adds a note).
    pub fn was_truncated(&self) -> bool {
        self.truncated_entries > 0 || self.dropped_entries > 0
    }
}

/// Apply the char budget to entries about to be injected into the system
/// prompt.
///
/// `entries` must be ordered oldest-first (the fork's file-backed stores
/// append new entries at the end).
pub fn apply_memory_char_budget(entries: &[String], cfg: &MemoryBudget) -> MemoryBudgetReport {
    // Stage 1: per-entry truncation.
    let mut kept: Vec<String> = Vec::with_capacity(entries.len());
    let mut truncated_entries = 0usize;
    for e in entries {
        let mut e = e.clone();
        let len = e.chars().count();
        if cfg.max_chars_per_entry > 0 && len > cfg.max_chars_per_entry {
            let cut: String = e.chars().take(cfg.max_chars_per_entry).collect();
            e = format!("{cut}… {TRUNCATED_MARKER}");
            truncated_entries += 1;
        }
        kept.push(e);
    }

    // Stage 2: drop oldest entries first while over the total budget.
    let mut dropped_entries = 0usize;
    if cfg.max_total_chars > 0 {
        let mut drop = vec![false; kept.len()];
        let mut current: usize = kept.iter().map(|e| e.chars().count()).sum();
        for i in 0..kept.len() {
            if current <= cfg.max_total_chars {
                break;
            }
            // Never drop the newest entry: one over-budget entry beats an
            // empty memory block.
            if kept.len() - dropped_entries <= 1 {
                break;
            }
            current -= kept[i].chars().count();
            drop[i] = true;
            dropped_entries += 1;
        }
        if dropped_entries > 0 {
            kept = kept
                .into_iter()
                .enumerate()
                .filter(|(i, _)| !drop[*i])
                .map(|(_, e)| e)
                .collect();
        }
    }

    MemoryBudgetReport {
        entries: kept,
        truncated_entries,
        dropped_entries,
    }
}

/// Note appended to a memory prompt block when oldest entries were dropped
/// to fit the total budget.
pub fn truncated_dropped_note(dropped: usize, total_budget: usize) -> String {
    format!(
        "{TRUNCATED_MARKER} {dropped} oldest entries dropped to fit the {total_budget}-char budget."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn per_entry_truncation_adds_marker() {
        let cfg = MemoryBudget {
            max_chars_per_entry: 10,
            max_total_chars: 0,
        };
        let r = apply_memory_char_budget(&entries(&["x".repeat(100).as_str()]), &cfg);
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.truncated_entries, 1);
        assert_eq!(r.dropped_entries, 0);
        assert!(r.entries[0].ends_with(TRUNCATED_MARKER));
        assert!(r.was_truncated());
    }

    #[test]
    fn zero_budget_means_no_limit() {
        let cfg = MemoryBudget {
            max_chars_per_entry: 0,
            max_total_chars: 0,
        };
        let r = apply_memory_char_budget(&entries(&["x".repeat(5000).as_str()]), &cfg);
        assert_eq!(r.entries[0].len(), 5000);
        assert!(!r.was_truncated());
    }

    #[test]
    fn oldest_entries_dropped_first() {
        let cfg = MemoryBudget {
            max_chars_per_entry: 0,
            max_total_chars: 7,
        };
        // 3 × 4 chars = 12 > 7: drop "aaaa", then "bbbb" (4 ≤ 7 stops).
        let r = apply_memory_char_budget(&entries(&["aaaa", "bbbb", "cccc"]), &cfg);
        assert_eq!(r.dropped_entries, 2);
        assert_eq!(r.entries, vec!["cccc".to_string()]);
        assert!(r.was_truncated());
    }

    #[test]
    fn newest_entry_never_fully_dropped() {
        let cfg = MemoryBudget {
            max_chars_per_entry: 0,
            max_total_chars: 1,
        };
        let r = apply_memory_char_budget(&entries(&["aaaa", "bbbb"]), &cfg);
        // Both over budget; only the oldest may be dropped, newest kept.
        assert_eq!(r.entries, vec!["bbbb".to_string()]);
        assert_eq!(r.dropped_entries, 1);
    }

    #[test]
    fn exact_fit_keeps_everything() {
        let cfg = MemoryBudget {
            max_chars_per_entry: 0,
            max_total_chars: 8,
        };
        let r = apply_memory_char_budget(&entries(&["aaaa", "bbbb"]), &cfg);
        assert_eq!(r.entries.len(), 2);
        assert!(!r.was_truncated());
    }

    #[test]
    fn dropped_note_mentions_count_and_budget() {
        let note = truncated_dropped_note(3, 8000);
        assert!(note.contains(TRUNCATED_MARKER));
        assert!(note.contains('3'));
        assert!(note.contains("8000"));
    }
}
