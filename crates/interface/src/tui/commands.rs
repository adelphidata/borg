//! Slash command handlers extracted from `App::handle_submit`.
//!
//! Each method corresponds to a TUI slash command (e.g., `/memory`, `/pairing`).

use anyhow::Result;

use cortex::config::CollaborationMode;
use cortex::types::Message;

use super::app::{App, AppAction, AppState};
use super::history::HistoryCell;
use super::status_popup::StatusTab;

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
            "/skills" => return Some(self.cmd_plugins()),
            "/history" => return Some(self.cmd_history()),
            "/logs" => return Some(self.handle_logs_command(None)),
            "/status" | "/stats" => {
                let client = self.client.clone();
                self.status_popup.show(&self.config, &client);
                return Some(Ok(AppAction::EnterAltScreen));
            }
            "/xp" => {
                let client = self.client.clone();
                self.status_popup
                    .show_tab(&self.config, StatusTab::Xp, &client);
                return Some(Ok(AppAction::EnterAltScreen));
            }
            "/evolution" => {
                let client = self.client.clone();
                self.status_popup
                    .show_tab(&self.config, StatusTab::Evolution, &client);
                return Some(Ok(AppAction::EnterAltScreen));
            }
            "/card" => {
                self.push_borg_card();
                return Some(Ok(AppAction::Continue));
            }
            "/doctor" => return Some(Ok(AppAction::RunDoctor)),
            "/doctor squelches" => return Some(self.cmd_doctor_squelch_list()),
            "/doctor squelch" | "/doctor unsquelch" => {
                return Some(self.cmd_doctor_squelch_help());
            }
            "/dream" => return Some(self.cmd_dream()),
            "/heal" => return Some(self.cmd_heal()),
            // `/update` and `/update <flags>` are handled by the prefix block below.
            "/pairing" => {
                if let Ok(data_dir) = cortex::config::Config::data_dir() {
                    let client = self.client.clone();
                    self.pairing_popup.show(&self.config, &data_dir, &client);
                    return Some(Ok(AppAction::EnterAltScreen));
                } else {
                    self.push_system_message(
                        "Error: could not determine data directory".to_string(),
                    );
                    return Some(Ok(AppAction::Continue));
                }
            }
            "/settings" => {
                let client = self.client.clone();
                self.settings_popup.show(&self.config, &client);
                return Some(Ok(AppAction::EnterAltScreen));
            }
            "/model" => {
                let client = self.client.clone();
                self.model_popup.show(&self.config, &client);
                return Some(Ok(AppAction::Continue));
            }
            "/plugins" => return Some(self.cmd_plugins()),
            "/projects" => {
                let client = self.client.clone();
                self.projects_popup.show(&client);
                return Some(Ok(AppAction::EnterAltScreen));
            }
            "/schedule-tasks" | "/schedule" => {
                let client = self.client.clone();
                self.schedule_popup.show(&client);
                return Some(Ok(AppAction::EnterAltScreen));
            }
            "/migrate" => {
                self.migrate_popup.show();
                return Some(Ok(AppAction::EnterAltScreen));
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
            "/sessions" => {
                let client = self.client.clone();
                self.sessions_popup.show(&client);
                return Some(Ok(AppAction::EnterAltScreen));
            }
            "/save" => return Some(Ok(AppAction::SaveSession)),
            "/export" => {
                return Some(Ok(AppAction::ExportCurrentSession {
                    format: cortex::export::ExportFormat::Json,
                }));
            }
            "/new" => return Some(self.cmd_new()),
            "/fork" => return Some(self.cmd_fork()),
            "/resume" => {
                self.push_system_message(
                    "Usage: /resume <session-id>\nUse /sessions to browse saved sessions."
                        .to_string(),
                );
                return Some(Ok(AppAction::Continue));
            }
            _ => {}
        }

        if let Some(rest) = trimmed.strip_prefix("/export ") {
            let arg = rest.trim();
            match arg.parse::<cortex::export::ExportFormat>() {
                Ok(format) => {
                    return Some(Ok(AppAction::ExportCurrentSession { format }));
                }
                Err(e) => {
                    self.push_system_message(format!("Error: {e}\nUsage: /export [json|csv|txt]"));
                    return Some(Ok(AppAction::Continue));
                }
            }
        }

        if let Some(rest) = trimmed.strip_prefix("/resume ") {
            let id = rest.trim();
            if id.is_empty() {
                self.push_system_message(
                    "Usage: /resume <session-id>\nUse /sessions to browse saved sessions."
                        .to_string(),
                );
                return Some(Ok(AppAction::Continue));
            }
            self.cells.clear();
            self.queued_messages.clear();
            return Some(Ok(AppAction::LoadSession { id: id.to_string() }));
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

        if trimmed == "/btw" {
            self.push_system_message(
                "Usage: /btw <question>\n  \
                 Ask a side question using the current session's context. \
                 Answer appears in a dismissable popup (Esc). Not persisted."
                    .to_string(),
            );
            return Some(Ok(AppAction::Continue));
        }
        if let Some(rest) = trimmed.strip_prefix("/btw ") {
            return Some(self.cmd_btw(rest.trim()));
        }

        if trimmed == "/memory cleanup" {
            return Some(self.cmd_memory_cleanup());
        }

        if let Some(rest) = trimmed.strip_prefix("/doctor squelch ") {
            return Some(self.cmd_doctor_squelch_set(rest.trim()));
        }
        if let Some(rest) = trimmed.strip_prefix("/doctor unsquelch ") {
            return Some(self.cmd_doctor_squelch_clear(rest.trim()));
        }

        if let Some(rest) = trimmed.strip_prefix("/settings ") {
            return Some(self.cmd_settings_set(rest));
        }

        if let Some(rest) = trimmed.strip_prefix("/model ") {
            return Some(self.cmd_model_set(rest.trim()));
        }

        if trimmed == "/update" || trimmed.starts_with("/update ") {
            let rest = trimmed.strip_prefix("/update").map(str::trim).unwrap_or("");
            return Some(
                match crate::commands::update_args::UpdateArgs::parse_slash(rest) {
                    Ok(args) => Ok(AppAction::SelfUpdate {
                        prerelease: args.prerelease,
                        check: args.check,
                    }),
                    Err(e) => {
                        // clap renders the message; trim trailing newline before showing.
                        let msg = e.to_string();
                        self.push_system_message(format!("/update: {}", msg.trim_end()));
                        Ok(AppAction::Continue)
                    }
                },
            );
        }

        // /pairing approve (no args) — show usage
        if trimmed == "/pairing approve" {
            self.push_system_message(
                "Usage: /pairing approve <code>\n   or: /pairing <code>".to_string(),
            );
            return Some(Ok(AppAction::Continue));
        }

        // /pairing approve <code>
        if let Some(rest) = trimmed.strip_prefix("/pairing approve ") {
            let code = rest.trim();
            if !code.is_empty() {
                return Some(self.cmd_pairing_approve(code));
            }
            self.push_system_message(
                "Usage: /pairing approve <code>\n   or: /pairing <code>".to_string(),
            );
            return Some(Ok(AppAction::Continue));
        }

        // /pairing revoke — redirect to popup
        if trimmed == "/pairing revoke" || trimmed.starts_with("/pairing revoke ") {
            self.push_system_message(
                "Use /pairing to manage sender pairing (press 'd' to revoke)".to_string(),
            );
            return Some(Ok(AppAction::Continue));
        }

        // /pairing <code> (shortcut)
        if let Some(rest) = trimmed.strip_prefix("/pairing ") {
            let code = rest.trim();
            if !code.is_empty() && !code.starts_with('-') {
                return Some(self.cmd_pairing_approve(code));
            }
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
             /btw <q>   - Ask a side question (ephemeral, not persisted)\n  \
             /compact   - Compact conversation history\n  \
             /clear     - Clear conversation\n  \
             /cancel    - Stop the current in-progress turn\n  \
             /undo      - Undo last agent turn\n\
             \n  \
             /memory    - Show memory\n  \
             /history   - Show conversation history\n  \
             /logs      - Show activity log (error|warn|info|debug|all|raw)\n  \
             /doctor    - Run diagnostics\n  \
             /dream     - Run memory consolidation now\n  \
             /doctor squelches - List active doctor squelches\n  \
             /doctor squelch <Category>:<Name>   - Suppress a finding from heartbeat / heal\n  \
             /doctor unsquelch <Category>:<Name> - Resume reminders for it\n  \
             /heal      - Run the self-healing maintenance sweep on demand\n  \
             /status    - Show agent vitals\n  \
             /poke      - Trigger immediate heartbeat\n  \
             /pairing   - Manage sender pairing\n  \
             /pairing approve <code> - Approve a pairing request\n  \
             /pairing <code> - Approve (shortcut)\n  \
             /update    - Update borg to latest version\n\
             \n  \
             /sessions  - Browse and load saved sessions (press 'e' to export)\n  \
             /save      - Save current session\n  \
             /new       - Start new session\n  \
             /fork      - Branch the current session into a new one (preserves history up to now)\n  \
             /resume <id> - Resume a saved session by id (prefix ok)\n  \
             /export [fmt] - Export current session (fmt: json|csv|txt, default json)\n\
             \n  \
             /plugins   - Manage plugins, channels, and tools\n  \
             /projects  - List projects\n  \
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
        let memory = cortex::memory::load_memory_context_db(self.config.memory.max_context_tokens)?;
        let text = if memory.is_empty() {
            "No memories loaded.".to_string()
        } else {
            memory
        };
        self.push_system_message(text);
        Ok(AppAction::Continue)
    }

    fn cmd_history(&mut self) -> Result<AppAction> {
        match cortex::logging::read_history_formatted(50, false) {
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

    fn cmd_pairing_approve(&mut self, code: &str) -> Result<AppAction> {
        let client = self.client.clone();
        let result: std::result::Result<String, String> =
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async move {
                    let mut c = client.lock().await;

                    // Extract channel from prefix, or fall back to cross-channel lookup
                    let channel_name =
                        if let Some((channel, _)) = cortex::pairing::parse_prefixed_code(code) {
                            channel.to_string()
                        } else {
                            let list = c
                                .list_pairing_requests(None)
                                .await
                                .map_err(|e| format!("Error looking up code: {e}"))?;
                            match list.requests.iter().find(|r| {
                                r.code.eq_ignore_ascii_case(code) && r.status == "pending"
                            }) {
                                Some(r) => r.channel_name.clone(),
                                None => {
                                    return Err(format!(
                                        "No pending pairing request found for code '{}'",
                                        code.to_uppercase()
                                    ));
                                }
                            }
                        };

                    c.approve_pairing(&channel_name, code)
                        .await
                        .map_err(|e| format!("Failed to approve: {e}"))?;

                    let display = cortex::pairing::channel_display_name(&channel_name);
                    Ok(format!("Approved: {code} on {display}"))
                })
            });

        match result {
            Ok(msg) => self.push_system_message(msg),
            Err(msg) => self.push_system_message(msg),
        }
        Ok(AppAction::Continue)
    }

    fn cmd_plugins(&mut self) -> Result<AppAction> {
        if let Ok(data_dir) = cortex::config::Config::data_dir() {
            self.plugins_popup.show(&self.config, &data_dir);
            Ok(AppAction::EnterAltScreen)
        } else {
            self.push_system_message("Error: could not determine data directory".to_string());
            Ok(AppAction::Continue)
        }
    }

    fn cmd_new(&mut self) -> Result<AppAction> {
        self.cells.clear();
        self.queued_messages.clear();
        self.session_prompt_tokens = 0;
        self.session_completion_tokens = 0;
        Ok(AppAction::NewSession)
    }

    /// Fork the current session at its most recent message. Resolution of
    /// the live `agent_session_id` and the DB copy happen in the event loop
    /// so the work can run async + share the daemon's writer.
    fn cmd_fork(&mut self) -> Result<AppAction> {
        Ok(AppAction::ForkSession)
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
        match cortex::memory::list_memory_inventory(&self.config) {
            Ok(items) => {
                if items.is_empty() {
                    self.push_system_message(
                        "No memory entries or knowledge files found.".to_string(),
                    );
                } else {
                    let mut text = String::from("Memory inventory (oldest first):\n");
                    for item in &items {
                        let modified = item
                            .modified_at
                            .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                            .unwrap_or_else(|| "unknown".to_string());
                        text.push_str(&format!(
                            "  [{}:{}] {} ({} bytes, updated: {modified})\n",
                            item.store.label(),
                            item.scope,
                            item.name,
                            item.size_bytes
                        ));
                    }
                    text.push_str(
                        "\nTo update a saved memory entry, ask the agent to use write_memory.",
                    );
                    self.push_system_message(text.trim_end().to_string());
                }
            }
            Err(e) => {
                self.push_system_message(format!("Error listing memory inventory: {e}"));
            }
        }
        Ok(AppAction::Continue)
    }

    fn cmd_settings_set(&mut self, rest: &str) -> Result<AppAction> {
        let parts: Vec<&str> = rest.splitn(2, ' ').collect();
        if parts.len() == 2 {
            let key = parts[0].to_string();
            let value = parts[1].to_string();
            match self.config.apply_setting(&key, &value) {
                Ok(confirmation) => {
                    self.push_system_message(format!("Updated: {confirmation}"));
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

    /// `/model <provider-id>/<model-id>` — set both provider and model in one shot.
    /// Splits on the first `/` so OpenRouter model IDs like
    /// `openrouter/anthropic/claude-sonnet-4` parse as provider=`openrouter`,
    /// model=`anthropic/claude-sonnet-4`.
    fn cmd_model_set(&mut self, rest: &str) -> Result<AppAction> {
        let (provider, model) = match rest.split_once('/') {
            Some((p, m)) if !p.is_empty() && !m.is_empty() => (p.trim(), m.trim()),
            _ => {
                self.push_system_message(
                    "Usage: /model <provider-id>/<model-id>\nOr /model to open the picker."
                        .to_string(),
                );
                return Ok(AppAction::Continue);
            }
        };

        if let Err(e) = self.config.apply_setting("provider", provider) {
            self.push_system_message(format!("Error: {e}"));
            return Ok(AppAction::Continue);
        }
        if let Err(e) = self.config.apply_setting("model", model) {
            self.push_system_message(format!("Error: {e}"));
            return Ok(AppAction::Continue);
        }

        // Persist to the daemon. Failures log + warn but don't break the
        // already-applied in-process config update. Spawned fire-and-forget so
        // it works whether we're called from the TUI event loop (multi-thread
        // runtime) or a unit test (no surrounding tokio context: skip).
        let provider_owned = provider.to_string();
        let model_owned = model.to_string();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let client = self.client.clone();
            handle.spawn(async move {
                let mut c = client.lock().await;
                if let Err(e) = c.set_setting("provider", &provider_owned).await {
                    tracing::warn!("cmd_model_set: failed to persist provider: {e}");
                }
                if let Err(e) = c.set_setting("model", &model_owned).await {
                    tracing::warn!("cmd_model_set: failed to persist model: {e}");
                }
            });
        } else {
            tracing::debug!(
                "cmd_model_set: no tokio runtime in scope (likely a unit test); \
                 skipping daemon persistence"
            );
        }

        self.push_system_message(format!("Updated: provider = {provider}, model = {model}"));
        Ok(AppAction::ConfigReloaded)
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
            // Daemon mode: no local scheduler, send via HTTP. The
            // confirmation line is emitted by the AppAction::Poke handler
            // once the spawn fires — keeping it as one line, not two.
            Ok(AppAction::Poke)
        }
    }

    /// `/btw <question>` — spawn an ephemeral, tool-less side agent that
    /// answers using the current transcript snapshot. Does not touch
    /// `AppState`, the main agent turn, or the DB.
    pub(super) fn cmd_btw(&mut self, question: &str) -> Result<AppAction> {
        if question.is_empty() {
            self.push_system_message("Usage: /btw <question>".to_string());
            return Ok(AppAction::Continue);
        }
        let snapshot = snapshot_cells_as_messages(&self.cells);
        self.btw_popup.show_loading(question.to_string());
        Ok(AppAction::StartBtw {
            question: question.to_string(),
            snapshot,
        })
    }

    /// `/heal` — run the self-healing maintenance sweep on demand. This is the
    /// same routine the daemon fires nightly at 02:00, exposed for users who
    /// want to force a sweep without waiting.
    fn cmd_heal(&mut self) -> Result<AppAction> {
        let client = self.client.clone();
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut c = client.lock().await;
                c.heal().await
            })
        });
        match result {
            Ok(report) => {
                self.push_system_message(format_heal_report(&report));
            }
            Err(e) => {
                tracing::warn!(error = %e, "on-demand /heal: daemon heal RPC failed");
                self.push_system_message(format!("Self-healing failed: {e}"));
            }
        }
        Ok(AppAction::Continue)
    }

    fn cmd_dream(&mut self) -> Result<AppAction> {
        let client = self.client.clone();
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut c = client.lock().await;
                c.dream().await
            })
        });
        match result {
            Ok(report) => {
                self.push_system_message(crate::commands::misc::format_dream_report(&report))
            }
            Err(e) => {
                tracing::warn!(error = %e, "on-demand /dream: daemon dream RPC failed");
                self.push_system_message(format!("Dream failed: {e}"));
            }
        }
        Ok(AppAction::Continue)
    }

    /// Squelch a doctor finding so it stops firing in the heartbeat
    /// security nudge and the daily maintenance sweep, until the TTL set
    /// by `maintenance.security_squelch_ttl_days` (default 30) elapses.
    /// Argument format: `<Category>:<Name>` — the same key shown next to
    /// each finding in `/doctor` output.
    fn cmd_doctor_squelch_set(&mut self, arg: &str) -> Result<AppAction> {
        let Some((category, name)) = arg.split_once(':') else {
            self.push_system_message(
                "Usage: /doctor squelch <Category>:<Name>\n\
                 Example: /doctor squelch Host Security:Application Firewall\n\
                 (Find the exact key under each finding in /doctor.)"
                    .to_string(),
            );
            return Ok(AppAction::Continue);
        };
        let category = category.trim();
        let name = name.trim();
        if category.is_empty() || name.is_empty() {
            self.push_system_message("Usage: /doctor squelch <Category>:<Name>".to_string());
            return Ok(AppAction::Continue);
        }
        let ttl_secs = (self.config.maintenance.security_squelch_ttl_days as i64) * 86_400;
        let now = chrono::Utc::now().timestamp();
        let expires_at = now.saturating_add(ttl_secs);
        match doctor_squelch_db()
            .and_then(|db| db.doctor_squelch_set(category, name, now, expires_at, None))
        {
            Ok(()) => self.push_system_message(format!(
                "✓ squelched {category}:{name} for {} days — will resume after",
                self.config.maintenance.security_squelch_ttl_days
            )),
            Err(e) => self.push_system_message(format!("/doctor squelch failed: {e}")),
        }
        Ok(AppAction::Continue)
    }

    /// Drop a previously-set squelch so the finding starts firing again.
    fn cmd_doctor_squelch_clear(&mut self, arg: &str) -> Result<AppAction> {
        let Some((category, name)) = arg.split_once(':') else {
            self.push_system_message("Usage: /doctor unsquelch <Category>:<Name>".to_string());
            return Ok(AppAction::Continue);
        };
        let category = category.trim();
        let name = name.trim();
        match doctor_squelch_db().and_then(|db| db.doctor_squelch_clear(category, name)) {
            Ok(true) => self.push_system_message(format!("✓ unsquelched {category}:{name}")),
            Ok(false) => self.push_system_message(format!(
                "no active squelch for {category}:{name} — nothing to clear"
            )),
            Err(e) => self.push_system_message(format!("/doctor unsquelch failed: {e}")),
        }
        Ok(AppAction::Continue)
    }

    /// Show the active squelch list, with how long each one has left.
    fn cmd_doctor_squelch_list(&mut self) -> Result<AppAction> {
        let now = chrono::Utc::now().timestamp();
        match doctor_squelch_db().and_then(|db| db.doctor_squelch_list_active(now)) {
            Ok(rows) => {
                if rows.is_empty() {
                    self.push_system_message(
                        "No active squelches. Use /doctor squelch <Category>:<Name> to suppress \
                         a finding from heartbeat reminders for the next 30 days."
                            .to_string(),
                    );
                } else {
                    let mut text =
                        String::from("Active /doctor squelches\n────────────────────────\n");
                    for r in rows {
                        let days_left = ((r.expires_at - now).max(0) / 86_400) + 1;
                        text.push_str(&format!(
                            "  {}:{}  ({}d left)\n",
                            r.category, r.name, days_left
                        ));
                    }
                    text.push_str("Use /doctor unsquelch <Category>:<Name> to drop one.");
                    self.push_system_message(text);
                }
            }
            Err(e) => self.push_system_message(format!("/doctor squelches failed: {e}")),
        }
        Ok(AppAction::Continue)
    }

    /// Inline help for the squelch sub-commands. Triggered when a user
    /// types `/doctor squelch` or `/doctor unsquelch` without the key.
    fn cmd_doctor_squelch_help(&mut self) -> Result<AppAction> {
        self.push_system_message(
            "/doctor squelch <Category>:<Name>    Suppress a finding from heartbeat / heal\n\
             /doctor unsquelch <Category>:<Name>  Resume reminders for it\n\
             /doctor squelches                    List active squelches\n\
             \n\
             Squelches expire after maintenance.security_squelch_ttl_days (default 30 days).\n\
             Find each finding's key in the /doctor output."
                .to_string(),
        );
        Ok(AppAction::Continue)
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

/// Open `~/.borg/borg.db` for `/doctor squelch …` commands. The TUI
/// otherwise routes everything through the daemon RPC, but the squelch
/// table is small, append-mostly, and SQLite WAL is concurrent-safe — so
/// adding a one-off RPC just to flip a row would be over-engineering.
fn doctor_squelch_db() -> Result<cortex::db::Database> {
    cortex::db::Database::open()
}

/// Render a daemon-side [`protocols::admin::HealReport`] as a system-message
/// body. Mirrors the look of `format_heal_summary` but consumes the
/// gRPC-friendly proto returned by `Admin.Heal` rather than the in-process
/// MaintenanceReport.
pub(super) fn format_heal_report(report: &protocols::admin::HealReport) -> String {
    let mut text = if report.skipped {
        String::from("Self-healing sweep skipped\n───────────────────────────\n")
    } else {
        String::from("Self-healing sweep complete\n───────────────────────────\n")
    };
    if report.skipped {
        text.push_str("maintenance.enabled is false");
    } else {
        text.push_str(&format!(
            "Pruned {} log files ({} bytes truncated), {} activity rows, evicted {} embeddings, pruned {} workflows, healed {} stalled tasks, recovered {} wedged runs, purged {} messages, tombstoned {} audit events, pruned {} backups",
            report.pruned_logs,
            report.log_bytes_truncated,
            report.pruned_activity,
            report.evicted_embeddings,
            report.workflows_pruned,
            report.healed_tasks,
            report.wedged_runs_recovered,
            report.messages_purged,
            report.audit_events_tombstoned,
            report.backups_pruned,
        ));
        if !report.auto_backup_path.is_empty() {
            text.push_str("\nBackup created: ");
            text.push_str(&report.auto_backup_path);
        }
    }
    if !report.warnings.is_empty() {
        text.push_str("\n\nWarnings:");
        for w in &report.warnings {
            text.push_str("\n  • ");
            text.push_str(w);
        }
    }
    text
}

/// Render a [`MaintenanceReport`] as the multi-line system-message body
/// shown after `/heal`. Retained for tests covering the in-process formatter
/// shape; the production `/heal` path now goes through [`format_heal_report`]
/// against the daemon-side `HealReport`.
#[cfg(test)]
fn format_heal_summary(report: &cortex::maintenance::MaintenanceReport) -> String {
    let mut text = String::from("Self-healing sweep complete\n───────────────────────────\n");
    text.push_str(&report.activity_line());
    if !report.persistent_warnings.is_empty() {
        text.push_str("\n\nPersistent warnings (seen in 2+ consecutive runs):");
        for w in &report.persistent_warnings {
            text.push_str("\n  • ");
            text.push_str(w);
        }
    }
    text
}

/// Build a best-effort `Vec<Message>` snapshot from the TUI transcript.
///
/// Only `User` and `Assistant` cells are included — tool calls and system
/// notices are omitted because the ephemeral `/btw` agent has no tools and
/// no way to act on them, so feeding them in would just waste tokens.
/// Empty assistant cells (e.g. a still-streaming turn the user hasn't
/// received text for yet) are also skipped.
pub(super) fn snapshot_cells_as_messages(cells: &[HistoryCell]) -> Vec<Message> {
    let mut out = Vec::with_capacity(cells.len());
    for cell in cells {
        match cell {
            HistoryCell::User { text } if !text.is_empty() => {
                out.push(Message::user(text.clone()));
            }
            HistoryCell::Assistant { text, .. } if !text.is_empty() => {
                out.push(Message::assistant(text.clone()));
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex::types::Role;

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

    #[test]
    fn commands_rs_registers_heal_command() {
        // Regression guard: /heal is a documented user-facing command. If this
        // match arm gets deleted, the slash handler silently falls through to
        // "Unknown command" — easy to miss in review because the code still
        // compiles. This mirrors the /stats + /status guard above.
        let src = include_str!("commands.rs");
        let code = match src.find("#[cfg(test)]") {
            Some(idx) => &src[..idx],
            None => src,
        };
        assert!(code.contains("\"/heal\""), "commands.rs must handle /heal");
    }

    #[test]
    fn commands_rs_registers_dream_command() {
        let src = include_str!("commands.rs");
        let code = match src.find("#[cfg(test)]") {
            Some(idx) => &src[..idx],
            None => src,
        };
        assert!(
            code.contains("\"/dream\""),
            "commands.rs must handle /dream"
        );
    }

    #[test]
    fn format_dream_report_renders_counts_and_sections() {
        let out = crate::commands::misc::format_dream_report(&protocols::admin::DreamReport {
            entries_extracted: 3,
            dupes_merged: 2,
            tokens_used: 144,
            nightly_completed: true,
            weekly_completed: true,
            warnings: vec!["check INDEX".into()],
            nightly_summary: "wrote durable facts".into(),
            weekly_summary: "merged duplicate notes".into(),
        });
        assert!(out.contains("Entries extracted: 3"), "{out}");
        assert!(out.contains("Dupes merged: 2"), "{out}");
        assert!(out.contains("Tokens used: 144"), "{out}");
        assert!(out.contains("Nightly: wrote durable facts"), "{out}");
        assert!(out.contains("Weekly: merged duplicate notes"), "{out}");
        assert!(out.contains("check INDEX"), "{out}");
    }

    #[test]
    fn format_heal_summary_includes_activity_line_and_header() {
        let report = cortex::maintenance::MaintenanceReport {
            pass_count: 12,
            warn_count: 1,
            fail_count: 0,
            log_files_deleted: 2,
            ..Default::default()
        };
        let out = format_heal_summary(&report);
        assert!(
            out.starts_with("Self-healing sweep complete"),
            "summary must lead with a header: {out:?}"
        );
        assert!(
            out.contains(&report.activity_line()),
            "summary must embed the canonical activity_line so headless \
             and interactive runs agree on the numbers: {out:?}"
        );
        assert!(
            !out.contains("Persistent warnings"),
            "no persistent warnings section when list is empty: {out:?}"
        );
    }

    #[test]
    fn format_heal_summary_lists_persistent_warnings_when_present() {
        // Persistent warnings are the whole point of surfacing maintenance to
        // the user — a sweep that has "Data:SqliteIntegrity" for a second day
        // in a row must not be hidden behind a single pass/warn/fail counter.
        let report = cortex::maintenance::MaintenanceReport {
            pass_count: 10,
            warn_count: 2,
            fail_count: 0,
            persistent_warnings: vec![
                "Data:SqliteIntegrity".to_string(),
                "Filesystem:LogDir".to_string(),
            ],
            ..Default::default()
        };
        let out = format_heal_summary(&report);
        assert!(
            out.contains("Persistent warnings"),
            "must show persistent warnings section: {out:?}"
        );
        assert!(
            out.contains("Data:SqliteIntegrity") && out.contains("Filesystem:LogDir"),
            "every persistent warning must be rendered: {out:?}"
        );
    }

    #[test]
    fn format_heal_report_renders_expanded_counts() {
        let out = format_heal_report(&protocols::admin::HealReport {
            pruned_logs: 2,
            pruned_activity: 5,
            evicted_embeddings: 7,
            healed_tasks: 11,
            warnings: vec!["Skills:tamper audit".into()],
            log_bytes_truncated: 1234,
            workflows_pruned: 13,
            messages_purged: 17,
            audit_events_tombstoned: 19,
            wedged_runs_recovered: 23,
            backups_pruned: 29,
            auto_backup_path: "/tmp/backup.tar.zst".into(),
            skipped: false,
        });
        assert!(out.contains("1234 bytes truncated"), "{out:?}");
        assert!(out.contains("13 workflows"), "{out:?}");
        assert!(out.contains("recovered 23 wedged runs"), "{out:?}");
        assert!(
            out.contains("Backup created: /tmp/backup.tar.zst"),
            "{out:?}"
        );
        assert!(out.contains("Skills:tamper audit"), "{out:?}");
    }

    #[test]
    fn format_heal_report_renders_skip_state() {
        let out = format_heal_report(&protocols::admin::HealReport {
            skipped: true,
            ..Default::default()
        });
        assert!(out.starts_with("Self-healing sweep skipped"), "{out:?}");
        assert!(out.contains("maintenance.enabled is false"), "{out:?}");
    }

    #[test]
    fn snapshot_skips_non_conversation_cells() {
        // /btw should only see what the user and the main agent said — not
        // tool output, not system notices. Otherwise the snapshot bloats and
        // the side agent gets confused about what's "real" conversation.
        let cells = vec![
            HistoryCell::System {
                text: "ignore me".to_string(),
            },
            HistoryCell::User {
                text: "hello".to_string(),
            },
            HistoryCell::Thinking {
                text: "reasoning".to_string(),
            },
            HistoryCell::assistant("hi there".to_string(), false),
            HistoryCell::ToolResult {
                output: "tool output".to_string(),
                is_error: false,
                duration_ms: None,
                display_label: "Ran tool".to_string(),
                tool_name: "run_shell".to_string(),
                args_json: None,
                collapsed: false,
            },
            HistoryCell::Separator,
            HistoryCell::User {
                text: "".to_string(), // empty — should be skipped
            },
        ];
        let msgs = snapshot_cells_as_messages(&cells);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[1].role, Role::Assistant);
        match msgs[0].content.as_ref().unwrap() {
            cortex::types::MessageContent::Text(t) => assert_eq!(t, "hello"),
            _ => panic!("expected text content"),
        }
    }

    #[test]
    fn snapshot_preserves_order() {
        // Multi-turn conversations must preserve chronological order — if the
        // snapshot reshuffles, the side agent will answer against a garbled
        // transcript and give a confidently wrong answer.
        let cells = vec![
            HistoryCell::User {
                text: "q1".to_string(),
            },
            HistoryCell::assistant("a1".to_string(), false),
            HistoryCell::User {
                text: "q2".to_string(),
            },
            HistoryCell::assistant("a2".to_string(), false),
        ];
        let msgs = snapshot_cells_as_messages(&cells);
        let texts: Vec<_> = msgs
            .iter()
            .filter_map(|m| match m.content.as_ref() {
                Some(cortex::types::MessageContent::Text(t)) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["q1", "a1", "q2", "a2"]);
    }
}
