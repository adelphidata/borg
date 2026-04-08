use chrono::NaiveTime;
use chrono_tz::Tz;
use cron::Schedule;
use std::str::FromStr;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

use borg_core::config::HeartbeatConfig;
use borg_core::tasks::parse_interval;

/// Minimum allowed heartbeat interval (60 seconds) to prevent API waste.
const MIN_INTERVAL_SECS: u64 = borg_core::constants::MIN_HEARTBEAT_INTERVAL_SECS;

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
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QuietHours => write!(f, "quiet hours"),
            Self::EmptyResponse => write!(f, "empty response"),
            Self::DuplicateResponse => write!(f, "duplicate response"),
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
pub struct HeartbeatScheduler {
    config: HeartbeatConfig,
    timezone: Tz,
    poke_rx: Option<mpsc::Receiver<()>>,
    /// Parsed quiet hours (start, end) cached at construction time.
    quiet_hours: Option<(NaiveTime, NaiveTime)>,
}

impl HeartbeatScheduler {
    /// Create a new scheduler with the given config, timezone, and poke signal receiver.
    pub fn new(config: HeartbeatConfig, timezone: Tz, poke_rx: mpsc::Receiver<()>) -> Self {
        let quiet_hours = Self::parse_quiet_hours(&config);
        Self {
            config,
            timezone,
            poke_rx: Some(poke_rx),
            quiet_hours,
        }
    }

    fn parse_quiet_hours(config: &HeartbeatConfig) -> Option<(NaiveTime, NaiveTime)> {
        let start_str = config.quiet_hours_start.as_ref()?;
        let end_str = config.quiet_hours_end.as_ref()?;
        let start = match NaiveTime::parse_from_str(start_str, "%H:%M") {
            Ok(t) => t,
            Err(_) => {
                warn!("Invalid quiet_hours_start '{start_str}', expected HH:MM — quiet hours disabled");
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

    #[instrument(skip_all)]
    async fn run_interval(&mut self, tx: mpsc::Sender<HeartbeatEvent>, cancel: CancellationToken) {
        let mut interval =
            parse_interval(&self.config.interval).unwrap_or(std::time::Duration::from_secs(1800));

        if interval.as_secs() < MIN_INTERVAL_SECS {
            warn!(
                "Heartbeat interval {}s is below minimum {}s, clamping",
                interval.as_secs(),
                MIN_INTERVAL_SECS
            );
            interval = std::time::Duration::from_secs(MIN_INTERVAL_SECS);
        }

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

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    info!("Heartbeat scheduler shutting down");
                    return;
                }
                _ = self.recv_poke() => {
                    // Poke signal — fire immediately, skip quiet hours (intentional poke)
                    debug!("Heartbeat: poke signal received, firing immediately");
                    if !Self::try_fire(&tx) { return; }
                }
                _ = ticker.tick() => {
                    if self.is_quiet_hours() {
                        debug!("Heartbeat: in quiet hours, skipping");
                        if let Err(e) = tx.try_send(HeartbeatEvent::Result(HeartbeatResult::Skipped {
                            reason: SkipReason::QuietHours,
                        })) {
                            debug!("Failed to send quiet-hours skip event: {e}");
                        }
                        continue;
                    }
                    if !Self::try_fire(&tx) { return; }
                }
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
            let mut upcoming = schedule.upcoming(chrono::Utc);
            if let (Some(t1), Some(t2)) = (upcoming.next(), upcoming.next()) {
                let interval = (t2 - t1).to_std().unwrap_or_default();
                if interval < std::time::Duration::from_secs(60) {
                    warn!(
                        "Cron expression '{cron_expr}' fires more often than once per minute ({interval:?}). Falling back to interval."
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

        loop {
            let now = chrono::Local::now();
            let next = match schedule.upcoming(chrono::Local).next() {
                Some(t) => t,
                None => {
                    tracing::error!("Cron schedule exhausted, falling back to interval mode");
                    self.run_interval(tx, cancel).await;
                    return;
                }
            };

            let wait = (next - now)
                .to_std()
                .unwrap_or(std::time::Duration::from_secs(60));

            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    info!("Heartbeat scheduler shutting down");
                    return;
                }
                _ = self.recv_poke() => {
                    debug!("Heartbeat: poke signal received, firing immediately");
                    if !Self::try_fire(&tx) { return; }
                }
                _ = tokio::time::sleep(wait) => {
                    if self.is_quiet_hours() {
                        debug!("Heartbeat: in quiet hours, skipping");
                        if let Err(e) = tx.try_send(HeartbeatEvent::Result(HeartbeatResult::Skipped {
                            reason: SkipReason::QuietHours,
                        })) {
                            debug!("Failed to send quiet-hours skip event: {e}");
                        }
                        continue;
                    }
                    if !Self::try_fire(&tx) { return; }
                }
            }
        }
    }

    fn is_quiet_hours(&self) -> bool {
        let Some((start, end)) = self.quiet_hours else {
            return false;
        };

        let now = chrono::Utc::now().with_timezone(&self.timezone).time();

        if start <= end {
            now >= start && now < end
        } else {
            now >= start || now < end
        }
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
        };
        let sched = test_scheduler(config, chrono_tz::UTC);
        assert!(!sched.is_quiet_hours());
    }

    #[test]
    fn quiet_hours_uses_timezone() {
        let config = HeartbeatConfig {
            interval: "30m".to_string(),
            quiet_hours_start: Some("00:00".to_string()),
            quiet_hours_end: Some("06:00".to_string()),
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
        };
        // Verify the timezone field is used and doesn't panic
        let sched = test_scheduler(config, chrono_tz::US::Eastern);
        let _ = sched.is_quiet_hours();
    }

    #[test]
    fn default_config_has_quiet_hours() {
        let config = HeartbeatConfig::default();
        assert_eq!(config.quiet_hours_start, Some("00:00".to_string()));
        assert_eq!(config.quiet_hours_end, Some("06:00".to_string()));
        assert!(config.channels.is_empty());
    }

    #[test]
    fn quiet_hours_wraparound_midnight() {
        // Quiet hours that cross midnight: 22:00 - 06:00
        let config = HeartbeatConfig {
            interval: "30m".to_string(),
            quiet_hours_start: Some("22:00".to_string()),
            quiet_hours_end: Some("06:00".to_string()),
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
        };
        let sched = test_scheduler(config, chrono_tz::UTC);
        // This test just ensures the wrap-around path doesn't panic.
        // The actual result depends on current UTC time.
        let _ = sched.is_quiet_hours();
    }

    #[test]
    fn quiet_hours_only_start_configured() {
        let config = HeartbeatConfig {
            interval: "30m".to_string(),
            quiet_hours_start: Some("00:00".to_string()),
            quiet_hours_end: None,
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
        };
        let sched = test_scheduler(config, chrono_tz::UTC);
        // Missing end => not quiet
        assert!(!sched.is_quiet_hours());
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

    #[tokio::test]
    async fn interval_below_minimum_clamped() {
        let config = HeartbeatConfig {
            interval: "30s".to_string(), // below MIN_INTERVAL_SECS (60)
            quiet_hours_start: None,
            quiet_hours_end: None,
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
        };

        let (poke_tx, poke_rx) = mpsc::channel(8);
        let (fire_tx, mut fire_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        let scheduler = HeartbeatScheduler::new(config, chrono_tz::UTC, poke_rx);
        let cancel_clone = cancel.clone();
        tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

        // Drain SchedulerStarted
        drain_started(&mut fire_rx).await;

        // Should still respond to poke (interval gets clamped, not rejected)
        poke_tx.send(()).await.unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), fire_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert!(matches!(event, HeartbeatEvent::Fire));
        cancel.cancel();
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

    #[tokio::test]
    async fn scheduler_emits_started_event() {
        let config = HeartbeatConfig {
            interval: "30m".to_string(),
            quiet_hours_start: None,
            quiet_hours_end: None,
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
        };

        let (_poke_tx, poke_rx) = mpsc::channel(8);
        let (fire_tx, mut fire_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        let scheduler = HeartbeatScheduler::new(config, chrono_tz::UTC, poke_rx);
        let cancel_clone = cancel.clone();
        tokio::spawn(async move { scheduler.run(fire_tx, cancel_clone).await });

        // First event should be SchedulerStarted
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), fire_rx.recv())
            .await
            .expect("timed out waiting for SchedulerStarted")
            .expect("channel closed");

        match event {
            HeartbeatEvent::SchedulerStarted { mode } => {
                assert!(
                    mode.contains("interval"),
                    "expected mode to contain 'interval', got: {mode}"
                );
            }
            other => panic!("expected SchedulerStarted, got: {other:?}"),
        }

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

    #[test]
    fn skip_reason_display() {
        assert_eq!(SkipReason::QuietHours.to_string(), "quiet hours");
        assert_eq!(SkipReason::EmptyResponse.to_string(), "empty response");
        assert_eq!(
            SkipReason::DuplicateResponse.to_string(),
            "duplicate response"
        );
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
}
