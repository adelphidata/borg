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
    fn gateway_backoff_is_one_indexed_and_bounded() {
        // The gateway-specific concern in this wrapper is the 1-indexed
        // `consecutive_errors` → 0-indexed attempt conversion (via
        // `saturating_sub(1)`). If that regresses, the first error would
        // immediately schedule a delay for attempt=1 instead of attempt=0,
        // skipping the MIN-bucket entirely.
        let first = calculate_backoff(1, MIN, MAX, FACTOR, JITTER);
        assert!(
            first.as_secs_f64() >= MIN.as_secs_f64() - 0.01
                && first.as_secs_f64() <= MIN.as_secs_f64() * (1.0 + JITTER) + 0.01,
            "first attempt must land in MIN±jitter, got {first:?}"
        );

        let later = calculate_backoff(3, MIN, MAX, FACTOR, JITTER);
        assert!(
            later >= first,
            "attempt 3 must be at least as long as attempt 1 ({later:?} vs {first:?})"
        );

        let saturated = calculate_backoff(100, MIN, MAX, FACTOR, JITTER);
        assert!(
            saturated.as_secs_f64() <= MAX.as_secs_f64() * (1.0 + JITTER),
            "saturation must cap at MAX±jitter, got {saturated:?}"
        );
    }
}
