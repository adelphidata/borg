mod agents_popup;
mod app;
mod btw_popup;
mod colors;
mod command_popup;
mod commands;
mod composer;
mod constants;
mod custom_commands;
mod custom_terminal;
mod diff_render;
mod external_editor;
mod file_popup;
mod file_search;
mod goodbye;
mod highlight;
mod history;
mod insert_history;
mod layout;
mod line_utils;
mod markdown;
mod mcp_popup;
pub(crate) mod migrate_popup;
mod model_popup;
mod pairing_popup;
mod paste_burst;
mod plan_overlay;
mod plugins_popup;
mod popup_utils;
mod schedule_popup;
mod sessions_popup;
mod settings_popup;
mod shimmer;
mod status_popup;
mod subprocesses_popup;
pub(crate) mod theme;
mod theme_popup;
pub(crate) mod themes;
mod toast;
mod tool_display;
mod transcript_pager;
mod tui_lifecycle;
mod update_prompt;
mod usage_popup;
mod wrapping;

use std::io::stdout;
use std::panic;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream};

// ============================================================================
// IMPORTANT: Native text selection MUST work in this TUI. DO NOT regress.
// ============================================================================
// Click+drag selection (for copy/paste) must work exactly like any normal
// terminal, with no modifier keys required. Any form of mouse tracking
// reported to the application breaks this.
//
// Strategy: inline-viewport mode delegates scrollback to the terminal. xterm
// Alternate Scroll Mode (?1007h) is enabled ONLY when transitioning into an
// alt-screen surface (via `tui_lifecycle::Tui::enter_alt_screen`). Trackpad/
// wheel events scroll the terminal's scrollback natively when the inline
// viewport is active. We never enable any mouse tracking mode, so click+drag
// text selection stays handled entirely by the terminal.
//
// FORBIDDEN — DO NOT ADD any of these (each one will regress text selection):
//   - ?1000h  Normal button tracking — clicks go to app, breaks selection.
//   - ?1002h  Button-event (drag) tracking — same, plus eats drag events.
//   - ?1003h  Any-event tracking — captures every mouse movement.
//   - ?1006h  SGR extended coordinates — only meaningful with the above.
//   - crossterm::event::EnableMouseCapture — it turns on ?1000h + ?1002h + ?1003h + ?1006h.
//
// Source-string guard tests below will fail the build if any forbidden mode
// is reintroduced. See CLAUDE.md "Mouse Interaction" for the full rationale.
// ============================================================================

use futures::StreamExt;
use protocols::admin::HeartbeatEvent as PbHeartbeatEvent;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use cortex::agent::AgentEvent;
use cortex::config::{CollaborationMode, Config};
use cortex::telemetry::BorgMetrics;
use heartbeat::scheduler::HeartbeatEvent;

use crate::daemon_client::{forward_event_stream, DaemonClient};

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
        match gateway::GatewayServer::new(gw_config, gw_shutdown, gw_metrics, None, None) {
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
            // probe_imessage is async (libsql open + query), so the probe-and-spawn
            // sequence runs inside a single tokio task.
            let im_config = config.clone();
            let im_shutdown = shutdown;
            tokio::spawn(async move {
                let probe = gateway::imessage::probe::probe_imessage().await;
                match probe.status {
                    gateway::imessage::probe::ProbeStatus::Ok => {
                        match gateway::imessage::start_imessage_monitor(im_config, im_shutdown)
                            .await
                        {
                            Ok(_handle) => tracing::info!("iMessage monitor started"),
                            Err(e) => tracing::warn!("iMessage monitor failed: {e}"),
                        }
                    }
                    gateway::imessage::probe::ProbeStatus::NoDiskAccess => {
                        tracing::warn!("iMessage: Full Disk Access required (System Settings > Privacy & Security). Skipping monitor.");
                    }
                    other => {
                        tracing::warn!("iMessage probe: {other}. Skipping monitor.");
                    }
                }
            });
        }
    }

    true
}

/// Restart the gateway: if a daemon owns the port, signal it via HTTP;
/// otherwise cancel the local token and respawn in-process.
/// Build the user-facing post-install message for a plugin.
///
/// Channel plugins get an extra line explaining the pairing workflow: the user
/// must DM the bot first so a pairing code is generated, then approve it in
/// the TUI. The gateway-restart outcome is intentionally NOT included here —
/// it is logged separately for debugging.
fn build_post_install_message(plugin_name: &str, notes: &[String], is_channel: bool) -> String {
    let mut msg = format!("Installed {plugin_name}");
    for note in notes {
        msg.push_str(&format!("\n  {note}"));
    }
    if is_channel {
        msg.push_str(&format!(
            "\n  Send a message to the bot on {plugin_name} to get your pairing code, then run: /pairing approve <CODE>"
        ));
    }
    msg
}

async fn restart_gateway(gateway_shutdown: &Arc<Mutex<CancellationToken>>) -> String {
    // Try to signal a running daemon's gateway first
    let config = match Config::load_from_db().await {
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
///
/// Inline-viewport mode never enters the alternate screen at startup, so the
/// guard only needs to pop input modes (raw mode, bracketed paste, focus) via
/// `tui_lifecycle::restore`.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = tui_lifecycle::restore();
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
    // Kick off a background fetch of the OpenRouter model catalog so the
    // `/model` picker reflects the live list (dropping stale IDs, tagging
    // free models). Fire-and-forget: if it fails, the picker falls back to
    // the hardcoded curated list.
    crate::openrouter_catalog::spawn_prefetch();

    let config = Config::load_from_db().await?;

    // Initialize the active theme from the persisted `ui.theme` setting.
    // Done before any TUI render so the very first frame already reflects
    // the user's choice (otherwise the welcome banner flashes default
    // colors first, then snaps). Unknown names fall back to the builtin
    // `default` inside `themes::resolve`, so a fat-fingered DB value never
    // breaks boot.
    match Config::data_dir() {
        Ok(data_dir) => {
            let resolved = themes::resolve(&config.ui.theme, &themes::user_themes_dir(&data_dir));
            themes::set_active(resolved);
        }
        Err(e) => {
            tracing::warn!(
                target: "borg::tui::themes",
                error = %e,
                "could not resolve borg data dir at startup; using builtin 'default' theme",
            );
        }
    }

    // "Update available" prompt — runs before alt-screen / daemon connect so
    // a user choosing "Update now" exits cleanly before we wire up the rest
    // of the TUI. Network and RPC failures inside the prompt are swallowed
    // (logged) so a flaky GitHub never blocks boot.
    if let update_prompt::UpdatePromptOutcome::RunUpdate =
        update_prompt::run_update_prompt_if_needed(&config).await?
    {
        println!("Updating borg...");
        match cortex::update::perform_update(false).await {
            Ok(result) => {
                println!("{}", format_install_outcome(&result));
                return Ok(None);
            }
            Err(e) => {
                eprintln!("Update failed: {e:#}");
                eprintln!("Continuing to TUI with current version.");
            }
        }
    }

    let metrics = BorgMetrics::from_config(&config);

    // Connect to (or auto-spawn) the daemon. The agent loop, hooks (vitals,
    // activity, bond, evolution, budget, script), and config watcher all live
    // inside `borgd` now — registered in `AgentBackend::new` rather than here.
    let mut client = crate::daemon_client::connect_or_spawn().await?;
    let (session_id, _last_seq) = client.open_session(resume.as_deref()).await?;

    // Resume only when explicit: `borg --resume <id>` on the CLI or `/resume
    // <id>` inside the TUI. Launching the TUI with no args always starts a
    // fresh session.
    let mut resumed_info: Option<(String, usize)> = None;
    if let Some(prefix) = resume.as_deref() {
        match cortex::session::resolve_session_id(prefix) {
            Ok(meta) => {
                let title = meta.title.clone();
                let count = meta.message_count;
                if let Err(e) = client.load_agent_session(&session_id, &meta.id).await {
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
    }

    // Heartbeat scheduling lives in the daemon now. The TUI subscribes to
    // `Admin.SubscribeHeartbeatEvents` and forwards proto events into the
    // App's existing heartbeat_rx so the rest of the rendering stays
    // identical to the in-process scheduler.
    let heartbeat_cancel = CancellationToken::new();
    let _heartbeat_guard = heartbeat_cancel.clone().drop_guard();
    let (hb_proto_tx, hb_proto_rx) = mpsc::channel::<HeartbeatEvent>(32);
    let heartbeat_rx: Option<mpsc::Receiver<HeartbeatEvent>> = Some(hb_proto_rx);
    let heartbeat_event_tx: Option<mpsc::Sender<HeartbeatEvent>> = Some(hb_proto_tx.clone());
    let poke_tx: Option<mpsc::Sender<()>> = None;
    {
        let mut sub_client = client.clone();
        let mirror_session_id = session_id.clone();
        let cancel = heartbeat_cancel.clone();
        tokio::spawn(async move {
            let mut stream = match sub_client.subscribe_heartbeat_events().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("subscribe_heartbeat_events failed: {e}");
                    return;
                }
            };
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    next = stream.message() => {
                        match next {
                            Ok(Some(evt)) => {
                                let result = proto_heartbeat_result(evt);
                                // Mirror successful heartbeats into the active TUI
                                // session BEFORE forwarding the event so the user
                                // can't beat the mirror with a fast "yes please"
                                // reply — if the next Send RPC outraced the
                                // mirror, the agent's turn would assemble its
                                // history without the heartbeat and we'd be back
                                // to the original bug.
                                if let heartbeat::scheduler::HeartbeatResult::Ran {
                                    message,
                                    ..
                                } = &result
                                {
                                    if let Err(e) = sub_client
                                        .append_assistant_message(
                                            &mirror_session_id,
                                            message,
                                        )
                                        .await
                                    {
                                        tracing::warn!(
                                            "mirror heartbeat into session {mirror_session_id}: {e}"
                                        );
                                    }
                                }
                                if hb_proto_tx
                                    .send(HeartbeatEvent::Result(result))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            Ok(None) => return,
                            Err(e) => {
                                tracing::warn!("heartbeat stream error: {e}");
                                return;
                            }
                        }
                    }
                }
            }
        });
    }

    // Goal-lifecycle subscription. Mirrors the heartbeat pattern above:
    // open the stream once at TUI startup, forward each event into the
    // App's `goal_event_rx`. The App updates `goal_status` so the footer
    // hint reflects "paused" / "budget reached" within broadcast latency
    // without polling. Initial seed comes from `GetGoal` so the footer is
    // already correct before the first event lands.
    let (goal_ui_tx, goal_ui_rx) = mpsc::channel::<crate::tui::app::GoalUiEvent>(16);
    let goal_event_rx: Option<mpsc::Receiver<crate::tui::app::GoalUiEvent>> = Some(goal_ui_rx);
    {
        let seed_client = client.clone();
        let seed_tx = goal_ui_tx.clone();
        tokio::spawn(async move {
            let mut c = seed_client;
            match c.get_goal("").await {
                Ok(detail) if detail.present => {
                    let status = cortex::goals::GoalStatus::from_str(&detail.status);
                    let _ = seed_tx
                        .send(crate::tui::app::GoalUiEvent {
                            kind: cortex::goals::event_kind::STATUS_CHANGED.to_string(),
                            status,
                        })
                        .await;
                }
                Ok(_) => {
                    let _ = seed_tx
                        .send(crate::tui::app::GoalUiEvent {
                            kind: cortex::goals::event_kind::CLEARED.to_string(),
                            status: None,
                        })
                        .await;
                }
                Err(e) => tracing::warn!("initial GetGoal seed failed: {e}"),
            }
        });
    }
    {
        let mut sub_client = client.clone();
        let cancel = heartbeat_cancel.clone();
        tokio::spawn(async move {
            let mut stream = match sub_client.subscribe_goal_events().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("subscribe_goal_events failed: {e}");
                    return;
                }
            };
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    next = stream.message() => {
                        match next {
                            Ok(Some(evt)) => {
                                let status = evt
                                    .goal
                                    .as_ref()
                                    .filter(|g| g.present)
                                    .and_then(|g| cortex::goals::GoalStatus::from_str(&g.status));
                                if goal_ui_tx
                                    .send(crate::tui::app::GoalUiEvent {
                                        kind: evt.kind,
                                        status,
                                    })
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            Ok(None) => return,
                            Err(e) => {
                                tracing::warn!("goal stream error: {e}");
                                return;
                            }
                        }
                    }
                }
            }
        });
    }

    // Auto-start gateway if enabled and any channels are installed
    let gateway_shutdown_token = CancellationToken::new();
    let _gateway_guard = gateway_shutdown_token.clone().drop_guard();
    spawn_gateway(&config, gateway_shutdown_token.clone(), metrics.clone());
    let gateway_shutdown = Arc::new(Mutex::new(gateway_shutdown_token));

    // Query terminal background before entering alt screen (some terminals
    // don't respond to the query inside alternate screen).
    colors::query_terminal_bg();

    tui_lifecycle::set_modes()?;

    // Guard ensures terminal is restored on any exit path (error or normal)
    let _guard = TerminalGuard;

    // Install panic hook that restores terminal before printing panic
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = tui_lifecycle::restore();
        original_hook(info);
    }));

    let backend = CrosstermBackend::new(stdout());
    let custom_terminal = custom_terminal::Terminal::with_options(backend)?;
    let mut tui = tui_lifecycle::Tui::new(custom_terminal)?;

    let app_client = Arc::new(Mutex::new(client.clone()));
    let mut app = App::new(
        config,
        heartbeat_rx,
        heartbeat_event_tx,
        poke_tx,
        app_client,
        goal_event_rx,
    );
    if let Some((title, count)) = resumed_info {
        app.push_system_message(format!("Resumed session: {title} ({count} messages)"));
    }
    // Show drift notice on session start (computed daemon-side from raw
    // vitals state — the cli only renders if non-empty).
    match client.get_vitals().await {
        Ok(v) => {
            if !v.drift_notice.is_empty() {
                app.push_system_message(v.drift_notice);
            }
        }
        Err(e) => tracing::warn!(error = %e, "session-start vitals fetch failed"),
    }
    // Auto-trigger first conversation if SETUP.md exists (fresh onboarding),
    // otherwise fire a throttled proactive greeting that reuses the heartbeat
    // pipeline. Throttle is shared with the scheduled heartbeat via the
    // persisted `heartbeat.last_fired_at` setting so we don't double-nudge.
    let setup_exists = Config::data_dir()
        .map(|d| d.join("SETUP.md").exists())
        .unwrap_or(false);
    if setup_exists {
        app.queued_messages.push_back(app::QueuedMessage {
            text: "Hello! I just finished onboarding.".to_string(),
            images: Vec::new(),
        });
    } else if should_fire_session_start_greeting(&app.config) {
        // Daemon owns heartbeat scheduling + execution. Ask it to fire one
        // immediately; the proto event stream forwards the result back into
        // our existing `heartbeat_rx` rendering path.
        let mut trigger_client = client.clone();
        tokio::spawn(async move {
            if let Err(e) = trigger_client
                .trigger_heartbeat("session_start", true)
                .await
            {
                tracing::warn!("session-start heartbeat trigger failed: {e}");
            }
        });
    }

    let mut event_stream = EventStream::new();
    let tick_rate = Duration::from_millis(100);

    run_event_loop(
        &mut tui,
        &mut app,
        &mut client,
        &session_id,
        &mut event_stream,
        tick_rate,
        &gateway_shutdown,
        &heartbeat_cancel,
    )
    .await
}

/// Convert the TUI's image attachments into the proto's `InlineImage` shape.
/// The wire format takes raw bytes (NOT base64) — daemon side encodes when
/// constructing the agent's multimodal message.
fn images_to_proto(images: Vec<composer::ImageAttachment>) -> Vec<protocols::session::InlineImage> {
    images
        .into_iter()
        .map(|img| protocols::session::InlineImage {
            mime_type: img.mime_type,
            data: img.data,
            filename: String::new(),
        })
        .collect()
}

/// Gate for the TUI-open proactive greeting: respects the user's enable flag
/// and the shared `heartbeat.last_fired_at` throttle so scheduled heartbeats
/// and TUI-open greetings can't nudge within the same window.
fn should_fire_session_start_greeting(config: &Config) -> bool {
    if !config.heartbeat.session_start_enabled {
        return false;
    }
    let throttle_secs = i64::from(config.heartbeat.session_start_throttle_minutes) * 60;
    if throttle_secs <= 0 {
        return true;
    }
    let now = chrono::Utc::now().timestamp();
    let last = config.heartbeat.last_fired_at;
    last <= 0 || now - last >= throttle_secs
}

fn proto_heartbeat_result(evt: PbHeartbeatEvent) -> heartbeat::scheduler::HeartbeatResult {
    match evt.status.as_str() {
        "ran" => heartbeat::scheduler::HeartbeatResult::Ran {
            message: evt.message,
            duration_ms: evt.duration_ms as u64,
        },
        "skipped" => heartbeat::scheduler::HeartbeatResult::Skipped {
            reason: proto_skip_reason(&evt.skip_reason, &evt.message),
        },
        _ => heartbeat::scheduler::HeartbeatResult::Failed { error: evt.message },
    }
}

fn proto_skip_reason(skip_reason: &str, message: &str) -> heartbeat::scheduler::SkipReason {
    match skip_reason {
        "quiet_hours" => heartbeat::scheduler::SkipReason::QuietHours,
        "duplicate_response" => heartbeat::scheduler::SkipReason::DuplicateResponse,
        "empty_response" => heartbeat::scheduler::SkipReason::EmptyResponse,
        _ => match message {
            "quiet hours" => heartbeat::scheduler::SkipReason::QuietHours,
            "duplicate response" => heartbeat::scheduler::SkipReason::DuplicateResponse,
            _ => heartbeat::scheduler::SkipReason::EmptyResponse,
        },
    }
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

/// Write a session export to the current working directory and surface the
/// result in the transcript. Errors (RPC failure, file-system failure) are
/// reported to the user — we never silently swallow.
async fn handle_export(
    app: &mut App<'_>,
    client: &mut DaemonClient,
    session_id: &str,
    format: cortex::export::ExportFormat,
) {
    let format_str = match format {
        cortex::export::ExportFormat::Json => "json",
        cortex::export::ExportFormat::Csv => "csv",
        cortex::export::ExportFormat::Txt => "txt",
    };
    let resp = match client.export_session(session_id, format_str).await {
        Ok(r) => r,
        Err(e) => {
            app.push_system_message(format!("Export failed: {e}"));
            return;
        }
    };
    let rendered = String::from_utf8_lossy(&resp.content).into_owned();
    let filename = resp.suggested_filename;
    let path = match std::env::current_dir() {
        Ok(cwd) => cwd.join(&filename),
        Err(e) => {
            app.push_system_message(format!("Export failed (cwd): {e}"));
            return;
        }
    };
    // create_new closes the TOCTOU window between exists() and write(). If the
    // filename already exists (same second stamp, rapid re-export), fail loudly
    // rather than clobber — the transcript message tells the user.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut f) => {
            use std::io::Write;
            if let Err(e) = f.write_all(rendered.as_bytes()) {
                app.push_system_message(format!("Export failed writing {}: {e}", path.display()));
                return;
            }
        }
        Err(e) => {
            app.push_system_message(format!(
                "Export aborted: could not create {} (refusing to overwrite): {e}",
                path.display()
            ));
            return;
        }
    }
    let short: String = session_id.chars().take(8).collect();
    app.push_system_message(format!("Exported session {} → {}", short, path.display()));
}

/// User-facing message after a `/update --check` (dry-run) completes.
fn format_check_outcome(current: &str, latest: &str) -> String {
    if cortex::update::is_newer(current, latest) {
        format!("Update available: {current} → {latest} (not installed; run /update to apply)")
    } else {
        format!("Already up to date ({current})")
    }
}

/// User-facing message after a `/update` install attempt completes.
fn format_install_outcome(result: &cortex::update::UpdateResult) -> String {
    match &result.status {
        cortex::update::UpdateStatus::AlreadyUpToDate => {
            format!("Already up to date ({})", result.current_version)
        }
        cortex::update::UpdateStatus::Updated { from, to } => {
            format!("Updated borg: {from} → {to}\nPlease restart borg to use the new version.")
        }
    }
}

/// Drain `app.pending_scrollback` into the terminal's native scrollback above
/// the inline viewport, then redraw the viewport at the height computed from
/// the current app state. Single source of truth for rendering — every call
/// site in the event loop goes through here.
fn draw_viewport(tui: &mut tui_lifecycle::Tui, app: &mut App<'_>) -> Result<()> {
    // Push any newly-stable cells (e.g. the User cell pushed by
    // `handle_submit`) to terminal scrollback BEFORE drawing. The inline
    // viewport's live tail is bounded — without this flush, freshly-pushed
    // user messages stay clipped inside the small viewport area until the
    // first agent event arrives, producing a multi-second perceived lag
    // between hitting Enter and seeing the message.
    let flush_width = if app.last_viewport_width > 0 {
        app.last_viewport_width
    } else {
        tui.terminal_size()?.width.max(1)
    };
    app.flush_stable_cells(flush_width);
    let pending = std::mem::take(&mut app.pending_scrollback);
    tui.insert_history_lines(pending);
    let viewport_height = app.compute_viewport_height(tui.terminal_size()?);
    tui.draw(viewport_height, |frame| app.render_viewport(frame))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_event_loop(
    tui: &mut tui_lifecycle::Tui,
    app: &mut App<'_>,
    client: &mut DaemonClient,
    session_id: &str,
    event_stream: &mut EventStream,
    tick_rate: Duration,
    gateway_shutdown: &Arc<Mutex<CancellationToken>>,
    heartbeat_cancel: &CancellationToken,
) -> Result<Option<ResumeHint>> {
    let mut tick_interval = tokio::time::interval(tick_rate);

    loop {
        draw_viewport(tui, app)?;

        let action = tokio::select! {
            biased;

            // Terminal events FIRST under `biased`. A hot LLM stream keeps
            // `event_rx` perpetually ready; if the agent-event arm sits above
            // this one, biased select picks it every iteration and Esc never
            // reaches `handle_key` — the user-visible "esc to interrupt does
            // nothing" bug. Keys are low-volume; an agent-event burst can
            // tolerate one extra select-cycle. INVARIANT: this arm must stay
            // above `app.event_rx`. A source-grep guard test in this file
            // enforces the ordering.
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => app.handle_key(key)?,
                    Some(Ok(Event::Paste(text))) => app.handle_paste(text),
                    // Intentionally NO `Event::Mouse` arm. We never enable
                    // any mouse tracking mode (see `tui_lifecycle::set_modes`),
                    // so no mouse events arrive. Inline-viewport mode delegates
                    // scrollback to the terminal; alt-screen surfaces enable
                    // xterm ?1007h so wheel events arrive as Up/Down keys.
                    // DO NOT re-introduce a mouse handler — it would require
                    // ?1000h/?1002h/?1003h, all of which break text selection.
                    Some(Ok(Event::Resize(_, _))) => AppAction::Continue,
                    Some(Err(_)) | None => AppAction::Quit,
                    _ => AppAction::Continue,
                }
            }

            // Agent events
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

            // Goal-lifecycle events (footer indicator only).
            Some(evt) = async {
                if let Some(rx) = &mut app.goal_event_rx {
                    rx.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                app.apply_goal_event(evt);
                AppAction::Continue
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
                        // Daemon now schedules + runs heartbeats; the proto
                        // forwarder only emits Result events. A Fire here is
                        // unexpected — log and ignore rather than spawning a
                        // local turn (which would race the daemon).
                        tracing::warn!(
                            "ignoring HeartbeatEvent::Fire in TUI; daemon owns scheduling"
                        );
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

            // /btw background task results
            event = async {
                if let Some(rx) = &mut app.btw_result_rx {
                    rx.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                if let Some(result) = event {
                    app.process_btw_result(result);
                }
                AppAction::Continue
            }

            // Voice-mode lifecycle events. The recorder publishes
            // Status/Transcript/SilentLimitReached here; a Transcript
            // with `voice.auto_submit` becomes a normal SendMessage so
            // we go through the same persistence and agent dispatch
            // path the keyboard composer uses.
            voice_ev = async {
                if let Some(rx) = &mut app.voice_rx {
                    rx.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                app.process_voice_event(voice_ev)?
            }

            // Tick for status bar elapsed time + throbber animation + paste burst flush
            _ = tick_interval.tick() => {
                app.tick_throbber();
                app.tick_paste_burst();
                app.tick_file_search();
                drain_queued_if_idle(app)?
            }
        };

        match action {
            AppAction::Quit => {
                // Release the cpal stream and any in-flight playback so
                // CoreAudio/ALSA are clean before the TUI tears down.
                app.teardown_voice_loop();
                // Browser-close is daemon-side now; no TUI hook needed.
                let hint = match client.get_state(session_id).await {
                    Ok(s) if s.message_count > 0 => Some(ResumeHint {
                        short_id: s.agent_session_id.chars().take(8).collect(),
                        title: s.title,
                    }),
                    Ok(_) => None,
                    Err(e) => {
                        tracing::warn!(error = %e, "GetState on quit failed; skipping resume hint");
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
                draw_viewport(tui, app)?;
                // Steer channel: when the user types into a streaming turn,
                // App routes the text to `steer_tx`; this task forwards each
                // message to the daemon via `Session.Steer`.
                let (steer_tx, mut steer_rx) = tokio::sync::mpsc::unbounded_channel();
                app.steer_tx = Some(steer_tx);
                app.pending_steers.clear();

                {
                    let mut steer_client = client.clone();
                    let sid = session_id.to_string();
                    tokio::spawn(async move {
                        while let Some(text) = steer_rx.recv().await {
                            if let Err(e) = steer_client.steer(&sid, &text).await {
                                tracing::warn!(error = %e, "Session.Steer failed");
                            }
                        }
                    });
                }

                // Cancel the in-flight turn if the user hits the cancel hot
                // path. The token is already shared with the App; we just
                // bridge it to the daemon's `Session.Cancel` here.
                {
                    let mut cancel_client = client.clone();
                    let sid = session_id.to_string();
                    let cancel_for_task = cancel.clone();
                    tokio::spawn(async move {
                        cancel_for_task.cancelled().await;
                        if let Err(e) = cancel_client.cancel(&sid).await {
                            tracing::warn!(error = %e, "Session.Cancel failed");
                        }
                    });
                }

                let mut send_client = client.clone();
                let sid = session_id.to_string();
                let proto_images = images_to_proto(images);
                tokio::spawn(async move {
                    match send_client
                        .send_message_with_images(&sid, &input, proto_images)
                        .await
                    {
                        Ok(stream) => {
                            forward_event_stream(send_client, sid, stream, event_tx).await;
                        }
                        Err(e) => {
                            let _ = event_tx.send(AgentEvent::Error(format!("{e:#}"))).await;
                        }
                    }
                });
            }
            AppAction::ForkSession => {
                // Forking writes to SQLite (new session row + copied messages).
                // The daemon is the single writer — do NOT open the DB here.
                match client.fork_session(session_id).await {
                    Ok(forked) => {
                        let id = forked.forked_agent_session_id;
                        let short: String = id.chars().take(8).collect();
                        app.push_system_message(format!(
                            "Forked {} messages → {} ({short})",
                            forked.message_count, forked.title
                        ));
                        // Reuse the LoadSession path so daemon state, FTS, and
                        // popups all converge on the new session id.
                        app.cells.clear();
                        app.session_prompt_tokens = 0;
                        app.session_completion_tokens = 0;
                        app.last_prompt_tokens = 0;
                        if let Err(e) = client.load_agent_session(session_id, &id).await {
                            app.push_system_message(format!("Forked, but switching failed: {e}"));
                        }
                    }
                    Err(e) => app.push_system_message(format!("Fork failed: {e}")),
                }
            }
            AppAction::CompactHistory => match client.compact(session_id).await {
                Ok(r) => {
                    let dropped = r.dropped as usize;
                    let after = r.after_tokens as usize;
                    app.push_system_message(format!(
                        "Compacted: {dropped} messages dropped, ~{after} tokens remain"
                    ));
                }
                Err(e) => app.push_system_message(format!("Compact failed: {e}")),
            },
            AppAction::ClearHistory => match client.clear_history(session_id).await {
                Ok(()) => {
                    if let Err(e) = tui.clear_terminal_scrollback() {
                        tracing::warn!(error = %e, "clear terminal scrollback failed");
                    }
                    app.push_system_message("Conversation cleared.".to_string());
                }
                Err(e) => app.push_system_message(format!("Clear failed: {e}")),
            },
            AppAction::ClearTranscript => {
                if let Err(e) = tui.clear_terminal_scrollback() {
                    tracing::warn!(error = %e, "clear terminal scrollback failed");
                }
            }
            AppAction::UndoLastTurn => match client.undo(session_id).await {
                Ok(r) => {
                    let removed = r.removed as usize;
                    if removed > 0 {
                        app.push_system_message(format!(
                            "Undid last turn ({removed} messages removed)."
                        ));
                    } else {
                        app.push_system_message("Nothing to undo.".to_string());
                    }
                }
                Err(e) => app.push_system_message(format!("Undo failed: {e}")),
            },
            AppAction::RewindTo { nth_user_message } => {
                match client.rewind(session_id, nth_user_message as u64).await {
                    Ok(r) => {
                        let removed = r.removed as usize;
                        if removed > 0 {
                            app.push_system_message(format!(
                                "Rewound conversation ({removed} messages removed). Edit and re-send."
                            ));
                        } else {
                            app.push_system_message("Nothing to rewind.".to_string());
                        }
                    }
                    Err(e) => app.push_system_message(format!("Rewind failed: {e}")),
                }
            }
            AppAction::UpdateSetting { key, value } => {
                match client.set_setting(&key, &value).await {
                    Ok(()) => {
                        // Theme is the one setting that demands a live
                        // process-side side effect: every render reads
                        // `themes::active()`, so the swap has to land
                        // before the next frame. Anything else (model,
                        // temperature, …) is consumed by the daemon, not
                        // by the TUI render path. Only swap on a
                        // successful DB write — otherwise the next
                        // restart would resurrect the old theme, leaving
                        // the user's "did it take?" intuition broken.
                        if key == "ui.theme" {
                            match cortex::config::Config::data_dir() {
                                Ok(data_dir) => {
                                    let resolved = themes::resolve(
                                        &value,
                                        &themes::user_themes_dir(&data_dir),
                                    );
                                    themes::set_active(resolved);
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        target: "borg::tui::themes",
                                        error = %e,
                                        "could not resolve borg data dir; theme persisted to DB but live-swap skipped",
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        app.push_system_message(format!("Warning: set_setting failed: {e}"));
                    }
                }
            }
            AppAction::ConfigReloaded => {
                // Config now lives in the daemon — no in-process mutation to
                // propagate. The daemon's config watcher fans out reloads.
            }
            AppAction::SaveSession => {
                if let Err(e) = client.auto_save(session_id).await {
                    app.push_system_message(format!("Save failed: {e}"));
                } else {
                    match client.get_state(session_id).await {
                        Ok(s) => {
                            app.push_system_message(format!(
                                "Session saved: {} ({})",
                                s.title, s.agent_session_id
                            ));
                        }
                        Err(e) => {
                            app.push_system_message(format!("Saved, but GetState failed: {e}"))
                        }
                    }
                }
            }
            AppAction::NewSession => {
                if let Err(e) = client.new_agent_session(session_id).await {
                    app.push_system_message(format!("New session failed: {e}"));
                } else {
                    app.push_system_message("New session started.".to_string());
                }
            }
            AppAction::ExportSession { id, format } => {
                handle_export(app, client, &id, format).await;
            }
            AppAction::ExportCurrentSession { format } => {
                let id = match client.get_state(session_id).await {
                    Ok(s) => s.agent_session_id,
                    Err(e) => {
                        app.push_system_message(format!("Export failed: {e}"));
                        continue;
                    }
                };
                handle_export(app, client, &id, format).await;
            }
            AppAction::LoadSession { id } => {
                // Clear existing UI state before loading a new session.
                app.cells.clear();
                app.session_prompt_tokens = 0;
                app.session_completion_tokens = 0;
                app.last_prompt_tokens = 0;
                // Support partial ID matching (prefix) via shared resolver.
                let load_id = match cortex::session::resolve_session_id(&id) {
                    Ok(meta) => meta.id,
                    Err(cortex::session::ResolveSessionError::Ambiguous { count, .. }) => {
                        app.push_system_message(format!(
                            "Ambiguous session ID '{id}' — matches {count} sessions. Be more specific."
                        ));
                        continue;
                    }
                    // NotFound / Empty: fall through to load_agent_session which
                    // will surface a clean "Session '<id>' not found" error.
                    Err(_) => id.clone(),
                };

                match client.load_agent_session(session_id, &load_id).await {
                    Ok(_) => match client.get_state(session_id).await {
                        Ok(s) => {
                            app.push_system_message(format!(
                                "Loaded session: {} ({} messages)",
                                s.title, s.message_count
                            ));
                        }
                        Err(e) => {
                            app.push_system_message(format!("Loaded, but GetState failed: {e}"));
                        }
                    },
                    Err(e) => {
                        app.push_system_message(format!("Failed to load session: {e}"));
                    }
                }
            }
            AppAction::LaunchExternalEditor => {
                let current_text = app.composer.text();
                // Inline-viewport mode: the external editor takes the full
                // screen below the viewport naturally — no alt-screen bounce
                // needed. Pop input modes so the editor sees a normal TTY,
                // then re-arm them when control returns.
                let _ = tui_lifecycle::restore();

                let result = external_editor::open_external_editor(&current_text);

                let _ = tui_lifecycle::set_modes();
                // Force a full repaint after the editor's screen output.
                tui.terminal.invalidate_viewport();

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
                let data_dir = match cortex::config::Config::data_dir() {
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
                            if let Some(def) = plugins::catalog::find_by_id(&id) {
                                match plugins::installer::install(
                                    def,
                                    &data_dir,
                                    &credentials,
                                    None,
                                )
                                .await
                                {
                                    Ok(install_result) => {
                                        // Resolve item_name for installed tool/channel.
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

                                        // Build credentials JSON (cli holds the
                                        // keychain ref types — daemon just persists
                                        // the resulting blob to the settings table).
                                        let credentials_json = if install_result
                                            .credential_entries
                                            .is_empty()
                                        {
                                            String::new()
                                        } else {
                                            let mut cfg =
                                                Config::load_from_db().await.unwrap_or_default();
                                            for entry in &install_result.credential_entries {
                                                cfg.credentials.insert(
                                                        entry.key.clone(),
                                                        cortex::config::CredentialValue::Ref(
                                                            cortex::security::secrets_resolve::SecretRef::Keychain {
                                                                service: entry.service.clone(),
                                                                account: entry.account.clone(),
                                                            },
                                                        ),
                                                    );
                                            }
                                            serde_json::to_string(&cfg.credentials)
                                                .unwrap_or_default()
                                        };

                                        let mut req = protocols::admin::PluginInstallRequest {
                                            plugin_id: def.id.to_string(),
                                            plugin_name: def.name.to_string(),
                                            kind: def.kind.to_string(),
                                            category: def.category.to_string(),
                                            file_hashes: install_result
                                                .file_hashes
                                                .iter()
                                                .map(|(p, h)| protocols::admin::PluginFileHash {
                                                    path: p.clone(),
                                                    hash: h.clone(),
                                                })
                                                .collect(),
                                            installed_tool: None,
                                            installed_channel: None,
                                            credentials_json,
                                        };
                                        match def.kind {
                                            plugins::PluginKind::Tool => {
                                                req.installed_tool =
                                                    Some(protocols::admin::PluginInstalledTool {
                                                        name: item_name.to_string(),
                                                        description: def.description.to_string(),
                                                        runtime: runtime.to_string(),
                                                        plugin_id: def.id.to_string(),
                                                    });
                                            }
                                            plugins::PluginKind::Channel => {
                                                let webhook = format!("/webhook/{item_name}");
                                                req.installed_channel = Some(
                                                    protocols::admin::PluginInstalledChannel {
                                                        name: item_name.to_string(),
                                                        description: def.description.to_string(),
                                                        runtime: runtime.to_string(),
                                                        plugin_id: def.id.to_string(),
                                                        webhook,
                                                    },
                                                );
                                            }
                                        }

                                        if let Err(e) = client.plugin_install(req).await {
                                            tracing::warn!("Admin.PluginInstall failed: {e}");
                                            results.push(format!(
                                                "Failed to record {} install: {e}",
                                                def.name
                                            ));
                                            continue;
                                        }

                                        // Reload config to pick up newly saved credentials
                                        match cortex::config::Config::load_from_db().await {
                                            Ok(c) => app.config = c,
                                            Err(e) => {
                                                tracing::warn!(
                                                    "Failed to reload config after plugin install: {e}"
                                                );
                                            }
                                        }

                                        let is_channel = def.kind == plugins::PluginKind::Channel;
                                        if is_channel {
                                            let gw_msg = restart_gateway(gateway_shutdown).await;
                                            tracing::info!(
                                                "Gateway restart after installing {}: {gw_msg}",
                                                def.name
                                            );
                                        }
                                        results.push(build_post_install_message(
                                            def.name,
                                            &install_result.notes,
                                            is_channel,
                                        ));
                                    }
                                    Err(e) => {
                                        results
                                            .push(format!("Failed to install {}: {e}", def.name));
                                    }
                                }
                            }
                        }
                        plugins_popup::PluginAction::SetSkillEnabled { name, enabled } => {
                            let key = format!("skills.entries.{name}.enabled");
                            let val = enabled.to_string();
                            match app.config.apply_setting(&key, &val) {
                                Ok(_) => match client.set_setting(&key, &val).await {
                                    Ok(()) => {
                                        let status = if enabled { "enabled" } else { "disabled" };
                                        results.push(format!("{name}: {status}"));
                                    }
                                    Err(e) => {
                                        results.push(format!("Error persisting {name}: {e}"));
                                    }
                                },
                                Err(e) => {
                                    results.push(format!("Error updating {name}: {e}"));
                                }
                            }
                        }
                        plugins_popup::PluginAction::SetWasmPluginDisabled {
                            plugin_id,
                            disabled,
                        } => {
                            // The popup never emits this for a quarantined row,
                            // so we only handle the verified ↔ disabled flip.
                            let setting_key = "plugins.wasm.disabled_plugin_ids";
                            let mut next_list = app.config.plugins.wasm.disabled_plugin_ids.clone();
                            if disabled {
                                if !next_list.iter().any(|p| p == &plugin_id) {
                                    next_list.push(plugin_id.clone());
                                }
                            } else {
                                next_list.retain(|p| p != &plugin_id);
                            }
                            let json = match serde_json::to_string(&next_list) {
                                Ok(s) => s,
                                Err(e) => {
                                    results.push(format!("Failed to serialize disabled list: {e}"));
                                    continue;
                                }
                            };
                            match app.config.apply_setting(setting_key, &json) {
                                Ok(_) => match client.set_setting(setting_key, &json).await {
                                    Ok(()) => {
                                        // Also flip the DB row status so a
                                        // restart with no setting reload still
                                        // honors the disable.
                                        let db_outcome = match cortex::db::Database::open().await {
                                            Ok(db) => {
                                                if disabled {
                                                    db.disable_wasm_plugin(&plugin_id).await
                                                } else {
                                                    db.enable_wasm_plugin(&plugin_id).await
                                                }
                                            }
                                            Err(e) => Err(e),
                                        };
                                        if let Err(e) = db_outcome {
                                            results.push(format!(
                                                "{plugin_id}: persist status failed: {e}"
                                            ));
                                            continue;
                                        }
                                        let event = if disabled {
                                            cortex::db::audit_events::WASM_PLUGIN_DISABLED
                                        } else {
                                            cortex::db::audit_events::WASM_PLUGIN_ENABLED
                                        };
                                        cortex::db::try_emit_global(
                                            event,
                                            &plugin_id,
                                            &serde_json::json!({
                                                "plugin_id": &plugin_id,
                                                "scope": "plugin",
                                            }),
                                        )
                                        .await;
                                        let verb = if disabled { "disabled" } else { "enabled" };
                                        results.push(format!("{plugin_id}: {verb}"));
                                    }
                                    Err(e) => {
                                        results.push(format!(
                                            "Error persisting WASM disable list: {e}"
                                        ));
                                    }
                                },
                                Err(e) => {
                                    results.push(format!("Error updating WASM disable list: {e}"));
                                }
                            }
                        }
                        plugins_popup::PluginAction::Uninstall { id } => {
                            if let Some(def) = plugins::catalog::find_by_id(&id) {
                                // Remove keychain entries
                                if let Err(e) = plugins::installer::uninstall(def, &data_dir) {
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
                                if def.kind == plugins::PluginKind::Channel {
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
                    .unwrap_or(CollaborationMode::Execute);
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

                    if let Err(e) = client.clear_history(session_id).await {
                        tracing::warn!(error = %e, "PlanProceed clear_history failed");
                    }
                    app.cells.clear();
                    app.session_prompt_tokens = 0;
                    app.session_completion_tokens = 0;
                    app.last_prompt_tokens = 0;

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
                for action in actions {
                    match action {
                        schedule_popup::ScheduleAction::ToggleStatus {
                            task_id,
                            new_status,
                        } => {
                            let short = &task_id[..8.min(task_id.len())];
                            let res = if new_status == "paused" {
                                client.pause_scheduled_task(&task_id).await
                            } else {
                                client.resume_scheduled_task(&task_id).await
                            };
                            match res {
                                Ok(()) => {
                                    results.push(format!("Task {short} {new_status}"));
                                }
                                Err(e) => results.push(format!("Error: {e}")),
                            }
                        }
                        schedule_popup::ScheduleAction::UpdateSchedule {
                            task_id,
                            schedule_type,
                            new_expr,
                        } => {
                            let short = &task_id[..8.min(task_id.len())];
                            let spec = protocols::admin::ScheduledTaskUpdate {
                                id: task_id.clone(),
                                name: String::new(),
                                prompt: String::new(),
                                schedule_type: schedule_type.clone(),
                                schedule_expr: new_expr.clone(),
                                timezone: String::new(),
                            };
                            match client.update_scheduled_task(spec).await {
                                Ok(_) => results
                                    .push(format!("Task {short} schedule updated to {new_expr}")),
                                Err(e) => results.push(format!("Error: {e}")),
                            }
                        }
                        schedule_popup::ScheduleAction::DeleteTask { task_id } => {
                            let short = &task_id[..8.min(task_id.len())];
                            match client.delete_scheduled_task(&task_id).await {
                                Ok(()) => results.push(format!("Task {short} deleted")),
                                Err(e) => results.push(format!("Error: {e}")),
                            }
                        }
                    }
                }
                if !results.is_empty() {
                    app.push_system_message(results.join("\n"));
                }
            }
            AppAction::RunPairingActions { actions } => {
                let mut results: Vec<String> = Vec::new();
                for action in actions {
                    match action {
                        pairing_popup::PairingAction::Approve { channel, code } => {
                            match client.approve_pairing(&channel, &code).await {
                                Ok(()) => {
                                    let display = cortex::pairing::channel_display_name(&channel);
                                    results.push(format!("Approved: {code} on {display}"));
                                    // Fire-and-forget greeting via the daemon's
                                    // Admin.SendApprovalGreeting RPC.
                                    let mut greet_client = client.clone();
                                    let ch = channel.clone();
                                    tokio::spawn(async move {
                                        let senders = match greet_client
                                            .list_approved_senders(Some(&ch))
                                            .await
                                        {
                                            Ok(list) => list.senders,
                                            Err(e) => {
                                                tracing::warn!(
                                                    error = %e,
                                                    "approval greeting: list_approved_senders failed"
                                                );
                                                return;
                                            }
                                        };
                                        if let Some(sender) =
                                            senders.iter().max_by_key(|s| s.approved_at)
                                        {
                                            if let Err(e) = greet_client
                                                .send_approval_greeting(&ch, &sender.sender_id)
                                                .await
                                            {
                                                tracing::warn!(
                                                    error = %e,
                                                    "Admin.SendApprovalGreeting failed"
                                                );
                                            }
                                        }
                                    });
                                }
                                Err(e) => {
                                    results.push(format!("Failed to approve: {e}"));
                                }
                            }
                        }
                        pairing_popup::PairingAction::Revoke { channel, sender_id } => match client
                            .revoke_approved_sender(&channel, &sender_id)
                            .await
                        {
                            Ok(()) => {
                                let display = cortex::pairing::channel_display_name(&channel);
                                results.push(format!("Revoked sender {sender_id} from {display}."));
                            }
                            Err(e) => {
                                results.push(format!("Failed to revoke: {e}"));
                            }
                        },
                    }
                }
                if !results.is_empty() {
                    app.push_system_message(results.join("\n"));
                }
            }
            AppAction::RunMigration { actions } => {
                for action in actions {
                    match action {
                        migrate_popup::MigrateAction::Apply { plan, source_data } => {
                            let borg_dir = match cortex::config::Config::data_dir() {
                                Ok(d) => d,
                                Err(e) => {
                                    app.push_system_message(format!("Migration error: {e}"));
                                    continue;
                                }
                            };
                            match cortex::migrate::apply::apply_plan(&plan, &source_data, &borg_dir)
                                .await
                            {
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
                                    let mut log_client = client.clone();
                                    let log_msg = msg.clone();
                                    tokio::spawn(async move {
                                        if let Err(e) = log_client
                                            .log_activity("info", "migrate", &log_msg)
                                            .await
                                        {
                                            tracing::warn!("Admin.LogActivity failed: {e}");
                                        }
                                    });
                                    app.push_system_message(msg);
                                    // Reload config to pick up migrated settings
                                    app.config = cortex::config::Config::load_from_db()
                                        .await
                                        .unwrap_or_default();
                                }
                                Err(e) => {
                                    let msg = format!("Migration failed: {e}");
                                    let mut log_client = client.clone();
                                    let log_msg = msg.clone();
                                    tokio::spawn(async move {
                                        if let Err(e) = log_client
                                            .log_activity("error", "migrate", &log_msg)
                                            .await
                                        {
                                            tracing::warn!("Admin.LogActivity failed: {e}");
                                        }
                                    });
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
            AppAction::SelfUpdate { prerelease, check } => {
                app.push_system_message(format!(
                    "Checking for updates{}...",
                    if prerelease {
                        " (including pre-releases)"
                    } else {
                        ""
                    }
                ));
                draw_viewport(tui, app)?;
                if check {
                    match cortex::update::fetch_latest_release(prerelease).await {
                        Ok(release) => {
                            let latest = release
                                .tag_name
                                .strip_prefix('v')
                                .unwrap_or(&release.tag_name);
                            let current = cortex::update::current_version();
                            app.push_system_message(format_check_outcome(current, latest));
                        }
                        Err(e) => {
                            app.push_system_message(format!("Update check failed: {e:#}"));
                        }
                    }
                } else {
                    match cortex::update::perform_update(prerelease).await {
                        Ok(result) => {
                            if let cortex::update::UpdateStatus::Updated { to, .. } = &result.status
                            {
                                app.pending_update_version = Some(to.clone());
                            }
                            app.push_system_message(format_install_outcome(&result));
                        }
                        Err(e) => {
                            app.push_system_message(format!("Update failed: {e:#}"));
                        }
                    }
                }
            }
            AppAction::Uninstall => {
                // Step 1: Uninstall daemon service + kill any lingering borg processes
                app.push_system_message("Stopping daemon...".to_string());
                draw_viewport(tui, app)?;
                if let Err(e) = crate::borgd_launcher::uninstall_service() {
                    tracing::debug!("Service uninstall skipped: {e}");
                }
                crate::borgd_launcher::kill_other_borg_processes();

                // Step 2: Stop all in-process background tasks (gateway, heartbeat)
                // so they can't reopen / recreate the database after we delete it.
                heartbeat_cancel.cancel();
                {
                    if let Ok(gw) = gateway_shutdown.try_lock() {
                        gw.cancel();
                    }
                }
                // Browser + DB are owned by the daemon now; ask it to shut
                // down so the data directory can be removed cleanly. A proper
                // `Admin.Shutdown` call will replace the no-op below; for now
                // sandboxed Uninstall surfaces any file-locked errors that
                // result.
                // Let spawned tasks notice cancellation and wind down
                tokio::task::yield_now().await;

                // Step 3: Data directory
                app.push_system_message("Removing data directory (~/.borg/)...".to_string());
                draw_viewport(tui, app)?;
                if let Ok(data_dir) = cortex::config::Config::data_dir() {
                    if let Err(e) = std::fs::remove_dir_all(&data_dir) {
                        tracing::warn!("Failed to remove data dir: {e}");
                        app.push_system_message(format!("Warning: failed to remove data dir: {e}"));
                        draw_viewport(tui, app)?;
                    }
                }

                // Step 4: Binary
                app.push_system_message("Removing binary...".to_string());
                draw_viewport(tui, app)?;
                if let Ok(exe) = std::env::current_exe() {
                    let exe = exe.canonicalize().unwrap_or(exe);
                    if let Err(e) = std::fs::remove_file(&exe) {
                        tracing::debug!("Could not remove binary: {e}");
                    }
                }

                // Fullscreen ASCII art goodbye
                goodbye::render(tui)?;
                tokio::time::sleep(Duration::from_secs(3)).await;

                // Final sweep: delete data dir again in case a background task
                // recreated it (e.g. Database::open creates ~/.borg/ via create_dir_all)
                if let Ok(data_dir) = cortex::config::Config::data_dir() {
                    let _ = std::fs::remove_dir_all(&data_dir);
                }
                return Ok(None);
            }
            AppAction::Poke => {
                let mut poke_client = client.clone();
                tokio::spawn(async move {
                    if let Err(e) = poke_client.poke().await {
                        tracing::warn!("Admin.Poke failed: {e}");
                    }
                });
                let borg_name = app
                    .config
                    .user
                    .agent_name
                    .clone()
                    .unwrap_or_else(|| "Borg".to_string());
                app.push_system_message(format!("*poked {borg_name}*"));
            }
            AppAction::RunDoctor => {
                app.push_system_message("Borg Doctor\n───────────".to_string());
                let (tx, rx) = tokio::sync::mpsc::channel(32);
                app.doctor_rx = Some(rx);
                let config = app.config.clone();
                // Doctor step runner is now async (DB-backed checks). Drive it
                // on a regular tokio task so the async DB calls have a runtime.
                tokio::spawn(async move {
                    use cortex::doctor::DiagnosticRunner;
                    let mut runner = DiagnosticRunner::new();
                    let mut all_checks: Vec<cortex::doctor::DiagnosticCheck> = Vec::new();
                    while let Some(label) = runner.peek_label(&config) {
                        let _ = tx
                            .send(app::DoctorEvent::Analyzing {
                                label: label.to_string(),
                            })
                            .await;
                        if let Some((_label, checks)) = runner.next_step(&config).await {
                            all_checks.extend(checks.clone());
                            let _ = tx
                                .send(app::DoctorEvent::Result {
                                    label: label.to_string(),
                                    checks,
                                })
                                .await;
                        }
                    }
                    let report = cortex::doctor::DiagnosticReport { checks: all_checks };
                    let (pass, warn, fail) = report.counts();
                    let _ = tx.send(app::DoctorEvent::Done { pass, warn, fail }).await;
                });
            }
            AppAction::StartBtw { question, snapshot } => {
                // Cancel any previous in-flight /btw — single-slot policy.
                // The old task's result will arrive at the popup and be
                // discarded by process_btw_result because the popup's
                // Loading question will have moved on.
                if let Some(prev) = app.btw_cancel.take() {
                    prev.cancel();
                }
                // Lazily create the result channel on first use. Kept on the
                // App so subsequent /btw runs reuse the same receiver.
                if app.btw_result_tx.is_none() {
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<app::BtwResult>();
                    app.btw_result_tx = Some(tx);
                    app.btw_result_rx = Some(rx);
                }
                let tx = match app.btw_result_tx.as_ref() {
                    Some(t) => t.clone(),
                    None => {
                        tracing::warn!("/btw: result channel missing — dropping request");
                        return Ok(None);
                    }
                };
                let cancel = tokio_util::sync::CancellationToken::new();
                app.btw_cancel = Some(cancel.clone());
                let config = app.config.clone();
                let q = question.clone();
                tokio::spawn(async move {
                    let outcome = cortex::agent::run_btw(config, q.clone(), snapshot, cancel)
                        .await
                        .map_err(|e| format!("{e:#}"));
                    if tx
                        .send(app::BtwResult {
                            question: q,
                            outcome,
                        })
                        .is_err()
                    {
                        tracing::debug!("/btw: result receiver dropped before completion");
                    }
                });
            }
            // EnterAltScreen / LeaveAltScreen are intentionally absorbed here
            // and applied via the reconciliation step below. Going through the
            // single reconciliation path keeps `tui.enter_alt_screen` (which
            // saves the inline viewport) from being called twice in a row when
            // an explicit emit is followed by the reconcile pass.
            AppAction::EnterAltScreen | AppAction::LeaveAltScreen => {}
            AppAction::Continue => {}
        }

        // Reconcile alt-screen state with the App's desired surface. Single
        // source of truth: any popup-show / pager-dismiss path that mutates
        // visibility flips this on the next frame, no matter where it lives
        // (slash commands, popup-internal key handlers, agent events).
        let want_alt = app.wants_alt_screen();
        let have_alt = tui.is_alt_screen_active();
        if want_alt && !have_alt {
            tui.enter_alt_screen()?;
        } else if !want_alt && have_alt {
            tui.leave_alt_screen()?;
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

    /// Forbidden escape-sequence modes. Presence of ANY of these in the code
    /// paths that emit ANSI to the terminal will break native text selection.
    const FORBIDDEN_MODES: &[&str] = &["?1000", "?1002", "?1003", "?1006"];

    // ------------------------------------------------------------------------
    // Post-install message composition
    //
    // Regression guard tied to a real user complaint: the UI previously said
    // "Gateway restarted." (noise — moved to tracing::info!) and
    // "Approve senders with: /pairing approve <CODE>" (confusing — didn't
    // explain the user must DM the bot first to generate the code).
    // ------------------------------------------------------------------------

    // ------------------------------------------------------------------------
    // /update outcome formatting
    // ------------------------------------------------------------------------

    #[test]
    fn format_check_outcome_already_up_to_date() {
        let msg = format_check_outcome("0.5.0", "0.5.0");
        assert_eq!(msg, "Already up to date (0.5.0)");
    }

    #[test]
    fn format_check_outcome_update_available() {
        let msg = format_check_outcome("0.5.0", "0.6.0");
        assert!(
            msg.starts_with("Update available: 0.5.0 → 0.6.0"),
            "missing version arrow: {msg}"
        );
        assert!(
            msg.contains("not installed"),
            "must clarify dry-run did not install: {msg}"
        );
    }

    #[test]
    fn format_check_outcome_does_not_promote_downgrade() {
        // is_newer is false when "latest" is older — message should be "up to date",
        // never offer a downgrade.
        let msg = format_check_outcome("1.0.0", "0.9.0");
        assert!(msg.contains("Already up to date"), "{msg}");
        assert!(!msg.contains("Update available"), "{msg}");
    }

    #[test]
    fn format_install_outcome_already_up_to_date() {
        let result = cortex::update::UpdateResult {
            current_version: "0.5.0".into(),
            latest_version: "0.5.0".into(),
            status: cortex::update::UpdateStatus::AlreadyUpToDate,
        };
        assert_eq!(
            format_install_outcome(&result),
            "Already up to date (0.5.0)"
        );
    }

    #[test]
    fn format_install_outcome_updated_includes_restart_prompt() {
        let result = cortex::update::UpdateResult {
            current_version: "0.5.0".into(),
            latest_version: "0.6.0".into(),
            status: cortex::update::UpdateStatus::Updated {
                from: "0.5.0".into(),
                to: "0.6.0".into(),
            },
        };
        let msg = format_install_outcome(&result);
        assert!(msg.contains("0.5.0 → 0.6.0"), "{msg}");
        assert!(
            msg.contains("restart"),
            "user must be told to restart: {msg}"
        );
    }

    #[test]
    fn post_install_message_does_not_mention_gateway_restarted() {
        let msg = build_post_install_message("Telegram", &[], true);
        assert!(
            !msg.contains("Gateway restarted"),
            "user-facing install message must not mention gateway restart (log-only): {msg}"
        );
    }

    #[test]
    fn post_install_channel_message_explains_pairing_workflow() {
        let msg = build_post_install_message("Telegram", &[], true);
        assert!(
            msg.contains("Installed Telegram"),
            "missing install line: {msg}"
        );
        assert!(
            msg.contains("Send a message to the bot on Telegram"),
            "channel install must tell user to DM the bot first: {msg}"
        );
        assert!(
            msg.contains("/pairing approve <CODE>"),
            "channel install must include the /pairing approve command: {msg}"
        );
    }

    #[test]
    fn post_install_non_channel_message_omits_pairing_hint() {
        let msg = build_post_install_message("Gmail", &["foo".to_string()], false);
        assert!(msg.starts_with("Installed Gmail"));
        assert!(msg.contains("foo"), "notes should be preserved: {msg}");
        assert!(
            !msg.contains("/pairing"),
            "non-channel plugins don't need pairing instructions: {msg}"
        );
    }

    #[test]
    fn session_start_disabled_never_fires() {
        let mut config = Config::default();
        config.heartbeat.session_start_enabled = false;
        config.heartbeat.last_fired_at = 0;
        assert!(!should_fire_session_start_greeting(&config));
    }

    #[test]
    fn session_start_fires_when_never_fired() {
        let mut config = Config::default();
        config.heartbeat.session_start_enabled = true;
        config.heartbeat.session_start_throttle_minutes = 30;
        config.heartbeat.last_fired_at = 0;
        assert!(should_fire_session_start_greeting(&config));
    }

    #[test]
    fn session_start_suppressed_inside_throttle_window() {
        let mut config = Config::default();
        config.heartbeat.session_start_enabled = true;
        config.heartbeat.session_start_throttle_minutes = 30;
        // Fired 5 minutes ago — inside the 30-minute window.
        config.heartbeat.last_fired_at = chrono::Utc::now().timestamp() - 5 * 60;
        assert!(!should_fire_session_start_greeting(&config));
    }

    #[test]
    fn session_start_fires_after_throttle_window() {
        let mut config = Config::default();
        config.heartbeat.session_start_enabled = true;
        config.heartbeat.session_start_throttle_minutes = 30;
        // Fired 45 minutes ago — past the 30-minute window.
        config.heartbeat.last_fired_at = chrono::Utc::now().timestamp() - 45 * 60;
        assert!(should_fire_session_start_greeting(&config));
    }

    #[test]
    fn session_start_zero_throttle_always_fires() {
        let mut config = Config::default();
        config.heartbeat.session_start_enabled = true;
        config.heartbeat.session_start_throttle_minutes = 0;
        config.heartbeat.last_fired_at = chrono::Utc::now().timestamp();
        assert!(should_fire_session_start_greeting(&config));
    }

    #[test]
    fn proto_heartbeat_result_preserves_skip_reason() {
        let evt = PbHeartbeatEvent {
            source: "scheduled".to_string(),
            started_at_unix: 0,
            finished_at_unix: 0,
            status: "skipped".to_string(),
            message: "quiet hours".to_string(),
            duration_ms: 0.0,
            skip_reason: "quiet_hours".to_string(),
        };
        let result = proto_heartbeat_result(evt);
        assert!(matches!(
            result,
            heartbeat::scheduler::HeartbeatResult::Skipped {
                reason: heartbeat::scheduler::SkipReason::QuietHours
            }
        ));
    }

    #[test]
    fn proto_heartbeat_result_maps_duplicate_response() {
        let evt = PbHeartbeatEvent {
            source: "scheduled".to_string(),
            started_at_unix: 0,
            finished_at_unix: 0,
            status: "skipped".to_string(),
            message: "duplicate response".to_string(),
            duration_ms: 0.0,
            skip_reason: "duplicate_response".to_string(),
        };
        let result = proto_heartbeat_result(evt);
        assert!(matches!(
            result,
            heartbeat::scheduler::HeartbeatResult::Skipped {
                reason: heartbeat::scheduler::SkipReason::DuplicateResponse
            }
        ));
    }

    #[test]
    fn proto_heartbeat_result_defaults_unknown_skip_reason_to_empty_response() {
        let evt = PbHeartbeatEvent {
            source: "scheduled".to_string(),
            started_at_unix: 0,
            finished_at_unix: 0,
            status: "skipped".to_string(),
            message: "empty response".to_string(),
            duration_ms: 0.0,
            skip_reason: String::new(),
        };
        let result = proto_heartbeat_result(evt);
        assert!(matches!(
            result,
            heartbeat::scheduler::HeartbeatResult::Skipped {
                reason: heartbeat::scheduler::SkipReason::EmptyResponse
            }
        ));
    }

    #[test]
    fn proto_heartbeat_result_falls_back_to_message_for_older_daemons() {
        let evt = PbHeartbeatEvent {
            source: "scheduled".to_string(),
            started_at_unix: 0,
            finished_at_unix: 0,
            status: "skipped".to_string(),
            message: "quiet hours".to_string(),
            duration_ms: 0.0,
            skip_reason: String::new(),
        };
        let result = proto_heartbeat_result(evt);
        assert!(matches!(
            result,
            heartbeat::scheduler::HeartbeatResult::Skipped {
                reason: heartbeat::scheduler::SkipReason::QuietHours
            }
        ));
    }

    // The EnableAlternateScroll/DisableAlternateScroll command escape tests
    // moved to `tui_lifecycle.rs` along with the structs themselves. The
    // source-level forbidden-mode guards below cover the same invariants
    // for this module.

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
    fn run_event_loop_polls_terminal_input_before_agent_events() {
        // INVARIANT: in the `biased` select! inside `run_event_loop`, the
        // terminal-event arm (`event_stream.next()`) MUST appear before the
        // agent-event arm (`app.event_rx`). A hot LLM stream keeps event_rx
        // perpetually ready; reversing the order means biased select picks
        // agent events every iteration and Esc never reaches `handle_key` —
        // the user-visible "esc to interrupt does nothing" bug that this
        // guard was added to prevent.
        let src = include_str!("mod.rs");
        let code = strip_tests_and_comments(src);
        let term_idx = code
            .find("event_stream.next()")
            .expect("event_stream.next() arm not found in run_event_loop");
        let agent_idx = code
            .find("app.event_rx")
            .expect("app.event_rx arm not found in run_event_loop");
        assert!(
            term_idx < agent_idx,
            "terminal-event arm must be polled before agent-event arm in the \
             biased select! inside run_event_loop. Reversing this order will \
             starve key input during streaming and break Esc-to-interrupt."
        );
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
        let src = concat!(
            include_str!("app/mod.rs"),
            include_str!("app/render.rs"),
            include_str!("app/events.rs"),
        );
        let code = strip_tests_and_comments(src);
        for mode in FORBIDDEN_MODES {
            assert!(
                !code.contains(mode),
                "crates/cli/src/tui/app/ code contains forbidden mouse mode {mode}. \
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
        let src = concat!(
            include_str!("app/mod.rs"),
            include_str!("app/render.rs"),
            include_str!("app/events.rs"),
        );
        let code = strip_tests_and_comments(src);
        assert!(
            !code.contains("Event::Mouse("),
            "app/ must not match Event::Mouse — we do not enable mouse \
             tracking, and adding such a handler would require enabling a \
             forbidden mode."
        );
        assert!(
            !code.contains("fn handle_mouse"),
            "app/ must not define handle_mouse — there is no mouse event \
             source. Wheel is routed as KeyCode::Up/Down via ?1007h and \
             handled in handle_key."
        );
    }

    #[test]
    fn transcript_pager_source_does_not_enable_mouse() {
        // The pager is a new full-screen overlay; verify it doesn't reintroduce
        // any of the forbidden mouse-tracking patterns. If this fails, the
        // pager's text-selection ergonomics will silently break.
        let src = include_str!("transcript_pager.rs");
        let code = strip_tests_and_comments(src);
        for mode in FORBIDDEN_MODES {
            assert!(
                !code.contains(mode),
                "transcript_pager.rs contains forbidden mouse mode {mode}"
            );
        }
        for forbidden in [
            "EnableMouseCapture",
            "DisableMouseCapture",
            "Event::Mouse(",
            "MouseEventKind",
            "fn handle_mouse",
        ] {
            assert!(
                !code.contains(forbidden),
                "transcript_pager.rs must not reference {forbidden}"
            );
        }
    }

    #[test]
    fn app_rs_has_no_mouse_event_kind_references() {
        let src = concat!(
            include_str!("app/mod.rs"),
            include_str!("app/render.rs"),
            include_str!("app/events.rs"),
        );
        let code = strip_tests_and_comments(src);
        assert!(
            !code.contains("MouseEventKind"),
            "app/ must not reference crossterm::event::MouseEventKind — \
             mouse events are not delivered to this crate."
        );
    }

    // ------------------------------------------------------------------------
    // Inline-viewport guards — Track B refactor.
    // ------------------------------------------------------------------------

    #[test]
    fn render_rs_does_not_import_scrollbar() {
        let src = include_str!("app/render.rs");
        assert!(
            !src.contains("Scrollbar"),
            "Scrollbar widget removed in inline-viewport refactor — terminal owns scrolling now"
        );
    }

    #[test]
    fn app_struct_has_no_scroll_offset() {
        let src = include_str!("app/mod.rs");
        let struct_section = src.split("pub struct App").nth(1).unwrap();
        let struct_body = struct_section.split('}').next().unwrap();
        assert!(
            !struct_body.contains("scroll_offset"),
            "scroll_offset field removed in inline-viewport refactor"
        );
        assert!(
            !struct_body.contains("auto_scroll"),
            "auto_scroll field removed in inline-viewport refactor"
        );
        assert!(
            !struct_body.contains("transcript_area"),
            "transcript_area field removed in inline-viewport refactor"
        );
    }

    #[test]
    fn tui_lifecycle_set_modes_does_not_enter_alt_screen() {
        // set_modes is the canonical entry-mode setter for inline-viewport
        // mode. It must NOT enter the alternate screen at startup — that
        // would defeat the purpose of inline-viewport (the terminal must own
        // scrollback, which only works in the main screen). Alt-screen entry
        // is a per-popup action via `Tui::enter_alt_screen`.
        let src = include_str!("tui_lifecycle.rs");
        let needle = "pub fn set_modes()";
        let start = src.find(needle).expect("set_modes not found");
        let body_start = src[start..].find('{').expect("set_modes body") + start;
        // Find the matching closing brace (depth tracking).
        let bytes = src.as_bytes();
        let mut depth: i32 = 0;
        let mut end = body_start;
        for (i, b) in bytes.iter().enumerate().skip(body_start) {
            match *b {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let body = &src[body_start..=end];
        assert!(
            !body.contains("EnterAlternateScreen"),
            "tui_lifecycle::set_modes must NOT enter the alternate screen — \
             inline-viewport mode requires the main screen to own scrollback"
        );
    }
}
