//! Teams API error classification for structured retry and logging.
//!
//! Categorizes HTTP status codes into actionable error kinds.

/// Classified Teams API error kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeamsErrorKind {
    /// 401/403 — credentials invalid, expired, or insufficient permissions.
    Auth,
    /// 429 — rate limited. `retry_after_secs` from the Retry-After header.
    Throttled { retry_after_secs: u64 },
    /// 408, 5xx — temporary failures worth retrying with backoff.
    Transient,
    /// 4xx (other than auth/throttle) — request is malformed or resource not found.
    Permanent,
    /// Anything else (network errors, unexpected codes).
    Unknown,
}

/// Maximum Retry-After value we'll honor (5 minutes).
const MAX_RETRY_AFTER_SECS: u64 = 300;
/// Default Retry-After if the header is missing or unparseable.
const DEFAULT_RETRY_AFTER_SECS: u64 = 5;

/// Classify an HTTP status code into a `TeamsErrorKind`.
///
/// `retry_after` is the raw value of the `Retry-After` header, if present.
pub fn classify_status(status: u16, retry_after: Option<&str>) -> TeamsErrorKind {
    match status {
        401 | 403 => TeamsErrorKind::Auth,
        429 => {
            let secs = extract_retry_after(retry_after, DEFAULT_RETRY_AFTER_SECS);
            TeamsErrorKind::Throttled {
                retry_after_secs: secs,
            }
        }
        408 | 500..=599 => TeamsErrorKind::Transient,
        400..=499 => TeamsErrorKind::Permanent,
        _ => TeamsErrorKind::Unknown,
    }
}

/// Human-readable hint for a `TeamsErrorKind`, useful in log messages.
pub fn error_hint(kind: &TeamsErrorKind) -> &'static str {
    match kind {
        TeamsErrorKind::Auth => {
            "Token may be expired or permissions insufficient; refresh and retry"
        }
        TeamsErrorKind::Throttled { retry_after_secs } => {
            // Can't interpolate in a static str, so provide a generic hint
            let _ = retry_after_secs;
            "Rate limited by Teams; wait before retrying"
        }
        TeamsErrorKind::Transient => "Temporary failure; retry with exponential backoff",
        TeamsErrorKind::Permanent => "Request is malformed or resource not found; do not retry",
        TeamsErrorKind::Unknown => "Unexpected error; check logs for details",
    }
}

/// Parse a `Retry-After` header value as seconds.
///
/// Falls back to `default_secs` if the header is missing or unparseable.
/// Caps at 300 seconds to avoid excessively long waits.
pub fn extract_retry_after(header_value: Option<&str>, default_secs: u64) -> u64 {
    match header_value {
        Some(val) => val
            .trim()
            .parse::<u64>()
            .unwrap_or(default_secs)
            .min(MAX_RETRY_AFTER_SECS),
        None => default_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Full HTTP status → `TeamsErrorKind` matrix. Boundary codes for
    /// each range are exercised so an off-by-one in `classify_status`
    /// (e.g. shifting the 5xx range, accidentally swallowing 429 into the
    /// 4xx Permanent bucket) fails a specific row.
    #[test]
    fn classify_status_matrix() {
        let cases: &[(u16, TeamsErrorKind)] = &[
            (401, TeamsErrorKind::Auth),
            (403, TeamsErrorKind::Auth),
            (408, TeamsErrorKind::Transient),
            (500, TeamsErrorKind::Transient),
            (502, TeamsErrorKind::Transient),
            (503, TeamsErrorKind::Transient),
            (504, TeamsErrorKind::Transient),
            (400, TeamsErrorKind::Permanent),
            (404, TeamsErrorKind::Permanent),
            // 2xx and zero must NOT classify as a known retryable bucket.
            (200, TeamsErrorKind::Unknown),
            (0, TeamsErrorKind::Unknown),
        ];
        for (status, expected) in cases {
            assert_eq!(&classify_status(*status, None), expected, "status={status}");
        }
    }

    /// 429 carries a `Retry-After` payload that drives backoff. The
    /// header MUST be parsed when valid, capped at `MAX_RETRY_AFTER_SECS`
    /// to prevent multi-hour stalls, and fall back to the default when
    /// missing or unparseable.
    #[test]
    fn classify_429_extracts_capped_retry_after_with_default_fallback() {
        let cases: &[(Option<&str>, u64)] = &[
            (Some("30"), 30),
            (None, DEFAULT_RETRY_AFTER_SECS),
            (Some("999"), MAX_RETRY_AFTER_SECS),
            (Some("not-a-number"), DEFAULT_RETRY_AFTER_SECS),
        ];
        for (header, expected_secs) in cases {
            assert_eq!(
                classify_status(429, *header),
                TeamsErrorKind::Throttled {
                    retry_after_secs: *expected_secs,
                },
                "Retry-After={header:?}"
            );
        }
    }

    /// `extract_retry_after` is the lower-level helper that owns
    /// trimming, capping, default fallback, and the zero-is-valid axis
    /// (zero means "retry immediately" per RFC 7231 §7.1.3, not an error).
    #[test]
    fn extract_retry_after_handles_trim_cap_default_and_zero() {
        let cases: &[(Option<&str>, u64)] = &[
            (Some("30"), 30),
            (None, 5),
            (Some("abc"), 5),
            (Some("600"), MAX_RETRY_AFTER_SECS),
            (Some("  15  "), 15),
            (Some("0"), 0),
        ];
        for (header, expected) in cases {
            assert_eq!(
                extract_retry_after(*header, 5),
                *expected,
                "header={header:?}"
            );
        }
    }

    /// Every kind must produce a non-empty, kind-distinguishing hint —
    /// the operator-facing log message would be useless otherwise. Also
    /// guards against accidentally returning the same generic hint for
    /// all variants (a regression that would erase retry semantics from
    /// logs).
    #[test]
    fn error_hint_is_non_empty_and_unique_per_kind() {
        let variants = [
            TeamsErrorKind::Auth,
            TeamsErrorKind::Throttled {
                retry_after_secs: 5,
            },
            TeamsErrorKind::Transient,
            TeamsErrorKind::Permanent,
            TeamsErrorKind::Unknown,
        ];
        let hints: Vec<&'static str> = variants.iter().map(error_hint).collect();
        for (kind, hint) in variants.iter().zip(&hints) {
            assert!(!hint.is_empty(), "hint for {kind:?} is empty");
        }
        let unique: std::collections::HashSet<_> = hints.iter().copied().collect();
        assert_eq!(
            unique.len(),
            hints.len(),
            "every TeamsErrorKind must map to a distinct hint, got duplicates in {hints:?}"
        );
    }
}
