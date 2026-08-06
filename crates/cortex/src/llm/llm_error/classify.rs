//! Error-text classification: maps opaque provider error strings, Anthropic SSE
//! `error.type` values, and 402 bodies onto a [`FailoverReason`]. The needle
//! arrays are the single source of truth shared by the typed-status path
//! ([`super::classify_status`]), the structured-stream path
//! ([`classify_anthropic_stream_error`]), and the opaque-string path
//! ([`classify_error_text`]) so they can't drift apart.

use super::{FailoverReason, LlmError};

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
    // Structured 413-style type re-wrapped into a plain message by
    // aggregators/proxies (no status attribute survives). Routed here so the
    // reactive-compaction recovery fires; the underscored form does not match
    // the spaced "request too large" needle above.
    "request_too_large",
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
    // Per-minute rate windows. Substring-matches both the spelled-out forms
    // ("tokens per minute") and the abbreviated ones OpenAI actually sends
    // ("tokens per min (TPM)"). A body naming a per-minute window is a rate
    // limit even when it also contains overflow-ish wording like "request
    // too large" or "too many tokens".
    "tokens per min",
    "requests per min",
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
    // Stall-detector kills: the SSE watchdog aborts with "No data received
    // for {N}s" and the Bedrock loop with "No Bedrock stream data received
    // for {N}s". These must classify as Timeout or the stale-stream circuit
    // breaker (and transient-retry paths) never see the wedged-stream case.
    "no data received",
    "stream data received",
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

pub(super) fn body_indicates_context_overflow(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    matches_any(&lower, OVERFLOW_NEEDLES)
}

/// True when a 404 body mentions the model, suggesting the model id itself is
/// the problem rather than a generic routing 404.
pub(super) fn body_mentions_model(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("model")
}

/// True when a 401 body signals an *expired* credential rather than a wrong
/// one. AWS surfaces this several ways: SigV4 returns `Signature expired: ...`
/// when creds (or a short-lived `AWS_BEARER_TOKEN_BEDROCK`) age out, and STS
/// session credentials return `The security token included in the request is
/// expired` / `ExpiredTokenException`. Borg cannot refresh an env-provided
/// token in-process, so this routes to a "refresh your credentials" message
/// rather than the generic auth error.
pub(super) fn body_indicates_expired_credential(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("signature expired")
        || lower.contains("expired token")
        || lower.contains("expiredtoken")
        || lower.contains("token has expired")
        || (lower.contains("security token") && lower.contains("expired"))
}

/// True when a 403 body is a Cloudflare edge block rather than a provider
/// auth/permission denial. Cloudflare fronts several LLM providers; when an
/// egress IP is blocked (commonly a restricted region, a flagged VPN/proxy, or
/// a WAF rule) it answers with a 403 HTML interstitial naming Cloudflare and a
/// "blocked"/"Sorry, you have been blocked" phrase. Rotating API keys cannot
/// fix this, so it routes to a "this looks like a network/region block" message
/// instead of the generic permission-denied error.
pub(super) fn body_indicates_cloudflare_block(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    // Require both the Cloudflare attribution AND a *block-interstitial* phrase
    // specific to the edge-block page ("Attention Required! | Cloudflare",
    // "Sorry, you have been blocked"). Deliberately NOT a bare "blocked": a real
    // account/permission 403 proxied through Cloudflare can say "your account has
    // been blocked", and a bare Ray ID rides on every Cloudflare-proxied response
    // — keying on either would mislabel a genuine credential/permission denial as
    // a network/region block and send the user chasing their VPN instead.
    lower.contains("cloudflare")
        && (lower.contains("you have been blocked") || lower.contains("attention required"))
}

/// True when an OpenRouter 404 body indicates the account's provider filters
/// blocked every upstream that serves the model — distinct from "model not
/// found" even though both return 404. Example body:
///   {"error":{"message":"No allowed providers are available for the selected
///   model.","code":404,"metadata":{"available_providers":[...]}}}
pub(super) fn body_indicates_no_allowed_providers(body: &str) -> bool {
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
/// - a rate-limit needle beats context-overflow phrases: throttle bodies
///   routinely embed token wording ("Throttling error: Too many tokens",
///   "Request too large ... on tokens per min (TPM)"), and compacting a
///   healthy session on every 429 is far worse than backing off,
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

    // Disambiguation: a context-overflow phrase wins only when nothing in
    // the body signals a rate limit. Throttle bodies routinely embed token
    // wording — Bedrock's "Throttling error: Too many tokens, please wait
    // before trying again." and OpenAI's "Request too large for ... on
    // tokens per min (TPM)" are both 429-class — and a genuine overflow
    // (400 "prompt is too long") never carries a rate-limit needle. Guarding
    // on the full needle set keeps a throttle from triggering spurious
    // reactive compaction of a healthy session.
    let mentions_overflow = matches_any(lower, OVERFLOW_NEEDLES);
    let mentions_rate_limit = matches_any(lower, RATE_LIMIT_NEEDLES);
    if mentions_overflow && !mentions_rate_limit {
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
        status: None,
    }
}

/// Disambiguate a 402 response between transient usage-limit windows
/// (returns to service) and hard credit exhaustion (operator must act).
/// Transient-style 402s are classified as `RateLimit` so the credential pool
/// cools them; permanent-style 402s are classified as `Billing`.
///
/// Heuristic: usage/quota markers AND a transient signal (try-again /
/// window / reset) → RateLimit; otherwise Billing.
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
