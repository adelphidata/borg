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
        let total_tokens = approx_token_count(text);
        return format!("[output truncated — {total_tokens} tokens / {total_lines} lines omitted]");
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
    let total_tokens = approx_token_count(text);

    let prefix = &text[..prefix_end];
    let suffix = &text[suffix_start..];

    format!(
        "{prefix}\n\n…[head+tail of {total_tokens} tokens / {total_lines} total lines; {omitted_tokens} tokens omitted]…\n\n{suffix}"
    )
}

/// Estimate token count from text length.
pub fn approx_token_count(text: &str) -> usize {
    text.len().saturating_add(APPROX_BYTES_PER_TOKEN - 1) / APPROX_BYTES_PER_TOKEN
}

/// UTF-8-safe truncate that caps `s` at `max_chars` characters and appends
/// `ellipsis` when truncation occurs. When `s.chars().count() <= max_chars`,
/// returns `s` unchanged. The ellipsis is appended *after* the truncated prefix
/// (total result length = max_chars + ellipsis.chars().count() chars).
///
/// Use when call-site cares about the input's intrinsic length and wants
/// "first N chars + …" semantics. For width-bounded columnar output where
/// the total must not exceed a fixed width, use [`truncate_to_width`].
pub fn truncate_with_ellipsis(s: &str, max_chars: usize, ellipsis: &str) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let prefix: String = s.chars().take(max_chars).collect();
    format!("{prefix}{ellipsis}")
}

/// UTF-8-safe truncate that caps the total result at `width` characters,
/// reserving one character for the ellipsis `…` when truncation occurs.
pub fn truncate_to_width(s: &str, width: usize) -> String {
    truncate_to_width_with(s, width, "…")
}

/// Like [`truncate_to_width`] but with a caller-chosen ellipsis. Total result
/// length stays ≤ `width` chars (the ellipsis chars count toward the budget).
pub fn truncate_to_width_with(s: &str, width: usize, ellipsis: &str) -> String {
    if width == 0 {
        return String::new();
    }
    if s.chars().count() <= width {
        return s.to_string();
    }
    let elen = ellipsis.chars().count();
    let keep = width.saturating_sub(elen);
    let prefix: String = s.chars().take(keep).collect();
    format!("{prefix}{ellipsis}")
}

/// Like [`truncate_to_width`] but left-justify-pads with spaces when `s` is
/// shorter than `width`. Used for CLI columnar output.
pub fn pad_or_truncate(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count >= width {
        return truncate_to_width(s, width);
    }
    let pad = width - count;
    format!("{s}{}", " ".repeat(pad))
}

/// Find the largest byte index <= `index` that is a char boundary.
pub(crate) fn floor_char_boundary(s: &str, index: usize) -> usize {
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
        assert!(result.contains("omitted"));
        // Should start with 'a's (prefix preserved)
        assert!(result.starts_with("aaa"));
        // Should end with 'a's (suffix preserved)
        assert!(result.ends_with("aaa"));
    }

    #[test]
    fn discloses_pre_truncation_total_size() {
        // 1000 bytes ≈ 250 tokens before truncation. The marker must surface the
        // original total so the model knows how much it isn't seeing, not just
        // the omitted slice.
        let text = "a".repeat(1000);
        let result = truncate_output(&text, 50);
        assert!(
            result.contains(&format!("{} tokens", approx_token_count(&text))),
            "marker should disclose the original total token count: {result}"
        );
    }

    #[test]
    fn zero_budget_discloses_total_tokens() {
        let text = "a".repeat(1000);
        let result = truncate_output(&text, 0);
        assert!(result.contains("omitted"));
        assert!(
            result.contains(&format!("{} tokens", approx_token_count(&text))),
            "zero-budget marker should disclose the original total: {result}"
        );
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
        assert!(result.contains("omitted"));
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

    #[test]
    fn truncate_with_ellipsis_under_max_returns_unchanged() {
        assert_eq!(truncate_with_ellipsis("hello", 10, "…"), "hello");
    }

    #[test]
    fn truncate_with_ellipsis_appends_after_max_chars() {
        let result = truncate_with_ellipsis("hello world", 5, "…");
        assert_eq!(result, "hello…");
    }

    #[test]
    fn truncate_with_ellipsis_three_dot_variant() {
        let long = "a".repeat(100);
        let result = truncate_with_ellipsis(&long, 10, "...");
        assert!(result.ends_with("..."));
        assert_eq!(result.chars().count(), 13); // 10 + "..."
    }

    #[test]
    fn truncate_with_ellipsis_is_utf8_safe_on_multibyte_inputs() {
        let s = "hello\u{1F600}world"; // emoji at byte boundary 5..9
        let result = truncate_with_ellipsis(s, 6, "...");
        // chars 0..6 = "hello🙂", then "..."
        assert_eq!(result.chars().count(), 9);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_to_width_caps_total_chars() {
        assert_eq!(truncate_to_width("hello world", 5), "hell…");
        assert_eq!(truncate_to_width("hi", 5), "hi");
        assert_eq!(truncate_to_width("abc", 0), "");
    }

    #[test]
    fn pad_or_truncate_pads_short_inputs() {
        assert_eq!(pad_or_truncate("hi", 5), "hi   ");
    }

    #[test]
    fn pad_or_truncate_truncates_long_inputs() {
        assert_eq!(pad_or_truncate("hello world", 5), "hell…");
    }

    #[test]
    fn pad_or_truncate_handles_multibyte_without_panic() {
        // Pre-fix, byte-indexing into a string with multibyte chars at the
        // truncation boundary would panic. The chars-based path is safe.
        let s = "ab\u{1F600}cd"; // a, b, 🙂, c, d
        let result = pad_or_truncate(s, 4);
        assert_eq!(result.chars().count(), 4);
        assert!(result.ends_with('…'));
    }
}
