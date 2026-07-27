//! Free types and helpers for tool-call validation.
//!
//! None of these are methods on `Agent` — they are standalone validators / partitioners
//! consumed by the agent loop.

use std::collections::HashMap;
use std::time::Duration;

use rand::Rng;
use tracing::warn;

use crate::config::loop_guard::LoopGuardConfig;
use crate::constants;
use crate::types::{FunctionCall, Message, ToolCall};

/// Per-call verdict from [`ToolCallRepeatTracker::observe`]. The agent loop
/// dispatches on this:
///
/// - [`RepeatDecision::Allow`] — first emission of a `(name, args)` payload
///   in the current streak; execute normally.
/// - [`RepeatDecision::Backoff`] — identical follow-up before the cap; sleep
///   the returned duration, then execute. Buys the operator an
///   interrupt-window before the hard block fires.
/// - [`RepeatDecision::Block`] — at or past `identical_call_max`; skip
///   execution and substitute a synthetic error tool_result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RepeatDecision {
    Allow,
    Backoff(Duration),
    Block,
}

/// Detects runs of identical `(tool_name, arguments)` tool calls within a
/// single user turn and escalates the response: pass-through → jittered
/// exponential backoff → hard block. Counts strictly-consecutive matches: a
/// differing call resets the streak.
///
/// Equality is byte-exact on the raw `arguments` string — no whitespace or
/// JSON-key-order normalization. A stuck LLM that emits the same canonical
/// payload every iteration will be caught; one that randomizes whitespace
/// will not. Trade-off accepted to keep the guard cheap and avoid
/// hashing-vs-parsing correctness questions.
///
/// Defends against the stuck-LLM failure mode: an agent looping on the same
/// call burns rate-guard quota and pollutes context until the request
/// overflows. The 25-iteration ceiling caps damage but doesn't notice
/// duplication; this guard stops the spam earlier and the backoff window
/// gives an operator time to interrupt before the hard cap.
#[derive(Debug)]
pub(crate) struct ToolCallRepeatTracker {
    pub(super) last_signature: Option<u64>,
    pub(super) consecutive_count: u32,
    identical_call_max: u32,
    backoff_initial_ms: u64,
    backoff_max_ms: u64,
}

impl Default for ToolCallRepeatTracker {
    fn default() -> Self {
        Self::new(&LoopGuardConfig::default())
    }
}

impl ToolCallRepeatTracker {
    pub(crate) fn new(cfg: &LoopGuardConfig) -> Self {
        Self {
            last_signature: None,
            consecutive_count: 0,
            // Floor at 2: a value of 1 would block the very first call
            // (count=1 >= cap=1 → Block), bricking the agent. The `nonzero`
            // setter rejects 0 but lets 1 through; clamp here as a defense.
            identical_call_max: cfg.identical_call_max.max(2),
            backoff_initial_ms: cfg.backoff_initial_ms,
            backoff_max_ms: cfg.backoff_max_ms.max(cfg.backoff_initial_ms),
        }
    }

    /// Hash `(name, args)`, update streak state, and return the per-call
    /// decision. The signature is stored regardless of the decision so the
    /// streak keeps growing across already-blocked calls — every repeated
    /// call past the cap stays blocked, not just the first one to trip it.
    pub(super) fn observe(&mut self, name: &str, args: &str) -> RepeatDecision {
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
        self.decision_for(self.consecutive_count)
    }

    fn decision_for(&self, count: u32) -> RepeatDecision {
        if count >= self.identical_call_max {
            return RepeatDecision::Block;
        }
        if count <= 1 {
            return RepeatDecision::Allow;
        }
        // count is one of 2..identical_call_max. base doubles each step
        // starting at backoff_initial_ms; saturating_mul guards against
        // overflow at large caps; the clamp on backoff_max_ms gives the
        // operator a predictable ceiling on the un-jittered delay.
        let steps = count - 2; // 0-based exponent: count=2 → 2^0
        let multiplier = 1u64.checked_shl(steps.min(63)).unwrap_or(u64::MAX);
        let base = self
            .backoff_initial_ms
            .saturating_mul(multiplier)
            .min(self.backoff_max_ms);
        let factor: f64 = rand::rng().random_range(0.75..1.25);
        let jittered = (base as f64 * factor).round() as u64;
        RepeatDecision::Backoff(Duration::from_millis(jittered))
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

/// Tracks how many times each hallucinated tool name has been observed in
/// the current turn. The first attempt on a given name surfaces the soft
/// `"Unknown tool: X"` error so a typo can be corrected; after
/// `unknown_tool_max_per_name` attempts, the agent substitutes a terminal
/// refusal so the model breaks out of the retry cycle instead of burning
/// the whole iteration budget on a name that will never resolve.
#[derive(Debug)]
pub(crate) struct UnknownToolTracker {
    seen: HashMap<String, u32>,
    max_per_name: u32,
}

impl Default for UnknownToolTracker {
    fn default() -> Self {
        Self::new(&LoopGuardConfig::default())
    }
}

impl UnknownToolTracker {
    pub(crate) fn new(cfg: &LoopGuardConfig) -> Self {
        Self {
            seen: HashMap::new(),
            // Floor at 1: 0 would refuse on the very first unknown-name call, same failure mode as identical_call_max=1 above.
            max_per_name: cfg.unknown_tool_max_per_name.max(1),
        }
    }

    /// Record one observation of `name` and return the running count
    /// (1-based). The caller compares against [`Self::should_refuse`] to
    /// decide between the soft error and the terminal refusal.
    pub(super) fn observe(&mut self, name: &str) -> u32 {
        let entry = self.seen.entry(name.to_string()).or_insert(0);
        *entry = entry.saturating_add(1);
        *entry
    }

    /// Returns true once a given name's running count has reached the
    /// configured ceiling. Encodes the policy in one place so the two
    /// dispatch call sites stay in lockstep.
    pub(super) fn should_refuse(&self, count: u32) -> bool {
        count >= self.max_per_name
    }

    pub(super) fn reset(&mut self) {
        self.seen.clear();
    }
}

/// Synthetic tool_result body for a hallucinated tool name that has been
/// refused enough times to break the retry cycle. The "does not exist"
/// phrasing is stable across providers so any future grep / regression
/// test can detect the terminal-refusal surface without coupling to a
/// model-specific apology pattern.
pub(super) fn unknown_tool_refusal_message(name: &str, count: u32) -> String {
    format!(
        "Error: tool '{name}' does not exist and has been refused {count} times this turn. \
         Available tools are listed in the system prompt. Stop calling '{name}' — \
         either invoke a real tool or respond to the user with text."
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
///
/// Duplicate ids within one batch are uniquified here — first occurrence
/// keeps its id, later collisions get a deterministic `<id>_d<n>` suffix.
/// Some providers reuse one id across different calls in a batch (degraded
/// models at long context, OpenAI-compatible shims); left as-is, both calls
/// execute but the next iteration's history dedup keeps only the first
/// call/result pair per id, so the second call's real output silently
/// vanishes and the model confabulates it. The suffix is deterministic —
/// never a random UUID — so replayed histories stay byte-stable for prompt
/// caching. Renaming here (before the assistant message is built from the
/// partitioned output) keeps the call record, dispatch, and results
/// consistent under the same renamed id.
pub(super) fn partition_tool_calls(
    tool_calls: &[PartialToolCall],
    truncated_due_to_length: bool,
) -> (Vec<ToolCall>, Vec<InvalidToolCall>) {
    let mut valid = Vec::new();
    let mut invalid = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();
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
        let id = if seen_ids.insert(ptc.id.clone()) {
            ptc.id.clone()
        } else {
            let mut n = 1usize;
            let mut candidate = format!("{}_d{n}", ptc.id);
            while !seen_ids.insert(candidate.clone()) {
                n += 1;
                candidate = format!("{}_d{n}", ptc.id);
            }
            warn!(
                "Duplicate tool call id '{}' (name='{}') renamed to '{candidate}' to keep \
                 call/result pairing lossless",
                ptc.id, ptc.name
            );
            candidate
        };
        if ptc.name.is_empty() {
            // A tool call with an id but no name is a "phantom" the model can
            // loop on. Unlike a missing id, this one is addressable, so we feed
            // back a terse error instead of silently dropping it — silence
            // gives the model no signal and it re-emits the same empty call.
            // We can't echo an empty function name to the provider (Anthropic /
            // OpenAI reject a nameless tool_use block), so the synthetic call
            // carries a placeholder name. The placeholder MUST satisfy the
            // provider tool-name pattern `^[a-zA-Z0-9_-]{1,64}$` — a name with
            // spaces/parens is echoed back verbatim as the tool_use `name` on
            // the next turn and 400s the whole conversation, strictly worse than
            // the old silent drop. The reason stays terse so we don't prime
            // further bad calls with a tool-catalog dump.
            warn!(
                "Surfacing terse error for unnamed tool call (id={})",
                ptc.id
            );
            invalid.push(InvalidToolCall {
                id: id.clone(),
                name: "missing_name".to_string(),
                reason: "tool call had no tool name — re-issue the call with a valid tool name"
                    .to_string(),
                raw_arguments: ptc.arguments.clone(),
            });
            continue;
        }
        if ptc.name.len() > constants::MAX_TOOL_NAME_LEN {
            let reason = format!("tool name exceeds {} bytes", constants::MAX_TOOL_NAME_LEN);
            warn!("Invalid tool call '{}': {}", ptc.name, reason);
            invalid.push(InvalidToolCall {
                id: id.clone(),
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
                id: id.clone(),
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
                id: id.clone(),
                name: ptc.name.clone(),
                reason,
                raw_arguments: ptc.arguments.clone(),
            });
            continue;
        }
        valid.push(ToolCall {
            id: id.clone(),
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

/// Merge one `ToolCallDelta` into the index-keyed accumulator. Keying by
/// `index` (not `id`) is load-bearing: providers that emit parallel tool calls
/// (OpenAI `parallel_tool_calls`, Kimi-for-coding, etc.) send `id` and `name`
/// only on the first delta of each call; subsequent deltas carry only
/// `arguments` fragments tagged with their `index`. An `id`-keyed aggregator
/// collapses later argument fragments into the wrong call or spawns phantoms.
///
/// Returns `false` and emits a `tracing::warn!` when `index >= max_tool_calls`;
/// caller discards the delta.
pub(super) fn apply_tool_call_delta(
    tool_calls: &mut Vec<PartialToolCall>,
    index: usize,
    id: Option<String>,
    name: Option<String>,
    arguments_delta: &str,
    max_tool_calls: usize,
) -> bool {
    if index >= max_tool_calls {
        warn!("Tool call index {index} exceeds limit {max_tool_calls}, ignoring");
        return false;
    }
    while tool_calls.len() <= index {
        tool_calls.push(PartialToolCall::default());
    }
    if let Some(id) = id {
        tool_calls[index].id = id;
    }
    if let Some(name) = name {
        tool_calls[index].name = name;
    }
    tool_calls[index].arguments.push_str(arguments_delta);
    true
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
            seq: None,
        }
    }

    #[test]
    fn parallel_tool_call_deltas_aggregate_by_index_not_id() {
        // Regression guard: when a provider streams two parallel tool calls,
        // `id` and `name` arrive only on the first delta of each call;
        // subsequent argument fragments carry only `index`. If the aggregator
        // ever keyed by `id` instead of `index`, the later argument fragments
        // (which arrive with `id = None`) would collapse into call 0 or spawn
        // a phantom third entry. Interleave indices 0 and 1 to force the
        // failure if that regression returns.
        let mut tool_calls: Vec<PartialToolCall> = Vec::new();

        // First chunk: both calls announce themselves with id+name+arg-start.
        assert!(apply_tool_call_delta(
            &mut tool_calls,
            0,
            Some("call_a".into()),
            Some("fs_read".into()),
            "{\"path\":",
            64,
        ));
        assert!(apply_tool_call_delta(
            &mut tool_calls,
            1,
            Some("call_b".into()),
            Some("run_shell".into()),
            "{\"cmd\":",
            64,
        ));

        // Subsequent chunks: only index + argument fragment. Interleave the
        // order so an id-keyed aggregator can't accidentally pass by luck.
        assert!(apply_tool_call_delta(
            &mut tool_calls,
            1,
            None,
            None,
            "\"ls\"",
            64
        ));
        assert!(apply_tool_call_delta(
            &mut tool_calls,
            0,
            None,
            None,
            "\"/etc\"",
            64
        ));
        assert!(apply_tool_call_delta(
            &mut tool_calls,
            0,
            None,
            None,
            "}",
            64
        ));
        assert!(apply_tool_call_delta(
            &mut tool_calls,
            1,
            None,
            None,
            "}",
            64
        ));

        assert_eq!(tool_calls.len(), 2, "must produce exactly two calls");
        assert_eq!(tool_calls[0].id, "call_a");
        assert_eq!(tool_calls[0].name, "fs_read");
        assert_eq!(tool_calls[0].arguments, "{\"path\":\"/etc\"}");
        assert_eq!(tool_calls[1].id, "call_b");
        assert_eq!(tool_calls[1].name, "run_shell");
        assert_eq!(tool_calls[1].arguments, "{\"cmd\":\"ls\"}");
    }

    #[test]
    fn apply_tool_call_delta_discards_above_cap() {
        let mut tool_calls: Vec<PartialToolCall> = Vec::new();
        assert!(!apply_tool_call_delta(
            &mut tool_calls,
            64,
            Some("x".into()),
            Some("y".into()),
            "{}",
            64,
        ));
        assert!(tool_calls.is_empty(), "over-cap delta must not grow vec");
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
    fn partition_tool_calls_drops_calls_with_no_id() {
        // No id → no way to address a synthetic tool_result, so it stays dropped.
        let calls = vec![ptc("", "apply_patch", "{}")];
        let (valid, invalid) = partition_tool_calls(&calls, false);
        assert!(valid.is_empty());
        assert!(
            invalid.is_empty(),
            "a call with no id must not be promoted to invalid — nothing to address",
        );
    }

    #[test]
    fn partition_tool_calls_surfaces_terse_error_for_unnamed_call() {
        // id present but no name: addressable phantom. It must produce a terse
        // invalid result (so the model gets feedback and stops looping) rather
        // than being silently dropped, and must carry a non-empty placeholder
        // name so the echoed tool_call doesn't violate the provider contract.
        let calls = vec![ptc("call_1", "", "{}")];
        let (valid, invalid) = partition_tool_calls(&calls, false);
        assert!(valid.is_empty());
        assert_eq!(
            invalid.len(),
            1,
            "unnamed-but-addressable call must be surfaced"
        );
        assert_eq!(invalid[0].id, "call_1");
        // The placeholder is echoed back as the provider tool_use `name`, so it
        // must match `^[a-zA-Z0-9_-]{1,64}$` — spaces/parens 400 the whole
        // conversation, worse than a silent drop.
        let name = &invalid[0].name;
        assert!(
            !name.is_empty()
                && name.len() <= 64
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "placeholder name must satisfy the provider tool-name pattern, got: {name:?}",
        );
        assert!(
            invalid[0].reason.contains("no tool name"),
            "reason must tell the model the name was missing, got: {}",
            invalid[0].reason,
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
    fn partition_tool_calls_uniquifies_duplicate_ids() {
        // Providers can reuse one call id across different calls in a batch.
        // Both calls must survive with distinct, deterministic ids — losing
        // one means its result silently vanishes from replayed history and
        // the model confabulates the missing output.
        let calls = vec![
            ptc("call_1", "read_file", "{\"path\": \"a.txt\"}"),
            ptc("call_1", "read_file", "{\"path\": \"b.txt\"}"),
            ptc("call_1", "read_file", "{\"path\": \"c.txt\"}"),
        ];
        let (valid, invalid) = partition_tool_calls(&calls, false);
        assert!(invalid.is_empty());
        assert_eq!(valid.len(), 3, "all duplicate-id calls must survive");
        assert_eq!(valid[0].id, "call_1", "first occurrence keeps its id");
        assert_eq!(valid[1].id, "call_1_d1");
        assert_eq!(valid[2].id, "call_1_d2");
        // Deterministic across runs: same batch → same renames (prompt-cache
        // prefix stability on replay depends on this).
        let (again, _) = partition_tool_calls(&calls, false);
        assert_eq!(
            valid.iter().map(|c| &c.id).collect::<Vec<_>>(),
            again.iter().map(|c| &c.id).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn partition_tool_calls_uniquify_spans_valid_and_invalid() {
        // A duplicate between a valid call and an invalid one must also be
        // renamed — both produce tool_results, and a shared id would collapse
        // them into one pair on the next iteration's history dedup. A crafted
        // id that pre-collides with the rename scheme must be stepped over.
        let calls = vec![
            ptc("call_1", "read_file", "{\"path\": \"a.txt\"}"),
            ptc("call_1_d1", "read_file", "{}"),
            ptc("call_1", "apply_patch", "{not json"),
        ];
        let (valid, invalid) = partition_tool_calls(&calls, false);
        assert_eq!(valid.len(), 2);
        assert_eq!(invalid.len(), 1);
        assert_eq!(valid[0].id, "call_1");
        assert_eq!(valid[1].id, "call_1_d1");
        assert_eq!(
            invalid[0].id, "call_1_d2",
            "rename must step over a pre-existing colliding id"
        );
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

    fn default_tracker() -> ToolCallRepeatTracker {
        ToolCallRepeatTracker::new(&LoopGuardConfig::default())
    }

    fn tracker_with(initial: u64, max: u64, cap: u32) -> ToolCallRepeatTracker {
        ToolCallRepeatTracker::new(&LoopGuardConfig {
            identical_call_max: cap,
            backoff_initial_ms: initial,
            backoff_max_ms: max,
            unknown_tool_max_per_name: 2,
        })
    }

    fn assert_backoff_in_band(d: &RepeatDecision, base_ms: u64) {
        let RepeatDecision::Backoff(dur) = d else {
            panic!("expected Backoff, got {d:?}");
        };
        let lo = (base_ms as f64 * 0.75).round() as u128;
        let hi = (base_ms as f64 * 1.25).round() as u128;
        let actual = dur.as_millis();
        assert!(
            (lo..=hi).contains(&actual),
            "backoff {actual}ms must land in jitter band [{lo}, {hi}] for base {base_ms}ms"
        );
    }

    #[test]
    fn repeat_count_1_returns_allow() {
        // Spec: first emission of a `(name, args)` payload is never throttled,
        // regardless of config. The streak only starts climbing from the 2nd
        // identical call.
        let mut t = default_tracker();
        assert_eq!(t.observe("run_shell", "echo a"), RepeatDecision::Allow);
    }

    #[test]
    fn repeat_count_2_returns_backoff_in_jitter_band() {
        // Spec: the 2nd identical call is the first one to back off, and the
        // delay must land in [0.75 * initial, 1.25 * initial]. Asserts the
        // configured initial value with the jitter band rather than a
        // hard-coded number so the test survives default tuning changes.
        let mut t = tracker_with(100, 800, 5);
        assert_eq!(t.observe("run_shell", "echo a"), RepeatDecision::Allow);
        assert_backoff_in_band(&t.observe("run_shell", "echo a"), 100);
    }

    #[test]
    fn repeat_backoff_doubles_until_cap_with_jitter() {
        // Spec: with identical_call_max = 5, initial = 100ms, max = 300ms,
        // the un-jittered base ladder is 100, 200, 300 (last value clamped),
        // each within the ±25% jitter band, then Block. Encodes the doubling
        // + clamp + jitter invariants simultaneously.
        let mut t = tracker_with(100, 300, 5);
        assert_eq!(t.observe("run_shell", "x"), RepeatDecision::Allow);
        assert_backoff_in_band(&t.observe("run_shell", "x"), 100);
        assert_backoff_in_band(&t.observe("run_shell", "x"), 200);
        assert_backoff_in_band(&t.observe("run_shell", "x"), 300);
        assert_eq!(t.observe("run_shell", "x"), RepeatDecision::Block);
    }

    #[test]
    fn repeat_backoff_jitter_is_stochastic() {
        // Spec: jitter must actually fuzz the delay — a regression where
        // jitter is wired but always returns the midpoint (or zero) would
        // leave a synchronized retry pattern. Sample 200 times at count=2
        // with a fixed config and assert the band is tightly bracketed,
        // catching the silent-midpoint failure mode.
        let initial = 1000u64;
        let mut minimum = u128::MAX;
        let mut maximum = 0u128;
        for _ in 0..200 {
            let mut t = tracker_with(initial, initial * 4, 5);
            let _ = t.observe("run_shell", "x");
            if let RepeatDecision::Backoff(d) = t.observe("run_shell", "x") {
                minimum = minimum.min(d.as_millis());
                maximum = maximum.max(d.as_millis());
            } else {
                panic!("expected Backoff at count=2");
            }
        }
        assert!(
            minimum <= (initial as f64 * 0.85) as u128,
            "min jitter {minimum}ms must reach below 0.85x initial {initial}ms — \
             jitter wiring may be broken (always returning midpoint?)"
        );
        assert!(
            maximum >= (initial as f64 * 1.15) as u128,
            "max jitter {maximum}ms must reach above 1.15x initial {initial}ms — \
             jitter wiring may be broken (always returning midpoint?)"
        );
    }

    #[test]
    fn repeat_count_max_returns_block() {
        let mut t = tracker_with(50, 200, 3);
        let _ = t.observe("run_shell", "x");
        let _ = t.observe("run_shell", "x");
        assert_eq!(t.observe("run_shell", "x"), RepeatDecision::Block);
    }

    #[test]
    fn repeat_identical_call_max_one_does_not_brick_agent() {
        // Regression: a misconfigured `identical_call_max = 1` would, under a
        // naive `count >= cap` check, block the very first call (count=1 ≥
        // cap=1 → Block). The constructor must floor the cap at 2 so the
        // agent always gets to make at least one tool call before backoff.
        let cfg = LoopGuardConfig {
            identical_call_max: 1,
            backoff_initial_ms: 50,
            backoff_max_ms: 200,
            unknown_tool_max_per_name: 2,
        };
        let mut t = ToolCallRepeatTracker::new(&cfg);
        assert_eq!(
            t.observe("run_shell", "x"),
            RepeatDecision::Allow,
            "first call must always execute regardless of misconfigured cap"
        );
    }

    #[test]
    fn repeat_keeps_blocking_after_threshold() {
        let mut t = tracker_with(50, 200, 3);
        let _ = t.observe("run_shell", "x");
        let _ = t.observe("run_shell", "x");
        assert_eq!(t.observe("run_shell", "x"), RepeatDecision::Block);
        assert_eq!(t.observe("run_shell", "x"), RepeatDecision::Block);
    }

    #[test]
    fn repeat_different_args_reset_ladder() {
        // Spec: a differing call resets the streak to 1 → Allow. Prevents the
        // "ladder sticky after divergence" regression where a backoff state
        // would leak into the next, unrelated tool call.
        let mut t = tracker_with(100, 800, 4);
        let _ = t.observe("run_shell", "A");
        let _ = t.observe("run_shell", "A");
        // Diverge — fresh streak starts.
        assert_eq!(t.observe("run_shell", "B"), RepeatDecision::Allow);
        // And the next identical to B is still only count=2, not count=4.
        assert_backoff_in_band(&t.observe("run_shell", "B"), 100);
    }

    #[test]
    fn repeat_interleaved_never_throttles() {
        let mut t = tracker_with(100, 800, 3);
        for _ in 0..10 {
            assert_eq!(t.observe("run_shell", "x"), RepeatDecision::Allow);
            assert_eq!(t.observe("read_file", "x"), RepeatDecision::Allow);
        }
    }

    #[test]
    fn repeat_different_names_same_args_not_throttled() {
        let mut t = tracker_with(100, 800, 3);
        assert_eq!(
            t.observe("read_file", "{\"path\":\"x\"}"),
            RepeatDecision::Allow
        );
        assert_eq!(
            t.observe("list_dir", "{\"path\":\"x\"}"),
            RepeatDecision::Allow
        );
        assert_eq!(
            t.observe("read_file", "{\"path\":\"x\"}"),
            RepeatDecision::Allow
        );
    }

    #[test]
    fn repeat_reset_clears_state() {
        // Spec: reset() must zero the streak so the next identical call returns
        // Allow, mirroring the per-turn lifecycle: the agent loop resets at the
        // top of every iteration.
        let mut t = tracker_with(50, 200, 3);
        let _ = t.observe("run_shell", "x");
        let _ = t.observe("run_shell", "x");
        assert_eq!(t.observe("run_shell", "x"), RepeatDecision::Block);
        t.reset();
        assert_eq!(t.consecutive_count, 0);
        assert_eq!(t.last_signature, None);
        assert_eq!(t.observe("run_shell", "x"), RepeatDecision::Allow);
    }

    #[test]
    fn repeat_block_message_mentions_call_and_count() {
        let body = repeated_tool_call_block_message("run_shell", 3);
        assert!(body.contains("run_shell"), "must include tool name: {body}");
        assert!(body.contains("3"), "must include count: {body}");
        assert!(
            body.contains("Vary arguments") || body.contains("vary arguments"),
            "must instruct LLM to vary or stop: {body}"
        );
    }

    fn unknown_tracker_with(max_per_name: u32) -> UnknownToolTracker {
        UnknownToolTracker::new(&LoopGuardConfig {
            identical_call_max: 3,
            backoff_initial_ms: 500,
            backoff_max_ms: 5000,
            unknown_tool_max_per_name: max_per_name,
        })
    }

    #[test]
    fn unknown_tool_first_attempt_returns_count_1() {
        // Spec: a typo on a tool name must surface the soft "Unknown tool: X"
        // error on the first attempt so the LLM can recover. The terminal
        // refusal only fires after `unknown_tool_max_per_name` strikes.
        let mut t = unknown_tracker_with(2);
        assert_eq!(t.observe("fly_to_moon"), 1);
        assert!(!t.should_refuse(1));
    }

    #[test]
    fn unknown_tool_distinct_names_track_independently() {
        // Spec: a hallucination on one name must not burn the budget for an
        // unrelated hallucination on a different name. Prevents the
        // pathological case where a typo on tool A pre-poisons tool B.
        let mut t = unknown_tracker_with(2);
        assert_eq!(t.observe("foo"), 1);
        assert_eq!(t.observe("bar"), 1);
        assert_eq!(t.observe("foo"), 2);
        assert_eq!(t.observe("bar"), 2);
    }

    #[test]
    fn unknown_tool_repeated_name_increments_to_refusal() {
        // Spec: at unknown_tool_max_per_name, should_refuse must flip true.
        // Proves the threshold is reachable and is the gate the dispatch
        // call site relies on.
        let mut t = unknown_tracker_with(2);
        let first = t.observe("fly_to_moon");
        let second = t.observe("fly_to_moon");
        assert_eq!(first, 1);
        assert_eq!(second, 2);
        assert!(!t.should_refuse(first));
        assert!(t.should_refuse(second));
    }

    #[test]
    fn unknown_tool_reset_clears_per_name_state() {
        let mut t = unknown_tracker_with(2);
        let _ = t.observe("fly_to_moon");
        let _ = t.observe("fly_to_moon");
        t.reset();
        assert_eq!(
            t.observe("fly_to_moon"),
            1,
            "after reset the same name must read as a fresh first attempt"
        );
    }

    #[test]
    fn unknown_tool_refusal_message_includes_name_and_count_and_does_not_exist() {
        // Spec: the terminal refusal must carry the name, the count, and the
        // stable phrase "does not exist" so the model sees a hard-stop
        // signal distinct from the soft "Unknown tool" first-attempt error.
        // This is the user-visible string that breaks the retry cycle —
        // changing it shape-shifts behavior across providers.
        let body = unknown_tool_refusal_message("fly_to_moon", 3);
        assert!(body.contains("fly_to_moon"));
        assert!(body.contains("3"));
        assert!(
            body.contains("does not exist"),
            "stable phrase 'does not exist' must remain in the refusal — \
             it is the signal the model uses to stop retrying: {body}"
        );
    }
}
