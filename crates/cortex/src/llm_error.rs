use std::fmt;
use std::time::Duration;

use crate::provider::Provider;
use crate::secrets::redact_secrets;

/// Maximum bytes of provider response body retained in error messages.
/// Bodies are redacted first, then truncated — keeps logs bounded and avoids
/// echoing large quota/billing payloads or accidental secret material.
const ERROR_BODY_MAX_LEN: usize = 512;

/// Sanitize a provider error response body before it is embedded in an
/// `anyhow!` chain (which subsequently flows to `tracing::warn!`). Runs
/// `redact_secrets` (catches sk-…, ghp_…, AKIA…, JWTs, KEY=value, DB URIs,
/// private keys) and truncates to `ERROR_BODY_MAX_LEN` chars.
fn sanitize_body(body: &str) -> String {
    let redacted = redact_secrets(body);
    if redacted.len() <= ERROR_BODY_MAX_LEN {
        redacted
    } else {
        let mut end = ERROR_BODY_MAX_LEN;
        while !redacted.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…[truncated]", &redacted[..end])
    }
}

// ── Error classification ──

/// Why a provider failed — drives cooldown duration, key rotation, failover,
/// and reactive recovery decisions (compaction, payload shrink).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailoverReason {
    /// Authentication failure that may be a bad key — rotating to another key
    /// in the pool could recover (401).
    Auth,
    /// Authorization denied in a way rotating keys won't fix (403) — account
    /// disabled, org-level policy, model gated behind entitlement.
    AuthPermanent,
    /// Billing or quota exhaustion (402).
    Billing,
    /// Too many requests (429).
    RateLimit,
    /// Server overloaded or unavailable (500/502/503/504).
    Overloaded,
    /// Network timeout or connection failure.
    Timeout,
    /// Malformed request with no useful recovery path (400 without
    /// context/payload keywords).
    Format,
    /// Request body exceeds the model's context window — compaction before
    /// retry is the recovery path.
    ContextOverflow,
    /// Request body exceeds the transport payload cap (413).
    PayloadTooLarge,
    /// The requested model id isn't available on this provider (404 with
    /// "model" in the body) — fast-fail, don't retry.
    ModelNotFound,
    /// Unclassified error.
    Unknown,
}

impl fmt::Display for FailoverReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auth => write!(f, "auth"),
            Self::AuthPermanent => write!(f, "auth_permanent"),
            Self::Billing => write!(f, "billing"),
            Self::RateLimit => write!(f, "rate_limit"),
            Self::Overloaded => write!(f, "overloaded"),
            Self::Timeout => write!(f, "timeout"),
            Self::Format => write!(f, "format"),
            Self::ContextOverflow => write!(f, "context_overflow"),
            Self::PayloadTooLarge => write!(f, "payload_too_large"),
            Self::ModelNotFound => write!(f, "model_not_found"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

/// An error from an LLM provider request.
#[derive(Debug)]
pub enum LlmError {
    /// Transient error that may succeed on retry.
    Retryable {
        /// The underlying error.
        source: anyhow::Error,
        /// Optional server-suggested retry delay.
        retry_after: Option<Duration>,
        /// Classification of the failure.
        reason: FailoverReason,
    },
    /// Permanent error that will not succeed on retry.
    Fatal {
        /// The underlying error.
        source: anyhow::Error,
        /// Classification of the failure.
        reason: FailoverReason,
    },
    /// Request was cancelled via cancellation token.
    Interrupted,
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Retryable { source, .. } => write!(f, "{source}"),
            Self::Fatal { source, .. } => write!(f, "{source}"),
            Self::Interrupted => write!(f, "request interrupted"),
        }
    }
}

impl std::error::Error for LlmError {}

impl LlmError {
    /// Returns `true` if this error is transient and may succeed on retry.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Retryable { .. })
    }

    /// Returns the failure classification for this error.
    pub fn reason(&self) -> FailoverReason {
        match self {
            Self::Retryable { reason, .. } | Self::Fatal { reason, .. } => *reason,
            Self::Interrupted => FailoverReason::Unknown,
        }
    }

    /// True when the agent layer should compact conversation history and
    /// retry the turn before propagating this error. Currently only set for
    /// `ContextOverflow` — payload caps are a different recovery path.
    pub fn should_compress(&self) -> bool {
        matches!(self.reason(), FailoverReason::ContextOverflow)
    }

    /// True when rotating to another credential in the pool may recover.
    /// `AuthPermanent` deliberately returns false — rotating won't help when
    /// the account itself is denied.
    pub fn should_rotate_credential(&self) -> bool {
        matches!(
            self.reason(),
            FailoverReason::Auth | FailoverReason::RateLimit
        )
    }

    /// True when, after retries and rotation fail, the agent should try the
    /// next provider in the fallback chain. `ModelNotFound` and
    /// `AuthPermanent` skip fallback — they're user-fixable, not transient.
    pub fn should_fallback(&self) -> bool {
        !matches!(
            self.reason(),
            FailoverReason::ModelNotFound | FailoverReason::AuthPermanent
        )
    }

    /// True when the agent should surface the error immediately without
    /// retrying or compacting — the request is structurally broken and
    /// retrying would just burn tokens.
    pub fn is_fast_fail(&self) -> bool {
        matches!(
            self.reason(),
            FailoverReason::ModelNotFound
                | FailoverReason::AuthPermanent
                | FailoverReason::PayloadTooLarge
        )
    }
}

pub(crate) fn classify_status(
    status: reqwest::StatusCode,
    body: &str,
    provider: Provider,
) -> LlmError {
    // Prefer the structured `retry_after` field; fall back to free-form body hints
    // (`quotaResetDelay`, `resets_at`, "try again in N seconds", …).
    let retry_after = parse_retry_after(body).or_else(|| parse_retry_hint_from_body(body));
    // Classifier predicates run on the raw body (their needles are intentionally
    // benign), but anything embedded in the user-visible error chain is sanitized
    // first — provider error bodies have leaked auth/billing context historically.
    let safe = sanitize_body(body);

    match status.as_u16() {
        429 => LlmError::Retryable {
            source: anyhow::anyhow!("{provider} returned 429 (rate limited): {safe}"),
            retry_after,
            reason: FailoverReason::RateLimit,
        },
        500 | 502 | 504 => LlmError::Retryable {
            source: anyhow::anyhow!("{provider} returned {status}: {safe}"),
            retry_after: None,
            reason: FailoverReason::Overloaded,
        },
        503 => LlmError::Retryable {
            source: anyhow::anyhow!("{provider} returned {status} (overloaded): {safe}"),
            retry_after: None,
            reason: FailoverReason::Overloaded,
        },
        401 => LlmError::Fatal {
            source: anyhow::anyhow!("{provider} returned 401 (auth error): {safe}"),
            reason: FailoverReason::Auth,
        },
        403 => LlmError::Fatal {
            source: anyhow::anyhow!("{provider} returned 403 (auth permanent): {safe}"),
            reason: FailoverReason::AuthPermanent,
        },
        402 => {
            // 402 is overloaded — some providers return it for transient
            // usage-limit windows that recover on their own, others for a
            // hard credit-exhaustion. Both rotate, but the messaging differs.
            let reason = classify_402(body);
            let label = match reason {
                FailoverReason::RateLimit => "usage limit",
                _ => "billing error",
            };
            // Retry-after may be set when the 402 is a transient usage-limit.
            if matches!(reason, FailoverReason::RateLimit) {
                LlmError::Retryable {
                    source: anyhow::anyhow!("{provider} returned {status} ({label}): {safe}"),
                    retry_after,
                    reason,
                }
            } else {
                LlmError::Fatal {
                    source: anyhow::anyhow!("{provider} returned {status} ({label}): {safe}"),
                    reason,
                }
            }
        }
        404 if body_indicates_no_allowed_providers(body) => LlmError::Fatal {
            source: anyhow::anyhow!(
                "{provider} returned 404 (no allowed providers for model — check account provider preferences at https://openrouter.ai/settings/preferences): {safe}"
            ),
            reason: FailoverReason::AuthPermanent,
        },
        404 if body_mentions_model(body) => LlmError::Fatal {
            source: anyhow::anyhow!("{provider} returned 404 (model not found): {safe}"),
            reason: FailoverReason::ModelNotFound,
        },
        413 => LlmError::Fatal {
            source: anyhow::anyhow!("{provider} returned 413 (payload too large): {safe}"),
            reason: FailoverReason::PayloadTooLarge,
        },
        400 if body_indicates_context_overflow(body) => LlmError::Fatal {
            source: anyhow::anyhow!("{provider} returned 400 (context overflow): {safe}"),
            reason: FailoverReason::ContextOverflow,
        },
        400 => LlmError::Fatal {
            source: anyhow::anyhow!("{provider} returned {status} (bad request): {safe}"),
            reason: FailoverReason::Format,
        },
        _ => LlmError::Fatal {
            source: anyhow::anyhow!("{provider} returned {status}: {safe}"),
            reason: FailoverReason::Unknown,
        },
    }
}

/// Detect provider error bodies that signal the request exceeded the model's
/// context window. Providers return 400 with varied phrasing; keep this
/// conservative — a false positive would trigger an unnecessary compaction,
/// but a false negative just degrades to the previous behavior (fatal error).
fn body_indicates_context_overflow(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    // Common phrases across OpenAI, Anthropic, Gemini, OpenRouter, Groq.
    const NEEDLES: &[&str] = &[
        "context length",
        "context window",
        "maximum context",
        "too many tokens",
        "prompt is too long",
        "input is too long",
        "exceeds the maximum",
        "exceeds context",
        "token limit",
    ];
    NEEDLES.iter().any(|n| lower.contains(n))
}

/// True when a 404 body mentions the model, suggesting the model id itself is
/// the problem rather than a generic routing 404.
fn body_mentions_model(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("model")
}

/// True when an OpenRouter 404 body indicates the account's provider filters
/// blocked every upstream that serves the model — distinct from "model not
/// found" even though both return 404. Example body:
///   {"error":{"message":"No allowed providers are available for the selected
///   model.","code":404,"metadata":{"available_providers":[...]}}}
fn body_indicates_no_allowed_providers(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("no allowed providers") || lower.contains("available_providers")
}

/// Recover a `FailoverReason` from an error message string.
///
/// Used by the agent layer to make recovery decisions (e.g. reactive
/// compaction) when the only available signal is the `StreamEvent::Error`
/// payload — the typed error was already serialized by the time it crossed
/// the stream channel.
///
/// Matches on both `classify_status`-produced markers (e.g. "context overflow")
/// and raw provider phrases, for cases where the error came from outside the
/// classifier (transport errors, upstream adapters).
pub fn classify_error_text(msg: &str) -> Option<FailoverReason> {
    let lower = msg.to_ascii_lowercase();

    // Classifier-produced markers first — cheap exact substrings.
    if lower.contains("(context overflow)") {
        return Some(FailoverReason::ContextOverflow);
    }
    if lower.contains("(model not found)") {
        return Some(FailoverReason::ModelNotFound);
    }
    if lower.contains("(payload too large)") {
        return Some(FailoverReason::PayloadTooLarge);
    }
    if lower.contains("(auth permanent)") {
        return Some(FailoverReason::AuthPermanent);
    }

    // Fallback: raw provider phrases (for errors that bypassed classify_status,
    // e.g. Claude CLI adapter). Kept in sync with body_indicates_context_overflow.
    const OVERFLOW_NEEDLES: &[&str] = &[
        "context length",
        "context window",
        "maximum context",
        "too many tokens",
        "prompt is too long",
        "input is too long",
        "exceeds the maximum",
        "exceeds context",
        "token limit",
    ];
    if OVERFLOW_NEEDLES.iter().any(|n| lower.contains(n)) {
        return Some(FailoverReason::ContextOverflow);
    }

    None
}

pub(crate) fn classify_network_error(err: anyhow::Error) -> LlmError {
    LlmError::Retryable {
        source: err,
        retry_after: None,
        reason: FailoverReason::Timeout,
    }
}

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
    let secs = v["error"]["retry_after"].as_f64()?;
    if !secs.is_finite() || secs <= 0.0 {
        return None;
    }
    const MAX_RETRY: f64 = 24.0 * 60.0 * 60.0;
    Some(Duration::from_secs_f64(secs.min(MAX_RETRY)))
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
            // Default to seconds for "sec"/"s"/missing unit and any unrecognized
            // unit suffix — matches Hermes' parser's permissive fallback.
            1000.0
        };
        return Some(Duration::from_millis((n * mult_ms) as u64));
    }
    None
}

/// Disambiguate a 402 response between transient usage-limit windows
/// (returns to service) and hard credit exhaustion (operator must act).
/// Transient-style 402s are classified as `RateLimit` so the credential pool
/// cools them; permanent-style 402s are classified as `Billing`.
///
/// Heuristic mirrors Hermes: usage/quota markers AND a transient signal
/// (try-again / window / reset) → RateLimit; otherwise Billing.
pub fn classify_402(error_msg: &str) -> FailoverReason {
    let lower = error_msg.to_ascii_lowercase();
    const USAGE_LIMIT_PATTERNS: &[&str] = &[
        "usage limit",
        "quota",
        "limit exceeded",
        "key limit exceeded",
        "rate limit",
    ];
    const TRANSIENT_SIGNALS: &[&str] = &[
        "try again",
        "retry",
        "resets at",
        "reset in",
        "reset_at",
        "resets_at",
        "wait",
        "requests remaining",
        "periodic",
        "window",
    ];
    let has_usage_limit = USAGE_LIMIT_PATTERNS.iter().any(|p| lower.contains(p));
    let has_transient = TRANSIENT_SIGNALS.iter().any(|p| lower.contains(p));
    if has_usage_limit && has_transient {
        FailoverReason::RateLimit
    } else {
        FailoverReason::Billing
    }
}

use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
mod tests {
    use super::*;

    // ── FailoverReason Display ──

    #[test]
    fn failover_reason_display() {
        assert_eq!(FailoverReason::Auth.to_string(), "auth");
        assert_eq!(FailoverReason::Billing.to_string(), "billing");
        assert_eq!(FailoverReason::RateLimit.to_string(), "rate_limit");
        assert_eq!(FailoverReason::Overloaded.to_string(), "overloaded");
        assert_eq!(FailoverReason::Timeout.to_string(), "timeout");
        assert_eq!(FailoverReason::Format.to_string(), "format");
        assert_eq!(FailoverReason::Unknown.to_string(), "unknown");
    }

    // ── LlmError methods ──

    #[test]
    fn retryable_error_is_retryable() {
        let err = LlmError::Retryable {
            source: anyhow::anyhow!("rate limited"),
            retry_after: Some(Duration::from_secs(5)),
            reason: FailoverReason::RateLimit,
        };
        assert!(err.is_retryable());
        assert_eq!(err.reason(), FailoverReason::RateLimit);
    }

    #[test]
    fn fatal_error_is_not_retryable() {
        let err = LlmError::Fatal {
            source: anyhow::anyhow!("auth failed"),
            reason: FailoverReason::Auth,
        };
        assert!(!err.is_retryable());
        assert_eq!(err.reason(), FailoverReason::Auth);
    }

    #[test]
    fn interrupted_error_is_not_retryable() {
        let err = LlmError::Interrupted;
        assert!(!err.is_retryable());
        assert_eq!(err.reason(), FailoverReason::Unknown);
    }

    #[test]
    fn llm_error_display() {
        let retryable = LlmError::Retryable {
            source: anyhow::anyhow!("overloaded"),
            retry_after: None,
            reason: FailoverReason::Overloaded,
        };
        assert_eq!(retryable.to_string(), "overloaded");

        let fatal = LlmError::Fatal {
            source: anyhow::anyhow!("bad request"),
            reason: FailoverReason::Format,
        };
        assert_eq!(fatal.to_string(), "bad request");

        assert_eq!(LlmError::Interrupted.to_string(), "request interrupted");
    }

    // ── classify_status ──

    #[test]
    fn classify_429_is_retryable_rate_limit() {
        let err = classify_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "{}",
            Provider::OpenRouter,
        );
        assert!(err.is_retryable());
        assert_eq!(err.reason(), FailoverReason::RateLimit);
    }

    #[test]
    fn classify_500_is_retryable_overloaded() {
        let err = classify_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "error",
            Provider::OpenAi,
        );
        assert!(err.is_retryable());
        assert_eq!(err.reason(), FailoverReason::Overloaded);
    }

    #[test]
    fn classify_502_is_retryable() {
        let err = classify_status(reqwest::StatusCode::BAD_GATEWAY, "", Provider::Anthropic);
        assert!(err.is_retryable());
        assert_eq!(err.reason(), FailoverReason::Overloaded);
    }

    #[test]
    fn classify_503_is_retryable_overloaded() {
        let err = classify_status(
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "",
            Provider::Gemini,
        );
        assert!(err.is_retryable());
        assert_eq!(err.reason(), FailoverReason::Overloaded);
    }

    #[test]
    fn classify_504_is_retryable() {
        let err = classify_status(
            reqwest::StatusCode::GATEWAY_TIMEOUT,
            "",
            Provider::OpenRouter,
        );
        assert!(err.is_retryable());
        assert_eq!(err.reason(), FailoverReason::Overloaded);
    }

    #[test]
    fn classify_401_is_fatal_auth() {
        let err = classify_status(
            reqwest::StatusCode::UNAUTHORIZED,
            "invalid key",
            Provider::OpenAi,
        );
        assert!(!err.is_retryable());
        assert_eq!(err.reason(), FailoverReason::Auth);
    }

    #[test]
    fn classify_403_is_fatal_auth_permanent() {
        let err = classify_status(
            reqwest::StatusCode::FORBIDDEN,
            "forbidden",
            Provider::Anthropic,
        );
        assert!(!err.is_retryable());
        assert_eq!(err.reason(), FailoverReason::AuthPermanent);
    }

    #[test]
    fn classify_402_is_fatal_billing() {
        let err = classify_status(
            reqwest::StatusCode::PAYMENT_REQUIRED,
            "insufficient credits",
            Provider::OpenRouter,
        );
        assert!(!err.is_retryable());
        assert_eq!(err.reason(), FailoverReason::Billing);
    }

    #[test]
    fn classify_400_is_fatal_format() {
        let err = classify_status(
            reqwest::StatusCode::BAD_REQUEST,
            "malformed",
            Provider::OpenAi,
        );
        assert!(!err.is_retryable());
        assert_eq!(err.reason(), FailoverReason::Format);
    }

    #[test]
    fn classify_unknown_status_is_fatal() {
        let err = classify_status(reqwest::StatusCode::IM_A_TEAPOT, "", Provider::OpenRouter);
        assert!(!err.is_retryable());
        assert_eq!(err.reason(), FailoverReason::Unknown);
    }

    // ── classify_network_error ──

    #[test]
    fn network_error_is_retryable_timeout() {
        let err = classify_network_error(anyhow::anyhow!("connection reset"));
        assert!(err.is_retryable());
        assert_eq!(err.reason(), FailoverReason::Timeout);
    }

    // ── parse_retry_after ──

    #[test]
    fn parse_retry_after_from_json() {
        let body = r#"{"error":{"retry_after":2.5}}"#;
        let dur = parse_retry_after(body);
        assert_eq!(dur, Some(Duration::from_secs_f64(2.5)));
    }

    #[test]
    fn parse_retry_after_missing_field() {
        let body = r#"{"error":{"message":"too many requests"}}"#;
        assert_eq!(parse_retry_after(body), None);
    }

    #[test]
    fn parse_retry_after_invalid_json() {
        assert_eq!(parse_retry_after("not json"), None);
    }

    #[test]
    fn parse_retry_after_empty_body() {
        assert_eq!(parse_retry_after(""), None);
    }

    #[test]
    fn parse_retry_after_rejects_zero_and_negative() {
        // Defends the cooldown floor — a hostile/degenerate `retry_after: -1`
        // or `0` shouldn't unpark a credential immediately.
        assert_eq!(parse_retry_after(r#"{"error":{"retry_after":0}}"#), None);
        assert_eq!(parse_retry_after(r#"{"error":{"retry_after":-5}}"#), None);
    }

    #[test]
    fn parse_retry_after_clamps_to_24h() {
        let dur =
            parse_retry_after(r#"{"error":{"retry_after":999999999}}"#).expect("clamped, not None");
        assert_eq!(dur, Duration::from_secs(24 * 60 * 60));
    }

    // ── parse_retry_after_header ──

    #[test]
    fn parse_retry_after_header_seconds_form() {
        assert_eq!(
            parse_retry_after_header("300"),
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn parse_retry_after_header_with_whitespace() {
        assert_eq!(
            parse_retry_after_header(" 45 "),
            Some(Duration::from_secs(45))
        );
    }

    #[test]
    fn parse_retry_after_header_rejects_zero_and_negative() {
        assert_eq!(parse_retry_after_header("0"), None);
        assert_eq!(parse_retry_after_header("-5"), None);
    }

    #[test]
    fn parse_retry_after_header_clamps_to_24h() {
        // 99_999_999 seconds → clamped to 86_400 (24h).
        let d = parse_retry_after_header("99999999").expect("clamped value");
        assert_eq!(d, Duration::from_secs(86_400));
    }

    #[test]
    fn parse_retry_after_header_rejects_non_numeric() {
        // HTTP-date form is technically valid per RFC 7231 but we don't
        // parse it — providers we talk to always emit the seconds form.
        assert_eq!(
            parse_retry_after_header("Wed, 21 Oct 2026 07:28:00 GMT"),
            None
        );
        assert_eq!(parse_retry_after_header(""), None);
        assert_eq!(parse_retry_after_header("abc"), None);
    }

    // ── override_retry_after ──

    #[test]
    fn override_retry_after_replaces_on_retryable() {
        let err = LlmError::Retryable {
            source: anyhow::anyhow!("rate limited"),
            retry_after: Some(Duration::from_secs(5)),
            reason: FailoverReason::RateLimit,
        };
        let overridden = override_retry_after(err, Some(Duration::from_secs(300)));
        if let LlmError::Retryable { retry_after, .. } = overridden {
            assert_eq!(retry_after, Some(Duration::from_secs(300)));
        } else {
            panic!("variant preserved");
        }
    }

    #[test]
    fn override_retry_after_noop_when_override_is_none() {
        let err = LlmError::Retryable {
            source: anyhow::anyhow!("rate limited"),
            retry_after: Some(Duration::from_secs(5)),
            reason: FailoverReason::RateLimit,
        };
        let kept = override_retry_after(err, None);
        if let LlmError::Retryable { retry_after, .. } = kept {
            assert_eq!(retry_after, Some(Duration::from_secs(5)));
        } else {
            panic!("variant preserved");
        }
    }

    #[test]
    fn override_retry_after_passthrough_on_fatal() {
        let err = LlmError::Fatal {
            source: anyhow::anyhow!("auth"),
            reason: FailoverReason::Auth,
        };
        // Fatal variants don't carry retry_after — override should be a no-op.
        let kept = override_retry_after(err, Some(Duration::from_secs(300)));
        assert!(matches!(kept, LlmError::Fatal { .. }));
    }

    // ── classify_status with retry_after in body ──

    // ── New variants: context overflow, payload too large, model not found ──

    #[test]
    fn classify_400_with_context_overflow_body() {
        // Real-ish OpenAI-style error: "This model's maximum context length is..."
        let body =
            r#"{"error":{"message":"This model's maximum context length is 200000 tokens"}}"#;
        let err = classify_status(reqwest::StatusCode::BAD_REQUEST, body, Provider::Anthropic);
        assert!(!err.is_retryable());
        assert_eq!(err.reason(), FailoverReason::ContextOverflow);
        assert!(err.should_compress());
    }

    #[test]
    fn classify_400_anthropic_prompt_too_long() {
        // Anthropic phrasing variant.
        let body = r#"{"error":{"type":"invalid_request_error","message":"prompt is too long: 205000 tokens > 200000 maximum"}}"#;
        let err = classify_status(reqwest::StatusCode::BAD_REQUEST, body, Provider::Anthropic);
        assert_eq!(err.reason(), FailoverReason::ContextOverflow);
        assert!(err.should_compress());
    }

    #[test]
    fn classify_400_generic_bad_request_is_format_not_overflow() {
        // A 400 without any context-related phrasing must NOT be misclassified —
        // compacting on a genuine format error would silently drop history.
        let err = classify_status(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"invalid role 'agent' in messages[0]"}}"#,
            Provider::OpenAi,
        );
        assert_eq!(err.reason(), FailoverReason::Format);
        assert!(!err.should_compress());
    }

    #[test]
    fn classify_404_with_model_body_is_model_not_found() {
        let body = r#"{"error":{"message":"The model 'gpt-6-ultra' does not exist"}}"#;
        let err = classify_status(reqwest::StatusCode::NOT_FOUND, body, Provider::OpenAi);
        assert_eq!(err.reason(), FailoverReason::ModelNotFound);
        assert!(err.is_fast_fail());
        assert!(!err.should_fallback());
    }

    #[test]
    fn classify_404_no_allowed_providers_is_auth_permanent() {
        // Real OpenRouter response when the account's allowed-providers filter
        // excludes every upstream that serves the model. The body mentions
        // "model" too — verify the more specific branch wins and the surfaced
        // message points the user at provider preferences.
        let body = r#"{"error":{"message":"No allowed providers are available for the selected model.","code":404,"metadata":{"available_providers":["fireworks","together","minimax"]}}}"#;
        let err = classify_status(reqwest::StatusCode::NOT_FOUND, body, Provider::OpenRouter);
        assert_eq!(err.reason(), FailoverReason::AuthPermanent);
        let rendered = err.to_string();
        assert!(
            rendered.contains("no allowed providers"),
            "expected user-facing message, got: {rendered}"
        );
        assert!(
            rendered.contains("openrouter.ai/settings/preferences"),
            "expected preferences link, got: {rendered}"
        );
    }

    #[test]
    fn classify_404_without_model_body_is_unknown() {
        // Generic routing 404 — not model-specific, don't fast-fail.
        let err = classify_status(
            reqwest::StatusCode::NOT_FOUND,
            "not found",
            Provider::OpenAi,
        );
        assert_eq!(err.reason(), FailoverReason::Unknown);
    }

    #[test]
    fn classify_413_is_payload_too_large() {
        let err = classify_status(reqwest::StatusCode::PAYLOAD_TOO_LARGE, "", Provider::OpenAi);
        assert_eq!(err.reason(), FailoverReason::PayloadTooLarge);
        assert!(err.is_fast_fail());
    }

    // ── Recovery-hint invariants ──
    //
    // should_compress() must be true for ContextOverflow only. Reactive
    // compaction is destructive to history, so any drift here silently drops
    // messages on errors that aren't really about context length.
    #[test]
    fn should_compress_only_for_context_overflow() {
        for reason in ALL_REASONS {
            let err = LlmError::Fatal {
                source: anyhow::anyhow!("test"),
                reason: *reason,
            };
            let expected = *reason == FailoverReason::ContextOverflow;
            assert_eq!(
                err.should_compress(),
                expected,
                "should_compress for {reason:?}"
            );
        }
    }

    #[test]
    fn should_rotate_credential_skips_auth_permanent() {
        // Auth (401) rotates; AuthPermanent (403) must NOT — the whole
        // account is denied, rotating keys burns them pointlessly.
        let auth = LlmError::Fatal {
            source: anyhow::anyhow!("401"),
            reason: FailoverReason::Auth,
        };
        let perm = LlmError::Fatal {
            source: anyhow::anyhow!("403"),
            reason: FailoverReason::AuthPermanent,
        };
        assert!(auth.should_rotate_credential());
        assert!(!perm.should_rotate_credential());
    }

    #[test]
    fn should_fallback_skips_model_not_found_and_auth_permanent() {
        for reason in ALL_REASONS {
            let err = LlmError::Fatal {
                source: anyhow::anyhow!("test"),
                reason: *reason,
            };
            let expected = !matches!(
                reason,
                FailoverReason::ModelNotFound | FailoverReason::AuthPermanent
            );
            assert_eq!(
                err.should_fallback(),
                expected,
                "should_fallback for {reason:?}"
            );
        }
    }

    // ── classify_error_text ──

    #[test]
    fn classify_error_text_recognizes_classifier_marker() {
        // Match the format produced by `classify_status` for 400s.
        let msg = "anthropic returned 400 (context overflow): prompt too long";
        assert_eq!(
            classify_error_text(msg),
            Some(FailoverReason::ContextOverflow)
        );
    }

    #[test]
    fn classify_error_text_recognizes_raw_provider_phrases() {
        // Bypass the classifier marker — raw phrase (e.g., from a transport
        // adapter that didn't go through classify_status).
        assert_eq!(
            classify_error_text("this model's maximum context length is 200k tokens"),
            Some(FailoverReason::ContextOverflow)
        );
        assert_eq!(
            classify_error_text("prompt is too long: 205k > 200k"),
            Some(FailoverReason::ContextOverflow)
        );
    }

    #[test]
    fn classify_error_text_returns_none_for_unrelated() {
        assert_eq!(classify_error_text("connection reset by peer"), None);
        assert_eq!(
            classify_error_text("rate limit exceeded, retry in 30s"),
            None
        );
    }

    #[test]
    fn classify_error_text_recognizes_other_markers() {
        assert_eq!(
            classify_error_text("openai returned 404 (model not found): gpt-6"),
            Some(FailoverReason::ModelNotFound)
        );
        assert_eq!(
            classify_error_text("openai returned 413 (payload too large): "),
            Some(FailoverReason::PayloadTooLarge)
        );
        assert_eq!(
            classify_error_text("anthropic returned 403 (auth permanent): "),
            Some(FailoverReason::AuthPermanent)
        );
    }

    /// Every variant of `FailoverReason`. Listed explicitly rather than derived
    /// so adding a new variant forces an update here and in the recovery-hint
    /// tests above — that's the compile-time guard this const provides.
    const ALL_REASONS: &[FailoverReason] = &[
        FailoverReason::Auth,
        FailoverReason::AuthPermanent,
        FailoverReason::Billing,
        FailoverReason::RateLimit,
        FailoverReason::Overloaded,
        FailoverReason::Timeout,
        FailoverReason::Format,
        FailoverReason::ContextOverflow,
        FailoverReason::PayloadTooLarge,
        FailoverReason::ModelNotFound,
        FailoverReason::Unknown,
    ];

    // ── Body redaction & truncation ──

    #[test]
    fn classify_redacts_anthropic_key_in_body() {
        // Provider 401 echoing back the key Borg sent. Must not survive into the
        // error chain (which is logged at WARN by agent and gateway).
        let leaked = "sk-ant-api03-aBcDeFgHiJkLmNoPqRsTuVwXyZ012345678ZZZ";
        let body = format!(r#"{{"error":{{"message":"invalid key {leaked}"}}}}"#);
        let err = classify_status(
            reqwest::StatusCode::UNAUTHORIZED,
            &body,
            Provider::Anthropic,
        );
        let rendered = err.to_string();
        assert!(
            !rendered.contains(leaked),
            "raw key leaked into error: {rendered}"
        );
        assert!(
            rendered.contains("[REDACTED"),
            "expected redaction marker, got: {rendered}"
        );
    }

    #[test]
    fn classify_redacts_credential_assignment_in_body() {
        // KEY=value style env-leaking response. capture_only pattern keeps the
        // variable name visible but masks the value.
        let body =
            "OPENAI_API_KEY=sk-1234567890abcdef1234567890abcdef1234567890abcdef returned an error";
        let err = classify_status(reqwest::StatusCode::BAD_GATEWAY, body, Provider::OpenAi);
        let rendered = err.to_string();
        assert!(
            !rendered.contains("sk-1234567890abcdef"),
            "key leaked: {rendered}"
        );
        assert!(rendered.contains("[REDACTED"), "no marker: {rendered}");
    }

    #[test]
    fn classify_truncates_oversized_body() {
        // Bodies above ERROR_BODY_MAX_LEN must be truncated to keep daemon.log
        // bounded under provider chattiness.
        let body = "x".repeat(ERROR_BODY_MAX_LEN * 4);
        let err = classify_status(
            reqwest::StatusCode::BAD_GATEWAY,
            &body,
            Provider::OpenRouter,
        );
        let rendered = err.to_string();
        assert!(
            rendered.contains("…[truncated]"),
            "expected truncation: len={}",
            rendered.len()
        );
        // Total rendered string includes the prefix ("openrouter returned …: ");
        // the body portion alone is what we're capping.
        assert!(
            rendered.len() < body.len(),
            "body not truncated: {}",
            rendered.len()
        );
    }

    #[test]
    fn classify_429_extracts_retry_after() {
        let body = r#"{"error":{"retry_after":10.0}}"#;
        let err = classify_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            body,
            Provider::OpenRouter,
        );
        assert!(err.is_retryable());
        if let LlmError::Retryable { retry_after, .. } = err {
            assert_eq!(retry_after, Some(Duration::from_secs(10)));
        } else {
            panic!("expected Retryable");
        }
    }

    // ── parse_retry_hint_from_body ──

    #[test]
    fn retry_hint_empty_body_is_none() {
        assert!(parse_retry_hint_from_body("").is_none());
    }

    #[test]
    fn retry_hint_iso8601_resets_at() {
        let future = chrono::Utc::now() + chrono::Duration::seconds(300);
        let body = format!(r#"{{"resets_at":"{}"}}"#, future.to_rfc3339());
        let d = parse_retry_hint_from_body(&body).expect("should parse");
        assert!(
            d.as_secs() >= 298 && d.as_secs() <= 301,
            "got {}",
            d.as_secs()
        );
    }

    #[test]
    fn retry_hint_epoch_seconds() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let body = format!(r#"{{"resets_at":{}}}"#, now + 120);
        let d = parse_retry_hint_from_body(&body).expect("should parse");
        assert!(d.as_secs() >= 118 && d.as_secs() <= 121);
    }

    #[test]
    fn retry_hint_epoch_millis_autodetected() {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let body = format!(r#"{{"resets_at":{}}}"#, now_ms + 60_000);
        let d = parse_retry_hint_from_body(&body).expect("should parse");
        assert!(d.as_secs() >= 58 && d.as_secs() <= 61);
    }

    #[test]
    fn retry_hint_quota_reset_delay_ms() {
        let body = r#"{"error":{"message":"quotaResetDelay: 2500ms"}}"#;
        let d = parse_retry_hint_from_body(body).expect("should parse");
        // clamp_retry enforces a 1s floor.
        assert!(d.as_secs() >= 1);
    }

    #[test]
    fn retry_hint_quota_reset_delay_seconds() {
        let body = r#"{"error":{"message":"quotaResetDelay: 30s"}}"#;
        let d = parse_retry_hint_from_body(body).expect("should parse");
        assert_eq!(d.as_secs(), 30);
    }

    #[test]
    fn retry_hint_try_again_in_minutes() {
        let body = r#"{"error":{"message":"please try again in 5 minutes"}}"#;
        let d = parse_retry_hint_from_body(body).expect("should parse");
        assert_eq!(d.as_secs(), 300);
    }

    #[test]
    fn retry_hint_retry_after_seconds() {
        let body = r#"{"error":{"message":"please retry after 45 seconds"}}"#;
        let d = parse_retry_hint_from_body(body).expect("should parse");
        assert_eq!(d.as_secs(), 45);
    }

    #[test]
    fn retry_hint_past_timestamp_is_none() {
        let past = chrono::Utc::now() - chrono::Duration::hours(1);
        let body = format!(r#"{{"resets_at":"{}"}}"#, past.to_rfc3339());
        assert!(parse_retry_hint_from_body(&body).is_none());
    }

    #[test]
    fn retry_hint_clamps_to_24h() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // 5 days in the future — must clamp to 24h.
        let body = format!(r#"{{"resets_at":{}}}"#, now + 5 * 86_400);
        let d = parse_retry_hint_from_body(&body).expect("should parse");
        assert_eq!(d.as_secs(), 24 * 60 * 60);
    }

    #[test]
    fn retry_hint_malformed_json_no_panic() {
        // Body is not JSON but contains a free-form hint.
        let d = parse_retry_hint_from_body("not json, try again in 30 seconds");
        assert_eq!(d, Some(Duration::from_secs(30)));
    }

    #[test]
    fn retry_hint_openrouter_wrapped() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let inner = format!(r#"{{"error":{{"resets_at":{}}}}}"#, now + 90);
        let body = format!(
            r#"{{"error":{{"message":"upstream","metadata":{{"raw":{}}}}}}}"#,
            serde_json::to_string(&inner).unwrap()
        );
        let d = parse_retry_hint_from_body(&body).expect("should parse");
        assert!(d.as_secs() >= 88 && d.as_secs() <= 92);
    }

    // ── classify_402 ──

    #[test]
    fn classify_402_usage_limit_transient_is_rate_limit() {
        assert_eq!(
            classify_402("usage limit reached, try again in 5 minutes"),
            FailoverReason::RateLimit
        );
    }

    #[test]
    fn classify_402_credit_exhausted_is_billing() {
        assert_eq!(
            classify_402("credit balance exhausted"),
            FailoverReason::Billing
        );
    }

    #[test]
    fn classify_402_plain_payment_required_is_billing() {
        assert_eq!(classify_402("payment required"), FailoverReason::Billing);
    }

    #[test]
    fn classify_402_quota_resets_in_is_rate_limit() {
        assert_eq!(
            classify_402("quota exceeded; resets at 2026-05-12"),
            FailoverReason::RateLimit
        );
    }

    #[test]
    fn classify_status_402_with_transient_phrasing_is_retryable() {
        let body = r#"{"error":{"message":"usage limit reached, try again in 60 seconds"}}"#;
        let err = classify_status(
            reqwest::StatusCode::PAYMENT_REQUIRED,
            body,
            Provider::OpenRouter,
        );
        assert!(err.is_retryable(), "transient 402 should be retryable");
        assert_eq!(err.reason(), FailoverReason::RateLimit);
    }
}
