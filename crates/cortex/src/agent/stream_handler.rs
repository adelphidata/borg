//! Stream-event handling and text-only response finalization.
//!
//! These methods process the per-iteration LLM stream into history mutations and TUI
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

use super::tool_validation::{apply_tool_call_delta, PartialToolCall};
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

/// Consecutive stale/timed-out stream kills tolerated before the per-session
/// circuit breaker starts short-circuiting turns.
pub(crate) const MAX_CONSECUTIVE_STALE_STREAMS: u32 = 5;

/// Cross-turn stale-stream circuit breaker.
///
/// A session wedged against an unresponsive provider hits the stall detector
/// on every turn: each attempt burns the full chunk timeout (× the client's
/// internal retries/fallbacks) and yields nothing. The client bounds a single
/// hung stream; nothing bounds *repeated cross-turn* staleness. This counter
/// tracks consecutive stale kills and, at [`MAX_CONSECUTIVE_STALE_STREAMS`],
/// short-circuits the next turn with an actionable error instead.
///
/// The breaker is half-open rather than latched: after a short-circuited
/// turn, the following turn is allowed through as a single probe so a
/// recovered provider can re-prove itself without a config change (a
/// long-lived `Agent` survives `/new`, so a latched breaker could wedge the
/// process permanently).
///
/// Resets: a completed stream, a usable partial result (chunks arrived — the
/// provider is demonstrably responsive), a mid-stream fallback activation,
/// or a model/provider switch via config reload.
#[derive(Debug, Default)]
pub(crate) struct StreamHealth {
    consecutive_stale: u32,
    /// Set when the tripped breaker has already short-circuited a turn; the
    /// next turn is let through as the single probe.
    probe_armed: bool,
}

impl StreamHealth {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// A stream was killed as stale/timed out with nothing usable received.
    /// Returns the new streak length for logging.
    pub(crate) fn record_stale_kill(&mut self) -> u32 {
        self.consecutive_stale += 1;
        self.consecutive_stale
    }

    /// The provider demonstrated responsiveness (completed stream or usable
    /// partial result) — clear the streak.
    pub(crate) fn record_responsive(&mut self) {
        *self = Self::default();
    }

    /// The LLM target changed (model/provider switch, fallback activation,
    /// primary restore) — the new target deserves a clean slate.
    pub(crate) fn record_provider_switch(&mut self) {
        *self = Self::default();
    }

    /// Current consecutive-stale streak (for the user-facing error message).
    pub(crate) fn streak(&self) -> u32 {
        self.consecutive_stale
    }

    /// Turn-entry gate. `true` means the turn must abort immediately with
    /// [`wedged_session_error`]; the following call (next turn) is allowed
    /// through as a single probe attempt.
    pub(crate) fn should_abort_turn(&mut self) -> bool {
        if self.consecutive_stale < MAX_CONSECUTIVE_STALE_STREAMS {
            return false;
        }
        if self.probe_armed {
            // The previous turn already aborted with the actionable error;
            // let this one through as the probe.
            self.probe_armed = false;
            false
        } else {
            self.probe_armed = true;
            true
        }
    }
}

/// Actionable error for a session whose streams keep dying as stale.
pub(super) fn wedged_session_error(consecutive: u32) -> String {
    format!(
        "Aborting before the LLM call: the last {consecutive} stream attempts in this \
         session failed on timeouts or network errors without producing any output, \
         so this turn would very likely fail the same way. The session appears wedged \
         against an unreachable or unresponsive provider. Try switching the model or \
         provider in /settings, check the provider's status page and your network, or \
         start a new session. Your next message will make a single probe attempt; a \
         completed response resets this breaker."
    )
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
                    // Send errors mean the TUI receiver dropped (e.g. UI torn down mid-turn); the loop keeps running regardless.
                    let _ = event_tx.send(AgentEvent::TextDelta(redacted)).await;
                }
            }
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                apply_tool_call_delta(
                    tool_calls,
                    index,
                    id,
                    name,
                    &arguments_delta,
                    MAX_TOOL_CALLS,
                );
            }
            StreamEvent::Usage(usage) => {
                // Settle any pending compaction anti-thrash verdict with the
                // provider's *real* prompt size. Cache read/creation tokens
                // are added because Anthropic-style accounting reports them
                // disjoint from `prompt_tokens`; for OpenAI-style providers
                // (cached ⊆ prompt) this can overcount, which errs toward a
                // strike — the safe direction, since a doubtful reading must
                // never reset the strike and the guard self-heals once
                // history shrinks.
                let real_prompt = usage
                    .prompt_tokens
                    .saturating_add(usage.cached_input_tokens)
                    .saturating_add(usage.cache_creation_tokens);
                self.compaction_strikes
                    .observe_real_prompt_tokens(real_prompt);
                self.record_usage(&usage).await;
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
            StreamEvent::ToolIntent => {
                // The provider says this response ends in tool calls. If none
                // actually parse, the text-only path uses this to nudge once
                // instead of ending the turn on the model's narration.
                self.tool_intent_signalled = true;
            }
            StreamEvent::Failover { provider, model } => {
                // The client switched to a fallback slot mid-request. Record it
                // so the next iteration's <environment> section reports the
                // model that actually served the response, keeping the model's
                // self-identification truthful during a primary outage.
                tracing::info!(provider = %provider, model = %model, "LLM failover; syncing prompt model identity");
                // Fallback activation targets a different provider/model —
                // give the stale-stream breaker a clean slate so the fallback
                // isn't short-circuited by the primary's streak.
                self.stream_health.record_provider_switch();
                self.active_failover_identity = Some((provider, model));
            }
            StreamEvent::AttemptStarted { attempt } => {
                // Liveness only — forward it so a sub-agent's stall watchdog
                // sees that a request is in flight. A retry storm or a long
                // prefill emits nothing else for minutes.
                if event_tx
                    .send(AgentEvent::LlmAttemptStarted { attempt })
                    .await
                    .is_err()
                {
                    trace!("Event channel closed, could not deliver attempt marker");
                }
            }
            StreamEvent::Reconnecting {
                attempt,
                delay_secs,
            } => {
                // The wait is about to be seconds-to-a-minute long. Forward it
                // so the surface can name the pause; without this the turn
                // just goes quiet and reads as a hang.
                warn!("LLM connection lost; reconnecting in {delay_secs}s (attempt {attempt})");
                if event_tx
                    .send(AgentEvent::LlmReconnecting {
                        attempt,
                        delay_secs,
                    })
                    .await
                    .is_err()
                {
                    trace!("Event channel closed, could not deliver reconnect notice");
                }
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
                // Buffer regardless of `show_thinking`: the empty-response
                // terminal delivers an excerpt of this, and a user who turned
                // the thinking trace off still deserves an answer when the
                // model put one only in its reasoning. Never persisted.
                self.turn_reasoning.push_str(&delta);
                if self.turn_reasoning.len() > super::REASONING_BUFFER_CAP {
                    // Keep the tail (the concluding thought), trimmed to a
                    // char boundary so the excerpt stays valid UTF-8.
                    let cut = self.turn_reasoning.len() - super::REASONING_BUFFER_CAP;
                    let cut = (cut..self.turn_reasoning.len())
                        .find(|i| self.turn_reasoning.is_char_boundary(*i))
                        .unwrap_or(self.turn_reasoning.len());
                    self.turn_reasoning.drain(..cut);
                }
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
        self.hook_registry.dispatch(&hook_ctx).await;

        // Don't persist empty assistant responses — they pollute the context
        // window and cause Gemini to reject subsequent requests ("no parts").
        if !text_content.trim().is_empty() {
            self.log_and_persist(Message::assistant(text_content)).await;
        } else {
            // The turn is ending with nothing visible. Silence here reads as
            // a completed turn, so deliver the model's own reasoning tail if
            // it produced any (it frequently holds the actual answer), and an
            // explicit notice otherwise. Display-only: the excerpt goes out
            // as a text delta, while the persisted assistant message carries
            // only a neutral marker — chain-of-thought replayed as assistant
            // content reads to providers as injected reasoning and can
            // deterministically produce another empty response, making this
            // path self-reinforcing.
            let terminal = self.empty_response_terminal();
            tracing::debug!("Empty assistant response; delivering terminal notice");
            let _ = event_tx.send(AgentEvent::TextDelta(terminal)).await;
            self.log_and_persist(Message::assistant(super::EMPTY_RESPONSE_PERSISTED_MARKER))
                .await;
        }
        self.maybe_record_plan_emission(text_content).await;
        self.auto_save();
        self.metrics.agent_turns.add(1, &[]);
        let _ = event_tx.send(AgentEvent::TurnComplete).await;
    }

    /// Build the message delivered when a turn ends with no visible text:
    /// a labeled excerpt of the buffered reasoning, or an explicit notice
    /// when the model produced nothing at all.
    ///
    /// The label is unconditional so the excerpt can never be mistaken for a
    /// normal answer, and the text is redacted on the same terms as any other
    /// assistant output. Display-only: this string is emitted as a text
    /// delta but must never be persisted (see
    /// [`super::EMPTY_RESPONSE_PERSISTED_MARKER`]).
    pub(super) fn empty_response_terminal(&self) -> String {
        let reasoning = self.turn_reasoning.trim();
        if reasoning.is_empty() {
            return super::EMPTY_RESPONSE_NO_REASONING.to_string();
        }
        let excerpt = self.maybe_redact(reasoning.to_string());
        format!("{}\n\n{excerpt}", super::EMPTY_RESPONSE_REASONING_LABEL)
    }

    /// Award plan-emission XP when in Plan mode and the response carries a
    /// `<proposed_plan>` block. Per-session cap lives in
    /// `evolution::plan_emission::record_plan_emission` (DB-backed via
    /// `EXISTS` over evolution_events.metadata_json).
    pub(super) async fn maybe_record_plan_emission(&self, text_content: &str) {
        if self.config.conversation.collaboration_mode != crate::config::CollaborationMode::Plan {
            return;
        }
        if !(text_content.contains("<proposed_plan>") && text_content.contains("</proposed_plan>"))
        {
            return;
        }
        let session_id = self.session.meta.id.clone();
        let guard = self.db_guard().await;
        let Some(db) = guard.as_ref() else {
            tracing::debug!("plan_emission: no DB available, skipping");
            return;
        };
        match crate::evolution::plan_emission::record_plan_emission(db, &session_id, text_content)
            .await
        {
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
    /// again.
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
            let marker = format!(
                "[stream interrupted: {}]",
                super::interrupted::one_line(&err)
            );
            let content = if text_content.trim().is_empty() {
                marker.clone()
            } else {
                format!("{text_content}\n\n{marker}")
            };
            self.log_and_persist(Message::assistant(&content)).await;
            // The marker went to history but never to the user: a truncated
            // or interrupted turn looked identical to a complete one on
            // screen. Emit just the marker — `text_content` already streamed.
            let _ = event_tx
                .send(AgentEvent::TextDelta(format!("\n\n{marker}")))
                .await;
            let _ = event_tx.send(AgentEvent::TurnComplete).await;
            return TextOnlyOutcome::TerminateTurn;
        }

        // The provider signalled tool use but the payload carried no calls
        // (observed in the wild as finish_reason="tool_calls" with an empty
        // array). Ending the turn here would silently accept the model's
        // narration ("Let me check…") as the final answer — on autonomous
        // lanes that is a no-op run with nothing to show. Nudge once.
        if self.tool_intent_signalled && !self.nudged_for_dropped_tools {
            self.nudged_for_dropped_tools = true;
            warn!("provider signalled tool use but no tool calls arrived; nudging once");
            self.log_and_persist(Message::system(
                "Your response signalled a tool call, but no tool call was received. \
                 Issue the tool call now, or state plainly that you are finished.",
            ))
            .await;
            return TextOnlyOutcome::NudgeAndContinue;
        }

        if should_nudge_for_response(text_content, needs_response, nudged_for_response) {
            self.log_and_persist(Message::system(
                "Respond to the user with a brief confirmation of what you just did.",
            ))
            .await;
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
            Some(err) => format!(
                "[stream interrupted: {}; incomplete tool calls discarded]",
                super::interrupted::one_line(&err)
            ),
            None => "[incomplete tool calls discarded]".to_string(),
        };
        let content = if text_content.is_empty() {
            format!("[response interrupted — {trailer}]")
        } else {
            format!("{text_content}\n\n{trailer}")
        };
        self.log_and_persist(Message::assistant(&content)).await;
        let _ = event_tx.send(AgentEvent::TurnComplete).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{wedged_session_error, StreamHealth, MAX_CONSECUTIVE_STALE_STREAMS};

    const AGENT_RS_SRC: &str = include_str!("stream_handler.rs");

    // -- stale-stream circuit breaker --

    #[test]
    fn breaker_aborts_the_turn_after_five_consecutive_stale_kills() {
        let mut h = StreamHealth::new();
        for i in 1..MAX_CONSECUTIVE_STALE_STREAMS {
            assert_eq!(h.record_stale_kill(), i);
            assert!(
                !h.should_abort_turn(),
                "breaker must not trip below {MAX_CONSECUTIVE_STALE_STREAMS} kills (at {i})"
            );
        }
        h.record_stale_kill();
        assert!(
            h.should_abort_turn(),
            "the attempt after {MAX_CONSECUTIVE_STALE_STREAMS} consecutive stale kills \
             must short-circuit instead of burning the full timeout again"
        );
    }

    #[test]
    fn breaker_allows_a_single_probe_after_the_abort() {
        // Half-open: the turn after a short-circuit goes through as one
        // probe; if the probe also dies stale, the turn after that aborts
        // again. This is what lets a recovered provider re-prove itself.
        let mut h = StreamHealth::new();
        for _ in 0..MAX_CONSECUTIVE_STALE_STREAMS {
            h.record_stale_kill();
        }
        assert!(h.should_abort_turn(), "tripped breaker aborts");
        assert!(!h.should_abort_turn(), "next turn is the probe");
        h.record_stale_kill(); // probe died stale too
        assert!(
            h.should_abort_turn(),
            "post-probe failure re-arms the abort"
        );
    }

    #[test]
    fn completed_stream_resets_the_streak() {
        let mut h = StreamHealth::new();
        for _ in 0..MAX_CONSECUTIVE_STALE_STREAMS {
            h.record_stale_kill();
        }
        h.record_responsive();
        assert!(
            !h.should_abort_turn(),
            "a completed (or usable-partial) stream must fully reset the breaker"
        );
        // The streak restarts from zero — one new stale kill is far from a trip.
        assert_eq!(h.record_stale_kill(), 1);
        assert!(!h.should_abort_turn());
    }

    #[test]
    fn provider_switch_resets_a_tripped_breaker() {
        // Model switch / fallback activation / primary restore all funnel
        // through record_provider_switch: the new target must get a clean
        // slate rather than inheriting the old target's streak.
        let mut h = StreamHealth::new();
        for _ in 0..MAX_CONSECUTIVE_STALE_STREAMS {
            h.record_stale_kill();
        }
        assert!(h.should_abort_turn());
        h.record_provider_switch();
        assert!(!h.should_abort_turn());
        assert_eq!(h.streak(), 0);
    }

    #[test]
    fn wedged_error_is_actionable() {
        // Spec: the abort message must tell the user the session appears
        // wedged and what to try — not just that an error occurred.
        let msg = wedged_session_error(MAX_CONSECUTIVE_STALE_STREAMS);
        assert!(msg.contains("wedged"), "must name the condition: {msg}");
        for remedy in ["/settings", "new session", "probe"] {
            assert!(msg.contains(remedy), "must suggest {remedy:?}: {msg}");
        }
        assert!(
            msg.contains(&MAX_CONSECUTIVE_STALE_STREAMS.to_string()),
            "must report how many attempts stalled"
        );
    }

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
