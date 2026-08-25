/// Signal text formatting: convert markdown to plain text with style ranges.
///
/// Signal uses positional `{start, length, style}` annotations on plain text
/// rather than inline markup. This module converts common markdown patterns
/// into Signal's native text style format.
use serde::Serialize;

use crate::message_processing::chunker;

/// Supported Signal text style types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SignalStyle {
    Bold,
    Italic,
    Strikethrough,
    Monospace,
    Spoiler,
}

/// A positional text style annotation for Signal messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextStyle {
    /// Character offset where the style begins.
    pub start: usize,
    /// Number of characters the style spans.
    pub length: usize,
    /// The style type to apply.
    pub style: SignalStyle,
}

/// The result of markdown-to-Signal conversion: plain text with style annotations.
#[derive(Debug, Clone)]
pub struct FormattedText {
    /// Plain text with all markdown delimiters stripped.
    pub text: String,
    /// Style ranges referencing character positions in `text`.
    pub styles: Vec<TextStyle>,
}

/// Convert markdown text into Signal's plain text + style ranges format.
///
/// Supported patterns:
/// - `**bold**` → BOLD
/// - `*italic*` → ITALIC
/// - `` `mono` `` → MONOSPACE (inline)
/// - `~~~code~~~` or ` ```code``` ` → MONOSPACE (block, language tag stripped)
/// - `~~strikethrough~~` → STRIKETHROUGH
/// - `||spoiler||` → SPOILER
pub fn markdown_to_signal(input: &str) -> FormattedText {
    let mut text = String::with_capacity(input.len());
    let mut styles = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Fenced code block: ```
        if i + 2 < len && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`' {
            if let Some((content, end)) = find_fenced_code(&chars, i) {
                let start = text.chars().count();
                text.push_str(&content);
                let length = content.chars().count();
                if length > 0 {
                    styles.push(TextStyle {
                        start,
                        length,
                        style: SignalStyle::Monospace,
                    });
                }
                i = end;
                continue;
            }
        }

        // Bold: **text**
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some((content, end)) = extract_delimited(&chars, i, ['*', '*']) {
                let start = text.chars().count();
                text.push_str(&content);
                let length = content.chars().count();
                if length > 0 {
                    styles.push(TextStyle {
                        start,
                        length,
                        style: SignalStyle::Bold,
                    });
                }
                i = end;
                continue;
            }
        }

        // Strikethrough: ~~text~~
        if i + 1 < len && chars[i] == '~' && chars[i + 1] == '~' {
            if let Some((content, end)) = extract_delimited(&chars, i, ['~', '~']) {
                let start = text.chars().count();
                text.push_str(&content);
                let length = content.chars().count();
                if length > 0 {
                    styles.push(TextStyle {
                        start,
                        length,
                        style: SignalStyle::Strikethrough,
                    });
                }
                i = end;
                continue;
            }
        }

        // Spoiler: ||text||
        if i + 1 < len && chars[i] == '|' && chars[i + 1] == '|' {
            if let Some((content, end)) = extract_delimited(&chars, i, ['|', '|']) {
                let start = text.chars().count();
                text.push_str(&content);
                let length = content.chars().count();
                if length > 0 {
                    styles.push(TextStyle {
                        start,
                        length,
                        style: SignalStyle::Spoiler,
                    });
                }
                i = end;
                continue;
            }
        }

        // Italic: *text*  (single star, not double)
        if chars[i] == '*' {
            if let Some((content, end)) = extract_single_delimited(&chars, i, '*') {
                let start = text.chars().count();
                text.push_str(&content);
                let length = content.chars().count();
                if length > 0 {
                    styles.push(TextStyle {
                        start,
                        length,
                        style: SignalStyle::Italic,
                    });
                }
                i = end;
                continue;
            }
        }

        // Inline code: `text`
        if chars[i] == '`' {
            if let Some((content, end)) = extract_single_delimited(&chars, i, '`') {
                let start = text.chars().count();
                text.push_str(&content);
                let length = content.chars().count();
                if length > 0 {
                    styles.push(TextStyle {
                        start,
                        length,
                        style: SignalStyle::Monospace,
                    });
                }
                i = end;
                continue;
            }
        }

        text.push(chars[i]);
        i += 1;
    }

    FormattedText { text, styles }
}

/// Find the closing ``` for a fenced code block, stripping the optional language tag.
/// Returns (content, end_index) where end_index is past the closing ```.
fn find_fenced_code(chars: &[char], start: usize) -> Option<(String, usize)> {
    let len = chars.len();
    // Skip opening ```
    let mut i = start + 3;

    // Skip optional language tag (everything until newline)
    while i < len && chars[i] != '\n' {
        i += 1;
    }
    // Skip the newline itself
    if i < len && chars[i] == '\n' {
        i += 1;
    }

    let content_start = i;

    // Find closing ```
    while i + 2 < len {
        if chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`' {
            let content: String = chars[content_start..i].iter().collect();
            // Trim trailing newline from content
            let content = content.trim_end_matches('\n').to_string();
            return Some((content, i + 3));
        }
        i += 1;
    }

    None // No closing found
}

/// Extract content between a two-character delimiter (e.g. `**`, `~~`, `||`).
/// Returns (inner_content, end_index) where end_index is past the closing delimiter.
fn extract_delimited(chars: &[char], start: usize, delim: [char; 2]) -> Option<(String, usize)> {
    let len = chars.len();
    let mut i = start + 2; // Skip opening delimiter
    let content_start = i;

    while i + 1 < len {
        if chars[i] == delim[0] && chars[i + 1] == delim[1] {
            if i == content_start {
                return None; // Empty content
            }
            let content: String = chars[content_start..i].iter().collect();
            return Some((content, i + 2));
        }
        i += 1;
    }

    None // No closing found
}

/// Extract content between a single-character delimiter (e.g. `*`, `` ` ``).
/// Ensures this isn't actually a double delimiter (e.g. `**`).
fn extract_single_delimited(chars: &[char], start: usize, delim: char) -> Option<(String, usize)> {
    let len = chars.len();
    // A doubled delimiter belongs to the two-char form (`**`), so bail and let
    // that parser claim it.
    if start + 1 < len && chars[start + 1] == delim {
        return None;
    }

    let mut i = start + 1; // Skip opening delimiter
    let content_start = i;

    while i < len {
        if chars[i] == delim {
            if i + 1 < len && chars[i + 1] == delim {
                i += 2;
                continue;
            }
            if i == content_start {
                return None; // Empty content
            }
            let content: String = chars[content_start..i].iter().collect();
            return Some((content, i + 1));
        }
        i += 1;
    }

    None // No closing found
}

/// Split a `FormattedText` into chunks, clamping style ranges to each chunk.
///
/// Uses `chunker::chunk_text` for the text splitting, then remaps style ranges
/// so each chunk has independently valid start/length values.
pub fn chunk_with_styles(formatted: &FormattedText, max_chars: usize) -> Vec<FormattedText> {
    let chunks = chunker::chunk_text(&formatted.text, max_chars);
    if chunks.is_empty() {
        return vec![];
    }
    if chunks.len() == 1 {
        let text = match chunks.into_iter().next() {
            Some(t) => t,
            None => return vec![],
        };
        return vec![FormattedText {
            text,
            styles: formatted.styles.clone(),
        }];
    }

    let mut result = Vec::with_capacity(chunks.len());
    let mut char_offset: usize = 0;

    for chunk in &chunks {
        let chunk_chars = chunk.chars().count();
        let chunk_end = char_offset + chunk_chars;

        let mut chunk_styles = Vec::new();
        for style in &formatted.styles {
            let style_end = style.start + style.length;

            // Skip styles that don't overlap this chunk
            if style_end <= char_offset || style.start >= chunk_end {
                continue;
            }

            // Clamp to chunk boundaries
            let clamped_start = style.start.max(char_offset);
            let clamped_end = style_end.min(chunk_end);
            let length = clamped_end - clamped_start;

            if length > 0 {
                chunk_styles.push(TextStyle {
                    start: clamped_start - char_offset,
                    length,
                    style: style.style,
                });
            }
        }

        result.push(FormattedText {
            text: chunk.clone(),
            styles: chunk_styles,
        });

        char_offset = chunk_end;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passthrough() {
        let result = markdown_to_signal("hello world");
        assert_eq!(result.text, "hello world");
        assert!(result.styles.is_empty());
    }

    /// Each delimiter pair must (a) strip from the output text and
    /// (b) emit a single style span at the correct character offset.
    /// Verifying all five styles in one table guards against per-delimiter
    /// regressions (e.g. accidentally consuming the wrong number of chars
    /// or emitting the wrong `SignalStyle` enum).
    #[test]
    fn each_delimiter_strips_to_text_and_emits_single_style_span() {
        let cases = [
            ("hello **world**", "hello world", 6, 5, SignalStyle::Bold),
            ("hello *world*", "hello world", 6, 5, SignalStyle::Italic),
            ("~~deleted~~", "deleted", 0, 7, SignalStyle::Strikethrough),
            ("||hidden||", "hidden", 0, 6, SignalStyle::Spoiler),
            (
                "run `cargo test`",
                "run cargo test",
                4,
                10,
                SignalStyle::Monospace,
            ),
        ];
        for (input, expected_text, start, length, style) in cases {
            let result = markdown_to_signal(input);
            assert_eq!(result.text, expected_text, "text for {input:?}");
            assert_eq!(
                result.styles,
                vec![TextStyle {
                    start,
                    length,
                    style,
                }],
                "styles for {input:?}",
            );
        }
    }

    #[test]
    fn code_block() {
        let result = markdown_to_signal("```rust\nfn main() {}\n```");
        assert_eq!(result.text, "fn main() {}");
        assert_eq!(
            result.styles,
            vec![TextStyle {
                start: 0,
                length: 12,
                style: SignalStyle::Monospace,
            }]
        );
    }

    #[test]
    fn code_block_no_language() {
        let result = markdown_to_signal("```\nhello\n```");
        assert_eq!(result.text, "hello");
        assert_eq!(result.styles.len(), 1);
        assert_eq!(result.styles[0].style, SignalStyle::Monospace);
    }

    #[test]
    fn multiple_styles() {
        let result = markdown_to_signal("**bold** and *italic* and `code`");
        assert_eq!(result.text, "bold and italic and code");
        assert_eq!(result.styles.len(), 3);
        assert_eq!(result.styles[0].style, SignalStyle::Bold);
        assert_eq!(result.styles[0].start, 0);
        assert_eq!(result.styles[0].length, 4);
        assert_eq!(result.styles[1].style, SignalStyle::Italic);
        assert_eq!(result.styles[1].start, 9);
        assert_eq!(result.styles[1].length, 6);
        assert_eq!(result.styles[2].style, SignalStyle::Monospace);
        assert_eq!(result.styles[2].start, 20);
        assert_eq!(result.styles[2].length, 4);
    }

    #[test]
    fn unmatched_markers_pass_through() {
        let result = markdown_to_signal("hello **world");
        assert_eq!(result.text, "hello **world");
        assert!(result.styles.is_empty());
    }

    #[test]
    fn unmatched_single_star() {
        let result = markdown_to_signal("5 * 3 = 15");
        assert_eq!(result.text, "5 * 3 = 15");
        assert!(result.styles.is_empty());
    }

    /// Empty content between matched delimiters (e.g. `****`, `~~~~`)
    /// must NOT emit a zero-length style span — Signal rejects those —
    /// and the original characters pass through as plain text.
    #[test]
    fn empty_delimiters_emit_no_styles_and_pass_through_literally() {
        let result = markdown_to_signal("**** and ~~~~");
        assert!(
            result.styles.is_empty(),
            "empty delimiters must not emit zero-length spans, got {:?}",
            result.styles,
        );
        // We don't pin exact output (the parser is allowed to consume or
        // re-emit the delimiter pair) but the delimiter chars must survive
        // somewhere so the user sees what they typed.
        assert!(
            result.text.contains("**") || result.text.contains("~~"),
            "empty-delimiter characters should pass through as literal text, got {:?}",
            result.text,
        );
    }

    /// Boundary inputs where a delimiter's lookahead lands on or past the end
    /// of the string, and lone delimiters followed later by a doubled closer.
    /// All must pass through verbatim with no styles — an off-by-one in the
    /// lookahead bounds check panics on out-of-range indexing, and a lookahead
    /// reading the wrong index mis-parses the lone delimiter as an opener and
    /// silently drops user text.
    #[test]
    fn boundary_and_lone_delimiters_pass_through_verbatim() {
        let cases = [
            "``",    // double backtick at end — fence lookahead needs a 3rd char
            "a``",   // fence lookahead lands exactly past the end
            "ab*",   // trailing single star — bold lookahead at the last index
            "ab|",   // trailing single pipe — spoiler lookahead at the last index
            "ab~",   // trailing single tilde — strikethrough lookahead
            "*ab**", // lone star then a doubled closer: not bold, not italic
            "~ab~~", // lone tilde then a doubled closer: not strikethrough
            "|ab||", // lone pipe then a doubled closer: not a spoiler
        ];
        for input in cases {
            let result = markdown_to_signal(input);
            assert_eq!(result.text, input, "text for {input:?}");
            assert!(
                result.styles.is_empty(),
                "no styles expected for {input:?}, got {:?}",
                result.styles
            );
        }
    }

    /// A fence opener is only the exact sequence ``` at position i..i+3.
    /// Text before the fence must survive, and a stray backtick pair with a
    /// non-backtick third char must not swallow following text as a block.
    #[test]
    fn fence_detection_uses_exact_backtick_offsets() {
        // Leading text before a real fence is kept, fence content styled.
        let result = markdown_to_signal("x```\ny\n```");
        assert_eq!(result.text, "xy");
        assert_eq!(
            result.styles,
            vec![TextStyle {
                start: 1,
                length: 1,
                style: SignalStyle::Monospace,
            }]
        );

        // The third backtick is checked at i+2, not at a scaled offset.
        let result = markdown_to_signal("abc```\nx\n```");
        assert_eq!(result.text, "abcx");
        assert_eq!(
            result.styles,
            vec![TextStyle {
                start: 3,
                length: 1,
                style: SignalStyle::Monospace,
            }]
        );
    }

    /// A single backtick never opens a fence, even when an unrelated ```
    /// appears later — `\`a\`` stays inline code and the trailing orphan
    /// fence passes through as text.
    #[test]
    fn single_backtick_is_inline_code_not_a_fence() {
        let result = markdown_to_signal("`a`\nb\n```");
        assert_eq!(result.text, "a\nb\n```");
        assert_eq!(
            result.styles,
            vec![TextStyle {
                start: 0,
                length: 1,
                style: SignalStyle::Monospace,
            }]
        );
    }

    /// An empty fenced block (```\n```), like empty inline delimiters, must
    /// not emit a zero-length style span — Signal rejects those.
    #[test]
    fn empty_fenced_block_emits_no_style() {
        let result = markdown_to_signal("```\n```");
        assert_eq!(result.text, "");
        assert!(
            result.styles.is_empty(),
            "empty code block must not emit a zero-length span, got {:?}",
            result.styles
        );
    }

    /// A spoiler opener requires BOTH pipes: text before `||` must never be
    /// absorbed into the spoiler span.
    #[test]
    fn spoiler_after_leading_text() {
        let result = markdown_to_signal("a||b||");
        assert_eq!(result.text, "ab");
        assert_eq!(
            result.styles,
            vec![TextStyle {
                start: 1,
                length: 1,
                style: SignalStyle::Spoiler,
            }]
        );
    }

    #[test]
    fn empty_input() {
        let result = markdown_to_signal("");
        assert_eq!(result.text, "");
        assert!(result.styles.is_empty());
    }

    #[test]
    fn unicode_positions() {
        // Emoji are single chars in Rust
        let result = markdown_to_signal("😀 **bold** 🎉");
        assert_eq!(result.text, "😀 bold 🎉");
        assert_eq!(result.styles[0].start, 2);
        assert_eq!(result.styles[0].length, 4);
    }

    /// When the formatted text fits in one chunk, `chunk_with_styles` must
    /// pass styles through unmodified — re-emitting them with new offsets
    /// would silently corrupt single-message sends.
    #[test]
    fn chunk_single_chunk_preserves_styles_verbatim() {
        let formatted = markdown_to_signal("**hello**");
        let chunks = chunk_with_styles(&formatted, 100);
        assert_eq!(chunks.len(), 1, "input fits in one chunk");
        assert_eq!(chunks[0].text, "hello");
        assert_eq!(
            chunks[0].styles, formatted.styles,
            "single-chunk styles must equal source styles, no remapping"
        );
    }

    #[test]
    fn chunk_splits_styles_across_boundary() {
        // Create a long formatted text where a bold span crosses the chunk boundary
        let plain = "a".repeat(3990);
        let bold_part = "b".repeat(20);
        let input = format!("{plain}**{bold_part}**");
        let formatted = markdown_to_signal(&input);

        assert_eq!(formatted.text.len(), 4010); // 3990 + 20
        assert_eq!(formatted.styles[0].start, 3990);
        assert_eq!(formatted.styles[0].length, 20);

        let chunks = chunk_with_styles(&formatted, 4000);
        assert_eq!(chunks.len(), 2);

        // First chunk should have the first 10 chars of the bold span
        let first_bold: Vec<_> = chunks[0]
            .styles
            .iter()
            .filter(|s| s.style == SignalStyle::Bold)
            .collect();
        assert_eq!(first_bold.len(), 1);
        assert_eq!(first_bold[0].start, 3990);
        assert_eq!(first_bold[0].length, 10);

        // Second chunk should have the remaining 10 chars
        let second_bold: Vec<_> = chunks[1]
            .styles
            .iter()
            .filter(|s| s.style == SignalStyle::Bold)
            .collect();
        assert_eq!(second_bold.len(), 1);
        assert_eq!(second_bold[0].start, 0);
        assert_eq!(second_bold[0].length, 10);
    }

    #[test]
    fn chunk_empty_input() {
        let formatted = FormattedText {
            text: String::new(),
            styles: vec![],
        };
        let chunks = chunk_with_styles(&formatted, 100);
        assert!(chunks.is_empty());
    }

    /// Signal expects style tags as SCREAMING_SNAKE_CASE in JSON — covers
    /// the full enum + verifies positional fields round-trip on the wire.
    #[test]
    fn signal_wire_format_uppercases_styles_and_preserves_positions() {
        let styles = [
            (SignalStyle::Bold, "BOLD"),
            (SignalStyle::Italic, "ITALIC"),
            (SignalStyle::Strikethrough, "STRIKETHROUGH"),
            (SignalStyle::Monospace, "MONOSPACE"),
            (SignalStyle::Spoiler, "SPOILER"),
        ];
        for (style, expected_tag) in styles {
            let ts = TextStyle {
                start: 3,
                length: 7,
                style,
            };
            let json = serde_json::to_value(&ts).unwrap();
            assert_eq!(json["start"], 3, "start for {style:?}");
            assert_eq!(json["length"], 7, "length for {style:?}");
            assert_eq!(json["style"], expected_tag, "tag for {style:?}");
        }
    }
}
