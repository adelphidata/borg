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
        // `!cmd` shell mode, checked before the slash table because `!` can
        // never begin a slash command. Only a leading `!` counts — "tell me
        // about !important" is an ordinary message.
        if let Some(rest) = trimmed.strip_prefix('!') {
            if !self.config.ui.shell_mode_enabled {
                return None;
            }
            let command = rest.trim();
            if command.is_empty() {
                return Some(self.cmd_help());
            }
            return Some(Ok(AppAction::ExecShell {
                command: command.to_string(),
            }));
        }

        // Exact matches
        match trimmed {
            "quit" | "exit" | "/exit" => return Some(Ok(AppAction::Quit)),
            "help" | "/help" => return Some(self.cmd_help()),
            "/pwd" | "/cwd" => return Some(self.cmd_pwd()),
            "/memory" => return Some(self.cmd_memory()),
            "/skills" => return Some(self.cmd_plugins()),
            "/history" => return Some(self.cmd_history()),
            "/logs" => return Some(self.handle_logs_command(None)),
            "/status" | "/stats" => {
                let client = self.client.clone();
                self.status_popup.show(&self.config, &client);
                return Some(Ok(AppAction::EnterAltScreen));
            }
            "/usage" => {
                let client = self.client.clone();
                // 0 = ask the daemon to use its default window (30 days).
                self.usage_popup.show(0, &client);
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
            // `/theme` opens the picker; `/skin` is an alias for muscle
            // memory operators may have from other tools — same destination.
            "/theme" | "/skin" => {
                self.theme_popup.show(&self.config);
                return Some(Ok(AppAction::Continue));
            }
            "/plugins" | "/plugin" => return Some(self.cmd_plugins()),
            "/mcp" => return Some(self.cmd_mcp()),
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
            "/reload" => return Some(self.cmd_reload("")),
            "/terminal-setup" => return Some(self.cmd_terminal_setup("")),
            "/browser" => return Some(self.cmd_browser("")),
            "/cancel" | "/stop" | "/abort" => return Some(self.cmd_cancel()),
            "/restart" => return Some(Ok(AppAction::RestartGateway)),
            "/context" => return Some(Ok(AppAction::ShowContextBreakdown)),
            "/compact" => return Some(Ok(AppAction::CompactHistory)),
            "/clear" => {
                self.cells.clear();
                self.cells_emitted = 0;
                self.pending_scrollback.clear();
                self.banner_emitted = false;
                self.queued_messages.clear();
                return Some(Ok(AppAction::ClearHistory));
            }
            "/undo" => return Some(Ok(AppAction::UndoLastTurn)),
            "/resume-turn" => return Some(Ok(self.begin_resume_turn())),
            "/sessions" => {
                let client = self.client.clone();
                self.sessions_popup.show(&client);
                return Some(Ok(AppAction::EnterAltScreen));
            }
            "/subprocesses" | "/procs" => {
                let client = self.client.clone();
                self.subprocesses_popup.show(&client);
                return Some(Ok(AppAction::EnterAltScreen));
            }
            "/agents" => {
                let client = self.client.clone();
                self.agents_popup.show(&client);
                return Some(Ok(AppAction::EnterAltScreen));
            }
            "/save" => return Some(Ok(AppAction::SaveSession)),
            "/export" => {
                return Some(Ok(AppAction::ExportCurrentSession {
                    format: cortex::export::ExportFormat::Json,
                    destination: crate::tui::app::ExportDestination::DefaultFile,
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
            // `/export <format> [destination]`. The destination is optional and
            // may itself contain spaces, so only the first word is the format.
            let rest = rest.trim();
            let (format_arg, dest_arg) = match rest.split_once(char::is_whitespace) {
                Some((f, d)) => (f, d),
                None => (rest, ""),
            };
            match format_arg.parse::<cortex::export::ExportFormat>() {
                Ok(format) => {
                    return Some(Ok(AppAction::ExportCurrentSession {
                        format,
                        destination: crate::tui::app::parse_export_destination(dest_arg),
                    }));
                }
                Err(e) => {
                    self.push_system_message(format!(
                        "Error: {e}\nUsage: /export [json|csv|txt|md] [path|clipboard]"
                    ));
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
            self.reset_transcript();
            self.queued_messages.clear();
            return Some(Ok(AppAction::LoadSession { id: id.to_string() }));
        }

        if let Some(rest) = trimmed.strip_prefix("/logs ") {
            return Some(self.handle_logs_command(Some(rest.trim())));
        }

        if let Some(rest) = trimmed.strip_prefix("/browser ") {
            return Some(self.cmd_browser(rest.trim()));
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

        // `/wt` is the alias, matching the `borg wt` CLI shorthand.
        for name in ["/worktree", "/wt"] {
            if trimmed == name {
                return Some(self.cmd_worktree(""));
            }
            if let Some(rest) = trimmed.strip_prefix(&format!("{name} ")) {
                return Some(self.cmd_worktree(rest.trim()));
            }
        }

        if trimmed == "/goal" {
            return Some(self.cmd_goal_show());
        }
        if let Some(rest) = trimmed.strip_prefix("/goal ") {
            return Some(self.cmd_goal_dispatch(rest.trim()));
        }

        // `/hb` is the alias; both land on the same parser.
        for name in ["/heartbeat", "/hb"] {
            if trimmed == name {
                return Some(self.cmd_heartbeat(HeartbeatSubcommand::Status));
            }
            if let Some(rest) = trimmed.strip_prefix(&format!("{name} ")) {
                return Some(self.cmd_heartbeat(parse_heartbeat_subcommand(rest.trim())));
            }
        }

        if trimmed == "/handoff" {
            return Some(self.cmd_handoff_show());
        }
        if let Some(rest) = trimmed.strip_prefix("/handoff ") {
            return Some(self.cmd_handoff_request(rest.trim()));
        }

        if trimmed == "/think" {
            return Some(self.cmd_think_show());
        }
        if let Some(rest) = trimmed.strip_prefix("/think ") {
            return Some(self.cmd_think_set(rest.trim()));
        }

        if let Some(rest) = trimmed.strip_prefix("/reload ") {
            return Some(self.cmd_reload(rest.trim()));
        }

        if trimmed == "/voice" {
            return Some(self.cmd_voice(""));
        }
        if let Some(rest) = trimmed.strip_prefix("/voice ") {
            return Some(self.cmd_voice(rest.trim()));
        }

        if let Some(question) = cortex::agent::parse_btw_command(trimmed) {
            if question.is_empty() {
                self.push_system_message(
                    "Usage: /btw <question>\n  \
                     Ask a side question using the current session's context. \
                     Answer appears in a dismissable popup (Esc). Not persisted."
                        .to_string(),
                );
                return Some(Ok(AppAction::Continue));
            }
            return Some(self.cmd_btw(question));
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

        // `/usage <days>` — open the popup with an explicit window. Bad
        // input falls back to the default with a one-line system message
        // rather than silently zeroing — silent zero would render an empty
        // popup with no signal that the arg was rejected.
        if let Some(rest) = trimmed.strip_prefix("/usage ") {
            let arg = rest.trim();
            let days = match arg.parse::<u32>() {
                Ok(n) => n,
                Err(_) => {
                    self.push_system_message(format!(
                        "/usage: couldn't parse '{arg}' as days; using default."
                    ));
                    0
                }
            };
            let client = self.client.clone();
            self.usage_popup.show(days, &client);
            return Some(Ok(AppAction::EnterAltScreen));
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

        if let Some(rest) = trimmed.strip_prefix("/terminal-setup ") {
            return Some(self.cmd_terminal_setup(rest));
        }

        if trimmed.starts_with('/') {
            self.push_system_message(format!(
                "Unknown command: {trimmed}\nType /help for available commands."
            ));
            return Some(Ok(AppAction::Continue));
        }

        None // Not a command
    }

    fn cmd_help(&mut self) -> Result<AppAction> {
        self.push_system_message(HELP_TEXT.to_string());
        Ok(AppAction::Continue)
    }
}

/// `/help`. Every command in the `/` popup is listed here (a test checks).
pub(super) const HELP_TEXT: &str = "Commands:\n  \
             /help      - Show this help\n  \
             /settings  - Configure settings\n  \
             /usage [days] - Token spend, model + tool usage over a window (default 30 days)\n  \
             /mode      - Switch collaboration mode (execute/plan)\n  \
             /plan      - Shortcut for /mode plan (toggles read-only plan mode)\n  \
             /think [lvl] - Show or set thinking level (off|minimal|low|medium|high|xhigh|adaptive|max)\n  \
             /goal <text> - Give this session a goal to work toward (/goal for status, pause|resume|clear)\n  \
             /heartbeat every <interval> <prompt> - Re-send a prompt here while idle (alias: /hb)\n  \
             /theme     - Pick TUI theme (alias: /skin)\n\
             \n  \
             !<cmd>     - Run a shell command directly, no LLM turn (e.g. `!git status`)\n  \
             /btw <q>   - Ask a side question (ephemeral, not persisted)\n  \
             /context   - Context-window usage breakdown\n  \
             /compact   - Compact conversation history\n  \
             /clear     - Clear conversation\n  \
             /cancel    - Stop the current in-progress turn\n  \
             /undo      - Undo last agent turn\n  \
             /resume-turn - Continue a reply that was cut short\n\
             \n  \
             /memory    - Show memory\n  \
             /pwd       - Show the working directory\n  \
             /history   - Show conversation history\n  \
             /logs      - Show activity log (error|warn|info|debug|all|raw)\n  \
             /doctor    - Run diagnostics\n  \
             /dream     - Run memory consolidation now\n  \
             /doctor squelches - List active doctor squelches\n  \
             /doctor squelch <Category>:<Name>   - Suppress a finding from heartbeat / heal\n  \
             /doctor unsquelch <Category>:<Name> - Resume reminders for it\n  \
             /heal      - Run the self-healing maintenance sweep on demand\n  \
             /reload [mcp|skills] - Hot-reload MCP servers and/or user skills without restart\n  \
             /status    - Show agent vitals\n  \
             /voice     - Voice mode: /voice [on|off|rec|tts|status]\n  \
             /poke      - Trigger immediate heartbeat\n  \
             /terminal-setup [target] - Install IDE terminal keybindings (auto|vscode|cursor|windsurf)\n  \
             /pairing   - Manage sender pairing\n  \
             /pairing approve <code> - Approve a pairing request\n  \
             /pairing <code> - Approve (shortcut)\n  \
             /update    - Update borg to latest version\n\
             \n  \
             /sessions  - Browse and load saved sessions (press 'e' to export)\n  \
             /subprocesses - View tracked subprocesses (p: toggle pin, r: reload)\n  \
             /save      - Save current session\n  \
             /new       - Start new session\n  \
             /fork      - Branch the current session into a new one (preserves history up to now)\n  \
             /worktree  - Isolated git checkouts to code in (alias: /wt)\n  \
             /resume <id> - Resume a saved session by id (prefix ok)\n  \
             /export [fmt] [path|clipboard] - Export current session (fmt: json|csv|txt|md, default json)\n\
             \n  \
             /plugins   - Manage plugins, channels, and tools\n  \
             /skills    - Manage skills (alias of /plugins)\n  \
             /model     - Switch LLM provider/model\n  \
             /schedule  - Manage scheduled tasks\n  \
             /migrate   - Import from another agent\n  \
             /mcp       - Show configured MCP servers\n  \
             /agents    - View and control sub-agents\n  \
             /browser [connect|disconnect|status] - Manage the browser connection\n  \
             /handoff <platform> - Continue this conversation on a messaging channel\n  \
             /restart   - Restart gateway server\n\
             \n  \
             /xp        - Show XP summary and feed\n  \
             /evolution - Show evolution overview\n  \
             /card      - Print an ASCII share card\n  \
             /stats     - Show agent vitals (alias of /status)\n\
             \n  \
             /uninstall - Remove all borg data and binary\n  \
             /exit      - Exit (also: quit, exit)";

impl App<'_> {
    /// Cancel the in-progress agent turn, if any. Equivalent to pressing Esc
    /// during streaming — provided as an explicit command so messaging-channel
    /// users (who can't press Esc) have a consistent verb across surfaces.
    pub(super) fn cmd_cancel(&mut self) -> Result<AppAction> {
        if let Some(token) = self.cancel_token.take() {
            token.cancel();
            self.event_rx = None;
            self.steer_tx = None;
            self.pending_steers.clear();
            // Pause any Active goal so the Ralph loop doesn't self-continue
            // past the operator's Esc/`/cancel`. Fire-and-forget: cancel must
            // feel instant, so we never block the TUI on the daemon round
            // trip. The footer is driven by `SubscribeGoalEvents`, so the
            // status flip lands in the UI on its own once the RPC completes.
            // The no-runtime branch (unit tests for `cmd_cancel`) is a no-op.
            if tokio::runtime::Handle::try_current().is_ok() {
                let client = self.client.clone();
                let host = self.host_session_id.clone();
                tokio::spawn(async move {
                    let mut c = client.lock().await;
                    let live = c.live_session_id(&host).await;
                    if let Err(e) = c.pause_goal_on_interrupt(&live).await {
                        tracing::warn!("cmd_cancel: pause_goal_on_interrupt failed: {e}");
                    }
                });
            }
            self.push_system_message("[cancelled]".to_string());
            Ok(AppAction::Continue)
        } else {
            self.push_system_message("Nothing to cancel.".to_string());
            Ok(AppAction::Continue)
        }
    }

    /// `/pwd` — this client's working directory, and the repo that contains it.
    ///
    /// Worth its own command because the answer is not always the directory the
    /// user launched from: `borg --worktree` puts the client in a sibling
    /// checkout, and the repo root (not the cwd) is what scopes `--continue`
    /// and the workspace-relative paths in tool output.
    ///
    /// Deliberately *not* described as where the agent writes. The workspace is
    /// bound daemon-side at `Session.Open`, so a session opened in one
    /// directory and later resumed from another would make that claim false.
    /// Reporting what this process can actually see keeps it honest.
    fn cmd_pwd(&mut self) -> Result<AppAction> {
        let text = match std::env::current_dir() {
            Ok(cwd) => {
                let (cwd, repo_root) = cortex::session::workspace_identity(&cwd);
                match repo_root {
                    // Only worth a second line when it differs — in a plain
                    // directory, or at the top of a repo, one line says it all.
                    Some(root) if root != cwd => {
                        format!("{}\nrepository: {}", cwd.display(), root.display())
                    }
                    _ => cwd.display().to_string(),
                }
            }
            Err(e) => format!("Cannot read the working directory: {e}"),
        };
        self.push_system_message(text);
        Ok(AppAction::Continue)
    }

    fn cmd_memory(&mut self) -> Result<AppAction> {
        let memory = crate::async_bridge::block_on(cortex::memory::load_memory_context_db(
            self.config.memory.max_context_tokens,
        ))?;
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
        let result: std::result::Result<String, String> = tokio::task::block_in_place(|| {
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
                        // `r.code` is the SHA-256 hash of the lowercased
                        // pairing code. Hash the user input the same way
                        // before comparing.
                        let target_hash = cortex::db::hash_pairing_code(&code.to_lowercase());
                        match list
                            .requests
                            .iter()
                            .find(|r| r.code == target_hash && r.status == "pending")
                        {
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

    /// `/mcp` — read-only listing of configured MCP servers. Mutating
    /// operations (`add`, `remove`, `disable`, `test`) still flow
    /// through the `borg mcp` CLI so the trust model stays anchored
    /// in one place; this popup is the TUI parity for `borg mcp list`.
    fn cmd_mcp(&mut self) -> Result<AppAction> {
        self.mcp_popup.show(&self.config);
        Ok(AppAction::EnterAltScreen)
    }

    fn cmd_new(&mut self) -> Result<AppAction> {
        self.reset_transcript();
        self.queued_messages.clear();
        self.session_prompt_tokens = 0;
        self.session_completion_tokens = 0;
        self.last_prompt_tokens = 0;
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
            "Current collaboration mode: {current}\nUsage: /mode <execute|plan>"
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
                .unwrap_or(CollaborationMode::Execute)
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
        self.set_mode(next);
        self.push_system_message(format!("[collaboration mode: {next}]"));
    }

    fn cmd_memory_cleanup(&mut self) -> Result<AppAction> {
        match crate::async_bridge::block_on(cortex::memory::list_memory_inventory(&self.config)) {
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
                            .map(|t| t.format(crate::commands::TS_FMT).to_string())
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

    /// `/think` (bare) — show the current global thinking level, any per-
    /// model overrides, and the effective level resolved for the currently
    /// selected model.
    fn cmd_think_show(&mut self) -> Result<AppAction> {
        let global = serde_json::to_value(&self.config.llm.thinking)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "off".to_string());
        let model = if self.config.llm.model.is_empty() {
            "<none>".to_string()
        } else {
            self.config.llm.model.clone()
        };
        let effective =
            serde_json::to_value(self.config.llm.effective_thinking(&self.config.llm.model))
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| "off".to_string());

        let mut msg = format!("Thinking: global = {global}, effective for {model} = {effective}");
        if !self.config.llm.thinking_overrides.is_empty() {
            msg.push_str("\nPer-model overrides:");
            for (needle, level) in &self.config.llm.thinking_overrides {
                let level_str = serde_json::to_value(level)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                msg.push_str(&format!("\n  {needle} -> {level_str}"));
            }
        }
        msg.push_str("\nSet with: /think <off|minimal|low|medium|high|xhigh|adaptive|max>");
        self.push_system_message(msg);
        Ok(AppAction::Continue)
    }

    /// `/think <level>` — set the global `llm.thinking` setting, persisting
    /// to the DB so it survives restart (parity with `/settings llm.thinking
    /// <level>`). Rejected levels surface the accepted list rather than
    /// silently succeeding.
    fn cmd_think_set(&mut self, level: &str) -> Result<AppAction> {
        const ACCEPTED: &str = "off|minimal|low|medium|high|xhigh|adaptive|max";
        let level = level.trim().to_ascii_lowercase();
        if level.is_empty() {
            self.push_system_message(format!("Usage: /think <{ACCEPTED}>"));
            return Ok(AppAction::Continue);
        }
        match self.config.apply_setting("llm.thinking", &level) {
            Ok(_) => {
                // Persist to daemon best-effort; the in-process Config is
                // already updated. Pattern mirrors cmd_model_set.
                let level_owned = level.clone();
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    let client = self.client.clone();
                    handle.spawn(async move {
                        let mut c = client.lock().await;
                        if let Err(e) = c.set_setting("llm.thinking", &level_owned).await {
                            tracing::warn!("cmd_think_set: failed to persist llm.thinking: {e}");
                        }
                    });
                }
                self.push_system_message(format!("Updated: llm.thinking = {level}"));
                Ok(AppAction::UpdateSetting {
                    key: "llm.thinking".to_string(),
                    value: level,
                })
            }
            Err(_) => {
                self.push_system_message(format!(
                    "Error: '{level}' is not a thinking level.\nAccepted: {ACCEPTED}"
                ));
                Ok(AppAction::Continue)
            }
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

    /// `/browser [connect|disconnect|status] [url]` — manage CDP attach.
    /// Default action when no args is `status`. The Admin RPC owns URL
    /// parsing, reachability probing, optional auto-launch, and DB
    /// persistence — this handler just renders the daemon's `messages`
    /// list and final connection state to the transcript.
    fn cmd_browser(&mut self, arg: &str) -> Result<AppAction> {
        let arg = arg.trim();
        let (action, url): (&str, Option<&str>) = if arg.is_empty() {
            ("status", None)
        } else {
            let mut parts = arg.splitn(2, char::is_whitespace);
            let act = parts.next().unwrap_or("").to_lowercase();
            let url = parts.next().map(str::trim).filter(|s| !s.is_empty());
            match act.as_str() {
                "connect" | "disconnect" | "status" => {
                    return self.run_browser_rpc(act.as_str(), url);
                }
                _ => {
                    self.push_system_message(
                        "Usage: /browser [connect|disconnect|status] [url]".to_string(),
                    );
                    return Ok(AppAction::Continue);
                }
            }
        };
        self.run_browser_rpc(action, url)
    }

    fn run_browser_rpc(&mut self, action: &str, url: Option<&str>) -> Result<AppAction> {
        let client = self.client.clone();
        let action_owned = action.to_string();
        let url_owned = url.map(std::string::ToString::to_string);
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut c = client.lock().await;
                c.browser_manage(&action_owned, url_owned.as_deref()).await
            })
        });
        match result {
            Ok(reply) => {
                for line in &reply.messages {
                    self.push_system_message(line.clone());
                }
                let summary = match action {
                    "status" if reply.connected => {
                        format!("browser connected: {}", reply.url)
                    }
                    "status" => "browser not connected (try /browser connect [url])".to_string(),
                    "connect" if reply.connected => {
                        format!("browser connected: {}", reply.url)
                    }
                    "connect" => "browser not connected — see messages above".to_string(),
                    "disconnect" => "browser reverted to default (headless local)".to_string(),
                    _ => return Ok(AppAction::Continue),
                };
                self.push_system_message(summary);
            }
            Err(e) => {
                self.push_system_message(format!("/browser failed: {e}"));
            }
        }
        Ok(AppAction::Continue)
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

    /// `/reload [mcp|skills]` — hot-reload MCP catalog and/or user skills
    /// without restarting the daemon. No argument means reload both.
    ///
    /// MCP: diffs DB-resident server config against the running session map
    /// and spins up new / tears down removed servers in place. Mid-flight
    /// tool calls survive teardown — they hold their own session handle.
    /// Skills: re-scans `~/.borg/skills/` and invalidates per-session skill
    /// caches so the next turn picks up new/edited `SKILL.md` files.
    fn cmd_reload(&mut self, arg: &str) -> Result<AppAction> {
        use protocols::admin::reload_request::Target;
        let target = match arg.trim().to_lowercase().as_str() {
            "" | "all" => Target::All as i32,
            "mcp" => Target::Mcp as i32,
            "skills" => Target::Skills as i32,
            other => {
                self.push_system_message(format!(
                    "Usage: /reload [mcp|skills]\nunknown target: {other}"
                ));
                return Ok(AppAction::Continue);
            }
        };
        self.run_in_background("Reloading…", move |mut c| async move {
            match c.reload(target).await {
                Ok(resp) => format_reload_response(target, &resp),
                Err(e) => {
                    tracing::warn!(error = %e, "on-demand /reload: daemon Reload RPC failed");
                    format!("Reload failed: {e}")
                }
            }
        });
        Ok(AppAction::Continue)
    }

    /// `/heal` — run the self-healing maintenance sweep on demand. This is the
    /// same routine the daemon fires nightly at 02:00, exposed for users who
    /// want to force a sweep without waiting.
    fn cmd_heal(&mut self) -> Result<AppAction> {
        self.run_in_background(
            "Running the self-healing sweep. This can take a minute; you can keep working.",
            |mut c| async move {
                match c.heal().await {
                    Ok(report) => format_heal_report(&report),
                    Err(e) => {
                        tracing::warn!(error = %e, "on-demand /heal: daemon heal RPC failed");
                        format!("Self-healing failed: {e}")
                    }
                }
            },
        );
        Ok(AppAction::Continue)
    }

    /// `/terminal-setup [auto|vscode|cursor|windsurf]` — install IDE terminal
    /// keybindings so the TUI receives multiline-submit and undo/redo keys
    /// rather than having the IDE swallow them. Default is auto-detect.
    fn cmd_terminal_setup(&mut self, arg: &str) -> Result<AppAction> {
        use crate::terminal_setup as ts;

        let target = match ts::parse_terminal_setup_target(arg) {
            Ok(t) => t,
            Err(bad) => {
                self.push_system_message(format!(
                    "usage: /terminal-setup [auto|vscode|cursor|windsurf]\n\
                     unrecognized target: {bad}"
                ));
                return Ok(AppAction::Continue);
            }
        };

        let Some(home) = dirs::home_dir() else {
            self.push_system_message(
                "/terminal-setup: could not determine home directory".to_string(),
            );
            return Ok(AppAction::Continue);
        };

        let env = ts::ProcessEnv;
        let fs = ts::RealFileOps;
        let now_iso = chrono::Utc::now().to_rfc3339();
        let os = ts::Os::current();

        let result = match target {
            Some(t) => ts::configure_terminal_keybindings(t, os, &env, &home, &fs, &now_iso),
            None => ts::configure_detected_terminal_keybindings(os, &env, &home, &fs, &now_iso),
        };

        self.push_system_message(result.message);
        if result.success && result.requires_restart {
            self.push_system_message(
                "Restart the IDE terminal for the new keybindings to take effect.".to_string(),
            );
        }
        Ok(AppAction::Continue)
    }

    fn cmd_dream(&mut self) -> Result<AppAction> {
        self.run_in_background(
            "Consolidating memory. This can take a minute; you can keep working.",
            |mut c| async move {
                match c.dream().await {
                    Ok(report) => crate::commands::misc::format_dream_report(&report),
                    Err(e) => {
                        tracing::warn!(error = %e, "on-demand /dream: daemon dream RPC failed");
                        format!("Dream failed: {e}")
                    }
                }
            },
        );
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
        let category = category.trim().to_string();
        let name = name.trim().to_string();
        if category.is_empty() || name.is_empty() {
            self.push_system_message("Usage: /doctor squelch <Category>:<Name>".to_string());
            return Ok(AppAction::Continue);
        }
        let ttl_days = self.config.maintenance.security_squelch_ttl_days as i64;
        let ttl_secs = ttl_days * 86_400;
        let now = chrono::Utc::now().timestamp();
        let expires_at = now.saturating_add(ttl_secs);
        let client = self.client.clone();
        let cat = category.clone();
        let nm = name.clone();
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut c = client.lock().await;
                c.doctor_squelch_set(&cat, &nm, expires_at, None).await
            })
        });
        match result {
            Ok(()) => self.push_system_message(format!(
                "✓ squelched {category}:{name} for {ttl_days} days — will resume after"
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
        let category = category.trim().to_string();
        let name = name.trim().to_string();
        let client = self.client.clone();
        let cat = category.clone();
        let nm = name.clone();
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut c = client.lock().await;
                c.doctor_squelch_clear(&cat, &nm).await
            })
        });
        match result {
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
        match crate::async_bridge::block_on(async {
            let db = cortex::db::Database::open_readonly().await?;
            db.doctor_squelch_list_active(now).await
        }) {
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

    /// `/goal` (bare) — render a summary card for the most recent session's
    /// goal, or "no goal set" if none exists.
    ///
    /// Reads the DB directly (read-only) and looks up the most-recently-active
    /// session. The daemon may be writing concurrently, but `Database::open_readonly`
    /// is safe under WAL.
    /// `/worktree [new <name> | remove <name> [--force]]`.
    ///
    /// Bare form reports where this session is working and what else exists;
    /// the two subcommands are the only mutations, and both are one deliberate
    /// step past the overview rather than options on the first screen.
    fn cmd_worktree(&mut self, args: &str) -> Result<AppAction> {
        let body = match parse_worktree_command(args) {
            Ok(cmd) => crate::async_bridge::block_on(run_worktree_command(cmd)),
            Err(usage) => usage,
        };
        self.push_system_message(body);
        Ok(AppAction::Continue)
    }

    fn cmd_goal_show(&mut self) -> Result<AppAction> {
        let body = match self.fetch_goal_summary_via_rpc() {
            Ok(text) => text,
            Err(e) => format!("/goal: failed to read goal — {e}"),
        };
        self.push_system_message(body);
        Ok(AppAction::Continue)
    }

    /// Render the current goal as a 5-7 line summary card. Talks to the
    /// daemon over gRPC (rather than opening the SQLite file directly) so
    /// the TUI is correct even when the daemon is on a different host or
    /// the file is locked by a write transaction.
    fn fetch_goal_summary_via_rpc(&self) -> Result<String> {
        let client = self.client.clone();
        let host = self.host_session_id.clone();
        let detail = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut c = client.lock().await;
                let live = c.live_session_id(&host).await;
                c.get_goal(&live).await
            })
        })?;
        if !detail.present {
            return Ok("No goal set for this session.\nUse `/goal <text>` to set one.".to_string());
        }
        Ok(format_goal_detail(&detail))
    }

    /// `/goal <args>` — dispatch user-driven goal management. Pause / resume /
    /// clear / set / replace all route through the Admin gRPC RPCs so the
    /// daemon owns the canonical write and publishes the lifecycle event
    /// onto the shared bus the TUI footer subscribes to. No more direct DB
    /// writes from the TUI; no more synthetic agent instructions for the
    /// initial-set path.
    fn cmd_goal_dispatch(&mut self, args: &str) -> Result<AppAction> {
        match parse_goal_subcommand(args) {
            GoalSubcommand::Show => self.cmd_goal_show(),
            GoalSubcommand::Clear => {
                let report = match self.rpc_clear_goal() {
                    Ok(()) => "/goal: cleared".to_string(),
                    Err(e) => format!("/goal: clear failed — {}", daemon_reason(&e)),
                };
                self.push_system_message(report);
                Ok(AppAction::Continue)
            }
            GoalSubcommand::Pause => {
                let report = match self.rpc_set_goal_status("paused") {
                    Ok(detail) if detail.present => {
                        format!("/goal: paused — \"{}\"", detail.objective)
                    }
                    Ok(_) => "/goal: no active goal to pause".to_string(),
                    Err(e) => format!("/goal: pause failed — {}", daemon_reason(&e)),
                };
                self.push_system_message(report);
                Ok(AppAction::Continue)
            }
            GoalSubcommand::Resume => {
                let report = match self.rpc_set_goal_status("active") {
                    Ok(detail) if detail.present => {
                        format!("/goal: resumed — \"{}\"", detail.objective)
                    }
                    Ok(_) => "/goal: no paused goal to resume".to_string(),
                    Err(e) => format!("/goal: resume failed — {}", daemon_reason(&e)),
                };
                self.push_system_message(report);
                Ok(AppAction::Continue)
            }
            GoalSubcommand::BareEdit => {
                // Borg has no goal-editor popup, so surface an explicit
                // usage error. Without this guard the literal text "edit"
                // would be accepted as an objective.
                self.push_system_message(
                    "Usage: /goal edit <text> — replace the current objective.".to_string(),
                );
                Ok(AppAction::Continue)
            }
            GoalSubcommand::EmptyObjective => {
                self.push_system_message("/goal: Goal objective must not be empty.".to_string());
                Ok(AppAction::Continue)
            }
            GoalSubcommand::Usage => {
                self.push_system_message(GOAL_USAGE.to_string());
                Ok(AppAction::Continue)
            }
            GoalSubcommand::Gate(sub) => self.cmd_goal_gate(sub),
            GoalSubcommand::Budget { budget } => {
                if let Some(b) = budget {
                    if let Err(e) =
                        cortex::goals::validate_budget(Some(b), self.config.goals.max_token_budget)
                    {
                        self.push_system_message(format!("/goal: {e}"));
                        return Ok(AppAction::Continue);
                    }
                }
                // SetGoal{SetBudget} changes the budget in place — unlike
                // `--budget N <text>`, it preserves the objective, goal_id,
                // and accumulated accounting. Requires an existing goal.
                let report = match self.rpc_set_goal_budget(budget) {
                    Ok(detail) => {
                        let shown = if detail.token_budget_present {
                            detail.token_budget.to_string()
                        } else {
                            "unbounded".to_string()
                        };
                        format!("/goal: budget set to {shown}")
                    }
                    Err(e) => format!("/goal: budget change failed — {}", daemon_reason(&e)),
                };
                self.push_system_message(report);
                Ok(AppAction::Continue)
            }
            GoalSubcommand::Set { objective, budget } => {
                let trimmed = match cortex::goals::validate_objective(&objective) {
                    Ok(v) => v,
                    Err(e) => {
                        self.push_system_message(format!("/goal: {e}"));
                        return Ok(AppAction::Continue);
                    }
                };
                if let Some(budget_val) = budget {
                    if let Err(e) = cortex::goals::validate_budget(
                        Some(budget_val),
                        self.config.goals.max_token_budget,
                    ) {
                        self.push_system_message(format!("/goal: {e}"));
                        return Ok(AppAction::Continue);
                    }
                }
                // SetGoal{Replace} = "create or replace", so this is the
                // single path for both initial-set and edit. The daemon
                // publishes the lifecycle event onto the goal bus; the
                // agent's next prompt build picks up the new `<goal_context>`.
                let report = match self.rpc_set_goal_replace(&trimmed, budget) {
                    Ok(detail) => format!("/goal: set — \"{}\"", detail.objective),
                    Err(e) => format!("/goal: set failed — {}", daemon_reason(&e)),
                };
                self.push_system_message(report);
                Ok(AppAction::Continue)
            }
        }
    }

    /// `/handoff` — show the current session's handoff state.
    fn cmd_handoff_show(&mut self) -> Result<AppAction> {
        let report = match self.rpc_handoff_status() {
            Ok(s) if s.state.is_empty() => {
                "No handoff in progress for this session.\nUse `/handoff <platform>` to start one (e.g. `/handoff telegram`).".to_string()
            }
            Ok(s) => format_handoff_status(&s),
            Err(e) => format!("/handoff: status lookup failed — {e}"),
        };
        self.push_system_message(report);
        Ok(AppAction::Continue)
    }

    /// `/handoff <platform>` — mark this session as pending handoff to the
    /// destination channel. The destination gateway claims the row on its
    /// next sweep.
    fn cmd_handoff_request(&mut self, args: &str) -> Result<AppAction> {
        let platform = args.trim();
        if platform.is_empty() {
            self.push_system_message(
                "/handoff: missing platform.\nUsage: /handoff <telegram|slack|discord|signal|imessage|...>".to_string(),
            );
            return Ok(AppAction::Continue);
        }
        let report = match self.rpc_request_handoff(platform) {
            Ok(()) => format!(
                "/handoff: pending → {platform}. The destination will pick it up on its next sweep."
            ),
            Err(e) => format!("/handoff: failed — {e}"),
        };
        self.push_system_message(report);
        Ok(AppAction::Continue)
    }

    fn rpc_handoff_status(&self) -> Result<protocols::admin::HandoffStatusReply> {
        let client = self.client.clone();
        let host = self.host_session_id.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut c = client.lock().await;
                let live = c.live_session_id(&host).await;
                c.get_handoff_status(&live).await
            })
        })
    }

    fn rpc_request_handoff(&self, platform: &str) -> Result<()> {
        let client = self.client.clone();
        let host = self.host_session_id.clone();
        let platform = platform.to_string();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut c = client.lock().await;
                let live = c.live_session_id(&host).await;
                c.request_handoff(&live, &platform).await
            })
        })
    }

    /// `/goal gate …` — add, list, remove, or clear the goal's quality gates.
    ///
    /// Every op round-trips through the daemon and re-renders the full list, so
    /// the user always sees the state their change produced rather than the
    /// state the client guessed.
    fn cmd_goal_gate(&mut self, sub: GateSubcommand) -> Result<AppAction> {
        use protocols::admin::goal_gate_mutation::Op;

        let (op, command, ordinal) = match sub {
            GateSubcommand::List => (Op::List, String::new(), 0),
            GateSubcommand::Add(c) => (Op::Add, c, 0),
            GateSubcommand::Remove(n) => (Op::Remove, String::new(), n),
            GateSubcommand::Clear => (Op::Clear, String::new(), 0),
            GateSubcommand::Usage => {
                self.push_system_message(GOAL_USAGE.to_string());
                return Ok(AppAction::Continue);
            }
        };

        let report = match self.rpc_mutate_goal_gates(op as i32, &command, ordinal) {
            Ok(list) => format_goal_gates(&list),
            Err(e) => format!("/goal gate: failed — {e}"),
        };
        self.push_system_message(report);
        Ok(AppAction::Continue)
    }

    fn rpc_mutate_goal_gates(
        &self,
        op: i32,
        command: &str,
        ordinal: i64,
    ) -> Result<protocols::admin::GoalGateList> {
        let client = self.client.clone();
        let host = self.host_session_id.clone();
        let command = command.to_string();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut c = client.lock().await;
                let live = c.live_session_id(&host).await;
                c.mutate_goal_gates(&live, op, &command, ordinal).await
            })
        })
    }

    /// `/heartbeat …` — manage this session's recurring prompt.
    fn cmd_heartbeat(&mut self, sub: HeartbeatSubcommand) -> Result<AppAction> {
        use protocols::admin::session_heartbeat_mutation::Op;

        let (op, prompt, interval) = match sub {
            HeartbeatSubcommand::Status => (Op::Status, String::new(), 0),
            HeartbeatSubcommand::Set {
                interval_secs,
                prompt,
            } => (Op::Set, prompt, interval_secs),
            HeartbeatSubcommand::Pause => (Op::Pause, String::new(), 0),
            HeartbeatSubcommand::Resume => (Op::Resume, String::new(), 0),
            HeartbeatSubcommand::Clear => (Op::Clear, String::new(), 0),
            HeartbeatSubcommand::Usage => {
                self.push_system_message(HEARTBEAT_USAGE.to_string());
                return Ok(AppAction::Continue);
            }
        };

        let client = self.client.clone();
        let host = self.host_session_id.clone();
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut c = client.lock().await;
                let live = c.live_session_id(&host).await;
                c.mutate_session_heartbeat(&live, op as i32, &prompt, interval)
                    .await
            })
        });
        let report = match result {
            Ok(detail) => format_session_heartbeat(&detail),
            Err(e) => format!("/heartbeat: failed — {e}"),
        };
        self.push_system_message(report);
        Ok(AppAction::Continue)
    }

    fn rpc_clear_goal(&self) -> Result<()> {
        let client = self.client.clone();
        let host = self.host_session_id.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut c = client.lock().await;
                let live = c.live_session_id(&host).await;
                c.clear_goal(&live).await
            })
        })
    }

    fn rpc_set_goal_status(&self, status: &str) -> Result<protocols::admin::GoalDetail> {
        let client = self.client.clone();
        let host = self.host_session_id.clone();
        let status = status.to_string();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut c = client.lock().await;
                let live = c.live_session_id(&host).await;
                c.set_goal_status(&live, &status).await
            })
        })
    }

    fn rpc_set_goal_budget(&self, budget: Option<i64>) -> Result<protocols::admin::GoalDetail> {
        let client = self.client.clone();
        let host = self.host_session_id.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut c = client.lock().await;
                let live = c.live_session_id(&host).await;
                c.set_goal_budget(&live, budget).await
            })
        })
    }

    fn rpc_set_goal_replace(
        &self,
        objective: &str,
        budget: Option<i64>,
    ) -> Result<protocols::admin::GoalDetail> {
        let client = self.client.clone();
        let host = self.host_session_id.clone();
        let objective = objective.to_string();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut c = client.lock().await;
                let live = c.live_session_id(&host).await;
                c.set_goal_replace(&live, &objective, budget).await
            })
        })
    }

    // ── /voice ─────────────────────────────────────────────────────────

    /// Dispatch `/voice [on|off|rec|tts|status]`. Bare `/voice` toggles
    /// the continuous loop on/off. Unknown subcommands print usage —
    /// silently doing nothing erodes user trust when a typo happens.
    pub(super) fn cmd_voice(&mut self, sub: &str) -> Result<AppAction> {
        let normalized = sub.trim().to_lowercase();
        match normalized.as_str() {
            "" | "toggle" => {
                if self.voice_loop.is_some() {
                    self.cmd_voice_stop()
                } else {
                    self.cmd_voice_start(VoiceMode::Continuous)
                }
            }
            "on" => self.cmd_voice_start(VoiceMode::Continuous),
            "rec" | "record" => self.cmd_voice_start(VoiceMode::Single),
            "off" => self.cmd_voice_stop(),
            "tts" => self.cmd_voice_tts_toggle(),
            "status" => self.cmd_voice_status(),
            _ => {
                self.push_system_message(
                    "Usage: /voice [on|off|rec|tts|status]\n  \
                     /voice         toggle continuous voice mode\n  \
                     /voice rec     single recording, auto-submits transcript\n  \
                     /voice tts     toggle speaking the agent's replies\n  \
                     /voice status  show current voice-mode state"
                        .to_string(),
                );
                Ok(AppAction::Continue)
            }
        }
    }

    fn cmd_voice_start(&mut self, mode: VoiceMode) -> Result<AppAction> {
        // Run the precheck in a pure function so the guard table is
        // testable without spinning up an App. A `Some(msg)` here means
        // the user gets `msg` as a system reply and the command exits;
        // `None` means we proceed to open the mic.
        if let Some(msg) = voice_start_precheck(&self.config, self.voice_loop.is_some()) {
            self.push_system_message(msg);
            return Ok(AppAction::Continue);
        }
        // Precheck guarantees `from_config` returns `Some` here. Still
        // pattern-match (rather than `expect`/`unwrap`) to satisfy
        // clippy's expect-used lint and surface any future regression
        // in the precheck as a clean user-visible message rather than
        // a panic.
        let transcriber =
            match cortex::media::understanding::AudioTranscriber::from_config(&self.config) {
                Some(t) => std::sync::Arc::new(t),
                None => {
                    self.push_system_message(
                        "voice: STT provider vanished between precheck and start. \
                     Re-run /voice on after fixing audio.* config."
                            .to_string(),
                    );
                    return Ok(AppAction::Continue);
                }
            };

        let recorder_result =
            cortex::media::voice_capture::AudioRecorder::new(self.config.voice.clone());
        let recorder = match recorder_result {
            Ok(r) => {
                std::sync::Arc::new(r) as std::sync::Arc<dyn cortex::media::voice_capture::Recorder>
            }
            Err(e) => {
                self.push_system_message(format!(
                    "voice: failed to open microphone — {e}.\n  \
                     Ensure a default input device is available."
                ));
                return Ok(AppAction::Continue);
            }
        };

        let (voice_loop, voice_rx) = cortex::media::voice_loop::VoiceLoop::new(
            self.config.voice.clone(),
            recorder,
            transcriber,
        );

        let start_result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                match mode {
                    VoiceMode::Continuous => voice_loop.start_continuous().await,
                    VoiceMode::Single => voice_loop.start_single().await,
                }
            })
        });
        if let Err(e) = start_result {
            self.push_system_message(format!("voice: failed to start — {e}"));
            return Ok(AppAction::Continue);
        }

        self.voice_single_shot = matches!(mode, VoiceMode::Single);
        self.voice_loop = Some(voice_loop);
        self.voice_rx = Some(voice_rx);
        self.voice_status = Some(cortex::media::voice_loop::VoiceStatus::Listening);

        let label = match mode {
            VoiceMode::Continuous => "voice mode enabled — listening (continuous).",
            VoiceMode::Single => "voice mode enabled — single recording.",
        };
        let tts_note = if self.config.voice.auto_tts {
            " TTS on."
        } else {
            ""
        };
        self.push_system_message(format!("{label}{tts_note}\n  /voice off to stop."));
        Ok(AppAction::Continue)
    }

    fn cmd_voice_stop(&mut self) -> Result<AppAction> {
        let Some(vl) = self.voice_loop.take() else {
            self.push_system_message("voice mode is not running.".to_string());
            return Ok(AppAction::Continue);
        };
        // Stop the audio playback first so a TTS reply that's still
        // talking doesn't keep going after the user hit "off".
        cortex::media::playback::stop_playback();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                vl.stop().await;
                vl.shutdown().await;
            });
        });
        self.voice_rx = None;
        self.voice_status = None;
        self.voice_single_shot = false;
        self.push_system_message("voice mode disabled.".to_string());
        Ok(AppAction::Continue)
    }

    fn cmd_voice_tts_toggle(&mut self) -> Result<AppAction> {
        let new_value = !self.config.voice.auto_tts;
        // Apply locally so `/voice status` reflects the change
        // immediately, and persist via the standard settings path.
        self.config.voice.auto_tts = new_value;
        let state = if new_value { "enabled" } else { "disabled" };
        self.push_system_message(format!("voice TTS {state}."));
        Ok(AppAction::UpdateSetting {
            key: "voice.auto_tts".to_string(),
            value: new_value.to_string(),
        })
    }

    fn cmd_voice_status(&mut self) -> Result<AppAction> {
        let on = self.voice_loop.is_some();
        let tts = self.config.voice.auto_tts;
        let enabled = self.config.voice.enabled;
        let mode_label = match (on, self.voice_single_shot) {
            (false, _) => "off",
            (true, true) => "single-shot",
            (true, false) => "continuous",
        };
        let status_label = match self.voice_status {
            Some(cortex::media::voice_loop::VoiceStatus::Listening) => "listening",
            Some(cortex::media::voice_loop::VoiceStatus::Transcribing) => "transcribing",
            Some(cortex::media::voice_loop::VoiceStatus::Idle) | None => "idle",
        };
        let transcriber_ready =
            cortex::media::understanding::AudioTranscriber::from_config(&self.config).is_some();
        let stt_label = if transcriber_ready {
            "configured"
        } else {
            "MISSING — set audio.enabled + audio.models"
        };
        let tts_ready = cortex::media::tts::TtsSynthesizer::from_config(&self.config).is_some();
        let tts_provider_label = if tts_ready {
            "configured"
        } else {
            "MISSING — set tts.enabled + tts.models"
        };
        self.push_system_message(format!(
            "Voice Mode\n  \
             Allowed:        {}\n  \
             Mode:           {}\n  \
             State:          {}\n  \
             Auto-submit:    {}\n  \
             Auto-TTS:       {}\n  \
             STT provider:   {}\n  \
             TTS provider:   {}",
            if enabled {
                "yes"
            } else {
                "no (voice.enabled is false)"
            },
            mode_label,
            status_label,
            if self.config.voice.auto_submit {
                "on"
            } else {
                "off"
            },
            if tts { "on" } else { "off" },
            stt_label,
            tts_provider_label,
        ));
        Ok(AppAction::Continue)
    }
}

/// Which flavor of voice loop `/voice` starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VoiceMode {
    Continuous,
    Single,
}

/// Render a daemon `ContextBreakdown` as transcript text: one bar-chart row
/// per component plus a free-space row and a total line. Pure so the format
/// is testable without a daemon.
pub(super) fn format_context_breakdown(breakdown: &protocols::session::ContextBreakdown) -> String {
    const BAR_WIDTH: usize = 24;
    /// Detail rows rendered per parent bucket before collapsing to "+N more"
    /// — keeps a many-skill / many-server install from flooding the popup.
    const DETAIL_CAP: usize = 10;
    let window = breakdown.context_window.max(1);
    // Detail rows itemize part of their parent bucket: counting them here
    // would double-charge the window.
    let used: u64 = breakdown
        .components
        .iter()
        .filter(|c| !c.detail)
        .map(|c| c.tokens)
        .sum();
    let free = breakdown.context_window.saturating_sub(used);
    let label_width = breakdown
        .components
        .iter()
        .filter(|c| !c.detail)
        .map(|c| c.label.len())
        .chain(std::iter::once("Free space".len()))
        .max()
        .unwrap_or(0);

    let mut out = format!(
        "Context window: ~{used} / {} tokens ({:.0}% used)\n",
        breakdown.context_window,
        (used as f64 / window as f64) * 100.0
    );
    let push_bar_row = |out: &mut String, label: &str, tokens: u64| {
        let frac = tokens as f64 / window as f64;
        let filled = ((frac * BAR_WIDTH as f64).round() as usize).min(BAR_WIDTH);
        let bar: String = "█".repeat(filled) + &"░".repeat(BAR_WIDTH - filled);
        out.push_str(&format!(
            "  {label:<label_width$}  {bar}  ~{tokens:>8} ({:>4.1}%)\n",
            frac * 100.0
        ));
    };

    let comps = &breakdown.components;
    let mut i = 0;
    while i < comps.len() {
        push_bar_row(&mut out, &comps[i].label, comps[i].tokens);
        i += 1;
        // Gather this bucket's detail children (they directly follow it).
        let mut details: Vec<&protocols::session::ContextComponent> = Vec::new();
        while i < comps.len() && comps[i].detail {
            details.push(&comps[i]);
            i += 1;
        }
        // Largest first; defensive re-sort in case a daemon version emits
        // them unsorted (name tiebreak keeps equal sizes stable).
        details.sort_by(|a, b| b.tokens.cmp(&a.tokens).then_with(|| a.label.cmp(&b.label)));
        for d in details.iter().take(DETAIL_CAP) {
            let pct = (d.tokens as f64 / window as f64) * 100.0;
            out.push_str(&format!("    ↳ {}  ~{} ({pct:.1}%)\n", d.label, d.tokens));
        }
        if details.len() > DETAIL_CAP {
            out.push_str(&format!("    ↳ +{} more\n", details.len() - DETAIL_CAP));
        }
    }
    push_bar_row(&mut out, "Free space", free);
    out.push_str("Estimates reflect the next request, not cumulative session spend.");
    out
}

/// Pure precheck for `/voice on` / `/voice rec`. Returns the system-message
/// text to surface when the command can't run; `None` means "go ahead and
/// open the mic". Keeps the guard table out of the App so it can be tested
/// without a full TUI harness.
///
/// Order matters: a disabled voice config always loses to anything else
/// (telling the user "already running" when the feature is disabled would
/// be a lie). The "already running" guard fires before STT checks so
/// `voice on` while a loop is live doesn't change its mind based on
/// transient credential availability.
pub(super) fn voice_start_precheck(
    config: &cortex::config::Config,
    voice_loop_running: bool,
) -> Option<String> {
    if !config.voice.enabled {
        return Some(
            "voice mode is disabled. Enable it first:\n  \
             /settings voice.enabled true"
                .to_string(),
        );
    }
    if voice_loop_running {
        return Some("voice mode is already running.".to_string());
    }
    if cortex::media::understanding::AudioTranscriber::from_config(config).is_none() {
        return Some(
            "voice: no audio transcription provider configured.\n  \
             Set `audio.enabled true` and add at least one model to `audio.models`."
                .to_string(),
        );
    }
    None
}

/// Render a `HandoffStatusReply` as a 3–5 line summary card body.
fn format_handoff_status(reply: &protocols::admin::HandoffStatusReply) -> String {
    let mut out = String::from("Handoff\n───────\n");
    out.push_str(&format!("state:    {}\n", reply.state));
    if !reply.target_platform.is_empty() {
        out.push_str(&format!("target:   {}\n", reply.target_platform));
    }
    if reply.requested_at_unix > 0 {
        out.push_str(&format!(
            "requested: {}\n",
            chrono::DateTime::from_timestamp(reply.requested_at_unix, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(|| reply.requested_at_unix.to_string())
        ));
    }
    if !reply.error.is_empty() {
        out.push_str(&format!("error:    {}\n", reply.error));
    }
    out
}

/// Render a [`protocols::admin::GoalDetail`] as a 5-7 line summary card body.
/// Mirrors [`format_goal_summary`] — but consumes the wire type rather than
/// the in-process `cortex::goals::Goal`, since the TUI no longer opens the
/// DB directly for reads.
fn format_goal_detail(detail: &protocols::admin::GoalDetail) -> String {
    let mut out = String::from("Goal\n────\n");
    let status_label = cortex::goals::GoalStatus::from_str(&detail.status)
        .map(|s| s.label().to_string())
        .unwrap_or_else(|| detail.status.clone());
    out.push_str(&format!("Status:    {status_label}\n"));
    out.push_str(&format!("Objective: {}\n", detail.objective));
    out.push_str(&format!(
        "Time used: {}\n",
        format_elapsed_seconds(detail.time_used_seconds)
    ));
    let budget = if detail.token_budget_present {
        detail.token_budget.to_string()
    } else {
        "unbounded".to_string()
    };
    out.push_str(&format!("Tokens:    {} / {}\n", detail.tokens_used, budget));
    let parsed = cortex::goals::GoalStatus::from_str(&detail.status);
    let hint = match parsed {
        Some(cortex::goals::GoalStatus::Active) => {
            "Commands: /goal <new text>, /goal budget N, /goal pause, /goal clear"
        }
        Some(cortex::goals::GoalStatus::Paused) => {
            "Commands: /goal <new text>, /goal budget N, /goal resume, /goal clear"
        }
        Some(cortex::goals::GoalStatus::Blocked)
        | Some(cortex::goals::GoalStatus::UsageLimited) => {
            "Commands: /goal <new text>, /goal resume, /goal clear"
        }
        Some(cortex::goals::GoalStatus::BudgetLimited)
        | Some(cortex::goals::GoalStatus::Complete) => "Commands: /goal <new text>, /goal clear",
        None => "Commands: /goal <new text>, /goal clear",
    };
    out.push_str(hint);
    out
}

/// Multi-line usage card for `/goal`.
/// Render a gate list (plus any mutation report) for the transcript.
fn format_goal_gates(list: &protocols::admin::GoalGateList) -> String {
    let mut out = String::new();
    if !list.report.is_empty() {
        out.push_str(&format!("/goal gate: {}\n", list.report));
    }
    if list.gates.is_empty() {
        out.push_str("No quality gates. Add one with `/goal gate add <command>`.");
        return out;
    }
    out.push_str("Quality gates (all must exit 0 before the goal completes):");
    for g in &list.gates {
        // Report the last verdict, not just the command — "which of these is
        // red?" is the question a user actually has here.
        let state = if !g.last_exit_code_present {
            "not yet run".to_string()
        } else if g.last_exit_code == 0 {
            "passing".to_string()
        } else {
            format!(
                "FAILING (exit {}, {} attempt(s))",
                g.last_exit_code, g.attempts
            )
        };
        out.push_str(&format!("\n  {}. {}  — {state}", g.ordinal, g.command));
    }
    out
}

const GOAL_USAGE: &str = "Usage:\n  /goal                       Show current goal\n  \
     /goal <text>                Set or replace the session goal\n  \
     /goal --budget N <text>     Set with a token budget\n  \
     /goal budget N|none         Change the budget in place\n  \
     /goal edit <text>           Replace the objective\n  \
     /goal pause|resume|clear    Manage goal status\n  \
     /goal gate add <cmd>        Require <cmd> to exit 0 before completing\n  \
     /goal gate [list]           Show gates and their last result\n  \
     /goal gate remove <N>       Drop gate N\n  \
     /goal gate clear            Drop all gates";

/// Parsed `/heartbeat …` subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum HeartbeatSubcommand {
    /// `/heartbeat` / `/heartbeat status`.
    Status,
    /// `/heartbeat every <interval> <prompt>`.
    Set {
        /// Firing interval in seconds (clamped to the 60s floor server-side).
        interval_secs: i64,
        /// Prompt injected as an ordinary user message.
        prompt: String,
    },
    /// `/heartbeat pause`.
    Pause,
    /// `/heartbeat resume`.
    Resume,
    /// `/heartbeat clear`.
    Clear,
    /// Anything else — show usage rather than guessing.
    Usage,
}

/// Parse a duration like `90s`, `10m`, `2h`, `1d`, or a bare number of seconds.
///
/// Returns `None` for anything unrecognized so the caller can show usage — a
/// silently-misread interval is worse than an error, since the user would only
/// discover it when the heartbeat fired at the wrong cadence.
pub(super) fn parse_interval_secs(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (digits, mult) = match raw.chars().last()? {
        's' | 'S' => (&raw[..raw.len() - 1], 1),
        'm' | 'M' => (&raw[..raw.len() - 1], 60),
        'h' | 'H' => (&raw[..raw.len() - 1], 3600),
        'd' | 'D' => (&raw[..raw.len() - 1], 86_400),
        _ => (raw, 1),
    };
    let n: i64 = digits.trim().parse().ok()?;
    if n <= 0 {
        return None;
    }
    n.checked_mul(mult)
}

/// What `/worktree` was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum WorktreeCommand {
    /// Report the current worktree and list the others.
    Overview,
    /// Create a worktree named `name`.
    Create { name: String },
    /// Delete the worktree matching `target`.
    Delete { target: String, force: bool },
}

/// Usage text shown for anything this parser does not recognize.
const WORKTREE_USAGE: &str = "Usage:\n  \
     /worktree                    where this session is working, and what else exists\n  \
     /worktree new <name>         create an isolated checkout on its own branch\n  \
     /worktree remove <name>      delete one (add --force to discard changes)";

/// Parse the text after `/worktree`.
///
/// `Err` carries the usage text rather than an error type: every rejection here
/// ends the same way — showing the operator the three forms that work.
pub(super) fn parse_worktree_command(rest: &str) -> Result<WorktreeCommand, String> {
    let rest = rest.trim();
    if rest.is_empty() || rest == "list" || rest == "status" {
        return Ok(WorktreeCommand::Overview);
    }
    let (verb, arg) = match rest.split_once(char::is_whitespace) {
        Some((v, a)) => (v, a.trim()),
        None => (rest, ""),
    };
    match verb {
        "new" | "add" | "create" => {
            if arg.is_empty() {
                return Err(WORKTREE_USAGE.to_string());
            }
            Ok(WorktreeCommand::Create {
                name: arg.to_string(),
            })
        }
        "remove" | "rm" | "delete" => {
            // `--force` is accepted on either side of the name so an operator
            // retrying after a refusal can just append it to what they typed.
            let mut force = false;
            let mut words: Vec<&str> = Vec::new();
            for word in arg.split_whitespace() {
                if word == "--force" || word == "-f" {
                    force = true;
                } else {
                    words.push(word);
                }
            }
            let target = words.join(" ");
            if target.is_empty() {
                return Err(WORKTREE_USAGE.to_string());
            }
            Ok(WorktreeCommand::Delete { target, force })
        }
        _ => Err(WORKTREE_USAGE.to_string()),
    }
}

/// Execute a parsed `/worktree` command and return the text to display.
pub(super) async fn run_worktree_command(cmd: WorktreeCommand) -> String {
    use crate::commands::worktree::tui;
    match cmd {
        WorktreeCommand::Overview => tui::overview().await,
        WorktreeCommand::Create { name } => tui::create(&name).await,
        WorktreeCommand::Delete { target, force } => tui::delete(&target, force).await,
    }
}

/// Parse the text after `/heartbeat`.
pub(super) fn parse_heartbeat_subcommand(rest: &str) -> HeartbeatSubcommand {
    let rest = rest.trim();
    match rest {
        "" | "status" => return HeartbeatSubcommand::Status,
        "pause" => return HeartbeatSubcommand::Pause,
        "resume" => return HeartbeatSubcommand::Resume,
        "clear" | "off" => return HeartbeatSubcommand::Clear,
        _ => {}
    }
    let Some(after) = rest.strip_prefix("every ") else {
        return HeartbeatSubcommand::Usage;
    };
    let mut parts = after.trim().splitn(2, char::is_whitespace);
    let Some(interval_raw) = parts.next() else {
        return HeartbeatSubcommand::Usage;
    };
    let Some(interval_secs) = parse_interval_secs(interval_raw) else {
        return HeartbeatSubcommand::Usage;
    };
    // The prompt is the rest of the line verbatim — it is prose, so splitting
    // or normalizing it would change what the agent is asked to do.
    let prompt = parts.next().unwrap_or("").trim().to_string();
    if prompt.is_empty() {
        return HeartbeatSubcommand::Usage;
    }
    HeartbeatSubcommand::Set {
        interval_secs,
        prompt,
    }
}

/// The daemon's own reason from an RPC error, without the
/// `<Op> failed (<Code>): <handler>: ` frame `rpc_err` puts around it. The
/// frame is for logs; in the transcript it reads as a crash.
fn daemon_reason(e: &anyhow::Error) -> String {
    let text = e.to_string();
    let Some((frame, reason)) = text.split_once("): ") else {
        return text;
    };
    if !frame.contains(" failed (") {
        return text;
    }
    match reason.split_once(": ") {
        Some((handler, rest)) if !handler.contains(' ') => rest.to_string(),
        _ => reason.to_string(),
    }
}

/// Render `SessionHeartbeatDetail` (plus any mutation report) for the transcript.
fn format_session_heartbeat(d: &protocols::admin::SessionHeartbeatDetail) -> String {
    let mut out = String::new();
    if !d.report.is_empty() {
        out.push_str(&format!("/heartbeat: {}\n", d.report));
    }
    if !d.present {
        out.push_str(
            "No heartbeat set for this session. \
             Example: `/heartbeat every 15m Check whether the deploy finished`.",
        );
        return out;
    }
    let state = if d.paused {
        "paused".to_string()
    } else {
        format!("next fire in {}", format_secs(d.seconds_until_due))
    };
    out.push_str(&format!(
        "Heartbeat: every {} — {state}\n  \"{}\"",
        format_interval(d.interval_secs),
        d.prompt
    ));
    out
}

/// An interval exactly as set: the largest unit that divides it, so
/// `every 90s` reads back as `90s`, not a rounded `1m`.
fn format_interval(secs: i64) -> String {
    match secs {
        s if s > 0 && s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s > 0 && s % 3600 == 0 => format!("{}h", s / 3600),
        s if s > 0 && s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// Approximate time left, for a countdown: `45s`, `15m`, `2h`, `1d`.
fn format_secs(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

const HEARTBEAT_USAGE: &str = "Usage:\n  \
     /heartbeat every <interval> <prompt>   Recurring prompt in THIS session (min 60s)\n  \
     /heartbeat [status]                    Show it\n  \
     /heartbeat pause|resume|clear          Manage it\n\n\
     Intervals: 90s, 10m, 2h, 1d. For durable isolated jobs use the scheduler instead.";

/// The `gate` verbs under `/goal`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GateSubcommand {
    /// `/goal gate` / `/goal gate list`.
    List,
    /// `/goal gate add <command>`.
    Add(String),
    /// `/goal gate remove <N>`.
    Remove(i64),
    /// `/goal gate clear`.
    Clear,
    /// Anything else under `gate` — show usage rather than guessing.
    Usage,
}

/// Parsed `/goal …` subcommand. Splits the dispatcher's pure-parse half from
/// the side-effecting RPC half so the parser can be tested independently.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum GoalSubcommand {
    /// `/goal` (no args) — show summary card.
    Show,
    /// `/goal clear`.
    Clear,
    /// `/goal pause`.
    Pause,
    /// `/goal resume`.
    Resume,
    /// `/goal edit` (bare) — surfaces a usage error rather than silently
    /// setting the objective to the literal text "edit".
    BareEdit,
    /// `/goal edit ` (trailing whitespace, empty objective).
    EmptyObjective,
    /// `/goal --budget` with no value — show usage.
    Usage,
    /// `/goal budget N` / `/goal budget none` — change the token budget of the
    /// existing goal in place (preserves `goal_id` + accounting). `None`
    /// clears the budget (unbounded). Distinct from the `--budget N <text>`
    /// prefix on [`GoalSubcommand::Set`], which replaces the objective.
    Budget {
        /// New budget; `None` means unbounded.
        budget: Option<i64>,
    },
    /// `/goal gate …` — manage the goal's quality gates.
    Gate(GateSubcommand),
    /// `/goal <text>` or `/goal edit <text>` or `/goal --budget N <text>`.
    Set {
        /// Raw objective text (unvalidated, to be passed to
        /// [`cortex::goals::validate_objective`]).
        objective: String,
        /// Optional token budget from the `--budget N` prefix.
        budget: Option<i64>,
    },
}

/// Parse the text after `/goal gate`.
///
/// `add` deliberately takes the rest of the line verbatim — a gate command is a
/// shell line (`cargo test -p coding -- --nocapture`), so splitting on
/// whitespace or trying to strip quotes would mangle it.
pub(super) fn parse_gate_subcommand(rest: &str) -> GateSubcommand {
    let rest = rest.trim();
    if rest.is_empty() || rest == "list" {
        return GateSubcommand::List;
    }
    if rest == "clear" {
        return GateSubcommand::Clear;
    }
    if let Some(command) = rest.strip_prefix("add ") {
        let command = command.trim();
        if command.is_empty() {
            return GateSubcommand::Usage;
        }
        return GateSubcommand::Add(command.to_string());
    }
    if let Some(n) = rest
        .strip_prefix("remove ")
        .or_else(|| rest.strip_prefix("rm "))
    {
        // An unparseable ordinal is a typo, not a request to add a gate named
        // "remove x" — say so instead of doing something surprising.
        return match n.trim().parse::<i64>() {
            Ok(n) if n > 0 => GateSubcommand::Remove(n),
            _ => GateSubcommand::Usage,
        };
    }
    GateSubcommand::Usage
}

pub(super) fn parse_goal_subcommand(args: &str) -> GoalSubcommand {
    match args.trim() {
        "" => return GoalSubcommand::Show,
        "clear" => return GoalSubcommand::Clear,
        "pause" => return GoalSubcommand::Pause,
        "resume" => return GoalSubcommand::Resume,
        "edit" => return GoalSubcommand::BareEdit,
        "--budget" => return GoalSubcommand::Usage,
        "gate" | "gates" => return GoalSubcommand::Gate(GateSubcommand::List),
        _ => {}
    }
    // `gate` verbs are matched before the objective fallback so `/goal gate
    // add cargo test` manages a gate rather than replacing the objective with
    // the literal text "gate add cargo test".
    for prefix in ["gate ", "gates "] {
        if let Some(rest) = args.trim().strip_prefix(prefix) {
            return GoalSubcommand::Gate(parse_gate_subcommand(rest.trim()));
        }
    }
    // `/goal budget N` / `/goal budget none` — in-place budget change. Only a
    // lone integer or none/unbounded qualifies. A word-valued first token (e.g.
    // `budget report for Q3`) falls through and is treated as an objective, so
    // this never hijacks a legitimate goal that happens to start with "budget".
    // But a numeric-looking value — with trailing junk (`budget 5000 tokens`)
    // or a malformed number (`budget 5,000`) — routes to Usage rather than
    // silently becoming an objective *replace* that resets accounting: the
    // operator clearly meant a budget change, so tell them the correct form.
    if let Some(rest) = args.trim().strip_prefix("budget ") {
        let rest = rest.trim();
        let mut parts = rest.splitn(2, char::is_whitespace);
        let first = parts.next().unwrap_or("");
        let has_trailing = parts.next().is_some_and(|t| !t.trim().is_empty());
        let is_clear_kw =
            first.eq_ignore_ascii_case("none") || first.eq_ignore_ascii_case("unbounded");
        let looks_numeric = first
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit() || c == '-');
        if is_clear_kw || looks_numeric {
            if has_trailing {
                return GoalSubcommand::Usage;
            }
            if is_clear_kw {
                return GoalSubcommand::Budget { budget: None };
            }
            return match first.parse::<i64>() {
                Ok(n) => GoalSubcommand::Budget { budget: Some(n) },
                // Numeric-looking but unparseable (`5,000`, `5000x`) — surface
                // usage instead of an accidental objective replace.
                Err(_) => GoalSubcommand::Usage,
            };
        }
    }
    // A literal `--budget ` prefix is always an attempt at the flag — if the
    // value doesn't parse as an integer, treat that as Usage rather than
    // letting the malformed flag text leak into the objective. "Objective
    // starts with --budget" is far less likely than a typo on the budget.
    if args.starts_with("--budget ") && !args.starts_with("--budget --") {
        let after = &args["--budget ".len()..];
        let first = after.split_whitespace().next().unwrap_or("");
        if first.parse::<i64>().is_err() {
            return GoalSubcommand::Usage;
        }
    }
    let (budget, body) = parse_goal_budget_prefix(args);
    // `--budget N` with nothing after the number → usage, not a goal whose
    // objective is the empty string.
    if budget.is_some() && body.trim().is_empty() {
        return GoalSubcommand::Usage;
    }
    // `edit ` prefix strips for the `/goal edit <text>` form. Trim trailing
    // whitespace after stripping so `/goal edit ` reports empty cleanly
    // rather than falling into the generic "objective must not be empty"
    // path via validate_objective (which would also work, but the explicit
    // arm makes the test surface sharper).
    let after_edit = body.strip_prefix("edit ").map(str::trim);
    if let Some(stripped) = after_edit {
        if stripped.is_empty() {
            return GoalSubcommand::EmptyObjective;
        }
        return GoalSubcommand::Set {
            objective: stripped.to_string(),
            budget,
        };
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return GoalSubcommand::EmptyObjective;
    }
    GoalSubcommand::Set {
        objective: trimmed.to_string(),
        budget,
    }
}

/// Parse a leading `--budget N` flag from `args`, returning `(budget, rest)`.
///
/// `rest` is the post-flag remainder (objective text). When the flag is
/// present with a numeric value but no trailing objective (e.g. `--budget 5000`
/// or `--budget 5000 `), returns `(Some(n), "")` so the dispatcher can
/// report a usage error rather than letting `parse_goal_subcommand` treat
/// the literal `--budget 5000` text as the objective.
fn parse_goal_budget_prefix(args: &str) -> (Option<i64>, &str) {
    if let Some(rest) = args.strip_prefix("--budget ") {
        let mut parts = rest.splitn(2, char::is_whitespace);
        if let Some(num) = parts.next() {
            if let Ok(n) = num.parse::<i64>() {
                let rest2 = parts.next().unwrap_or("");
                return (Some(n), rest2);
            }
        }
    }
    (None, args)
}

fn format_elapsed_seconds(s: i64) -> String {
    let s = s.max(0);
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    let rem = s % 60;
    if m < 60 {
        return format!("{m}m {rem}s");
    }
    let h = m / 60;
    let mm = m % 60;
    format!("{h}h {mm}m")
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
            "Pruned {} log files ({} bytes truncated), {} activity rows, evicted {} embeddings, healed {} stalled tasks, recovered {} wedged runs, purged {} messages, tombstoned {} audit events",
            report.pruned_logs,
            report.log_bytes_truncated,
            report.pruned_activity,
            report.evicted_embeddings,
            report.healed_tasks,
            report.wedged_runs_recovered,
            report.messages_purged,
            report.audit_events_tombstoned,
        ));
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

/// Render a daemon-side [`protocols::admin::ReloadResponse`] as a one-line
/// (or short multi-line) system message. `target` is the proto enum value
/// the caller used; it shapes the heading and decides which sections to
/// include.
pub(super) fn format_reload_response(
    target: i32,
    resp: &protocols::admin::ReloadResponse,
) -> String {
    use protocols::admin::reload_request::Target;
    let parsed = Target::try_from(target).unwrap_or(Target::All);
    let (show_mcp, show_skills) = match parsed {
        Target::Mcp => (true, false),
        Target::Skills => (false, true),
        Target::All | Target::Unspecified => (true, true),
    };
    let mut lines: Vec<String> = Vec::new();
    if show_mcp {
        let mut parts: Vec<String> = Vec::new();
        if !resp.mcp_added.is_empty() {
            parts.push(format!(
                "+{} ({})",
                resp.mcp_added.len(),
                resp.mcp_added.join(", ")
            ));
        }
        if !resp.mcp_removed.is_empty() {
            parts.push(format!(
                "-{} ({})",
                resp.mcp_removed.len(),
                resp.mcp_removed.join(", ")
            ));
        }
        if resp.mcp_unchanged > 0 {
            parts.push(format!("{} unchanged", resp.mcp_unchanged));
        }
        if !resp.mcp_failures.is_empty() {
            parts.push(format!("{} failed", resp.mcp_failures.len()));
        }
        let summary = if parts.is_empty() {
            "no MCP servers configured".to_string()
        } else {
            parts.join(", ")
        };
        lines.push(format!(
            "MCP reloaded: {summary}; {} tool(s) available",
            resp.mcp_total_tools
        ));
        for f in &resp.mcp_failures {
            lines.push(format!("  • {}: {}", f.server, f.error));
        }
    }
    if show_skills {
        let missing = if resp.skills_user_unavailable > 0 {
            format!(", {} missing deps", resp.skills_user_unavailable)
        } else {
            String::new()
        };
        let disabled = if resp.skills_disabled > 0 {
            format!(", {} disabled", resp.skills_disabled)
        } else {
            String::new()
        };
        lines.push(format!(
            "Skills reloaded: {} total ({} user{missing}{disabled}); next turn picks up changes",
            resp.skills_total, resp.skills_user,
        ));
    }
    lines.join("\n")
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
    use crate::tui::app::{parse_export_destination, ExportDestination};
    use cortex::types::Role;

    #[test]
    fn heartbeat_off_clears_like_clear() {
        assert_eq!(
            parse_heartbeat_subcommand("off"),
            HeartbeatSubcommand::Clear
        );
        assert_eq!(
            parse_heartbeat_subcommand("clear"),
            HeartbeatSubcommand::Clear
        );
    }

    #[test]
    fn a_goal_rpc_failure_shows_only_the_daemon_reason() {
        let e = anyhow::anyhow!(
            "Admin.SetGoal{{SetBudget}} failed (FailedPrecondition): set_goal: no goal set for this session"
        );
        assert_eq!(daemon_reason(&e), "no goal set for this session");
        // Not an RPC frame: the text stays as it is.
        let e = anyhow::anyhow!("connection refused: daemon down");
        assert_eq!(daemon_reason(&e), "connection refused: daemon down");
    }

    /// Spec: the destination is optional, so every pre-existing `/export` and
    /// `/export <fmt>` spelling must still mean "write the suggested filename
    /// into the current directory".
    #[test]
    fn an_absent_export_destination_stays_the_default_file() {
        for input in ["", "   "] {
            assert_eq!(
                parse_export_destination(input),
                ExportDestination::DefaultFile,
                "input {input:?}"
            );
        }
    }

    #[test]
    fn clipboard_is_recognized_regardless_of_case() {
        for input in ["clipboard", "Clipboard", "CLIPBOARD"] {
            assert_eq!(
                parse_export_destination(input),
                ExportDestination::Clipboard,
                "input {input:?}"
            );
        }
    }

    /// A `~` must be expanded here rather than handed to the OS, which does no
    /// such expansion and would create a directory literally named `~`.
    #[test]
    fn a_tilde_destination_is_expanded() {
        let ExportDestination::File(path) = parse_export_destination("~/out.md") else {
            panic!("a path destination");
        };
        assert!(path.is_absolute(), "{path:?}");
        assert!(!path.to_string_lossy().starts_with('~'), "{path:?}");
        assert!(path.ends_with("out.md"));
    }

    /// Paths with spaces are ordinary on macOS, so only the *first* word is
    /// the format — the rest is the destination, verbatim.
    #[test]
    fn a_destination_may_contain_spaces() {
        assert_eq!(
            parse_export_destination("my notes/session one.md"),
            ExportDestination::File("my notes/session one.md".into())
        );
    }

    /// The bare form and its read-only synonyms must never mutate anything —
    /// an operator typing `/worktree` to look around must not create one.
    #[test]
    fn parse_worktree_bare_forms_are_read_only() {
        for input in ["", "  ", "list", "status"] {
            assert_eq!(
                parse_worktree_command(input),
                Ok(WorktreeCommand::Overview),
                "input {input:?}"
            );
        }
    }

    /// All three creation verbs are accepted because operators arrive from
    /// different tools; the name is taken verbatim so slugging stays in one
    /// place (the core module), not duplicated in the parser.
    #[test]
    fn parse_worktree_create_accepts_each_verb_and_keeps_the_name_verbatim() {
        for verb in ["new", "add", "create"] {
            assert_eq!(
                parse_worktree_command(&format!("{verb} Fix Login Bug")),
                Ok(WorktreeCommand::Create {
                    name: "Fix Login Bug".to_string()
                }),
                "verb {verb}"
            );
        }
    }

    /// `--force` is the difference between refusing and discarding uncommitted
    /// work, so it must be recognized wherever the operator puts it — and must
    /// never be mistaken for the worktree's name.
    #[test]
    fn parse_worktree_remove_reads_force_from_either_side() {
        assert_eq!(
            parse_worktree_command("remove spike"),
            Ok(WorktreeCommand::Delete {
                target: "spike".to_string(),
                force: false
            })
        );
        for input in [
            "remove spike --force",
            "rm --force spike",
            "delete -f spike",
        ] {
            assert_eq!(
                parse_worktree_command(input),
                Ok(WorktreeCommand::Delete {
                    target: "spike".to_string(),
                    force: true
                }),
                "input {input:?}"
            );
        }
    }

    /// A verb with no target, or an unknown verb, must show usage rather than
    /// guess. Guessing here deletes the wrong checkout.
    #[test]
    fn parse_worktree_rejects_incomplete_and_unknown_commands() {
        for input in ["new", "add   ", "remove", "rm --force", "switch fix", "-f"] {
            let got = parse_worktree_command(input);
            assert_eq!(
                got,
                Err(WORKTREE_USAGE.to_string()),
                "input {input:?} must show usage, got {got:?}"
            );
        }
    }

    #[test]
    fn parse_goal_budget_prefix_extracts_numeric_budget() {
        let (b, rest) = parse_goal_budget_prefix("--budget 5000 ship it");
        assert_eq!(b, Some(5000));
        assert_eq!(rest, "ship it");
    }

    #[test]
    fn parse_goal_budget_prefix_no_flag_returns_input_unchanged() {
        let (b, rest) = parse_goal_budget_prefix("ship it");
        assert!(b.is_none());
        assert_eq!(rest, "ship it");
    }

    #[test]
    fn heartbeat_interval_units_parse_and_bad_ones_are_refused() {
        // A misread interval is silent until the heartbeat fires at the wrong
        // cadence, so anything unrecognized must surface usage instead of being
        // coerced to a plausible-looking number.
        assert_eq!(parse_interval_secs("90s"), Some(90));
        assert_eq!(parse_interval_secs("10m"), Some(600));
        assert_eq!(parse_interval_secs("2h"), Some(7200));
        assert_eq!(parse_interval_secs("1d"), Some(86_400));
        assert_eq!(parse_interval_secs("300"), Some(300), "bare = seconds");
        assert_eq!(parse_interval_secs("15M"), Some(900), "case-insensitive");

        assert_eq!(parse_interval_secs(""), None);
        assert_eq!(parse_interval_secs("soon"), None);
        assert_eq!(parse_interval_secs("0"), None);
        assert_eq!(parse_interval_secs("-5m"), None);
        assert_eq!(parse_interval_secs("1.5h"), None);
    }

    #[test]
    fn heartbeat_set_keeps_the_prompt_verbatim() {
        // The prompt is prose the agent acts on; normalizing it would change
        // the instruction the user wrote.
        assert_eq!(
            parse_heartbeat_subcommand("every 15m Check CI for PR #12, then summarize"),
            HeartbeatSubcommand::Set {
                interval_secs: 900,
                prompt: "Check CI for PR #12, then summarize".to_string(),
            }
        );
    }

    #[test]
    fn heartbeat_verbs_and_malformed_input_route_correctly() {
        assert_eq!(parse_heartbeat_subcommand(""), HeartbeatSubcommand::Status);
        assert_eq!(
            parse_heartbeat_subcommand("status"),
            HeartbeatSubcommand::Status
        );
        assert_eq!(
            parse_heartbeat_subcommand("pause"),
            HeartbeatSubcommand::Pause
        );
        assert_eq!(
            parse_heartbeat_subcommand("resume"),
            HeartbeatSubcommand::Resume
        );
        assert_eq!(
            parse_heartbeat_subcommand("clear"),
            HeartbeatSubcommand::Clear
        );

        // An interval with no prompt would set a heartbeat that says nothing.
        assert_eq!(
            parse_heartbeat_subcommand("every 10m"),
            HeartbeatSubcommand::Usage
        );
        // Missing the `every` keyword entirely.
        assert_eq!(
            parse_heartbeat_subcommand("10m check ci"),
            HeartbeatSubcommand::Usage
        );
        assert_eq!(
            parse_heartbeat_subcommand("every soon check ci"),
            HeartbeatSubcommand::Usage
        );
    }

    #[test]
    fn format_session_heartbeat_distinguishes_paused_from_counting_down() {
        // "Why hasn't it fired?" is the question this display answers.
        let base = protocols::admin::SessionHeartbeatDetail {
            present: true,
            session_id: "s".into(),
            prompt: "Check CI".into(),
            interval_secs: 900,
            paused: false,
            seconds_until_due: 120,
            report: String::new(),
        };
        let running = format_session_heartbeat(&base);
        assert!(running.contains("every 15m"), "{running}");
        assert!(running.contains("next fire in 2m"), "{running}");
        assert!(running.contains("Check CI"), "{running}");

        let paused = format_session_heartbeat(&protocols::admin::SessionHeartbeatDetail {
            paused: true,
            ..base
        });
        assert!(paused.contains("paused"), "{paused}");
        assert!(
            !paused.contains("next fire"),
            "a paused heartbeat must not advertise a countdown: {paused}"
        );
    }

    #[test]
    fn heartbeat_interval_reads_back_as_it_was_set() {
        // `/hb every 90s …` read back as "every 1m": the display rounded the
        // interval the user had just typed.
        for (secs, shown) in [
            (60, "1m"),
            (90, "90s"),
            (900, "15m"),
            (5400, "90m"),
            (7200, "2h"),
            (86_400, "1d"),
            (90_000, "25h"),
        ] {
            assert_eq!(format_interval(secs), shown, "{secs}s");
        }
    }

    #[test]
    fn format_session_heartbeat_tells_the_user_how_to_set_one() {
        let none = protocols::admin::SessionHeartbeatDetail::default();
        let out = format_session_heartbeat(&none);
        assert!(out.contains("/heartbeat every"), "{out}");
    }

    #[test]
    fn goal_gate_verbs_are_not_swallowed_as_objectives() {
        // The failure this guards: `/goal gate add cargo test` falling through
        // to the objective fallback and *replacing the goal* with the literal
        // text "gate add cargo test", silently discarding the real objective
        // and its accounting.
        assert_eq!(
            parse_goal_subcommand("gate"),
            GoalSubcommand::Gate(GateSubcommand::List)
        );
        assert_eq!(
            parse_goal_subcommand("gate list"),
            GoalSubcommand::Gate(GateSubcommand::List)
        );
        assert_eq!(
            parse_goal_subcommand("gate clear"),
            GoalSubcommand::Gate(GateSubcommand::Clear)
        );
        assert_eq!(
            parse_goal_subcommand("gate add cargo test -p coding"),
            GoalSubcommand::Gate(GateSubcommand::Add("cargo test -p coding".to_string()))
        );
        assert_eq!(
            parse_goal_subcommand("gate remove 2"),
            GoalSubcommand::Gate(GateSubcommand::Remove(2))
        );
    }

    #[test]
    fn goal_gate_add_keeps_the_command_line_verbatim() {
        // A gate command is a shell line. Splitting on whitespace, stripping
        // quotes, or trimming flags would silently change what gets run.
        let raw = r#"bash -c "cargo test 2>&1 | tail -5""#;
        assert_eq!(
            parse_gate_subcommand(&format!("add {raw}")),
            GateSubcommand::Add(raw.to_string())
        );
    }

    #[test]
    fn goal_gate_rejects_malformed_input_instead_of_guessing() {
        // A bad ordinal is a typo. Coercing it (to 0, or to "add a gate named
        // remove x") would either no-op silently or create a nonsense gate.
        assert_eq!(parse_gate_subcommand("remove"), GateSubcommand::Usage);
        assert_eq!(parse_gate_subcommand("remove x"), GateSubcommand::Usage);
        assert_eq!(parse_gate_subcommand("remove 0"), GateSubcommand::Usage);
        assert_eq!(parse_gate_subcommand("remove -1"), GateSubcommand::Usage);
        assert_eq!(parse_gate_subcommand("add"), GateSubcommand::Usage);
        assert_eq!(parse_gate_subcommand("add    "), GateSubcommand::Usage);
        assert_eq!(parse_gate_subcommand("frobnicate"), GateSubcommand::Usage);
    }

    #[test]
    fn a_goal_objective_may_still_start_with_the_word_gate() {
        // Only the `gate` *verb* is claimed; an objective that happens to begin
        // with "gate" (no recognized sub-verb) must remain an objective.
        assert_eq!(
            parse_goal_subcommand("gateway rewrite for the delivery queue"),
            GoalSubcommand::Set {
                objective: "gateway rewrite for the delivery queue".to_string(),
                budget: None,
            }
        );
    }

    #[test]
    fn format_goal_gates_reports_which_gate_is_red() {
        // "Which one is failing?" is the question this list exists to answer.
        let list = protocols::admin::GoalGateList {
            report: String::new(),
            gates: vec![
                protocols::admin::GoalGate {
                    ordinal: 1,
                    command: "cargo fmt --check".to_string(),
                    attempts: 0,
                    last_exit_code_present: true,
                    last_exit_code: 0,
                },
                protocols::admin::GoalGate {
                    ordinal: 2,
                    command: "cargo test".to_string(),
                    attempts: 2,
                    last_exit_code_present: true,
                    last_exit_code: 101,
                },
                protocols::admin::GoalGate {
                    ordinal: 3,
                    command: "cargo clippy".to_string(),
                    attempts: 0,
                    last_exit_code_present: false,
                    last_exit_code: 0,
                },
            ],
        };
        let out = format_goal_gates(&list);
        assert!(out.contains("1. cargo fmt --check  — passing"), "{out}");
        assert!(
            out.contains("2. cargo test  — FAILING (exit 101, 2 attempt(s))"),
            "{out}"
        );
        assert!(out.contains("3. cargo clippy  — not yet run"), "{out}");
    }

    #[test]
    fn format_goal_gates_tells_the_user_how_to_add_one_when_empty() {
        let list = protocols::admin::GoalGateList {
            report: String::new(),
            gates: Vec::new(),
        };
        assert!(format_goal_gates(&list).contains("/goal gate add"));
    }

    #[test]
    fn parse_goal_subcommand_dispatches_known_keywords() {
        assert_eq!(parse_goal_subcommand(""), GoalSubcommand::Show);
        assert_eq!(parse_goal_subcommand("   "), GoalSubcommand::Show);
        assert_eq!(parse_goal_subcommand("clear"), GoalSubcommand::Clear);
        assert_eq!(parse_goal_subcommand("pause"), GoalSubcommand::Pause);
        assert_eq!(parse_goal_subcommand("resume"), GoalSubcommand::Resume);
    }

    #[test]
    fn parse_goal_subcommand_set_from_objective() {
        let parsed = parse_goal_subcommand("ship it");
        assert_eq!(
            parsed,
            GoalSubcommand::Set {
                objective: "ship it".to_string(),
                budget: None,
            }
        );
    }

    #[test]
    fn parse_goal_subcommand_set_from_edit_prefix() {
        let parsed = parse_goal_subcommand("edit ship it");
        assert_eq!(
            parsed,
            GoalSubcommand::Set {
                objective: "ship it".to_string(),
                budget: None,
            }
        );
    }

    #[test]
    fn parse_goal_subcommand_set_with_budget() {
        let parsed = parse_goal_subcommand("--budget 5000 ship it");
        assert_eq!(
            parsed,
            GoalSubcommand::Set {
                objective: "ship it".to_string(),
                budget: Some(5000),
            }
        );
    }

    #[test]
    fn parse_goal_subcommand_bare_edit_does_not_set_literal() {
        // Regression: `/goal edit` (no trailing text) used to silently set the
        // session goal to the literal text "edit". The parser must surface a
        // distinct BareEdit so the dispatcher can report usage instead.
        assert_eq!(parse_goal_subcommand("edit"), GoalSubcommand::BareEdit);
    }

    #[test]
    fn parse_goal_subcommand_empty_after_edit_prefix() {
        // `/goal edit ` (trailing whitespace, no objective) collapses to the
        // same intent as bare `/goal edit` — the user wanted to type an
        // objective and didn't. Both must error out instead of setting the
        // goal text to "" or to the literal "edit".
        assert_eq!(parse_goal_subcommand("edit "), GoalSubcommand::BareEdit);
        assert_eq!(parse_goal_subcommand("edit   "), GoalSubcommand::BareEdit);
    }

    #[test]
    fn parse_goal_subcommand_budget_flag_without_value_is_usage() {
        // `/goal --budget` alone is operator confusion, not a goal whose
        // objective is the literal "--budget".
        assert_eq!(parse_goal_subcommand("--budget"), GoalSubcommand::Usage);
    }

    #[test]
    fn parse_goal_subcommand_budget_value_without_objective_is_usage() {
        // Regression: `/goal --budget 5000` (numeric budget, no objective)
        // used to silently fall through with `budget=None` and set the goal
        // objective to the literal "--budget 5000". The same bug shape as
        // bare `/goal edit`. Both bare and trailing-whitespace variants must
        // route to Usage so the operator gets told what's missing.
        assert_eq!(
            parse_goal_subcommand("--budget 5000"),
            GoalSubcommand::Usage
        );
        assert_eq!(
            parse_goal_subcommand("--budget 5000 "),
            GoalSubcommand::Usage
        );
        assert_eq!(
            parse_goal_subcommand("--budget 5000   "),
            GoalSubcommand::Usage
        );
    }

    #[test]
    fn parse_goal_subcommand_non_numeric_budget_is_usage() {
        // `--budget abc` is an operator typo, not "objective starts with the
        // literal text --budget". Surface a usage error rather than swallowing
        // the malformed flag value into the objective.
        assert_eq!(
            parse_goal_subcommand("--budget abc ship it"),
            GoalSubcommand::Usage
        );
        assert_eq!(parse_goal_subcommand("--budget abc"), GoalSubcommand::Usage);
    }

    #[test]
    fn parse_goal_subcommand_budget_subcommand_sets_numeric() {
        // `/goal budget N` is the in-place budget change — distinct from the
        // `--budget N <text>` prefix, it carries no objective.
        assert_eq!(
            parse_goal_subcommand("budget 5000"),
            GoalSubcommand::Budget { budget: Some(5000) }
        );
        assert_eq!(
            parse_goal_subcommand("budget   250  "),
            GoalSubcommand::Budget { budget: Some(250) }
        );
    }

    #[test]
    fn parse_goal_subcommand_budget_none_clears() {
        // `budget none` / `budget unbounded` clear the budget (case-insensitive).
        for s in [
            "budget none",
            "budget NONE",
            "budget unbounded",
            "budget Unbounded",
        ] {
            assert_eq!(
                parse_goal_subcommand(s),
                GoalSubcommand::Budget { budget: None },
                "input {s:?} must clear the budget"
            );
        }
    }

    #[test]
    fn parse_goal_subcommand_budget_negative_is_carried_for_validation() {
        // A negative value parses as an integer here; the dispatcher rejects it
        // via `validate_budget` so the operator sees "must be positive" rather
        // than the parser silently dropping it.
        assert_eq!(
            parse_goal_subcommand("budget -5"),
            GoalSubcommand::Budget { budget: Some(-5) }
        );
    }

    #[test]
    fn parse_goal_subcommand_budget_numeric_with_trailing_junk_is_usage() {
        // Regression for the footgun: a numeric budget value followed by
        // trailing text must NOT silently fall through to an objective
        // *replace* (which resets accounting). The operator clearly meant a
        // budget change, so route to Usage.
        assert_eq!(
            parse_goal_subcommand("budget 5000 tokens"),
            GoalSubcommand::Usage
        );
        // Malformed number (thousands separator, stray suffix) — same intent,
        // same protection.
        assert_eq!(parse_goal_subcommand("budget 5,000"), GoalSubcommand::Usage);
        assert_eq!(parse_goal_subcommand("budget 5000x"), GoalSubcommand::Usage);
        // A clear-keyword with trailing junk is likewise malformed.
        assert_eq!(
            parse_goal_subcommand("budget none now"),
            GoalSubcommand::Usage
        );
    }

    #[test]
    fn parse_goal_subcommand_budget_word_is_an_objective_not_a_subcommand() {
        // Only a numeric or none/unbounded value triggers the budget
        // subcommand. A goal that legitimately begins with "budget" must not be
        // hijacked — `/goal budget report for Q3` is an objective.
        assert_eq!(
            parse_goal_subcommand("budget report for Q3"),
            GoalSubcommand::Set {
                objective: "budget report for Q3".to_string(),
                budget: None,
            }
        );
        // Bare `budget` (no value) is likewise just an objective, not a
        // malformed subcommand.
        assert_eq!(
            parse_goal_subcommand("budget"),
            GoalSubcommand::Set {
                objective: "budget".to_string(),
                budget: None,
            }
        );
    }

    #[test]
    fn parse_goal_budget_prefix_value_without_rest_returns_empty_rest() {
        // `--budget N` with no trailing objective must surface the parsed
        // budget so the caller can branch into Usage. Returning
        // `(None, "--budget N")` was the original bug.
        let (b, rest) = parse_goal_budget_prefix("--budget 5000");
        assert_eq!(b, Some(5000));
        assert_eq!(rest, "");
        let (b, rest) = parse_goal_budget_prefix("--budget 5000 ");
        assert_eq!(b, Some(5000));
        assert_eq!(rest, "");
    }

    #[test]
    fn parse_goal_budget_prefix_rejects_non_numeric_budget() {
        // `--budget foo ship it` — the number is unparseable, so the whole
        // string is returned untouched (the agent will then fail validation).
        let (b, rest) = parse_goal_budget_prefix("--budget abc ship it");
        assert!(b.is_none());
        assert_eq!(rest, "--budget abc ship it");
    }

    #[test]
    fn format_elapsed_seconds_buckets_correctly() {
        assert_eq!(format_elapsed_seconds(45), "45s");
        assert_eq!(format_elapsed_seconds(125), "2m 5s");
        assert_eq!(format_elapsed_seconds(3 * 3600 + 7 * 60), "3h 7m");
        // Clamp negative input to 0.
        assert_eq!(format_elapsed_seconds(-5), "0s");
    }

    #[test]
    fn format_goal_detail_lists_status_and_objective_and_hint() {
        let detail = protocols::admin::GoalDetail {
            present: true,
            session_id: "s".into(),
            goal_id: "g1".into(),
            objective: "ship it".into(),
            status: "active".into(),
            token_budget_present: true,
            token_budget: 5000,
            tokens_used: 1200,
            time_used_seconds: 125,
            created_at: 0,
            updated_at: 0,
        };
        let s = format_goal_detail(&detail);
        assert!(s.contains("Status:    active"));
        assert!(s.contains("Objective: ship it"));
        assert!(s.contains("2m 5s"));
        assert!(s.contains("1200 / 5000"));
        assert!(s.contains("/goal pause"));
    }

    #[test]
    fn format_goal_detail_offers_resume_when_usage_limited() {
        // When the provider cut us off (UsageLimited), the summary card must
        // point the operator at `/goal resume` once the provider is happy
        // again. Falling back to the BudgetLimited/Complete arm (clear-only)
        // would strand the goal.
        let detail = protocols::admin::GoalDetail {
            present: true,
            session_id: "s".into(),
            goal_id: "g1".into(),
            objective: "ship it".into(),
            status: "usage_limited".into(),
            token_budget_present: false,
            token_budget: 0,
            tokens_used: 0,
            time_used_seconds: 0,
            created_at: 0,
            updated_at: 0,
        };
        let s = format_goal_detail(&detail);
        assert!(
            s.contains("usage limited"),
            "label must use the spaced human form: {s}"
        );
        assert!(
            s.contains("/goal resume"),
            "usage-limited goals are resumable, hint must include /goal resume: {s}"
        );
        assert!(
            s.contains("/goal clear"),
            "usage-limited goals are also clearable: {s}"
        );
    }

    #[test]
    fn format_goal_detail_drops_pause_hint_when_complete() {
        let detail = protocols::admin::GoalDetail {
            present: true,
            session_id: "s".into(),
            goal_id: "g1".into(),
            objective: "done".into(),
            status: "complete".into(),
            token_budget_present: false,
            token_budget: 0,
            tokens_used: 0,
            time_used_seconds: 0,
            created_at: 0,
            updated_at: 0,
        };
        let s = format_goal_detail(&detail);
        assert!(!s.contains("pause"));
        assert!(!s.contains("resume"));
        assert!(s.contains("/goal clear"));
        assert!(s.contains("unbounded"));
    }

    // Source-grep guards for `/heal`, `/dream`, `/status`, `/stats` removed:
    // these slash commands are reachable via `app::try_handle_command(...)`,
    // which is exercised by the runtime-behavior tests in `app/mod.rs`
    // (`/stats` → status popup) and by the `/heal` / `/dream` formatter tests
    // below. The grep tests only verified that a string literal appeared in
    // a file — they would pass even if the handler was wired to the wrong
    // action — so they don't add a real failure mode.

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
            messages_purged: 17,
            audit_events_tombstoned: 19,
            wedged_runs_recovered: 23,
            backups_pruned: 0,
            auto_backup_path: String::new(),
            skipped: false,
        });
        assert!(out.contains("1234 bytes truncated"), "{out:?}");
        assert!(out.contains("recovered 23 wedged runs"), "{out:?}");
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

    /// Spec: the reload summary renders the diff in a way that lets the
    /// operator answer "did my MCP catalog change?" at a glance. Counts
    /// alone are not enough — server *names* must appear, since "+1
    /// server" without a name leaves the operator wondering whether the
    /// one that landed is the one they just added. Regression guard for
    /// the formatter shape: a refactor that drops names from the line
    /// would technically still be valid prose but would silently degrade
    /// the operator-facing UX.
    #[test]
    fn format_reload_response_shows_mcp_diff_with_names_and_tool_count() {
        let resp = protocols::admin::ReloadResponse {
            mcp_added: vec!["new-server".into()],
            mcp_removed: vec!["old-server".into()],
            mcp_unchanged: 2,
            mcp_failures: vec![protocols::admin::McpReloadFailure {
                server: "broken".into(),
                error: "connect timeout".into(),
            }],
            mcp_total_tools: 14,
            skills_total: 0,
            skills_user: 0,
            skills_user_unavailable: 0,
            skills_disabled: 0,
        };
        let out = format_reload_response(2 /* TARGET_MCP */, &resp);
        assert!(out.contains("+1 (new-server)"), "added diff: {out:?}");
        assert!(out.contains("-1 (old-server)"), "removed diff: {out:?}");
        assert!(out.contains("2 unchanged"), "unchanged count: {out:?}");
        assert!(out.contains("1 failed"), "failure count: {out:?}");
        assert!(out.contains("14 tool"), "tool count: {out:?}");
        assert!(
            out.contains("broken: connect timeout"),
            "per-failure detail: {out:?}"
        );
        assert!(
            !out.contains("Skills reloaded"),
            "MCP-only target must not include skills line: {out:?}"
        );
    }

    /// Spec: `/reload skills` must report on what the operator actually
    /// cares about — total count and how many of their user skills are
    /// unavailable due to missing deps. Hiding the unavailable count
    /// would be a silent-failure UX bug: the operator drops in a SKILL.md
    /// requiring `gh`, sees "reload succeeded", and never knows it isn't
    /// actually usable until the LLM fails to invoke it.
    #[test]
    fn format_reload_response_surfaces_skills_unavailable_count() {
        let resp = protocols::admin::ReloadResponse {
            mcp_added: vec![],
            mcp_removed: vec![],
            mcp_unchanged: 0,
            mcp_failures: vec![],
            mcp_total_tools: 0,
            skills_total: 25,
            skills_user: 3,
            skills_user_unavailable: 1,
            skills_disabled: 2,
        };
        let out = format_reload_response(3 /* TARGET_SKILLS */, &resp);
        assert!(out.contains("25 total"), "total count: {out:?}");
        assert!(out.contains("3 user"), "user count: {out:?}");
        assert!(
            out.contains("1 missing deps"),
            "missing-deps count must surface: {out:?}"
        );
        assert!(out.contains("2 disabled"), "disabled count: {out:?}");
        assert!(
            !out.contains("MCP reloaded"),
            "skills-only target must not include MCP line: {out:?}"
        );
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
            HistoryCell::Separator { label: None },
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

    // ── /voice precheck ────────────────────────────────────────────────

    /// Spec: with `voice.enabled = false`, `/voice on` must surface the
    /// "voice mode is disabled" hint and exit early — regardless of
    /// whether an STT provider is configured or a loop is already
    /// running. This is the user-facing contract for the feature flag.
    #[test]
    fn voice_start_precheck_refuses_when_voice_disabled() {
        let mut config = cortex::config::Config::default();
        config.voice.enabled = false;
        config.audio.enabled = true; // even with STT ready
        let msg = voice_start_precheck(&config, false).expect("must refuse");
        assert!(
            msg.contains("voice mode is disabled"),
            "expected disabled hint: {msg}"
        );
        assert!(
            msg.contains("voice.enabled"),
            "expected mention of the setting key: {msg}"
        );
    }

    /// Spec: `/voice on` with the loop already running must say so
    /// (rather than silently spawning a duplicate or claiming the STT
    /// provider is missing). Disabled flag has priority; this guard runs
    /// next.
    #[test]
    fn voice_start_precheck_refuses_when_already_running() {
        let mut config = cortex::config::Config::default();
        config.voice.enabled = true;
        let msg = voice_start_precheck(&config, true).expect("must refuse");
        assert!(
            msg.contains("already running"),
            "expected already-running message: {msg}"
        );
    }

    /// Spec: voice mode is unusable without an STT provider; the
    /// precheck must surface that explicitly so the operator knows where
    /// to look. A bare config (no `audio.models`) is the realistic
    /// not-yet-set-up shape.
    #[test]
    fn voice_start_precheck_refuses_when_no_stt_provider() {
        let mut config = cortex::config::Config::default();
        config.voice.enabled = true;
        // audio.enabled defaults to false → AudioTranscriber::from_config
        // returns None → precheck must flag the missing provider.
        let msg = voice_start_precheck(&config, false).expect("must refuse");
        assert!(
            msg.contains("no audio transcription provider"),
            "expected STT-missing hint: {msg}"
        );
        assert!(
            msg.contains("audio.enabled") && msg.contains("audio.models"),
            "expected actionable setting names: {msg}"
        );
    }

    /// Spec: when voice is enabled, no loop is running, and at least one
    /// STT provider builds successfully, the precheck must return `None`
    /// so the caller proceeds to open the mic. This pins the happy path.
    #[test]
    fn voice_start_precheck_passes_with_voice_and_stt_ready() {
        // Unique env var so this test doesn't race the cortex per-arm
        // tests on the standard names.
        let var = "BORG_TUI_VOICE_PRECHECK_OPENAI_KEY";
        unsafe { std::env::set_var(var, "test-key") };

        let mut config = cortex::config::Config::default();
        config.voice.enabled = true;
        config.audio.enabled = true;
        config.audio.models = vec![cortex::config::AudioModelConfig {
            provider: "openai".to_string(),
            model: None,
            api_key_env: Some(var.to_string()),
            language: None,
            timeout_ms: None,
            base_url: None,
            xai_stt_format: None,
            xai_stt_diarize: None,
            command: None,
        }];

        assert!(
            voice_start_precheck(&config, false).is_none(),
            "precheck should pass with voice + STT ready"
        );

        unsafe { std::env::remove_var(var) };
    }

    #[test]
    fn format_context_breakdown_renders_all_components_and_free_space() {
        use protocols::session::{ContextBreakdown, ContextComponent};
        let breakdown = ContextBreakdown {
            context_window: 100_000,
            components: vec![
                ContextComponent {
                    label: "System prompt (cache-stable prefix)".into(),
                    tokens: 10_000,
                    detail: false,
                },
                ContextComponent {
                    label: "Conversation history".into(),
                    tokens: 40_000,
                    detail: false,
                },
            ],
        };
        let out = format_context_breakdown(&breakdown);
        assert!(
            out.contains("~50000 / 100000 tokens (50% used)"),
            "header must total the components: {out}"
        );
        assert!(out.contains("System prompt (cache-stable prefix)"));
        assert!(out.contains("Conversation history"));
        // Free space is derived (window − used), never trusted from the wire.
        assert!(
            out.contains("Free space") && out.contains("~   50000 (50.0%)"),
            "free-space row must be derived from the same numbers: {out}"
        );
        // A 40% component paints 40% of the 24-cell bar.
        let history_line = out
            .lines()
            .find(|l| l.contains("Conversation history"))
            .unwrap();
        let filled = history_line.matches('█').count();
        assert_eq!(filled, 10, "40% of 24 cells rounds to 10: {history_line}");
    }

    /// Detail rows (per-skill / per-MCP-server) itemize their parent bucket:
    /// they must render indented under it, never count toward the header
    /// total or free space, sort largest-first, and collapse past the cap
    /// into a "+N more" line.
    #[test]
    fn format_context_breakdown_indents_caps_and_never_double_counts_details() {
        use protocols::session::{ContextBreakdown, ContextComponent};
        let top = |label: &str, tokens| ContextComponent {
            label: label.into(),
            tokens,
            detail: false,
        };
        let child = |label: &str, tokens| ContextComponent {
            label: label.into(),
            tokens,
            detail: true,
        };
        let mut components = vec![
            top("System prompt (dynamic suffix)", 30_000),
            // 12 skill children (each 100 tokens): two must collapse.
            // Emitted smallest-variant-first to prove the renderer re-sorts.
            child("Skill: aa", 100),
        ];
        components.push(child("Skill: big", 5_000));
        for i in 0..10 {
            components.push(child(&format!("Skill: s{i:02}"), 100));
        }
        components.push(top("Tool schemas (MCP)", 6_000));
        components.push(child("MCP: linear", 1_000));
        components.push(child("MCP: github", 5_000));
        let breakdown = ContextBreakdown {
            context_window: 100_000,
            components,
        };

        let out = format_context_breakdown(&breakdown);

        // Header totals only the parent buckets: 30k + 6k, not +details.
        assert!(
            out.contains("~36000 / 100000 tokens (36% used)"),
            "detail rows must not inflate the total: {out}"
        );
        // Free space derives from the same non-detail sum.
        assert!(
            out.contains("Free space") && out.contains("~   64000"),
            "free space must ignore detail rows: {out}"
        );
        // Children render indented, largest first, under their own parent.
        let lines: Vec<&str> = out.lines().collect();
        let dyn_line = lines
            .iter()
            .position(|l| l.contains("System prompt (dynamic suffix)"))
            .unwrap();
        assert!(
            lines[dyn_line + 1].starts_with("    ↳ Skill: big"),
            "largest skill must lead its parent's details: {out}"
        );
        let mcp_line = lines
            .iter()
            .position(|l| l.contains("Tool schemas (MCP)"))
            .unwrap();
        assert!(
            lines[mcp_line + 1].starts_with("    ↳ MCP: github")
                && lines[mcp_line + 2].starts_with("    ↳ MCP: linear"),
            "server details must sort descending under the MCP bucket: {out}"
        );
        // 12 skill children against a cap of 10 → exactly 2 collapse.
        assert!(
            out.contains("↳ +2 more"),
            "children past the cap must collapse into +N more: {out}"
        );
        let skill_rows = out.matches("↳ Skill: ").count();
        assert_eq!(skill_rows, 10, "cap must hold at 10 rendered rows: {out}");
    }

    #[test]
    fn format_context_breakdown_overfull_window_clamps_free_space() {
        use protocols::session::{ContextBreakdown, ContextComponent};
        // Estimates can overshoot a genuinely full window; free space must
        // clamp to zero instead of underflowing.
        let breakdown = ContextBreakdown {
            context_window: 1_000,
            components: vec![ContextComponent {
                label: "Conversation history".into(),
                tokens: 1_500,
                detail: false,
            }],
        };
        let out = format_context_breakdown(&breakdown);
        assert!(
            out.contains("~       0") || out.contains("~0 "),
            "free space must clamp at zero: {out}"
        );
    }

    /// Order spec: when voice is disabled AND a loop is somehow already
    /// flagged running, the disabled message wins. Telling a user "already
    /// running" while the feature is off would be misleading; the
    /// actionable fix is to flip the feature flag.
    #[test]
    fn voice_start_precheck_disabled_beats_already_running() {
        let mut config = cortex::config::Config::default();
        config.voice.enabled = false;
        let msg = voice_start_precheck(&config, true).expect("must refuse");
        assert!(
            msg.contains("voice mode is disabled"),
            "disabled message must win over already-running: got {msg}"
        );
    }
}
