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
    // Structured `error.code` values OpenAI sends with a 429 status when the
    // account, not the window, is exhausted: a prepaid balance at zero, or an
    // organization / project spend cap. Retrying or cooling the key cannot
    // clear these — the operator has to top up or raise the cap — so they
    // must classify as billing ahead of the 429 rate-limit needles.
    "credit_balance_exhausted",
    "billing_hard_limit_reached",
    "spend_limit_exceeded",
    "spend limit exceeded",
    "organization_usage_limit_exceeded",
];

/// Structured `error.code` / `error.type` values that mean the *account* is
/// exhausted — a prepaid balance at zero, a hard billing cap, or an
/// organization / project spend cap. Providers send these with a 429 status,
/// but no cooldown clears them: the operator has to top up or raise the cap.
///
/// Deliberately narrower than [`BILLING_NEEDLES`]. A 429 body only turns into
/// a billing verdict on one of these exact codes (or the unmistakable OpenAI
/// quota sentence below); generic wording like "quota exceeded" also appears
/// on per-minute quota throttles (Gemini `RESOURCE_EXHAUSTED`), which must
/// stay retryable.
const BILLING_ERROR_CODES: &[&str] = &[
    "insufficient_quota",
    "billing_hard_limit_reached",
    "credit_balance_exhausted",
    "organization_spend_limit_exceeded",
    "project_spend_limit_exceeded",
    "organization_usage_limit_exceeded",
];

/// Free-text fallback for a 429 body whose structured code did not survive a
/// proxy re-wrap. OpenAI's message for `insufficient_quota`.
const BILLING_QUOTA_PHRASE: &str = "exceeded your current quota";

/// Markers of a per-window throttle. Gemini's free-tier 429 reuses the
/// "exceeded your current quota" sentence with a numeric code, so the prose
/// fallback must not fire when any of these are present.
const THROTTLE_MARKERS: &[&str] = &[
    "resource_exhausted",
    "retrydelay",
    "perminute",
    "per minute",
];

/// Structured code a 503 carries when the provider asks the client to back
/// off (a throttle), as opposed to a genuine outage.
const SLOW_DOWN_CODE: &str = "slow_down";

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
    // Structured 503 back-off code — a throttle wearing an outage status.
    "slow_down",
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

/// Request parameters Borg never puts on the wire, on any route. When a 400
/// rejects one of these by name, the provider's own gateway injected the field
/// into its upstream call and then rejected it — a server-side flake, not a
/// deterministic request-shape error, so retrying the identical request is the
/// correct recovery.
///
/// Deliberately narrow. Before adding an entry, verify no transport builder
/// sends it on ANY route; a parameter Borg sends somewhere needs a
/// per-provider exception here instead of a blanket entry, or a genuine
/// client-side bad parameter stops failing fast.
const SERVER_INJECTED_PARAMS: &[&str] = &["prompt_cache_retention"];

/// Phrases that make the mention an actual rejection of the parameter rather
/// than an incidental appearance in the body.
const PARAM_REJECTION_NEEDLES: &[&str] =
    &["not supported", "unsupported", "unknown", "unrecognized"];

/// True when a 400 body blames a parameter Borg never sends — see
/// [`SERVER_INJECTED_PARAMS`].
pub(super) fn body_indicates_server_injected_param(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    matches_any(&lower, SERVER_INJECTED_PARAMS) && matches_any(&lower, PARAM_REJECTION_NEEDLES)
}

/// Phrases that name the tool-call / function-call part of a request. One of
/// these must be present for a body to read as a malformed tool call; a bare
/// "invalid arguments" could be about anything.
const TOOL_CALL_NEEDLES: &[&str] = &[
    "tool_call",
    "tool call",
    "tool_use",
    "tool use",
    "function_call",
    "function call",
    "function.arguments",
];

/// Phrases that name the *arguments* of that tool call as the offending part
/// (OpenAI `function.arguments`, Anthropic `tool_use.input`).
const TOOL_ARGUMENT_NEEDLES: &[&str] = &["argument", "input"];

/// Phrases that make the mention a parse/validation failure rather than an
/// incidental appearance ("tool_use blocks must be followed by tool_result"
/// is a sequencing error, not broken JSON, and is deliberately excluded).
const MALFORMED_NEEDLES: &[&str] = &[
    "invalid",
    "malformed",
    "not valid",
    "must be a valid",
    "should be a valid",
    "must be valid",
    "failed to parse",
    "could not parse",
    "unable to parse",
    "cannot parse",
    "parse error",
    "unparseable",
    "unexpected token",
    "unexpected end of json",
    "not well-formed",
];

/// True when a 400 body blames the request's own tool-call arguments — the
/// model emitted argument JSON the endpoint refuses to parse or validate.
///
/// All three halves must match: a tool-call needle, an argument needle, and a
/// malformed needle. Typical shapes:
///   OpenAI:    `Invalid parameter: 'tool_calls[0].function.arguments' must be a valid JSON string`
///   Anthropic: `messages.3.content.0.tool_use.input: Input should be a valid dictionary`
///   Relays:    `Invalid tool call arguments: failed to parse JSON`
pub(super) fn body_indicates_malformed_tool_call(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    matches_any(&lower, TOOL_CALL_NEEDLES)
        && matches_any(&lower, TOOL_ARGUMENT_NEEDLES)
        && matches_any(&lower, MALFORMED_NEEDLES)
}

/// Lower-cased string values of the structured `error.code` and `error.type`
/// fields of a JSON error body. Numeric codes (OpenRouter echoes the HTTP
/// status there) carry no extra meaning and are skipped. Empty for a
/// non-JSON body.
fn structured_error_codes(body: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    ["code", "type"]
        .iter()
        .filter_map(|k| v["error"][*k].as_str())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Decide a body against a set of structured codes. When the body carries a
/// string `error.code` / `error.type`, that verdict is authoritative — a
/// `rate_limit_exceeded` code stays a rate limit even if its prose mentions a
/// billing word. Only a body without one (non-JSON, or an aggregator that
/// re-wrapped the upstream error as a message with a numeric code) falls back
/// to a substring match on the raw text.
fn body_has_code(body: &str, codes: &[&str], fallback_phrases: &[&str]) -> bool {
    let structured = structured_error_codes(body);
    if !structured.is_empty() {
        return structured.iter().any(|c| codes.contains(&c.as_str()));
    }
    let lower = body.to_ascii_lowercase();
    matches_any(&lower, codes) || matches_any(&lower, fallback_phrases)
}

/// True when a 429 body says the account is out of money or over a spend cap
/// rather than over a request-rate window — see [`BILLING_ERROR_CODES`].
pub(super) fn body_indicates_billing_exhaustion(body: &str) -> bool {
    if structured_error_codes(body).is_empty()
        && matches_any(&body.to_ascii_lowercase(), THROTTLE_MARKERS)
    {
        return false;
    }
    body_has_code(body, BILLING_ERROR_CODES, &[BILLING_QUOTA_PHRASE])
}

/// True when a 503 body carries the `slow_down` back-off code: the provider is
/// throttling this client, so it is a rate limit, not an outage.
pub(super) fn body_indicates_slow_down(body: &str) -> bool {
    body_has_code(body, &[SLOW_DOWN_CODE], &[])
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

/// Phrases that mean "the image you sent is bad", not "this model is blind".
/// A malformed, oversized, or undownloadable image can carry both an image
/// needle and a rejection needle ("unsupported mimetype for image_url"), and
/// misreading it as a model capability verdict would blind a vision-capable
/// model for the rest of the process over one corrupt file.
const MALFORMED_IMAGE_NEEDLES: &[&str] = &[
    "decode",
    "corrupt",
    "malformed",
    "image format",
    "mimetype",
    "mime type",
    "media type",
    "too large",
    "download",
    "fetch the image",
];

/// True when the provider rejected the request because the *model* cannot
/// accept image input — distinct from a wrong model id even though OpenRouter
/// reports both as 404. Example body:
///   {"error":{"message":"No endpoints found that support image input","code":404}}
///
/// Both halves must match: an image phrase AND a rejection phrase. A bare
/// "image" needle would fire on ordinary prose (an `image_url` mentioned in a
/// harmless 400) and strip pixels a capable model could have read. Bodies that
/// blame the image itself (see [`MALFORMED_IMAGE_NEEDLES`]) are excluded for
/// the same reason.
pub(super) fn body_indicates_image_unsupported(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    let mentions_image = lower.contains("image input")
        || lower.contains("image_url")
        || lower.contains("image content")
        || lower.contains("image parts")
        || lower.contains("multimodal");
    let rejects = lower.contains("no endpoints")
        || lower.contains("not support")
        || lower.contains("unsupported")
        || lower.contains("does not accept")
        || lower.contains("not enabled");
    let blames_the_image = MALFORMED_IMAGE_NEEDLES.iter().any(|n| lower.contains(n));
    mentions_image && rejects && !blames_the_image
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
    if lower.contains("(image input unsupported)") || body_indicates_image_unsupported(lower) {
        return Some(FailoverReason::ImageUnsupported);
    }
    if lower.contains("(auth permanent)") {
        return Some(FailoverReason::AuthPermanent);
    }
    // Marker or raw body. The body check matters because this text path also
    // formats errors that never went through `classify_status` — without it
    // the user gets a truncated provider blob for the one failure they have
    // to act on themselves.
    if lower.contains("(malformed tool call)") || body_indicates_malformed_tool_call(lower) {
        return Some(FailoverReason::MalformedToolCall);
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
            if body_indicates_malformed_tool_call(&lower) {
                FailoverReason::MalformedToolCall
            } else if matches_any(&lower, OVERFLOW_NEEDLES) {
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
