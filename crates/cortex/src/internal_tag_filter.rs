/// Tag names whose contents are stripped from assistant text before it is
/// emitted to the UI or persisted. All prevent chain-of-thought from
/// re-entering context on replay. Matching is ASCII case-insensitive
/// (`<Thinking>`, `<THINK>`, … are stripped too) — names here must be
/// lowercase. `mm:think` is MiniMax's namespaced reasoning block.
const STRIPPED_TAGS: &[&str] = &["internal", "thinking", "think", "reasoning", "mm:think"];

/// Strip `<internal>`, `<thinking>`, `<think>` and `<reasoning>` blocks
/// (tag names matched ASCII case-insensitively) from text to prevent
/// chain-of-thought leakage into the UI or persisted history.
///
/// A closed block is stripped wherever it sits. An unclosed block is
/// stripped to the end of the text only when its opening tag starts the
/// text or a line (leading spaces/tabs allowed) — the shape of a reasoning
/// preamble the stream cut off. An unclosed tag in the middle of a line is
/// a mention ("use the `<internal>` tag to …") and is kept as written,
/// with the text after it.
pub fn strip_internal_tags(text: &str) -> String {
    scan(text).cleaned
}

/// Result of one pass over the text: the cleaned output, and how many of
/// its leading bytes no later input can change.
struct Scan {
    cleaned: String,
    /// `None`: every block is closed, so all of `cleaned` is final (bar a
    /// trailing partial opening tag). `Some(n)`: an unclosed block is open
    /// and only the first `n` bytes of `cleaned` — the text before it — are
    /// final; a later close would strip what follows.
    unresolved_at: Option<usize>,
}

fn scan(text: &str) -> Scan {
    // ASCII lowercasing is length- and boundary-preserving, so positions
    // found in `lower` index directly into `text`.
    let lower = text.to_ascii_lowercase();
    let mut result = String::with_capacity(text.len());
    let mut unresolved_at = None;
    let mut cursor = 0usize;
    loop {
        // Find the earliest opening tag among the set.
        let rem = &lower[cursor..];
        let mut earliest: Option<(usize, &'static str)> = None;
        for tag in STRIPPED_TAGS {
            let open = format!("<{tag}>");
            if let Some(pos) = rem.find(&open) {
                if earliest.is_none_or(|(p, _)| pos < p) {
                    earliest = Some((pos, tag));
                }
            }
        }
        let Some((rel, tag)) = earliest else {
            result.push_str(&text[cursor..]);
            return Scan {
                cleaned: result,
                unresolved_at,
            };
        };
        let start = cursor + rel;
        result.push_str(&text[cursor..start]);
        let open_len = tag.len() + 2;
        let close = format!("</{tag}>");
        if let Some(end) = lower[start..].find(&close) {
            cursor = start + end + close.len();
            continue;
        }
        // Unclosed. Until the stream ends, a close may still arrive, so
        // nothing from here on is final.
        unresolved_at.get_or_insert(result.len());
        if starts_line(&result, result.len()) {
            // A reasoning block the stream cut off: strip to the end.
            return Scan {
                cleaned: result,
                unresolved_at,
            };
        }
        // A mid-line mention of the tag: keep it and scan on after it.
        result.push_str(&text[start..start + open_len]);
        cursor = start + open_len;
    }
}

/// Whether byte offset `pos` of `text` begins the text or a line, with
/// only spaces or tabs before it on that line.
fn starts_line(text: &str, pos: usize) -> bool {
    text.as_bytes()[..pos]
        .iter()
        .rev()
        .find(|&&b| b != b' ' && b != b'\t')
        .is_none_or(|&b| b == b'\n')
}

/// Streaming filter that buffers text to strip `STRIPPED_TAGS` blocks
/// (`<internal>`, `<thinking>`, `<think>`, `<reasoning>`, any ASCII case)
/// in real-time.
///
/// Text after an unclosed opening tag is held back wherever the tag sits:
/// a later close would strip it, so emitting it early could leak a block.
/// When the stream ends, [`Self::flush`] releases what
/// [`strip_internal_tags`] keeps — a mid-line mention and the text after it.
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
        let Scan {
            cleaned,
            unresolved_at,
        } = scan(&self.raw);

        let safe_end = match unresolved_at {
            Some(pos) => pos,
            None => {
                // Hold back any trailing partial-open-tag bytes.
                let hold_back = partial_tag_overlap(&self.raw);
                cleaned.len().saturating_sub(hold_back)
            }
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

    /// Strips tags over the whole buffer, ignoring any streaming hold-back —
    /// use only once the stream is complete.
    pub fn full_clean(&self) -> String {
        strip_internal_tags(&self.raw)
    }
}

/// Longest suffix of `text` that is a non-empty prefix (ASCII
/// case-insensitive) of any stripped open tag (e.g. `<int`, `<Thinking`).
/// Used to hold back streaming output when a chunk boundary might be
/// splitting an opening tag.
pub fn partial_tag_overlap(text: &str) -> usize {
    let text_bytes = text.as_bytes();
    let mut best = 0;
    for tag in STRIPPED_TAGS {
        let open = format!("<{tag}>");
        let tag_bytes = open.as_bytes();
        for len in (1..tag_bytes.len()).rev() {
            if text_bytes.len() >= len
                && text_bytes[text_bytes.len() - len..].eq_ignore_ascii_case(&tag_bytes[..len])
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
        // An unclosed block strips to the end only when its opening tag
        // starts the text or a line. Mid-line it is a mention of the tag,
        // and the text after it is part of the answer.
        let cases: &[(&str, &str)] = &[
            // Leading unclosed tag: a cut-off reasoning preamble.
            ("<internal>never closed", ""),
            ("<think>plan\nmore plan", ""),
            // Start of a later line, with leading spaces or tabs.
            ("Answer.\n<internal>never closed", "Answer.\n"),
            ("Answer.\n  \t<thinking>never closed", "Answer.\n  \t"),
            // Mid-line mention: kept as written, tail included.
            (
                "Use the <internal> tag to hide notes. Next step: run tests.",
                "Use the <internal> tag to hide notes. Next step: run tests.",
            ),
            (
                "Hello <internal>never closed",
                "Hello <internal>never closed",
            ),
            (
                "Hello <thinking>never closed",
                "Hello <thinking>never closed",
            ),
            // A closed block is still stripped after a mid-line mention.
            (
                "Wrap notes in <think> tags. <internal>x</internal>Done.",
                "Wrap notes in <think> tags. Done.",
            ),
            // A mid-line mention followed by a line-start unclosed block:
            // the mention stays, the cut-off block goes.
            (
                "Use <internal> here.\n<thinking>cut off",
                "Use <internal> here.\n",
            ),
            // A cut-off block right after a stripped one still opens the
            // kept text (or line), so it is reasoning, not a mention.
            ("<think>a</think><think>b", ""),
            ("x\n<i>y</i><internal>z", "x\n<i>y</i><internal>z"),
            ("x\n<think>y</think><internal>z", "x\n"),
        ];
        for (input, expected) in cases {
            assert_eq!(strip_internal_tags(input), *expected, "input={input:?}");
        }
    }

    /// Push `pieces` through a filter, then flush, and return the text the
    /// consumer saw. It must always equal `full_clean()`: the consumer and
    /// the persisted message may never disagree.
    fn stream(pieces: &[&str]) -> (Vec<Option<String>>, String) {
        let mut filter = InternalTagFilter::new();
        let pushed: Vec<Option<String>> = pieces.iter().map(|p| filter.push(p)).collect();
        let mut shown: String = pushed.iter().flatten().cloned().collect();
        shown.push_str(&filter.flush().unwrap_or_default());
        assert_eq!(shown, filter.full_clean(), "pieces={pieces:?}");
        (pushed, shown)
    }

    #[test]
    fn streaming_mid_line_mention_is_held_then_released_at_flush() {
        // The text after a mid-line open tag is held back (a close could
        // still arrive and strip it), then released at the end — not lost.
        let (pushed, shown) = stream(&[
            "Use the <internal> tag ",
            "to hide notes. ",
            "Next step: run tests.",
        ]);
        assert_eq!(pushed[0].as_deref(), Some("Use the "));
        assert_eq!(pushed[1], None, "held back while the tag is unclosed");
        assert_eq!(pushed[2], None);
        assert_eq!(
            shown,
            "Use the <internal> tag to hide notes. Next step: run tests."
        );
    }

    #[test]
    fn streaming_mid_line_open_that_later_closes_never_leaks() {
        // Same opening shape as a mention, but the close arrives: nothing
        // inside the block may reach the consumer.
        let (pushed, shown) = stream(&["Hello <internal>sec", "ret</internal> world"]);
        assert_eq!(pushed[0].as_deref(), Some("Hello "));
        assert_eq!(pushed[1].as_deref(), Some(" world"));
        assert_eq!(shown, "Hello  world");
    }

    #[test]
    fn streaming_cut_off_block_after_a_stripped_block_never_leaks() {
        let (pushed, shown) = stream(&["<think>a</think>", "<think>b", " more"]);
        assert!(pushed.iter().flatten().all(|p| p.is_empty()), "{pushed:?}");
        assert_eq!(shown, "");
    }

    #[test]
    fn streaming_line_start_unclosed_block_is_dropped_at_flush() {
        let (pushed, shown) = stream(&["Answer.\n", "<think>plan the ", "next step"]);
        assert_eq!(pushed[0].as_deref(), Some("Answer.\n"));
        assert_eq!(pushed[1], None);
        assert_eq!(pushed[2], None);
        assert_eq!(shown, "Answer.\n");
    }

    #[test]
    fn streaming_mention_before_a_closed_block_of_another_tag() {
        // A mid-line `<think>` whose close never comes, with a closed
        // `<internal>` block after it: the closed block is stripped, the
        // mention survives, and the stream agrees with the final text.
        let (_, shown) = stream(&["a <think> b ", "<internal>c</internal>", " d"]);
        assert_eq!(shown, "a <think> b  d");
    }

    #[test]
    fn namespaced_reasoning_tag_is_stripped_whole_and_streamed() {
        assert_eq!(
            strip_internal_tags("<mm:think>plan the answer</mm:think>Done."),
            "Done."
        );
        let mut filter = InternalTagFilter::new();
        let mut shown = String::new();
        for piece in ["Hi <mm:", "think>hidden</mm:th", "ink> there"] {
            shown.push_str(&filter.push(piece).unwrap_or_default());
        }
        assert_eq!(shown, "Hi  there");
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
        let input = "<thinking>never closed";
        assert_eq!(strip_internal_tags(input), "");
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
        // Tag matching is exact on the name (up to ASCII case): longer
        // lookalikes like `<thinkingaloud>`, `<thinky>`, `<reason>` and
        // `<reasoned>` must pass through untouched.
        let input = "<thinkingaloud>y</thinkingaloud> <thinky>z</thinky> <reason>w</reason> <reasoned>v</reasoned>";
        assert_eq!(strip_internal_tags(input), input);
    }

    #[test]
    fn namespaced_minimax_reasoning_is_stripped_even_when_split_across_chunks() {
        assert_eq!(strip_internal_tags("A <mm:think>plan</mm:think>B"), "A B");
        let mut filter = InternalTagFilter::new();
        assert_eq!(filter.push("answer <mm:th").as_deref(), Some("answer "));
        assert_eq!(filter.push("ink>private</mm:th").as_deref(), None);
        assert_eq!(filter.push("ink> done").as_deref(), Some(" done"));
    }

    #[test]
    fn strip_think_and_reasoning_tags() {
        let input = "A <think>short form</think> B <reasoning>cot</reasoning> C";
        assert_eq!(strip_internal_tags(input), "A  B  C");
    }

    #[test]
    fn strip_tags_ascii_case_insensitive() {
        // Models emit tag names with arbitrary casing; open and close casing
        // need not even agree. Kept text must preserve its original case.
        let cases: &[(&str, &str)] = &[
            ("Hello <Thinking>secret</Thinking> World", "Hello  World"),
            ("Hello <THINK>secret</think> World", "Hello  World"),
            ("Hello <INTERNAL>secret</Internal> World", "Hello  World"),
            ("Hello <Reasoning>secret</REASONING> World", "Hello  World"),
            // Leading unclosed mixed-case open tag strips to the end.
            ("<ThInKiNg>never closed", ""),
        ];
        for (input, expected) in cases {
            assert_eq!(strip_internal_tags(input), *expected, "input={input:?}");
        }
    }

    #[test]
    fn filter_streaming_mixed_case_split_open_tag() {
        // `<THINK>` split across stream chunks, closed with different casing.
        let mut filter = InternalTagFilter::new();
        let r1 = filter.push("prefix<TH");
        assert_eq!(r1.as_deref(), Some("prefix"));
        let r2 = filter.push("INK>secret</Think> world");
        assert_eq!(r2.as_deref(), Some(" world"));
        assert_eq!(filter.full_clean(), "prefix world");
    }

    #[test]
    fn filter_streaming_reasoning_tag() {
        let mut filter = InternalTagFilter::new();
        let r1 = filter.push("a<reasoning>co");
        assert_eq!(r1.as_deref(), Some("a"));
        let r2 = filter.push("t</reasoning>b");
        assert_eq!(r2.as_deref(), Some("b"));
    }

    #[test]
    fn partial_tag_overlap_case_insensitive() {
        assert_eq!(partial_tag_overlap("text<THIN"), 5);
        assert_eq!(partial_tag_overlap("text<Reasoning"), 10);
        assert_eq!(partial_tag_overlap("text<InT"), 4);
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
