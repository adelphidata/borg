use std::time::Duration;

use rand::RngExt;

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
            // Caps the doubling at ~5 min: long enough that a wedged remote
            // dependency stops being hammered, short enough that recovery is
            // noticed within one heartbeat interval.
            max_delay: Duration::from_secs(300),
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

#[cfg(test)]
mod tests {
    use super::*;

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
