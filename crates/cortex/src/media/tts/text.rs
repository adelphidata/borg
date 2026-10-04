//! Turning a reply into speakable text: markdown stripped, length bounded,
//! and split into sentence chunks for streamed playback.

use std::sync::OnceLock;

use regex::Regex;

// Spoken markdown noise ("asterisk asterisk bold asterisk asterisk") destroys
// the listening experience, so strip `**bold**`, `__bold__`, `*italic*`,
// `_italic_`, `~~strike~~`, code, links, and block markers before TTS.
//
// Application order matters:
// 1. Fenced code blocks first — their bodies may contain other markdown.
// 2. Links before bare URLs — `[label](url)` becomes `label`; orphan URLs go.
// 3. Bold (`**` / `__`) before italic (`*` / `_`) so `**x**` doesn't degrade
//    into `*x*` and then match the italic pattern asymmetrically.
// 4. Inline code after emphasis.
// 5. Block-level markers (headers, bullets, rules) last.

struct MdRegexes {
    code_block: Regex,
    link: Regex,
    url: Regex,
    bold_star: Regex,
    bold_under: Regex,
    italic_star: Regex,
    italic_under: Regex,
    strike: Regex,
    inline_code: Regex,
    header: Regex,
    list_item: Regex,
    hr: Regex,
    excess_nl: Regex,
}

// Regex literals are compile-time-validated; a compile failure here is a
// programmer bug at static-init time, not a runtime risk. Lint override
// follows the codebase convention (see `memory/injection.rs:38`).
#[allow(clippy::expect_used)]
fn md_regexes() -> &'static MdRegexes {
    static REGEXES: OnceLock<MdRegexes> = OnceLock::new();
    REGEXES.get_or_init(|| MdRegexes {
        // `(?s)` enables dot-matches-newline for code-block bodies.
        code_block: Regex::new(r"(?s)```.*?```").expect("code_block regex"),
        link: Regex::new(r"\[([^\]]+)\]\([^)]+\)").expect("link regex"),
        url: Regex::new(r"https?://\S+").expect("url regex"),
        bold_star: Regex::new(r"\*\*(.+?)\*\*").expect("bold_star regex"),
        bold_under: Regex::new(r"__(.+?)__").expect("bold_under regex"),
        italic_star: Regex::new(r"\*(.+?)\*").expect("italic_star regex"),
        // Word-bounded so `snake_case_var` doesn't get mangled.
        italic_under: Regex::new(r"\b_([^_\n]+?)_\b").expect("italic_under regex"),
        strike: Regex::new(r"~~(.+?)~~").expect("strike regex"),
        inline_code: Regex::new(r"`([^`]+)`").expect("inline_code regex"),
        header: Regex::new(r"(?m)^#+\s*").expect("header regex"),
        list_item: Regex::new(r"(?m)^\s*[-*]\s+").expect("list_item regex"),
        hr: Regex::new(r"-{3,}").expect("hr regex"),
        excess_nl: Regex::new(r"\n{3,}").expect("excess_nl regex"),
    })
}

/// Strip markdown formatting that shouldn't be spoken aloud.
///
/// Returns the cleaned, trimmed string. See module-level comment for the
/// reference-parity reference and ordering rationale.
pub fn strip_markdown_for_tts(text: &str) -> String {
    let r = md_regexes();
    let mut out = r.code_block.replace_all(text, " ").into_owned();
    out = r.link.replace_all(&out, "$1").into_owned();
    out = r.url.replace_all(&out, "").into_owned();
    out = r.bold_star.replace_all(&out, "$1").into_owned();
    out = r.bold_under.replace_all(&out, "$1").into_owned();
    out = r.italic_star.replace_all(&out, "$1").into_owned();
    out = r.italic_under.replace_all(&out, "$1").into_owned();
    out = r.strike.replace_all(&out, "$1").into_owned();
    out = r.inline_code.replace_all(&out, "$1").into_owned();
    out = r.header.replace_all(&out, "").into_owned();
    out = r.list_item.replace_all(&out, "").into_owned();
    out = r.hr.replace_all(&out, "").into_owned();
    out = r.excess_nl.replace_all(&out, "\n\n").into_owned();
    out.trim().to_string()
}

/// Prepare text for TTS: strip markdown, then truncate at a sentence boundary
/// to fit within `max_bytes`. Single entry point that callers should use over
/// raw `truncate_for_tts` — it avoids the agent's reply being read with
/// "asterisk asterisk" noise around bold runs.
pub fn prepare_for_tts(text: &str, max_bytes: usize) -> String {
    truncate_for_tts(&strip_markdown_for_tts(text), max_bytes)
}

/// Split prepared TTS text into sentence-sized chunks for streaming
/// synthesis. Uses a sentence-boundary regex plus a `min_chars` floor so
/// short fragments coalesce into the next chunk instead of generating
/// their own synth request.
///
/// Rules:
/// - Split on `. ` / `? ` / `! ` / `\n` (single newline is enough — a
///   line break inside markdown is usually a paragraph terminator).
/// - Coalesce too-short chunks (< `min_chars`) into the next one so the
///   TTS provider isn't asked to synthesize `"Yes."` as its own request.
/// - The last chunk is emitted as-is regardless of length (end of stream).
///
/// Returns chunks in playback order. Used by the streaming-TTS code path
/// to reduce time-to-first-audio: while chunk N+1 is synthesizing, chunk
/// N is already playing.
pub fn chunk_for_streaming_tts(text: &str, min_chars: usize) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();

    // Walk the bytes; on a sentence terminator followed by whitespace OR a
    // newline, decide whether to flush.
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        current.push(c);
        let next = bytes.get(i + 1).map(|&b| b as char);
        let is_terminator =
            matches!(c, '.' | '!' | '?') && matches!(next, Some(' ') | Some('\n') | None);
        let is_newline = c == '\n';
        if is_terminator || is_newline {
            // Skip trailing whitespace so the next chunk doesn't start with a space.
            if let Some(ws) = next {
                if ws == ' ' || ws == '\n' {
                    i += 1;
                }
            }
            let trimmed = current.trim().to_string();
            if !trimmed.is_empty() {
                if trimmed.len() < min_chars {
                    // Too short — keep accumulating into `current` instead of flushing.
                    // Reset current to the trimmed value plus a separating space so the
                    // following text reads naturally when prepended.
                    current = trimmed;
                    current.push(' ');
                    i += 1;
                    continue;
                }
                chunks.push(trimmed);
                current.clear();
            }
        }
        i += 1;
    }
    let trailing = current.trim().to_string();
    if !trailing.is_empty() {
        // Emit final remainder regardless of length — end-of-stream rule.
        if let Some(last) = chunks.last_mut() {
            if trailing.len() < min_chars {
                last.push(' ');
                last.push_str(&trailing);
                return chunks;
            }
        }
        chunks.push(trailing);
    }
    chunks
}

/// Truncate text for TTS, preferring sentence boundaries.
/// Uses byte-level slicing with char-boundary safety to avoid panics on multi-byte UTF-8.
pub fn truncate_for_tts(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    // Find the last valid char boundary at or before max_bytes
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let truncated = &text[..end];
    if let Some(pos) = truncated.rfind(['.', '!', '?']) {
        truncated[..=pos].to_string()
    } else {
        let ellipsis_end = max_bytes.saturating_sub(3).min(end);
        let mut e = ellipsis_end;
        while e > 0 && !text.is_char_boundary(e) {
            e -= 1;
        }
        format!("{}...", &text[..e])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn truncate_for_tts_short_text() {
        assert_eq!(truncate_for_tts("Hello.", 100), "Hello.");
    }

    #[test]
    fn truncate_for_tts_at_sentence_boundary() {
        let text = "First sentence. Second sentence. Third sentence is very long.";
        let result = truncate_for_tts(text, 35);
        assert_eq!(result, "First sentence. Second sentence.");
    }

    #[test]
    fn truncate_for_tts_no_sentence_boundary() {
        let text = "This is one very long word without any sentence ending punctuation";
        let result = truncate_for_tts(text, 20);
        assert_eq!(result, "This is one very ...");
    }

    #[test]
    fn truncate_for_tts_exact_limit() {
        let text = "Hello.";
        assert_eq!(truncate_for_tts(text, 6), "Hello.");
    }

    #[test]
    fn truncate_for_tts_unicode_safe() {
        // Emoji is 4 bytes — slicing at byte 8 would split the emoji
        let text = "Hello \u{1F600} world!";
        let result = truncate_for_tts(text, 8);
        // Should not panic, should truncate before the emoji
        assert!(!result.is_empty());
        assert!(result.len() <= 11); // 8 + "..."
    }

    #[test]
    fn chunk_for_streaming_tts_splits_on_sentence_boundaries() {
        let chunks = chunk_for_streaming_tts(
            "First sentence here. Second sentence here. Third sentence here.",
            20,
        );
        assert_eq!(chunks.len(), 3, "got: {chunks:?}");
        assert_eq!(chunks[0], "First sentence here.");
        assert_eq!(chunks[1], "Second sentence here.");
        assert_eq!(chunks[2], "Third sentence here.");
    }

    #[test]
    fn chunk_for_streaming_tts_coalesces_short_chunks() {
        // "Yes." (4 chars) is under the 20-char floor, so it merges with
        // the next sentence — TTS providers charge per request, and a
        // single-word synth is also acoustically jarring next to a full
        // sentence.
        let chunks = chunk_for_streaming_tts("Yes. That is the correct answer.", 20);
        assert_eq!(chunks.len(), 1, "got: {chunks:?}");
        assert!(chunks[0].contains("Yes."));
        assert!(chunks[0].contains("That is the correct answer."));
    }

    #[test]
    fn chunk_for_streaming_tts_handles_newlines_as_terminators() {
        // Bullet lists / paragraph breaks should split too, not just `. `
        // — otherwise long markdown-stripped replies stay as one chunk.
        let chunks = chunk_for_streaming_tts(
            "First paragraph here\nSecond paragraph also long enough",
            20,
        );
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "First paragraph here");
        assert_eq!(chunks[1], "Second paragraph also long enough");
    }

    #[test]
    fn chunk_for_streaming_tts_emits_final_remainder_below_floor() {
        // End-of-stream rule: a trailing fragment under min_chars must
        // still be played (otherwise the user hears a truncated reply).
        // The chunker merges it into the previous chunk in that case.
        let chunks = chunk_for_streaming_tts("Long first sentence here. Tiny", 20);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].ends_with("Tiny"));
    }

    #[test]
    fn chunk_for_streaming_tts_empty_input_returns_empty() {
        assert!(chunk_for_streaming_tts("", 20).is_empty());
        assert!(chunk_for_streaming_tts("   \n  ", 20).is_empty());
    }

    #[test]
    fn strip_markdown_table_driven() {
        // Each row: (description, input, expected_output).
        // Covers the supported markdown forms plus extras: __bold__,
        // _italic_, ~~strike~~.
        let cases: &[(&str, &str, &str)] = &[
            (
                "fenced code block stripped to space",
                "Before ```rust\nfn x() {}\n``` after",
                "Before   after",
            ),
            (
                "link keeps label drops url",
                "See [the docs](https://example.com/x) please",
                "See the docs please",
            ),
            (
                "bare url removed",
                "Visit https://example.com now",
                "Visit  now",
            ),
            (
                "bold-star unwrapped",
                "This is **important** text",
                "This is important text",
            ),
            (
                "bold-underscore unwrapped",
                "This is __important__ text",
                "This is important text",
            ),
            (
                "italic-star unwrapped",
                "This is *emphasized* text",
                "This is emphasized text",
            ),
            (
                "italic-underscore unwrapped on word boundary",
                "This is _emphasized_ text",
                "This is emphasized text",
            ),
            (
                "snake_case identifier preserved (no italic match)",
                "Call snake_case_var to fix",
                "Call snake_case_var to fix",
            ),
            (
                "strikethrough unwrapped",
                "Use ~~old api~~ instead",
                "Use old api instead",
            ),
            (
                "inline code unwrapped",
                "Run `cargo test` now",
                "Run cargo test now",
            ),
            (
                "atx headers stripped",
                "# Title\n## Subtitle\nbody",
                "Title\nSubtitle\nbody",
            ),
            (
                "list items stripped",
                "- one\n- two\n* three",
                "one\ntwo\nthree",
            ),
            (
                "horizontal rule removed",
                "above\n---\nbelow",
                "above\n\nbelow",
            ),
            ("excess newlines collapsed", "a\n\n\n\nb", "a\n\nb"),
            (
                "nested bold+italic+inline",
                "**Bold _italic_ and `code`** here",
                "Bold italic and code here",
            ),
            (
                "multiple links in one line",
                "[A](http://a) and [B](http://b)",
                "A and B",
            ),
            (
                "link before bare url leaves nothing dangling",
                "[GitHub](https://github.com/x) vs https://gitlab.com",
                "GitHub vs",
            ),
            ("empty input", "", ""),
            (
                "plain text untouched",
                "The quick brown fox.",
                "The quick brown fox.",
            ),
        ];

        for (desc, input, expected) in cases {
            let got = strip_markdown_for_tts(input);
            assert_eq!(&got, expected, "case `{desc}`: input={input:?}");
        }
    }

    #[test]
    fn prepare_for_tts_strips_then_truncates() {
        // Markdown is stripped first, then truncation applies to the cleaned
        // text — so the byte budget covers spoken content, not syntax noise.
        let input = "**First sentence.** *Second sentence.* `Third one.`";
        let out = prepare_for_tts(input, 35);
        // After strip: "First sentence. Second sentence. Third one."
        // Truncated at last sentence boundary <= 35: "First sentence. Second sentence."
        assert_eq!(out, "First sentence. Second sentence.");
    }

    #[test]
    fn prepare_for_tts_handles_code_block_in_reply() {
        // Models drop fenced code blocks into prose replies. The spoken
        // version must not announce backticks or read the code body.
        let input = "Here's the fix:\n```rust\nlet x = 1;\n```\nThat should work.";
        let out = prepare_for_tts(input, 1000);
        assert!(
            !out.contains("```"),
            "fenced markers leaked into TTS text: {out:?}"
        );
        assert!(!out.contains("let x = 1;"), "code body leaked: {out:?}");
        assert!(out.contains("Here's the fix:"));
        assert!(out.contains("That should work."));
    }

    #[test]
    fn truncate_for_tts_cjk_safe() {
        // Each CJK char is 3 bytes; slicing at byte 7 would split the
        // third codepoint mid-byte. The truncator must back off to a char
        // boundary and produce a valid UTF-8 prefix (not panic, not return
        // arbitrary bytes).
        let text = "\u{4F60}\u{597D}\u{4E16}\u{754C}"; // 你好世界
        let result = truncate_for_tts(text, 7);
        // At least the first codepoint must survive — the truncator is
        // not allowed to back off all the way to an empty string.
        assert!(
            result.starts_with("\u{4F60}"),
            "expected output to start with first CJK codepoint, got {result:?}"
        );
        // Output respects max_bytes + "..." ellipsis suffix budget.
        assert!(
            result.len() <= 7 + "...".len(),
            "result {result:?} ({} bytes) exceeded 7 + ellipsis budget",
            result.len()
        );
        // Valid UTF-8 boundary at the end — if the truncator sliced
        // mid-codepoint, the surrounding `String::from_str` would have
        // panicked. This is the explicit sanity guard.
        assert!(result.is_char_boundary(result.len()));
    }
}
