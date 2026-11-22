/// Signal text formatting: convert markdown to plain text with style ranges.
///
/// Signal uses positional style annotations on plain text rather than inline
/// markup. This module converts common markdown patterns into Signal's native
/// text style format. Offsets and lengths count UTF-16 code units — the unit
/// Signal (a Java/JavaScript stack) indexes message bodies in — so an emoji
/// before a styled word shifts its start by two, not one.
use serde::{Serialize, Serializer};

use crate::message_processing::chunker::{self, utf16_len};

/// Supported Signal text style types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalStyle {
    Bold,
    Italic,
    Strikethrough,
    Monospace,
    Spoiler,
}

impl SignalStyle {
    /// The style name signal-cli expects in a `textStyle` entry.
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Bold => "BOLD",
            Self::Italic => "ITALIC",
            Self::Strikethrough => "STRIKETHROUGH",
            Self::Monospace => "MONOSPACE",
            Self::Spoiler => "SPOILER",
        }
    }
}

/// A positional text style annotation for Signal messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextStyle {
    /// UTF-16 code-unit offset where the style begins.
    pub start: usize,
    /// Number of UTF-16 code units the style spans.
    pub length: usize,
    /// The style type to apply.
    pub style: SignalStyle,
}

impl TextStyle {
    /// signal-cli's JSON-RPC `send` reads each `textStyle` entry as a string
    /// matching `start:length:STYLE` (e.g. `"0:4:BOLD"`), not as an object.
    pub fn to_wire(&self) -> String {
        format!("{}:{}:{}", self.start, self.length, self.style.as_wire())
    }
}

impl Serialize for TextStyle {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_wire())
    }
}

/// The result of markdown-to-Signal conversion: plain text with style annotations.
#[derive(Debug, Clone)]
pub struct FormattedText {
    /// Plain text with all markdown delimiters stripped.
    pub text: String,
    /// Style ranges referencing UTF-16 code-unit positions in `text`.
    pub styles: Vec<TextStyle>,
}

/// Convert markdown text into Signal's plain text + style ranges format.
///
/// Supported patterns:
/// - `**bold**` → BOLD
/// - `*italic*` → ITALIC (CommonMark-style flanking: the opening `*` is not
///   followed by whitespace, the closing `*` is not preceded by whitespace,
///   and a pair never spans a newline — so `* item` bullets and `2 * 3`
///   arithmetic stay plain)
/// - `` `mono` `` → MONOSPACE (inline)
/// - ` ```code``` ` → MONOSPACE (block, language tag stripped)
/// - `~~strikethrough~~` → STRIKETHROUGH
/// - `||spoiler||` → SPOILER
///
/// Styled content is itself parsed, so `**run `cargo`**` bolds "run " and
/// sets "cargo" in monospace. A code span carries MONOSPACE only: an
/// enclosing bold/italic/strikethrough skips it, and a delimiter inside a
/// code span never closes an outer style.
pub fn markdown_to_signal(input: &str) -> FormattedText {
    let chars: Vec<char> = input.chars().collect();
    let mut out = Builder::default();
    out.convert(&chars);
    FormattedText {
        text: out.text,
        styles: out.styles,
    }
}

/// Accumulates the plain text, its running UTF-16 length, and style spans.
#[derive(Default)]
struct Builder {
    text: String,
    units: usize,
    styles: Vec<TextStyle>,
}

impl Builder {
    fn push_str(&mut self, s: &str) {
        self.text.push_str(s);
        self.units += utf16_len(s);
    }

    fn push_char(&mut self, c: char) {
        self.text.push(c);
        self.units += c.len_utf16();
    }

    /// Record `style` over `[start, end)`; zero-length spans are dropped
    /// (Signal rejects them).
    fn span(&mut self, start: usize, end: usize, style: SignalStyle) {
        if end > start {
            self.styles.push(TextStyle {
                start,
                length: end - start,
                style,
            });
        }
    }

    /// Append code content verbatim under one MONOSPACE span.
    fn push_code(&mut self, content: &str) {
        let start = self.units;
        self.push_str(content);
        self.span(start, self.units, SignalStyle::Monospace);
    }

    /// Append `content` parsed as markdown, then cover it with `style` —
    /// except the code spans inside it, which stay MONOSPACE only. A spoiler
    /// still covers code: it hides text rather than styling it.
    fn push_styled(&mut self, content: &[char], style: SignalStyle) {
        let start = self.units;
        let inner_first = self.styles.len();
        self.convert(content);
        let end = self.units;
        if style == SignalStyle::Spoiler {
            self.span(start, end, style);
            return;
        }
        let mut code: Vec<(usize, usize)> = self.styles[inner_first..]
            .iter()
            .filter(|s| s.style == SignalStyle::Monospace)
            .map(|s| (s.start, s.start + s.length))
            .collect();
        code.sort_unstable();
        let mut cursor = start;
        for (code_start, code_end) in code {
            if code_start > cursor {
                self.span(cursor, code_start, style);
            }
            cursor = cursor.max(code_end);
        }
        self.span(cursor, end, style);
    }

    fn convert(&mut self, chars: &[char]) {
        let len = chars.len();
        let mut i = 0;

        while i < len {
            // Fenced code block: ```
            if i + 2 < len && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`' {
                if let Some((content, end)) = find_fenced_code(chars, i) {
                    self.push_code(&content);
                    i = end;
                    continue;
                }
            }

            // Two-char delimiters: **bold**, ~~strikethrough~~, ||spoiler||
            if i + 1 < len && chars[i] == chars[i + 1] {
                let style = match chars[i] {
                    '*' => Some(SignalStyle::Bold),
                    '~' => Some(SignalStyle::Strikethrough),
                    '|' => Some(SignalStyle::Spoiler),
                    _ => None,
                };
                if let Some(style) = style {
                    if let Some((content, end)) = extract_delimited(chars, i, chars[i]) {
                        self.push_styled(&chars[content], style);
                        i = end;
                        continue;
                    }
                }
            }

            // Italic: *text*  (single star, not double)
            if chars[i] == '*' {
                if let Some((content, end)) = extract_italic(chars, i) {
                    self.push_styled(&chars[content], SignalStyle::Italic);
                    i = end;
                    continue;
                }
            }

            // Inline code: `text`
            if chars[i] == '`' {
                if let Some(close) = code_span_close(chars, i) {
                    let content: String = chars[i + 1..close].iter().collect();
                    self.push_code(&content);
                    i = close + 1;
                    continue;
                }
            }

            self.push_char(chars[i]);
            i += 1;
        }
    }
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

/// Index of the backtick closing the inline code span opened at `open`, if
/// any. A doubled backtick is never a single-backtick opener or closer, so
/// it is skipped while scanning (and an opener followed by one is refused).
fn code_span_close(chars: &[char], open: usize) -> Option<usize> {
    if chars.get(open + 1) == Some(&'`') {
        return None;
    }
    let mut i = open + 1;
    while i < chars.len() {
        if chars[i] == '`' {
            if chars.get(i + 1) == Some(&'`') {
                i += 2;
                continue;
            }
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Extract content between a doubled delimiter (`**`, `~~`, `||`) opened at
/// `start`. Returns (content_range, end_index) where end_index is past the
/// closing delimiter. A delimiter inside a code span is code, not a closer.
fn extract_delimited(
    chars: &[char],
    start: usize,
    delim: char,
) -> Option<(std::ops::Range<usize>, usize)> {
    let len = chars.len();
    let content_start = start + 2; // Skip opening delimiter
    let mut i = content_start;

    while i + 1 < len {
        if chars[i] == '`' {
            if let Some(close) = code_span_close(chars, i) {
                i = close + 1;
                continue;
            }
        }
        if chars[i] == delim && chars[i + 1] == delim {
            if i == content_start {
                return None; // Empty content
            }
            return Some((content_start..i, i + 2));
        }
        i += 1;
    }

    None // No closing found
}

/// Extract `*italic*` content for the single `*` at `start`, with
/// CommonMark-style flanking: the opener must not be followed by whitespace,
/// the closer must not be preceded by whitespace, and the pair must not span
/// a newline. A doubled `**` belongs to bold and is skipped, as is any code
/// span. Returns (content_range, end_index past the closer).
fn extract_italic(chars: &[char], start: usize) -> Option<(std::ops::Range<usize>, usize)> {
    let next = *chars.get(start + 1)?;
    if next == '*' || next.is_whitespace() {
        return None;
    }
    let content_start = start + 1;
    let mut i = content_start;
    while i < chars.len() {
        match chars[i] {
            '\n' => return None,
            '`' => {
                if let Some(close) = code_span_close(chars, i) {
                    i = close + 1;
                    continue;
                }
            }
            '*' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                continue;
            }
            '*' if !chars[i - 1].is_whitespace() => {
                return Some((content_start..i, i + 1));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Split a `FormattedText` into chunks, rebasing style ranges onto each chunk.
///
/// Uses `chunker::chunk_text` for the text splitting. Paragraph and line
/// splits drop the separator (`\n\n` / `\n`) and any trailing whitespace, so
/// a chunk's offset in the source is recovered from the text itself rather
/// than by summing chunk lengths. A style spanning a split is clipped into
/// each chunk it touches; a part that lands only on dropped separator text
/// is discarded.
pub fn chunk_with_styles(formatted: &FormattedText, max_chars: usize) -> Vec<FormattedText> {
    let chunks = chunker::chunk_text(&formatted.text, max_chars);
    if chunks.len() <= 1 {
        return chunks
            .into_iter()
            .map(|text| FormattedText {
                text,
                styles: formatted.styles.clone(),
            })
            .collect();
    }

    let source = formatted.text.as_str();
    let mut result = Vec::with_capacity(chunks.len());
    let mut byte_cursor = 0usize;
    let mut unit_cursor = 0usize;

    for chunk in chunks {
        let chunk_byte = chunk_start(source, byte_cursor, &chunk);
        let chunk_start_units = unit_cursor + utf16_len(&source[byte_cursor..chunk_byte]);
        let chunk_end_units = chunk_start_units + utf16_len(&chunk);

        let styles = formatted
            .styles
            .iter()
            .filter_map(|style| {
                let start = style.start.max(chunk_start_units);
                let end = (style.start + style.length).min(chunk_end_units);
                (end > start).then(|| TextStyle {
                    start: start - chunk_start_units,
                    length: end - start,
                    style: style.style,
                })
            })
            .collect();

        byte_cursor = chunk_byte + chunk.len();
        unit_cursor = chunk_end_units;
        result.push(FormattedText {
            text: chunk,
            styles,
        });
    }

    result
}

/// Byte offset in `source` where `chunk` begins, given the previous chunk
/// ended at byte `from`.
///
/// `chunk_text` never skips leading text: each chunk is a prefix of what is
/// left, except that a trimmed chunk leaves its trailing whitespace (and the
/// dropped separator) behind. So the gap between `from` and the real start is
/// whitespace only, and the chunk's own first non-whitespace char lines up
/// with the first non-whitespace char at or after `from`.
fn chunk_start(source: &str, from: usize, chunk: &str) -> usize {
    let rest = &source[from..];
    let lead = chunk.len() - chunk.trim_start().len();
    if lead == chunk.len() {
        // Empty or all-whitespace chunk: only an untrimmed split emits one,
        // and those start exactly where the previous chunk ended.
        return from;
    }
    let first_content = from + (rest.len() - rest.trim_start().len());
    if let Some(start) = first_content.checked_sub(lead) {
        if start >= from && source.get(start..).is_some_and(|s| s.starts_with(chunk)) {
            return start;
        }
    }
    tracing::warn!("signal: chunk offset not found at the expected position; searching");
    rest.find(chunk).map_or(from, |pos| from + pos)
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

    /// An unclosed fence whose input ends in two backticks must pass through
    /// verbatim. The closing-fence scan looks at `i`, `i+1`, `i+2`; an
    /// off-by-one in its bounds check (`i + 2 <= len`) indexes one past the
    /// end and panics exactly on this shape.
    #[test]
    fn unclosed_fence_ending_in_two_backticks_passes_through() {
        let result = markdown_to_signal("```\nx``");
        assert_eq!(result.text, "```\nx``");
        assert!(result.styles.is_empty(), "got {:?}", result.styles);
    }

    /// The closing-fence scan must require all THREE backticks at `i..i+3`.
    /// A scan that re-reads `chars[i]` instead of `chars[i+1]` treats a
    /// backtick-nonbacktick-backtick run inside the block as a closer and
    /// truncates the code content.
    #[test]
    fn fence_close_requires_three_consecutive_backticks() {
        let result = markdown_to_signal("```\n`a`\n```");
        assert_eq!(result.text, "`a`");
        assert_eq!(
            result.styles,
            vec![TextStyle {
                start: 0,
                length: 3,
                style: SignalStyle::Monospace,
            }]
        );
    }

    /// Text after a closed fence must survive: the resume index is the close
    /// position plus three, and a scaled index (`i * 3`) overshoots and drops
    /// the tail.
    #[test]
    fn text_after_closed_fence_is_kept() {
        let result = markdown_to_signal("```\nab\n``` tail");
        assert_eq!(result.text, "ab tail");
        assert_eq!(
            result.styles,
            vec![TextStyle {
                start: 0,
                length: 2,
                style: SignalStyle::Monospace,
            }]
        );
    }

    /// A lone `*` inside bold content is content, not a closer: the closing
    /// scan must check the SECOND delimiter char at `i+1`, otherwise `**a*b**`
    /// closes at the single star and swallows the following char.
    #[test]
    fn single_star_inside_bold_content_is_kept() {
        let result = markdown_to_signal("**a*b**");
        assert_eq!(result.text, "a*b");
        assert_eq!(
            result.styles,
            vec![TextStyle {
                start: 0,
                length: 3,
                style: SignalStyle::Bold,
            }]
        );
    }

    /// A doubled `**` inside italic content is skipped by advancing exactly
    /// two chars. A scaled advance (`i *= 2`) jumps past the real closer once
    /// the doubled pair sits at index >= 4 and mis-parses the whole span.
    #[test]
    fn doubled_star_inside_italic_content_advances_by_two() {
        let result = markdown_to_signal("xy*a**b*");
        assert_eq!(result.text, "xya**b");
        assert_eq!(
            result.styles,
            vec![TextStyle {
                start: 2,
                length: 4,
                style: SignalStyle::Italic,
            }]
        );
    }

    /// A style contained entirely in the FIRST chunk must land there and only
    /// there. This pins the overlap test's real span end (`start + length`):
    /// a corrupted span end either drops the style from its own chunk or
    /// leaks it into later chunks with underflowing offsets.
    #[test]
    fn chunk_style_in_first_chunk_only_stays_there() {
        let input = format!("**{}**{}", "b".repeat(5), "a".repeat(7995));
        let formatted = markdown_to_signal(&input);
        assert_eq!(formatted.text.chars().count(), 8000);
        assert_eq!(formatted.styles[0].start, 0);
        assert_eq!(formatted.styles[0].length, 5);

        let chunks = chunk_with_styles(&formatted, 4000);
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            chunks[0].styles,
            vec![TextStyle {
                start: 0,
                length: 5,
                style: SignalStyle::Bold,
            }],
            "the bold span belongs to the first chunk"
        );
        assert!(
            chunks[1].styles.is_empty(),
            "a span ending at char 5 must not leak into the second chunk, got {:?}",
            chunks[1].styles
        );
    }

    /// A zero-length span in the input must be dropped from every chunk —
    /// Signal rejects zero-length style ranges, and multi-chunk remapping is
    /// the last gate before the wire.
    #[test]
    fn chunk_drops_zero_length_styles() {
        let formatted = FormattedText {
            text: "a".repeat(8000),
            styles: vec![TextStyle {
                start: 100,
                length: 0,
                style: SignalStyle::Bold,
            }],
        };
        let chunks = chunk_with_styles(&formatted, 4000);
        assert_eq!(chunks.len(), 2);
        for (i, chunk) in chunks.iter().enumerate() {
            assert!(
                chunk.styles.is_empty(),
                "chunk {i} must not carry a zero-length span, got {:?}",
                chunk.styles
            );
        }
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

    /// The text each style range covers, sliced in UTF-16 code units the way
    /// Signal reads the offsets.
    fn styled_slices(ft: &FormattedText) -> Vec<(SignalStyle, String)> {
        let units: Vec<u16> = ft.text.encode_utf16().collect();
        ft.styles
            .iter()
            .map(|s| {
                let slice = units
                    .get(s.start..s.start + s.length)
                    .unwrap_or_else(|| panic!("style {s:?} out of range for {:?}", ft.text));
                (
                    s.style,
                    String::from_utf16(slice).expect("range splits a surrogate pair"),
                )
            })
            .collect()
    }

    /// Offsets count UTF-16 code units: an astral emoji is two units, so
    /// "🎉 **Done**" styles from unit 3, and every range slices back to the
    /// word the markdown wrapped.
    #[test]
    fn style_offsets_count_utf16_units() {
        let cases: &[(&str, &[(SignalStyle, &str)])] = &[
            ("🎉 **Done**", &[(SignalStyle::Bold, "Done")]),
            ("😀 **bold** 🎉", &[(SignalStyle::Bold, "bold")]),
            (
                "👍🏽 *ok* then `🚀 go`",
                &[
                    (SignalStyle::Italic, "ok"),
                    (SignalStyle::Monospace, "🚀 go"),
                ],
            ),
            ("é **x**", &[(SignalStyle::Bold, "x")]),
        ];
        for (input, want) in cases {
            let ft = markdown_to_signal(input);
            let want: Vec<(SignalStyle, String)> =
                want.iter().map(|(st, t)| (*st, t.to_string())).collect();
            assert_eq!(styled_slices(&ft), want, "input {input:?}");
        }
        let done = markdown_to_signal("🎉 **Done**");
        assert_eq!(
            done.styles,
            vec![TextStyle {
                start: 3,
                length: 4,
                style: SignalStyle::Bold,
            }]
        );
    }

    /// A paragraph split drops the `\n\n` between chunks, so a style in the
    /// second chunk must be rebased on where that chunk really starts — not
    /// on the summed lengths of the trimmed chunks before it. Emoji in the
    /// second chunk also pin the UTF-16 rebasing.
    #[test]
    fn styles_in_later_chunks_point_at_their_text() {
        let input = format!(
            "{}\n\n{} 🎉 **Done** and *also*\n{}\n\n`tail`",
            "a".repeat(3000),
            "b".repeat(1500),
            "c".repeat(3000),
        );
        let ft = markdown_to_signal(&input);
        let chunks = chunk_with_styles(&ft, 4000);
        assert_eq!(
            chunks.len(),
            3,
            "chunks: {:?}",
            chunks.iter().map(|c| c.text.len()).collect::<Vec<_>>()
        );
        assert!(chunks[0].styles.is_empty());
        assert_eq!(
            styled_slices(&chunks[1]),
            vec![
                (SignalStyle::Bold, "Done".to_string()),
                (SignalStyle::Italic, "also".to_string()),
            ]
        );
        assert_eq!(
            styled_slices(&chunks[2]),
            vec![(SignalStyle::Monospace, "tail".to_string())]
        );
    }

    /// A style spanning a dropped separator is clipped into both chunks; the
    /// separator itself carries nothing.
    #[test]
    fn style_spanning_a_paragraph_split_is_clipped_into_both_chunks() {
        let input = format!("**{}\n\n{}**", "a".repeat(3000), "b".repeat(1500));
        let ft = markdown_to_signal(&input);
        let chunks = chunk_with_styles(&ft, 4000);
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            styled_slices(&chunks[0]),
            vec![(SignalStyle::Bold, "a".repeat(3000))]
        );
        assert_eq!(
            styled_slices(&chunks[1]),
            vec![(SignalStyle::Bold, "b".repeat(1500))]
        );
    }

    /// One formatter case: markdown in, plain text out, styled slices out.
    type StyleCase<'a> = (&'a str, &'a str, &'a [(SignalStyle, &'a str)]);

    /// A code span is MONOSPACE only: an enclosing style skips it, and a
    /// delimiter inside it never closes an outer style.
    #[test]
    fn code_spans_carry_monospace_only() {
        let cases: &[StyleCase] = &[
            (
                "**run `cargo test` now**",
                "run cargo test now",
                &[
                    (SignalStyle::Monospace, "cargo test"),
                    (SignalStyle::Bold, "run "),
                    (SignalStyle::Bold, " now"),
                ],
            ),
            // The `**` inside the code span is code, so the bold never closes.
            (
                "**a `b** c`",
                "**a b** c",
                &[(SignalStyle::Monospace, "b** c")],
            ),
            ("*x `y* z`", "*x y* z", &[(SignalStyle::Monospace, "y* z")]),
            (
                "`a **b** c`",
                "a **b** c",
                &[(SignalStyle::Monospace, "a **b** c")],
            ),
            (
                "~~old `code`~~",
                "old code",
                &[
                    (SignalStyle::Monospace, "code"),
                    (SignalStyle::Strikethrough, "old "),
                ],
            ),
        ];
        for (input, want_text, want) in cases {
            let ft = markdown_to_signal(input);
            assert_eq!(ft.text, *want_text, "text for {input:?}");
            let want: Vec<(SignalStyle, String)> =
                want.iter().map(|(st, t)| (*st, t.to_string())).collect();
            assert_eq!(styled_slices(&ft), want, "styles for {input:?}");
        }
    }

    /// A single `*` only italicises when it flanks text: bullets, spaced
    /// arithmetic, and a pair split across lines stay literal.
    #[test]
    fn single_star_italic_requires_flanking() {
        let cases: &[StyleCase] = &[
            ("* apple\n* banana", "* apple\n* banana", &[]),
            ("2 * 3 and 4 * 5", "2 * 3 and 4 * 5", &[]),
            ("a *b\nc* d", "a *b\nc* d", &[]),
            ("*open but spaced *", "*open but spaced *", &[]),
            (
                "*real italic*",
                "real italic",
                &[(SignalStyle::Italic, "real italic")],
            ),
            ("**bold**", "bold", &[(SignalStyle::Bold, "bold")]),
            (
                "* item with *emph* inside",
                "* item with emph inside",
                &[(SignalStyle::Italic, "emph")],
            ),
            (
                "*a * b* and **c**",
                "a * b and c",
                &[(SignalStyle::Italic, "a * b"), (SignalStyle::Bold, "c")],
            ),
        ];
        for (input, want_text, want) in cases {
            let ft = markdown_to_signal(input);
            assert_eq!(ft.text, *want_text, "text for {input:?}");
            let want: Vec<(SignalStyle, String)> =
                want.iter().map(|(st, t)| (*st, t.to_string())).collect();
            assert_eq!(styled_slices(&ft), want, "styles for {input:?}");
        }
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

    /// signal-cli's JSON-RPC `send` parses each `textStyle` entry as a
    /// `start:length:STYLE` string (`(\d+):(\d+):(.+)`); an object is not
    /// read at all. Covers the full enum.
    #[test]
    fn signal_wire_format_is_start_length_style_string() {
        let styles = [
            (SignalStyle::Bold, "3:7:BOLD"),
            (SignalStyle::Italic, "3:7:ITALIC"),
            (SignalStyle::Strikethrough, "3:7:STRIKETHROUGH"),
            (SignalStyle::Monospace, "3:7:MONOSPACE"),
            (SignalStyle::Spoiler, "3:7:SPOILER"),
        ];
        for (style, expected) in styles {
            let ts = TextStyle {
                start: 3,
                length: 7,
                style,
            };
            let json = serde_json::to_value(&ts).unwrap();
            assert_eq!(json, serde_json::json!(expected), "wire form for {style:?}");
        }
    }
}
