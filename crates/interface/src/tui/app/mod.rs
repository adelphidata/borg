mod events;
mod key_handlers;
mod render;

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
use super::transcript_pager::TranscriptPager;

/// Single source of truth for the list of popups that participate in
/// key/paste/visibility routing. Every dispatch site in this file uses this
/// macro, so adding a popup is a one-line change.
///
/// `$body!($self, $popup)` is invoked once per popup field. Callers define
/// an inner `macro_rules!` for the per-popup step and pass its name.
macro_rules! for_each_routed_popup {
    ($self:ident, $body:ident) => {
        $body!($self, settings_popup);
        $body!($self, model_popup);
        $body!($self, plugins_popup);
        $body!($self, pairing_popup);
        $body!($self, sessions_popup);
        $body!($self, schedule_popup);
        $body!($self, migrate_popup);
        $body!($self, status_popup);
        $body!($self, btw_popup);
    };
}

/// Trait for popup windows that handle keyboard and paste events.
/// Each popup converts its domain-specific actions into `AppAction` variants internally.
///
/// Dispatch is currently macro-driven on concrete types (see
/// `for_each_routed_popup!`), so the trait is not invoked through `dyn`; it
/// documents the shared contract and makes accidental drift at the signature
/// level a compile error.
pub(super) trait PopupHandler {
    #[allow(dead_code)]
    fn is_visible(&self) -> bool;
    /// Handle a key event. `config` is provided for popups that need to mutate settings.
    /// Returns `Ok(Some(action))` if an action should be dispatched, `Ok(None)` to absorb
    /// the event (caller will map to `Continue`).
    fn handle_key_event(
        &mut self,
        key: crossterm::event::KeyEvent,
        config: &mut Config,
    ) -> Result<Option<AppAction>>;
    /// Handle a paste event. Returns `true` if consumed.
    fn handle_paste_event(&mut self, _text: &str) -> bool {
        false
    }
    /// True when the popup is currently in a free-text-input mode (API key,
    /// cron expression, project name, etc.). Used by the global `/` fall-through
    /// in `dispatch_key_to_popup` so we don't hijack `/` from legitimate input
    /// like cron `*/5 * * * *` or paths/URLs in setting values.
    fn wants_text_input(&self) -> bool {
        false
    }
}

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

/// Live header + details shown in the status row while `AppState::Streaming`.
///
/// Driven by agent events (reasoning deltas, tool starts, etc.) so the user
/// has evidence the turn is still doing work — elapsed time alone is
/// indistinguishable from a hang on a multi-minute reasoning turn.
#[derive(Debug, Clone)]
pub struct StreamStatus {
    pub header: String,
    pub details: Option<String>,
}

impl Default for StreamStatus {
    fn default() -> Self {
        Self {
            header: "Working".to_string(),
            details: None,
        }
    }
}

/// Max displayed width (in chars) of the details snippet before left-ellipsis.
/// Kept small so the row still fits on narrow terminals.
const STREAM_STATUS_DETAILS_MAX_CHARS: usize = 80;

impl StreamStatus {
    pub fn reset(&mut self) {
        self.header.clear();
        self.header.push_str("Working");
        self.details = None;
    }

    pub fn set_header(&mut self, header: impl Into<String>) {
        self.header = header.into();
    }

    /// Update the details snippet. Newlines are flattened to spaces and the
    /// string is trimmed to the tail `STREAM_STATUS_DETAILS_MAX_CHARS` chars
    /// with a leading ellipsis so the freshest bytes stay visible.
    pub fn set_details(&mut self, details: Option<String>) {
        self.details = details
            .map(|s| s.replace('\n', " ").trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|s| {
                let count = s.chars().count();
                if count <= STREAM_STATUS_DETAILS_MAX_CHARS {
                    s
                } else {
                    let skip = count - STREAM_STATUS_DETAILS_MAX_CHARS;
                    let tail: String = s.chars().skip(skip).collect();
                    format!("…{tail}")
                }
            });
    }
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

/// Result of a background `/btw` run, sent from the spawned task back to the
/// TUI event loop so it can drive the popup state machine on the main thread.
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
}

impl<'a> App<'a> {
    pub fn new(
        config: Config,
        heartbeat_rx: Option<mpsc::Receiver<HeartbeatEvent>>,
        heartbeat_event_tx: Option<mpsc::Sender<HeartbeatEvent>>,
        poke_tx: Option<mpsc::Sender<()>>,
        client: Arc<Mutex<DaemonClient>>,
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
            btw_popup: BtwPopup::new(),
            btw_cancel: None,
            btw_result_rx: None,
            btw_result_tx: None,
            doctor_rx: None,
            toasts: super::toast::ToastStack::new(),
            client,
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

    pub fn tick_throbber(&mut self) {
        if !matches!(self.state, AppState::Idle) {
            self.throbber_state.calc_next();
        }
    }

    /// Transition into `AppState::Streaming` and reset the live status row.
    ///
    /// Call this for *new* turns (user submit, queued message). For resume
    /// transitions from approval/input prompts where the turn was already
    /// in flight, use `resume_streaming` so the existing header/details
    /// (e.g. "Running run_shell") aren't wiped while the tool is still
    /// running server-side.
    pub fn enter_streaming(&mut self) {
        self.state = AppState::Streaming {
            start: Instant::now(),
        };
        self.stream_status = StreamStatus::default();
    }

    /// Transition back to `AppState::Streaming` from a prompt state
    /// (approval / input request) without touching `stream_status` —
    /// the agent is still mid-turn and its header is still accurate.
    pub fn resume_streaming(&mut self) {
        self.state = AppState::Streaming {
            start: Instant::now(),
        };
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

    /// Returns `true` if any popup overlay is currently visible.
    ///
    /// Used to suppress input routing to the composer and to prevent
    /// `drain_queued_if_idle` from starting a new streaming turn while the
    /// user is interacting with a popup.
    pub fn any_popup_visible(&self) -> bool {
        macro_rules! check {
            ($self:ident, $popup:ident) => {
                if $self.$popup.is_visible() {
                    return true;
                }
            };
        }
        for_each_routed_popup!(self, check);
        false
    }

    /// Dismiss every routed popup. Used when a global key (e.g. `/`) needs to
    /// reach the composer regardless of current popup state.
    pub fn close_all_popups(&mut self) {
        macro_rules! close {
            ($self:ident, $popup:ident) => {
                if $self.$popup.is_visible() {
                    $self.$popup.dismiss();
                }
            };
        }
        for_each_routed_popup!(self, close);
    }

    /// True when any visible popup is currently in a free-text-input mode
    /// (settings value editing, model API key entry, plugin credentials,
    /// project name/description, schedule cron). The global `/` fall-through
    /// must NOT fire while one of these is active — `/` is a valid character
    /// in cron expressions, paths, URLs, and many credential formats.
    pub fn any_popup_wants_text_input(&self) -> bool {
        macro_rules! check {
            ($self:ident, $popup:ident) => {
                if $self.$popup.is_visible() && $self.$popup.wants_text_input() {
                    return true;
                }
            };
        }
        for_each_routed_popup!(self, check);
        false
    }

    /// Whether the TUI currently wants an alt-screen surface.
    ///
    /// True when any of the large popups in the spec's "alt-screen wrap audit"
    /// is visible, or while the full-screen transcript pager is open. The
    /// event loop reconciles this against [`super::tui_lifecycle::Tui::is_alt_screen_active`]
    /// after every iteration; smaller popups (`command_popup`, `model_popup`,
    /// `file_popup`, plan overlay, file search) stay inline.
    pub fn wants_alt_screen(&self) -> bool {
        if matches!(self.state, AppState::TranscriptPager) {
            return true;
        }
        self.settings_popup.is_visible()
            || self.pairing_popup.is_visible()
            || self.sessions_popup.is_visible()
            || self.schedule_popup.is_visible()
            || self.migrate_popup.is_visible()
            || self.status_popup.is_visible()
            || self.btw_popup.is_visible()
            || self.plugins_popup.is_visible()
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

    /// Route a paste event to the first visible popup that accepts paste input.
    /// Returns `true` if a popup consumed the paste (caller should return Continue).
    fn dispatch_paste_to_popup(&mut self, text: &str) -> bool {
        macro_rules! route {
            ($self:ident, $popup:ident) => {
                if $self.$popup.is_visible() {
                    $self.$popup.handle_paste_event(text);
                    return true;
                }
            };
        }
        for_each_routed_popup!(self, route);
        false
    }

    /// Route a key event to the first visible popup.
    /// Returns `Some(AppAction)` if a popup handled the key, `None` if no popup is visible.
    fn dispatch_key_to_popup(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> Result<Option<AppAction>> {
        // Slash is the global command-palette entry. When a non-text-input
        // popup is open (e.g. Settings browsing list, Status overview),
        // typing `/stats` would otherwise be silently dropped because popups
        // have modal capture. Dismiss the popup and let `/` fall through to
        // the composer.
        //
        // Skipped when a popup is in a text-input mode (cron `*/5`, API keys,
        // paths, project names) — those need `/` as a literal character.
        if matches!(key.code, crossterm::event::KeyCode::Char('/'))
            && key.modifiers.is_empty()
            && self.any_popup_visible()
            && !self.any_popup_wants_text_input()
        {
            self.close_all_popups();
            return Ok(None);
        }

        // The macro expands to a chain of `if popup.is_visible() { return ... }`
        // blocks — one per popup. Writing it this way avoids the split-borrow
        // problem that blocks iterating an `&mut dyn PopupHandler` array when
        // `handle_key_event` also needs `&mut self.config`.
        macro_rules! route {
            ($self:ident, $popup:ident) => {
                if $self.$popup.is_visible() {
                    let action = $self.$popup.handle_key_event(key, &mut $self.config)?;
                    return Ok(Some(action.unwrap_or(AppAction::Continue)));
                }
            };
        }
        for_each_routed_popup!(self, route);
        Ok(None)
    }

    /// Handle a bracketed paste event (entire pasted text as a single string).
    ///
    /// Popups consume the paste exclusively — either into their text-input
    /// buffer or as a no-op when no input field is active. Paste never leaks
    /// to the composer while any popup is open, regardless of `AppState`.
    pub fn handle_paste(&mut self, text: String) -> AppAction {
        // Popups get absolute first priority across ALL states.
        // A popup can be visible during Streaming if drain_queued_if_idle
        // started a turn while a popup was open.
        if self.dispatch_paste_to_popup(&text) {
            return AppAction::Continue;
        }

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

    /// If `trimmed` invokes a user-defined slash command (`/name [args]`),
    /// return the expanded template body. Returns `None` for built-ins,
    /// non-slash input, or unknown names — those proceed to the existing
    /// dispatcher / user-message path.
    fn expand_custom_command(&self, trimmed: &str) -> Option<String> {
        if !trimmed.starts_with('/') {
            return None;
        }
        let (name, args) = match trimmed.split_once(char::is_whitespace) {
            Some((n, rest)) => (n, rest.trim_start()),
            None => (trimmed, ""),
        };
        let cmd = self.command_popup.lookup(name)?;
        let template = cmd.template.as_deref()?;
        Some(super::custom_commands::expand_template(template, args))
    }

    pub(super) fn handle_submit(&mut self, input: &str) -> Result<AppAction> {
        // User-defined slash commands (`~/.borg/commands/<name>.md`) expand
        // their template and flow down the normal user-message path. Must run
        // BEFORE built-in dispatch so a custom command isn't rejected as
        // "unknown command", and so it can never shadow a built-in (the
        // popup already drops collisions; this is a second guard).
        let expanded_owned = self.expand_custom_command(input.trim());
        let input: &str = match &expanded_owned {
            Some(s) => s.as_str(),
            None => input,
        };
        if expanded_owned.is_none() {
            if let Some(result) = self.try_handle_command(input.trim()) {
                return result;
            }
        }

        // Prepare to send to agent
        self.cells.push(HistoryCell::User {
            text: input.to_string(),
        });
        self.cells.push(HistoryCell::assistant(String::new(), true));

        // Take image attachments before file refs
        let images = self.composer.take_image_attachments();

        // Client-side `@path` expansion. Files get read (≤50 KB),
        // directories get listed (depth 1), blocked/missing paths
        // produce skip cards. Cards render as one-line rows above the
        // assistant response; raw content goes only to the LLM via
        // `augmented_text`.
        let expansion = cortex::mentions::expand_mentions(input, &self.config);
        // Insert mention cards BEFORE the assistant cell we pushed above
        // so they render in the order: User → cards → Assistant.
        if !expansion.cards.is_empty() {
            let insert_at = self.cells.len().saturating_sub(1);
            for card in &expansion.cards {
                self.cells.insert(
                    insert_at,
                    HistoryCell::MentionCard {
                        label: card.label(),
                    },
                );
            }
        }
        let final_input = expansion.augmented_text;

        let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(256);
        self.event_rx = Some(event_rx);

        let cancel = CancellationToken::new();
        self.cancel_token = Some(cancel.clone());

        self.enter_streaming();

        Ok(AppAction::SendMessage {
            input: final_input,
            images,
            event_tx,
            cancel,
        })
    }

    /// Pop the next queued message (FIFO) for dispatch.
    pub fn pop_next_queued(&mut self) -> Option<QueuedMessage> {
        self.queued_messages.pop_front()
    }

    /// Submit a queued message (called from the event loop when a turn completes).
    /// Pushes the User cell to scrollback now that the queued message is
    /// actually being sent — at queue time the message lives only in the
    /// queue preview widget.
    pub fn handle_queued_submit(&mut self, qm: QueuedMessage) -> Result<AppAction> {
        self.cells.push(HistoryCell::User {
            text: qm.text.clone(),
        });
        // Expand `@path` mentions in the queued text. Cards render
        // between the user cell and the assistant cell we're about to push.
        // See `handle_submit` for the same pattern.
        let expansion = cortex::mentions::expand_mentions(&qm.text, &self.config);
        for card in &expansion.cards {
            self.cells.push(HistoryCell::MentionCard {
                label: card.label(),
            });
        }
        let final_input = expansion.augmented_text;

        self.cells.push(HistoryCell::assistant(String::new(), true));

        let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(256);
        self.event_rx = Some(event_rx);

        let cancel = CancellationToken::new();
        self.cancel_token = Some(cancel.clone());

        self.enter_streaming();

        Ok(AppAction::SendMessage {
            input: final_input,
            images: qm.images,
            event_tx,
            cancel,
        })
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
        let client = Arc::new(Mutex::new(DaemonClient::lazy_for_tests()));
        App::new(config, None, None, None, client)
    }

    // ========================================================================
    // build_doctor_warning_lines — pure rendering helper for the startup
    // amber warning card. Drives the formatter directly without touching the
    // daemon or SQLite.
    // ========================================================================

    fn warns(s: &[&str]) -> Vec<String> {
        s.iter().map(|w| (*w).to_string()).collect()
    }

    fn flatten(lines: &[ratatui::text::Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn doctor_card_empty_for_no_warnings() {
        assert!(super::super::history::build_doctor_warning_lines(&[], 80).is_empty());
    }

    #[test]
    fn doctor_card_singular_for_one_category() {
        let lines = super::super::history::build_doctor_warning_lines(
            &warns(&["Memory:Index staleness"]),
            80,
        );
        let text = flatten(&lines);
        assert!(text.contains("⚠ 1 Warning "), "label: {text}");
        assert!(text.contains("Issue with Memory."), "summary: {text}");
        assert!(text.contains("/doctor"), "footer cta: {text}");
    }

    #[test]
    fn doctor_card_two_categories_uses_and() {
        let lines = super::super::history::build_doctor_warning_lines(
            &warns(&["Memory:a", "Gateway:b"]),
            80,
        );
        let text = flatten(&lines);
        assert!(
            text.contains("Issues with Memory and Gateway."),
            "got: {text}"
        );
    }

    #[test]
    fn doctor_card_three_categories_uses_oxford_and() {
        let lines = super::super::history::build_doctor_warning_lines(
            &warns(&["Memory:a", "Gateway:b", "Heartbeat:c"]),
            80,
        );
        let text = flatten(&lines);
        assert!(
            text.contains("Issues with Memory, Gateway, and Heartbeat."),
            "got: {text}"
        );
    }

    #[test]
    fn doctor_card_overflow_collapses_extra_categories() {
        // 8 warnings spanning 5 distinct categories — first 3 listed, the
        // remaining 2 collapse into "and 2 more". Label still shows the raw
        // warning total (8), not the deduped category count.
        let lines = super::super::history::build_doctor_warning_lines(
            &warns(&[
                "Embeddings:provider",
                "Embeddings:cache",
                "Gateway:dir",
                "Heartbeat:age",
                "Memory:idx",
                "Memory:cap",
                "Sandbox:profile",
                "Sandbox:bin",
            ]),
            80,
        );
        let text = flatten(&lines);
        assert!(text.contains("⚠ 8 Warnings "), "label: {text}");
        assert!(
            text.contains("Issues with Embeddings, Gateway, Heartbeat, and 2 more."),
            "summary: {text}"
        );
    }

    #[test]
    fn doctor_card_dedupes_repeated_categories() {
        // Three warnings, all under one category — should render as a single
        // "Issue with Memory" (singular) since only one category remains.
        let lines = super::super::history::build_doctor_warning_lines(
            &warns(&["Memory:a", "Memory:b", "Memory:c"]),
            80,
        );
        let text = flatten(&lines);
        assert!(text.contains("⚠ 3 Warnings "), "label: {text}");
        assert!(text.contains("Issue with Memory."), "summary: {text}");
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    // ========================================================================
    // /btw routing — popup visibility does not disturb main agent state, late
    // results against a dismissed/superseded popup are discarded.
    // ========================================================================

    #[test]
    fn cmd_btw_empty_pushes_usage_does_not_open_popup() {
        // Regression: /btw with no question must not fire the background task,
        // otherwise it would spin up an LLM call (and burn tokens) on typos.
        let mut app = make_app();
        let action = app.handle_submit("/btw").unwrap();
        assert!(matches!(action, AppAction::Continue));
        assert!(!app.btw_popup.is_visible());
    }

    #[test]
    fn cmd_btw_with_question_opens_loading_and_emits_start_action() {
        let mut app = make_app();
        let action = app.handle_submit("/btw what is X?").unwrap();
        match action {
            AppAction::StartBtw { question, .. } => assert_eq!(question, "what is X?"),
            _ => panic!("expected StartBtw action"),
        }
        assert!(app.btw_popup.is_visible());
    }

    #[test]
    fn cmd_btw_during_streaming_does_not_touch_app_state() {
        // Hard requirement: /btw never cancels or otherwise alters the main
        // turn. If this regresses, typing /btw mid-stream kills the user's
        // in-progress answer.
        let mut app = make_app();
        app.state = AppState::Streaming {
            start: Instant::now(),
        };
        let _ = app.handle_submit("/btw quick thing").unwrap();
        assert!(matches!(app.state, AppState::Streaming { .. }));
        assert!(app.btw_popup.is_visible());
    }

    #[test]
    fn btw_result_for_superseded_question_is_discarded() {
        // A second /btw while one is in flight supersedes the first. The
        // old result must not overwrite the new Loading state.
        let mut app = make_app();
        app.btw_popup.show_loading("new question".to_string());
        app.process_btw_result(BtwResult {
            question: "old question".to_string(),
            outcome: Ok("stale answer".to_string()),
        });
        match app.btw_popup.state() {
            super::super::btw_popup::BtwState::Loading { question } => {
                assert_eq!(question, "new question");
            }
            other => panic!("expected Loading to persist, got {other:?}"),
        }
    }

    #[test]
    fn btw_result_for_dismissed_popup_does_not_resurrect_it() {
        // User hit Esc — don't pop the window back up when the background
        // task finally finishes.
        let mut app = make_app();
        app.btw_popup.show_loading("q".to_string());
        app.btw_popup.dismiss();
        app.process_btw_result(BtwResult {
            question: "q".to_string(),
            outcome: Ok("late answer".to_string()),
        });
        assert!(!app.btw_popup.is_visible());
    }

    #[test]
    fn btw_result_ok_transitions_popup_to_ready() {
        let mut app = make_app();
        app.btw_popup.show_loading("q".to_string());
        app.process_btw_result(BtwResult {
            question: "q".to_string(),
            outcome: Ok("the answer".to_string()),
        });
        match app.btw_popup.state() {
            super::super::btw_popup::BtwState::Ready { answer, .. } => {
                assert_eq!(answer, "the answer");
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn btw_result_err_transitions_popup_to_error() {
        let mut app = make_app();
        app.btw_popup.show_loading("q".to_string());
        app.process_btw_result(BtwResult {
            question: "q".to_string(),
            outcome: Err("provider down".to_string()),
        });
        match app.btw_popup.state() {
            super::super::btw_popup::BtwState::Error { error, .. } => {
                assert_eq!(error, "provider down");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // ========================================================================
    // Transcript pager (Ctrl+T) — opening, routing, dismissal.
    // ========================================================================

    #[test]
    fn ctrl_t_opens_transcript_pager_from_idle() {
        let mut app = make_app();
        assert!(matches!(app.state, AppState::Idle));
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .unwrap();
        assert!(matches!(app.state, AppState::TranscriptPager));
        assert!(app.transcript_pager.visible);
    }

    #[test]
    fn keys_route_to_pager_while_open_and_skip_composer() {
        let mut app = make_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .unwrap();
        // While the pager is open, typing 'q' must dismiss the pager rather
        // than appearing in the composer (which would happen if dispatch leaked).
        app.handle_key(key(KeyCode::Char('q'))).unwrap();
        assert!(matches!(app.state, AppState::Idle));
        assert!(!app.transcript_pager.visible);
        assert!(app.composer.is_empty(), "composer must not receive 'q'");
    }

    #[test]
    fn esc_in_pager_dismisses_back_to_idle() {
        let mut app = make_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .unwrap();
        app.handle_key(key(KeyCode::Esc)).unwrap();
        assert!(matches!(app.state, AppState::Idle));
        assert!(!app.transcript_pager.visible);
    }

    // ========================================================================
    // Alt-screen wrap (Track D) — emit EnterAltScreen / LeaveAltScreen and
    // surface the desired alt-screen state to the event loop.
    //
    // Regression guard: if any of these break, large popups will paint into
    // the inline viewport and clip / overwrite the composer.
    // ========================================================================

    #[test]
    fn ctrl_t_emits_enter_alt_screen() {
        let mut app = make_app();
        let action = app
            .handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .unwrap();
        assert!(
            matches!(action, AppAction::EnterAltScreen),
            "Ctrl+T must emit EnterAltScreen so run_event_loop can call tui.enter_alt_screen()"
        );
        assert!(app.wants_alt_screen());
    }

    #[test]
    fn dismissing_pager_emits_leave_alt_screen() {
        let mut app = make_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .unwrap();
        let action = app.handle_key(key(KeyCode::Esc)).unwrap();
        assert!(
            matches!(action, AppAction::LeaveAltScreen),
            "Dismissing the pager must emit LeaveAltScreen so the inline viewport restores"
        );
        assert!(!app.wants_alt_screen());
    }

    #[test]
    fn settings_popup_show_drives_wants_alt_screen() {
        let mut app = make_app();
        assert!(!app.wants_alt_screen());
        let client = app.client.clone();
        app.settings_popup.show(&app.config, &client);
        assert!(
            app.wants_alt_screen(),
            "settings popup must request alt-screen surface"
        );
        app.settings_popup.dismiss();
        assert!(!app.wants_alt_screen());
    }

    #[test]
    fn small_popups_do_not_request_alt_screen() {
        // command_popup / model_popup / file_popup / plan_overlay / file_search
        // stay inline. Opening the model popup must not flip wants_alt_screen.
        let mut app = make_app();
        let client = app.client.clone();
        app.model_popup.show(&app.config, &client);
        assert!(app.model_popup.is_visible());
        assert!(
            !app.wants_alt_screen(),
            "model_popup is inline; alt-screen must not be requested"
        );
    }

    #[test]
    fn ctrl_t_in_pager_dismisses_back_to_idle() {
        let mut app = make_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .unwrap();
        assert!(matches!(app.state, AppState::Idle));
    }

    #[test]
    fn slash_in_pager_enters_search_mode_not_composer() {
        let mut app = make_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .unwrap();
        app.handle_key(key(KeyCode::Char('/'))).unwrap();
        assert!(app.transcript_pager.is_searching());
        assert!(app.composer.is_empty(), "composer must not receive '/'");
    }

    /// Inline-viewport mode: Up arrow with an empty composer falls through
    /// to composer history navigation (no app-side scroll model intercepts
    /// it). Pre-port this would have been hijacked by Rule B for wheel
    /// scrolling — that branch is gone now that the terminal owns scrolling.
    #[test]
    fn up_arrow_with_empty_composer_recalls_history() {
        let mut app = make_app();
        app.composer.set_text("first");
        app.composer.handle_key(key(KeyCode::Enter));
        app.composer.set_text("second");
        app.composer.handle_key(key(KeyCode::Enter));
        assert!(
            app.composer.is_empty(),
            "composer must be empty after submit"
        );

        app.handle_key(key(KeyCode::Up)).unwrap();

        assert_eq!(
            app.composer.text(),
            "second",
            "Up with empty composer must recall most recent history entry"
        );
        assert!(app.composer.is_browsing_history());
    }

    // --- Reverse-incremental history search (Ctrl+R) routing ---

    fn ctrl(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_r_enters_history_search_mode() {
        let mut app = make_app();
        app.composer.set_text("prev");
        app.composer.handle_key(key(KeyCode::Enter));

        app.handle_key(ctrl('r')).unwrap();
        assert!(app.composer.is_searching());
        // Empty query yields preview of newest entry.
        assert_eq!(app.composer.text(), "prev");
    }

    #[test]
    fn ctrl_p_escape_hatch_preserved_after_cancel() {
        // Regression guard: after canceling search, Ctrl+P must still
        // navigate composer history (existing invariant).
        let mut app = make_app();
        app.composer.set_text("recalled");
        app.composer.handle_key(key(KeyCode::Enter));

        app.handle_key(ctrl('r')).unwrap();
        app.handle_key(key(KeyCode::Esc)).unwrap();
        assert!(!app.composer.is_searching());

        app.handle_key(ctrl('p')).unwrap();
        assert_eq!(app.composer.text(), "recalled");
        assert!(app.composer.is_browsing_history());
    }

    #[test]
    fn search_enter_accepts_then_next_enter_submits() {
        let mut app = make_app();
        app.composer.set_text("prev cmd");
        app.composer.handle_key(key(KeyCode::Enter));

        app.handle_key(ctrl('r')).unwrap();
        app.handle_key(key(KeyCode::Char('p'))).unwrap();
        assert_eq!(app.composer.text(), "prev cmd");
        // First Enter accepts and exits search, keeps text in composer.
        app.handle_key(key(KeyCode::Enter)).unwrap();
        assert!(!app.composer.is_searching());
        assert_eq!(app.composer.text(), "prev cmd");
    }

    // --- Context-usage % (surfaced in footer) ---
    //
    // Regression guards for the "ctx 100% but autocompaction never ran" bug.
    // The footer percent must reflect the *current* context window
    // (last_prompt_tokens) — not cumulative session token counts. If anyone
    // reintroduces `+=` accumulation in handle_event_usage these tests fail.

    fn usage(prompt_tokens: u64) -> cortex::llm::UsageData {
        cortex::llm::UsageData {
            prompt_tokens,
            ..Default::default()
        }
    }

    #[test]
    fn ctx_pct_zero_by_default() {
        let app = make_app();
        assert_eq!(app.compute_context_pct(), 0);
    }

    #[test]
    fn ctx_pct_reflects_current_context_not_cumulative_session_tokens() {
        // Bug being guarded: `compute_context_pct` summed
        // session_prompt_tokens + session_completion_tokens, both `+=` per
        // turn, so ten turns at 25% each saturated the gauge to 100% even
        // though the live context was still ~25%.
        let mut app = make_app();
        let max = app.config.conversation.max_history_tokens.max(1) as u64;
        let quarter = max / 4;
        for _ in 0..10 {
            app.process_agent_event(AgentEvent::Usage(usage(quarter)));
        }
        // Cumulative would be 250%-clamped-to-100. Correct is ~25%.
        let pct = app.compute_context_pct();
        assert!(
            (24..=26).contains(&pct),
            "expected ~25%, got {pct}% (cumulative regression)"
        );
    }

    #[test]
    fn ctx_pct_drops_after_history_compacted_event() {
        let mut app = make_app();
        let max = app.config.conversation.max_history_tokens.max(1) as u64;
        // Drive the gauge up to 90%.
        app.process_agent_event(AgentEvent::Usage(usage((max * 90) / 100)));
        assert!(app.compute_context_pct() >= 89);
        // Compaction event should snap the gauge to the post-compaction size.
        app.process_agent_event(AgentEvent::HistoryCompacted {
            dropped: 5,
            before_tokens: ((max * 90) / 100) as usize,
            after_tokens: (max / 4) as usize,
            iterative: false,
        });
        let pct = app.compute_context_pct();
        assert!(
            (24..=26).contains(&pct),
            "expected ctx % to drop to ~25% after compaction, got {pct}%"
        );
    }

    #[test]
    fn ctx_pct_clamps_to_100_on_overflow() {
        let mut app = make_app();
        let max = app.config.conversation.max_history_tokens.max(1) as u64;
        app.last_prompt_tokens = max * 5;
        assert_eq!(app.compute_context_pct(), 100);
    }

    fn qm(text: &str) -> QueuedMessage {
        QueuedMessage {
            text: text.to_string(),
            images: Vec::new(),
        }
    }

    // --- Mention expansion wiring (regression guard) ---
    //
    // If submit stops running `cortex::mentions::expand_mentions`, the TUI
    // silently falls back to firing `read_file`/`list_dir` tool calls (slow,
    // one extra turn per mention). These tests assert:
    //   1. the outgoing `AppAction::SendMessage.input` contains the
    //      `<attached_file>` block for an `@path` in the user text,
    //   2. a `HistoryCell::MentionCard` is inserted for each resolved
    //      mention so the user sees the `Read foo (N lines)` row.
    // Both conditions must hold together; either alone is a regression.

    #[test]
    fn handle_submit_expands_at_file_mention_into_outgoing_message() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("note.md");
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
        let mut app = make_app();
        let input = format!("summarize @{} please", file.display());

        let action = app.handle_submit(&input).unwrap();

        match action {
            AppAction::SendMessage { input: sent, .. } => {
                assert!(
                    sent.contains("<attached_file"),
                    "outgoing input missing <attached_file>: {sent}"
                );
                assert!(
                    sent.contains("alpha\nbeta\ngamma"),
                    "outgoing input missing file content"
                );
            }
            _ => panic!("expected SendMessage"),
        }

        let has_card = app.cells.iter().any(|c| {
            matches!(c, HistoryCell::MentionCard { label } if label.starts_with("Read ") && label.contains("(3 lines)"))
        });
        let card_labels: Vec<String> = app
            .cells
            .iter()
            .filter_map(|c| match c {
                HistoryCell::MentionCard { label } => Some(label.clone()),
                _ => None,
            })
            .collect();
        assert!(
            has_card,
            "expected a `Read … (3 lines)` MentionCard in transcript; got cards: {card_labels:?}"
        );
    }

    #[test]
    fn handle_submit_without_mentions_emits_no_cards() {
        // Guard against the expander accidentally injecting a card or
        // block for plain text (e.g. email addresses, words with `@`).
        let mut app = make_app();
        let action = app.handle_submit("ping foo@bar.com how are you").unwrap();
        match action {
            AppAction::SendMessage { input, .. } => {
                assert!(
                    !input.contains("<attached_file"),
                    "email must not trigger an attachment: {input}"
                );
            }
            _ => panic!("expected SendMessage"),
        }
        assert!(app
            .cells
            .iter()
            .all(|c| !matches!(c, HistoryCell::MentionCard { .. })));
    }

    #[test]
    fn handle_queued_submit_also_expands_mentions() {
        // Queued sends go through a different function; must also expand.
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("q.md");
        std::fs::write(&file, "one\ntwo\n").unwrap();
        let mut app = make_app();
        let text = format!("@{} ?", file.display());
        let action = app.handle_queued_submit(qm(&text)).unwrap();
        match action {
            AppAction::SendMessage { input, .. } => {
                assert!(
                    input.contains("<attached_file"),
                    "queued submit must also expand mentions: {input}"
                );
            }
            _ => panic!("expected SendMessage"),
        }
        assert!(app
            .cells
            .iter()
            .any(|c| matches!(c, HistoryCell::MentionCard { .. })));
    }

    #[test]
    fn drain_pushes_user_cell_to_history() {
        // Queued messages are not echoed to scrollback at queue time —
        // the user cell is pushed when the queued message actually drains.
        let mut app = make_app();
        let before = app.cells.len();
        app.handle_queued_submit(qm("hello queued")).unwrap();
        let user_cell = app
            .cells
            .iter()
            .find_map(|c| match c {
                HistoryCell::User { text } if text == "hello queued" => Some(text.clone()),
                _ => None,
            })
            .expect("drain must push the queued message as a User cell");
        assert_eq!(user_cell, "hello queued");
        // User cell + Assistant streaming cell, at minimum.
        assert!(app.cells.len() > before);
    }

    #[test]
    fn queueing_during_streaming_does_not_push_history_cell() {
        // While streaming, pressing Enter on text should add to the queue
        // only — no echo cells in scrollback. Bug regression: dual-render
        // path produced only a thin highlight sliver in the transcript.
        use crossterm::event::{KeyEvent, KeyModifiers};
        let mut app = make_app();
        app.state = AppState::Streaming {
            start: Instant::now(),
        };
        // No steer_tx → fallback queue path.
        app.steer_tx = None;
        app.composer.set_text("queued while busy");
        let cells_before = app.cells.len();

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(
            app.cells.len(),
            cells_before,
            "queueing must not push any cells to scrollback"
        );
        assert_eq!(app.queued_messages.len(), 1);
        assert_eq!(app.queued_messages[0].text, "queued while busy");
        assert!(app.composer.text().is_empty());
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
        let w = 80u16;

        // Empty queue
        assert_eq!(app.compute_queue_preview_height(w), 0);

        // 1 short message: header(1) + body(1) + hint(1) = 3
        app.queued_messages.push_back(qm("a"));
        assert_eq!(app.compute_queue_preview_height(w), 3);

        // 3 short messages: header(1) + body(3) + hint(1) = 5
        app.queued_messages.push_back(qm("b"));
        app.queued_messages.push_back(qm("c"));
        assert_eq!(app.compute_queue_preview_height(w), 5);

        // 5 messages: only first 3 shown (no overflow line), height unchanged
        app.queued_messages.push_back(qm("d"));
        app.queued_messages.push_back(qm("e"));
        assert_eq!(app.compute_queue_preview_height(w), 5);
    }

    #[test]
    fn test_queue_preview_height_wraps_long_message() {
        let mut app = make_app();
        // inner width = 20 - 4 = 16; message of 50 chars wraps to ~4 lines,
        // capped at 3 + 1 overflow line.
        let long = "a".repeat(50);
        app.queued_messages.push_back(qm(&long));
        let h = app.compute_queue_preview_height(20);
        // header(1) + 3 wrapped + 1 overflow + hint(1) = 6
        assert_eq!(h, 6);
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

    #[test]
    fn esc_during_streaming_cancels_even_with_event_rx_backlog() {
        // Regression: a hot LLM stream keeps `event_rx` perpetually ready.
        // The fix lives in `run_event_loop`'s biased select! (terminal-input
        // arm now polled before the agent-event arm) — this test pins the
        // App-level contract the fix relies on: handle_key(Esc) cancels the
        // turn synchronously without depending on `event_rx` being drained.
        // If a future change makes Esc handling consume from event_rx or
        // otherwise gate on it, this test will fail.
        let mut app = make_app();
        app.state = AppState::Streaming {
            start: Instant::now(),
        };
        let token = CancellationToken::new();
        app.cancel_token = Some(token.clone());

        // Backlog of agent events sitting in the channel — simulating a
        // mid-stream burst of TextDelta tokens that biased select would
        // otherwise prefer.
        let (tx, rx) = mpsc::channel::<AgentEvent>(256);
        for i in 0..100 {
            tx.try_send(AgentEvent::TextDelta(format!("tok{i}")))
                .expect("backlog send");
        }
        app.event_rx = Some(rx);

        app.handle_key(key(KeyCode::Esc)).unwrap();

        assert!(token.is_cancelled(), "Esc must cancel the turn token");
        assert!(app.cancel_token.is_none(), "cancel_token must be taken");
        assert!(
            matches!(app.state, AppState::Idle),
            "Esc must drop streaming state to Idle"
        );
        assert!(app.event_rx.is_none(), "Esc must drop event_rx");
        assert_eq!(last_system_text(&app).unwrap(), "[interrupted]");

        // Dropping event_rx without draining must close the channel — i.e.
        // Esc handling did not pull events off the queue. If a future change
        // routes Esc through the agent-event path, the receiver would still
        // be alive and the sender would not see the close.
        assert!(
            tx.is_closed(),
            "event_rx must be dropped, closing the channel"
        );
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
        app.cells
            .push(HistoryCell::assistant("hi".to_string(), false));
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
        app.cells
            .push(HistoryCell::assistant("resp1".to_string(), false));
        app.cells.push(HistoryCell::User {
            text: "second".to_string(),
        });
        app.cells
            .push(HistoryCell::assistant("resp2".to_string(), false));

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
    fn settings_prefix_not_rejected() {
        let mut app = make_app();
        let _ = app.handle_submit("/settings temperature 0.5").unwrap();
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
            CollaborationMode::Execute
        );
        let _ = app.handle_submit("/plan").unwrap();
        assert_eq!(
            app.config.conversation.collaboration_mode,
            CollaborationMode::Plan,
            "/plan should switch collaboration mode to Plan"
        );
        assert_eq!(
            app.previous_collab_mode,
            Some(CollaborationMode::Execute),
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
        assert_eq!(app.previous_collab_mode, Some(CollaborationMode::Execute));
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
    fn shift_tab_cycles_modes_and_manages_previous_collab_mode() {
        // Shift+Tab (BackTab) is the only keyboard path through
        // `cycle_collaboration_mode`. Cycle is Execute ↔ Plan only — there is
        // no Default mode. Invariant: stash the prior mode when *entering*
        // Plan so the post-turn review overlay can restore it; clear when
        // leaving Plan.
        let mut app = make_app();
        let back_tab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);

        // Execute → Plan. Entering Plan stashes Execute.
        app.handle_key(back_tab).unwrap();
        assert_eq!(
            app.config.conversation.collaboration_mode,
            CollaborationMode::Plan
        );
        assert_eq!(
            app.previous_collab_mode,
            Some(CollaborationMode::Execute),
            "entering Plan must stash the prior mode for restore"
        );

        // Plan → Execute. Leaving Plan must clear the stash.
        app.handle_key(back_tab).unwrap();
        assert_eq!(
            app.config.conversation.collaboration_mode,
            CollaborationMode::Execute
        );
        assert_eq!(
            app.previous_collab_mode, None,
            "leaving Plan via cycle must clear the stashed mode"
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

    #[test]
    fn turn_complete_moves_final_assistant_tail_into_pending_scrollback() {
        let mut app = make_app();
        app.last_viewport_width = 80;
        app.cells
            .push(HistoryCell::assistant("done".to_string(), true));

        app.process_agent_event(AgentEvent::TurnComplete);

        assert_eq!(app.cells_emitted, app.cells.len());
        let pending_text = app
            .pending_scrollback
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            pending_text.contains("done"),
            "finalized assistant tail must move into pending scrollback, got: {pending_text:?}"
        );
    }

    // --- Update command ---

    fn assert_self_update(input: &str, want_pre: bool, want_check: bool) {
        let mut app = make_app();
        let result = app.handle_submit(input).unwrap();
        match result {
            AppAction::SelfUpdate { prerelease, check } => {
                assert_eq!(prerelease, want_pre, "input {input:?}: prerelease");
                assert_eq!(check, want_check, "input {input:?}: check");
            }
            _ => panic!("input {input:?}: expected SelfUpdate"),
        }
    }

    #[test]
    fn update_command_full_flag_matrix() {
        let cases: &[(&str, bool, bool)] = &[
            ("/update", false, false),
            ("/update --prerelease", true, false),
            ("/update --pre", true, false),
            ("/update --dev", true, false),
            ("/update -p", true, false),
            ("/update --check", false, true),
            ("/update -c", false, true),
            ("/update -pc", true, true),
            ("/update --prerelease --check", true, true),
            ("/update --check --dev", true, true),
        ];
        for (input, pre, check) in cases {
            assert_self_update(input, *pre, *check);
        }
    }

    #[test]
    fn update_command_unknown_flag_does_not_invoke_update() {
        let mut app = make_app();
        let result = app.handle_submit("/update --bogus").unwrap();
        assert!(
            matches!(result, AppAction::Continue),
            "unknown flag must not trigger SelfUpdate"
        );
        assert!(
            app.cells.iter().any(|c| matches!(
                c,
                HistoryCell::System { text } if text.starts_with("/update:")
            )),
            "expected a /update error message in transcript"
        );
    }

    #[test]
    fn update_command_does_not_swallow_similar_prefix() {
        // `/updateXYZ` (no space) must NOT be treated as `/update`. Otherwise the
        // prefix handler would silently eat unrelated user input.
        let mut app = make_app();
        let result = app.handle_submit("/updateNOPE").unwrap();
        assert!(
            !matches!(result, AppAction::SelfUpdate { .. }),
            "/updateNOPE must not match /update"
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
        // A random key should be consumed by the popup, not the composer.
        let initial_text = app.composer.text().to_string();
        app.handle_key(key(KeyCode::Char('a'))).unwrap();
        assert_eq!(app.composer.text(), initial_text);
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
    fn uninstall_release_event_is_ignored() {
        // REPORT_EVENT_TYPES delivers both Press and Release. The Enter that
        // submits `/uninstall` must not re-fire as "cancel" via its Release
        // event inside ConfirmingUninstall. Regression guard for the
        // auto-cancel bug.
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app();
        app.state = AppState::ConfirmingUninstall;
        let release = KeyEvent::new_with_kind_and_state(
            KeyCode::Enter,
            KeyModifiers::NONE,
            KeyEventKind::Release,
            KeyEventState::NONE,
        );
        let action = app.handle_key(release).unwrap();
        assert!(matches!(action, AppAction::Continue));
        assert!(matches!(app.state, AppState::ConfirmingUninstall));
        assert!(!app
            .cells
            .iter()
            .any(|c| matches!(c, HistoryCell::System { text } if text.contains("cancelled"))));
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
        let client = Arc::new(Mutex::new(DaemonClient::lazy_for_tests()));
        let mut app = App::new(config, None, None, Some(tx), client);
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
        // No pre-send chatter — the confirmation is emitted by the
        // AppAction::Poke handler in tui::mod.rs after the request fires,
        // not by the command dispatcher.
    }

    #[test]
    fn poke_full_channel_shows_pending_message() {
        let config = Config::default();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        // Fill the channel
        tx.try_send(()).unwrap();
        let client = Arc::new(Mutex::new(DaemonClient::lazy_for_tests()));
        let mut app = App::new(config, None, None, Some(tx), client);
        let result = app.try_handle_command("/poke");
        assert!(result.is_some());
        let action = result.unwrap().unwrap();
        assert!(matches!(action, AppAction::Continue));
        let sys = last_system_text(&app).unwrap();
        assert!(sys.contains("already pending"));
    }

    // --- Popup vs Streaming: paste/key must never leak to composer ---

    #[test]
    fn any_popup_visible_reflects_plugins_popup() {
        let mut app = make_app();
        assert!(!app.any_popup_visible());
        let data_dir = std::env::temp_dir().join("borg-test-any-popup");
        app.plugins_popup.show(&app.config, &data_dir);
        assert!(app.any_popup_visible());
    }

    /// Mirror of the per-popup `wants_text_input` tests at the App level:
    /// no popup open, no popup wants text input.
    #[test]
    fn any_popup_wants_text_input_false_when_idle() {
        let app = make_app();
        assert!(!app.any_popup_wants_text_input());
    }

    /// Critical gating test: when a popup is in a text-input mode (cron `*/5`,
    /// API key, path setting, etc.) the global `/` interceptor MUST NOT fire.
    /// Otherwise `/` mid-input dismisses the popup and silently dumps a single
    /// `/` into the composer — the original bug this fix could have introduced.
    #[test]
    fn slash_does_not_dismiss_popup_in_text_input_mode() {
        let mut app = make_app();
        let client = app.client.clone();
        app.settings_popup.show(&app.config, &client);
        app.settings_popup
            .force_editing_for_test("cron */5".to_string());
        assert!(app.any_popup_wants_text_input());

        app.handle_key(key(KeyCode::Char('/'))).unwrap();

        assert!(
            app.settings_popup.is_visible(),
            "popup must stay open while editing — `/` is a valid value char"
        );
        assert!(
            app.composer.text().is_empty(),
            "composer must NOT receive `/` — it belongs in the value buffer"
        );
    }

    /// Regression: typing `/` while another popup is open must dismiss that
    /// popup and reach the composer, so slash commands like `/stats` still
    /// work without first hitting Esc. Previously the popup's modal capture
    /// silently ate every keystroke.
    #[test]
    fn slash_dismisses_open_popup_and_falls_through_to_composer() {
        let mut app = make_app();
        let client = app.client.clone();
        app.settings_popup.show(&app.config, &client);
        assert!(app.settings_popup.is_visible());

        app.handle_key(key(KeyCode::Char('/'))).unwrap();

        assert!(
            !app.settings_popup.is_visible(),
            "settings popup should be dismissed by global slash"
        );
        assert_eq!(
            app.composer.text(),
            "/",
            "slash should reach composer, not be swallowed by the popup"
        );

        // Confirm the slash command itself still routes to the Status popup
        // (independent of typing mechanics — `try_handle_command` is the same
        // dispatch path the composer reaches after Enter).
        app.try_handle_command("/stats")
            .expect("/stats handled")
            .unwrap();
        assert!(
            app.status_popup.is_visible(),
            "/stats should open the status popup"
        );
    }

    #[test]
    fn paste_goes_to_popup_not_composer_during_streaming() {
        let mut app = make_app();
        // Open plugins popup
        let data_dir = std::env::temp_dir().join("borg-test-paste-stream");
        app.plugins_popup.show(&app.config, &data_dir);
        // Force Streaming state (simulates drain_queued_if_idle starting a turn)
        app.state = AppState::Streaming {
            start: std::time::Instant::now(),
        };
        // Paste should be swallowed by the popup, not queued as a steer
        let action = app.handle_paste("secret-token".to_string());
        assert!(matches!(action, AppAction::Continue));
        // Composer must remain empty
        assert!(app.composer.text().is_empty());
        // pending_steers must remain empty (paste was NOT queued)
        assert!(app.pending_steers.is_empty());
    }

    #[test]
    fn key_chars_go_to_popup_not_composer_during_streaming() {
        let mut app = make_app();
        let data_dir = std::env::temp_dir().join("borg-test-key-stream");
        app.plugins_popup.show(&app.config, &data_dir);

        // Select Telegram and enter credential input phase
        let tg_idx = app
            .plugins_popup
            .items_for_test()
            .iter()
            .position(|i| i.0 == "messaging/telegram")
            .unwrap();
        app.plugins_popup.set_cursor_for_test(tg_idx);
        // Force uninstalled so toggle + Enter enters CredentialInput
        app.plugins_popup.force_uninstalled_for_test(tg_idx);
        app.handle_key(key(KeyCode::Char(' '))).unwrap(); // toggle
        app.handle_key(key(KeyCode::Enter)).unwrap(); // enter cred input

        // Now force Streaming
        app.state = AppState::Streaming {
            start: std::time::Instant::now(),
        };

        // Type chars — they should go to the popup, not the composer
        app.handle_key(key(KeyCode::Char('a'))).unwrap();
        app.handle_key(key(KeyCode::Char('b'))).unwrap();
        app.handle_key(key(KeyCode::Char('c'))).unwrap();

        assert!(app.composer.text().is_empty());
    }

    #[test]
    fn model_exact_opens_popup() {
        let mut app = make_app();
        assert!(!app.model_popup.is_visible());
        let _ = app.handle_submit("/model").unwrap();
        assert!(app.model_popup.is_visible(), "/model should open the popup");
        let has_unknown = app
            .cells
            .iter()
            .any(|c| matches!(c, HistoryCell::System { text } if text.contains("Unknown command")));
        assert!(!has_unknown, "/model should not be treated as unknown");
    }

    #[test]
    fn model_prefix_sets_provider_and_model() {
        let mut app = make_app();
        let _ = app
            .handle_submit("/model openrouter/anthropic/claude-sonnet-4")
            .unwrap();
        assert_eq!(app.config.llm.provider.as_deref(), Some("openrouter"));
        assert_eq!(app.config.llm.model, "anthropic/claude-sonnet-4");
        let has_unknown = app
            .cells
            .iter()
            .any(|c| matches!(c, HistoryCell::System { text } if text.contains("Unknown command")));
        assert!(!has_unknown);
    }

    #[test]
    fn model_prefix_rejects_malformed() {
        let mut app = make_app();
        let original_provider = app.config.llm.provider.clone();
        let original_model = app.config.llm.model.clone();
        let _ = app.handle_submit("/model no-slash-here").unwrap();
        assert_eq!(app.config.llm.provider, original_provider);
        assert_eq!(app.config.llm.model, original_model);
        let has_usage = app
            .cells
            .iter()
            .any(|c| matches!(c, HistoryCell::System { text } if text.contains("Usage: /model")));
        assert!(has_usage, "malformed /model should show usage hint");
    }

    // ========================================================================
    // request_user_input — multi-question carousel key handling.
    // Guards the new batched contract: ↑/↓ move cursor, ←/→ navigate between
    // questions preserving per-question state, Tab toggles Notes editor, Space
    // toggles multi-select rows, Enter on the last question submits the whole
    // batch as a HashMap<id, Answer>, Esc cancels with an interrupted marker
    // per question.
    // ========================================================================

    use cortex::tool_handlers::user_input::{Answer, Question, QuestionOption, INTERRUPTED_MARKER};
    use std::collections::HashMap;

    fn make_question(id: &str, header: &str, labels: &[&str], multi_select: bool) -> Question {
        Question {
            id: id.to_string(),
            header: header.to_string(),
            question: format!("Pick from {header}"),
            multi_select,
            options: labels
                .iter()
                .map(|l| QuestionOption {
                    label: (*l).to_string(),
                    description: format!("desc for {l}"),
                })
                .collect(),
        }
    }

    fn seed_awaiting_with_questions(
        app: &mut App<'_>,
        questions: Vec<Question>,
    ) -> oneshot::Receiver<HashMap<String, Answer>> {
        let (tx, rx) = oneshot::channel::<HashMap<String, Answer>>();
        let per_question_state = (0..questions.len())
            .map(|_| QuestionState::default())
            .collect();
        app.state = AppState::AwaitingInput {
            questions,
            current_idx: 0,
            per_question_state,
            respond: Some(tx),
        };
        rx
    }

    #[test]
    fn enter_on_last_question_submits_full_answer_map() {
        let mut app = make_app();
        let mut rx = seed_awaiting_with_questions(
            &mut app,
            vec![
                make_question("color", "Color", &["red", "blue"], false),
                make_question("size", "Size", &["small", "large"], false),
            ],
        );

        // Q1: pick "blue" (move cursor down to index 1) then advance.
        app.handle_key(key(KeyCode::Down)).unwrap();
        app.handle_key(key(KeyCode::Enter)).unwrap(); // advances to Q2
                                                      // Q2: leave on default (cursor 0 = "small") then submit.
        app.handle_key(key(KeyCode::Enter)).unwrap();

        let map = rx.try_recv().expect("response sent");
        assert_eq!(map.get("color").unwrap().answers, vec!["blue".to_string()]);
        assert_eq!(map.get("size").unwrap().answers, vec!["small".to_string()]);
        assert!(matches!(app.state, AppState::Streaming { .. }));
    }

    #[test]
    fn arrow_keys_navigate_between_questions_preserving_per_question_state() {
        let mut app = make_app();
        let _rx = seed_awaiting_with_questions(
            &mut app,
            vec![
                make_question("a", "A", &["x", "y", "z"], false),
                make_question("b", "B", &["p", "q"], false),
            ],
        );

        // Move Q1 cursor to "z" (index 2).
        app.handle_key(key(KeyCode::Down)).unwrap();
        app.handle_key(key(KeyCode::Down)).unwrap();
        // Right arrow: advance to Q2 — Q1 cursor must be retained.
        app.handle_key(key(KeyCode::Right)).unwrap();
        // Move Q2 cursor to index 1.
        app.handle_key(key(KeyCode::Down)).unwrap();
        // Left arrow: back to Q1 — Q1 cursor should still be 2 (not reset).
        app.handle_key(key(KeyCode::Left)).unwrap();

        match &app.state {
            AppState::AwaitingInput {
                current_idx,
                per_question_state,
                ..
            } => {
                assert_eq!(*current_idx, 0);
                assert_eq!(per_question_state[0].cursor, 2);
                assert_eq!(per_question_state[1].cursor, 1);
            }
            _ => panic!("expected AwaitingInput"),
        }
    }

    #[test]
    fn tab_toggles_notes_field_on_current_question() {
        let mut app = make_app();
        let _rx = seed_awaiting_with_questions(
            &mut app,
            vec![make_question("only", "Only", &["a", "b"], false)],
        );

        // Tab opens notes; subsequent character keys land in notes, not options.
        app.handle_key(key(KeyCode::Tab)).unwrap();
        app.handle_key(key(KeyCode::Char('h'))).unwrap();
        app.handle_key(key(KeyCode::Char('i'))).unwrap();
        match &app.state {
            AppState::AwaitingInput {
                per_question_state, ..
            } => {
                assert!(per_question_state[0].notes_open);
                assert_eq!(per_question_state[0].notes, "hi");
            }
            _ => panic!("expected AwaitingInput"),
        }
        // Tab again closes notes.
        app.handle_key(key(KeyCode::Tab)).unwrap();
        match &app.state {
            AppState::AwaitingInput {
                per_question_state, ..
            } => assert!(!per_question_state[0].notes_open),
            _ => panic!("expected AwaitingInput"),
        }
    }

    #[test]
    fn multi_select_question_accumulates_selections_with_space() {
        let mut app = make_app();
        let mut rx = seed_awaiting_with_questions(
            &mut app,
            vec![make_question(
                "channels",
                "Channels",
                &["telegram", "slack", "discord"],
                true,
            )],
        );

        // Cursor on "telegram" (idx 0): toggle on.
        app.handle_key(key(KeyCode::Char(' '))).unwrap();
        // Move to "discord" (idx 2): toggle on.
        app.handle_key(key(KeyCode::Down)).unwrap();
        app.handle_key(key(KeyCode::Down)).unwrap();
        app.handle_key(key(KeyCode::Char(' '))).unwrap();
        // Submit.
        app.handle_key(key(KeyCode::Enter)).unwrap();

        let map = rx.try_recv().expect("response sent");
        // Multi-select preserves option order, not toggle order.
        assert_eq!(
            map.get("channels").unwrap().answers,
            vec!["telegram".to_string(), "discord".to_string()]
        );
    }

    #[test]
    fn esc_cancels_and_returns_interrupted_marker_per_question() {
        let mut app = make_app();
        let mut rx = seed_awaiting_with_questions(
            &mut app,
            vec![
                make_question("a", "A", &["x", "y"], false),
                make_question("b", "B", &["p", "q"], false),
            ],
        );

        app.handle_key(key(KeyCode::Esc)).unwrap();

        let map = rx.try_recv().expect("response sent");
        assert_eq!(
            map.get("a").unwrap().answers,
            vec![INTERRUPTED_MARKER.to_string()]
        );
        assert_eq!(
            map.get("b").unwrap().answers,
            vec![INTERRUPTED_MARKER.to_string()]
        );
        assert!(matches!(app.state, AppState::Streaming { .. }));
    }

    #[test]
    fn notes_appended_with_user_note_prefix_on_submit() {
        // Regression guard: free-form notes must round-trip into the answer
        // payload prefixed with `user_note: ` so the LLM can distinguish them
        // from option labels.
        let mut app = make_app();
        let mut rx = seed_awaiting_with_questions(
            &mut app,
            vec![make_question("color", "Color", &["red", "blue"], false)],
        );

        // Open notes, type "I love crimson", close, submit.
        app.handle_key(key(KeyCode::Tab)).unwrap();
        for c in "crimson".chars() {
            app.handle_key(key(KeyCode::Char(c))).unwrap();
        }
        app.handle_key(key(KeyCode::Tab)).unwrap();
        app.handle_key(key(KeyCode::Enter)).unwrap();

        let map = rx.try_recv().expect("response sent");
        let entry = map.get("color").unwrap();
        assert_eq!(entry.answers[0], "red");
        assert_eq!(entry.answers[1], "user_note: crimson");
    }

    #[test]
    fn esc_does_not_close_notes_it_cancels_the_batch() {
        // Regression guard: previously Esc only closed the notes editor when
        // it was open, leaving users with no way to cancel from inside notes.
        // Now Esc always cancels the batch, regardless of notes state.
        let mut app = make_app();
        let mut rx = seed_awaiting_with_questions(
            &mut app,
            vec![make_question("a", "A", &["x", "y"], false)],
        );

        // Open notes, type something, then Esc.
        app.handle_key(key(KeyCode::Tab)).unwrap();
        app.handle_key(key(KeyCode::Char('z'))).unwrap();
        app.handle_key(key(KeyCode::Esc)).unwrap();

        let map = rx.try_recv().expect("response sent");
        assert_eq!(
            map.get("a").unwrap().answers,
            vec![INTERRUPTED_MARKER.to_string()]
        );
    }

    #[test]
    fn ctrl_c_cancel_fills_interrupted_markers_per_question() {
        // Regression guard: Ctrl+C used to send an empty HashMap, which
        // serialize_answers would fill with empty answer lists — a different
        // shape from Esc cancellation. Now both produce identical [interrupted]
        // markers so the LLM sees one cancel shape.
        use crossterm::event::KeyModifiers;
        let mut app = make_app();
        let mut rx = seed_awaiting_with_questions(
            &mut app,
            vec![
                make_question("a", "A", &["x", "y"], false),
                make_question("b", "B", &["p", "q"], false),
            ],
        );

        let evt = crossterm::event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        app.handle_key(evt).unwrap();

        let map = rx.try_recv().expect("response sent");
        assert_eq!(
            map.get("a").unwrap().answers,
            vec![INTERRUPTED_MARKER.to_string()]
        );
        assert_eq!(
            map.get("b").unwrap().answers,
            vec![INTERRUPTED_MARKER.to_string()]
        );
    }

    #[test]
    fn moving_cursor_to_other_auto_opens_notes_in_single_select() {
        // The bare "Other" label tells the LLM nothing without context. So when
        // the cursor lands on the Other row, the notes editor pops open
        // automatically, prompting the user to type their actual answer.
        let mut app = make_app();
        let _rx = seed_awaiting_with_questions(
            &mut app,
            vec![make_question("color", "Color", &["red", "blue"], false)],
        );
        // 2 options + 1 Other → two Downs lands on the Other row.
        app.handle_key(key(KeyCode::Down)).unwrap();
        app.handle_key(key(KeyCode::Down)).unwrap();
        match &app.state {
            AppState::AwaitingInput {
                per_question_state, ..
            } => {
                assert!(
                    per_question_state[0].notes_open,
                    "expected notes to auto-open on Other"
                );
            }
            _ => panic!("expected AwaitingInput"),
        }
    }

    #[test]
    fn toggling_other_in_multi_select_auto_opens_notes() {
        let mut app = make_app();
        let _rx = seed_awaiting_with_questions(
            &mut app,
            vec![make_question(
                "channels",
                "Channels",
                &["telegram", "slack"],
                true,
            )],
        );
        // Move cursor to the Other row (idx 2), then toggle with Space.
        app.handle_key(key(KeyCode::Down)).unwrap();
        app.handle_key(key(KeyCode::Down)).unwrap();
        app.handle_key(key(KeyCode::Char(' '))).unwrap();
        match &app.state {
            AppState::AwaitingInput {
                per_question_state, ..
            } => {
                assert!(
                    per_question_state[0].other_selected,
                    "Other should be toggled on"
                );
                assert!(
                    per_question_state[0].notes_open,
                    "toggling Other on should auto-open notes"
                );
            }
            _ => panic!("expected AwaitingInput"),
        }
    }

    #[test]
    fn multi_select_with_other_and_notes_round_trips_in_option_order() {
        // Combined-path coverage: multi-select picks two options + Other +
        // free-form note. The serialized answer must contain options in their
        // declared order, then "Other", then the user_note line.
        let mut app = make_app();
        let mut rx = seed_awaiting_with_questions(
            &mut app,
            vec![make_question(
                "channels",
                "Channels",
                &["telegram", "slack", "discord"],
                true,
            )],
        );

        // Toggle telegram (idx 0).
        app.handle_key(key(KeyCode::Char(' '))).unwrap();
        // Move to slack (idx 1) and toggle.
        app.handle_key(key(KeyCode::Down)).unwrap();
        app.handle_key(key(KeyCode::Char(' '))).unwrap();
        // Move past discord (idx 2) to Other (idx 3) and toggle.
        app.handle_key(key(KeyCode::Down)).unwrap();
        app.handle_key(key(KeyCode::Down)).unwrap();
        app.handle_key(key(KeyCode::Char(' '))).unwrap();
        // Other auto-opened notes; type the note and Tab back out.
        for c in "via webhook".chars() {
            app.handle_key(key(KeyCode::Char(c))).unwrap();
        }
        app.handle_key(key(KeyCode::Tab)).unwrap();
        app.handle_key(key(KeyCode::Enter)).unwrap();

        let map = rx.try_recv().expect("response sent");
        let answers = &map.get("channels").unwrap().answers;
        assert_eq!(
            answers,
            &vec![
                "telegram".to_string(),
                "slack".to_string(),
                "Other".to_string(),
                "user_note: via webhook".to_string(),
            ]
        );
    }

    #[test]
    fn other_row_is_appended_below_options() {
        // The "Other" row is auto-added at index `options.len()` so total rows
        // is options.len() + 1. Cursor wrap-around must include it.
        let mut app = make_app();
        let _rx = seed_awaiting_with_questions(
            &mut app,
            vec![make_question("color", "Color", &["red", "blue"], false)],
        );
        // 2 options + 1 Other = 3 rows. Two Downs lands us on "Other".
        app.handle_key(key(KeyCode::Down)).unwrap();
        app.handle_key(key(KeyCode::Down)).unwrap();
        match &app.state {
            AppState::AwaitingInput {
                per_question_state,
                questions,
                ..
            } => {
                assert_eq!(per_question_state[0].cursor, questions[0].options.len());
            }
            _ => panic!("expected AwaitingInput"),
        }
    }

    // ========================================================================
    // StreamStatus — the live header + details shown in the status row.
    // ========================================================================

    #[test]
    fn stream_status_set_details_passes_short_string_through() {
        let mut s = StreamStatus::default();
        s.set_details(Some("hello world".to_string()));
        assert_eq!(s.details.as_deref(), Some("hello world"));
    }

    #[test]
    fn stream_status_set_details_left_ellipsizes_when_over_max() {
        let mut s = StreamStatus::default();
        // 200 `a`s → tail 80 chars kept, leading `…` prefix added.
        let long: String = "a".repeat(200);
        s.set_details(Some(long));
        let out = s.details.expect("details");
        assert!(out.starts_with('…'), "expected leading ellipsis, got {out}");
        // 1 leading char (the ellipsis) + STREAM_STATUS_DETAILS_MAX_CHARS tail.
        assert_eq!(out.chars().count(), 1 + STREAM_STATUS_DETAILS_MAX_CHARS);
    }

    #[test]
    fn stream_status_set_details_flattens_newlines() {
        let mut s = StreamStatus::default();
        s.set_details(Some("line one\nline two".to_string()));
        assert_eq!(s.details.as_deref(), Some("line one line two"));
    }

    #[test]
    fn stream_status_set_details_none_on_empty_string() {
        let mut s = StreamStatus::default();
        s.set_details(Some(String::new()));
        assert!(s.details.is_none());
        s.set_details(Some("   \n  ".to_string()));
        assert!(s.details.is_none(), "whitespace-only should also clear");
    }

    #[test]
    fn stream_status_reset_restores_working_header_and_clears_details() {
        let mut s = StreamStatus::default();
        s.set_header("Running run_shell");
        s.set_details(Some("cargo build".to_string()));
        s.reset();
        assert_eq!(s.header, "Working");
        assert!(s.details.is_none());
    }

    #[test]
    fn enter_streaming_resets_stream_status() {
        let mut app = make_app();
        app.stream_status.set_header("Running run_shell");
        app.stream_status.set_details(Some("cargo build".into()));
        app.enter_streaming();
        assert!(matches!(app.state, AppState::Streaming { .. }));
        // New turn → fresh slate so stale "Running <previous-tool>" text
        // can't leak from a prior turn's last phase.
        assert_eq!(app.stream_status.header, "Working");
        assert!(app.stream_status.details.is_none());
    }

    #[test]
    fn resume_streaming_preserves_stream_status() {
        // After a shell-approval prompt, we resume streaming — the tool is
        // still running on the agent side, so the "Running run_shell" header
        // must NOT be wiped on the UI.
        let mut app = make_app();
        app.stream_status.set_header("Running run_shell");
        app.stream_status.set_details(Some("cargo build".into()));
        app.resume_streaming();
        assert!(matches!(app.state, AppState::Streaming { .. }));
        assert_eq!(app.stream_status.header, "Running run_shell");
        assert_eq!(app.stream_status.details.as_deref(), Some("cargo build"));
    }

    // Event-driven header transitions ----------------------------------------

    #[test]
    fn preparing_event_sets_preparing_header() {
        let mut app = make_app();
        app.enter_streaming();
        app.process_agent_event(AgentEvent::Preparing);
        assert_eq!(app.stream_status.header, "Preparing");
        assert!(app.stream_status.details.is_none());
    }

    #[test]
    fn text_delta_flips_header_to_responding() {
        let mut app = make_app();
        app.enter_streaming();
        app.process_agent_event(AgentEvent::ThinkingDelta("reasoning about it".into()));
        assert_eq!(app.stream_status.header, "Thinking");
        app.process_agent_event(AgentEvent::TextDelta("Here's the plan".into()));
        assert_eq!(app.stream_status.header, "Responding");
        // Responding phase is driven by visible tokens, not reasoning tail,
        // so details is cleared to avoid mixing signals.
        assert!(app.stream_status.details.is_none());
    }

    #[test]
    fn thinking_delta_populates_details() {
        let mut app = make_app();
        app.enter_streaming();
        app.process_agent_event(AgentEvent::ThinkingDelta(
            "weighing options for the migration".into(),
        ));
        assert_eq!(app.stream_status.header, "Thinking");
        assert_eq!(
            app.stream_status.details.as_deref(),
            Some("weighing options for the migration")
        );
    }

    #[test]
    fn tool_executing_sets_running_header_with_arg_summary() {
        let mut app = make_app();
        app.enter_streaming();
        app.process_agent_event(AgentEvent::ToolExecuting {
            name: "run_shell".into(),
            args: r#"{"command":"cargo test --all"}"#.into(),
        });
        assert_eq!(app.stream_status.header, "Running run_shell");
        assert_eq!(
            app.stream_status.details.as_deref(),
            Some("cargo test --all"),
            "details should prefer run_shell.command over a raw JSON blob"
        );
    }

    #[test]
    fn tool_result_clears_details_so_turn_doesnt_look_stuck_on_old_tool() {
        let mut app = make_app();
        app.enter_streaming();
        app.process_agent_event(AgentEvent::ToolExecuting {
            name: "run_shell".into(),
            args: r#"{"command":"ls"}"#.into(),
        });
        app.process_agent_event(AgentEvent::ToolResult {
            name: "run_shell".into(),
            result: "file1\nfile2".into(),
        });
        // After a tool result, header returns to a neutral "Working" state
        // until the next event decides what comes next (more reasoning /
        // another tool / final text).
        assert_eq!(app.stream_status.header, "Working");
        assert!(app.stream_status.details.is_none());
    }

    #[test]
    fn tool_output_delta_surfaces_latest_nonempty_line_as_details() {
        let mut app = make_app();
        app.enter_streaming();
        app.process_agent_event(AgentEvent::ToolOutputDelta {
            name: "run_shell".into(),
            delta: "compiling crate A\n\ncompiling crate B\n".into(),
            is_stderr: false,
        });
        // The trailing blank line must be skipped so the user sees the
        // freshest informative line.
        assert_eq!(
            app.stream_status.details.as_deref(),
            Some("compiling crate B")
        );
    }

    // summarize_tool_args ----------------------------------------------------

    #[test]
    fn summarize_tool_args_prefers_known_fields_per_tool() {
        use super::events::summarize_tool_args;
        assert_eq!(
            summarize_tool_args("read_file", r#"{"path":"src/main.rs","offset":0}"#),
            "src/main.rs",
            "path is preferred over offset for read_file"
        );
        assert_eq!(
            summarize_tool_args("web_fetch", r#"{"url":"https://example.com"}"#),
            "https://example.com"
        );
    }

    #[test]
    fn summarize_tool_args_falls_back_to_first_string_for_unknown_tools() {
        use super::events::summarize_tool_args;
        assert_eq!(
            summarize_tool_args("custom_tool", r#"{"foo":"bar"}"#),
            "bar"
        );
    }

    #[test]
    fn summarize_tool_args_returns_raw_on_parse_failure() {
        use super::events::summarize_tool_args;
        assert_eq!(summarize_tool_args("run_shell", "not json"), "not json");
    }

    #[test]
    fn summarize_tool_args_empty_is_empty() {
        use super::events::summarize_tool_args;
        assert_eq!(summarize_tool_args("run_shell", ""), "");
    }
}
