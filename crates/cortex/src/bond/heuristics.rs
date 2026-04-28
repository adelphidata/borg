//! Suggestion-acceptance heuristics. Acceptance patterns require stronger
//! signal than bare "yes" / "ok" to reduce false positives from
//! conversational filler. Multi-word phrases preferred.

use regex::Regex;
use std::sync::LazyLock;

#[allow(clippy::expect_used)]
static ACCEPTANCE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(do it|go ahead|sounds good|approved|let'?s do it|yes,? do|yes please|absolutely)\b",
    )
    .expect("compile-time literal regex")
});

#[allow(clippy::expect_used)]
static REJECTION_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(no don'?t|don'?t do|cancel that|never mind|skip that|wrong suggestion|stop suggesting|not helpful)\b"
    ).expect("compile-time literal regex")
});

/// Returns `true` if the user message heuristically looks like acceptance.
pub fn looks_like_acceptance(msg: &str) -> bool {
    ACCEPTANCE_PATTERN.is_match(msg)
}

/// Returns `true` if the user message heuristically looks like rejection.
pub fn looks_like_rejection(msg: &str) -> bool {
    REJECTION_PATTERN.is_match(msg)
}
