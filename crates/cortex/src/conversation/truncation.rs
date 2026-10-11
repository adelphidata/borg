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

    for msg in history[half..].iter_mut().rev() {
        if msg.role != Role::Tool {
            continue;
        }
        // Only compact if the message has substantial content
        let placeholder = match &msg.content {
            Some(MessageContent::Text(s))
                if estimate_tokens(s) > constants::TOOL_RESULT_COMPACT_THRESHOLD =>
            {
                // Keep the outcome: a failed call must not read as a success
                // once its output is gone.
                match tool_outcome(s) {
                    Some(outcome) => format!("[compacted: output removed — {outcome}]"),
                    None => "[compacted: output removed]".to_string(),
                }
            }
            _ => continue,
        };
        msg.content = Some(MessageContent::Text(placeholder));
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
    degrade_old_tool_results(history, 0);
}

/// [`age_based_tool_result_degradation`] with a minimum-reclaim gate. Returns
/// the tokens reclaimed, or `0` when nothing was committed.
///
/// Degradation rewrites messages the provider has *already seen*, which
/// invalidates the prompt-cache prefix from the earliest rewritten position
/// onward. Because the tier windows are measured in messages from the end, that
/// position advances every turn: left ungated, one or two results cross a tier
/// boundary per turn, each rewrite breaks the cached prefix, and the session
/// pays full input price on every turn to save a few dozen tokens.
///
/// The gate makes those breaks episodic instead. Pending savings accumulate
/// across turns while the plan is repeatedly declined — declining costs nothing,
/// since a declined plan leaves history byte-identical — and commit in one batch
/// once they are worth a cache break. No extra state is needed for the
/// hysteresis: already-degraded messages yield no further savings, so the turn
/// after a commit naturally plans a near-empty batch and is declined again.
///
/// `min_reclaim_tokens == 0` disables the gate and restores the unconditional
/// behavior.
pub fn degrade_old_tool_results(history: &mut [Message], min_reclaim_tokens: usize) -> usize {
    let len = history.len();
    if len == 0 {
        return 0;
    }

    // Plan first, commit second: a rejected plan must not have touched history,
    // and planning is cheap enough (no LLM, no clone of the transcript) to redo
    // each turn.
    let mut plan: Vec<(usize, String)> = Vec::new();
    let mut reclaimed = 0usize;

    for (i, msg) in history.iter().enumerate() {
        if msg.role != Role::Tool {
            continue;
        }

        let age = len - 1 - i; // 0 = newest, len-1 = oldest
        if age < AGE_TIER1_WINDOW {
            continue; // recent: full fidelity
        }

        let text = match &msg.content {
            Some(MessageContent::Text(s)) => s.as_str(),
            _ => continue,
        };

        let toks = estimate_tokens(text);

        let replacement = if age < AGE_TIER2_WINDOW {
            // Tier 2: truncate large results
            if toks > AGE_TIER1_TOKEN_THRESHOLD {
                truncate_with_context(text)
            } else {
                continue;
            }
        } else {
            // Tier 3: one-liner
            if toks > AGE_TIER2_TOKEN_THRESHOLD {
                let status = tool_outcome(text).unwrap_or_else(|| "ok".to_string());
                let tool_id = msg.tool_call_id.as_deref().unwrap_or("unknown");
                format!("[tool result {tool_id} — {status}]")
            } else {
                continue;
            }
        };

        // A "degradation" that grows the message reclaims nothing; count only
        // real savings so a no-op rewrite can't drag the batch over the line.
        reclaimed += toks.saturating_sub(estimate_tokens(&replacement));
        plan.push((i, replacement));
    }

    if plan.is_empty() || reclaimed < min_reclaim_tokens {
        return 0;
    }

    for (i, replacement) in plan {
        history[i].content = Some(MessageContent::Text(replacement));
    }
    reclaimed
}

/// Leading wording of the results Borg writes when a call is refused rather
/// than run: shell policy, blocked paths, Plan mode, and a hook block that
/// gave no reason.
const DENIED_PREFIXES: &[&str] = &[
    crate::tool_handlers::SHELL_DENIED_PREFIX,
    "Access denied:",
    constants::PLAN_MODE_MUTATION_DENIED,
    constants::TOOL_SKIPPED_BY_HOOK,
];

/// Outcome of a tool result that went wrong, for the stubs that replace it:
/// `failed (exit N)`, `failed`, `denied`, or `cancelled`. `None` means the
/// call succeeded.
///
/// Executed results carry a `<tool_output …>` wrapper, so the body is read
/// past it; skipped calls (Plan mode, hook blocks, rate limits) are stored
/// unwrapped. A shell `Exit code:` line is authoritative when present, so a
/// clean exit whose output quotes an error stays ok.
pub(crate) fn tool_outcome(text: &str) -> Option<String> {
    let body = match text.strip_prefix("<tool_output") {
        Some(rest) => rest.split_once('\n').map_or("", |(_, b)| b),
        None => text,
    }
    .trim_start();

    if let Some(rest) = body.strip_prefix("Exit code:") {
        return match rest.lines().next().unwrap_or("").trim().parse::<i64>() {
            Ok(0) => None,
            Ok(n) => Some(format!("failed (exit {n})")),
            Err(_) => Some("failed".to_string()),
        };
    }
    if DENIED_PREFIXES.iter().any(|p| body.starts_with(p)) {
        return Some("denied".to_string());
    }
    if body.starts_with(constants::TOOL_CANCELLED_BY_USER) {
        return Some("cancelled".to_string());
    }
    let head = body.get(..6).unwrap_or(body).to_ascii_lowercase();
    // A JSON result reporting an `"error"` key counts as a failure too.
    if head.starts_with("error")
        || head.starts_with("failed")
        || (body.starts_with('{') && body.contains("\"error\""))
    {
        return Some("failed".to_string());
    }
    None
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
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
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
    fn reclaim_gate_leaves_history_byte_identical_when_declined() {
        // This is the whole point of the gate: a declined pass must not touch
        // a single byte, because any edit to already-sent history invalidates
        // the provider prompt-cache prefix from that position onward. Asserting
        // on a token count instead of exact content would miss a rewrite that
        // happened to be token-neutral.
        let mut history = make_history(12);
        let before: Vec<Option<String>> = history
            .iter()
            .map(|m| m.text_content().map(String::from))
            .collect();

        let reclaimed = degrade_old_tool_results(&mut history, usize::MAX);

        assert_eq!(reclaimed, 0, "an unreachable threshold must decline");
        let after: Vec<Option<String>> = history
            .iter()
            .map(|m| m.text_content().map(String::from))
            .collect();
        assert_eq!(before, after, "a declined pass must not rewrite history");
    }

    #[test]
    fn reclaim_gate_commits_once_the_batch_is_worth_a_cache_break() {
        // Same history, a threshold the batch clears: the pass commits and
        // reports what it saved.
        let mut history = make_history(12);
        let before = history_tokens(&history);

        let reclaimed = degrade_old_tool_results(&mut history, 1);

        assert!(reclaimed > 0, "an eligible batch must commit");
        let saved = before - history_tokens(&history);
        assert_eq!(
            reclaimed, saved,
            "the reported reclaim must match the actual history shrink"
        );
    }

    #[test]
    fn zero_threshold_reproduces_the_ungated_behavior() {
        // The escape hatch has to be exact — operators disabling the gate must
        // get the old semantics, not an approximation of them.
        let mut gated = make_history(12);
        let mut ungated = make_history(12);

        degrade_old_tool_results(&mut gated, 0);
        age_based_tool_result_degradation(&mut ungated);

        let gated_text: Vec<Option<String>> = gated
            .iter()
            .map(|m| m.text_content().map(String::from))
            .collect();
        let ungated_text: Vec<Option<String>> = ungated
            .iter()
            .map(|m| m.text_content().map(String::from))
            .collect();
        assert_eq!(gated_text, ungated_text);
    }

    #[test]
    fn a_committed_pass_declines_on_the_next_turn_without_extra_state() {
        // The hysteresis comes for free: once the eligible results are already
        // degraded, the next plan reclaims nothing and is declined, so the
        // cache prefix stays stable until enough new drift accumulates. If this
        // regresses, degradation re-fires every turn and the gate buys nothing.
        let mut history = make_history(12);
        assert!(degrade_old_tool_results(&mut history, 1) > 0);
        assert_eq!(
            degrade_old_tool_results(&mut history, 1),
            0,
            "a settled history must not keep re-breaking the cache"
        );
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

    // -- tool_outcome --

    /// Wrap a body the way `tool_execution` stores an executed tool result.
    fn wrapped(name: &str, body: &str) -> String {
        format!("<tool_output name=\"{name}\" trust=\"external\">\n{body}\n</tool_output>")
    }

    fn padded_output(lines: usize) -> String {
        (0..lines)
            .map(|j| format!("line {j} of output\n"))
            .collect()
    }

    #[test]
    fn tool_outcome_reads_real_result_shapes() {
        let out = padded_output(5);
        let cases: Vec<(&str, String, Option<&str>)> = vec![
            (
                "shell exit 0",
                wrapped(
                    "run_shell",
                    &format!("Exit code: 0\n--- stdout ---\n{out}--- stderr ---\n"),
                ),
                None,
            ),
            (
                "shell exit 0 quoting an error",
                wrapped(
                    "run_shell",
                    "Exit code: 0\n--- stdout ---\nError: none\n--- stderr ---\n",
                ),
                None,
            ),
            (
                "shell exit 1",
                wrapped(
                    "run_shell",
                    &format!("Exit code: 1\n--- stdout ---\n{out}--- stderr ---\n"),
                ),
                Some("failed (exit 1)"),
            ),
            (
                "shell killed (-1)",
                wrapped(
                    "run_shell",
                    "Exit code: -1\n--- stdout ---\n\n--- stderr ---\n",
                ),
                Some("failed (exit -1)"),
            ),
            (
                "tty exit 2",
                wrapped(
                    "run_shell",
                    &format!("Exit code: 2\n--- output (tty) ---\n{out}"),
                ),
                Some("failed (exit 2)"),
            ),
            (
                "shell policy denial",
                wrapped(
                    "run_shell",
                    &format!(
                        "{} by policy: recursive delete of /",
                        crate::tool_handlers::SHELL_DENIED_PREFIX
                    ),
                ),
                Some("denied"),
            ),
            (
                "blocked path",
                wrapped(
                    "read_file",
                    "Access denied: /home/u/.ssh/id_rsa is in a blocked path.",
                ),
                Some("denied"),
            ),
            (
                "plan mode (unwrapped skip)",
                constants::PLAN_MODE_MUTATION_DENIED.into(),
                Some("denied"),
            ),
            (
                "hook block without reason",
                constants::TOOL_SKIPPED_BY_HOOK.into(),
                Some("denied"),
            ),
            (
                "user cancel",
                constants::TOOL_CANCELLED_BY_USER.into(),
                Some("cancelled"),
            ),
            (
                "handler error",
                wrapped("web_fetch", "Error: connection refused"),
                Some("failed"),
            ),
            (
                "rate limit (unwrapped skip)",
                "Error: rate limit exceeded for shell commands".into(),
                Some("failed"),
            ),
            (
                "json error result",
                wrapped("schedule", "{\"error\": \"no such task\"}"),
                Some("failed"),
            ),
            ("plain success", wrapped("read_file", &out), None),
            (
                "success mentioning failure mid-text",
                wrapped("read_file", "fn main() {}\n// Failed attempts are logged\n"),
                None,
            ),
        ];
        for (label, text, want) in cases {
            assert_eq!(tool_outcome(&text).as_deref(), want, "{label}: {text}");
        }
    }

    #[test]
    fn degraded_one_liner_keeps_the_failure() {
        let mut history = make_history(12);
        let body = format!(
            "Exit code: 2\n--- stdout ---\n{}--- stderr ---\nboom\n",
            padded_output(50)
        );
        history[2] = make_tool_result("call_0", &wrapped("run_shell", &body));
        let ok_body = format!("Exit code: 0\n--- stdout ---\n{}", padded_output(50));
        history[5] = make_tool_result("call_1", &wrapped("run_shell", &ok_body));

        age_based_tool_result_degradation(&mut history);

        assert_eq!(
            history[2].text_content().unwrap(),
            "[tool result call_0 — failed (exit 2)]"
        );
        assert_eq!(
            history[5].text_content().unwrap(),
            "[tool result call_1 — ok]"
        );
    }

    #[test]
    fn compacted_placeholder_keeps_the_failure() {
        let failing = wrapped(
            "run_shell",
            &format!("Exit code: 1\n--- stdout ---\n{}", padded_output(40)),
        );
        let passing = wrapped(
            "run_shell",
            &format!("Exit code: 0\n--- stdout ---\n{}", padded_output(40)),
        );
        let mut history = vec![
            make_user("a"),
            make_assistant("b"),
            make_tool_call_msg("", "c1", "run_shell"),
            make_tool_result("c1", &failing),
            make_tool_call_msg("", "c2", "run_shell"),
            make_tool_result("c2", &passing),
        ];

        compact_tool_results(&mut history, 1);

        assert_eq!(
            history[3].text_content().unwrap(),
            "[compacted: output removed — failed (exit 1)]"
        );
        assert_eq!(
            history[5].text_content().unwrap(),
            "[compacted: output removed]"
        );
    }
}
