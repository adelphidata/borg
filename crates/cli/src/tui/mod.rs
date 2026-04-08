mod app;
mod colors;
mod command_popup;
mod commands;
mod composer;
mod external_editor;
mod file_popup;
mod goodbye;
mod history;
mod layout;
mod markdown;
pub(crate) mod migrate_popup;
mod pairing_popup;
mod paste_burst;
mod plan_overlay;
mod plugins_popup;
mod popup_utils;
mod projects_popup;
mod schedule_popup;
mod sessions_popup;
mod settings_popup;
mod shimmer;
mod status_popup;
pub(crate) mod theme;
mod tool_display;

use std::io::stdout;
use std::panic;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event, EventStream};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;

// ============================================================================
// IMPORTANT: Native text selection MUST work in this TUI. DO NOT regress.
// ============================================================================
// Users expect click+drag selection (for copy/paste) to work exactly like any
// normal terminal, with no modifier keys required. Any form of mouse tracking
// reported to the application breaks this, because the terminal hands click
// events to us instead of engaging its own selection handler.
//
// Strategy: we enable ONLY xterm "Alternate Scroll Mode" (?1007h). In alt
// screen, the terminal itself translates mouse-wheel events into CUR_UP /
// CUR_DOWN key sequences that arrive as normal KeyCode::Up / KeyCode::Down.
// We never enable any mouse tracking mode, so click+drag text selection stays
// handled entirely by the terminal — exactly like in `less`, `vim`, etc.
//
// FORBIDDEN — DO NOT ADD any of these (each one will regress text selection):
//   - ?1000h  Normal button tracking — clicks go to app, breaks selection.
//   - ?1002h  Button-event (drag) tracking — same, plus eats drag events.
//   - ?1003h  Any-event tracking — captures every mouse movement.
//   - ?1006h  SGR extended coordinates — only meaningful with the above.
//   - crossterm::event::EnableMouseCapture — it turns on ?1000h + ?1002h + ?1003h + ?1006h.
//
// Wheel-sourced arrow keys are routed to transcript scroll vs composer history
// in App::handle_key using the existing scroll_offset state as disambiguator.
//
// This has regressed multiple times. Source-string guard tests in this module
// and in app.rs will fail the build if any forbidden mode is reintroduced.
// See CLAUDE.md "Mouse Interaction" for the full rationale.
// Reference implementation: reference/codex/codex-rs/tui/src/tui.rs
// ============================================================================

/// Enable xterm Alternate Scroll Mode (`?1007h`).
///
/// When active inside the alternate screen, the terminal translates
/// mouse-wheel events into cursor-up / cursor-down key sequences. No mouse
/// tracking is enabled, so native click+drag text selection is untouched.
struct EnableAlternateScroll;

impl crossterm::Command for EnableAlternateScroll {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        // ?1007h — xterm Alternate Scroll Mode. DO NOT add any other mode here.
        f.write_str("\x1b[?1007h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        // Windows consoles deliver wheel events through input records without
        // needing an xterm-style mode switch, and there is no native equivalent
        // to ?1007h. No-op is correct here.
        Ok(())
    }
}

/// Disable xterm Alternate Scroll Mode (`?1007l`).
struct DisableAlternateScroll;

impl crossterm::Command for DisableAlternateScroll {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str("\x1b[?1007l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Ok(())
    }
}
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use borg_core::agent::{Agent, AgentEvent};
use borg_core::config::{CollaborationMode, Config};
use borg_core::telemetry::BorgMetrics;
use borg_heartbeat::scheduler::{HeartbeatEvent, HeartbeatResult, HeartbeatScheduler};

use app::{App, AppAction};
use history::HistoryCell;

/// Spawn the gateway server and/or iMessage monitor.
/// Always starts the gateway (native channels are detected via credentials inside GatewayServer).
fn spawn_gateway(config: &Config, shutdown: CancellationToken, metrics: BorgMetrics) -> bool {
    let gw_config = config.clone();
    #[allow(clippy::redundant_clone)]
    let gw_shutdown = shutdown.clone();
    let gw_metrics = metrics;
    tokio::spawn(async move {
        match borg_gateway::GatewayServer::new(gw_config, gw_shutdown, gw_metrics, None) {
            Ok(server) => {
                if let Err(e) = server.run().await {
                    let msg = e.to_string();
                    if msg.contains("address already in use") || msg.contains("AddrInUse") {
                        tracing::warn!("Gateway: {e}");
                    } else {
                        tracing::error!("Gateway exited with error: {e}");
                    }
                }
            }
            Err(e) => tracing::error!("Failed to initialize gateway: {e}"),
        }
    });

    // Start native iMessage monitor if channel is installed (mirrors service.rs)
    #[cfg(target_os = "macos")]
    if let Ok(data_dir) = Config::data_dir() {
        let imessage_dir = data_dir.join("channels/imessage");
        if imessage_dir.join("channel.toml").exists() {
            let probe = borg_gateway::imessage::probe::probe_imessage();
            match probe.status {
                borg_gateway::imessage::probe::ProbeStatus::Ok => {
                    let im_config = config.clone();
                    let im_shutdown = shutdown;
                    tokio::spawn(async move {
                        match borg_gateway::imessage::start_imessage_monitor(im_config, im_shutdown)
                            .await
                        {
                            Ok(_handle) => tracing::info!("iMessage monitor started"),
                            Err(e) => tracing::warn!("iMessage monitor failed: {e}"),
                        }
                    });
                }
                borg_gateway::imessage::probe::ProbeStatus::NoDiskAccess => {
                    tracing::warn!("iMessage: Full Disk Access required (System Settings > Privacy & Security). Skipping monitor.");
                }
                other => {
                    tracing::warn!("iMessage probe: {other}. Skipping monitor.");
                }
            }
        }
    }

    true
}

/// Restart the gateway: if a daemon owns the port, signal it via HTTP;
/// otherwise cancel the local token and respawn in-process.
async fn restart_gateway(gateway_shutdown: &Arc<Mutex<CancellationToken>>) -> String {
    // Try to signal a running daemon's gateway first
    let config = match Config::load_from_db() {
        Ok(c) => c,
        Err(e) => return format!("Gateway restart failed: could not reload config: {e}"),
    };
    let addr = format!("{}:{}", config.gateway.host, config.gateway.port);
    let url = format!("http://{addr}/internal/restart");

    let daemon_signalled = async {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .ok()?;
        let resp = client.post(&url).send().await.ok()?;
        resp.status().is_success().then_some(())
    }
    .await
    .is_some();

    if daemon_signalled {
        return "Gateway restarted.".to_string();
    }

    // No daemon running — restart the in-process gateway
    let shutdown = gateway_shutdown.try_lock();
    let Ok(mut shutdown) = shutdown else {
        return "Gateway restart failed: could not acquire lock".to_string();
    };

    shutdown.cancel();

    let new_token = CancellationToken::new();
    *shutdown = new_token.clone();

    spawn_gateway(&config, new_token, BorgMetrics::noop());
    "Gateway restarted.".to_string()
}

/// Guard that restores terminal state on drop (both normal exit and early error return).
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = stdout().execute(DisableBracketedPaste);
        let _ = stdout().execute(DisableAlternateScroll);
        let _ = disable_raw_mode();
        let _ = stdout().execute(LeaveAlternateScreen);
    }
}

/// Information needed to print a "resume this session" hint after the TUI exits.
pub struct ResumeHint {
    /// Short (8-char) session ID for compact display.
    pub short_id: String,
    /// Session title for display.
    pub title: String,
}

pub async fn run(resume: Option<String>) -> Result<Option<ResumeHint>> {
    let config = Config::load_from_db()?;
    let metrics = BorgMetrics::from_config(&config);
    let mut agent = Agent::new(config.clone(), metrics.clone())?;

    // Start config hot reload watcher (polls DB for changes)
    let _config_watcher = match borg_core::config_watcher::ConfigWatcher::start(config.clone()) {
        Ok(watcher) => {
            agent.set_config_watcher(watcher.subscribe());
            Some(watcher)
        }
        Err(e) => {
            tracing::warn!("Config watcher failed to start: {e}");
            None
        }
    };

    // Register vitals hook for passive health tracking
    if let Ok(vitals_hook) = borg_core::vitals::VitalsHook::new() {
        agent.hook_registry_mut().register(Box::new(vitals_hook));
    }

    // Register activity log hook for structured event logging
    if let Ok(activity_hook) = borg_core::activity_log::ActivityHook::new() {
        agent.hook_registry_mut().register(Box::new(activity_hook));
    }

    // Register bond hook for trust tracking (after vitals so events are available)
    if let Ok(bond_hook) = borg_core::bond::BondHook::new() {
        agent.hook_registry_mut().register(Box::new(bond_hook));
    }

    // Register evolution hook for XP tracking and specialization
    if config.evolution.enabled {
        if let Ok(evolution_hook) = borg_core::evolution::EvolutionHook::new() {
            agent.hook_registry_mut().register(Box::new(evolution_hook));
        }
    }

    // Resume logic: explicit --resume <id> picks a specific session; otherwise
    // auto-resume the most recently updated non-empty session (existing behavior).
    let mut resumed_info: Option<(String, usize)> = None;
    if let Some(prefix) = resume.as_deref() {
        match borg_core::session::resolve_session_id(prefix) {
            Ok(meta) => {
                let title = meta.title.clone();
                let count = meta.message_count;
                if let Err(e) = agent.load_session(&meta.id) {
                    eprintln!("borg: failed to load session '{}': {e}", meta.id);
                    return Ok(None);
                }
                resumed_info = Some((title, count));
            }
            Err(e) => {
                eprintln!("borg: {e}");
                return Ok(None);
            }
        }
    } else if let Ok(Some(session)) = borg_core::session::load_last_session() {
        if !session.messages.is_empty() {
            let title = session.meta.title.clone();
            let count = session.meta.message_count;
            if agent.load_session(&session.meta.id).is_ok() {
                resumed_info = Some((title, count));
            }
        }
    }

    let agent = Arc::new(Mutex::new(agent));

    // Start heartbeat scheduler unless daemon is already running its own.
    // The daemon holds a lock in SQLite that is refreshed every 60s and goes
    // stale after 300s. If a live daemon is detected, skip the TUI scheduler
    // to avoid duplicate heartbeat firings.
    let heartbeat_cancel = CancellationToken::new();
    let _heartbeat_guard = heartbeat_cancel.clone().drop_guard();
    let daemon_running = borg_core::db::Database::open()
        .map(|db| db.is_daemon_lock_held())
        .unwrap_or(false);
    let (heartbeat_rx, heartbeat_event_tx, poke_tx) = if !daemon_running {
        let (hb_tx, hb_rx) = mpsc::channel::<HeartbeatEvent>(32);
        // Keep poke_tx alive so the poke_rx channel doesn't close immediately
        let (poke_tx, poke_rx) = mpsc::channel::<()>(8);
        let poke_tx_for_app = poke_tx.clone();
        let tz = config.user_timezone();
        let scheduler = HeartbeatScheduler::new(config.heartbeat.clone(), tz, poke_rx);
        let hb_cancel = heartbeat_cancel.clone();
        let hb_tx_clone = hb_tx.clone();
        tokio::spawn(async move {
            // Move poke_tx into the task to keep it alive for the scheduler's lifetime
            let _poke_tx = poke_tx;
            scheduler.run(hb_tx, hb_cancel).await;
        });
        (Some(hb_rx), Some(hb_tx_clone), Some(poke_tx_for_app))
    } else {
        tracing::info!(
            "Daemon is running — TUI heartbeat scheduler skipped (daemon owns heartbeat)"
        );
        (None, None, None)
    };

    // Auto-start gateway if enabled and any channels are installed
    let gateway_shutdown_token = CancellationToken::new();
    let _gateway_guard = gateway_shutdown_token.clone().drop_guard();
    spawn_gateway(&config, gateway_shutdown_token.clone(), metrics.clone());
    let gateway_shutdown = Arc::new(Mutex::new(gateway_shutdown_token));

    // Query terminal background before entering alt screen (some terminals
    // don't respond to the query inside alternate screen).
    colors::query_terminal_bg();

    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    stdout().execute(EnableAlternateScroll)?;
    stdout().execute(EnableBracketedPaste)?;

    // Guard ensures terminal is restored on any exit path (error or normal)
    let _guard = TerminalGuard;

    // Install panic hook that restores terminal before printing panic
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = stdout().execute(DisableBracketedPaste);
        let _ = stdout().execute(DisableAlternateScroll);
        let _ = disable_raw_mode();
        let _ = stdout().execute(LeaveAlternateScreen);
        original_hook(info);
    }));

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(config, heartbeat_rx, heartbeat_event_tx, poke_tx);
    if let Some((title, count)) = resumed_info {
        app.push_system_message(format!("Resumed session: {title} ({count} messages)"));
    }
    // Show vitals + evolution header on session start
    if let Ok(db) = borg_core::db::Database::open() {
        if let Ok(state) = db.get_vitals_state() {
            let now = chrono::Utc::now();
            let state = borg_core::vitals::apply_decay(&state, now);
            let drift = borg_core::vitals::detect_drift(&state, now);
            if let Some(notice) = borg_core::vitals::format_drift_notice(&drift) {
                app.push_system_message(notice);
            }
        }
    }
    // Auto-trigger first conversation if SETUP.md exists (fresh onboarding)
    if let Ok(data_dir) = Config::data_dir() {
        let setup_path = data_dir.join("SETUP.md");
        if setup_path.exists() {
            app.queued_messages.push_back(app::QueuedMessage {
                text: String::new(),
                images: Vec::new(),
            });
        }
    }

    let mut event_stream = EventStream::new();
    let tick_rate = Duration::from_millis(100);

    run_event_loop(
        &mut terminal,
        &mut app,
        &agent,
        &mut event_stream,
        tick_rate,
        &gateway_shutdown,
    )
    .await
}

/// Delegate to the shared heartbeat turn implementation.
async fn run_heartbeat_turn(config: &Config) -> HeartbeatResult {
    crate::service::execute_heartbeat_turn(config).await
}

/// If the app just became idle and has queued messages, auto-submit the next one.
/// Remaining messages stay in the queue and drain on subsequent TurnComplete events.
/// If the last turn errored, pause the queue and notify the user.
fn drain_queued_if_idle(app: &mut App<'_>) -> Result<AppAction> {
    if matches!(app.state, app::AppState::Idle) {
        // Don't start a new streaming turn while a popup is open — it would
        // change AppState to Streaming and break input routing to the popup.
        if app.any_popup_visible() {
            return Ok(AppAction::Continue);
        }
        if app.last_turn_errored && !app.queued_messages.is_empty() {
            if !app.queue_pause_notified {
                app.push_system_message(
                    "[queue paused — enter to resume, esc to clear]".to_string(),
                );
                app.queue_pause_notified = true;
            }
            return Ok(AppAction::Continue);
        }
        if let Some(qm) = app.pop_next_queued() {
            return app.handle_queued_submit(qm);
        }
    }
    Ok(AppAction::Continue)
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App<'_>,
    agent: &Arc<Mutex<Agent>>,
    event_stream: &mut EventStream,
    tick_rate: Duration,
    gateway_shutdown: &Arc<Mutex<CancellationToken>>,
) -> Result<Option<ResumeHint>> {
    let mut tick_interval = tokio::time::interval(tick_rate);

    loop {
        terminal.draw(|frame| app.render(frame))?;

        let action = tokio::select! {
            biased;

            // Agent events (check first for responsiveness)
            event = async {
                if let Some(rx) = &mut app.event_rx {
                    rx.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                match event {
                    Some(ev) => {
                        app.process_agent_event(ev);
                        drain_queued_if_idle(app)?
                    }
                    None => {
                        // Channel closed (agent task finished or panicked)
                        app.handle_agent_channel_closed();
                        drain_queued_if_idle(app)?
                    }
                }
            }

            // Heartbeat events
            Some(event) = async {
                if let Some(rx) = &mut app.heartbeat_rx {
                    rx.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                match event {
                    HeartbeatEvent::Fire => {
                        // Run a heartbeat agent turn in the background
                        let hb_config = app.config.clone();
                        let hb_tx_clone = app.heartbeat_event_tx.clone();
                        tokio::spawn(async move {
                            let result = run_heartbeat_turn(&hb_config).await;
                            if let Some(tx) = hb_tx_clone {
                                let _ = tx.send(HeartbeatEvent::Result(result)).await;
                            }
                        });
                    }
                    _ => {
                        app.process_heartbeat(event);
                    }
                }
                AppAction::Continue
            }

            // Doctor diagnostic events
            event = async {
                if let Some(rx) = &mut app.doctor_rx {
                    rx.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                if let Some(ev) = event {
                    app.process_doctor_event(ev);
                } else {
                    app.doctor_rx = None;
                }
                AppAction::Continue
            }

            // Terminal events
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => app.handle_key(key)?,
                    Some(Ok(Event::Paste(text))) => app.handle_paste(text),
                    // Intentionally NO `Event::Mouse` arm. We do not enable any
                    // mouse tracking mode (see EnableAlternateScroll above), so
                    // no mouse events should ever arrive here. Wheel events are
                    // delivered as Up/Down key events via xterm ?1007h and are
                    // handled in App::handle_key. DO NOT re-introduce a mouse
                    // handler — it would require enabling ?1000h/?1002h/?1003h,
                    // all of which break native text selection.
                    Some(Ok(Event::Resize(_, _))) => AppAction::Continue,
                    Some(Err(_)) | None => AppAction::Quit,
                    _ => AppAction::Continue,
                }
            }

            // Tick for status bar elapsed time + throbber animation + paste burst flush
            _ = tick_interval.tick() => {
                app.tick_throbber();
                app.tick_paste_burst();
                drain_queued_if_idle(app)?
            }
        };

        match action {
            AppAction::Quit => {
                let hint = {
                    let mut a = agent.lock().await;
                    a.close_browser().await;
                    let s = a.session();
                    if s.meta.message_count > 0 {
                        let short = s.meta.id.chars().take(8).collect::<String>();
                        Some(ResumeHint {
                            short_id: short,
                            title: s.meta.title.clone(),
                        })
                    } else {
                        None
                    }
                };
                return Ok(hint);
            }
            AppAction::SendMessage {
                input,
                images,
                event_tx,
                cancel,
            } => {
                // Set up steer channel for mid-turn user guidance
                let (steer_tx, steer_rx) = tokio::sync::mpsc::unbounded_channel();
                app.steer_tx = Some(steer_tx);
                app.pending_steers.clear();

                let agent_clone = Arc::clone(agent);
                tokio::spawn(async move {
                    let mut agent = agent_clone.lock().await;
                    agent.set_steer_channel(steer_rx);
                    if images.is_empty() {
                        if let Err(e) = agent
                            .send_message_with_cancel(&input, event_tx.clone(), cancel)
                            .await
                        {
                            let _ = event_tx.send(AgentEvent::Error(e.to_string())).await;
                        }
                    } else {
                        // Build multimodal message with text + images
                        use base64::Engine as _;
                        use borg_core::types::{ContentPart, MediaData, Message};
                        let engine = base64::engine::general_purpose::STANDARD;
                        let mut parts = vec![ContentPart::Text(input)];
                        for img in images {
                            parts.push(ContentPart::ImageBase64 {
                                media: MediaData {
                                    mime_type: img.mime_type,
                                    data: engine.encode(&img.data),
                                    filename: None,
                                },
                            });
                        }
                        let msg = Message::user_multimodal(parts);
                        if let Err(e) = agent.send_message_raw(msg, event_tx.clone(), cancel).await
                        {
                            let _ = event_tx.send(AgentEvent::Error(e.to_string())).await;
                        }
                    }
                });
            }
            AppAction::CompactHistory => {
                let mut agent = agent.lock().await;
                let (before, after) = agent.compact().await;
                let freed = before.saturating_sub(after);
                app.push_system_message(format!(
                    "Compacted: {before} → {after} tokens ({freed} freed)"
                ));
            }
            AppAction::ClearHistory => {
                let mut agent = agent.lock().await;
                agent.clear_history();
                app.push_system_message("Conversation cleared.".to_string());
            }
            AppAction::ShowUsage => {
                let agent = agent.lock().await;
                let (msg_count, token_count) = agent.conversation_stats();
                drop(agent);

                let prompt_tok = app.session_prompt_tokens;
                let completion_tok = app.session_completion_tokens;
                let total_tok = prompt_tok + completion_tok;

                let mut text = format!(
                    "Session: {msg_count} messages, ~{token_count} estimated tokens\n\
                     LLM usage: {prompt_tok} prompt + {completion_tok} completion = {total_tok} total tokens\n"
                );

                for (label, days) in [("24h", 1), ("7d", 7), ("30d", 30)] {
                    match borg_core::logging::count_messages_for_period(days) {
                        Ok(stats) => {
                            text.push_str(&format!(
                                "{label}: {} user, {} assistant, {} tool calls\n",
                                stats.user_messages, stats.assistant_messages, stats.tool_calls
                            ));
                        }
                        Err(e) => {
                            text.push_str(&format!("{label}: error reading logs: {e}\n"));
                        }
                    }
                }

                // Show monthly budget info
                let budget_limit = app.config.budget.monthly_token_limit;
                if budget_limit > 0 {
                    if let Ok(db) = borg_core::db::Database::open() {
                        if let Ok(used) = db.monthly_token_total() {
                            let pct = if budget_limit > 0 {
                                (used as f64 / budget_limit as f64 * 100.0) as u64
                            } else {
                                0
                            };
                            text.push_str(&format!(
                                "Budget: {used}/{budget_limit} tokens ({pct}%) used this month\n"
                            ));
                        }
                        if let Ok(Some(cost)) = db.monthly_total_cost() {
                            text.push_str(&format!("Estimated cost: ${cost:.4}\n"));
                        }
                    }
                }

                app.push_system_message(text.trim_end().to_string());
            }
            AppAction::UndoLastTurn => {
                let mut agent = agent.lock().await;
                let removed = agent.undo();
                if removed > 0 {
                    app.push_system_message(format!(
                        "Undid last turn ({removed} messages removed)."
                    ));
                } else {
                    app.push_system_message("Nothing to undo.".to_string());
                }
            }
            AppAction::RewindTo { nth_user_message } => {
                let mut agent = agent.lock().await;
                let removed = agent.rewind_to_nth_user_message(nth_user_message);
                if removed > 0 {
                    app.push_system_message(format!(
                        "Rewound conversation ({removed} messages removed). Edit and re-send."
                    ));
                } else {
                    app.push_system_message("Nothing to rewind.".to_string());
                }
            }
            AppAction::UpdateSetting { key, value } => {
                let mut agent = agent.lock().await;
                if let Err(e) = agent.config_mut().apply_setting(&key, &value) {
                    app.push_system_message(format!("Warning: failed to sync agent config: {e}"));
                }
            }
            AppAction::ConfigReloaded => {
                let mut agent = agent.lock().await;
                *agent.config_mut() = app.config.clone();
            }
            AppAction::SaveSession => {
                let mut agent = agent.lock().await;
                agent.auto_save();
                let session = agent.session();
                app.push_system_message(format!(
                    "Session saved: {} ({})",
                    session.meta.title, session.meta.id
                ));
            }
            AppAction::NewSession => {
                let mut agent = agent.lock().await;
                agent.new_session();
                app.push_system_message("New session started.".to_string());
            }
            AppAction::LoadSession { id } => {
                // Support partial ID matching (prefix) via shared resolver.
                let load_id = match borg_core::session::resolve_session_id(&id) {
                    Ok(meta) => meta.id,
                    Err(borg_core::session::ResolveSessionError::Ambiguous { count, .. }) => {
                        app.push_system_message(format!(
                            "Ambiguous session ID '{id}' — matches {count} sessions. Be more specific."
                        ));
                        continue;
                    }
                    // NotFound / Empty: fall through to agent.load_session which will
                    // produce a clean "Session '<id>' not found" error below.
                    Err(_) => id.clone(),
                };

                let mut agent = agent.lock().await;
                match agent.load_session(&load_id) {
                    Ok(()) => {
                        let session = agent.session();
                        let title = session.meta.title.clone();
                        let count = session.meta.message_count;
                        app.push_system_message(format!(
                            "Loaded session: {title} ({count} messages)"
                        ));
                    }
                    Err(e) => {
                        app.push_system_message(format!("Failed to load session: {e}"));
                    }
                }
            }
            AppAction::LaunchExternalEditor => {
                let current_text = app.composer.text();
                // Leave alternate screen and disable raw mode for editor
                let _ = disable_raw_mode();
                let _ = stdout().execute(LeaveAlternateScreen);

                let result = external_editor::open_external_editor(&current_text);

                // Restore terminal
                let _ = enable_raw_mode();
                let _ = stdout().execute(EnterAlternateScreen);
                terminal.clear()?;

                match result {
                    Ok(text) => {
                        let trimmed = text.trim_end();
                        app.composer.set_text(trimmed);
                    }
                    Err(e) => {
                        app.push_system_message(format!("Editor error: {e}"));
                    }
                }
            }
            AppAction::RunPlugins { actions } => {
                let data_dir = match borg_core::config::Config::data_dir() {
                    Ok(dir) => dir,
                    Err(e) => {
                        app.push_system_message(format!("Failed to resolve data directory: {e}"));
                        continue;
                    }
                };
                let mut results: Vec<String> = Vec::new();

                for action in actions {
                    match action {
                        plugins_popup::PluginAction::Install { id, credentials } => {
                            if let Some(def) = borg_plugins::catalog::find_by_id(&id) {
                                match borg_plugins::installer::install(
                                    def,
                                    &data_dir,
                                    &credentials,
                                    None,
                                )
                                .await
                                {
                                    Ok(install_result) => {
                                        // Record in DB + store file hashes
                                        if let Ok(db) = borg_core::db::Database::open() {
                                            if let Err(e) = db.insert_plugin(
                                                def.id,
                                                def.name,
                                                &def.kind.to_string(),
                                                &def.category.to_string(),
                                            ) {
                                                tracing::warn!("Failed to record plugin: {e}");
                                            }

                                            // Store file hashes for integrity verification
                                            for (path, hash) in &install_result.file_hashes {
                                                if let Err(e) =
                                                    db.insert_file_hash(def.id, path, hash)
                                                {
                                                    tracing::warn!(
                                                        "Failed to store file hash for {path}: {e}"
                                                    );
                                                }
                                            }

                                            // Register installed tool/channel
                                            let item_name = if def.is_native {
                                                def.id.rsplit('/').next().unwrap_or(def.id)
                                            } else if let Some(first_tmpl) = def.templates.first() {
                                                first_tmpl
                                                    .relative_path
                                                    .split('/')
                                                    .next()
                                                    .unwrap_or(def.id)
                                            } else {
                                                def.id
                                            };
                                            let runtime =
                                                if def.is_native { "native" } else { "python" };
                                            match def.kind {
                                                borg_plugins::PluginKind::Tool => {
                                                    if let Err(e) = db.insert_installed_tool(
                                                        item_name,
                                                        def.description,
                                                        runtime,
                                                        def.id,
                                                    ) {
                                                        tracing::warn!(
                                                            "Failed to register tool: {e}"
                                                        );
                                                    }
                                                }
                                                borg_plugins::PluginKind::Channel => {
                                                    let webhook = format!("/webhook/{item_name}");
                                                    if let Err(e) = db.insert_installed_channel(
                                                        item_name,
                                                        def.description,
                                                        runtime,
                                                        def.id,
                                                        &webhook,
                                                    ) {
                                                        tracing::warn!(
                                                            "Failed to register channel: {e}"
                                                        );
                                                    }
                                                }
                                            }
                                        }

                                        // Wire credential entries to DB
                                        if !install_result.credential_entries.is_empty() {
                                            if let Ok(mut cfg) = Config::load_from_db() {
                                                for entry in &install_result.credential_entries {
                                                    cfg.credentials.insert(
                                                        entry.key.clone(),
                                                        borg_core::config::CredentialValue::Ref(
                                                            borg_core::secrets_resolve::SecretRef::Keychain {
                                                                service: entry.service.clone(),
                                                                account: entry.account.clone(),
                                                            },
                                                        ),
                                                    );
                                                }
                                                if let Ok(json) =
                                                    serde_json::to_string(&cfg.credentials)
                                                {
                                                    if let Ok(db) = borg_core::db::Database::open()
                                                    {
                                                        if let Err(e) =
                                                            db.set_setting("credentials", &json)
                                                        {
                                                            tracing::warn!(
                                                                "Failed to save credentials after plugin install: {e}"
                                                            );
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        let mut msg = format!("Installed {}", def.name);
                                        for note in &install_result.notes {
                                            msg.push_str(&format!("\n  {note}"));
                                        }
                                        if def.kind == borg_plugins::PluginKind::Channel {
                                            let gw_msg = restart_gateway(gateway_shutdown).await;
                                            msg.push_str(&format!("\n  {gw_msg}"));
                                            msg.push_str(
                                                "\n  Approve senders with: /pairing approve <CODE>",
                                            );
                                        }
                                        results.push(msg);
                                    }
                                    Err(e) => {
                                        results
                                            .push(format!("Failed to install {}: {e}", def.name));
                                    }
                                }
                            }
                        }
                        plugins_popup::PluginAction::SetSkillEnabled { name, enabled } => {
                            if let Ok(db) = borg_core::db::Database::open() {
                                let key = format!("skills.entries.{name}.enabled");
                                let val = enabled.to_string();
                                match app.config.apply_setting(&key, &val) {
                                    Ok(_) => {
                                        let _ = db.set_setting(&key, &val);
                                        let status = if enabled { "enabled" } else { "disabled" };
                                        results.push(format!("{name}: {status}"));
                                    }
                                    Err(e) => {
                                        results.push(format!("Error updating {name}: {e}"));
                                    }
                                }
                            } else {
                                results.push(format!(
                                    "Error persisting {name}: could not open database"
                                ));
                            }
                        }
                        plugins_popup::PluginAction::Uninstall { id } => {
                            if let Some(def) = borg_plugins::catalog::find_by_id(&id) {
                                // Remove keychain entries
                                if let Err(e) = borg_plugins::installer::uninstall(def, &data_dir) {
                                    results.push(format!("Failed to remove {}: {e}", def.name));
                                    continue;
                                }

                                // Wipe entire data directory (~/.borg/)
                                if data_dir.exists() {
                                    if let Err(e) = std::fs::remove_dir_all(&data_dir) {
                                        tracing::warn!(
                                            "Failed to remove data directory {}: {e}",
                                            data_dir.display()
                                        );
                                    }
                                }

                                let mut msg = format!("Removed {}", def.name);
                                msg.push_str("\n  Data directory wiped.");
                                if def.kind == borg_plugins::PluginKind::Channel {
                                    let gw_msg = restart_gateway(gateway_shutdown).await;
                                    msg.push_str(&format!("\n  {gw_msg}"));
                                }
                                results.push(msg);
                            }
                        }
                    }
                }

                if !results.is_empty() {
                    app.push_system_message(results.join("\n"));
                }
            }
            AppAction::PlanProceed { clear_context } => {
                // Exit Plan mode before the follow-up turn — otherwise the proceed
                // message would itself be blocked by mutation constraints.
                let restored = app
                    .previous_collab_mode
                    .take()
                    .unwrap_or(CollaborationMode::Default);
                if app.config.conversation.collaboration_mode != restored {
                    app.config.conversation.collaboration_mode = restored;
                    app.push_system_message(format!("[mode: {restored}]"));
                }

                let proceed_msg = if clear_context {
                    // Extract plan text before clearing so the agent knows what to do
                    let plan_text = app
                        .cells
                        .iter()
                        .rev()
                        .find_map(|c| {
                            if let HistoryCell::Assistant { text, .. } = c {
                                if !text.trim().is_empty() {
                                    return Some(text.clone());
                                }
                            }
                            None
                        })
                        .unwrap_or_default();

                    let mut agent = agent.lock().await;
                    agent.clear_history();
                    app.cells.clear();
                    app.session_prompt_tokens = 0;
                    app.session_completion_tokens = 0;

                    if plan_text.is_empty() {
                        "Proceed with the plan as outlined.".to_string()
                    } else {
                        format!(
                            "Here is the plan we agreed on:\n\n{plan_text}\n\nProceed with this plan."
                        )
                    }
                } else {
                    "Proceed with the plan as outlined.".to_string()
                };
                app.queued_messages.push_front(app::QueuedMessage {
                    text: proceed_msg,
                    images: Vec::new(),
                });
            }
            AppAction::RunScheduleActions { actions } => {
                let mut results: Vec<String> = Vec::new();
                if let Ok(db) = borg_core::db::Database::open() {
                    for action in actions {
                        match action {
                            schedule_popup::ScheduleAction::ToggleStatus {
                                task_id,
                                new_status,
                            } => match db.update_task_status(&task_id, &new_status) {
                                Ok(true) => {
                                    results.push(format!(
                                        "Task {} {new_status}",
                                        &task_id[..8.min(task_id.len())]
                                    ));
                                }
                                Ok(false) => {
                                    results.push(format!(
                                        "Task {} not found",
                                        &task_id[..8.min(task_id.len())]
                                    ));
                                }
                                Err(e) => results.push(format!("Error: {e}")),
                            },
                            schedule_popup::ScheduleAction::UpdateSchedule {
                                task_id,
                                schedule_type,
                                new_expr,
                            } => {
                                let update = borg_core::db::UpdateTask {
                                    name: None,
                                    prompt: None,
                                    schedule_type: Some(&schedule_type),
                                    schedule_expr: Some(&new_expr),
                                    timezone: None,
                                };
                                match db.update_task(&task_id, &update) {
                                    Ok(true) => {
                                        results.push(format!(
                                            "Task {} schedule updated to {new_expr}",
                                            &task_id[..8.min(task_id.len())]
                                        ));
                                    }
                                    Ok(false) => {
                                        results.push(format!(
                                            "Task {} not found",
                                            &task_id[..8.min(task_id.len())]
                                        ));
                                    }
                                    Err(e) => results.push(format!("Error: {e}")),
                                }
                            }
                            schedule_popup::ScheduleAction::DeleteTask { task_id } => {
                                match db.delete_task(&task_id) {
                                    Ok(true) => {
                                        results.push(format!(
                                            "Task {} deleted",
                                            &task_id[..8.min(task_id.len())]
                                        ));
                                    }
                                    Ok(false) => {
                                        results.push(format!(
                                            "Task {} not found",
                                            &task_id[..8.min(task_id.len())]
                                        ));
                                    }
                                    Err(e) => results.push(format!("Error: {e}")),
                                }
                            }
                            schedule_popup::ScheduleAction::CancelWorkflow { workflow_id } => {
                                match db.cancel_workflow(&workflow_id) {
                                    Ok(true) => {
                                        results.push(format!(
                                            "Workflow {} cancelled",
                                            &workflow_id[..8.min(workflow_id.len())]
                                        ));
                                    }
                                    Ok(false) => {
                                        results.push(format!(
                                            "Workflow {} not found or already finished",
                                            &workflow_id[..8.min(workflow_id.len())]
                                        ));
                                    }
                                    Err(e) => results.push(format!("Error: {e}")),
                                }
                            }
                            schedule_popup::ScheduleAction::DeleteWorkflow { workflow_id } => {
                                match db.delete_workflow(&workflow_id) {
                                    Ok(true) => {
                                        results.push(format!(
                                            "Workflow {} deleted",
                                            &workflow_id[..8.min(workflow_id.len())]
                                        ));
                                    }
                                    Ok(false) => {
                                        results.push(format!(
                                            "Workflow {} not found",
                                            &workflow_id[..8.min(workflow_id.len())]
                                        ));
                                    }
                                    Err(e) => results.push(format!("Error: {e}")),
                                }
                            }
                        }
                    }
                } else {
                    results.push("Error: could not open database".to_string());
                }
                if !results.is_empty() {
                    app.push_system_message(results.join("\n"));
                }
            }
            AppAction::RunProjectActions { actions } => {
                let mut results: Vec<String> = Vec::new();
                if let Ok(db) = borg_core::db::Database::open() {
                    for action in actions {
                        match action {
                            projects_popup::ProjectAction::Create {
                                id,
                                name,
                                description,
                            } => match db.create_project(&id, &name, &description) {
                                Ok(()) => {
                                    results.push(format!("Project '{name}' created"));
                                }
                                Err(e) => results.push(format!("Error: {e}")),
                            },
                            projects_popup::ProjectAction::Update {
                                project_id,
                                name,
                                description,
                            } => {
                                match db.update_project(
                                    &project_id,
                                    name.as_deref(),
                                    description.as_deref(),
                                    None,
                                ) {
                                    Ok(true) => {
                                        results.push(format!(
                                            "Project {} updated",
                                            &project_id[..8.min(project_id.len())]
                                        ));
                                    }
                                    Ok(false) => {
                                        results.push(format!(
                                            "Project {} not found",
                                            &project_id[..8.min(project_id.len())]
                                        ));
                                    }
                                    Err(e) => results.push(format!("Error: {e}")),
                                }
                            }
                            projects_popup::ProjectAction::Archive { project_id } => {
                                match db.archive_project(&project_id) {
                                    Ok(true) => {
                                        results.push(format!(
                                            "Project {} archived",
                                            &project_id[..8.min(project_id.len())]
                                        ));
                                    }
                                    Ok(false) => {
                                        results.push(format!(
                                            "Project {} not found or already archived",
                                            &project_id[..8.min(project_id.len())]
                                        ));
                                    }
                                    Err(e) => results.push(format!("Error: {e}")),
                                }
                            }
                            projects_popup::ProjectAction::Unarchive { project_id } => {
                                match db.update_project(&project_id, None, None, Some("active")) {
                                    Ok(true) => {
                                        results.push(format!(
                                            "Project {} unarchived",
                                            &project_id[..8.min(project_id.len())]
                                        ));
                                    }
                                    Ok(false) => {
                                        results.push(format!(
                                            "Project {} not found",
                                            &project_id[..8.min(project_id.len())]
                                        ));
                                    }
                                    Err(e) => results.push(format!("Error: {e}")),
                                }
                            }
                            projects_popup::ProjectAction::Delete { project_id } => {
                                match db.delete_project(&project_id) {
                                    Ok(true) => {
                                        results.push(format!(
                                            "Project {} deleted",
                                            &project_id[..8.min(project_id.len())]
                                        ));
                                    }
                                    Ok(false) => {
                                        results.push(format!(
                                            "Project {} not found",
                                            &project_id[..8.min(project_id.len())]
                                        ));
                                    }
                                    Err(e) => results.push(format!("Error: {e}")),
                                }
                            }
                        }
                    }
                } else {
                    results.push("Error: could not open database".to_string());
                }
                if !results.is_empty() {
                    app.push_system_message(results.join("\n"));
                }
            }
            AppAction::RunPairingActions { actions } => {
                let mut results: Vec<String> = Vec::new();
                if let Ok(db) = borg_core::db::Database::open() {
                    for action in actions {
                        match action {
                            pairing_popup::PairingAction::Approve { channel, code } => {
                                match db.approve_pairing(&channel, &code) {
                                    Ok(row) => {
                                        let display = borg_core::pairing::channel_display_name(
                                            &row.channel_name,
                                        );
                                        results.push(format!(
                                            "Approved: {} on {} (sender: {})",
                                            row.code, display, row.sender_id
                                        ));
                                        // Fire-and-forget greeting
                                        let config = app.config.clone();
                                        let ch = row.channel_name;
                                        let sid = row.sender_id;
                                        tokio::spawn(async move {
                                            crate::service::send_approval_greeting(
                                                &config, &ch, &sid,
                                            )
                                            .await;
                                        });
                                    }
                                    Err(e) => {
                                        results.push(format!("Failed to approve: {e}"));
                                    }
                                }
                            }
                            pairing_popup::PairingAction::Revoke { channel, sender_id } => {
                                match db.revoke_sender(&channel, &sender_id) {
                                    Ok(true) => {
                                        let display =
                                            borg_core::pairing::channel_display_name(&channel);
                                        results.push(format!(
                                            "Revoked sender {sender_id} from {display}."
                                        ));
                                    }
                                    Ok(false) => {
                                        results.push(format!(
                                            "No approved sender found for {channel}/{sender_id}."
                                        ));
                                    }
                                    Err(e) => {
                                        results.push(format!("Failed to revoke: {e}"));
                                    }
                                }
                            }
                        }
                    }
                } else {
                    results.push("Error: could not open database".to_string());
                }
                if !results.is_empty() {
                    app.push_system_message(results.join("\n"));
                }
            }
            AppAction::RunMigration { actions } => {
                for action in actions {
                    match action {
                        migrate_popup::MigrateAction::Apply { plan, source_data } => {
                            let borg_dir = match borg_core::config::Config::data_dir() {
                                Ok(d) => d,
                                Err(e) => {
                                    app.push_system_message(format!("Migration error: {e}"));
                                    continue;
                                }
                            };
                            match borg_core::migrate::apply::apply_plan(
                                &plan,
                                &source_data,
                                &borg_dir,
                            ) {
                                Ok(result) => {
                                    let mut msg = String::from("Migration complete:");
                                    if result.config_changes_applied > 0 {
                                        msg.push_str(&format!(
                                            "\n  {} config change(s) applied",
                                            result.config_changes_applied
                                        ));
                                    }
                                    if result.credentials_added > 0 {
                                        msg.push_str(&format!(
                                            "\n  {} credential(s) added",
                                            result.credentials_added
                                        ));
                                    }
                                    if result.memory_files_copied > 0 {
                                        msg.push_str(&format!(
                                            "\n  {} memory file(s) copied",
                                            result.memory_files_copied
                                        ));
                                    }
                                    if result.persona_copied {
                                        msg.push_str("\n  Persona copied to IDENTITY.md");
                                    }
                                    if result.skills_copied > 0 {
                                        msg.push_str(&format!(
                                            "\n  {} skill(s) copied",
                                            result.skills_copied
                                        ));
                                    }
                                    for warning in &result.warnings {
                                        msg.push_str(&format!("\n  Warning: {warning}"));
                                    }
                                    // Log to activity log so /logs shows it
                                    if let Ok(db) = borg_core::db::Database::open() {
                                        borg_core::activity_log::log_activity(
                                            &db, "info", "migrate", &msg,
                                        );
                                    }
                                    app.push_system_message(msg);
                                    // Reload config to pick up migrated settings
                                    app.config = borg_core::config::Config::load_from_db()
                                        .unwrap_or_default();
                                }
                                Err(e) => {
                                    let msg = format!("Migration failed: {e}");
                                    if let Ok(db) = borg_core::db::Database::open() {
                                        borg_core::activity_log::log_activity(
                                            &db, "error", "migrate", &msg,
                                        );
                                    }
                                    app.push_system_message(msg);
                                }
                            }
                        }
                    }
                }
            }
            AppAction::RestartGateway => {
                let msg = restart_gateway(gateway_shutdown).await;
                app.push_system_message(msg);
            }
            AppAction::SelfUpdate { dev } => {
                app.push_system_message(format!(
                    "Checking for updates{}...",
                    if dev { " (including pre-releases)" } else { "" }
                ));
                terminal.draw(|f| app.render(f))?;
                match borg_core::update::perform_update(dev).await {
                    Ok(result) => match result.status {
                        borg_core::update::UpdateStatus::AlreadyUpToDate => {
                            app.push_system_message(format!(
                                "Already up to date ({})",
                                result.current_version
                            ));
                        }
                        borg_core::update::UpdateStatus::Updated { from, to } => {
                            app.push_system_message(format!(
                                "Updated borg: {from} → {to}\nPlease restart borg to use the new version."
                            ));
                        }
                    },
                    Err(e) => {
                        app.push_system_message(format!("Update failed: {e}"));
                    }
                }
            }
            AppAction::Uninstall => {
                // Step 1: Daemon service
                app.push_system_message("Removing daemon service...".to_string());
                terminal.draw(|f| app.render(f))?;
                if let Err(e) = crate::service::uninstall_service() {
                    tracing::debug!("Service uninstall skipped: {e}");
                }

                // Step 2: Data directory
                app.push_system_message("Removing data directory (~/.borg/)...".to_string());
                terminal.draw(|f| app.render(f))?;
                if let Ok(data_dir) = borg_core::config::Config::data_dir() {
                    if let Err(e) = std::fs::remove_dir_all(&data_dir) {
                        app.push_system_message(format!("Warning: failed to remove data dir: {e}"));
                        terminal.draw(|f| app.render(f))?;
                    }
                }

                // Step 3: Binary
                app.push_system_message("Removing binary...".to_string());
                terminal.draw(|f| app.render(f))?;
                if let Ok(exe) = std::env::current_exe() {
                    let exe = exe.canonicalize().unwrap_or(exe);
                    if let Err(e) = std::fs::remove_file(&exe) {
                        tracing::debug!("Could not remove binary: {e}");
                    }
                }

                // Close browser
                agent.lock().await.close_browser().await;

                // Fullscreen ASCII art goodbye
                goodbye::render(terminal)?;
                tokio::time::sleep(Duration::from_secs(3)).await;
                // Binary is being removed — no point offering a resume hint.
                return Ok(None);
            }
            AppAction::Poke => {
                let config = app.config.clone();
                tokio::spawn(async move {
                    let url = format!(
                        "http://{}:{}/internal/poke",
                        config.gateway.host, config.gateway.port
                    );
                    let client = reqwest::Client::new();
                    match client
                        .post(&url)
                        .timeout(std::time::Duration::from_secs(5))
                        .send()
                        .await
                    {
                        Ok(r) if r.status().is_success() => {}
                        Ok(r) => {
                            tracing::warn!("Poke via HTTP failed: {}", r.status());
                        }
                        Err(e) => {
                            tracing::warn!("Poke via HTTP failed: {e}");
                        }
                    }
                });
                app.push_system_message("[poke: sent to daemon]".to_string());
            }
            AppAction::RunDoctor => {
                app.push_system_message("Borg Doctor\n───────────".to_string());
                let (tx, rx) = tokio::sync::mpsc::channel(32);
                app.doctor_rx = Some(rx);
                let config = app.config.clone();
                tokio::task::spawn_blocking(move || {
                    use borg_core::doctor::DiagnosticRunner;
                    let mut runner = DiagnosticRunner::new();
                    let mut all_checks: Vec<borg_core::doctor::DiagnosticCheck> = Vec::new();
                    while let Some(label) = runner.peek_label(&config) {
                        let _ = tx.blocking_send(app::DoctorEvent::Analyzing {
                            label: label.to_string(),
                        });
                        if let Some((_label, checks)) = runner.next_step(&config) {
                            all_checks.extend(checks.clone());
                            let _ = tx.blocking_send(app::DoctorEvent::Result {
                                label: label.to_string(),
                                checks,
                            });
                        }
                    }
                    let report = borg_core::doctor::DiagnosticReport { checks: all_checks };
                    let (pass, warn, fail) = report.counts();
                    let _ = tx.blocking_send(app::DoctorEvent::Done { pass, warn, fail });
                });
            }
            AppAction::Continue => {}
        }
    }
}

// =============================================================================
// Tests — DO NOT REMOVE OR WEAKEN.
// =============================================================================
// These tests enforce the native-text-selection invariant. If any of them fail,
// text selection in the TUI is almost certainly broken in at least one
// terminal. This has regressed multiple times; the belt-and-suspenders approach
// (sequence tests + source-string guards + lifecycle test) is intentional.
// See CLAUDE.md "Mouse Interaction" for the full rationale.
// =============================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::Command;

    /// Forbidden escape-sequence modes. Presence of ANY of these in the code
    /// paths that emit ANSI to the terminal will break native text selection.
    const FORBIDDEN_MODES: &[&str] = &["?1000", "?1002", "?1003", "?1006"];

    // ------------------------------------------------------------------------
    // Escape-sequence correctness
    // ------------------------------------------------------------------------

    #[test]
    fn enable_alternate_scroll_emits_exactly_1007h() {
        let mut buf = String::new();
        EnableAlternateScroll.write_ansi(&mut buf).unwrap();
        assert_eq!(
            buf, "\x1b[?1007h",
            "EnableAlternateScroll must emit exactly ?1007h (xterm Alternate \
             Scroll Mode). Anything else risks breaking text selection."
        );
    }

    #[test]
    fn disable_alternate_scroll_emits_exactly_1007l() {
        let mut buf = String::new();
        DisableAlternateScroll.write_ansi(&mut buf).unwrap();
        assert_eq!(buf, "\x1b[?1007l");
    }

    #[test]
    fn enable_alternate_scroll_contains_no_forbidden_modes() {
        let mut buf = String::new();
        EnableAlternateScroll.write_ansi(&mut buf).unwrap();
        for mode in FORBIDDEN_MODES {
            assert!(
                !buf.contains(mode),
                "EnableAlternateScroll must NOT emit {mode} — it breaks native text selection"
            );
        }
    }

    #[test]
    fn disable_alternate_scroll_contains_no_forbidden_modes() {
        let mut buf = String::new();
        DisableAlternateScroll.write_ansi(&mut buf).unwrap();
        for mode in FORBIDDEN_MODES {
            assert!(
                !buf.contains(mode),
                "DisableAlternateScroll must NOT reference {mode}"
            );
        }
    }

    #[test]
    fn enable_and_disable_are_symmetric() {
        let mut enable = String::new();
        let mut disable = String::new();
        EnableAlternateScroll.write_ansi(&mut enable).unwrap();
        DisableAlternateScroll.write_ansi(&mut disable).unwrap();
        // Strip the trailing `h`/`l` and compare — catches someone changing
        // one side without the other.
        assert_eq!(
            enable.trim_end_matches('h'),
            disable.trim_end_matches('l'),
            "enable/disable sequences must target the same mode"
        );
    }

    // ------------------------------------------------------------------------
    // Source-level guards — read this file and app.rs via include_str! and
    // assert no forbidden mode appears in CODE (comments are allowed to
    // reference them; they must, to document why they're forbidden).
    // ------------------------------------------------------------------------

    /// Strip line comments AND the `#[cfg(test)] mod tests { ... }` module
    /// from a Rust source string. This lets us search code-only (i.e.,
    /// non-test runtime code) for forbidden patterns while still allowing
    /// comments and test scaffolding to reference them — comments must
    /// document WHY the modes are forbidden, and tests must reference them
    /// to assert their absence.
    ///
    /// Intentionally simple: the line-comment stripper does not handle block
    /// comments or string literals containing `//`, and the test-module cut
    /// is a naive "everything from the first `#[cfg(test)]` onward" slice.
    /// Both are sufficient for the two files we guard.
    fn strip_tests_and_comments(src: &str) -> String {
        let code = match src.find("#[cfg(test)]") {
            Some(idx) => &src[..idx],
            None => src,
        };
        code.lines()
            .map(|line| match line.find("//") {
                Some(idx) => &line[..idx],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn mod_rs_code_contains_no_forbidden_mouse_modes() {
        let src = include_str!("mod.rs");
        let code = strip_tests_and_comments(src);
        for mode in FORBIDDEN_MODES {
            assert!(
                !code.contains(mode),
                "crates/cli/src/tui/mod.rs code contains forbidden mouse mode {mode}. \
                 This will break native text selection. See the comment block above \
                 EnableAlternateScroll for the allowed modes and the rationale."
            );
        }
    }

    #[test]
    fn app_rs_code_contains_no_forbidden_mouse_modes() {
        let src = include_str!("app.rs");
        let code = strip_tests_and_comments(src);
        for mode in FORBIDDEN_MODES {
            assert!(
                !code.contains(mode),
                "crates/cli/src/tui/app.rs code contains forbidden mouse mode {mode}. \
                 No mouse tracking mode may be referenced from app code — mouse events \
                 are not delivered to the app at all (wheel → arrow keys via ?1007h)."
            );
        }
    }

    #[test]
    fn mod_rs_code_does_not_call_crossterm_enable_mouse_capture() {
        let src = include_str!("mod.rs");
        let code = strip_tests_and_comments(src);
        assert!(
            !code.contains("EnableMouseCapture"),
            "crossterm's EnableMouseCapture enables ?1000h/?1002h/?1003h/?1006h, \
             all of which break native text selection. Use EnableAlternateScroll \
             (?1007h) instead."
        );
        assert!(
            !code.contains("DisableMouseCapture"),
            "crossterm's DisableMouseCapture must not appear in code — we never \
             enable mouse capture, so there is nothing to disable."
        );
    }

    #[test]
    fn app_rs_has_no_event_mouse_match_arm() {
        let src = include_str!("app.rs");
        let code = strip_tests_and_comments(src);
        assert!(
            !code.contains("Event::Mouse("),
            "app.rs must not match Event::Mouse — we do not enable mouse \
             tracking, and adding such a handler would require enabling a \
             forbidden mode."
        );
        assert!(
            !code.contains("fn handle_mouse"),
            "app.rs must not define handle_mouse — there is no mouse event \
             source. Wheel is routed as KeyCode::Up/Down via ?1007h and \
             handled in handle_key."
        );
    }

    #[test]
    fn app_rs_has_no_mouse_event_kind_references() {
        let src = include_str!("app.rs");
        let code = strip_tests_and_comments(src);
        assert!(
            !code.contains("MouseEventKind"),
            "app.rs must not reference crossterm::event::MouseEventKind — \
             mouse events are not delivered to this crate."
        );
    }

    // ------------------------------------------------------------------------
    // Lifecycle — dropping TerminalGuard must emit the disable sequence.
    // ------------------------------------------------------------------------

    #[test]
    fn disable_alternate_scroll_command_can_be_written_to_buffer() {
        // Smoke test that the Command impl is still usable by crossterm.
        // (TerminalGuard::drop writes to stdout which isn't capturable here;
        // we verify the Command encoding instead, which is the only state
        // the guard's drop reaches through.)
        let mut buf = String::new();
        DisableAlternateScroll.write_ansi(&mut buf).unwrap();
        assert!(buf.ends_with('l'));
        assert!(buf.contains("?1007"));
    }
}
