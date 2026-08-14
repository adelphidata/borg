//! Detecting and resuming an interrupted assistant turn.
//!
//! When a turn is cancelled, the stream errors, or streamed tool calls fail
//! validation, the agent persists whatever text arrived with a trailing marker
//! line so the record shows the response was cut short. See
//! [`super::Agent::run_agent_loop`] and [`super::stream_handler`] for the
//! emission sites.
//!
//! The marker is a record, not context the model should continue *after*. On
//! resume the marker is dropped from the in-memory history only, so the model
//! carries on from its own partial sentence, while the persisted row keeps the
//! marker as the audit record of what happened. The two deliberately differ for
//! that one message.

/// Line prefixes the agent uses to mark a turn that did not finish.
const INTERRUPTION_PREFIXES: &[&str] = &[
    "[response interrupted",
    "[stream interrupted",
    "[incomplete tool calls discarded",
];

/// Whether `line` is one of the agent's interruption markers.
fn is_marker_line(line: &str) -> bool {
    let line = line.trim();
    line.ends_with(']') && INTERRUPTION_PREFIXES.iter().any(|p| line.starts_with(p))
}

/// Collapse an error into a single line for embedding in a marker.
///
/// A marker is recognized by its *last* line, so a provider error carrying a
/// newline — plenty of transport and HTTP errors do — would produce a marker
/// whose tail no longer looks like one. The turn would then be unresumable on
/// exactly the failure this feature exists for.
pub(super) fn one_line(err: &str) -> String {
    err.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Strip a trailing interruption marker from assistant text.
///
/// Returns `None` when the text does not end in a marker — that is the signal
/// that there is no interrupted turn to resume. Returns `Some("")` when the
/// marker was the whole message, meaning nothing partial survived.
pub(super) fn strip_interruption_marker(text: &str) -> Option<String> {
    let trimmed = text.trim_end();
    let (head, last_line) = match trimmed.rfind('\n') {
        Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
        None => ("", trimmed),
    };
    if !is_marker_line(last_line) {
        return None;
    }
    Some(head.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec: every marker the agent can emit must be recognized. A marker the
    /// stripper misses would be fed back to the model as if it were the
    /// assistant's own words, and the model would answer the marker.
    #[test]
    fn every_emitted_marker_is_recognized_and_removed() {
        let cases = [
            "Here is the plan\n\n[response interrupted]",
            "Here is the plan\n\n[stream interrupted: connection reset]",
            "Here is the plan\n\n[stream interrupted: eof; incomplete tool calls discarded]",
            "Here is the plan\n\n[incomplete tool calls discarded]",
        ];
        for case in cases {
            assert_eq!(
                strip_interruption_marker(case).as_deref(),
                Some("Here is the plan"),
                "marker not stripped from {case:?}"
            );
        }
    }

    /// Spec: a provider error carrying a newline must not break marker
    /// recognition. The stripper reads the last line, so an un-flattened error
    /// would leave a tail that does not look like a marker, and `/resume-turn`
    /// would refuse the exact turn it exists to rescue.
    #[test]
    fn a_multi_line_provider_error_still_produces_a_recognizable_marker() {
        let err = "connection reset\n  caused by: broken pipe";
        let content = format!(
            "Here is the plan\n\n[stream interrupted: {}]",
            one_line(err)
        );
        assert_eq!(
            strip_interruption_marker(&content).as_deref(),
            Some("Here is the plan")
        );
    }

    /// Spec: a marker-only message strips to empty rather than reporting "not
    /// interrupted". The turn *was* interrupted; there is simply no partial
    /// text to continue from, and the caller decides what to do about that.
    #[test]
    fn a_marker_only_message_strips_to_nothing() {
        assert_eq!(
            strip_interruption_marker("[response interrupted — [stream interrupted: eof]]")
                .as_deref(),
            Some("")
        );
    }

    /// Spec: a complete answer must not be treated as resumable, even when it
    /// happens to talk about interruptions or ends in a bracketed line.
    #[test]
    fn a_complete_answer_is_not_resumable() {
        for case in [
            "All done.",
            "The stream interrupted the download, so I retried.",
            "See [1] for details.",
            "",
        ] {
            assert_eq!(
                strip_interruption_marker(case),
                None,
                "{case:?} must not look interrupted"
            );
        }
    }

    /// Spec: only a *trailing* marker counts. An earlier turn's marker sits
    /// mid-message after compaction, and resuming from it would silently delete
    /// everything the model said afterwards.
    #[test]
    fn a_marker_that_is_not_last_is_left_alone() {
        assert_eq!(
            strip_interruption_marker("[response interrupted]\n\nI carried on anyway."),
            None
        );
    }
}
