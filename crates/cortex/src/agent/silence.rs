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

use crate::constants::{HEARTBEAT_OK_TOKEN, SILENT_REPLY_TOKEN};

/// Sentinel string documented for `schedule.create` tasks: a scheduled run
/// answering with this on its own line suppresses channel delivery. Used for
/// "ping me only when something changed"-style automations.
pub const SILENT_SENTINEL: &str = "[SILENT]";

/// Every marker the model may emit to mean "nothing to deliver".
/// `[SILENT]` is the documented `schedule.create` sentinel, `<SILENT>` the
/// `<silent_replies>` protocol token, `HEARTBEAT_OK` the heartbeat ack.
const SILENT_MARKERS: &[&str] = &[SILENT_SENTINEL, SILENT_REPLY_TOKEN, HEARTBEAT_OK_TOKEN];

/// True when `text` is exactly one silence marker, tolerating trailing
/// punctuation or whitespace the model may append (`[SILENT].`, `<SILENT>!`).
/// Substring matches (prose discussing the marker, `status=[SILENT]`) MUST
/// NOT count: the marker must open the text and only punctuation/whitespace
/// may follow. We can't simply trim trailing punctuation off first, since the
/// markers themselves end in `]` / `>`.
fn is_marker_only(text: &str) -> bool {
    SILENT_MARKERS
        .iter()
        .any(|marker| match text.strip_prefix(marker) {
            Some(rest) => rest
                .chars()
                .all(|c| c.is_ascii_punctuation() || c.is_whitespace()),
            None => false,
        })
}

/// Silence check for autonomous lanes (scheduled tasks, heartbeat): the
/// marker suppresses delivery when it stands alone on the first or last
/// non-empty line, even when the model appended (or prepended) an
/// explanation of why it is staying quiet.
pub fn is_silent_autonomous(text: &str) -> bool {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next();
    let last = lines.next_back();
    first.is_some_and(is_marker_only) || last.is_some_and(is_marker_only)
}

/// Silence check for interactive lanes (gateway channel conversations): only
/// a message that is entirely the marker suppresses delivery.
pub fn is_silent_interactive(text: &str) -> bool {
    is_marker_only(text.trim())
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
            "the answer is 42",
        ] {
            assert!(!is_silent_interactive(s), "{s:?} should deliver");
        }
    }
}
