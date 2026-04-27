//! Heartbeat scheduler glue inside the daemon.
//!
//! Owns the [`HeartbeatScheduler`] (from `heartbeat`), the agent turn
//! that runs on each fire, and the broadcast channel feeding
//! `Admin.SubscribeHeartbeatEvents` subscribers.
//!
//! Note: the in-process `crates/cli/src/service/mod.rs::execute_heartbeat_turn`
//! currently still exists for the legacy `borg daemon` CLI path. This module
//! is the daemon-native version that runs against `cortex::Agent` directly
//! (Stage E will route through `DaemonSessionExecutor` once the heartbeat
//! prompt builder lands inside the daemon's session machinery).

use crate::session::DaemonSessionExecutor;
use anyhow::Result;
use cortex::agent::AgentEvent;
use cortex::config::Config;
use gateway::{InboundTurn, SessionExecutor};
use heartbeat::scheduler::{
    HeartbeatEvent as SchedulerEvent, HeartbeatResult, HeartbeatScheduler, SkipReason,
};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// Why a heartbeat turn is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatSource {
    /// Scheduler fire — interval, cron, or `/poke`.
    Scheduled,
    /// TUI opened post-onboarding — proactive greeting.
    SessionStart,
}

impl HeartbeatSource {
    /// Wire-format string used by the proto event.
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::SessionStart => "session_start",
        }
    }
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

/// Spawn the heartbeat scheduler + an agent-turn runner. Returns the bus
/// that `Admin.SubscribeHeartbeatEvents` reads from and the `poke_tx` channel
/// the Admin.Poke handler signals.
///
/// `shutdown` cancels both spawned tasks. `executor` is used to run each
/// heartbeat turn through the daemon's session machinery (so heartbeats share
/// the per-session history + hooks like any other turn).
pub fn spawn(
    config: Config,
    executor: Arc<DaemonSessionExecutor>,
    shutdown: CancellationToken,
) -> (HeartbeatBus, mpsc::Sender<()>) {
    let (hb_tx, mut hb_rx) = mpsc::channel::<SchedulerEvent>(32);
    let (poke_tx, poke_rx) = mpsc::channel::<()>(8);

    let tz = config.user_timezone();
    let scheduler = HeartbeatScheduler::new(config.heartbeat.clone(), tz, poke_rx);
    let scheduler_cancel = shutdown.clone();
    tokio::spawn(async move {
        scheduler.run(hb_tx, scheduler_cancel).await;
    });

    let bus = HeartbeatBus::default();
    let bus_for_runner = bus.clone();
    let runner_cancel = shutdown;
    let runner_config = config;
    let runner_executor = executor;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = runner_cancel.cancelled() => return,
                evt = hb_rx.recv() => {
                    let Some(evt) = evt else { return };
                    if let SchedulerEvent::Fire = evt {
                        // Serialize fires: dedup state (LAST_HASH/LAST_HASH_TIME)
                        // is read-modify-write on two independent atomics, so
                        // overlapping fires would race. Heartbeats are rare —
                        // running sequentially is fine.
                        run_and_broadcast(
                            &runner_executor,
                            &runner_config,
                            HeartbeatSource::Scheduled,
                            &bus_for_runner,
                        ).await;
                    }
                }
            }
        }
    });

    (bus, poke_tx)
}

/// Run a heartbeat turn and broadcast the result on `bus`. Fire-and-forget.
pub async fn run_and_broadcast(
    executor: &DaemonSessionExecutor,
    config: &Config,
    source: HeartbeatSource,
    bus: &HeartbeatBus,
) {
    let started = chrono::Utc::now().timestamp();
    let started_at = Instant::now();
    let result = execute_heartbeat_turn(executor, config, source).await;
    let finished = chrono::Utc::now().timestamp();
    let duration_ms = started_at.elapsed().as_secs_f64() * 1000.0;

    let (status, message) = match result {
        HeartbeatResult::Ran { message, .. } => {
            // Deliver to configured channels (matches legacy
            // `daemon_heartbeat_turn` in service/mod.rs).
            deliver_to_channels(config, &message).await;
            ("ran", message)
        }
        HeartbeatResult::Skipped { reason } => ("skipped", reason.to_string()),
        HeartbeatResult::Failed { error } => ("failed", error),
    };

    bus.publish(DaemonHeartbeatEvent {
        source,
        started_at_unix: started,
        finished_at_unix: finished,
        status,
        message,
        duration_ms,
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
    executor: &DaemonSessionExecutor,
    config: &Config,
    source: HeartbeatSource,
) -> HeartbeatResult {
    static LAST_HASH: AtomicU64 = AtomicU64::new(0);
    static LAST_HASH_TIME: AtomicU64 = AtomicU64::new(0);

    let started = Instant::now();

    let mut config = config.clone();
    config.llm.cache.ttl = config.llm.cache.ttl.resolve(true);

    let user_msg = match source {
        HeartbeatSource::Scheduled => build_scheduled_heartbeat_prompt(&config),
        HeartbeatSource::SessionStart => build_session_start_prompt(&config),
    };

    let session_key = format!("heartbeat:{}", source.as_wire());
    let turn = InboundTurn {
        session_key,
        text: user_msg,
        images: Vec::new(),
    };
    let (mut event_rx, _cancel) = match executor.start_turn(turn).await {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!("Heartbeat: executor.start_turn failed: {e}");
            return HeartbeatResult::Failed {
                error: format!("agent error: {e}"),
            };
        }
    };

    let mut response = String::new();
    while let Some(event) = event_rx.recv().await {
        if let AgentEvent::TextDelta(delta) = event {
            response.push_str(&delta);
        }
    }

    let duration_ms = started.elapsed().as_millis() as u64;
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

    if let Ok(adb) = cortex::db::Database::open() {
        if let Err(e) = adb.set_setting("heartbeat.last_fired_at", &now_secs.to_string()) {
            tracing::warn!("Heartbeat: failed to persist last_fired_at: {e}");
        }
    }

    HeartbeatResult::Ran {
        message: trimmed,
        duration_ms,
    }
}

/// Daemon variant of `is_zero_info_heartbeat` (kept in sync with cli's copy).
fn is_zero_info_heartbeat(response: &str) -> bool {
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

fn build_scheduled_heartbeat_prompt(config: &Config) -> String {
    let mut user_msg = "*heartbeat tick*".to_string();
    if let Some(cl) = cortex::memory::load_heartbeat_checklist() {
        user_msg.push_str("\n\n# Heartbeat Checklist\n");
        user_msg.push_str(&cl);
    }
    // Proactive nudges: append LLM directives for heartbeat-only conditions
    // (e.g. no messaging channels configured). Rate-limited via the `meta`
    // table so each nudge fires at most once per its declared cooldown.
    // See `crates/borgd/src/heartbeat_augmenters.rs` for how to add one.
    let augmenter_db = cortex::db::Database::open().ok();
    let nudges = crate::heartbeat_augmenters::collect(config, augmenter_db.as_ref());
    if !nudges.is_empty() {
        user_msg.push_str("\n\n<proactive_nudges>\n");
        for n in &nudges {
            user_msg.push_str("- ");
            user_msg.push_str(n);
            user_msg.push('\n');
        }
        user_msg.push_str("</proactive_nudges>");
        tracing::info!("Heartbeat: injected {} proactive nudge(s)", nudges.len());
    }
    user_msg
}

fn build_session_start_prompt(config: &Config) -> String {
    use chrono::{TimeZone, Timelike, Utc};

    let tz = config.user_timezone();
    let now_utc = Utc::now();
    let now_local = tz.from_utc_datetime(&now_utc.naive_utc());

    let hour = now_local.hour();
    let weekday = now_local.format("%A").to_string();

    let (recent_activity, hours_since_last) = match cortex::db::Database::open() {
        Ok(db) => {
            let ctx = cortex::daily_summary::gather_daily_context(&db).unwrap_or_default();
            let gap = db
                .sessions_since(0)
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
    msg
}

/// Send `text` to every channel listed in `config.heartbeat.channels`.
async fn deliver_to_channels(config: &Config, text: &str) {
    for channel_name in &config.heartbeat.channels {
        if let Err(e) = deliver_to_channel(config, channel_name, text).await {
            tracing::warn!("Heartbeat delivery to {channel_name} failed: {e}");
        }
    }
}

async fn deliver_to_channel(config: &Config, channel_name: &str, text: &str) -> Result<()> {
    let recipients = resolve_recipients(config, channel_name)?;
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

fn resolve_recipients(config: &Config, channel_name: &str) -> Result<Vec<String>> {
    let db = cortex::db::Database::open()?;
    let approved = db.list_approved_senders(Some(channel_name))?;
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
}
