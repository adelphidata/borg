//! History editing: undo, rewind, and structural normalization.

use std::collections::HashSet;

use crate::types::Message;

/// Undo the last agent turn: remove everything after the last user message.
/// Returns the number of messages removed, or 0 if there is nothing to undo.
pub fn undo_last_turn(history: &mut Vec<Message>) -> usize {
    // Find the index of the last user message
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

/// Normalize conversation history to prevent API errors.
///
/// Ensures structural invariants:
/// 1. Every tool call has a corresponding tool result (synthesize if missing).
/// 2. Every tool result has a corresponding tool call (remove orphans).
///
/// This is called before sending to the LLM to prevent malformed conversations
/// that can cause API rejections.
pub fn normalize_history(history: &mut Vec<Message>) {
    // Collect all tool call IDs from assistant messages
    let call_ids: HashSet<String> = history
        .iter()
        .filter_map(|m| m.tool_calls.as_ref())
        .flat_map(|tcs| tcs.iter().map(|tc| tc.id.clone()))
        .collect();

    // Collect all tool result IDs
    let result_ids: HashSet<String> = history
        .iter()
        .filter_map(|m| m.tool_call_id.as_ref())
        .cloned()
        .collect();

    // 1. Synthesize missing results for tool calls that have no result
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
        let synthetic = Message::tool_result(&call_id, "[tool call aborted — no result received]");
        let insert_at = (assistant_idx + 1).min(history.len());
        history.insert(insert_at, synthetic);
    }

    // 2. Remove orphaned tool results (results with no matching call)
    history.retain(|m| {
        if let Some(ref tid) = m.tool_call_id {
            call_ids.contains(tid)
        } else {
            true
        }
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
