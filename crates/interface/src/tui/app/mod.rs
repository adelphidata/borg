mod composer_submit;
mod events;
mod key_handlers;
mod popups;
mod render;
mod streaming;

pub(super) use popups::PopupHandler;
pub use streaming::{BacktrackPhase, StreamStatus};

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use ratatui::text::Line;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;

use crate::daemon_client::DaemonClient;

use cortex::agent::AgentEvent;
use cortex::config::{CollaborationMode, Config};
use cortex::doctor::DiagnosticCheck;
use heartbeat::scheduler::HeartbeatEvent;
use throbber_widgets_tui::ThrobberState;

use super::btw_popup::BtwPopup;
use super::command_popup::CommandPopup;
use super::composer::Composer;
use super::file_popup::FileSearchPopup;
use super::file_search::FileSearchService;
use super::history::HistoryCell;
use super::migrate_popup::{MigrateAction, MigratePopup};
use super::model_popup::ModelPopup;
use super::pairing_popup::{PairingAction, PairingPopup};
use super::plan_overlay::PlanOverlay;
use super::plugins_popup::{PluginAction, PluginsPopup};
use super::schedule_popup::{ScheduleAction, SchedulePopup};
use super::sessions_popup::SessionsPopup;
use super::settings_popup::SettingsPopup;
use super::status_popup::StatusPopup;
use super::subprocesses_popup::SubprocessesPopup;
use super::transcript_pager::TranscriptPager;
use super::usage_popup::UsagePopup;

/// Per-question UI state for the `AwaitingInput` carousel.
///
/// Tracks which option the user has highlighted (`cursor`), which options they
/// have committed (`multi_selected` for multi-select questions; ignored when
/// the question is single-select and the answer is `[options[cursor]]` on
/// submit), whether the auto-appended "Other" row is selected, and the
/// Tab-toggled notes box state.
#[derive(Debug, Clone, Default)]
pub struct QuestionState {
    /// Highlighted row in the option list. `0..=options.len()` — index
    /// `options.len()` selects the auto-appended "Other" row.
    pub cursor: usize,
    /// Set of row indices the user committed (multi-select only).
    pub multi_selected: std::collections::HashSet<usize>,
    /// True when the user committed the "Other" free-form row. The note text
    /// itself lives in `notes`.
    pub other_selected: bool,
    /// Whether the Notes editor is open (Tab toggles).
    pub notes_open: bool,
    /// Free-form note text. Submitted to the agent prefixed with `user_note: `
    /// when non-empty, regardless of whether "Other" was picked.
    pub notes: String,
}

pub enum AppState {
    Idle,
    // Live header/details shown beside the elapsed clock live on `App` as
    // `stream_status`, not inside this variant, so the many call sites that
    // re-enter Streaming after approval/input prompts don't each have to
    // preserve them.
    Streaming {
        start: Instant,
    },
    AwaitingApproval {
        respond: Option<oneshot::Sender<bool>>,
    },
    /// Agent has asked one or more questions mid-turn via `request_user_input`;
    /// waiting for the user to walk through the carousel and submit answers.
    ///
    /// `questions` is the immutable batch (up to `MAX_QUESTIONS` entries). `current_idx` is the
    /// active question; `per_question_state` parallels `questions` and holds
    /// per-question UI state (cursor + selections + notes). `respond` carries
    /// the oneshot back to the agent — `None` once consumed.
    AwaitingInput {
        questions: Vec<cortex::tool_handlers::user_input::Question>,
        current_idx: usize,
        per_question_state: Vec<QuestionState>,
        respond: Option<
            oneshot::Sender<
                std::collections::HashMap<String, cortex::tool_handlers::user_input::Answer>,
            >,
        >,
    },
    PlanReview,
    /// Full-screen transcript pager open (Ctrl+T). All keys route to the pager.
    TranscriptPager,
    ConfirmingUninstall,
}

/// A message queued during streaming, preserving both text and image attachments.
pub struct QueuedMessage {
    pub text: String,
    pub images: Vec<super::composer::ImageAttachment>,
}

/// Result of a background `/btw` run, sent from the spawned task back to the
/// TUI event loop so it can drive the popup state machine on the main thread.
/// One goal-lifecycle update forwarded from the daemon. The forwarder task
/// translates `protocols::admin::GoalEvent` into this UI-side type so the
/// app loop doesn't see the wire format directly.
#[derive(Debug, Clone)]
pub struct GoalUiEvent {
    /// Lifecycle kind from `cortex::goals::event_kind`. Used only for
    /// disambiguating `cleared` (no goal) vs. everything else.
    pub kind: String,
    /// Status of the goal after the mutation, or `None` for kind=cleared.
    pub status: Option<cortex::goals::GoalStatus>,
}

pub struct BtwResult {
    pub question: String,
    pub outcome: Result<String, String>,
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

/// Cached ambient-status snapshot rendered in the TUI header as the
/// `class: {name} [the {Archetype}] Lv.{n} ({mood})` line.
///
/// Computed by `App::refresh_ambient_status` off the evolution + vitals +
/// bond DB state. Kept as a cache so `render()` never hits the DB.
#[derive(Debug, Clone)]
pub struct AmbientStatus {
    /// Evolved custom name; `None` means the agent is still the unnamed
    /// placeholder and the formatter renders `"Base Borg"` instead.
    pub evolution_name: Option<String>,
    /// Current evolution level.
    pub level: u8,
    /// Mood derived from vitals/bond/evolution via `compute_mood`.
    pub mood: cortex::evolution::Mood,
    /// Current dominant archetype, if any. Only rendered once
    /// `evolution_name` is `Some` (archetype attaches as an epithet to a
    /// real proper noun).
    pub archetype: Option<cortex::evolution::Archetype>,
}

/// Fetch the persistent doctor warnings from the daemon, returning the raw
/// `category:name` strings. Returns `None` when there are no warnings (or
/// the daemon RPC fails — caller treats that as "show nothing").
///
/// Rendered once at TUI startup as a `HistoryCell::DoctorWarning` so it
/// scrolls away like any other cell — intentionally not a pinned banner.
/// `borg doctor` is the source of truth for the full list.
pub(super) fn load_persistent_warning_notice(
    client: &Arc<Mutex<DaemonClient>>,
) -> Option<Vec<String>> {
    let warnings = block_on_client(client, |c| {
        Box::pin(async move { c.get_persistent_warnings().await })
    })?;
    if warnings.warnings.is_empty() {
        None
    } else {
        Some(warnings.warnings)
    }
}

/// Load the current ambient-status snapshot via the daemon. Errors propagate
/// to the caller so `refresh_ambient_status` can decide whether to clear the
/// cache and log a single warning (rather than duplicate messages per step).
fn load_ambient_status(client: &Arc<Mutex<DaemonClient>>) -> anyhow::Result<AmbientStatus> {
    let snap = block_on_client(client, |c| {
        Box::pin(async move { c.get_ambient_status().await })
    })
    .ok_or_else(|| anyhow::anyhow!("ambient_status: daemon RPC unavailable"))?;

    // Map proto strings back to typed enums. Empty/unknown values are
    // tolerated — they collapse to "Base Borg" / no archetype / Stable mood
    // in the formatter.
    let archetype = if snap.archetype.is_empty() {
        None
    } else {
        cortex::evolution::Archetype::parse(&snap.archetype)
    };
    let mood = match snap.mood.as_str() {
        "focused" => cortex::evolution::Mood::Focused,
        "strained" => cortex::evolution::Mood::Strained,
        "learning" => cortex::evolution::Mood::Learning,
        "drifting" => cortex::evolution::Mood::Drifting,
        "ascending" => cortex::evolution::Mood::Ascending,
        _ => cortex::evolution::Mood::Stable,
    };
    Ok(AmbientStatus {
        evolution_name: if snap.evolution_name.is_empty() {
            None
        } else {
            Some(snap.evolution_name)
        },
        level: snap.level.min(u8::MAX as u32) as u8,
        mood,
        archetype,
    })
}

/// Run a single admin/status RPC from a sync context. Returns `None` when
/// no tokio runtime is current (test paths) or the RPC errors. Logs the
/// error in the latter case.
fn block_on_client<F, T>(client: &Arc<Mutex<DaemonClient>>, f: F) -> Option<T>
where
    F: for<'b> FnOnce(
        &'b mut DaemonClient,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<T>> + Send + 'b>,
    >,
{
    let client = client.clone();
    let handle = tokio::runtime::Handle::try_current().ok()?;
    let res = tokio::task::block_in_place(|| {
        handle.block_on(async move {
            let mut c = client.lock().await;
            f(&mut c).await
        })
    });
    match res {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("daemon RPC failed: {e:#}");
            None
        }
    }
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
    /// Fork the current agent session at its most recent message into a new
    /// session. The event loop resolves the live agent_session_id, performs
    /// the DB-level copy, and then dispatches a `LoadSession` for the fork.
    ForkSession,
    ClearHistory,
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
    RunPairingActions {
        actions: Vec<PairingAction>,
    },
    RunMigration {
        actions: Vec<MigrateAction>,
    },
    SelfUpdate {
        prerelease: bool,
        check: bool,
    },
    Uninstall,
    RunDoctor,
    Poke,
    /// Spawn a background `/btw` side-question agent. Event loop handles the
    /// actual `tokio::spawn`; the TUI has already set the popup to Loading.
    StartBtw {
        question: String,
        snapshot: Vec<cortex::types::Message>,
    },
    /// Export a specific session by id (from the `/sessions` popup).
    ExportSession {
        id: String,
        format: cortex::export::ExportFormat,
    },
    /// Export the currently-loaded session (via `/export [format]`).
    ExportCurrentSession {
        format: cortex::export::ExportFormat,
    },
    /// Signal that a full-screen surface has just opened (large popup or the
    /// transcript pager). The event loop reconciles to alt-screen on the next
    /// frame; the variant exists so the action surface documents the intent
    /// and tests can assert which key paths request alt-screen.
    EnterAltScreen,
    /// Signal that the active alt-screen surface has just dismissed.
    /// Reconciliation in the event loop restores the inline viewport.
    LeaveAltScreen,
}

pub struct App<'a> {
    pub cells: Vec<HistoryCell>,
    pub state: AppState,
    pub composer: Composer<'a>,
    pub command_popup: CommandPopup,
    pub settings_popup: SettingsPopup,
    pub model_popup: ModelPopup,
    pub plugins_popup: PluginsPopup,
    /// Number of history cells already emitted to the scrollback above the
    /// inline viewport. Cells `< cells_emitted` were rendered into terminal
    /// scrollback via `insert_history_lines` and must never be re-rendered
    /// in the viewport or mutated again.
    pub cells_emitted: usize,
    /// Lines staged for the next `insert_history_lines` flush into the
    /// terminal's native scrollback. Drained by the event loop between
    /// `App::render_viewport` and `Tui::draw`.
    pub pending_scrollback: Vec<Line<'static>>,
    /// True once the branded header has been pushed into `pending_scrollback`.
    /// The header is flushed exactly once on the first frame; the viewport
    /// itself never re-renders it.
    pub banner_emitted: bool,
    /// Width of the transcript region from the most recent `render_viewport`,
    /// used by `flush_stable_cells` to wrap cells consistently with the
    /// upcoming viewport draw.
    pub last_viewport_width: u16,
    pub config: Config,
    pub event_rx: Option<mpsc::Receiver<AgentEvent>>,
    pub heartbeat_rx: Option<mpsc::Receiver<HeartbeatEvent>>,
    /// Receives goal-lifecycle events forwarded from the daemon's
    /// `Admin.SubscribeGoalEvents` stream. The TUI's select! loop drains
    /// this and updates [`Self::goal_status`] so the footer reacts within
    /// the broadcast latency (~100ms locally).
    pub goal_event_rx: Option<mpsc::Receiver<GoalUiEvent>>,
    /// Retained for symmetry with the legacy in-process heartbeat scheduler;
    /// the daemon owns scheduling now so nothing in the TUI feeds events
    /// back through this sender. Kept on the struct so re-introducing a
    /// local fallback path is a one-liner if needed.
    #[allow(dead_code)]
    pub heartbeat_event_tx: Option<mpsc::Sender<HeartbeatEvent>>,
    pub poke_tx: Option<mpsc::Sender<()>>,
    pub cancel_token: Option<CancellationToken>,
    /// Accumulated token usage for the current session (surfaced via /usage).
    pub session_prompt_tokens: u64,
    pub session_completion_tokens: u64,
    /// Prompt-token count from the most recent LLM Usage event — i.e. the
    /// current effective context window size. Overwritten (not summed) per
    /// turn, and reset to the post-compaction history token count when a
    /// `HistoryCompacted` event arrives. This is the source for the footer
    /// `ctx %` indicator.
    pub last_prompt_tokens: u64,
    /// Messages queued by Enter during streaming, auto-submitted FIFO on turn complete
    pub queued_messages: VecDeque<QueuedMessage>,
    /// Whether the last agent turn ended with an error (pauses queue drain)
    pub last_turn_errored: bool,
    /// Whether the "[queue paused]" message has already been shown (prevents duplicates)
    pub queue_pause_notified: bool,
    /// Conversation backtrack state machine
    pub backtrack: BacktrackPhase,
    pub plan_overlay: PlanOverlay,
    pub transcript_pager: TranscriptPager,
    /// When the user enters Plan collaboration mode, the previous mode is stashed here
    /// so the post-turn review overlay can restore it on "Proceed". `None` when the
    /// user is not currently in a transient Plan-then-execute flow.
    pub previous_collab_mode: Option<CollaborationMode>,
    pub pairing_popup: PairingPopup,
    pub sessions_popup: SessionsPopup,
    pub schedule_popup: SchedulePopup,
    pub migrate_popup: MigratePopup,
    pub file_popup: FileSearchPopup,
    /// Background filesystem walker feeding `file_popup` asynchronously.
    pub file_search: FileSearchService,
    pub throbber_state: ThrobberState,
    /// Live header/details surfaced in the status row while streaming.
    pub stream_status: StreamStatus,
    /// Channel for sending steer messages to the agent mid-turn.
    pub steer_tx: Option<mpsc::UnboundedSender<String>>,
    /// Steers queued in UI, cleared when agent confirms receipt.
    pub pending_steers: VecDeque<String>,
    /// Current plan steps displayed inline (updated by PlanUpdated events).
    pub plan_steps: Vec<cortex::types::PlanStep>,
    /// Cached ambient-status line for the banner (class: ... header).
    pub ambient_status: Option<AmbientStatus>,
    /// Version installed by `/update` in the current process, pending restart.
    pub pending_update_version: Option<String>,
    pub status_popup: StatusPopup,
    pub usage_popup: UsagePopup,
    pub subprocesses_popup: SubprocessesPopup,
    pub btw_popup: BtwPopup,
    /// Cancel token for the currently-in-flight `/btw` background task, if
    /// any. A new `/btw` invocation cancels the previous one.
    pub btw_cancel: Option<CancellationToken>,
    /// Channel the `/btw` background task posts its result onto. The TUI
    /// event loop drains this between polls and forwards to the popup.
    pub btw_result_rx: Option<mpsc::UnboundedReceiver<BtwResult>>,
    pub btw_result_tx: Option<mpsc::UnboundedSender<BtwResult>>,
    pub doctor_rx: Option<mpsc::Receiver<DoctorEvent>>,
    /// Transient corner notifications; auto-expire.
    pub toasts: super::toast::ToastStack,
    /// Shared daemon client for admin RPCs from popups + commands.
    pub client: Arc<Mutex<DaemonClient>>,
    /// Cached goal status for the active session. Updated by the
    /// `SubscribeGoalEvents` listener spawned at startup so the footer
    /// reacts within ~100ms of a daemon-side mutation. `None` until the
    /// initial `GetGoal` seed completes (or when no goal is set).
    pub goal_status: Option<cortex::goals::GoalStatus>,
}

impl<'a> App<'a> {
    pub fn new(
        config: Config,
        heartbeat_rx: Option<mpsc::Receiver<HeartbeatEvent>>,
        heartbeat_event_tx: Option<mpsc::Sender<HeartbeatEvent>>,
        poke_tx: Option<mpsc::Sender<()>>,
        client: Arc<Mutex<DaemonClient>>,
        goal_event_rx: Option<mpsc::Receiver<GoalUiEvent>>,
    ) -> Self {
        let blocked_paths = config.security.blocked_paths.clone();
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let file_search = FileSearchService::spawn(cwd, blocked_paths);
        let mut app = Self {
            cells: Vec::new(),
            state: AppState::Idle,
            composer: {
                let mut c = Composer::new();
                c.load_persistent_history();
                c
            },
            command_popup: {
                let custom = match cortex::config::Config::data_dir() {
                    Ok(dir) => super::custom_commands::load_from_dir(&dir.join("commands")),
                    Err(err) => {
                        tracing::warn!(%err, "data_dir lookup failed; skipping custom commands");
                        Vec::new()
                    }
                };
                CommandPopup::with_custom(custom)
            },
            settings_popup: SettingsPopup::new(),
            model_popup: ModelPopup::new(),
            plugins_popup: PluginsPopup::new(),
            cells_emitted: 0,
            pending_scrollback: Vec::new(),
            banner_emitted: false,
            last_viewport_width: 0,
            config,
            event_rx: None,
            heartbeat_rx,
            goal_event_rx,
            heartbeat_event_tx,
            poke_tx,
            cancel_token: None,
            session_prompt_tokens: 0,
            session_completion_tokens: 0,
            last_prompt_tokens: 0,
            queued_messages: VecDeque::new(),
            last_turn_errored: false,
            queue_pause_notified: false,
            backtrack: BacktrackPhase::Inactive,
            plan_overlay: PlanOverlay::new(),
            transcript_pager: TranscriptPager::new(),
            previous_collab_mode: None,
            pairing_popup: PairingPopup::new(),
            sessions_popup: SessionsPopup::new(),
            schedule_popup: SchedulePopup::new(),
            migrate_popup: MigratePopup::new(),
            file_popup: FileSearchPopup::new(),
            file_search,
            throbber_state: ThrobberState::default(),
            stream_status: StreamStatus::default(),
            steer_tx: None,
            pending_steers: VecDeque::new(),
            plan_steps: Vec::new(),
            ambient_status: None,
            pending_update_version: None,
            status_popup: StatusPopup::new(),
            usage_popup: UsagePopup::new(),
            subprocesses_popup: SubprocessesPopup::new(),
            btw_popup: BtwPopup::new(),
            btw_cancel: None,
            btw_result_rx: None,
            btw_result_tx: None,
            doctor_rx: None,
            toasts: super::toast::ToastStack::new(),
            client,
            goal_status: None,
        };
        app.refresh_ambient_status();
        // One-shot persistent-warning notice. Rendered once after the
        // opening card on TUI startup, then scrolls away like any other
        // System cell — intentionally *not* a pinned banner.
        if let Some(warnings) = load_persistent_warning_notice(&app.client) {
            app.cells
                .push(super::history::HistoryCell::DoctorWarning { warnings });
        }
        app
    }

    /// Apply a goal-lifecycle update streamed from the daemon. Updates the
    /// cached `goal_status` field so the footer hint re-renders on the next
    /// frame.
    pub fn apply_goal_event(&mut self, evt: GoalUiEvent) {
        if evt.kind == cortex::goals::event_kind::CLEARED {
            self.goal_status = None;
        } else {
            self.goal_status = evt.status;
        }
    }

    /// Refresh the cached `AmbientStatus` via the daemon. Called from
    /// `App::new` and after every tool result. On RPC failure the status is
    /// cleared to `None` with a warning.
    pub fn refresh_ambient_status(&mut self) {
        self.ambient_status = match load_ambient_status(&self.client) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("ambient_status: refresh failed: {e:#}");
                None
            }
        };
    }

    /// Push a short-lived info toast into the corner overlay.
    pub fn toast_info(&mut self, text: impl Into<String>) {
        self.toasts.push(text, super::toast::ToastVariant::Info);
    }

    /// Drain any file-search result batches posted by the background walker
    /// and hand them to the popup. Stale batches (where the returned query
    /// no longer matches the popup's `pending_query`) are dropped inside
    /// `FileSearchPopup::apply_results`.
    pub fn tick_file_search(&mut self) {
        for result in self.file_search.drain_results() {
            self.file_popup.apply_results(result.query, result.matches);
        }
    }

    pub fn tick_paste_burst(&mut self) {
        // Don't flush paste-burst into the composer while a popup is open.
        if !self.any_popup_visible() {
            self.composer.tick();
        }
    }

    /// Toggle the collapsed state on the most recent collapsible tool result.
    /// Returns `true` when a cell was flipped, `false` when nothing qualified.
    pub fn toggle_last_collapsible(&mut self) -> bool {
        for cell in self.cells.iter_mut().rev() {
            if cell.is_collapsible_result() {
                return cell.toggle_collapsed();
            }
        }
        false
    }

    pub(super) fn handle_logs_command(&mut self, arg: Option<&str>) -> Result<AppAction> {
        // /logs raw — old behavior: tail tui.log
        if arg == Some("raw") {
            let log_path = match cortex::config::Config::logs_dir() {
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

        let level_arg = level_filter.unwrap_or("info").to_string();
        let text = block_on_client(&self.client, move |c| {
            Box::pin(async move { c.get_activity_log_text(50, &level_arg, "").await })
        })
        .map(|res| res.text)
        .unwrap_or_else(|| "Error querying activity log via daemon".to_string());

        self.push_system_message(text);
        Ok(AppAction::Continue)
    }
}

#[cfg(test)]
mod tests;
