//! Helpers for channels that render messages as plain text (SMS, iMessage) and
//! cannot interpret Markdown code fences.
//!
//! Rich-text channels (Telegram, Slack, Discord, Signal, WhatsApp, Teams,
//! Google Chat) handle ``` ``` ``` natively and must NOT call these helpers —
//! they rely on the fence to render output as monospace.

/// Remove a single outer triple-backtick fence if — and only if — it wraps the
/// entire message. Inline or mid-message fences are preserved.
///
/// This is used for plain-text surfaces (SMS, iMessage) where the fence would
/// otherwise appear as literal backticks in the received message.
pub fn strip_outer_code_fence(s: &str) -> String {
    let trimmed = s.trim_matches('\n');

    let Some(after_open) = trimmed.strip_prefix("```") else {
        return s.to_string();
    };
    let Some(body_with_close) = after_open.split_once('\n').map(|(_lang, rest)| rest) else {
        return s.to_string();
    };
    let Some(body) = body_with_close.strip_suffix("```") else {
        return s.to_string();
    };
    let body = body.strip_suffix('\n').unwrap_or(body);

    if body.contains("```") {
        return s.to_string();
    }

    body.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_simple_wrap() {
        let input = "```\nhello\nworld\n```";
        assert_eq!(strip_outer_code_fence(input), "hello\nworld");
    }

    #[test]
    fn strips_wrap_with_language_tag() {
        let input = "```text\nhello\n```";
        assert_eq!(strip_outer_code_fence(input), "hello");
    }

    #[test]
    fn strips_wrap_with_leading_trailing_newlines() {
        let input = "\n```\nbody\n```\n";
        assert_eq!(strip_outer_code_fence(input), "body");
    }

    #[test]
    fn preserves_inline_fence() {
        let input = "before ```inline``` after";
        assert_eq!(strip_outer_code_fence(input), input);
    }

    #[test]
    fn preserves_multiple_fences() {
        let input = "```\nfirst\n```\nmiddle\n```\nsecond\n```";
        assert_eq!(strip_outer_code_fence(input), input);
    }

    #[test]
    fn preserves_unfenced() {
        let input = "just plain text";
        assert_eq!(strip_outer_code_fence(input), input);
    }

    #[test]
    fn preserves_only_opening_fence() {
        let input = "```\nunterminated";
        assert_eq!(strip_outer_code_fence(input), input);
    }
}
