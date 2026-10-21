//! Cron job definition and related types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::schedule::{parse_schedule, system_local_tz, validate_timezone};

// ---------------------------------------------------------------------------
// JobStatus
// ---------------------------------------------------------------------------

/// Status of a cron job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Job is active and will be scheduled.
    Active,
    /// Job is paused and will not be scheduled.
    Paused,
    /// Job has completed its repeat count.
    Completed,
    /// Job failed during its last execution.
    Failed,
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobStatus::Active => write!(f, "active"),
            JobStatus::Paused => write!(f, "paused"),
            JobStatus::Completed => write!(f, "completed"),
            JobStatus::Failed => write!(f, "failed"),
        }
    }
}

// ---------------------------------------------------------------------------
// OverlapPolicy
// ---------------------------------------------------------------------------

/// What to do when a tick arrives while the same job's previous run is still
/// in flight.
///
/// - [`OverlapPolicy::Skip`] (default): skip the tick with a warning log and
///   advance the schedule without counting a run.
/// - [`OverlapPolicy::Allow`]: start another run concurrently with the
///   in-flight one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlapPolicy {
    #[default]
    Skip,
    Allow,
}

impl std::fmt::Display for OverlapPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OverlapPolicy::Skip => write!(f, "skip"),
            OverlapPolicy::Allow => write!(f, "allow"),
        }
    }
}

impl FromStr for OverlapPolicy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "skip" | "skip-if-running" => Ok(OverlapPolicy::Skip),
            "allow" => Ok(OverlapPolicy::Allow),
            other => Err(format!(
                "unknown overlap policy: `{other}` (use `skip` or `allow`)"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// ModelConfig
// ---------------------------------------------------------------------------

/// Optional model configuration override for a cron job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelConfig {
    /// Provider name override (e.g. "openai", "anthropic").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Model name override (e.g. "gpt-4o", "claude-3-5-sonnet").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

// ---------------------------------------------------------------------------
// DeliverTarget / DeliverConfig
// ---------------------------------------------------------------------------

/// Target platform for delivering cron job results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliverTarget {
    /// Return results to the origin (caller).
    Origin,
    /// Deliver locally (e.g. write to file, log).
    Local,
    /// Deliver via Telegram.
    Telegram,
    /// Deliver via Discord.
    Discord,
    /// Deliver via Slack.
    Slack,
    /// Deliver via Email.
    Email,
    /// Deliver via WhatsApp.
    WhatsApp,
    /// Deliver via Signal.
    Signal,
    /// Deliver via Matrix.
    Matrix,
    /// Deliver via Mattermost.
    Mattermost,
    /// Deliver via DingTalk.
    DingTalk,
    /// Deliver via Feishu.
    Feishu,
    /// Deliver via WeCom.
    WeCom,
    /// Deliver via WeChat.
    Weixin,
    /// Deliver via BlueBubbles (iMessage).
    BlueBubbles,
    /// Deliver via SMS.
    Sms,
    /// Deliver via Home Assistant.
    HomeAssistant,
}

/// Delivery configuration for a cron job's results.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeliverConfig {
    /// The target platform for delivery.
    pub target: DeliverTarget,
    /// Platform-specific identifier (e.g. chat_id, channel, email address).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

// ---------------------------------------------------------------------------
// CronJob
// ---------------------------------------------------------------------------

/// A scheduled cron job that runs an agent prompt on a recurring basis.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CronJob {
    /// Unique identifier (UUID).
    pub id: String,
    /// Human-readable name for this job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Cron expression schedule (e.g. "0 9 * * *") or any accepted schedule
    /// format ("every 15m", "daily", "at 2030-01-01T09:00:00+08:00", …).
    /// Stored in canonical form (see [`crate::schedule`]).
    pub schedule: String,
    /// The prompt to send to the agent when this job fires.
    pub prompt: String,
    /// Skills to load for the agent (by name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    /// Optional model configuration override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelConfig>,
    /// Optional delivery configuration for results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliver: Option<DeliverConfig>,
    /// Current status of the job.
    #[serde(default = "default_job_status")]
    pub status: JobStatus,
    /// When this job was created.
    pub created_at: DateTime<Utc>,
    /// When this job was last executed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<DateTime<Utc>>,
    /// When this job is next scheduled to run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run: Option<DateTime<Utc>>,
    /// Maximum number of times to repeat this job (None = unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<u32>,
    /// Number of times this job has been executed so far.
    #[serde(default)]
    pub run_count: u32,
    /// Optional script content to execute instead of an agent prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// IANA timezone name for interpreting the schedule's wall-clock
    /// (e.g. "Asia/Makassar"). `None` = system-local timezone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// Overlap policy when a tick arrives while the previous run is still
    /// in flight; defaults to [`OverlapPolicy::Skip`].
    #[serde(default)]
    pub overlap: OverlapPolicy,
}

fn default_job_status() -> JobStatus {
    JobStatus::Active
}

impl CronJob {
    /// Create a new CronJob with a generated UUID and the given schedule/prompt.
    ///
    /// The schedule is parsed with [`parse_schedule`]; on success it is
    /// stored in canonical form and `next_run` is computed in the
    /// system-local timezone. On parse failure the raw string is kept and
    /// `next_run` stays `None` — [`CronJob::validate`] reports the problem
    /// when the job is created through the scheduler.
    pub fn new(schedule: impl Into<String>, prompt: impl Into<String>) -> Self {
        let raw = schedule.into();
        let now = Utc::now();
        let (schedule, next_run) = match parse_schedule(&raw) {
            Ok(parsed) => {
                let tz = system_local_tz();
                let next = parsed.next_run_after(tz, now, None);
                (parsed.canonical(), next)
            }
            Err(_) => (raw, None),
        };
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: None,
            schedule,
            prompt: prompt.into(),
            skills: None,
            model: None,
            deliver: None,
            status: JobStatus::Active,
            created_at: now,
            last_run: None,
            next_run,
            repeat: None,
            run_count: 0,
            script: None,
            timezone: None,
            overlap: OverlapPolicy::default(),
        }
    }

    /// Validate the cron job definition.
    ///
    /// Checks:
    /// - The schedule is a valid schedule expression (cron / `every` /
    ///   `at` / keyword — see [`crate::schedule`])
    /// - The timezone, when set, is a valid IANA name
    /// - The prompt is non-empty (or a script is provided)
    /// - The repeat value is not zero
    pub fn validate(&self) -> Result<(), String> {
        // Validate schedule
        if self.schedule.trim().is_empty() {
            return Err("Schedule expression cannot be empty".to_string());
        }
        if let Err(e) = parse_schedule(&self.schedule) {
            return Err(format!("Invalid schedule: {e}"));
        }

        // Validate timezone
        if let Some(ref tz) = self.timezone {
            validate_timezone(tz)?;
        }

        // Validate prompt or script
        if self.prompt.trim().is_empty()
            && self.script.as_ref().map_or(true, |s| s.trim().is_empty())
        {
            return Err("Either prompt or script must be non-empty".to_string());
        }

        // Validate repeat
        if let Some(repeat) = self.repeat {
            if repeat == 0 {
                return Err("Repeat count must be greater than zero if specified".to_string());
            }
        }

        Ok(())
    }

    /// Resolve the job's timezone: the configured IANA name when set and
    /// valid, otherwise the system-local timezone (UTC when undetectable).
    /// An invalid configured name falls back with a warning.
    pub fn job_timezone(&self) -> chrono_tz::Tz {
        match self.timezone.as_deref() {
            Some(name) => match name.parse::<chrono_tz::Tz>() {
                Ok(tz) => tz,
                Err(_) => {
                    tracing::warn!(
                        "invalid timezone `{name}` on cron job {}; falling back to system-local",
                        self.id
                    );
                    system_local_tz()
                }
            },
            None => system_local_tz(),
        }
    }

    /// True when this is a one-shot (`at …`) job.
    pub fn is_oneshot(&self) -> bool {
        parse_schedule(&self.schedule)
            .map(|p| p.is_oneshot())
            .unwrap_or(false)
    }

    /// Compute the next run time based on the schedule, starting from `after`.
    ///
    /// A one-shot that already fired (`last_run` set) never reschedules.
    pub fn compute_next_run(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let tz = self.job_timezone();
        parse_schedule(&self.schedule)
            .ok()?
            .next_run_after(tz, after, self.last_run)
    }

    /// Advance `next_run` without marking a run (used when a tick is skipped
    /// by the skip-if-running policy): `last_run` and `run_count` are
    /// untouched. When the recomputed time is still due, force it +60s so a
    /// very tight schedule does not spin.
    pub fn advance_past_due(&mut self, now: DateTime<Utc>) {
        self.next_run = self.compute_next_run(now);
        if let Some(n) = self.next_run {
            if n <= now {
                self.next_run = Some(now + chrono::Duration::seconds(60));
            }
        }
    }

    /// Check whether this job is due to run at the given time.
    pub fn is_due(&self, now: DateTime<Utc>) -> bool {
        if self.status != JobStatus::Active {
            return false;
        }
        match self.next_run {
            Some(next) => now >= next,
            None => false,
        }
    }

    /// Mark the job as having just been run: increment run_count, set last_run,
    /// and compute next_run. Returns false if the job has reached its repeat limit.
    pub fn mark_executed(&mut self, now: DateTime<Utc>) -> bool {
        self.run_count += 1;
        self.last_run = Some(now);

        // Check repeat limit
        if let Some(repeat) = self.repeat {
            if self.run_count >= repeat {
                self.status = JobStatus::Completed;
                self.next_run = None;
                return false;
            }
        }

        // Schedule next run
        self.next_run = self.compute_next_run(now);
        true
    }

    /// Mark the job as failed.
    pub fn mark_failed(&mut self) {
        self.status = JobStatus::Failed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_job_new() {
        let job = CronJob::new("0 9 * * *", "Say hello");
        assert_eq!(job.status, JobStatus::Active);
        assert_eq!(job.run_count, 0);
        assert!(job.next_run.is_some());
        assert!(job.id.len() > 0);
    }

    #[test]
    fn test_validate_valid() {
        let job = CronJob::new("0 9 * * *", "Say hello");
        assert!(job.validate().is_ok());
    }

    #[test]
    fn test_validate_empty_schedule() {
        let mut job = CronJob::new("", "Say hello");
        job.schedule = "".to_string();
        assert!(job.validate().is_err());
    }

    #[test]
    fn test_validate_empty_prompt() {
        let mut job = CronJob::new("0 9 * * *", "");
        job.prompt = "".to_string();
        assert!(job.validate().is_err());
    }

    #[test]
    fn test_validate_zero_repeat() {
        let mut job = CronJob::new("0 9 * * *", "Say hello");
        job.repeat = Some(0);
        assert!(job.validate().is_err());
    }

    #[test]
    fn test_is_due() {
        let job = CronJob::new("* * * * *", "Say hello");
        // A minutely job: next_run is in the next minute.
        // is_due returns true when now >= next_run, so it might not be due yet
        // if we just created it. Instead, verify that next_run is set and is
        // in the near future.
        assert!(job.next_run.is_some());
        let next = job.next_run.unwrap();
        // next_run should be within the next 2 minutes
        let diff = next - Utc::now();
        assert!(
            diff.num_seconds() >= 0 && diff.num_seconds() <= 120,
            "next_run should be within the next 2 minutes, got {}s",
            diff.num_seconds()
        );
    }

    #[test]
    fn test_is_due_paused() {
        let mut job = CronJob::new("* * * * *", "Say hello");
        job.status = JobStatus::Paused;
        assert!(!job.is_due(Utc::now()));
    }

    #[test]
    fn test_mark_executed_increments() {
        let mut job = CronJob::new("* * * * *", "Say hello");
        let now = Utc::now();
        let result = job.mark_executed(now);
        assert!(result);
        assert_eq!(job.run_count, 1);
        assert_eq!(job.last_run, Some(now));
        assert!(job.next_run.is_some());
    }

    #[test]
    fn test_mark_executed_repeat_limit() {
        let mut job = CronJob::new("* * * * *", "Say hello");
        job.repeat = Some(1);
        let now = Utc::now();
        let result = job.mark_executed(now);
        assert!(!result);
        assert_eq!(job.status, JobStatus::Completed);
        assert!(job.next_run.is_none());
    }

    #[test]
    fn test_mark_failed() {
        let mut job = CronJob::new("* * * * *", "Say hello");
        job.mark_failed();
        assert_eq!(job.status, JobStatus::Failed);
    }

    #[test]
    fn test_job_status_display() {
        assert_eq!(JobStatus::Active.to_string(), "active");
        assert_eq!(JobStatus::Paused.to_string(), "paused");
        assert_eq!(JobStatus::Completed.to_string(), "completed");
        assert_eq!(JobStatus::Failed.to_string(), "failed");
    }

    #[test]
    fn test_serde_roundtrip() {
        let job = CronJob::new("0 9 * * *", "Say hello");
        let json = serde_json::to_string(&job).unwrap();
        let parsed: CronJob = serde_json::from_str(&json).unwrap();
        assert_eq!(job, parsed);
    }

    // --- Schedule formats / timezone / overlap (Wave 6 parity) ---

    #[test]
    fn test_new_stores_canonical_schedule() {
        // 5-field input is normalized to canonical 6-field form.
        let job = CronJob::new("0 9 * * *", "Say hello");
        assert_eq!(job.schedule, "0 0 9 * * *");
        assert!(job.validate().is_ok());

        let job = CronJob::new("every 15m", "Say hello");
        assert_eq!(job.schedule, "every 900s");
        assert!(job.validate().is_ok());

        let job = CronJob::new("daily", "Say hello");
        assert_eq!(job.schedule, "0 0 0 * * *");
        assert!(job.validate().is_ok());
    }

    #[test]
    fn test_new_keeps_raw_on_parse_failure() {
        // Invalid schedules are kept verbatim so validate() can report them.
        let job = CronJob::new("not a schedule", "Say hello");
        assert!(job.next_run.is_none());
        let err = job.validate().unwrap_err();
        assert!(
            err.contains("Invalid schedule") && err.contains("every 15m"),
            "error must name accepted formats: {err}"
        );
    }

    #[test]
    fn test_validate_rejects_bad_timezone() {
        let mut job = CronJob::new("0 9 * * *", "Say hello");
        job.timezone = Some("Mars/Olympus".to_string());
        let err = job.validate().unwrap_err();
        assert!(err.contains("IANA"), "message: {err}");

        job.timezone = Some("Asia/Makassar".to_string());
        assert!(job.validate().is_ok());
    }

    #[test]
    fn test_job_timezone_defaults_and_fallbacks() {
        let job = CronJob::new("0 9 * * *", "Say hello");
        // Default = system-local (must not panic).
        let _ = job.job_timezone();

        let mut bad = job.clone();
        bad.timezone = Some("not/a/zone".to_string());
        // Invalid configured name falls back instead of panicking.
        let _ = bad.job_timezone();

        let mut good = job.clone();
        good.timezone = Some("Asia/Makassar".to_string());
        assert_eq!(good.job_timezone(), chrono_tz::Asia::Makassar);
    }

    #[test]
    fn test_overlap_policy_defaults_and_parses() {
        let job = CronJob::new("0 9 * * *", "Say hello");
        assert_eq!(job.overlap, OverlapPolicy::Skip);
        assert_eq!(
            "skip".parse::<OverlapPolicy>().unwrap(),
            OverlapPolicy::Skip
        );
        assert_eq!(
            "skip-if-running".parse::<OverlapPolicy>().unwrap(),
            OverlapPolicy::Skip
        );
        assert_eq!(
            "allow".parse::<OverlapPolicy>().unwrap(),
            OverlapPolicy::Allow
        );
        assert_eq!(OverlapPolicy::Skip.to_string(), "skip");
        assert_eq!(OverlapPolicy::Allow.to_string(), "allow");
        assert!("bogus".parse::<OverlapPolicy>().is_err());
    }

    #[test]
    fn test_serde_backward_compat_without_new_fields() {
        // Jobs persisted before timezone/overlap existed must still load,
        // with the new fields taking their defaults.
        let old = serde_json::json!({
            "id": "abc",
            "schedule": "0 0 9 * * *",
            "prompt": "Say hello",
            "status": "active",
            "created_at": "2026-10-22T00:00:00Z",
            "run_count": 0
        });
        let job: CronJob = serde_json::from_value(old).unwrap();
        assert_eq!(job.timezone, None);
        assert_eq!(job.overlap, OverlapPolicy::Skip);
    }

    #[test]
    fn test_serde_roundtrip_with_timezone_and_overlap() {
        let mut job = CronJob::new("every 1h", "Say hello");
        job.timezone = Some("Asia/Makassar".to_string());
        job.overlap = OverlapPolicy::Allow;
        let json = serde_json::to_string(&job).unwrap();
        let parsed: CronJob = serde_json::from_str(&json).unwrap();
        assert_eq!(job, parsed);
    }

    #[test]
    fn test_is_oneshot() {
        let job = CronJob::new("at 2030-06-15T01:00:00Z", "Say hello");
        assert!(job.is_oneshot());
        assert!(job.next_run.is_some());

        let cron = CronJob::new("0 9 * * *", "Say hello");
        assert!(!cron.is_oneshot());
    }

    #[test]
    fn test_advance_past_due_moves_schedule_forward() {
        let mut job = CronJob::new("* * * * *", "Say hello");
        let now = Utc::now();
        job.next_run = Some(now - chrono::Duration::seconds(5));
        job.advance_past_due(now);
        let next = job.next_run.expect("must be rescheduled");
        assert!(next > now, "skipped tick must move next_run forward");
        // last_run untouched: this was a skip, not a run.
        assert_eq!(job.last_run, None);
        assert_eq!(job.run_count, 0);
    }

    #[test]
    fn test_oneshot_next_run_none_after_fire() {
        let mut job = CronJob::new("at 2030-06-15T01:00:00Z", "Say hello");
        let now = Utc::now();
        assert!(job.mark_executed(now));
        // A fired one-shot never reschedules.
        assert_eq!(job.next_run, None);
        assert!(!job.is_due(now + chrono::Duration::hours(48)));
    }
}
