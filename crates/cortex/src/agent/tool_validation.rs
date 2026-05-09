//! Free types and helpers for tool-call validation.
//!
//! Pulled out of `agent/mod.rs` as part of the structural split. None of these
//! are methods on `Agent` — they are standalone validators / partitioners
//! consumed by the agent loop.

use tracing::warn;

use crate::constants;
use crate::types::{FunctionCall, Message, ToolCall};

/// Detects runs of identical `(tool_name, arguments)` tool calls within a
/// single user turn. Counts strictly-consecutive matches: a differing call
/// resets the streak. The Nth and any subsequent identical calls (where N
/// = `MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS`) report `block = true` so the
/// caller can substitute a synthetic error tool_result instead of executing.
///
/// Equality is byte-exact on the raw `arguments` string — no whitespace or
/// JSON-key-order normalization. A stuck LLM that emits the same canonical
/// payload every iteration will be caught; one that randomizes whitespace
/// will not. Trade-off accepted to keep the guard cheap and avoid
/// hashing-vs-parsing correctness questions.
///
/// Defends against the OpenClaw failure mode: a stuck LLM looping on the
/// same call burns rate-guard quota and pollutes context until the request
/// overflows. The 25-iteration ceiling caps damage but doesn't notice
/// duplication; this guard stops the spam earlier.
#[derive(Default, Debug)]
pub(crate) struct ToolCallRepeatTracker {
    pub(super) last_signature: Option<u64>,
    pub(super) consecutive_count: u32,
}

impl ToolCallRepeatTracker {
    /// Hash `(name, args)`, update streak state, and return whether the
    /// current call should be blocked. The signature is stored regardless
    /// of the block decision so the streak can keep growing across
    /// already-blocked calls (so test #2 sees indices 2,3,4 blocked, not
    /// just index 2).
    pub(super) fn observe(&mut self, name: &str, args: &str) -> bool {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        name.hash(&mut hasher);
        args.hash(&mut hasher);
        let sig = hasher.finish();
        if self.last_signature == Some(sig) {
            self.consecutive_count = self.consecutive_count.saturating_add(1);
        } else {
            self.last_signature = Some(sig);
            self.consecutive_count = 1;
        }
        self.consecutive_count >= constants::MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS
    }

    pub(super) fn reset(&mut self) {
        self.last_signature = None;
        self.consecutive_count = 0;
    }
}

/// Synthetic tool_result body returned in place of a blocked repeated call.
/// Static text (modulo the call count) so the LLM gets a clear signal to
/// vary arguments or stop calling tools.
pub(super) fn repeated_tool_call_block_message(name: &str, count: u32) -> String {
    format!(
        "Error: repeated identical tool call detected ({count} consecutive calls to `{name}` \
         with the same arguments). Vary arguments or stop calling tools and respond to the user."
    )
}

/// A tool call the LLM emitted that failed validation but still has enough
/// metadata (id + name) to address a synthetic `tool_result` back to the
/// model. We surface these as "Error: ..." results in the TUI and feed them
/// into history so the LLM can retry on the next turn.
#[derive(Debug, Clone)]
pub(super) struct InvalidToolCall {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) reason: String,
    pub(super) raw_arguments: String,
}

/// Partition partial tool calls from the LLM stream into:
/// - `valid`: well-formed `ToolCall`s ready to execute.
/// - `invalid`: addressable failures (id + name present) that must be surfaced
///   as synthetic error tool_results so the LLM gets feedback and retries.
///
/// Calls without a usable id are fully discarded (no way to address a
/// `tool_result`); they show up only in `tracing::warn!` for ops visibility.
pub(super) fn partition_tool_calls(
    tool_calls: &[PartialToolCall],
    truncated_due_to_length: bool,
) -> (Vec<ToolCall>, Vec<InvalidToolCall>) {
    let mut valid = Vec::new();
    let mut invalid = Vec::new();
    for ptc in tool_calls
        .iter()
        .take(constants::MAX_TOOL_CALLS_PER_RESPONSE)
    {
        if ptc.id.is_empty() {
            warn!(
                "Dropping unaddressable tool call (missing id, name='{}')",
                ptc.name
            );
            continue;
        }
        if ptc.name.is_empty() {
            warn!(
                "Dropping unaddressable tool call (id={}, missing name)",
                ptc.id
            );
            continue;
        }
        if ptc.name.len() > constants::MAX_TOOL_NAME_LEN {
            let reason = format!("tool name exceeds {} bytes", constants::MAX_TOOL_NAME_LEN);
            warn!("Invalid tool call '{}': {}", ptc.name, reason);
            invalid.push(InvalidToolCall {
                id: ptc.id.clone(),
                name: ptc.name.clone(),
                reason,
                raw_arguments: ptc.arguments.clone(),
            });
            continue;
        }
        if ptc.arguments.len() > constants::MAX_TOOL_ARGS_LEN {
            let reason = format!(
                "arguments exceed {} bytes ({} received)",
                constants::MAX_TOOL_ARGS_LEN,
                ptc.arguments.len()
            );
            warn!("Invalid tool call '{}': {}", ptc.name, reason);
            invalid.push(InvalidToolCall {
                id: ptc.id.clone(),
                name: ptc.name.clone(),
                reason,
                raw_arguments: ptc.arguments.clone(),
            });
            continue;
        }
        if let Err(e) = serde_json::from_str::<serde_json::Value>(&ptc.arguments) {
            // When the provider signaled max_tokens cut-off this turn, the
            // unparseable JSON is almost certainly the truncation symptom
            // (closing quote / brace never arrived). Replace the cryptic
            // serde error with a clear directive so the model splits the
            // work instead of looping on the same oversized call.
            let reason = if truncated_due_to_length {
                format!(
                    "response truncated at max_tokens; your tool call was cut off mid-output. \
                     Split the work into multiple smaller tool calls (e.g. several apply_patch \
                     calls, or write the file in chunks) and try again. \
                     (underlying parse error: {e})"
                )
            } else {
                format!("invalid JSON arguments: {e}")
            };
            warn!("Invalid tool call '{}': {}", ptc.name, reason);
            invalid.push(InvalidToolCall {
                id: ptc.id.clone(),
                name: ptc.name.clone(),
                reason,
                raw_arguments: ptc.arguments.clone(),
            });
            continue;
        }
        valid.push(ToolCall {
            id: ptc.id.clone(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: ptc.name.clone(),
                arguments: ptc.arguments.clone(),
            },
        });
    }
    (valid, invalid)
}

/// Maximum chars of raw arguments echoed back to the LLM in a synthetic
/// error tool_result. Long enough for the model to spot where its JSON broke
/// without ballooning the next prompt.
const INVALID_TOOL_ARGS_PREVIEW_LEN: usize = 2000;

/// Format a synthetic tool_result body for an invalid tool call. Prefix is
/// `Error:` so existing `is_error` detection (TUI red box, hook is_error
/// flag) lights up automatically.
pub(super) fn format_invalid_tool_result(invalid: &InvalidToolCall) -> String {
    let preview = if invalid.raw_arguments.len() > INVALID_TOOL_ARGS_PREVIEW_LEN {
        // Walk back to the nearest char boundary so multi-byte UTF-8 inside
        // the LLM's malformed JSON can't panic the agent loop.
        let mut end = INVALID_TOOL_ARGS_PREVIEW_LEN;
        while end > 0 && !invalid.raw_arguments.is_char_boundary(end) {
            end -= 1;
        }
        format!(
            "{}…[truncated; total {} bytes]",
            &invalid.raw_arguments[..end],
            invalid.raw_arguments.len()
        )
    } else {
        invalid.raw_arguments.clone()
    };
    format!(
        "Error: invalid tool arguments — {reason}\n\nraw arguments:\n{preview}",
        reason = invalid.reason,
    )
}

#[derive(Default)]
pub(super) struct PartialToolCall {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) arguments: String,
}

/// Detect whether the latest assistant turn issued tool_calls that haven't
/// all been answered yet. Used by [`super::Agent::append_assistant_message`] to
/// avoid splitting a tool_use/tool_result pair, which both Anthropic and
/// OpenAI reject on the next turn.
///
/// Walks back from the end through `Tool` messages, collecting answered
/// `tool_call_id`s. Stops at the first non-tool message; if it is an
/// assistant with `tool_calls`, every call id must be in the answered set.
pub(super) fn has_unresolved_tool_calls(history: &[Message]) -> bool {
    use crate::types::Role;
    let mut answered: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for msg in history.iter().rev() {
        match msg.role {
            Role::Tool => {
                if let Some(id) = msg.tool_call_id.as_deref() {
                    answered.insert(id);
                }
            }
            Role::Assistant => {
                if let Some(calls) = msg.tool_calls.as_ref() {
                    return calls.iter().any(|c| !answered.contains(c.id.as_str()));
                }
                return false;
            }
            _ => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FunctionCall, Role, ToolCall};

    fn assistant_with_tools(ids: &[&str]) -> Message {
        let calls = ids
            .iter()
            .map(|id| ToolCall {
                id: (*id).to_string(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "noop".into(),
                    arguments: "{}".into(),
                },
            })
            .collect();
        Message {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(calls),
            tool_call_id: None,
            timestamp: None,
        }
    }

    #[test]
    fn has_unresolved_tool_calls_idle_history_returns_false() {
        let history = vec![
            Message::user("hello"),
            Message::assistant("hi there"),
            Message::user("how are you"),
            Message::assistant("fine"),
        ];
        assert!(!has_unresolved_tool_calls(&history));
    }

    #[test]
    fn has_unresolved_tool_calls_empty_history_returns_false() {
        assert!(!has_unresolved_tool_calls(&[]));
    }

    #[test]
    fn has_unresolved_tool_calls_open_tool_call_returns_true() {
        let history = vec![
            Message::user("run the thing"),
            assistant_with_tools(&["call_a", "call_b"]),
            Message::tool_result("call_a", "ok"),
        ];
        assert!(has_unresolved_tool_calls(&history));
    }

    #[test]
    fn has_unresolved_tool_calls_all_answered_returns_false() {
        let history = vec![
            Message::user("run the thing"),
            assistant_with_tools(&["call_a", "call_b"]),
            Message::tool_result("call_a", "ok"),
            Message::tool_result("call_b", "ok"),
        ];
        assert!(!has_unresolved_tool_calls(&history));
    }

    fn ptc(id: &str, name: &str, args: &str) -> PartialToolCall {
        PartialToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args.to_string(),
        }
    }

    #[test]
    fn partition_tool_calls_classifies_invalid_json_as_addressable() {
        let calls = vec![ptc("call_1", "apply_patch", "{\"input\": \"*** Begin")];
        let (valid, invalid) = partition_tool_calls(&calls, false);
        assert!(valid.is_empty(), "truncated JSON must not be marked valid");
        assert_eq!(invalid.len(), 1, "exactly one invalid call expected");
        assert_eq!(invalid[0].id, "call_1");
        assert_eq!(invalid[0].name, "apply_patch");
        assert!(
            invalid[0].reason.contains("invalid JSON"),
            "reason should describe the JSON parse failure, got: {}",
            invalid[0].reason,
        );
        assert_eq!(invalid[0].raw_arguments, "{\"input\": \"*** Begin");
    }

    #[test]
    fn partition_tool_calls_substitutes_truncation_hint_when_max_tokens_signaled() {
        let calls = vec![ptc("call_1", "apply_patch", "{\"input\": \"*** Begin")];
        let (valid, invalid) = partition_tool_calls(&calls, true);
        assert!(valid.is_empty());
        assert_eq!(invalid.len(), 1);
        let reason = &invalid[0].reason;
        assert!(
            reason.contains("truncated at max_tokens"),
            "reason must name the truncation cause, got: {reason}"
        );
        assert!(
            reason.contains("Split the work"),
            "reason must direct the model to split the work, got: {reason}"
        );
        assert!(
            !reason.starts_with("invalid JSON"),
            "truncation reason must shadow the bare JSON parse error, got: {reason}"
        );
    }

    #[test]
    fn partition_tool_calls_drops_unaddressable_calls() {
        let calls = vec![ptc("", "apply_patch", "{}"), ptc("call_1", "", "{}")];
        let (valid, invalid) = partition_tool_calls(&calls, false);
        assert!(valid.is_empty());
        assert!(
            invalid.is_empty(),
            "unaddressable calls must not be promoted to invalid bucket — \
             we have no id to address a synthetic tool_result to",
        );
    }

    #[test]
    fn partition_tool_calls_classifies_each_failure_axis() {
        let oversized_name = "x".repeat(constants::MAX_TOOL_NAME_LEN + 1);
        let oversized_args = format!("\"{}\"", "y".repeat(constants::MAX_TOOL_ARGS_LEN));
        let cases: Vec<(&str, PartialToolCall, &str)> = vec![
            (
                "oversized_name",
                ptc("id_a", &oversized_name, "{}"),
                "tool name exceeds",
            ),
            (
                "oversized_args",
                ptc("id_b", "run_shell", &oversized_args),
                "arguments exceed",
            ),
            (
                "invalid_json",
                ptc("id_c", "apply_patch", "{not json"),
                "invalid JSON",
            ),
        ];
        for (label, call, reason_substr) in cases {
            let (valid, invalid) = partition_tool_calls(std::slice::from_ref(&call), false);
            assert!(valid.is_empty(), "{label}: should not be valid");
            assert_eq!(invalid.len(), 1, "{label}: expected one invalid entry");
            assert_eq!(invalid[0].id, call.id, "{label}: id preserved");
            assert_eq!(invalid[0].name, call.name, "{label}: name preserved");
            assert!(
                invalid[0].reason.contains(reason_substr),
                "{label}: reason `{}` should mention `{reason_substr}`",
                invalid[0].reason,
            );
        }
    }

    #[test]
    fn partition_tool_calls_passes_well_formed() {
        let calls = vec![ptc("call_1", "read_file", "{\"path\": \"a.txt\"}")];
        let (valid, invalid) = partition_tool_calls(&calls, false);
        assert_eq!(valid.len(), 1);
        assert!(invalid.is_empty());
        assert_eq!(valid[0].id, "call_1");
        assert_eq!(valid[0].function.name, "read_file");
        assert_eq!(valid[0].function.arguments, "{\"path\": \"a.txt\"}");
    }

    #[test]
    fn format_invalid_tool_result_is_error_prefixed_and_includes_args() {
        let inv = InvalidToolCall {
            id: "call_1".into(),
            name: "apply_patch".into(),
            reason: "invalid JSON arguments: EOF while parsing a string".into(),
            raw_arguments: "{\"input\": \"*** Begin".into(),
        };
        let body = format_invalid_tool_result(&inv);
        assert!(
            body.starts_with("Error:"),
            "must keep `Error:` prefix so TUI red-box rendering and \
             `is_error` hook flag both fire — got: {body}",
        );
        assert!(
            body.contains("invalid JSON arguments"),
            "reason must surface in the body so the LLM can fix its next attempt",
        );
        assert!(
            body.contains("{\"input\": \"*** Begin"),
            "raw arguments must be echoed so the LLM sees what it sent",
        );
    }

    #[test]
    fn format_invalid_tool_result_handles_multibyte_boundary() {
        let mut args = String::with_capacity(INVALID_TOOL_ARGS_PREVIEW_LEN * 3);
        for _ in 0..(INVALID_TOOL_ARGS_PREVIEW_LEN - 1) {
            args.push('a');
        }
        args.push('€'); // 3 bytes
        for _ in 0..INVALID_TOOL_ARGS_PREVIEW_LEN {
            args.push('b');
        }
        let inv = InvalidToolCall {
            id: "id".into(),
            name: "n".into(),
            reason: "r".into(),
            raw_arguments: args,
        };
        let body = format_invalid_tool_result(&inv);
        assert!(body.contains("truncated"));
    }

    #[test]
    fn format_invalid_tool_result_truncates_long_args() {
        let inv = InvalidToolCall {
            id: "call_1".into(),
            name: "apply_patch".into(),
            reason: "invalid JSON".into(),
            raw_arguments: "z".repeat(INVALID_TOOL_ARGS_PREVIEW_LEN * 2),
        };
        let body = format_invalid_tool_result(&inv);
        assert!(body.contains("truncated"));
        assert!(
            body.len() < INVALID_TOOL_ARGS_PREVIEW_LEN + 500,
            "preview must cap the echoed args (got {} bytes)",
            body.len(),
        );
    }

    fn run_observe(seq: &[(&str, &str)]) -> Vec<bool> {
        let mut t = ToolCallRepeatTracker::default();
        seq.iter().map(|(n, a)| t.observe(n, a)).collect()
    }

    #[test]
    fn tool_call_repeat_blocks_on_third_consecutive_identical() {
        let blocked = run_observe(&[("run_shell", "echo a"); 3]);
        assert_eq!(blocked, vec![false, false, true]);
    }

    #[test]
    fn tool_call_repeat_keeps_blocking_after_threshold() {
        let blocked = run_observe(&[("run_shell", "echo a"); 5]);
        assert_eq!(blocked, vec![false, false, true, true, true]);
    }

    #[test]
    fn tool_call_repeat_streak_resets_on_different_args() {
        let seq = [
            ("run_shell", "echo a"),
            ("run_shell", "echo b"),
            ("run_shell", "echo a"),
            ("run_shell", "echo a"),
            ("run_shell", "echo a"),
        ];
        let blocked = run_observe(&seq);
        assert_eq!(blocked, vec![false, false, false, false, true]);
    }

    #[test]
    fn tool_call_repeat_interleaved_never_blocks() {
        let seq = [
            ("run_shell", "x"),
            ("read_file", "x"),
            ("run_shell", "x"),
            ("read_file", "x"),
            ("run_shell", "x"),
            ("read_file", "x"),
        ];
        let blocked = run_observe(&seq);
        assert!(blocked.iter().all(|&b| !b), "got: {blocked:?}");
    }

    #[test]
    fn tool_call_repeat_below_threshold_never_blocks() {
        let mut seq: Vec<(&str, &str)> = vec![("run_shell", "echo a"); 2];
        for i in 0..100 {
            seq.push(("read_file", if i % 2 == 0 { "p1" } else { "p2" }));
        }
        let blocked = run_observe(&seq);
        assert!(blocked.iter().all(|&b| !b));
    }

    #[test]
    fn tool_call_repeat_whitespace_sensitive() {
        let seq = [
            ("run_shell", "echo a"),
            ("run_shell", "echo a "),
            ("run_shell", "echo a"),
        ];
        let blocked = run_observe(&seq);
        assert_eq!(blocked, vec![false, false, false]);
    }

    #[test]
    fn tool_call_repeat_different_names_same_args_not_blocked() {
        let seq = [
            ("read_file", "{\"path\":\"x\"}"),
            ("list_dir", "{\"path\":\"x\"}"),
            ("read_file", "{\"path\":\"x\"}"),
        ];
        let blocked = run_observe(&seq);
        assert_eq!(blocked, vec![false, false, false]);
    }

    #[test]
    fn tool_call_repeat_reset_clears_streak() {
        let mut t = ToolCallRepeatTracker::default();
        for _ in 0..3 {
            t.observe("run_shell", "echo a");
        }
        assert_eq!(t.consecutive_count, 3);
        t.reset();
        assert_eq!(t.consecutive_count, 0);
        assert_eq!(t.last_signature, None);
        assert!(!t.observe("run_shell", "echo a"));
        assert!(!t.observe("run_shell", "echo a"));
        assert!(t.observe("run_shell", "echo a"));
    }

    #[test]
    fn tool_call_repeat_block_message_mentions_call_and_count() {
        let body = repeated_tool_call_block_message("run_shell", 3);
        assert!(body.contains("run_shell"), "must include tool name: {body}");
        assert!(body.contains("3"), "must include count: {body}");
        assert!(
            body.contains("Vary arguments") || body.contains("vary arguments"),
            "must instruct LLM to vary or stop: {body}"
        );
    }
}
