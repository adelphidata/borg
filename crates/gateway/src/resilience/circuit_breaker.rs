use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Circuit breaker for API calls that may fail persistently.
///
/// Prevents infinite failure loops (e.g. 401 loops that can cause bot deletion).
/// Uses atomics for lock-free operation on the hot path.
pub struct CircuitBreaker {
    consecutive_failures: AtomicU32,
    open: AtomicBool,
    suspended_until: AtomicU64,
    failure_threshold: u32,
    suspension_secs: u64,
}

impl CircuitBreaker {
    pub fn new(failure_threshold: u32, suspension_secs: u64) -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
            open: AtomicBool::new(false),
            suspended_until: AtomicU64::new(0),
            failure_threshold,
            suspension_secs,
        }
    }

    /// Returns `true` if the circuit is open (calls should be skipped).
    pub fn is_open(&self) -> bool {
        if !self.open.load(Ordering::Acquire) {
            return false;
        }

        let now = now_secs();
        let suspended_until = self.suspended_until.load(Ordering::Acquire);
        if now >= suspended_until {
            // Suspension period elapsed — half-open: allow a probe
            self.consecutive_failures.store(0, Ordering::Release);
            self.open.store(false, Ordering::Release);
            false
        } else {
            true
        }
    }

    /// Record a successful call — resets the failure counter.
    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Release);
        self.open.store(false, Ordering::Release);
    }

    /// Record a failure. Increments counter unconditionally.
    /// Use `record_failure_status` for status-code-aware tracking.
    pub fn record_failure(&self) {
        let count = self.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1;
        if count >= self.failure_threshold {
            self.suspended_until
                .store(now_secs() + self.suspension_secs, Ordering::Release);
            self.open.store(true, Ordering::Release);
        }
    }

    /// Record a failure with the given HTTP status code.
    /// Only 401 status codes contribute to the circuit breaker threshold.
    pub fn record_failure_status(&self, status: u16) {
        if status != 401 {
            return;
        }
        self.record_failure();
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_circuit_is_closed() {
        let cb = CircuitBreaker::new(3, 300);
        assert!(!cb.is_open());
    }

    #[test]
    fn opens_after_threshold() {
        let cb = CircuitBreaker::new(3, 300);
        for _ in 0..3 {
            cb.record_failure();
        }
        assert!(cb.is_open());
    }

    #[test]
    fn status_filtering() {
        let cb = CircuitBreaker::new(3, 300);
        for _ in 0..20 {
            cb.record_failure_status(500);
        }
        assert!(!cb.is_open());
    }

    #[test]
    fn success_resets_counter() {
        let cb = CircuitBreaker::new(3, 300);
        cb.record_failure();
        cb.record_failure();
        cb.record_success();
        cb.record_failure();
        assert!(!cb.is_open());
    }

    #[test]
    fn reopens_after_suspension_period() {
        let cb = CircuitBreaker::new(3, 300);
        for _ in 0..3 {
            cb.record_failure();
        }
        assert!(cb.is_open());
        // Simulate suspension period elapsed
        cb.suspended_until.store(0, Ordering::Relaxed);
        assert!(!cb.is_open());
    }
}
