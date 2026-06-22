//! Heartbeat scheduler glue inside the daemon.
//!
//! Owns the [`HeartbeatScheduler`] (from `heartbeat`), the agent turn
//! that runs on each fire, and the broadcast channel feeding
//! `Admin.SubscribeHeartbeatEvents` subscribers.
//!
//! Runs against `cortex::Agent` directly today; once the heartbeat
//! prompt builder moves inside the daemon's session machinery, this
//! module routes through `DaemonSessionExecutor` instead.

use crate::session::convert::{PROMPT_REQUEST_SHELL, PROMPT_REQUEST_USER_INPUT};
use crate::session::DaemonSessionExecutor;
use anyhow::Result;
use cortex::agent::AgentEvent;
use cortex::config::Config;
use cortex::goals::{Goal, GoalStatus};
use gateway::{InboundTurn, SessionExecutor, TurnKind};
use heartbeat::scheduler::{HeartbeatResult, SkipReason};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

/// Time we let the executor flush its terminal `Error` event after we
/// cancel a hung turn, before dropping the receiver. Avoids spurious
/// `broadcast::error::Closed` warnings in the bridge log when the backend
/// is mid-shutdown.
const HEARTBEAT_DRAIN_GRACE: Duration = Duration::from_millis(150);

/// Outcome of [`collect_heartbeat_response`]. Modeled separately from
/// `HeartbeatResult` so the helper stays a pure async-channel function with
/// no DB / config / dedup coupling — the outer `execute_heartbeat_turn`
/// owns all of that.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CollectOutcome {
    /// The session executor closed `event_rx` cleanly. Carries the
    /// concatenated `TextDelta` payload for downstream zero-info / dedup
    /// logic to inspect.
    Completed(String),
    /// The configured wall-clock budget elapsed before the channel closed.
    /// The cancel token has been triggered; the event_rx loop has already
    /// done a brief grace drain.
    Timeout,
    /// The agent emitted a `SystemNotice` whose category matches one of the
    /// `prompt_request:*` constants (the new wire shape produced by
    /// [`crate::session::convert::from_proto`] for `UserInputRequest` /
    /// `ShellConfirmation`). Heartbeats are non-interactive — there is no
    /// client to call `RespondToPrompt`, so the underlying tool would park
    /// for its 5-minute oneshot timeout. We cancel the turn and report
    /// immediately so the bus sees a terminal event inside the
    /// smorgasborg eval's 90s budget.
    InteractivePromptRequested(String),
}

/// Drive the heartbeat agent's event channel until the turn ends, the
/// configured `timeout` elapses, or the agent issues an interactive prompt
/// request. Returns immediately on cancel-worthy conditions and triggers
/// the cancel token so the executor's backend can wind down its turn.
pub(crate) async fn collect_heartbeat_response(
    mut event_rx: mpsc::Receiver<AgentEvent>,
    cancel: CancellationToken,
    timeout: Duration,
) -> CollectOutcome {
    let deadline = Instant::now() + timeout;
    let mut response = String::new();
    loop {
        let now = Instant::now();
        if now >= deadline {
            cancel.cancel();
            drain_briefly(&mut event_rx).await;
            return CollectOutcome::Timeout;
        }
        let remaining = deadline - now;
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Err(_) => {
                cancel.cancel();
                drain_briefly(&mut event_rx).await;
                return CollectOutcome::Timeout;
            }
            Ok(None) => return CollectOutcome::Completed(response),
            Ok(Some(AgentEvent::TextDelta(delta))) => {
                response.push_str(&delta);
            }
            Ok(Some(AgentEvent::SystemNotice { category, text }))
                if category == PROMPT_REQUEST_USER_INPUT || category == PROMPT_REQUEST_SHELL =>
            {
                cancel.cancel();
                drain_briefly(&mut event_rx).await;
                // Cap at 200 chars (mirrors `log_heartbeat_activity`) so a
                // pathological shell command can't bloat the activity log.
                let snippet: String = text.chars().take(200).collect();
                return CollectOutcome::InteractivePromptRequested(format!(
                    "{category}: {snippet}"
                ));
            }
            Ok(Some(_)) => {
                // Other event variants (ToolExecuting, Usage, unrelated
                // SystemNotices, …) don't contribute to the textual
                // response but also don't end the turn.
            }
        }
    }
}

async fn drain_briefly(event_rx: &mut mpsc::Receiver<AgentEvent>) {
    let drain_deadline = Instant::now() + HEARTBEAT_DRAIN_GRACE;
    loop {
        let now = Instant::now();
        if now >= drain_deadline {
            return;
        }
        let remaining = drain_deadline - now;
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Err(_) | Ok(None) => return,
            Ok(Some(_)) => {}
        }
    }
}

/// Why a heartbeat turn is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatSource {
    /// Scheduler fire — interval, cron, or `/poke`.
    Scheduled,
    /// TUI opened post-onboarding — proactive greeting.
    SessionStart,
}

impl HeartbeatSource {
    /// Serialize to the proto wire string.
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::SessionStart => "session_start",
        }
    }

    /// Parse from the proto wire string.
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "scheduled" => Some(Self::Scheduled),
            "session_start" => Some(Self::SessionStart),
            _ => None,
        }
    }
}

/// External request to run a heartbeat turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatTrigger {
    /// Why this turn is running.
    pub source: HeartbeatSource,
    /// When true, run even if quiet hours would normally suppress it.
    pub bypass_quiet_hours: bool,
}

/// Daemon broadcast event for `Admin.SubscribeHeartbeatEvents`.
#[derive(Debug, Clone)]
pub struct DaemonHeartbeatEvent {
    /// Why this turn ran.
    pub source: HeartbeatSource,
    /// Unix timestamp at turn start.
    pub started_at_unix: i64,
    /// Unix timestamp at turn finish.
    pub finished_at_unix: i64,
    /// `"ran"` | `"skipped"` | `"failed"`.
    pub status: &'static str,
    /// User-visible message or skip/failure reason.
    pub message: String,
    /// Structured skip reason for `"skipped"` events, when available.
    pub skip_reason: Option<&'static str>,
    /// Wall-clock duration of the agent turn.
    pub duration_ms: f64,
}

/// Bus of heartbeat events. Cheap-to-clone — wraps a `broadcast::Sender`.
#[derive(Clone)]
pub struct HeartbeatBus {
    tx: broadcast::Sender<DaemonHeartbeatEvent>,
}

impl Default for HeartbeatBus {
    fn default() -> Self {
        // Capacity 32: deep enough that a momentarily-slow subscriber doesn't
        // get marked Lagged on bursty SessionStart greetings; small enough that
        // a leaked subscriber can't pin much memory.
        let (tx, _) = broadcast::channel(32);
        Self { tx }
    }
}

impl HeartbeatBus {
    /// Subscribe to heartbeat events. Drop the receiver to unsubscribe.
    pub fn subscribe(&self) -> broadcast::Receiver<DaemonHeartbeatEvent> {
        self.tx.subscribe()
    }

    fn publish(&self, evt: DaemonHeartbeatEvent) {
        // `send` errors only when there are no live subscribers — that's the
        // expected steady state when nobody has opened the TUI yet, so no log.
        let _ = self.tx.send(evt);
    }

    /// Test-only: publish a synthetic event. Wraps the private `publish` so
    /// integration tests don't need to spin up a real LLM-backed turn just to
    /// assert that the wire-level subscription forwards events.
    #[doc(hidden)]
    pub fn publish_for_test(&self, evt: DaemonHeartbeatEvent) {
        self.publish(evt)
    }
}

/// Process-wide handle to the live `HeartbeatBus`. Set once by
/// `heartbeat::spawn` so the unified scheduler dispatcher
/// (`task_runner::execute_heartbeat_task`) can publish events into the same
/// bus that `Admin.SubscribeHeartbeatEvents` reads from — without plumbing
/// the bus through `TaskExecContext`. Without this, scheduled heartbeat
/// firings would be invisible to TUI status views.
static GLOBAL_BUS: OnceLock<HeartbeatBus> = OnceLock::new();

/// Publish a heartbeat event into the process-global bus, if one has been
/// installed. No-op when called before `heartbeat::spawn` (e.g. unit tests
/// that drive the dispatcher in isolation).
pub(crate) fn publish_global(evt: DaemonHeartbeatEvent) {
    if let Some(bus) = GLOBAL_BUS.get() {
        bus.publish(evt);
    }
}

/// Spawn the heartbeat scheduler + an agent-turn runner. Returns the bus
/// that `Admin.SubscribeHeartbeatEvents` reads from, the rich trigger channel
/// Admin RPCs use, and the legacy `poke_tx` channel the HTTP gateway still
/// signals.
///
/// `shutdown` cancels both spawned tasks. `executor` is used to run each
/// heartbeat turn through the daemon's session machinery (so heartbeats share
/// the per-session history + hooks like any other turn).
pub fn spawn(
    config: Config,
    executor: Arc<DaemonSessionExecutor>,
    shutdown: CancellationToken,
    config_rx: watch::Receiver<Config>,
) -> (
    HeartbeatBus,
    mpsc::Sender<HeartbeatTrigger>,
    mpsc::Sender<()>,
) {
    // Scheduled heartbeat firings flow through the unified scheduled-task
    // dispatcher (`task_type="heartbeat"` row in `scheduled_tasks`); spawning
    // a parallel in-process timer here would double-fire on every tick. The
    // trigger-channel loop below is preserved so TUI-initiated SessionStart
    // greetings (which still need executor context) work.
    let _ = config;
    let (trigger_tx, mut trigger_rx) = mpsc::channel::<HeartbeatTrigger>(8);
    let (poke_tx, _poke_rx) = mpsc::channel::<()>(8);

    let bus = HeartbeatBus::default();
    // Install the bus in the process-global slot so the unified scheduled-task
    // dispatcher can publish into it. Idempotent — repeated calls (e.g. across
    // test setups) keep the first bus and silently ignore later inits.
    let _ = GLOBAL_BUS.set(bus.clone());
    let bus_for_runner = bus.clone();
    let runner_cancel = shutdown;
    let runner_executor = executor;
    // Hold the watch receiver and re-borrow the latest `Config` per iteration
    // so quiet-hours suppression on `Admin.Trigger`, dedup-window sizing, and
    // channel delivery all see edits made via `Admin.SetSetting` immediately.
    let mut runner_config_rx = config_rx;
    tokio::spawn(async move {
        loop {
            let runner_config = runner_config_rx.borrow_and_update().clone();
            tokio::select! {
                biased;
                _ = runner_cancel.cancelled() => return,
                trigger = trigger_rx.recv() => {
                    let Some(trigger) = trigger else { return };
                    if should_skip_explicit_trigger(trigger, &runner_config) {
                        publish_scheduler_result(
                            &bus_for_runner,
                            trigger.source,
                            HeartbeatResult::Skipped {
                                reason: SkipReason::QuietHours,
                            },
                        );
                        continue;
                    }
                    run_and_broadcast(
                        runner_executor.as_ref(),
                        &runner_config,
                        trigger,
                        &bus_for_runner,
                    ).await;
                }
                _ = runner_config_rx.changed() => {
                    // Loop back so the next iteration re-borrows the fresh config.
                }
            }
        }
    });

    (bus, trigger_tx, poke_tx)
}

/// Run a heartbeat turn and broadcast the result on `bus`. Fire-and-forget.
///
/// Takes `&dyn SessionExecutor` rather than the concrete
/// `DaemonSessionExecutor` so integration tests can substitute a stub —
/// the function only needs `executor.start_turn(...)`. Production callers
/// pass the real `DaemonSessionExecutor` unchanged.
pub async fn run_and_broadcast(
    executor: &dyn SessionExecutor,
    config: &Config,
    trigger: HeartbeatTrigger,
    bus: &HeartbeatBus,
) {
    let started = chrono::Utc::now().timestamp();
    let started_at = Instant::now();
    let result = execute_heartbeat_turn(executor, config, trigger.source).await;
    let finished = chrono::Utc::now().timestamp();
    let duration_ms = started_at.elapsed().as_secs_f64() * 1000.0;

    let (status, message, skip_reason) = match result {
        HeartbeatResult::Ran { message, .. } => {
            deliver_to_channels(config, &message).await;
            ("ran", message, None)
        }
        HeartbeatResult::Skipped { reason } => (
            "skipped",
            reason.to_string(),
            Some(skip_reason_wire(&reason)),
        ),
        HeartbeatResult::Failed { error } => ("failed", error, None),
    };

    log_heartbeat_activity(trigger.source, status, &message);

    bus.publish(DaemonHeartbeatEvent {
        source: trigger.source,
        started_at_unix: started,
        finished_at_unix: finished,
        status,
        message,
        skip_reason,
        duration_ms,
    });
}

/// Record a heartbeat outcome to `activity_log`. Doctor's `check_heartbeat`
/// reads this table to detect a stalled scheduler — without these writes
/// the check warns "no heartbeat activity recorded" even when ticks are
/// firing normally. Best-effort: a failed log entry must not break the
/// heartbeat path.
pub(crate) fn log_heartbeat_activity(source: HeartbeatSource, status: &str, message: &str) {
    // Fire-and-forget — opening the DB and writing the row is fast, but
    // we run it on a blocking thread to keep this off the runtime.
    let source_wire = source.as_wire().to_string();
    let status = status.to_string();
    let level = if status == "ran" { "info" } else { "debug" };
    let summary = if message.is_empty() {
        format!("heartbeat ({source_wire}): {status}")
    } else {
        // chars().take to avoid splitting a multibyte codepoint — heartbeat
        // messages can carry channel names, file paths, or LLM output with
        // non-ASCII characters that would otherwise panic on byte slicing.
        let snippet: String = message
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .chars()
            .take(200)
            .collect();
        format!("heartbeat ({source_wire}, {status}): {snippet}")
    };
    tokio::spawn(async move {
        match cortex::db::Database::open().await {
            Ok(db) => {
                if let Err(e) = db.log_activity(level, "heartbeat", &summary, None).await {
                    tracing::warn!("heartbeat: log_activity failed: {e}");
                }
            }
            Err(e) => tracing::warn!("heartbeat: db open for activity log failed: {e}"),
        }
    });
}

fn should_skip_explicit_trigger(trigger: HeartbeatTrigger, config: &Config) -> bool {
    !trigger.bypass_quiet_hours
        && trigger.source == HeartbeatSource::Scheduled
        && is_quiet_hours(config)
}

fn publish_scheduler_result(bus: &HeartbeatBus, source: HeartbeatSource, result: HeartbeatResult) {
    let (status, message, skip_reason) = match result {
        HeartbeatResult::Ran {
            message,
            duration_ms: _,
        } => ("ran", message, None),
        HeartbeatResult::Skipped { reason } => (
            "skipped",
            reason.to_string(),
            Some(skip_reason_wire(&reason)),
        ),
        HeartbeatResult::Failed { error } => ("failed", error, None),
    };

    log_heartbeat_activity(source, status, &message);

    let now = chrono::Utc::now().timestamp();
    bus.publish(DaemonHeartbeatEvent {
        source,
        started_at_unix: now,
        finished_at_unix: now,
        status,
        message,
        skip_reason,
        duration_ms: 0.0,
    });
}

/// Run a heartbeat turn through the daemon's `DaemonSessionExecutor` and
/// return a structured outcome. The session_key is stable per source so
/// scheduled and session-start heartbeats reuse their respective histories.
///
/// Routes through the SessionRegistry rather than constructing an `Agent`
/// directly, so hooks (vitals, evolution, …) fire the same way they do for
/// user-driven turns.
pub async fn execute_heartbeat_turn(
    executor: &dyn SessionExecutor,
    config: &Config,
    source: HeartbeatSource,
) -> HeartbeatResult {
    static LAST_HASH: AtomicU64 = AtomicU64::new(0);
    static LAST_HASH_TIME: AtomicU64 = AtomicU64::new(0);

    // No LLM provider = no possible turn. Skip cleanly instead of letting the
    // agent loop bail with a loud `Failed { "No LLM provider selected" }` —
    // the most common cause is the onboarding race (provider just written to
    // the DB but the ConfigWatcher's 3s poll hasn't refreshed this backend's
    // snapshot yet), and a "heartbeat errored during first-run" toast is
    // exactly the wrong first impression. Also catches deliberately-unset
    // and credential-revoked states.
    if config
        .llm
        .provider
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        return HeartbeatResult::Skipped {
            reason: SkipReason::NoLlmProvider,
        };
    }

    let started = Instant::now();

    let mut config = config.clone();
    config.llm.cache.ttl = config.llm.cache.ttl.resolve(true);

    let user_msg = match source {
        HeartbeatSource::Scheduled => {
            let nudges = collect_scheduled_nudges(&config).await;
            build_scheduled_heartbeat_prompt(nudges)
        }
        HeartbeatSource::SessionStart => build_session_start_prompt(&config).await,
    };

    let session_key = format!("heartbeat:{}", source.as_wire());
    let turn = InboundTurn {
        session_key,
        text: user_msg,
        images: Vec::new(),
        kind: TurnKind::Internal,
    };
    let (event_rx, cancel) = match executor.start_turn(turn).await {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!("Heartbeat: executor.start_turn failed: {e}");
            return HeartbeatResult::Failed {
                error: format!("agent error: {e}"),
            };
        }
    };

    let timeout_secs = config.heartbeat.turn_timeout_secs.max(1);
    let outcome =
        collect_heartbeat_response(event_rx, cancel, Duration::from_secs(timeout_secs)).await;
    let duration_ms = started.elapsed().as_millis() as u64;

    let response = match outcome {
        CollectOutcome::Completed(text) => text,
        CollectOutcome::Timeout => {
            return HeartbeatResult::Failed {
                error: format!("heartbeat turn exceeded {timeout_secs}s timeout"),
            };
        }
        CollectOutcome::InteractivePromptRequested(detail) => {
            return HeartbeatResult::Failed {
                error: format!("agent requested interactive input during heartbeat ({detail})"),
            };
        }
    };

    let trimmed = response.trim().to_string();

    if trimmed.is_empty() || is_zero_info_heartbeat(&trimmed) {
        return HeartbeatResult::Skipped {
            reason: SkipReason::EmptyResponse,
        };
    }

    let mut hasher = DefaultHasher::new();
    trimmed.hash(&mut hasher);
    let hash = hasher.finish();
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let prev_hash = LAST_HASH.swap(hash, Ordering::Relaxed);
    let prev_time = LAST_HASH_TIME.swap(now_secs, Ordering::Relaxed);

    let dedup_window = cortex::tasks::parse_interval(&config.heartbeat.interval)
        .unwrap_or(std::time::Duration::from_secs(1800))
        .as_secs()
        .saturating_mul(2);

    if prev_hash == hash && prev_time > 0 && now_secs.saturating_sub(prev_time) < dedup_window {
        return HeartbeatResult::Skipped {
            reason: SkipReason::DuplicateResponse,
        };
    }

    match cortex::db::Database::open().await {
        Ok(adb) => {
            if let Err(e) = adb
                .set_setting("heartbeat.last_fired_at", &now_secs.to_string())
                .await
            {
                tracing::warn!("Heartbeat: failed to persist last_fired_at: {e}");
            }
        }
        Err(e) => {
            tracing::warn!("Heartbeat: failed to open DB for last_fired_at persistence: {e}");
        }
    }

    HeartbeatResult::Ran {
        message: trimmed,
        duration_ms,
    }
}

/// Daemon variant of `is_zero_info_heartbeat` (kept in sync with cli's copy).
pub(crate) fn is_zero_info_heartbeat(response: &str) -> bool {
    const TRIVIAL_PATTERNS: &[&str] = &[
        "ok",
        "okay",
        "nothing new",
        "nothing to report",
        "no updates",
        "no news",
        "all good",
        "all clear",
        "all quiet",
        "all is well",
        "quiet",
        "no changes",
        "nothing",
        "nothing here",
        "nothing happening",
        "standing by",
        "standing by.",
    ];
    let normalized: String = response
        .trim()
        .trim_end_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    if normalized.is_empty() {
        return true;
    }
    if normalized.len() > 40 {
        return false;
    }
    TRIVIAL_PATTERNS.iter().any(|pat| {
        normalized == *pat || normalized == format!("{pat}.") || normalized == format!("{pat}!")
    })
}

fn skip_reason_wire(reason: &SkipReason) -> &'static str {
    match reason {
        SkipReason::QuietHours => "quiet_hours",
        SkipReason::EmptyResponse => "empty_response",
        SkipReason::DuplicateResponse => "duplicate_response",
        SkipReason::NoLlmProvider => "no_llm_provider",
    }
}

/// Collect this tick's proactive nudges. Each augmenter is rate-limited via
/// the `meta` table, so `collect` mutates cooldown state and MUST be called
/// exactly once per tick — the caller decides the agentic-vs-cheap path from
/// whether the result is empty, then threads the same `Vec` into
/// [`build_scheduled_heartbeat_prompt`].
pub(crate) async fn collect_scheduled_nudges(config: &Config) -> Vec<String> {
    let nudge_db = open_heartbeat_db_for_nudges().await;
    crate::heartbeat_augmenters::collect(config, nudge_db.as_ref()).await
}

pub(crate) fn build_scheduled_heartbeat_prompt(nudges: Vec<String>) -> String {
    let mut user_msg = "*heartbeat tick*".to_string();
    if let Some(cl) = cortex::memory::load_heartbeat_checklist() {
        user_msg.push_str("\n\n# Heartbeat Checklist\n");
        user_msg.push_str(&cl);
    }
    append_proactive_nudges(&mut user_msg, nudges);
    user_msg
}

/// Session key for the scheduled-heartbeat conversation. Remediation goals
/// live here so they persist across ticks and resume automatically — the
/// owner never chats in this session, so any goal present is a remediation
/// goal the heartbeat itself created.
pub(crate) fn scheduled_session_key() -> String {
    format!("heartbeat:{}", HeartbeatSource::Scheduled.as_wire())
}

/// Meta-table keys backing the max-ticks backstop. `TICKS` counts how many
/// scheduled ticks the current goal has stayed `Active`; `GOAL_ID` records
/// which goal the count belongs to so a fresh goal restarts the counter.
const REMEDIATION_TICKS_KEY: &str = "heartbeat_remediation_ticks";
const REMEDIATION_GOAL_KEY: &str = "heartbeat_remediation_goal_id";

/// Run an agentic remediation turn through the executor — the full agent loop
/// (tools + the per-session goal/Ralph loop), unlike the cheap tool-less
/// check-in. Returns the collected response text. The turn runs in the
/// scheduled-heartbeat session so an agent-created goal persists across ticks.
pub(crate) async fn run_scheduled_remediation_turn(
    executor: &dyn SessionExecutor,
    user_msg: String,
    timeout_secs: u64,
) -> CollectOutcome {
    let session_key = scheduled_session_key();
    // Materialize the session under this exact id BEFORE the turn so
    // `AgentBackend::new`'s `load_session(session_key)` succeeds and the
    // agent's `meta.id` becomes `session_key` rather than a fresh random
    // UUID. Without this the agent-created goal is stored under a random id
    // and the budget / max-ticks / escalation queries below — which key on
    // `session_key` — would never find it.
    if let Err(e) = cortex::session::ensure_session_on_disk(&session_key) {
        tracing::warn!(
            "Heartbeat remediation: could not materialize session '{session_key}': {e}"
        );
    }
    let turn = InboundTurn {
        session_key,
        text: user_msg,
        images: Vec::new(),
        kind: TurnKind::Internal,
    };
    match executor.start_turn(turn).await {
        Ok((event_rx, cancel)) => {
            collect_heartbeat_response(event_rx, cancel, Duration::from_secs(timeout_secs.max(1)))
                .await
        }
        Err(e) => {
            tracing::warn!("Heartbeat remediation: start_turn failed: {e}");
            CollectOutcome::Completed(String::new())
        }
    }
}

/// What to do with the heartbeat message after an agentic remediation turn.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RemediationFollowup {
    /// Deliver this message regardless of the agent's own (possibly silent)
    /// output — a terminal or escalation announcement the owner must see.
    Announce(String),
    /// No override; run the agent's own response text through the normal
    /// silent / zero-info / dedup gates.
    Passthrough,
}

/// Inspect the remediation goal after an agentic turn and decide follow-up:
/// enforce the per-goal token budget, advance the max-ticks backstop, and
/// turn terminal goal states into owner-facing announcements.
pub(crate) async fn handle_remediation_goal(config: &Config) -> RemediationFollowup {
    let db = match cortex::db::Database::open().await {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!("Heartbeat remediation: DB open for goal follow-up failed: {e}");
            return RemediationFollowup::Passthrough;
        }
    };
    let session_key = scheduled_session_key();
    let goal = match db.get_goal(&session_key).await {
        Ok(Some(g)) => g,
        Ok(None) => {
            // No goal: the agent fixed it inline this turn or judged none was
            // warranted. Drop any stale counter and pass through.
            clear_remediation_ticks(&db).await;
            return RemediationFollowup::Passthrough;
        }
        Err(e) => {
            tracing::warn!("Heartbeat remediation: get_goal failed: {e}");
            return RemediationFollowup::Passthrough;
        }
    };

    let max = config.heartbeat.remediation_max_ticks;
    match goal.status {
        // All terminal states: announce once, then delete the goal. Deleting
        // (rather than leaving it Blocked) means the escalation can't re-fire
        // every subsequent tick and the session frees up for an unrelated
        // future remediation. Heartbeat goals are autonomous — the owner
        // doesn't resume them via `/goal`; if the problem persists a later
        // nudge re-attempts with a fresh goal.
        GoalStatus::Complete
        | GoalStatus::Blocked
        | GoalStatus::UsageLimited
        | GoalStatus::BudgetLimited => {
            let followup = remediation_followup_for(goal.status, &goal.objective, 0, max);
            if let Err(e) = db.delete_goal(&session_key).await {
                tracing::warn!("Heartbeat remediation: delete terminal goal failed: {e}");
            }
            clear_remediation_ticks(&db).await;
            followup
        }
        GoalStatus::Paused => RemediationFollowup::Passthrough,
        GoalStatus::Active => {
            ensure_remediation_budget(&db, config, &goal).await;
            let ticks = bump_remediation_ticks(&db, &goal.goal_id).await;
            let followup =
                remediation_followup_for(GoalStatus::Active, &goal.objective, ticks, max);
            // An Announce on an Active goal means the max-ticks backstop tripped
            // — the goal keeps believing it can finish (or is stalled on an
            // un-fixable error like a provider 401). Delete it so it can't churn
            // forever; a future nudge can re-attempt fresh.
            if matches!(followup, RemediationFollowup::Announce(_)) {
                if let Err(e) = db.delete_goal(&session_key).await {
                    tracing::warn!("Heartbeat remediation: delete stalled goal failed: {e}");
                }
                clear_remediation_ticks(&db).await;
            }
            followup
        }
    }
}

/// Pure message-selection for a remediation follow-up: maps a goal's status
/// (and, for an `Active` goal, the running tick count vs the backstop) to
/// either an owner-facing announcement or a passthrough. Kept side-effect-free
/// so the escalation matrix — including the max-ticks backstop — is unit
/// testable without a DB.
pub(crate) fn remediation_followup_for(
    status: GoalStatus,
    objective: &str,
    ticks: u32,
    max_ticks: u32,
) -> RemediationFollowup {
    match status {
        GoalStatus::Complete => RemediationFollowup::Announce(format!("✓ Resolved: {objective}")),
        GoalStatus::Blocked => RemediationFollowup::Announce(format!(
            "⚠ I hit a wall working on: {objective} — I need your help to go further."
        )),
        GoalStatus::UsageLimited => RemediationFollowup::Announce(format!(
            "⚠ Can't continue remediation of: {objective} — the LLM provider cut us off \
             (usage limit or invalid credentials). This needs your attention."
        )),
        GoalStatus::BudgetLimited => RemediationFollowup::Announce(format!(
            "⚠ Spent the remediation token budget on: {objective} without finishing. \
             Use /goal to resume if you want me to keep going."
        )),
        GoalStatus::Paused => RemediationFollowup::Passthrough,
        GoalStatus::Active => {
            if max_ticks > 0 && ticks > max_ticks {
                RemediationFollowup::Announce(format!(
                    "⚠ Couldn't resolve: {objective} after {ticks} attempts — pausing it. \
                     This likely needs your help."
                ))
            } else {
                // In-progress: let the agent's own (usually silent) output run
                // through the normal gates so the owner isn't pinged each tick.
                RemediationFollowup::Passthrough
            }
        }
    }
}

/// Whether a scheduled tick should run the agentic remediation path instead of
/// the cheap tool-less check-in. Pure so the gate is table-testable: go agentic
/// only when remediation is enabled, an executor is available, and there is
/// something to act on (a nudge fired this tick, or a remediation goal is
/// already in flight).
pub(crate) fn remediation_gate_open(
    enabled: bool,
    has_executor: bool,
    has_nudges: bool,
    active_goal: bool,
) -> bool {
    enabled && has_executor && (has_nudges || active_goal)
}

/// Apply the configured remediation token budget to a goal the agent created
/// without one, so a single remediation can't spend unbounded tokens. No-op
/// when the goal already has a budget or the cap is disabled (`0`).
async fn ensure_remediation_budget(db: &cortex::db::Database, config: &Config, goal: &Goal) {
    if goal.token_budget.is_some() {
        return;
    }
    let budget = config.heartbeat.remediation_token_budget;
    if budget == 0 {
        return;
    }
    if let Err(e) = db
        .update_goal(
            &goal.session_id,
            cortex::db::GoalUpdate {
                token_budget: Some(Some(budget as i64)),
                expected_goal_id: Some(goal.goal_id.clone()),
                ..Default::default()
            },
        )
        .await
    {
        tracing::warn!("Heartbeat remediation: set goal budget failed: {e}");
    }
}

/// Increment the max-ticks counter, resetting it when the active goal changed.
/// Returns the post-increment count (1 on the first tick of a goal).
async fn bump_remediation_ticks(db: &cortex::db::Database, goal_id: &str) -> u32 {
    let same_goal = matches!(
        db.get_meta(REMEDIATION_GOAL_KEY).await,
        Ok(Some(ref prev)) if prev == goal_id
    );
    let prev = if same_goal {
        db.get_meta(REMEDIATION_TICKS_KEY)
            .await
            .ok()
            .flatten()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0)
    } else {
        0
    };
    let count = prev.saturating_add(1);
    if let Err(e) = db.set_meta(REMEDIATION_GOAL_KEY, goal_id).await {
        tracing::warn!("Heartbeat remediation: persist goal-id meta failed: {e}");
    }
    if let Err(e) = db.set_meta(REMEDIATION_TICKS_KEY, &count.to_string()).await {
        tracing::warn!("Heartbeat remediation: persist ticks meta failed: {e}");
    }
    count
}

/// Reset the max-ticks backstop once a remediation reaches a terminal state.
async fn clear_remediation_ticks(db: &cortex::db::Database) {
    if let Err(e) = db.set_meta(REMEDIATION_TICKS_KEY, "0").await {
        tracing::warn!("Heartbeat remediation: clear ticks meta failed: {e}");
    }
    if let Err(e) = db.set_meta(REMEDIATION_GOAL_KEY, "").await {
        tracing::warn!("Heartbeat remediation: clear goal-id meta failed: {e}");
    }
}

async fn build_session_start_prompt(config: &Config) -> String {
    use chrono::{TimeZone, Timelike, Utc};

    let tz = config.user_timezone();
    let now_utc = Utc::now();
    let now_local = tz.from_utc_datetime(&now_utc.naive_utc());

    let hour = now_local.hour();
    let weekday = now_local.format("%A").to_string();

    let (recent_activity, hours_since_last) = match cortex::db::Database::open().await {
        Ok(db) => {
            let ctx = cortex::daily_summary::gather_daily_context(&db)
                .await
                .unwrap_or_default();
            let gap = db
                .sessions_since(0)
                .await
                .ok()
                .and_then(|rows| rows.first().map(|s| s.updated_at))
                .map(|ts| (now_utc.timestamp() - ts).max(0) / 3600);
            (ctx, gap)
        }
        Err(e) => {
            tracing::warn!("SessionStart: failed to open db for recent activity: {e}");
            (String::new(), None)
        }
    };

    let mut msg = String::from("*session start greeting*\n\n<time_context>\n");
    msg.push_str(&format!("hour={hour:02} weekday={weekday}"));
    if let Some(h) = hours_since_last {
        msg.push_str(&format!(" hours_since_last_session={h}"));
    } else {
        msg.push_str(" hours_since_last_session=none");
    }
    msg.push_str("\n</time_context>\n\n<recent_activity>\n");
    if recent_activity.trim().is_empty() {
        msg.push_str("(none)\n");
    } else {
        msg.push_str(&recent_activity);
    }
    msg.push_str("</recent_activity>\n\n<instructions>\n");
    msg.push_str(
        "The user just opened the TUI. Greet them briefly and warmly. \
         If recent_activity names a concrete thread, reference it specifically and offer to \
         continue. One or two sentences.\n",
    );
    msg.push_str("</instructions>");
    let nudge_db = open_heartbeat_db_for_nudges().await;
    append_proactive_nudges(
        &mut msg,
        crate::heartbeat_augmenters::collect_session_start(config, nudge_db.as_ref()).await,
    );
    msg
}

fn append_proactive_nudges(user_msg: &mut String, nudges: Vec<String>) {
    if nudges.is_empty() {
        return;
    }
    user_msg.push_str("\n\n<proactive_nudges>\n");
    for n in &nudges {
        user_msg.push_str("- ");
        user_msg.push_str(n);
        user_msg.push('\n');
    }
    user_msg.push_str("</proactive_nudges>");
    tracing::debug!("Heartbeat: injected {} proactive nudge(s)", nudges.len());
}

async fn open_heartbeat_db_for_nudges() -> Option<cortex::db::Database> {
    match cortex::db::Database::open().await {
        Ok(db) => Some(db),
        Err(e) => {
            tracing::warn!("Heartbeat: failed to open DB for proactive nudges: {e}");
            None
        }
    }
}

pub(crate) fn is_quiet_hours(config: &Config) -> bool {
    use chrono::{NaiveTime, Utc};

    let Some(start_str) = config.heartbeat.quiet_hours_start.as_deref() else {
        return false;
    };
    let Some(end_str) = config.heartbeat.quiet_hours_end.as_deref() else {
        return false;
    };
    let Ok(start) = NaiveTime::parse_from_str(start_str, "%H:%M") else {
        tracing::warn!("Invalid quiet_hours_start '{start_str}', expected HH:MM");
        return false;
    };
    let Ok(end) = NaiveTime::parse_from_str(end_str, "%H:%M") else {
        tracing::warn!("Invalid quiet_hours_end '{end_str}', expected HH:MM");
        return false;
    };

    let now = Utc::now().with_timezone(&config.user_timezone()).time();
    if start <= end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
}

/// Send `text` to every channel listed in `config.heartbeat.channels`.
pub(crate) async fn deliver_to_channels(config: &Config, text: &str) {
    for channel_name in &config.heartbeat.channels {
        if let Err(e) = deliver_to_channel(config, channel_name, text).await {
            tracing::warn!("Heartbeat delivery to {channel_name} failed: {e}");
        }
    }
}

async fn deliver_to_channel(config: &Config, channel_name: &str, text: &str) -> Result<()> {
    let recipients = resolve_recipients(config, channel_name).await?;
    if recipients.is_empty() {
        return Ok(());
    }
    for sender_id in &recipients {
        if let Err(e) = crate::channel_delivery::send(config, channel_name, sender_id, text).await {
            tracing::warn!("Heartbeat: deliver to {channel_name}:{sender_id} failed: {e}");
        }
    }
    Ok(())
}

async fn resolve_recipients(config: &Config, channel_name: &str) -> Result<Vec<String>> {
    let db = cortex::db::Database::open().await?;
    let approved = db.list_approved_senders(Some(channel_name)).await?;
    let approved_ids: Vec<String> = approved.into_iter().map(|s| s.sender_id).collect();
    Ok(resolve_recipients_for_channel(
        config.heartbeat.recipients.get(channel_name),
        approved_ids,
    ))
}

/// Decide which approved sender IDs receive a heartbeat for one channel.
///
/// Pure (no DB) so the precedence rules are unit-testable:
/// - no approved senders → nobody;
/// - override contains `"*"` → every approved sender;
/// - non-empty override → approved senders also present in the override,
///   in override order;
/// - no override (or an empty one) → just the first approved sender.
fn resolve_recipients_for_channel(
    override_ids: Option<&Vec<String>>,
    approved_ids: Vec<String>,
) -> Vec<String> {
    if approved_ids.is_empty() {
        return Vec::new();
    }
    match override_ids {
        Some(ids) if ids.iter().any(|s| s == "*") => approved_ids,
        Some(ids) if !ids.is_empty() => {
            let approved_set: std::collections::HashSet<&str> =
                approved_ids.iter().map(String::as_str).collect();
            ids.iter()
                .filter(|id| approved_set.contains(id.as_str()))
                .cloned()
                .collect()
        }
        _ => approved_ids.into_iter().take(1).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The agentic-vs-cheap gate: go agentic only when remediation is enabled,
    /// an executor exists, and there is something to act on (a nudge fired or a
    /// remediation goal is already running). Every other combination must stay
    /// on the cheap tool-less path so quiet ticks don't pay for the agent loop.
    #[test]
    fn remediation_gate_requires_enabled_executor_and_a_trigger() {
        // (enabled, has_executor, has_nudges, active_goal) -> open?
        let cases = [
            ((true, true, true, false), true),   // nudge fired
            ((true, true, false, true), true),   // goal in flight
            ((true, true, true, true), true),    // both
            ((true, true, false, false), false), // quiet tick → cheap path
            ((false, true, true, true), false),  // master switch off
            ((true, false, true, true), false),  // no executor (e.g. tests)
        ];
        for ((enabled, exec, nudges, goal), expected) in cases {
            assert_eq!(
                remediation_gate_open(enabled, exec, nudges, goal),
                expected,
                "gate({enabled},{exec},{nudges},{goal})"
            );
        }
    }

    /// Terminal goal states must escalate to the owner; an in-progress goal
    /// stays silent until the max-ticks backstop trips, at which point it
    /// escalates rather than churning forever.
    #[test]
    fn remediation_followup_escalates_terminal_states_and_backstop() {
        use GoalStatus::*;
        let obj = "fix the stale cache";
        let max = 12;

        for status in [Complete, Blocked, UsageLimited, BudgetLimited] {
            assert!(
                matches!(
                    remediation_followup_for(status, obj, 0, max),
                    RemediationFollowup::Announce(ref m) if m.contains(obj)
                ),
                "{status:?} should announce and name the objective"
            );
        }

        // Paused is owner-driven; nothing to say.
        assert_eq!(
            remediation_followup_for(Paused, obj, 0, max),
            RemediationFollowup::Passthrough
        );

        // Active under the cap stays quiet (agent's own output passes through).
        assert_eq!(
            remediation_followup_for(Active, obj, max, max),
            RemediationFollowup::Passthrough
        );

        // Active past the cap escalates and reports the attempt count.
        match remediation_followup_for(Active, obj, max + 1, max) {
            RemediationFollowup::Announce(m) => {
                assert!(m.contains(obj));
                assert!(m.contains(&(max + 1).to_string()), "names attempt count");
            }
            RemediationFollowup::Passthrough => panic!("backstop must escalate past max_ticks"),
        }

        // max_ticks == 0 disables the backstop — an Active goal never escalates
        // on count alone, no matter how high.
        assert_eq!(
            remediation_followup_for(Active, obj, 9999, 0),
            RemediationFollowup::Passthrough
        );
    }

    /// `heartbeat::spawn` MUST NOT spawn an in-process
    /// `HeartbeatScheduler::run` timer. Scheduled heartbeat firings flow
    /// through the unified scheduled-task dispatcher
    /// (`task_type="heartbeat"`); a parallel timer would double-fire on every
    /// tick. Source-grep guard so a future refactor can't silently
    /// re-introduce the parallel loop. Pairs with the TUI mouse-capture
    /// invariant guards.
    #[test]
    fn heartbeat_spawn_does_not_run_legacy_scheduler() {
        // Build the forbidden patterns at runtime so this guard test's own
        // source text doesn't trip the grep against itself.
        let scheduler_run = ["scheduler", "run("].join(".");
        let scheduler_new = ["HeartbeatScheduler", "new("].join("::");
        let src = include_str!("heartbeat.rs");
        // Strip this test's own body before searching so its mention of the
        // patterns inside string literals doesn't false-positive.
        let test_marker = "fn heartbeat_spawn_does_not_run_legacy_scheduler";
        let scan = match src.find(test_marker) {
            Some(idx) => &src[..idx],
            None => src,
        };
        assert!(
            !scan.contains(&scheduler_run),
            "heartbeat::spawn must not invoke the legacy timer — scheduled \
             firings route through the task dispatcher; a parallel loop \
             would double-fire on every tick"
        );
        assert!(
            !scan.contains(&scheduler_new),
            "heartbeat::spawn must not construct the legacy scheduler — the \
             seeded heartbeat row owns the schedule"
        );
    }

    #[test]
    fn zero_info_recognizes_trivial_acks() {
        for ack in &[
            "OK",
            "ok.",
            "Nothing new",
            "all good",
            "   nothing to report   ",
        ] {
            assert!(is_zero_info_heartbeat(ack), "expected '{ack}' zero-info");
        }
    }

    #[test]
    fn zero_info_passes_through_real_content() {
        for msg in &[
            "You have 3 new emails",
            "ok, I finished reviewing the PR and left 4 comments on auth.rs",
        ] {
            assert!(!is_zero_info_heartbeat(msg), "expected '{msg}' real");
        }
    }

    #[test]
    fn heartbeat_source_round_trips_wire_values() {
        assert_eq!(
            HeartbeatSource::from_wire("scheduled"),
            Some(HeartbeatSource::Scheduled)
        );
        assert_eq!(
            HeartbeatSource::from_wire("session_start"),
            Some(HeartbeatSource::SessionStart)
        );
        assert_eq!(HeartbeatSource::from_wire("bogus"), None);
    }

    #[test]
    fn skip_reason_wire_uses_stable_strings() {
        assert_eq!(skip_reason_wire(&SkipReason::QuietHours), "quiet_hours");
        assert_eq!(
            skip_reason_wire(&SkipReason::EmptyResponse),
            "empty_response"
        );
        assert_eq!(
            skip_reason_wire(&SkipReason::NoLlmProvider),
            "no_llm_provider"
        );
        assert_eq!(
            skip_reason_wire(&SkipReason::DuplicateResponse),
            "duplicate_response"
        );
    }

    #[tokio::test]
    async fn collect_heartbeat_response_completes_on_channel_close() {
        // Positive path: agent emits two TextDeltas and the executor closes
        // the channel. We must concatenate the text and return Completed.
        let (tx, rx) = mpsc::channel::<AgentEvent>(8);
        tx.send(AgentEvent::TextDelta("hello, ".into()))
            .await
            .unwrap();
        tx.send(AgentEvent::TextDelta("world".into()))
            .await
            .unwrap();
        drop(tx);
        let cancel = CancellationToken::new();
        let outcome = collect_heartbeat_response(rx, cancel.clone(), Duration::from_secs(5)).await;
        assert_eq!(outcome, CollectOutcome::Completed("hello, world".into()));
        assert!(
            !cancel.is_cancelled(),
            "clean channel close must not cancel the turn"
        );
    }

    #[tokio::test]
    async fn collect_heartbeat_response_returns_timeout_when_no_terminal_event() {
        // Negative regression for the original bug class: if the agent
        // hangs (channel never closes, no TurnComplete), the helper must
        // bound the wait and trigger the cancel token so the executor can
        // wind down the turn. Pre-fix this path waited forever.
        let (tx, rx) = mpsc::channel::<AgentEvent>(8);
        let cancel = CancellationToken::new();
        let outcome =
            collect_heartbeat_response(rx, cancel.clone(), Duration::from_millis(50)).await;
        assert_eq!(outcome, CollectOutcome::Timeout);
        assert!(
            cancel.is_cancelled(),
            "timeout must cancel the turn so the backend stops working"
        );
        drop(tx);
    }

    #[tokio::test]
    async fn collect_heartbeat_response_short_circuits_on_user_input_prompt_request() {
        // Direct regression guard for issues/empty-state-heartbeat-poke-yields-zero-*.md.
        // Pre-fix, `from_proto` dropped UserInputRequest; the bridge
        // produced no event and the heartbeat hung for the request_user_input
        // tool's 5-minute oneshot. Post-fix it surfaces as a SystemNotice with
        // PROMPT_REQUEST_USER_INPUT, which we must treat as a terminal
        // condition for the (non-interactive) heartbeat turn.
        let (tx, rx) = mpsc::channel::<AgentEvent>(8);
        tx.send(AgentEvent::SystemNotice {
            category: PROMPT_REQUEST_USER_INPUT.into(),
            text: "1 questions".into(),
        })
        .await
        .unwrap();
        let cancel = CancellationToken::new();
        let started = Instant::now();
        // Use a long timeout so a buggy implementation that fell through to
        // the deadline would visibly take seconds.
        let outcome = collect_heartbeat_response(rx, cancel.clone(), Duration::from_secs(30)).await;
        let elapsed = started.elapsed();
        match outcome {
            CollectOutcome::InteractivePromptRequested(detail) => {
                assert!(
                    detail.contains(PROMPT_REQUEST_USER_INPUT),
                    "detail should identify the prompt category, got {detail:?}"
                );
            }
            other => panic!("expected InteractivePromptRequested, got {other:?}"),
        }
        assert!(cancel.is_cancelled(), "must cancel the agent turn");
        assert!(
            elapsed < Duration::from_secs(2),
            "must short-circuit fast (got {elapsed:?})"
        );
        drop(tx);
    }

    #[tokio::test]
    async fn collect_heartbeat_response_short_circuits_on_shell_prompt_request() {
        // Symmetric guard for ShellConfirmation. Same hang shape as the
        // user-input case before from_proto stopped silently dropping it.
        let (tx, rx) = mpsc::channel::<AgentEvent>(8);
        tx.send(AgentEvent::SystemNotice {
            category: PROMPT_REQUEST_SHELL.into(),
            text: "rm -rf /tmp/foo".into(),
        })
        .await
        .unwrap();
        let cancel = CancellationToken::new();
        let outcome = collect_heartbeat_response(rx, cancel.clone(), Duration::from_secs(30)).await;
        match outcome {
            CollectOutcome::InteractivePromptRequested(detail) => {
                assert!(detail.contains(PROMPT_REQUEST_SHELL));
                assert!(detail.contains("rm -rf"));
            }
            other => panic!("expected InteractivePromptRequested, got {other:?}"),
        }
        assert!(cancel.is_cancelled());
        drop(tx);
    }

    #[tokio::test]
    async fn collect_heartbeat_response_ignores_non_prompt_system_notices() {
        // Guards against the short-circuit over-firing on any SystemNotice.
        // Posture transitions / budget alerts arrive as system_notice events
        // too — they must NOT abort the heartbeat turn.
        let (tx, rx) = mpsc::channel::<AgentEvent>(8);
        tx.send(AgentEvent::SystemNotice {
            category: "posture".into(),
            text: "boosted".into(),
        })
        .await
        .unwrap();
        tx.send(AgentEvent::TextDelta("hello".into()))
            .await
            .unwrap();
        drop(tx);
        let cancel = CancellationToken::new();
        let outcome = collect_heartbeat_response(rx, cancel.clone(), Duration::from_secs(5)).await;
        assert_eq!(outcome, CollectOutcome::Completed("hello".into()));
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn explicit_trigger_quiet_hours_only_applies_to_scheduled_non_bypass_runs() {
        let mut config = Config::default();
        config.heartbeat.quiet_hours_start = Some("00:00".to_string());
        config.heartbeat.quiet_hours_end = Some("23:59".to_string());

        assert!(should_skip_explicit_trigger(
            HeartbeatTrigger {
                source: HeartbeatSource::Scheduled,
                bypass_quiet_hours: false,
            },
            &config,
        ));
        assert!(!should_skip_explicit_trigger(
            HeartbeatTrigger {
                source: HeartbeatSource::Scheduled,
                bypass_quiet_hours: true,
            },
            &config,
        ));
        assert!(!should_skip_explicit_trigger(
            HeartbeatTrigger {
                source: HeartbeatSource::SessionStart,
                bypass_quiet_hours: false,
            },
            &config,
        ));
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn recipients_no_override_takes_first_approved() {
        // No override → exactly the first approved sender (not all of them),
        // so a heartbeat doesn't fan out to everyone by default.
        let approved = ids(&["alice", "bob", "carol"]);
        assert_eq!(
            resolve_recipients_for_channel(None, approved),
            ids(&["alice"])
        );
    }

    #[test]
    fn recipients_empty_override_takes_first_approved() {
        // An empty override is treated like no override.
        let empty: Vec<String> = Vec::new();
        let approved = ids(&["alice", "bob"]);
        assert_eq!(
            resolve_recipients_for_channel(Some(&empty), approved),
            ids(&["alice"])
        );
    }

    #[test]
    fn recipients_wildcard_override_takes_all_approved() {
        let over = ids(&["*"]);
        let approved = ids(&["alice", "bob", "carol"]);
        assert_eq!(
            resolve_recipients_for_channel(Some(&over), approved),
            ids(&["alice", "bob", "carol"])
        );
    }

    #[test]
    fn recipients_explicit_override_intersects_with_approved() {
        // Override names a subset; unapproved ids in the override are dropped
        // (can't message someone who never approved), and order follows the
        // override list.
        let over = ids(&["carol", "mallory", "alice"]);
        let approved = ids(&["alice", "bob", "carol"]);
        assert_eq!(
            resolve_recipients_for_channel(Some(&over), approved),
            ids(&["carol", "alice"])
        );
    }

    #[test]
    fn recipients_no_approved_senders_is_empty_even_with_wildcard() {
        // A wildcard override cannot conjure recipients out of an empty
        // approved list.
        let over = ids(&["*"]);
        assert!(resolve_recipients_for_channel(Some(&over), Vec::new()).is_empty());
    }
}
