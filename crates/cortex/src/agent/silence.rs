//! Intentional-silence detection for agent replies.
//!
//! The system prompt (`<silent_replies>`, the heartbeat protocol) and the
//! `schedule.create` tool description all promise the model that a
//! marker-only reply suppresses delivery. This module is the single place
//! that promise is kept — every delivery lane (scheduled tasks, heartbeat,
//! gateway channels) routes through these helpers so the lanes cannot drift
//! apart on what counts as silence.
//!
//! Two strictness levels exist because the delivery lanes have opposite
//! failure costs:
//!
//! - **Autonomous** (scheduled tasks, heartbeat): nobody is waiting on the
//!   other end, so a leaked "nothing happened" note is a pointless
//!   notification on every tick. Models reliably append an explanation of
//!   their own silence; the explanation has no reader here, so the marker
//!   suppresses on its own first *or* last non-empty line.
//! - **Interactive** (channel conversations): a human asked and is waiting.
//!   Swallowing a real answer because it happens to open with a marker is far
//!   worse than showing a stray marker, so only a message that is *entirely*
//!   the marker suppresses.

use unicode_normalization::UnicodeNormalization;

use crate::constants::{HEARTBEAT_OK_TOKEN, SILENT_REPLY_TOKEN};

/// Sentinel string documented for `schedule.create` tasks: a scheduled run
/// answering with this on its own line suppresses channel delivery. Used for
/// "ping me only when something changed"-style automations.
pub const SILENT_SENTINEL: &str = "[SILENT]";

/// Every marker the model may emit to mean "nothing to deliver".
/// `[SILENT]` is the documented `schedule.create` sentinel, `<SILENT>` the
/// `<silent_replies>` protocol token, `HEARTBEAT_OK` the heartbeat ack.
const SILENT_MARKERS: &[&str] = &[SILENT_SENTINEL, SILENT_REPLY_TOKEN, HEARTBEAT_OK_TOKEN];

/// Format characters a model or a renderer can slip inside a marker without
/// changing how it looks (`[\u{200B}SILENT]`). They are not whitespace, so a
/// trim never removes them.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// The comparison form of a reply: compatibility-folded (full-width brackets
/// become ASCII), invisible characters dropped, and the padding inside a
/// bracket pair removed, so `[ SILENT ]` reads as `[SILENT]`. Used only to
/// decide silence; the delivered text is never rewritten with it.
fn canonical(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    // Inline whitespace whose fate depends on the next visible character.
    let mut pending = String::new();
    let mut after_open = false;
    for c in text.nfkc().filter(|c| !is_invisible(*c)) {
        if c != '\n' && c.is_whitespace() {
            if !after_open {
                pending.push(c);
            }
            continue;
        }
        if !matches!(c, ']' | '>') {
            out.push_str(&pending);
        }
        pending.clear();
        out.push(c);
        after_open = matches!(c, '[' | '<');
    }
    out.push_str(&pending);
    out
}

/// True when `text` is exactly one silence marker, tolerating trailing
/// punctuation or whitespace the model may append (`[SILENT].`, `<SILENT>!`),
/// letter case, and the spellings [`canonical`] folds away.
/// Substring matches (prose discussing the marker, `status=[SILENT]`) MUST
/// NOT count: the marker must open the text and only punctuation/whitespace
/// may follow. We can't simply trim trailing punctuation off first, since the
/// markers themselves end in `]` / `>`.
fn is_marker_only(text: &str) -> bool {
    let text = canonical(text);
    let text = text.trim();
    SILENT_MARKERS.iter().any(|marker| {
        text.get(..marker.len()).is_some_and(|head| {
            // Only a bracketed marker folds case: lowercase
            // `heartbeat_ok` is an ordinary identifier.
            head == *marker || (marker.starts_with(['[', '<']) && head.eq_ignore_ascii_case(marker))
        }) && text[marker.len()..]
            .chars()
            .all(|c| c.is_ascii_punctuation() || c.is_whitespace())
    })
}

/// Markdown a model wraps around the marker: code spans and emphasis.
const WRAPPING: [char; 4] = ['`', '*', '_', '~'];

/// Drop the code-span or emphasis run that opens a line. The closing run is
/// trailing punctuation, which [`is_marker_only`] already tolerates.
/// A run followed by a space is a list bullet (`* item`), not wrapping.
fn peel_wrapping(line: &str) -> &str {
    let line = line.trim();
    let peeled = line.trim_start_matches(WRAPPING);
    if peeled.starts_with(char::is_whitespace) {
        line
    } else {
        peeled
    }
}

/// The single line inside a fenced block that is the whole reply, else the
/// reply unchanged. A fence that holds more than one line is a real code
/// block and stays content.
fn unfence(text: &str) -> &str {
    let Some(body) = text
        .strip_prefix("```")
        .and_then(|rest| rest.strip_suffix("```"))
    else {
        return text;
    };
    match body.split_once('\n') {
        Some((info, inner)) if !info.contains('`') && !inner.trim().contains('\n') => inner.trim(),
        _ => text,
    }
}

/// True when `a` and `b` differ by at most one inserted, removed or replaced
/// character.
fn within_one_edit(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (short, long) = if a.len() <= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    if long.len() - short.len() > 1 {
        return false;
    }
    let same = short
        .iter()
        .zip(long.iter())
        .take_while(|(x, y)| x == y)
        .count();
    if same == short.len() {
        return true;
    }
    if short.len() == long.len() {
        short[same + 1..] == long[same + 1..]
    } else {
        short[same..] == long[same + 1..]
    }
}

/// True when the WHOLE reply is one damaged `[SILENT]`: a bracketed word one
/// letter edit away (`[SLENT]`, `[SLIENT]` is two and stays content), or a
/// marker the provider cut off before the closing bracket (`[`, `[SIL`,
/// `[SILENT`). A second word, or a bracketed word further away (`[URGENT]`,
/// `[SILENCE]`), is a real message.
fn is_damaged_marker(reply: &str) -> bool {
    let reply =
        peel_wrapping(reply).trim_end_matches(|c: char| WRAPPING.contains(&c) || c.is_whitespace());
    let Some(inner) = reply.strip_prefix('[') else {
        return false;
    };
    // Sentence punctuation can follow a closed marker only: `[...` is not a
    // cut-off marker.
    let inner = match inner.trim_end_matches(['.', '!']) {
        closed if closed.ends_with(']') => closed,
        _ => inner,
    };
    if !inner.chars().all(|c| c.is_alphabetic() || c == ']') {
        return false;
    }
    let target = "silent";
    match inner.strip_suffix(']') {
        Some(word) => !word.contains(']') && within_one_edit(&word.to_lowercase(), target),
        None => !inner.contains(']') && target.starts_with(&inner.to_lowercase()),
    }
}

/// Silence check for autonomous lanes (scheduled tasks, heartbeat): the
/// marker suppresses delivery when it stands alone on the first or last
/// non-empty line, even when the model appended (or prepended) an
/// explanation of why it is staying quiet. Nobody reads these replies as
/// they arrive, so a marker in code formatting or emphasis, and a whole
/// reply that is one damaged marker, are silence too.
pub fn is_silent_autonomous(text: &str) -> bool {
    let text = canonical(text);
    let text = unfence(text.trim());
    if is_damaged_marker(text) {
        return true;
    }
    // Peel after the empty-line filter: a fence line is all wrapping, and it
    // must stay a line of its own so the code under it is not read as first.
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next();
    let last = lines.next_back();
    let is_marker_line = |line: &str| is_marker_only(peel_wrapping(line));
    first.is_some_and(is_marker_line) || last.is_some_and(is_marker_line)
}

/// Silence check for interactive lanes (gateway channel conversations): only
/// a message that is entirely the marker suppresses delivery.
pub fn is_silent_interactive(text: &str) -> bool {
    is_marker_only(text.trim())
}

/// Drop a silence marker the model appended beneath a real answer.
///
/// The prompt says the marker must never follow content, but models still
/// close a reply with it ("Here's the summary.\n\n<SILENT>"). Interactive
/// lanes deliver such a reply (see [`is_silent_interactive`]), so without
/// this pass the recipient sees the protocol token verbatim. Only a marker
/// standing alone on the last non-empty line is removed; a marker inside a
/// line is prose and stays. A marker-only reply is returned unchanged so the
/// suppression check above keeps its say.
pub fn strip_trailing_silence_marker(text: &str) -> String {
    let trimmed = text.trim_end();
    let Some(last_line_start) = trimmed.rfind('\n').map(|i| i + 1) else {
        return text.to_string();
    };
    if !is_marker_only(trimmed[last_line_start..].trim()) {
        return text.to_string();
    }
    let body = trimmed[..last_line_start].trim_end();
    if body.is_empty() {
        return text.to_string();
    }
    body.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autonomous_suppresses_marker_forms() {
        for s in [
            "[SILENT]",
            "  [SILENT]  ",
            "[SILENT].",
            "[SILENT]!",
            "\n\n[SILENT]\n",
            "<SILENT>",
            "<SILENT>.",
            "HEARTBEAT_OK",
            "HEARTBEAT_OK.",
            // Marker first, explanation after — the shape models commonly
            // emit on autonomous lanes.
            "[SILENT]\nbackground notes go here",
            "<SILENT>\nnothing new on the ticket since the last tick",
            // Explanation first, marker on its own last line.
            "Nothing actionable changed since the last run.\n[SILENT]",
        ] {
            assert!(is_silent_autonomous(s), "{s:?} should suppress delivery");
        }
    }

    #[test]
    fn autonomous_delivers_real_content() {
        for s in [
            "",
            "Deployed successfully.",
            // Marker sharing a line with prose is not a marker line.
            "[SILENT] but actually here's an update",
            "The status is [SILENT] according to the log",
            "status=[SILENT]",
            "we discussed the <SILENT> token today",
            // Marker buried mid-message (neither first nor last line).
            "intro\n[SILENT]\nreal content follows",
        ] {
            assert!(!is_silent_autonomous(s), "{s:?} should deliver");
        }
    }

    #[test]
    fn interactive_suppresses_only_marker_only_messages() {
        for s in ["[SILENT]", "  <SILENT>  ", "[SILENT].", "HEARTBEAT_OK"] {
            assert!(is_silent_interactive(s), "{s:?} should suppress delivery");
        }
        for s in [
            "",
            // A marker followed by anything substantive is a real answer on an
            // interactive lane — deliver it rather than risk swallowing it.
            "[SILENT]\nactually, here is the summary you asked for",
            "[SILENT] here's the update",
            "[SILENT]\r\nreal answer",
            "the answer is 42",
        ] {
            assert!(!is_silent_interactive(s), "{s:?} should deliver");
        }
    }

    #[test]
    fn trailing_marker_under_real_text_is_stripped() {
        for (input, expected) in [
            ("Here's the answer.\n\n<SILENT>", "Here's the answer."),
            ("Here's the answer.\n[SILENT]\n", "Here's the answer."),
            (
                "line one\nline two\n  HEARTBEAT_OK.  ",
                "line one\nline two",
            ),
            ("done\n<SILENT>!", "done"),
        ] {
            assert_eq!(strip_trailing_silence_marker(input), expected, "{input:?}");
        }
    }

    #[test]
    fn marker_inside_a_line_or_alone_is_kept() {
        for s in [
            "",
            "the answer is 42",
            "[SILENT]",
            "  <SILENT>  ",
            "\n\n[SILENT]\n",
            "status=[SILENT]",
            "we discussed the <SILENT> token today",
            "done <SILENT>",
            "<SILENT>\nhere is the summary",
            "intro\n[SILENT]\nreal content follows",
        ] {
            assert_eq!(strip_trailing_silence_marker(s), s, "{s:?}");
        }
    }

    #[test]
    fn padded_invisible_and_case_spellings_are_the_marker() {
        for s in [
            "[ SILENT ]",
            "[SILENT ]",
            "[ SILENT]",
            "[\u{200B}SILENT]",
            "\u{FEFF}[SILENT]\u{200B}",
            "[silent]",
            "< SILENT >",
            "\u{FF3B}SILENT\u{FF3D}",
        ] {
            assert!(is_silent_autonomous(s), "{s:?} autonomous");
            assert!(is_silent_interactive(s), "{s:?} interactive");
        }
        assert_eq!(
            strip_trailing_silence_marker("Here's the answer.\n[ SILENT ]"),
            "Here's the answer."
        );
    }

    #[test]
    fn autonomous_reads_a_wrapped_marker_as_silence() {
        for s in [
            "`[SILENT]`",
            "``[SILENT]``",
            "```\n[SILENT]\n```",
            "```text\n[SILENT]\n```",
            "**[SILENT]**",
            "_[SILENT]_",
            "`[SILENT]`\nnothing changed since the last run",
            "Nothing changed.\n`[SILENT]`",
        ] {
            assert!(is_silent_autonomous(s), "{s:?} should suppress delivery");
            // A person is waiting on an interactive lane: only the plain
            // marker is silence there.
            assert!(!is_silent_interactive(s), "{s:?} should deliver");
        }
        for s in [
            // A real code block, even one whose first line is the marker.
            "```\n[SILENT]\nlet x = 1;\n```",
            "```\nlet x = 1;\n```",
            // A list bullet is not emphasis around the marker.
            "* [SILENT]\n* disk at 91%",
            // Lowercase `heartbeat_ok` is an identifier, not the ack.
            "Failing tests:\nheartbeat_ok",
            "The job printed `[SILENT]` twice",
            "`[SILENT]` is what the last run returned",
        ] {
            assert!(!is_silent_autonomous(s), "{s:?} should deliver");
        }
    }

    #[test]
    fn autonomous_reads_a_damaged_marker_as_silence() {
        for s in [
            "[",
            "[S",
            "[SIL",
            "[SILENT",
            "[SLENT]",
            "[SILEN]",
            "[SILENTT]",
            "[SIKENT]",
            "`[SLENT]`",
            "[ SLENT ]",
            "[silnt].",
        ] {
            assert!(is_silent_autonomous(s), "{s:?} should suppress delivery");
            assert!(!is_silent_interactive(s), "{s:?} should deliver");
        }
        for s in [
            "[URGENT]",
            "[SILVER]",
            "[SILENCE]",
            "[SLIENT]",
            "[OK]",
            "[]",
            "[SLENT] disk is full",
            "[SIL the build broke",
            "[1]",
            "[...",
            "[!",
            "[ ]",
            "日本語のお知らせ",
            "\u{200B}\n\u{200B}",
            "S",
        ] {
            assert!(!is_silent_autonomous(s), "{s:?} should deliver");
        }
    }

    #[test]
    fn one_edit_distance_is_exact() {
        for (a, b, want) in [
            ("silent", "silent", true),
            ("slent", "silent", true),
            ("silentt", "silent", true),
            ("sikent", "silent", true),
            ("", "s", true),
            ("slient", "silent", false),
            ("silence", "silent", false),
            ("sil", "silent", false),
        ] {
            assert_eq!(within_one_edit(a, b), want, "{a:?} vs {b:?}");
        }
    }
}
