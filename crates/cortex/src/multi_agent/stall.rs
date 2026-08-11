//! Progress watchdog for a running sub-agent.
//!
//! Extracted from the spawn loop's `tokio::select!` so the state machine can
//! be exercised without a live agent stream — the loop itself has no seam a
//! test can reach.

use crate::agent::AgentEvent;

use super::spawn::STALL_GRACE_CYCLES;

/// What the caller should do after one heartbeat tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TickOutcome {
    /// Still within the threshold — nothing to do.
    Quiet,
    /// Threshold crossed for the first time since the last progress: publish
    /// `AgentTreeEventKind::Stalled` and start the grace window.
    EmitStalled,
    /// Grace window exhausted with zero progress: cancel the child.
    Cancel,
}

/// Raise the configured stale-cycle count so it can never be shorter than a
/// single in-flight request is allowed to take.
///
/// One attempt emits `LlmAttemptStarted` and then nothing until the response
/// begins, so the quiet window between them is bounded by
/// `llm.request_timeout_ms`, not by the configured cycle count. An operator who
/// raises that timeout for a slow local model would otherwise have the watchdog
/// cancel a child mid-prefill — the threshold has to move with it. One extra
/// cycle of slack absorbs the rounding plus the tick that fires just before the
/// timeout would.
pub(super) fn effective_stale_cycles(
    configured_cycles: u32,
    interval_secs: u64,
    request_timeout_ms: u64,
) -> u32 {
    let interval_secs = interval_secs.max(1);
    let timeout_secs = request_timeout_ms.div_ceil(1000);
    let floor = timeout_secs.div_ceil(interval_secs).saturating_add(1);
    configured_cycles.max(u32::try_from(floor).unwrap_or(u32::MAX))
}

/// Counts consecutive heartbeat ticks with no forward progress.
pub(super) struct StallWatchdog {
    idle_cycles: u32,
    stalled_emitted: bool,
    stale_threshold: u32,
}

impl StallWatchdog {
    pub(super) fn new(stale_threshold: u32) -> Self {
        Self {
            idle_cycles: 0,
            stalled_emitted: false,
            stale_threshold: stale_threshold.max(1),
        }
    }

    /// Feed one event. Returns `true` when it counted as forward progress and
    /// the idle counter was reset.
    ///
    /// The rule is *"the model produced something, or a tool finished"*:
    ///
    /// - `TextDelta` — the model is emitting a reply.
    /// - `ThinkingDelta` — the model is emitting reasoning. A wedged
    ///   subprocess cannot synthesize reasoning tokens, so this is as
    ///   trustworthy a progress signal as `TextDelta`. Without it, a model
    ///   that thinks for longer than the threshold before its first visible
    ///   token is killed mid-reason.
    /// - `Usage` — a provider round-trip completed. Discrete (one per LLM
    ///   call) and unforgeable by a hung tool, and it covers the gap where a
    ///   tool-call-only turn emits no other progress event between request
    ///   start and `ToolResult`.
    /// - `ToolResult` — a tool finished, success or failure. A failed tool is
    ///   still observable forward progress.
    /// - `LlmAttemptStarted` — a request went out on the wire. Discrete (one
    ///   per attempt) and produced by the transport, so a hung tool cannot
    ///   forge it. Without it the watchdog is blind to three windows that emit
    ///   nothing else: a retry storm (`max_retries` × `request_timeout_ms` plus
    ///   backoff), each leg of the `llm.fallback` chain, and the summarizer
    ///   call that compaction makes before the main stream opens. All three
    ///   can outlast the threshold on a healthy child.
    ///
    /// Deliberately NOT progress:
    ///
    /// - `ToolOutputDelta` — a hung shell streaming stdout (`yes`, a wedged
    ///   tail) would otherwise reset the counter forever and the watchdog
    ///   could never fire. This exclusion is the whole point of the watchdog;
    ///   do not "fix" it by resetting on every event.
    /// - `ToolExecuting` — "a tool started" is precisely the state that then
    ///   hangs.
    pub(super) fn on_event(&mut self, event: &AgentEvent) -> bool {
        let is_progress = matches!(
            event,
            AgentEvent::TextDelta(_)
                | AgentEvent::ThinkingDelta(_)
                | AgentEvent::Usage(_)
                | AgentEvent::ToolResult { .. }
                | AgentEvent::LlmAttemptStarted { .. }
                // A reconnect wait is the client alive and working, not a
                // wedged child — cancelling it would abandon a turn that is
                // about to recover.
                | AgentEvent::LlmReconnecting { .. }
        );
        if is_progress {
            self.idle_cycles = 0;
            self.stalled_emitted = false;
        }
        is_progress
    }

    /// Advance one heartbeat interval.
    pub(super) fn on_tick(&mut self) -> TickOutcome {
        self.idle_cycles = self.idle_cycles.saturating_add(1);
        if !self.stalled_emitted && self.idle_cycles >= self.stale_threshold {
            self.stalled_emitted = true;
            return TickOutcome::EmitStalled;
        }
        if self.stalled_emitted
            && self.idle_cycles >= self.stale_threshold.saturating_add(STALL_GRACE_CYCLES)
        {
            return TickOutcome::Cancel;
        }
        TickOutcome::Quiet
    }

    /// Consecutive idle heartbeat ticks. Used to render `quiet_secs` in the
    /// structured stall error.
    pub(super) fn idle_cycles(&self) -> u32 {
        self.idle_cycles
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THRESHOLD: u32 = 3;

    fn thinking() -> AgentEvent {
        AgentEvent::ThinkingDelta("reasoning…".to_string())
    }

    fn tool_output() -> AgentEvent {
        AgentEvent::ToolOutputDelta {
            tool_call_id: "call-1".to_string(),
            name: "run_shell".to_string(),
            delta: "y\ny\ny\n".to_string(),
            is_stderr: false,
        }
    }

    #[test]
    fn thinking_only_stream_never_trips_stall_watchdog() {
        // A reasoning model can emit nothing but thinking tokens for minutes.
        // Killing it mid-reason destroys real work and reports it as a hang.
        let mut w = StallWatchdog::new(THRESHOLD);
        for i in 0..(THRESHOLD + STALL_GRACE_CYCLES + 5) {
            w.on_event(&thinking());
            assert_eq!(
                w.on_tick(),
                TickOutcome::Quiet,
                "thinking must count as progress (iteration {i})"
            );
        }
    }

    #[test]
    fn tool_output_delta_only_stream_still_trips_watchdog() {
        // The deliberate exclusion: a hung shell flooding stdout must still
        // be caught. This fails if someone widens `on_event` to reset on
        // every event.
        let mut w = StallWatchdog::new(THRESHOLD);
        let mut outcomes = Vec::new();
        for _ in 0..(THRESHOLD + STALL_GRACE_CYCLES) {
            assert!(
                !w.on_event(&tool_output()),
                "tool output is not forward progress"
            );
            outcomes.push(w.on_tick());
        }
        assert_eq!(
            outcomes[(THRESHOLD - 1) as usize],
            TickOutcome::EmitStalled,
            "must announce the stall exactly at the threshold: {outcomes:?}"
        );
        assert_eq!(
            outcomes.last().copied(),
            Some(TickOutcome::Cancel),
            "must cancel once the grace window is exhausted: {outcomes:?}"
        );
    }

    #[test]
    fn usage_event_resets_idle_counter() {
        // A tool-call-only turn emits no text and no thinking; `Usage` is the
        // only proof the provider round-trip completed.
        let mut w = StallWatchdog::new(THRESHOLD);
        for _ in 0..(THRESHOLD - 1) {
            assert_eq!(w.on_tick(), TickOutcome::Quiet);
        }
        assert!(w.on_event(&AgentEvent::Usage(crate::llm::UsageData {
            prompt_tokens: 10,
            completion_tokens: 20,
            total_tokens: 30,
            cached_input_tokens: 0,
            cache_creation_tokens: 0,
            provider: "test".to_string(),
            model: "test-model".to_string(),
            provider_cost_usd: None,
        })));
        assert_eq!(w.idle_cycles(), 0);
        for _ in 0..(THRESHOLD - 1) {
            assert_eq!(
                w.on_tick(),
                TickOutcome::Quiet,
                "counter must have restarted from zero"
            );
        }
    }

    #[test]
    fn progress_inside_grace_window_recovers_and_can_rearm() {
        let mut w = StallWatchdog::new(THRESHOLD);
        for _ in 0..(THRESHOLD - 1) {
            assert_eq!(w.on_tick(), TickOutcome::Quiet);
        }
        assert_eq!(w.on_tick(), TickOutcome::EmitStalled);

        // Late progress inside the grace window must cancel the cancellation.
        assert!(w.on_event(&AgentEvent::TextDelta("finally".to_string())));
        for _ in 0..(THRESHOLD - 1) {
            assert_eq!(w.on_tick(), TickOutcome::Quiet);
        }
        // …and the watchdog must be able to announce a *second* stall, not
        // stay latched from the first one.
        assert_eq!(w.on_tick(), TickOutcome::EmitStalled);
    }

    #[test]
    fn retry_storm_of_attempt_starts_never_cancels() {
        // A provider outage retries `max_retries` times per leg and then walks
        // the fallback chain. Each attempt can burn the full request timeout
        // and emits no text, no thinking, and no Usage — the response never
        // arrives. Before `LlmAttemptStarted` counted, that silence was
        // indistinguishable from a frozen child and the child was cancelled
        // with a stall error while the transport was working normally.
        let mut w = StallWatchdog::new(THRESHOLD);
        for i in 0..(THRESHOLD + STALL_GRACE_CYCLES + 5) {
            assert!(
                w.on_event(&AgentEvent::LlmAttemptStarted { attempt: i }),
                "an outgoing request attempt is forward progress (iteration {i})"
            );
            assert_eq!(w.on_tick(), TickOutcome::Quiet);
        }
    }

    #[test]
    fn attempt_start_alone_cannot_hold_off_the_watchdog_forever() {
        // The marker fires once per attempt, not on a timer. A child that
        // opens one request and then genuinely wedges must still be caught —
        // otherwise the exclusion list above is meaningless.
        let mut w = StallWatchdog::new(THRESHOLD);
        assert!(w.on_event(&AgentEvent::LlmAttemptStarted { attempt: 0 }));
        let mut outcomes = Vec::new();
        for _ in 0..(THRESHOLD + STALL_GRACE_CYCLES) {
            outcomes.push(w.on_tick());
        }
        assert_eq!(
            outcomes.last().copied(),
            Some(TickOutcome::Cancel),
            "a single attempt must not buy unlimited quiet: {outcomes:?}"
        );
    }

    #[test]
    fn stale_threshold_floor_tracks_the_request_timeout() {
        // The quiet window between `LlmAttemptStarted` and the first response
        // token is bounded by `llm.request_timeout_ms`, not by the configured
        // cycle count. An operator running a slow local model raises that
        // timeout; the watchdog has to widen with it or it cancels healthy
        // children mid-prefill.
        //
        // Defaults: 15 cycles × 30s = 450s vs a 120s timeout → config wins.
        assert_eq!(effective_stale_cycles(15, 30, 120_000), 15);

        // A 600s timeout needs 20 cycles to cover it, +1 for rounding slack.
        assert_eq!(effective_stale_cycles(15, 30, 600_000), 21);

        // Sub-interval timeouts still leave a full cycle of slack rather than
        // collapsing the threshold to zero.
        assert_eq!(effective_stale_cycles(1, 30, 5_000), 2);

        // A zero interval is clamped to 1s rather than dividing by zero.
        assert_eq!(effective_stale_cycles(1, 0, 3_000), 4);
    }
}
