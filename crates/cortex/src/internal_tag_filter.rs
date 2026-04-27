/// Strip `<internal>...</internal>` blocks from text to prevent chain-of-thought leakage.
pub fn strip_internal_tags(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(start) = remaining.find("<internal>") {
        result.push_str(&remaining[..start]);
        if let Some(end) = remaining[start..].find("</internal>") {
            remaining = &remaining[start + end + "</internal>".len()..];
        } else {
            // Unclosed tag — strip everything from <internal> onward
            return result;
        }
    }
    result.push_str(remaining);
    result
}

/// Streaming filter that buffers text to strip `<internal>` blocks in real-time.
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
        // Don't emit past an unclosed <internal> tag
        let safe_end = if let Some(pos) = self.raw.rfind("<internal") {
            // Check if this opening tag has a matching close
            if self.raw[pos..].contains("</internal>") {
                cleaned.len()
            } else {
                // Unclosed — only emit up to the tag start in cleaned text
                let raw_before_tag = &self.raw[..pos];
                strip_internal_tags(raw_before_tag).len()
            }
        } else {
            // Also hold back if we might be starting a tag (partial `<inter...`)
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

/// Check if the end of `text` is a partial match for `<internal>`.
pub fn partial_tag_overlap(text: &str) -> usize {
    let tag = "<internal>";
    let text_bytes = text.as_bytes();
    let tag_bytes = tag.as_bytes();
    for len in (1..tag_bytes.len()).rev() {
        if text_bytes.len() >= len && text_bytes[text_bytes.len() - len..] == tag_bytes[..len] {
            return len;
        }
    }
    0
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
}
