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
///
/// When a split is needed it prefers the last whitespace inside the limit
/// (searching back at most `CAPTION_SPLIT_LOOKBACK` characters) so the cut
/// doesn't land mid-word; with no nearby whitespace it falls back to the
/// hard character cut.
pub fn split_caption(text: &str) -> (String, Option<String>) {
    /// How far back from the hard limit to look for a whitespace boundary.
    /// Bounded so a caption that is one long unbroken token still splits
    /// near the limit instead of collapsing to a tiny head.
    const CAPTION_SPLIT_LOOKBACK: usize = 120;

    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= TELEGRAM_CAPTION_LIMIT {
        return (text.to_string(), None);
    }

    let cut = (1..=CAPTION_SPLIT_LOOKBACK)
        .map(|back| TELEGRAM_CAPTION_LIMIT - back)
        .find(|&i| chars[i].is_whitespace())
        // Split *after* the whitespace so the follow-up starts clean.
        .map(|i| i + 1)
        .unwrap_or(TELEGRAM_CAPTION_LIMIT);

    let head: String = chars[..cut].iter().collect();
    let rest: String = chars[cut..].iter().collect();
    let rest = rest.trim_start();
    // An all-whitespace remainder is no remainder — an empty follow-up
    // message would be rejected by the Bot API.
    if rest.is_empty() {
        (head.trim_end().to_string(), None)
    } else {
        (head.trim_end().to_string(), Some(rest.to_string()))
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

    /// Every supported markdown delimiter must map to its Telegram HTML
    /// counterpart. Verifying all variants in one table guards against
    /// (a) emitting the wrong tag, (b) consuming the wrong number of
    /// delimiter chars, and (c) the language-tag stripping logic on
    /// fenced code drifting from the no-language path.
    #[test]
    fn each_delimiter_maps_to_telegram_html_tag() {
        let cases = [
            ("this is **bold** text", "this is <b>bold</b> text"),
            ("this is *italic* text", "this is <i>italic</i> text"),
            (
                "use `cargo build` here",
                "use <code>cargo build</code> here",
            ),
            ("```rust\nfn main() {}\n```", "<pre>fn main() {}\n</pre>"),
            ("```\nhello\n```", "<pre>hello\n</pre>"),
            // Language tags may contain underscores and must still be
            // stripped from the rendered block.
            ("```objective_c\nx\n```", "<pre>x\n</pre>"),
            // A first line that is not a language tag (spaces) is content
            // and must survive verbatim.
            ("```not a lang\nx\n```", "<pre>not a lang\nx\n</pre>"),
            // A fence opener is exactly three backticks AT the scan
            // position — preceding prose must never be folded into the
            // block, and scanning must resume right after the closer.
            (
                "before ```\ncode\n``` after",
                "before <pre>code\n</pre> after",
            ),
            // Inline code directly followed by a fence: the single
            // backtick must not be mistaken for a fence opener.
            ("`a` ```\nx\n```", "<code>a</code> <pre>x\n</pre>"),
            // Two backticks are an empty inline code span, not a fence —
            // including hard against the end of input (the fence
            // lookahead must not read past the buffer).
            ("x``", "x<code></code>"),
            ("``\n```\nx\n```", "<code></code>\n<pre>x\n</pre>"),
            // The CLOSING fence is also exactly three backticks — inline
            // backticks inside the block (here two backticks a gap apart)
            // must not terminate it early.
            ("```\na`b`c\n```", "<pre>a`b`c\n</pre>"),
            // A single '*' followed by a later "**" is italic then bold —
            // the bold opener must require two ADJACENT stars, not treat
            // the lone star as one.
            ("*i* then **b**", "<i>i</i> then <b>b</b>"),
            (
                "click [here](https://example.com) now",
                r#"click <a href="https://example.com">here</a> now"#,
            ),
            // A link at the very start of the input — the ']' scan begins
            // right after the opening bracket, never before it.
            (
                "[a](https://example.com) leads",
                r#"<a href="https://example.com">a</a> leads"#,
            ),
            // ']' as the final char: there is no room for '(' — must pass
            // through as literal text, not read past the end of input.
            ("tail [x]", "tail [x]"),
            // ']' not immediately followed by '(' is not a link, even when
            // a parenthesized group appears later in the text.
            ("[a] (b)", "[a] (b)"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                markdown_to_telegram_html(input),
                expected,
                "input={input:?}"
            );
        }
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
    fn split_caption_prefers_word_boundary() {
        // Words of 9 chars + space; the hard limit would land mid-word, the
        // split must back up to the preceding space instead.
        let word = "abcdefghi ";
        let text = word.repeat(200); // 2000 chars
        let (cap, rest) = split_caption(&text);
        let rest = rest.expect("over-limit caption must split");
        assert!(
            cap.chars().count() <= TELEGRAM_CAPTION_LIMIT,
            "caption must fit the limit"
        );
        assert!(
            cap.ends_with("abcdefghi"),
            "cut must not land mid-word: ...{:?}",
            cap.chars().rev().take(12).collect::<String>()
        );
        assert!(
            rest.starts_with("abcdefghi"),
            "follow-up must start at a word: {:?}",
            rest.chars().take(12).collect::<String>()
        );
        // The chosen boundary must be the LAST whitespace inside the limit
        // (index 1019 for 10-char words), not some earlier one — a caption
        // cut far short of the limit wastes the media message.
        assert_eq!(
            cap.chars().count(),
            1019,
            "cut must land at the last word boundary before the limit"
        );
        assert_eq!(rest.chars().count(), 2000 - 1020);
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

    /// Nested `***...***` is not in our delimiter grammar — but the
    /// parser must NOT panic, hang, or drop the inner content. The
    /// graceful-fallback contract: output is non-empty and the original
    /// inner text survives (possibly with literal delimiter chars
    /// passing through).
    #[test]
    fn nested_bold_italic_passes_inner_text_through_without_panic() {
        let result = markdown_to_telegram_html("***bold italic***");
        assert!(!result.is_empty());
        assert!(
            result.contains("bold italic"),
            "inner text must survive even when nested delimiters aren't supported, got {result:?}"
        );
    }
}
