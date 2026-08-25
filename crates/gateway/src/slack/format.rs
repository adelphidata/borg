use crate::format_common::strip_header;

/// Convert standard Markdown to Slack mrkdwn format.
///
/// Conversions:
/// - `**bold**` / `__bold__` → `*bold*`
/// - `[text](url)` → `<url|text>`
/// - `# Header` → `*Header*`
/// - Code blocks (``` and `) are preserved (Slack uses the same syntax)
pub fn markdown_to_mrkdwn(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut in_code_block = false;

    for line in text.lines() {
        let mut in_inline_code = false;
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

        // mrkdwn has no heading syntax, so headings degrade to bold (`*`).
        if let Some(header_text) = strip_header(line) {
            result.push('*');
            result.push_str(header_text.trim());
            result.push('*');
            continue;
        }

        let chars: Vec<char> = line.chars().collect();
        let len = chars.len();
        let mut i = 0;

        while i < len {
            // Skip inline code spans
            if chars[i] == '`' {
                in_inline_code = !in_inline_code;
                result.push('`');
                i += 1;
                continue;
            }

            if in_inline_code {
                result.push(chars[i]);
                i += 1;
                continue;
            }

            // Bold: **text** → *text*
            if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
                if let Some(end) = find_closing(&chars, i + 2, ['*', '*']) {
                    result.push('*');
                    let inner: String = chars[i + 2..end].iter().collect();
                    result.push_str(&inner);
                    result.push('*');
                    i = end + 2;
                    continue;
                }
            }

            // Bold: __text__ → *text*
            if i + 1 < len && chars[i] == '_' && chars[i + 1] == '_' {
                if let Some(end) = find_closing(&chars, i + 2, ['_', '_']) {
                    result.push('*');
                    let inner: String = chars[i + 2..end].iter().collect();
                    result.push_str(&inner);
                    result.push('*');
                    i = end + 2;
                    continue;
                }
            }

            // Links: [text](url) → <url|text>
            if chars[i] == '[' {
                if let Some((link_text, url, end)) = parse_markdown_link(&chars, i) {
                    result.push('<');
                    result.push_str(&url);
                    result.push('|');
                    result.push_str(&link_text);
                    result.push('>');
                    i = end;
                    continue;
                }
            }

            result.push(chars[i]);
            i += 1;
        }
    }

    result
}

/// Find closing delimiter (two-char sequences like ** or __).
fn find_closing(chars: &[char], start: usize, delim: [char; 2]) -> Option<usize> {
    let len = chars.len();
    let mut i = start;
    while i + 1 < len {
        if chars[i] == delim[0] && chars[i + 1] == delim[1] {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Parse a markdown link `[text](url)` starting at position `start`.
/// Returns (link_text, url, end_position) where end_position is after the closing `)`.
fn parse_markdown_link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let mut i = start + 1;
    let len = chars.len();
    while i < len && chars[i] != ']' {
        i += 1;
    }
    if i >= len {
        return None;
    }
    let link_text: String = chars[start + 1..i].iter().collect();

    // Expect ( immediately after ]
    i += 1;
    if i >= len || chars[i] != '(' {
        return None;
    }

    let url_start = i + 1;
    while i < len && chars[i] != ')' {
        i += 1;
    }
    if i >= len {
        return None;
    }
    let url: String = chars[url_start..i].iter().collect();

    Some((link_text, url, i + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every markdown→mrkdwn delimiter mapping in one table — Slack's
    /// mrkdwn collapses both bold sigils (`**` and `__`) into single-`*`,
    /// reframes `[text](url)` as `<url|text>`, and renders all ATX header
    /// levels as bold (mrkdwn has no header tag).
    #[test]
    fn delimiters_map_to_mrkdwn_targets() {
        let cases = [
            ("**hello**", "*hello*"),
            ("__hello__", "*hello*"),
            (
                "[click here](https://example.com)",
                "<https://example.com|click here>",
            ),
            ("# Title", "*Title*"),
            ("## Subtitle", "*Subtitle*"),
            ("### Section", "*Section*"),
        ];
        for (input, expected) in cases {
            assert_eq!(markdown_to_mrkdwn(input), expected, "input={input:?}");
        }
    }

    #[test]
    fn code_block_preserved() {
        let input = "```\n**not bold**\n```";
        assert_eq!(markdown_to_mrkdwn(input), input);
    }

    #[test]
    fn inline_code_preserved() {
        assert_eq!(markdown_to_mrkdwn("`**not bold**`"), "`**not bold**`");
    }

    #[test]
    fn mixed_content() {
        assert_eq!(
            markdown_to_mrkdwn("This is **bold** and [link](https://x.com)"),
            "This is *bold* and <https://x.com|link>"
        );
    }

    /// Inputs that should pass through unchanged: plain text, empty,
    /// multi-line plain, and unmatched delimiters (no closing pair).
    #[test]
    fn passthrough_inputs_are_unchanged() {
        let identities = ["hello world", "", "line 1\nline 2\nline 3", "**unclosed"];
        for input in identities {
            assert_eq!(markdown_to_mrkdwn(input), input, "no-op for {input:?}");
        }
    }

    /// Delimiter-scanning edge cases. Each row pins one boundary of the
    /// two-char delimiter scan:
    /// - a closing pair must be exactly two chars — a lone `*`/`_` inside
    ///   or before a pair must never close (or open) bold;
    /// - a trailing lone delimiter at end-of-line must not read past the
    ///   buffer;
    /// - `__` pairs must only open at a real `__`, not at arbitrary text;
    /// - text after a closing pair resumes immediately after it (offset
    ///   `end + 2`, not further).
    #[test]
    fn delimiter_scan_edge_cases() {
        let cases = [
            // Lone `*` inside a `**` pair is content, not a closer.
            ("**a*b**", "*a*b*"),
            // Unclosed bold with a trailing lone `*` — unchanged, no OOB.
            ("**abc*", "**abc*"),
            // Lone `*` never opens bold even with a `**` later on the line.
            ("a*b**", "a*b**"),
            // Underscore analog: unclosed `__` with a trailing lone `_`.
            ("__abc_", "__abc_"),
            // `__` pair after leading plain text converts; the text stays.
            ("ab__cd__", "ab*cd*"),
            // Lone `_` never opens bold even with a `__` later on the line.
            ("a_b__", "a_b__"),
            // Text after the closing `__` resumes right after the pair.
            ("__ab__ tail", "*ab* tail"),
        ];
        for (input, expected) in cases {
            assert_eq!(markdown_to_mrkdwn(input), expected, "input={input:?}");
        }
    }
}
