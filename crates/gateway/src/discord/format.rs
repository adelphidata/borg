/// Convert standard Markdown to Discord-compatible format.
///
/// Discord renders most standard Markdown natively (bold, italic, code,
/// links, block quotes, strikethrough). The one exception is ATX headers
/// (`# Header`) which Discord does NOT render in regular messages.
///
/// This function converts headers to bold text while preserving everything
/// else, including content inside fenced code blocks.
pub fn markdown_to_discord(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut in_code_block = false;

    for line in text.lines() {
        if !result.is_empty() {
            result.push('\n');
        }

        // Toggle fenced code blocks
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            result.push_str(line);
            continue;
        }

        // Don't convert inside code blocks
        if in_code_block {
            result.push_str(line);
            continue;
        }

        // Convert headers: `# Header` → `**Header**`
        if let Some(header_text) = strip_header(line) {
            result.push_str("**");
            result.push_str(header_text.trim());
            result.push_str("**");
            continue;
        }

        result.push_str(line);
    }

    result
}

/// Strip markdown header prefix (# through ######) and return the text.
fn strip_header(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("###### ") {
        Some(rest)
    } else if let Some(rest) = trimmed.strip_prefix("##### ") {
        Some(rest)
    } else if let Some(rest) = trimmed.strip_prefix("#### ") {
        Some(rest)
    } else if let Some(rest) = trimmed.strip_prefix("### ") {
        Some(rest)
    } else if let Some(rest) = trimmed.strip_prefix("## ") {
        Some(rest)
    } else if let Some(rest) = trimmed.strip_prefix("# ") {
        Some(rest)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_h1_to_bold() {
        assert_eq!(markdown_to_discord("# Title"), "**Title**");
    }

    #[test]
    fn header_h2_to_bold() {
        assert_eq!(markdown_to_discord("## Subtitle"), "**Subtitle**");
    }

    #[test]
    fn header_h3_to_bold() {
        assert_eq!(markdown_to_discord("### Section"), "**Section**");
    }

    #[test]
    fn header_h6_to_bold() {
        assert_eq!(markdown_to_discord("###### Deep"), "**Deep**");
    }

    #[test]
    fn code_block_headers_preserved() {
        let input = "```\n# Not a header\n## Also not\n```";
        assert_eq!(markdown_to_discord(input), input);
    }

    #[test]
    fn inline_code_preserved() {
        // Inline code with a hash is not a header
        assert_eq!(markdown_to_discord("`# not a header`"), "`# not a header`");
    }

    #[test]
    fn bold_preserved() {
        assert_eq!(markdown_to_discord("**bold**"), "**bold**");
    }

    #[test]
    fn link_preserved() {
        assert_eq!(
            markdown_to_discord("[click](https://example.com)"),
            "[click](https://example.com)"
        );
    }

    #[test]
    fn block_quote_preserved() {
        assert_eq!(markdown_to_discord("> quoted text"), "> quoted text");
    }

    #[test]
    fn plain_text_unchanged() {
        assert_eq!(markdown_to_discord("hello world"), "hello world");
    }

    #[test]
    fn empty_string() {
        assert_eq!(markdown_to_discord(""), "");
    }

    #[test]
    fn multiline_mixed() {
        let input = "# Title\nSome text\n```\n# code comment\n```\n## Another heading";
        let expected = "**Title**\nSome text\n```\n# code comment\n```\n**Another heading**";
        assert_eq!(markdown_to_discord(input), expected);
    }

    #[test]
    fn multiline_preserved() {
        let input = "line 1\nline 2\nline 3";
        assert_eq!(markdown_to_discord(input), input);
    }

    #[test]
    fn header_with_leading_spaces() {
        // Indented headers should still be converted
        assert_eq!(markdown_to_discord("  # Indented"), "**Indented**");
    }
}
