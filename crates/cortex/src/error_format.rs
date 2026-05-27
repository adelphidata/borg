//! Friendly error message formatting for external channels.
//!
//! Classifies raw provider/agent errors and returns human-readable messages
//! suitable for sending to Telegram, Slack, Discord, etc. instead of leaking
//! raw JSON blobs, HTML Cloudflare pages, or internal error details.

use std::fmt;

/// Where the error is being displayed — determines which actionable hints to append.
///
/// Adding a new context (e.g. `Api`) is one enum variant + rows in `ACTION_HINTS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorContext {
    /// Interactive TUI — can reference `/settings` and slash commands.
    Tui,
    /// External messaging channel (Telegram, Slack, etc.) — no slash commands.
    Gateway,
}

/// Classified category of an error for user-facing formatting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    /// Provider returned 429 — rate limited.
    RateLimit,
    /// Provider returned 402 — billing/quota exhausted.
    Billing,
    /// Provider returned 401/403 — authentication error.
    Auth,
    /// Provider returned 500/502/503/504 — server overloaded.
    Overloaded,
    /// Network-level failure (DNS, connection refused, reset).
    Transport,
    /// Context window exceeded.
    ContextOverflow,
    /// Request timed out.
    Timeout,
    /// Unclassified error.
    Unknown,
}

impl fmt::Display for ErrorCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RateLimit => write!(f, "rate_limit"),
            Self::Billing => write!(f, "billing"),
            Self::Auth => write!(f, "auth"),
            Self::Overloaded => write!(f, "overloaded"),
            Self::Transport => write!(f, "transport"),
            Self::ContextOverflow => write!(f, "context_overflow"),
            Self::Timeout => write!(f, "timeout"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

/// Classify a raw error string into an `ErrorCategory`.
///
/// Delegates to the central LLM-error classifier (`classify_error_text`) so
/// the user-facing message-formatting path and the failover-decision path
/// share one taxonomy. The mapping here is the boundary between the
/// `FailoverReason` enum (recovery-strategy primitive) and `ErrorCategory`
/// (UI-text primitive):
///
/// - `Auth` and `AuthPermanent` collapse — the user-visible advice is the
///   same ("check your API key"); the rotate-vs-skip distinction is the
///   failover layer's concern, not the user's.
/// - `Timeout` is refined into `Transport` here when the underlying message
///   looks like a network failure (connection refused, DNS, TLS) so the
///   user gets actionable network guidance vs. generic "timed out".
/// - `PayloadTooLarge` rolls into `ContextOverflow` for messaging — both
///   mean "the request was too big," and the recovery advice is identical.
pub fn classify_error(raw: &str) -> ErrorCategory {
    use crate::llm::llm_error::{classify_error_text, FailoverReason};

    match classify_error_text(raw) {
        Some(FailoverReason::RateLimit) => ErrorCategory::RateLimit,
        Some(FailoverReason::Billing) => ErrorCategory::Billing,
        Some(FailoverReason::Auth) | Some(FailoverReason::AuthPermanent) => ErrorCategory::Auth,
        Some(FailoverReason::Overloaded) => ErrorCategory::Overloaded,
        Some(FailoverReason::ContextOverflow) | Some(FailoverReason::PayloadTooLarge) => {
            ErrorCategory::ContextOverflow
        }
        Some(FailoverReason::Timeout) => {
            // UI refinement: distinguish "couldn't reach you" from
            // "took too long" so the user-facing message is actionable.
            // Both map to FailoverReason::Timeout for retry purposes.
            if extract_transport_detail(raw).is_some() {
                ErrorCategory::Transport
            } else {
                ErrorCategory::Timeout
            }
        }
        Some(FailoverReason::ModelNotFound)
        | Some(FailoverReason::Format)
        | Some(FailoverReason::Unknown)
        | None => ErrorCategory::Unknown,
    }
}

/// Strip HTML content from error messages (e.g., Cloudflare error pages).
///
/// Returns the raw string if it doesn't look like HTML.
fn strip_html(raw: &str) -> &str {
    if raw.contains("<!DOCTYPE") || raw.contains("<html") || raw.contains("<HTML") {
        // Don't send HTML to users — use a generic indicator
        return "(HTML error page)";
    }
    raw
}

/// Format a friendly, user-facing error message from a raw error string.
///
/// The returned message is safe to send to external messaging channels.
/// It never leaks raw JSON, HTML, or internal error details.
pub fn format_friendly_error(raw: &str) -> String {
    let category = classify_error(raw);
    let _cleaned = strip_html(raw);

    match category {
        ErrorCategory::RateLimit => {
            let hint = extract_retry_hint(raw);
            let base = match hint {
                Some(h) => format!(
                    "The AI provider is temporarily rate-limited. {h} Please try again shortly."
                ),
                None => {
                    "The AI provider is temporarily rate-limited. Please try again in a moment."
                        .to_string()
                }
            };
            match extract_openrouter_byok_hint(raw) {
                Some(extra) => format!("{base} {extra}"),
                None => base,
            }
        }

        ErrorCategory::Billing => {
            let provider = extract_provider_name(raw);
            match provider {
                Some(p) => format!(
                    "The {p} API key has run out of credits or exceeded its quota. \
                     Please check the billing dashboard and top up."
                ),
                None => "The AI provider API key has run out of credits or exceeded its quota. \
                         Please check the billing dashboard."
                    .to_string(),
            }
        }

        ErrorCategory::Auth => {
            let provider = extract_provider_name(raw);
            match provider {
                Some(p) => format!(
                    "Authentication with {p} failed. The API key may be invalid or expired. \
                     Please check your configuration."
                ),
                None => "Authentication with the AI provider failed. The API key may be invalid \
                         or expired. Please check your configuration."
                    .to_string(),
            }
        }

        ErrorCategory::Overloaded => "The AI provider is currently experiencing high load. \
             The request will be retried automatically, or you can try again in a moment."
            .to_string(),

        ErrorCategory::Transport => {
            let detail = extract_transport_detail(raw);
            match detail {
                Some(d) => format!(
                    "Could not reach the AI provider: {d}. \
                     Please check network connectivity."
                ),
                None => "Could not reach the AI provider. Please check network connectivity."
                    .to_string(),
            }
        }

        ErrorCategory::ContextOverflow => {
            "The conversation has exceeded the model's context window. \
             Try starting a new conversation or clearing history."
                .to_string()
        }

        ErrorCategory::Timeout => {
            "The request to the AI provider timed out. Please try again.".to_string()
        }

        ErrorCategory::Unknown => {
            // For unknown errors, give a safe generic message
            // Truncate to avoid leaking huge error blobs
            let safe = strip_html(raw);
            let truncated = if safe.len() > 200 {
                let mut end = 200;
                while end > 0 && !safe.is_char_boundary(end) {
                    end -= 1;
                }
                format!("An unexpected error occurred: {}...", &safe[..end])
            } else {
                format!("An unexpected error occurred: {safe}")
            };
            truncated
        }
    }
}

/// Table-driven action hints: `(category, context) → hint text`.
///
/// To add a new hint, append a row. To add a new context, add an [`ErrorContext`]
/// variant and corresponding rows here.
struct ActionHint {
    category: ErrorCategory,
    context: ErrorContext,
    hint: &'static str,
}

const ACTION_HINTS: &[ActionHint] = &[
    ActionHint {
        category: ErrorCategory::RateLimit,
        context: ErrorContext::Tui,
        hint: "Use /model to switch models.",
    },
    ActionHint {
        category: ErrorCategory::RateLimit,
        context: ErrorContext::Gateway,
        hint: "If this persists, try switching to a different model.",
    },
    ActionHint {
        category: ErrorCategory::Auth,
        context: ErrorContext::Tui,
        hint: "Update your API key via /model or /settings → API key.",
    },
    ActionHint {
        category: ErrorCategory::Billing,
        context: ErrorContext::Tui,
        hint: "Check billing or switch providers in /model or /settings.",
    },
];

/// Format a friendly error with context-specific actionable hints.
///
/// Builds on [`format_friendly_error`] and appends a hint from `ACTION_HINTS`
/// when one matches the `(category, context)` pair.
pub fn format_error_with_context(raw: &str, context: ErrorContext) -> String {
    let base = format_friendly_error(raw);
    let category = classify_error(raw);

    let hint = ACTION_HINTS
        .iter()
        .find(|h| h.category == category && h.context == context)
        .map(|h| h.hint);

    match hint {
        Some(h) => format!("{base} {h}"),
        None => base,
    }
}

/// Try to extract a retry-after hint from an error message.
fn extract_retry_hint(raw: &str) -> Option<String> {
    let lower = raw.to_lowercase();

    // Look for "retry after X seconds" patterns
    for pattern in &["retry after", "retry_after", "retry in"] {
        if let Some(pos) = lower.find(pattern) {
            let after = &raw[pos..];
            // Extract a reasonable snippet (up to 60 chars)
            let snippet: String = after.chars().take(60).collect();
            // Only use if it contains a number (likely a time indicator)
            if snippet.chars().any(|c| c.is_ascii_digit()) {
                return Some(format!("({})", snippet.trim()));
            }
        }
    }

    None
}

/// Try to extract the provider name from a raw error message.
///
/// `classify_status` always formats errors as `"{provider} returned {status}..."`
/// where `{provider}` is `Provider::as_str()` (lowercase). We parse the prefix
/// to get an exact match rather than scanning the entire body for substrings.
fn extract_provider_name(raw: &str) -> Option<&'static str> {
    // Provider names as produced by Provider::as_str() → display name
    const PROVIDERS: &[(&str, &str)] = &[
        ("anthropic", "Anthropic"),
        ("openrouter", "OpenRouter"),
        ("openai", "OpenAI"),
        ("gemini", "Gemini"),
        ("deepseek", "DeepSeek"),
        ("groq", "Groq"),
        ("xai", "xAI"),
        ("ollama", "Ollama"),
        ("claude-cli", "Claude CLI"),
    ];

    let prefix = raw.split_once(' ').map(|(w, _)| w).unwrap_or(raw);
    let lower = prefix.to_lowercase();
    for &(key, display) in PROVIDERS {
        if lower == key {
            return Some(display);
        }
    }
    None
}

/// Extract OpenRouter inference-provider rate-limit guidance.
///
/// When OpenRouter routes a request to an upstream inference provider (e.g.
/// DeepInfra, Fireworks) that is rate-limited and the user hasn't added their
/// own provider key (`is_byok=false`), the 429 metadata names the provider
/// and points to OpenRouter Settings → Integrations. Surface that as an
/// actionable second sentence — the right fix isn't always `/model`, it's
/// adding the user's own key or restricting the model's allowed providers.
fn extract_openrouter_byok_hint(raw: &str) -> Option<String> {
    let prefix = raw.split_once(' ').map(|(w, _)| w).unwrap_or("");
    if !prefix.eq_ignore_ascii_case("openrouter") {
        return None;
    }
    if !raw.contains(r#""is_byok":false"#) {
        return None;
    }
    let provider = raw
        .split(r#""provider_name":""#)
        .nth(1)
        .and_then(|s| s.split('"').next())
        .filter(|s| !s.is_empty());
    let lead = match provider {
        Some(p) => format!("The upstream inference provider ({p}) is rate-limited."),
        None => "The upstream inference provider is rate-limited.".to_string(),
    };
    Some(format!(
        "{lead} Add your own provider key in OpenRouter → Settings → Integrations \
         to use your own quota, or restrict the model to providers you've enabled."
    ))
}

/// Extract a user-friendly transport error detail.
fn extract_transport_detail(raw: &str) -> Option<&str> {
    let lower = raw.to_lowercase();
    if lower.contains("connection refused") {
        Some("connection refused by the provider endpoint")
    } else if lower.contains("connection reset") {
        Some("network connection was interrupted")
    } else if lower.contains("dns") {
        Some("DNS lookup for the provider endpoint failed")
    } else if lower.contains("network unreachable") || lower.contains("no route to host") {
        Some("the provider endpoint is unreachable from this host")
    } else if lower.contains("tls") || lower.contains("ssl") || lower.contains("certificate") {
        Some("TLS/SSL connection failed")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Table-driven classifier check. Each row pins a real provider/transport
    /// message against the category the user-facing layer relies on for
    /// branching (retry vs. surface vs. abort). Collapsed from 18 near-
    /// duplicate per-pattern tests.
    #[test]
    fn classify_error_dispatches_to_correct_category() {
        let cases: &[(&str, ErrorCategory)] = &[
            // RateLimit
            (
                "OpenRouter returned 429 (rate limited): too many requests",
                ErrorCategory::RateLimit,
            ),
            (
                "Rate limit exceeded for this model",
                ErrorCategory::RateLimit,
            ),
            // Billing
            (
                "OpenAI returned 402 (billing error): payment required",
                ErrorCategory::Billing,
            ),
            (
                "You exceeded your current quota, please check your plan",
                ErrorCategory::Billing,
            ),
            ("Error: insufficient_quota", ErrorCategory::Billing),
            // Auth
            (
                "Anthropic returned 401 (auth error): bad key",
                ErrorCategory::Auth,
            ),
            ("returned 403 (auth error): forbidden", ErrorCategory::Auth),
            ("Invalid API key provided", ErrorCategory::Auth),
            // Overloaded (5xx)
            (
                "Anthropic returned 503 (overloaded)",
                ErrorCategory::Overloaded,
            ),
            ("returned 502: bad gateway", ErrorCategory::Overloaded),
            (
                "returned 500: internal server error",
                ErrorCategory::Overloaded,
            ),
            // Transport
            (
                "error sending request: connection refused",
                ErrorCategory::Transport,
            ),
            (
                "DNS lookup failed for api.openai.com",
                ErrorCategory::Transport,
            ),
            (
                "TLS handshake failed: certificate verify failed",
                ErrorCategory::Transport,
            ),
            // ContextOverflow
            (
                "This request exceeds the model's maximum context length",
                ErrorCategory::ContextOverflow,
            ),
            (
                "prompt is too long: 150000 tokens > 128000 maximum",
                ErrorCategory::ContextOverflow,
            ),
            // Timeout
            ("request timed out after 30s", ErrorCategory::Timeout),
            (
                "deadline exceeded waiting for response",
                ErrorCategory::Timeout,
            ),
            // Unknown (fallthrough)
            (
                "something completely unexpected happened",
                ErrorCategory::Unknown,
            ),
        ];
        for (input, expected) in cases {
            let actual = classify_error(input);
            assert_eq!(
                actual, *expected,
                "classify_error({input:?}) = {actual:?}, expected {expected:?}"
            );
        }
    }

    // ── format_friendly_error tests ──

    #[test]
    fn friendly_rate_limit() {
        let msg =
            format_friendly_error("OpenRouter returned 429 (rate limited): too many requests");
        assert!(msg.contains("rate-limited"));
        assert!(msg.contains("try again"));
        assert!(!msg.contains("429"));
    }

    #[test]
    fn friendly_rate_limit_with_retry_hint() {
        let msg = format_friendly_error(
            "OpenAI returned 429: rate limited, retry after 30 seconds please",
        );
        assert!(msg.contains("rate-limited"));
        assert!(msg.contains("retry after"));
    }

    #[test]
    fn friendly_billing() {
        let msg =
            format_friendly_error("OpenAI returned 402 (billing error): insufficient credits");
        assert!(msg.contains("OpenAI"));
        assert!(msg.contains("credits"));
        assert!(msg.contains("billing"));
        assert!(!msg.contains("402"));
    }

    #[test]
    fn friendly_billing_no_provider() {
        let msg = format_friendly_error("returned 402: payment required");
        assert!(msg.contains("credits"));
        assert!(!msg.contains("402"));
    }

    #[test]
    fn friendly_auth() {
        let msg = format_friendly_error("Anthropic returned 401 (auth error): invalid key");
        assert!(msg.contains("Anthropic"));
        assert!(msg.contains("API key"));
        assert!(!msg.contains("401"));
    }

    #[test]
    fn friendly_overloaded() {
        let msg = format_friendly_error("returned 503: service unavailable, server overloaded");
        assert!(msg.contains("high load"));
        assert!(!msg.contains("503"));
    }

    #[test]
    fn friendly_transport_connection_refused() {
        let msg = format_friendly_error("error: connection refused to api.openai.com:443");
        assert!(msg.contains("connection refused"));
        assert!(msg.contains("network"));
    }

    #[test]
    fn friendly_transport_dns() {
        let msg = format_friendly_error("DNS lookup failed for api.anthropic.com");
        assert!(msg.contains("DNS"));
        assert!(msg.contains("network"));
    }

    #[test]
    fn friendly_context_overflow() {
        let msg = format_friendly_error("maximum context length exceeded: 200000 > 128000 tokens");
        assert!(msg.contains("context window"));
        assert!(msg.contains("new conversation"));
    }

    #[test]
    fn friendly_timeout() {
        let msg = format_friendly_error("request timed out after 120s");
        assert!(msg.contains("timed out"));
    }

    #[test]
    fn friendly_unknown_truncates_long_errors() {
        let long_error = "x".repeat(500);
        let msg = format_friendly_error(&long_error);
        assert!(msg.len() < 300);
        assert!(msg.ends_with("..."));
    }

    #[test]
    fn friendly_strips_html() {
        let html_error = "<!DOCTYPE html><html><body><h1>502 Bad Gateway</h1></body></html>";
        let msg = format_friendly_error(html_error);
        assert!(!msg.contains("<html"));
        assert!(!msg.contains("DOCTYPE"));
    }

    // ── strip_html tests ──

    #[test]
    fn strip_html_detects_doctype() {
        assert_eq!(
            strip_html("<!DOCTYPE html><html><body>error</body></html>"),
            "(HTML error page)"
        );
    }

    #[test]
    fn strip_html_passes_through_non_html() {
        assert_eq!(strip_html("just a normal error"), "just a normal error");
    }

    #[test]
    fn strip_html_detects_html_tag() {
        assert_eq!(
            strip_html("<html><head></head><body>Cloudflare</body></html>"),
            "(HTML error page)"
        );
    }

    // ── extract_provider_name tests ──

    #[test]
    fn extract_provider_anthropic() {
        assert_eq!(
            extract_provider_name("Anthropic returned 401"),
            Some("Anthropic")
        );
    }

    #[test]
    fn extract_provider_openai() {
        assert_eq!(extract_provider_name("OpenAI returned 429"), Some("OpenAI"));
    }

    #[test]
    fn extract_provider_openrouter() {
        assert_eq!(
            extract_provider_name("OpenRouter returned 503"),
            Some("OpenRouter")
        );
    }

    #[test]
    fn extract_provider_gemini() {
        assert_eq!(extract_provider_name("Gemini API error"), Some("Gemini"));
    }

    #[test]
    fn extract_provider_deepseek() {
        assert_eq!(
            extract_provider_name("DeepSeek rate limited"),
            Some("DeepSeek")
        );
    }

    #[test]
    fn extract_provider_groq() {
        assert_eq!(extract_provider_name("Groq error"), Some("Groq"));
    }

    #[test]
    fn extract_provider_ollama() {
        assert_eq!(
            extract_provider_name("Ollama connection failed"),
            Some("Ollama")
        );
    }

    #[test]
    fn extract_provider_unknown() {
        assert_eq!(extract_provider_name("some error"), None);
    }

    // ── extract_transport_detail tests ──

    #[test]
    fn transport_detail_connection_refused() {
        assert_eq!(
            extract_transport_detail("connection refused"),
            Some("connection refused by the provider endpoint")
        );
    }

    #[test]
    fn transport_detail_connection_reset() {
        assert_eq!(
            extract_transport_detail("connection reset by peer"),
            Some("network connection was interrupted")
        );
    }

    #[test]
    fn transport_detail_dns() {
        assert_eq!(
            extract_transport_detail("DNS resolution failed"),
            Some("DNS lookup for the provider endpoint failed")
        );
    }

    #[test]
    fn transport_detail_unreachable() {
        assert_eq!(
            extract_transport_detail("network unreachable"),
            Some("the provider endpoint is unreachable from this host")
        );
    }

    #[test]
    fn transport_detail_tls() {
        assert_eq!(
            extract_transport_detail("TLS handshake error"),
            Some("TLS/SSL connection failed")
        );
    }

    #[test]
    fn transport_detail_unknown() {
        assert_eq!(extract_transport_detail("broken pipe"), None);
    }

    // ── ErrorCategory display tests ──

    #[test]
    fn error_category_display() {
        assert_eq!(ErrorCategory::RateLimit.to_string(), "rate_limit");
        assert_eq!(ErrorCategory::Billing.to_string(), "billing");
        assert_eq!(ErrorCategory::Auth.to_string(), "auth");
        assert_eq!(ErrorCategory::Overloaded.to_string(), "overloaded");
        assert_eq!(ErrorCategory::Transport.to_string(), "transport");
        assert_eq!(
            ErrorCategory::ContextOverflow.to_string(),
            "context_overflow"
        );
        assert_eq!(ErrorCategory::Timeout.to_string(), "timeout");
        assert_eq!(ErrorCategory::Unknown.to_string(), "unknown");
    }

    // ── Edge cases ──

    #[test]
    fn empty_error_classifies_as_unknown() {
        assert_eq!(classify_error(""), ErrorCategory::Unknown);
    }

    #[test]
    fn friendly_empty_error() {
        let msg = format_friendly_error("");
        assert!(msg.contains("unexpected error"));
    }

    #[test]
    fn classify_case_insensitive() {
        assert_eq!(
            classify_error("RATE LIMIT EXCEEDED"),
            ErrorCategory::RateLimit
        );
        assert_eq!(
            classify_error("Connection Refused"),
            ErrorCategory::Transport
        );
    }

    #[test]
    fn context_overflow_not_confused_with_rate_limit_tpm() {
        // "tokens per minute" is a rate limit, not context overflow
        assert_eq!(
            classify_error("429: Rate limit reached. Limit: 100000 tokens per minute"),
            ErrorCategory::RateLimit
        );
    }

    #[test]
    fn extract_retry_hint_with_seconds() {
        let hint = extract_retry_hint("Rate limit: retry after 30 seconds").expect("hint parsed");
        assert!(hint.contains("30"));
    }

    #[test]
    fn extract_retry_hint_none_when_no_number() {
        let hint = extract_retry_hint("please wait a moment");
        assert!(hint.is_none());
    }

    // ── format_error_with_context tests ──

    #[test]
    fn context_tui_rate_limit_suggests_model_switch() {
        let msg = format_error_with_context(
            "openrouter returned 429 (rate limited): too many requests",
            ErrorContext::Tui,
        );
        assert!(msg.contains("/model"));
        assert!(msg.contains("switch models"));
    }

    #[test]
    fn context_gateway_rate_limit_suggests_different_model() {
        let msg = format_error_with_context(
            "openrouter returned 429 (rate limited): too many requests",
            ErrorContext::Gateway,
        );
        assert!(msg.contains("switching to a different model"));
        assert!(!msg.contains("/settings"));
        assert!(!msg.contains("/model"));
    }

    #[test]
    fn context_tui_auth_suggests_key_edit() {
        let msg = format_error_with_context(
            "Anthropic returned 401 (auth error): invalid key",
            ErrorContext::Tui,
        );
        assert!(msg.contains("/model") || msg.contains("/settings"));
        assert!(msg.contains("API key"));
    }

    #[test]
    fn context_tui_billing_suggests_provider_switch() {
        let msg =
            format_error_with_context("OpenAI returned 402: payment required", ErrorContext::Tui);
        assert!(msg.contains("/model") || msg.contains("/settings"));
    }

    #[test]
    fn context_no_hint_for_timeout() {
        let base = format_friendly_error("request timed out after 30s");
        let with_ctx = format_error_with_context("request timed out after 30s", ErrorContext::Tui);
        assert_eq!(base, with_ctx);
    }

    #[test]
    fn context_gateway_no_hint_for_auth() {
        let base = format_friendly_error("Anthropic returned 401: invalid key");
        let with_ctx =
            format_error_with_context("Anthropic returned 401: invalid key", ErrorContext::Gateway);
        assert_eq!(base, with_ctx);
    }

    #[test]
    fn context_real_openrouter_429_error() {
        let raw = r#"openrouter returned 429 (rate limited): {"error":{"message":"Provider returned error","code":429,"metadata":{"raw":"moonshotai/kimi-k2.5 is temporarily rate-limited upstream. Please retry shortly, or add your own key to accumulate your rate limits: https://openrouter.ai/settings/integrations","provider_name":"DeepInfra","is_byok":false}}}"#;

        let tui_msg = format_error_with_context(raw, ErrorContext::Tui);
        assert!(tui_msg.contains("rate-limited"));
        assert!(tui_msg.contains("/model"));
        assert!(!tui_msg.contains(r#"{"error"#));
        // BYOK guidance: name the upstream provider and point to OpenRouter
        // Integrations so users learn the real fix.
        assert!(
            tui_msg.contains("DeepInfra"),
            "expected upstream provider name in message, got: {tui_msg}"
        );
        assert!(
            tui_msg.contains("OpenRouter") && tui_msg.contains("Integrations"),
            "expected OpenRouter Integrations guidance, got: {tui_msg}"
        );

        let gw_msg = format_error_with_context(raw, ErrorContext::Gateway);
        assert!(gw_msg.contains("rate-limited"));
        assert!(gw_msg.contains("switching to a different model"));
        assert!(!gw_msg.contains(r#"{"error"#));
        assert!(gw_msg.contains("DeepInfra"));
    }

    #[test]
    fn openrouter_byok_hint_skipped_when_byok_true() {
        // If the user has their own provider key, the rate limit is theirs to
        // own — don't tell them to add a key they already added.
        let raw = r#"openrouter returned 429 (rate limited): {"error":{"metadata":{"provider_name":"DeepInfra","is_byok":true}}}"#;
        let msg = format_error_with_context(raw, ErrorContext::Tui);
        assert!(!msg.contains("Integrations"), "got: {msg}");
        assert!(!msg.contains("DeepInfra"), "got: {msg}");
    }

    #[test]
    fn openrouter_byok_hint_skipped_for_other_providers() {
        // Only OpenRouter has the upstream-provider-routing concept.
        let raw = r#"anthropic returned 429: {"error":{"metadata":{"provider_name":"DeepInfra","is_byok":false}}}"#;
        let msg = format_error_with_context(raw, ErrorContext::Tui);
        assert!(!msg.contains("OpenRouter"), "got: {msg}");
    }
}
