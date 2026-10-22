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
    /// Tool calls started but not yet answered. Counted rather than flagged:
    /// a child can run several calls at once, so one `ToolResult` must not
    /// close the window while its siblings are still running.
    tools_in_flight: u32,
    /// Ticks elapsed since the *first* of the current batch of calls started.
    /// Deliberately not reset by a later call in the same batch — otherwise a
    /// child issuing back-to-back calls re-arms the window forever and the
    /// bound stops being a bound.
    tool_cycles: u32,
    tool_grace_cycles: u32,
}

impl StallWatchdog {
    pub(super) fn new(stale_threshold: u32, tool_grace_cycles: u32) -> Self {
        Self {
            idle_cycles: 0,
            stalled_emitted: false,
            stale_threshold: stale_threshold.max(1),
            tools_in_flight: 0,
            tool_cycles: 0,
            tool_grace_cycles,
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
    ///   hangs. It does not reset the idle counter. It does open a *bounded*
    ///   grace window (`tool_grace_cycles`) during which ticks are not
    ///   counted as idle, because a quiet build or a long test suite emits
    ///   nothing at all while it works and `run_shell` is exempt from the
    ///   builtin tool deadline. The window is finite, so a tool that hangs
    ///   past it is still cancelled — later, never not at all.
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
        match event {
            // A call is outstanding from here until its result arrives. The
            // elapsed counter starts only on the transition from "nothing
            // running" — a second call joining an already-open window inherits
            // the time the first has already used.
            AgentEvent::ToolExecuting { .. } => {
                if self.tools_in_flight == 0 {
                    self.tool_cycles = 0;
                }
                self.tools_in_flight = self.tools_in_flight.saturating_add(1);
            }
            AgentEvent::ToolResult { .. } => {
                self.tools_in_flight = self.tools_in_flight.saturating_sub(1);
            }
            _ => {}
        }
        is_progress
    }

    /// Advance one heartbeat interval.
    pub(super) fn on_tick(&mut self) -> TickOutcome {
        // A tool call still inside its grace window holds the idle counter
        // where it is — the child is not quiet, its tool is.
        if self.tools_in_flight > 0 && self.tool_cycles < self.tool_grace_cycles {
            self.tool_cycles = self.tool_cycles.saturating_add(1);
            return TickOutcome::Quiet;
        }
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
    const TOOL_GRACE: u32 = 4;

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
        let mut w = StallWatchdog::new(THRESHOLD, TOOL_GRACE);
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
        let mut w = StallWatchdog::new(THRESHOLD, TOOL_GRACE);
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
        let mut w = StallWatchdog::new(THRESHOLD, TOOL_GRACE);
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
            response_id: None,
            upstream: None,
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
        let mut w = StallWatchdog::new(THRESHOLD, TOOL_GRACE);
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
        let mut w = StallWatchdog::new(THRESHOLD, TOOL_GRACE);
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
        let mut w = StallWatchdog::new(THRESHOLD, TOOL_GRACE);
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

    #[test]
    fn a_silent_long_tool_call_is_not_counted_as_idle() {
        // A quiet build emits ToolExecuting and then nothing until it
        // finishes. Without the grace window it is cancelled mid-work.
        let mut w = StallWatchdog::new(THRESHOLD, TOOL_GRACE);
        w.on_event(&AgentEvent::ToolExecuting {
            tool_call_id: "c1".to_string(),
            name: "run_shell".to_string(),
            args: "{}".to_string(),
        });
        for tick in 0..TOOL_GRACE {
            assert_eq!(
                w.on_tick(),
                TickOutcome::Quiet,
                "tick {tick} inside the tool grace window must stay quiet"
            );
        }
        assert_eq!(w.idle_cycles(), 0, "grace ticks must not accrue as idle");

        w.on_event(&AgentEvent::ToolResult {
            tool_call_id: "c1".to_string(),
            name: "run_shell".to_string(),
            result: "done".to_string(),
            images: Vec::new(),
        });
        assert_eq!(w.on_tick(), TickOutcome::Quiet);
    }

    #[test]
    fn a_tool_that_outlives_its_grace_window_still_stalls_and_cancels() {
        // The bound is what keeps the watchdog useful: a wedged tool is
        // cancelled late, not never.
        let mut w = StallWatchdog::new(THRESHOLD, TOOL_GRACE);
        w.on_event(&AgentEvent::ToolExecuting {
            tool_call_id: "c1".to_string(),
            name: "run_shell".to_string(),
            args: "{}".to_string(),
        });
        for _ in 0..TOOL_GRACE {
            assert_eq!(w.on_tick(), TickOutcome::Quiet);
        }
        // Grace spent: the ordinary countdown resumes from zero.
        let mut outcomes = Vec::new();
        for _ in 0..(THRESHOLD + STALL_GRACE_CYCLES) {
            outcomes.push(w.on_tick());
        }
        assert!(
            outcomes.contains(&TickOutcome::EmitStalled),
            "a tool past its grace window must still be reported stalled"
        );
        assert_eq!(
            outcomes.last(),
            Some(&TickOutcome::Cancel),
            "and must still be cancelled"
        );
    }

    #[test]
    fn tool_output_alone_never_extends_the_grace_window() {
        // The exclusion that makes the watchdog work: a hung `yes` streams
        // forever and must not buy itself more time.
        let mut w = StallWatchdog::new(THRESHOLD, TOOL_GRACE);
        w.on_event(&AgentEvent::ToolExecuting {
            tool_call_id: "c1".to_string(),
            name: "run_shell".to_string(),
            args: "{}".to_string(),
        });
        let mut ticks = 0;
        loop {
            w.on_event(&AgentEvent::ToolOutputDelta {
                tool_call_id: "c1".to_string(),
                name: "run_shell".to_string(),
                delta: "y\n".to_string(),
                is_stderr: false,
            });
            if w.on_tick() == TickOutcome::Cancel {
                break;
            }
            ticks += 1;
            assert!(ticks < 100, "a stdout-spewing tool must still be cancelled");
        }
    }

    fn executing(id: &str) -> AgentEvent {
        AgentEvent::ToolExecuting {
            tool_call_id: id.to_string(),
            name: "run_shell".to_string(),
            args: "{}".to_string(),
        }
    }

    fn finished(id: &str) -> AgentEvent {
        AgentEvent::ToolResult {
            tool_call_id: id.to_string(),
            name: "run_shell".to_string(),
            result: "done".to_string(),
            images: Vec::new(),
        }
    }

    #[test]
    fn back_to_back_tool_calls_cannot_re_arm_the_grace_window_forever() {
        // The bound has to be a bound. If each new call reset the elapsed
        // counter, a child looping over tool calls would never accrue an idle
        // cycle and the watchdog could never fire — which is the failure the
        // watchdog exists to catch.
        let mut w = StallWatchdog::new(THRESHOLD, TOOL_GRACE);
        let mut ticks = 0;
        loop {
            w.on_event(&executing(&format!("c{ticks}")));
            if w.on_tick() == TickOutcome::Cancel {
                break;
            }
            ticks += 1;
            assert!(
                ticks < 100,
                "a child issuing endless tool calls must still be cancelled"
            );
        }
    }

    #[test]
    fn one_finished_call_does_not_close_the_window_on_its_siblings() {
        // Parallel tool calls: the first result must not expose the others to
        // the idle counter while they are still legitimately running.
        let mut w = StallWatchdog::new(THRESHOLD, TOOL_GRACE);
        w.on_event(&executing("a"));
        w.on_event(&executing("b"));
        w.on_event(&finished("a"));

        assert_eq!(
            w.on_tick(),
            TickOutcome::Quiet,
            "call b is still running, so this tick is not idle"
        );
        assert_eq!(w.idle_cycles(), 0, "no idle cycle may be charged yet");

        w.on_event(&finished("b"));
        // Both done: the ordinary countdown resumes.
        let mut outcomes = Vec::new();
        for _ in 0..(THRESHOLD + STALL_GRACE_CYCLES) {
            outcomes.push(w.on_tick());
        }
        assert!(
            outcomes.contains(&TickOutcome::EmitStalled),
            "with nothing in flight the watchdog must resume counting"
        );
    }
}
