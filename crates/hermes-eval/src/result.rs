//! Run / task result records for evaluation runs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::adapter::BenchmarkMetadata;
use crate::verifier::VerificationOutcome;

/// Terminal state of a single evaluated task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Passed,
    Failed,
    Timeout,
    Error,
    Skipped,
}

/// Per-task result record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub task_id: String,
    pub category: Option<String>,
    pub status: TaskStatus,
    pub outcome: VerificationOutcome,
    pub agent_turns: u32,
    pub duration: Duration,
    /// Tokens consumed by the agent across the rollout.
    pub tokens_input: u64,
    pub tokens_output: u64,
    /// Estimated cost in USD (based on `usage_pricing`).
    pub cost_usd: f64,
    pub error: Option<String>,
}

/// Aggregate metrics across all tasks in a run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AggregateMetrics {
    pub total: u32,
    pub passed: u32,
    pub failed: u32,
    pub timeout: u32,
    pub error: u32,
    pub skipped: u32,
    /// Pass@1 = passed / total.
    pub pass_at_1: f64,
    pub total_duration: Duration,
    pub total_tokens_input: u64,
    pub total_tokens_output: u64,
    pub total_cost_usd: f64,
    /// Pass/fail threshold in percent (0–100) applied to this run, when the
    /// run was configured with one (`RunnerConfig::pass_threshold_pct`).
    /// `None` when no threshold was configured.
    #[serde(default)]
    pub threshold_pct: Option<f64>,
    /// Whether the run's score reached `threshold_pct`. `None` when no
    /// threshold was configured.
    #[serde(default)]
    pub suite_passed: Option<bool>,
}

/// Pass/fail verdict for an eval run: true when `score_pct` reaches
/// `threshold_pct` (parity with the reference port's `hermes eval run`
/// semantics; the tiny epsilon absorbs float rounding).
pub fn eval_passes(score_pct: f64, threshold_pct: f64) -> bool {
    score_pct + 1e-9 >= threshold_pct
}

/// Clamp a threshold to the valid 0–100 percent range.
pub fn clamp_threshold(t: f64) -> f64 {
    t.clamp(0.0, 100.0)
}

/// A complete evaluation run record. Written to disk for reproducibility
/// and to power `hermes bench compare`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub benchmark: BenchmarkMetadata,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Seed used for any randomised subsampling, for reproducibility.
    pub seed: u64,
    /// Concurrency level used for the rollout.
    pub concurrency: u32,
    /// Model identifier the agent was driven by, for provenance.
    pub model: String,
    pub tasks: Vec<TaskResult>,
    pub metrics: AggregateMetrics,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_passes_at_threshold() {
        assert!(eval_passes(80.0, 80.0));
        assert!(eval_passes(80.0000001, 80.0));
        assert!(!eval_passes(79.9, 80.0));
    }

    #[test]
    fn clamp_threshold_bounds() {
        assert_eq!(clamp_threshold(150.0), 100.0);
        assert_eq!(clamp_threshold(-5.0), 0.0);
        assert_eq!(clamp_threshold(72.5), 72.5);
    }

    #[test]
    fn metrics_default_has_no_verdict() {
        let m = AggregateMetrics::default();
        assert_eq!(m.threshold_pct, None);
        assert_eq!(m.suite_passed, None);
    }
}
