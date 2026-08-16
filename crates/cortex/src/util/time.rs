//! Tiny time helpers shared across crates.

/// Wall-clock seconds since the UNIX epoch. Saturates to 0 on a backward
/// system clock — callers use this for informational timestamps, not keys.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The host's IANA zone, resolved once. `iana_time_zone` reads the OS setting
/// from disk, and prompts carrying this line are rebuilt every turn.
fn host_timezone() -> Option<&'static str> {
    static HOST_TZ: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    HOST_TZ
        .get_or_init(crate::config::Config::detect_host_timezone)
        .as_deref()
}

/// Current wall-clock time with its zone name, abbreviation, and UTC offset —
/// e.g. `2026-08-16 14:03:22 EDT (America/New_York, UTC-04:00)`.
///
/// A bare local timestamp is ambiguous to a model in exactly the cases that
/// matter: near a DST boundary it cannot tell EST from EDT, and tools that take
/// an instant (calendars, schedulers, MCP servers) either reject a naive
/// datetime or silently record it an hour out. `chrono::Local` renders `%Z` as
/// a numeric offset with no zone name at all, so the IANA identifier has to
/// come from the configured or detected zone.
///
/// `configured` is the operator's `user.timezone`; the host zone fills in when
/// it is unset, and a plain local offset when neither resolves — still
/// unambiguous, just without a name for the zone.
///
/// Second-resolution and stable within a second, so it does not by itself
/// invalidate a cached prompt prefix — keep it in a dynamic prompt suffix.
pub fn current_time_line(configured: Option<&str>) -> String {
    let zone = match configured {
        Some(name) => Some(name),
        None => host_timezone(),
    };
    let named = zone.and_then(|name| name.parse::<chrono_tz::Tz>().ok());
    match named {
        Some(tz) => {
            let now = chrono::Utc::now().with_timezone(&tz);
            format!(
                "{} ({}, UTC{})",
                now.format("%Y-%m-%d %H:%M:%S %Z"),
                tz.name(),
                now.format("%:z")
            )
        }
        None => {
            let now = chrono::Local::now();
            format!(
                "{} (UTC{})",
                now.format("%Y-%m-%d %H:%M:%S"),
                now.format("%:z")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With a zone configured, the line must carry all three parts a consumer
    /// needs: the wall-clock reading, the IANA identifier, and the numeric UTC
    /// offset. `chrono::Local` cannot supply the identifier at all, which is
    /// why the zone is resolved explicitly.
    #[test]
    fn a_configured_zone_yields_name_abbreviation_and_offset() {
        let line = current_time_line(Some("America/New_York"));
        assert!(
            line.contains("America/New_York"),
            "must name the IANA zone: {line}"
        );
        assert!(
            line.contains("UTC-05:00") || line.contains("UTC-04:00"),
            "must carry the numeric offset for either side of DST: {line}"
        );
        assert!(
            line.contains("EST") || line.contains("EDT"),
            "must carry the zone abbreviation: {line}"
        );
        // A fixed zone renders its own wall clock, not the host's.
        let tokyo = current_time_line(Some("Asia/Tokyo"));
        assert!(
            tokyo.contains("Asia/Tokyo") && tokyo.contains("UTC+09:00"),
            "{tokyo}"
        );
    }

    /// An unparseable configured zone must not lose the offset — falling back
    /// to a bare local timestamp would reintroduce the ambiguity this line
    /// exists to remove.
    #[test]
    fn an_unusable_zone_still_carries_an_offset() {
        let line = current_time_line(Some("Not/AZone"));
        assert!(
            line.contains("UTC"),
            "offset must survive the fallback: {line}"
        );
    }

    /// The line stays second-resolution and self-consistent: the date must
    /// parse back with the offset it advertises.
    #[test]
    fn the_rendered_line_parses_back_as_a_real_instant() {
        let line = current_time_line(Some("Europe/Berlin"));
        let stamp = line.split(" (").next().expect("timestamp half");
        let offset = line
            .rsplit("UTC")
            .next()
            .and_then(|s| s.strip_suffix(')'))
            .expect("offset in the parenthetical");
        let datetime = stamp.rsplitn(2, ' ').last().expect("date and time");
        let parsed = chrono::DateTime::parse_from_str(
            &format!("{datetime} {offset}"),
            "%Y-%m-%d %H:%M:%S %:z",
        );
        assert!(parsed.is_ok(), "line must round-trip: {line} ({parsed:?})");
    }

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
