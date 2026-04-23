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

    #[test]
    fn classify_401_as_auth() {
        assert_eq!(classify_status(401, None), TeamsErrorKind::Auth);
    }

    #[test]
    fn classify_403_as_auth() {
        assert_eq!(classify_status(403, None), TeamsErrorKind::Auth);
    }

    #[test]
    fn classify_429_with_retry_after() {
        assert_eq!(
            classify_status(429, Some("30")),
            TeamsErrorKind::Throttled {
                retry_after_secs: 30
            }
        );
    }

    #[test]
    fn classify_429_without_retry_after() {
        assert_eq!(
            classify_status(429, None),
            TeamsErrorKind::Throttled {
                retry_after_secs: DEFAULT_RETRY_AFTER_SECS
            }
        );
    }

    #[test]
    fn classify_429_caps_retry_after() {
        assert_eq!(
            classify_status(429, Some("999")),
            TeamsErrorKind::Throttled {
                retry_after_secs: MAX_RETRY_AFTER_SECS
            }
        );
    }

    #[test]
    fn classify_429_invalid_header_uses_default() {
        assert_eq!(
            classify_status(429, Some("not-a-number")),
            TeamsErrorKind::Throttled {
                retry_after_secs: DEFAULT_RETRY_AFTER_SECS
            }
        );
    }

    #[test]
    fn classify_408_as_transient() {
        assert_eq!(classify_status(408, None), TeamsErrorKind::Transient);
    }

    #[test]
    fn classify_500_as_transient() {
        assert_eq!(classify_status(500, None), TeamsErrorKind::Transient);
    }

    #[test]
    fn classify_502_as_transient() {
        assert_eq!(classify_status(502, None), TeamsErrorKind::Transient);
    }

    #[test]
    fn classify_503_as_transient() {
        assert_eq!(classify_status(503, None), TeamsErrorKind::Transient);
    }

    #[test]
    fn classify_504_as_transient() {
        assert_eq!(classify_status(504, None), TeamsErrorKind::Transient);
    }

    #[test]
    fn classify_400_as_permanent() {
        assert_eq!(classify_status(400, None), TeamsErrorKind::Permanent);
    }

    #[test]
    fn classify_404_as_permanent() {
        assert_eq!(classify_status(404, None), TeamsErrorKind::Permanent);
    }

    #[test]
    fn classify_200_as_unknown() {
        assert_eq!(classify_status(200, None), TeamsErrorKind::Unknown);
    }

    #[test]
    fn classify_0_as_unknown() {
        assert_eq!(classify_status(0, None), TeamsErrorKind::Unknown);
    }

    #[test]
    fn error_hint_non_empty_for_all_variants() {
        let variants = [
            TeamsErrorKind::Auth,
            TeamsErrorKind::Throttled {
                retry_after_secs: 5,
            },
            TeamsErrorKind::Transient,
            TeamsErrorKind::Permanent,
            TeamsErrorKind::Unknown,
        ];
        for kind in &variants {
            let hint = error_hint(kind);
            assert!(!hint.is_empty(), "hint for {kind:?} should not be empty");
        }
    }

    #[test]
    fn extract_retry_after_valid() {
        assert_eq!(extract_retry_after(Some("30"), 5), 30);
    }

    #[test]
    fn extract_retry_after_missing() {
        assert_eq!(extract_retry_after(None, 5), 5);
    }

    #[test]
    fn extract_retry_after_invalid() {
        assert_eq!(extract_retry_after(Some("abc"), 5), 5);
    }

    #[test]
    fn extract_retry_after_capped() {
        assert_eq!(extract_retry_after(Some("600"), 5), MAX_RETRY_AFTER_SECS);
    }

    #[test]
    fn extract_retry_after_whitespace() {
        assert_eq!(extract_retry_after(Some("  15  "), 5), 15);
    }

    #[test]
    fn extract_retry_after_zero() {
        assert_eq!(extract_retry_after(Some("0"), 5), 0);
    }
}
