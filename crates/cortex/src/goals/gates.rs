//! Quality gates: deterministic checks a goal must pass before it completes.
//!
//! A goal's completion is otherwise declared by the model, which can be wrong
//! or optimistic — a Ralph loop will happily call "fix the failing tests" done
//! against a red suite. A gate is a shell command attached to the goal that
//! must exit 0, evaluated at the turn boundary before any continuation is
//! injected. A failing gate does two things: it blocks completion, and its exit
//! code plus output tail become the continuation prompt, so the next turn
//! argues with concrete evidence instead of a prose verdict.
//!
//! Gates run only at the turn boundary, never mid-turn, so `/goal gate …`
//! management is always safe to issue while the agent is working.

use crate::db::GoalGate;

/// Bytes of gate output fed back to the model.
///
/// The tail, not the head: a failing test suite prints its summary and the
/// first failure detail last, while the head is usually compilation noise.
/// ~3 KB is enough for a few failures without crowding out the conversation.
const GATE_OUTPUT_TAIL_BYTES: usize = 3072;

/// What the gate pass decided for this turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    /// No gates configured, or every gate passed. The goal may be judged
    /// complete and the loop continues normally.
    AllPassed,
    /// A gate failed and still has retries left. Carries the continuation
    /// prompt built from its evidence.
    Failed {
        /// 1-based gate ordinal.
        ordinal: i64,
        /// Prompt to inject instead of the ordinary continuation.
        prompt: String,
    },
    /// A gate exhausted its retry budget. The caller pauses the goal — the
    /// agent has demonstrated it cannot satisfy this check on its own.
    Exhausted {
        /// 1-based gate ordinal.
        ordinal: i64,
        /// Operator-facing explanation.
        message: String,
    },
}

/// Keep the last [`GATE_OUTPUT_TAIL_BYTES`] of `output`, trimmed to a char
/// boundary so the result stays valid UTF-8.
pub(crate) fn output_tail(output: &str) -> &str {
    if output.len() <= GATE_OUTPUT_TAIL_BYTES {
        return output;
    }
    let start = output.len() - GATE_OUTPUT_TAIL_BYTES;
    let start = (start..output.len())
        .find(|i| output.is_char_boundary(*i))
        .unwrap_or(output.len());
    &output[start..]
}

/// Continuation prompt for a failed gate.
///
/// Deliberately concrete: the command, its exit code, and its real output. The
/// point of a gate over an LLM judge is that the agent iterates against the
/// actual failure rather than a summary of it.
pub(crate) fn failure_prompt(gate: &GoalGate, exit_code: i64, output: &str) -> String {
    let tail = output_tail(output);
    let body = if tail.trim().is_empty() {
        "(the command produced no output)"
    } else {
        tail
    };
    format!(
        "A quality gate for this goal is failing, so the goal is not complete.\n\n\
         <gate_failure ordinal=\"{ordinal}\" exit_code=\"{exit_code}\">\n\
         <command>{command}</command>\n\
         <output>\n{body}\n</output>\n\
         </gate_failure>\n\n\
         Fix the underlying problem so this command exits 0. Do not modify or \
         remove the gate itself, and do not claim the goal is complete while it \
         is failing.",
        ordinal = gate.ordinal,
        command = gate.command,
    )
}

/// Why this gate command's exit code could never prove the check passed, or
/// `None` when the status is the work's own.
///
/// A gate is only worth anything because exit 0 means the check passed. A
/// command ending in `| tee log` or `|| true` exits 0 no matter what happened,
/// so it is a gate that always passes — worse than no gate, because it reads
/// like verification. The message names the shape and the one-line fix.
pub(crate) fn unverifiable_reason(command: &str) -> Option<String> {
    let shape = crate::policy::masked_exit_shape(command)?;
    Some(format!(
        "this gate can never fail: {}. A gate passes on exit 0, so it would pass \
         however the check went. Drop the trailing pipe or `||` and let the command \
         report its own status.",
        shape.describe()
    ))
}

/// Message shown when a gate burns through its retry budget.
pub(crate) fn exhausted_message(gate: &GoalGate, max_attempts: i64) -> String {
    format!(
        "Goal paused: quality gate {} (`{}`) failed {} times in a row. \
         Fix it manually, drop it with `/goal gate remove {}`, then `/goal resume`.",
        gate.ordinal, gate.command, max_attempts, gate.ordinal
    )
}

/// Whether this run shows that the agent did work since the last run.
///
/// This feeds the retry cap only. It never decides whether a gate runs: a
/// gate runs at every turn boundary, because no cheap signal can prove that
/// its result would be the same.
///
/// Progress is a workspace that visibly changed, or a different exit code.
/// The output text is deliberately not compared: most test runners print
/// timings, so the text differs on every run and the cap would never fire
/// for an agent that is stuck. When nothing can be compared (first run, or
/// no git repository and the same exit code) the run counts as no progress,
/// since the alternative is again a cap that never fires.
pub(crate) fn made_progress(gate: &GoalGate, fingerprint: Option<&str>, exit_code: i64) -> bool {
    let workspace_changed = matches!(
        (fingerprint, gate.last_fingerprint.as_deref()),
        (Some(now), Some(before)) if now != before
    );
    let verdict_changed = gate.last_exit_code.is_some_and(|last| last != exit_code);
    workspace_changed || verdict_changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(ordinal: i64) -> GoalGate {
        GoalGate {
            session_id: "s".into(),
            ordinal,
            command: "cargo test".into(),
            attempts: 0,
            last_exit_code: None,
            last_output: None,
            last_fingerprint: None,
            created_at: 0,
        }
    }

    #[test]
    fn failure_prompt_carries_the_real_evidence() {
        // The whole reason a gate beats an LLM judge is that the agent gets the
        // actual failure text. A prompt that only said "a gate failed" would
        // leave it guessing.
        let p = failure_prompt(&gate(2), 101, "test tests::foo ... FAILED\n1 failed");
        assert!(p.contains("cargo test"), "must name the command: {p}");
        assert!(p.contains("101"), "must carry the exit code: {p}");
        assert!(p.contains("tests::foo"), "must carry the output: {p}");
    }

    #[test]
    fn failure_prompt_survives_a_silent_command() {
        // A command that fails with no output must still produce a usable
        // prompt rather than an empty <output> block the model reads as noise.
        let p = failure_prompt(&gate(1), 1, "   \n  ");
        assert!(p.contains("no output"), "{p}");
    }

    #[test]
    fn output_tail_keeps_the_end_and_stays_utf8() {
        // Test runners put the summary and the first real failure at the end,
        // so the tail is the informative half. Multi-byte characters near the
        // cut must not be split.
        let long = format!("{}✅ done", "x".repeat(GATE_OUTPUT_TAIL_BYTES * 2));
        let tail = output_tail(&long);
        assert!(tail.ends_with("✅ done"));
        assert!(tail.len() <= GATE_OUTPUT_TAIL_BYTES);
        // Short output is returned whole.
        assert_eq!(output_tail("short"), "short");
    }

    #[test]
    fn progress_needs_a_visible_change() {
        // (last_fingerprint, last_exit_code, fingerprint now, exit code now, progress)
        type Case = (
            Option<&'static str>,
            Option<i64>,
            Option<&'static str>,
            i64,
            bool,
        );
        let cases: &[Case] = &[
            // The workspace changed.
            (Some("abc"), Some(1), Some("def"), 1, true),
            // Nothing visible changed: the agent is stuck.
            (Some("abc"), Some(1), Some("abc"), 1, false),
            // A different failure is a different state, also outside git.
            (Some("abc"), Some(101), Some("abc"), 1, true),
            (None, Some(101), None, 1, true),
            // An unknown fingerprint on either side is not a change.
            (None, Some(1), Some("abc"), 1, false),
            (Some("abc"), Some(1), None, 1, false),
            (None, Some(1), None, 1, false),
            // The first run has nothing to compare with.
            (None, None, Some("abc"), 1, false),
        ];
        for (last_fp, last_exit, now_fp, exit_code, expected) in cases {
            let mut g = gate(1);
            g.last_fingerprint = last_fp.map(str::to_string);
            g.last_exit_code = *last_exit;
            assert_eq!(
                made_progress(&g, *now_fp, *exit_code),
                *expected,
                "last=({last_fp:?}, {last_exit:?}) now=({now_fp:?}, {exit_code})"
            );
        }
    }

    /// A gate exists to make exit 0 mean "the check passed". A command whose
    /// status the shell takes from a pipe consumer or a `||` fallback passes
    /// unconditionally, so it must be rejected rather than trusted — this is
    /// the shape a model reaches for after being told to `tee` its test runs.
    #[test]
    fn always_passing_gate_commands_are_rejected() {
        // (command, must be rejected)
        let cases: &[(&str, bool)] = &[
            ("cargo test 2>&1 | tee /tmp/borg-test.log", true),
            ("cargo test | tail -20", true),
            ("cargo test || true", true),
            ("cargo clippy -- -D warnings || echo failed", true),
            // Verifiable gates: the status is the check's own.
            ("cargo test", false),
            ("cargo fmt --check && cargo clippy -- -D warnings", false),
            ("cargo test 2>&1", false),
            // A pipe into a real predicate still decides pass/fail honestly.
            ("cargo test | grep -q 'test result: ok'", false),
        ];
        for (command, rejected) in cases {
            assert_eq!(
                unverifiable_reason(command).is_some(),
                *rejected,
                "wrong verdict for gate command {command:?}"
            );
        }
    }

    /// The rejection has to be actionable: it names why the gate is useless and
    /// what to change, or the operator just re-adds the same command.
    #[test]
    fn rejection_explains_the_shape_and_the_fix() {
        let reason = unverifiable_reason("cargo test | tee log").expect("rejected");
        assert!(reason.contains("`tee`"), "must name the consumer: {reason}");
        assert!(
            reason.contains("never fail"),
            "must say why the gate is useless: {reason}"
        );
        assert!(
            reason.contains("pipe"),
            "must name what to remove: {reason}"
        );
    }

    #[test]
    fn exhausted_message_tells_the_operator_how_to_get_unstuck() {
        let m = exhausted_message(&gate(3), 3);
        assert!(m.contains("/goal gate remove 3"), "{m}");
        assert!(m.contains("/goal resume"), "{m}");
    }
}
