//! History editing: undo, rewind, and structural normalization.

use std::collections::HashSet;

use crate::types::Message;

/// Undo the last agent turn: remove everything after the last user message.
/// Returns the number of messages removed, or 0 if there is nothing to undo.
pub fn undo_last_turn(history: &mut Vec<Message>) -> usize {
    let last_user_idx = history
        .iter()
        .rposition(|m| m.role == crate::types::Role::User);

    match last_user_idx {
        Some(idx) => {
            // If the last message IS the user message, pop it too (undo the user's input)
            // Otherwise, pop everything after the last user message (undo the assistant turn)
            let remove_from = if idx == history.len() - 1 {
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
pub fn rewind_to_nth_user(history: &mut Vec<Message>, n: usize) -> usize {
    let user_positions: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == crate::types::Role::User)
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
/// 1. Every tool call ID appears at most once (drop later duplicates).
/// 2. Every tool call has a corresponding tool result (synthesize if missing).
/// 3. Every tool result has a corresponding tool call (remove orphans).
/// 4. Every tool result ID appears at most once (keep the first real result,
///    falling back to the first synthetic placeholder).
///
/// This is called before sending to the LLM to prevent malformed conversations
/// that can cause API rejections — strict providers reject any payload where
/// a tool_call_id appears twice, on either the call or the result side.
pub fn normalize_history(history: &mut Vec<Message>) {
    dedup_tool_calls(history);

    let call_ids: HashSet<String> = history
        .iter()
        .filter_map(|m| m.tool_calls.as_ref())
        .flat_map(|tcs| tcs.iter().map(|tc| tc.id.clone()))
        .collect();

    let result_ids: HashSet<String> = history
        .iter()
        .filter_map(|m| m.tool_call_id.as_ref())
        .cloned()
        .collect();

    let missing: Vec<(usize, String)> = history
        .iter()
        .enumerate()
        .filter_map(|(i, m)| m.tool_calls.as_ref().map(|tcs| (i, tcs)))
        .flat_map(|(i, tcs)| {
            tcs.iter()
                .filter(|tc| !result_ids.contains(&tc.id))
                .map(move |tc| (i, tc.id.clone()))
        })
        .collect();

    // Insert synthetic results after their assistant message (in reverse to
    // preserve indices).
    for (assistant_idx, call_id) in missing.into_iter().rev() {
        let synthetic = Message::tool_result(&call_id, SYNTHETIC_RESULT_TEXT);
        let insert_at = (assistant_idx + 1).min(history.len());
        history.insert(insert_at, synthetic);
    }

    history.retain(|m| {
        if let Some(ref tid) = m.tool_call_id {
            call_ids.contains(tid)
        } else {
            true
        }
    });

    dedup_tool_results(history);
}

/// Drop repeated tool-call IDs from assistant messages, keeping the first
/// occurrence — later duplicates are replay/retry artifacts, and the first
/// call is the one whose result the model actually saw. An assistant message
/// left with no calls and no content is removed entirely (providers reject
/// empty assistant turns).
fn dedup_tool_calls(history: &mut Vec<Message>) {
    let mut seen: HashSet<String> = HashSet::new();
    let mut idx = 0;
    while idx < history.len() {
        let mut emptied = false;
        if let Some(tcs) = history[idx].tool_calls.as_mut() {
            let had_calls = !tcs.is_empty();
            tcs.retain(|tc| {
                let fresh = seen.insert(tc.id.clone());
                if !fresh {
                    tracing::warn!(
                        call_id = %tc.id,
                        "normalize_history: dropping duplicate tool call"
                    );
                }
                fresh
            });
            if had_calls && tcs.is_empty() {
                // Every call in this message was a duplicate.
                history[idx].tool_calls = None;
                emptied = history[idx].content.is_none();
            }
        }
        if emptied {
            history.remove(idx);
        } else {
            idx += 1;
        }
    }
}

/// Collapse repeated tool-result IDs to one result per ID: the first
/// non-synthetic result if any exists (a real result outranks a placeholder
/// synthesized on an earlier normalize pass), otherwise the first occurrence.
/// The surviving content always lands in the *earliest* slot — strict
/// providers require a tool result to directly follow its assistant
/// `tool_calls` message, so a later real result must not keep its own,
/// possibly distant, position.
fn dedup_tool_results(history: &mut Vec<Message>) {
    let is_synthetic = |m: &Message| m.text_content().is_some_and(|t| t == SYNTHETIC_RESULT_TEXT);

    // First pass: per result ID, the earliest slot and the winning content.
    let mut first_slot: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut winner: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, m) in history.iter().enumerate() {
        let Some(id) = m.tool_call_id.as_ref() else {
            continue;
        };
        match winner.get(id) {
            None => {
                first_slot.insert(id.clone(), i);
                winner.insert(id.clone(), i);
            }
            Some(&w) => {
                if is_synthetic(&history[w]) && !is_synthetic(m) {
                    winner.insert(id.clone(), i);
                }
            }
        }
    }

    // Move a later-positioned winner's content into the earliest slot.
    for (id, &w) in &winner {
        let f = first_slot[id];
        if w != f {
            history[f] = history[w].clone();
        }
    }

    // Second pass: drop every occurrence outside the earliest slot.
    let mut i = 0usize;
    history.retain(|m| {
        let keep = match m.tool_call_id.as_ref() {
            Some(id) => first_slot.get(id) == Some(&i),
            None => true,
        };
        if !keep {
            tracing::warn!(
                call_id = m.tool_call_id.as_deref().unwrap_or(""),
                "normalize_history: dropping duplicate tool result"
            );
        }
        i += 1;
        keep
    });
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
                name: "duplicate calls across assistant messages keep first, pairing stays 1:1",
                input: vec![
                    make_user("q"),
                    make_tool_call_msg("", "c1", "run_shell"),
                    make_tool_result("c1", "out1"),
                    make_tool_call_msg("", "c1", "run_shell"),
                    make_tool_result("c1", "out2"),
                ],
                expected_call_ids: vec!["c1"],
                expected_results: vec![("c1", "out1")],
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
    fn normalize_removes_assistant_message_emptied_by_call_dedup() {
        // A content-less assistant message whose only tool call is a duplicate
        // must be dropped entirely — an empty assistant turn is as rejectable
        // as the duplicate id itself.
        let mut history = vec![
            make_user("q"),
            make_tool_call_msg("", "c1", "run_shell"),
            make_tool_result("c1", "out"),
            make_tool_call_msg("", "c1", "run_shell"),
        ];
        normalize_history(&mut history);

        let empty_assistant = history
            .iter()
            .any(|m| m.role == Role::Assistant && m.content.is_none() && m.tool_calls.is_none());
        assert!(
            !empty_assistant,
            "emptied assistant message must be removed"
        );
        let call_count = history
            .iter()
            .filter_map(|m| m.tool_calls.as_ref())
            .flat_map(|tcs| tcs.iter())
            .filter(|tc| tc.id == "c1")
            .count();
        assert_eq!(call_count, 1);
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
