use std::sync::LazyLock;
use tiktoken_rs::{cl100k_base, CoreBPE};

static BPE: LazyLock<Option<CoreBPE>> = LazyLock::new(|| match cl100k_base() {
    Ok(bpe) => Some(bpe),
    Err(e) => {
        tracing::error!("Failed to load cl100k_base tokenizer: {e} — using byte heuristic");
        None
    }
});

/// Estimate token count using the cl100k_base BPE tokenizer.
///
/// cl100k_base is the encoding used by GPT-4, ChatGPT, and text-embedding-ada-002.
/// It provides a reasonable approximation for most LLMs available through OpenRouter,
/// including Claude models.
///
/// Falls back to a byte-count heuristic (~4 bytes per token) if the tokenizer
/// fails to initialize.
pub fn estimate_tokens(text: &str) -> usize {
    match BPE.as_ref() {
        Some(bpe) => bpe.encode_with_special_tokens(text).len(),
        None => text.len().div_ceil(4),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_returns_zero() {
        // Documented contract — the cap-enforcement path in
        // `enforce_memory_size` treats `estimate_tokens("") == 0` as "fits
        // in any budget".
        assert_eq!(estimate_tokens(""), 0);
    }

    /// Bounded sanity ranges for the BPE encoder. Ranges are deliberately
    /// wide — the goal is to detect a regression that swaps the encoder
    /// for the byte fallback (`len / 4`) or returns nonsense, not to
    /// fingerprint the exact cl100k tokenization (which is `tiktoken-rs`'
    /// job to test).
    #[test]
    fn estimate_within_expected_bounds_for_known_inputs() {
        let english = "The quick brown fox jumps over the lazy dog. ".repeat(10);
        let repeated = "hello ".repeat(100);
        // (input, min_tokens, max_tokens) — a swap to the byte-count
        // fallback (len/4) would fail at least one of these rows.
        let cases: &[(&str, usize, usize)] = &[
            ("Hello, world!", 1, 9),
            (&english, 50, 200),
            // Highly repetitive — BPE merges keep this near the word count
            // (~100), not the byte fallback's ~150.
            (&repeated, 80, 120),
        ];
        for (input, lo, hi) in cases {
            let n = estimate_tokens(input);
            assert!(
                (*lo..=*hi).contains(&n),
                "estimate_tokens(<input of len {}>) = {n}, expected in {}..={}",
                input.len(),
                lo,
                hi,
            );
        }
    }

    /// Production relies on a post-condition: non-empty input must
    /// tokenize to ≥ 1. The byte-fallback branch would otherwise return 0
    /// for very short inputs and the cap check would let arbitrarily
    /// large entries through under the "0 tokens fits any budget" branch.
    #[test]
    fn nonempty_input_always_tokenizes_to_at_least_one() {
        for input in ["a", "   ", "你好", "\n", "!@#"] {
            assert!(
                estimate_tokens(input) >= 1,
                "non-empty input {input:?} must produce ≥ 1 token, got 0"
            );
        }
    }
}
