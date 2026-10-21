//! Run-level retry ladder for cron job execution.
//!
//! A failure to run a job because the model/provider is unreachable (network
//! error, 5xx, timeout) is retried up to [`MAX_ATTEMPTS`] times with jittered
//! backoff via [`run_job_with_retries`]; deterministic errors (auth failures,
//! bad requests, context overflow, …) fail immediately without retry.
//!
//! This layers on top of the per-LLM-call retry inside the agent loop: the
//! inner retry stabilizes each individual call, while this outer ladder
//! stabilizes the whole job run (including runners without internal retry).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hermes_core::{AgentError, AgentResult};

use crate::job::CronJob;
use crate::runner::CronRunner;
use crate::scheduler::CronError;

/// Total attempts per job run (1 initial + retries).
pub const MAX_ATTEMPTS: u32 = 3;
/// Base backoff delay between retries.
pub const BASE_DELAY: Duration = Duration::from_secs(1);
/// Upper bound of the exponential backoff component (before jitter).
pub const MAX_DELAY: Duration = Duration::from_secs(30);
/// Upper bound of the total wait for one retry (including jitter).
pub const MAX_TOTAL_DELAY: Duration = Duration::from_secs(45);

/// Classify a cron run error: retryable (transient) or not (deterministic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    /// Safe to retry: timeouts, rate limits, 5xx, network failures.
    Retryable,
    /// Fails immediately: auth, bad request, context overflow, config, …
    Fatal,
}

/// True when the error is worth retrying at the run level.
pub fn is_retryable(err: &CronError) -> bool {
    matches!(classify(err), RetryClass::Retryable)
}

/// Classify a [`CronError`] for the retry ladder.
///
/// Variant-first: [`AgentError::Timeout`], [`AgentError::RateLimited`] and
/// [`AgentError::Io`] are retryable by kind. Message-bearing variants
/// (`LlmApi`, `Gateway`, `ToolExecution`) are scanned for transient vs
/// deterministic markers, because the agent loop wraps provider failures into
/// `LlmApi(String)`. Deterministic markers win over transient ones.
pub fn classify(err: &CronError) -> RetryClass {
    match err {
        CronError::Agent(agent) => classify_agent(agent),
        // Scheduler/persistence/config problems are not transient run failures.
        _ => RetryClass::Fatal,
    }
}

fn classify_agent(err: &AgentError) -> RetryClass {
    match err {
        AgentError::Timeout(_) | AgentError::RateLimited { .. } | AgentError::Io(_) => {
            RetryClass::Retryable
        }
        AgentError::LlmApi(msg) | AgentError::Gateway(msg) | AgentError::ToolExecution(msg) => {
            classify_message(msg)
        }
        // AuthFailed, Config, Interrupted, InvalidToolCall, ContextTooLong,
        // MaxTurnsExceeded: deterministic — retrying cannot help.
        _ => RetryClass::Fatal,
    }
}

/// Deterministic markers are checked first: a "401" in the message beats a
/// "timeout" mention.
fn classify_message(msg: &str) -> RetryClass {
    let lower = msg.to_lowercase();
    const FATAL_MARKERS: &[&str] = &[
        "400",
        "401",
        "403",
        "404",
        "422",
        "unauthorized",
        "forbidden",
        "bad request",
        "invalid api key",
        "invalid_api_key",
        "authentication",
        "not found",
        "context_length_exceeded",
        "context too long",
        "maximum context",
        "token limit",
        "invalid tool call",
        "max turns",
    ];
    if FATAL_MARKERS.iter().any(|m| lower.contains(m)) {
        return RetryClass::Fatal;
    }
    const RETRYABLE_MARKERS: &[&str] = &[
        "408",
        "429",
        "500",
        "502",
        "503",
        "504",
        "timeout",
        "timed out",
        "time-out",
        "connection reset",
        "connection refused",
        "connection aborted",
        "connection closed",
        "network",
        "temporarily unavailable",
        "temporar",
        "econnreset",
        "broken pipe",
        "too many requests",
        "rate limit",
        "overloaded",
        "service unavailable",
        "gateway timeout",
        "bad gateway",
        "dns",
    ];
    if RETRYABLE_MARKERS.iter().any(|m| lower.contains(m)) {
        return RetryClass::Retryable;
    }
    // Unknown: do not retry — spinning on a deterministic failure is worse
    // than failing fast.
    RetryClass::Fatal
}

/// Deterministic exponential backoff for retry `attempt` (1-based):
/// `min(1s * 2^(attempt-1), 30s)`.
pub fn backoff_base_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(30);
    let secs = BASE_DELAY
        .as_secs()
        .saturating_mul(1u64 << shift)
        .min(MAX_DELAY.as_secs());
    Duration::from_secs(secs)
}

static JITTER_STATE: AtomicU64 = AtomicU64::new(0x9E3779B97F4A7C15);

/// Pseudo-random number in [0, 1): xorshift64* seeded from time + an atomic
/// counter. No `rand` dependency.
fn unit_random() -> f64 {
    let tick = JITTER_STATE.fetch_add(0x9E3779B97F4A7C15, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x1234_5678);
    let mut x = nanos ^ tick;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    let r = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
    // Top 53 bits -> full f64 precision.
    ((r >> 11) as f64) / ((1u64 << 53) as f64)
}

/// Backoff delay with jitter for retry `attempt` (1-based):
/// `base + uniform[0, 0.5 * base]`, capped at 45 seconds.
pub fn backoff_delay(attempt: u32) -> Duration {
    let base = backoff_base_delay(attempt);
    let jitter = Duration::from_secs_f64(unit_random() * 0.5 * base.as_secs_f64());
    base.saturating_add(jitter).min(MAX_TOTAL_DELAY)
}

/// Run one job with the retry ladder: up to [`MAX_ATTEMPTS`] total attempts,
/// retrying only retryable errors with jittered backoff between attempts.
pub async fn run_job_with_retries(
    runner: &CronRunner,
    job: &CronJob,
) -> Result<AgentResult, CronError> {
    retry_op(|| runner.run_job(job), MAX_ATTEMPTS, backoff_delay).await
}

/// Core of the ladder over a generic fallible operation, with injectable
/// attempt count and delay function, so tests can run the ladder without
/// real sleeping. `delay_for_attempt` receives the 1-based retry number
/// (1 = first retry, after the initial attempt failed).
pub async fn retry_op<F, Fut>(
    mut op: F,
    max_attempts: u32,
    delay_for_attempt: fn(u32) -> Duration,
) -> Result<AgentResult, CronError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<AgentResult, CronError>>,
{
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match op().await {
            Ok(result) => return Ok(result),
            Err(e) if attempt < max_attempts && is_retryable(&e) => {
                let delay = delay_for_attempt(attempt);
                tracing::warn!(
                    "cron job run failed (attempt {attempt}/{max_attempts}): {e}; retrying in {delay:?}"
                );
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn agent(e: AgentError) -> CronError {
        CronError::Agent(e)
    }

    fn no_delay(_attempt: u32) -> Duration {
        Duration::ZERO
    }

    /// Scripted op: fails `fail_times` times with `err`, then succeeds.
    struct Script {
        calls: AtomicUsize,
        fail_times: usize,
        err: AgentError,
    }

    impl Script {
        async fn op(&self) -> Result<AgentResult, CronError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_times {
                Err(agent(self.err.clone()))
            } else {
                // A minimal successful AgentResult.
                Ok(AgentResult {
                    messages: vec![],
                    total_turns: 1,
                    finished_naturally: true,
                    ..Default::default()
                })
            }
        }
    }

    #[test]
    fn retryable_variants_by_kind() {
        assert!(is_retryable(&agent(AgentError::Timeout("t".into()))));
        assert!(is_retryable(&agent(AgentError::RateLimited {
            retry_after_secs: None
        })));
        assert!(is_retryable(&agent(AgentError::Io("econnreset".into()))));
    }

    #[test]
    fn fatal_variants_by_kind() {
        for e in [
            AgentError::AuthFailed("bad key".into()),
            AgentError::Config("bad".into()),
            AgentError::Interrupted { message: None },
            AgentError::InvalidToolCall("bad".into()),
            AgentError::ContextTooLong,
            AgentError::MaxTurnsExceeded,
        ] {
            assert!(
                !is_retryable(&agent(e.clone())),
                "{e:?} must not be retryable"
            );
        }
        // Non-agent errors are never retried.
        assert!(!is_retryable(&CronError::Scheduler("boom".into())));
        assert!(!is_retryable(&CronError::InvalidJob("boom".into())));
    }

    #[test]
    fn message_markers_classify_llm_api() {
        // Transient markers → retryable.
        for msg in [
            "HTTP 503 Service Unavailable",
            "request timed out",
            "connection reset by peer",
            "502 Bad Gateway",
            "429 Too Many Requests",
        ] {
            assert!(
                is_retryable(&agent(AgentError::LlmApi(msg.into()))),
                "`{msg}` must be retryable"
            );
        }
        // Deterministic markers → fatal, even with a transient word nearby.
        for msg in [
            "HTTP 401 Unauthorized",
            "403 Forbidden",
            "400 Bad Request: invalid api key",
            "context_length_exceeded: too many tokens",
        ] {
            assert!(
                !is_retryable(&agent(AgentError::LlmApi(msg.into()))),
                "`{msg}` must NOT be retryable"
            );
        }
        // Unknown message → fail fast, no retry.
        assert!(!is_retryable(&agent(AgentError::LlmApi(
            "something unexpected happened".into()
        ))));
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        assert_eq!(backoff_base_delay(1), Duration::from_secs(1));
        assert_eq!(backoff_base_delay(2), Duration::from_secs(2));
        assert_eq!(backoff_base_delay(3), Duration::from_secs(4));
        assert_eq!(backoff_base_delay(100), MAX_DELAY);
        for attempt in 1..=4 {
            let d = backoff_delay(attempt);
            assert!(
                d >= backoff_base_delay(attempt) && d <= MAX_TOTAL_DELAY,
                "attempt {attempt}: {d:?}"
            );
        }
    }

    #[tokio::test]
    async fn ladder_retries_transient_then_succeeds() {
        let script = Script {
            calls: AtomicUsize::new(0),
            fail_times: 2,
            err: AgentError::Timeout("timed out".into()),
        };
        let res = retry_op(|| script.op(), 3, no_delay).await;
        assert!(res.is_ok());
        assert_eq!(
            script.calls.load(Ordering::SeqCst),
            3,
            "two timeouts then success = exactly 3 attempts"
        );
    }

    #[tokio::test]
    async fn ladder_retries_5xx_message_then_succeeds() {
        let script = Script {
            calls: AtomicUsize::new(0),
            fail_times: 2,
            err: AgentError::LlmApi("HTTP 503 Service Unavailable".into()),
        };
        let res = retry_op(|| script.op(), 3, no_delay).await;
        assert!(res.is_ok());
        assert_eq!(script.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn ladder_fails_fast_on_deterministic_error() {
        let script = Script {
            calls: AtomicUsize::new(0),
            fail_times: 100,
            err: AgentError::AuthFailed("bad key".into()),
        };
        let res = retry_op(|| script.op(), 3, no_delay).await;
        assert!(res.is_err());
        assert_eq!(
            script.calls.load(Ordering::SeqCst),
            1,
            "auth failure must not be retried"
        );
    }

    #[tokio::test]
    async fn ladder_gives_up_after_max_attempts() {
        let script = Script {
            calls: AtomicUsize::new(0),
            fail_times: 100,
            err: AgentError::Timeout("timed out".into()),
        };
        let res = retry_op(|| script.op(), 3, no_delay).await;
        assert!(res.is_err());
        assert_eq!(
            script.calls.load(Ordering::SeqCst),
            3,
            "persistent transient failure = exactly MAX_ATTEMPTS attempts"
        );
    }
}
