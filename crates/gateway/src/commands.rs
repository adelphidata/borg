use anyhow::Result;
use async_trait::async_trait;
use cortex::config::Config;
use cortex::db::Database;
use tracing::warn;

/// Metadata for a single gateway slash command.
///
/// [`GATEWAY_COMMANDS`] is the single source of truth: parsing
/// ([`Command::parse`]), `/help` output, and native platform menu
/// registration all derive from this table, so adding a command means
/// adding one entry (plus its dispatch arm, which the compiler enforces).
pub struct CommandDef {
    pub name: &'static str,
    pub description: &'static str,
    pub accepts_args: bool,
    /// If true, the command is not intercepted — it passes through to the agent.
    pub pass_through: bool,
    /// The parsed command this entry maps to.
    cmd: Command,
    /// Alternate names accepted by the parser (without the leading `/`),
    /// e.g. `/stop` and `/abort` for `/cancel`. Not shown in menus.
    aliases: &'static [&'static str],
}

/// All gateway slash commands. Used for text parsing, /help output, and native
/// platform menu registration (Telegram setMyCommands, Discord global commands).
pub const GATEWAY_COMMANDS: &[CommandDef] = &[
    CommandDef {
        name: "start",
        cmd: Command::Start,
        aliases: &[],
        description: "Start a conversation with Borg",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "help",
        cmd: Command::Help,
        aliases: &[],
        description: "Show help and available commands",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "commands",
        cmd: Command::Commands,
        aliases: &[],
        description: "List all slash commands",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "status",
        cmd: Command::Status,
        aliases: &[],
        description: "Show current session info",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "new",
        cmd: Command::New,
        aliases: &[],
        description: "Start a new session",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "reset",
        cmd: Command::Reset,
        aliases: &[],
        description: "Clear current session messages",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "usage",
        cmd: Command::Usage,
        aliases: &[],
        description: "Show token usage and budget",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "skill",
        cmd: Command::Skill,
        aliases: &[],
        description: "Run a skill by name",
        accepts_args: true,
        pass_through: true,
    },
    CommandDef {
        name: "skills",
        cmd: Command::Skills,
        aliases: &[],
        description: "List available skills",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "whoami",
        cmd: Command::WhoAmI,
        aliases: &[],
        description: "Show your sender identity",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "memory",
        cmd: Command::Memory,
        aliases: &[],
        description: "Show memory inventory",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "doctor",
        cmd: Command::Doctor,
        aliases: &[],
        description: "Run system diagnostics",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "dream",
        cmd: Command::Dream,
        aliases: &[],
        description: "Run memory consolidation now",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "pairing",
        cmd: Command::Pairing,
        aliases: &[],
        description: "Show pairing status",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "voice",
        cmd: Command::Voice,
        aliases: &[],
        description: "Show or change Telegram voice reply mode",
        accepts_args: true,
        pass_through: false,
    },
    CommandDef {
        name: "cancel",
        cmd: Command::Cancel,
        aliases: &["stop", "abort"],
        description: "Stop the current in-progress turn",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "mode",
        cmd: Command::Mode,
        aliases: &[],
        description: "Show or switch collaboration mode (default/execute/plan)",
        accepts_args: true,
        pass_through: false,
    },
    CommandDef {
        name: "plan",
        cmd: Command::Plan,
        aliases: &[],
        description: "Enter plan mode, optionally with a message",
        accepts_args: true,
        pass_through: false,
    },
    CommandDef {
        name: "compact",
        cmd: Command::Compact,
        aliases: &[],
        description: "Trim session history, keeping last N messages",
        accepts_args: true,
        pass_through: false,
    },
    CommandDef {
        name: "undo",
        cmd: Command::Undo,
        aliases: &[],
        description: "Undo the last agent turn",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "history",
        cmd: Command::History,
        aliases: &[],
        description: "Show recent conversation messages",
        accepts_args: true,
        pass_through: false,
    },
    CommandDef {
        name: "schedule",
        cmd: Command::Schedule,
        aliases: &[],
        description: "List scheduled tasks",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "settings",
        cmd: Command::Settings,
        aliases: &[],
        description: "Show or change a setting",
        accepts_args: true,
        pass_through: false,
    },
    CommandDef {
        name: "poke",
        cmd: Command::Poke,
        aliases: &[],
        description: "Trigger an immediate heartbeat",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "evolution",
        cmd: Command::Evolution,
        aliases: &[],
        description: "Show evolution stage, archetype, readiness",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "xp",
        cmd: Command::Xp,
        aliases: &[],
        description: "Show XP totals, sources, and recent feed",
        accepts_args: false,
        pass_through: false,
    },
    CommandDef {
        name: "card",
        cmd: Command::Card,
        aliases: &[],
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

#[derive(Clone, Copy)]
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
    Dream,
    Pairing,
    Voice,
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
    /// any trailing arguments. Recognized names come from
    /// [`GATEWAY_COMMANDS`], so the parser can't drift from the menu table.
    fn parse(text: &str) -> Option<(Self, &str)> {
        let first_word = text.split_whitespace().next()?;
        let lowered = first_word.to_ascii_lowercase();
        // Strip @botname suffix
        let cmd_str = lowered.split('@').next().unwrap_or(&lowered);
        let name = cmd_str.strip_prefix('/')?;
        let args = text[first_word.len()..].trim_start();
        let def = GATEWAY_COMMANDS
            .iter()
            .find(|d| d.name == name || d.aliases.contains(&name))?;
        Some((def.cmd, args))
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

/// Returns `true` if `text` parses as the `/dream` slash command. Handled
/// separately from [`try_handle_command`] because dream requires async work.
pub fn is_dream_command(text: &str) -> bool {
    matches!(Command::parse(text), Some((Command::Dream, _)))
}

/// Try to handle a slash command. Returns `Some(response)` if the message was
/// a recognised command, `None` to fall through to the agent.
///
/// Note: `/cancel` is **not** handled here — it is intercepted in
/// `handler::invoke_agent_with_auto_reply` via [`is_cancel_command`] so the
/// async cancellation path can run without holding a DB reference.
#[allow(clippy::too_many_arguments)]
pub async fn try_handle_command(
    text: &str,
    db: &Database,
    config: &Config,
    channel_name: &str,
    channel_id: Option<&str>,
    session_key: &str,
    session_id: &str,
    sender_id: &str,
) -> Option<String> {
    let (cmd, args) = Command::parse(text)?;
    let response = match cmd {
        Command::Start => handle_help(), // welcome approved users with help; unapproved users see pairing challenge before reaching here
        Command::Help => handle_help(),
        Command::Commands => handle_commands(),
        Command::Status => handle_status(db, session_id).await,
        Command::New => handle_new(db, channel_name, session_key).await,
        Command::Reset => handle_reset(db, session_id).await,
        Command::Usage => handle_usage(db, config, session_id).await,
        Command::Skill => return None, // Pass through to agent
        Command::Skills => handle_skills(config),
        Command::WhoAmI => handle_whoami(sender_id, channel_name, db).await,
        Command::Memory => handle_memory(config).await,
        Command::Doctor => handle_doctor(config).await,
        Command::Dream => return None,
        Command::Pairing => handle_pairing(db, channel_name, sender_id).await,
        Command::Voice => handle_voice(db, config, channel_name, channel_id, args).await,
        // Handled out-of-band in the caller; see `is_cancel_command`.
        Command::Cancel => return None,
        Command::Mode => handle_mode(db, config, args).await,
        Command::Plan => {
            return handle_plan(db, args).await;
        }
        Command::Compact => handle_compact(db, session_id, args).await,
        Command::Undo => handle_undo(db, session_id).await,
        Command::History => handle_history(db, session_id, args).await,
        Command::Schedule => handle_schedule(db).await,
        Command::Settings => handle_settings(db, config, args).await,
        // Handled out-of-band in the caller; see `is_poke_command`.
        Command::Poke => return None,
        Command::Evolution => handle_evolution(db).await,
        Command::Xp => handle_xp(db).await,
        Command::Card => handle_card(db, args).await,
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

async fn handle_status(db: &Database, session_id: &str) -> String {
    let msg_count = db.count_session_messages(session_id).await.unwrap_or(0);
    let short_id = session_id.get(..8).unwrap_or(session_id);
    format!("Session: {short_id}...\nMessages: {msg_count}")
}

async fn handle_new(db: &Database, channel_name: &str, session_key: &str) -> String {
    let new_id = uuid::Uuid::new_v4().to_string();
    match db
        .update_channel_session_id(channel_name, session_key, &new_id)
        .await
    {
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

async fn handle_reset(db: &Database, session_id: &str) -> String {
    match db.delete_session_messages(session_id).await {
        Ok(count) => format!("Cleared {count} messages from current session."),
        Err(e) => {
            warn!("Failed to reset session: {e}");
            "Failed to reset session.".to_string()
        }
    }
}

async fn handle_usage(db: &Database, config: &Config, session_id: &str) -> String {
    let msg_count = db.count_session_messages(session_id).await.unwrap_or(0);
    let monthly_tokens = db.monthly_token_total().await.unwrap_or(0);

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
    match cortex::skills::load_all_skills(&resolved_creds, &config.skills, None) {
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

async fn handle_whoami(sender_id: &str, channel_name: &str, db: &Database) -> String {
    let approved = db
        .is_sender_approved(channel_name, sender_id)
        .await
        .unwrap_or(false);
    let status = if approved { "Approved" } else { "Not approved" };
    format!("Sender: {sender_id}\nChannel: {channel_name}\nPairing: {status}")
}

async fn handle_memory(config: &Config) -> String {
    match cortex::memory::list_memory_inventory(config).await {
        Ok(items) if items.is_empty() => "No memory entries or knowledge files.".to_string(),
        Ok(items) => {
            let mut lines = vec![format!("Memory inventory ({}):", items.len())];
            for item in &items {
                let size_kb = item.size_bytes as f64 / 1024.0;
                lines.push(format!(
                    "  [{}:{}] {} ({:.1} KB)",
                    item.store.label(),
                    item.scope,
                    item.name,
                    size_kb
                ));
            }
            lines.join("\n")
        }
        Err(e) => {
            warn!("Failed to list memory inventory: {e}");
            "Failed to list memory inventory.".to_string()
        }
    }
}

async fn handle_doctor(config: &Config) -> String {
    format_doctor_report(&cortex::doctor::run_diagnostics(config).await)
}

/// Render a diagnostics report for a messaging channel: one summary line,
/// then only the checks that need attention. Pure so the branch logic is
/// testable without running the (network-probing) diagnostics themselves.
fn format_doctor_report(report: &cortex::doctor::DiagnosticReport) -> String {
    let (pass, warnings, fail) = report.counts();
    let mut lines = vec![format!(
        "Diagnostics: {pass} passed, {warnings} warning(s), {fail} failed"
    )];
    for check in &report.checks {
        match &check.status {
            cortex::doctor::CheckStatus::Warn(msg) => {
                lines.push(format!("  ⚠ {} — {msg}", check.name));
            }
            cortex::doctor::CheckStatus::Fail(msg) => {
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

/// The setting `/mode` and `/plan` change. Every client reads the same one,
/// so a chat switch applies to the TUI and the apps too.
const MODE_KEY: &str = "conversation.collaboration_mode";

/// Store `mode` and push it to the running agents, so the next turn uses it.
async fn store_mode(db: &Database, mode: cortex::config::CollaborationMode) -> anyhow::Result<()> {
    db.set_setting(MODE_KEY, &mode.to_string()).await?;
    cortex::config_watcher::reload_now().await;
    Ok(())
}

async fn handle_mode(db: &Database, config: &Config, args: &str) -> String {
    if args.is_empty() {
        let current = config.conversation.collaboration_mode;
        return format!("Collaboration mode: {current}\nUsage: /mode <execute|plan>");
    }
    let Ok(mode) = args.trim().parse::<cortex::config::CollaborationMode>() else {
        return "Invalid mode. Choose: execute, plan".to_string();
    };
    if let Err(e) = store_mode(db, mode).await {
        warn!("Failed to set mode: {e}");
        return "Failed to set mode.".to_string();
    }
    format!("Switched to {mode} mode for all conversations.")
}

async fn handle_voice(
    db: &Database,
    config: &Config,
    channel_name: &str,
    channel_id: Option<&str>,
    args: &str,
) -> String {
    let arg_lower = args.trim().to_ascii_lowercase();

    // Discord voice-channel subcommands.
    // `/voice join <channel_id>` joins; `/voice channel <id>` switches;
    // `/voice leave` disconnects. When built with the
    // `discord-voice-channels` feature AND a Discord bot is configured,
    // these route to the songbird-backed manager; otherwise return the
    // stub message naming the gap.
    if matches!(arg_lower.as_str(), "join" | "leave")
        || arg_lower.starts_with("join ")
        || arg_lower.starts_with("channel ")
    {
        return handle_discord_voice_channel(channel_id, &arg_lower).await;
    }

    if channel_name != "telegram" {
        return "Voice mode is currently implemented for Telegram only.".to_string();
    }

    let Some(chat_id) = channel_id.filter(|id| !id.is_empty()) else {
        return "Voice mode is unavailable because this Telegram chat ID is missing.".to_string();
    };

    let key = crate::voice::voice_mode_setting_key(channel_name, chat_id);
    let current =
        match crate::voice::resolve_voice_reply_mode(db, config, channel_name, chat_id).await {
            Ok(mode) => mode,
            Err(e) => {
                warn!("Failed to read voice mode: {e}");
                return "Failed to read voice mode.".to_string();
            }
        };
    let arg = args.trim().to_ascii_lowercase();

    if arg.is_empty() || arg == "status" {
        if let Some(reason) = crate::voice::voice_reply_unavailable_reason(config) {
            return format!("Voice mode: {}. {reason}", current.status_label());
        }
        return format!("Voice mode: {}", current.status_label());
    }

    let next = match arg.as_str() {
        "on" | "enable" => crate::voice::VoiceReplyMode::VoiceOnly,
        "tts" => crate::voice::VoiceReplyMode::All,
        "off" | "disable" => crate::voice::VoiceReplyMode::Off,
        _ => {
            return "Usage: /voice [on|off|tts|status]".to_string();
        }
    };

    if next != crate::voice::VoiceReplyMode::Off {
        if let Some(reason) = crate::voice::voice_reply_unavailable_reason(config) {
            return reason.to_string();
        }
    }

    if let Err(e) = db.set_setting(&key, next.as_str()).await {
        warn!("Failed to set voice mode: {e}");
        return "Failed to update voice mode.".to_string();
    }

    format!("Voice mode: {}", next.status_label())
}

/// Dispatch a Discord voice-channel subcommand. When built with
/// `discord-voice-channels` AND the voice manager handle has been
/// installed by `Server::run`, this drives the songbird-backed manager
/// to actually join/leave the voice gateway. Otherwise returns a clear
/// "not wired" message so users know what's available.
async fn handle_discord_voice_channel(channel_id: Option<&str>, arg_lower: &str) -> String {
    #[cfg(feature = "discord-voice-channels")]
    {
        if let Some(handle) = crate::discord::voice_channels::voice_manager_handle() {
            return dispatch_voice_channel_via_handle(handle, channel_id, arg_lower).await;
        }
    }
    let _ = channel_id; // unused when feature disabled
    let action = if arg_lower == "leave" {
        "leave"
    } else if arg_lower.starts_with("channel ") || arg_lower == "channel" {
        "switch"
    } else {
        "join"
    };
    format!(
        "Discord voice-channel `{action}` is not active in this build. \
         Rebuild with `--features discord-voice-channels` and configure a \
         Discord bot token to enable; until then the `/voice on|off|tts|status` \
         family is what's available."
    )
}

#[cfg(feature = "discord-voice-channels")]
async fn dispatch_voice_channel_via_handle(
    handle: &crate::discord::voice_channels::VoiceManagerHandle,
    discord_channel_id: Option<&str>,
    arg_lower: &str,
) -> String {
    // The user-facing inputs:
    //   `/voice join` — join the user's current voice channel (TODO: we
    //                   need a guild lookup + Discord-side resolution of
    //                   "user's current channel" which requires GUILD_MEMBERS
    //                   intent + cache; reject for now).
    //   `/voice join <channel_id>` — explicit channel id.
    //   `/voice channel <name>` — switch to a different channel.
    //   `/voice leave` — disconnect.
    //
    // Resolving channel names → ids would need a guild cache, which our
    // Discord client doesn't maintain (interactions come pre-resolved with
    // channel_id). Accept only the explicit-id form and direct users at it.
    let parts: Vec<&str> = arg_lower.split_whitespace().collect();
    let action = parts.first().copied().unwrap_or("");
    let arg = parts.get(1).copied().unwrap_or("");
    // discord_channel_id is the channel_id of the text channel the command
    // came from. We need the guild_id, which Discord interactions also carry
    // — but our current dispatch path drops it. As a minimum-viable shim:
    // require the user to supply guild_id explicitly via `<guild>:<channel>`
    // syntax until guild_id plumbing lands in commands::dispatch.
    let parse_target = |s: &str| -> Option<(u64, u64)> {
        let (g, c) = s.split_once(':')?;
        let g = g.trim().parse::<u64>().ok()?;
        let c = c.trim().parse::<u64>().ok()?;
        // `<guild>:user:<id>` is the user-resolve form — exclude it here
        // so the explicit-channel path doesn't accidentally accept it.
        if g == 0 || c == 0 {
            return None;
        }
        Some((g, c))
    };
    // `<guild>:user:<user_id>` form → resolve current channel via cache.
    let parse_user_target = |s: &str| -> Option<(u64, u64)> {
        let mut parts = s.split(':');
        let g = parts.next()?.trim().parse::<u64>().ok()?;
        if parts.next()?.trim() != "user" {
            return None;
        }
        let u = parts.next()?.trim().parse::<u64>().ok()?;
        Some((g, u))
    };

    match action {
        "join" => {
            // Three accepted forms in priority order:
            //   /voice join <guild>:<channel>   — explicit
            //   /voice join <guild>:user:<user> — resolve user's current channel
            //   /voice join                     — resolve from interaction context
            // The third form requires guild_id + caller user_id wired
            // from the slash-command dispatcher, which isn't plumbed
            // yet — fall back to a usage hint when arg is empty.
            if let Some((guild_id, channel_id)) = parse_target(arg) {
                return match handle.join(guild_id, channel_id).await {
                    Ok(()) => {
                        format!("Joined Discord voice channel {channel_id} in guild {guild_id}.")
                    }
                    Err(e) => format!("Failed to join voice channel: {e}"),
                };
            }
            // `<guild>:user:<user_id>` — auto-resolve via voice-state cache.
            if let Some((guild_id, user_id)) = parse_user_target(arg) {
                return match handle.resolve_user_channel(guild_id, user_id).await {
                    Some(channel_id) => match handle.join(guild_id, channel_id).await {
                        Ok(()) => format!(
                            "Joined user {user_id}'s voice channel ({channel_id}) in guild {guild_id}."
                        ),
                        Err(e) => format!("Failed to join voice channel: {e}"),
                    },
                    None => format!(
                        "Don't know what channel user {user_id} is in. \
                         They need to be in a voice channel and the bot must \
                         have observed their VOICE_STATE_UPDATE event since \
                         it connected."
                    ),
                };
            }
            format!(
                "Usage: /voice join <guild_id>:<channel_id>  or  \
                 /voice join <guild_id>:user:<user_id> (auto-resolves their channel). \
                 Discord interaction context: {discord_channel_id:?}"
            )
        }
        "leave" => {
            let Some(guild_id) = arg.parse::<u64>().ok() else {
                return "Usage: /voice leave <guild_id>".to_string();
            };
            match handle.leave(guild_id).await {
                Ok(()) => format!("Left Discord voice channel in guild {guild_id}."),
                Err(e) => format!("Failed to leave voice channel: {e}"),
            }
        }
        "channel" => {
            // Switching channel = leave + rejoin a different channel id.
            let target = parse_target(arg);
            let Some((guild_id, channel_id)) = target else {
                return "Usage: /voice channel <guild_id>:<channel_id>".to_string();
            };
            let _ = handle.leave(guild_id).await;
            match handle.join(guild_id, channel_id).await {
                Ok(()) => format!("Switched to voice channel {channel_id}."),
                Err(e) => format!("Failed to switch voice channel: {e}"),
            }
        }
        _ => "Usage: /voice [join|leave|channel] <guild_id>:<channel_id>".to_string(),
    }
}

/// Handle `/plan [message]`. Returns `Some(response)` for bare `/plan`,
/// `None` for `/plan <message>` (pass-through to agent after setting mode).
async fn handle_plan(db: &Database, args: &str) -> Option<String> {
    if let Err(e) = store_mode(db, cortex::config::CollaborationMode::Plan).await {
        warn!("Failed to set plan mode: {e}");
        return Some("Failed to enter plan mode.".to_string());
    }
    if args.is_empty() {
        Some("Entered plan mode. Send your message to begin planning.".to_string())
    } else {
        // Pass through: the agent reloads the stored mode at turn start.
        None
    }
}

async fn handle_compact(db: &Database, session_id: &str, args: &str) -> String {
    let keep: usize = if args.is_empty() {
        20
    } else {
        match args.trim().parse() {
            Ok(n) if n > 0 => n,
            _ => return "Usage: /compact [keep_count] (positive integer, default 20)".to_string(),
        }
    };
    match db.compact_session_messages(session_id, keep).await {
        Ok(0) => format!("Nothing to compact (≤{keep} messages)."),
        Ok(deleted) => format!("Compacted: removed {deleted} older messages, kept last {keep}."),
        Err(e) => {
            warn!("Failed to compact session: {e}");
            "Failed to compact session.".to_string()
        }
    }
}

async fn handle_undo(db: &Database, session_id: &str) -> String {
    match db.delete_last_assistant_turn(session_id).await {
        Ok(0) => "Nothing to undo.".to_string(),
        Ok(n) => format!("Undone: removed {n} messages from last agent turn."),
        Err(e) => {
            warn!("Failed to undo: {e}");
            "Failed to undo last turn.".to_string()
        }
    }
}

async fn handle_history(db: &Database, session_id: &str, args: &str) -> String {
    let count: usize = if args.is_empty() {
        10
    } else {
        match args.trim().parse() {
            Ok(n) if n > 0 && n <= 50 => n,
            _ => return "Usage: /history [count] (1-50, default 10)".to_string(),
        }
    };
    match db.load_session_lineage_rows(session_id).await {
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

async fn handle_schedule(db: &Database) -> String {
    match db.list_tasks().await {
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

async fn handle_settings(db: &Database, config: &Config, args: &str) -> String {
    if args.is_empty() {
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
        let key = parts[0];
        if let Ok(Some(val)) = db.get_setting(key).await {
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
            if let Err(e) = db.set_setting(key, value).await {
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

async fn handle_evolution(db: &Database) -> String {
    match cortex::evolution::dispatch(cortex::evolution::EvolutionCommand::Evolution, db).await {
        Ok(out) => wrap_monospace(out.text),
        Err(e) => {
            warn!("/evolution dispatch failed: {e}");
            "Failed to render evolution.".to_string()
        }
    }
}

async fn handle_xp(db: &Database) -> String {
    match cortex::evolution::dispatch(cortex::evolution::EvolutionCommand::Xp, db).await {
        Ok(out) => wrap_monospace(out.text),
        Err(e) => {
            warn!("/xp dispatch failed: {e}");
            "Failed to render XP.".to_string()
        }
    }
}

/// Channels cannot write to the user's filesystem, so `--out <path>` is rejected
/// outright rather than silently ignored. Inline ASCII card only.
async fn handle_card(db: &Database, args: &str) -> String {
    if !args.trim().is_empty() {
        return "/card on messaging channels does not accept arguments (no --out).".to_string();
    }
    match cortex::evolution::dispatch(cortex::evolution::EvolutionCommand::Card { out: None }, db)
        .await
    {
        Ok(out) => wrap_monospace(out.text),
        Err(e) => {
            warn!("/card dispatch failed: {e}");
            "Failed to render card.".to_string()
        }
    }
}

async fn handle_pairing(db: &Database, channel_name: &str, sender_id: &str) -> String {
    let approved = db
        .is_sender_approved(channel_name, sender_id)
        .await
        .unwrap_or(false);
    if approved {
        return format!("You are approved on channel '{channel_name}'.");
    }
    match db.find_pending_for_sender(channel_name, sender_id).await {
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
    fn parse_maps_slash_commands_to_variants() {
        use std::mem::discriminant;
        let cases: &[(&str, Command, &str)] = &[
            // Core commands.
            ("/start", Command::Start, ""),
            ("/help", Command::Help, ""),
            ("/commands", Command::Commands, ""),
            ("/status", Command::Status, ""),
            ("/new", Command::New, ""),
            ("/reset", Command::Reset, ""),
            ("/usage", Command::Usage, ""),
            ("/skill", Command::Skill, ""),
            ("/skills", Command::Skills, ""),
            ("/whoami", Command::WhoAmI, ""),
            ("/memory", Command::Memory, ""),
            ("/doctor", Command::Doctor, ""),
            ("/dream", Command::Dream, ""),
            ("/pairing", Command::Pairing, ""),
            ("/voice", Command::Voice, ""),
            ("/cancel", Command::Cancel, ""),
            ("/mode", Command::Mode, ""),
            ("/plan", Command::Plan, ""),
            ("/compact", Command::Compact, ""),
            ("/undo", Command::Undo, ""),
            ("/history", Command::History, ""),
            ("/schedule", Command::Schedule, ""),
            ("/settings", Command::Settings, ""),
            ("/poke", Command::Poke, ""),
            ("/evolution", Command::Evolution, ""),
            ("/xp", Command::Xp, ""),
            ("/card", Command::Card, ""),
            // Cancel aliases.
            ("/stop", Command::Cancel, ""),
            ("/abort", Command::Cancel, ""),
            // Case-insensitive.
            ("/Status", Command::Status, ""),
            ("/HELP", Command::Help, ""),
            ("/Evolution", Command::Evolution, ""),
            ("/XP", Command::Xp, ""),
            // @botname suffix stripped.
            ("/help@MyBorgBot", Command::Help, ""),
            ("/status@BotName", Command::Status, ""),
            ("/xp@MyBorgBot", Command::Xp, ""),
            ("/skills@Bot extra", Command::Skills, "extra"),
            // Args preserved verbatim.
            ("/mode execute", Command::Mode, "execute"),
            (
                "/plan analyze the codebase",
                Command::Plan,
                "analyze the codebase",
            ),
            ("/compact 10", Command::Compact, "10"),
            ("/history 25", Command::History, "25"),
            (
                "/settings temperature 0.5",
                Command::Settings,
                "temperature 0.5",
            ),
            ("/skill weather", Command::Skill, "weather"),
            ("/card --out /tmp/x", Command::Card, "--out /tmp/x"),
            ("/status extra args", Command::Status, "extra args"),
        ];
        for (input, want_cmd, want_args) in cases {
            let (cmd, args) =
                Command::parse(input).unwrap_or_else(|| panic!("expected Some for {input}"));
            assert_eq!(
                discriminant(&cmd),
                discriminant(want_cmd),
                "wrong variant for {input}"
            );
            assert_eq!(args, *want_args, "wrong args for {input}");
        }
    }

    #[test]
    fn parse_rejects_non_commands() {
        for input in ["/unknown", "hello", ""] {
            assert!(
                Command::parse(input).is_none(),
                "expected None for {input:?}"
            );
        }
    }

    #[test]
    fn wrap_monospace_strips_trailing_newlines_only() {
        // Trailing newlines collapsed; the output must end with exactly one
        // newline before the closing fence.
        assert_eq!(wrap_monospace("hi\n\n".to_string()), "```\nhi\n```");
        // Leading/embedded whitespace preserved verbatim — destroying it
        // would mangle code-like output (indentation matters).
        assert_eq!(wrap_monospace("  x".to_string()), "```\n  x\n```");
        assert_eq!(wrap_monospace("a\nb\nc".to_string()), "```\na\nb\nc\n```");
        // A single trailing newline still trims to the same shape.
        assert_eq!(wrap_monospace("a\n".to_string()), "```\na\n```");
        // Empty string round-trips to an empty fenced block, not a missing one.
        assert_eq!(wrap_monospace(String::new()), "```\n\n```");
    }

    #[test]
    fn handle_help_lists_every_gateway_command() {
        let out = handle_help();
        assert!(out.starts_with("Available commands:"));
        for cmd in GATEWAY_COMMANDS {
            // Every registered command must surface in /help — guards against
            // adding a CommandDef but forgetting to wire it through.
            let needle = format!("/{}", cmd.name);
            assert!(
                out.contains(&needle),
                "/help output missing {needle}: {out}"
            );
            assert!(
                out.contains(cmd.description),
                "/help output missing description for {}: {out}",
                cmd.name
            );
            // accepts_args must render `<args>`; non-arg commands must not
            // attach `<args>` to their line.
            let line = out
                .lines()
                .find(|l| l.contains(&format!("/{}", cmd.name)) && l.contains(cmd.description))
                .unwrap_or_else(|| panic!("no line for {}", cmd.name));
            if cmd.accepts_args {
                assert!(
                    line.contains("<args>"),
                    "{} accepts args but /help line lacks <args>: {line}",
                    cmd.name
                );
            } else {
                assert!(
                    !line.contains("<args>"),
                    "{} does not accept args but /help line has <args>: {line}",
                    cmd.name
                );
            }
        }
    }

    #[test]
    fn handle_commands_marks_pass_through_distinctly() {
        let out = handle_commands();
        assert!(out.starts_with("Slash commands:"));
        for cmd in GATEWAY_COMMANDS {
            let line = out
                .lines()
                .find(|l| l.contains(&format!("/{}", cmd.name)) && l.contains(cmd.description))
                .unwrap_or_else(|| panic!("/commands missing entry for {}", cmd.name));
            if cmd.pass_through {
                assert!(
                    line.ends_with("(sent to agent)"),
                    "{} is pass_through but line lacks (sent to agent) suffix: {line}",
                    cmd.name
                );
            } else {
                assert!(
                    !line.contains("(sent to agent)"),
                    "{} is not pass_through but line marks it as such: {line}",
                    cmd.name
                );
            }
        }
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

    #[test]
    fn is_dream_command_matches_aliases() {
        assert!(is_dream_command("/dream"));
        assert!(is_dream_command("/Dream"));
        assert!(is_dream_command("/dream@MyBorgBot"));
        assert!(!is_dream_command("/status"));
        assert!(!is_dream_command("dream"));
    }

    #[tokio::test]
    async fn cancel_command_intercepted_via_session_executor() {
        // Simulates the handler.rs interception path: a /cancel message
        // routes to `SessionExecutor::cancel(session_id)`. Real failure mode:
        // a regression that fails to call `cancel` on the executor would let
        // a runaway turn keep emitting tokens after the user typed `/cancel`.
        use crate::session_executor::{InboundTurn, SessionExecutor};
        use async_trait::async_trait;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        struct StubExec(Arc<AtomicBool>);

        #[async_trait]
        impl SessionExecutor for StubExec {
            async fn start_turn(
                &self,
                _t: InboundTurn,
            ) -> anyhow::Result<(
                tokio::sync::mpsc::Receiver<cortex::agent::AgentEvent>,
                tokio_util::sync::CancellationToken,
            )> {
                anyhow::bail!("not used in this test")
            }
            async fn cancel(&self, _id: &str) -> bool {
                self.0.store(true, Ordering::SeqCst);
                true
            }
            async fn cancel_all(&self) -> usize {
                0
            }
            async fn active_turns(&self) -> usize {
                0
            }
        }

        let flag = Arc::new(AtomicBool::new(false));
        let exec: Arc<dyn SessionExecutor> = Arc::new(StubExec(flag.clone()));
        assert!(is_cancel_command("/cancel"));
        assert!(exec.cancel("test-session").await);
        assert!(flag.load(Ordering::SeqCst));
    }

    async fn gateway_test_db() -> Database {
        Database::test_db().await
    }

    #[tokio::test]
    async fn handle_memory_lists_inventory_across_stores() {
        let _guard = crate::test_env::borg_home_guard();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_HOME", tmp.path());

        // Fresh home: the empty-inventory message, not a zero-item listing.
        let empty = handle_memory(&Config::default()).await;

        let db = cortex::db::Database::open().await.unwrap();
        // 2048 bytes → the listing must report exactly 2.0 KB.
        db.upsert_memory_entry("global", "shared-note", &"x".repeat(2048))
            .await
            .unwrap();
        let memory_dir = tmp.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(memory_dir.join("memory.md"), "knowledge file").unwrap();

        let out = handle_memory(&Config::default()).await;
        std::env::remove_var("BORG_HOME");

        assert_eq!(empty, "No memory entries or knowledge files.");
        assert!(out.contains("Memory inventory"));
        assert!(
            out.contains("[long-term:global] shared-note (2.0 KB)"),
            "{out}"
        );
        assert!(out.contains("[knowledge:global] global/memory.md"));
    }

    #[test]
    fn help_lists_dream_command() {
        let out = handle_help();
        assert!(out.contains("/dream"), "{out}");
    }

    #[tokio::test]
    async fn handle_card_rejects_out_argument() {
        let db = gateway_test_db().await;
        let out = handle_card(&db, "--out /tmp/x").await;
        assert!(
            out.starts_with("/card on messaging channels does not accept arguments"),
            "got: {out}"
        );
    }

    #[tokio::test]
    async fn handle_card_inline_renders_ascii() {
        let db = gateway_test_db().await;
        let out = handle_card(&db, "").await;
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
    #[tokio::test]
    async fn evolution_handlers_wrap_success_output_in_code_fence() {
        let db = gateway_test_db().await;

        for out in [
            handle_card(&db, "").await,
            handle_xp(&db).await,
            handle_evolution(&db).await,
        ] {
            assert!(out.starts_with("```\n"), "expected opening fence: {out}");
            assert!(out.ends_with("\n```"), "expected closing fence: {out}");
        }
    }

    #[tokio::test]
    async fn handle_card_error_string_is_unwrapped() {
        let db = gateway_test_db().await;
        let out = handle_card(&db, "--out /tmp/x").await;
        assert!(!out.starts_with("```"), "error should not be fenced: {out}");
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

    #[tokio::test]
    async fn handle_voice_join_returns_stub_status() {
        // The slash surface accepts `/voice join`, `/voice channel <name>`,
        // `/voice leave` and returns a clear "not wired" message — keeps users
        // from wondering whether they typed the command wrong while the audio
        // bridging is genuinely deferred.
        let db = gateway_test_db().await;
        let config = Config::default();
        let cases = ["join", "join General", "leave", "channel Lobby"];
        for input in cases {
            let resp = handle_voice(&db, &config, "discord", Some("999"), input).await;
            // Default-feature build: returns "not active in this build"
            // explanation. Feature-on build with the songbird stack: returns
            // either a usage hint or the actual operation outcome. Both
            // shapes mention `voice` somewhere so the test stays meaningful
            // across feature configurations.
            assert!(
                !resp.is_empty(),
                "input {input:?}: response must not be empty"
            );
            assert!(
                resp.to_lowercase().contains("voice")
                    || resp.to_lowercase().contains("usage")
                    || resp.to_lowercase().contains("join")
                    || resp.to_lowercase().contains("leave"),
                "input {input:?}: response should reference the voice command surface, got: {resp}"
            );
        }
    }

    #[tokio::test]
    async fn handle_voice_join_works_from_any_channel() {
        // The stub path runs BEFORE the Telegram-only guard, so users
        // attempting /voice join from any channel see the same message.
        // (When the songbird stack lands, this same dispatch routes to
        // Discord-specific join logic.)
        let db = gateway_test_db().await;
        let config = Config::default();
        let from_telegram = handle_voice(&db, &config, "telegram", Some("1"), "join").await;
        let from_slack = handle_voice(&db, &config, "slack", Some("C1"), "join").await;
        assert_eq!(from_telegram, from_slack);
    }

    #[tokio::test]
    async fn handle_voice_uses_default_mode_from_config() {
        let db = gateway_test_db().await;
        let config = Config::default();
        let out = handle_voice(&db, &config, "telegram", Some("42"), "status").await;
        assert_eq!(
            out,
            "Voice mode: Off (text only). Voice replies require `tts.enabled = true`."
        );
    }

    #[tokio::test]
    async fn handle_voice_rejects_enable_when_tts_is_unavailable() {
        let db = gateway_test_db().await;
        let config = Config::default();

        let on = handle_voice(&db, &config, "telegram", Some("42"), "on").await;
        assert_eq!(on, "Voice replies require `tts.enabled = true`.");
        let key = crate::voice::voice_mode_setting_key("telegram", "42");
        assert_eq!(db.get_setting(&key).await.unwrap().as_deref(), None);
    }

    #[tokio::test]
    async fn handle_voice_can_enable_all_and_voice_only_modes() {
        let db = gateway_test_db().await;
        let mut config = Config::default();
        config.tts.enabled = true;
        config.tts.models = vec![cortex::config::TtsModelConfig {
            provider: "openai".to_string(),
            model: None,
            voice: None,
            timeout_ms: None,
            api_key_env: Some("BORG_TEST_TTS_KEY".to_string()),
            base_url: None,
            language: None,
            sample_rate_hz: None,
            bit_rate: None,
            max_text_length: None,
            name: None,
            command: None,
            output_extension: None,
            xai_auto_speech_tags: None,
        }];
        std::env::set_var("BORG_TEST_TTS_KEY", "test-key");

        let on = handle_voice(&db, &config, "telegram", Some("42"), "on").await;
        assert_eq!(on, "Voice mode: On (voice reply to voice messages)");
        let status = handle_voice(&db, &config, "telegram", Some("42"), "status").await;
        assert_eq!(status, "Voice mode: On (voice reply to voice messages)");

        let tts = handle_voice(&db, &config, "telegram", Some("42"), "tts").await;
        assert_eq!(tts, "Voice mode: TTS (voice reply to all messages)");
        let key = crate::voice::voice_mode_setting_key("telegram", "42");
        assert_eq!(db.get_setting(&key).await.unwrap().as_deref(), Some("all"));

        std::env::remove_var("BORG_TEST_TTS_KEY");
    }

    #[tokio::test]
    async fn handle_voice_rejects_non_telegram_channels() {
        let db = gateway_test_db().await;
        let config = Config::default();
        let out = handle_voice(&db, &config, "slack", Some("C123"), "on").await;
        assert_eq!(
            out,
            "Voice mode is currently implemented for Telegram only."
        );
    }

    #[tokio::test]
    async fn try_handle_command_dispatches_and_passes_through() {
        let db = gateway_test_db().await;
        let config = Config::default();

        let resp = try_handle_command(
            "/undo", &db, &config, "telegram", None, "skey", "sid-undo", "sender",
        )
        .await;
        assert_eq!(resp.as_deref(), Some("Nothing to undo."));

        // Non-commands and out-of-band/pass-through commands must fall
        // through to the agent (or the async interceptors) untouched.
        for passthrough in [
            "hello world",
            "/skill weather",
            "/cancel",
            "/poke",
            "/dream",
        ] {
            let resp = try_handle_command(
                passthrough,
                &db,
                &config,
                "telegram",
                None,
                "skey",
                "sid-undo",
                "sender",
            )
            .await;
            assert!(resp.is_none(), "{passthrough} must not be intercepted");
        }
    }

    #[tokio::test]
    async fn handle_status_reports_short_id_and_message_count() {
        let db = gateway_test_db().await;
        let sid = "statussession-1";
        for text in ["one", "two"] {
            db.insert_message(sid, "user", Some(text), None, None, None, None)
                .await
                .unwrap();
        }
        assert_eq!(
            handle_status(&db, sid).await,
            "Session: statusse...\nMessages: 2"
        );
        // Session ids shorter than 8 chars fall back to the full id.
        assert_eq!(
            handle_status(&db, "abc").await,
            "Session: abc...\nMessages: 0"
        );
    }

    #[tokio::test]
    async fn handle_new_rotates_channel_session() {
        let db = gateway_test_db().await;
        assert_eq!(
            handle_new(&db, "tg", "sender1").await,
            "No existing session found to replace."
        );

        let old = db.resolve_channel_session("tg", "sender1").await.unwrap();
        let out = handle_new(&db, "tg", "sender1").await;
        assert!(out.starts_with("New session started ("), "{out}");
        let new = db
            .lookup_channel_session("tg", "sender1")
            .await
            .unwrap()
            .expect("session row still exists");
        assert_ne!(new, old, "session id must actually rotate");
        assert!(
            out.contains(&new[..8]),
            "response must echo the new session's short id: {out}"
        );
    }

    #[tokio::test]
    async fn handle_reset_deletes_and_counts_messages() {
        let db = gateway_test_db().await;
        let sid = "reset-session";
        for text in ["a", "b"] {
            db.insert_message(sid, "user", Some(text), None, None, None, None)
                .await
                .unwrap();
        }
        assert_eq!(
            handle_reset(&db, sid).await,
            "Cleared 2 messages from current session."
        );
        assert_eq!(db.count_session_messages(sid).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn handle_usage_reports_counts_and_budget_pct() {
        let db = gateway_test_db().await;
        let sid = "usage-session";
        db.insert_message(sid, "user", Some("q"), None, None, None, None)
            .await
            .unwrap();
        db.log_token_usage(30, 20, 50, "test", "test-model", None)
            .await
            .unwrap();

        let mut config = Config::default();
        // No budget configured → no budget line.
        config.budget.monthly_token_limit = 0;
        assert_eq!(
            handle_usage(&db, &config, sid).await,
            "Session messages: 1\nMonthly tokens: 50"
        );

        // 50 of 200 tokens = exactly 25.0%.
        config.budget.monthly_token_limit = 200;
        assert_eq!(
            handle_usage(&db, &config, sid).await,
            "Session messages: 1\nMonthly tokens: 50\nBudget: 25.0% of 200 used"
        );
    }

    #[test]
    fn handle_skills_lists_bundled_skills() {
        // Built-in skills are compiled in, so the default config always
        // yields a non-empty listing with one line per skill.
        let out = handle_skills(&Config::default());
        assert!(out.starts_with("Skills ("), "{out}");
        assert!(
            out.lines().count() > 1,
            "expected at least one skill line: {out}"
        );
    }

    #[tokio::test]
    async fn handle_whoami_reports_identity_and_pairing() {
        let db = gateway_test_db().await;
        assert_eq!(
            handle_whoami("u42", "telegram", &db).await,
            "Sender: u42\nChannel: telegram\nPairing: Not approved"
        );
    }

    #[test]
    fn format_doctor_report_summarizes_and_lists_problems() {
        use cortex::doctor::{DiagnosticCheck, DiagnosticReport};

        // All pass → summary plus the all-clear line.
        let report = DiagnosticReport {
            checks: vec![
                DiagnosticCheck::pass("cat", "alpha"),
                DiagnosticCheck::pass("cat", "beta"),
            ],
        };
        assert_eq!(
            format_doctor_report(&report),
            "Diagnostics: 2 passed, 0 warning(s), 0 failed\n  All checks passed."
        );

        // Warn only → the warning line, and no premature all-clear.
        let report = DiagnosticReport {
            checks: vec![
                DiagnosticCheck::pass("cat", "alpha"),
                DiagnosticCheck::warn("cat", "disk", "nearly full"),
            ],
        };
        assert_eq!(
            format_doctor_report(&report),
            "Diagnostics: 1 passed, 1 warning(s), 0 failed\n  ⚠ disk — nearly full"
        );

        // Fail only → the failure line, and no all-clear.
        let report = DiagnosticReport {
            checks: vec![DiagnosticCheck::fail("cat", "db", "cannot open")],
        };
        assert_eq!(
            format_doctor_report(&report),
            "Diagnostics: 0 passed, 0 warning(s), 1 failed\n  ✗ db — cannot open"
        );

        // Warn + fail together → both listed, still no all-clear.
        let report = DiagnosticReport {
            checks: vec![
                DiagnosticCheck::warn("cat", "disk", "nearly full"),
                DiagnosticCheck::fail("cat", "db", "cannot open"),
            ],
        };
        let out = format_doctor_report(&report);
        assert!(out.contains("⚠ disk — nearly full"), "{out}");
        assert!(out.contains("✗ db — cannot open"), "{out}");
        assert!(!out.contains("All checks passed."), "{out}");
    }

    #[tokio::test]
    async fn handle_mode_shows_sets_and_validates() {
        let db = gateway_test_db().await;
        let config = Config::default();

        let show = handle_mode(&db, &config, "").await;
        assert!(show.starts_with("Collaboration mode: "), "{show}");
        assert!(show.contains("Usage: /mode <execute|plan>"), "{show}");

        assert_eq!(
            handle_mode(&db, &config, "plan").await,
            "Switched to plan mode for all conversations."
        );
        // The setting every agent reads, not a per-chat key nothing reads.
        assert_eq!(
            db.get_setting(MODE_KEY).await.unwrap().as_deref(),
            Some("plan")
        );

        for bad in ["yolo", "default"] {
            assert_eq!(
                handle_mode(&db, &config, bad).await,
                "Invalid mode. Choose: execute, plan"
            );
        }
        assert_eq!(
            db.get_setting(MODE_KEY).await.unwrap().as_deref(),
            Some("plan")
        );
    }

    #[tokio::test]
    async fn handle_voice_off_persists_and_reports() {
        let db = gateway_test_db().await;
        let config = Config::default();
        // Disabling voice needs no TTS stack — it must always work.
        let out = handle_voice(&db, &config, "telegram", Some("77"), "off").await;
        assert_eq!(out, "Voice mode: Off (text only)");
        let key = crate::voice::voice_mode_setting_key("telegram", "77");
        assert_eq!(db.get_setting(&key).await.unwrap().as_deref(), Some("off"));
    }

    #[tokio::test]
    async fn handle_voice_channel_subcommands_name_the_right_action() {
        // Default build: the voice-channel path must (a) intercept before
        // the Telegram-only guard, and (b) name the specific action the
        // user asked for in the stub message.
        let db = gateway_test_db().await;
        let config = Config::default();
        let cases = [
            ("leave", "`leave`"),
            ("channel 1:2", "`switch`"),
            ("join", "`join`"),
            ("join 1:2", "`join`"),
        ];
        for (input, action) in cases {
            let resp = handle_voice(&db, &config, "discord", Some("999"), input).await;
            if resp.contains("not active in this build") {
                assert!(
                    resp.contains(action),
                    "input {input:?}: stub must name {action}, got: {resp}"
                );
                assert_ne!(
                    resp, "Voice mode is currently implemented for Telegram only.",
                    "voice-channel subcommand {input:?} must bypass the Telegram guard"
                );
            }
        }
    }

    #[tokio::test]
    async fn handle_plan_sets_mode_and_passes_message_through() {
        let db = gateway_test_db().await;

        // Bare /plan: respond directly, mode persisted.
        let out = handle_plan(&db, "").await;
        assert_eq!(
            out.as_deref(),
            Some("Entered plan mode. Send your message to begin planning.")
        );
        assert_eq!(
            db.get_setting(MODE_KEY).await.unwrap().as_deref(),
            Some("plan")
        );

        // /plan <message>: mode persisted, message passes through (None).
        db.set_setting(MODE_KEY, "execute").await.unwrap();
        let out = handle_plan(&db, "design the schema").await;
        assert!(out.is_none(), "with a message /plan must pass through");
        assert_eq!(
            db.get_setting(MODE_KEY).await.unwrap().as_deref(),
            Some("plan")
        );
    }

    #[tokio::test]
    async fn handle_compact_validates_args_and_trims() {
        let db = gateway_test_db().await;
        let sid = "compact-session";
        for i in 0..8 {
            db.insert_message(sid, "user", Some(&format!("m{i}")), None, None, None, None)
                .await
                .unwrap();
        }

        let usage = "Usage: /compact [keep_count] (positive integer, default 20)";
        assert_eq!(handle_compact(&db, sid, "abc").await, usage);
        assert_eq!(handle_compact(&db, sid, "0").await, usage);

        assert_eq!(
            handle_compact(&db, sid, "5").await,
            "Compacted: removed 3 older messages, kept last 5."
        );
        assert_eq!(
            handle_compact(&db, sid, "").await,
            "Nothing to compact (≤20 messages)."
        );
    }

    #[tokio::test]
    async fn handle_undo_on_empty_session_says_so() {
        let db = gateway_test_db().await;
        assert_eq!(handle_undo(&db, "undo-empty").await, "Nothing to undo.");
    }

    #[tokio::test]
    async fn handle_history_validates_args_and_lists_tail() {
        let db = gateway_test_db().await;
        let sid = "history-session";
        // History reads session lineage, which requires the sessions row.
        db.upsert_session(sid, 0, 0, 0, "m", "t").await.unwrap();
        for (role, text) in [
            ("user", "first message"),
            ("assistant", "second message"),
            ("user", "third message"),
        ] {
            db.insert_message(sid, role, Some(text), None, None, None, None)
                .await
                .unwrap();
        }

        let usage = "Usage: /history [count] (1-50, default 10)";
        // Invalid counts against a *populated* session, so a broken guard
        // produces a listing instead of the usage hint.
        for bad in ["abc", "0", "51"] {
            assert_eq!(handle_history(&db, sid, bad).await, usage, "arg {bad:?}");
        }
        // 50 is the inclusive upper bound.
        assert!(handle_history(&db, sid, "50")
            .await
            .starts_with("Last 3 messages:"));

        assert_eq!(
            handle_history(&db, "history-empty", "").await,
            "No messages in current session."
        );

        let out = handle_history(&db, sid, "2").await;
        assert!(out.starts_with("Last 2 messages:"), "{out}");
        assert!(out.contains("[assistant] second message"), "{out}");
        assert!(out.contains("[user] third message"), "{out}");
        assert!(
            !out.contains("first message"),
            "only the tail must be shown: {out}"
        );
    }

    #[tokio::test]
    async fn handle_history_truncates_only_past_200_chars() {
        let db = gateway_test_db().await;
        let sid = "history-trunc";
        db.upsert_session(sid, 0, 0, 0, "m", "t").await.unwrap();
        db.insert_message(sid, "user", Some(&"x".repeat(250)), None, None, None, None)
            .await
            .unwrap();
        db.insert_message(sid, "user", Some(&"y".repeat(200)), None, None, None, None)
            .await
            .unwrap();

        let out = handle_history(&db, sid, "").await;
        // 250 chars → cut at 200 with an ellipsis.
        assert!(
            out.contains(&format!("{}…", "x".repeat(200))),
            "long message not truncated"
        );
        assert!(
            !out.contains(&"x".repeat(201)),
            "truncation must cut at 200 chars"
        );
        // Exactly 200 chars → untouched, no ellipsis.
        assert!(out.contains(&"y".repeat(200)));
        assert!(
            !out.contains(&format!("{}…", "y".repeat(200))),
            "boundary-length message must not gain an ellipsis"
        );
    }

    #[tokio::test]
    async fn handle_schedule_lists_tasks_with_next_run() {
        let db = gateway_test_db().await;
        // Migrations seed default maintenance tasks — clear them so both
        // the empty branch and the single-task listing are deterministic.
        for task in db.list_tasks().await.unwrap() {
            assert!(db.delete_task(&task.id).await.unwrap());
        }
        assert_eq!(handle_schedule(&db).await, "No scheduled tasks.");

        db.create_task(&cortex::db::NewTask {
            id: "t1",
            name: "nightly-backup",
            prompt: "run backup",
            schedule_type: "cron",
            schedule_expr: "0 0 * * *",
            timezone: "UTC",
            next_run: Some(1_767_225_600), // 2026-01-01T00:00:00Z
            max_retries: None,
            timeout_ms: None,
            delivery_channel: None,
            delivery_target: None,
            allowed_tools: None,
            task_type: "prompt",
            pre_run_command: None,
            pre_run_cwd: None,
            pre_run_timeout_ms: None,
            min_interval_secs: None,
            continuity: false,
        })
        .await
        .unwrap();

        let out = handle_schedule(&db).await;
        assert!(out.starts_with("Scheduled tasks (1):"), "{out}");
        assert!(
            out.contains("nightly-backup (0 0 * * *) [active] next: 2026-01-01 00:00 UTC"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn handle_settings_lists_gets_sets_and_refuses() {
        let db = gateway_test_db().await;
        let config = Config::default();

        // No args → current-settings listing with usage.
        let listing = handle_settings(&db, &config, "").await;
        assert!(listing.starts_with("Current settings:"), "{listing}");
        assert!(
            listing.contains(&format!("  model = {}", config.llm.model)),
            "{listing}"
        );
        assert!(
            listing.contains("Usage: /settings <key> <value>"),
            "{listing}"
        );

        // Single-key reads fall back to compiled config values once no DB
        // override exists (test_db seeding may pre-populate some keys).
        for key in ["model", "temperature", "max_tokens"] {
            db.delete_setting(key).await.unwrap();
        }
        assert_eq!(
            handle_settings(&db, &config, "model").await,
            format!("model = {}", config.llm.model)
        );
        assert_eq!(
            handle_settings(&db, &config, "temperature").await,
            format!("temperature = {}", config.llm.temperature)
        );
        assert_eq!(
            handle_settings(&db, &config, "max_tokens").await,
            format!("max_tokens = {}", config.llm.max_tokens)
        );
        assert_eq!(
            handle_settings(&db, &config, "no.such.setting").await,
            "Unknown or read-only setting: no.such.setting"
        );

        // Set path: validate, persist, confirm.
        let set = handle_settings(&db, &config, "temperature 0.4").await;
        assert!(set.starts_with("Updated:"), "{set}");
        assert_eq!(
            db.get_setting("temperature").await.unwrap().as_deref(),
            Some("0.4")
        );

        // Security-critical keys are refused from messaging channels.
        assert_eq!(
            handle_settings(&db, &config, "security.blocked_paths x").await,
            "Setting 'security.blocked_paths' cannot be changed from a messaging channel."
        );
    }

    #[tokio::test]
    async fn handle_settings_reads_provider_key() {
        let db = gateway_test_db().await;
        db.delete_setting("provider").await.unwrap();

        // Unset provider reads back as the auto-detect placeholder, not the
        // "unknown setting" fallback — provider is a first-class readable key.
        let config = Config::default();
        assert_eq!(
            handle_settings(&db, &config, "provider").await,
            "provider = (auto-detect)"
        );

        let mut config = Config::default();
        config.llm.provider = Some("openrouter".to_string());
        assert_eq!(
            handle_settings(&db, &config, "provider").await,
            "provider = openrouter"
        );
    }

    #[tokio::test]
    async fn handle_pairing_reports_each_pairing_state() {
        let db = gateway_test_db().await;

        // Unknown sender: no request on file yet.
        assert_eq!(
            handle_pairing(&db, "telegram", "stranger").await,
            "No pairing request found. Send a message to start the pairing process."
        );

        // Challenge issued → pending, waiting on the owner.
        let mut config = Config::default();
        config.gateway.dm_policy = cortex::pairing::DmPolicy::Pairing;
        let code =
            match cortex::pairing::check_sender_access(&db, &config, "telegram", "pairee", None)
                .await
                .unwrap()
            {
                cortex::pairing::AccessCheckResult::Challenge { code, .. } => code,
                other => panic!("expected Challenge, got {other:?}"),
            };
        let out = handle_pairing(&db, "telegram", "pairee").await;
        assert!(out.contains("pending pairing request"), "{out}");

        // Approved → the approved confirmation, naming the channel.
        db.approve_pairing("telegram", &code).await.unwrap();
        assert_eq!(
            handle_pairing(&db, "telegram", "pairee").await,
            "You are approved on channel 'telegram'."
        );
    }

    #[cfg(feature = "discord-voice-channels")]
    mod voice_channel_dispatch {
        use super::*;
        use crate::discord::voice_channels::{self, VoiceCommand};
        use tokio::sync::mpsc;

        /// Stub manager: services the command channel deterministically so
        /// the dispatch grammar is testable without a Discord connection.
        fn spawn_stub_manager() -> voice_channels::VoiceManagerHandle {
            let (tx, mut rx) = mpsc::channel::<VoiceCommand>(8);
            tokio::spawn(async move {
                while let Some(cmd) = rx.recv().await {
                    match cmd {
                        VoiceCommand::Join { reply, .. } => {
                            let _ = reply.send(Ok(()));
                        }
                        VoiceCommand::Leave { reply, .. } => {
                            let _ = reply.send(Ok(()));
                        }
                        VoiceCommand::ResolveUserChannel {
                            guild_id,
                            user_id,
                            reply,
                        } => {
                            let known = guild_id == 1 && user_id == 42;
                            let _ = reply.send(known.then_some(777));
                        }
                        // Dropping the reply channel fails the caller —
                        // these commands are not exercised here.
                        _ => {}
                    }
                }
            });
            voice_channels::handle_for_tests(tx)
        }

        #[tokio::test]
        async fn dispatch_grammar_routes_each_action() {
            let handle = spawn_stub_manager();
            let cases: &[(&str, &str)] = &[
                // Explicit-id join.
                ("join 1:2", "Joined Discord voice channel 2 in guild 1."),
                // Zero ids are rejected as targets → usage hint.
                ("join 0:5", "Usage: /voice join"),
                // User-resolve join via the voice-state cache.
                (
                    "join 1:user:42",
                    "Joined user 42's voice channel (777) in guild 1.",
                ),
                // Unknown user → explanatory message, not a join.
                ("join 2:user:9", "Don't know what channel user 9 is in."),
                ("leave 3", "Left Discord voice channel in guild 3."),
                ("leave", "Usage: /voice leave <guild_id>"),
                ("channel 4:6", "Switched to voice channel 6."),
                (
                    "channel nope",
                    "Usage: /voice channel <guild_id>:<channel_id>",
                ),
                ("", "Usage: /voice [join|leave|channel]"),
            ];
            for (input, want) in cases {
                let out =
                    dispatch_voice_channel_via_handle(&handle, Some("text-chan"), input).await;
                assert!(
                    out.starts_with(want),
                    "input {input:?}: expected prefix {want:?}, got: {out}"
                );
            }
        }
    }
}
