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
}
