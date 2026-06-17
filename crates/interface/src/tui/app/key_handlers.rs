//! Keyboard input handlers for each `AppState`.
//!
//! All key routing lives here. The public entry point `App::handle_key`
//! dispatches to the per-state handler, which in turn delegates to scroll
//! primitives and small extracted helpers (`scroll_up_by`, `scroll_to_bottom`,
//! `cycle_collaboration_mode`, etc.) also defined in this file.
//!
//! ⚠ Mouse tracking is intentionally disabled at the terminal level
//! (see `tui/mod.rs` — `EnableAlternateScroll`). Wheel events arrive as
//! `KeyCode::Up`/`KeyCode::Down`. See the Rule A/B/C comment inside
//! `handle_key_idle` — do NOT collapse those tiers.

use anyhow::Result;

use cortex::config::CollaborationMode;

use super::super::history::{ApprovalStatus, HistoryCell};
use super::super::plan_overlay::PlanOption;
use super::super::transcript_pager::PagerAction;
use super::{
    extract_at_query, try_paste_clipboard_image, App, AppAction, AppState, BacktrackPhase,
    QueuedMessage,
};

/// True for a bare Ctrl+Z (the conventional shell suspend chord). Other
/// modifier combinations (Ctrl+Shift+Z, Ctrl+Alt+Z) are deliberately excluded
/// so a future binding can claim them without colliding with suspend.
pub(crate) fn is_suspend_key(key: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    key.code == KeyCode::Char('z')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::SHIFT)
        && !key.modifiers.contains(KeyModifiers::ALT)
}

impl<'a> App<'a> {
    // The terminal owns scrollback in inline-viewport mode (xterm Alternate
    // Scroll Mode + `insert_history_lines`). The app no longer maintains a
    // scroll model, so there are no scroll primitives here — wheel/trackpad
    // events scroll the terminal natively, and Up/Down route to composer
    // history.

    /// Enter backtrack-selection mode if there are any past user messages.
    /// Returns `true` when the mode was entered.
    fn try_enter_backtrack(&mut self) -> bool {
        let user_indices: Vec<usize> = self
            .cells
            .iter()
            .enumerate()
            .filter(|(_, c)| matches!(c, HistoryCell::User { .. }))
            .map(|(i, _)| i)
            .collect();
        if user_indices.is_empty() {
            return false;
        }
        self.backtrack = BacktrackPhase::Selecting {
            user_message_indices: user_indices,
            cursor: 0, // 0 = most recent
        };
        true
    }

    /// Cycle collaboration mode: Execute ↔ Plan.
    /// Tracks entering/leaving Plan so the post-turn review overlay can
    /// restore the user's prior mode on "Proceed".
    fn cycle_collaboration_mode(&mut self) {
        let current = self.config.conversation.collaboration_mode;
        let next = match current {
            CollaborationMode::Execute => CollaborationMode::Plan,
            CollaborationMode::Plan => CollaborationMode::Execute,
        };
        if next == CollaborationMode::Plan {
            self.previous_collab_mode = Some(current);
        } else {
            self.previous_collab_mode = None;
        }
        self.config.conversation.collaboration_mode = next;
        self.push_system_message(format!("[mode: {next}]"));
        self.toast_info(format!("Mode → {next}"));
    }

    // =========================================================================
    // NO MOUSE HANDLER.
    // =========================================================================
    // We do not enable any mouse tracking mode (see tui::mod::EnableAlternateScroll),
    // so the event loop never dispatches crossterm mouse events to us. This is
    // what keeps native click+drag text selection working in all terminals with
    // no modifier keys.
    //
    // Mouse-wheel scroll still works because xterm Alternate Scroll Mode
    // (?1007h) makes the terminal translate wheel events into CUR_UP / CUR_DOWN
    // key sequences. Those arrive as KeyCode::Up / KeyCode::Down and are routed
    // by `handle_key_idle` via a three-tier heuristic: transcript scroll when
    // already scrolled up or composer is idle, composer history when the
    // composer has text or is browsing history.
    //
    // DO NOT add `fn handle_mouse`. DO NOT match `Event::Mouse` in tui/mod.rs.
    // Either would require turning on a mouse tracking mode, which breaks text
    // selection. Source-guard tests in `tui/mod.rs` enforce this.
    // =========================================================================

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Result<AppAction> {
        // REPORT_EVENT_TYPES (kitty keyboard protocol, enabled in
        // tui_lifecycle::set_modes) delivers both Press and Release for every
        // key. Forwarding Release would double-fire Enter/Esc handling — e.g.
        // the Enter that submits `/uninstall` would immediately re-trigger as
        // "cancel" inside ConfirmingUninstall. Only Press (and Repeat) reach
        // the per-state handlers below.
        if key.kind == crossterm::event::KeyEventKind::Release {
            return Ok(AppAction::Continue);
        }
        // BottomPane host takes priority: when a modal view (e.g. the
        // request_user_input picker overlay) is active, route every key
        // there first. The host returns `true` when a view consumed the
        // key, in which case the App-level state handlers below stay out
        // of it. Ctrl-C is routed via `on_ctrl_c` so the App can decide
        // whether to ALSO cancel the in-flight turn after the overlay
        // dismisses itself.
        if self.bottom_pane.is_active() {
            use crossterm::event::{KeyCode, KeyModifiers};
            let is_ctrl_c =
                key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
            if is_ctrl_c {
                // Let the view handle it (mark itself done with interrupt
                // markers); then we also tear down the in-flight turn,
                // matching the legacy `handle_key_awaiting_input` Ctrl-C
                // semantics. on_ctrl_c returning Handled is the signal we
                // need to also kill the turn.
                let outcome = self.bottom_pane.on_ctrl_c();
                if outcome == super::super::bottom_pane::CancellationEvent::Handled {
                    if let Some(token) = self.cancel_token.take() {
                        token.cancel();
                    }
                    self.event_rx = None;
                    self.steer_tx = None;
                    self.composer.set_text("");
                    self.cells.push(HistoryCell::System {
                        text: "[interrupted]".to_string(),
                    });
                    self.state = AppState::Idle;
                }
                return Ok(AppAction::Continue);
            }
            // Normal key path: dispatch to active view. After dispatch,
            // the host may have dropped the view (overlay submitted or
            // Esc-cancelled). In either case we resume the streaming
            // surface — the responder oneshot was already consumed
            // inside the overlay's submit/cancel paths.
            self.bottom_pane.handle_key_event(key);
            if !self.bottom_pane.is_active() {
                self.composer.set_text("");
                self.resume_streaming();
            }
            return Ok(AppAction::Continue);
        }
        // Ctrl+Z suspends to the shell — but only from an inline surface. While
        // a popup or alt-screen view owns the screen, restore/resume would fight
        // that surface's own terminal state, so we leave Ctrl+Z to it.
        if is_suspend_key(&key) && !self.any_popup_visible() && !self.wants_alt_screen() {
            return Ok(AppAction::Suspend);
        }
        match &self.state {
            AppState::ConfirmingUninstall => self.handle_key_confirming_uninstall(key),
            AppState::AwaitingApproval { .. } => self.handle_key_awaiting_approval(key),
            AppState::PlanReview => self.handle_key_plan_review(key),
            AppState::TranscriptPager => self.handle_key_transcript_pager(key),
            AppState::Streaming { .. } => self.handle_key_streaming(key),
            AppState::Idle => self.handle_key_idle(key),
        }
    }

    fn handle_key_confirming_uninstall(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> Result<AppAction> {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                return Ok(AppAction::Uninstall);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Enter => {
                self.push_system_message("Uninstall cancelled.".to_string());
                self.state = AppState::Idle;
            }
            _ => {}
        }
        Ok(AppAction::Continue)
    }

    fn handle_key_awaiting_approval(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> Result<AppAction> {
        use crossterm::event::KeyCode;
        if let AppState::AwaitingApproval { respond } = &mut self.state {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if let Some(tx) = respond.take() {
                        let _ = tx.send(true);
                    }
                    if let Some(HistoryCell::ShellApproval { status, .. }) = self.cells.last_mut() {
                        *status = ApprovalStatus::Approved;
                    }
                    self.resume_streaming();
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    if let Some(tx) = respond.take() {
                        let _ = tx.send(false);
                    }
                    if let Some(HistoryCell::ShellApproval { status, .. }) = self.cells.last_mut() {
                        *status = ApprovalStatus::Denied;
                    }
                    self.resume_streaming();
                }
                _ => {}
            }
        }
        Ok(AppAction::Continue)
    }

    fn handle_key_plan_review(&mut self, key: crossterm::event::KeyEvent) -> Result<AppAction> {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::BackTab => {
                self.plan_overlay.cycle();
            }
            KeyCode::Char('1') => {
                self.plan_overlay.select(PlanOption::ClearAndProceed);
            }
            KeyCode::Char('2') => {
                self.plan_overlay.select(PlanOption::ProceedWithContext);
            }
            KeyCode::Char('3') => {
                self.plan_overlay.select(PlanOption::TypeFeedback);
            }
            KeyCode::Enter => {
                let selected = self.plan_overlay.selected();
                self.plan_overlay.dismiss();
                self.state = AppState::Idle;
                match selected {
                    PlanOption::ClearAndProceed => {
                        return Ok(AppAction::PlanProceed {
                            clear_context: true,
                        });
                    }
                    PlanOption::ProceedWithContext => {
                        return Ok(AppAction::PlanProceed {
                            clear_context: false,
                        });
                    }
                    PlanOption::TypeFeedback => {
                        // Dismiss to idle so user can type feedback
                    }
                }
            }
            KeyCode::Esc => {
                self.plan_overlay.dismiss();
                self.state = AppState::Idle;
            }
            _ => {}
        }
        Ok(AppAction::Continue)
    }

    fn handle_key_transcript_pager(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> Result<AppAction> {
        match self.transcript_pager.handle_key(key) {
            PagerAction::Dismiss => {
                self.transcript_pager.dismiss();
                self.state = AppState::Idle;
                return Ok(AppAction::LeaveAltScreen);
            }
            PagerAction::None => {}
        }
        Ok(AppAction::Continue)
    }

    fn handle_key_streaming(&mut self, key: crossterm::event::KeyEvent) -> Result<AppAction> {
        use crossterm::event::{KeyCode, KeyModifiers};
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            // Cancel the in-flight agent task via token
            if let Some(token) = self.cancel_token.take() {
                token.cancel();
            }
            self.event_rx = None;
            self.steer_tx = None;
            self.pending_steers.clear();
            for cell in self.cells.iter_mut().rev() {
                if let HistoryCell::Assistant { streaming, .. } = cell {
                    *streaming = false;
                    break;
                }
            }
            // Restore queued messages to composer instead of discarding
            if !self.queued_messages.is_empty() {
                let mut dropped_images = 0usize;
                let mut messages: Vec<QueuedMessage> = self.queued_messages.drain(..).collect();
                let queued_text: String = messages
                    .iter()
                    .map(|qm| qm.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let current = self.composer.text();
                let restored = if current.trim().is_empty() {
                    queued_text
                } else {
                    format!("{}\n{}", current.trim(), queued_text)
                };
                self.composer.set_text(&restored);
                // Restore images if only one queued message had them
                if messages.len() == 1 {
                    let qm = messages.remove(0);
                    if !qm.images.is_empty() {
                        self.composer.set_image_attachments(qm.images);
                    }
                } else {
                    for qm in &messages {
                        dropped_images += qm.images.len();
                    }
                }
                if dropped_images > 0 {
                    self.cells.push(HistoryCell::System {
                        text: format!(
                            "[interrupted — queued messages restored to composer ({dropped_images} image{} discarded)]",
                            if dropped_images == 1 { "" } else { "s" }
                        ),
                    });
                } else {
                    self.cells.push(HistoryCell::System {
                        text: "[interrupted — queued messages restored to composer]".to_string(),
                    });
                }
            } else {
                self.cells.push(HistoryCell::System {
                    text: "[interrupted]".to_string(),
                });
            }
            self.previous_collab_mode = None;
            self.state = AppState::Idle;
        } else if key.code == KeyCode::Up && key.modifiers.contains(KeyModifiers::ALT) {
            // Pop last queued message back into composer for editing
            if let Some(qm) = self.queued_messages.pop_back() {
                self.composer.set_text(&qm.text);
                self.composer.set_image_attachments(qm.images);
                // Remove the User + System cells that were added when it was queued
                let len = self.cells.len();
                if len >= 2
                    && matches!(self.cells[len - 1], HistoryCell::System { .. })
                    && matches!(self.cells[len - 2], HistoryCell::User { .. })
                {
                    self.cells.truncate(len - 2);
                }
            }
        } else if key.code == KeyCode::Enter {
            // Send as a steer (mid-turn injection at tool boundary)
            let text = self.composer.text().trim().to_string();
            // Intercept /cancel mid-stream so it doesn't get sent to
            // the agent as a steer. Equivalent to pressing Esc.
            if matches!(text.as_str(), "/cancel" | "/stop" | "/abort") {
                self.composer.set_text("");
                if let Some(token) = self.cancel_token.take() {
                    token.cancel();
                }
                self.event_rx = None;
                self.steer_tx = None;
                self.pending_steers.clear();
                for cell in self.cells.iter_mut().rev() {
                    if let HistoryCell::Assistant { streaming, .. } = cell {
                        *streaming = false;
                        break;
                    }
                }
                self.cells.push(HistoryCell::System {
                    text: "[cancelled]".to_string(),
                });
                self.previous_collab_mode = None;
                self.state = AppState::Idle;
                return Ok(AppAction::Continue);
            }
            if !text.is_empty() {
                if let Some(ref steer_tx) = self.steer_tx {
                    let _ = steer_tx.send(text.clone());
                    self.cells.push(HistoryCell::User { text: text.clone() });
                    self.cells.push(HistoryCell::System {
                        text: "[steer queued — will be sent at next tool boundary]".to_string(),
                    });
                    self.pending_steers.push_back(text);
                } else {
                    // Fallback: queue normally if no steer channel.
                    // The message is shown only in the queue preview widget
                    // until the agent finishes; on drain it gets pushed
                    // to scrollback as a normal user cell.
                    let images = self.composer.take_image_attachments();
                    self.queued_messages
                        .push_back(QueuedMessage { text, images });
                }
                self.composer.set_text("");
            }
        } else if key.code == KeyCode::Char('e') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.toggle_last_collapsible();
        } else if key.code == KeyCode::Tab {
            // No-op during streaming
        } else if self.any_popup_visible() {
            // A popup can be open during Streaming if
            // drain_queued_if_idle started a turn while a popup was
            // visible. Route keys to the popup, not the composer.
            // Discard the returned action — we only need Continue during streaming.
            let _ = self.dispatch_key_to_popup(key)?;
        } else {
            // Pass other keys to composer so user can type ahead
            self.composer.handle_key(key);
        }
        Ok(AppAction::Continue)
    }

    fn handle_key_idle(&mut self, key: crossterm::event::KeyEvent) -> Result<AppAction> {
        use crossterm::event::{KeyCode, KeyModifiers};

        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(AppAction::Quit);
        }

        // Popups get first priority for all key events (including Esc)
        if let Some(action) = self.dispatch_key_to_popup(key)? {
            return Ok(action);
        }

        // Reverse-incremental history search owns every key while active,
        // ahead of Esc/scroll/shortcut routing. Ctrl+C still quits because
        // it was already matched above.
        if self.composer.is_searching() {
            if let Some(text) = self.composer.handle_key(key) {
                return self.handle_submit(&text);
            }
            return Ok(AppAction::Continue);
        }

        // Handle backtrack mode (selecting a past user message to rewind to)
        if let Some(action) = self.handle_backtrack_key(key)? {
            return Ok(action);
        }

        // Handle error-paused queue: Enter resumes, Esc clears
        if self.last_turn_errored && !self.queued_messages.is_empty() {
            match key.code {
                KeyCode::Enter => {
                    self.last_turn_errored = false;
                    self.queue_pause_notified = false;
                    if let Some(qm) = self.queued_messages.pop_front() {
                        return self.handle_queued_submit(qm);
                    }
                    return Ok(AppAction::Continue);
                }
                KeyCode::Esc => {
                    self.last_turn_errored = false;
                    self.queue_pause_notified = false;
                    self.queued_messages.clear();
                    self.push_system_message("[queue cleared]".to_string());
                    return Ok(AppAction::Continue);
                }
                _ => {}
            }
        }

        // Esc with empty composer enters backtrack mode
        if key.code == KeyCode::Esc
            && self.composer.is_empty()
            && !self.last_turn_errored
            && self.try_enter_backtrack()
        {
            return Ok(AppAction::Continue);
        }

        // Ctrl+L — clear visual transcript
        if key.code == KeyCode::Char('l') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.cells.clear();
            self.cells_emitted = 0;
            self.pending_scrollback.clear();
            self.banner_emitted = false;
            self.toast_info("Transcript cleared");
            return Ok(AppAction::ClearTranscript);
        }

        // Ctrl+T — open full-screen transcript pager (scrollable + searchable)
        if key.code == KeyCode::Char('t') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.transcript_pager.show();
            self.state = AppState::TranscriptPager;
            return Ok(AppAction::EnterAltScreen);
        }

        // Ctrl+B / Ctrl+F / Ctrl+U intentionally fall through — the terminal
        // owns scrollback in inline-viewport mode, so app-level page scrolls
        // are no-ops. Trackpad/wheel scrolls the terminal pane natively.

        // Ctrl+D — quit when composer is empty (EOF)
        if key.code == KeyCode::Char('d')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && self.composer.is_empty()
        {
            return Ok(AppAction::Quit);
        }

        // Ctrl+G — launch external editor
        if key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(AppAction::LaunchExternalEditor);
        }

        // Ctrl+E — toggle collapse on the most recent expandable tool result
        if key.code == KeyCode::Char('e')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && self.toggle_last_collapsible()
        {
            return Ok(AppAction::Continue);
        }

        // Ctrl+V — paste clipboard image (fall through to normal paste if no image)
        if key.code == KeyCode::Char('v')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && try_paste_clipboard_image(&mut self.composer)
        {
            return Ok(AppAction::Continue);
        }

        // Shift+Tab — cycle collaboration mode (default → execute → plan → default)
        if key.code == KeyCode::BackTab {
            self.cycle_collaboration_mode();
            return Ok(AppAction::Continue);
        }

        // Voice push-to-talk hotkey — matches `voice.record_key`
        // (default `ctrl-b`). Fires a single-shot
        // recording; VAD silence detection auto-stops and transcribes.
        // Press-to-toggle rather than hold-to-record so it works on
        // terminals that don't emit KeyEventKind::Release reliably.
        // Only active when `voice.enabled` so the default Ctrl+B
        // doesn't shadow terminal page-back for users not using voice.
        if self.config.voice.enabled && matches_voice_record_key(key, &self.config.voice.record_key)
        {
            return Ok(self.cmd_voice("rec").unwrap_or(AppAction::Continue));
        }

        if let Some(action) = self.handle_command_popup_key(key)? {
            return Ok(action);
        }
        if let Some(action) = self.handle_file_popup_key(key)? {
            return Ok(action);
        }

        // Up / Down / Page / Home / End fall through to the composer.
        // Inline-viewport mode delegates transcript scrolling to the
        // terminal (xterm Alternate Scroll Mode). The composer's
        // history navigation handles Up/Down; PageUp/PageDown/Home/End
        // are no-ops at the App level — the terminal owns scrolling.
        // Ctrl+P / Ctrl+N remain the explicit history-recall keys.

        if let Some(text) = self.composer.handle_key(key) {
            return self.handle_submit(&text);
        }
        // Update popup filters after normal key input, unless a history
        // search is now active — the preview text isn't a user query and
        // shouldn't open `/` or `@` popups.
        if !self.composer.is_searching() {
            let text = self.composer.text();
            self.command_popup.update_filter(&text);
            if !self.command_popup.is_visible() {
                if let Some(q) = extract_at_query(&text) {
                    self.file_popup.set_pending_query(&q);
                    self.file_search.on_query(&q);
                } else {
                    self.file_popup.dismiss();
                }
            }
        }
        Ok(AppAction::Continue)
    }

    /// Handle keys during backtrack mode (selecting a past user message to rewind to).
    /// Returns `Some(action)` if handled, `None` if backtrack is inactive.
    fn handle_backtrack_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> Result<Option<AppAction>> {
        use crossterm::event::KeyCode;

        if let BacktrackPhase::Selecting {
            ref user_message_indices,
            ref mut cursor,
        } = self.backtrack
        {
            match key.code {
                KeyCode::Up => {
                    if *cursor + 1 < user_message_indices.len() {
                        *cursor += 1;
                    }
                }
                KeyCode::Down => {
                    if *cursor > 0 {
                        *cursor -= 1;
                    }
                }
                KeyCode::Enter => {
                    let indices = user_message_indices.clone();
                    let cur = *cursor;
                    let cell_idx = indices[indices.len() - 1 - cur];
                    let text = if let HistoryCell::User { text } = &self.cells[cell_idx] {
                        text.clone()
                    } else {
                        String::new()
                    };
                    let nth = self.cells[..=cell_idx]
                        .iter()
                        .filter(|c| matches!(c, HistoryCell::User { .. }))
                        .count()
                        - 1;
                    self.backtrack = BacktrackPhase::Inactive;
                    self.cells.truncate(cell_idx);
                    self.composer.set_text(&text);
                    return Ok(Some(AppAction::RewindTo {
                        nth_user_message: nth,
                    }));
                }
                KeyCode::Esc => {
                    self.backtrack = BacktrackPhase::Inactive;
                }
                _ => {}
            }
            return Ok(Some(AppAction::Continue));
        }
        Ok(None)
    }

    /// Handle keys when the command popup is visible.
    /// Returns `Some(action)` if handled, `None` if popup not visible.
    fn handle_command_popup_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> Result<Option<AppAction>> {
        use crossterm::event::KeyCode;

        if !self.command_popup.is_visible() {
            return Ok(None);
        }
        match key.code {
            KeyCode::Up => {
                self.command_popup.move_up();
            }
            KeyCode::Down => {
                self.command_popup.move_down();
            }
            KeyCode::Tab => {
                if let Some(cmd) = self.command_popup.selected_command() {
                    let name = cmd.name.to_string();
                    self.composer.set_text(&name);
                    self.command_popup.dismiss();
                }
            }
            KeyCode::Enter => {
                if let Some(cmd) = self.command_popup.selected_command() {
                    let name = cmd.name.to_string();
                    self.composer.set_text("");
                    self.command_popup.dismiss();
                    return Ok(Some(self.handle_submit(&name)?));
                }
                let text = self.composer.text().trim().to_string();
                self.composer.set_text("");
                self.command_popup.dismiss();
                if text.is_empty() {
                    return Ok(Some(AppAction::Continue));
                }
                return Ok(Some(self.handle_submit(&text)?));
            }
            KeyCode::Esc => {
                self.command_popup.dismiss();
                self.composer.set_text("");
            }
            _ => {
                if let Some(text) = self.composer.handle_key(key) {
                    self.command_popup.dismiss();
                    return Ok(Some(self.handle_submit(&text)?));
                }
                let text = self.composer.text();
                self.command_popup.update_filter(&text);
            }
        }
        Ok(Some(AppAction::Continue))
    }

    /// Handle keys when the file popup is visible.
    /// Returns `Some(action)` if handled, `None` if popup not visible.
    fn handle_file_popup_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> Result<Option<AppAction>> {
        use crossterm::event::KeyCode;

        if !self.file_popup.is_visible() {
            return Ok(None);
        }
        match key.code {
            KeyCode::Up => {
                self.file_popup.move_up();
            }
            KeyCode::Down => {
                self.file_popup.move_down();
            }
            KeyCode::Tab | KeyCode::Enter => {
                if let Some(file) = self.file_popup.selected_file() {
                    let display = file.display.clone();
                    let path = file.full_path.clone();
                    let is_dir = file.is_dir;
                    if is_dir {
                        self.composer.set_partial_mention(display);
                        let text = self.composer.text();
                        if let Some(q) = extract_at_query(&text) {
                            self.file_popup.set_pending_query(&q);
                            self.file_search.on_query(&q);
                        } else {
                            self.file_popup.dismiss();
                        }
                    } else {
                        let _ = path; // mentions expander re-resolves from display at submit
                        self.composer.complete_file_mention(&display);
                        self.file_popup.dismiss();
                    }
                }
            }
            KeyCode::Esc => {
                self.file_popup.dismiss();
            }
            _ => {
                self.composer.handle_key(key);
                let text = self.composer.text();
                if let Some(q) = extract_at_query(&text) {
                    self.file_popup.set_pending_query(&q);
                    self.file_search.on_query(&q);
                } else {
                    self.file_popup.dismiss();
                }
            }
        }
        Ok(Some(AppAction::Continue))
    }
}

/// Check whether `event` matches the configured `voice.record_key` spec.
///
/// Spec format: `<modifier>-<key>` lowercase. Recognized modifiers:
/// `ctrl`, `alt`, `shift`. Keys: single chars (`b`, `v`) or named keys
/// (`f1`-`f12`, `enter`, `space`, `tab`, `esc`). Unrecognized specs
/// return false so a misconfigured record_key fails closed rather than
/// hijacking a random key.
///
/// Examples: `ctrl-b`, `alt-v`, `ctrl-shift-r`, `f5`.
fn matches_voice_record_key(event: crossterm::event::KeyEvent, spec: &str) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    let spec = spec.trim().to_ascii_lowercase();
    if spec.is_empty() {
        return false;
    }
    let parts: Vec<&str> = spec.split('-').collect();
    let (mods_parts, key_part) = parts.split_at(parts.len() - 1);
    let key_part = key_part[0];

    let mut want = KeyModifiers::NONE;
    for m in mods_parts {
        match *m {
            "ctrl" | "control" => want |= KeyModifiers::CONTROL,
            "alt" | "meta" | "option" => want |= KeyModifiers::ALT,
            "shift" => want |= KeyModifiers::SHIFT,
            _ => return false, // unknown modifier — don't match anything
        }
    }
    let code_matches = match key_part {
        "enter" | "return" => event.code == KeyCode::Enter,
        "space" => event.code == KeyCode::Char(' '),
        "tab" => event.code == KeyCode::Tab,
        "esc" | "escape" => event.code == KeyCode::Esc,
        s if s.starts_with('f') && s.len() > 1 => s[1..]
            .parse::<u8>()
            .map(KeyCode::F)
            .is_ok_and(|c| c == event.code),
        s if s.chars().count() == 1 => {
            // Single character. Crossterm reports Ctrl+letter as
            // KeyCode::Char(<lower>) on most terminals — compare both
            // cases so an uppercased spec ("ctrl-B") still works.
            let Some(want_c) = s.chars().next() else {
                return false;
            };
            event.code == KeyCode::Char(want_c)
                || event.code == KeyCode::Char(want_c.to_ascii_uppercase())
        }
        _ => false,
    };
    // SHIFT is allowed-extra for letter keys because some terminals
    // pass it through even when the user typed lowercase; require exact
    // match on CTRL/ALT but be lenient on SHIFT.
    let got_relevant = event.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT);
    let want_relevant = want & (KeyModifiers::CONTROL | KeyModifiers::ALT);
    code_matches && got_relevant == want_relevant
}

#[cfg(test)]
mod voice_key_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn ctrl_b_matches_default_spec() {
        // Default record_key is "ctrl-b" — the most common chord users
        // see on terminals; the matcher must accept the canonical event.
        let e = ev(KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert!(matches_voice_record_key(e, "ctrl-b"));
    }

    #[test]
    fn plain_b_does_not_match_ctrl_b() {
        // No modifier on the event → must not hijack normal typing.
        let e = ev(KeyCode::Char('b'), KeyModifiers::NONE);
        assert!(!matches_voice_record_key(e, "ctrl-b"));
    }

    #[test]
    fn alt_v_matches_alt_dash_v() {
        let e = ev(KeyCode::Char('v'), KeyModifiers::ALT);
        assert!(matches_voice_record_key(e, "alt-v"));
    }

    #[test]
    fn f5_matches_function_key_spec() {
        let e = ev(KeyCode::F(5), KeyModifiers::NONE);
        assert!(matches_voice_record_key(e, "f5"));
    }

    #[test]
    fn unknown_modifier_fails_closed() {
        // Garbage spec → never matches; better than hijacking a random
        // event because the user typoed their config.
        let e = ev(KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert!(!matches_voice_record_key(e, "hyper-b"));
    }

    #[test]
    fn empty_spec_does_not_match_anything() {
        let e = ev(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(!matches_voice_record_key(e, ""));
    }

    #[test]
    fn bare_ctrl_z_is_a_suspend_chord() {
        assert!(is_suspend_key(&ev(KeyCode::Char('z'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn plain_z_is_not_suspend() {
        // Otherwise normal typing of "z" would suspend the app.
        assert!(!is_suspend_key(&ev(KeyCode::Char('z'), KeyModifiers::NONE)));
    }

    #[test]
    fn ctrl_shift_z_and_ctrl_alt_z_are_not_suspend() {
        // Only the bare chord suspends; decorated variants stay free for
        // future bindings (e.g. a redo) without colliding with suspend.
        assert!(!is_suspend_key(&ev(
            KeyCode::Char('z'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
        assert!(!is_suspend_key(&ev(
            KeyCode::Char('z'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )));
    }

    #[test]
    fn ctrl_other_key_is_not_suspend() {
        assert!(!is_suspend_key(&ev(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }
}
