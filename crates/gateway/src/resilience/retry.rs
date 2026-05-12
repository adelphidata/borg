use std::time::Duration;

use cortex::constants;
use cortex::util::retry::{backoff_with_config, BackoffConfig};
use tracing::{info, warn};

use crate::executor::ChannelExecutor;

/// Configuration for exponential-backoff retry of outbound message delivery.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of retry attempts after the initial send.
    pub max_retries: u32,
    /// Backoff timing parameters (shared with the rest of the workspace via
    /// `cortex::util::retry::BackoffConfig`).
    pub backoff: BackoffConfig,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: constants::RETRY_MAX_RETRIES,
            backoff: BackoffConfig {
                initial: Duration::from_millis(constants::RETRY_INITIAL_DELAY_MS),
                factor: constants::RETRY_BACKOFF_FACTOR,
                max_delay: Duration::from_millis(constants::RETRY_MAX_DELAY_MS),
                jitter_fraction: constants::RETRY_JITTER_FACTOR,
            },
        }
    }
}

impl RetryPolicy {
    /// Compute the delay before the given retry attempt (0-indexed), with jitter.
    ///
    /// Delegates to `cortex::util::retry::backoff_with_config` so the entire workspace
    /// shares one backoff implementation. Jitter is additive
    /// (`base + up to jitter_fraction * base`).
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        backoff_with_config(attempt, &self.backoff)
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

    fn policy(
        initial_ms: u64,
        factor: f64,
        max_ms: u64,
        jitter: f64,
        max_retries: u32,
    ) -> RetryPolicy {
        RetryPolicy {
            max_retries,
            backoff: BackoffConfig {
                initial: Duration::from_millis(initial_ms),
                factor,
                max_delay: Duration::from_millis(max_ms),
                jitter_fraction: jitter,
            },
        }
    }

    #[test]
    fn backoff_increases_exponentially() {
        let p = policy(1000, 2.0, 300_000, 0.0, 5);
        assert_eq!(p.delay_for_attempt(0).as_millis(), 1000);
        assert_eq!(p.delay_for_attempt(1).as_millis(), 2000);
        assert_eq!(p.delay_for_attempt(2).as_millis(), 4000);
    }

    #[test]
    fn backoff_capped_at_max() {
        let p = policy(100_000, 10.0, 300_000, 0.0, 5);
        assert!(p.delay_for_attempt(5).as_millis() <= 300_000);
    }

    #[test]
    fn zero_initial_delay() {
        let p = policy(0, 2.0, 300_000, 0.0, 3);
        assert_eq!(p.delay_for_attempt(0).as_millis(), 0);
    }

    #[test]
    fn delay_monotonically_increases_without_jitter() {
        let p = policy(100, 2.0, 1_000_000, 0.0, 10);
        let mut prev = Duration::ZERO;
        for attempt in 0..5 {
            let d = p.delay_for_attempt(attempt);
            assert!(d >= prev, "attempt {attempt}: {d:?} < {prev:?}");
            prev = d;
        }
    }

    #[test]
    fn jitter_stays_within_additive_bounds() {
        // Additive jitter: result lies in [base, base * (1 + jitter_fraction)].
        let p = policy(10_000, 1.0, 300_000, 0.1, 5);
        for _ in 0..100 {
            let ms = p.delay_for_attempt(0).as_millis();
            assert!(ms >= 10_000, "delay {ms} below base");
            assert!(ms <= 11_000, "delay {ms} above base + 10%");
        }
    }

    use crate::executor::ChannelExecutor;
    use crate::manifest::ChannelManifest;

    fn fast_policy(max_retries: u32) -> RetryPolicy {
        policy(1, 2.0, 5, 0.0, max_retries)
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

        let p = fast_policy(2);
        let outcome = send_with_retry(&executor, "{}", &[], &p).await;
        match outcome {
            RetryOutcome::Exhausted(msg) => assert!(!msg.is_empty()),
            other => panic!("expected Exhausted, got {other:?}"),
        }
        let calls = std::fs::read_to_string(&counter).unwrap_or_default();
        assert_eq!(
            calls.lines().count(),
            (p.max_retries as usize) + 1,
            "must invoke max_retries + 1 times"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn send_with_retry_recovers_after_transient_failure() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("calls");
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
}
