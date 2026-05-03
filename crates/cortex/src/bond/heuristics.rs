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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acceptance_matches_documented_phrases() {
        for msg in [
            "do it",
            "go ahead",
            "sounds good",
            "approved",
            "let's do it",
            "lets do it", // apostrophe-free
            "yes please",
            "absolutely",
            "yes, do it",
            "DO IT", // case-insensitive
            "Please go ahead and ship",
            "ok, do it",
        ] {
            assert!(looks_like_acceptance(msg), "expected acceptance: {msg:?}");
        }
    }

    /// Invariant the heuristic exists to enforce: bare conversational filler
    /// ("yes", "ok") must NOT count as acceptance — that produces too many
    /// false positives during normal chit-chat.
    #[test]
    fn acceptance_rejects_bare_filler_words() {
        for msg in [
            "",
            "yes",
            "ok",
            "okay",
            "yeah",
            "yep",
            "sure",
            "doingit",     // no word boundary
            "approvedish", // substring, not whole word
            "hello world",
        ] {
            assert!(
                !looks_like_acceptance(msg),
                "filler should not match acceptance: {msg:?}"
            );
        }
    }

    #[test]
    fn rejection_matches_documented_phrases() {
        for msg in [
            "no don't",
            "no dont",
            "don't do that",
            "dont do that",
            "cancel that",
            "never mind",
            "skip that",
            "wrong suggestion",
            "stop suggesting that",
            "not helpful",
            "NEVER MIND", // case-insensitive
        ] {
            assert!(looks_like_rejection(msg), "expected rejection: {msg:?}");
        }
    }

    #[test]
    fn rejection_rejects_bare_negatives() {
        for msg in ["", "no", "nope", "not sure", "skip", "stop", "hello"] {
            assert!(
                !looks_like_rejection(msg),
                "bare negative should not match rejection: {msg:?}"
            );
        }
    }

    /// Acceptance and rejection regexes must not both fire on the same message
    /// — the bond hook checks acceptance first, but a true overlap would mean
    /// arbitrary order-dependent classification. Catches future regex edits
    /// that accidentally widen one to overlap the other.
    #[test]
    fn acceptance_and_rejection_are_mutually_exclusive() {
        let samples = [
            "do it",
            "go ahead",
            "sounds good",
            "approved",
            "let's do it",
            "yes please",
            "absolutely",
            "no don't",
            "don't do that",
            "cancel that",
            "never mind",
            "skip that",
            "wrong suggestion",
            "stop suggesting",
            "not helpful",
        ];
        for msg in samples {
            assert!(
                !(looks_like_acceptance(msg) && looks_like_rejection(msg)),
                "{msg:?} matched BOTH acceptance and rejection — heuristics overlap"
            );
        }
    }
}
