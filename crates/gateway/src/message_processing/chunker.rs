/// Split text into non-empty chunks. Falls back to a single-element vec containing
/// the original text if `chunk_text` returns empty (e.g. for empty input).
pub fn chunk_text_nonempty(text: &str, max_chars: usize) -> Vec<String> {
    let chunks = chunk_text(text, max_chars);
    if chunks.is_empty() {
        if text.is_empty() {
            return vec![];
        }
        vec![text.to_string()]
    } else {
        chunks
    }
}

/// Split text into chunks that fit within `max_chars` (character count, not bytes).
///
/// Splitting priority: paragraph boundary (`\n\n`), line boundary (`\n`),
/// sentence boundary (`. `), then hard character split.
pub fn chunk_text(text: &str, max_chars: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![];
    }
    if max_chars == 0 {
        return vec![text.to_string()];
    }
    if text.chars().count() <= max_chars {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.chars().count() <= max_chars {
            chunks.push(remaining.to_string());
            break;
        }

        // Find the byte offset of the max_chars-th character
        let byte_limit = remaining
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());
        let window = &remaining[..byte_limit];

        // Try paragraph boundary
        if let Some(pos) = window.rfind("\n\n") {
            let split_at = pos + 2;
            chunks.push(remaining[..split_at].trim_end().to_string());
            remaining = &remaining[split_at..];
            continue;
        }

        // Try line boundary
        if let Some(pos) = window.rfind('\n') {
            let split_at = pos + 1;
            chunks.push(remaining[..split_at].trim_end().to_string());
            remaining = &remaining[split_at..];
            continue;
        }

        // Try sentence boundary
        if let Some(pos) = window.rfind(". ") {
            let split_at = pos + 2;
            chunks.push(remaining[..split_at].to_string());
            remaining = &remaining[split_at..];
            continue;
        }

        // Hard split at max_chars (byte_limit is char-aligned)
        chunks.push(remaining[..byte_limit].to_string());
        remaining = &remaining[byte_limit..];
    }

    chunks
}

/// Split markdown into chunks without ever leaving a code fence open.
///
/// [`chunk_text`] is markup-blind: a boundary that lands inside a ``` fence
/// leaves one chunk with an unterminated block and the next starting with
/// orphaned code body — after per-chunk rendering, the platform rejects the
/// malformed markup (Telegram: "can't parse entities"). This variant walks
/// lines and tracks fence state:
///
/// - boundaries between chunks land only outside fences when possible;
/// - a fence block too large for one chunk is split by closing the fence at
///   the chunk end and reopening it (with its info string) at the start of
///   the next chunk, so every chunk renders standalone;
/// - a single overlong line falls back to [`chunk_text`]'s hard split.
pub fn chunk_markdown_fenced(text: &str, max_chars: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![];
    }
    if max_chars == 0 || text.chars().count() <= max_chars {
        return vec![text.to_string()];
    }

    const FENCE: &str = "```";
    // Room reserved for "\n```" so an open fence can always be closed within
    // the budget.
    let close_reserve = FENCE.len() + 1;

    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0usize;
    // `Some(info)` while inside a fence; `info` is the opening line's info
    // string (e.g. "rust") used to reopen the fence in the next chunk.
    let mut open_fence: Option<String> = None;

    let flush = |chunks: &mut Vec<String>,
                 current: &mut String,
                 current_chars: &mut usize,
                 open_fence: &Option<String>| {
        if let Some(info) = open_fence {
            current.push('\n');
            current.push_str(FENCE);
            chunks.push(std::mem::take(current));
            current.push_str(FENCE);
            current.push_str(info);
            *current_chars = current.chars().count();
        } else {
            let flushed = std::mem::take(current);
            let trimmed = flushed.trim_end();
            if !trimmed.is_empty() {
                chunks.push(trimmed.to_string());
            }
            *current_chars = 0;
        }
    };

    // A chunk holding only the reopened-fence scaffold ("```info") must not
    // be flushed: it would emit a degenerate empty code block as its own
    // message.
    let is_bare_scaffold = |current: &str, open_fence: &Option<String>| {
        open_fence
            .as_ref()
            .is_some_and(|info| current == format!("{FENCE}{info}"))
    };

    let lines: Vec<&str> = text.split('\n').collect();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        let is_fence_line = line.trim_start().starts_with(FENCE);
        let line_chars = line.chars().count();

        // A whole fence block that fits inside one chunk is appended intact —
        // never split by the per-line close/reopen reserves below. If it
        // doesn't fit beside the current content but would fit a fresh
        // chunk, flush the prose first and place it there.
        if is_fence_line && open_fence.is_none() {
            if let Some((total, close_off)) = whole_fence_chars(&lines[i..]) {
                let sep = usize::from(!current.is_empty());
                if current_chars + sep + total > max_chars
                    && total <= max_chars
                    && !current.is_empty()
                {
                    flush(&mut chunks, &mut current, &mut current_chars, &open_fence);
                }
                let sep = usize::from(!current.is_empty());
                if current_chars + sep + total <= max_chars {
                    for fence_line in &lines[i..=i + close_off] {
                        if !current.is_empty() {
                            current.push('\n');
                            current_chars += 1;
                        }
                        current.push_str(fence_line);
                        current_chars += fence_line.chars().count();
                    }
                    i += close_off + 1;
                    continue;
                }
                // Too large to fit whole anywhere — fall through to the
                // close/reopen splitting below.
            }
        }

        // Closing fence arriving while the chunk holds only the reopened
        // scaffold: emitting "```info\n```" would be a degenerate empty
        // block (the body already shipped, properly closed, in earlier
        // chunks). Drop the scaffold and the close together.
        if is_fence_line
            && open_fence.is_some()
            && is_bare_scaffold(&current, &open_fence)
            && !current.is_empty()
        {
            current.clear();
            current_chars = 0;
            open_fence = None;
            i += 1;
            continue;
        }
        // +1 for the joining newline when the chunk already has content.
        let sep = usize::from(!current.is_empty());
        // While inside a fence, reserve room for BOTH bracketing sides: the
        // "\n```" close appended to this chunk and the "```info" reopen the
        // next chunk starts with (already counted in current_chars once
        // present, so subtracting it here keeps scaffold + line + close
        // within max_chars even for a line at exactly the budget).
        let reopen_reserve = open_fence
            .as_ref()
            .map(|info| FENCE.len() + info.chars().count() + 1)
            .unwrap_or(0);
        let budget = if open_fence.is_some() || is_fence_line {
            max_chars.saturating_sub(close_reserve + reopen_reserve)
        } else {
            max_chars
        };

        if current_chars + sep + line_chars > budget
            && !current.is_empty()
            && !is_bare_scaffold(&current, &open_fence)
        {
            flush(&mut chunks, &mut current, &mut current_chars, &open_fence);
        }

        // A single line that alone exceeds the budget: hard-split it via
        // chunk_text; the flush's close/reopen bracketing keeps every piece
        // well-formed when this happens inside a fence.
        if line_chars > budget.max(1) {
            if !current.trim().is_empty() && !is_bare_scaffold(&current, &open_fence) {
                flush(&mut chunks, &mut current, &mut current_chars, &open_fence);
            }
            for piece in chunk_text(line, budget.max(1)) {
                if !current.is_empty() {
                    current.push('\n');
                    current_chars += 1;
                }
                current.push_str(&piece);
                current_chars += piece.chars().count();
                flush(&mut chunks, &mut current, &mut current_chars, &open_fence);
            }
            if is_fence_line {
                // Pathological (an overlong fence marker line); treat as a
                // toggle so state stays consistent.
                open_fence = toggle_fence(open_fence, line);
            }
            i += 1;
            continue;
        }

        if !current.is_empty() {
            current.push('\n');
            current_chars += 1;
        }
        current.push_str(line);
        current_chars += line_chars;

        if is_fence_line {
            open_fence = toggle_fence(open_fence, line);
        }
        i += 1;
    }

    if !current.trim().is_empty() {
        // Drop a bare reopened-fence scaffold left when a fence split landed
        // exactly at the end of input — it would render as an empty block.
        let bare_reopen = open_fence
            .as_ref()
            .is_some_and(|info| current.trim() == format!("{FENCE}{info}").trim_end());
        if !bare_reopen {
            chunks.push(current.trim_end().to_string());
        }
    }
    if chunks.is_empty() && !text.is_empty() {
        return vec![text.to_string()];
    }
    chunks
}

/// `(total_chars, close_offset)` of the fence block opening at `lines[0]`:
/// total chars of the block joined with `\n` through its closing ``` line,
/// and that closing line's offset from the opening. `None` when the fence
/// never closes — the caller then falls back to close/reopen splitting.
fn whole_fence_chars(lines: &[&str]) -> Option<(usize, usize)> {
    let close_idx = lines
        .iter()
        .skip(1)
        .position(|l| l.trim_start().starts_with("```"))?
        + 1;
    let chars: usize = lines[..=close_idx]
        .iter()
        .map(|l| l.chars().count())
        .sum::<usize>()
        + close_idx; // joining newlines
    Some((chars, close_idx))
}

/// Fence state transition for a line starting with ```.
fn toggle_fence(open: Option<String>, line: &str) -> Option<String> {
    match open {
        Some(_) => None,
        None => Some(line.trim_start().trim_start_matches('`').trim().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_single_chunk() {
        let chunks = chunk_text("Hello world", 100);
        assert_eq!(chunks, vec!["Hello world"]);
    }

    #[test]
    fn empty_text_returns_empty() {
        let chunks = chunk_text("", 100);
        assert!(chunks.is_empty());
    }

    #[test]
    fn exact_limit_single_chunk() {
        let text = "abcde";
        let chunks = chunk_text(text, 5);
        assert_eq!(chunks, vec!["abcde"]);
    }

    #[test]
    fn paragraph_splitting() {
        let text = "First paragraph.\n\nSecond paragraph.";
        let chunks = chunk_text(text, 25);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "First paragraph.");
        assert_eq!(chunks[1], "Second paragraph.");
    }

    #[test]
    fn line_splitting() {
        let text = "Line one.\nLine two.\nLine three.";
        let chunks = chunk_text(text, 20);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "Line one.\nLine two.");
        assert_eq!(chunks[1], "Line three.");
    }

    #[test]
    fn hard_split_no_boundaries() {
        let text = "abcdefghijklmnop";
        let chunks = chunk_text(text, 5);
        assert_eq!(chunks, vec!["abcde", "fghij", "klmno", "p"]);
    }

    #[test]
    fn sentence_splitting() {
        let text = "First sentence. Second sentence. Third sentence.";
        let chunks = chunk_text(text, 35);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "First sentence. Second sentence. ");
        assert_eq!(chunks[1], "Third sentence.");
    }

    #[test]
    fn multibyte_unicode_no_panic() {
        let text = "Hello \u{1F600}\u{1F600}\u{1F600} world";
        let chunks = chunk_text(text, 8);
        // "Hello 😀😀" = 8 chars, "😀 world" = 7 chars
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "Hello \u{1F600}\u{1F600}");
        assert_eq!(chunks[1], "\u{1F600} world");
    }

    #[test]
    fn cjk_characters() {
        let text = "\u{4F60}\u{597D}\u{4E16}\u{754C}\u{FF01}";
        let chunks = chunk_text(text, 3);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "\u{4F60}\u{597D}\u{4E16}");
        assert_eq!(chunks[1], "\u{754C}\u{FF01}");
    }

    #[test]
    fn zero_max_chars_returns_single_chunk() {
        let chunks = chunk_text("hello world", 0);
        assert_eq!(chunks, vec!["hello world"]);
    }

    #[test]
    fn chunk_text_nonempty_empty_input() {
        let chunks = chunk_text_nonempty("", 100);
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunk_text_nonempty_short_text() {
        let chunks = chunk_text_nonempty("hello", 100);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn chunk_text_nonempty_splits_properly() {
        let text = "First paragraph.\n\nSecond paragraph.";
        let chunks = chunk_text_nonempty(text, 25);
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn single_char_max_produces_single_chars() {
        let chunks = chunk_text("abc", 1);
        assert_eq!(chunks, vec!["a", "b", "c"]);
    }

    #[test]
    fn mixed_boundaries_prefers_paragraph() {
        let text = "Hello world. Nice day.\n\nSecond paragraph.";
        let chunks = chunk_text(text, 30);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "Hello world. Nice day.");
        assert_eq!(chunks[1], "Second paragraph.");
    }

    // ── chunk_markdown_fenced ──
    //
    // Contract: every chunk must be standalone-renderable markdown — an even
    // number of ``` fence markers per chunk, so no chunk opens a block the
    // next chunk is expected to close.

    fn fence_count(s: &str) -> usize {
        s.lines()
            .filter(|l| l.trim_start().starts_with("```"))
            .count()
    }

    #[test]
    fn fenced_short_text_untouched() {
        let text = "hello\n```rust\nlet x = 1;\n```\nworld";
        assert_eq!(chunk_markdown_fenced(text, 100), vec![text]);
    }

    #[test]
    fn fenced_never_splits_inside_small_fence() {
        // Prose forces a split; the fence block fits in one chunk and must
        // arrive intact rather than being cut mid-block.
        let text = "aaaa aaaa aaaa aaaa\n\n```py\nprint(1)\nprint(2)\n```\n\nbbbb";
        let chunks = chunk_markdown_fenced(text, 30);
        assert!(chunks.len() >= 2, "expected a split, got {chunks:?}");
        for chunk in &chunks {
            assert_eq!(
                fence_count(chunk) % 2,
                0,
                "chunk leaves a fence open: {chunk:?}"
            );
        }
        assert!(
            chunks
                .iter()
                .any(|c| c.contains("print(1)") && c.contains("print(2)")),
            "small fence must stay whole: {chunks:?}"
        );
    }

    #[test]
    fn fenced_large_fence_closed_and_reopened_with_info_string() {
        let body: Vec<String> = (0..30).map(|i| format!("line number {i:02}")).collect();
        let text = format!("```rust\n{}\n```", body.join("\n"));
        let chunks = chunk_markdown_fenced(&text, 120);
        assert!(chunks.len() > 1, "expected multi-chunk, got {chunks:?}");
        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(
                fence_count(chunk) % 2,
                0,
                "chunk {i} leaves a fence open: {chunk:?}"
            );
            assert!(
                chunk.chars().count() <= 120,
                "chunk {i} exceeds budget: {} chars",
                chunk.chars().count()
            );
        }
        // Continuation chunks reopen with the original info string.
        for chunk in &chunks[1..] {
            assert!(
                chunk.starts_with("```rust"),
                "continuation must reopen the fence with its info string: {chunk:?}"
            );
        }
        // No content lost.
        let rejoined: String = chunks.join("\n");
        for line in &body {
            assert!(rejoined.contains(line), "lost line {line:?}");
        }
    }

    #[test]
    fn fenced_plain_prose_matches_no_fence_semantics() {
        let text = "para one is here.\n\npara two is here.\n\npara three.";
        let chunks = chunk_markdown_fenced(text, 20);
        assert!(chunks.len() >= 2);
        assert!(chunks.iter().all(|c| !c.contains("```")));
        let rejoined = chunks.join("\n");
        for word in ["para one", "para two", "para three"] {
            assert!(rejoined.contains(word), "lost {word:?} in {chunks:?}");
        }
    }

    #[test]
    fn fenced_empty_and_zero_budget() {
        assert!(chunk_markdown_fenced("", 100).is_empty());
        assert_eq!(chunk_markdown_fenced("abc", 0), vec!["abc"]);
    }

    /// Boundary regression: fence body lines near the budget, combined with a
    /// long info string, must neither push a chunk past `max_chars` (the
    /// reopen scaffold + close fence both count) nor emit a degenerate
    /// "```info\n```" empty-block chunk.
    #[test]
    fn fenced_boundary_lines_with_long_info_string_stay_within_budget() {
        let max = 80;
        let info = "rust,ignore,no_run";
        // Lines sized to crowd the budget from every offset.
        let mut body = String::new();
        for len in [60usize, 70, 55, 68, 40, 72] {
            body.push_str(&"x".repeat(len));
            body.push('\n');
        }
        let text = format!("```{info}\n{body}```");
        let chunks = chunk_markdown_fenced(&text, max);
        assert!(chunks.len() > 1, "expected a split: {chunks:?}");
        for (i, chunk) in chunks.iter().enumerate() {
            assert!(
                chunk.chars().count() <= max,
                "chunk {i} exceeds max ({} chars): {chunk:?}",
                chunk.chars().count()
            );
            assert_eq!(
                fence_count(chunk) % 2,
                0,
                "chunk {i} leaves a fence open: {chunk:?}"
            );
            let stripped: String = chunk
                .lines()
                .filter(|l| !l.trim_start().starts_with("```"))
                .collect();
            assert!(
                !stripped.trim().is_empty(),
                "degenerate empty-fence chunk {i}: {chunk:?}"
            );
        }
        // No body content lost.
        let rejoined = chunks.join("\n");
        assert_eq!(
            rejoined.matches('x').count(),
            60 + 70 + 55 + 68 + 40 + 72,
            "fence body content lost across the split"
        );
    }
}
