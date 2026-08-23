//! Head/tail compaction planners and full compaction orchestrators that
//! weave LLM summarization in with the planner output.

use tracing::{debug, instrument, warn};

use crate::constants;
use crate::constants::COMPACTION_MARKER_TOKENS;
use crate::llm::LlmClient;
use crate::types::{ContentPart, Message, MessageContent, Role};

use super::summarize::{summarize_with_llm, summarize_with_llm_v2, SummarizeFailure};
use super::tokens::{history_tokens, message_tokens};

/// Opening fence of the XML-tagged compaction marker. The marker is wrapped
/// in `<compaction_summary trust="internal">...</compaction_summary>` so the
/// agent's XML trust boundaries keep the summary on the internal side — and
/// so user messages that happen to begin with the human-facing heading
/// string cannot hijack the iterative-summary path.
pub const COMPACTION_MARKER_OPEN: &str = "<compaction_summary trust=\"internal\">";
/// Closing fence of the XML-tagged compaction marker.
pub const COMPACTION_MARKER_CLOSE: &str = "</compaction_summary>";
/// Human-facing heading rendered above the tagged summary so transcripts
/// stay readable. Not used for marker detection — detection uses the tag
/// fence which user input cannot forge through
/// [`crate::security::sanitize::sanitize_xml_boundaries`].
pub const COMPACTION_MARKER_HEADING: &str =
    "[Earlier conversation was summarized to fit context limits.]";

/// Render a compaction summary body as the marker message the model reads:
/// the human-facing heading above the XML-tagged fence that keeps the summary
/// on the internal trust side. This is the single source of the marker's
/// shape, shared by the live in-band fold ([`compact_history_v2`]) and the
/// log-replay projection ([`crate::conversation::project_history`]) so a
/// resumed session renders a byte-identical summary. The body must already be
/// sanitized (see [`crate::security::sanitize::sanitize_xml_boundaries`]).
pub fn render_compaction_marker(body: &str) -> Message {
    Message::user(format!(
        "{COMPACTION_MARKER_HEADING}\n\n{COMPACTION_MARKER_OPEN}\n{body}\n{COMPACTION_MARKER_CLOSE}"
    ))
}

/// Flatten the summarized region into one text blob for anchor harvesting.
///
/// Reads the same messages the summarizer was given, so the index can only ever
/// contain identifiers that were genuinely in the compacted region.
fn summarizer_region_text(middle: &[Message]) -> String {
    middle
        .iter()
        .filter_map(|m| m.text_content())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A plan for compacting a message history: which head/tail slices to
/// preserve and which middle range to summarize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionPlan {
    /// Exclusive end of the head-protected region: messages `[0..head_end)`
    /// are kept verbatim at the start.
    pub head_end: usize,
    /// Inclusive start of the tail-protected region: messages
    /// `[tail_start..]` are kept verbatim at the end.
    pub tail_start: usize,
}

/// Reserve the model's output-token budget out of the context window when
/// sizing the working history.
///
/// The prompt (system + tools + history) and the generated response share one
/// context window, so the usable *input* budget is
/// `context_window - output_reservation`. A history budget configured larger than
/// that lets the conversation grow until the provider rejects the request for
/// lack of room to answer — compaction never fires because the threshold was
/// computed against the full window. This clamps the configured history budget to
/// the usable input budget so compaction triggers early enough to leave the
/// reserved output space.
///
/// Floored at a quarter of the window so a pathological config
/// (`output_reservation >= context_window`) still leaves room to make progress
/// rather than collapsing the budget to zero.
pub fn reserve_output_budget(
    configured_history_budget: usize,
    context_window: usize,
    output_reservation: usize,
) -> usize {
    let usable = context_window
        .saturating_sub(output_reservation)
        .max(context_window / 4);
    configured_history_budget.min(usable)
}

/// Effective history budget for a config: the configured cap clamped by the
/// active model's usable window (context window minus the output
/// reservation).
///
/// Single source for the agent loop's compaction threshold and any surface
/// that displays context occupancy — a percentage computed against the raw
/// configured cap overstates headroom whenever the model's window is the
/// binding constraint (e.g. a 32k-window model under a 100k configured cap).
pub fn effective_history_budget_for(config: &crate::config::Config) -> usize {
    reserve_output_budget(
        config.conversation.max_history_tokens,
        crate::llm::model_registry::context_window_for(&config.llm.model) as usize,
        config.llm.max_tokens as usize,
    )
}

/// Decay head protection across repeated compactions.
///
/// `protect_first_n` keeps the original user request and early framing verbatim
/// through the *first* compaction; that content is then captured in the
/// compaction summary marker. Re-pinning those raw turns on every later pass
/// fossilizes them into every future window and starves the tail of budget, so
/// once a prior summary exists the effective head protection decays to zero.
///
/// `has_prior_summary` is the durable signal a compaction already ran (the
/// summary marker is itself a projection of the session log, so there is no
/// separate counter to drift).
pub fn effective_head_protection(protect_first_n: usize, has_prior_summary: bool) -> usize {
    if has_prior_summary {
        0
    } else {
        protect_first_n
    }
}

/// Strip reasoning-trace spans (`<think>`, `<thinking>`, `<reasoning>`,
/// `<internal>`, any ASCII case) from text bound for — or produced by — the
/// compaction summarizer. A summary that quotes a reasoning trace gets
/// folded into the next iterative summary, so traces compound across
/// compactions and bloat every future context window. Thin domain alias for
/// the crate-wide [`crate::internal_tag_filter::strip_internal_tags`], which
/// owns the stripped-tag set and the unclosed-tag drop semantics.
fn strip_reasoning_traces(text: &str) -> String {
    crate::internal_tag_filter::strip_internal_tags(text)
}

/// Copy `messages` for the summarizer with reasoning-trace spans removed
/// from assistant text. Non-assistant messages pass through untouched — a
/// user quoting a `<think>` tag is content, not a trace.
fn sanitize_for_summarizer(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|m| {
            if m.role != Role::Assistant {
                return m.clone();
            }
            let mut cleaned = m.clone();
            match &m.content {
                Some(MessageContent::Text(t)) => {
                    cleaned.content = Some(MessageContent::Text(strip_reasoning_traces(t)));
                }
                Some(MessageContent::Parts(parts)) => {
                    let stripped = parts
                        .iter()
                        .map(|p| match p {
                            ContentPart::Text(t) => ContentPart::Text(strip_reasoning_traces(t)),
                            other => other.clone(),
                        })
                        .collect();
                    cleaned.content = Some(MessageContent::Parts(stripped));
                }
                None => {}
            }
            cleaned
        })
        .collect()
}

/// Determine which messages need to be dropped for compaction.
/// Returns `Some(keep_from_index)` if compaction is needed, `None` otherwise.
pub fn plan_compaction(history: &[Message], max_tokens: usize) -> Option<usize> {
    let total = history_tokens(history);
    // Trigger compaction with a safety margin to leave headroom for the next response
    let threshold = (max_tokens as f64 * constants::COMPACTION_SAFETY_MARGIN) as usize;
    if total <= threshold {
        return None;
    }

    debug!(
        "Conversation history ({total} tokens) exceeds budget ({max_tokens} tokens), compacting"
    );

    // Walk backwards from the end, accumulating tokens for messages we keep.
    let mut keep_from = history.len();
    let mut budget_used: usize = 0;
    // Reserve tokens for the truncation marker we'll insert.
    let marker_tokens = COMPACTION_MARKER_TOKENS;
    let effective_budget = max_tokens.saturating_sub(marker_tokens);

    for i in (0..history.len()).rev() {
        let msg_tok = message_tokens(&history[i]);
        if budget_used + msg_tok > effective_budget {
            break;
        }
        budget_used += msg_tok;
        keep_from = i;
    }

    // Ensure we keep at least the last message.
    if keep_from >= history.len() {
        keep_from = history.len().saturating_sub(1);
    }

    // If the kept portion starts with a Tool message, we need to also drop it
    // because it would be an orphaned tool result (its assistant message with
    // the tool_calls was dropped). Walk forward past any leading Tool messages.
    while keep_from < history.len() && history[keep_from].role == crate::types::Role::Tool {
        keep_from += 1;
    }

    // If we ended up keeping everything, nothing to compact.
    if keep_from == 0 {
        return None;
    }

    Some(keep_from)
}

/// Plan a head-protected compaction of `history`.
///
/// Builds on [`plan_compaction`] and additionally preserves the first
/// `protect_first_n` messages so the original user request and any early
/// framing stays intact across repeated compactions. The budget passed to
/// the underlying tail planner is reduced by the protected head's token cost
/// so the resulting compacted history stays within `max_tokens`.
///
/// Returns `None` when compaction is not needed (history fits the budget),
/// or when the protected head would fully consume the budget (callers
/// should fall back to non-head-protected compaction in that case).
pub fn plan_compaction_v2(
    history: &[Message],
    max_tokens: usize,
    protect_first_n: usize,
) -> Option<CompactionPlan> {
    let head_end = protect_first_n.min(history.len());
    let head_tokens: usize = history[..head_end].iter().map(message_tokens).sum();
    // If the protected head alone would consume (at least) half the budget,
    // the head is too big to compact around — signal failure so the caller
    // can degrade to non-head-protected compaction rather than stalling
    // indefinitely at an over-budget history.
    if head_tokens * 2 >= max_tokens {
        return None;
    }
    // Reduce the tail budget by the head's token cost so the post-compaction
    // history actually fits within `max_tokens` rather than exceeding it by
    // the head contribution.
    let tail_budget = max_tokens - head_tokens;
    let tail_start = plan_compaction(history, tail_budget)?;
    // Clamp head so we never produce an inverted range. If the tail would
    // start inside the head region, there is nothing to summarize.
    if head_end >= tail_start {
        return None;
    }
    Some(CompactionPlan {
        head_end,
        tail_start,
    })
}

/// Compute the `[from_seq, to_seq]` log range a head-protected compaction will
/// fold, derived from the working history *before* the compaction mutates it
/// (the planner is pure). The caller records a
/// [`crate::conversation::SessionEventKind::CompactionCheckpoint`] over this
/// range so a fresh worker reconstructs the same compacted view from the log.
///
/// `from_seq` is one past the last *persisted* message in the protected head,
/// so the fold covers the entire middle — including raw rows an earlier
/// iterative summary already subsumed (their seqs are `>= from_seq`). That is
/// what lets [`crate::conversation::project_history`] collapse repeated
/// compactions to a single, current summary. `to_seq` is the last persisted
/// message in the summarized middle.
///
/// Returns `None` when there is nothing to compact, or when the middle holds no
/// persisted rows (e.g. only unpersisted injected messages) — in which case no
/// durable checkpoint can be anchored and the in-memory fold stands alone.
pub fn compaction_checkpoint_range(
    history: &[Message],
    max_tokens: usize,
    protect_first_n: usize,
) -> Option<(i64, i64)> {
    let plan = plan_compaction_v2(history, max_tokens, protect_first_n)?;
    let head_max_seq = history[..plan.head_end].iter().filter_map(|m| m.seq).max();
    let mut middle_seqs = history[plan.head_end..plan.tail_start]
        .iter()
        .filter_map(|m| m.seq);
    let first_middle = middle_seqs.next()?;
    // `next_back` walks from the end (the slice iterator is double-ended), so we
    // get the last persisted middle seq without scanning the whole range.
    let to_seq = middle_seqs.next_back().unwrap_or(first_middle);
    // Fold from just past the protected head; with no persisted head, fold from
    // the first persisted message in the middle.
    let from_seq = head_max_seq.map_or(first_middle, |h| h + 1);
    Some((from_seq, to_seq))
}

/// Hard ceiling for a compaction summary, as a fraction of the replaced
/// middle's token count: the marker must cost at most 90% of what it removes,
/// so every committed compaction shrinks history by a real margin.
///
/// The word cap in the summarizer prompt is a request, not a bound — chunked
/// summarization merges several chunk summaries and the anchor index is
/// appended afterwards, so nothing upstream stops a marker from costing more
/// than the middle it replaces. Committing such a swap makes history LARGER
/// while reporting a successful compaction, and nothing downstream compares
/// before/after sizes — the session then wedges over budget with the
/// compactor insisting it is making progress.
///
/// Deliberately a loose bound (not, say, half the middle): a legitimate
/// summary of a small middle is dense — squeezing it harder deletes the
/// identifiers and anchors the marker exists to preserve. The guard is
/// against growth, not against modest summaries.
fn summary_token_budget(middle_tokens: usize) -> usize {
    (middle_tokens.saturating_sub(middle_tokens / 10)).max(1)
}

/// Enforce a hard token ceiling on a compaction summary by deterministic
/// truncation. Truncation, not refusal: a refusal leaves the session over
/// budget with no path forward, while a truncated summary still archives the
/// middle and shrinks history. The appended note tells the model content was
/// cut, so it reaches for `memory_search` instead of inventing what is gone.
fn truncate_to_token_budget(text: String, budget_tokens: usize) -> String {
    use crate::memory::tokenizer::estimate_tokens;
    if estimate_tokens(&text) <= budget_tokens {
        return text;
    }
    const NOTE: &str = "\n[truncated: the summary would have outgrown the messages it replaced]";
    let mut kept = text;
    loop {
        let tokens = estimate_tokens(&kept);
        if kept.is_empty() || tokens + estimate_tokens(NOTE) <= budget_tokens {
            break;
        }
        // Proportional cut with 10% slack; BPE is nonlinear, so loop until
        // the estimate converges under the budget. Each pass shrinks the text
        // (the target is capped at len-1), so termination is structural.
        let target = kept
            .len()
            .saturating_mul(budget_tokens)
            .saturating_mul(9)
            .checked_div(tokens.saturating_mul(10))
            .unwrap_or(0);
        let mut cut = target.min(kept.len().saturating_sub(1));
        while cut > 0 && !kept.is_char_boundary(cut) {
            cut -= 1;
        }
        if cut == 0 {
            kept.clear();
            break;
        }
        kept.truncate(cut);
    }
    warn!("compaction summary exceeded its token budget ({budget_tokens}), truncated");
    format!("{kept}{NOTE}")
}

/// Compact conversation history using the LLM to summarize dropped messages.
///
/// Strategy:
/// - Always preserve the most recent messages (they provide immediate context).
/// - When the history exceeds the budget, drop the oldest messages and use the
///   LLM to generate a rich summary of what was discussed.
/// - Tool result messages are only kept if their corresponding assistant
///   tool-call message is also kept (orphaned tool results confuse the API).
///
/// Returns `Err` only when the summarizer itself failed; the caller uses the
/// [`SummarizeFailure`] class to decide whether retrying next turn is useful.
/// Having nothing to compact is `Ok`.
#[instrument(skip_all, fields(max_tokens = max_tokens))]
pub async fn compact_history(
    history: &mut Vec<Message>,
    max_tokens: usize,
    llm: &mut LlmClient,
) -> Result<(), SummarizeFailure> {
    let Some(keep_from) = plan_compaction(history, max_tokens) else {
        return Ok(());
    };

    let dropped = keep_from;
    debug!("Dropping {dropped} old messages from conversation history");

    // Use LLM to summarize the dropped messages. Abort without mutating history
    // when the summarizer gave us nothing usable (e.g. a provider auth outage):
    // dropping the range for a degraded placeholder would irrecoverably lose
    // context, whereas leaving history intact lets the next turn retry once the
    // provider recovers. Mirrors the v2 path's empty-summary bail-out.
    //
    // Assistant reasoning traces are stripped from the summarizer's input and
    // its output so chain-of-thought can't compound into iterative summaries.
    let sanitized = sanitize_for_summarizer(&history[..dropped]);
    let summary = match summarize_with_llm(&sanitized, llm).await {
        Ok(s) => s,
        Err(failure) => {
            warn!("compaction summary unavailable, leaving history untouched");
            return Err(failure);
        }
    };
    let summary = strip_reasoning_traces(&summary);
    if summary.trim().is_empty() {
        warn!("compaction summary was reasoning-only, leaving history untouched");
        // The provider answered — this is a bad response, not an outage.
        return Err(SummarizeFailure::Transient);
    }

    // Would-grow guard: the marker must cost strictly less than the messages
    // it replaces, or the "compaction" grows history while reporting success.
    let middle_tokens: usize = history[..dropped].iter().map(message_tokens).sum();
    let summary = truncate_to_token_budget(summary, summary_token_budget(middle_tokens));
    let marker = Message::user(summary);
    // Backstop for a middle so small even a fully-truncated marker outweighs
    // it: refuse rather than commit a growth. Never mutates history.
    if message_tokens(&marker) >= middle_tokens {
        warn!("compaction marker would outweigh the {middle_tokens}-token middle, refusing");
        return Err(SummarizeFailure::Transient);
    }

    let mut compacted = Vec::with_capacity(history.len() - dropped + 1);
    compacted.push(marker);
    compacted.extend(history.drain(dropped..));
    *history = compacted;
    Ok(())
}

/// Compact history preserving a head region and iterating prior summaries.
///
/// An upgrade of [`compact_history`]:
/// - The first `protect_first_n` messages stay verbatim at the start so the
///   original user request and framing survive repeated compactions.
/// - When `previous_summary` is `Some`, the summarizer is told to UPDATE the
///   prior summary with the new turns rather than summarize from scratch.
/// - The new summary is written back into `previous_summary` so the next
///   compaction iterates on it again.
///
/// Returns the number of dropped messages (0 when no compaction was
/// necessary). Callers can use this to decide whether to emit a UX event.
/// Returns `Err` only when the summarizer failed — see [`compact_history`].
#[instrument(skip_all, fields(max_tokens = max_tokens, head = protect_first_n))]
pub async fn compact_history_v2(
    history: &mut Vec<Message>,
    max_tokens: usize,
    protect_first_n: usize,
    previous_summary: &mut Option<String>,
    llm: &mut LlmClient,
) -> Result<usize, SummarizeFailure> {
    let Some(plan) = plan_compaction_v2(history, max_tokens, protect_first_n) else {
        return Ok(0);
    };

    let dropped = plan.tail_start - plan.head_end;
    debug!(
        "Compacting: head={}, middle={dropped}, tail={}",
        plan.head_end,
        history.len() - plan.tail_start
    );

    // Strip assistant reasoning traces before the middle reaches the
    // summarizer, and again from the summary before it is stored, so
    // chain-of-thought can't compound across iterative summaries.
    let middle = sanitize_for_summarizer(&history[plan.head_end..plan.tail_start]);

    // Lift the anchor section off the prior summary before it goes back to the
    // summarizer. Its whole purpose is to hold identifiers the model must not
    // rewrite, so re-submitting it invites exactly the paraphrase it exists to
    // prevent — and spends the word budget re-transcribing what we can append
    // deterministically afterwards.
    let (prior_body, carried_anchors) = match previous_summary.as_deref() {
        Some(prior) => super::anchors::split_anchor_section(prior),
        None => (String::new(), Vec::new()),
    };
    let prior_body = (!prior_body.trim().is_empty()).then_some(prior_body);

    // Bail out without mutating history if the summarizer gave us nothing
    // usable — better to let the next turn try again than to drop the
    // middle with no context at all. Shares the v1 path's let-else abort.
    let summary_body = match summarize_with_llm_v2(&middle, prior_body.as_deref(), llm).await {
        Ok(s) => s,
        Err(failure) => {
            warn!("compact_history_v2: no usable summary, leaving history untouched");
            return Err(failure);
        }
    };
    let summary_body = strip_reasoning_traces(&summary_body);
    if summary_body.trim().is_empty() {
        warn!("compact_history_v2: summary was reasoning-only, leaving history untouched");
        // The provider answered — a bad response, not an outage.
        return Err(SummarizeFailure::Transient);
    }

    // Append the exact identifiers this region carried. Harvested by regex
    // from the same messages the summarizer saw, so the facts with no semantic
    // redundancy — SHAs, issue numbers, paths, error text — survive at full
    // fidelity whatever the summarizer chose to keep. Done after the call so
    // no model ever gets the chance to rewrite them.
    let anchor_index =
        super::anchors::build_anchor_index(&summarizer_region_text(&middle), &carried_anchors);

    // Would-grow guard: the marker must cost strictly less than the middle it
    // replaces. The prose is truncated first, inside whatever budget the
    // anchor index leaves — anchors are the highest-fidelity facts and sit at
    // the tail, so a naive whole-string cut would delete them first. The
    // second pass is the backstop for an anchor index that alone exceeds the
    // budget.
    let middle_tokens: usize = history[plan.head_end..plan.tail_start]
        .iter()
        .map(message_tokens)
        .sum();
    // The budget bounds the whole rendered marker, so the frame the body will
    // be wrapped in (heading, fences, role overhead) is paid up front.
    let frame_tokens = message_tokens(&render_compaction_marker(""));
    let budget = summary_token_budget(middle_tokens).saturating_sub(frame_tokens);
    let prose_budget =
        budget.saturating_sub(crate::memory::tokenizer::estimate_tokens(&anchor_index));
    let summary_body = truncate_to_token_budget(summary_body, prose_budget);
    let summary_body = truncate_to_token_budget(format!("{summary_body}{anchor_index}"), budget);

    // Sanitize the summary body to strip any XML tag boundaries the LLM may
    // have injected (e.g. a stray `</compaction_summary>` inside a code
    // block). Without this, a malicious input could close our tagged fence
    // early and smuggle untrusted content onto the internal side.
    let safe_body = crate::security::sanitize::sanitize_xml_boundaries(&summary_body);
    let marker = render_compaction_marker(&safe_body);
    // Backstop for a middle so small even a fully-truncated marker (frame +
    // truncation note) outweighs it: the budget math above cannot shrink the
    // frame, so refuse rather than commit a growth. Never mutates history.
    if message_tokens(&marker) >= middle_tokens {
        warn!(
            "compact_history_v2: marker would outweigh the {middle_tokens}-token middle, refusing"
        );
        return Err(SummarizeFailure::Transient);
    }

    let mut compacted = Vec::with_capacity(plan.head_end + 1 + history.len() - plan.tail_start);
    compacted.extend(history.drain(..plan.head_end));
    // Skip the middle (which is still sitting at the front of the original vec)
    history.drain(..dropped);
    compacted.push(marker);
    compacted.append(history);
    *history = compacted;

    *previous_summary = Some(summary_body);
    Ok(dropped)
}

/// True when `msg` is an agent-generated compaction marker row.
///
/// Markers are `Role::User` rows by design (the model treats the summary as
/// conversational input), which makes every "find the latest user message"
/// scan a potential aliasing hazard: when the protected tail holds only
/// assistant/tool rows, the marker IS the most recent `Role::User` row, and
/// naive scans read the summary as the user's request (memory ranking,
/// conditional tool loading) or truncate at it (`/undo`), silently deleting
/// the compacted context. Spoof-resistance matches
/// [`extract_last_compaction_summary`]: the exact agent-generated prefix
/// `{HEADING}\n\n{OPEN_FENCE}\n` is required, and
/// [`crate::security::sanitize::sanitize_xml_boundaries`] strips the fence
/// from untrusted content.
pub fn is_compaction_marker(msg: &Message) -> bool {
    msg.role == Role::User
        && msg
            .text_content()
            .and_then(|t| t.strip_prefix(COMPACTION_MARKER_HEADING))
            .and_then(|t| t.strip_prefix("\n\n"))
            .and_then(|t| t.strip_prefix(COMPACTION_MARKER_OPEN))
            .is_some_and(|t| t.starts_with('\n'))
}

/// Extract a previous-summary body from a compacted history, if any.
///
/// Walks `history` in reverse looking for the most recent agent-generated
/// compaction marker. To defeat user spoofing, we require the message to
/// START with the exact agent-generated shape:
/// `{HEADING}\n\n{OPEN_FENCE}\n` — a user typing the fence into a normal
/// message (even at the very top) will also have to match the heading line
/// preceding it, which [`crate::security::sanitize::sanitize_xml_boundaries`]
/// additionally strips from untrusted content. Returns the body between the
/// fences, trimmed.
pub fn extract_last_compaction_summary(history: &[Message]) -> Option<String> {
    let expected_prefix = format!("{COMPACTION_MARKER_HEADING}\n\n{COMPACTION_MARKER_OPEN}\n");
    for msg in history.iter().rev() {
        if msg.role != Role::User {
            continue;
        }
        let Some(text) = msg.text_content() else {
            continue;
        };
        let Some(after_prefix) = text.strip_prefix(expected_prefix.as_str()) else {
            continue;
        };
        if let Some(close_idx) = after_prefix.find(COMPACTION_MARKER_CLOSE) {
            return Some(after_prefix[..close_idx].trim().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FunctionCall, Message, MessageContent, Role, ToolCall};

    /// Within budget → returned byte-for-byte, no truncation note.
    #[test]
    fn summary_within_budget_is_untouched() {
        let text = "short summary".to_string();
        assert_eq!(truncate_to_token_budget(text.clone(), 1_000), text);
    }

    /// Over budget → cut under the budget, with the truncation note appended
    /// so the model knows content is gone.
    #[test]
    fn oversized_summary_is_truncated_under_its_budget() {
        let text = "word ".repeat(4_000);
        let budget = 100;
        let out = truncate_to_token_budget(text, budget);
        assert!(
            crate::memory::tokenizer::estimate_tokens(&out) <= budget,
            "truncated summary must fit its budget"
        );
        assert!(
            out.contains("[truncated"),
            "the cut must be announced, not silent: {out}"
        );
    }

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

    /// A message carrying an explicit durable log seq.
    fn seq_msg(seq: i64, role: Role, text: &str) -> Message {
        Message {
            role,
            content: Some(MessageContent::Text(text.to_string())),
            tool_calls: None,
            tool_call_id: None,
            timestamp: None,
            seq: Some(seq),
        }
    }

    // -- compaction_checkpoint_range --

    #[test]
    fn checkpoint_range_none_when_under_budget() {
        let history = vec![
            seq_msg(1, Role::User, "hi"),
            seq_msg(2, Role::Assistant, "hello"),
        ];
        assert!(compaction_checkpoint_range(&history, 100_000, 3).is_none());
    }

    #[test]
    fn checkpoint_range_matches_head_protected_plan() {
        // The durable range must bracket exactly the messages the head-protected
        // fold summarizes: from one past the last protected-head seq, to the
        // last middle seq. Ties the range to the planner so it can't drift.
        let mut history: Vec<Message> = Vec::new();
        for i in 0..30 {
            history.push(seq_msg(
                2 * i + 1,
                Role::User,
                &format!("message {i} with lots of padding"),
            ));
            history.push(seq_msg(
                2 * i + 2,
                Role::Assistant,
                &format!("response {i} with padding"),
            ));
        }
        let plan = plan_compaction_v2(&history, 200, 3).expect("should compact");
        let (from_seq, to_seq) =
            compaction_checkpoint_range(&history, 200, 3).expect("should yield a range");
        assert_eq!(from_seq, history[plan.head_end - 1].seq.unwrap() + 1);
        assert_eq!(to_seq, history[plan.tail_start - 1].seq.unwrap());
    }

    #[test]
    fn checkpoint_range_folds_previously_summarized_rows() {
        // Iterative case: the working set already carries a summary marker (seq
        // None) between the protected head and the live tail. The next
        // checkpoint must start one past the last *real* head message — not the
        // marker — so the fold re-covers the raw rows the old summary stood in
        // for (their seqs are >= from_seq). That is what lets project_history
        // collapse repeated compactions to a single current summary.
        let mut history = vec![
            seq_msg(1, Role::User, "original request with padding padding"),
            seq_msg(2, Role::Assistant, "early framing with padding padding"),
            // Unpersisted summary marker from an earlier compaction.
            Message::user("earlier conversation was summarized".to_string()),
        ];
        for i in 0..30 {
            history.push(seq_msg(
                10 + 2 * i,
                Role::User,
                &format!("later message {i} with padding padding"),
            ));
            history.push(seq_msg(
                11 + 2 * i,
                Role::Assistant,
                &format!("later response {i} with padding"),
            ));
        }
        let (from_seq, _to_seq) =
            compaction_checkpoint_range(&history, 200, 3).expect("should yield a range");
        assert_eq!(
            from_seq, 3,
            "fold starts past the last real head message (seq 2), not the seqless marker"
        );
    }

    #[test]
    fn checkpoint_range_none_when_middle_unpersisted() {
        // If the entire compactible middle is unpersisted (injected) messages,
        // there is no seq to anchor a durable checkpoint — the in-memory fold
        // must stand alone.
        let mut history = vec![
            seq_msg(1, Role::User, "head a with padding"),
            seq_msg(2, Role::Assistant, "head b with padding"),
            seq_msg(3, Role::User, "head c with padding"),
        ];
        for i in 0..30 {
            history.push(Message::user(format!(
                "injected {i} with padding padding padding"
            )));
            history.push(Message::assistant(format!(
                "injected reply {i} with padding padding"
            )));
        }
        assert!(
            plan_compaction_v2(&history, 200, 3).is_some(),
            "test setup: compaction must actually be needed"
        );
        assert!(
            compaction_checkpoint_range(&history, 200, 3).is_none(),
            "no persisted middle → no durable checkpoint range"
        );
    }

    // -- plan_compaction_v2 + compact_history_v2 --

    #[test]
    fn plan_v2_noop_when_under_budget() {
        let history = vec![make_user("hi"), make_assistant("hello")];
        assert!(plan_compaction_v2(&history, 100_000, 3).is_none());
    }

    #[test]
    fn plan_v2_returns_head_bound() {
        let mut history: Vec<Message> = Vec::new();
        for i in 0..30 {
            history.push(make_user(&format!("message {i} with lots of padding")));
            history.push(make_assistant(&format!("response {i} with padding")));
        }

        let plan = plan_compaction_v2(&history, 200, 3).expect("should compact");
        assert_eq!(plan.head_end, 3, "head protects first 3 messages");
        assert!(plan.tail_start > plan.head_end);
        assert!(plan.tail_start < history.len());
    }

    #[test]
    fn plan_v2_clamps_head_to_not_exceed_tail() {
        let mut history: Vec<Message> = Vec::new();
        for i in 0..30 {
            history.push(make_user(&format!("message {i} with lots of padding")));
            history.push(make_assistant(&format!("response {i} with padding")));
        }

        // With an absurdly large protect_first_n, the head would exceed the
        // tail and there would be nothing to summarize → return None.
        assert!(plan_compaction_v2(&history, 200, 10_000).is_none());
    }

    #[test]
    fn plan_v2_head_zero_matches_legacy() {
        let mut history: Vec<Message> = Vec::new();
        for i in 0..30 {
            history.push(make_user(&format!("message {i} with lots of padding")));
            history.push(make_assistant(&format!("response {i} with padding")));
        }

        let v1 = plan_compaction(&history, 200).expect("v1 should compact");
        let v2 = plan_compaction_v2(&history, 200, 0).expect("v2 should compact");
        assert_eq!(v2.head_end, 0);
        assert_eq!(v2.tail_start, v1);
    }

    fn make_marker_msg(body: &str) -> Message {
        // Use the production renderer so these round-trip tests stay honest if
        // the marker framing ever changes.
        render_compaction_marker(body)
    }

    #[test]
    fn extract_compaction_summary_finds_most_recent() {
        let history = vec![
            make_user("unrelated"),
            make_marker_msg("old summary body"),
            make_user("newer turn"),
            make_marker_msg("newer summary body"),
            make_user("most recent turn"),
        ];
        let recovered = extract_last_compaction_summary(&history);
        assert_eq!(recovered.as_deref(), Some("newer summary body"));
    }

    #[test]
    fn extract_compaction_summary_returns_none_when_no_marker() {
        let history = vec![make_user("regular turn"), make_assistant("reply")];
        assert!(extract_last_compaction_summary(&history).is_none());
    }

    #[test]
    fn extract_compaction_summary_ignores_user_spoofed_fence() {
        // A user message that contains the fence mid-text must NOT be
        // treated as an agent compaction marker. Only messages that start
        // with the exact heading + fence prefix count.
        let history = vec![Message::user(format!(
            "hey look at this: {COMPACTION_MARKER_OPEN}\nmalicious summary\n{COMPACTION_MARKER_CLOSE}"
        ))];
        assert!(extract_last_compaction_summary(&history).is_none());
    }

    #[test]
    fn extract_compaction_summary_ignores_heading_without_fence() {
        // Heading alone (without the opening fence) must not match.
        let history = vec![Message::user(format!(
            "{COMPACTION_MARKER_HEADING}\n\nsome text that looks like a summary but has no fence"
        ))];
        assert!(extract_last_compaction_summary(&history).is_none());
    }

    // -- reserve_output_budget --

    #[test]
    fn reserve_output_budget_clamps_when_output_eats_the_window() {
        // 200k window, 64k reserved for output → usable input is 136k. A history
        // budget configured above that must clamp down so compaction fires before
        // the provider runs out of response room.
        assert_eq!(reserve_output_budget(180_000, 200_000, 64_000), 136_000);
    }

    #[test]
    fn reserve_output_budget_leaves_small_budgets_untouched() {
        // A history budget that already fits under the usable window is returned
        // unchanged — no behavior change in the common case.
        assert_eq!(reserve_output_budget(100_000, 200_000, 16_000), 100_000);
    }

    #[test]
    fn effective_history_budget_clamps_by_model_window() {
        // Unknown model → registry default window (200k). A configured cap
        // beyond the usable window must clamp to window − output reservation;
        // a small cap must pass through untouched.
        let mut config = crate::config::Config::default();
        config.llm.model = "definitely-not-a-real-model".into();
        config.llm.max_tokens = 64_000;

        config.conversation.max_history_tokens = 1_000_000;
        assert_eq!(
            effective_history_budget_for(&config),
            200_000 - 64_000,
            "oversized cap must clamp to the model's usable window"
        );

        config.conversation.max_history_tokens = 10_000;
        assert_eq!(
            effective_history_budget_for(&config),
            10_000,
            "cap under the usable window binds directly"
        );
    }

    #[test]
    fn reserve_output_budget_floors_at_quarter_window_on_pathological_config() {
        // Output reservation >= window would collapse usable input to zero; the
        // floor keeps a quarter-window so compaction can still make progress.
        assert_eq!(reserve_output_budget(180_000, 200_000, 250_000), 50_000);
    }

    // -- effective_head_protection (decay) --

    #[test]
    fn head_protection_decays_after_first_compaction() {
        // First compaction (no prior summary) honors the configured protection;
        // every subsequent one (summary exists) decays it to zero so the early
        // turns stop being re-pinned verbatim.
        assert_eq!(effective_head_protection(4, false), 4);
        assert_eq!(effective_head_protection(4, true), 0);
    }

    #[test]
    fn head_protection_decay_unpins_early_turns_on_second_pass() {
        // Behavioral: an over-budget history is planned twice. On the first pass
        // the head [0..protect) is kept verbatim; on a later pass (summary
        // present) head protection decays, so the early turns become eligible for
        // summarization instead of fossilizing into every future window.
        let mut history: Vec<Message> = Vec::new();
        for i in 0..40 {
            history.push(make_user(&format!(
                "user turn number {i} with some filler text"
            )));
            history.push(make_assistant(&"assistant reply ".repeat(8)));
        }
        let protect = 4;
        let budget = 400;

        let first = effective_head_protection(protect, false);
        let plan_first = plan_compaction_v2(&history, budget, first).expect("first compaction");
        assert_eq!(
            plan_first.head_end, protect,
            "first pass pins the early turns"
        );

        let later = effective_head_protection(protect, true);
        let plan_later = plan_compaction_v2(&history, budget, later).expect("later compaction");
        assert_eq!(
            plan_later.head_end, 0,
            "later pass no longer pins early turns"
        );
        assert!(
            plan_later.head_end < plan_first.head_end,
            "head protection must decay so early turns stop fossilizing"
        );
    }

    // -- plan_compaction --

    #[test]
    fn plan_noop_when_under_budget() {
        let history = vec![make_user("hi"), make_assistant("hello")];
        assert!(plan_compaction(&history, 100_000).is_none());
    }

    #[test]
    fn plan_identifies_messages_to_drop() {
        let mut history: Vec<Message> = Vec::new();
        for i in 0..20 {
            history.push(make_user(&format!(
                "message number {i} with some padding text"
            )));
            history.push(make_assistant(&format!(
                "response number {i} with some padding text"
            )));
        }

        let keep_from = plan_compaction(&history, 200);
        assert!(keep_from.is_some());
        let keep_from = keep_from.unwrap();
        // Should drop most messages but keep at least the last few
        assert!(keep_from > 0);
        assert!(keep_from < history.len());
    }

    #[test]
    fn plan_preserves_most_recent_messages() {
        // Use large messages so the full history overwhelms the threshold even
        // after the 200-token marker reserve, forcing a real compaction with a
        // budget that still fits only the most recent pair. (Tiny messages can't
        // exceed the reserve, which is why the old version's assertion was
        // wrapped in a never-failing `if let Some`.)
        let big = "padding ".repeat(60); // ~120 tokens each
        let history = vec![
            make_user(&format!("old message 1 {big}")),
            make_assistant(&format!("old response 1 {big}")),
            make_user(&format!("old message 2 {big}")),
            make_assistant(&format!("old response 2 {big}")),
            make_user(&format!("recent question {big}")),
            make_assistant(&format!("recent answer {big}")),
        ];

        // Budget = marker reserve + the last pair (+slack). effective_budget
        // then fits the last pair; the six big messages overflow the threshold.
        let last_pair_tokens = message_tokens(&history[4]) + message_tokens(&history[5]);
        let max_tokens = COMPACTION_MARKER_TOKENS + last_pair_tokens + 10;
        assert!(
            history_tokens(&history) as f64
                > max_tokens as f64 * constants::COMPACTION_SAFETY_MARGIN,
            "test setup: full history ({}) must exceed threshold ({})",
            history_tokens(&history),
            (max_tokens as f64 * constants::COMPACTION_SAFETY_MARGIN) as usize,
        );
        let keep_from = plan_compaction(&history, max_tokens)
            .expect("over-threshold history must trigger compaction");
        // The most recent pair (indices 4, 5) must survive, and older messages
        // must actually be dropped.
        assert!(
            (1..=4).contains(&keep_from),
            "keep_from={keep_from} must drop old messages yet preserve the most \
             recent pair (indices 4, 5)"
        );
    }

    #[test]
    fn plan_skips_leading_tool_results() {
        // The leading user message (idx 0) is huge so it gets dropped. The
        // tool_result (idx 2) is sized so the budget covers idx 2..=4 but the
        // tiny tool_call at idx 1 tips it over — so the naive walk-back stops at
        // keep_from=2 (the tool_result). That is an orphan, because its tool_call
        // assistant (idx 1) is dropped, so the planner must skip forward to idx 3.
        let big = "padding ".repeat(200); // huge leading message → dropped
        let result_big = "out ".repeat(40); // sizeable tool_result body
        let history = vec![
            make_user(&format!("do something {big}")),
            make_tool_call_msg("", "call_1", "run_shell"),
            make_tool_result("call_1", &format!("command output {result_big}")),
            make_user("recent question"),
            make_assistant("recent answer"),
        ];

        // effective_budget (= max_tokens - marker reserve) is sized to fit
        // exactly idx 2..=4. Walk-back keeps 4,3,2 then breaks on the tiny idx 1,
        // leaving keep_from=2 (orphaned tool_result) → skip-forward to idx 3.
        let fit_2_to_4: usize = history[2..].iter().map(message_tokens).sum();
        let max_tokens = COMPACTION_MARKER_TOKENS + fit_2_to_4;
        assert!(
            history_tokens(&history) as f64
                > max_tokens as f64 * constants::COMPACTION_SAFETY_MARGIN,
            "test setup: full history ({}) must exceed threshold ({})",
            history_tokens(&history),
            (max_tokens as f64 * constants::COMPACTION_SAFETY_MARGIN) as usize,
        );
        let keep_from = plan_compaction(&history, max_tokens)
            .expect("over-threshold history must trigger compaction");
        // The kept portion must not start with a Tool message, which would be
        // an orphaned tool_result (its assistant tool_call message was dropped).
        // The skip-forward must have advanced past the tool_result at idx 2.
        assert!(
            keep_from >= 3,
            "planner must skip the orphaned tool_result at idx 2; keep_from={keep_from}"
        );
        assert_ne!(
            history[keep_from].role,
            Role::Tool,
            "kept window must not start on an orphaned tool_result at index {keep_from}"
        );
    }

    #[test]
    fn plan_with_zero_budget() {
        let history = vec![
            make_user("first"),
            make_assistant("second"),
            make_user("third"),
        ];
        let result = plan_compaction(&history, 0);
        // Should want to compact
        assert!(result.is_some());
    }

    #[test]
    fn plan_single_message() {
        let history = vec![make_user("only message")];
        let result = plan_compaction(&history, 0);
        // With a single message, the walk-back clamps keep_from to 0, and the
        // `keep_from == 0` guard returns None — there is nothing to drop before
        // the last message. Assert the exact documented outcome (None), not the
        // permissive "None or Some(0)" which would accept a buggy Some(0) that
        // signals an empty kept window.
        assert_eq!(
            result, None,
            "single-message history has nothing to compact; must return None"
        );
    }

    #[test]
    fn plan_empty_history() {
        let history: Vec<Message> = Vec::new();
        assert!(plan_compaction(&history, 100).is_none());
    }

    // -- safety margin --

    #[test]
    fn safety_margin_triggers_earlier() {
        // With COMPACTION_SAFETY_MARGIN = 0.85, compaction triggers when
        // total > budget * 0.85. Use larger messages to avoid rounding issues.
        let mut history: Vec<Message> = Vec::new();
        for i in 0..6 {
            history.push(make_user(&format!(
                "Message {i}: The quick brown fox jumps over the lazy dog repeatedly."
            )));
            history.push(make_assistant(&format!(
                "Response {i}: A longer response with plenty of text to accumulate tokens."
            )));
        }

        let total = history_tokens(&history);
        assert!(total > 100, "Need enough tokens, got {total}");

        // Budget where total is ~80% → below 85% threshold → no compaction
        let generous_budget = (total as f64 / 0.80) as usize;
        assert!(
            plan_compaction(&history, generous_budget).is_none(),
            "should NOT compact at ~80% usage (below 85% margin)"
        );

        // Budget where total is ~90% → above 85% threshold → should compact
        let tight_budget = (total as f64 / 0.90) as usize;
        assert!(
            plan_compaction(&history, tight_budget).is_some(),
            "should compact at ~90% usage (above 85% margin)"
        );
    }

    // -- summarizer reasoning-trace hygiene --

    #[test]
    fn strip_reasoning_traces_removes_all_tag_families_case_insensitively() {
        let input = "keep A<think>hidden 1</think> keep B<THINKING>hidden 2</THINKING>\
                     <Reasoning>hidden 3</Reasoning> keep C<internal>hidden 4</internal> keep D";
        let out = strip_reasoning_traces(input);
        for kept in ["keep A", "keep B", "keep C", "keep D"] {
            assert!(
                out.contains(kept),
                "visible text {kept:?} must survive: {out:?}"
            );
        }
        for hidden in ["hidden 1", "hidden 2", "hidden 3", "hidden 4"] {
            assert!(
                !out.contains(hidden),
                "reasoning span {hidden:?} must be stripped: {out:?}"
            );
        }
    }

    #[test]
    fn strip_reasoning_traces_drops_unclosed_trace_remainder() {
        // A stream cut off mid-trace leaves an unclosed tag; the half-emitted
        // trace must not leak into the summarizer.
        let out = strip_reasoning_traces("visible <think>never closed and secret");
        assert_eq!(out.trim(), "visible");
    }

    #[test]
    fn summarizer_input_strips_assistant_traces_but_not_user_text() {
        let history = vec![
            Message::user("what does <think> mean in HTML? padding padding"),
            Message::assistant(
                "<think>I should check the docs first, secret plan here</think>It is not a real tag.",
            ),
            Message {
                role: Role::Assistant,
                content: Some(MessageContent::Parts(vec![
                    ContentPart::Text("<REASONING>multi part trace</REASONING>Answer: 42".into()),
                ])),
                tool_calls: None,
                tool_call_id: None,
                timestamp: None,
                seq: None,
            },
        ];
        let sanitized = sanitize_for_summarizer(&history);
        assert_eq!(sanitized.len(), history.len(), "no messages may be dropped");
        // User text is content, not a trace — byte-identical.
        assert_eq!(
            sanitized[0].text_content(),
            history[0].text_content(),
            "user messages must pass through untouched"
        );
        let a1 = sanitized[1].text_content().unwrap();
        assert!(
            !a1.contains("secret plan"),
            "trace must be stripped: {a1:?}"
        );
        assert!(a1.contains("It is not a real tag."), "answer must survive");
        let a2 = sanitized[2].text_content().unwrap();
        assert!(
            !a2.contains("multi part trace"),
            "trace in Parts must be stripped"
        );
        assert!(a2.contains("Answer: 42"), "Parts answer must survive");
    }

    /// Serve one canned SSE summary and return the base URL to point a client at.
    ///
    /// The summarizer's own output is the variable under test here: it is made
    /// deliberately lossy so the assertions can tell what the fold preserved
    /// from a model that dropped everything specific.
    fn serve_one_summary(summary: &'static str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        let port = listener.local_addr().expect("local addr").port();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 65536];
                let _ = sock.read(&mut buf);
                let body = serde_json::json!({"choices":[{"delta":{"content": summary}}]});
                let sse = format!("data: {body}\n\ndata: [DONE]\n\n");
                let _ = sock.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                         Connection: close\r\n\r\n{sse}"
                    )
                    .as_bytes(),
                );
                let _ = sock.flush();
            }
        });
        format!("http://127.0.0.1:{port}/v1/chat/completions")
    }

    async fn client_returning(summary: &'static str) -> LlmClient {
        let mut config = crate::config::Config::default();
        config.llm.provider = Some("custom".to_string());
        config.llm.model = "test-model".to_string();
        config.llm.base_url = Some(serve_one_summary(summary));
        config.llm.max_retries = 0;
        LlmClient::new(&config).await.expect("client builds")
    }

    /// The failure this guards: a summarizer that writes a fluent, plausible
    /// summary and quietly drops every identifier in it. Prose can be
    /// re-derived from context; a commit SHA, an issue number, and the path
    /// that was edited cannot. They must reach the marker whatever the model
    /// chose to write, because nothing downstream can tell that they are gone.
    #[tokio::test]
    async fn identifiers_reach_the_marker_even_when_the_summarizer_drops_them() {
        let mut history: Vec<Message> = Vec::new();
        history.push(make_user("Please fix the crash."));
        for i in 0..8 {
            history.push(make_user(&format!(
                "Turn {i}: investigating #87403 in crates/cortex/src/llm/gemini.rs, \
                 reproduced at commit 9b987f73ab with plenty of filler text to spend budget."
            )));
            history.push(make_assistant(&format!(
                "Reply {i}: still seeing NotFoundError, adding more words so this history \
                 comfortably exceeds the compaction budget for the planner."
            )));
        }
        let total = history_tokens(&history);
        let tight_budget = (total as f64 / 0.90) as usize;

        // A summary that is entirely non-specific — exactly the failure mode.
        let mut llm = client_returning(
            "## Goal\nFix a crash.\n\n## Progress\n**Done** Investigated the issue \
             and found the bug in a source file.",
        )
        .await;
        let mut previous: Option<String> = None;
        let dropped = compact_history_v2(&mut history, tight_budget, 1, &mut previous, &mut llm)
            .await
            .expect("summarizer returned a summary");
        assert!(dropped > 0, "precondition: the fold must have run");

        let marker = history
            .iter()
            .find(|m| is_compaction_marker(m))
            .expect("a marker must be spliced in");
        let text = marker.text_content().expect("marker text");
        for identifier in [
            "#87403",
            "crates/cortex/src/llm/gemini.rs",
            "9b987f73ab",
            "NotFoundError",
        ] {
            assert!(
                text.contains(identifier),
                "the fold lost {identifier}, which the summary never carried: {text}"
            );
        }
        // The pointer back to the archived rows ships with them, so a model
        // missing a detail looks it up instead of inventing one.
        assert!(text.contains("memory_search"), "recovery pointer: {text}");
        // What is handed to the next compaction must be what the model sees,
        // or the marker and the replayed checkpoint would disagree.
        assert_eq!(
            previous.as_deref().map(str::trim),
            Some(
                text.trim_start_matches(COMPACTION_MARKER_HEADING)
                    .trim_start()
                    .trim_start_matches(COMPACTION_MARKER_OPEN)
                    .trim_end_matches(COMPACTION_MARKER_CLOSE)
                    .trim()
            ),
            "the stored summary must match the spliced marker body"
        );
    }

    /// The would-grow bug: nothing upstream bounds the summarizer's output,
    /// so a marker can cost more than the middle it replaces — history GROWS
    /// while the compactor reports success, and no caller compares sizes.
    /// A compaction must always shrink history; an oversized summary is
    /// truncated (announced in the marker), never committed whole.
    #[tokio::test]
    async fn compaction_never_grows_history_even_when_the_summary_would() {
        let mut history: Vec<Message> = Vec::new();
        for i in 0..8 {
            history.push(make_user(&format!(
                "Turn {i}: a modest message with enough words to give the planner \
                 a middle region worth folding away this pass."
            )));
            history.push(make_assistant(&format!(
                "Reply {i}: acknowledged, adding filler so the history exceeds \
                 the compaction budget."
            )));
        }
        let before = history_tokens(&history);
        let tight_budget = (before as f64 / 0.90) as usize;

        // A summary far larger than the whole history it summarizes.
        let huge: &'static str = Box::leak(
            format!("## Goal\n{}", "verbose padding sentence. ".repeat(2_000)).into_boxed_str(),
        );
        let mut llm = client_returning(huge).await;
        let mut previous: Option<String> = None;
        let dropped = compact_history_v2(&mut history, tight_budget, 1, &mut previous, &mut llm)
            .await
            .expect("compaction runs");
        assert!(dropped > 0, "precondition: the fold must have run");

        let after = history_tokens(&history);
        assert!(
            after < before,
            "compaction must shrink history: before={before}, after={after}"
        );
        let marker = history
            .iter()
            .find(|m| is_compaction_marker(m))
            .expect("a marker must be spliced in");
        assert!(
            marker
                .text_content()
                .expect("marker text")
                .contains("[truncated"),
            "the truncation must be announced in the marker"
        );
    }

    /// A second compaction must not re-submit the anchor section to the model:
    /// that is the one part of the summary that exists because it must never be
    /// paraphrased. It is lifted off before the call and re-appended after, so
    /// identifiers found in the first pass survive a pass that never mentions
    /// them again.
    #[tokio::test]
    async fn anchors_survive_a_second_compaction_that_never_mentions_them() {
        let mut history: Vec<Message> = Vec::new();
        for i in 0..8 {
            history.push(make_user(&format!(
                "Later turn {i}: unrelated work on the settings table, wordy enough to \
                 push this history over the compaction budget on its own."
            )));
            history.push(make_assistant(&format!(
                "Later reply {i}: acknowledged, continuing with more filler text here."
            )));
        }
        let total = history_tokens(&history);
        let tight_budget = (total as f64 / 0.90) as usize;

        // Stands in for the first pass's output: a body plus a banked index.
        let mut previous = Some(format!(
            "## Goal\nFix the crash.\n\n{}\nissues/PRs: #87403\ncommits: 9b987f73ab\n",
            super::super::anchors::ANCHOR_HEADING
        ));
        let mut llm = client_returning("## Goal\nFix the crash.\n\n## Progress\nMoved on.").await;
        compact_history_v2(&mut history, tight_budget, 0, &mut previous, &mut llm)
            .await
            .expect("second compaction summarizes");

        let stored = previous.expect("a summary is stored");
        assert!(
            stored.contains("#87403") && stored.contains("9b987f73ab"),
            "anchors from the first pass must survive a pass that never repeats them: {stored}"
        );
        assert_eq!(
            stored
                .matches(super::super::anchors::ANCHOR_HEADING)
                .count(),
            1,
            "the section must be replaced, not stacked: {stored}"
        );
    }

    /// A summarizer that fails — here a provider whose endpoint refuses the
    /// connection, standing in for a persistent auth outage — must NOT drop the
    /// summarized range for a degraded placeholder. `compact_history` leaves the
    /// working history byte-for-byte intact so the next turn can retry once the
    /// provider recovers. Regression guard for the v1 compaction abort path
    /// (the v2 path already bails on an empty summary).
    #[tokio::test]
    async fn compact_history_aborts_instead_of_degrading_when_summarizer_fails() {
        // Over-budget history so `plan_compaction` fires and summarization is
        // actually attempted (otherwise the test would pass vacuously).
        let mut history: Vec<Message> = Vec::new();
        for i in 0..8 {
            history.push(make_user(&format!(
                "Message {i}: the quick brown fox jumps over the lazy dog, repeated enough to cost tokens."
            )));
            history.push(make_assistant(&format!(
                "Response {i}: a sufficiently long reply with plenty of text to accumulate token budget."
            )));
        }
        let before: Vec<Option<String>> = history
            .iter()
            .map(|m| m.text_content().map(str::to_string))
            .collect();
        let total = history_tokens(&history);
        let tight_budget = (total as f64 / 0.90) as usize;
        assert!(
            plan_compaction(&history, tight_budget).is_some(),
            "precondition: history must be over budget so summarization is attempted"
        );

        // Ollama client pointed at a closed loopback port: `chat()` fails fast
        // (connection refused, no retries) → the summarizer returns `None`.
        let mut config = crate::config::Config::default();
        config.llm.provider = Some("ollama".to_string());
        config.llm.model = "llama3.3".to_string();
        config.llm.base_url = Some("http://127.0.0.1:1/v1/chat/completions".to_string());
        config.llm.max_retries = 0;
        config.llm.initial_retry_delay_ms = 0;
        let mut llm = LlmClient::new(&config)
            .await
            .expect("offline ollama client builds");

        let outcome = compact_history(&mut history, tight_budget, &mut llm).await;
        assert_eq!(
            outcome,
            Err(SummarizeFailure::Transient),
            "an unreachable summarizer is a blip, not a credential problem — \
             classifying it as persistent would stop compacting for the session"
        );

        let after: Vec<Option<String>> = history
            .iter()
            .map(|m| m.text_content().map(str::to_string))
            .collect();
        assert_eq!(
            after, before,
            "aborted compaction must leave history untouched (no drop, no placeholder)"
        );
        assert!(
            !history.iter().any(|m| m
                .text_content()
                .is_some_and(|t| t.contains(COMPACTION_MARKER_HEADING))),
            "no degraded summary placeholder may be inserted on summarizer failure"
        );
    }
}
