use std::time::Instant;

use tokio::sync::oneshot;

use cortex::agent::AgentEvent;
use cortex::config::CollaborationMode;
use heartbeat::scheduler::{HeartbeatEvent, HeartbeatResult, SkipReason};

use super::super::history::{ApprovalStatus, HistoryCell};
use super::super::notify;
use super::{App, AppState, BtwResult, DoctorEvent};

/// Default viewport width used by [`App::flush_after_event`] before the first
/// frame has rendered (and `last_viewport_width` is still 0).
const DEFAULT_FLUSH_WIDTH: u16 = 80;

impl<'a> App<'a> {
    pub fn push_system_message(&mut self, text: String) {
        self.cells.push(HistoryCell::System { text });
    }

    /// Start the view over for another conversation (`/new`, `/resume`,
    /// `/fork`, a loaded session). What already reached terminal scrollback
    /// stays there, but the watermark into `cells` must restart with them:
    /// left behind, the next tool result indexed past the end and crashed.
    pub fn reset_transcript(&mut self) {
        self.truncate_cells(0);
    }

    /// Drop cells from `len` on, keeping the scrollback watermark in range.
    pub fn truncate_cells(&mut self, len: usize) {
        self.cells.truncate(len);
        self.cells_emitted = self.cells_emitted.min(self.cells.len());
    }

    /// The one way to change the collaboration mode: it updates the copy the
    /// footer reads and queues the write the daemon's tool gate depends on.
    pub fn set_mode(&mut self, next: CollaborationMode) {
        self.config.conversation.collaboration_mode = next;
        self.mode_sync_pending = true;
    }

    /// Show an event from a turn this TUI did not start. The first one of
    /// such a turn opens it in the view, as a submit would for the user's own.
    pub fn process_external_event(&mut self, ev: AgentEvent) {
        if matches!(self.state, AppState::Idle) {
            self.push_system_message("[Borg started a turn on its own]".to_string());
            self.enter_streaming();
        }
        self.process_agent_event(ev);
    }

    /// Run a slow daemon RPC off the event loop and show the text it returns
    /// as a system message. Blocking on it instead freezes drawing and input
    /// for as long as the RPC takes: `/heal`'s host audit alone runs for
    /// tens of seconds. The task works on its own clone of the client, so
    /// the shared one stays free for the popups and status refreshes.
    pub fn run_in_background<F, Fut>(&mut self, started: &str, work: F)
    where
        F: FnOnce(crate::daemon_client::DaemonClient) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = String> + Send + 'static,
    {
        self.push_system_message(started.to_string());
        let client = self.client.clone();
        let tx = self.notice_tx.clone();
        tokio::spawn(async move {
            let c = client.lock().await.clone();
            // The receiver only goes away with the App; nothing to tell then.
            let _ = tx.send(work(c).await);
        });
    }

    /// Flush newly-stable cells to the pending scrollback buffer using the
    /// last-known viewport width (cached in `render_viewport`). Falls back to
    /// `DEFAULT_FLUSH_WIDTH` before the first frame has rendered.
    fn flush_after_event(&mut self) {
        let width = if self.last_viewport_width > 0 {
            self.last_viewport_width
        } else {
            DEFAULT_FLUSH_WIDTH
        };
        self.flush_stable_cells(width);
    }

    /// Process a doctor event from the async diagnostics task.
    pub fn process_doctor_event(&mut self, event: DoctorEvent) {
        self.process_doctor_event_inner(event);
        self.flush_after_event();
    }

    fn process_doctor_event_inner(&mut self, event: DoctorEvent) {
        match event {
            DoctorEvent::Analyzing { label } => {
                self.push_system_message(format!("⏳ Analyzing {label}..."));
            }
            DoctorEvent::Result { label, checks } => {
                // Replace the "Analyzing..." cell with actual results.
                // Append the (Category:Name) key to non-pass findings so
                // the user knows what to pass to /doctor squelch.
                let mut text = format!("{label}\n");
                for check in &checks {
                    text.push_str(&check.format_line());
                    if !matches!(check.status, cortex::doctor::CheckStatus::Pass) {
                        text.push_str(&format!("    key: {}:{}", check.category, check.name));
                    }
                    text.push('\n');
                }
                let result_text = text.trim_end().to_string();
                // Find and replace the last Analyzing cell for this label.
                // Bound the search to un-flushed cells: anything earlier is
                // already in terminal scrollback and must not be mutated.
                let search_start = self.cells_emitted;
                if let Some(rel_pos) = self.cells[search_start..].iter().rposition(|cell| {
                    matches!(cell, HistoryCell::System { text } if text.contains(&format!("Analyzing {label}...")))
                }) {
                    let pos = search_start + rel_pos;
                    debug_assert!(pos >= self.cells_emitted, "mutating flushed cell at {pos}");
                    self.cells[pos] = HistoryCell::System { text: result_text };
                } else {
                    self.push_system_message(result_text);
                }
            }
            DoctorEvent::Done { pass, warn, fail } => {
                let summary = format!("Summary: {pass} passed, {warn} warning(s), {fail} failed");
                self.push_system_message(summary);
                self.doctor_rx = None;
            }
        }
    }

    /// Route a completed `/btw` background task back to the popup. Must not
    /// touch `AppState` or the main agent's streaming state — `/btw` is
    /// explicitly orthogonal to the primary turn.
    pub fn process_btw_result(&mut self, result: BtwResult) {
        // If the popup was already dismissed (user gave up), discard silently
        // rather than re-opening it. Also discard if the question on the
        // popup no longer matches — a newer `/btw` has superseded this one.
        use super::super::btw_popup::BtwState;
        let matches_current = match self.btw_popup.state() {
            BtwState::Loading { question } => question == &result.question,
            _ => false,
        };
        if !matches_current {
            return;
        }
        match result.outcome {
            Ok(answer) => self.btw_popup.show_ready(result.question, answer),
            Err(err) => self.btw_popup.show_error(result.question, err),
        }
        self.btw_cancel = None;
    }

    pub fn process_agent_event(&mut self, event: AgentEvent) {
        self.process_agent_event_inner(event);
        self.flush_after_event();
    }

    fn process_agent_event_inner(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Preparing => self.handle_event_preparing(),
            AgentEvent::TextDelta(delta) => self.handle_event_text_delta(delta),
            AgentEvent::ThinkingDelta(delta) => self.handle_event_thinking_delta(delta),
            AgentEvent::ToolExecuting { name, args, .. } => {
                self.handle_event_tool_executing(name, args)
            }
            AgentEvent::ToolResult { name, result, .. } => {
                self.handle_event_tool_result(name, result)
            }
            AgentEvent::ShellConfirmation { command, respond } => {
                self.handle_event_shell_confirmation(command, respond)
            }
            AgentEvent::Usage(usage) => self.handle_event_usage(usage),
            // Liveness marker for the sub-agent stall watchdog — nothing to
            // render. The spinner already covers "a request is in flight".
            AgentEvent::LlmAttemptStarted { .. } => {}
            AgentEvent::TextRetracted { bytes } => self.handle_event_text_retracted(bytes),
            AgentEvent::LlmReconnecting { delay_secs, .. } => {
                self.handle_event_llm_reconnecting(delay_secs)
            }
            AgentEvent::TurnComplete => self.handle_event_turn_complete(),
            AgentEvent::Error(e) => self.handle_event_error(e),
            AgentEvent::SteerReceived { text } => self.handle_event_steer_received(text),
            AgentEvent::PlanUpdated { steps } => self.handle_event_plan_updated(steps),
            AgentEvent::UserInputRequest { questions, respond } => {
                self.handle_event_user_input_request(questions, respond)
            }
            AgentEvent::SubAgentUpdate { .. } => {
                // Sub-agent updates are informational; no TUI action needed yet.
            }
            AgentEvent::ToolOutputDelta {
                delta, is_stderr, ..
            } => self.handle_event_tool_output_delta(delta, is_stderr),
            AgentEvent::HistoryCompacted {
                dropped,
                before_tokens,
                after_tokens,
                iterative,
            } => {
                let saved = before_tokens.saturating_sub(after_tokens);
                let mode = if iterative { "updated" } else { "new" };
                // Snap the ctx-% gauge to the new history size immediately,
                // rather than waiting for the next Usage event. Without this
                // the footer keeps showing pre-compaction utilization until
                // the next turn lands.
                self.last_prompt_tokens = after_tokens as u64;
                self.push_system_message(format!(
                    "Compacted {dropped} messages ({mode} summary, saved ~{saved} tokens)"
                ));
            }
            AgentEvent::SystemNotice { category, text } => {
                // Daemon-emitted out-of-band notices (posture transitions,
                // budget alerts). Render as a system cell so the user sees
                // them between turns. `category` is informational only — the
                // text already describes what happened.
                let _ = category;
                self.push_system_message(text);
            }
        }
    }

    fn handle_event_preparing(&mut self) {
        self.cells.push(HistoryCell::Thinking {
            text: String::new(),
        });
        self.stream_status.set_header("Preparing");
        self.stream_status.set_details(None);
    }

    /// Name the pause when the connection to the provider drops. The wait is
    /// seconds to a minute long, and the spinner alone cannot distinguish it
    /// from a model that is simply thinking — which is what makes it read as
    /// a hang. Plain words, not an error code: the next action is to wait.
    fn handle_event_llm_reconnecting(&mut self, delay_secs: u64) {
        self.stream_status.set_header("Reconnecting");
        self.stream_status.set_details(Some(format!(
            "waiting for network — retrying in {delay_secs}s"
        )));
    }

    fn handle_event_text_delta(&mut self, delta: String) {
        // Remove empty Thinking placeholder (from Preparing event)
        if matches!(self.cells.last(), Some(HistoryCell::Thinking { text }) if text.is_empty()) {
            self.cells.pop();
        }
        if let Some(HistoryCell::Assistant { text, .. }) = self.cells.last_mut() {
            text.push_str(&delta);
        } else {
            self.cells.push(HistoryCell::assistant(delta, true));
        }
        // First visible assistant token → phase flips to "Responding"; further
        // deltas are cheap string compares so we don't re-allocate.
        if self.stream_status.header != "Responding" {
            self.stream_status.set_header("Responding");
            self.stream_status.set_details(None);
        }
    }

    /// The attempt that streamed this text failed and the full response
    /// comes again. Remove the streamed partial from the assistant cell that
    /// is in progress. A cell that is in the terminal scrollback already
    /// cannot change.
    fn handle_event_text_retracted(&mut self, bytes: usize) {
        let last = self.cells.len().saturating_sub(1);
        if last < self.cells_emitted {
            return;
        }
        let Some(HistoryCell::Assistant { text, .. }) = self.cells.last_mut() else {
            return;
        };
        cortex::agent::retract_text_tail(text, bytes);
        if text.is_empty() {
            self.cells.pop();
        }
    }

    fn handle_event_thinking_delta(&mut self, delta: String) {
        // Check if the second-to-last cell is a Thinking cell (last is the
        // streaming Assistant cell), or if the last cell itself is Thinking.
        let len = self.cells.len();
        let thinking_idx = if len >= 2 {
            if matches!(self.cells[len - 2], HistoryCell::Thinking { .. }) {
                Some(len - 2)
            } else if matches!(self.cells[len - 1], HistoryCell::Thinking { .. }) {
                Some(len - 1)
            } else {
                None
            }
        } else if len == 1 && matches!(self.cells[0], HistoryCell::Thinking { .. }) {
            Some(0)
        } else {
            None
        };

        if let Some(idx) = thinking_idx {
            debug_assert!(idx >= self.cells_emitted, "mutating flushed cell at {idx}");
            if let HistoryCell::Thinking { text, .. } = &mut self.cells[idx] {
                text.push_str(&delta);
            }
        } else {
            // Insert thinking cell before the trailing Assistant cell so
            // text deltas continue appending to the Assistant cell at the end.
            let insert_pos =
                if len > 0 && matches!(self.cells[len - 1], HistoryCell::Assistant { .. }) {
                    len - 1
                } else {
                    len
                };
            self.cells.insert(
                insert_pos,
                HistoryCell::Thinking {
                    text: delta.clone(),
                },
            );
        }
        if self.stream_status.header != "Thinking" {
            self.stream_status.set_header("Thinking");
        }
        self.stream_status.set_details(Some(delta));
    }

    fn handle_event_tool_executing(&mut self, name: String, args: String) {
        // Drop a trailing empty streaming Assistant cell: models sometimes open
        // with a tool call after emitting no text, which would otherwise leave
        // a dangling cell rendering only the streaming cursor `▊`.
        if matches!(self.cells.last(), Some(HistoryCell::Assistant { text, .. }) if text.trim().is_empty())
        {
            self.cells.pop();
        }
        // Status row: "Running <tool>" + a compact args snippet so the user
        // can see *which* call is in flight (e.g. a long-running shell).
        self.stream_status.set_header(format!("Running {name}"));
        self.stream_status
            .set_details(Some(summarize_tool_args(&name, &args)));
        self.cells.push(HistoryCell::ToolStart {
            name,
            args,
            completed: false,
            start_time: Some(Instant::now()),
        });
    }

    fn handle_event_tool_result(&mut self, name: String, result: String) {
        // Mark matching ToolStart as completed and compute duration + display label.
        // Bound the search to un-flushed cells — earlier ToolStarts are already
        // in terminal scrollback as `completed: false` and cannot be mutated.
        let mut duration_ms = None;
        let mut matched_args = None;
        let search_start = self.cells_emitted;
        debug_assert!(search_start <= self.cells.len(), "watermark past end");
        for cell in self.cells[search_start..].iter_mut().rev() {
            if let HistoryCell::ToolStart {
                name: ref start_name,
                args,
                completed,
                start_time,
                ..
            } = cell
            {
                if start_name == &name && !*completed {
                    *completed = true;
                    matched_args = Some(args.clone());
                    if let Some(t) = start_time {
                        duration_ms = Some(t.elapsed().as_millis() as u64);
                    }
                    break;
                }
            }
        }
        let display_label = if let Some(ref args) = matched_args {
            let cat = super::super::tool_display::classify_tool(&name, args);
            super::super::tool_display::tool_result_label(&cat)
        } else {
            format!("Ran {name}")
        };
        // The final ToolResult replaces the live-streaming preview for *this*
        // tool call only. Remove just the most recent ToolStreaming cell so a
        // second tool running concurrently doesn't have its preview wiped.
        // Bound the search to un-flushed cells.
        let search_start = self.cells_emitted;
        if let Some(rel_idx) = self.cells[search_start..]
            .iter()
            .rposition(|c| matches!(c, HistoryCell::ToolStreaming { .. }))
        {
            let pos = search_start + rel_idx;
            debug_assert!(pos >= self.cells_emitted, "removing flushed cell at {pos}");
            self.cells.remove(pos);
        }
        let is_error = result.starts_with("Error:");
        let line_count = result.lines().count();
        let collapsed = line_count > super::super::constants::COLLAPSE_THRESHOLD;
        self.cells.push(HistoryCell::ToolResult {
            output: result,
            is_error,
            duration_ms,
            display_label,
            tool_name: name,
            args_json: matched_args,
            collapsed,
        });
        // Tool finished; clear the "Running X" phase so the next stream event
        // (thinking delta / text delta) decides the new header. If no further
        // event arrives before TurnComplete, "Working" is the honest label.
        self.stream_status.reset();
        // EvolutionHook ran alongside the tool result on the agent side;
        // the cached ambient-status header may now be stale (level, mood,
        // archetype, hint) — refresh off the updated DB state.
        self.refresh_ambient_status();
    }

    fn handle_event_shell_confirmation(&mut self, command: String, respond: oneshot::Sender<bool>) {
        self.cells.push(HistoryCell::ShellApproval {
            command,
            status: ApprovalStatus::Pending,
        });
        self.state = AppState::AwaitingApproval {
            respond: Some(respond),
        };
    }

    fn handle_event_usage(&mut self, usage: cortex::llm::UsageData) {
        self.session_prompt_tokens += usage.prompt_tokens;
        self.session_completion_tokens += usage.completion_tokens;
        // Overwrite (not sum) — `prompt_tokens` is the current context size,
        // not a delta. Drives the footer `ctx %` indicator.
        self.last_prompt_tokens = usage.prompt_tokens;
    }

    fn handle_event_turn_complete(&mut self) {
        let flush_width = if self.last_viewport_width > 0 {
            self.last_viewport_width
        } else {
            DEFAULT_FLUSH_WIDTH
        };
        self.steer_tx = None;
        self.pending_steers.clear();
        // Clean up any leftover empty thinking or assistant placeholders
        // (e.g. an Assistant cell whose text never arrived because the turn
        // produced only a tool call). Only walk un-flushed cells — anything
        // earlier is already in scrollback and must not be removed.
        let split = self.cells_emitted.min(self.cells.len());
        let tail: Vec<HistoryCell> = self.cells.split_off(split);
        let kept_tail: Vec<HistoryCell> = tail
            .into_iter()
            .filter(|c| {
                !matches!(c, HistoryCell::Thinking { text } if text.is_empty())
                    && !matches!(c, HistoryCell::Assistant { text, .. } if text.trim().is_empty())
            })
            .collect();
        self.cells.extend(kept_tail);
        // Finalize any still-streaming Assistant cell in the un-flushed tail.
        for cell in self.cells[split..].iter_mut().rev() {
            if let HistoryCell::Assistant { streaming, .. } = cell {
                *streaming = false;
                break;
            }
        }
        // Turn divider, emitted only after turns that did concrete
        // work (tool calls / edits). Label construction (elapsed/tool-count
        // policy) lives on `HistoryCell::turn_separator`; the handler just
        // measures the turn and hands those numbers over.
        if self.cells[split..].iter().any(HistoryCell::is_tool_flow) {
            let tool_calls = self.cells[split..]
                .iter()
                .filter(|c| matches!(c, HistoryCell::ToolResult { .. }))
                .count();
            let elapsed_secs = match &self.state {
                AppState::Streaming { start } => Some(start.elapsed().as_secs()),
                _ => None,
            };
            self.cells
                .push(HistoryCell::turn_separator(tool_calls, elapsed_secs));
        }
        // Force the entire finalized tail into scrollback now so it cannot
        // disappear from the live viewport before native scrollback owns it.
        self.flush_cell_range_to_scrollback(split, self.cells.len(), flush_width);
        self.last_turn_errored = false;
        self.queue_pause_notified = false;
        self.queue_turn_notification();
        if self.config.conversation.collaboration_mode == CollaborationMode::Plan {
            let pct = self.compute_context_pct();
            let name = self
                .config
                .user
                .agent_name
                .clone()
                .unwrap_or_else(|| "Borg".to_string());
            self.plan_overlay.show(pct, name);
            self.state = AppState::PlanReview;
        } else {
            self.state = AppState::Idle;
        }

        // Auto-TTS hook: when /voice tts is on, speak the assistant's
        // reply through the configured TTS provider. Runs after the
        // turn is fully finalized so the spoken text matches the
        // transcript the user sees. Pauses the active mic during
        // playback to avoid the feedback loop where TTS audio feeds
        // back as the next user message.
        self.maybe_speak_assistant_reply();
    }

    /// Queue a terminal notification for the turn that just ended, when it
    /// ran long enough to matter and the user has not turned it off. Only a
    /// `Streaming` state carries a start time, so a `TurnComplete` seen from
    /// any other state (already idle, plan review) never notifies.
    fn queue_turn_notification(&mut self) {
        let elapsed = match &self.state {
            AppState::Streaming { start } => start.elapsed(),
            _ => return,
        };
        if !notify::should_notify(self.config.ui.notify_on_complete, elapsed) {
            return;
        }
        let name = self.config.user.agent_name.as_deref().unwrap_or("Borg");
        let body = format!(
            "Finished in {}",
            super::duration::fmt_compact_duration(elapsed.as_secs() as i64)
        );
        self.pending_notification =
            notify::notification_sequence(notify::detect_from_process_env(), name, &body);
    }

    /// Hand the queued notification sequence to the event loop, which owns
    /// the terminal handle.
    pub fn take_pending_notification(&mut self) -> Option<String> {
        self.pending_notification.take()
    }

    fn handle_event_error(&mut self, e: String) {
        let flush_width = if self.last_viewport_width > 0 {
            self.last_viewport_width
        } else {
            DEFAULT_FLUSH_WIDTH
        };
        self.cells.push(HistoryCell::System {
            text: cortex::error_format::format_error_with_context(
                &e,
                cortex::error_format::ErrorContext::Tui,
            ),
        });
        // Finalize any still-streaming Assistant cell in the un-flushed tail.
        let split = self.cells_emitted.min(self.cells.len());
        for cell in self.cells[split..].iter_mut().rev() {
            if let HistoryCell::Assistant { streaming, .. } = cell {
                *streaming = false;
                break;
            }
        }
        // Error finalizes the turn; hand the full tail to scrollback now.
        self.flush_cell_range_to_scrollback(split, self.cells.len(), flush_width);
        self.last_turn_errored = true;
        // If a transient Plan flow failed, roll back to the prior mode so the
        // user isn't left trapped with mutations blocked.
        if let Some(prev) = self.previous_collab_mode.take() {
            self.set_mode(prev);
        }
        self.state = AppState::Idle;
    }

    fn handle_event_steer_received(&mut self, text: String) {
        if let Some(pos) = self.pending_steers.iter().position(|s| *s == text) {
            self.pending_steers.remove(pos);
        }
    }

    fn handle_event_plan_updated(&mut self, steps: Vec<cortex::types::PlanStep>) {
        self.plan_steps = steps.clone();
        // Update existing Plan cell in-place, or insert a new one. Bound the
        // search to un-flushed cells so we never mutate scrollback content.
        let search_start = self.cells_emitted;
        let existing = self.cells[search_start..]
            .iter()
            .rposition(|c| matches!(c, HistoryCell::Plan { .. }))
            .map(|rel| search_start + rel);
        if let Some(idx) = existing {
            debug_assert!(idx >= self.cells_emitted, "mutating flushed cell at {idx}");
            self.cells[idx] = HistoryCell::Plan { steps };
        } else {
            self.cells.push(HistoryCell::Plan { steps });
        }
    }

    fn handle_event_user_input_request(
        &mut self,
        questions: Vec<cortex::tool_handlers::user_input::Question>,
        respond: oneshot::Sender<
            std::collections::HashMap<String, cortex::tool_handlers::user_input::Answer>,
        >,
    ) {
        // Mirror the questions (and their options) into history so the prompt
        // stays visible after the carousel closes.
        let mut text = String::new();
        let total = questions.len();
        for (qi, q) in questions.iter().enumerate() {
            if qi > 0 {
                text.push_str("\n\n");
            }
            text.push_str(&format!(
                "[agent asks ({}/{}) {}]: {}",
                qi + 1,
                total,
                q.header,
                q.question
            ));
            for (i, opt) in q.options.iter().enumerate() {
                text.push_str(&format!(
                    "\n  {}. {} — {}",
                    i + 1,
                    opt.label,
                    opt.description
                ));
            }
        }
        self.cells.push(HistoryCell::System { text });
        // Install the BottomPane picker overlay. Empty batches (cortex
        // validates ≥ 1 question, so this is purely defensive) get the
        // responder back via `Err` so we can reply with an empty map
        // rather than silently dropping the channel and stalling the LLM.
        match super::super::bottom_pane::request_user_input::RequestUserInputOverlay::new_with_responder(
            questions,
            respond,
        ) {
            Ok(overlay) => {
                self.bottom_pane.set_view(Box::new(overlay));
            }
            Err(tx) => {
                tracing::warn!("user-input: empty question batch reached TUI; replying empty");
                if tx.send(std::collections::HashMap::new()).is_err() {
                    tracing::warn!("user-input: empty-batch reply receiver already dropped");
                }
            }
        }
    }

    fn handle_event_tool_output_delta(&mut self, delta: String, is_stderr: bool) {
        // Surface the most recent output line as live status details so the
        // user can see streaming tools (e.g. `cargo build`) are actually
        // making progress — not just sitting on an open pipe.
        if let Some(last_line) = delta.lines().rev().find(|l| !l.trim().is_empty()) {
            self.stream_status.set_details(Some(last_line.to_string()));
        }
        // Attach to a ToolStreaming cell at the tail of the history. We only
        // consider it "active" if the most recent cell is ToolStreaming or
        // ToolStart — appending anywhere else would reorder transcript cells
        // (e.g. shove an assistant message down the viewport).
        let target = match self.cells.last() {
            Some(HistoryCell::ToolStreaming { .. }) => Some(self.cells.len() - 1),
            Some(HistoryCell::ToolStart { .. }) => {
                self.cells.push(HistoryCell::ToolStreaming {
                    lines: Vec::new(),
                    dropped: 0,
                });
                Some(self.cells.len() - 1)
            }
            _ => {
                tracing::warn!(
                    "tool output delta with no active ToolStart at tail — dropping {} bytes",
                    delta.len()
                );
                None
            }
        };
        if let Some(idx) = target {
            debug_assert!(idx >= self.cells_emitted, "mutating flushed cell at {idx}");
            if let HistoryCell::ToolStreaming { lines, dropped } = &mut self.cells[idx] {
                let parts: Vec<&str> = delta.split('\n').collect();
                for (i, part) in parts.iter().enumerate() {
                    if i == 0 {
                        match lines.last_mut() {
                            Some((buf, stream)) if *stream == is_stderr => {
                                // Per-line byte clamp: rendering truncates to
                                // viewport width anyway, and an unclamped
                                // newline-free stream grows one String without
                                // bound and re-parses it every frame.
                                let budget = super::super::constants::TOOL_STREAM_MAX_LINE_BYTES
                                    .saturating_sub(buf.len());
                                if budget > 0 {
                                    buf.push_str(clamp_at_char_boundary(part, budget));
                                }
                            }
                            _ => lines.push((
                                clamp_at_char_boundary(
                                    part,
                                    super::super::constants::TOOL_STREAM_MAX_LINE_BYTES,
                                )
                                .to_string(),
                                is_stderr,
                            )),
                        }
                    } else {
                        lines.push((
                            clamp_at_char_boundary(
                                part,
                                super::super::constants::TOOL_STREAM_MAX_LINE_BYTES,
                            )
                            .to_string(),
                            is_stderr,
                        ));
                    }
                }
                // Line-count cap: only the tail is ever rendered live; evict
                // from the front and remember how much so the "(N lines
                // above)" notice stays truthful.
                let cap = super::super::constants::TOOL_STREAM_MAX_LINES;
                if lines.len() > cap {
                    let excess = lines.len() - cap;
                    lines.drain(..excess);
                    *dropped += excess as u64;
                }
            }
        }
    }

    /// Handle the agent event channel closing (agent task finished or panicked).
    pub fn handle_agent_channel_closed(&mut self) {
        self.handle_agent_channel_closed_inner();
        self.flush_after_event();
    }

    fn handle_agent_channel_closed_inner(&mut self) {
        let flush_width = if self.last_viewport_width > 0 {
            self.last_viewport_width
        } else {
            DEFAULT_FLUSH_WIDTH
        };
        self.event_rx = None;
        let split = self.cells_emitted.min(self.cells.len());
        for cell in self.cells[split..].iter_mut().rev() {
            if let HistoryCell::Assistant { streaming, .. } = cell {
                *streaming = false;
                break;
            }
        }
        self.flush_cell_range_to_scrollback(split, self.cells.len(), flush_width);
        // The channel also closes after every finished turn. Only a turn cut
        // off mid-flight is cleaned up here; a finished Plan turn sits in
        // PlanReview, whose overlay decides the mode (Proceed restores it).
        if !matches!(self.state, AppState::Idle | AppState::PlanReview) {
            if let Some(prev) = self.previous_collab_mode.take() {
                self.set_mode(prev);
            }
            // If queued messages exist, treat unexpected close as an error so queue-pause
            // gives the user a chance to resume or clear instead of silently losing input.
            if !self.queued_messages.is_empty() {
                self.last_turn_errored = true;
                self.push_system_message("[agent disconnected — queue paused]".to_string());
            }
            self.state = AppState::Idle;
        }
    }

    pub fn process_heartbeat(&mut self, event: HeartbeatEvent) {
        self.process_heartbeat_inner(event);
        self.flush_after_event();
    }

    fn process_heartbeat_inner(&mut self, event: HeartbeatEvent) {
        match event {
            HeartbeatEvent::Fire => {
                // Fire events are handled by the TUI event loop (runs agent turn).
                // If we receive one here, it means the event loop forwarded it.
            }
            HeartbeatEvent::SchedulerStarted { mode: _mode } => {
                // The heartbeat scheduler now runs in `borgd`; the cli no
                // longer logs scheduler-start activity itself. The daemon
                // emits the corresponding activity_log row via its own
                // bootstrap path.
            }
            HeartbeatEvent::Result(result) => match result {
                HeartbeatResult::Ran { message, .. } => {
                    self.cells.push(HistoryCell::Heartbeat { text: message });
                }
                HeartbeatResult::Skipped { reason } => {
                    match &reason {
                        SkipReason::EmptyResponse | SkipReason::DuplicateResponse => {
                            self.push_system_message(format!(
                                "[heartbeat: nothing to report ({reason})]"
                            ));
                        }
                        SkipReason::QuietHours
                        | SkipReason::NoLlmProvider
                        | SkipReason::InteractiveInputUnavailable => {
                            // Configured-state / non-actionable skips — none is
                            // an error nor something the user can act on from a
                            // toast. Log only. `NoLlmProvider` is the
                            // onboarding-race path: first heartbeat fires before
                            // the ConfigWatcher poll picks up the just-set
                            // provider. `InteractiveInputUnavailable` is the
                            // agent trying to ask a question in a proactive turn
                            // that has no one to answer it.
                            tracing::debug!("Heartbeat skipped: {reason}");
                        }
                    }
                }
                HeartbeatResult::Failed { error } => {
                    self.push_system_message(format!("[heartbeat error: {error}]"));
                }
            },
        }
    }

    /// Drain one voice-loop event. Returns the [`AppAction`] the event
    /// loop should dispatch. A `Transcript` with `voice.auto_submit`
    /// becomes a normal `handle_submit` call so the agent turn runs
    /// through the same persistence path as a typed message — never
    /// duplicate that path.
    pub fn process_voice_event(
        &mut self,
        event: Option<cortex::media::voice_loop::VoiceEvent>,
    ) -> anyhow::Result<super::AppAction> {
        use cortex::media::voice_loop::{VoiceEvent, VoiceStatus};
        let Some(event) = event else {
            // Channel closed — voice loop is gone.
            self.voice_loop = None;
            self.voice_rx = None;
            self.voice_status = None;
            self.voice_single_shot = false;
            return Ok(super::AppAction::Continue);
        };
        match event {
            VoiceEvent::Status(s) => {
                self.voice_status = Some(s);
                if matches!(s, VoiceStatus::Idle) && self.voice_single_shot {
                    // Single-shot finished — release the loop so the
                    // user can `/voice rec` again without a manual off.
                    self.teardown_voice_loop();
                }
                Ok(super::AppAction::Continue)
            }
            VoiceEvent::Transcript(text) => {
                let text = text.trim().to_string();
                if text.is_empty() {
                    return Ok(super::AppAction::Continue);
                }
                // Strip a leading `/` so a transcription that happens to
                // start with that character (Whisper occasionally leaks
                // formatting, or the user literally says "slash …")
                // cannot reach `try_handle_command` and silently fire a
                // slash command the user never typed.
                let text = text.trim_start_matches('/').trim().to_string();
                if text.is_empty() {
                    return Ok(super::AppAction::Continue);
                }
                if self.config.voice.auto_submit {
                    // Echo the transcript as a system breadcrumb so the
                    // user sees what the recognizer heard before the
                    // agent reply starts streaming.
                    self.push_system_message(format!("[voice] {text}"));
                    self.handle_submit(&text)
                } else {
                    // Drop into the composer for review.
                    self.composer.set_text(&text);
                    Ok(super::AppAction::Continue)
                }
            }
            VoiceEvent::SilentLimitReached => {
                self.push_system_message(
                    "voice: no speech detected for several cycles — voice mode disabled."
                        .to_string(),
                );
                self.teardown_voice_loop();
                Ok(super::AppAction::Continue)
            }
            VoiceEvent::Error(e) => {
                self.push_system_message(format!("voice: {e}"));
                Ok(super::AppAction::Continue)
            }
        }
    }

    pub(crate) fn teardown_voice_loop(&mut self) {
        if let Some(vl) = self.voice_loop.take() {
            // Shutdown is async; we run it inline so the recorder is
            // released before the next slash command can fire.
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async move {
                    vl.shutdown().await;
                });
            });
        }
        cortex::media::playback::stop_playback();
        self.voice_rx = None;
        self.voice_status = None;
        self.voice_single_shot = false;
    }

    /// Auto-TTS hook called after every turn completes. Speaks the
    /// finalized assistant message if `voice.auto_tts` is on. When a
    /// voice loop is active, pauses the live recorder for the duration
    /// of playback (TTS-feedback guard) and resumes it afterwards;
    /// without a loop, still synthesizes and plays so `auto_tts` works
    /// as a standalone "speak replies" switch.
    pub(super) fn maybe_speak_assistant_reply(&mut self) {
        if !self.config.voice.auto_tts {
            return;
        }
        let Some(synth) = cortex::media::tts::TtsSynthesizer::from_config(&self.config) else {
            return;
        };
        let Some(text) = self.last_assistant_text() else {
            return;
        };
        let speak = cortex::media::tts::prepare_for_tts(&text, self.config.tts.max_text_length);
        if speak.trim().is_empty() {
            return;
        }
        let synth = std::sync::Arc::new(synth);
        let format_str = self.config.tts.default_format.clone();
        let format = cortex::media::tts::AudioFormat::from_str_lossy(&format_str);
        let voice_loop = self.voice_loop.clone();
        // Sentence-stream the reply: each chunk's synthesis runs in
        // parallel while earlier chunks play in order. Cuts perceived
        // time-to-first-audio dramatically on multi-sentence replies.
        // Min-chars floor (20) avoids one-word synth requests.
        let chunks = cortex::media::tts::chunk_for_streaming_tts(&speak, 20);
        if chunks.is_empty() {
            return;
        }
        tokio::spawn(async move {
            // Fan out the synth calls in order — Vec<JoinHandle<...>> keeps
            // playback order intact even when later chunks finish first.
            let mut pending: Vec<tokio::task::JoinHandle<_>> = Vec::with_capacity(chunks.len());
            for chunk in chunks {
                let synth = synth.clone();
                pending.push(tokio::spawn(async move {
                    synth.synthesize(&chunk, None, format).await
                }));
            }
            // Open the mic-pause window once across the whole reply so
            // the recorder doesn't flicker between chunks.
            if let Some(vl) = &voice_loop {
                vl.notify_tts_started().await;
            }
            for (i, handle) in pending.into_iter().enumerate() {
                let result = match handle.await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(chunk = i, error = %e, "voice auto-tts: synth task panicked");
                        continue;
                    }
                };
                let (bytes, fmt) = match result {
                    Ok((bytes, fmt, _attempts)) => (bytes, fmt),
                    Err(e) => {
                        tracing::warn!(chunk = i, error = %e, "voice auto-tts: synth failed");
                        continue;
                    }
                };
                let path = match write_tmp_audio(&bytes, fmt.extension()) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(chunk = i, error = %e, "voice auto-tts: temp audio write failed");
                        continue;
                    }
                };
                if let Err(e) = cortex::media::playback::play_audio_file(&path).await {
                    tracing::warn!(chunk = i, error = %e, "voice auto-tts playback failed");
                }
                // Playback is done with the temp chunk; a leftover file in tmp is harmless.
                let _ = tokio::fs::remove_file(&path).await;
            }
            if let Some(vl) = &voice_loop {
                vl.notify_tts_finished().await;
            }
        });
    }

    /// Newest finalized assistant cell text in the un-flushed tail. We
    /// look at the just-finalized cell because by the time the
    /// turn-complete event fires it has been pushed and unmarked
    /// `streaming`.
    fn last_assistant_text(&self) -> Option<String> {
        for cell in self.cells.iter().rev() {
            if let HistoryCell::Assistant { text, .. } = cell {
                if !text.trim().is_empty() {
                    return Some(text.clone());
                }
            }
        }
        None
    }
}

/// Truncate `s` to at most `max` bytes without splitting a multi-byte char.
/// Used to clamp retained live-stream lines; slicing mid-char would panic.
pub(super) fn clamp_at_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

fn write_tmp_audio(bytes: &[u8], ext: &str) -> anyhow::Result<std::path::PathBuf> {
    let dir = cortex::media::voice_capture::voice_scratch_dir()?;
    let stamp = chrono::Utc::now().format("%Y%m%d_%H%M%S_%f");
    let path = dir.join(format!("tts_{stamp}.{ext}"));
    // Owner-only, and never through a symlink — synthesized speech is the
    // user's conversation read aloud.
    cortex::util::unix_ext::replace_private(&path, bytes)?;
    Ok(path)
}

/// Pull the most informative short substring out of a tool's raw JSON args for
/// display in the live status row. This is a best-effort summary — the
/// status row is purely informational, so an unparseable blob falls back to
/// the raw arg string (which the details setter will left-ellipsize).
///
/// Explicit field preference by common tool name (`run_shell.command`,
/// `read_file.path`, etc.) beats generic "first string value" extraction
/// because an `apply_patch` invocation's first string is usually the giant
/// `input` blob rather than the target path.
pub(super) fn summarize_tool_args(name: &str, args: &str) -> String {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let parsed: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return trimmed.to_string(),
    };
    let Some(obj) = parsed.as_object() else {
        return trimmed.to_string();
    };

    // Preferred fields per tool — first hit wins.
    const PREFERRED: &[(&str, &[&str])] = &[
        ("run_shell", &["command"]),
        ("read_file", &["path"]),
        ("list_dir", &["path"]),
        ("apply_patch", &["target", "path"]),
        ("write_memory", &["name"]),
        ("read_memory", &["name"]),
        ("web_fetch", &["url"]),
        ("web_search", &["query"]),
        ("browser", &["url", "action"]),
    ];
    let preferred_fields: &[&str] = PREFERRED
        .iter()
        .find_map(|(n, fields)| (*n == name).then_some(*fields))
        .unwrap_or(&[]);
    for field in preferred_fields {
        if let Some(v) = obj.get(*field).and_then(|v| v.as_str()) {
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }

    // Generic fallback: first non-empty string value in the object.
    for (_, v) in obj {
        if let Some(s) = v.as_str() {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    trimmed.to_string()
}
