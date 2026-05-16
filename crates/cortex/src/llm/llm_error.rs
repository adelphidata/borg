use std::fmt;
use std::time::Duration;

use crate::llm::provider::Provider;
use crate::security::secrets::redact_secrets;

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
        401 => {
            // Gemini AI Studio uses static API keys (no token-refresh dance)
            // — a 401 means the key is wrong/revoked and rotation alone won't
            // recover it. Treat as permanent so the pool retires the
            // credential instead of cooling-down-and-retrying.
            let reason = if matches!(provider, Provider::Gemini) {
                FailoverReason::AuthPermanent
            } else {
                FailoverReason::Auth
            };
            LlmError::Fatal {
                source: anyhow::anyhow!("{provider} returned 401 (auth error): {safe}"),
                reason,
            }
        }
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

/// Phrases that signal the request exceeded the model's context window.
/// Shared between `body_indicates_context_overflow` (the 400-body refinement
/// in `classify_status`), the Anthropic `invalid_request_error` SSE branch,
/// and `classify_error_text` so the typed-status, structured-stream, and
/// opaque-string paths stay in sync.
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
    "request too large",
    "prompt too long",
    "maximum token",
    "exceeds the model",
];

const AUTH_PERMANENT_NEEDLES: &[&str] = &[
    " 403 ",
    "(403)",
    "http 403",
    "status 403",
    "forbidden",
    "permission_error",
    "permission_denied",
];

const AUTH_NEEDLES: &[&str] = &[
    " 401 ",
    "(401)",
    "http 401",
    "status 401",
    "unauthorized",
    "invalid api key",
    "invalid_api_key",
    "authentication failed",
    "authentication_error",
    "auth error",
];

const BILLING_NEEDLES: &[&str] = &[
    " 402 ",
    "(402)",
    "http 402",
    "status 402",
    "payment required",
    "insufficient_quota",
    "quota exceeded",
    "exceeded your current quota",
    "out of credits",
    "no credits",
    "billing error",
];

const RATE_LIMIT_NEEDLES: &[&str] = &[
    " 429 ",
    "(429)",
    "http 429",
    "status 429",
    " rate limit",
    "rate-limit",
    "rate_limit",
    "rate_limit_error",
    "too many requests",
    "throttl",
];

const OVERLOADED_NEEDLES: &[&str] = &[
    " 500 ",
    "(500)",
    "http 500",
    "status 500",
    " 502 ",
    "(502)",
    "http 502",
    "status 502",
    " 503 ",
    "(503)",
    "http 503",
    "status 503",
    " 504 ",
    "(504)",
    "http 504",
    "status 504",
    "overloaded_error",
    "overloaded",
    "service unavailable",
    "bad gateway",
    "internal server error",
    "api_error",
];

const TIMEOUT_NEEDLES: &[&str] = &[
    "timed out",
    "timeout",
    "timeout_error",
    "request timed out",
    "deadline exceeded",
    "connection reset",
    "connection refused",
    "broken pipe",
    "network unreachable",
    "no route to host",
    " dns ",
    "dns lookup",
    "dns resolution",
    " tls ",
    "tls handshake",
    " ssl ",
    "ssl handshake",
    "certificate verify",
];

fn matches_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

fn body_indicates_context_overflow(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    matches_any(&lower, OVERFLOW_NEEDLES)
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

/// Recover a `FailoverReason` from an opaque error message string.
///
/// This is the single string-matching front for every place in cortex that
/// can only see an error as text — the agent loop's reactive-compaction
/// trigger, the user-facing `error_format` layer, the scheduled-task retry
/// decision, and any provider adapter (e.g. Claude CLI) that surfaces
/// `StreamEvent::Error(String)` after the typed `LlmError` was already
/// stringified. Keeping one shared classifier prevents the three subsystems
/// from drifting apart on what counts as "auth" vs "billing" vs "transient".
///
/// Order matters because some phrases overlap:
/// - context-overflow phrases beat rate-limit unless the body explicitly
///   names a TPM/RPM rate (Anthropic 429s name the window),
/// - auth-permanent beats auth (403 needles would otherwise look like 401),
/// - billing beats rate-limit ("throttl" rarely co-occurs with billing
///   phrases, but quota-style errors should not retry).
pub fn classify_error_text(msg: &str) -> Option<FailoverReason> {
    // Pad with spaces so " 429 ", " 500 ", " dns " etc. style needles match
    // at message edges as well as in the middle.
    let padded = format!(" {} ", msg.to_ascii_lowercase());
    let lower = padded.as_str();

    // Classifier-produced markers first — cheap, unambiguous.
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

    // Disambiguation: a context-overflow phrase wins over rate-limit unless
    // the body explicitly mentions a per-minute rate (TPM/RPM). Anthropic
    // 429s often include both "rate_limit_error" and a prompt-length number,
    // so without this guard the wrong branch fires on each direction.
    let mentions_overflow = matches_any(lower, OVERFLOW_NEEDLES);
    let mentions_tpm = lower.contains("tokens per minute") || lower.contains("requests per minute");
    if mentions_overflow && !mentions_tpm {
        return Some(FailoverReason::ContextOverflow);
    }

    if matches_any(lower, AUTH_PERMANENT_NEEDLES) {
        return Some(FailoverReason::AuthPermanent);
    }
    if matches_any(lower, AUTH_NEEDLES) {
        return Some(FailoverReason::Auth);
    }
    if matches_any(lower, BILLING_NEEDLES) {
        return Some(FailoverReason::Billing);
    }
    if matches_any(lower, RATE_LIMIT_NEEDLES) {
        return Some(FailoverReason::RateLimit);
    }
    if matches_any(lower, OVERLOADED_NEEDLES) {
        return Some(FailoverReason::Overloaded);
    }
    if matches_any(lower, TIMEOUT_NEEDLES) {
        return Some(FailoverReason::Timeout);
    }

    None
}

/// Map an Anthropic SSE `error.type` string to a `FailoverReason`.
///
/// Anthropic stream errors carry a structured `error.type` (e.g.
/// `overloaded_error`, `rate_limit_error`, `authentication_error`).
/// Before this helper existed, the SSE handler bucketed every variant as
/// `Overloaded`, defeating credential rotation and fast-fail on the
/// streaming path. For `invalid_request_error` (the catch-all for
/// malformed bodies) the message text disambiguates: a prompt-length body
/// should drive compaction; anything else is `Format`. Unknown
/// `error.type` strings fall through to `classify_error_text` so a
/// provider rolling out a new variant still classifies usefully.
pub(crate) fn classify_anthropic_stream_error(err_type: &str, err_msg: &str) -> FailoverReason {
    match err_type {
        "overloaded_error" | "api_error" => FailoverReason::Overloaded,
        "rate_limit_error" => FailoverReason::RateLimit,
        "authentication_error" => FailoverReason::Auth,
        "permission_error" => FailoverReason::AuthPermanent,
        "not_found_error" => FailoverReason::ModelNotFound,
        "timeout_error" => FailoverReason::Timeout,
        "request_too_large" => FailoverReason::PayloadTooLarge,
        "invalid_request_error" => {
            let lower = err_msg.to_ascii_lowercase();
            if matches_any(&lower, OVERFLOW_NEEDLES) {
                FailoverReason::ContextOverflow
            } else {
                FailoverReason::Format
            }
        }
        _ => classify_error_text(err_msg).unwrap_or(FailoverReason::Unknown),
    }
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
///
/// Also recognises Google's `RetryInfo` shape — `error.details[].retryDelay:
/// "37s"` — which Gemini AI Studio emits on 429 responses. Without this, a
/// Gemini 429 would fall back to the default cooldown and ignore the
/// server-honoured delay.
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
///
/// The Google APIs spec puts `RetryInfo` in `details[]` at most once per
/// response, so the first parseable match is canonical. Only the proto3-JSON
/// Duration form (`"<N>s"` / `"<N.M>s"`) is recognised — the ISO-8601
/// (`"PT37S"`) and ms-suffix (`"37000ms"`) variants seen on other Google
/// surfaces don't appear on AI Studio's 429 path today.
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

    #[test]
    fn parse_retry_after_reads_google_retry_delay() {
        // Gemini AI Studio 429s carry the server's cooldown in `RetryInfo`
        // proto encoding inside `error.details[]`. Without this branch a
        // Gemini 429 would fall back to the default cooldown and the pool
        // would over-park the credential.
        let body = r#"{
          "error": {
            "code": 429,
            "message": "Quota exceeded.",
            "status": "RESOURCE_EXHAUSTED",
            "details": [
              {"@type": "type.googleapis.com/google.rpc.QuotaFailure", "violations": []},
              {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "37s"}
            ]
          }
        }"#;
        assert_eq!(parse_retry_after(body), Some(Duration::from_secs(37)));
    }

    #[test]
    fn parse_retry_after_zero_falls_through_to_google_retry_delay() {
        // Regression guard: a degenerate top-level `retry_after: 0` (or any
        // non-positive value) must not mask the Google `retryDelay` fallback.
        // Pre-fix the early `secs?` short-circuited on the zero, the post-
        // validation `<= 0.0` guard returned None, and the valid 37s hint
        // was silently lost — the pool would over-park the credential.
        let body = r#"{
          "error": {
            "code": 429,
            "retry_after": 0,
            "details": [
              {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "37s"}
            ]
          }
        }"#;
        assert_eq!(parse_retry_after(body), Some(Duration::from_secs(37)));
        let neg = r#"{
          "error": {
            "retry_after": -5,
            "details": [
              {"retryDelay": "12s"}
            ]
          }
        }"#;
        assert_eq!(parse_retry_after(neg), Some(Duration::from_secs(12)));
    }

    #[test]
    fn classify_status_429_with_google_retry_delay_is_retryable_with_delay() {
        // Wires the full path: status::classify_status → parse_retry_after →
        // Google `retryDelay` parser. Regression guard for the Gemini 429
        // backoff issue.
        let body = r#"{
          "error": {
            "code": 429,
            "message": "Quota exceeded.",
            "status": "RESOURCE_EXHAUSTED",
            "details": [
              {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "37s"}
            ]
          }
        }"#;
        let err = classify_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            body,
            Provider::Gemini,
        );
        match err {
            LlmError::Retryable {
                retry_after,
                reason,
                ..
            } => {
                assert_eq!(reason, FailoverReason::RateLimit);
                assert_eq!(retry_after, Some(Duration::from_secs(37)));
            }
            other => unreachable!("expected Retryable, got {other:?}"),
        }
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

    #[test]
    fn classify_401_for_gemini_is_auth_permanent_not_rotatable() {
        // Gemini AI Studio uses static API keys with no refresh path. A 401
        // means the key is structurally bad, so the pool must retire the
        // credential — not cool-down-and-retry it like a rotatable Auth.
        // Guards a regression where consolidating the deleted Gemini-specific
        // classifier into `classify_status` dropped the 401→AuthPermanent
        // distinction the original encoded.
        let err = classify_status(
            reqwest::StatusCode::UNAUTHORIZED,
            r#"{"error":{"message":"API key not valid"}}"#,
            Provider::Gemini,
        );
        assert_eq!(err.reason(), FailoverReason::AuthPermanent);
        assert!(err.is_fast_fail());
        // Sanity: OpenAI 401 keeps the rotatable Auth semantics.
        let err = classify_status(
            reqwest::StatusCode::UNAUTHORIZED,
            r#"{"error":{"message":"Invalid API key"}}"#,
            Provider::OpenAi,
        );
        assert_eq!(err.reason(), FailoverReason::Auth);
    }

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

    // ── classify_error_text: extended taxonomy (auth / billing / rate_limit /
    //    overloaded / timeout). One table per direction so a missing needle
    //    or a misordered branch fails a concrete row instead of a generic
    //    "didn't classify" assertion.

    /// Real provider phrases that MUST classify. Each entry is the kind of
    /// string that can land in a `StreamEvent::Error(_)` body or a logged
    /// anyhow chain — none of these are synthesized for the test. If a
    /// real provider rolls out a phrasing not covered here, this is the
    /// table to add a row to.
    const POSITIVE_CASES: &[(&str, FailoverReason)] = &[
        // Auth: classifier marker + raw phrases providers actually emit.
        (
            "openai returned 401 (auth error): bad key",
            FailoverReason::Auth,
        ),
        ("Invalid API key provided", FailoverReason::Auth),
        (
            "authentication_error: x-api-key header is invalid",
            FailoverReason::Auth,
        ),
        // AuthPermanent: 403 outranks 401 — listed first so the table also
        // exercises the precedence rule, not just the needle.
        (
            "anthropic returned 403 (auth permanent): account disabled",
            FailoverReason::AuthPermanent,
        ),
        (
            "permission_error: organization does not have access to model",
            FailoverReason::AuthPermanent,
        ),
        // Billing: includes provider-marker and raw OpenAI-style phrasing.
        (
            "openrouter returned 402 (billing error): insufficient_quota",
            FailoverReason::Billing,
        ),
        (
            "You exceeded your current quota, please check your plan",
            FailoverReason::Billing,
        ),
        ("HTTP 402 Payment Required", FailoverReason::Billing),
        // RateLimit: classifier marker + raw 429 phrasing + the Anthropic
        // structured-error name. The TPM-mention case is the disambiguation
        // guard — it must beat the OVERFLOW_NEEDLES "tokens" overlap.
        (
            "openai returned 429 (rate limited): too many requests",
            FailoverReason::RateLimit,
        ),
        ("HTTP 429 Too Many Requests", FailoverReason::RateLimit),
        (
            "rate_limit_error: 5000 input tokens per minute exceeded",
            FailoverReason::RateLimit,
        ),
        // Overloaded: server-side 5xx, both classifier-marker-shaped and
        // the SSE `overloaded_error` structured-error name.
        (
            "anthropic stream error (overloaded_error): Overloaded",
            FailoverReason::Overloaded,
        ),
        ("HTTP 502 Bad Gateway", FailoverReason::Overloaded),
        ("internal server error", FailoverReason::Overloaded),
        // Timeout: network failures collapse into Timeout because the
        // failover layer retries+falls-back identically for both. Each row
        // pins a distinct phrasing path.
        ("connection reset by peer", FailoverReason::Timeout),
        ("request timed out after 30000ms", FailoverReason::Timeout),
        ("deadline exceeded", FailoverReason::Timeout),
        (
            "DNS lookup failed for api.openai.com",
            FailoverReason::Timeout,
        ),
        // ContextOverflow: raw provider phrasing variants.
        (
            "This model's maximum context length is 200000 tokens",
            FailoverReason::ContextOverflow,
        ),
        (
            "prompt is too long: 205000 tokens > 200000 maximum",
            FailoverReason::ContextOverflow,
        ),
    ];

    /// Strings that MUST NOT classify. Regression net against an overly-eager
    /// needle: substring traps ("5000" matching "500"), false-positive verbs
    /// ("connected" / "cached"), and empty input.
    const NEGATIVE_CASES: &[&str] = &[
        "Hello world",
        "processed 5000 items",
        "connected on port 4290",
        "successfully cached prompt",
        "",
    ];

    #[test]
    fn classify_error_text_positive_table() {
        for (input, expected) in POSITIVE_CASES {
            assert_eq!(
                classify_error_text(input),
                Some(*expected),
                "input={input:?}",
            );
        }
    }

    #[test]
    fn classify_error_text_negative_table() {
        for input in NEGATIVE_CASES {
            assert_eq!(
                classify_error_text(input),
                None,
                "expected None for input={input:?}",
            );
        }
    }

    #[test]
    fn classify_error_text_recovery_hints_match_reason() {
        // Each FailoverReason flips a specific set of recovery hints
        // (`should_compress`, `should_rotate_credential`, `should_fallback`,
        // `is_fast_fail`). Spec-pin those for every category the agent layer
        // actually keys on. If the LlmError variant or hint logic changes,
        // this fails on the actual behavior, not the enum tag.
        for (input, reason) in POSITIVE_CASES {
            let err = match reason {
                FailoverReason::Auth
                | FailoverReason::AuthPermanent
                | FailoverReason::Billing
                | FailoverReason::ContextOverflow
                | FailoverReason::PayloadTooLarge
                | FailoverReason::ModelNotFound
                | FailoverReason::Format => LlmError::Fatal {
                    source: anyhow::anyhow!("{input}"),
                    reason: *reason,
                },
                _ => LlmError::Retryable {
                    source: anyhow::anyhow!("{input}"),
                    retry_after: None,
                    reason: *reason,
                },
            };
            // Compaction only fires for ContextOverflow.
            assert_eq!(
                err.should_compress(),
                *reason == FailoverReason::ContextOverflow,
                "should_compress for {reason:?} on input={input:?}",
            );
            // Credential rotation only for transient-auth / rate-limit.
            let rotate_expected =
                matches!(reason, FailoverReason::Auth | FailoverReason::RateLimit);
            assert_eq!(
                err.should_rotate_credential(),
                rotate_expected,
                "should_rotate_credential for {reason:?} on input={input:?}",
            );
            // Fast-fail only for the user-fixable / structurally-broken cases.
            let fast_fail_expected = matches!(
                reason,
                FailoverReason::ModelNotFound
                    | FailoverReason::AuthPermanent
                    | FailoverReason::PayloadTooLarge
            );
            assert_eq!(
                err.is_fast_fail(),
                fast_fail_expected,
                "is_fast_fail for {reason:?} on input={input:?}",
            );
        }
    }

    #[test]
    fn classify_error_text_tpm_rate_limit_beats_context_overflow() {
        // Anthropic 429s include "rate_limit_error" and a per-minute token
        // count, and the OVERFLOW_NEEDLES set contains "tokens" via
        // "too many tokens". The TPM mention must keep this as RateLimit;
        // mis-classifying as ContextOverflow would silently trigger
        // history compaction on a transient throttle.
        assert_eq!(
            classify_error_text("rate_limit_error: 200000 tokens per minute exceeded"),
            Some(FailoverReason::RateLimit),
        );
        assert_eq!(
            classify_error_text("rate limit reached. Limit: 100000 requests per minute"),
            Some(FailoverReason::RateLimit),
        );
    }

    #[test]
    fn classify_error_text_context_overflow_with_429_substring_still_overflow() {
        // The opposite direction: an explicit context-length message that
        // happens to mention a 429 code (e.g. a logged proxy hop) must
        // remain ContextOverflow — compacting is the recovery, not retry.
        assert_eq!(
            classify_error_text(
                "this model's maximum context length is 200000 tokens (provider returned 429)"
            ),
            Some(FailoverReason::ContextOverflow),
        );
    }

    #[test]
    fn classify_error_text_auth_permanent_outranks_auth() {
        // 403 markers must win over 401 markers when both happen to appear.
        assert_eq!(
            classify_error_text(
                "anthropic returned 403 (auth permanent): also mentions 401 in the metadata"
            ),
            Some(FailoverReason::AuthPermanent),
        );
    }

    // ── classify_anthropic_stream_error ──

    /// Every Anthropic SSE error.type value we know about plus the
    /// invalid_request_error split. The bug this method fixes was: every
    /// stream error landed as Overloaded, so an auth-during-stream burned
    /// the retry budget. Each row here is a regression on that.
    const ANTHROPIC_STREAM_CASES: &[(&str, &str, FailoverReason)] = &[
        ("overloaded_error", "Overloaded", FailoverReason::Overloaded),
        ("api_error", "internal error", FailoverReason::Overloaded),
        (
            "rate_limit_error",
            "too many requests",
            FailoverReason::RateLimit,
        ),
        (
            "authentication_error",
            "invalid x-api-key",
            FailoverReason::Auth,
        ),
        (
            "permission_error",
            "org disabled",
            FailoverReason::AuthPermanent,
        ),
        (
            "not_found_error",
            "model not found",
            FailoverReason::ModelNotFound,
        ),
        ("timeout_error", "took too long", FailoverReason::Timeout),
        (
            "request_too_large",
            "100MB body",
            FailoverReason::PayloadTooLarge,
        ),
        // invalid_request_error → ContextOverflow ONLY when the message
        // names a context-length issue; otherwise Format.
        (
            "invalid_request_error",
            "prompt is too long: 205k > 200k",
            FailoverReason::ContextOverflow,
        ),
        (
            "invalid_request_error",
            "messages: at least one message is required",
            FailoverReason::Format,
        ),
    ];

    #[test]
    fn classify_anthropic_stream_error_table() {
        for (err_type, err_msg, expected) in ANTHROPIC_STREAM_CASES {
            assert_eq!(
                classify_anthropic_stream_error(err_type, err_msg),
                *expected,
                "type={err_type:?} msg={err_msg:?}",
            );
        }
    }

    #[test]
    fn classify_anthropic_stream_error_unknown_type_with_overflow_message_falls_through() {
        // A future Anthropic error.type we don't recognize should still
        // classify usefully from the message body — that's the
        // classify_error_text fallback. A context-overflow phrase must
        // surface as ContextOverflow, not Unknown.
        assert_eq!(
            classify_anthropic_stream_error(
                "future_unknown_error",
                "this model's maximum context length is 200000 tokens"
            ),
            FailoverReason::ContextOverflow,
        );
    }

    #[test]
    fn classify_anthropic_stream_error_unknown_type_with_opaque_message_is_unknown() {
        // The original bug: an unrecognized stream error was forced to
        // Overloaded. Now it must surface as Unknown so the failover layer
        // doesn't retry forever on something that isn't actually transient.
        assert_eq!(
            classify_anthropic_stream_error("future_unknown_error", "something happened"),
            FailoverReason::Unknown,
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
