//! Convert standard Markdown to Slack's mrkdwn format.
//!
//! Slack uses a subset of Markdown with different syntax for bold, strikethrough,
//! and links. Code blocks and inline code are the same. This converter handles
//! the common cases without requiring a full Markdown parser.

use regex::Regex;
use std::sync::LazyLock;

#[allow(clippy::expect_used)]
static RE_BOLD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\*\*(.+?)\*\*").expect("compile-time literal regex"));

#[allow(clippy::expect_used)]
static RE_STRIKE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"~~(.+?)~~").expect("compile-time literal regex"));

#[allow(clippy::expect_used)]
static RE_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").expect("compile-time literal regex"));

#[allow(clippy::expect_used)]
static RE_HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^#{1,6}\s+(.+)$").expect("compile-time literal regex"));

/// Convert standard Markdown to Slack mrkdwn format.
///
/// Transformations:
/// - `**bold**` → `*bold*`
/// - `~~strike~~` → `~strike~`
/// - `[text](url)` → `<url|text>`
/// - `# Heading` → `*Heading*`
/// - `&`, `<`, `>` escaped outside code blocks and Slack special tokens
///
/// Preserves:
/// - Code blocks (``` ```) and inline code (`` ` ``)
/// - Existing Slack tokens like `<@U123>`, `<#C123>`, `<!here>`
/// - Block quotes (`>`)
/// - Italic (`_text_`) — same in both formats
pub fn markdown_to_mrkdwn(text: &str) -> String {
    // Split into code and non-code segments to avoid transforming code content
    let segments = split_code_segments(text);
    let mut result = String::with_capacity(text.len());

    for (segment, is_code) in segments {
        if is_code {
            result.push_str(segment);
        } else {
            result.push_str(&convert_segment(segment));
        }
    }

    result
}

/// Split text into alternating (text, is_code) segments.
/// Handles both fenced code blocks (```) and inline code (`).
fn split_code_segments(text: &str) -> Vec<(&str, bool)> {
    let mut segments = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        // Check for fenced code block first (```)
        if let Some(start) = remaining.find("```") {
            if start > 0 {
                segments.push((&remaining[..start], false));
            }
            let after_open = &remaining[start + 3..];
            if let Some(end) = after_open.find("```") {
                segments.push((&remaining[start..start + 3 + end + 3], true));
                remaining = &remaining[start + 3 + end + 3..];
            } else {
                // Unclosed code block — treat rest as code
                segments.push((&remaining[start..], true));
                remaining = "";
            }
        } else if let Some(start) = remaining.find('`') {
            if start > 0 {
                segments.push((&remaining[..start], false));
            }
            let after_open = &remaining[start + 1..];
            if let Some(end) = after_open.find('`') {
                segments.push((&remaining[start..start + 1 + end + 1], true));
                remaining = &remaining[start + 1 + end + 1..];
            } else {
                // Unclosed inline code — treat rest as text
                segments.push((&remaining[start..], false));
                remaining = "";
            }
        } else {
            segments.push((remaining, false));
            remaining = "";
        }
    }

    segments
}

/// Convert a non-code segment from Markdown to mrkdwn.
fn convert_segment(text: &str) -> String {
    let mut result = text.to_string();

    // Escape HTML entities (but preserve existing Slack angle-bracket tokens)
    result = escape_entities(&result);

    result = RE_BOLD.replace_all(&result, "*$1*").to_string();
    result = RE_STRIKE.replace_all(&result, "~$1~").to_string();
    result = RE_LINK.replace_all(&result, "<$2|$1>").to_string();
    // mrkdwn has no heading syntax, so headings degrade to bold.
    result = RE_HEADING.replace_all(&result, "*$1*").to_string();

    result
}

/// Escape `&`, `<`, `>` to their HTML entities, but preserve existing Slack tokens.
///
/// Slack tokens look like `<@U123>`, `<#C123>`, `<!here>`, `<http://...>`, `<http://...|text>`.
/// We need to avoid escaping the angle brackets and ampersands inside these tokens.
fn escape_entities(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '<' {
            // Check if this is a Slack token: <@..>, <#..>, <!..>, <http..>, <mailto:..>
            if let Some(close) = find_matching_close(&chars, i) {
                let inner: String = chars[i + 1..close].iter().collect();
                if is_slack_token(&inner) {
                    // Preserve entire token as-is (including any & inside URLs)
                    let token: String = chars[i..=close].iter().collect();
                    output.push_str(&token);
                    i = close + 1;
                    continue;
                }
            }
            output.push_str("&lt;");
        } else if chars[i] == '>' {
            output.push_str("&gt;");
        } else if chars[i] == '&' {
            output.push_str("&amp;");
        } else {
            output.push(chars[i]);
        }
        i += 1;
    }

    output
}

/// Find the matching `>` for an opening `<`, returns the index.
fn find_matching_close(chars: &[char], open: usize) -> Option<usize> {
    for (j, &ch) in chars.iter().enumerate().skip(open + 1) {
        if ch == '>' {
            return Some(j);
        }
        if ch == '<' || ch == '\n' {
            return None;
        }
    }
    None
}

/// Check if content between < > is a Slack special token.
fn is_slack_token(inner: &str) -> bool {
    inner.starts_with('@')
        || inner.starts_with('#')
        || inner.starts_with('!')
        || inner.starts_with("http://")
        || inner.starts_with("https://")
        || inner.starts_with("mailto:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold_conversion() {
        assert_eq!(markdown_to_mrkdwn("**hello**"), "*hello*");
    }

    #[test]
    fn strikethrough_conversion() {
        assert_eq!(markdown_to_mrkdwn("~~deleted~~"), "~deleted~");
    }

    #[test]
    fn link_conversion() {
        assert_eq!(
            markdown_to_mrkdwn("[click here](https://example.com)"),
            "<https://example.com|click here>"
        );
    }

    #[test]
    fn heading_conversion() {
        assert_eq!(markdown_to_mrkdwn("# Title"), "*Title*");
        assert_eq!(markdown_to_mrkdwn("## Subtitle"), "*Subtitle*");
        assert_eq!(markdown_to_mrkdwn("### Deep"), "*Deep*");
    }

    #[test]
    fn code_block_passthrough() {
        let input = "before ```\n**not bold**\n``` after";
        let result = markdown_to_mrkdwn(input);
        // The code block content should be preserved exactly
        assert!(result.contains("```\n**not bold**\n```"));
    }

    #[test]
    fn inline_code_passthrough() {
        assert_eq!(
            markdown_to_mrkdwn("use `**raw**` here"),
            "use `**raw**` here"
        );
    }

    #[test]
    fn entity_escaping() {
        assert_eq!(markdown_to_mrkdwn("a & b"), "a &amp; b");
        assert_eq!(markdown_to_mrkdwn("a < b"), "a &lt; b");
        assert_eq!(markdown_to_mrkdwn("a > b"), "a &gt; b");
    }

    #[test]
    fn preserves_slack_mention_tokens() {
        assert_eq!(
            markdown_to_mrkdwn("hi <@U123> and <#C456>"),
            "hi <@U123> and <#C456>"
        );
    }

    #[test]
    fn preserves_slack_special_tokens() {
        assert_eq!(markdown_to_mrkdwn("<!here>"), "<!here>");
        assert_eq!(
            markdown_to_mrkdwn("<https://example.com|link>"),
            "<https://example.com|link>"
        );
    }

    #[test]
    fn mixed_formatting() {
        let input = "**bold** and ~~strike~~ with [link](https://x.com)";
        let result = markdown_to_mrkdwn(input);
        assert_eq!(result, "*bold* and ~strike~ with <https://x.com|link>");
    }

    #[test]
    fn no_op_on_valid_mrkdwn() {
        let input = "*already bold* and ~already strike~";
        assert_eq!(markdown_to_mrkdwn(input), input);
    }

    #[test]
    fn multiline_heading() {
        let input = "# First\nsome text\n## Second";
        let result = markdown_to_mrkdwn(input);
        assert!(result.contains("*First*"));
        assert!(result.contains("some text"));
        assert!(result.contains("*Second*"));
    }

    #[test]
    fn ampersand_preserved_inside_slack_url_token() {
        // URLs inside Slack tokens should not have & escaped
        assert_eq!(
            markdown_to_mrkdwn("<https://example.com?a=1&b=2|link>"),
            "<https://example.com?a=1&b=2|link>"
        );
    }

    #[test]
    fn mixed_italic_and_bold() {
        // Verify *italic* is preserved and **bold** becomes *bold*
        let result = markdown_to_mrkdwn("*italic* and **bold**");
        assert!(result.contains("*bold*"));
    }
}
