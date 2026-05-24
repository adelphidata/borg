//! Tiny time helpers shared across crates.

/// Wall-clock seconds since the UNIX epoch. Saturates to 0 on a backward
/// system clock — callers use this for informational timestamps, not keys.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_unix_is_after_2024_and_before_2100() {
        // Real-world failure: a swap to `as_millis()` or `as_nanos()` would
        // overflow / return a wildly wrong magnitude. Anchor to a known
        // sane window that holds for every build between 2024 and 2100.
        let t = now_unix();
        assert!(t > 1_704_067_200, "expected > 2024-01-01, got {t}");
        assert!(t < 4_102_444_800, "expected < 2100-01-01, got {t}");
    }

    #[test]
    fn now_unix_is_monotonic_within_same_call_window() {
        // Two successive calls must produce non-decreasing values. Catches
        // any future regression that derives the timestamp from a per-call
        // random or wraps with `%`.
        let a = now_unix();
        let b = now_unix();
        assert!(b >= a, "now_unix went backward: {a} -> {b}");
    }
}
