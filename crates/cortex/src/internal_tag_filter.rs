/// Tag names whose contents are stripped from assistant text before it is
/// emitted to the UI or persisted. Both prevent chain-of-thought from
/// re-entering context on replay.
const STRIPPED_TAGS: &[&str] = &["internal", "thinking"];

/// Strip `<internal>...</internal>` and `<thinking>...</thinking>` blocks from
/// text to prevent chain-of-thought leakage into the UI or persisted history.
pub fn strip_internal_tags(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;
    loop {
        // Find the earliest opening tag among the set.
        let mut earliest: Option<(usize, &'static str)> = None;
        for tag in STRIPPED_TAGS {
            let open = format!("<{tag}>");
            if let Some(pos) = remaining.find(&open) {
                if earliest.is_none_or(|(p, _)| pos < p) {
                    earliest = Some((pos, tag));
                }
            }
        }
        let Some((start, tag)) = earliest else {
            result.push_str(remaining);
            return result;
        };
        result.push_str(&remaining[..start]);
        let close = format!("</{tag}>");
        if let Some(end) = remaining[start..].find(&close) {
            remaining = &remaining[start + end + close.len()..];
        } else {
            // Unclosed tag — strip everything from the opening tag onward.
            return result;
        }
    }
}

/// Streaming filter that buffers text to strip `<internal>` and `<thinking>`
/// blocks in real-time.
#[derive(Default)]
pub struct InternalTagFilter {
    raw: String,
    emitted_len: usize,
}

impl InternalTagFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append new text and return the portion safe to emit.
    pub fn push(&mut self, delta: &str) -> Option<String> {
        self.raw.push_str(delta);
        let cleaned = strip_internal_tags(&self.raw);

        // If there's an unclosed opening tag somewhere in `raw`, only emit up
        // to that opening position (in cleaned coordinates).
        let mut unclosed_open: Option<usize> = None;
        for tag in STRIPPED_TAGS {
            let open = format!("<{tag}>");
            let close = format!("</{tag}>");
            if let Some(pos) = self.raw.rfind(&open) {
                if !self.raw[pos..].contains(&close) {
                    unclosed_open = Some(match unclosed_open {
                        Some(prev) => prev.min(pos),
                        None => pos,
                    });
                }
            }
        }

        let safe_end = if let Some(pos) = unclosed_open {
            strip_internal_tags(&self.raw[..pos]).len()
        } else {
            // Hold back any trailing partial-open-tag bytes.
            let hold_back = partial_tag_overlap(&self.raw);
            cleaned.len().saturating_sub(hold_back)
        };

        if safe_end > self.emitted_len {
            let new_text = cleaned[self.emitted_len..safe_end].to_string();
            self.emitted_len = safe_end;
            Some(new_text)
        } else {
            None
        }
    }

    /// Flush remaining buffered text (called when stream ends).
    pub fn flush(&mut self) -> Option<String> {
        let cleaned = strip_internal_tags(&self.raw);
        if cleaned.len() > self.emitted_len {
            let remaining = cleaned[self.emitted_len..].to_string();
            self.emitted_len = cleaned.len();
            Some(remaining)
        } else {
            None
        }
    }

    /// Return the full cleaned text.
    pub fn full_clean(&self) -> String {
        strip_internal_tags(&self.raw)
    }
}

/// Longest suffix of `text` that is a non-empty prefix of any stripped open
/// tag (e.g. `<int`, `<thinking`). Used to hold back streaming output when a
/// chunk boundary might be splitting an opening tag.
pub fn partial_tag_overlap(text: &str) -> usize {
    let text_bytes = text.as_bytes();
    let mut best = 0;
    for tag in STRIPPED_TAGS {
        let open = format!("<{tag}>");
        let tag_bytes = open.as_bytes();
        for len in (1..tag_bytes.len()).rev() {
            if text_bytes.len() >= len
                && text_bytes[text_bytes.len() - len..] == tag_bytes[..len]
                && len > best
            {
                best = len;
                break;
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_internal_tags_basic() {
        let input = "Hello <internal>secret thinking</internal> world";
        assert_eq!(strip_internal_tags(input), "Hello  world");
    }

    #[test]
    fn strip_internal_tags_multiple() {
        let input = "A <internal>x</internal> B <internal>y</internal> C";
        assert_eq!(strip_internal_tags(input), "A  B  C");
    }

    #[test]
    fn strip_internal_tags_multiline() {
        let input = "Hello <internal>\nthinking\nacross lines\n</internal> world";
        assert_eq!(strip_internal_tags(input), "Hello  world");
    }

    #[test]
    fn strip_internal_tags_no_tags() {
        let input = "Hello world";
        assert_eq!(strip_internal_tags(input), "Hello world");
    }

    #[test]
    fn strip_internal_tags_unclosed() {
        let input = "Hello <internal>never closed";
        assert_eq!(strip_internal_tags(input), "Hello ");
    }

    #[test]
    fn strip_internal_tags_empty() {
        assert_eq!(strip_internal_tags(""), "");
    }

    #[test]
    fn internal_tag_filter_streaming() {
        let mut filter = InternalTagFilter::new();
        // Simulate streaming: "Hello <internal>secret</internal> world"
        let r1 = filter.push("Hello ");
        assert_eq!(r1, Some("Hello ".to_string()));

        let r2 = filter.push("<internal>sec");
        assert_eq!(r2, None); // buffered, inside tag

        let r3 = filter.push("ret</internal> world");
        assert!(r3.is_some());
        assert_eq!(r3.as_deref(), Some(" world"));
    }

    #[test]
    fn internal_tag_filter_no_tags() {
        let mut filter = InternalTagFilter::new();
        let r = filter.push("Hello world");
        assert_eq!(r, Some("Hello world".to_string()));
    }

    #[test]
    fn partial_tag_overlap_basic() {
        assert_eq!(partial_tag_overlap("text<"), 1);
        assert_eq!(partial_tag_overlap("text<int"), 4);
        assert_eq!(partial_tag_overlap("text<internal"), 9);
        assert_eq!(partial_tag_overlap("text"), 0);
    }

    #[test]
    fn strip_thinking_tags_basic() {
        let input = "Hello <thinking>secret reasoning</thinking> world";
        assert_eq!(strip_internal_tags(input), "Hello  world");
    }

    #[test]
    fn strip_thinking_tags_multiline() {
        let input = "Hello <thinking>\nstep 1\nstep 2\n</thinking> world";
        assert_eq!(strip_internal_tags(input), "Hello  world");
    }

    #[test]
    fn strip_thinking_tags_unclosed() {
        let input = "Hello <thinking>never closed";
        assert_eq!(strip_internal_tags(input), "Hello ");
    }

    #[test]
    fn strip_mixed_internal_and_thinking() {
        // Interleaved tag types should both be stripped, left-to-right.
        let input = "A <thinking>x</thinking> B <internal>y</internal> C";
        assert_eq!(strip_internal_tags(input), "A  B  C");

        let input2 = "<internal>a</internal>X<thinking>b</thinking>Y<internal>c</internal>";
        assert_eq!(strip_internal_tags(input2), "XY");
    }

    #[test]
    fn thinking_filter_streaming() {
        let mut filter = InternalTagFilter::new();
        let r1 = filter.push("Hello ");
        assert_eq!(r1.as_deref(), Some("Hello "));

        let r2 = filter.push("<thinking>thou");
        assert_eq!(r2, None);

        let r3 = filter.push("ghts</thinking> world");
        assert_eq!(r3.as_deref(), Some(" world"));
    }

    #[test]
    fn thinking_filter_streaming_partial_overlap() {
        let mut filter = InternalTagFilter::new();
        // Push a chunk that ends mid-opening-tag for <thinking>.
        let r1 = filter.push("prefix<thin");
        assert_eq!(r1.as_deref(), Some("prefix"));
        let r2 = filter.push("king>secret</thinking>tail");
        assert_eq!(r2.as_deref(), Some("tail"));
    }

    #[test]
    fn partial_tag_overlap_thinking() {
        assert_eq!(partial_tag_overlap("text<thin"), 5);
        assert_eq!(partial_tag_overlap("text<thinking"), 9);
        // `<` alone counts as a 1-char partial regardless of which tag.
        assert_eq!(partial_tag_overlap("text<"), 1);
    }

    #[test]
    fn thinking_filter_streaming_split_close_tag() {
        // The CLOSING tag itself is split across chunk boundaries.
        let mut filter = InternalTagFilter::new();
        let r1 = filter.push("a<thinking>x</think");
        assert_eq!(r1.as_deref(), Some("a"));
        let r2 = filter.push("ing>tail");
        assert_eq!(r2.as_deref(), Some("tail"));
    }

    #[test]
    fn strip_nested_same_tag_outer_close_wins() {
        // Spec: first `</thinking>` closes; trailing `</thinking>` leaks as
        // literal text. LLMs don't nest these in practice; pin the behavior.
        let input = "<thinking><thinking>x</thinking></thinking>tail";
        assert_eq!(strip_internal_tags(input), "</thinking>tail");
    }

    #[test]
    fn strip_preserves_lookalike_tags() {
        // `<think>` (not in the set), `<thinkingaloud>`, `<thinky>` must pass
        // through. Spec: scope is limited to `<thinking>` per ROADMAP item.
        let input = "<think>x</think> <thinkingaloud>y</thinkingaloud> <thinky>z</thinky>";
        assert_eq!(strip_internal_tags(input), input);
    }

    #[test]
    fn strip_preserves_inline_word_thinking() {
        let input = "I was thinking about <internal>x</internal> the problem";
        assert_eq!(
            strip_internal_tags(input),
            "I was thinking about  the problem"
        );

        let plain = "I was thinking about the problem";
        assert_eq!(strip_internal_tags(plain), plain);
    }
}
