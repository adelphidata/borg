//! Non-LLM tool-result reduction: share-limit, age-based degradation, and
//! newest-first compaction. These run before any LLM-summary call in the
//! autocompaction pipeline.

use crate::memory::tokenizer::estimate_tokens;
use crate::types::{Message, MessageContent, Role};

use super::tokens::history_tokens;

use crate::constants;

/// Age-based degradation thresholds.
///
/// Tool results within `AGE_TIER1_WINDOW` recent messages are kept at full fidelity.
/// Between tier 1 and tier 2, results over `AGE_TIER1_TOKEN_THRESHOLD` are truncated.
/// Older than tier 2, results over `AGE_TIER2_TOKEN_THRESHOLD` become one-liners.
pub const AGE_TIER1_WINDOW: usize = 12;
pub const AGE_TIER2_WINDOW: usize = 24;
pub const AGE_TIER1_TOKEN_THRESHOLD: usize = 200;
pub const AGE_TIER2_TOKEN_THRESHOLD: usize = 50;

/// Compact the newest tool results to preserve the prompt cache prefix.
///
/// Replaces tool result content with a placeholder, cheaply reclaiming tokens
/// without calling the LLM. Only compacts results from the newest half of the
/// history so the oldest messages (cached prefix) stay byte-identical.
pub fn compact_tool_results(history: &mut [Message], max_tokens: usize) {
    let total = history_tokens(history);
    if total <= max_tokens {
        return;
    }

    let half = history.len() / 2;
    let placeholder = "[compacted: output removed]";

    for msg in history[half..].iter_mut().rev() {
        if msg.role != Role::Tool {
            continue;
        }
        // Only compact if the message has substantial content
        let msg_toks = match &msg.content {
            Some(MessageContent::Text(s)) => estimate_tokens(s),
            _ => continue,
        };
        if msg_toks > constants::TOOL_RESULT_COMPACT_THRESHOLD {
            msg.content = Some(MessageContent::Text(placeholder.to_string()));
        }
    }
}

/// Enforce per-tool-result share limit: no single tool result should exceed
/// `share_pct` of `max_history_tokens`.
pub fn enforce_tool_result_share_limit(history: &mut [Message], max_tokens: usize, share_pct: f64) {
    let max_per_result = (max_tokens as f64 * share_pct) as usize;
    if max_per_result == 0 {
        return;
    }

    for msg in history.iter_mut() {
        if msg.role != Role::Tool {
            continue;
        }
        if let Some(MessageContent::Text(ref s)) = msg.content {
            let toks = estimate_tokens(s);
            if toks > max_per_result {
                let truncated = crate::util::truncate::truncate_output(s, max_per_result);
                msg.content = Some(MessageContent::Text(truncated));
            }
        }
    }
}

/// Progressively degrade old tool results to save tokens without losing recent context.
///
/// - Messages in the last `AGE_TIER1_WINDOW` positions: untouched.
/// - Messages between tier 1 and tier 2: tool results over `AGE_TIER1_TOKEN_THRESHOLD`
///   tokens are truncated to first 3 lines + omission note + last 2 lines.
/// - Messages older than tier 2: tool results over `AGE_TIER2_TOKEN_THRESHOLD` tokens
///   are replaced with a one-line status summary.
pub fn age_based_tool_result_degradation(history: &mut [Message]) {
    let len = history.len();
    if len == 0 {
        return;
    }

    for (i, msg) in history.iter_mut().enumerate() {
        if msg.role != Role::Tool {
            continue;
        }

        let age = len - 1 - i; // 0 = newest, len-1 = oldest
        if age < AGE_TIER1_WINDOW {
            continue; // recent: full fidelity
        }

        let text = match &msg.content {
            Some(MessageContent::Text(s)) => s.clone(),
            _ => continue,
        };

        let toks = estimate_tokens(&text);

        if age < AGE_TIER2_WINDOW {
            // Tier 2: truncate large results
            if toks > AGE_TIER1_TOKEN_THRESHOLD {
                msg.content = Some(MessageContent::Text(truncate_with_context(&text)));
            }
        } else {
            // Tier 3: one-liner
            if toks > AGE_TIER2_TOKEN_THRESHOLD {
                let text_lower = text.to_lowercase();
                let status = if text_lower.starts_with("error")
                    || text_lower.starts_with("fail")
                    || text_lower.contains("\"error\"")
                {
                    "error"
                } else {
                    "ok"
                };
                let tool_id = msg.tool_call_id.as_deref().unwrap_or("unknown");
                msg.content = Some(MessageContent::Text(format!(
                    "[tool result {tool_id} — {status}]"
                )));
            }
        }
    }
}

/// Truncate text to first 3 lines + omission note + last 2 lines.
fn truncate_with_context(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= 7 {
        return text.to_string();
    }
    let head: Vec<&str> = lines[..3].to_vec();
    let tail: Vec<&str> = lines[lines.len() - 2..].to_vec();
    let omitted = lines.len() - 5;
    format!(
        "{}\n[{omitted} lines omitted]\n{}",
        head.join("\n"),
        tail.join("\n")
    )
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

    // -- compact_tool_results --

    #[test]
    fn compact_tool_results_noop_under_budget() {
        let mut history = vec![
            make_user("test"),
            make_tool_call_msg("", "c1", "run_shell"),
            make_tool_result("c1", "short output"),
            make_assistant("done"),
        ];
        compact_tool_results(&mut history, 100_000);
        // Nothing should change when under budget
        assert_eq!(history.len(), 4);
        assert_eq!(history[2].text_content().unwrap(), "short output");
    }

    #[test]
    fn compact_tool_results_compacts_newest_preserving_cache_prefix() {
        let big_output = "x".repeat(500);
        let mut history = vec![
            make_user("old question"),
            make_tool_call_msg("", "c1", "run_shell"),
            make_tool_result("c1", &big_output),
            make_assistant("old answer"),
            make_user("new question"),
            make_tool_call_msg("", "c2", "run_shell"),
            make_tool_result("c2", &big_output),
            make_assistant("new answer"),
        ];
        // Set budget very low to trigger compaction
        compact_tool_results(&mut history, 10);
        // The old tool result (index 2, in first half) should be PRESERVED (cache prefix)
        let old_result = history[2].text_content().unwrap();
        assert_eq!(
            old_result, big_output,
            "old result should be preserved for prompt cache prefix"
        );
        // The new tool result (index 6, in second half) should be compacted
        let new_result = history[6].text_content().unwrap();
        assert!(
            new_result.contains("compacted"),
            "new result should be compacted: {new_result}"
        );
    }

    #[test]
    fn compact_tool_results_preserves_all_oldest_half() {
        let big_output = "x".repeat(500);
        let mut history = vec![
            make_user("q1"),
            make_tool_call_msg("", "c1", "run_shell"),
            make_tool_result("c1", &big_output),
            make_assistant("a1"),
            make_user("q2"),
            make_tool_call_msg("", "c2", "run_shell"),
            make_tool_result("c2", &big_output),
            make_assistant("a2"),
            make_user("q3"),
            make_tool_call_msg("", "c3", "run_shell"),
            make_tool_result("c3", &big_output),
            make_assistant("a3"),
        ];
        compact_tool_results(&mut history, 10);
        // First half (indices 0..6) should be untouched
        assert_eq!(history[2].text_content().unwrap(), big_output);
        // Second half results should be compacted (newest first)
        assert!(history[6].text_content().unwrap().contains("compacted"));
        assert!(history[10].text_content().unwrap().contains("compacted"));
    }

    #[test]
    fn compact_tool_results_skips_small_results() {
        let mut history = vec![
            make_user("test"),
            make_tool_call_msg("", "c1", "run_shell"),
            make_tool_result("c1", "ok"), // Very small, should not be compacted
            make_user("next"),
        ];
        compact_tool_results(&mut history, 1); // Very small budget
                                               // "ok" is tiny (< 20 tokens) so should not be compacted
        assert_eq!(history[2].text_content().unwrap(), "ok");
    }

    // -- enforce_tool_result_share_limit --

    #[test]
    fn share_limit_noop_when_results_small() {
        let mut history = vec![
            make_user("test"),
            make_tool_call_msg("", "c1", "run_shell"),
            make_tool_result("c1", "short"),
            make_assistant("done"),
        ];
        enforce_tool_result_share_limit(&mut history, 100_000, 0.5);
        assert_eq!(history[2].text_content().unwrap(), "short");
    }

    #[test]
    fn share_limit_truncates_huge_result() {
        let huge = "x".repeat(100_000);
        let mut history = vec![
            make_user("test"),
            make_tool_call_msg("", "c1", "run_shell"),
            make_tool_result("c1", &huge),
            make_assistant("done"),
        ];
        // 1000 tokens budget, 50% share = 500 tokens max per result
        enforce_tool_result_share_limit(&mut history, 1000, 0.5);
        let result = history[2].text_content().unwrap();
        assert!(result.len() < huge.len(), "result should be truncated");
        assert!(
            result.contains("omitted"),
            "truncation marker should disclose omitted output: {result}"
        );
    }

    #[test]
    fn share_limit_zero_max_tokens_noop() {
        let mut history = vec![
            make_user("test"),
            make_tool_call_msg("", "c1", "run_shell"),
            make_tool_result("c1", "data"),
        ];
        enforce_tool_result_share_limit(&mut history, 0, 0.5);
        assert_eq!(history[2].text_content().unwrap(), "data");
    }

    #[test]
    fn compaction_pipeline_shrinks_over_budget_history() {
        // End-to-end behavioral guard for the autocompaction trigger pipeline
        // that `compact_history_if_needed` runs (sans the LLM-summary stage):
        //   1. enforce_tool_result_share_limit — caps any single tool result
        //   2. compact_tool_results — replaces older results with markers
        //
        // Regression for "ctx 100% but autocompaction never ran": this test
        // proves the non-LLM half of the pipeline actually shrinks an
        // over-budget history. If a future refactor breaks
        // `compact_tool_results`, this fails before the user notices the TUI
        // hanging at full context.
        let huge = "x".repeat(40_000); // ~10k tokens each
        let mut history = vec![
            make_user("q1"),
            make_tool_call_msg("", "c1", "run_shell"),
            make_tool_result("c1", &huge),
            make_assistant("a1"),
            make_user("q2"),
            make_tool_call_msg("", "c2", "run_shell"),
            make_tool_result("c2", &huge),
            make_assistant("a2"),
            make_user("q3"),
            make_tool_call_msg("", "c3", "run_shell"),
            make_tool_result("c3", &huge),
            make_assistant("a3"),
        ];
        let max = 2_000;
        let before = history_tokens(&history);
        assert!(
            before > max,
            "test setup: history must start over budget (was {before}, max {max})",
        );

        enforce_tool_result_share_limit(&mut history, max, 0.5);
        compact_tool_results(&mut history, max);

        let after = history_tokens(&history);
        assert!(
            after < before / 2,
            "compaction pipeline must materially shrink history ({before} -> {after})",
        );
        // Tool results must have been mutated — either truncated (the marker
        // discloses "omitted" output) or replaced by a compaction marker. If
        // neither, the pipeline didn't run.
        let any_mutated = history
            .iter()
            .filter_map(|m| m.text_content())
            .any(|t| t.contains("omitted") || t.contains("compacted"));
        assert!(
            any_mutated,
            "expected at least one tool result to be truncated or compacted",
        );
    }

    #[test]
    fn share_limit_skips_non_tool_messages() {
        let big = "x".repeat(100_000);
        let mut history = vec![make_user(&big), make_assistant(&big)];
        enforce_tool_result_share_limit(&mut history, 100, 0.5);
        // User and assistant messages should not be touched
        assert_eq!(history[0].text_content().unwrap(), big);
        assert_eq!(history[1].text_content().unwrap(), big);
    }

    // -- age_based_tool_result_degradation --

    /// Build a history with N user/assistant/tool triplets.
    fn make_history(n: usize) -> Vec<Message> {
        let mut history = Vec::new();
        for i in 0..n {
            let id = format!("call_{i}");
            history.push(make_user(&format!("turn {i}")));
            history.push(make_tool_call_msg("", &id, "run_shell"));
            // Large tool result — enough to trigger degradation
            let result_text: String = (0..50).map(|j| format!("line {j} of output\n")).collect();
            history.push(make_tool_result(&id, &result_text));
        }
        history
    }

    #[test]
    fn age_tier1_preserves_recent() {
        let mut history = make_history(4); // 12 messages, all within AGE_TIER1_WINDOW
        let original: Vec<String> = history
            .iter()
            .filter(|m| m.role == Role::Tool)
            .filter_map(|m| m.text_content().map(String::from))
            .collect();

        age_based_tool_result_degradation(&mut history);

        let after: Vec<String> = history
            .iter()
            .filter(|m| m.role == Role::Tool)
            .filter_map(|m| m.text_content().map(String::from))
            .collect();

        assert_eq!(original, after, "recent messages should be untouched");
    }

    #[test]
    fn age_tier2_truncates_large_results() {
        let mut history = make_history(10); // 30 messages
        age_based_tool_result_degradation(&mut history);

        // The oldest tool results should be truncated (tier 2 or 3)
        let first_tool = history.iter().find(|m| m.role == Role::Tool).unwrap();
        let text = first_tool.text_content().unwrap();
        assert!(
            text.contains("lines omitted") || text.contains("[tool result"),
            "old tool results should be degraded: {text}"
        );
    }

    #[test]
    fn age_tier3_replaces_old_results() {
        let mut history = make_history(12); // 36 messages — oldest are definitely tier 3
        age_based_tool_result_degradation(&mut history);

        let first_tool = history.iter().find(|m| m.role == Role::Tool).unwrap();
        let text = first_tool.text_content().unwrap();
        assert!(
            text.starts_with("[tool result"),
            "very old tool results should be one-liners: {text}"
        );
    }

    #[test]
    fn age_degradation_preserves_non_tool_messages() {
        let mut history = make_history(10);
        let user_msgs_before: Vec<String> = history
            .iter()
            .filter(|m| m.role == Role::User)
            .filter_map(|m| m.text_content().map(String::from))
            .collect();

        age_based_tool_result_degradation(&mut history);

        let user_msgs_after: Vec<String> = history
            .iter()
            .filter(|m| m.role == Role::User)
            .filter_map(|m| m.text_content().map(String::from))
            .collect();

        assert_eq!(user_msgs_before, user_msgs_after);
    }

    #[test]
    fn age_degradation_idempotent() {
        let mut history = make_history(10);
        age_based_tool_result_degradation(&mut history);
        let after_first: Vec<String> = history
            .iter()
            .filter_map(|m| m.text_content().map(String::from))
            .collect();

        age_based_tool_result_degradation(&mut history);
        let after_second: Vec<String> = history
            .iter()
            .filter_map(|m| m.text_content().map(String::from))
            .collect();

        assert_eq!(after_first, after_second, "running twice should be no-op");
    }

    #[test]
    fn age_degradation_empty_history() {
        let mut history: Vec<Message> = vec![];
        age_based_tool_result_degradation(&mut history);
        assert!(history.is_empty());
    }

    #[test]
    fn truncate_with_context_short_text() {
        let text = "line 1\nline 2\nline 3";
        assert_eq!(truncate_with_context(text), text);
    }

    #[test]
    fn truncate_with_context_long_text() {
        let lines: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
        let text = lines.join("\n");
        let result = truncate_with_context(&text);
        assert!(result.contains("line 0"));
        assert!(result.contains("lines omitted"));
        assert!(result.contains("line 19"));
    }
}
