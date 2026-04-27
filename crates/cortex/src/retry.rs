use std::time::Duration;

use rand::Rng;

/// Configuration for exponential backoff with jitter.
#[derive(Debug, Clone)]
pub struct BackoffConfig {
    /// Base delay for the first attempt.
    pub initial: Duration,
    /// Exponential growth factor per attempt.
    pub factor: f64,
    /// Hard upper bound on the computed delay.
    pub max_delay: Duration,
    /// Jitter fraction applied to the delay. `0.1` means up to +10% additive jitter.
    pub jitter_fraction: f64,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(200),
            factor: 2.0,
            max_delay: Duration::from_secs(300), // 5 minutes
            jitter_fraction: 0.1,
        }
    }
}

/// Compute the delay for an exponential backoff with jitter.
///
/// `attempt` is 0-indexed: attempt 0 → `initial`, attempt 1 → `initial * factor`, etc.
/// Jitter adds random noise controlled by `config.jitter_fraction`.
pub fn backoff_with_config(attempt: u32, config: &BackoffConfig) -> Duration {
    let max_ms = config.max_delay.as_secs_f64() * 1000.0;
    let base = config.initial.as_millis() as f64 * config.factor.powi(attempt as i32);
    let capped = if base.is_finite() {
        base.clamp(0.0, max_ms)
    } else {
        max_ms
    };
    let jitter = capped * config.jitter_fraction * rand::rng().random::<f64>();
    let delay_ms = (capped + jitter).clamp(0.0, max_ms + max_ms * config.jitter_fraction);
    Duration::from_millis(delay_ms as u64)
}

/// Convenience wrapper: 0-indexed attempt, ±10% symmetric jitter, 5-minute cap.
pub fn backoff_delay(attempt: u32, initial: Duration, factor: f64) -> Duration {
    const MAX_DELAY_MS: f64 = 300_000.0; // 5 minutes
    let base = initial.as_millis() as f64 * factor.powi(attempt as i32);
    let jitter = rand::rng().random_range(0.9..=1.1);
    let raw = base * jitter;
    let delay_ms = if raw.is_finite() {
        raw.clamp(0.0, MAX_DELAY_MS)
    } else {
        MAX_DELAY_MS
    };
    Duration::from_millis(delay_ms as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_attempt_zero_is_near_initial() {
        let delay = backoff_delay(0, Duration::from_millis(200), 2.0);
        // Should be within ±10% of 200ms
        assert!(delay.as_millis() >= 180);
        assert!(delay.as_millis() <= 220);
    }

    #[test]
    fn backoff_increases_with_attempts() {
        let d0 = backoff_delay(0, Duration::from_millis(200), 2.0);
        let d1 = backoff_delay(1, Duration::from_millis(200), 2.0);
        let d2 = backoff_delay(2, Duration::from_millis(200), 2.0);
        // Each should roughly double (within jitter tolerance)
        assert!(d1.as_millis() > d0.as_millis());
        assert!(d2.as_millis() > d1.as_millis());
    }

    #[test]
    fn backoff_stays_reasonable() {
        let delay = backoff_delay(5, Duration::from_millis(200), 2.0);
        // 200 * 2^5 = 6400ms, with jitter ≤ ~7040ms
        assert!(delay.as_millis() <= 8000);
    }

    #[test]
    fn backoff_caps_at_max_delay() {
        // Very high attempt count would overflow without capping
        let delay = backoff_delay(1000, Duration::from_millis(200), 2.0);
        assert!(delay.as_millis() <= 300_000); // 5 min cap
        assert!(delay.as_millis() > 0);
    }

    #[test]
    fn backoff_handles_zero_initial() {
        let delay = backoff_delay(5, Duration::from_millis(0), 2.0);
        assert_eq!(delay.as_millis(), 0);
    }

    #[test]
    fn backoff_factor_one_no_growth() {
        // With factor 1.0, delay should stay near initial regardless of attempt
        // Wider tolerance than jitter range to focus on "no exponential growth"
        let d0 = backoff_delay(0, Duration::from_millis(500), 1.0);
        let d5 = backoff_delay(5, Duration::from_millis(500), 1.0);
        let d10 = backoff_delay(10, Duration::from_millis(500), 1.0);
        for d in [d0, d5, d10] {
            assert!(d.as_millis() >= 440, "delay {} too low", d.as_millis());
            assert!(d.as_millis() <= 560, "delay {} too high", d.as_millis());
        }
    }

    #[test]
    fn backoff_jitter_within_bounds() {
        // Run multiple times to statistically verify jitter stays within ±10%
        let initial = Duration::from_millis(1000);
        for _ in 0..100 {
            let delay = backoff_delay(0, initial, 1.0);
            assert!(
                delay.as_millis() >= 900,
                "jitter below -10%: {}ms",
                delay.as_millis()
            );
            assert!(
                delay.as_millis() <= 1100,
                "jitter above +10%: {}ms",
                delay.as_millis()
            );
        }
    }

    #[test]
    fn backoff_large_initial_clamps_to_max() {
        // Initial of 10 minutes should clamp to 5-minute max
        let delay = backoff_delay(0, Duration::from_secs(600), 2.0);
        assert!(delay.as_millis() <= 300_000);
        // Should be near the 300s cap (minus jitter)
        assert!(delay.as_millis() >= 270_000);
    }

    // ── backoff_with_config tests ──

    #[test]
    fn config_backoff_increases_with_attempts() {
        let config = BackoffConfig {
            initial: Duration::from_secs(1),
            factor: 2.0,
            max_delay: Duration::from_secs(60),
            jitter_fraction: 0.1,
        };
        let b0 = backoff_with_config(0, &config);
        let b2 = backoff_with_config(2, &config);
        assert!(b0.as_secs_f64() >= 1.0);
        assert!(b2.as_secs_f64() > b0.as_secs_f64());
    }

    #[test]
    fn config_backoff_capped_at_max() {
        let config = BackoffConfig {
            initial: Duration::from_secs(1),
            factor: 2.0,
            max_delay: Duration::from_secs(60),
            jitter_fraction: 0.1,
        };
        let b = backoff_with_config(100, &config);
        assert!(b.as_secs_f64() <= 60.0 * 1.1 + 0.01);
    }

    #[test]
    fn config_backoff_first_attempt_near_initial() {
        let config = BackoffConfig {
            initial: Duration::from_secs(1),
            factor: 2.0,
            max_delay: Duration::from_secs(60),
            jitter_fraction: 0.1,
        };
        let b = backoff_with_config(0, &config);
        assert!(b.as_secs_f64() >= 1.0);
        assert!(b.as_secs_f64() <= 1.1 + 0.01);
    }
}
