use std::time::Instant;

use tokio::sync::oneshot;

use borg_core::agent::AgentEvent;
use borg_core::config::CollaborationMode;
use borg_heartbeat::scheduler::{HeartbeatEvent, HeartbeatResult, SkipReason};

use super::super::history::{ApprovalStatus, HistoryCell};
use super::{App, AppState, BtwResult, DoctorEvent};

impl<'a> App<'a> {
    pub fn push_system_message(&mut self, text: String) {
        self.cells.push(HistoryCell::System { text });
    }

    /// Process a doctor event from the async diagnostics task.
    pub fn process_doctor_event(&mut self, event: DoctorEvent) {
        match event {
            DoctorEvent::Analyzing { label } => {
                self.push_system_message(format!("⏳ Analyzing {label}..."));
            }
            DoctorEvent::Result { label, checks } => {
                // Replace the "Analyzing..." cell with actual results
                let mut text = format!("{label}\n");
                for check in &checks {
                    text.push_str(&check.format_line());
                    text.push('\n');
                }
                let result_text = text.trim_end().to_string();
                // Find and replace the last Analyzing cell for this label
                if let Some(pos) = self.cells.iter().rposition(|cell| {
                    matches!(cell, HistoryCell::System { text } if text.contains(&format!("Analyzing {label}...")))
                }) {
                    self.cells[pos] = HistoryCell::System { text: result_text };
                } else {
                    self.push_system_message(result_text);
                }
            }
            DoctorEvent::Done { pass, warn, fail } => {
                self.push_system_message(format!(
                    "Summary: {pass} passed, {warn} warning(s), {fail} failed"
                ));
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
        match event {
            AgentEvent::Preparing => self.handle_event_preparing(),
            AgentEvent::TextDelta(delta) => self.handle_event_text_delta(delta),
            AgentEvent::ThinkingDelta(delta) => self.handle_event_thinking_delta(delta),
            AgentEvent::ToolExecuting { name, args } => {
                self.handle_event_tool_executing(name, args)
            }
            AgentEvent::ToolResult { name, result } => self.handle_event_tool_result(name, result),
            AgentEvent::ShellConfirmation { command, respond } => {
                self.handle_event_shell_confirmation(command, respond)
            }
            AgentEvent::Usage(usage) => self.handle_event_usage(usage),
            AgentEvent::TurnComplete => self.handle_event_turn_complete(),
            AgentEvent::Error(e) => self.handle_event_error(e),
            AgentEvent::SteerReceived { text } => self.handle_event_steer_received(text),
            AgentEvent::PlanUpdated { steps } => self.handle_event_plan_updated(steps),
            AgentEvent::UserInputRequest {
                prompt,
                choices,
                allow_custom,
                respond,
            } => self.handle_event_user_input_request(prompt, choices, allow_custom, respond),
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
                self.push_system_message(format!(
                    "Compacted {dropped} messages ({mode} summary, saved ~{saved} tokens)"
                ));
            }
        }
    }

    fn handle_event_preparing(&mut self) {
        self.cells.push(HistoryCell::Thinking {
            text: String::new(),
        });
        self.stream_status.set_header("Preparing");
        self.stream_status.set_details(None);
        if self.auto_scroll {
            self.scroll_offset = 0;
        }
    }

    fn handle_event_text_delta(&mut self, delta: String) {
        // Remove empty Thinking placeholder (from Preparing event)
        if matches!(self.cells.last(), Some(HistoryCell::Thinking { text }) if text.is_empty()) {
            self.cells.pop();
        }
        if let Some(HistoryCell::Assistant { text, .. }) = self.cells.last_mut() {
            text.push_str(&delta);
        } else {
            self.cells.push(HistoryCell::Assistant {
                text: delta,
                streaming: true,
            });
        }
        // First visible assistant token → phase flips to "Responding"; further
        // deltas are cheap string compares so we don't re-allocate.
        if self.stream_status.header != "Responding" {
            self.stream_status.set_header("Responding");
            self.stream_status.set_details(None);
        }
        if self.auto_scroll {
            self.scroll_offset = 0;
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
        if self.auto_scroll {
            self.scroll_offset = 0;
        }
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
        if self.auto_scroll {
            self.scroll_offset = 0;
        }
    }

    fn handle_event_tool_result(&mut self, name: String, result: String) {
        // Mark matching ToolStart as completed and compute duration + display label
        let mut duration_ms = None;
        let mut matched_args = None;
        for cell in self.cells.iter_mut().rev() {
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
        let is_error = result.starts_with("Error:");
        let line_count = result.lines().count();
        let collapsed = line_count > super::super::history::COLLAPSE_THRESHOLD;
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
        if self.auto_scroll {
            self.scroll_offset = 0;
        }
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
        if self.auto_scroll {
            self.scroll_offset = 0;
        }
    }

    fn handle_event_usage(&mut self, usage: borg_core::llm::UsageData) {
        self.session_prompt_tokens += usage.prompt_tokens;
        self.session_completion_tokens += usage.completion_tokens;
    }

    fn handle_event_turn_complete(&mut self) {
        // Clean up steer channel on turn completion
        self.steer_tx = None;
        self.pending_steers.clear();
        // Clean up any leftover empty thinking or assistant placeholders
        // (e.g. an Assistant cell whose text never arrived because the turn
        // produced only a tool call).
        self.cells.retain(|c| {
            !matches!(c, HistoryCell::Thinking { text } if text.is_empty())
                && !matches!(c, HistoryCell::Assistant { text, .. } if text.trim().is_empty())
        });
        for cell in self.cells.iter_mut().rev() {
            if let HistoryCell::Assistant { streaming, .. } = cell {
                *streaming = false;
                break;
            }
        }
        self.last_turn_errored = false;
        self.queue_pause_notified = false;
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
    }

    fn handle_event_error(&mut self, e: String) {
        self.cells.push(HistoryCell::System {
            text: borg_core::error_format::format_error_with_context(
                &e,
                borg_core::error_format::ErrorContext::Tui,
            ),
        });
        for cell in self.cells.iter_mut().rev() {
            if let HistoryCell::Assistant { streaming, .. } = cell {
                *streaming = false;
                break;
            }
        }
        self.last_turn_errored = true;
        // If a transient Plan flow failed, roll back to the prior mode so the
        // user isn't left trapped with mutations blocked.
        if let Some(prev) = self.previous_collab_mode.take() {
            self.config.conversation.collaboration_mode = prev;
        }
        self.state = AppState::Idle;
    }

    fn handle_event_steer_received(&mut self, text: String) {
        // Remove matching steer from pending
        if let Some(pos) = self.pending_steers.iter().position(|s| *s == text) {
            self.pending_steers.remove(pos);
        }
    }

    fn handle_event_plan_updated(&mut self, steps: Vec<borg_core::types::PlanStep>) {
        self.plan_steps = steps.clone();
        // Update existing Plan cell in-place, or insert a new one
        let existing = self
            .cells
            .iter()
            .rposition(|c| matches!(c, HistoryCell::Plan { .. }));
        if let Some(idx) = existing {
            self.cells[idx] = HistoryCell::Plan { steps };
        } else {
            self.cells.push(HistoryCell::Plan { steps });
        }
        if self.auto_scroll {
            self.scroll_offset = 0;
        }
    }

    fn handle_event_user_input_request(
        &mut self,
        prompt: String,
        choices: Vec<borg_core::tool_handlers::user_input::UserInputChoice>,
        allow_custom: bool,
        respond: oneshot::Sender<String>,
    ) {
        // Show prompt (and choices, if any) in history so they remain visible after answering.
        let mut text = format!("[agent asks: {prompt}]");
        if !choices.is_empty() {
            for (i, c) in choices.iter().enumerate() {
                let desc = c
                    .description
                    .as_deref()
                    .map(|d| format!(" — {d}"))
                    .unwrap_or_default();
                text.push_str(&format!("\n  {}. {}{}", i + 1, c.label, desc));
            }
        }
        self.cells.push(HistoryCell::System { text });
        // If choices provided but allow_custom is false, start in selection mode (custom_mode=false).
        // If no choices, the free-text path is the only option (custom_mode=true acts as free-text).
        let custom_mode = choices.is_empty();
        self.state = AppState::AwaitingInput {
            prompt,
            choices,
            cursor: 0,
            custom_mode,
            allow_custom,
            respond: Some(respond),
        };
        if self.auto_scroll {
            self.scroll_offset = 0;
        }
    }

    fn handle_event_tool_output_delta(&mut self, delta: String, is_stderr: bool) {
        // Surface the most recent output line as live status details so the
        // user can see streaming tools (e.g. `cargo build`) are actually
        // making progress — not just sitting on an open pipe.
        if let Some(last_line) = delta.lines().rev().find(|l| !l.trim().is_empty()) {
            self.stream_status.set_details(Some(last_line.to_string()));
        }
        if let Some(HistoryCell::ToolStreaming { lines, .. }) = self.cells.last_mut() {
            lines.push((delta, is_stderr));
        } else {
            self.cells.push(HistoryCell::ToolStreaming {
                lines: vec![(delta, is_stderr)],
            });
        }
        if self.auto_scroll {
            self.scroll_offset = 0;
        }
    }

    /// Handle the agent event channel closing (agent task finished or panicked).
    pub fn handle_agent_channel_closed(&mut self) {
        self.event_rx = None;
        for cell in self.cells.iter_mut().rev() {
            if let HistoryCell::Assistant { streaming, .. } = cell {
                *streaming = false;
                break;
            }
        }
        // If a transient Plan flow was in flight, roll back to the prior mode.
        if let Some(prev) = self.previous_collab_mode.take() {
            self.config.conversation.collaboration_mode = prev;
        }
        if !matches!(self.state, AppState::Idle) {
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
        match event {
            HeartbeatEvent::Fire => {
                // Fire events are handled by the TUI event loop (runs agent turn).
                // If we receive one here, it means the event loop forwarded it.
            }
            HeartbeatEvent::SchedulerStarted { mode } => {
                if let Ok(db) = borg_core::db::Database::open() {
                    borg_core::activity_log::log_activity(
                        &db,
                        "info",
                        "heartbeat",
                        &format!("Heartbeat scheduler started ({mode})"),
                    );
                }
            }
            HeartbeatEvent::Result(result) => match result {
                HeartbeatResult::Ran { message, .. } => {
                    self.cells.push(HistoryCell::Heartbeat { text: message });
                    if self.auto_scroll {
                        self.scroll_offset = 0;
                    }
                }
                HeartbeatResult::Skipped { reason } => {
                    match &reason {
                        SkipReason::EmptyResponse | SkipReason::DuplicateResponse => {
                            self.push_system_message(format!(
                                "[heartbeat: nothing to report ({reason})]"
                            ));
                        }
                        SkipReason::QuietHours => {
                            // Don't spam the TUI with quiet hours skips
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
