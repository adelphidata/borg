//! Tiny time helpers shared across crates.

/// Wall-clock seconds since the UNIX epoch. Saturates to 0 on a backward
/// system clock — callers use this for informational timestamps, not keys.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
