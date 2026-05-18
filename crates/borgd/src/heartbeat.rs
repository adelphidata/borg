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
use gateway::{InboundTurn, SessionExecutor};
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
    let _ = config; // signature preserved for callers
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
            // Deliver to configured channels (matches legacy
            // `daemon_heartbeat_turn` in service/mod.rs).
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
/// Mirrors the legacy `crates/cli/src/service/mod.rs::execute_heartbeat_turn`
/// but never constructs an `Agent` directly — everything routes through the
/// SessionRegistry so hooks (vitals, evolution, …) fire the same way they do
/// for user-driven turns. The dedup window and zero-info filter are identical.
pub async fn execute_heartbeat_turn(
    executor: &dyn SessionExecutor,
    config: &Config,
    source: HeartbeatSource,
) -> HeartbeatResult {
    static LAST_HASH: AtomicU64 = AtomicU64::new(0);
    static LAST_HASH_TIME: AtomicU64 = AtomicU64::new(0);

    let started = Instant::now();

    let mut config = config.clone();
    config.llm.cache.ttl = config.llm.cache.ttl.resolve(true);

    let user_msg = match source {
        HeartbeatSource::Scheduled => build_scheduled_heartbeat_prompt(&config).await,
        HeartbeatSource::SessionStart => build_session_start_prompt(&config).await,
    };

    let session_key = format!("heartbeat:{}", source.as_wire());
    let turn = InboundTurn {
        session_key,
        text: user_msg,
        images: Vec::new(),
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
    }
}

pub(crate) async fn build_scheduled_heartbeat_prompt(config: &Config) -> String {
    let mut user_msg = "*heartbeat tick*".to_string();
    if let Some(cl) = cortex::memory::load_heartbeat_checklist() {
        user_msg.push_str("\n\n# Heartbeat Checklist\n");
        user_msg.push_str(&cl);
    }
    let nudge_db = open_heartbeat_db_for_nudges().await;
    append_proactive_nudges(
        &mut user_msg,
        crate::heartbeat_augmenters::collect(config, nudge_db.as_ref()).await,
    );
    user_msg
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
    if approved.is_empty() {
        return Ok(Vec::new());
    }
    let override_list = config.heartbeat.recipients.get(channel_name);
    let result = match override_list {
        Some(ids) if ids.iter().any(|s| s == "*") => {
            approved.into_iter().map(|s| s.sender_id).collect()
        }
        Some(ids) if !ids.is_empty() => {
            let approved_set: std::collections::HashSet<String> =
                approved.into_iter().map(|s| s.sender_id).collect();
            ids.iter()
                .filter(|id| approved_set.contains(id.as_str()))
                .cloned()
                .collect()
        }
        _ => approved.into_iter().take(1).map(|s| s.sender_id).collect(),
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
