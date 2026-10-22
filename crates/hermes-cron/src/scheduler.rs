//! Cron scheduler engine.
//!
//! The `CronScheduler` manages the lifecycle of cron jobs: creation, listing,
//! updating, pausing, resuming, removal, and execution. It runs a background
//! loop that polls for due jobs (default every 60s, overridable via
//! `HERMES_CRON_TICK_SECS`) and dispatches them to the `CronRunner`.
//!
//! Each tick:
//! - acquires the cross-process [`TickLock`][crate::tick_lock] so two
//!   schedulers (e.g. after a watchdog relaunch) never double-fire a job;
//! - runs every due job in its own async task (concurrently), so one slow
//!   job does not block the others;
//! - applies the job's [`OverlapPolicy`][crate::job::OverlapPolicy]
//!   (`skip` by default): a tick arriving while the same job's previous run
//!   is still in flight is skipped with a warning;
//! - runs each job through the [`run_job_with_retries`][crate::retry] ladder
//!   (transient failures retried up to 3x with jittered backoff);
//! - removes one-shot (`at …`) jobs from the store after they fire once.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use hermes_core::AgentResult;
use tokio::sync::{broadcast, Mutex, Notify};
use tokio::time::{self, Duration};

use crate::completion::CronCompletionEvent;
use crate::job::{CronJob, JobStatus, OverlapPolicy};
use crate::persistence::JobPersistence;
use crate::retry::run_job_with_retries;
use crate::runner::CronRunner;
use crate::tick_lock::TickLock;

/// IDs of jobs with a run currently in flight (for skip-if-running).
pub type RunningJobs = Arc<std::sync::Mutex<HashSet<String>>>;

/// Background poll interval for due jobs. Default **60** seconds.
///
/// Override with **`HERMES_CRON_TICK_SECS`** (integer **1–300**) for integration tests
/// or local debugging; values outside the range are clamped.
fn cron_poll_interval() -> Duration {
    let secs = std::env::var("HERMES_CRON_TICK_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(60)
        .clamp(1, 300);
    Duration::from_secs(secs)
}

// ---------------------------------------------------------------------------
// CronError
// ---------------------------------------------------------------------------

/// Errors that can occur during cron scheduler operations.
#[derive(Debug, thiserror::Error)]
pub enum CronError {
    #[error("Job not found: {0}")]
    JobNotFound(String),

    #[error("Invalid job: {0}")]
    InvalidJob(String),

    #[error("Job already exists: {0}")]
    JobAlreadyExists(String),

    #[error("Job is paused: {0}")]
    JobPaused(String),

    #[error("Job is completed: {0}")]
    JobCompleted(String),

    #[error("Agent error: {0}")]
    Agent(#[from] hermes_core::AgentError),

    #[error("Persistence error: {0}")]
    Persistence(String),

    #[error("Scheduler error: {0}")]
    Scheduler(String),
}

// ---------------------------------------------------------------------------
// CronScheduler
// ---------------------------------------------------------------------------

/// The cron scheduler manages cron jobs and runs a background loop that
/// dispatches due jobs to the runner.
pub struct CronScheduler {
    /// In-memory job store.
    jobs: Arc<Mutex<HashMap<String, CronJob>>>,
    /// Persistence backend.
    persistence: Arc<dyn JobPersistence>,
    /// Job runner.
    runner: Arc<CronRunner>,
    /// Optional broadcast of job completion (e.g. gateway HTTP webhooks).
    completion_tx: Option<broadcast::Sender<CronCompletionEvent>>,
    /// Notification handle to stop the scheduler loop.
    stop_notify: Arc<Notify>,
    /// Whether the scheduler loop is running.
    running: Arc<Mutex<bool>>,
    /// Job IDs with a run currently in flight (skip-if-running policy).
    in_flight: RunningJobs,
}

impl CronScheduler {
    /// Create a new cron scheduler.
    pub fn new(persistence: Arc<dyn JobPersistence>, runner: Arc<CronRunner>) -> Self {
        Self {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            persistence,
            runner,
            completion_tx: None,
            stop_notify: Arc::new(Notify::new()),
            running: Arc::new(Mutex::new(false)),
            in_flight: Arc::new(std::sync::Mutex::new(HashSet::new())),
        }
    }

    /// Receive [`CronCompletionEvent`] on every finished run (scheduled or manual).
    pub fn set_completion_broadcast(&mut self, tx: broadcast::Sender<CronCompletionEvent>) {
        self.completion_tx = Some(tx);
    }

    fn emit_completion(
        tx: Option<&broadcast::Sender<CronCompletionEvent>>,
        job: &CronJob,
        trigger: &'static str,
        outcome: Result<&AgentResult, String>,
    ) {
        let Some(sender) = tx else {
            return;
        };
        let ev = CronCompletionEvent::new(job, trigger, outcome);
        if let Err(e) = sender.send(ev) {
            tracing::debug!("cron completion broadcast dropped: {}", e);
        }
    }

    /// Load persisted jobs into the scheduler.
    ///
    /// Call this before `start()` to re-register jobs from the previous session.
    pub async fn load_persisted_jobs(&self) -> Result<(), CronError> {
        let jobs = self
            .persistence
            .load_jobs()
            .await
            .map_err(|e| CronError::Persistence(e.to_string()))?;

        let mut guard = self.jobs.lock().await;
        for job in jobs {
            tracing::info!(
                "Loaded persisted job '{}' ({})",
                job.name.as_deref().unwrap_or(&job.id),
                job.id
            );
            guard.insert(job.id.clone(), job);
        }

        tracing::info!("Loaded {} persisted cron jobs", guard.len());
        Ok(())
    }

    /// Start the scheduler loop in the background.
    ///
    /// The loop sleeps (see [`cron_poll_interval`]) then dispatches due jobs.
    /// Returns immediately; the loop runs as a spawned tokio task.
    pub async fn start(&self) {
        let mut running = self.running.lock().await;
        if *running {
            tracing::warn!("Scheduler is already running");
            return;
        }
        *running = true;
        drop(running);

        tracing::info!("Starting cron scheduler");

        let jobs = self.jobs.clone();
        let runner = self.runner.clone();
        let persistence = self.persistence.clone();
        let completion_tx = self.completion_tx.clone();
        let stop_notify = self.stop_notify.clone();
        let running_flag = self.running.clone();
        let in_flight = self.in_flight.clone();

        tokio::spawn(async move {
            loop {
                // Check if we should stop
                if !*running_flag.lock().await {
                    break;
                }

                // Wait for next poll tick or stop signal
                tokio::select! {
                    _ = time::sleep(cron_poll_interval()) => {
                        // Tick: check for due jobs
                    }
                    _ = stop_notify.notified() => {
                        tracing::info!("Scheduler received stop signal");
                        break;
                    }
                }

                Self::tick(&jobs, &persistence, &runner, &completion_tx, &in_flight).await;
            }

            tracing::info!("Scheduler loop exited");
        });
    }

    /// Process a single tick: run every due job (public so tests and
    /// embedders can drive ticks deterministically without the background
    /// loop).
    pub async fn process_due_jobs(&self) {
        Self::tick(
            &self.jobs,
            &self.persistence,
            &self.runner,
            &self.completion_tx,
            &self.in_flight,
        )
        .await;
    }

    /// One tick: acquire the cross-process tick lock, then run every due job
    /// in its own async task (concurrently).
    async fn tick(
        jobs: &Arc<Mutex<HashMap<String, CronJob>>>,
        persistence: &Arc<dyn JobPersistence>,
        runner: &Arc<CronRunner>,
        completion_tx: &Option<broadcast::Sender<CronCompletionEvent>>,
        in_flight: &RunningJobs,
    ) {
        // Cross-process tick lock: when another scheduler (e.g. a leftover
        // process after a watchdog relaunch) holds it, skip the whole tick
        // with a warning — never double-fire.
        let _tick_lock = match persistence.data_dir() {
            Some(dir) => match TickLock::acquire(&dir) {
                Ok(Some(lock)) => Some(lock),
                Ok(None) => {
                    tracing::warn!("tick skipped: another scheduler holds the cron tick lock");
                    return;
                }
                Err(e) => {
                    tracing::warn!("tick skipped: failed to acquire cron tick lock: {e}");
                    return;
                }
            },
            None => {
                tracing::debug!(
                    "cron persistence has no data dir; tick proceeds without a tick lock"
                );
                None
            }
        };

        let now = Utc::now();
        let due_jobs: Vec<CronJob> = {
            let guard = jobs.lock().await;
            guard
                .values()
                .filter(|job| job.is_due(now))
                .cloned()
                .collect()
        };

        for job in due_jobs {
            // Skip-if-running: atomic check-and-mark under one lock.
            if job.overlap == OverlapPolicy::Skip {
                let already_running = {
                    let mut set = in_flight.lock().unwrap();
                    !set.insert(job.id.clone()) // insert=false when the id is already present
                };
                if already_running {
                    tracing::warn!(
                        job_id = %job.id,
                        "tick skipped: previous run still in flight (skip-if-running)"
                    );
                    let mut skipped = job.clone();
                    skipped.advance_past_due(Utc::now());
                    Self::store_job(jobs, persistence, &skipped).await;
                    continue;
                }
            }
            let tracked = job.overlap == OverlapPolicy::Skip;
            let jobs = jobs.clone();
            let persistence = persistence.clone();
            let runner = runner.clone();
            let completion_tx = completion_tx.clone();
            let in_flight = in_flight.clone();
            tokio::spawn(async move {
                // Guard releases the id from the in-flight set — including on
                // panic — so the job never wedges. It is dropped explicitly
                // right after mark_executed/mark_failed (below), BEFORE the
                // async store: at that point next_run has advanced (or the
                // job is failed/completed), so a concurrent tick cannot
                // double-start, and observers seeing run_count increment can
                // rely on the in-flight slot being released (no await between
                // the mark and the drop).
                let _guard = RunningGuard::new(in_flight, tracked.then(|| job.id.clone()));
                let job_id = job.id.clone();
                let job_name = job.name.clone().unwrap_or_else(|| job_id.clone());
                tracing::info!("Executing cron job '{job_name}' ({job_id})");
                let mut job = job;
                let now = Utc::now();
                // Run-level retry ladder: transient failures (unreachable
                // model/provider) are retried; deterministic ones fail fast.
                match run_job_with_retries(&runner, &job).await {
                    Ok(result) => {
                        tracing::info!(
                            "Cron job '{job_id}' completed successfully (turns: {})",
                            result.total_turns
                        );
                        Self::emit_completion(
                            completion_tx.as_ref(),
                            &job,
                            "schedule",
                            Ok(&result),
                        );
                        job.mark_executed(now);
                    }
                    Err(e) => {
                        tracing::error!("Cron job '{job_id}' failed: {e}");
                        Self::emit_completion(
                            completion_tx.as_ref(),
                            &job,
                            "schedule",
                            Err(e.to_string()),
                        );
                        job.mark_failed();
                    }
                }
                // Release the in-flight slot synchronously with the mark above.
                drop(_guard);

                if job.is_oneshot() {
                    // One-shot: remove from the store after firing exactly
                    // once, whatever the run's outcome — it has "fired".
                    {
                        let mut guard = jobs.lock().await;
                        guard.remove(&job_id);
                    }
                    match persistence.delete_job(&job_id).await {
                        Ok(()) => tracing::info!(
                            "one-shot cron job '{job_id}' finished; removed from store"
                        ),
                        Err(e) => {
                            tracing::warn!("failed to remove one-shot cron job '{job_id}': {e}")
                        }
                    }
                } else {
                    Self::store_job(&jobs, &persistence, &job).await;
                }
            });
        }
    }

    /// Update a job in memory and persist it.
    async fn store_job(
        jobs: &Arc<Mutex<HashMap<String, CronJob>>>,
        persistence: &Arc<dyn JobPersistence>,
        job: &CronJob,
    ) {
        {
            let mut guard = jobs.lock().await;
            guard.insert(job.id.clone(), job.clone());
        }
        if let Err(e) = persistence.save_job(job).await {
            tracing::error!("Failed to persist job '{}': {}", job.id, e);
        }
    }

    /// Stop the scheduler loop.
    pub async fn stop(&self) {
        let mut running = self.running.lock().await;
        if !*running {
            tracing::warn!("Scheduler is not running");
            return;
        }
        *running = false;
        drop(running);

        self.stop_notify.notify_waiters();
        tracing::info!("Cron scheduler stopped");
    }

    /// Check if the scheduler is currently running.
    pub async fn is_running(&self) -> bool {
        *self.running.lock().await
    }

    /// Create a new cron job.
    ///
    /// Validates the job, generates a UUID if needed, computes the next run
    /// time, persists it, and registers it in memory.
    pub async fn create_job(&self, mut job: CronJob) -> Result<String, CronError> {
        // Validate
        job.validate().map_err(CronError::InvalidJob)?;

        // Generate UUID if empty
        if job.id.is_empty() {
            job.id = uuid::Uuid::new_v4().to_string();
        }

        let id = job.id.clone();
        let schedule = job.schedule.clone();

        // Check for duplicate
        {
            let guard = self.jobs.lock().await;
            if guard.contains_key(&id) {
                return Err(CronError::JobAlreadyExists(id));
            }
        }

        // Compute next_run if not set
        if job.next_run.is_none() && job.status == JobStatus::Active {
            job.next_run = job.compute_next_run(Utc::now());
        }

        // Persist
        self.persistence
            .save_job(&job)
            .await
            .map_err(|e| CronError::Persistence(e.to_string()))?;

        // Register in memory
        {
            let mut guard = self.jobs.lock().await;
            guard.insert(id.clone(), job);
        }

        tracing::info!("Created cron job '{}' ({})", id, schedule);
        Ok(id)
    }

    /// List all registered cron jobs.
    pub async fn list_jobs(&self) -> Vec<CronJob> {
        let guard = self.jobs.lock().await;
        guard.values().cloned().collect()
    }

    /// Get a specific job by ID.
    pub async fn get_job(&self, id: &str) -> Option<CronJob> {
        let guard = self.jobs.lock().await;
        guard.get(id).cloned()
    }

    /// Update an existing cron job.
    pub async fn update_job(&self, id: &str, updated: CronJob) -> Result<(), CronError> {
        // Validate the updated job
        updated.validate().map_err(CronError::InvalidJob)?;

        let mut guard = self.jobs.lock().await;
        if !guard.contains_key(id) {
            return Err(CronError::JobNotFound(id.to_string()));
        }

        // Ensure the ID stays consistent
        let mut job = updated;
        job.id = id.to_string();

        // Recompute next_run if schedule changed
        if job.status == JobStatus::Active && job.next_run.is_none() {
            job.next_run = job.compute_next_run(Utc::now());
        }

        // Persist
        self.persistence
            .save_job(&job)
            .await
            .map_err(|e| CronError::Persistence(e.to_string()))?;

        guard.insert(id.to_string(), job);
        Ok(())
    }

    /// Pause a cron job.
    pub async fn pause_job(&self, id: &str) -> Result<(), CronError> {
        let mut guard = self.jobs.lock().await;
        let job = guard
            .get_mut(id)
            .ok_or_else(|| CronError::JobNotFound(id.to_string()))?;

        if job.status == JobStatus::Paused {
            return Err(CronError::JobPaused(id.to_string()));
        }
        if job.status == JobStatus::Completed {
            return Err(CronError::JobCompleted(id.to_string()));
        }

        job.status = JobStatus::Paused;
        drop(guard);

        // Persist the change
        if let Some(job) = self.get_job(id).await {
            self.persistence
                .save_job(&job)
                .await
                .map_err(|e| CronError::Persistence(e.to_string()))?;
        }

        tracing::info!("Paused cron job '{}'", id);
        Ok(())
    }

    /// Resume a paused cron job.
    pub async fn resume_job(&self, id: &str) -> Result<(), CronError> {
        let mut guard = self.jobs.lock().await;
        let job = guard
            .get_mut(id)
            .ok_or_else(|| CronError::JobNotFound(id.to_string()))?;

        if job.status != JobStatus::Paused {
            return Err(CronError::Scheduler(format!(
                "Job '{}' is not paused (status: {})",
                id, job.status
            )));
        }

        job.status = JobStatus::Active;
        job.next_run = job.compute_next_run(Utc::now());
        drop(guard);

        // Persist the change
        if let Some(job) = self.get_job(id).await {
            self.persistence
                .save_job(&job)
                .await
                .map_err(|e| CronError::Persistence(e.to_string()))?;
        }

        tracing::info!("Resumed cron job '{}'", id);
        Ok(())
    }

    /// Remove a cron job.
    pub async fn remove_job(&self, id: &str) -> Result<(), CronError> {
        let mut guard = self.jobs.lock().await;
        if guard.remove(id).is_none() {
            return Err(CronError::JobNotFound(id.to_string()));
        }
        drop(guard);

        // Delete from persistence
        self.persistence
            .delete_job(id)
            .await
            .map_err(|e| CronError::Persistence(e.to_string()))?;

        tracing::info!("Removed cron job '{}'", id);
        Ok(())
    }

    /// Manually trigger a cron job to run immediately.
    ///
    /// The job is executed regardless of its schedule or status (except
    /// Completed jobs). The run count is not incremented for manual triggers.
    pub async fn run_job(&self, id: &str) -> Result<AgentResult, CronError> {
        let job = {
            let guard = self.jobs.lock().await;
            guard
                .get(id)
                .cloned()
                .ok_or_else(|| CronError::JobNotFound(id.to_string()))?
        };

        if job.status == JobStatus::Completed {
            return Err(CronError::JobCompleted(id.to_string()));
        }

        tracing::info!("Manually triggering cron job '{}'", id);
        let run_result = self.runner.run_job(&job).await;
        match &run_result {
            Ok(result) => {
                Self::emit_completion(self.completion_tx.as_ref(), &job, "manual", Ok(result))
            }
            Err(e) => Self::emit_completion(
                self.completion_tx.as_ref(),
                &job,
                "manual",
                Err(e.to_string()),
            ),
        }
        let result = run_result?;

        // Update last_run but don't increment run_count for manual triggers
        {
            let mut guard = self.jobs.lock().await;
            if let Some(j) = guard.get_mut(id) {
                j.last_run = Some(Utc::now());
                if j.status == JobStatus::Failed {
                    // Reset status to Active on successful manual run
                    j.status = JobStatus::Active;
                }
            }
        }

        // Persist the updated job
        if let Some(job) = self.get_job(id).await {
            self.persistence
                .save_job(&job)
                .await
                .map_err(|e| CronError::Persistence(e.to_string()))?;
        }

        Ok(result)
    }
}

/// Releases a job id from the in-flight set when the run task ends.
struct RunningGuard {
    running: RunningJobs,
    id: Option<String>,
}

impl RunningGuard {
    fn new(running: RunningJobs, id: Option<String>) -> Self {
        Self { running, id }
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            // Poisoned-mutex resilience: a poisoned lock still lets us clean up.
            if let Ok(mut set) = self.running.lock() {
                set.remove(&id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration as StdDuration;

    use async_trait::async_trait;
    use futures::stream::{self, StreamExt};
    use hermes_agent::agent_loop::ToolRegistry;
    use hermes_core::{AgentError, LlmProvider, LlmResponse, Message, StreamChunk, ToolSchema};
    use tempfile::{tempdir, TempDir};

    use crate::cli_support::cron_scheduler_for_data_dir;
    use crate::job::{CronJob, OverlapPolicy};
    use crate::persistence::FileJobPersistence;
    use crate::runner::CronRunner;
    use crate::tick_lock::TickLock;

    fn make_test_scheduler() -> CronScheduler {
        let dir = tempdir().expect("tempdir");
        cron_scheduler_for_data_dir(dir.path().to_path_buf())
    }

    /// Scheduler with a custom runner over `dir`.
    fn scheduler_with_runner(dir: &TempDir, runner: CronRunner) -> CronScheduler {
        let persistence = Arc::new(FileJobPersistence::with_dir(dir.path().to_path_buf()));
        CronScheduler::new(persistence, Arc::new(runner))
    }

    fn runner_with_provider(provider: Arc<dyn LlmProvider>) -> CronRunner {
        CronRunner::new(provider, Arc::new(ToolRegistry::new()))
    }

    /// Force a persisted job to be due now: rewrite its JSON `next_run`,
    /// then reload into the scheduler's memory.
    async fn force_due(sched: &CronScheduler, dir: &TempDir, id: &str) {
        let path = dir.path().join(format!("{id}.json"));
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        v["next_run"] = serde_json::json!((Utc::now() - chrono::Duration::seconds(5)).to_rfc3339());
        std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();
        sched.load_persisted_jobs().await.unwrap();
    }

    /// Wait until the persisted JSON file reflects the given run_count.
    ///
    /// `store_job` updates in-memory state before the async file write lands;
    /// a `force_due` rewrite racing that file write would be clobbered. Wait
    /// for the file to catch up first so the rewrite is stable.
    async fn wait_persisted_run_count(dir: &TempDir, id: &str, expected: u32) {
        let path = dir.path().join(format!("{id}.json"));
        poll_until(
            || {
                std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                    .and_then(|v| v.get("run_count").and_then(|n| n.as_u64()))
                    .map(|n| n as u32 == expected)
                    .unwrap_or(false)
            },
            "persisted run_count catches up",
        )
        .await;
    }

    /// Poll until `cond()` is true (timeout keeps the suite hang-free).
    async fn poll_until(cond: impl Fn() -> bool, what: &str) {
        tokio::time::timeout(StdDuration::from_secs(10), async {
            while !cond() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for: {what}"));
    }

    /// Synchronous run_count read for use inside poll_until's Fn closure.
    fn run_count_blocking(sched: &CronScheduler, id: &str) -> u32 {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async { sched.get_job(id).await.map(|j| j.run_count).unwrap_or(0) })
        })
    }

    /// Provider that counts calls and returns "done" immediately.
    struct CountingProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl LlmProvider for CountingProvider {
        async fn chat_completion(
            &self,
            _messages: &[Message],
            _tools: &[ToolSchema],
            _max_tokens: Option<u32>,
            _temperature: Option<f64>,
            model: Option<&str>,
            _extra_body: Option<&serde_json::Value>,
        ) -> Result<LlmResponse, AgentError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(LlmResponse {
                message: Message::assistant("done"),
                usage: None,
                model: model.unwrap_or("mock").to_string(),
                finish_reason: Some("stop".to_string()),
            })
        }

        fn chat_completion_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSchema],
            _max_tokens: Option<u32>,
            _temperature: Option<f64>,
            _model: Option<&str>,
            _extra_body: Option<&serde_json::Value>,
        ) -> futures::stream::BoxStream<'static, Result<StreamChunk, AgentError>> {
            stream::empty().boxed()
        }
    }

    /// Provider whose FIRST call per unique prompt blocks on `release` until
    /// the test fires it; later calls return immediately. Lets tests hold a
    /// run in flight deterministically. `first_calls` counts blocked first
    /// calls; `calls` counts every call.
    struct BlockingProvider {
        first_calls: AtomicUsize,
        calls: AtomicUsize,
        seen: std::sync::Mutex<HashSet<String>>,
        release: tokio::sync::Notify,
    }

    impl BlockingProvider {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                first_calls: AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
                seen: std::sync::Mutex::new(HashSet::new()),
                release: tokio::sync::Notify::new(),
            })
        }

        fn prompt_key(messages: &[Message]) -> String {
            messages
                .iter()
                .rev()
                .find(|m| matches!(m.role, hermes_core::MessageRole::User))
                .and_then(|m| m.content.clone())
                .unwrap_or_default()
        }
    }

    #[async_trait]
    impl LlmProvider for BlockingProvider {
        async fn chat_completion(
            &self,
            messages: &[Message],
            _tools: &[ToolSchema],
            _max_tokens: Option<u32>,
            _temperature: Option<f64>,
            model: Option<&str>,
            _extra_body: Option<&serde_json::Value>,
        ) -> Result<LlmResponse, AgentError> {
            let key = Self::prompt_key(messages);
            self.calls.fetch_add(1, Ordering::SeqCst);
            let first = {
                let mut seen = self.seen.lock().unwrap();
                seen.insert(key)
            };
            if first {
                self.first_calls.fetch_add(1, Ordering::SeqCst);
                // Block (with a backstop so a broken test cannot hang the suite).
                let _ =
                    tokio::time::timeout(StdDuration::from_secs(15), self.release.notified()).await;
            }
            Ok(LlmResponse {
                message: Message::assistant("done"),
                usage: None,
                model: model.unwrap_or("mock").to_string(),
                finish_reason: Some("stop".to_string()),
            })
        }

        fn chat_completion_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSchema],
            _max_tokens: Option<u32>,
            _temperature: Option<f64>,
            _model: Option<&str>,
            _extra_body: Option<&serde_json::Value>,
        ) -> futures::stream::BoxStream<'static, Result<StreamChunk, AgentError>> {
            stream::empty().boxed()
        }
    }

    /// Newtype so the same `Arc<BlockingProvider>` can be moved into the
    /// runner while tests keep a handle to it.
    struct BlockingProviderClone(Arc<BlockingProvider>);

    #[async_trait]
    impl LlmProvider for BlockingProviderClone {
        async fn chat_completion(
            &self,
            messages: &[Message],
            tools: &[ToolSchema],
            max_tokens: Option<u32>,
            temperature: Option<f64>,
            model: Option<&str>,
            extra_body: Option<&serde_json::Value>,
        ) -> Result<LlmResponse, AgentError> {
            self.0
                .chat_completion(messages, tools, max_tokens, temperature, model, extra_body)
                .await
        }

        fn chat_completion_stream(
            &self,
            messages: &[Message],
            tools: &[ToolSchema],
            max_tokens: Option<u32>,
            temperature: Option<f64>,
            model: Option<&str>,
            extra_body: Option<&serde_json::Value>,
        ) -> futures::stream::BoxStream<'static, Result<StreamChunk, AgentError>> {
            self.0.chat_completion_stream(
                messages,
                tools,
                max_tokens,
                temperature,
                model,
                extra_body,
            )
        }
    }

    #[tokio::test]
    async fn test_create_job_validates() {
        let mut job = CronJob::new("", "");
        job.prompt = "".to_string();
        // Validation should fail for empty schedule
        assert!(job.validate().is_err());
    }

    #[tokio::test]
    async fn test_job_status_active_by_default() {
        let job = CronJob::new("* * * * *", "test");
        assert_eq!(job.status, JobStatus::Active);
    }

    #[tokio::test]
    async fn test_scheduler_create_job_roundtrip() {
        let sched = make_test_scheduler();
        let job = CronJob::new("0 * * * *", "hello");
        let id = sched.create_job(job).await.expect("create");
        let loaded = sched.get_job(&id).await.expect("get");
        assert_eq!(loaded.prompt, "hello");
        assert_eq!(sched.list_jobs().await.len(), 1);
    }

    // --- Wave 6 parity: tick lock / overlap / concurrency / one-shot ---

    #[tokio::test]
    async fn tick_skipped_when_lock_held_by_other_scheduler() {
        let dir = tempdir().unwrap();
        let provider = Arc::new(CountingProvider {
            calls: AtomicUsize::new(0),
        });
        let sched = scheduler_with_runner(&dir, runner_with_provider(provider.clone()));

        let id = sched
            .create_job(CronJob::new("every 1s", "lock me"))
            .await
            .unwrap();
        force_due(&sched, &dir, &id).await;

        // Simulate another scheduler holding the tick lock.
        let _other = TickLock::acquire(dir.path())
            .unwrap()
            .expect("lock must be acquirable");
        sched.process_due_jobs().await;
        tokio::time::sleep(StdDuration::from_millis(300)).await;
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            0,
            "job must not run while another scheduler holds the tick lock"
        );

        // After the lock is released the next tick runs normally.
        drop(_other);
        sched.process_due_jobs().await;
        poll_until(
            || provider.calls.load(Ordering::SeqCst) >= 1,
            "job runs after lock release",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn due_jobs_run_concurrently_not_sequentially() {
        let dir = tempdir().unwrap();
        let provider = BlockingProvider::new();
        let sched = scheduler_with_runner(
            &dir,
            runner_with_provider(Arc::new(BlockingProviderClone(provider.clone()))),
        );

        let id_a = sched
            .create_job(CronJob::new("every 1s", "job-a"))
            .await
            .unwrap();
        let id_b = sched
            .create_job(CronJob::new("every 1s", "job-b"))
            .await
            .unwrap();
        force_due(&sched, &dir, &id_a).await;
        force_due(&sched, &dir, &id_b).await;

        sched.process_due_jobs().await;
        // Both runs must be in flight at the same time: each blocks on its
        // first provider call, so first_calls only reaches 2 when both have
        // started (a sequential scheduler could never start the second).
        poll_until(
            || provider.first_calls.load(Ordering::SeqCst) == 2,
            "both jobs in flight concurrently",
        )
        .await;

        provider.release.notify_waiters();
        poll_until(
            || run_count_blocking(&sched, &id_a) == 1 && run_count_blocking(&sched, &id_b) == 1,
            "both jobs completed",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn overlap_skip_skips_tick_while_running() {
        let dir = tempdir().unwrap();
        let provider = BlockingProvider::new();
        let sched = scheduler_with_runner(
            &dir,
            runner_with_provider(Arc::new(BlockingProviderClone(provider.clone()))),
        );

        let mut job = CronJob::new("every 1s", "skip-me");
        job.overlap = OverlapPolicy::Skip;
        let id = sched.create_job(job).await.unwrap();
        force_due(&sched, &dir, &id).await;

        // Tick 1: the run starts and blocks in the provider.
        sched.process_due_jobs().await;
        poll_until(
            || provider.first_calls.load(Ordering::SeqCst) == 1,
            "first run started",
        )
        .await;

        // Tick 2 while the first run is still in flight: must be skipped
        // (the job is still due because the blocked run was never marked).
        sched.process_due_jobs().await;
        tokio::time::sleep(StdDuration::from_millis(300)).await;
        assert_eq!(
            provider.first_calls.load(Ordering::SeqCst),
            1,
            "second tick must be skipped while the first run is in flight"
        );

        // Release the first run; it completes.
        provider.release.notify_waiters();
        poll_until(
            || run_count_blocking(&sched, &id) == 1,
            "first run completed",
        )
        .await;
        // Ensure the async persist (file write) has landed before rewriting
        // the file in force_due, or the rewrite races and gets clobbered.
        wait_persisted_run_count(&dir, &id, 1).await;

        // The in-flight set must have been released: force due again and the
        // next tick starts a second run. Snapshot total calls first — the
        // second run reuses the seen prompt, so it shows up as a new call
        // rather than a new first-call.
        let calls_before = provider.calls.load(Ordering::SeqCst);
        force_due(&sched, &dir, &id).await;
        sched.process_due_jobs().await;
        poll_until(
            || provider.calls.load(Ordering::SeqCst) > calls_before,
            "second run starts after release",
        )
        .await;
        provider.release.notify_waiters();
        poll_until(
            || run_count_blocking(&sched, &id) == 2,
            "second run completed",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn overlap_allow_runs_despite_inflight() {
        let dir = tempdir().unwrap();
        let provider = BlockingProvider::new();
        let mut sched = scheduler_with_runner(
            &dir,
            runner_with_provider(Arc::new(BlockingProviderClone(provider.clone()))),
        );
        // Completion events prove each run finished (run_count is racy when
        // two runs of the same job complete out of order).
        let (completion_tx, mut completion_rx) = tokio::sync::broadcast::channel(16);
        sched.set_completion_broadcast(completion_tx);

        let mut job = CronJob::new("every 1s", "allow-me");
        job.overlap = OverlapPolicy::Allow;
        let id = sched.create_job(job).await.unwrap();
        force_due(&sched, &dir, &id).await;

        // Tick 1: first run starts and blocks.
        sched.process_due_jobs().await;
        poll_until(
            || provider.first_calls.load(Ordering::SeqCst) == 1,
            "first run started",
        )
        .await;

        // Tick 2 while the first run is in flight: overlap=allow starts a
        // second run (next_run is still in the past: the blocked run was
        // never marked executed). The first run is blocked in the provider,
        // so any new provider call must belong to the second run.
        sched.process_due_jobs().await;
        poll_until(
            || provider.calls.load(Ordering::SeqCst) >= 2,
            "second run starts despite in-flight first run",
        )
        .await;

        // Both runs complete: exactly two completion events arrive.
        provider.release.notify_waiters();
        for n in 1..=2 {
            tokio::time::timeout(StdDuration::from_secs(10), completion_rx.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for completion event {n}"))
                .expect("completion channel must stay open");
        }
    }

    #[tokio::test]
    async fn oneshot_job_removed_after_single_fire() {
        let dir = tempdir().unwrap();
        let sched = scheduler_with_runner(
            &dir,
            runner_with_provider(Arc::new(CountingProvider {
                calls: AtomicUsize::new(0),
            })),
        );

        // One-shot an hour out, then force it due now.
        let future = Utc::now() + chrono::Duration::hours(1);
        let job = CronJob::new(format!("at {}", future.to_rfc3339()), "once");
        assert!(job.is_oneshot());
        let id = sched.create_job(job).await.unwrap();
        force_due(&sched, &dir, &id).await;

        sched.process_due_jobs().await;

        // The one-shot must disappear from the store after firing once.
        poll_until(
            || !Path::new(&dir.path().join(format!("{id}.json"))).exists(),
            "one-shot job removed from store",
        )
        .await;
        assert!(
            sched.get_job(&id).await.is_none(),
            "one-shot job must not linger in memory"
        );
    }
}
