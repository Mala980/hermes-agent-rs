#![allow(
    clippy::len_zero,
    clippy::redundant_closure,
    clippy::trim_split_whitespace,
    clippy::unnecessary_map_or,
    dropping_references,
    clippy::double_must_use
)]
//! # hermes-cron
//!
//! Cron job scheduler for Hermes Agent (Requirement 13).
//!
//! Provides a cron-based scheduler that can create, manage, persist, and
//! execute recurring agent tasks. Jobs are defined by a schedule (cron
//! expression, `every <interval>`, `at <RFC3339>`, or a keyword like
//! `daily`), an agent prompt, and optional skill/model/deliver
//! configurations.
//!
//! Schedules are interpreted in the job's timezone (IANA, default
//! system-local). The scheduler runs due jobs concurrently, applies a
//! skip-if-running overlap policy per job (configurable), retries transient
//! run failures with a jittered-backoff ladder, and guards each tick with a
//! cross-process lock so two schedulers never double-fire a job.

pub mod backend;
pub mod cli_support;
pub mod completion;
pub mod job;
pub mod persistence;
pub mod retry;
pub mod runner;
pub mod schedule;
pub mod scheduler;
pub mod tick_lock;

// Re-export primary types
pub use backend::ScheduledCronjobBackend;
pub use cli_support::{cron_scheduler_for_data_dir, MinimalCronLlm};
pub use completion::CronCompletionEvent;
pub use job::{CronJob, DeliverConfig, DeliverTarget, JobStatus, ModelConfig, OverlapPolicy};
pub use persistence::{FileJobPersistence, JobPersistence, SqliteJobPersistence};
pub use retry::{classify, is_retryable, retry_op, run_job_with_retries, RetryClass};
pub use runner::CronRunner;
pub use schedule::{parse_schedule, system_local_tz, validate_timezone, OnceTime, ParsedSchedule};
pub use scheduler::{CronError, CronScheduler, RunningJobs};
pub use tick_lock::{TickLock, TICK_LOCK_FILE};
