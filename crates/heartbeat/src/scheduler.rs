use chrono::NaiveTime;
use chrono_tz::Tz;
use cron::Schedule;
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, warn};

use cortex::config::{Config, HeartbeatConfig};
use cortex::tasks::parse_interval;
use cortex::util::panic_utils::panic_msg;

/// Hard floor enforced regardless of `heartbeat.min_interval_secs`. A
/// configured value of 0 is bumped to 1 second so a misconfiguration
/// can't busy-loop the scheduler.
const HARD_FLOOR_SECS: u64 = 1;

/// The scheduler's liveness counter is updated on every loop iteration.
/// To make stall detection possible even when the scheduler is
/// legitimately sleeping for a long time (e.g. waiting for a cron fire
/// hours away), we cap any single sleep at this value and force a wake
/// to refresh the counter.
///
/// 60s is small enough that a watchdog with a 5min stale threshold has
/// 10× headroom (no false positives), and large enough that the wake
/// cost is negligible (one timer firing per minute).
pub const LIVENESS_TICK_MAX: std::time::Duration = std::time::Duration::from_secs(60);

/// Brief pause after a panicked iteration. Without this, a deterministic
/// panic source (e.g. malformed config that fails the same way every
/// loop) would burn CPU spinning. 500ms gives enough time for the
/// supervisor's degraded-state cap to engage if the panic is persistent.
const POST_PANIC_BACKOFF: std::time::Duration = std::time::Duration::from_millis(500);

/// Per-iteration control flow for the panic-isolated run loops.
#[derive(Debug, PartialEq, Eq)]
enum LoopControl {
    Continue,
    Stop,
}

/// Per-iteration control flow specific to the cron run loop. Adds a
/// `FallbackToInterval` arm for the case where the cron schedule has
/// produced no upcoming fire times and we need to hand control to the
/// interval-mode loop.
#[derive(Debug, PartialEq, Eq)]
enum CronStep {
    Continue,
    Stop,
    FallbackToInterval,
}

/// Outcome of a heartbeat agent turn.
#[derive(Debug, Clone)]
pub enum HeartbeatResult {
    /// Agent produced a message to display.
    Ran {
        /// The agent's response text.
        message: String,
        /// How long the agent turn took in milliseconds.
        duration_ms: u64,
    },
    /// Heartbeat was skipped.
    Skipped {
        /// Why the heartbeat was skipped.
        reason: SkipReason,
    },
    /// Heartbeat failed.
    Failed {
        /// Error description.
        error: String,
    },
}

/// Why a heartbeat was skipped.
#[derive(Debug, Clone)]
pub enum SkipReason {
    /// Currently in quiet hours window.
    QuietHours,
    /// Agent produced no output.
    EmptyResponse,
    /// Agent response was identical to the previous heartbeat.
    DuplicateResponse,
    /// No LLM provider is configured — heartbeat has nothing to call.
    /// Onboarding-not-done, provider cleared, or all credentials revoked.
    NoLlmProvider,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QuietHours => write!(f, "quiet hours"),
            Self::EmptyResponse => write!(f, "empty response"),
            Self::DuplicateResponse => write!(f, "duplicate response"),
            Self::NoLlmProvider => write!(f, "no llm provider"),
        }
    }
}

/// Events emitted by the heartbeat scheduler.
#[derive(Debug, Clone)]
pub enum HeartbeatEvent {
    /// Timer fired — consumer should run a heartbeat agent turn.
    Fire,
    /// Scheduler started successfully — for TUI to show confirmation.
    SchedulerStarted {
        /// Scheduling mode description (e.g., "cron: */30 * * * *" or "interval: 30m").
        mode: String,
    },
    /// Result of a heartbeat turn (sent by consumer after agent turn completes).
    Result(HeartbeatResult),
}

/// Pure timer that emits `Fire` events on schedule. Does not call LLM directly —
/// the consumer (daemon or TUI) is responsible for running the agent turn.
///
/// The scheduler is structurally hardened against three failure modes:
///
/// 1. **Iteration panic.** Each loop iteration body is wrapped in
///    `catch_unwind`; a panic in cron parsing, config apply, or any
///    descendant logs and the loop continues. This stops a single bad
///    cron row, a clock-jump arithmetic edge case, or an unexpected
///    `unwrap` from killing proactive check-ins.
///
/// 2. **Stalled iteration.** The [`liveness`](Self::liveness) counter
///    is updated after every select! return. An external watchdog can
///    detect "loop alive but not iterating" (deadlock, hung await) and
///    trigger a restart via the supervisor. Sleeps inside the loop are
///    capped at [`LIVENESS_TICK_MAX`] so the counter advances even
///    during legitimately long waits (e.g. cron fire hours away).
///
/// 3. **Crashed task.** When everything else fails — panic outside any
///    iteration body, or a panic that escapes `catch_unwind` — the
///    `borgd` subsystem supervisor catches the resulting `JoinError`
///    and respawns. That's the last line of defense.
pub struct HeartbeatScheduler {
    config: HeartbeatConfig,
    timezone: Tz,
    poke_rx: Option<mpsc::Receiver<()>>,
    /// Parsed quiet hours (start, end), refreshed on every config reload.
    quiet_hours: Option<(NaiveTime, NaiveTime)>,
    /// Optional live-reload subscription. When set, the run loop wakes on
    /// `changed()` and applies the new `heartbeat.*` settings without restart.
    config_rx: Option<watch::Receiver<Config>>,
    /// Monotonic iteration counter. Incremented after every select!
    /// return and every [`LIVENESS_TICK_MAX`] forced wake. Read by the
    /// daemon-side watchdog: an unchanged value across the stale
    /// threshold proves the loop is stuck (no real-time clock dependency,
    /// works under `tokio::time::pause` for tests).
    liveness: Arc<AtomicU64>,
}

impl HeartbeatScheduler {
    /// Build a scheduler bound to the given config/timezone and listening on `poke_rx`.
    pub fn new(config: HeartbeatConfig, timezone: Tz, poke_rx: mpsc::Receiver<()>) -> Self {
        let quiet_hours = Self::parse_quiet_hours(&config);
        Self {
            config,
            timezone,
            poke_rx: Some(poke_rx),
            quiet_hours,
            config_rx: None,
            liveness: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Hand out a clone of the liveness counter. The daemon-side watchdog
    /// reads this counter on a polling cadence — an unchanged value
    /// across multiple polls (totaling its stale threshold) proves the
    /// scheduler task is wedged.
    pub fn liveness_handle(&self) -> Arc<AtomicU64> {
        self.liveness.clone()
    }

    fn touch_liveness(&self) {
        self.liveness.fetch_add(1, Ordering::Relaxed);
    }

    /// Subscribe the scheduler to live config updates. After this, changes to
    /// `heartbeat.*` settings (interval, min_interval_secs, quiet hours) take
    /// effect on the next reload tick without restarting the daemon.
    ///
    /// Live cron-expression changes and interval↔cron mode switches are NOT
    /// supported — the active mode is whatever `run` chose at startup.
    pub fn set_config_rx(&mut self, rx: watch::Receiver<Config>) {
        self.config_rx = Some(rx);
    }

    /// Replace the active heartbeat config and re-parse quiet hours. Bad
    /// quiet-hours input falls back to disabled (matching the constructor —
    /// `parse_quiet_hours` logs the warning).
    fn apply_config(&mut self, new_config: HeartbeatConfig) {
        self.config = new_config;
        self.quiet_hours = Self::parse_quiet_hours(&self.config);
    }

    fn parse_quiet_hours(config: &HeartbeatConfig) -> Option<(NaiveTime, NaiveTime)> {
        let start_str = config.quiet_hours_start.as_ref()?;
        let end_str = config.quiet_hours_end.as_ref()?;
        let start = match NaiveTime::parse_from_str(start_str, "%H:%M") {
            Ok(t) => t,
            Err(_) => {
                warn!(
                    "Invalid quiet_hours_start '{start_str}', expected HH:MM — quiet hours disabled"
                );
                return None;
            }
        };
        let end = match NaiveTime::parse_from_str(end_str, "%H:%M") {
            Ok(t) => t,
            Err(_) => {
                warn!("Invalid quiet_hours_end '{end_str}', expected HH:MM — quiet hours disabled");
                return None;
            }
        };
        Some((start, end))
    }

    /// Start the scheduler loop, emitting `Fire` events until cancellation.
    ///
    /// The interval/cron mode is decided once here at startup. Live-reload
    /// (via `set_config_rx`) refreshes `heartbeat.*` values within the active
    /// mode, but flipping `heartbeat.cron` between null and non-null still
    /// requires a daemon restart.
    #[instrument(skip_all)]
    pub async fn run(mut self, tx: mpsc::Sender<HeartbeatEvent>, cancel: CancellationToken) {
        if let Some(ref cron_expr) = self.config.cron {
            self.run_cron(cron_expr.clone(), tx, cancel).await;
        } else {
            self.run_interval(tx, cancel).await;
        }
    }

    /// Receive a poke signal, or pend forever if the channel is closed/absent.
    #[instrument(skip_all)]
    async fn recv_poke(&mut self) {
        match &mut self.poke_rx {
            Some(rx) => match rx.recv().await {
                Some(()) => {}
                None => {
                    // Sender dropped — disable poke to avoid busy-loop
                    debug!("Poke channel closed, disabling poke");
                    self.poke_rx = None;
                    std::future::pending().await
                }
            },
            None => std::future::pending().await,
        }
    }

    /// Try to send a Fire event without blocking. Returns `false` if the channel is closed
    /// (meaning the consumer is gone and the scheduler should shut down).
    fn try_fire(tx: &mpsc::Sender<HeartbeatEvent>) -> bool {
        match tx.try_send(HeartbeatEvent::Fire) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!("Heartbeat: consumer busy, skipping fire");
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                info!("Heartbeat: consumer dropped, shutting down");
                false
            }
        }
    }

    /// Compute the clamped interval from the current config, logging when the
    /// raw value is bumped to the floor.
    fn clamped_interval(&self) -> std::time::Duration {
        // 1800s = 30min default when `heartbeat.interval` is unparseable.
        let raw =
            parse_interval(&self.config.interval).unwrap_or(std::time::Duration::from_secs(1800));
        let min_secs = self.config.min_interval_secs.max(HARD_FLOOR_SECS);
        if raw.as_secs() < min_secs {
            warn!(
                "Heartbeat interval {}s is below minimum {}s, clamping",
                raw.as_secs(),
                min_secs
            );
            std::time::Duration::from_secs(min_secs)
        } else {
            raw
        }
    }

    /// Await `changed()` on the given receiver, or pend forever if absent.
    /// Returning `false` means the watch sender was dropped — caller should
    /// drop the receiver to avoid a busy-loop on a closed channel.
    ///
    /// Free function rather than a `&mut self` method so callers can hold the
    /// receiver in a local and still borrow other `self` fields concurrently
    /// inside a `tokio::select!`.
    async fn recv_config_change(rx: &mut Option<watch::Receiver<Config>>) -> bool {
        match rx {
            Some(rx) => rx.changed().await.is_ok(),
            None => std::future::pending().await,
        }
    }

    #[instrument(skip_all)]
    async fn run_interval(&mut self, tx: mpsc::Sender<HeartbeatEvent>, cancel: CancellationToken) {
        let mut config_rx = self.config_rx.take();
        let interval = self.clamped_interval();

        info!("Heartbeat scheduler started (interval: {interval:?})");
        if let Some((start, end)) = self.quiet_hours {
            info!(
                "Heartbeat quiet hours: {} - {} (timezone: {})",
                start, end, self.timezone
            );
        }

        let mode = format!("interval: {}", self.config.interval);
        if let Err(e) = tx.try_send(HeartbeatEvent::SchedulerStarted { mode }) {
            warn!("Failed to send SchedulerStarted event: {e}");
        }

        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await; // Skip immediate first tick

        // Independent ticker that wakes the loop every LIVENESS_TICK_MAX so
        // the liveness counter stays current even when the main interval is
        // longer than the watchdog's stale threshold.
        let mut liveness_ticker = tokio::time::interval(LIVENESS_TICK_MAX);
        liveness_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        liveness_ticker.tick().await; // Skip immediate first tick.

        // Touch liveness once before the first await on the main select.
        // Without this, a fresh scheduler reads 0 until the first
        // iteration completes — which can be up to LIVENESS_TICK_MAX
        // away, leaving the watchdog unable to distinguish "just
        // booted, healthy" from "wedged at startup."
        self.touch_liveness();

        loop {
            // Each iteration body is panic-isolated: a panic in
            // apply_config, try_fire, is_quiet_hours, or any descendant
            // turns into a logged warn + brief backoff + next loop, never
            // a task death. Combined with the supervisor's panic catcher
            // (which catches anything that escapes this), proactive
            // check-ins should keep firing through transient bugs.
            let outcome = AssertUnwindSafe(self.interval_iteration(
                &tx,
                &cancel,
                &mut config_rx,
                &mut ticker,
                &mut liveness_ticker,
            ))
            .catch_unwind()
            .await;
            self.touch_liveness();
            match outcome {
                Ok(LoopControl::Continue) => continue,
                Ok(LoopControl::Stop) => return,
                Err(payload) => {
                    let msg = panic_msg(&payload);
                    error!("Heartbeat: iteration panicked: {msg}");
                    tokio::time::sleep(POST_PANIC_BACKOFF).await;
                    continue;
                }
            }
        }
    }

    /// Body of one interval-mode iteration. Returns whether the loop
    /// should continue or stop. Extracted from `run_interval` so the
    /// caller can wrap it in `catch_unwind` and turn an iteration panic
    /// into a logged warn instead of a task death.
    async fn interval_iteration(
        &mut self,
        tx: &mpsc::Sender<HeartbeatEvent>,
        cancel: &CancellationToken,
        config_rx: &mut Option<watch::Receiver<Config>>,
        ticker: &mut tokio::time::Interval,
        liveness_ticker: &mut tokio::time::Interval,
    ) -> LoopControl {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                info!("Heartbeat scheduler shutting down");
                LoopControl::Stop
            }
            _ = liveness_ticker.tick() => {
                // Pure liveness tick; no other action. The caller calls
                // touch_liveness() on every return so we don't need to
                // touch it here as well.
                LoopControl::Continue
            }
            ok = Self::recv_config_change(config_rx) => {
                if !ok {
                    debug!("Heartbeat: config watch closed, disabling reload");
                    *config_rx = None;
                    return LoopControl::Continue;
                }
                let new_hb = config_rx
                    .as_ref()
                    .map(|rx| rx.borrow().heartbeat.clone());
                if let Some(new_hb) = new_hb {
                    self.apply_config(new_hb);
                    let new_interval = self.clamped_interval();
                    info!("Heartbeat: config reload applied (interval: {new_interval:?})");
                    *ticker = tokio::time::interval(new_interval);
                    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    ticker.tick().await;
                }
                LoopControl::Continue
            }
            _ = self.recv_poke() => {
                debug!("Heartbeat: poke signal received, firing immediately");
                if !Self::try_fire(tx) { LoopControl::Stop } else { LoopControl::Continue }
            }
            _ = ticker.tick() => {
                if self.is_quiet_hours() {
                    debug!("Heartbeat: in quiet hours, skipping");
                    if let Err(e) = tx.try_send(HeartbeatEvent::Result(HeartbeatResult::Skipped {
                        reason: SkipReason::QuietHours,
                    })) {
                        debug!("Failed to send quiet-hours skip event: {e}");
                    }
                    return LoopControl::Continue;
                }
                if !Self::try_fire(tx) { LoopControl::Stop } else { LoopControl::Continue }
            }
        }
    }

    #[instrument(skip_all)]
    async fn run_cron(
        &mut self,
        cron_expr: String,
        tx: mpsc::Sender<HeartbeatEvent>,
        cancel: CancellationToken,
    ) {
        let schedule = match Schedule::from_str(&cron_expr) {
            Ok(s) => s,
            Err(e) => {
                warn!("Invalid cron expression '{cron_expr}': {e}. Falling back to interval.");
                self.run_interval(tx, cancel).await;
                return;
            }
        };

        // Validate minimum interval between cron firings
        {
            let min_secs = self.config.min_interval_secs.max(HARD_FLOOR_SECS);
            let mut upcoming = schedule.upcoming(chrono::Utc);
            if let (Some(t1), Some(t2)) = (upcoming.next(), upcoming.next()) {
                let interval = (t2 - t1).to_std().unwrap_or_default();
                if interval < std::time::Duration::from_secs(min_secs) {
                    warn!(
                        "Cron expression '{cron_expr}' fires more often than minimum {min_secs}s ({interval:?}). Falling back to interval."
                    );
                    self.run_interval(tx, cancel).await;
                    return;
                }
            }
        }

        info!("Heartbeat scheduler started (cron: {cron_expr})");
        if let Some((start, end)) = self.quiet_hours {
            info!(
                "Heartbeat quiet hours: {} - {} (timezone: {})",
                start, end, self.timezone
            );
        }

        let mode = format!("cron: {cron_expr}");
        if let Err(e) = tx.try_send(HeartbeatEvent::SchedulerStarted { mode }) {
            warn!("Failed to send SchedulerStarted event: {e}");
        }

        // Move the watch receiver out of `self` so the select can borrow it
        // independently of `self.poke_rx`. Restored before any fallback to
        // `run_interval` so live-reload survives a cron-schedule exhaustion.
        let mut config_rx = self.config_rx.take();

        // Same boot-touch as run_interval: ensures the watchdog reads a
        // healthy liveness signal from the moment cron mode starts.
        self.touch_liveness();

        loop {
            let outcome =
                AssertUnwindSafe(self.cron_iteration(&schedule, &tx, &cancel, &mut config_rx))
                    .catch_unwind()
                    .await;
            self.touch_liveness();
            match outcome {
                Ok(CronStep::Continue) => continue,
                Ok(CronStep::Stop) => return,
                Ok(CronStep::FallbackToInterval) => {
                    self.config_rx = config_rx;
                    self.run_interval(tx, cancel).await;
                    return;
                }
                Err(payload) => {
                    let msg = panic_msg(&payload);
                    error!("Heartbeat: cron iteration panicked: {msg}");
                    tokio::time::sleep(POST_PANIC_BACKOFF).await;
                    continue;
                }
            }
        }
    }

    /// Body of one cron-mode iteration. Mirrors `interval_iteration` —
    /// each pass through the loop is panic-isolated and returns a
    /// control-flow signal rather than directly returning from the
    /// caller. `FallbackToInterval` is the cron-only escape hatch for a
    /// schedule that produced no upcoming fire times.
    async fn cron_iteration(
        &mut self,
        schedule: &Schedule,
        tx: &mpsc::Sender<HeartbeatEvent>,
        cancel: &CancellationToken,
        config_rx: &mut Option<watch::Receiver<Config>>,
    ) -> CronStep {
        let now = chrono::Local::now();
        let next = match schedule.upcoming(chrono::Local).next() {
            Some(t) => t,
            None => {
                tracing::error!("Cron schedule exhausted, falling back to interval mode");
                return CronStep::FallbackToInterval;
            }
        };

        // Long cron waits are capped at LIVENESS_TICK_MAX so the watchdog
        // sees a fresh liveness timestamp at least once per minute even
        // when the next fire is hours away. When the cap clips the wait,
        // we'll come back through the loop, log nothing, and reschedule.
        let raw_wait = (next - now)
            .to_std()
            .unwrap_or(std::time::Duration::from_secs(60));
        let wait = raw_wait.min(LIVENESS_TICK_MAX);
        let did_clip = wait < raw_wait;

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                info!("Heartbeat scheduler shutting down");
                CronStep::Stop
            }
            ok = Self::recv_config_change(config_rx) => {
                if !ok {
                    debug!("Heartbeat: config watch closed, disabling reload");
                    *config_rx = None;
                    return CronStep::Continue;
                }
                // TODO: live cron-expression reload + interval↔cron mode
                // switch. Today we only refresh quiet hours and config
                // values used at fire-time; changing `heartbeat.cron`
                // requires a daemon restart.
                let new_hb = config_rx
                    .as_ref()
                    .map(|rx| rx.borrow().heartbeat.clone());
                if let Some(new_hb) = new_hb {
                    self.apply_config(new_hb);
                    info!("Heartbeat: config reload applied (cron mode)");
                }
                CronStep::Continue
            }
            _ = self.recv_poke() => {
                debug!("Heartbeat: poke signal received, firing immediately");
                if !Self::try_fire(tx) { CronStep::Stop } else { CronStep::Continue }
            }
            _ = tokio::time::sleep(wait) => {
                if did_clip {
                    // Liveness cap fired before the real fire time — go
                    // back through the loop without firing or logging.
                    return CronStep::Continue;
                }
                if self.is_quiet_hours() {
                    debug!("Heartbeat: in quiet hours, skipping");
                    if let Err(e) = tx.try_send(HeartbeatEvent::Result(HeartbeatResult::Skipped {
                        reason: SkipReason::QuietHours,
                    })) {
                        debug!("Failed to send quiet-hours skip event: {e}");
                    }
                    return CronStep::Continue;
                }
                if !Self::try_fire(tx) { CronStep::Stop } else { CronStep::Continue }
            }
        }
    }

    fn is_quiet_hours(&self) -> bool {
        let Some((start, end)) = self.quiet_hours else {
            return false;
        };

        let now = chrono::Utc::now().with_timezone(&self.timezone).time();
        quiet_hours_contains(start, end, now)
    }
}

/// Pure check: is `now` within the quiet-hours window `[start, end)`?
/// Handles wrap-around (start > end) by treating the window as crossing midnight.
fn quiet_hours_contains(start: NaiveTime, end: NaiveTime, now: NaiveTime) -> bool {
    if start <= end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_scheduler(config: HeartbeatConfig, tz: Tz) -> HeartbeatScheduler {
        let (_tx, rx) = mpsc::channel(1);
        let quiet_hours = HeartbeatScheduler::parse_quiet_hours(&config);
        HeartbeatScheduler {
            config,
            timezone: tz,
            poke_rx: Some(rx),
            quiet_hours,
            config_rx: None,
            liveness: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Drain the initial SchedulerStarted event from the channel.
    async fn drain_started(rx: &mut mpsc::Receiver<HeartbeatEvent>) {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for SchedulerStarted")
            .expect("channel closed");
        assert!(
            matches!(event, HeartbeatEvent::SchedulerStarted { .. }),
            "expected SchedulerStarted, got: {event:?}"
        );
    }

    #[test]
    fn quiet_hours_no_config() {
        let config = HeartbeatConfig {
            interval: "30m".to_string(),
            quiet_hours_start: None,
            quiet_hours_end: None,
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
            ..Default::default()
        };
        let sched = test_scheduler(config, chrono_tz::UTC);
        assert!(!sched.is_quiet_hours());
    }

    #[test]
    fn quiet_hours_invalid_format() {
        let config = HeartbeatConfig {
            interval: "30m".to_string(),
            quiet_hours_start: Some("not-a-time".to_string()),
            quiet_hours_end: Some("also-bad".to_string()),
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
            ..Default::default()
        };
        let sched = test_scheduler(config, chrono_tz::UTC);
        assert!(!sched.is_quiet_hours());
    }

    #[test]
    fn quiet_hours_contains_boundary_table() {
        // Window 23:55–06:00 (wraps midnight). Half-open [start, end): start
        // is INSIDE, end is OUTSIDE. Boundary regression target — flipping
        // < to <= or >= to > in `quiet_hours_contains` must fail this.
        let start = NaiveTime::from_hms_opt(23, 55, 0).unwrap();
        let end = NaiveTime::from_hms_opt(6, 0, 0).unwrap();
        let cases: &[(NaiveTime, bool)] = &[
            (NaiveTime::from_hms_opt(23, 54, 59).unwrap(), false),
            (NaiveTime::from_hms_opt(23, 55, 0).unwrap(), true),
            (NaiveTime::from_hms_opt(0, 0, 0).unwrap(), true),
            (NaiveTime::from_hms_opt(5, 59, 59).unwrap(), true),
            (NaiveTime::from_hms_opt(6, 0, 0).unwrap(), false),
            (NaiveTime::from_hms_opt(12, 0, 0).unwrap(), false),
        ];
        for (now, expected) in cases {
            assert_eq!(
                quiet_hours_contains(start, end, *now),
                *expected,
                "now={now}"
            );
        }

        // Non-wrapping window 09:00–17:00 — same half-open semantics.
        let start = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let end = NaiveTime::from_hms_opt(17, 0, 0).unwrap();
        let cases: &[(NaiveTime, bool)] = &[
            (NaiveTime::from_hms_opt(8, 59, 59).unwrap(), false),
            (NaiveTime::from_hms_opt(9, 0, 0).unwrap(), true),
            (NaiveTime::from_hms_opt(16, 59, 59).unwrap(), true),
            (NaiveTime::from_hms_opt(17, 0, 0).unwrap(), false),
        ];
        for (now, expected) in cases {
            assert_eq!(
                quiet_hours_contains(start, end, *now),
                *expected,
                "now={now}"
            );
        }
    }

    #[tokio::test]
    async fn scheduler_cancels_cleanly() {
        let config = HeartbeatConfig {
            interval: "30m".to_string(),
            quiet_hours_start: None,
            quiet_hours_end: None,
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
            ..Default::default()
        };

        let (_poke_tx, poke_rx) = mpsc::channel(1);
        let (fire_tx, _fire_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();

        let scheduler = HeartbeatScheduler::new(config, chrono_tz::UTC, poke_rx);
        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

        // Cancel immediately
        cancel.cancel();

        // Should complete without hanging
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("scheduler did not shut down in time")
            .expect("scheduler task panicked");
    }

    #[tokio::test]
    async fn poke_signal_triggers_fire() {
        let config = HeartbeatConfig {
            interval: "30m".to_string(),
            quiet_hours_start: None,
            quiet_hours_end: None,
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
            ..Default::default()
        };

        let (poke_tx, poke_rx) = mpsc::channel(8);
        let (fire_tx, mut fire_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        let scheduler = HeartbeatScheduler::new(config, chrono_tz::UTC, poke_rx);
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            scheduler.run(fire_tx, cancel_clone).await;
        });

        // Drain SchedulerStarted
        drain_started(&mut fire_rx).await;

        // Send poke signal
        poke_tx.send(()).await.unwrap();

        // Should receive a Fire event
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), fire_rx.recv())
            .await
            .expect("timed out waiting for Fire event")
            .expect("channel closed");

        assert!(matches!(event, HeartbeatEvent::Fire));

        cancel.cancel();
    }

    #[tokio::test]
    async fn scheduler_skips_when_channel_full() {
        let config = HeartbeatConfig {
            interval: "30m".to_string(),
            quiet_hours_start: None,
            quiet_hours_end: None,
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
            ..Default::default()
        };

        let (poke_tx, poke_rx) = mpsc::channel(8);
        let (fire_tx, _fire_rx) = mpsc::channel(1); // capacity 1, never drained
        let cancel = CancellationToken::new();

        let scheduler = HeartbeatScheduler::new(config, chrono_tz::UTC, poke_rx);
        let cancel_clone = cancel.clone();
        tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

        // First poke fills the channel, second should be skipped (not block)
        poke_tx.send(()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        poke_tx.send(()).await.unwrap();

        // If try_send works, scheduler is still alive and responds to cancel
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel.cancel();
    }

    #[tokio::test]
    async fn scheduler_exits_when_channel_closed() {
        let config = HeartbeatConfig {
            interval: "1s".to_string(),
            quiet_hours_start: None,
            quiet_hours_end: None,
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
            ..Default::default()
        };

        let (poke_tx, poke_rx) = mpsc::channel(1);
        let (fire_tx, fire_rx) = mpsc::channel(1);
        drop(fire_rx); // Close the receiving end

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let scheduler = HeartbeatScheduler::new(config, chrono_tz::UTC, poke_rx);
        let handle = tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

        // Send a poke to trigger a try_send on the closed channel
        let _ = poke_tx.send(()).await;

        // Scheduler should exit on its own when it detects closed channel
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("scheduler should exit when channel closed")
            .expect("scheduler task panicked");
    }

    #[tokio::test(start_paused = true)]
    async fn interval_below_default_min_clamped_to_60s() {
        // With the default min_interval_secs (60), a configured 30s interval
        // must be clamped up. Advancing 30s of virtual time produces no Fire;
        // advancing past 60s does. Regression guard: if a future change drops
        // the clamp this test fires immediately at 30s and fails.
        let config = HeartbeatConfig {
            interval: "30s".to_string(),
            quiet_hours_start: None,
            quiet_hours_end: None,
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
            ..Default::default()
        };
        assert_eq!(config.min_interval_secs, 60, "default must be 60s");

        let (_poke_tx, poke_rx) = mpsc::channel(1);
        let (fire_tx, mut fire_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        let scheduler = HeartbeatScheduler::new(config, chrono_tz::UTC, poke_rx);
        let cancel_clone = cancel.clone();
        tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

        tokio::task::yield_now().await;
        drain_started(&mut fire_rx).await;

        // 30s in: still below the clamped 60s, no Fire yet.
        tokio::time::advance(std::time::Duration::from_secs(30)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(
            matches!(fire_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "30s should not produce a Fire when clamp is 60s"
        );

        // Past 60s: Fire arrives.
        tokio::time::advance(std::time::Duration::from_secs(32)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        match fire_rx.try_recv() {
            Ok(HeartbeatEvent::Fire) => {}
            other => panic!("expected Fire after 62s, got: {other:?}"),
        }
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn subminute_interval_fires_when_min_lowered() {
        // Operator/eval-suite knob: dropping min_interval_secs to 1 lets a
        // 3s interval actually fire on its 3s cadence rather than getting
        // clamped to 60s. Without this, smorgasborg's scheduler smoke test
        // would burn ≥60s of wall time per assertion.
        let config = HeartbeatConfig {
            interval: "3s".to_string(),
            min_interval_secs: 1,
            quiet_hours_start: None,
            quiet_hours_end: None,
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
            ..Default::default()
        };

        let (_poke_tx, poke_rx) = mpsc::channel(1);
        let (fire_tx, mut fire_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        let scheduler = HeartbeatScheduler::new(config, chrono_tz::UTC, poke_rx);
        let cancel_clone = cancel.clone();
        tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

        tokio::task::yield_now().await;
        drain_started(&mut fire_rx).await;

        tokio::time::advance(std::time::Duration::from_secs(4)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        match fire_rx.try_recv() {
            Ok(HeartbeatEvent::Fire) => {}
            other => panic!("expected Fire after 4s with min=1, got: {other:?}"),
        }
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn min_interval_zero_is_floored_to_one_second() {
        // Misconfiguration guard: a configured 0 must not busy-loop the
        // scheduler. The HARD_FLOOR_SECS = 1 bump means a 1s interval still
        // fires once per virtual second (not many times per virtual instant).
        let config = HeartbeatConfig {
            interval: "1s".to_string(),
            min_interval_secs: 0,
            quiet_hours_start: None,
            quiet_hours_end: None,
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
            ..Default::default()
        };

        let (_poke_tx, poke_rx) = mpsc::channel(1);
        let (fire_tx, mut fire_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        let scheduler = HeartbeatScheduler::new(config, chrono_tz::UTC, poke_rx);
        let cancel_clone = cancel.clone();
        tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

        tokio::task::yield_now().await;
        drain_started(&mut fire_rx).await;

        // Advance exactly one floor-tick. Expect at least one Fire and a
        // bounded number — busy-looping would dump dozens.
        tokio::time::advance(std::time::Duration::from_millis(1100)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        let mut fires = 0;
        while let Ok(HeartbeatEvent::Fire) = fire_rx.try_recv() {
            fires += 1;
        }
        assert!(fires >= 1, "expected at least one Fire after 1.1s");
        assert!(
            fires <= 4,
            "1s floor must not busy-loop; got {fires} fires in 1.1s"
        );
        cancel.cancel();
    }

    #[tokio::test]
    async fn cron_subminute_falls_back_unless_min_lowered() {
        // Two sub-cases share scaffolding to avoid near-duplicate tests.
        // Default min (60): "* * * * * *" (every second) must fall back to
        // interval mode. Lowered min (1): same expression runs as cron.
        async fn run_case(min_interval_secs: u64) -> String {
            let config = HeartbeatConfig {
                interval: "30m".to_string(),
                min_interval_secs,
                quiet_hours_start: None,
                quiet_hours_end: None,
                cron: Some("* * * * * *".to_string()),
                channels: Vec::new(),
                recipients: std::collections::BTreeMap::new(),
                ..Default::default()
            };
            let (_poke_tx, poke_rx) = mpsc::channel(1);
            let (fire_tx, mut fire_rx) = mpsc::channel(32);
            let cancel = CancellationToken::new();
            let scheduler = HeartbeatScheduler::new(config, chrono_tz::UTC, poke_rx);
            let cancel_clone = cancel.clone();
            tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), fire_rx.recv())
                .await
                .expect("timed out waiting for SchedulerStarted")
                .expect("channel closed");
            cancel.cancel();
            match event {
                HeartbeatEvent::SchedulerStarted { mode } => mode,
                other => panic!("expected SchedulerStarted, got: {other:?}"),
            }
        }

        let default_mode = run_case(60).await;
        assert!(
            default_mode.starts_with("interval"),
            "default min=60 must fall back to interval, got: {default_mode}"
        );

        let lowered_mode = run_case(1).await;
        assert!(
            lowered_mode.starts_with("cron"),
            "min=1 must keep cron mode, got: {lowered_mode}"
        );
    }

    #[tokio::test]
    async fn interval_empty_string_uses_default() {
        let config = HeartbeatConfig {
            interval: "".to_string(), // empty — falls back to 1800s
            quiet_hours_start: None,
            quiet_hours_end: None,
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
            ..Default::default()
        };

        let (poke_tx, poke_rx) = mpsc::channel(8);
        let (fire_tx, mut fire_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        let scheduler = HeartbeatScheduler::new(config, chrono_tz::UTC, poke_rx);
        let cancel_clone = cancel.clone();
        tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

        // Drain SchedulerStarted
        drain_started(&mut fire_rx).await;

        // Poke should still work
        poke_tx.send(()).await.unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), fire_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert!(matches!(event, HeartbeatEvent::Fire));
        cancel.cancel();
    }

    #[tokio::test]
    async fn cron_invalid_expression_falls_back() {
        let config = HeartbeatConfig {
            interval: "30m".to_string(),
            quiet_hours_start: None,
            quiet_hours_end: None,
            cron: Some("not-a-cron-expression".to_string()),
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
            ..Default::default()
        };

        let (poke_tx, poke_rx) = mpsc::channel(8);
        let (fire_tx, mut fire_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        let scheduler = HeartbeatScheduler::new(config, chrono_tz::UTC, poke_rx);
        let cancel_clone = cancel.clone();
        tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

        // Drain SchedulerStarted (from the interval fallback)
        drain_started(&mut fire_rx).await;

        // Should fall back to interval mode and respond to poke
        poke_tx.send(()).await.unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), fire_rx.recv())
            .await
            .expect("timed out — invalid cron should fall back to interval")
            .expect("channel closed");
        assert!(matches!(event, HeartbeatEvent::Fire));
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn interval_fires_on_schedule() {
        let config = HeartbeatConfig {
            interval: "60s".to_string(),
            quiet_hours_start: None,
            quiet_hours_end: None,
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
            ..Default::default()
        };

        let (_poke_tx, poke_rx) = mpsc::channel(1);
        let (fire_tx, mut fire_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        let scheduler = HeartbeatScheduler::new(config, chrono_tz::UTC, poke_rx);
        let cancel_clone = cancel.clone();
        tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

        // Let the spawned task start and consume the first immediate tick
        tokio::task::yield_now().await;

        // Drain SchedulerStarted
        match fire_rx.try_recv() {
            Ok(HeartbeatEvent::SchedulerStarted { .. }) => {}
            other => panic!("expected SchedulerStarted, got: {other:?}"),
        }

        // Advance past one interval (60s) + buffer.
        // With start_paused, time only advances when we explicitly advance or sleep.
        tokio::time::advance(std::time::Duration::from_secs(62)).await;

        // Yield to let the spawned task process the tick
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        // Should receive a Fire event from the interval (no poke needed)
        match fire_rx.try_recv() {
            Ok(event) => assert!(matches!(event, HeartbeatEvent::Fire)),
            Err(_) => panic!("expected Fire event after advancing past interval"),
        }
        cancel.cancel();
    }

    #[tokio::test]
    async fn multiple_rapid_pokes_dont_block() {
        let config = HeartbeatConfig {
            interval: "30m".to_string(),
            quiet_hours_start: None,
            quiet_hours_end: None,
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
            ..Default::default()
        };

        let (poke_tx, poke_rx) = mpsc::channel(16);
        let (fire_tx, mut fire_rx) = mpsc::channel(2); // small buffer
        let cancel = CancellationToken::new();

        let scheduler = HeartbeatScheduler::new(config, chrono_tz::UTC, poke_rx);
        let cancel_clone = cancel.clone();
        tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

        // Send 5 rapid pokes — some will be skipped due to full channel
        for _ in 0..5 {
            let _ = poke_tx.send(()).await;
        }

        // Drain what we can — should get at least 1, at most 2 (channel capacity)
        let mut count = 0;
        while let Ok(Some(_)) =
            tokio::time::timeout(std::time::Duration::from_millis(500), fire_rx.recv()).await
        {
            count += 1;
            if count >= 2 {
                break;
            }
        }
        assert!(count >= 1, "should receive at least one Fire event");
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn quiet_hours_emits_skip_event() {
        // Set quiet hours to cover the full day so they always apply
        let config = HeartbeatConfig {
            interval: "60s".to_string(),
            quiet_hours_start: Some("00:00".to_string()),
            quiet_hours_end: Some("23:59".to_string()),
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
            ..Default::default()
        };

        let (_poke_tx, poke_rx) = mpsc::channel(1);
        let (fire_tx, mut fire_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        let scheduler = HeartbeatScheduler::new(config, chrono_tz::UTC, poke_rx);
        let cancel_clone = cancel.clone();
        tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

        // Yield to let scheduler start
        tokio::task::yield_now().await;

        // First event is SchedulerStarted
        let event = fire_rx.try_recv().expect("expected SchedulerStarted");
        assert!(matches!(event, HeartbeatEvent::SchedulerStarted { .. }));

        // Advance past interval to trigger a tick during quiet hours
        tokio::time::advance(std::time::Duration::from_secs(62)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        // Should receive a Skipped { QuietHours } result
        match fire_rx.try_recv() {
            Ok(HeartbeatEvent::Result(HeartbeatResult::Skipped {
                reason: SkipReason::QuietHours,
            })) => {} // expected
            Ok(other) => panic!("expected Skipped(QuietHours), got: {other:?}"),
            Err(_) => panic!("expected Skipped(QuietHours) event but channel was empty"),
        }

        cancel.cancel();
    }

    #[tokio::test]
    async fn scheduler_does_not_burst_after_sleep() {
        // Verify MissedTickBehavior::Skip prevents burst-firing after a long pause
        tokio::time::pause();

        let config = HeartbeatConfig {
            interval: "60s".to_string(),
            quiet_hours_start: None,
            quiet_hours_end: None,
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
            ..Default::default()
        };

        let (_wake_tx, wake_rx) = mpsc::channel(8);
        let (fire_tx, mut fire_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        let scheduler = HeartbeatScheduler::new(config, chrono_tz::UTC, wake_rx);
        let cancel_clone = cancel.clone();
        tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

        // Drain SchedulerStarted
        drain_started(&mut fire_rx).await;

        // Advance past 10 intervals (600s) in one jump — simulates sleep/wake
        tokio::time::advance(std::time::Duration::from_secs(600)).await;
        // Yield to let the scheduler process
        tokio::task::yield_now().await;

        // Should get at most 1 Fire event, not 10 (thanks to MissedTickBehavior::Skip)
        let mut fire_count = 0;
        while let Ok(Some(event)) =
            tokio::time::timeout(std::time::Duration::from_millis(50), fire_rx.recv()).await
        {
            if matches!(event, HeartbeatEvent::Fire) {
                fire_count += 1;
            }
        }

        assert!(
            fire_count <= 1,
            "expected at most 1 fire after sleep, got {fire_count}"
        );

        cancel.cancel();
    }

    /// Build a Config wrapping the given heartbeat section. Other Config
    /// fields default — only `heartbeat` matters for these reload tests.
    fn config_with_heartbeat(hb: HeartbeatConfig) -> Config {
        let mut cfg = Config::default();
        cfg.heartbeat = hb;
        cfg
    }

    fn hb_config(interval: &str, qh_start: Option<&str>, qh_end: Option<&str>) -> HeartbeatConfig {
        HeartbeatConfig {
            interval: interval.to_string(),
            min_interval_secs: 1,
            quiet_hours_start: qh_start.map(str::to_string),
            quiet_hours_end: qh_end.map(str::to_string),
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
            ..Default::default()
        }
    }

    /// Quiet hours go live: scheduler starts firing, then a config update
    /// turns quiet hours on for the current virtual time and the next tick
    /// is reported as Skipped(QuietHours) instead of Fire — without any
    /// scheduler reconstruction.
    #[tokio::test(start_paused = true)]
    async fn quiet_hours_live_reload_suppresses_next_tick() {
        let hb = hb_config("60s", None, None);
        let initial = config_with_heartbeat(hb.clone());
        let (cfg_tx, cfg_rx) = watch::channel(initial);

        let (_poke_tx, poke_rx) = mpsc::channel(1);
        let (fire_tx, mut fire_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        let mut scheduler = HeartbeatScheduler::new(hb, chrono_tz::UTC, poke_rx);
        scheduler.set_config_rx(cfg_rx);
        let cancel_clone = cancel.clone();
        tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

        tokio::task::yield_now().await;
        drain_started(&mut fire_rx).await;

        // First tick fires normally.
        tokio::time::advance(std::time::Duration::from_secs(62)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        match fire_rx.try_recv() {
            Ok(HeartbeatEvent::Fire) => {}
            other => panic!("expected initial Fire, got: {other:?}"),
        }

        // Push a config update that covers the full clock — next tick must skip.
        let updated = config_with_heartbeat(hb_config("60s", Some("00:00"), Some("23:59")));
        cfg_tx.send(updated).expect("send updated config");
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        tokio::time::advance(std::time::Duration::from_secs(62)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        match fire_rx.try_recv() {
            Ok(HeartbeatEvent::Result(HeartbeatResult::Skipped {
                reason: SkipReason::QuietHours,
            })) => {}
            other => {
                panic!("expected Skipped(QuietHours) after live quiet-hours update, got: {other:?}")
            }
        }
        cancel.cancel();
    }

    /// Interval shortens live: a 60s scheduler picks up a 5s interval after
    /// reload and fires within 6s of virtual time.
    #[tokio::test(start_paused = true)]
    async fn interval_live_reload_shortens_cadence() {
        let hb = hb_config("60s", None, None);
        let (cfg_tx, cfg_rx) = watch::channel(config_with_heartbeat(hb.clone()));

        let (_poke_tx, poke_rx) = mpsc::channel(1);
        let (fire_tx, mut fire_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        let mut scheduler = HeartbeatScheduler::new(hb, chrono_tz::UTC, poke_rx);
        scheduler.set_config_rx(cfg_rx);
        let cancel_clone = cancel.clone();
        tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

        tokio::task::yield_now().await;
        drain_started(&mut fire_rx).await;

        // Live-update to a 5s interval.
        let updated = config_with_heartbeat(hb_config("5s", None, None));
        cfg_tx.send(updated).expect("send updated config");
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        // 6s of virtual time — under the original 60s, over the new 5s.
        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        match fire_rx.try_recv() {
            Ok(HeartbeatEvent::Fire) => {}
            other => panic!("expected Fire after 6s with live 5s interval, got: {other:?}"),
        }
        cancel.cancel();
    }

    /// Bad quiet-hours input on reload disables quiet hours — matches the
    /// constructor's parse_quiet_hours behavior. Locks in "fall back to
    /// disabled" rather than "retain previous valid value."
    #[tokio::test(start_paused = true)]
    async fn quiet_hours_bad_input_on_reload_disables() {
        // Start with valid quiet hours covering everything → ticks would skip.
        let hb = hb_config("60s", Some("00:00"), Some("23:59"));
        let (cfg_tx, cfg_rx) = watch::channel(config_with_heartbeat(hb.clone()));

        let (_poke_tx, poke_rx) = mpsc::channel(1);
        let (fire_tx, mut fire_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        let mut scheduler = HeartbeatScheduler::new(hb, chrono_tz::UTC, poke_rx);
        scheduler.set_config_rx(cfg_rx);
        let cancel_clone = cancel.clone();
        tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

        tokio::task::yield_now().await;
        drain_started(&mut fire_rx).await;

        // Push a config with garbage quiet-hours start. Per parse_quiet_hours,
        // both bounds are required and bad input disables the window entirely.
        let updated = config_with_heartbeat(hb_config("60s", Some("garbage"), Some("23:59")));
        cfg_tx.send(updated).expect("send updated config");
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        // Next tick must Fire — quiet hours are disabled, NOT preserved from
        // the previous valid config.
        tokio::time::advance(std::time::Duration::from_secs(62)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        match fire_rx.try_recv() {
            Ok(HeartbeatEvent::Fire) => {}
            Ok(HeartbeatEvent::Result(HeartbeatResult::Skipped { reason })) => panic!(
                "expected Fire after bad-input reload (quiet hours should be disabled), got Skipped({reason:?})"
            ),
            other => panic!("expected Fire after bad-input reload, got: {other:?}"),
        }
        cancel.cancel();
    }

    /// Cron mode also picks up live quiet-hours changes.
    #[tokio::test]
    async fn cron_quiet_hours_live_reload() {
        // Use a 1s cron in a real (non-paused) tokio runtime — we need
        // chrono::Local::now to advance for the cron schedule.
        let mut hb = hb_config("30m", None, None);
        hb.cron = Some("* * * * * *".to_string());
        let (cfg_tx, cfg_rx) = watch::channel(config_with_heartbeat(hb.clone()));

        let (_poke_tx, poke_rx) = mpsc::channel(1);
        let (fire_tx, mut fire_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        let mut scheduler = HeartbeatScheduler::new(hb.clone(), chrono_tz::UTC, poke_rx);
        scheduler.set_config_rx(cfg_rx);
        let cancel_clone = cancel.clone();
        tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

        // First event is SchedulerStarted (cron mode).
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), fire_rx.recv())
            .await
            .expect("timed out waiting for SchedulerStarted")
            .expect("channel closed");
        match event {
            HeartbeatEvent::SchedulerStarted { mode } => assert!(mode.starts_with("cron")),
            other => panic!("expected SchedulerStarted(cron), got: {other:?}"),
        }

        // Send live update enabling all-day quiet hours.
        let mut updated = hb;
        updated.quiet_hours_start = Some("00:00".to_string());
        updated.quiet_hours_end = Some("23:59".to_string());
        cfg_tx
            .send(config_with_heartbeat(updated))
            .expect("send updated config");

        // Drain up to ~3s of events; we expect a Skipped(QuietHours) and no Fire.
        let mut saw_skip = false;
        let mut saw_fire = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(500), fire_rx.recv()).await
            {
                Ok(Some(HeartbeatEvent::Result(HeartbeatResult::Skipped {
                    reason: SkipReason::QuietHours,
                }))) => saw_skip = true,
                Ok(Some(HeartbeatEvent::Fire)) => saw_fire = true,
                Ok(_) | Err(_) => {}
            }
            if saw_skip {
                break;
            }
        }
        cancel.cancel();
        assert!(
            saw_skip,
            "cron scheduler did not pick up live quiet-hours update (saw_fire={saw_fire})"
        );
    }
}
