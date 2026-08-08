//! ANSI/terminal escape-sequence stripping for text re-entering model context.
//!
//! Persisted session messages can contain raw terminal escape sequences
//! (colored shell output, cursor moves, OSC title sets). When that text is
//! recalled — session transcript indexing, message FTS snippets — the escape
//! bytes waste tokens and form a mild injection surface. Callers MUST strip
//! *before* truncating so escape bytes never consume the content budget.

use std::borrow::Cow;

/// Strip ANSI escape sequences and stray control characters from `text`.
///
/// Removes:
/// - CSI sequences (`ESC [` parameters `0x20..=0x3f`, one final `0x40..=0x7e`),
///   stopping at — without consuming — the first invalid byte so a malformed
///   sequence can't swallow the printable text that follows.
/// - OSC sequences (`ESC ]` payload terminated by BEL or ST `ESC \`). An
///   unterminated OSC consumes the remainder: that text is sequence payload,
///   not content.
/// - Two/three-byte escapes with intermediates (`ESC ( B` charset selects).
/// - Any other control character except `\n` and `\t` (lone ESC, `\r`,
///   backspace, C1 controls). A lone ESC drops only itself, never the
///   printable character after it.
///
/// Returns a borrow when nothing needs stripping (the common case).
pub fn strip_ansi(text: &str) -> Cow<'_, str> {
    // Fast path: predicate matches exactly what the loop below removes.
    if !text
        .chars()
        .any(|c| c == '\u{1b}' || (c.is_control() && c != '\n' && c != '\t'))
    {
        return Cow::Borrowed(text);
    }
    // Single pass over a peekable char iterator — no intermediate `Vec<char>`
    // (the slow path fires exactly on large ANSI-heavy transcripts, where a
    // 4-bytes-per-char side allocation would dwarf the text itself).
    let mut out = String::with_capacity(text.len());
    let mut it = text.chars().peekable();
    while let Some(ch) = it.next() {
        if ch == '\u{1b}' {
            skip_escape_sequence(&mut it);
        } else if matches!(ch, '\n' | '\t') || !ch.is_control() {
            out.push(ch);
        }
    }
    Cow::Owned(out)
}

/// Consume the remainder of an escape sequence whose introducing ESC has just
/// been taken from `it`. Iterative (not recursive) so an adversarial
/// `ESC ] ESC ] …` chain can't grow the stack: a bare ESC cutting an OSC
/// payload restarts the classification loop as a new sequence.
fn skip_escape_sequence(it: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    loop {
        match it.peek() {
            Some('[') => {
                // CSI: parameter bytes, then one final byte. An invalid final
                // byte is NOT consumed, so a malformed sequence can't swallow
                // the printable text that follows.
                it.next();
                while matches!(it.peek(), Some(c) if ('\u{20}'..='\u{3f}').contains(c)) {
                    it.next();
                }
                if matches!(it.peek(), Some(c) if ('@'..='~').contains(c)) {
                    it.next();
                }
                return;
            }
            Some(']') => {
                // OSC: payload until BEL or ST (`ESC \`). A bare ESC ends the
                // payload and starts a new sequence — reclassify it here.
                it.next();
                loop {
                    match it.next() {
                        None | Some('\u{07}') => return,
                        Some('\u{1b}') => {
                            if it.peek() == Some(&'\\') {
                                it.next();
                                return;
                            }
                            break; // reprocess as a fresh sequence
                        }
                        Some(_) => {}
                    }
                }
            }
            Some(c) if ('\u{20}'..='\u{2f}').contains(c) => {
                // nF escape: intermediates then a single final byte.
                it.next();
                while matches!(it.peek(), Some(c) if ('\u{20}'..='\u{2f}').contains(c)) {
                    it.next();
                }
                if matches!(it.peek(), Some(c) if ('\u{30}'..='\u{7e}').contains(c)) {
                    it.next();
                }
                return;
            }
            // Lone/other ESC: drop the ESC only.
            _ => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_escape_sequence_variants() {
        // (input, expected) — CSI / OSC / bare-ESC / control-char variants.
        let cases: &[(&str, &str)] = &[
            // Plain text untouched.
            ("no escapes here\n\ttabs ok", "no escapes here\n\ttabs ok"),
            // SGR color codes.
            ("\u{1b}[31merror\u{1b}[0m: bad", "error: bad"),
            // Multi-parameter SGR.
            ("\u{1b}[1;32;44mbold\u{1b}[m", "bold"),
            // Cursor movement + erase-line.
            ("line1\u{1b}[2A\u{1b}[Kline2", "line1line2"),
            // OSC window-title, BEL-terminated.
            ("\u{1b}]0;my title\u{07}after", "after"),
            // OSC hyperlink, ST-terminated.
            (
                "\u{1b}]8;;https://example.com\u{1b}\\link\u{1b}]8;;\u{1b}\\",
                "link",
            ),
            // Unterminated OSC swallows its payload (it is payload, not content).
            ("before\u{1b}]0;dangling", "before"),
            // OSC payload cut by a bare ESC that starts a CSI.
            ("\u{1b}]0;t\u{1b}[31mred", "red"),
            // Bare ESC drops only itself, keeping the next printable char.
            ("a\u{1b}b", "ab"),
            // ESC at end of string.
            ("tail\u{1b}", "tail"),
            // nF charset select (ESC ( B).
            ("\u{1b}(Bplain", "plain"),
            // Malformed CSI: invalid final byte is NOT consumed.
            ("\u{1b}[31\u{1b}[0mkeep", "keep"),
            // CSI truncated at end of string.
            ("x\u{1b}[3", "x"),
            // CR / backspace / C1 controls dropped, newline kept.
            ("a\rb\u{8}c\u{9b}d\ne", "abcd\ne"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                strip_ansi(input),
                *expected,
                "input: {:?}",
                input.escape_debug().to_string()
            );
        }
    }

    #[test]
    fn clean_text_borrows_without_allocating() {
        let text = "already clean\nwith\ttabs";
        assert!(matches!(strip_ansi(text), Cow::Borrowed(_)));
    }
}
