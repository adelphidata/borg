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

/// The visible notice for an answer that a provider-side filter stopped.
/// `reason` is the provider's stop label (`content_filter`, `refusal`, ...).
pub(super) fn content_filtered_notice(reason: &str) -> String {
    format!(
        "\n\n[The provider stopped this answer before it was complete ({reason}). \
         Try again with different wording.]"
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
                    self.emitted_text_bytes += redacted.len();
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
                if !usage.model.is_empty() {
                    self.turn_reasoning_model = Some(usage.model.clone());
                }
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
            StreamEvent::ContentFiltered { reason } => {
                // A safety or content filter stopped the answer. This is not
                // a length cut, so it must not trigger compaction; the stream
                // still runs to Done. Tell the user plainly why the answer
                // ends here, as visible text so every surface shows it. The
                // notice counts as emitted text so a later retraction covers
                // it too.
                let notice = content_filtered_notice(&reason);
                self.emitted_text_bytes += notice.len();
                let _ = event_tx.send(AgentEvent::TextDelta(notice)).await;
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
                self.metrics.record_llm_retry("failover");
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
                //
                // Attempt 0 is the request itself, already counted by
                // `borg.llm.requests`; only the re-attempts are retries.
                if attempt > 0 {
                    self.metrics.record_llm_retry("retry");
                }
                // Each attempt sends the full response again. An attempt
                // that failed after it emitted output (a connection that
                // closed before the completion marker, a stall in the middle
                // of the stream, the attempt before a failover) must leave
                // nothing in the collectors: the saved message holds the
                // output of the attempt that completes, and only that.
                // On the first attempt the collectors are empty already.
                // The consumers hold the deltas of the failed attempt too;
                // the retraction goes out before the new attempt's marker.
                self.retract_emitted_text(event_tx).await;
                *tag_filter = InternalTagFilter::new();
                tool_calls.clear();
                *truncated_due_to_length = false;
                self.tool_intent_signalled = false;
                self.turn_reasoning.clear();
                self.turn_reasoning_blocks.clear();
                self.turn_reasoning_content.clear();
                self.turn_reasoning_model = None;
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
                // Not counted in `borg.llm.retries`: this notice precedes the
                // backoff, and the attempt it announces is counted when
                // `AttemptStarted` arrives. Counting here too would report one
                // re-attempt as two.
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
            StreamEvent::ReasoningBlock(block) => {
                // Verbatim, in wire order: the signature verifies only on
                // the exact block the model returned.
                self.turn_reasoning_blocks.push(block);
            }
            StreamEvent::ThinkingDelta(delta) => {
                // The full text travels back to providers that require their
                // reasoning echoed on the next tool-call turn. Every other
                // provider also streams thinking as deltas, yet never takes
                // it back, so for them nothing is kept: the full trace would
                // otherwise sit in history and the session file for nothing.
                if self.reasoning_echo_expected() {
                    self.turn_reasoning_content.push_str(&delta);
                }
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

    /// Whether the provider serving this stream takes the turn's thinking
    /// text back as `reasoning_content` on the next tool-call request (the
    /// same rule the OpenAI-compatible wire applies when it builds that
    /// request). After a failover only the slot's provider is known, not
    /// its endpoint, so a custom endpoint reached by failover reads as
    /// "no echo": a missing echo costs one 400 at most, a stored trace for
    /// a provider that never wanted it costs every session.
    fn reasoning_echo_expected(&self) -> bool {
        let (provider, base_url) = match self.active_failover_identity.as_ref() {
            Some((provider, _)) => (provider.as_str(), None),
            None => (
                self.config.llm.provider.as_deref().unwrap_or(""),
                self.config.llm.base_url.as_deref(),
            ),
        };
        provider
            .parse::<crate::llm::provider::Provider>()
            .is_ok_and(|provider| crate::llm::provider_echoes_reasoning(provider, base_url))
    }

    /// Tell the consumers to drop the text deltas of the current iteration,
    /// because the agent discards the output that produced them. Sends
    /// nothing when no visible text went out.
    pub(super) async fn retract_emitted_text(&mut self, event_tx: &mpsc::Sender<AgentEvent>) {
        let bytes = std::mem::take(&mut self.emitted_text_bytes);
        if bytes == 0 {
            return;
        }
        if event_tx
            .send(AgentEvent::TextRetracted { bytes })
            .await
            .is_err()
        {
            trace!("Event channel closed, could not deliver text retraction");
        }
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

    /// Spec: `borg.llm.retries` must count each re-attempt exactly once, so the
    /// kinds stay summable.
    ///
    /// `Reconnecting` is emitted *before* the backoff and the retry loop then
    /// starts the next attempt, which `AttemptStarted` reports — so counting
    /// the notice as well scores one re-attempt twice. This drives both arms
    /// in the order the loop emits them.
    #[tokio::test]
    async fn a_reconnect_notice_is_not_counted_as_a_second_re_attempt() {
        use crate::internal_tag_filter::InternalTagFilter;
        use crate::llm::StreamEvent;
        use crate::telemetry::{names, tests as metrics_test, BorgMetrics};

        let (provider, meter, exporter) = metrics_test::test_metrics();
        let mut agent = crate::agent::tests::minimal_test_agent(crate::config::Config::default());
        agent.metrics = BorgMetrics::from_meter(&meter);

        // Kept alive: a dropped receiver would make every send fail, and the
        // arms under test would still be reached — a silent weakening.
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(32);
        let mut tag_filter = InternalTagFilter::new();
        let mut tool_calls = Vec::new();
        let mut stream_error = None;
        let mut truncated = false;

        // What the loop really emits for one connection-lost retry: the
        // notice, then the attempt it was announcing.
        for event in [
            StreamEvent::AttemptStarted { attempt: 0 },
            StreamEvent::Reconnecting {
                attempt: 0,
                delay_secs: 3,
            },
            StreamEvent::AttemptStarted { attempt: 1 },
        ] {
            agent
                .handle_stream_event(
                    event,
                    &mut tag_filter,
                    &mut tool_calls,
                    &mut stream_error,
                    &mut truncated,
                    &event_tx,
                )
                .await;
        }

        let rms = metrics_test::collect(&provider, &exporter);
        let kinds = metrics_test::sum_points_by_attr(&rms, names::LLM_RETRIES, "kind");
        assert_eq!(
            kinds.get("retry"),
            Some(&1),
            "one re-attempt must score once, not once per event announcing it"
        );
        assert_eq!(
            kinds.get("reconnect"),
            None,
            "a reconnect kind would double-count the attempt AttemptStarted already reports"
        );
    }

    /// Spec: the reasoning a stream produced rides on the assistant tool-call
    /// message it led to — signed Anthropic blocks verbatim and in order and
    /// the model the usage record names — so the next request of the loop
    /// can replay it. Anthropic never takes the thinking text back, so
    /// `reasoning_content` stays `None` for it. A failed attempt leaves none
    /// of it behind, and a saved message takes it with it so the next stream
    /// starts clean.
    #[tokio::test]
    async fn stream_reasoning_rides_on_the_assistant_tool_call_message() {
        use crate::internal_tag_filter::InternalTagFilter;
        use crate::llm::{StreamEvent, UsageData};

        let mut config = crate::config::Config::default();
        config.llm.provider = Some("anthropic".to_string());
        let mut agent = crate::agent::tests::minimal_test_agent(config);
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(64);
        let mut tag_filter = InternalTagFilter::new();
        let mut tool_calls = Vec::new();
        let mut stream_error = None;
        let mut truncated = false;
        let usage = |model: &str| {
            StreamEvent::Usage(UsageData {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                cached_input_tokens: 0,
                cache_creation_tokens: 0,
                provider: "anthropic".to_string(),
                model: model.to_string(),
                provider_cost_usd: None,
                response_id: None,
                upstream: None,
            })
        };
        let first = serde_json::json!({"type": "thinking", "thinking": "stale", "signature": "s0"});
        let thinking =
            serde_json::json!({"type": "thinking", "thinking": "plan", "signature": "s1"});
        let redacted = serde_json::json!({"type": "redacted_thinking", "data": "opaque=="});
        for event in [
            StreamEvent::AttemptStarted { attempt: 0 },
            StreamEvent::ReasoningBlock(first.clone()),
            StreamEvent::ThinkingDelta("stale".to_string()),
            usage("claude-opus-4-6"),
            StreamEvent::AttemptStarted { attempt: 1 },
            StreamEvent::ReasoningBlock(thinking.clone()),
            StreamEvent::ThinkingDelta("pl".to_string()),
            StreamEvent::ThinkingDelta("an".to_string()),
            StreamEvent::ReasoningBlock(redacted.clone()),
            usage("claude-opus-4-7"),
            StreamEvent::Done,
        ] {
            agent
                .handle_stream_event(
                    event,
                    &mut tag_filter,
                    &mut tool_calls,
                    &mut stream_error,
                    &mut truncated,
                    &event_tx,
                )
                .await;
        }

        let invalid = vec![super::super::tool_validation::InvalidToolCall {
            id: "call_1".to_string(),
            name: "fs_read".to_string(),
            reason: "malformed JSON".to_string(),
            raw_arguments: "{".to_string(),
        }];
        let cancel = tokio_util::sync::CancellationToken::new();
        agent
            .execute_tool_calls(&[], &invalid, "", &event_tx, &cancel)
            .await;

        let saved = agent
            .history
            .iter()
            .find(|m| m.role == crate::types::Role::Assistant && m.tool_calls.is_some())
            .expect("the assistant tool-call message must be saved");
        assert_eq!(
            saved.reasoning_blocks,
            Some(vec![thinking, redacted]),
            "only the completing attempt's blocks, in wire order"
        );
        assert_eq!(
            saved.reasoning_content, None,
            "Anthropic never takes the thinking text back, so none is stored"
        );
        assert_eq!(saved.reasoning_model.as_deref(), Some("claude-opus-4-7"));
        assert!(agent.turn_reasoning_blocks.is_empty(), "taken, not copied");
        assert!(agent.turn_reasoning_content.is_empty());
        assert!(agent.turn_reasoning_model.is_none());
    }

    /// Spec: the thinking text is kept on the tool-call turn only for a
    /// provider that requires it echoed on the next request (DeepSeek-style
    /// `reasoning_content`). Anthropic and OpenAI stream thinking as the
    /// same deltas and never take it back, so for them nothing is stored:
    /// the trace would otherwise sit in history and the session file for
    /// every tool-call turn, for nothing.
    #[tokio::test]
    async fn thinking_text_is_stored_only_for_providers_that_echo_it() {
        use crate::internal_tag_filter::InternalTagFilter;
        use crate::llm::{StreamEvent, UsageData};

        let cases: [(&str, Option<&str>, &str, Option<&str>); 3] = [
            ("anthropic", None, "claude-opus-4-7", None),
            ("openai", None, "o4-mini", None),
            ("deepseek", None, "deepseek-reasoner", Some("plan")),
        ];
        for (provider, base_url, model, want) in cases {
            let mut config = crate::config::Config::default();
            config.llm.provider = Some(provider.to_string());
            config.llm.base_url = base_url.map(str::to_string);
            let mut agent = crate::agent::tests::minimal_test_agent(config);
            let (event_tx, _event_rx) = tokio::sync::mpsc::channel(64);
            let mut tag_filter = InternalTagFilter::new();
            let mut tool_calls = Vec::new();
            let mut stream_error = None;
            let mut truncated = false;
            for event in [
                StreamEvent::AttemptStarted { attempt: 0 },
                StreamEvent::ThinkingDelta("pl".to_string()),
                StreamEvent::ThinkingDelta("an".to_string()),
                StreamEvent::Usage(UsageData {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                    cached_input_tokens: 0,
                    cache_creation_tokens: 0,
                    provider: provider.to_string(),
                    model: model.to_string(),
                    provider_cost_usd: None,
                    response_id: None,
                    upstream: None,
                }),
                StreamEvent::Done,
            ] {
                agent
                    .handle_stream_event(
                        event,
                        &mut tag_filter,
                        &mut tool_calls,
                        &mut stream_error,
                        &mut truncated,
                        &event_tx,
                    )
                    .await;
            }
            let invalid = vec![super::super::tool_validation::InvalidToolCall {
                id: "call_1".to_string(),
                name: "fs_read".to_string(),
                reason: "malformed JSON".to_string(),
                raw_arguments: "{".to_string(),
            }];
            let cancel = tokio_util::sync::CancellationToken::new();
            agent
                .execute_tool_calls(&[], &invalid, "", &event_tx, &cancel)
                .await;

            let saved = agent
                .history
                .iter()
                .find(|m| m.role == crate::types::Role::Assistant && m.tool_calls.is_some())
                .expect("the assistant tool-call message must be saved");
            assert_eq!(saved.reasoning_content.as_deref(), want, "{provider}");
            assert_eq!(saved.reasoning_model.as_deref(), Some(model), "{provider}");
        }
    }

    /// Spec: the saved assistant message holds the output of the attempt
    /// that completes, and only that.
    ///
    /// The event order is the one the client emits when the first attempt
    /// sends output and the connection closes before the completion marker:
    /// no `Done`, a reconnect notice, then a new attempt with the full answer.
    #[tokio::test]
    async fn a_retry_after_partial_output_saves_only_the_final_attempt() {
        use crate::agent::AgentEvent;
        use crate::internal_tag_filter::InternalTagFilter;
        use crate::llm::StreamEvent;

        let mut agent = crate::agent::tests::minimal_test_agent(crate::config::Config::default());
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(64);
        let mut tag_filter = InternalTagFilter::new();
        let mut tool_calls = Vec::new();
        let mut stream_error = None;
        let mut truncated = false;

        let delta = |text: &str| StreamEvent::TextDelta(text.to_string());
        for event in [
            StreamEvent::AttemptStarted { attempt: 0 },
            delta("The answer is fo"),
            StreamEvent::ThinkingDelta("first attempt thought".to_string()),
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("read_file".to_string()),
                arguments_delta: "{\"path\":\"/tm".to_string(),
            },
            StreamEvent::Truncated,
            StreamEvent::ToolIntent,
            StreamEvent::Reconnecting {
                attempt: 0,
                delay_secs: 5,
            },
            StreamEvent::AttemptStarted { attempt: 1 },
            delta("The answer is "),
            delta("forty-two."),
            StreamEvent::Done,
        ] {
            agent
                .handle_stream_event(
                    event,
                    &mut tag_filter,
                    &mut tool_calls,
                    &mut stream_error,
                    &mut truncated,
                    &event_tx,
                )
                .await;
        }

        assert!(
            tool_calls.is_empty(),
            "a half tool call of the failed attempt must not run"
        );
        assert!(!truncated, "the length cut belongs to the failed attempt");
        assert!(!agent.tool_intent_signalled);
        assert!(agent.turn_reasoning.is_empty());
        assert_eq!(tag_filter.full_clean(), "The answer is forty-two.");

        agent
            .handle_text_only_response(&tag_filter.full_clean(), None, false, false, &event_tx)
            .await;
        let saved: Vec<&str> = agent
            .history
            .iter()
            .filter(|m| matches!(m.role, crate::types::Role::Assistant))
            .filter_map(|m| m.text_content())
            .collect();
        assert_eq!(saved, vec!["The answer is forty-two."]);

        // The surface learns about the new attempt through the existing
        // event, after the deltas of the failed attempt.
        let mut seen = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            match event {
                AgentEvent::TextDelta(text) => seen.push(format!("text:{text}")),
                AgentEvent::LlmAttemptStarted { attempt } => {
                    seen.push(format!("attempt:{attempt}"))
                }
                AgentEvent::TextRetracted { bytes } => seen.push(format!("retract:{bytes}")),
                _ => {}
            }
        }
        assert_eq!(
            seen,
            vec![
                "attempt:0",
                "text:The answer is fo",
                "retract:16",
                "attempt:1",
                "text:The answer is ",
                "text:forty-two.",
            ]
        );
    }

    /// Spec: a filtered stop tells the user, in visible text, that the
    /// provider stopped the answer and why; it is not a length cut, so it
    /// must not arm the output-cap compaction path.
    #[tokio::test]
    async fn a_filtered_stop_is_a_visible_notice_and_not_a_length_cut() {
        use crate::agent::AgentEvent;
        use crate::internal_tag_filter::InternalTagFilter;
        use crate::llm::StreamEvent;

        let mut agent = crate::agent::tests::minimal_test_agent(crate::config::Config::default());
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(64);
        let mut tag_filter = InternalTagFilter::new();
        let mut tool_calls = Vec::new();
        let mut stream_error = None;
        let mut truncated = false;
        for event in [
            StreamEvent::TextDelta("Here is how to".to_string()),
            StreamEvent::ContentFiltered {
                reason: "content_filter".to_string(),
            },
            StreamEvent::Done,
        ] {
            agent
                .handle_stream_event(
                    event,
                    &mut tag_filter,
                    &mut tool_calls,
                    &mut stream_error,
                    &mut truncated,
                    &event_tx,
                )
                .await;
        }
        assert!(!truncated, "a filtered stop must not read as a length cut");
        assert_eq!(stream_error, None, "the stream itself did not fail");

        let mut texts = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            if let AgentEvent::TextDelta(text) = event {
                texts.push(text);
            }
        }
        assert_eq!(
            texts.len(),
            2,
            "the partial text, then the notice: {texts:?}"
        );
        assert_eq!(texts[0], "Here is how to");
        assert!(
            texts[1].contains("stopped this answer") && texts[1].contains("content_filter"),
            "the notice names the provider's stop label: {}",
            texts[1]
        );
        // The notice is visible text: a retraction after a retry must cover
        // it as well as the partial answer.
        assert_eq!(
            agent.emitted_text_bytes,
            texts[0].len() + texts[1].len(),
            "the notice counts as emitted text"
        );
        assert_eq!(
            tag_filter.full_clean(),
            "Here is how to",
            "the notice is display-only, never part of the saved assistant text"
        );
    }

    /// Drive `events` through the handler and return the text events and
    /// retractions that a consumer receives, in order.
    async fn consumer_view(
        agent: &mut crate::agent::Agent,
        events: Vec<crate::llm::StreamEvent>,
    ) -> Vec<String> {
        use crate::agent::AgentEvent;
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(64);
        let mut tag_filter = crate::internal_tag_filter::InternalTagFilter::new();
        let mut tool_calls = Vec::new();
        let mut stream_error = None;
        let mut truncated = false;
        for event in events {
            agent
                .handle_stream_event(
                    event,
                    &mut tag_filter,
                    &mut tool_calls,
                    &mut stream_error,
                    &mut truncated,
                    &event_tx,
                )
                .await;
        }
        let mut seen = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            match event {
                AgentEvent::TextDelta(text) => seen.push(format!("text:{text}")),
                AgentEvent::TextRetracted { bytes } => seen.push(format!("retract:{bytes}")),
                _ => {}
            }
        }
        seen
    }

    /// Spec: a retraction goes out only when a new attempt discards text
    /// that the consumers hold, and its size is the UTF-8 length of that
    /// text as it was sent.
    #[tokio::test]
    async fn a_retraction_is_sent_only_for_discarded_visible_text() {
        use crate::llm::StreamEvent;
        let delta = |text: &str| StreamEvent::TextDelta(text.to_string());
        let attempt = |attempt| StreamEvent::AttemptStarted { attempt };
        let thought = || StreamEvent::ThinkingDelta("hidden".to_string());
        let half_call = || StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("call_1".to_string()),
            name: Some("read_file".to_string()),
            arguments_delta: "{".to_string(),
        };

        // (name, stream events, consumer view)
        let cases: Vec<(&str, Vec<StreamEvent>, Vec<&str>)> = vec![
            (
                "first attempt",
                vec![attempt(0), delta("hello"), StreamEvent::Done],
                vec!["text:hello"],
            ),
            (
                "retry with no visible text",
                vec![attempt(0), thought(), half_call(), attempt(1), delta("ok")],
                vec!["text:ok"],
            ),
            (
                "retry after text held back by the tag filter",
                vec![
                    attempt(0),
                    delta("<internal>secret"),
                    attempt(1),
                    delta("ok"),
                ],
                vec!["text:ok"],
            ),
            (
                "multi-byte text counts bytes, not chars",
                vec![
                    attempt(0),
                    delta("caf\u{e9} "),
                    delta("\u{1f600}"),
                    attempt(1),
                ],
                vec!["text:caf\u{e9} ", "text:\u{1f600}", "retract:10"],
            ),
            (
                "a second retry retracts only the second attempt",
                vec![
                    attempt(0),
                    delta("abc"),
                    attempt(1),
                    delta("abcde"),
                    attempt(2),
                    delta("done"),
                ],
                vec![
                    "text:abc",
                    "retract:3",
                    "text:abcde",
                    "retract:5",
                    "text:done",
                ],
            ),
            (
                "the first attempt of a fallback leg retracts the failed leg",
                vec![
                    attempt(0),
                    delta("partial"),
                    StreamEvent::Failover {
                        provider: "p".to_string(),
                        model: "m".to_string(),
                    },
                    attempt(0),
                    delta("full"),
                ],
                vec!["text:partial", "retract:7", "text:full"],
            ),
        ];
        for (name, events, want) in cases {
            let mut agent =
                crate::agent::tests::minimal_test_agent(crate::config::Config::default());
            let seen = consumer_view(&mut agent, events).await;
            assert_eq!(seen, want, "{name}");
        }
    }

    /// Spec: the retraction counts the text as the consumers received it.
    /// Redaction changes the length, so the raw length is the wrong size.
    #[tokio::test]
    async fn a_retraction_counts_the_redacted_text() {
        use crate::llm::StreamEvent;
        let mut config = crate::config::Config::default();
        config.security.secret_detection = true;
        let mut agent = crate::agent::tests::minimal_test_agent(config);
        let secret = "key sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGH end";
        let seen = consumer_view(
            &mut agent,
            vec![
                StreamEvent::AttemptStarted { attempt: 0 },
                StreamEvent::TextDelta(secret.to_string()),
                StreamEvent::AttemptStarted { attempt: 1 },
            ],
        )
        .await;
        let sent = seen[0].strip_prefix("text:").expect("text first");
        assert_ne!(sent, secret, "the fixture must trigger redaction");
        assert_ne!(sent.len(), secret.len(), "redaction must change the length");
        assert_eq!(seen[1], format!("retract:{}", sent.len()));
    }

    /// Spec: text of an earlier tool iteration stays. The loop sets the
    /// count to zero when an iteration starts, so attempt 0 of the next
    /// iteration retracts nothing.
    #[tokio::test]
    async fn a_new_tool_iteration_retracts_nothing() {
        use crate::llm::StreamEvent;
        let mut agent = crate::agent::tests::minimal_test_agent(crate::config::Config::default());
        let first = consumer_view(
            &mut agent,
            vec![
                StreamEvent::AttemptStarted { attempt: 0 },
                StreamEvent::TextDelta("I will read the file.".to_string()),
                StreamEvent::Done,
            ],
        )
        .await;
        assert_eq!(first, vec!["text:I will read the file."]);
        // What `run_agent_loop` does at the top of each iteration.
        agent.emitted_text_bytes = 0;
        let second = consumer_view(
            &mut agent,
            vec![
                StreamEvent::AttemptStarted { attempt: 0 },
                StreamEvent::TextDelta("Done.".to_string()),
            ],
        )
        .await;
        assert_eq!(second, vec!["text:Done."]);
    }

    #[test]
    fn retract_text_tail_is_safe_on_any_buffer() {
        use crate::agent::retract_text_tail;
        // (buffer, bytes, result)
        let cases = [
            ("The answer is fo", 16, ""),
            ("Earlier text. partial", 7, "Earlier text. "),
            ("caf\u{e9} \u{1f600}", 4, "caf\u{e9} "),
            // A cut inside a char moves down to the boundary.
            ("a\u{1f600}", 2, "a"),
            ("ab\u{e9}", 1, "ab"),
            // A shorter buffer becomes empty.
            ("short", 500, ""),
            ("", 3, ""),
            ("keep", 0, "keep"),
        ];
        for (buffer, bytes, want) in cases {
            let mut text = buffer.to_string();
            retract_text_tail(&mut text, bytes);
            assert_eq!(text, want, "{buffer:?} minus {bytes}");
        }
    }

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
