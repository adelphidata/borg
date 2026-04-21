use anyhow::Result;
use async_trait::async_trait;
use borg_core::config::Config;
use borg_core::db::Database;
use tracing::warn;

/// Metadata for a single gateway slash command.
pub struct CommandDef {
    pub name: &'static str,
    pub description: &'static str,
    pub accepts_args: bool,
    /// If true, the command is not intercepted — it passes through to the agent.
    pub pass_through: bool,
}

/// All gateway slash commands. Used for text parsing, /help output, and native
/// platform menu registration (Telegram setMyCommands, Discord global commands).
pub const GATEWAY_COMMANDS: &[CommandDef] = &[
    CommandDef {
        name: "start",
        description: "Start a conversation with Borg",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "help",
        description: "Show help and available commands",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "commands",
        description: "List all slash commands",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "status",
        description: "Show current session info",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "new",
        description: "Start a new session",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "reset",
        description: "Clear current session messages",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "usage",
        description: "Show token usage and budget",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "skill",
        description: "Run a skill by name",
        accepts_args: true,
        pass_through: true,
    },
    CommandDef {
        name: "skills",
        description: "List available skills",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "whoami",
        description: "Show your sender identity",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "memory",
        description: "Show memory files",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "doctor",
        description: "Run system diagnostics",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "pairing",
        description: "Show pairing status",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "cancel",
        description: "Stop the current in-progress turn",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "mode",
        description: "Show or switch collaboration mode (default/execute/plan)",
        accepts_args: true,
        pass_through: false,
    },
    CommandDef {
        name: "plan",
        description: "Enter plan mode, optionally with a message",
        accepts_args: true,
        pass_through: false,
    },
    CommandDef {
        name: "compact",
        description: "Trim session history, keeping last N messages",
        accepts_args: true,
        pass_through: false,
    },
    CommandDef {
        name: "undo",
        description: "Undo the last agent turn",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "history",
        description: "Show recent conversation messages",
        accepts_args: true,
        pass_through: false,
    },
    CommandDef {
        name: "schedule",
        description: "List scheduled tasks",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "settings",
        description: "Show or change a setting",
        accepts_args: true,
        pass_through: false,
    },
    CommandDef {
        name: "poke",
        description: "Trigger an immediate heartbeat",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "evolution",
        description: "Show evolution stage, archetype, readiness",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "xp",
        description: "Show XP totals, sources, and recent feed",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "card",
        description: "Show your shareable ASCII evolution card",
        accepts_args: true,
        pass_through: false,
    },
];

/// Trait for platforms that support registering slash commands in native menus.
#[async_trait]
pub trait NativeCommandRegistration {
    async fn register_commands(&self, commands: &[CommandDef]) -> Result<()>;
}

enum Command {
    Start,
    Help,
    Commands,
    Status,
    New,
    Reset,
    Usage,
    Skill,
    Skills,
    WhoAmI,
    Memory,
    Doctor,
    Pairing,
    Cancel,
    Mode,
    Plan,
    Compact,
    Undo,
    History,
    Schedule,
    Settings,
    Poke,
    Evolution,
    Xp,
    Card,
}

impl Command {
    /// Parse a slash command from message text. Strips `@botname` suffix
    /// (Telegram group format: `/command@BotName`). Returns the command and
    /// any trailing arguments.
    fn parse(text: &str) -> Option<(Self, &str)> {
        let first_word = text.split_whitespace().next()?;
        let lowered = first_word.to_ascii_lowercase();
        // Strip @botname suffix
        let cmd_str = lowered.split('@').next().unwrap_or(&lowered);
        let args = text[first_word.len()..].trim_start();
        let cmd = match cmd_str {
            "/start" => Self::Start,
            "/help" => Self::Help,
            "/commands" => Self::Commands,
            "/status" => Self::Status,
            "/new" => Self::New,
            "/reset" => Self::Reset,
            "/usage" => Self::Usage,
            "/skill" => Self::Skill,
            "/skills" => Self::Skills,
            "/whoami" => Self::WhoAmI,
            "/memory" => Self::Memory,
            "/doctor" => Self::Doctor,
            "/pairing" => Self::Pairing,
            "/cancel" | "/stop" | "/abort" => Self::Cancel,
            "/mode" => Self::Mode,
            "/plan" => Self::Plan,
            "/compact" => Self::Compact,
            "/undo" => Self::Undo,
            "/history" => Self::History,
            "/schedule" => Self::Schedule,
            "/settings" => Self::Settings,
            "/poke" => Self::Poke,
            "/evolution" => Self::Evolution,
            "/xp" => Self::Xp,
            "/card" => Self::Card,
            _ => return None,
        };
        Some((cmd, args))
    }
}

/// Returns `true` if `text` parses as the `/cancel` slash command. Handled
/// separately from [`try_handle_command`] because cancellation requires an
/// async registry call, and the main dispatcher is intentionally synchronous
/// (it holds a `&Database` which is `!Send`).
pub fn is_cancel_command(text: &str) -> bool {
    matches!(Command::parse(text), Some((Command::Cancel, _)))
}

/// Returns `true` if `text` parses as the `/poke` slash command. Handled
/// separately from [`try_handle_command`] because poke requires an async
/// HTTP call to the daemon.
pub fn is_poke_command(text: &str) -> bool {
    matches!(Command::parse(text), Some((Command::Poke, _)))
}

/// Try to handle a slash command. Returns `Some(response)` if the message was
/// a recognised command, `None` to fall through to the agent.
///
/// Note: `/cancel` is **not** handled here — it is intercepted in
/// `handler::invoke_agent_with_auto_reply` via [`is_cancel_command`] so the
/// async cancellation path can run without holding a DB reference.
pub fn try_handle_command(
    text: &str,
    db: &Database,
    config: &Config,
    channel_name: &str,
    session_key: &str,
    session_id: &str,
    sender_id: &str,
) -> Option<String> {
    let (cmd, args) = Command::parse(text)?;
    let response = match cmd {
        Command::Start => handle_help(), // welcome approved users with help; unapproved users see pairing challenge before reaching here
        Command::Help => handle_help(),
        Command::Commands => handle_commands(),
        Command::Status => handle_status(db, session_id),
        Command::New => handle_new(db, channel_name, session_key),
        Command::Reset => handle_reset(db, session_id),
        Command::Usage => handle_usage(db, config, session_id),
        Command::Skill => return None, // Pass through to agent
        Command::Skills => handle_skills(config),
        Command::WhoAmI => handle_whoami(sender_id, channel_name, db),
        Command::Memory => handle_memory(),
        Command::Doctor => handle_doctor(config),
        Command::Pairing => handle_pairing(db, channel_name, sender_id),
        // Handled out-of-band in the caller; see `is_cancel_command`.
        Command::Cancel => return None,
        Command::Mode => handle_mode(db, config, session_id, args),
        Command::Plan => {
            return handle_plan(db, session_id, args);
        }
        Command::Compact => handle_compact(db, session_id, args),
        Command::Undo => handle_undo(db, session_id),
        Command::History => handle_history(db, session_id, args),
        Command::Schedule => handle_schedule(db),
        Command::Settings => handle_settings(db, config, args),
        // Handled out-of-band in the caller; see `is_poke_command`.
        Command::Poke => return None,
        Command::Evolution => handle_evolution(db),
        Command::Xp => handle_xp(db),
        Command::Card => handle_card(db, args),
    };
    Some(response)
}

fn handle_help() -> String {
    let mut lines = vec!["Available commands:".to_string()];
    for cmd in GATEWAY_COMMANDS {
        let prefix = if cmd.accepts_args {
            format!("  /{} <args>", cmd.name)
        } else {
            format!("  /{}", cmd.name)
        };
        lines.push(format!("{prefix}  — {}", cmd.description));
    }
    lines.join("\n")
}

fn handle_commands() -> String {
    let mut lines = vec!["Slash commands:".to_string()];
    for cmd in GATEWAY_COMMANDS {
        if cmd.pass_through {
            lines.push(format!(
                "  /{}  — {} (sent to agent)",
                cmd.name, cmd.description
            ));
        } else {
            lines.push(format!("  /{}  — {}", cmd.name, cmd.description));
        }
    }
    lines.join("\n")
}

fn handle_status(db: &Database, session_id: &str) -> String {
    let msg_count = db.count_session_messages(session_id).unwrap_or(0);
    let short_id = session_id.get(..8).unwrap_or(session_id);
    format!("Session: {short_id}...\nMessages: {msg_count}")
}

fn handle_new(db: &Database, channel_name: &str, session_key: &str) -> String {
    let new_id = uuid::Uuid::new_v4().to_string();
    match db.update_channel_session_id(channel_name, session_key, &new_id) {
        Ok(true) => {
            let short_id = &new_id[..8];
            format!("New session started ({short_id}...)")
        }
        Ok(false) => "No existing session found to replace.".to_string(),
        Err(e) => {
            warn!("Failed to create new session: {e}");
            "Failed to create new session.".to_string()
        }
    }
}

fn handle_reset(db: &Database, session_id: &str) -> String {
    match db.delete_session_messages(session_id) {
        Ok(count) => format!("Cleared {count} messages from current session."),
        Err(e) => {
            warn!("Failed to reset session: {e}");
            "Failed to reset session.".to_string()
        }
    }
}

fn handle_usage(db: &Database, config: &Config, session_id: &str) -> String {
    let msg_count = db.count_session_messages(session_id).unwrap_or(0);
    let monthly_tokens = db.monthly_token_total().unwrap_or(0);

    let mut lines = vec![
        format!("Session messages: {msg_count}"),
        format!("Monthly tokens: {monthly_tokens}"),
    ];

    if config.budget.monthly_token_limit > 0 {
        let pct = (monthly_tokens as f64 / config.budget.monthly_token_limit as f64) * 100.0;
        lines.push(format!(
            "Budget: {:.1}% of {} used",
            pct, config.budget.monthly_token_limit
        ));
    }

    lines.join("\n")
}

fn handle_skills(config: &Config) -> String {
    let resolved_creds = config.resolve_credentials();
    match borg_core::skills::load_all_skills(&resolved_creds, &config.skills) {
        Ok(skills) if skills.is_empty() => "No skills installed.".to_string(),
        Ok(skills) => {
            let mut lines = vec![format!("Skills ({}):", skills.len())];
            for skill in &skills {
                lines.push(format!("  {}", skill.summary_line()));
            }
            lines.join("\n")
        }
        Err(e) => {
            warn!("Failed to load skills: {e}");
            "Failed to list skills.".to_string()
        }
    }
}

fn handle_whoami(sender_id: &str, channel_name: &str, db: &Database) -> String {
    let approved = db
        .is_sender_approved(channel_name, sender_id)
        .unwrap_or(false);
    let status = if approved { "Approved" } else { "Not approved" };
    format!("Sender: {sender_id}\nChannel: {channel_name}\nPairing: {status}")
}

fn handle_memory() -> String {
    match borg_core::memory::list_memory_files() {
        Ok(files) if files.is_empty() => "No memory files.".to_string(),
        Ok(files) => {
            let mut lines = vec![format!("Memory files ({}):", files.len())];
            for f in &files {
                let size_kb = f.size_bytes as f64 / 1024.0;
                lines.push(format!("  {}  ({:.1} KB)", f.filename, size_kb));
            }
            lines.join("\n")
        }
        Err(e) => {
            warn!("Failed to list memory files: {e}");
            "Failed to list memory files.".to_string()
        }
    }
}

fn handle_doctor(config: &Config) -> String {
    let report = borg_core::doctor::run_diagnostics(config);
    let (pass, warnings, fail) = report.counts();
    let mut lines = vec![format!(
        "Diagnostics: {pass} passed, {warnings} warning(s), {fail} failed"
    )];
    for check in &report.checks {
        match &check.status {
            borg_core::doctor::CheckStatus::Warn(msg) => {
                lines.push(format!("  ⚠ {} — {msg}", check.name));
            }
            borg_core::doctor::CheckStatus::Fail(msg) => {
                lines.push(format!("  ✗ {} — {msg}", check.name));
            }
            _ => {}
        }
    }
    if warnings == 0 && fail == 0 {
        lines.push("  All checks passed.".to_string());
    }
    lines.join("\n")
}

fn handle_mode(db: &Database, config: &Config, session_id: &str, args: &str) -> String {
    let mode_key = format!("gw:mode:{session_id}");
    if args.is_empty() {
        let current = db
            .get_setting(&mode_key)
            .ok()
            .flatten()
            .unwrap_or_else(|| config.conversation.collaboration_mode.to_string());
        return format!("Collaboration mode: {current}\nUsage: /mode <default|execute|plan>");
    }
    let mode = args.trim().to_ascii_lowercase();
    match mode.as_str() {
        "default" | "execute" | "plan" => {
            if let Err(e) = db.set_setting(&mode_key, &mode) {
                warn!("Failed to set mode: {e}");
                return "Failed to set mode.".to_string();
            }
            format!("Switched to {mode} mode.")
        }
        _ => "Invalid mode. Choose: default, execute, plan".to_string(),
    }
}

/// Handle `/plan [message]`. Returns `Some(response)` for bare `/plan`,
/// `None` for `/plan <message>` (pass-through to agent after setting mode).
fn handle_plan(db: &Database, session_id: &str, args: &str) -> Option<String> {
    let mode_key = format!("gw:mode:{session_id}");
    if let Err(e) = db.set_setting(&mode_key, "plan") {
        warn!("Failed to set plan mode: {e}");
        return Some("Failed to enter plan mode.".to_string());
    }
    if args.is_empty() {
        Some("Entered plan mode. Send your message to begin planning.".to_string())
    } else {
        // Pass through to agent — the mode override in handler.rs will
        // pick up the setting we just wrote.
        None
    }
}

fn handle_compact(db: &Database, session_id: &str, args: &str) -> String {
    let keep: usize = if args.is_empty() {
        20
    } else {
        match args.trim().parse() {
            Ok(n) if n > 0 => n,
            _ => return "Usage: /compact [keep_count] (positive integer, default 20)".to_string(),
        }
    };
    match db.compact_session_messages(session_id, keep) {
        Ok(0) => format!("Nothing to compact (≤{keep} messages)."),
        Ok(deleted) => format!("Compacted: removed {deleted} older messages, kept last {keep}."),
        Err(e) => {
            warn!("Failed to compact session: {e}");
            "Failed to compact session.".to_string()
        }
    }
}

fn handle_undo(db: &Database, session_id: &str) -> String {
    match db.delete_last_assistant_turn(session_id) {
        Ok(0) => "Nothing to undo.".to_string(),
        Ok(n) => format!("Undone: removed {n} messages from last agent turn."),
        Err(e) => {
            warn!("Failed to undo: {e}");
            "Failed to undo last turn.".to_string()
        }
    }
}

fn handle_history(db: &Database, session_id: &str, args: &str) -> String {
    let count: usize = if args.is_empty() {
        10
    } else {
        match args.trim().parse() {
            Ok(n) if n > 0 && n <= 50 => n,
            _ => return "Usage: /history [count] (1-50, default 10)".to_string(),
        }
    };
    match db.load_session_messages(session_id) {
        Ok(msgs) if msgs.is_empty() => "No messages in current session.".to_string(),
        Ok(msgs) => {
            let start = msgs.len().saturating_sub(count);
            let mut lines = vec![format!("Last {} messages:", msgs.len().min(count))];
            for msg in &msgs[start..] {
                let content = msg.content.as_deref().unwrap_or("[tool call]");
                let preview = if content.len() > 200 {
                    // Find a safe UTF-8 char boundary to avoid panicking on
                    // multi-byte characters (CJK, emoji, etc.).
                    let end = (0..=200)
                        .rev()
                        .find(|&i| content.is_char_boundary(i))
                        .unwrap_or(0);
                    format!("{}…", &content[..end])
                } else {
                    content.to_string()
                };
                lines.push(format!("  [{}] {}", msg.role, preview));
            }
            lines.join("\n")
        }
        Err(e) => {
            warn!("Failed to load history: {e}");
            "Failed to load history.".to_string()
        }
    }
}

fn handle_schedule(db: &Database) -> String {
    match db.list_tasks() {
        Ok(tasks) if tasks.is_empty() => "No scheduled tasks.".to_string(),
        Ok(tasks) => {
            let mut lines = vec![format!("Scheduled tasks ({}):", tasks.len())];
            for t in &tasks {
                let next = t
                    .next_run
                    .map(|ts| {
                        chrono::DateTime::from_timestamp(ts, 0)
                            .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
                            .unwrap_or_else(|| "?".to_string())
                    })
                    .unwrap_or_else(|| "—".to_string());
                lines.push(format!(
                    "  {} ({}) [{}] next: {}",
                    t.name, t.schedule_expr, t.status, next
                ));
            }
            lines.join("\n")
        }
        Err(e) => {
            warn!("Failed to list tasks: {e}");
            "Failed to list tasks.".to_string()
        }
    }
}

/// Keys that are safe to change from a messaging channel. Security-critical
/// settings (sandbox, security, budget) are excluded to prevent remote
/// privilege escalation.
const GATEWAY_SAFE_SETTING_KEYS: &[&str] = &[
    "model",
    "temperature",
    "max_tokens",
    "provider",
    "conversation.max_iterations",
    "conversation.show_thinking",
    "conversation.tool_output_max_tokens",
    "memory.max_context_tokens",
    "skills.enabled",
    "skills.max_context_tokens",
];

fn handle_settings(db: &Database, config: &Config, args: &str) -> String {
    if args.is_empty() {
        // Show key settings
        let provider = config.llm.provider.as_deref().unwrap_or("(auto-detect)");
        let lines = [
            "Current settings:".to_string(),
            format!("  provider = {provider}"),
            format!("  model = {}", config.llm.model),
            format!("  temperature = {}", config.llm.temperature),
            format!("  max_tokens = {}", config.llm.max_tokens),
            format!(
                "  collaboration_mode = {}",
                config.conversation.collaboration_mode
            ),
            format!("  sandbox.enabled = {}", config.sandbox.enabled),
            "\nUsage: /settings <key> <value>".to_string(),
        ];
        return lines.join("\n");
    }
    let parts: Vec<&str> = args.splitn(2, ' ').collect();
    if parts.len() == 1 {
        // Show single setting value
        let key = parts[0];
        if let Ok(Some(val)) = db.get_setting(key) {
            return format!("{key} = {val} (DB override)");
        }
        match key {
            "model" => return format!("{key} = {}", config.llm.model),
            "temperature" => return format!("{key} = {}", config.llm.temperature),
            "max_tokens" => return format!("{key} = {}", config.llm.max_tokens),
            "provider" => {
                return format!(
                    "{key} = {}",
                    config.llm.provider.as_deref().unwrap_or("(auto-detect)")
                )
            }
            _ => return format!("Unknown or read-only setting: {key}"),
        }
    }
    // Two args: key value
    let key = parts[0];
    let value = parts[1].trim();
    if !GATEWAY_SAFE_SETTING_KEYS.contains(&key) {
        return format!("Setting '{key}' cannot be changed from a messaging channel.");
    }
    let mut clone = config.clone();
    match clone.apply_setting(key, value) {
        Ok(confirmation) => {
            if let Err(e) = db.set_setting(key, value) {
                warn!("Failed to persist setting: {e}");
                return format!("Validated but failed to persist: {e}");
            }
            format!("Updated: {confirmation}")
        }
        Err(e) => format!("Error: {e}"),
    }
}

/// Wrap evolution-command output in a fenced code block so channel formatters
/// render it as monospace (Telegram `<pre>`, Slack/Discord ``` ```, Signal
/// MONOSPACE style). Plain-text channels strip the outer fence at send time.
fn wrap_monospace(text: String) -> String {
    format!("```\n{}\n```", text.trim_end_matches('\n'))
}

fn handle_evolution(db: &Database) -> String {
    match borg_core::evolution::dispatch(borg_core::evolution::EvolutionCommand::Evolution, db) {
        Ok(out) => wrap_monospace(out.text),
        Err(e) => {
            warn!("/evolution dispatch failed: {e}");
            "Failed to render evolution.".to_string()
        }
    }
}

fn handle_xp(db: &Database) -> String {
    match borg_core::evolution::dispatch(borg_core::evolution::EvolutionCommand::Xp, db) {
        Ok(out) => wrap_monospace(out.text),
        Err(e) => {
            warn!("/xp dispatch failed: {e}");
            "Failed to render XP.".to_string()
        }
    }
}

/// Channels cannot write to the user's filesystem, so `--out <path>` is rejected
/// outright rather than silently ignored. Inline ASCII card only.
fn handle_card(db: &Database, args: &str) -> String {
    if !args.trim().is_empty() {
        return "/card on messaging channels does not accept arguments (no --out).".to_string();
    }
    match borg_core::evolution::dispatch(
        borg_core::evolution::EvolutionCommand::Card { out: None },
        db,
    ) {
        Ok(out) => wrap_monospace(out.text),
        Err(e) => {
            warn!("/card dispatch failed: {e}");
            "Failed to render card.".to_string()
        }
    }
}

fn handle_pairing(db: &Database, channel_name: &str, sender_id: &str) -> String {
    let approved = db
        .is_sender_approved(channel_name, sender_id)
        .unwrap_or(false);
    if approved {
        return format!("You are approved on channel '{channel_name}'.");
    }
    match db.find_pending_for_sender(channel_name, sender_id) {
        Ok(Some(_)) => format!(
            "You have a pending pairing request on channel '{channel_name}'. Ask the owner to approve it."
        ),
        Ok(None) => {
            "No pairing request found. Send a message to start the pairing process.".to_string()
        }
        Err(e) => {
            warn!("Failed to check pairing status: {e}");
            "Failed to check pairing status.".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_commands() {
        assert!(matches!(
            Command::parse("/status"),
            Some((Command::Status, _))
        ));
        assert!(matches!(Command::parse("/new"), Some((Command::New, _))));
        assert!(matches!(
            Command::parse("/reset"),
            Some((Command::Reset, _))
        ));
        assert!(matches!(
            Command::parse("/usage"),
            Some((Command::Usage, _))
        ));
        assert!(matches!(Command::parse("/help"), Some((Command::Help, _))));
        assert!(matches!(
            Command::parse("/commands"),
            Some((Command::Commands, _))
        ));
        assert!(matches!(
            Command::parse("/skills"),
            Some((Command::Skills, _))
        ));
        assert!(matches!(
            Command::parse("/whoami"),
            Some((Command::WhoAmI, _))
        ));
        assert!(matches!(
            Command::parse("/memory"),
            Some((Command::Memory, _))
        ));
        assert!(matches!(
            Command::parse("/doctor"),
            Some((Command::Doctor, _))
        ));
        assert!(matches!(
            Command::parse("/pairing"),
            Some((Command::Pairing, _))
        ));
        assert!(matches!(
            Command::parse("/skill"),
            Some((Command::Skill, _))
        ));
        assert!(matches!(
            Command::parse("/cancel"),
            Some((Command::Cancel, _))
        ));
        // /stop and /abort are aliases for /cancel.
        assert!(matches!(
            Command::parse("/stop"),
            Some((Command::Cancel, _))
        ));
        assert!(matches!(
            Command::parse("/abort"),
            Some((Command::Cancel, _))
        ));
    }

    #[test]
    fn is_cancel_command_matches_aliases() {
        assert!(is_cancel_command("/cancel"));
        assert!(is_cancel_command("/stop"));
        assert!(is_cancel_command("/abort"));
        assert!(is_cancel_command("/Cancel"));
        assert!(is_cancel_command("/cancel@MyBorgBot"));
        assert!(!is_cancel_command("/status"));
        assert!(!is_cancel_command("cancel"));
        assert!(!is_cancel_command(""));
    }

    #[tokio::test]
    async fn cancel_command_intercepted_via_in_flight_registry() {
        // Simulates the handler.rs interception path: a /cancel message finds
        // the session's in-flight token in the global registry and cancels it.
        use tokio_util::sync::CancellationToken;
        let session_id = "test-session-cancel-intercept";
        let token = CancellationToken::new();
        crate::in_flight::GLOBAL
            .register(session_id, token.clone())
            .await;
        assert!(is_cancel_command("/cancel"));
        assert!(crate::in_flight::GLOBAL.cancel(session_id).await);
        assert!(token.is_cancelled());
    }

    #[test]
    fn parse_evolution_commands() {
        assert!(matches!(
            Command::parse("/evolution"),
            Some((Command::Evolution, _))
        ));
        assert!(matches!(Command::parse("/xp"), Some((Command::Xp, _))));
        assert!(matches!(Command::parse("/card"), Some((Command::Card, _))));
        // case-insensitive
        assert!(matches!(
            Command::parse("/Evolution"),
            Some((Command::Evolution, _))
        ));
        assert!(matches!(Command::parse("/XP"), Some((Command::Xp, _))));
        // @botname suffix
        assert!(matches!(
            Command::parse("/xp@MyBorgBot"),
            Some((Command::Xp, _))
        ));
        // /card forwards its args (handler rejects non-empty on channels)
        let (cmd, args) = Command::parse("/card --out /tmp/x").unwrap();
        assert!(matches!(cmd, Command::Card));
        assert_eq!(args, "--out /tmp/x");
    }

    fn gateway_test_db() -> Database {
        Database::from_connection(rusqlite::Connection::open_in_memory().unwrap()).unwrap()
    }

    #[test]
    fn handle_card_rejects_out_argument() {
        let db = gateway_test_db();
        let out = handle_card(&db, "--out /tmp/x");
        assert!(
            out.starts_with("/card on messaging channels does not accept arguments"),
            "got: {out}"
        );
    }

    #[test]
    fn handle_card_inline_renders_ascii() {
        let db = gateway_test_db();
        let out = handle_card(&db, "");
        assert!(out.contains("Stage:"), "missing Stage: in {out}");
        assert!(
            out.contains('\u{256D}') || out.contains('\u{2570}'),
            "missing ASCII box border in {out}"
        );
    }

    // Evolution commands produce Unicode box-drawing + block-character output
    // that only looks right in a monospace font. The gateway handlers wrap
    // results in a ``` fence so channel formatters (Telegram <pre>, Slack
    // mrkdwn, Discord, Signal MONOSPACE range) render them correctly.
    // Plain-text channels strip the outer fence at send time.
    #[test]
    fn evolution_handlers_wrap_success_output_in_code_fence() {
        let db = gateway_test_db();

        for out in [handle_card(&db, ""), handle_xp(&db), handle_evolution(&db)] {
            assert!(out.starts_with("```\n"), "expected opening fence: {out}");
            assert!(out.ends_with("\n```"), "expected closing fence: {out}");
        }
    }

    #[test]
    fn handle_card_error_string_is_unwrapped() {
        let db = gateway_test_db();
        let out = handle_card(&db, "--out /tmp/x");
        assert!(!out.starts_with("```"), "error should not be fenced: {out}");
    }

    #[test]
    fn parse_new_commands() {
        assert!(matches!(Command::parse("/mode"), Some((Command::Mode, _))));
        assert!(matches!(Command::parse("/plan"), Some((Command::Plan, _))));
        assert!(matches!(
            Command::parse("/compact"),
            Some((Command::Compact, _))
        ));
        assert!(matches!(Command::parse("/undo"), Some((Command::Undo, _))));
        assert!(matches!(
            Command::parse("/history"),
            Some((Command::History, _))
        ));
        assert!(matches!(
            Command::parse("/schedule"),
            Some((Command::Schedule, _))
        ));
        assert!(matches!(
            Command::parse("/settings"),
            Some((Command::Settings, _))
        ));
        assert!(matches!(Command::parse("/poke"), Some((Command::Poke, _))));
    }

    #[test]
    fn parse_mode_with_args() {
        let (cmd, args) = Command::parse("/mode execute").unwrap();
        assert!(matches!(cmd, Command::Mode));
        assert_eq!(args, "execute");
    }

    #[test]
    fn parse_plan_with_args() {
        let (cmd, args) = Command::parse("/plan analyze the codebase").unwrap();
        assert!(matches!(cmd, Command::Plan));
        assert_eq!(args, "analyze the codebase");
    }

    #[test]
    fn parse_compact_with_count() {
        let (cmd, args) = Command::parse("/compact 10").unwrap();
        assert!(matches!(cmd, Command::Compact));
        assert_eq!(args, "10");
    }

    #[test]
    fn parse_history_with_count() {
        let (cmd, args) = Command::parse("/history 25").unwrap();
        assert!(matches!(cmd, Command::History));
        assert_eq!(args, "25");
    }

    #[test]
    fn parse_settings_with_key_value() {
        let (cmd, args) = Command::parse("/settings temperature 0.5").unwrap();
        assert!(matches!(cmd, Command::Settings));
        assert_eq!(args, "temperature 0.5");
    }

    #[test]
    fn is_poke_command_matches() {
        assert!(is_poke_command("/poke"));
        assert!(is_poke_command("/Poke"));
        assert!(is_poke_command("/poke@MyBorgBot"));
        assert!(!is_poke_command("/status"));
        assert!(!is_poke_command("poke"));
        assert!(!is_poke_command(""));
    }

    #[test]
    fn parse_unknown_command_returns_none() {
        assert!(Command::parse("/unknown").is_none());
        assert!(Command::parse("hello").is_none());
        assert!(Command::parse("").is_none());
    }

    #[test]
    fn parse_command_with_trailing_text() {
        let (cmd, args) = Command::parse("/status extra args").unwrap();
        assert!(matches!(cmd, Command::Status));
        assert_eq!(args, "extra args");
    }

    #[test]
    fn parse_skill_with_args() {
        let (cmd, args) = Command::parse("/skill weather").unwrap();
        assert!(matches!(cmd, Command::Skill));
        assert_eq!(args, "weather");
    }

    #[test]
    fn parse_case_insensitive() {
        assert!(matches!(
            Command::parse("/Status"),
            Some((Command::Status, _))
        ));
        assert!(matches!(Command::parse("/HELP"), Some((Command::Help, _))));
        assert!(matches!(Command::parse("/New"), Some((Command::New, _))));
        assert!(matches!(
            Command::parse("/SKILLS"),
            Some((Command::Skills, _))
        ));
    }

    #[test]
    fn parse_strips_botname_suffix() {
        assert!(matches!(
            Command::parse("/help@MyBorgBot"),
            Some((Command::Help, _))
        ));
        assert!(matches!(
            Command::parse("/status@BotName"),
            Some((Command::Status, _))
        ));
        assert!(matches!(
            Command::parse("/skills@Bot extra"),
            Some((Command::Skills, _))
        ));
    }

    #[test]
    fn help_contains_all_commands() {
        let help = handle_help();
        for cmd in GATEWAY_COMMANDS {
            assert!(
                help.contains(&format!("/{}", cmd.name)),
                "help missing /{}",
                cmd.name
            );
        }
    }

    #[test]
    fn commands_contains_all_commands() {
        let output = handle_commands();
        for cmd in GATEWAY_COMMANDS {
            assert!(
                output.contains(&format!("/{}", cmd.name)),
                "commands missing /{}",
                cmd.name
            );
        }
    }

    #[test]
    fn gateway_commands_no_duplicates() {
        let mut names: Vec<&str> = GATEWAY_COMMANDS.iter().map(|c| c.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), GATEWAY_COMMANDS.len());
    }
}
