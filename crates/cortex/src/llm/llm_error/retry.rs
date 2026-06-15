//! Retry-after extraction: parses the HTTP `Retry-After` header, the structured
//! `error.retry_after` JSON field, Google's `RetryInfo` proto encoding, and
//! free-form "try again in N" hints into a clamped [`Duration`]. Every shape is
//! bounded to `[1s, 24h]` so a degenerate hint can't park a credential.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::LlmError;

/// Parse an HTTP `Retry-After` header value. RFC 7231 allows two forms:
/// a delta in seconds (e.g. `"300"`) or an HTTP-date. We accept the seconds
/// form (provider servers we talk to use it exclusively) and clamp to a
/// 24-hour ceiling so a degenerate `Retry-After: 99999999` can't park a
/// credential for a month.
pub fn parse_retry_after_header(value: &str) -> Option<Duration> {
    let secs: f64 = value.trim().parse().ok()?;
    if !secs.is_finite() || secs <= 0.0 {
        return None;
    }
    const MAX_RETRY: f64 = 24.0 * 60.0 * 60.0;
    Some(Duration::from_secs_f64(secs.min(MAX_RETRY)))
}

/// Replace the `retry_after` field on a `Retryable` error with the supplied
/// override. Used by the request path to plumb the HTTP `Retry-After` header
/// in after `classify_status` has already inspected the body. Header wins
/// when both are present (it's authoritative from the provider).
pub(crate) fn override_retry_after(err: LlmError, override_value: Option<Duration>) -> LlmError {
    let Some(ra) = override_value else {
        return err;
    };
    match err {
        LlmError::Retryable { source, reason, .. } => LlmError::Retryable {
            source,
            retry_after: Some(ra),
            reason,
        },
        other => other,
    }
}

/// Extract a `retry_after` duration from a JSON error response body. Applies
/// the same finite/positive/24h-clamp guards as `parse_retry_after_header` so
/// a degenerate `"retry_after": -1` or `999999999` doesn't park a credential
/// for absurd durations.
pub fn parse_retry_after(body: &str) -> Option<Duration> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let raw = &v["error"]["retry_after"];
    // Number form is the spec'd shape (OpenAI, OpenRouter native); string form
    // ("30", "1.5") is a fallback for upstreams that pass through a stringified
    // value via OpenRouter's `error.metadata.raw` wrapper. Each candidate is
    // validated finite/positive before short-circuiting so a degenerate
    // `retry_after: 0` doesn't mask a valid Google `retryDelay` fallback.
    let valid = |n: f64| -> Option<f64> {
        if n.is_finite() && n > 0.0 {
            Some(n)
        } else {
            None
        }
    };
    let secs = raw
        .as_f64()
        .and_then(valid)
        .or_else(|| {
            raw.as_str()
                .and_then(|s| s.trim().parse::<f64>().ok())
                .and_then(valid)
        })
        .or_else(|| parse_google_retry_delay(&v).and_then(valid))?;
    const MAX_RETRY: f64 = 24.0 * 60.0 * 60.0;
    Some(Duration::from_secs_f64(secs.min(MAX_RETRY)))
}

/// Walk `error.details[]` for the first `retryDelay` string in Google's
/// `RetryInfo` proto encoding (e.g. `"37s"`, `"1.5s"`). Returns seconds as
/// `f64` so the caller can apply the same clamp guards as the other shapes.
fn parse_google_retry_delay(v: &serde_json::Value) -> Option<f64> {
    let details = v.get("error")?.get("details")?.as_array()?;
    for d in details {
        let Some(raw) = d.get("retryDelay").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(stripped) = raw.strip_suffix('s') else {
            continue;
        };
        if let Ok(n) = stripped.parse::<f64>() {
            return Some(n);
        }
    }
    None
}

/// Fallback extraction for free-form provider retry hints when the structured
/// `retry_after` field is absent. Recognizes:
///
/// - JSON `reset_at` / `resets_at` / `retry_until` (top-level OR nested under
///   `error.metadata.raw` — OpenRouter wraps upstream errors there).
/// - Epoch seconds and epoch milliseconds (autodetected via magnitude).
/// - ISO-8601 timestamps.
/// - Free-form patterns in error messages: `quotaResetDelay: 2500ms` (Google),
///   `try again in 5 minutes`, `retry after 45 seconds`.
///
/// Clamps the result to `[1s, 24h]` — defends against absurd hints like
/// `retry_after: 999999999`.
pub fn parse_retry_hint_from_body(body: &str) -> Option<Duration> {
    if body.is_empty() {
        return None;
    }
    let now = SystemTime::now();
    let now_unix_ms = now
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)?;

    let try_value = |v: &serde_json::Value| -> Option<Duration> {
        // Try numeric (epoch s or ms).
        if let Some(n) = v.as_f64() {
            let ms = if n.abs() > 1_000_000_000_000.0 {
                n as i64
            } else {
                (n * 1000.0) as i64
            };
            let delta_ms = ms - now_unix_ms;
            if delta_ms > 0 {
                return Some(clamp_retry(Duration::from_millis(delta_ms as u64)));
            }
            return None;
        }
        // Try string (ISO-8601 or numeric-as-string).
        if let Some(s) = v.as_str() {
            if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(s) {
                let ts_ms = ts.timestamp_millis();
                let delta_ms = ts_ms - now_unix_ms;
                if delta_ms > 0 {
                    return Some(clamp_retry(Duration::from_millis(delta_ms as u64)));
                }
            }
            if let Ok(n) = s.parse::<f64>() {
                let ms = if n.abs() > 1_000_000_000_000.0 {
                    n as i64
                } else {
                    (n * 1000.0) as i64
                };
                let delta_ms = ms - now_unix_ms;
                if delta_ms > 0 {
                    return Some(clamp_retry(Duration::from_millis(delta_ms as u64)));
                }
            }
        }
        None
    };

    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);

    for key in ["reset_at", "resets_at", "retry_until"] {
        if let Some(d) = try_value(&parsed[key]) {
            return Some(d);
        }
        // Nested at error.metadata.raw (OpenRouter wraps upstream errors).
        if let Some(d) = try_value(&parsed["error"]["metadata"]["raw"][key]) {
            return Some(d);
        }
        // Sometimes the upstream body is a JSON-encoded string inside `raw`.
        if let Some(s) = parsed["error"]["metadata"]["raw"].as_str() {
            if let Ok(inner) = serde_json::from_str::<serde_json::Value>(s) {
                if let Some(d) = try_value(&inner[key]) {
                    return Some(d);
                }
                if let Some(d) = try_value(&inner["error"][key]) {
                    return Some(d);
                }
            }
        }
    }

    // Free-form patterns in `error.message` or root.
    let msg_haystack = [
        parsed["error"]["message"].as_str().unwrap_or(""),
        parsed["message"].as_str().unwrap_or(""),
        body,
    ]
    .join(" ");
    let lower = msg_haystack.to_ascii_lowercase();

    // Google quotaResetDelay
    if let Some(d) = parse_unit_value(&lower, &["quotaresetdelay"]) {
        return Some(clamp_retry(d));
    }
    // "try again in N units" / "retry after N units"
    if let Some(d) = parse_unit_value(&lower, &["try again in", "retry after", "retry in"]) {
        return Some(clamp_retry(d));
    }
    None
}

fn clamp_retry(d: Duration) -> Duration {
    let secs = d.as_secs().clamp(1, 24 * 60 * 60);
    Duration::from_secs(secs)
}

/// Scan `text` for any of `markers` and parse a duration following the marker.
/// Accepts `ms`, `s`/`sec`/`seconds`, `m`/`min`/`minutes`, `h`/`hours`.
fn parse_unit_value(text: &str, markers: &[&str]) -> Option<Duration> {
    for marker in markers {
        let Some(start) = text.find(marker) else {
            continue;
        };
        let rest = &text[start + marker.len()..];
        // Skip non-digit chars (colons, spaces, quotes).
        let digits_start = rest.find(|c: char| c.is_ascii_digit())?;
        let after = &rest[digits_start..];
        let mut num = String::new();
        for ch in after.chars() {
            if ch.is_ascii_digit() || ch == '.' {
                num.push(ch);
            } else {
                break;
            }
        }
        if num.is_empty() {
            continue;
        }
        let n: f64 = num.parse().ok()?;
        let tail = &after[num.len()..].trim_start();
        let mult_ms: f64 = if tail.starts_with("ms") {
            1.0
        } else if tail.starts_with("hour") || tail.starts_with('h') {
            3_600_000.0
        } else if tail.starts_with("min") || tail.starts_with('m') {
            60_000.0
        } else {
            // Default to seconds for "sec"/"s"/missing unit and any
            // unrecognized unit suffix — permissive fallback so we never reject
            // a `Retry-After` we could plausibly interpret.
            1000.0
        };
        return Some(Duration::from_millis((n * mult_ms) as u64));
    }
    None
}
