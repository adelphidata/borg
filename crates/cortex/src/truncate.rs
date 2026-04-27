//! Utilities for truncating large tool outputs while preserving a prefix
//! and suffix on UTF-8 boundaries.
//!
//! Keeps the beginning and end of output so the LLM can see both the start
//! context and the final result.

use crate::constants;

const APPROX_BYTES_PER_TOKEN: usize = constants::APPROX_BYTES_PER_TOKEN;

/// Truncate output to fit within a token budget, preserving head and tail.
///
/// If the output fits within the budget, returns it unchanged. Otherwise,
/// splits the budget 50/50 between prefix and suffix, inserting a marker
/// showing how much was omitted.
pub fn truncate_output(text: &str, max_tokens: usize) -> String {
    if max_tokens == 0 {
        let total_lines = text.lines().count();
        return format!("[output truncated — {total_lines} lines omitted]");
    }

    let max_bytes = max_tokens.saturating_mul(APPROX_BYTES_PER_TOKEN);

    if text.len() <= max_bytes {
        return text.to_string();
    }

    let total_lines = text.lines().count();
    let left_budget = max_bytes / 2;
    let right_budget = max_bytes - left_budget;

    // Find UTF-8 safe split points
    let prefix_end = floor_char_boundary(text, left_budget);
    let suffix_start = ceil_char_boundary(text, text.len().saturating_sub(right_budget));

    // Ensure suffix doesn't overlap prefix
    let suffix_start = suffix_start.max(prefix_end);

    let omitted_bytes = suffix_start - prefix_end;
    let omitted_tokens =
        omitted_bytes.saturating_add(APPROX_BYTES_PER_TOKEN - 1) / APPROX_BYTES_PER_TOKEN;

    let prefix = &text[..prefix_end];
    let suffix = &text[suffix_start..];

    format!(
        "{prefix}\n\n…[{omitted_tokens} tokens truncated, {total_lines} total lines]…\n\n{suffix}"
    )
}

/// Estimate token count from text length.
pub fn approx_token_count(text: &str) -> usize {
    text.len().saturating_add(APPROX_BYTES_PER_TOKEN - 1) / APPROX_BYTES_PER_TOKEN
}

/// Find the largest byte index <= `index` that is a char boundary.
fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Find the smallest byte index >= `index` that is a char boundary.
fn ceil_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_truncation_when_under_budget() {
        let text = "hello world"; // 11 bytes = ~3 tokens
        let result = truncate_output(text, 100);
        assert_eq!(result, text);
    }

    #[test]
    fn truncates_large_output() {
        let text = "a".repeat(1000); // 1000 bytes = ~250 tokens
        let result = truncate_output(&text, 50); // 50 tokens = 200 bytes budget
        assert!(result.len() < text.len());
        assert!(result.contains("truncated"));
        // Should start with 'a's (prefix preserved)
        assert!(result.starts_with("aaa"));
        // Should end with 'a's (suffix preserved)
        assert!(result.ends_with("aaa"));
    }

    #[test]
    fn zero_budget_shows_omission() {
        let text = "line1\nline2\nline3\n";
        let result = truncate_output(text, 0);
        assert!(result.contains("omitted"));
    }

    #[test]
    fn handles_utf8_correctly() {
        // Multi-byte characters shouldn't be split
        let text = "aaaa🎉🎉🎉🎉bbbb".repeat(50);
        let result = truncate_output(&text, 20);
        // Should be valid UTF-8 (this would panic if we split a char)
        assert!(result.is_char_boundary(0));
        assert!(result.contains("truncated"));
    }

    #[test]
    fn approx_token_count_basic() {
        assert_eq!(approx_token_count(""), 0);
        assert_eq!(approx_token_count("abcd"), 1);
        assert_eq!(approx_token_count("ab"), 1); // rounds up
        let text = "a".repeat(400);
        assert_eq!(approx_token_count(&text), 100);
    }

    #[test]
    fn preserves_head_and_tail() {
        let mut text = String::new();
        text.push_str("HEAD_MARKER ");
        text.push_str(&"x".repeat(800));
        text.push_str(" TAIL_MARKER");
        let result = truncate_output(&text, 50);
        assert!(result.contains("HEAD_MARKER"));
        assert!(result.contains("TAIL_MARKER"));
    }

    #[test]
    fn empty_input() {
        assert_eq!(truncate_output("", 100), "");
    }

    #[test]
    fn exact_budget_no_truncation() {
        let text = "a".repeat(400); // 400 bytes = 100 tokens exactly
        let result = truncate_output(&text, 100);
        assert_eq!(result, text);
    }
}
