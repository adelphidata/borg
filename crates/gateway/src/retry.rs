use std::time::Duration;

use borg_core::constants;
use tracing::{info, warn};

use crate::executor::ChannelExecutor;

/// Configuration for exponential-backoff retry of outbound message delivery.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of retry attempts after the initial send.
    pub max_retries: u32,
    /// Base delay in milliseconds before the first retry.
    pub initial_delay_ms: u64,
    /// Upper bound on delay in milliseconds.
    pub max_delay_ms: u64,
    /// Multiplier applied to the delay on each successive attempt.
    pub backoff_factor: f64,
    /// Fraction of the delay used for random jitter (0.0 = none).
    pub jitter_factor: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: constants::RETRY_MAX_RETRIES,
            initial_delay_ms: constants::RETRY_INITIAL_DELAY_MS,
            max_delay_ms: constants::RETRY_MAX_DELAY_MS,
            backoff_factor: constants::RETRY_BACKOFF_FACTOR,
            jitter_factor: constants::RETRY_JITTER_FACTOR,
        }
    }
}

/// Result of a retried outbound send operation.
#[derive(Debug)]
pub enum RetryOutcome {
    /// The send succeeded; contains the script output.
    Success(String),
    /// A non-retryable failure occurred (exit code 4); contains the error.
    PermanentFailure(String),
    /// All retry attempts were exhausted; contains the last error.
    Exhausted(String),
}

impl RetryPolicy {
    /// Compute the delay before the given retry attempt (0-indexed), with jitter.
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let base = self.initial_delay_ms as f64 * self.backoff_factor.powi(attempt as i32);
        let capped = base.min(self.max_delay_ms as f64);

        let jitter_range = capped * self.jitter_factor;
        let jitter = jitter_range * (2.0 * rand::random::<f64>() - 1.0);
        let final_ms = (capped + jitter).max(0.0) as u64;

        Duration::from_millis(final_ms.min(self.max_delay_ms))
    }
}

/// Exit code 4 from outbound scripts signals a permanent/non-retryable failure.
const PERMANENT_FAILURE_EXIT_CODE: &str = "exited 4:";

/// Send an outbound message with automatic retries using exponential backoff.
pub async fn send_with_retry(
    executor: &ChannelExecutor<'_>,
    input_json: &str,
    blocked_paths: &[String],
    policy: &RetryPolicy,
) -> RetryOutcome {
    let mut last_error = String::new();

    for attempt in 0..=policy.max_retries {
        match executor.send_outbound(input_json, blocked_paths).await {
            Ok(output) => return RetryOutcome::Success(output),
            Err(e) => {
                last_error = e.to_string();

                if last_error.contains(PERMANENT_FAILURE_EXIT_CODE) {
                    warn!("Permanent outbound failure (exit 4): {last_error}");
                    return RetryOutcome::PermanentFailure(last_error);
                }

                if attempt < policy.max_retries {
                    let delay = policy.delay_for_attempt(attempt);
                    info!(
                        "Outbound attempt {}/{} failed, retrying in {:?}: {last_error}",
                        attempt + 1,
                        policy.max_retries + 1,
                        delay
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    warn!(
        "Outbound delivery exhausted after {} attempts: {last_error}",
        policy.max_retries + 1
    );
    RetryOutcome::Exhausted(last_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_increases_exponentially() {
        let policy = RetryPolicy {
            initial_delay_ms: 1000,
            backoff_factor: 2.0,
            jitter_factor: 0.0,
            max_delay_ms: 300_000,
            max_retries: 5,
        };

        let d0 = policy.delay_for_attempt(0);
        let d1 = policy.delay_for_attempt(1);
        let d2 = policy.delay_for_attempt(2);

        assert_eq!(d0.as_millis(), 1000);
        assert_eq!(d1.as_millis(), 2000);
        assert_eq!(d2.as_millis(), 4000);
    }

    #[test]
    fn backoff_capped_at_max() {
        let policy = RetryPolicy {
            initial_delay_ms: 100_000,
            backoff_factor: 10.0,
            jitter_factor: 0.0,
            max_delay_ms: 300_000,
            max_retries: 5,
        };

        let d = policy.delay_for_attempt(5);
        assert!(d.as_millis() <= 300_000);
    }

    #[test]
    fn zero_initial_delay() {
        let policy = RetryPolicy {
            initial_delay_ms: 0,
            backoff_factor: 2.0,
            jitter_factor: 0.0,
            max_delay_ms: 300_000,
            max_retries: 3,
        };
        let d = policy.delay_for_attempt(0);
        assert_eq!(d.as_millis(), 0);
    }

    #[test]
    fn delay_monotonically_increases_without_jitter() {
        let policy = RetryPolicy {
            initial_delay_ms: 100,
            backoff_factor: 2.0,
            jitter_factor: 0.0,
            max_delay_ms: 1_000_000,
            max_retries: 10,
        };
        let mut prev = Duration::ZERO;
        for attempt in 0..5 {
            let d = policy.delay_for_attempt(attempt);
            assert!(d >= prev, "attempt {attempt}: {d:?} < {prev:?}");
            prev = d;
        }
    }

    #[test]
    fn jitter_stays_within_bounds() {
        let policy = RetryPolicy {
            initial_delay_ms: 10_000,
            backoff_factor: 1.0,
            jitter_factor: 0.1,
            max_delay_ms: 300_000,
            max_retries: 5,
        };

        for _ in 0..100 {
            let d = policy.delay_for_attempt(0);
            let ms = d.as_millis();
            assert!(ms >= 9000, "delay {ms} too low");
            assert!(ms <= 11000, "delay {ms} too high");
        }
    }
}
