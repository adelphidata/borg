//! Slash command handlers extracted from `App::handle_submit`.
//!
//! Each method corresponds to a TUI slash command (e.g., `/memory`, `/pairing`).

use anyhow::Result;

use borg_core::config::CollaborationMode;

use super::app::{App, AppAction, AppState};

impl App<'_> {
    /// Try to handle `input` as a slash command. Returns `Some(action)` if it was a command,
    /// `None` if it should be treated as a user message.
    pub(super) fn try_handle_command(&mut self, trimmed: &str) -> Option<Result<AppAction>> {
        // Exact matches
        // Exact matches
        match trimmed {
            "quit" | "exit" | "/exit" => return Some(Ok(AppAction::Quit)),
            "help" | "/help" => return Some(self.cmd_help()),
            "/memory" => return Some(self.cmd_memory()),
            "/skills" => {
                self.skills_popup.show(&self.config);
                return Some(Ok(AppAction::Continue));
            }
            "/history" => return Some(self.cmd_history()),
            "/logs" => return Some(self.handle_logs_command(None)),
            "/status" | "/stats" => {
                self.status_popup.show(&self.config);
                return Some(Ok(AppAction::Continue));
            }
            "/doctor" => return Some(Ok(AppAction::RunDoctor)),
            "/update" => return Some(Ok(AppAction::SelfUpdate { dev: false })),
            "/pairing" => return Some(self.cmd_pairing()),
            "/settings" => {
                self.settings_popup.show(&self.config);
                return Some(Ok(AppAction::Continue));
            }
            "/plugins" => return Some(self.cmd_plugins()),
            "/schedule-tasks" | "/schedule" => {
                self.schedule_popup.show();
                return Some(Ok(AppAction::Continue));
            }
            "/migrate" => {
                self.migrate_popup.show();
                return Some(Ok(AppAction::Continue));
            }
            "/poke" => return Some(self.cmd_poke()),
            "/cancel" | "/stop" | "/abort" => return Some(self.cmd_cancel()),
            "/restart" => return Some(Ok(AppAction::RestartGateway)),
            "/compact" => return Some(Ok(AppAction::CompactHistory)),
            "/clear" => {
                self.cells.clear();
                self.queued_messages.clear();
                return Some(Ok(AppAction::ClearHistory));
            }
            "/usage" => return Some(Ok(AppAction::ShowUsage)),
            "/undo" => return Some(Ok(AppAction::UndoLastTurn)),
            "/sessions" => return Some(Ok(AppAction::ListSessions)),
            "/save" => return Some(Ok(AppAction::SaveSession)),
            "/new" => return Some(self.cmd_new()),
            _ => {}
        }

        // Prefix commands
        if let Some(rest) = trimmed.strip_prefix("/logs ") {
            return Some(self.handle_logs_command(Some(rest.trim())));
        }

        if trimmed == "/mode" {
            return Some(self.cmd_mode_show());
        }
        if let Some(rest) = trimmed.strip_prefix("/mode ") {
            return Some(self.cmd_mode_set(rest.trim()));
        }

        if trimmed == "/plan" {
            return Some(self.cmd_plan_toggle());
        }
        if let Some(rest) = trimmed.strip_prefix("/plan ") {
            return Some(self.cmd_plan_with_message(rest.trim()));
        }

        if trimmed == "/memory cleanup" {
            return Some(self.cmd_memory_cleanup());
        }

        if let Some(rest) = trimmed.strip_prefix("/load ") {
            return Some(self.cmd_load_session(rest.trim()));
        }

        if let Some(rest) = trimmed.strip_prefix("/settings ") {
            return Some(self.cmd_settings_set(rest));
        }

        if trimmed == "/update --dev" || trimmed == "/update dev" {
            return Some(Ok(AppAction::SelfUpdate { dev: true }));
        }

        if trimmed == "/uninstall" {
            return Some(self.cmd_uninstall());
        }

        // Reject unknown slash commands
        if trimmed.starts_with('/') {
            self.push_system_message(format!(
                "Unknown command: {trimmed}\nType /help for available commands."
            ));
            return Some(Ok(AppAction::Continue));
        }

        None // Not a command
    }

    fn cmd_help(&mut self) -> Result<AppAction> {
        self.push_system_message(
            "Commands:\n  \
             /help      - Show this help\n  \
             /settings  - Configure settings\n  \
             /usage     - Show usage stats\n  \
             /mode      - Switch collaboration mode (default/execute/plan)\n  \
             /plan      - Shortcut for /mode plan (toggles read-only plan mode)\n\
             \n  \
             /compact   - Compact conversation history\n  \
             /clear     - Clear conversation\n  \
             /cancel    - Stop the current in-progress turn\n  \
             /undo      - Undo last agent turn\n\
             \n  \
             /memory    - Show memory\n  \
             /skills    - List skills\n  \
             /history   - Show conversation history\n  \
             /logs      - Show activity log (error|warn|info|debug|all|raw)\n  \
             /doctor    - Run diagnostics\n  \
             /status    - Show agent vitals\n  \
             /poke      - Trigger immediate heartbeat\n  \
             /pairing   - Show channel pairing info\n  \
             /update    - Update borg to latest version\n\
             \n  \
             /sessions  - Browse saved sessions\n  \
             /save      - Save current session\n  \
             /new       - Start new session\n  \
             /load      - Load a saved session by ID\n\
             \n  \
             /plugins   - Browse integrations\n  \
             /schedule  - Manage scheduled tasks\n  \
             /migrate   - Import from another agent\n  \
             /restart   - Restart gateway server\n\
             \n  \
             /uninstall - Remove all borg data and binary\n  \
             quit/exit  - Exit"
                .to_string(),
        );
        Ok(AppAction::Continue)
    }

    /// Cancel the in-progress agent turn, if any. Equivalent to pressing Esc
    /// during streaming — provided as an explicit command so messaging-channel
    /// users (who can't press Esc) have a consistent verb across surfaces.
    pub(super) fn cmd_cancel(&mut self) -> Result<AppAction> {
        if let Some(token) = self.cancel_token.take() {
            token.cancel();
            self.event_rx = None;
            self.steer_tx = None;
            self.pending_steers.clear();
            self.push_system_message("[cancelled]".to_string());
            Ok(AppAction::Continue)
        } else {
            self.push_system_message("Nothing to cancel.".to_string());
            Ok(AppAction::Continue)
        }
    }

    fn cmd_memory(&mut self) -> Result<AppAction> {
        let memory = borg_core::memory::load_memory_context(self.config.memory.max_context_tokens)?;
        let text = if memory.is_empty() {
            "No memories loaded.".to_string()
        } else {
            memory
        };
        self.push_system_message(text);
        Ok(AppAction::Continue)
    }

    fn cmd_history(&mut self) -> Result<AppAction> {
        match borg_core::logging::read_history_formatted(50, false) {
            Ok(lines) => {
                let text = if lines.is_empty() {
                    "No conversation history for today.".to_string()
                } else {
                    lines.join("\n")
                };
                self.push_system_message(text);
            }
            Err(e) => {
                self.push_system_message(format!("Error reading history: {e}"));
            }
        }
        Ok(AppAction::Continue)
    }

    fn cmd_pairing(&mut self) -> Result<AppAction> {
        let mut output = String::from("Sender Pairing\n");
        output.push_str("────────────────────────────────\n\n");
        match borg_core::db::Database::open() {
            Ok(db) => {
                output.push_str("Pending Requests\n");
                match db.list_pairings(None) {
                    Ok(requests) => {
                        if requests.is_empty() {
                            output.push_str("  No pending requests.\n");
                        } else {
                            for r in &requests {
                                output.push_str(&format!(
                                    "  {} | {} | {}\n    → borg pairing approve {} {}\n",
                                    r.channel_name, r.sender_id, r.code, r.channel_name, r.code
                                ));
                            }
                        }
                    }
                    Err(e) => output.push_str(&format!("  Error: {e}\n")),
                }
                output.push_str("\nApproved Senders\n");
                match db.list_approved_senders(None) {
                    Ok(senders) => {
                        if senders.is_empty() {
                            output.push_str("  No approved senders.\n");
                        } else {
                            for s in &senders {
                                let name = s.display_name.as_deref().unwrap_or("—");
                                output.push_str(&format!(
                                    "  {} | {} | {}\n",
                                    s.channel_name, s.sender_id, name
                                ));
                            }
                        }
                    }
                    Err(e) => output.push_str(&format!("  Error: {e}\n")),
                }
            }
            Err(e) => output.push_str(&format!("Database error: {e}\n")),
        }
        self.push_system_message(output);
        Ok(AppAction::Continue)
    }

    fn cmd_plugins(&mut self) -> Result<AppAction> {
        if let Ok(data_dir) = borg_core::config::Config::data_dir() {
            self.plugins_popup.show(&data_dir);
        } else {
            self.push_system_message("Error: could not determine data directory".to_string());
        }
        Ok(AppAction::Continue)
    }

    fn cmd_new(&mut self) -> Result<AppAction> {
        self.cells.clear();
        self.queued_messages.clear();
        self.session_prompt_tokens = 0;
        self.session_completion_tokens = 0;
        Ok(AppAction::NewSession)
    }

    fn cmd_mode_show(&mut self) -> Result<AppAction> {
        let current = self.config.conversation.collaboration_mode;
        self.push_system_message(format!(
            "Current collaboration mode: {current}\nUsage: /mode <default|execute|plan>"
        ));
        Ok(AppAction::Continue)
    }

    fn cmd_mode_set(&mut self, mode_str: &str) -> Result<AppAction> {
        match mode_str.parse::<CollaborationMode>() {
            Ok(mode) => {
                self.set_collaboration_mode(mode);
            }
            Err(e) => {
                self.push_system_message(format!("Error: {e}"));
            }
        }
        Ok(AppAction::Continue)
    }

    /// `/plan` toggles between Plan mode and the previously active mode.
    ///
    /// `/plan` is the shortcut entry point for `/mode plan`; both wire through
    /// the same `CollaborationMode::Plan` state so there is only one source of
    /// truth. Entering Plan stashes the prior mode so the post-turn review
    /// overlay can restore it when the user chooses "Proceed".
    fn cmd_plan_toggle(&mut self) -> Result<AppAction> {
        let current = self.config.conversation.collaboration_mode;
        let next = if current == CollaborationMode::Plan {
            self.previous_collab_mode
                .take()
                .unwrap_or(CollaborationMode::Default)
        } else {
            CollaborationMode::Plan
        };
        self.set_collaboration_mode(next);
        Ok(AppAction::Continue)
    }

    fn cmd_plan_with_message(&mut self, message: &str) -> Result<AppAction> {
        self.set_collaboration_mode(CollaborationMode::Plan);
        if message.is_empty() {
            return Ok(AppAction::Continue);
        }
        self.handle_submit(message)
    }

    /// Switch collaboration mode, maintaining `previous_collab_mode` so the
    /// Plan review overlay can restore the prior mode on "Proceed".
    fn set_collaboration_mode(&mut self, next: CollaborationMode) {
        let current = self.config.conversation.collaboration_mode;
        if next == current {
            self.push_system_message(format!("[collaboration mode: {next}]"));
            return;
        }
        if next == CollaborationMode::Plan {
            // Entering Plan: stash the current mode so Proceed can restore it.
            self.previous_collab_mode = Some(current);
        } else {
            // Leaving Plan (or switching between non-Plan modes): the stashed
            // mode is stale and must be cleared.
            self.previous_collab_mode = None;
        }
        self.config.conversation.collaboration_mode = next;
        self.push_system_message(format!("[collaboration mode: {next}]"));
    }

    fn cmd_memory_cleanup(&mut self) -> Result<AppAction> {
        match borg_core::memory::list_memory_files() {
            Ok(files) => {
                if files.is_empty() {
                    self.push_system_message("No memory files found.".to_string());
                } else {
                    let mut text = String::from("Memory files (oldest first):\n");
                    for f in &files {
                        let modified = f
                            .modified_at
                            .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                            .unwrap_or_else(|| "unknown".to_string());
                        text.push_str(&format!(
                            "  {} ({} bytes, modified: {modified})\n",
                            f.filename, f.size_bytes
                        ));
                    }
                    text.push_str("\nTo delete a memory file, ask the agent to use write_memory.");
                    self.push_system_message(text.trim_end().to_string());
                }
            }
            Err(e) => {
                self.push_system_message(format!("Error listing memory files: {e}"));
            }
        }
        Ok(AppAction::Continue)
    }

    fn cmd_load_session(&mut self, id_str: &str) -> Result<AppAction> {
        let id = id_str.to_string();
        if id.is_empty() {
            self.push_system_message("Usage: /load <session_id>".to_string());
            return Ok(AppAction::Continue);
        }
        self.cells.clear();
        self.session_prompt_tokens = 0;
        self.session_completion_tokens = 0;
        Ok(AppAction::LoadSession { id })
    }

    fn cmd_settings_set(&mut self, rest: &str) -> Result<AppAction> {
        let parts: Vec<&str> = rest.splitn(2, ' ').collect();
        if parts.len() == 2 {
            let key = parts[0].to_string();
            let value = parts[1].to_string();
            match self.config.apply_setting(&key, &value) {
                Ok(confirmation) => {
                    self.push_system_message(format!("Updated: {confirmation}"));
                    if let Err(e) = self.config.save() {
                        self.push_system_message(format!("Warning: failed to save config: {e}"));
                    }
                    Ok(AppAction::UpdateSetting { key, value })
                }
                Err(e) => {
                    self.push_system_message(format!("Error: {e}"));
                    Ok(AppAction::Continue)
                }
            }
        } else {
            self.push_system_message(
                "Usage: /settings <key> <value>\nUse /settings to see current values.".to_string(),
            );
            Ok(AppAction::Continue)
        }
    }

    fn cmd_poke(&mut self) -> Result<AppAction> {
        if let Some(tx) = &self.poke_tx {
            match tx.try_send(()) {
                Ok(()) => {
                    self.push_system_message("[poke: heartbeat triggered]".to_string());
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    self.push_system_message("[poke: heartbeat already pending]".to_string());
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    self.push_system_message("[poke: scheduler not running]".to_string());
                }
            }
            Ok(AppAction::Continue)
        } else {
            // Daemon mode: no local scheduler, send via HTTP
            self.push_system_message("[poke: sending to daemon...]".to_string());
            Ok(AppAction::Poke)
        }
    }

    fn cmd_uninstall(&mut self) -> Result<AppAction> {
        self.push_system_message(
            "⚠ WARNING: This will permanently delete all Borg data (~/.borg/)\n\
             including config, memory, tools, skills, channels, database,\n\
             and remove the binary.\n\n\
             Proceed with uninstall? (y/N)"
                .to_string(),
        );
        self.state = AppState::ConfirmingUninstall;
        Ok(AppAction::Continue)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn commands_rs_has_stats_and_status_aliases() {
        let src = include_str!("commands.rs");
        // Strip test module to avoid self-matching
        let code = match src.find("#[cfg(test)]") {
            Some(idx) => &src[..idx],
            None => src,
        };
        assert!(
            code.contains("\"/stats\""),
            "commands.rs must handle /stats"
        );
        assert!(
            code.contains("\"/status\""),
            "commands.rs must handle /status"
        );
    }
}
