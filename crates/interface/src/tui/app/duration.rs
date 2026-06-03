//! Compact duration formatting for the bottom-right goal indicator.
//!
//! Renders an elapsed-seconds count as the largest non-zero unit only —
//! `"45s"`, `"12m"`, `"3h"`, `"2d"` — so the indicator stays narrow enough
//! to fit even on slim terminals.

/// Format `seconds` as a compact duration. Negative inputs and overflowing
/// systems clamp to `"0s"`.
pub fn fmt_compact_duration(seconds: i64) -> String {
    let s = seconds.max(0);
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    if m < 60 {
        return format!("{m}m");
    }
    let h = m / 60;
    if h < 24 {
        return format!("{h}h");
    }
    let d = h / 24;
    format!("{d}d")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_compact_duration_matches_table() {
        let cases: &[(i64, &str)] = &[
            (-5, "0s"),
            (0, "0s"),
            (1, "1s"),
            (45, "45s"),
            (59, "59s"),
            (60, "1m"),
            (90, "1m"),
            (3599, "59m"),
            (3600, "1h"),
            (5400, "1h"),
            (86_399, "23h"),
            (86_400, "1d"),
            (172_800, "2d"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                fmt_compact_duration(*input),
                *expected,
                "input={input} expected={expected}"
            );
        }
    }
}
