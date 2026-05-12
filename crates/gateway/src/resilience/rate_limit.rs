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
        self.check_count += 1;
        if self.check_count % 50 == 0 {
            self.prune_stale();
        }

        let now = Instant::now();
        let cutoff = now - self.window_duration;

        // Reject new keys if at capacity to prevent unbounded memory growth
        if !self.windows.contains_key(key) && self.windows.len() >= self.max_keys {
            return false;
        }

        let window = self.windows.entry(key.to_string()).or_default();

        // Remove expired entries
        while window.front().is_some_and(|&t| t < cutoff) {
            window.pop_front();
        }

        if window.len() >= self.max_requests as usize {
            return false;
        }

        window.push_back(now);
        true
    }

    fn prune_stale(&mut self) {
        let now = Instant::now();
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
        let mut limiter = SlidingWindowLimiter::new(2, Duration::from_millis(50));
        assert!(limiter.check("x"));
        assert!(limiter.check("x"));
        assert!(!limiter.check("x"));
        std::thread::sleep(Duration::from_millis(80));
        // After expiry, the window is empty again.
        assert!(limiter.check("x"));
    }

    #[test]
    fn prune_stale_drops_empty_windows() {
        let mut limiter = SlidingWindowLimiter::new(5, Duration::from_millis(20));
        for _ in 0..3 {
            assert!(limiter.check("ephemeral"));
        }
        assert!(limiter.windows.contains_key("ephemeral"));
        std::thread::sleep(Duration::from_millis(40));
        limiter.prune_stale();
        assert!(!limiter.windows.contains_key("ephemeral"));
    }
}
