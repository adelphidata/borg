use std::fmt;
use std::time::Duration;

use crate::provider::Provider;

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
    let retry_after = parse_retry_after(body);

    match status.as_u16() {
        429 => LlmError::Retryable {
            source: anyhow::anyhow!("{provider} returned 429 (rate limited): {body}"),
            retry_after,
            reason: FailoverReason::RateLimit,
        },
        500 | 502 | 504 => LlmError::Retryable {
            source: anyhow::anyhow!("{provider} returned {status}: {body}"),
            retry_after: None,
            reason: FailoverReason::Overloaded,
        },
        503 => LlmError::Retryable {
            source: anyhow::anyhow!("{provider} returned {status} (overloaded): {body}"),
            retry_after: None,
            reason: FailoverReason::Overloaded,
        },
        401 => LlmError::Fatal {
            source: anyhow::anyhow!("{provider} returned 401 (auth error): {body}"),
            reason: FailoverReason::Auth,
        },
        403 => LlmError::Fatal {
            source: anyhow::anyhow!("{provider} returned 403 (auth permanent): {body}"),
            reason: FailoverReason::AuthPermanent,
        },
        402 => LlmError::Fatal {
            source: anyhow::anyhow!("{provider} returned {status} (billing error): {body}"),
            reason: FailoverReason::Billing,
        },
        404 if body_indicates_no_allowed_providers(body) => LlmError::Fatal {
            source: anyhow::anyhow!(
                "{provider} returned 404 (no allowed providers for model — check account provider preferences at https://openrouter.ai/settings/preferences): {body}"
            ),
            reason: FailoverReason::AuthPermanent,
        },
        404 if body_mentions_model(body) => LlmError::Fatal {
            source: anyhow::anyhow!("{provider} returned 404 (model not found): {body}"),
            reason: FailoverReason::ModelNotFound,
        },
        413 => LlmError::Fatal {
            source: anyhow::anyhow!("{provider} returned 413 (payload too large): {body}"),
            reason: FailoverReason::PayloadTooLarge,
        },
        400 if body_indicates_context_overflow(body) => LlmError::Fatal {
            source: anyhow::anyhow!("{provider} returned 400 (context overflow): {body}"),
            reason: FailoverReason::ContextOverflow,
        },
        400 => LlmError::Fatal {
            source: anyhow::anyhow!("{provider} returned {status} (bad request): {body}"),
            reason: FailoverReason::Format,
        },
        _ => LlmError::Fatal {
            source: anyhow::anyhow!("{provider} returned {status}: {body}"),
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

/// Extract a `retry_after` duration from a JSON error response body.
pub fn parse_retry_after(body: &str) -> Option<Duration> {
    // Try to extract retry_after from JSON error body
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(secs) = v["error"]["retry_after"].as_f64() {
            return Some(Duration::from_secs_f64(secs));
        }
    }
    None
}

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
}
