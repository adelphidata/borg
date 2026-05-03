// Re-export the shared circuit breaker from gateway level.
// Telegram-specific wrappers preserve backward compatibility.

use cortex::constants;

pub use crate::circuit_breaker::CircuitBreaker as SharedCircuitBreaker;

const FAILURE_THRESHOLD: u32 = constants::TELEGRAM_CIRCUIT_FAILURE_THRESHOLD;
const SUSPENSION_SECS: u64 = constants::TELEGRAM_CIRCUIT_SUSPENSION_SECS;

/// Telegram-specific circuit breaker with pre-configured thresholds.
pub struct CircuitBreaker(SharedCircuitBreaker);

impl CircuitBreaker {
    pub fn new() -> Self {
        Self(SharedCircuitBreaker::new(
            FAILURE_THRESHOLD,
            SUSPENSION_SECS,
        ))
    }

    pub fn is_open(&self) -> bool {
        self.0.is_open()
    }

    pub fn record_success(&self) {
        self.0.record_success();
    }

    pub fn record_failure(&self, status: u16) {
        self.0.record_failure_status(status);
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_circuit_is_closed() {
        let cb = CircuitBreaker::new();
        assert!(!cb.is_open());
    }

    #[test]
    fn non_401_failures_ignored() {
        let cb = CircuitBreaker::new();
        for _ in 0..20 {
            cb.record_failure(500);
        }
        assert!(!cb.is_open());
    }

    #[test]
    fn opens_after_threshold_401s() {
        let cb = CircuitBreaker::new();
        for _ in 0..FAILURE_THRESHOLD {
            cb.record_failure(401);
        }
        assert!(cb.is_open());
    }

    #[test]
    fn success_resets_counter() {
        let cb = CircuitBreaker::new();
        for _ in 0..(FAILURE_THRESHOLD - 1) {
            cb.record_failure(401);
        }
        assert!(!cb.is_open());
        cb.record_success();
        // One more 401 shouldn't open it since counter was reset
        cb.record_failure(401);
        assert!(!cb.is_open());
    }

    #[test]
    fn mixed_failure_codes_only_count_401s() {
        let cb = CircuitBreaker::new();
        // Interleave non-401 errors that must NOT count toward the threshold.
        for _ in 0..(FAILURE_THRESHOLD - 1) {
            cb.record_failure(401);
            cb.record_failure(500);
            cb.record_failure(429);
            cb.record_failure(403);
        }
        // Threshold-1 401s only — still closed despite many other errors.
        assert!(!cb.is_open());
        // The Nth 401 trips it.
        cb.record_failure(401);
        assert!(cb.is_open());
    }

    #[test]
    fn at_threshold_minus_one_is_still_closed() {
        let cb = CircuitBreaker::new();
        for _ in 0..(FAILURE_THRESHOLD - 1) {
            cb.record_failure(401);
        }
        assert!(!cb.is_open(), "must remain closed at threshold-1");
        cb.record_failure(401);
        assert!(cb.is_open(), "must open exactly at threshold");
    }

    #[test]
    fn record_success_after_open_resets_counter() {
        let cb = CircuitBreaker::new();
        for _ in 0..FAILURE_THRESHOLD {
            cb.record_failure(401);
        }
        assert!(cb.is_open());

        // record_success must clear both the open flag and the counter so the
        // breaker behaves like a brand-new instance afterwards.
        cb.record_success();
        assert!(!cb.is_open());

        // Threshold-1 401s should still leave it closed (proving the counter reset).
        for _ in 0..(FAILURE_THRESHOLD - 1) {
            cb.record_failure(401);
        }
        assert!(!cb.is_open());
        cb.record_failure(401);
        assert!(cb.is_open(), "should re-open only after a fresh threshold");
    }
}
