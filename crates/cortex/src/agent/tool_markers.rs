//! Detection of bare template-scaffolding tokens that local/template-driven
//! backends (ollama and friends) emit as assistant *content* alongside a real
//! tool call.
//!
//! Left in place, such a token is persisted to the transcript, replayed as
//! fallback text on a later empty turn, and preserved through compaction —
//! at which point the model starts imitating it.
//!
//! Forward-only by design: this is applied at the moment an assistant turn is
//! built and is never run retroactively over persisted sessions. Do not add a
//! repair migration — compaction ages contaminated history out on its own,
//! and rewriting stored transcripts to fix a cosmetic artifact is a far worse
//! trade than living with it.

/// Scaffolding token names, in normalized form: lowercased, with a leading
/// `/` stripped and `_`, `-`, `|` removed.
///
/// Deliberately an allowlist rather than a shape heuristic. `[done]`, `[ok]`
/// and `[1]` are legitimate short assistant replies; a rule like "a bracketed
/// word on its own" would eat them.
const SCAFFOLD_TOKENS: &[&str] = &[
    "toolcall",
    "toolcalls",
    "tooluse",
    "tooluses",
    "toolcode",
    "availabletools",
    "tools",
    "functioncall",
    "functioncalls",
    "pythontag",
];

/// Longest input still considered. Belt-and-braces against a long paste that
/// happens to tokenize into bracket pairs.
const MAX_MARKER_LEN: usize = 64;

/// True when `text` consists *entirely* of bracketed scaffolding tokens.
///
/// Every condition is required:
/// 1. non-empty after trimming, and no longer than [`MAX_MARKER_LEN`];
/// 2. the whole string decomposes into bracketed tokens (`[…]`, `<…>`,
///    `<|…|>`, `{{…}}`) separated only by ASCII whitespace — any character
///    outside a bracket pair disqualifies it;
/// 3. each token's inner text is a single identifier (`/?[A-Za-z][\w-]*`),
///    so `[tool call]` and `[note: tool_call failed]` are not markers;
/// 4. each normalized identifier is in [`SCAFFOLD_TOKENS`].
pub(super) fn is_bare_tool_call_marker(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_MARKER_LEN {
        return false;
    }

    let mut rest = trimmed;
    let mut found_any = false;
    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let Some((inner, remainder)) = split_bracketed(rest) else {
            return false;
        };
        if !is_scaffold_identifier(inner) {
            return false;
        }
        found_any = true;
        rest = remainder;
    }
    found_any
}

/// Peel one bracketed token off the front of `s`, returning its inner text and
/// the remainder. `None` when `s` does not start with a supported bracket, or
/// the bracket is unterminated.
fn split_bracketed(s: &str) -> Option<(&str, &str)> {
    // Longest opener first so `<|` wins over `<` and `{{` over `{`.
    for (open, close) in [("<|", "|>"), ("{{", "}}"), ("[", "]"), ("<", ">")] {
        let Some(after_open) = s.strip_prefix(open) else {
            continue;
        };
        let end = after_open.find(close)?;
        let inner = &after_open[..end];
        let remainder = &after_open[end + close.len()..];
        return Some((inner, remainder));
    }
    None
}

/// True when `inner` is a single identifier whose normalized form names a
/// known scaffolding token.
fn is_scaffold_identifier(inner: &str) -> bool {
    let ident = inner.strip_prefix('/').unwrap_or(inner);
    if ident.is_empty() {
        return false;
    }
    let mut chars = ident.chars();
    // Must start with a letter — this is what keeps `[1]` and `[2]` out.
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '|') {
        return false;
    }
    let normalized: String = ident
        .chars()
        .filter(|c| *c != '_' && *c != '-' && *c != '|')
        .flat_map(char::to_lowercase)
        .collect();
    SCAFFOLD_TOKENS.contains(&normalized.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_scaffolding_tokens_are_markers() {
        let cases = [
            "[TOOL_CALLS]",
            "[tool_call]",
            " <tool_call>\n",
            "</tool_call>",
            "<tool_call></tool_call>",
            "<|python_tag|>",
            "[AVAILABLE_TOOLS]",
            "{{tool_code}}",
            "[TOOLS]",
            "[function_call]",
        ];
        for input in cases {
            assert!(
                is_bare_tool_call_marker(input),
                "{input:?} should be recognized as scaffolding"
            );
        }
    }

    #[test]
    fn legitimate_short_text_is_not_a_marker() {
        // The conservatism contract. This is what must fail if someone
        // loosens the rule into a shape heuristic ("a bracketed word").
        let cases = [
            "[done]",
            "[ok]",
            "[x] finished",
            "Calling [tool_call] now",
            "[tool_call] — running the build",
            "[1]",
            "see step [2]",
            "[note: tool_call failed]",
            "[tool call]",
            "",
            "   ",
            "tool_call",
            "<b>tool_call</b>",
        ];
        for input in cases {
            assert!(
                !is_bare_tool_call_marker(input),
                "{input:?} must NOT be treated as scaffolding"
            );
        }
    }

    #[test]
    fn marker_detection_requires_full_coverage() {
        // Rule 2 specifically: real prose around a token disqualifies it,
        // because that prose is content the user should see.
        assert!(!is_bare_tool_call_marker(
            "[TOOL_CALLS] Let me check the file."
        ));
        assert!(!is_bare_tool_call_marker("Result: [TOOL_CALLS]"));
    }

    #[test]
    fn oversized_input_is_never_a_marker() {
        let long = format!("[tool_call]{}", " [tool_call]".repeat(20));
        assert!(long.len() > MAX_MARKER_LEN);
        assert!(!is_bare_tool_call_marker(&long));
    }
}
