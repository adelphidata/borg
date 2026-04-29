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

    /// Cycle collaboration mode: Default → Execute → Plan → Default.
    /// Tracks entering/leaving Plan so the post-turn review overlay can
    /// restore the user's prior mode on "Proceed".
    fn cycle_collaboration_mode(&mut self) {
        let current = self.config.conversation.collaboration_mode;
        let next = match current {
            CollaborationMode::Default => CollaborationMode::Execute,
            CollaborationMode::Execute => CollaborationMode::Plan,
            CollaborationMode::Plan => CollaborationMode::Default,
        };
        if next == CollaborationMode::Plan && current != CollaborationMode::Plan {
            self.previous_collab_mode = Some(current);
        } else if next != CollaborationMode::Plan {
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
        match &self.state {
            AppState::ConfirmingUninstall => self.handle_key_confirming_uninstall(key),
            AppState::AwaitingApproval { .. } => self.handle_key_awaiting_approval(key),
            AppState::AwaitingInput { .. } => self.handle_key_awaiting_input(key),
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

    fn handle_key_awaiting_input(&mut self, key: crossterm::event::KeyEvent) -> Result<AppAction> {
        use crossterm::event::{KeyCode, KeyModifiers};
        let AppState::AwaitingInput {
            respond,
            choices,
            cursor,
            custom_mode,
            allow_custom,
            ..
        } = &mut self.state
        else {
            return Ok(AppAction::Continue);
        };

        // Ctrl+C always cancels, regardless of mode.
        if matches!(key.code, KeyCode::Char('c')) && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(tx) = respond.take() {
                let _ = tx.send("[user declined to answer]".to_string());
            }
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
            return Ok(AppAction::Continue);
        }

        if !*custom_mode && !choices.is_empty() {
            // Selection mode.
            let n = choices.len();
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    *cursor = if *cursor == 0 { n - 1 } else { *cursor - 1 };
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *cursor = (*cursor + 1) % n;
                }
                KeyCode::Char(c @ '1'..='9') => {
                    let idx = (c as u8 - b'1') as usize;
                    if idx < n {
                        *cursor = idx;
                        // Submit immediately on digit press for quick selection.
                        let label = choices[idx].label.clone();
                        if let Some(tx) = respond.take() {
                            let _ = tx.send(label);
                        }
                        self.composer.set_text("");
                        self.resume_streaming();
                    }
                }
                KeyCode::Enter => {
                    let label = choices[*cursor].label.clone();
                    if let Some(tx) = respond.take() {
                        let _ = tx.send(label);
                    }
                    self.composer.set_text("");
                    self.resume_streaming();
                }
                KeyCode::Tab => {
                    if *allow_custom {
                        *custom_mode = true;
                    }
                }
                KeyCode::Esc => {
                    if let Some(tx) = respond.take() {
                        let _ = tx.send("[user declined to answer]".to_string());
                    }
                    self.composer.set_text("");
                    self.resume_streaming();
                }
                _ => {}
            }
            return Ok(AppAction::Continue);
        }

        // Free-text / custom mode.
        match key.code {
            KeyCode::Enter => {
                let text = self.composer.text().trim().to_string();
                if text.is_empty() {
                    // Don't send empty responses — user must type something or press Esc
                    return Ok(AppAction::Continue);
                }
                if let Some(tx) = respond.take() {
                    let _ = tx.send(text);
                }
                self.composer.set_text("");
                self.resume_streaming();
            }
            KeyCode::Esc => {
                // If in custom_mode with choices available, Esc returns to selection mode
                // rather than declining, so the user can back out of a typed-answer detour.
                if *custom_mode && !choices.is_empty() {
                    *custom_mode = false;
                    self.composer.set_text("");
                } else {
                    if let Some(tx) = respond.take() {
                        let _ = tx.send("[user declined to answer]".to_string());
                    }
                    self.composer.set_text("");
                    self.resume_streaming();
                }
            }
            _ => {
                self.composer.handle_key(key);
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
                    // Fallback: queue normally if no steer channel
                    let images = self.composer.take_image_attachments();
                    self.cells.push(HistoryCell::User { text: text.clone() });
                    self.cells.push(HistoryCell::System {
                        text: format!("[queued — {} in queue]", self.queued_messages.len() + 1),
                    });
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
            return Ok(AppAction::Continue);
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

        if let Some(action) = self.handle_command_popup_key(key)? {
            return Ok(action);
        }
        if let Some(action) = self.handle_file_popup_key(key)? {
            return Ok(action);
        }

        // ------------------------------------------------------------
        // Up / Down / Page / Home / End fall through to the composer.
        // ------------------------------------------------------------
        // Inline-viewport mode delegates transcript scrolling to the
        // terminal (xterm Alternate Scroll Mode). The composer's
        // history navigation handles Up/Down; PageUp/PageDown/Home/End
        // are no-ops at the App level — the terminal owns scrolling.
        // Ctrl+P / Ctrl+N remain the explicit history-recall keys.
        // ------------------------------------------------------------

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
