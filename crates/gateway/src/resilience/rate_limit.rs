use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// Maximum number of unique keys tracked before rejecting new clients.
const DEFAULT_MAX_KEYS: usize = 50_000;

pub struct SlidingWindowLimiter {
    windows: HashMap<String, VecDeque<Instant>>,
    max_requests: u32,
    window_duration: Duration,
    check_count: u64,
    max_keys: usize,
}

impl SlidingWindowLimiter {
    pub fn new(max_requests: u32, window_duration: Duration) -> Self {
        Self {
            windows: HashMap::new(),
            max_requests,
            window_duration,
            check_count: 0,
            max_keys: DEFAULT_MAX_KEYS,
        }
    }

    pub fn is_exempt(addr: &IpAddr) -> bool {
        addr.is_loopback()
    }

    pub fn check(&mut self, key: &str) -> bool {
        self.check_at(key, Instant::now())
    }

    /// Core of [`Self::check`] with the clock injected, so window-boundary and
    /// sweep-cadence behavior can be pinned deterministically in tests.
    fn check_at(&mut self, key: &str, now: Instant) -> bool {
        self.check_count += 1;
        // Amortized sweep: pruning every call would be O(keys) per request, and
        // never pruning leaks a window per distinct client IP.
        if self.check_count.is_multiple_of(50) {
            self.prune_stale(now);
        }

        let cutoff = now - self.window_duration;

        // Reject new keys if at capacity to prevent unbounded memory growth
        if !self.windows.contains_key(key) && self.windows.len() >= self.max_keys {
            return false;
        }

        let window = self.windows.entry(key.to_string()).or_default();

        while window.front().is_some_and(|&t| t < cutoff) {
            window.pop_front();
        }

        if window.len() >= self.max_requests as usize {
            return false;
        }

        window.push_back(now);
        true
    }

    fn prune_stale(&mut self, now: Instant) {
        let cutoff = now - self.window_duration;
        self.windows.retain(|_, window| {
            while window.front().is_some_and(|&t| t < cutoff) {
                window.pop_front();
            }
            !window.is_empty()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_and_per_key_isolation() {
        // Covers: (1) exactly `limit` requests pass, (2) the limit+1th blocks,
        // (3) a different key has its own independent window.
        let mut limiter = SlidingWindowLimiter::new(2, Duration::from_secs(60));
        assert!(limiter.check("client-a"));
        assert!(limiter.check("client-a"));
        assert!(
            !limiter.check("client-a"),
            "third request for same key must be blocked"
        );
        assert!(
            limiter.check("client-b"),
            "different key must have its own window"
        );
    }

    #[test]
    fn loopback_addrs_are_exempt() {
        // Production gateway skips rate-limiting for loopback to keep local
        // testing / health checks unthrottled. Regression here would make
        // `borg status` flaky under load.
        assert!(SlidingWindowLimiter::is_exempt(
            &"127.0.0.1".parse::<IpAddr>().unwrap()
        ));
        assert!(SlidingWindowLimiter::is_exempt(
            &"::1".parse::<IpAddr>().unwrap()
        ));
        assert!(!SlidingWindowLimiter::is_exempt(
            &"192.168.1.1".parse::<IpAddr>().unwrap()
        ));
    }

    #[test]
    fn rejects_new_keys_when_capacity_exhausted() {
        let mut limiter = SlidingWindowLimiter::new(10, Duration::from_secs(60));
        limiter.max_keys = 2;
        assert!(limiter.check("a"));
        assert!(limiter.check("b"));
        // Third distinct key exceeds capacity; must be rejected.
        assert!(!limiter.check("c"));
        // Existing keys can still be counted.
        assert!(limiter.check("a"));
    }

    #[test]
    fn sliding_window_expires_old_entries() {
        let window = Duration::from_secs(1);
        let mut limiter = SlidingWindowLimiter::new(2, window);
        let t0 = Instant::now();
        assert!(limiter.check_at("x", t0));
        assert!(limiter.check_at("x", t0));
        assert!(!limiter.check_at("x", t0));
        // After expiry, the window is empty again.
        assert!(limiter.check_at("x", t0 + Duration::from_secs(2)));
    }

    #[test]
    fn window_boundary_is_inclusive() {
        // Expiry is strict (`t < cutoff`): an entry aged exactly
        // `window_duration` has not yet left the window, so a request landing
        // on the boundary instant still counts against the limit.
        let window = Duration::from_secs(60);
        let mut limiter = SlidingWindowLimiter::new(1, window);
        let t0 = Instant::now();
        assert!(limiter.check_at("k", t0));
        assert!(
            !limiter.check_at("k", t0 + window),
            "entry aged exactly window_duration must still count toward the limit"
        );
        assert!(
            limiter.check_at("k", t0 + window + Duration::from_nanos(1)),
            "entry strictly older than window_duration must expire"
        );
    }

    #[test]
    fn stale_sweep_runs_on_the_50th_check_not_before() {
        // The stale-key sweep is amortized to every 50th check. Between
        // sweeps a stale key still occupies a capacity slot; the sweep on the
        // 50th check frees it. If the check counter stopped advancing (stuck
        // at zero) the sweep would run on every call and the very second
        // check would already admit the new key.
        let window = Duration::from_secs(1);
        let mut limiter = SlidingWindowLimiter::new(10, window);
        limiter.max_keys = 1;
        let t0 = Instant::now();
        assert!(limiter.check_at("old", t0));
        let later = t0 + Duration::from_secs(5);
        // Checks 2..=49: no sweep yet, so the stale key blocks admission.
        for n in 0..48 {
            assert!(
                !limiter.check_at("new", later),
                "check #{} must not sweep the stale key",
                n + 2
            );
        }
        // Check #50 sweeps first, freeing the slot for the new key.
        assert!(
            limiter.check_at("new", later),
            "the 50th check must run the stale sweep and admit the new key"
        );
    }

    #[test]
    fn prune_stale_drops_empty_windows() {
        let window = Duration::from_secs(1);
        let mut limiter = SlidingWindowLimiter::new(5, window);
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(limiter.check_at("ephemeral", t0));
        }
        assert!(limiter.windows.contains_key("ephemeral"));
        limiter.prune_stale(t0 + Duration::from_secs(2));
        assert!(!limiter.windows.contains_key("ephemeral"));
    }
}
