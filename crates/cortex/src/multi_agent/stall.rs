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
}
