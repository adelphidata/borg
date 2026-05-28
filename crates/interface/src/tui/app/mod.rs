mod duration;
mod events;
mod key_handlers;
mod render;

use duration::fmt_compact_duration;

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

use super::agents_popup::AgentsPopup;
use super::btw_popup::BtwPopup;
use super::command_popup::CommandPopup;
use super::composer::Composer;
use super::file_popup::FileSearchPopup;
use super::file_search::FileSearchService;
use super::history::HistoryCell;
use super::mcp_popup::McpPopup;
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
use super::theme_popup::ThemePopup;
use super::transcript_pager::TranscriptPager;
use super::usage_popup::UsagePopup;

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
        $body!($self, theme_popup);
        $body!($self, plugins_popup);
        $body!($self, mcp_popup);
        $body!($self, pairing_popup);
        $body!($self, sessions_popup);
        $body!($self, schedule_popup);
        $body!($self, migrate_popup);
        $body!($self, status_popup);
        $body!($self, usage_popup);
        $body!($self, subprocesses_popup);
        $body!($self, agents_popup);
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
    /// Goal creation time as unix seconds. The footer subtracts this from
    /// `now` to render an elapsed-time tag like `(1h)`. `None` for `cleared`.
    pub started_at: Option<i64>,
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
    /// Purge terminal scrollback + visible screen without touching daemon
    /// conversation state. Used by Ctrl+L.
    ClearTranscript,
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
    /// `/theme` picker — pairs with `model_popup` as a quick-swap that
    /// avoids walking through `/settings`.
    pub theme_popup: ThemePopup,
    pub plugins_popup: PluginsPopup,
    /// Read-only operator view of configured MCP servers (`/mcp`).
    /// Pairs with `/plugins` — the WASM side stays in its own popup
    /// because the trust models, install verbs, and lifecycle don't
    /// share enough to fold into one screen.
    pub mcp_popup: McpPopup,
    /// Live spawn-tree dashboard for sub-agents (`/agents`). Reads
    /// `Admin.ListAgents` on open; mutating keys (`p`/`x`/`Shift+X`)
    /// route through the daemon's pause/kill RPCs.
    pub agents_popup: AgentsPopup,
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
    /// Goal `created_at` (unix seconds). The footer subtracts from `now` to
    /// produce the elapsed-time tag like `(1h)`. `None` mirrors
    /// `goal_status == None`.
    pub goal_started_at: Option<i64>,
    /// Active voice-mode loop. `Some` while `/voice on` or `/voice rec`
    /// is in flight; cleared on `/voice off` and `SilentLimitReached`.
    /// See `crates/cortex/src/media/voice_loop.rs`.
    pub voice_loop: Option<Arc<cortex::media::voice_loop::VoiceLoop>>,
    /// Drains lifecycle events from [`Self::voice_loop`]. Paired 1:1 with
    /// `voice_loop` — both fields are set/cleared together by the
    /// command handler.
    pub voice_rx: Option<mpsc::UnboundedReceiver<cortex::media::voice_loop::VoiceEvent>>,
    /// Latest status from the voice loop. Read by `/voice status` to
    /// report whether the loop is Listening / Transcribing / Idle.
    pub voice_status: Option<cortex::media::voice_loop::VoiceStatus>,
    /// Whether the active voice loop is a single-shot `/voice rec`. Used
    /// to suppress restart UX (no "listening…" indicator after the cycle
    /// closes).
    pub voice_single_shot: bool,
    /// Host for modal "views" displacing the composer — currently only the
    /// `request_user_input` picker overlay. When `is_active()` returns true
    /// the App routes keys to the pane and renders it above the composer.
    /// See `crates/interface/src/tui/bottom_pane/`.
    pub bottom_pane: super::bottom_pane::BottomPane,
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
            theme_popup: ThemePopup::new(),
            plugins_popup: PluginsPopup::new(),
            mcp_popup: McpPopup::new(),
            agents_popup: AgentsPopup::new(),
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
            goal_started_at: None,
            voice_loop: None,
            voice_rx: None,
            voice_status: None,
            voice_single_shot: false,
            bottom_pane: super::bottom_pane::BottomPane::new(),
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
    /// cached goal fields so the footer indicator re-renders on the next
    /// frame.
    pub fn apply_goal_event(&mut self, evt: GoalUiEvent) {
        if evt.kind == cortex::goals::event_kind::CLEARED {
            self.goal_status = None;
            self.goal_started_at = None;
        } else {
            self.goal_status = evt.status;
            // Only overwrite when the event carries a fresh started_at —
            // mutations like `set_budget` don't change it and shouldn't
            // reset the elapsed-time tag.
            if evt.started_at.is_some() {
                self.goal_started_at = evt.started_at;
            }
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
            || self.usage_popup.is_visible()
            || self.subprocesses_popup.is_visible()
            || self.agents_popup.is_visible()
            || self.btw_popup.is_visible()
            || self.plugins_popup.is_visible()
            || self.mcp_popup.is_visible()
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
            AppState::Idle => {
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
mod tests;
