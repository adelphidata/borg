use std::time::Duration;

use borg_core::retry::{backoff_with_config, BackoffConfig};

/// Calculate exponential backoff with jitter.
///
/// Thin wrapper around `borg_core::retry::backoff_with_config`.
/// `consecutive_errors` is 1-indexed (first error = 1).
pub fn calculate_backoff(
    consecutive_errors: u32,
    min_backoff: Duration,
    max_backoff: Duration,
    backoff_factor: f64,
    jitter_fraction: f64,
) -> Duration {
    let config = BackoffConfig {
        initial: min_backoff,
        factor: backoff_factor,
        max_delay: max_backoff,
        jitter_fraction,
    };
    // Gateway convention: errors are 1-indexed, so subtract 1 for 0-indexed attempt.
    backoff_with_config(consecutive_errors.saturating_sub(1), &config)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(1);
    const MAX: Duration = Duration::from_secs(60);
    const FACTOR: f64 = 2.0;
    const JITTER: f64 = 0.1;

    #[test]
    fn backoff_increases_with_errors() {
        let b1 = calculate_backoff(1, MIN, MAX, FACTOR, JITTER);
        let b3 = calculate_backoff(3, MIN, MAX, FACTOR, JITTER);
        assert!(b1.as_secs_f64() >= MIN.as_secs_f64());
        assert!(b3.as_secs_f64() <= MAX.as_secs_f64() * (1.0 + JITTER));
    }

    #[test]
    fn backoff_capped_at_max() {
        let b = calculate_backoff(100, MIN, MAX, FACTOR, JITTER);
        assert!(b.as_secs_f64() <= MAX.as_secs_f64() * (1.0 + JITTER));
    }

    #[test]
    fn backoff_first_attempt() {
        let b = calculate_backoff(1, MIN, MAX, FACTOR, JITTER);
        assert!(b.as_secs_f64() >= MIN.as_secs_f64());
        assert!(b.as_secs_f64() <= MIN.as_secs_f64() * (1.0 + JITTER) + 0.01);
    }
}
