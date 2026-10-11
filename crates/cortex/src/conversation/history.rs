//! History editing: undo, rewind, and structural normalization.

use std::collections::HashSet;

use crate::conversation::is_compaction_marker;
use crate::types::Message;

/// Undo the last agent turn: remove everything after the last user message.
/// Returns the number of messages removed, or 0 if there is nothing to undo.
///
/// Compaction markers are `Role::User` rows but are never user-authored:
/// when the most recent `Role::User` row is a marker (the protected tail
/// held only assistant/tool rows), undo removes the agent turn *after* it
/// and leaves the marker in place — popping the marker would silently
/// delete the summary of the entire compacted conversation, far more than
/// one turn.
pub fn undo_last_turn(history: &mut Vec<Message>) -> usize {
    let last_user_idx = history
        .iter()
        .rposition(|m| m.role == crate::types::Role::User);

    match last_user_idx {
        Some(idx) => {
            // If the last message IS the user message, pop it too (undo the user's input)
            // Otherwise, pop everything after the last user message (undo the assistant turn)
            let remove_from = if idx == history.len() - 1 {
                if is_compaction_marker(&history[idx]) {
                    // A trailing marker is not an undoable user input.
                    return 0;
                }
                // Last msg is user — remove it and find the *previous* user message
                // to also remove the prior assistant response
                idx
            } else {
                idx + 1
            };
            let removed = history.len() - remove_from;
            history.truncate(remove_from);
            removed
        }
        None => 0,
    }
}

/// Rewind conversation to the Nth user message (0-indexed, oldest-first).
/// Truncates everything from that user message onward (inclusive).
/// Returns the number of messages removed, or 0 if the index is out of range.
///
/// Only user-authored rows count toward `n` — a compaction marker is a
/// `Role::User` row the user never typed, and counting it would shift every
/// later index by one (callers number the user's visible messages).
pub fn rewind_to_nth_user(history: &mut Vec<Message>, n: usize) -> usize {
    let user_positions: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == crate::types::Role::User && !is_compaction_marker(m))
        .map(|(i, _)| i)
        .collect();

    if let Some(&history_idx) = user_positions.get(n) {
        let removed = history.len() - history_idx;
        history.truncate(history_idx);
        removed
    } else {
        0
    }
}

/// Content of a tool result synthesized by `normalize_history` for a call
/// that never received one. Also used to recognize synthetic placeholders
/// when deduplicating results.
const SYNTHETIC_RESULT_TEXT: &str = "[tool call aborted — no result received]";

/// Normalize conversation history to prevent API errors.
///
/// Ensures structural invariants:
/// 1. Every tool call is answered by exactly one tool result (one is
///    synthesized when the real result is missing).
/// 2. Every tool result answers an outstanding tool call — a result that
///    answers nothing is a replay artifact and is removed.
/// 3. Duplicate call IDs *within a single assistant message* collapse to one.
///
/// Pairing is tracked by *outstanding* call, not by "every ID ever seen".
/// `tool_call_id` is not globally unique in practice: some servers emit one
/// constant ID for every call they ever return. Under a seen-once rule the
/// second real result of such a session looks like a duplicate and is
/// deleted, so from the second call onward the model never sees any result —
/// it announces its next action and the turn dies with the work unfinished.
/// A call arms its ID, its result consumes it, and a later call is free to
/// arm the same ID again.
///
/// This is called before sending to the LLM to prevent malformed
/// conversations that can cause API rejections — strict providers reject any
/// payload where a `tool_call_id` appears twice while still unanswered, and
/// reject a result that follows no call.
pub fn normalize_history(history: &mut Vec<Message>) {
    let mut out: Vec<Message> = Vec::with_capacity(history.len());
    // Armed call IDs awaiting a result, in arm order, each with the index in
    // `out` of the assistant message that armed it.
    let mut outstanding: Vec<(String, usize)> = Vec::new();
    // Per ID, where the result that consumed it landed in `out`. Used only to
    // upgrade a synthetic placeholder to the real result that arrives after
    // it (a previous normalize pass can persist the placeholder). Cleared
    // when the ID is armed again, so it never spans two pairings.
    let mut consumed_at: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for msg in std::mem::take(history) {
        if let Some(id) = msg.tool_call_id.clone() {
            if let Some(pos) = outstanding.iter().position(|(oid, _)| *oid == id) {
                outstanding.remove(pos);
                consumed_at.insert(id, out.len());
                out.push(msg);
            } else if let Some(&at) = consumed_at.get(&id) {
                // The pairing already closed. Only a real result replacing the
                // placeholder we synthesized for it is worth keeping.
                if is_synthetic_result(&out[at]) && !is_synthetic_result(&msg) {
                    out[at] = msg;
                } else {
                    warn_dropped_result(&id);
                }
            } else {
                warn_dropped_result(&id);
            }
            continue;
        }

        // Anything that is not a tool result closes the pairing window: flush
        // a placeholder for every call still unanswered so each one sits
        // directly after its own assistant message's result block.
        flush_outstanding(&mut out, &mut outstanding, &mut consumed_at);

        let mut msg = msg;
        if let Some(tcs) = msg.tool_calls.as_mut() {
            let mut seen_here: HashSet<String> = HashSet::new();
            tcs.retain(|tc| {
                let fresh = seen_here.insert(tc.id.clone());
                if !fresh {
                    tracing::warn!(
                        call_id = %tc.id,
                        "normalize_history: dropping duplicate tool call"
                    );
                }
                fresh
            });
            if tcs.is_empty() {
                // An empty `tool_calls` array is itself rejectable, so carry
                // the message on its content alone — or drop it if it has
                // none, since an empty assistant turn is rejectable too.
                msg.tool_calls = None;
                if msg.content.is_none() {
                    continue;
                }
            }
        }

        let idx = out.len();
        if let Some(tcs) = msg.tool_calls.as_ref() {
            for tc in tcs {
                consumed_at.remove(&tc.id);
                outstanding.push((tc.id.clone(), idx));
            }
        }
        out.push(msg);
    }

    flush_outstanding(&mut out, &mut outstanding, &mut consumed_at);
    *history = out;
}

fn warn_dropped_result(id: &str) {
    tracing::warn!(
        call_id = %id,
        "normalize_history: dropping tool result with no outstanding call"
    );
}

fn is_synthetic_result(m: &Message) -> bool {
    m.text_content().is_some_and(|t| t == SYNTHETIC_RESULT_TEXT)
}

/// Answer every still-outstanding call with a placeholder, appended in arm
/// order so each lands after its own assistant message.
fn flush_outstanding(
    out: &mut Vec<Message>,
    outstanding: &mut Vec<(String, usize)>,
    consumed_at: &mut std::collections::HashMap<String, usize>,
) {
    for (id, _) in outstanding.drain(..) {
        consumed_at.insert(id.clone(), out.len());
        out.push(Message::tool_result(&id, SYNTHETIC_RESULT_TEXT));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FunctionCall, Message, MessageContent, Role, ToolCall};

    fn make_user(text: &str) -> Message {
        Message::user(text)
    }

    fn make_assistant(text: &str) -> Message {
        Message::assistant(text)
    }

    fn make_tool_call_msg(text: &str, call_id: &str, tool_name: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: if text.is_empty() {
                None
            } else {
                Some(MessageContent::Text(text.to_string()))
            },
            tool_calls: Some(vec![ToolCall {
                id: call_id.to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: tool_name.to_string(),
                    arguments: "{}".to_string(),
                },
            }]),
            tool_call_id: None,
            timestamp: None,
            seq: None,
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
        }
    }

    fn make_tool_result(call_id: &str, result: &str) -> Message {
        Message::tool_result(call_id, result)
    }

    // -- normalize_history --

    #[test]
    fn normalize_synthesizes_missing_tool_result() {
        let mut history = vec![
            make_user("do something"),
            make_tool_call_msg("", "call_1", "run_shell"),
            // No tool result for call_1
            make_user("next question"),
        ];
        normalize_history(&mut history);

        // Should now have a synthetic result for call_1
        let has_result = history
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some("call_1"));
        assert!(has_result, "Should synthesize missing tool result");
        assert!(history
            .iter()
            .any(|m| m.text_content().unwrap_or("").contains("aborted")));
    }

    #[test]
    fn normalize_removes_orphaned_tool_result() {
        let mut history = vec![
            make_user("do something"),
            make_tool_result("nonexistent_call", "orphaned output"),
            make_user("next"),
        ];
        normalize_history(&mut history);

        let has_orphan = history
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some("nonexistent_call"));
        assert!(!has_orphan, "Should remove orphaned tool result");
    }

    #[test]
    fn normalize_noop_on_valid_history() {
        let mut history = vec![
            make_user("test"),
            make_tool_call_msg("", "c1", "read_memory"),
            make_tool_result("c1", "memory content"),
            make_assistant("here is what I found"),
        ];
        let len_before = history.len();
        normalize_history(&mut history);
        assert_eq!(history.len(), len_before);
    }

    #[test]
    fn normalize_handles_empty_history() {
        let mut history: Vec<Message> = Vec::new();
        normalize_history(&mut history);
        assert!(history.is_empty());
    }

    // -- normalize_history: duplicate tool_call_id dedup --

    #[test]
    fn normalize_dedups_duplicate_ids_table() {
        struct Case {
            name: &'static str,
            input: Vec<Message>,
            // Flattened tool-call ids across assistant messages, in order.
            expected_call_ids: Vec<&'static str>,
            // (id, content) of surviving tool results, in order.
            expected_results: Vec<(&'static str, &'static str)>,
        }

        let cases = vec![
            Case {
                name: "happy path untouched",
                input: vec![
                    make_user("q"),
                    make_tool_call_msg("", "c1", "run_shell"),
                    make_tool_result("c1", "out1"),
                    make_assistant("done"),
                ],
                expected_call_ids: vec!["c1"],
                expected_results: vec![("c1", "out1")],
            },
            Case {
                name: "duplicate real results keep the first",
                input: vec![
                    make_user("q"),
                    make_tool_call_msg("", "c1", "run_shell"),
                    make_tool_result("c1", "first"),
                    make_tool_result("c1", "second"),
                ],
                expected_call_ids: vec!["c1"],
                expected_results: vec![("c1", "first")],
            },
            Case {
                name: "real result outranks earlier synthetic placeholder",
                input: vec![
                    make_user("q"),
                    make_tool_call_msg("", "c1", "run_shell"),
                    make_tool_result("c1", SYNTHETIC_RESULT_TEXT),
                    make_tool_result("c1", "late real output"),
                ],
                expected_call_ids: vec!["c1"],
                expected_results: vec![("c1", "late real output")],
            },
            Case {
                // A server that reuses one id for every call it ever returns
                // must keep every pairing: the second call is real work, not
                // a replay of the first.
                name: "a reused id across assistant messages keeps both pairings",
                input: vec![
                    make_user("q"),
                    make_tool_call_msg("", "c1", "run_shell"),
                    make_tool_result("c1", "out1"),
                    make_tool_call_msg("", "c1", "run_shell"),
                    make_tool_result("c1", "out2"),
                ],
                expected_call_ids: vec!["c1", "c1"],
                expected_results: vec![("c1", "out1"), ("c1", "out2")],
            },
            Case {
                name: "duplicate calls within one assistant message",
                input: vec![
                    make_user("q"),
                    Message {
                        role: Role::Assistant,
                        content: None,
                        tool_calls: Some(vec![
                            ToolCall {
                                id: "c1".to_string(),
                                call_type: "function".to_string(),
                                function: FunctionCall {
                                    name: "tool1".to_string(),
                                    arguments: "{}".to_string(),
                                },
                            },
                            ToolCall {
                                id: "c1".to_string(),
                                call_type: "function".to_string(),
                                function: FunctionCall {
                                    name: "tool1".to_string(),
                                    arguments: "{}".to_string(),
                                },
                            },
                        ]),
                        tool_call_id: None,
                        timestamp: None,
                        seq: None,
                        reasoning_blocks: None,
                        reasoning_content: None,
                        reasoning_model: None,
                    },
                    make_tool_result("c1", "out"),
                ],
                expected_call_ids: vec!["c1"],
                expected_results: vec![("c1", "out")],
            },
        ];

        for case in cases {
            let mut history = case.input;
            normalize_history(&mut history);

            let call_ids: Vec<&str> = history
                .iter()
                .filter_map(|m| m.tool_calls.as_ref())
                .flat_map(|tcs| tcs.iter().map(|tc| tc.id.as_str()))
                .collect();
            assert_eq!(call_ids, case.expected_call_ids, "calls for {}", case.name);

            let results: Vec<(&str, &str)> = history
                .iter()
                .filter(|m| m.tool_call_id.is_some())
                .map(|m| {
                    (
                        m.tool_call_id.as_deref().unwrap(),
                        m.text_content().unwrap_or(""),
                    )
                })
                .collect();
            assert_eq!(results, case.expected_results, "results for {}", case.name);
        }
    }

    #[test]
    fn normalize_removes_content_less_assistant_message_with_no_calls() {
        // An empty `tool_calls` array on a content-less assistant message is
        // rejectable twice over: providers refuse both the empty array and
        // the empty turn. The message must not survive normalization.
        let mut history = vec![
            make_user("q"),
            Message {
                role: Role::Assistant,
                content: None,
                tool_calls: Some(vec![]),
                tool_call_id: None,
                timestamp: None,
                seq: None,
                reasoning_blocks: None,
                reasoning_content: None,
                reasoning_model: None,
            },
            make_assistant("done"),
        ];
        normalize_history(&mut history);

        assert_eq!(history.len(), 2, "the empty assistant turn must be dropped");
        assert!(
            !history
                .iter()
                .any(|m| m.content.is_none() && m.tool_call_id.is_none()),
            "no content-less, call-less message may remain"
        );
    }

    #[test]
    fn normalize_keeps_every_result_when_one_id_is_reused_for_all_calls() {
        // Regression: a server that returns a single constant tool_call_id for
        // every call it ever makes (some local runtimes do) must not lose its
        // results. Under a seen-once rule the model stops seeing tool output
        // from the second call onward and the task is abandoned mid-way.
        let mut history = vec![make_user("do a 3-step task")];
        for step in 0..3 {
            history.push(make_tool_call_msg("", "same", "run_shell"));
            history.push(make_tool_result("same", &format!("step{step}")));
        }
        normalize_history(&mut history);

        let results: Vec<&str> = history
            .iter()
            .filter(|m| m.tool_call_id.is_some())
            .map(|m| m.text_content().unwrap_or(""))
            .collect();
        assert_eq!(
            results,
            vec!["step0", "step1", "step2"],
            "every step's output must reach the model"
        );
        assert!(
            !results.contains(&SYNTHETIC_RESULT_TEXT),
            "no real result may be replaced by a placeholder"
        );
    }

    #[test]
    fn normalize_drops_a_replayed_result_that_answers_nothing() {
        // The protection the outstanding-call rule must preserve: a result
        // re-emitted after its call was already answered is a retry artifact.
        let mut history = vec![
            make_user("q"),
            make_tool_call_msg("", "c1", "run_shell"),
            make_tool_result("c1", "out"),
            make_tool_result("c1", "out replayed"),
            make_assistant("done"),
        ];
        normalize_history(&mut history);

        let results: Vec<&str> = history
            .iter()
            .filter(|m| m.tool_call_id.is_some())
            .map(|m| m.text_content().unwrap_or(""))
            .collect();
        assert_eq!(results, vec!["out"], "the replay must be dropped");
    }

    #[test]
    fn normalize_synthesizes_for_a_reused_id_whose_first_call_went_unanswered() {
        // The first call never got a result, then the same id was armed
        // again. The first must still be answered — by a placeholder — so the
        // second call's real result is not mistaken for the first's.
        let mut history = vec![
            make_user("q"),
            make_tool_call_msg("", "same", "run_shell"),
            make_tool_call_msg("", "same", "run_shell"),
            make_tool_result("same", "real output"),
        ];
        normalize_history(&mut history);

        let results: Vec<&str> = history
            .iter()
            .filter(|m| m.tool_call_id.is_some())
            .map(|m| m.text_content().unwrap_or(""))
            .collect();
        assert_eq!(results, vec![SYNTHETIC_RESULT_TEXT, "real output"]);
        assert_eq!(
            history
                .iter()
                .filter_map(|m| m.tool_calls.as_ref())
                .flat_map(|tcs| tcs.iter())
                .count(),
            2,
            "both calls must survive"
        );
    }

    // -- undo_last_turn --

    #[test]
    fn undo_removes_assistant_response() {
        let mut history = vec![
            make_user("hello"),
            make_assistant("hi there"),
            make_user("do something"),
            make_assistant("done"),
        ];
        let removed = undo_last_turn(&mut history);
        assert_eq!(removed, 1);
        assert_eq!(history.len(), 3);
        assert_eq!(history.last().unwrap().text_content(), Some("do something"));
    }

    #[test]
    fn undo_removes_tool_call_and_results() {
        let mut history = vec![
            make_user("test"),
            make_tool_call_msg("", "c1", "run_shell"),
            make_tool_result("c1", "output"),
            make_assistant("done"),
        ];
        let removed = undo_last_turn(&mut history);
        // Should remove everything after the user message
        assert_eq!(removed, 3);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].text_content(), Some("test"));
    }

    #[test]
    fn undo_removes_trailing_user_message() {
        let mut history = vec![make_user("hello"), make_assistant("hi"), make_user("bye")];
        let removed = undo_last_turn(&mut history);
        assert_eq!(removed, 1);
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn undo_empty_history() {
        let mut history: Vec<Message> = Vec::new();
        let removed = undo_last_turn(&mut history);
        assert_eq!(removed, 0);
    }

    #[test]
    fn undo_single_user_message() {
        let mut history = vec![make_user("hello")];
        let removed = undo_last_turn(&mut history);
        assert_eq!(removed, 1);
        assert!(history.is_empty());
    }

    // -- rewind_to_nth_user --

    #[test]
    fn rewind_to_first_user_message() {
        let mut history = vec![
            make_user("hello"),
            make_assistant("hi"),
            make_user("world"),
            make_assistant("ok"),
        ];
        let removed = rewind_to_nth_user(&mut history, 0);
        assert_eq!(removed, 4);
        assert!(history.is_empty());
    }

    #[test]
    fn rewind_to_second_user_message() {
        let mut history = vec![
            make_user("hello"),
            make_assistant("hi"),
            make_user("world"),
            make_assistant("ok"),
        ];
        let removed = rewind_to_nth_user(&mut history, 1);
        assert_eq!(removed, 2);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].text_content(), Some("hello"));
        assert_eq!(history[1].text_content(), Some("hi"));
    }

    #[test]
    fn rewind_out_of_range() {
        let mut history = vec![make_user("hello"), make_assistant("hi")];
        let removed = rewind_to_nth_user(&mut history, 5);
        assert_eq!(removed, 0);
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn rewind_empty_history() {
        let mut history: Vec<Message> = Vec::new();
        let removed = rewind_to_nth_user(&mut history, 0);
        assert_eq!(removed, 0);
    }

    #[test]
    fn rewind_single_user_message() {
        let mut history = vec![make_user("hello")];
        let removed = rewind_to_nth_user(&mut history, 0);
        assert_eq!(removed, 1);
        assert!(history.is_empty());
    }

    // -- compaction-marker awareness --

    fn make_marker() -> Message {
        crate::conversation::render_compaction_marker("earlier context summary")
    }

    #[test]
    fn undo_after_marker_removes_agent_turn_but_keeps_marker() {
        // Marker is the most recent Role::User row (protected tail held only
        // assistant/tool rows). Undo must remove the agent turn after it and
        // leave the summary in the working set.
        let mut history = vec![
            make_user("original request"),
            make_marker(),
            make_tool_call_msg("", "call_1", "run_shell"),
            make_tool_result("call_1", "ok"),
            make_assistant("done"),
        ];
        let removed = undo_last_turn(&mut history);
        assert_eq!(removed, 3);
        assert_eq!(history.len(), 2);
        assert!(
            crate::conversation::is_compaction_marker(&history[1]),
            "marker must survive undo"
        );
    }

    #[test]
    fn undo_trailing_marker_is_a_noop() {
        // A trailing marker is not user-authored input — popping it would
        // delete the summary of the entire compacted conversation.
        let mut history = vec![make_user("original request"), make_marker()];
        let removed = undo_last_turn(&mut history);
        assert_eq!(removed, 0);
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn rewind_does_not_count_marker_as_user_position() {
        // Callers number the user's visible messages; the marker must not
        // shift those indices. n=1 is "world", not the marker.
        let mut history = vec![
            make_user("hello"),
            make_assistant("hi"),
            make_marker(),
            make_user("world"),
            make_assistant("ok"),
        ];
        let removed = rewind_to_nth_user(&mut history, 1);
        assert_eq!(removed, 2);
        assert_eq!(history.len(), 3);
        assert!(crate::conversation::is_compaction_marker(&history[2]));
    }

    #[test]
    fn marker_predicate_rejects_user_text_resembling_marker() {
        // A user typing the heading alone (the fence is stripped from
        // untrusted input by sanitize_xml_boundaries) must not be treated as
        // a marker — otherwise /undo skips over real user input.
        let lookalike = Message::user(format!(
            "{}\n\nsome pasted text",
            crate::conversation::COMPACTION_MARKER_HEADING
        ));
        assert!(!crate::conversation::is_compaction_marker(&lookalike));
        assert!(crate::conversation::is_compaction_marker(&make_marker()));
        assert!(!crate::conversation::is_compaction_marker(&make_assistant(
            "not a user row"
        )));
    }

    // -- normalize_history edge cases --

    #[test]
    fn normalize_handles_multiple_tool_calls_in_one_message() {
        let mut history = vec![
            make_user("test"),
            Message {
                role: Role::Assistant,
                content: None,
                tool_calls: Some(vec![
                    ToolCall {
                        id: "c1".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "tool1".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                    ToolCall {
                        id: "c2".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "tool2".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                ]),
                tool_call_id: None,
                timestamp: None,
                seq: None,
                reasoning_blocks: None,
                reasoning_content: None,
                reasoning_model: None,
            },
            make_tool_result("c1", "result1"),
            // c2 result is missing
        ];
        normalize_history(&mut history);

        // Should synthesize a result for c2
        let c2_result = history
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("c2"));
        assert!(c2_result.is_some(), "Should synthesize missing c2 result");
        assert!(c2_result
            .unwrap()
            .text_content()
            .unwrap()
            .contains("aborted"));
    }

    #[test]
    fn normalize_preserves_order_with_valid_calls() {
        let mut history = vec![
            make_user("q1"),
            make_tool_call_msg("", "c1", "read_memory"),
            make_tool_result("c1", "data"),
            make_user("q2"),
            make_tool_call_msg("", "c2", "run_shell"),
            make_tool_result("c2", "output"),
            make_assistant("final answer"),
        ];
        let len_before = history.len();
        normalize_history(&mut history);
        assert_eq!(
            history.len(),
            len_before,
            "valid history should be unchanged"
        );
    }

    // -- undo_last_turn edge cases --

    #[test]
    fn undo_with_only_assistant_messages() {
        let mut history = vec![make_assistant("hello"), make_assistant("world")];
        let removed = undo_last_turn(&mut history);
        // No user message found, so nothing to undo
        assert_eq!(removed, 0);
        assert_eq!(history.len(), 2);
    }
}
