//! Trivial-prompt classifier for memory prefetch gating.
//!
//! `load_memory_with_ranking` and `build_active_memory_section` both run on
//! every turn, keyed off the latest user message. A greeting like "hey how
//! are you doing" used to clear the old `len < 10` gate and trigger a full
//! embedding round-trip plus a hybrid memory search — pure overhead, since a
//! pleasantry ranks against nothing. This classifier is the shared gate.
//!
//! Deliberately conservative: a false "trivial" hides real recall, so
//! anything not confidently a greeting/ack — a question mark that survives
//! phrase matching, an unknown word, more than six words — is NOT trivial.
//! Heuristic only; no LLM call, no allocation beyond one normalize pass.

/// Whole-phrase matches (normalized: lowercased, punctuation stripped,
/// whitespace collapsed). Greetings and pleasantries that read as multi-word
/// sentences but carry no recall intent.
const TRIVIAL_PHRASES: &[&str] = &[
    "how are you",
    "how are you doing",
    "how are you doing today",
    "how are you today",
    "hows it going",
    "how is it going",
    "whats up",
    "whats good",
    "whats new",
    "hey how are you",
    "hi how are you",
    "hello how are you",
    "hey how are you doing",
    "hey how are you doing today",
    "hey hows it going",
    "hi hows it going",
    "good morning how are you",
    "long time no see",
    "sounds good to me",
    "thank you very much",
    "thank you so much",
    "thanks a lot",
    "thanks so much",
];

/// Single words that are trivial on their own or in any combination
/// ("ok thanks", "yes please", "good morning"). A phrase qualifies only if
/// EVERY token is in this set.
const TRIVIAL_WORDS: &[&str] = &[
    "hi",
    "hey",
    "hello",
    "yo",
    "sup",
    "howdy",
    "hiya",
    "morning",
    "afternoon",
    "evening",
    "night",
    "good",
    "gm",
    "gn",
    "ok",
    "okay",
    "k",
    "kk",
    "cool",
    "nice",
    "great",
    "awesome",
    "perfect",
    "sweet",
    "yes",
    "yeah",
    "yep",
    "yup",
    "no",
    "nope",
    "nah",
    "sure",
    "thanks",
    "thank",
    "you",
    "ty",
    "thx",
    "np",
    "welcome",
    "please",
    "pls",
    "lol",
    "haha",
    "hehe",
    "wow",
    "bye",
    "goodbye",
    "later",
    "cya",
    "goodnight",
];

/// Word-count ceiling: past this, the message is doing real work no matter
/// what its tokens look like.
const MAX_TRIVIAL_WORDS: usize = 6;

/// Minimum length for semantic ranking to be worthwhile — anything shorter
/// is trivially trivial ("ok", "next", "yes"). Kept from the pre-classifier
/// gate so behavior below the old threshold is unchanged.
const MIN_QUERY_CHARS_FOR_RANKING: usize = 10;

/// Whether a user prompt is a greeting/ack that memory prefetch should skip.
pub fn is_trivial_prompt(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.len() < MIN_QUERY_CHARS_FOR_RANKING {
        return true;
    }
    let normalized = normalize(trimmed);
    if normalized.is_empty() {
        return true;
    }
    let words: Vec<&str> = normalized.split(' ').collect();
    if words.len() > MAX_TRIVIAL_WORDS {
        return false;
    }
    if TRIVIAL_PHRASES.contains(&normalized.as_str()) {
        return true;
    }
    // A '?' that survives phrase matching signals a real question
    // ("hi, can you check my calendar?" but also "what did we decide?").
    if trimmed.contains('?') {
        return false;
    }
    words.iter().all(|w| TRIVIAL_WORDS.contains(w))
}

/// Lowercase, strip everything but letters/digits/spaces (apostrophes are
/// dropped so "what's" and "whats" normalize identically), collapse runs of
/// whitespace to one space.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            last_space = false;
        } else if c.is_whitespace() && !last_space {
            out.push(' ');
            last_space = true;
        }
        // Everything else (punctuation, emoji, apostrophes) is dropped.
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifier_verdicts() {
        // (prompt, trivial)
        let cases: &[(&str, bool)] = &[
            // Short acks: unchanged from the old len<10 gate.
            ("ok", true),
            ("yes", true),
            ("next", true),
            ("", true),
            // Greetings that cleared the old gate on length alone.
            ("hey how are you doing", true),
            ("hey how are you doing today", true),
            ("good morning!", true),
            ("hello there!!", false), // "there" is not a trivial word
            ("how's it going?", true),
            ("what's up", true),
            ("thanks so much!", true),
            ("ok thanks, sounds good", false), // "sounds"/"good" mix: "sounds" not in word set
            ("yes please", true),
            ("thank you very much", true),
            ("lol nice", true),
            // Real prompts that must never be gated.
            ("hi, can you check my calendar?", false),
            ("what did we decide about the deploy?", false),
            ("remind me what pavel said", false),
            ("good morning, any updates on the build?", false),
            ("how are you planning to fix the bug", false),
            ("thanks — now update the readme", false),
            ("where did I put the api key", false),
            // Over the word cap, even if every token looks trivial.
            ("ok ok ok ok ok ok ok", false),
        ];
        for (prompt, trivial) in cases {
            assert_eq!(
                is_trivial_prompt(prompt),
                *trivial,
                "verdict mismatch for {prompt:?}: expected trivial={trivial}"
            );
        }
    }

    #[test]
    fn normalize_collapses_punctuation_and_case() {
        assert_eq!(normalize("Hey!!   How's it GOING???"), "hey hows it going");
        assert_eq!(normalize("  ok. "), "ok");
    }
}
