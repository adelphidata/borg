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

    /// Header conversion is the entire job of this function: each
    /// `#`-prefixed line (any depth 1–6, possibly indented) becomes
    /// `**bold**`. All other markdown — and every line inside a fenced
    /// code block — is a no-op so Discord's native renderer handles it.
    #[test]
    fn each_header_depth_converts_to_bold_and_indented_headers_still_match() {
        let cases = [
            ("# Title", "**Title**"),
            ("## Subtitle", "**Subtitle**"),
            ("### Section", "**Section**"),
            ("###### Deep", "**Deep**"),
            ("  # Indented", "**Indented**"),
        ];
        for (input, expected) in cases {
            assert_eq!(markdown_to_discord(input), expected, "input={input:?}");
        }
    }

    /// Non-header markdown (bold, links, block quotes, plain text, empty)
    /// and `#` lines inside fenced/inline code blocks MUST pass through
    /// unmodified — Discord's renderer handles them natively and any
    /// alteration would corrupt the formatting users see.
    #[test]
    fn non_header_and_code_block_content_passes_through_unchanged() {
        let identities = [
            "**bold**",
            "[click](https://example.com)",
            "> quoted text",
            "hello world",
            "",
            "`# not a header`",
            "line 1\nline 2\nline 3",
            "```\n# Not a header\n## Also not\n```",
        ];
        for input in identities {
            assert_eq!(
                markdown_to_discord(input),
                input,
                "must be a no-op for {input:?}"
            );
        }
    }

    /// The mixed multi-line case is the integration check: headers
    /// convert, code-block content stays literal, and surrounding plain
    /// text is preserved verbatim.
    #[test]
    fn multiline_mixed() {
        let input = "# Title\nSome text\n```\n# code comment\n```\n## Another heading";
        let expected = "**Title**\nSome text\n```\n# code comment\n```\n**Another heading**";
        assert_eq!(markdown_to_discord(input), expected);
    }
}
