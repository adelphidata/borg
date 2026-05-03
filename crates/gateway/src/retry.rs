use std::time::Duration;

use cortex::constants;
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
/// Matches the format produced by `sandbox::runner::run_sandboxed_script` —
/// "Error (exit 4): ..." — surfaced via `bail!` in `ChannelExecutor::run_script`.
const PERMANENT_FAILURE_EXIT_CODE: &str = "Error (exit 4)";

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

    use crate::executor::ChannelExecutor;
    use crate::manifest::ChannelManifest;

    fn fast_policy(max_retries: u32) -> RetryPolicy {
        RetryPolicy {
            max_retries,
            initial_delay_ms: 1,
            max_delay_ms: 5,
            backoff_factor: 2.0,
            jitter_factor: 0.0,
        }
    }

    fn manifest_with_outbound(outbound: &str) -> ChannelManifest {
        toml::from_str(&format!(
            "name = \"retry-test\"\ndescription = \"d\"\nruntime = \"bash\"\n\n[scripts]\ninbound = \"in.sh\"\noutbound = \"{outbound}\"\n"
        ))
        .unwrap()
    }

    #[cfg(unix)]
    fn write_executable(path: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn send_with_retry_returns_success_on_first_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("calls");
        write_executable(
            &dir.path().join("out.sh"),
            &format!(
                "#!/bin/bash\necho one >> {counter}\necho ok\n",
                counter = counter.display()
            ),
        );
        let manifest = manifest_with_outbound("out.sh");
        let executor = ChannelExecutor::new(&manifest, dir.path());

        let outcome = send_with_retry(&executor, "{}", &[], &fast_policy(3)).await;
        match outcome {
            RetryOutcome::Success(s) => assert!(s.contains("ok"), "stdout: {s}"),
            other => panic!("expected Success, got {other:?}"),
        }
        let calls = std::fs::read_to_string(&counter).unwrap_or_default();
        assert_eq!(calls.lines().count(), 1, "must not retry on success");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn send_with_retry_returns_permanent_failure_on_exit_4_without_retrying() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("calls");
        write_executable(
            &dir.path().join("out.sh"),
            &format!(
                "#!/bin/bash\necho one >> {counter}\necho 'auth failed' >&2\nexit 4\n",
                counter = counter.display()
            ),
        );
        let manifest = manifest_with_outbound("out.sh");
        let executor = ChannelExecutor::new(&manifest, dir.path());

        // max_retries=5 — must still only invoke once because exit 4 is permanent.
        let outcome = send_with_retry(&executor, "{}", &[], &fast_policy(5)).await;
        match outcome {
            RetryOutcome::PermanentFailure(msg) => {
                assert!(msg.contains("exit 4"), "msg: {msg}")
            }
            other => panic!("expected PermanentFailure, got {other:?}"),
        }
        let calls = std::fs::read_to_string(&counter).unwrap_or_default();
        assert_eq!(
            calls.lines().count(),
            1,
            "exit 4 must short-circuit — no retries"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn send_with_retry_exhausts_after_max_retries_for_transient_errors() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("calls");
        write_executable(
            &dir.path().join("out.sh"),
            &format!(
                "#!/bin/bash\necho one >> {counter}\necho 'transient' >&2\nexit 1\n",
                counter = counter.display()
            ),
        );
        let manifest = manifest_with_outbound("out.sh");
        let executor = ChannelExecutor::new(&manifest, dir.path());

        let policy = fast_policy(2);
        let outcome = send_with_retry(&executor, "{}", &[], &policy).await;
        match outcome {
            RetryOutcome::Exhausted(msg) => assert!(!msg.is_empty()),
            other => panic!("expected Exhausted, got {other:?}"),
        }
        let calls = std::fs::read_to_string(&counter).unwrap_or_default();
        // Initial attempt + max_retries retries.
        assert_eq!(
            calls.lines().count(),
            (policy.max_retries as usize) + 1,
            "must invoke max_retries + 1 times"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn send_with_retry_recovers_after_transient_failure() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("calls");
        // Script appends a marker per call; fails (exit 1) on the first call,
        // succeeds (exit 0 with "ok") on every subsequent call.
        write_executable(
            &dir.path().join("out.sh"),
            &format!(
                "#!/bin/bash
echo x >> {counter}
n=$(wc -l < {counter} | tr -d ' ')
if [ \"$n\" -lt 2 ]; then
  echo 'first call failed' >&2
  exit 1
fi
echo ok
",
                counter = counter.display()
            ),
        );
        let manifest = manifest_with_outbound("out.sh");
        let executor = ChannelExecutor::new(&manifest, dir.path());

        let outcome = send_with_retry(&executor, "{}", &[], &fast_policy(3)).await;
        match outcome {
            RetryOutcome::Success(s) => assert!(s.contains("ok"), "stdout: {s}"),
            other => panic!("expected Success after retry, got {other:?}"),
        }
        let calls = std::fs::read_to_string(&counter).unwrap_or_default();
        assert_eq!(
            calls.lines().count(),
            2,
            "must retry exactly once before succeeding"
        );
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
