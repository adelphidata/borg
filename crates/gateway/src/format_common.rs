//! Shared helpers for converting the agent's markdown into channel-specific
//! formatting.
//!
//! Several channels strip or rewrite the same markdown constructs before
//! handing text to their platform API. Genuinely-identical helpers live here so
//! the channel `format` modules can share one implementation rather than drift.

/// Strip a markdown header prefix (`#` through `######`, optionally indented)
/// and return the header text, or `None` when the line is not a header.
pub fn strip_header(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    // Try deepest prefix first so `### ` doesn't match inside `###### `.
    for prefix in ["###### ", "##### ", "#### ", "### ", "## ", "# "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some(rest);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_each_header_depth() {
        assert_eq!(strip_header("# H1"), Some("H1"));
        assert_eq!(strip_header("###### H6"), Some("H6"));
        // Indented headers are still recognized.
        assert_eq!(strip_header("   ## Indented"), Some("Indented"));
    }

    #[test]
    fn deeper_prefix_wins_over_shallower() {
        // `###### text` must yield `text`, not `### text` (which would happen
        // if a shallower prefix were tried first).
        assert_eq!(strip_header("###### text"), Some("text"));
    }

    #[test]
    fn non_headers_return_none() {
        assert_eq!(strip_header("plain"), None);
        // No space after the hashes is not a header.
        assert_eq!(strip_header("#nospace"), None);
        // A bare hash run without trailing space+text is not a header.
        assert_eq!(strip_header("###"), None);
    }
}
