use std::collections::VecDeque;
use std::time::Instant;

use anyhow::Result;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};
use ratatui::Frame;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use borg_core::agent::AgentEvent;
use borg_core::config::{CollaborationMode, Config};
use borg_core::doctor::DiagnosticCheck;
use borg_heartbeat::scheduler::{HeartbeatEvent, HeartbeatResult, SkipReason};
use throbber_widgets_tui::{Throbber, ThrobberState, BRAILLE_EIGHT};

use super::command_popup::CommandPopup;
use super::composer::Composer;
use super::file_popup::FileSearchPopup;
use super::history::{ApprovalStatus, HistoryCell};
use super::layout;
use super::migrate_popup::{MigrateAction, MigratePopup};
use super::plan_overlay::{PlanOption, PlanOverlay};
use super::plugins_popup::{PluginAction, PluginsPopup};
use super::schedule_popup::{ScheduleAction, SchedulePopup};
use super::settings_popup::SettingsPopup;
use super::skills_popup::{SkillAction, SkillsPopup};
use super::status_popup::StatusPopup;
use super::theme;

pub enum AppState {
    Idle,
    Streaming {
        start: Instant,
    },
    AwaitingApproval {
        respond: Option<oneshot::Sender<bool>>,
    },
    /// Agent has asked a question mid-turn via `request_user_input`; waiting for user to type.
    AwaitingInput {
        prompt: String,
        respond: Option<oneshot::Sender<String>>,
    },
    PlanReview,
    ConfirmingUninstall,
}

/// A message queued during streaming, preserving both text and image attachments.
pub struct QueuedMessage {
    pub text: String,
    pub images: Vec<super::composer::ImageAttachment>,
}

/// State machine for conversation backtracking (rewinding to a past user message).
pub enum BacktrackPhase {
    /// Not in backtrack mode.
    Inactive,
    /// User is selecting a past message to rewind to.
    Selecting {
        /// Indices into `App::cells` that are `HistoryCell::User` messages, ordered oldest-first.
        user_message_indices: Vec<usize>,
        /// Cursor position within `user_message_indices` (0 = most recent, at the end).
        cursor: usize,
    },
}

/// Events sent from the async doctor task to the TUI event loop.
pub enum DoctorEvent {
    /// Progress indicator before a check category runs.
    Analyzing { label: String },
    /// Completed check category with results.
    Result {
        label: String,
        checks: Vec<DiagnosticCheck>,
    },
    /// All checks done; final summary counts.
    Done {
        pass: usize,
        warn: usize,
        fail: usize,
    },
}

pub enum AppAction {
    Continue,
    Quit,
    /// Request the event loop to spawn an agent call
    SendMessage {
        input: String,
        images: Vec<super::composer::ImageAttachment>,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    },
    CompactHistory,
    ClearHistory,
    ShowUsage,
    UndoLastTurn,
    /// Rewind conversation to the Nth user message (0-indexed, oldest-first).
    RewindTo {
        nth_user_message: usize,
    },
    LaunchExternalEditor,
    UpdateSetting {
        key: String,
        value: String,
    },
    /// All settings were reset to defaults; reload agent config entirely.
    ConfigReloaded,
    SaveSession,
    NewSession,
    LoadSession {
        id: String,
    },
    ListSessions,
    RestartGateway,
    RunPlugins {
        actions: Vec<PluginAction>,
    },
    PlanProceed {
        clear_context: bool,
    },
    RunScheduleActions {
        actions: Vec<ScheduleAction>,
    },
    RunSkillActions {
        actions: Vec<SkillAction>,
    },
    RunMigration {
        actions: Vec<MigrateAction>,
    },
    SelfUpdate {
        dev: bool,
    },
    Uninstall,
    RunDoctor,
    Poke,
}

pub struct App<'a> {
    pub cells: Vec<HistoryCell>,
    pub state: AppState,
    pub composer: Composer<'a>,
    pub command_popup: CommandPopup,
    pub settings_popup: SettingsPopup,
    pub plugins_popup: PluginsPopup,
    pub scroll_offset: usize,
    pub total_lines: usize,
    pub config: Config,
    pub event_rx: Option<mpsc::Receiver<AgentEvent>>,
    pub heartbeat_rx: Option<mpsc::Receiver<HeartbeatEvent>>,
    pub heartbeat_event_tx: Option<mpsc::Sender<HeartbeatEvent>>,
    pub poke_tx: Option<mpsc::Sender<()>>,
    pub cancel_token: Option<CancellationToken>,
    auto_scroll: bool,
    /// Accumulated token usage for the current session
    pub session_prompt_tokens: u64,
    pub session_completion_tokens: u64,
    /// Messages queued by Enter during streaming, auto-submitted FIFO on turn complete
    pub queued_messages: VecDeque<QueuedMessage>,
    /// Whether the last agent turn ended with an error (pauses queue drain)
    pub last_turn_errored: bool,
    /// Whether the "[queue paused]" message has already been shown (prevents duplicates)
    pub queue_pause_notified: bool,
    /// Conversation backtrack state machine
    pub backtrack: BacktrackPhase,
    pub plan_overlay: PlanOverlay,
    /// When the user enters Plan collaboration mode, the previous mode is stashed here
    /// so the post-turn review overlay can restore it on "Proceed". `None` when the
    /// user is not currently in a transient Plan-then-execute flow.
    pub previous_collab_mode: Option<CollaborationMode>,
    pub schedule_popup: SchedulePopup,
    pub skills_popup: SkillsPopup,
    pub migrate_popup: MigratePopup,
    pub file_popup: FileSearchPopup,
    pub throbber_state: ThrobberState,
    transcript_area: Rect,
    /// Channel for sending steer messages to the agent mid-turn.
    pub steer_tx: Option<mpsc::UnboundedSender<String>>,
    /// Steers queued in UI, cleared when agent confirms receipt.
    pub pending_steers: VecDeque<String>,
    /// Current plan steps displayed inline (updated by PlanUpdated events).
    pub plan_steps: Vec<borg_core::types::PlanStep>,
    /// Cached evolution title for banner display.
    pub evolution_title: Option<String>,
    pub status_popup: StatusPopup,
    pub doctor_rx: Option<mpsc::Receiver<DoctorEvent>>,
}

impl<'a> App<'a> {
    pub fn new(
        config: Config,
        heartbeat_rx: Option<mpsc::Receiver<HeartbeatEvent>>,
        heartbeat_event_tx: Option<mpsc::Sender<HeartbeatEvent>>,
        poke_tx: Option<mpsc::Sender<()>>,
    ) -> Self {
        let blocked_paths = config.security.blocked_paths.clone();
        Self {
            cells: Vec::new(),
            state: AppState::Idle,
            composer: Composer::new(),
            command_popup: CommandPopup::new(),
            settings_popup: SettingsPopup::new(),
            plugins_popup: PluginsPopup::new(),
            scroll_offset: 0,
            total_lines: 0,
            config,
            event_rx: None,
            heartbeat_rx,
            heartbeat_event_tx,
            poke_tx,
            cancel_token: None,
            auto_scroll: true,
            session_prompt_tokens: 0,
            session_completion_tokens: 0,
            queued_messages: VecDeque::new(),
            last_turn_errored: false,
            queue_pause_notified: false,
            backtrack: BacktrackPhase::Inactive,
            plan_overlay: PlanOverlay::new(),
            previous_collab_mode: None,
            schedule_popup: SchedulePopup::new(),
            skills_popup: SkillsPopup::new(),
            migrate_popup: MigratePopup::new(),
            file_popup: FileSearchPopup::with_config(
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
                blocked_paths,
            ),
            throbber_state: ThrobberState::default(),
            transcript_area: Rect::default(),
            steer_tx: None,
            pending_steers: VecDeque::new(),
            plan_steps: Vec::new(),
            evolution_title: borg_core::db::Database::open()
                .ok()
                .and_then(|db| db.get_evolution_state().ok())
                .map(|state| borg_core::evolution::format_compact(&state)),
            status_popup: StatusPopup::new(),
            doctor_rx: None,
        }
    }

    pub fn tick_throbber(&mut self) {
        if !matches!(self.state, AppState::Idle) {
            self.throbber_state.calc_next();
        }
    }

    pub fn tick_paste_burst(&mut self) {
        self.composer.tick();
    }

    /// Handle a bracketed paste event (entire pasted text as a single string).
    pub fn handle_paste(&mut self, text: String) -> AppAction {
        match self.state {
            AppState::Idle | AppState::AwaitingInput { .. } => {
                self.composer.handle_paste(&text);
                // Update popup filters (same as after handle_key)
                let composer_text = self.composer.text();
                if composer_text.starts_with('/') {
                    self.command_popup.update_filter(&composer_text);
                }
                AppAction::Continue
            }
            _ => {
                // While streaming, queue the pasted text
                self.pending_steers.push_back(text);
                AppAction::Continue
            }
        }
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
    // by `handle_key` to transcript scroll (when scroll_offset > 0) or composer
    // history (when pinned at the bottom).
    //
    // DO NOT add `fn handle_mouse`. DO NOT match `Event::Mouse` in mod.rs.
    // Either would require turning on a mouse tracking mode, which breaks text
    // selection. Source-guard tests in this file enforce this.
    // =========================================================================

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Result<AppAction> {
        use crossterm::event::{KeyCode, KeyModifiers};

        match &mut self.state {
            AppState::ConfirmingUninstall => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    return Ok(AppAction::Uninstall);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Enter => {
                    self.push_system_message("Uninstall cancelled.".to_string());
                    self.state = AppState::Idle;
                }
                _ => {}
            },
            AppState::AwaitingApproval { respond } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if let Some(tx) = respond.take() {
                        let _ = tx.send(true);
                    }
                    if let Some(HistoryCell::ShellApproval { status, .. }) = self.cells.last_mut() {
                        *status = ApprovalStatus::Approved;
                    }
                    self.state = AppState::Streaming {
                        start: Instant::now(),
                    };
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    if let Some(tx) = respond.take() {
                        let _ = tx.send(false);
                    }
                    if let Some(HistoryCell::ShellApproval { status, .. }) = self.cells.last_mut() {
                        *status = ApprovalStatus::Denied;
                    }
                    self.state = AppState::Streaming {
                        start: Instant::now(),
                    };
                }
                _ => {}
            },
            AppState::AwaitingInput { respond, .. } => match key.code {
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
                    self.state = AppState::Streaming {
                        start: Instant::now(),
                    };
                }
                KeyCode::Esc => {
                    if let Some(tx) = respond.take() {
                        let _ = tx.send("[user declined to answer]".to_string());
                    }
                    self.composer.set_text("");
                    self.state = AppState::Streaming {
                        start: Instant::now(),
                    };
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    // Ctrl+C: decline and cancel turn
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
                }
                _ => {
                    self.composer.handle_key(key);
                }
            },
            AppState::PlanReview => {
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
            }
            AppState::Streaming { .. } => {
                if key.code == KeyCode::Esc
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL))
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
                        let mut messages: Vec<QueuedMessage> =
                            self.queued_messages.drain(..).collect();
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
                                text: "[interrupted — queued messages restored to composer]"
                                    .to_string(),
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
                                text: "[steer queued — will be sent at next tool boundary]"
                                    .to_string(),
                            });
                            self.pending_steers.push_back(text);
                        } else {
                            // Fallback: queue normally if no steer channel
                            let images = self.composer.take_image_attachments();
                            self.cells.push(HistoryCell::User { text: text.clone() });
                            self.cells.push(HistoryCell::System {
                                text: format!(
                                    "[queued — {} in queue]",
                                    self.queued_messages.len() + 1
                                ),
                            });
                            self.queued_messages
                                .push_back(QueuedMessage { text, images });
                        }
                        self.composer.set_text("");
                        self.auto_scroll = true;
                    }
                } else if key.code == KeyCode::Tab {
                    // No-op during streaming
                } else {
                    // Pass other keys to composer so user can type ahead
                    self.composer.handle_key(key);
                }
            }
            AppState::Idle => {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Ok(AppAction::Quit);
                }

                // Popups get first priority for all key events (including Esc)
                if self.settings_popup.is_visible() {
                    if let Some(action) = self.settings_popup.handle_key(key, &mut self.config)? {
                        return Ok(action);
                    }
                    return Ok(AppAction::Continue);
                }

                if self.plugins_popup.is_visible() {
                    if let Some(actions) = self.plugins_popup.handle_key(key) {
                        return Ok(AppAction::RunPlugins { actions });
                    }
                    return Ok(AppAction::Continue);
                }

                if self.schedule_popup.is_visible() {
                    if let Some(actions) = self.schedule_popup.handle_key(key) {
                        return Ok(AppAction::RunScheduleActions { actions });
                    }
                    return Ok(AppAction::Continue);
                }

                if self.skills_popup.is_visible() {
                    if let Some(actions) = self.skills_popup.handle_key(key) {
                        return Ok(AppAction::RunSkillActions { actions });
                    }
                    return Ok(AppAction::Continue);
                }

                if self.migrate_popup.is_visible() {
                    if let Some(actions) = self.migrate_popup.handle_key(key) {
                        return Ok(AppAction::RunMigration { actions });
                    }
                    return Ok(AppAction::Continue);
                }

                if self.status_popup.is_visible() {
                    self.status_popup.handle_key(key);
                    return Ok(AppAction::Continue);
                }

                // Handle backtrack mode (selecting a past user message to rewind to)
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
                            // cursor 0 = most recent, which is the last element
                            let cell_idx = indices[indices.len() - 1 - cur];
                            let text = if let HistoryCell::User { text } = &self.cells[cell_idx] {
                                text.clone()
                            } else {
                                String::new()
                            };
                            // Count which user message this is (0-indexed, oldest-first)
                            let nth = self.cells[..=cell_idx]
                                .iter()
                                .filter(|c| matches!(c, HistoryCell::User { .. }))
                                .count()
                                - 1;
                            self.backtrack = BacktrackPhase::Inactive;
                            self.cells.truncate(cell_idx);
                            self.composer.set_text(&text);
                            self.auto_scroll = true;
                            return Ok(AppAction::RewindTo {
                                nth_user_message: nth,
                            });
                        }
                        KeyCode::Esc => {
                            self.backtrack = BacktrackPhase::Inactive;
                        }
                        _ => {}
                    }
                    return Ok(AppAction::Continue);
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
                if key.code == KeyCode::Esc && self.composer.is_empty() && !self.last_turn_errored {
                    let user_indices: Vec<usize> = self
                        .cells
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| matches!(c, HistoryCell::User { .. }))
                        .map(|(i, _)| i)
                        .collect();
                    if !user_indices.is_empty() {
                        self.backtrack = BacktrackPhase::Selecting {
                            user_message_indices: user_indices,
                            cursor: 0, // 0 = most recent
                        };
                        return Ok(AppAction::Continue);
                    }
                }

                // Ctrl+L — clear visual transcript
                if key.code == KeyCode::Char('l') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    self.cells.clear();
                    self.scroll_offset = 0;
                    self.auto_scroll = true;
                    return Ok(AppAction::Continue);
                }

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

                // Ctrl+V — paste clipboard image (fall through to normal paste if no image)
                if key.code == KeyCode::Char('v')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && try_paste_clipboard_image(&mut self.composer)
                {
                    return Ok(AppAction::Continue);
                }

                // Shift+Tab — cycle collaboration mode (default → execute → plan → default)
                if key.code == KeyCode::BackTab {
                    let current = self.config.conversation.collaboration_mode;
                    let next = match current {
                        CollaborationMode::Default => CollaborationMode::Execute,
                        CollaborationMode::Execute => CollaborationMode::Plan,
                        CollaborationMode::Plan => CollaborationMode::Default,
                    };
                    // Track entering/leaving Plan so the post-turn review overlay can
                    // restore the user's prior mode on "Proceed".
                    if next == CollaborationMode::Plan && current != CollaborationMode::Plan {
                        self.previous_collab_mode = Some(current);
                    } else if next != CollaborationMode::Plan {
                        self.previous_collab_mode = None;
                    }
                    self.config.conversation.collaboration_mode = next;
                    self.push_system_message(format!("[mode: {next}]"));
                    return Ok(AppAction::Continue);
                }

                if self.command_popup.is_visible() {
                    match key.code {
                        KeyCode::Up => {
                            self.command_popup.move_up();
                            return Ok(AppAction::Continue);
                        }
                        KeyCode::Down => {
                            self.command_popup.move_down();
                            return Ok(AppAction::Continue);
                        }
                        KeyCode::Tab => {
                            if let Some(cmd) = self.command_popup.selected_command() {
                                let name = cmd.name.to_string();
                                self.composer.set_text(&name);
                                self.command_popup.dismiss();
                            }
                            return Ok(AppAction::Continue);
                        }
                        KeyCode::Enter => {
                            if let Some(cmd) = self.command_popup.selected_command() {
                                let name = cmd.name.to_string();
                                self.composer.set_text("");
                                self.command_popup.dismiss();
                                return self.handle_submit(&name);
                            }
                            // No matching command — dismiss popup and submit raw text
                            let text = self.composer.text().trim().to_string();
                            self.composer.set_text("");
                            self.command_popup.dismiss();
                            if text.is_empty() {
                                return Ok(AppAction::Continue);
                            }
                            return self.handle_submit(&text);
                        }
                        KeyCode::Esc => {
                            self.command_popup.dismiss();
                            self.composer.set_text("");
                            return Ok(AppAction::Continue);
                        }
                        _ => {
                            // Pass key to composer, then update filter
                            if let Some(text) = self.composer.handle_key(key) {
                                self.command_popup.dismiss();
                                return self.handle_submit(&text);
                            }
                            let text = self.composer.text();
                            self.command_popup.update_filter(&text);
                            return Ok(AppAction::Continue);
                        }
                    }
                }

                if self.file_popup.is_visible() {
                    match key.code {
                        KeyCode::Up => {
                            self.file_popup.move_up();
                            return Ok(AppAction::Continue);
                        }
                        KeyCode::Down => {
                            self.file_popup.move_down();
                            return Ok(AppAction::Continue);
                        }
                        KeyCode::Tab | KeyCode::Enter => {
                            if let Some(file) = self.file_popup.selected_file() {
                                let display = file.display.clone();
                                let path = file.full_path.clone();
                                let is_dir = file.is_dir;
                                if is_dir {
                                    // Directory: rewrite the in-progress mention to
                                    // `@<path>/` (no trailing space, no FileRef) and
                                    // re-run the popup so the user can drill further.
                                    self.composer.set_partial_mention(display);
                                    let text = self.composer.text();
                                    if let Some(q) = extract_at_query(&text) {
                                        self.file_popup.update_query(&q);
                                    } else {
                                        self.file_popup.dismiss();
                                    }
                                } else {
                                    self.composer.add_file_ref(display, path);
                                    self.file_popup.dismiss();
                                }
                            }
                            return Ok(AppAction::Continue);
                        }
                        KeyCode::Esc => {
                            self.file_popup.dismiss();
                            return Ok(AppAction::Continue);
                        }
                        _ => {
                            self.composer.handle_key(key);
                            let text = self.composer.text();
                            if let Some(q) = extract_at_query(&text) {
                                self.file_popup.update_query(&q);
                            } else {
                                self.file_popup.dismiss();
                            }
                            return Ok(AppAction::Continue);
                        }
                    }
                }

                // ? — show keyboard shortcuts when composer is empty
                if key.code == KeyCode::Char('?')
                    && key.modifiers == KeyModifiers::NONE
                    && self.composer.is_empty()
                {
                    self.push_system_message(
                        "Keyboard Shortcuts:\n  \
                         Enter        — Send message\n  \
                         Shift+Enter  — New line\n  \
                         Up / Ctrl+P  — Previous history entry\n  \
                         Down / Ctrl+N — Next history entry\n  \
                         Esc          — Clear input / Rewind (when empty)\n  \
                         Ctrl+L       — Clear screen\n  \
                         Ctrl+D       — Quit (when empty)\n  \
                         Ctrl+G       — Open external editor ($EDITOR)\n  \
                         Enter        — Queue message while streaming\n  \
                         Alt+Up       — Edit last queued message\n  \
                         Ctrl+C       — Cancel / Quit\n  \
                         Shift+Tab    — Cycle mode (default/execute/plan)\n  \
                         PageUp/Down  — Scroll transcript\n  \
                         Mouse wheel  — Scroll transcript\n  \
                         /            — Show command menu"
                            .to_string(),
                    );
                    return Ok(AppAction::Continue);
                }

                // ------------------------------------------------------------
                // Up / Down arrow routing — dual semantics.
                // ------------------------------------------------------------
                // Mouse-wheel events are delivered to us as KeyCode::Up /
                // KeyCode::Down by the terminal, via xterm Alternate Scroll
                // Mode (?1007h — see tui::mod::EnableAlternateScroll). This is
                // the mechanism that lets us support wheel scroll WITHOUT
                // enabling any mouse tracking mode, which would otherwise
                // break native click+drag text selection.
                //
                // We cannot distinguish wheel-sourced arrows from keyboard
                // arrows (they are identical byte sequences), so Up/Down is
                // overloaded. Routing rule:
                //
                //   A. scroll_offset > 0 (already reading scrollback)
                //        → Up/Down scroll the transcript line-by-line.
                //          Reaching offset 0 re-enables auto-scroll.
                //   B. composer is idle (empty AND not browsing history)
                //        → Up/Down scroll the transcript (wheel semantics
                //          dominate in the normal typing-at-bottom state).
                //          Ctrl+P / Ctrl+N still recall shell history — they
                //          are handled inside the composer regardless.
                //   C. composer has text OR is already browsing history
                //        → Up/Down navigate composer history (the user is
                //          actively in the composer and expects arrows to
                //          move through drafts).
                //
                // PageUp / PageDown always scroll the transcript, independent
                // of this rule. Ctrl+P / Ctrl+N always hit composer history.
                // ------------------------------------------------------------
                match key.code {
                    KeyCode::PageUp => {
                        self.scroll_offset = self
                            .scroll_offset
                            .saturating_add(borg_core::constants::PAGE_SCROLL_LINES);
                        self.auto_scroll = false;
                        return Ok(AppAction::Continue);
                    }
                    KeyCode::PageDown => {
                        self.scroll_offset = self
                            .scroll_offset
                            .saturating_sub(borg_core::constants::PAGE_SCROLL_LINES);
                        if self.scroll_offset == 0 {
                            self.auto_scroll = true;
                        }
                        return Ok(AppAction::Continue);
                    }
                    KeyCode::Up
                        if key.modifiers.is_empty()
                            && (self.scroll_offset > 0
                                || (self.composer.is_empty()
                                    && !self.composer.is_browsing_history())) =>
                    {
                        let max_scroll = self
                            .total_lines
                            .saturating_sub(self.transcript_area.height as usize);
                        if max_scroll > 0 {
                            self.scroll_offset = (self.scroll_offset + 1).min(max_scroll);
                            self.auto_scroll = false;
                            return Ok(AppAction::Continue);
                        }
                        // No scrollable content — fall through to composer.
                    }
                    KeyCode::Down if key.modifiers.is_empty() && self.scroll_offset > 0 => {
                        self.scroll_offset = self.scroll_offset.saturating_sub(1);
                        if self.scroll_offset == 0 {
                            self.auto_scroll = true;
                        }
                        return Ok(AppAction::Continue);
                    }
                    _ => {}
                }

                if let Some(text) = self.composer.handle_key(key) {
                    return self.handle_submit(&text);
                }
                // Update popup filters after normal key input
                let text = self.composer.text();
                self.command_popup.update_filter(&text);
                if !self.command_popup.is_visible() {
                    if let Some(q) = extract_at_query(&text) {
                        self.file_popup.update_query(&q);
                    } else {
                        self.file_popup.dismiss();
                    }
                }
            }
        }

        Ok(AppAction::Continue)
    }

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

    pub(super) fn handle_logs_command(&mut self, arg: Option<&str>) -> Result<AppAction> {
        // /logs raw — old behavior: tail tui.log
        if arg == Some("raw") {
            let log_path = match borg_core::config::Config::logs_dir() {
                Ok(d) => d.join("tui.log"),
                Err(e) => {
                    self.push_system_message(format!("Error resolving log directory: {e}"));
                    return Ok(AppAction::Continue);
                }
            };
            let text = if log_path.exists() {
                use std::io::{Read, Seek, SeekFrom};
                const TAIL_BYTES: u64 = 32_768;
                match std::fs::File::open(&log_path) {
                    Ok(mut f) => {
                        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
                        if len > TAIL_BYTES {
                            let _ = f.seek(SeekFrom::End(-(TAIL_BYTES as i64)));
                        }
                        let mut buf = String::new();
                        let _ = f.read_to_string(&mut buf);
                        let lines: Vec<&str> = buf.lines().collect();
                        let start = if len > TAIL_BYTES {
                            1
                        } else {
                            lines.len().saturating_sub(50)
                        };
                        let tail: Vec<&str> = lines[start..].to_vec();
                        let tail = &tail[tail.len().saturating_sub(50)..];
                        tail.join("\n")
                    }
                    Err(e) => format!("Error reading log file: {e}"),
                }
            } else {
                "No log file found.".to_string()
            };
            self.push_system_message(if text.is_empty() {
                "Log file is empty.".to_string()
            } else {
                text
            });
            return Ok(AppAction::Continue);
        }

        // Structured activity log
        let level_filter = match arg {
            None | Some("info") => Some("info"),
            Some("error") => Some("error"),
            Some("warn") => Some("warn"),
            Some("debug") | Some("all") => Some("debug"),
            Some(other) => {
                self.push_system_message(format!(
                    "Unknown level '{other}'. Usage: /logs [error|warn|info|debug|all|raw]"
                ));
                return Ok(AppAction::Continue);
            }
        };

        let text = match borg_core::db::Database::open() {
            Ok(db) => match db.query_activity(50, level_filter, None) {
                Ok(entries) if entries.is_empty() => "No activity log entries.".to_string(),
                Ok(entries) => entries
                    .iter()
                    .rev()
                    .map(borg_core::activity_log::format_activity_entry)
                    .collect::<Vec<_>>()
                    .join("\n"),
                Err(e) => format!("Error querying activity log: {e}"),
            },
            Err(e) => format!("Error opening database: {e}"),
        };

        self.push_system_message(text);
        Ok(AppAction::Continue)
    }

    pub(super) fn handle_submit(&mut self, input: &str) -> Result<AppAction> {
        let trimmed = input.trim();

        // Try slash command dispatch (defined in commands.rs)
        if let Some(result) = self.try_handle_command(trimmed) {
            return result;
        }

        // Separator between turns
        if !self.cells.is_empty() {
            self.cells.push(HistoryCell::Separator);
        }
        // Prepare to send to agent
        self.cells.push(HistoryCell::User {
            text: input.to_string(),
        });
        self.cells.push(HistoryCell::Assistant {
            text: String::new(),
            streaming: true,
        });

        // Take image attachments before file refs
        let images = self.composer.take_image_attachments();

        // Inject file contents from @mentions
        let file_refs = self.composer.take_file_refs();
        let final_input = if file_refs.is_empty() {
            input.to_string()
        } else {
            let mut buf = input.to_string();
            for fref in &file_refs {
                match std::fs::read_to_string(&fref.path) {
                    Ok(contents) => {
                        const MAX_FILE_BYTES: usize = 100 * 1024;
                        let truncated = if contents.len() > MAX_FILE_BYTES {
                            format!(
                                "{}\n[truncated — file exceeds 100KB]",
                                &contents[..contents
                                    .char_indices()
                                    .take_while(|(i, _)| *i < MAX_FILE_BYTES)
                                    .last()
                                    .map(|(i, c)| i + c.len_utf8())
                                    .unwrap_or(0)]
                            )
                        } else {
                            contents
                        };
                        buf.push_str(&format!(
                            "\n\n<file path=\"{}\">\n{truncated}\n</file>",
                            fref.display
                        ));
                    }
                    Err(e) => {
                        buf.push_str(&format!(
                            "\n\n<file path=\"{}\">\n[error reading file: {e}]\n</file>",
                            fref.display
                        ));
                    }
                }
            }
            buf
        };

        let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(256);
        self.event_rx = Some(event_rx);

        let cancel = CancellationToken::new();
        self.cancel_token = Some(cancel.clone());

        self.state = AppState::Streaming {
            start: Instant::now(),
        };
        self.auto_scroll = true;

        Ok(AppAction::SendMessage {
            input: final_input,
            images,
            event_tx,
            cancel,
        })
    }

    pub fn process_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Preparing => {
                self.cells.push(HistoryCell::Thinking {
                    text: String::new(),
                });
                if self.auto_scroll {
                    self.scroll_offset = 0;
                }
            }
            AgentEvent::TextDelta(delta) => {
                // Remove empty Thinking placeholder (from Preparing event)
                if matches!(self.cells.last(), Some(HistoryCell::Thinking { text }) if text.is_empty())
                {
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
                if self.auto_scroll {
                    self.scroll_offset = 0;
                }
            }
            AgentEvent::ThinkingDelta(delta) => {
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
                    let insert_pos = if len > 0
                        && matches!(self.cells[len - 1], HistoryCell::Assistant { .. })
                    {
                        len - 1
                    } else {
                        len
                    };
                    self.cells
                        .insert(insert_pos, HistoryCell::Thinking { text: delta });
                }
                if self.auto_scroll {
                    self.scroll_offset = 0;
                }
            }
            AgentEvent::ToolExecuting { name, args } => {
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
            AgentEvent::ToolResult { name, result } => {
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
                    let cat = super::tool_display::classify_tool(&name, args);
                    super::tool_display::tool_result_label(&cat)
                } else {
                    format!("Ran {name}")
                };
                let is_error = result.starts_with("Error:");
                self.cells.push(HistoryCell::ToolResult {
                    output: result,
                    is_error,
                    duration_ms,
                    display_label,
                });
                if self.auto_scroll {
                    self.scroll_offset = 0;
                }
            }
            AgentEvent::ShellConfirmation { command, respond } => {
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
            AgentEvent::Usage(usage) => {
                self.session_prompt_tokens += usage.prompt_tokens;
                self.session_completion_tokens += usage.completion_tokens;
            }
            AgentEvent::TurnComplete => {
                // Clean up steer channel on turn completion
                self.steer_tx = None;
                self.pending_steers.clear();
                // Clean up any leftover empty thinking placeholders
                self.cells
                    .retain(|c| !matches!(c, HistoryCell::Thinking { text } if text.is_empty()));
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
            AgentEvent::Error(e) => {
                self.cells.push(HistoryCell::System {
                    text: format!("Error: {e}"),
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
            AgentEvent::SteerReceived { text } => {
                // Remove matching steer from pending
                if let Some(pos) = self.pending_steers.iter().position(|s| *s == text) {
                    self.pending_steers.remove(pos);
                }
            }
            AgentEvent::PlanUpdated { steps } => {
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
            AgentEvent::UserInputRequest { prompt, respond } => {
                // Show prompt and transition to awaiting input
                self.cells.push(HistoryCell::System {
                    text: format!("[agent asks: {prompt}]"),
                });
                self.state = AppState::AwaitingInput {
                    prompt,
                    respond: Some(respond),
                };
                if self.auto_scroll {
                    self.scroll_offset = 0;
                }
            }
            AgentEvent::SubAgentUpdate { .. } => {
                // Sub-agent updates are informational; no TUI action needed yet.
            }
            AgentEvent::ToolOutputDelta {
                delta, is_stderr, ..
            } => {
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

    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let show_status = !matches!(self.state, AppState::Idle);
        let composer_height = self.composer.height();
        let queue_preview_height = self.compute_queue_preview_height();
        let app_layout =
            layout::compute_layout(area, composer_height, show_status, queue_preview_height);

        self.render_transcript(frame, app_layout.transcript);
        if show_status {
            self.render_status(frame, app_layout.status);
        }
        if queue_preview_height > 0 {
            self.render_queue_preview(frame, app_layout.queue_preview);
        }
        self.composer.render(frame, app_layout.composer);
        self.render_footer(frame, app_layout.footer);
        self.plan_overlay.render(frame, app_layout.composer);
        self.command_popup.render(frame, app_layout.composer);
        self.file_popup.render(frame, app_layout.composer);
        self.settings_popup.render(frame, &self.config);
        self.plugins_popup.render(frame);
        self.schedule_popup.render(frame);
        self.skills_popup.render(frame);
        self.migrate_popup.render(frame);
        self.status_popup.render(frame);
    }

    fn compute_context_pct(&self) -> u8 {
        let max = self.config.conversation.max_history_tokens;
        if max == 0 {
            return 0;
        }
        let used = self.session_prompt_tokens + self.session_completion_tokens;
        ((used as f64 / max as f64) * 100.0).min(100.0) as u8
    }

    fn render_transcript(&mut self, frame: &mut Frame, area: Rect) {
        self.transcript_area = area;
        let width = area.width;
        let mut all_lines: Vec<Line<'static>> = Vec::new();

        let throbber_state = match &self.state {
            AppState::Streaming { .. } => Some(&self.throbber_state),
            _ => None,
        };

        // Always show branded header
        let version = env!("CARGO_PKG_VERSION");
        all_lines.push(Line::from(vec![
            Span::styled(
                "BORG",
                Style::default()
                    .fg(theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::from(" "),
            Span::styled(format!("v{version}"), theme::dim()),
        ]));
        all_lines.push(Line::default());

        let name = self.config.user.agent_name.as_deref().unwrap_or("Borg");
        all_lines.push(Line::from(vec![
            Span::styled("name:  ", theme::dim()),
            Span::from(name.to_string()),
        ]));

        all_lines.push(Line::from(vec![
            Span::styled("model: ", theme::dim()),
            Span::from(self.config.llm.model.clone()),
        ]));

        if let Some(ref evo_title) = self.evolution_title {
            all_lines.push(Line::from(vec![
                Span::styled("class: ", theme::dim()),
                Span::styled(evo_title.clone(), Style::default().fg(theme::CYAN)),
            ]));
        }

        all_lines.push(Line::default());

        for (i, cell) in self.cells.iter().enumerate() {
            if i > 0 && !cell.is_stream_continuation() {
                all_lines.push(Line::default());
            }
            all_lines.extend(cell.render(width, throbber_state));
        }

        let paragraph = Paragraph::new(all_lines).wrap(Wrap { trim: false });
        self.total_lines = paragraph.line_count(width);

        let visible_height = area.height as usize;
        let max_scroll = self.total_lines.saturating_sub(visible_height);
        self.scroll_offset = self.scroll_offset.min(max_scroll);
        let scroll_pos = max_scroll.saturating_sub(self.scroll_offset);

        // Clamp to u16 for ratatui's scroll API
        let scroll_pos_u16 = u16::try_from(scroll_pos).unwrap_or(u16::MAX);

        let paragraph = paragraph.scroll((scroll_pos_u16, 0));

        frame.render_widget(paragraph, area);

        if self.total_lines > visible_height {
            let mut scrollbar_state =
                ScrollbarState::new(max_scroll).position(max_scroll - self.scroll_offset);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight),
                area,
                &mut scrollbar_state,
            );
        }
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let line = match &self.state {
            AppState::Streaming { start, .. } => {
                let elapsed = theme::format_elapsed(start.elapsed().as_secs());
                let throbber = Throbber::default()
                    .throbber_set(BRAILLE_EIGHT)
                    .throbber_style(theme::tool_style());
                Line::from(vec![
                    Span::raw(" "),
                    throbber.to_symbol_span(&self.throbber_state),
                    Span::styled(format!("Working ({elapsed}"), theme::tool_style()),
                    Span::styled(" • esc to interrupt)", theme::dim()),
                ])
            }
            AppState::AwaitingApproval { .. } => Line::from(vec![Span::styled(
                format!(" {} Approval needed — press y or n", theme::BULLET),
                theme::error_style(),
            )]),
            AppState::AwaitingInput { .. } => Line::from(vec![Span::styled(
                format!(
                    " {} Agent needs your input — type and press enter",
                    theme::BULLET
                ),
                theme::tool_style(),
            )]),
            AppState::PlanReview => Line::from(vec![Span::styled(
                format!(" {} Plan ready — choose an action", theme::BULLET),
                theme::tool_style(),
            )]),
            AppState::ConfirmingUninstall => Line::from(vec![Span::styled(
                format!(" {} Confirm uninstall — press y or N", theme::BULLET),
                theme::error_style(),
            )]),
            AppState::Idle => Line::default(),
        };
        frame.render_widget(Paragraph::new(line), area);
    }

    /// Pop the next queued message (FIFO) for dispatch.
    pub fn pop_next_queued(&mut self) -> Option<QueuedMessage> {
        self.queued_messages.pop_front()
    }

    /// Submit a queued message (called from the event loop when a turn completes).
    /// The user message was already shown in the transcript when it was queued,
    /// so we skip the User cell push and only add Separator + Assistant cell.
    pub fn handle_queued_submit(&mut self, qm: QueuedMessage) -> Result<AppAction> {
        // Add separator between turns (User cell was already shown when queued)
        if !self.cells.is_empty() {
            self.cells.push(HistoryCell::Separator);
        }
        self.cells.push(HistoryCell::Assistant {
            text: String::new(),
            streaming: true,
        });

        let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(256);
        self.event_rx = Some(event_rx);

        let cancel = CancellationToken::new();
        self.cancel_token = Some(cancel.clone());

        self.state = AppState::Streaming {
            start: Instant::now(),
        };
        self.auto_scroll = true;

        Ok(AppAction::SendMessage {
            input: qm.text,
            images: qm.images,
            event_tx,
            cancel,
        })
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let left = match &self.state {
            AppState::Idle if matches!(self.backtrack, BacktrackPhase::Selecting { .. }) => {
                "↑/↓ select message  •  enter to rewind  •  esc to cancel".to_string()
            }
            AppState::Idle if self.last_turn_errored && !self.queued_messages.is_empty() => {
                "enter to resume queue  •  esc to clear queue".to_string()
            }
            AppState::Idle
                if self.config.conversation.collaboration_mode == CollaborationMode::Plan =>
            {
                "[plan]  •  shift+tab to toggle off  •  ? for shortcuts".to_string()
            }
            AppState::Idle if self.composer.is_empty() => {
                "esc to rewind  •  ? for shortcuts  •  quit to exit".to_string()
            }
            AppState::Idle => {
                "? for shortcuts  •  pgup/pgdn to scroll  •  quit to exit".to_string()
            }
            AppState::Streaming { .. } => {
                let count = self.queued_messages.len();
                if count > 0 {
                    format!(
                        "esc to cancel (queue preserved)  •  alt+↑ edit last  •  ({count} queued)"
                    )
                } else {
                    "esc to cancel  •  enter to queue".to_string()
                }
            }
            AppState::AwaitingApproval { .. } => "y to approve  •  n to deny".to_string(),
            AppState::AwaitingInput { prompt, .. } => {
                format!("type your answer  •  enter to send  •  esc to skip  [{prompt}]")
            }
            AppState::PlanReview => {
                "shift+tab: cycle  •  1-3: jump  •  enter: confirm  •  esc: dismiss".to_string()
            }
            AppState::ConfirmingUninstall => "y to uninstall  •  N / enter to cancel".to_string(),
        };
        let line = Line::from(Span::styled(format!(" {left}"), theme::dim()));
        frame.render_widget(Paragraph::new(line), area);
    }

    fn compute_queue_preview_height(&self) -> u16 {
        let count = self.queued_messages.len();
        if count == 0 {
            return 0;
        }
        let shown = count.min(3) as u16;
        let overflow = if count > 3 { 1u16 } else { 0 };
        // header + shown messages + overflow + hint
        1 + shown + overflow + 1
    }

    fn render_queue_preview(&self, frame: &mut Frame, area: Rect) {
        let dim_italic = Style::default()
            .fg(theme::DIM_WHITE)
            .add_modifier(Modifier::ITALIC);
        let mut lines: Vec<Line<'static>> = Vec::new();

        lines.push(Line::from(Span::styled(
            " Queued messages:".to_string(),
            theme::dim(),
        )));

        let count = self.queued_messages.len();
        let shown = count.min(3);
        for (i, qm) in self.queued_messages.iter().take(shown).enumerate() {
            let is_last_shown = i + 1 == shown && count <= 3;
            let truncated = if qm.text.len() > 50 {
                let end = qm
                    .text
                    .char_indices()
                    .map(|(idx, _)| idx)
                    .take_while(|&idx| idx <= 47)
                    .last()
                    .unwrap_or(0);
                format!("{}...", &qm.text[..end])
            } else {
                qm.text.clone()
            };
            let img_badge = if !qm.images.is_empty() {
                format!(
                    " [{} image{}]",
                    qm.images.len(),
                    if qm.images.len() == 1 { "" } else { "s" }
                )
            } else {
                String::new()
            };
            let prefix = if is_last_shown {
                theme::TREE_END
            } else {
                theme::TREE_MID
            };
            let style = if i + 1 == count {
                // Last item overall: underline to hint Alt+Up editability
                dim_italic.add_modifier(Modifier::UNDERLINED)
            } else {
                dim_italic
            };
            lines.push(Line::from(Span::styled(
                format!("  {prefix} {}. {truncated}{img_badge}", i + 1),
                style,
            )));
        }

        if count > 3 {
            lines.push(Line::from(Span::styled(
                format!("  ... and {} more", count - 3),
                theme::dim(),
            )));
        }

        lines.push(Line::from(Span::styled(
            "  Alt+Up to edit last".to_string(),
            theme::dim(),
        )));

        frame.render_widget(Paragraph::new(lines), area);
    }
}

/// Extract the query portion after the last `@` in text, if it looks like a file
/// mention in progress (preceded by whitespace or at position 0, and no space after it).
fn extract_at_query(text: &str) -> Option<String> {
    let at_pos = text.rfind('@')?;
    // Must be at start or preceded by whitespace
    if at_pos > 0 {
        let prev = text.as_bytes()[at_pos - 1];
        if prev != b' ' && prev != b'\t' && prev != b'\n' {
            return None;
        }
    }
    let after = &text[at_pos + 1..];
    // If there's a space, the mention is already completed
    if after.contains(' ') {
        return None;
    }
    Some(after.to_string())
}

/// Try to paste a clipboard image into the composer. Returns true if an image was pasted.
fn try_paste_clipboard_image(composer: &mut Composer) -> bool {
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        return false;
    };

    // Try image data first
    if let Ok(img_data) = clipboard.get_image() {
        if let Some(png_bytes) = rgba_to_png(
            &img_data.bytes,
            img_data.width as u32,
            img_data.height as u32,
        ) {
            composer.add_image(png_bytes, "image/png".to_string());
            return true;
        }
    }

    // Try text that looks like an image file path
    if let Ok(text) = clipboard.get_text() {
        let trimmed = text.trim();
        let path = std::path::Path::new(trimmed);
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let ext_lower = ext.to_lowercase();
            const IMG_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "heic", "heif"];
            if IMG_EXTS.contains(&ext_lower.as_str()) && path.exists() {
                if let Ok(bytes) = std::fs::read(path) {
                    let mime = match ext_lower.as_str() {
                        "png" => "image/png",
                        "jpg" | "jpeg" => "image/jpeg",
                        "gif" => "image/gif",
                        "webp" => "image/webp",
                        "bmp" => "image/bmp",
                        "heic" | "heif" => "image/heic",
                        _ => "application/octet-stream",
                    };
                    composer.add_image(bytes, mime.to_string());
                    return true;
                }
            }
        }
    }

    false
}

/// Convert raw RGBA pixel data to PNG bytes.
fn rgba_to_png(rgba_bytes: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let img = image::RgbaImage::from_raw(width, height, rgba_bytes.to_vec())?;
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn make_app() -> App<'static> {
        let config = Config::default();
        App::new(config, None, None, None)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    // ========================================================================
    // Up/Down arrow routing — dual semantics (transcript scroll vs. composer
    // history). See the big block comment in `App::handle_key`.
    //
    // CRITICAL: mouse wheel is delivered as KeyCode::Up/Down via xterm
    // Alternate Scroll Mode (?1007h) — see tui::mod::EnableAlternateScroll.
    // These tests pin down the routing rule so wheel-scroll and shell-style
    // history recall can coexist without regressing native text selection.
    // ========================================================================

    /// Helper: configure an app that has a scrollable transcript (so
    /// `max_scroll > 0` and the transcript-scroll branch can actually fire).
    fn app_with_scrollable_transcript() -> App<'static> {
        let mut app = make_app();
        app.transcript_area = Rect::new(0, 0, 80, 40);
        app.total_lines = 200;
        app
    }

    // --- Rule A: scroll_offset > 0 → arrows scroll the transcript ---

    #[test]
    fn arrow_up_scrolls_transcript_when_scroll_offset_positive() {
        let mut app = app_with_scrollable_transcript();
        app.scroll_offset = 5;
        app.auto_scroll = false;

        app.handle_key(key(KeyCode::Up)).unwrap();

        assert_eq!(app.scroll_offset, 6);
        assert!(!app.auto_scroll);
        assert!(app.composer.is_empty(), "composer must be untouched");
        assert!(!app.composer.is_browsing_history());
    }

    #[test]
    fn arrow_down_scrolls_transcript_when_scroll_offset_positive() {
        let mut app = app_with_scrollable_transcript();
        app.scroll_offset = 5;
        app.auto_scroll = false;

        app.handle_key(key(KeyCode::Down)).unwrap();

        assert_eq!(app.scroll_offset, 4);
        assert!(!app.auto_scroll);
    }

    #[test]
    fn arrow_down_at_offset_one_restores_auto_scroll() {
        let mut app = app_with_scrollable_transcript();
        app.scroll_offset = 1;
        app.auto_scroll = false;

        app.handle_key(key(KeyCode::Down)).unwrap();

        assert_eq!(app.scroll_offset, 0);
        assert!(app.auto_scroll);
    }

    #[test]
    fn arrow_up_scroll_clamped_to_max_offset() {
        let mut app = app_with_scrollable_transcript();
        let max_scroll = app
            .total_lines
            .saturating_sub(app.transcript_area.height as usize);
        app.scroll_offset = max_scroll;

        // Hammer Up many times — must never exceed the cap.
        for _ in 0..50 {
            app.handle_key(key(KeyCode::Up)).unwrap();
        }

        assert_eq!(app.scroll_offset, max_scroll);
    }

    // --- Rule B: idle composer at bottom → arrows scroll (wheel semantics) ---

    #[test]
    fn arrow_up_scrolls_transcript_when_composer_idle_and_scrollable() {
        let mut app = app_with_scrollable_transcript();
        assert!(app.composer.is_empty());
        assert_eq!(app.scroll_offset, 0);

        app.handle_key(key(KeyCode::Up)).unwrap();

        // Wheel-from-bottom: mouse wheel (delivered as Up) starts scrolling.
        assert_eq!(app.scroll_offset, 1);
        assert!(!app.auto_scroll);
        assert!(app.composer.is_empty());
    }

    #[test]
    fn arrow_up_falls_through_to_history_when_transcript_not_scrollable() {
        let mut app = make_app();
        // Default transcript_area is 0x0, so max_scroll == 0 regardless.
        app.composer.set_text("first");
        app.composer.handle_key(key(KeyCode::Enter));
        assert!(app.composer.is_empty());

        app.handle_key(key(KeyCode::Up)).unwrap();

        // No scrollable content → fall through to composer history recall.
        assert_eq!(app.scroll_offset, 0);
        assert_eq!(app.composer.text(), "first");
    }

    // --- Rule C: active composer → arrows navigate history ---

    #[test]
    fn arrow_up_navigates_history_when_composer_has_text() {
        let mut app = app_with_scrollable_transcript();
        app.composer.set_text("first message");
        app.composer.handle_key(key(KeyCode::Enter));
        app.composer.set_text("draft");

        app.handle_key(key(KeyCode::Up)).unwrap();

        assert_eq!(app.composer.text(), "first message");
        assert!(app.composer.is_browsing_history());
        assert_eq!(
            app.scroll_offset, 0,
            "transcript must NOT scroll while composer is active"
        );
    }

    #[test]
    fn arrow_down_navigates_history_while_browsing() {
        // Non-scrollable transcript (max_scroll == 0) so rule B falls through
        // to composer history; the test exercises rule C.
        let mut app = make_app();
        app.composer.set_text("msg1");
        app.composer.handle_key(key(KeyCode::Enter));
        app.composer.set_text("msg2");
        app.composer.handle_key(key(KeyCode::Enter));

        // Enter history browse mode.
        app.handle_key(key(KeyCode::Up)).unwrap(); // -> msg2
        app.handle_key(key(KeyCode::Up)).unwrap(); // -> msg1
        assert!(app.composer.is_browsing_history());
        assert_eq!(app.composer.text(), "msg1");

        // Down must keep navigating history, not scroll.
        app.handle_key(key(KeyCode::Down)).unwrap();
        assert_eq!(app.composer.text(), "msg2");
        assert_eq!(app.scroll_offset, 0);
    }

    // --- Escape hatch: Ctrl+P / Ctrl+N always navigate history ---

    #[test]
    fn ctrl_p_navigates_history_even_when_transcript_is_scrolled() {
        let mut app = app_with_scrollable_transcript();
        app.composer.set_text("msg1");
        app.composer.handle_key(key(KeyCode::Enter));
        app.scroll_offset = 10;
        app.auto_scroll = false;

        let ctrl_p = crossterm::event::KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        app.handle_key(ctrl_p).unwrap();

        // History recalled; transcript scroll untouched.
        assert_eq!(app.composer.text(), "msg1");
        assert_eq!(app.scroll_offset, 10);
    }

    // --- Wheel simulation: many Up events starting from bottom ---

    #[test]
    fn wheel_up_simulation_scrolls_transcript_progressively() {
        let mut app = app_with_scrollable_transcript();
        assert_eq!(app.scroll_offset, 0);

        for expected in 1..=10 {
            app.handle_key(key(KeyCode::Up)).unwrap();
            assert_eq!(
                app.scroll_offset, expected,
                "each Up (wheel tick) should advance scroll by 1"
            );
        }
        assert!(!app.auto_scroll);
        assert!(app.composer.is_empty());
    }

    #[test]
    fn wheel_down_simulation_restores_auto_scroll() {
        let mut app = app_with_scrollable_transcript();
        app.scroll_offset = 5;
        app.auto_scroll = false;

        for _ in 0..10 {
            app.handle_key(key(KeyCode::Down)).unwrap();
        }

        assert_eq!(app.scroll_offset, 0);
        assert!(app.auto_scroll);
    }

    // --- Regression guards: non-arrow keys unaffected ---

    #[test]
    fn printable_char_still_goes_to_composer_while_scrolled() {
        let mut app = app_with_scrollable_transcript();
        app.scroll_offset = 5;

        let ch = crossterm::event::KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        app.handle_key(ch).unwrap();

        assert_eq!(app.composer.text(), "x");
        assert_eq!(app.scroll_offset, 5, "typing must not move scroll");
    }

    // --- PageUp / PageDown ---

    #[test]
    fn page_up_scrolls_by_20() {
        let mut app = make_app();

        app.handle_key(key(KeyCode::PageUp)).unwrap();
        assert_eq!(app.scroll_offset, 20);
        assert!(!app.auto_scroll);
    }

    #[test]
    fn page_down_scrolls_by_20() {
        let mut app = make_app();
        app.scroll_offset = 40;
        app.auto_scroll = false;

        app.handle_key(key(KeyCode::PageDown)).unwrap();
        assert_eq!(app.scroll_offset, 20);
        assert!(!app.auto_scroll);
    }

    #[test]
    fn page_down_restores_auto_scroll_at_zero() {
        let mut app = make_app();
        app.scroll_offset = 10;
        app.auto_scroll = false;

        app.handle_key(key(KeyCode::PageDown)).unwrap();
        assert_eq!(app.scroll_offset, 0);
        assert!(app.auto_scroll);
    }

    fn qm(text: &str) -> QueuedMessage {
        QueuedMessage {
            text: text.to_string(),
            images: Vec::new(),
        }
    }

    #[test]
    fn test_tab_queues_multiple() {
        let mut app = make_app();
        app.queued_messages.push_back(qm("first"));
        app.queued_messages.push_back(qm("second"));
        app.queued_messages.push_back(qm("third"));

        assert_eq!(app.queued_messages.len(), 3);
        assert_eq!(app.pop_next_queued().unwrap().text, "first");
        assert_eq!(app.pop_next_queued().unwrap().text, "second");
        assert_eq!(app.pop_next_queued().unwrap().text, "third");
        assert!(app.pop_next_queued().is_none());
    }

    #[test]
    fn test_esc_clears_queue() {
        let mut app = make_app();
        app.queued_messages.push_back(qm("a"));
        app.queued_messages.push_back(qm("b"));

        app.queued_messages.clear();

        assert!(app.queued_messages.is_empty());
    }

    #[test]
    fn test_alt_up_pops_last() {
        let mut app = make_app();
        app.queued_messages.push_back(qm("a"));
        app.queued_messages.push_back(qm("b"));
        app.queued_messages.push_back(qm("c"));

        let last = app.queued_messages.pop_back().unwrap();
        assert_eq!(last.text, "c");
        assert_eq!(app.queued_messages.len(), 2);
        assert_eq!(app.queued_messages[0].text, "a");
        assert_eq!(app.queued_messages[1].text, "b");
    }

    #[test]
    fn test_drain_pops_front() {
        let mut app = make_app();
        app.queued_messages.push_back(qm("first"));
        app.queued_messages.push_back(qm("second"));

        let popped = app.pop_next_queued().unwrap();
        assert_eq!(popped.text, "first");
        assert_eq!(app.queued_messages.len(), 1);
        assert_eq!(app.queued_messages[0].text, "second");
    }

    #[test]
    fn test_queue_preview_height() {
        let mut app = make_app();

        // Empty queue
        assert_eq!(app.compute_queue_preview_height(), 0);

        // 1 message: header(1) + shown(1) + hint(1) = 3
        app.queued_messages.push_back(qm("a"));
        assert_eq!(app.compute_queue_preview_height(), 3);

        // 3 messages: header(1) + shown(3) + hint(1) = 5
        app.queued_messages.push_back(qm("b"));
        app.queued_messages.push_back(qm("c"));
        assert_eq!(app.compute_queue_preview_height(), 5);

        // 5 messages: header(1) + shown(3) + overflow(1) + hint(1) = 6
        app.queued_messages.push_back(qm("d"));
        app.queued_messages.push_back(qm("e"));
        assert_eq!(app.compute_queue_preview_height(), 6);
    }

    // --- Interrupt preserves queued messages ---

    #[test]
    fn interrupt_restores_queued_to_composer() {
        let mut app = make_app();
        app.state = AppState::Streaming {
            start: Instant::now(),
        };
        app.cancel_token = Some(CancellationToken::new());
        app.queued_messages.push_back(qm("msg1"));
        app.queued_messages.push_back(qm("msg2"));
        app.queued_messages.push_back(qm("msg3"));

        app.handle_key(key(KeyCode::Esc)).unwrap();

        assert!(app.queued_messages.is_empty());
        assert_eq!(app.composer.text(), "msg1\nmsg2\nmsg3");
        let sys = last_system_text(&app).unwrap();
        assert!(sys.contains("restored to composer"));
    }

    #[test]
    fn interrupt_empty_queue_shows_interrupted() {
        let mut app = make_app();
        app.state = AppState::Streaming {
            start: Instant::now(),
        };
        app.cancel_token = Some(CancellationToken::new());

        app.handle_key(key(KeyCode::Esc)).unwrap();

        let sys = last_system_text(&app).unwrap();
        assert_eq!(sys, "[interrupted]");
    }

    // --- /cancel slash command ---

    #[test]
    fn cmd_cancel_with_in_flight_token_cancels_and_clears() {
        let mut app = make_app();
        let token = CancellationToken::new();
        app.cancel_token = Some(token.clone());

        let action = app.cmd_cancel().unwrap();
        assert!(matches!(action, AppAction::Continue));
        assert!(token.is_cancelled());
        assert!(app.cancel_token.is_none());
        let sys = last_system_text(&app).unwrap();
        assert_eq!(sys, "[cancelled]");
    }

    #[test]
    fn cmd_cancel_without_in_flight_token_reports_nothing() {
        let mut app = make_app();
        app.cancel_token = None;

        let action = app.cmd_cancel().unwrap();
        assert!(matches!(action, AppAction::Continue));
        let sys = last_system_text(&app).unwrap();
        assert_eq!(sys, "Nothing to cancel.");
    }

    #[test]
    fn cancel_typed_mid_stream_interrupts_instead_of_steering() {
        // /cancel typed into the composer during streaming must not be sent
        // as a mid-turn steer — it should behave the same as pressing Esc.
        let mut app = make_app();
        app.state = AppState::Streaming {
            start: Instant::now(),
        };
        let token = CancellationToken::new();
        app.cancel_token = Some(token.clone());
        app.composer.set_text("/cancel");

        app.handle_key(key(KeyCode::Enter)).unwrap();

        assert!(token.is_cancelled());
        assert!(app.cancel_token.is_none());
        assert!(matches!(app.state, AppState::Idle));
        let sys = last_system_text(&app).unwrap();
        assert_eq!(sys, "[cancelled]");
    }

    #[test]
    fn interrupt_with_existing_composer_text() {
        let mut app = make_app();
        app.state = AppState::Streaming {
            start: Instant::now(),
        };
        app.cancel_token = Some(CancellationToken::new());
        app.composer.set_text("draft");
        app.queued_messages.push_back(qm("queued"));

        app.handle_key(key(KeyCode::Esc)).unwrap();

        assert_eq!(app.composer.text(), "draft\nqueued");
    }

    // --- Channel close robustness ---

    #[test]
    fn channel_close_with_queue_pauses() {
        let mut app = make_app();
        app.state = AppState::Streaming {
            start: Instant::now(),
        };
        app.queued_messages.push_back(qm("pending"));

        app.handle_agent_channel_closed();

        assert!(app.last_turn_errored);
        assert_eq!(app.queued_messages.len(), 1);
        let sys = last_system_text(&app).unwrap();
        assert!(sys.contains("queue paused"));
    }

    #[test]
    fn channel_close_empty_queue_no_error() {
        let mut app = make_app();
        app.state = AppState::Streaming {
            start: Instant::now(),
        };

        app.handle_agent_channel_closed();

        assert!(!app.last_turn_errored);
    }

    // --- Backtrack mode ---

    #[test]
    fn backtrack_enters_on_esc_empty_composer() {
        let mut app = make_app();
        app.cells.push(HistoryCell::User {
            text: "hello".to_string(),
        });
        app.cells.push(HistoryCell::Assistant {
            text: "hi".to_string(),
            streaming: false,
        });
        app.cells.push(HistoryCell::User {
            text: "world".to_string(),
        });

        app.handle_key(key(KeyCode::Esc)).unwrap();

        assert!(matches!(
            app.backtrack,
            BacktrackPhase::Selecting { cursor: 0, .. }
        ));
    }

    #[test]
    fn backtrack_no_user_messages_noop() {
        let mut app = make_app();
        app.cells.push(HistoryCell::System {
            text: "welcome".to_string(),
        });

        app.handle_key(key(KeyCode::Esc)).unwrap();

        assert!(matches!(app.backtrack, BacktrackPhase::Inactive));
    }

    #[test]
    fn backtrack_esc_cancels() {
        let mut app = make_app();
        app.cells.push(HistoryCell::User {
            text: "hello".to_string(),
        });

        app.handle_key(key(KeyCode::Esc)).unwrap(); // enter backtrack
        assert!(matches!(app.backtrack, BacktrackPhase::Selecting { .. }));

        app.handle_key(key(KeyCode::Esc)).unwrap(); // cancel
        assert!(matches!(app.backtrack, BacktrackPhase::Inactive));
    }

    #[test]
    fn backtrack_navigate_and_select() {
        let mut app = make_app();
        app.cells.push(HistoryCell::User {
            text: "first".to_string(),
        });
        app.cells.push(HistoryCell::Assistant {
            text: "resp1".to_string(),
            streaming: false,
        });
        app.cells.push(HistoryCell::User {
            text: "second".to_string(),
        });
        app.cells.push(HistoryCell::Assistant {
            text: "resp2".to_string(),
            streaming: false,
        });

        // Enter backtrack (cursor starts at 0 = most recent)
        app.handle_key(key(KeyCode::Esc)).unwrap();

        // Navigate up to select first (older) message
        app.handle_key(key(KeyCode::Up)).unwrap();

        // Select it
        let action = app.handle_key(key(KeyCode::Enter)).unwrap();

        assert!(matches!(
            action,
            AppAction::RewindTo {
                nth_user_message: 0
            }
        ));
        assert_eq!(app.composer.text(), "first");
        // Cells should be truncated to before the first user message
        assert_eq!(app.cells.len(), 0);
        assert!(matches!(app.backtrack, BacktrackPhase::Inactive));
    }

    #[test]
    fn backtrack_not_triggered_with_text_in_composer() {
        let mut app = make_app();
        app.cells.push(HistoryCell::User {
            text: "hello".to_string(),
        });
        app.composer.set_text("draft");

        // Esc with text in composer should NOT enter backtrack — it goes to composer's Esc handler
        app.handle_key(key(KeyCode::Esc)).unwrap();
        assert!(matches!(app.backtrack, BacktrackPhase::Inactive));
    }

    // --- /tools command ---

    fn last_system_text(app: &App) -> Option<String> {
        app.cells.iter().rev().find_map(|c| match c {
            HistoryCell::System { text } => Some(text.clone()),
            _ => None,
        })
    }

    #[test]
    fn preparing_event_adds_thinking_cell() {
        let mut app = make_app();
        app.process_agent_event(AgentEvent::Preparing);
        assert!(matches!(
            app.cells.last(),
            Some(HistoryCell::Thinking { text }) if text.is_empty()
        ));
    }

    #[test]
    fn text_delta_removes_preparing_placeholder() {
        let mut app = make_app();
        app.process_agent_event(AgentEvent::Preparing);
        app.process_agent_event(AgentEvent::TextDelta("Hello".into()));
        // The empty Thinking cell should be gone, replaced with Assistant
        assert!(!matches!(
            app.cells.first(),
            Some(HistoryCell::Thinking { text }) if text.is_empty()
        ));
        assert!(matches!(
            app.cells.last(),
            Some(HistoryCell::Assistant { text, .. }) if text == "Hello"
        ));
    }

    #[test]
    fn text_delta_preserves_nonempty_thinking() {
        let mut app = make_app();
        app.cells.push(HistoryCell::Thinking {
            text: "some thought".into(),
        });
        app.process_agent_event(AgentEvent::TextDelta("Hello".into()));
        // Thinking cell should still be there (non-empty, not a placeholder)
        assert!(matches!(
            &app.cells[0],
            HistoryCell::Thinking { text } if text == "some thought"
        ));
        assert!(matches!(
            app.cells.last(),
            Some(HistoryCell::Assistant { text, .. }) if text == "Hello"
        ));
    }

    #[test]
    fn preparing_then_thinking_delta_appends() {
        let mut app = make_app();
        app.process_agent_event(AgentEvent::Preparing);
        app.process_agent_event(AgentEvent::ThinkingDelta("reasoning...".into()));
        assert_eq!(app.cells.len(), 1);
        assert!(matches!(
            app.cells.last(),
            Some(HistoryCell::Thinking { text }) if text == "reasoning..."
        ));
    }

    // --- Slash command handling ---

    #[test]
    fn unknown_slash_command_rejected() {
        let mut app = make_app();
        let result = app.handle_submit("/foobar").unwrap();
        assert!(matches!(result, AppAction::Continue));
        let has_unknown_msg = app.cells.iter().any(|c| {
            matches!(
                c, HistoryCell::System { text } if text.contains("Unknown command")
            )
        });
        assert!(has_unknown_msg, "should show unknown command message");
    }

    #[test]
    fn unknown_slash_command_not_sent_to_agent() {
        let mut app = make_app();
        app.handle_submit("/notacommand").unwrap();
        // Should NOT create a User cell (which would mean it was sent to the agent)
        let has_user_cell = app
            .cells
            .iter()
            .any(|c| matches!(c, HistoryCell::User { .. }));
        assert!(
            !has_user_cell,
            "unknown slash command should not be sent to agent"
        );
    }

    #[test]
    fn help_command_works() {
        let mut app = make_app();
        let result = app.handle_submit("/help").unwrap();
        assert!(matches!(result, AppAction::Continue));
        let has_help = app.cells.iter().any(|c| {
            matches!(
                c, HistoryCell::System { text } if text.contains("/help")
            )
        });
        assert!(has_help, "should show help text");
    }

    #[test]
    fn mode_prefix_command_not_rejected() {
        let mut app = make_app();
        let result = app.handle_submit("/mode execute").unwrap();
        assert!(matches!(result, AppAction::Continue));
        // Should show mode change message, not "Unknown command"
        let has_unknown = app.cells.iter().any(|c| {
            matches!(
                c, HistoryCell::System { text } if text.contains("Unknown command")
            )
        });
        assert!(
            !has_unknown,
            "/mode execute should not be treated as unknown"
        );
    }

    #[test]
    fn load_prefix_command_not_rejected() {
        let mut app = make_app();
        let result = app.handle_submit("/load abc123").unwrap();
        // /load returns LoadSession action, not Continue
        assert!(matches!(result, AppAction::LoadSession { .. }));
    }

    #[test]
    fn settings_prefix_not_rejected() {
        let mut app = make_app();
        let result = app.handle_submit("/settings temperature 0.5").unwrap();
        // Should either update or error, not "Unknown command"
        let has_unknown = app.cells.iter().any(|c| {
            matches!(
                c, HistoryCell::System { text } if text.contains("Unknown command")
            )
        });
        assert!(
            !has_unknown,
            "/settings key value should not be treated as unknown"
        );
    }

    #[test]
    fn plan_prefix_not_rejected() {
        let mut app = make_app();
        let _result = app.handle_submit("/plan hello").unwrap();
        let has_unknown = app.cells.iter().any(|c| {
            matches!(
                c, HistoryCell::System { text } if text.contains("Unknown command")
            )
        });
        assert!(!has_unknown, "/plan <msg> should not be treated as unknown");
    }

    #[test]
    fn plan_toggle_sets_collaboration_mode_to_plan() {
        let mut app = make_app();
        assert_eq!(
            app.config.conversation.collaboration_mode,
            CollaborationMode::Default
        );
        let _ = app.handle_submit("/plan").unwrap();
        assert_eq!(
            app.config.conversation.collaboration_mode,
            CollaborationMode::Plan,
            "/plan should switch collaboration mode to Plan"
        );
        assert_eq!(
            app.previous_collab_mode,
            Some(CollaborationMode::Default),
            "/plan should stash the previous mode for restore"
        );
    }

    #[test]
    fn plan_toggle_twice_restores_previous_mode() {
        let mut app = make_app();
        app.config.conversation.collaboration_mode = CollaborationMode::Execute;
        let _ = app.handle_submit("/plan").unwrap();
        assert_eq!(
            app.config.conversation.collaboration_mode,
            CollaborationMode::Plan
        );
        let _ = app.handle_submit("/plan").unwrap();
        assert_eq!(
            app.config.conversation.collaboration_mode,
            CollaborationMode::Execute,
            "toggling /plan a second time should restore the prior mode"
        );
        assert_eq!(app.previous_collab_mode, None);
    }

    #[test]
    fn mode_plan_stashes_previous_mode() {
        let mut app = make_app();
        app.config.conversation.collaboration_mode = CollaborationMode::Execute;
        let _ = app.handle_submit("/mode plan").unwrap();
        assert_eq!(
            app.config.conversation.collaboration_mode,
            CollaborationMode::Plan
        );
        assert_eq!(app.previous_collab_mode, Some(CollaborationMode::Execute));
    }

    #[test]
    fn mode_switch_away_from_plan_clears_previous() {
        let mut app = make_app();
        let _ = app.handle_submit("/plan").unwrap();
        assert_eq!(app.previous_collab_mode, Some(CollaborationMode::Default));
        let _ = app.handle_submit("/mode execute").unwrap();
        assert_eq!(
            app.config.conversation.collaboration_mode,
            CollaborationMode::Execute
        );
        assert_eq!(
            app.previous_collab_mode, None,
            "leaving Plan via /mode should clear the stashed prior mode"
        );
    }

    #[test]
    fn turn_complete_in_plan_mode_opens_review_overlay() {
        let mut app = make_app();
        app.config.conversation.collaboration_mode = CollaborationMode::Plan;
        app.process_agent_event(AgentEvent::TurnComplete);
        assert!(
            matches!(app.state, AppState::PlanReview),
            "TurnComplete while in Plan mode should enter PlanReview"
        );
        assert_eq!(
            app.config.conversation.collaboration_mode,
            CollaborationMode::Plan,
            "mode should stay Plan until the user picks Proceed on the overlay"
        );
    }

    #[test]
    fn turn_complete_in_default_mode_returns_to_idle() {
        let mut app = make_app();
        app.process_agent_event(AgentEvent::TurnComplete);
        assert!(matches!(app.state, AppState::Idle));
    }

    // --- Update command ---

    #[test]
    fn update_command_returns_self_update() {
        let mut app = make_app();
        let result = app.handle_submit("/update").unwrap();
        assert!(
            matches!(result, AppAction::SelfUpdate { dev: false }),
            "/update should return SelfUpdate {{ dev: false }}"
        );
    }

    #[test]
    fn update_dev_command_returns_self_update_dev() {
        let mut app = make_app();
        let result = app.handle_submit("/update --dev").unwrap();
        assert!(
            matches!(result, AppAction::SelfUpdate { dev: true }),
            "/update --dev should return SelfUpdate {{ dev: true }}"
        );

        let mut app2 = make_app();
        let result2 = app2.handle_submit("/update dev").unwrap();
        assert!(
            matches!(result2, AppAction::SelfUpdate { dev: true }),
            "/update dev should return SelfUpdate {{ dev: true }}"
        );
    }

    // --- Status popup integration ---

    #[test]
    fn status_popup_opens_on_slash_status() {
        let mut app = make_app();
        assert!(!app.status_popup.is_visible());
        let _result = app.handle_submit("/status").unwrap();
        assert!(app.status_popup.is_visible());
    }

    #[test]
    fn status_popup_esc_closes() {
        let mut app = make_app();
        let _ = app.handle_submit("/status").unwrap();
        assert!(app.status_popup.is_visible());
        app.handle_key(key(KeyCode::Esc)).unwrap();
        assert!(!app.status_popup.is_visible());
    }

    #[test]
    fn status_popup_consumes_keys() {
        let mut app = make_app();
        let _ = app.handle_submit("/status").unwrap();
        assert!(app.status_popup.is_visible());
        // A random key should be consumed (not affect scroll_offset on app)
        let initial_scroll = app.scroll_offset;
        app.handle_key(key(KeyCode::Char('a'))).unwrap();
        assert_eq!(app.scroll_offset, initial_scroll);
        assert!(app.status_popup.is_visible());
    }

    // --- /uninstall ---

    #[test]
    fn uninstall_shows_warning_and_enters_confirming_state() {
        let mut app = make_app();
        let action = app.handle_submit("/uninstall").unwrap();
        assert!(matches!(action, AppAction::Continue));
        assert!(matches!(app.state, AppState::ConfirmingUninstall));
        assert!(app
            .cells
            .iter()
            .any(|c| matches!(c, HistoryCell::System { text } if text.contains("WARNING"))));
    }

    #[test]
    fn uninstall_confirm_y_returns_uninstall_action() {
        let mut app = make_app();
        app.state = AppState::ConfirmingUninstall;
        let action = app
            .handle_key(crossterm::event::KeyEvent::from(
                crossterm::event::KeyCode::Char('y'),
            ))
            .unwrap();
        assert!(matches!(action, AppAction::Uninstall));
    }

    #[test]
    fn uninstall_confirm_n_returns_to_idle() {
        let mut app = make_app();
        app.state = AppState::ConfirmingUninstall;
        let action = app
            .handle_key(crossterm::event::KeyEvent::from(
                crossterm::event::KeyCode::Char('n'),
            ))
            .unwrap();
        assert!(matches!(action, AppAction::Continue));
        assert!(matches!(app.state, AppState::Idle));
        assert!(app
            .cells
            .iter()
            .any(|c| matches!(c, HistoryCell::System { text } if text.contains("cancelled"))));
    }

    #[test]
    fn uninstall_enter_defaults_to_cancel() {
        let mut app = make_app();
        app.state = AppState::ConfirmingUninstall;
        let action = app
            .handle_key(crossterm::event::KeyEvent::from(
                crossterm::event::KeyCode::Enter,
            ))
            .unwrap();
        assert!(matches!(action, AppAction::Continue));
        assert!(matches!(app.state, AppState::Idle));
    }

    #[test]
    fn uninstall_not_in_autocomplete() {
        let popup = crate::tui::command_popup::CommandPopup::new();
        let all = popup.filtered();
        assert!(!all.iter().any(|c| c.name == "/uninstall"));
    }

    #[test]
    fn uninstall_partial_not_in_autocomplete() {
        let mut popup = crate::tui::command_popup::CommandPopup::new();
        popup.update_filter("/unins");
        let matches = popup.filtered();
        assert!(matches.is_empty());
    }

    // --- /poke command ---

    #[test]
    fn poke_with_channel_sends_and_continues() {
        let config = Config::default();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let mut app = App::new(config, None, None, Some(tx));
        let result = app.try_handle_command("/poke");
        assert!(result.is_some());
        let action = result.unwrap().unwrap();
        assert!(matches!(action, AppAction::Continue));
        let sys = last_system_text(&app).unwrap();
        assert!(sys.contains("heartbeat triggered"));
    }

    #[test]
    fn poke_without_channel_returns_poke_action() {
        let mut app = make_app(); // poke_tx is None
        let result = app.try_handle_command("/poke");
        assert!(result.is_some());
        let action = result.unwrap().unwrap();
        assert!(matches!(action, AppAction::Poke));
        let sys = last_system_text(&app).unwrap();
        assert!(sys.contains("sending to daemon"));
    }

    #[test]
    fn poke_full_channel_shows_pending_message() {
        let config = Config::default();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        // Fill the channel
        tx.try_send(()).unwrap();
        let mut app = App::new(config, None, None, Some(tx));
        let result = app.try_handle_command("/poke");
        assert!(result.is_some());
        let action = result.unwrap().unwrap();
        assert!(matches!(action, AppAction::Continue));
        let sys = last_system_text(&app).unwrap();
        assert!(sys.contains("already pending"));
    }
}
