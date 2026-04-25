/// Convert a subset of Markdown to Telegram-compatible HTML.
///
/// Handles: **bold**, *italic*, `code`, ```pre```, [text](url).
/// Escapes HTML entities in non-tag text first to prevent injection.
pub fn markdown_to_telegram_html(text: &str) -> String {
    // First pass: escape HTML entities
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");

    let mut result = String::with_capacity(escaped.len());
    let chars: Vec<char> = escaped.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Code blocks: ```...```
        if i + 2 < len && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`' {
            if let Some(end) = find_triple_backtick(&chars, i + 3) {
                let content: String = chars[i + 3..end].iter().collect();
                // Strip optional language tag on first line
                let code = if let Some(newline_pos) = content.find('\n') {
                    let first_line = &content[..newline_pos];
                    if first_line.chars().all(|c| c.is_alphanumeric() || c == '_') {
                        &content[newline_pos + 1..]
                    } else {
                        &content
                    }
                } else {
                    &content
                };
                result.push_str("<pre>");
                result.push_str(code);
                result.push_str("</pre>");
                i = end + 3;
                continue;
            }
        }

        // Inline code: `...`
        if chars[i] == '`' {
            if let Some(end) = find_char(&chars, '`', i + 1) {
                let content: String = chars[i + 1..end].iter().collect();
                result.push_str("<code>");
                result.push_str(&content);
                result.push_str("</code>");
                i = end + 1;
                continue;
            }
        }

        // Bold: **...**
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(end) = find_double_star(&chars, i + 2) {
                let content: String = chars[i + 2..end].iter().collect();
                result.push_str("<b>");
                result.push_str(&content);
                result.push_str("</b>");
                i = end + 2;
                continue;
            }
        }

        // Italic: *...*
        if chars[i] == '*' {
            if let Some(end) = find_char(&chars, '*', i + 1) {
                let content: String = chars[i + 1..end].iter().collect();
                result.push_str("<i>");
                result.push_str(&content);
                result.push_str("</i>");
                i = end + 1;
                continue;
            }
        }

        // Link: [text](url)
        if chars[i] == '[' {
            if let Some((text_end, url_start, url_end)) = find_markdown_link(&chars, i) {
                let link_text: String = chars[i + 1..text_end].iter().collect();
                let url: String = chars[url_start..url_end].iter().collect();
                // Undo HTML escaping in URL (the first pass escaped & to &amp; etc.)
                let url = url
                    .replace("&amp;", "&")
                    .replace("&lt;", "<")
                    .replace("&gt;", ">");
                result.push_str(&format!(r#"<a href="{url}">"#));
                result.push_str(&link_text);
                result.push_str("</a>");
                i = url_end + 1;
                continue;
            }
        }

        result.push(chars[i]);
        i += 1;
    }

    result
}

/// Telegram media caption byte limit. Captions longer than this are rejected
/// by the Bot API (`caption is too long`).
pub const TELEGRAM_CAPTION_LIMIT: usize = 1024;

/// Split a caption into a leading chunk that fits Telegram's media caption
/// limit and a remainder to send as a follow-up text message. The split
/// always falls on a UTF-8 char boundary (counting *characters*, not bytes,
/// which Telegram does for caption length too).
///
/// Returns `(caption, follow_up)` where:
/// - `caption` is at most `TELEGRAM_CAPTION_LIMIT` characters long;
/// - `follow_up` is the remaining text, or `None` if the input fit.
pub fn split_caption(text: &str) -> (String, Option<String>) {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(TELEGRAM_CAPTION_LIMIT).collect();
    let rest: String = chars.collect();
    if rest.is_empty() {
        (head, None)
    } else {
        (head, Some(rest))
    }
}

fn find_char(chars: &[char], target: char, start: usize) -> Option<usize> {
    (start..chars.len()).find(|&i| chars[i] == target)
}

fn find_double_star(chars: &[char], start: usize) -> Option<usize> {
    (start..chars.len().saturating_sub(1)).find(|&i| chars[i] == '*' && chars[i + 1] == '*')
}

fn find_triple_backtick(chars: &[char], start: usize) -> Option<usize> {
    (start..chars.len().saturating_sub(2))
        .find(|&i| chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`')
}

fn find_markdown_link(chars: &[char], start: usize) -> Option<(usize, usize, usize)> {
    // Find closing ]
    let text_end = find_char(chars, ']', start + 1)?;
    // Must be immediately followed by (
    if text_end + 1 >= chars.len() || chars[text_end + 1] != '(' {
        return None;
    }
    let url_start = text_end + 2;
    let url_end = find_char(chars, ')', url_start)?;
    Some((text_end, url_start, url_end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_unchanged() {
        assert_eq!(markdown_to_telegram_html("hello world"), "hello world");
    }

    #[test]
    fn html_entities_escaped() {
        assert_eq!(
            markdown_to_telegram_html("a < b & c > d"),
            "a &lt; b &amp; c &gt; d"
        );
    }

    #[test]
    fn bold_conversion() {
        assert_eq!(
            markdown_to_telegram_html("this is **bold** text"),
            "this is <b>bold</b> text"
        );
    }

    #[test]
    fn italic_conversion() {
        assert_eq!(
            markdown_to_telegram_html("this is *italic* text"),
            "this is <i>italic</i> text"
        );
    }

    #[test]
    fn inline_code_conversion() {
        assert_eq!(
            markdown_to_telegram_html("use `cargo build` here"),
            "use <code>cargo build</code> here"
        );
    }

    #[test]
    fn code_block_conversion() {
        assert_eq!(
            markdown_to_telegram_html("```rust\nfn main() {}\n```"),
            "<pre>fn main() {}\n</pre>"
        );
    }

    #[test]
    fn code_block_no_language() {
        assert_eq!(
            markdown_to_telegram_html("```\nhello\n```"),
            "<pre>hello\n</pre>"
        );
    }

    #[test]
    fn link_conversion() {
        assert_eq!(
            markdown_to_telegram_html("click [here](https://example.com) now"),
            r#"click <a href="https://example.com">here</a> now"#
        );
    }

    #[test]
    fn mixed_formatting() {
        assert_eq!(
            markdown_to_telegram_html("**bold** and *italic* and `code`"),
            "<b>bold</b> and <i>italic</i> and <code>code</code>"
        );
    }

    #[test]
    fn link_with_query_params_preserves_ampersand() {
        assert_eq!(
            markdown_to_telegram_html("visit [here](https://example.com?a=1&b=2) now"),
            r#"visit <a href="https://example.com?a=1&b=2">here</a> now"#
        );
    }

    #[test]
    fn unmatched_markers_pass_through() {
        assert_eq!(
            markdown_to_telegram_html("a single * star"),
            "a single * star"
        );
    }

    #[test]
    fn split_caption_short_text_no_followup() {
        let (cap, rest) = split_caption("hello world");
        assert_eq!(cap, "hello world");
        assert!(rest.is_none());
    }

    #[test]
    fn split_caption_at_exact_limit_no_followup() {
        let text = "a".repeat(TELEGRAM_CAPTION_LIMIT);
        let (cap, rest) = split_caption(&text);
        assert_eq!(cap.chars().count(), TELEGRAM_CAPTION_LIMIT);
        assert!(
            rest.is_none(),
            "text exactly at the limit must not produce a follow-up"
        );
    }

    #[test]
    fn split_caption_over_limit_yields_followup() {
        let text = "a".repeat(2000);
        let (cap, rest) = split_caption(&text);
        assert_eq!(cap.chars().count(), TELEGRAM_CAPTION_LIMIT);
        let rest = rest.expect("over-limit text must produce a follow-up");
        assert_eq!(rest.chars().count(), 2000 - TELEGRAM_CAPTION_LIMIT);
    }

    #[test]
    fn split_caption_respects_char_boundaries() {
        // "🌟" is 4 bytes but 1 char. Build a string whose split point is mid-emoji
        // when counted by bytes — verify we count chars, not bytes.
        let mut text = String::new();
        for _ in 0..(TELEGRAM_CAPTION_LIMIT + 5) {
            text.push('🌟');
        }
        let (cap, rest) = split_caption(&text);
        // Every code point survives intact — no panic, no replacement chars.
        assert_eq!(cap.chars().count(), TELEGRAM_CAPTION_LIMIT);
        assert!(cap.chars().all(|c| c == '🌟'));
        let rest = rest.unwrap();
        assert_eq!(rest.chars().count(), 5);
    }

    #[test]
    fn nested_bold_italic_not_supported_gracefully() {
        // We don't support nesting, but it shouldn't panic
        let result = markdown_to_telegram_html("***bold italic***");
        assert!(!result.is_empty());
    }
}
