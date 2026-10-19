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
//! execute recurring agent tasks. Jobs are defined by a cron expression
//! schedule, an agent prompt, and optional skill/model/deliver configurations.

pub mod backend;
pub mod cli_support;
pub mod completion;
pub mod job;
pub mod persistence;
pub mod runner;
pub mod scheduler;

// Re-export primary types
pub use backend::ScheduledCronjobBackend;
pub use cli_support::{cron_scheduler_for_data_dir, MinimalCronLlm};
pub use completion::CronCompletionEvent;
pub use job::{CronJob, DeliverConfig, DeliverTarget, JobStatus, ModelConfig};
pub use persistence::{FileJobPersistence, JobPersistence, SqliteJobPersistence};
pub use runner::CronRunner;
pub use scheduler::{CronError, CronScheduler};
