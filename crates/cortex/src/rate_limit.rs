//! Shared token-bucket rate limiter.
//!
//! A single implementation backing every per-key throttle in the crate (today:
//! per-MCP-server in [`crate::mcp_clients`] and per-WASM-plugin in
//! [`crate::wasm_plugins`]). Each caller keeps its own
//! `OnceLock<Mutex<HashMap<Key, TokenBucket>>>` map — they key differently — but
//! the refill math and fail-closed-on-misconfiguration behaviour live here so a
//! fix never has to be hand-replicated across modules.

use std::time::{Duration, Instant};

/// Classic token bucket. Tokens accrue at `per_minute / 60` per second up to
/// `burst`; one is consumed per call. A refused call returns the time until the
/// next token is available so the caller can surface a sensible retry hint.
pub struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    /// Create a bucket pre-filled with `initial` tokens (callers typically pass
    /// `burst` so the first calls succeed immediately).
    pub fn new(initial: f64) -> Self {
        Self {
            tokens: initial,
            last_refill: Instant::now(),
        }
    }

    /// Refill against wall-clock and try to consume one token. Returns `None`
    /// on success, or `Some(retry_after)` on refusal.
    pub fn try_consume(&mut self, per_minute: u32, burst: u32) -> Option<Duration> {
        if per_minute == 0 || burst == 0 {
            // Misconfigured-off must fail closed, never silently mean
            // "unlimited" — that would turn a clumsy operator into a
            // denial-of-budget.
            return Some(crate::constants::RATE_LIMIT_MISCONFIGURED_RETRY);
        }
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        let refill = elapsed * (f64::from(per_minute) / 60.0);
        self.tokens = (self.tokens + refill).min(f64::from(burst));
        self.last_refill = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            None
        } else {
            let deficit = 1.0 - self.tokens;
            let seconds_to_one = deficit / (f64::from(per_minute) / 60.0);
            Some(Duration::from_secs_f64(seconds_to_one.max(0.001)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_after_burst() {
        // With per_minute=60 and burst=2 the first two calls within ~0s
        // succeed and the third is refused. Locks the math against an
        // off-by-one refill regression.
        let mut b = TokenBucket::new(2.0);
        assert!(b.try_consume(60, 2).is_none(), "first take succeeds");
        assert!(b.try_consume(60, 2).is_none(), "second take succeeds");
        let denial = b.try_consume(60, 2).expect("third take must be refused");
        // Refill at 1/sec, deficit ≈ 1 token, so retry_after ≈ 1s. Allow wide
        // slack — clock noise can shave a few ms either way.
        assert!(
            denial.as_secs_f64() > 0.5 && denial.as_secs_f64() < 2.0,
            "retry_after should be ~1s, got {denial:?}",
        );
    }

    #[test]
    fn zero_config_fails_closed() {
        // per_minute=0 must NOT silently mean "unlimited"; burst=0 likewise.
        let mut b = TokenBucket::new(10.0);
        assert!(b.try_consume(0, 10).is_some());
        let mut b = TokenBucket::new(10.0);
        assert!(b.try_consume(60, 0).is_some());
    }

    #[test]
    fn refills_over_time() {
        // A drained bucket refills at `per_minute / 60` tokens per second up to
        // `burst`. Dropping the elapsed-seconds multiplier would silently turn
        // the bucket into either "never refills" or "always full" — both
        // observable only when a real operator hits the limit. `60 per minute,
        // burst = 1` is one token per second: after draining, sleeping 1.1s
        // must restore at least one token.
        let mut b = TokenBucket::new(1.0);
        assert!(b.try_consume(60, 1).is_none(), "initial token");
        assert!(
            b.try_consume(60, 1).is_some(),
            "second call must be refused — burst is 1"
        );
        std::thread::sleep(Duration::from_millis(1_100));
        assert!(
            b.try_consume(60, 1).is_none(),
            "after 1.1s the bucket must have refilled at least one token"
        );
    }
}
