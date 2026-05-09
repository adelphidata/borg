//! Stream-event handling and text-only response finalization.
//!
//! Pulled out of `agent/mod.rs` as part of the structural split. The methods
//! here process the per-iteration LLM stream into history mutations and TUI
//! events:
//!
//! - [`Agent::handle_stream_event`] consumes one [`StreamEvent`] and updates
//!   the inner-loop collectors.
//! - [`Agent::handle_text_only_response`] / [`Agent::finalize_text_response`]
//!   resolve a no-tool-calls turn (terminate, nudge, or persist with an
//!   interruption marker).
//! - [`Agent::maybe_record_plan_emission`] awards plan-emission XP when a
//!   Plan-mode response carries a `<proposed_plan>` block.
//! - [`Agent::persist_discarded_tool_calls`] writes the
//!   "[incomplete tool calls discarded]" marker.
//!
//! The `Outcome` enums are exposed at `pub(super)` so `run_agent_loop` (which
//! stays in `mod.rs`) can pattern-match on them.

use tokio::sync::mpsc;
use tracing::{trace, warn};

use crate::hooks::{HookData, HookPoint};
use crate::internal_tag_filter::InternalTagFilter;
use crate::llm::StreamEvent;
use crate::types::Message;

use super::tool_validation::PartialToolCall;
use super::{should_nudge_for_response, Agent, AgentEvent, MAX_TOOL_CALLS};

/// Control-flow outcome when `run_agent_loop` encounters a text-only LLM
/// response. The loop either terminates the turn or loops again with a nudge
/// instructing the model to produce a visible response for the user.
pub(super) enum TextOnlyOutcome {
    /// Exit `run_agent_loop` with `Ok(())`. All persistence and TurnComplete
    /// events have already been emitted by the helper.
    TerminateTurn,
    /// Loop again: the helper persisted a system nudge asking for a response.
    /// Caller must set `nudged_for_response = true` before continuing.
    NudgeAndContinue,
}

/// Result of processing a single `StreamEvent` from the LLM. `Terminal` means
/// the stream ended (Done or Error); the inner loop should break.
pub(super) enum StreamEventOutcome {
    Continue,
    Terminal,
}

impl Agent {
    /// Process one streamed `StreamEvent` from the LLM. Mutates the inner-loop
    /// collectors and returns whether the inner loop should continue or break.
    /// `Done` and `Error` are the only Terminal variants.
    pub(super) async fn handle_stream_event(
        &mut self,
        event: StreamEvent,
        tag_filter: &mut InternalTagFilter,
        tool_calls: &mut Vec<PartialToolCall>,
        stream_error: &mut Option<String>,
        truncated_due_to_length: &mut bool,
        event_tx: &mpsc::Sender<AgentEvent>,
    ) -> StreamEventOutcome {
        match event {
            StreamEvent::TextDelta(delta) => {
                if let Some(filtered) = tag_filter.push(&delta) {
                    let redacted = self.maybe_redact(filtered);
                    let _ = event_tx.send(AgentEvent::TextDelta(redacted)).await;
                }
            }
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                if index >= MAX_TOOL_CALLS {
                    warn!("Tool call index {index} exceeds limit {MAX_TOOL_CALLS}, ignoring");
                } else {
                    while tool_calls.len() <= index {
                        tool_calls.push(PartialToolCall::default());
                    }
                    if let Some(id) = id {
                        tool_calls[index].id = id;
                    }
                    if let Some(name) = name {
                        tool_calls[index].name = name;
                    }
                    tool_calls[index].arguments.push_str(&arguments_delta);
                }
            }
            StreamEvent::Usage(usage) => {
                self.record_usage(&usage);
                let _ = event_tx.send(AgentEvent::Usage(usage)).await;
            }
            StreamEvent::Truncated => {
                // Provider signaled max-tokens cut-off. The stream still
                // continues to Done so cost-attribution chunks can land;
                // we just remember the flag for finish_iteration so the
                // model can be given a clear "your output was truncated"
                // hint instead of the cryptic JSON parse failure that
                // shows up when a tool call's argument string was
                // unclosed by the cut-off.
                *truncated_due_to_length = true;
            }
            StreamEvent::Done => return StreamEventOutcome::Terminal,
            StreamEvent::Error(e) => {
                warn!("LLM stream emitted error mid-response: {e}");
                *stream_error = Some(e.clone());
                if event_tx.send(AgentEvent::Error(e)).await.is_err() {
                    trace!("Event channel closed, could not deliver stream error");
                }
                return StreamEventOutcome::Terminal;
            }
            StreamEvent::ThinkingDelta(delta) => {
                if self.config.conversation.show_thinking {
                    let _ = event_tx.send(AgentEvent::ThinkingDelta(delta)).await;
                }
            }
        }
        StreamEventOutcome::Continue
    }

    /// Finalize a text-only response: fire TurnComplete hook, persist, auto-save,
    /// and send the TurnComplete event.
    pub(super) async fn finalize_text_response(
        &mut self,
        text_content: &str,
        event_tx: &mpsc::Sender<AgentEvent>,
    ) {
        let hook_ctx = self.hook_ctx(
            HookPoint::TurnComplete,
            HookData::TurnEnd {
                total_tool_calls: 0,
            },
        );
        self.hook_registry.dispatch(&hook_ctx);

        // Don't persist empty assistant responses — they pollute the context
        // window and cause Gemini to reject subsequent requests ("no parts").
        if !text_content.trim().is_empty() {
            self.log_and_persist(Message::assistant(text_content));
        } else {
            tracing::debug!("Skipping persistence of empty assistant response");
        }
        self.maybe_record_plan_emission(text_content);
        self.auto_save();
        self.metrics.agent_turns.add(1, &[]);
        let _ = event_tx.send(AgentEvent::TurnComplete).await;
    }

    /// Award plan-emission XP when in Plan mode and the response carries a
    /// `<proposed_plan>` block. Per-session cap lives in
    /// `evolution::plan_emission::record_plan_emission` (DB-backed via
    /// `EXISTS` over evolution_events.metadata_json).
    pub(super) fn maybe_record_plan_emission(&self, text_content: &str) {
        if self.config.conversation.collaboration_mode != crate::config::CollaborationMode::Plan {
            return;
        }
        if !(text_content.contains("<proposed_plan>") && text_content.contains("</proposed_plan>"))
        {
            return;
        }
        let session_id = self.session.meta.id.clone();
        let guard = self.db_guard();
        let Some(db) = guard.as_ref() else {
            tracing::debug!("plan_emission: no DB available, skipping");
            return;
        };
        match crate::evolution::plan_emission::record_plan_emission(db, &session_id, text_content) {
            Ok(true) => tracing::debug!("plan_emission: recorded for session {session_id}"),
            Ok(false) => {
                tracing::debug!("plan_emission: already recorded for session {session_id}")
            }
            Err(e) => tracing::warn!("plan_emission: failed to record: {e}"),
        }
    }

    /// Handle a text-only LLM response (no tool calls). Either terminates the
    /// turn (by finalizing or recording a stream-interrupted marker) or signals
    /// that the caller should nudge the model with a reminder message and loop
    /// again. Centralizes the three exit branches that used to live inline in
    /// `run_agent_loop`.
    pub(super) async fn handle_text_only_response(
        &mut self,
        text_content: &str,
        stream_error: Option<String>,
        needs_response: bool,
        nudged_for_response: bool,
        event_tx: &mpsc::Sender<AgentEvent>,
    ) -> TextOnlyOutcome {
        // Stream errored out *and* there are no tool calls to recover —
        // persist whatever text arrived with an interruption marker so the
        // next turn (and any later resume) sees that the response was cut
        // short, not a normal completion.
        if let Some(err) = stream_error {
            let content = if text_content.trim().is_empty() {
                format!("[stream interrupted: {err}]")
            } else {
                format!("{text_content}\n\n[stream interrupted: {err}]")
            };
            self.log_and_persist(Message::assistant(&content));
            let _ = event_tx.send(AgentEvent::TurnComplete).await;
            return TextOnlyOutcome::TerminateTurn;
        }

        if should_nudge_for_response(text_content, needs_response, nudged_for_response) {
            self.log_and_persist(Message::system(
                "Respond to the user with a brief confirmation of what you just did.",
            ));
            return TextOnlyOutcome::NudgeAndContinue;
        }

        self.finalize_text_response(text_content, event_tx).await;
        TextOnlyOutcome::TerminateTurn
    }

    /// Persist an interrupted-turn marker when all streamed tool calls failed
    /// validation (names too long, bad JSON, etc.). Emitted as a single
    /// assistant message so the next turn sees the truncation.
    pub(super) async fn persist_discarded_tool_calls(
        &mut self,
        text_content: &str,
        stream_error: Option<String>,
        event_tx: &mpsc::Sender<AgentEvent>,
    ) {
        let trailer = match stream_error {
            Some(err) => format!("[stream interrupted: {err}; incomplete tool calls discarded]"),
            None => "[incomplete tool calls discarded]".to_string(),
        };
        let content = if text_content.is_empty() {
            format!("[response interrupted — {trailer}]")
        } else {
            format!("{text_content}\n\n{trailer}")
        };
        self.log_and_persist(Message::assistant(&content));
        let _ = event_tx.send(AgentEvent::TurnComplete).await;
    }
}

#[cfg(test)]
mod tests {
    const AGENT_RS_SRC: &str = include_str!("stream_handler.rs");

    #[test]
    fn record_usage_is_the_only_usage_persistence_path() {
        // If the match arm regains an inline db.log_token_usage_with_cache(...)
        // call, the outer future's frame grows again and the record_usage
        // extraction loses its purpose.
        // Locate the Usage arm in `handle_stream_event`. The pattern is built
        // with `concat!` so the test's own search literal can't match itself —
        // otherwise the only hit would be this very line.
        let pat = concat!("StreamEvent::Usage", "(usage) =>");
        let arm_start = AGENT_RS_SRC
            .find(pat)
            .expect("Usage arm must exist in handle_stream_event");
        let arm_tail_idx = arm_start + 400;
        let arm_tail = &AGENT_RS_SRC[arm_start..arm_tail_idx.min(AGENT_RS_SRC.len())];
        assert!(
            !arm_tail.contains("log_token_usage_with_cache"),
            "StreamEvent::Usage arm must delegate to self.record_usage; do not \
             inline the DB + pricing call back into handle_stream_event"
        );
    }
}
