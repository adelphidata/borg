//! The dispatcher: claim waiting events and start runs, within the limits.
//!
//! One loop, woken by [`wake::poke`] or a 15 s tick (a row written by
//! another process, a run budget that opened up). Runs live in a `JoinSet`
//! owned by the loop, so a supervisor restart aborts them with it, and the
//! boot reset hands their events back to the queue: no run is ever left
//! orphaned and none runs twice at once.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use cortex::config::Config;
use cortex::db::Database;
use cortex::subscriptions::{event_status, new_id, wake, RUN_ID_PREFIX};
use futures::FutureExt;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::bus::{kind, SubscriptionActivity, SubscriptionBus};
use super::{announce_pause, run, RunHost};

/// Longest sleep between two looks at the queue.
pub(crate) const TICK: Duration = Duration::from_secs(15);

/// Seconds in the daily cap's window.
const DAY_SECS: i64 = 24 * 3600;

/// What a finished run task hands back.
struct Joined {
    subscription_id: String,
    run_id: String,
    result: std::result::Result<Result<()>, String>,
}

/// The dispatcher's moving parts.
pub(crate) struct Dispatcher {
    /// Turns, busy checks and channel sends.
    pub host: Arc<dyn RunHost>,
    /// Live settings; read on every pass so a change applies without a
    /// restart.
    pub config_rx: watch::Receiver<Config>,
    /// Activity for the app.
    pub bus: SubscriptionBus,
}

impl Dispatcher {
    /// Run until `cancel`. Bumps `liveness` every pass, at least once per
    /// [`TICK`], for the supervisor's stall watchdog.
    pub async fn run(&self, cancel: CancellationToken, liveness: Arc<AtomicU64>) -> Result<()> {
        let notify = wake::install();
        let db = Database::open().await?;
        let reset = db.reset_running_subscription_events().await?;
        if reset > 0 {
            tracing::info!("subscriptions: {reset} events from runs cut short wait again");
        }
        let mut runs: JoinSet<Joined> = JoinSet::new();
        let mut cap_logged_at: i64 = 0;
        loop {
            liveness.fetch_add(1, Ordering::Relaxed);
            let config = self.config_rx.borrow().clone();
            self.drain_notices(&db, &config).await;
            match db.tapped_subscription_channels().await {
                Ok(channels) => wake::set_tapped_channels(channels),
                Err(e) => tracing::warn!("subscriptions: could not read tapped channels: {e}"),
            }
            if config.subscriptions.enabled {
                self.start_runs(&db, &config, &mut runs, &mut cap_logged_at)
                    .await?;
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    // Aborted runs keep their events `running`; the next
                    // boot's reset hands them back.
                    runs.shutdown().await;
                    return Ok(());
                }
                Some(joined) = runs.join_next(), if !runs.is_empty() => {
                    if let Ok(joined) = joined {
                        self.reap(&db, &config, joined).await;
                    }
                }
                _ = notify.notified() => {}
                _ = tokio::time::sleep(TICK) => {}
            }
        }
    }

    /// Claim and start runs while there is room.
    async fn start_runs(
        &self,
        db: &Database,
        config: &Config,
        runs: &mut JoinSet<Joined>,
        cap_logged_at: &mut i64,
    ) -> Result<()> {
        let max = config.subscriptions.max_concurrent.max(1);
        while runs.len() < max {
            let now = chrono::Utc::now().timestamp();
            let today = db.subscription_runs_since(now - DAY_SECS).await?;
            if today >= i64::from(config.subscriptions.max_runs_per_day) {
                if now - *cap_logged_at >= 3600 {
                    tracing::warn!(
                        "subscriptions: {today} runs in the last day reached subscriptions.max_runs_per_day; events wait"
                    );
                    *cap_logged_at = now;
                }
                return Ok(());
            }
            let run_id = new_id(RUN_ID_PREFIX);
            let Some(lead) = db.claim_next_subscription_event(&run_id, now).await? else {
                return Ok(());
            };
            let subscription_id = lead.subscription_id.clone();
            let host = self.host.clone();
            let bus = self.bus.clone();
            let config = config.clone();
            runs.spawn(async move {
                let result = AssertUnwindSafe(run::execute(host, config, bus, lead))
                    .catch_unwind()
                    .await
                    .map_err(|panic| {
                        panic
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "the run panicked".to_string())
                    });
                Joined {
                    subscription_id,
                    run_id,
                    result,
                }
            });
        }
        Ok(())
    }

    /// Close a run that ended in an error or a panic, so its events do not
    /// stay `running` and block the subscription.
    async fn reap(&self, db: &Database, config: &Config, joined: Joined) {
        let message = match joined.result {
            Ok(Ok(())) => return,
            Ok(Err(e)) => format!("Borg hit an internal error: {e:#}"),
            Err(panic) => format!("Borg hit an internal error: {panic}"),
        };
        tracing::warn!(subscription = %joined.subscription_id, run = %joined.run_id, "subscription run: {message}");
        match db
            .finish_subscription_run(
                &joined.subscription_id,
                &joined.run_id,
                event_status::FAILED,
                None,
                Some(&message),
                0,
            )
            .await
        {
            Ok(false) => return,
            Ok(true) => {}
            Err(e) => {
                tracing::warn!(subscription = %joined.subscription_id, "subscription: could not close a failed run: {e}");
                return;
            }
        }
        self.bus.publish(SubscriptionActivity {
            subscription_id: joined.subscription_id.clone(),
            kind: kind::FINISHED.to_string(),
            run_id: Some(joined.run_id),
            status: Some(event_status::FAILED.to_string()),
            summary: message,
            event_count: 0,
            emitted_at_unix: chrono::Utc::now().timestamp(),
        });
        let sub = match db.get_subscription(&joined.subscription_id).await {
            Ok(Some(sub)) => sub,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!("subscription: lookup after a failed run: {e}");
                return;
            }
        };
        match run::failure_breaker(db, &sub, config).await {
            Ok(Some(reason)) => {
                announce_pause(self.host.as_ref(), db, config, &self.bus, &sub, &reason).await;
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(subscription = %sub.id, "subscription: failure breaker: {e}"),
        }
    }

    /// Turn the producers' signals into app activity and pause notices.
    async fn drain_notices(&self, db: &Database, config: &Config) {
        let now = chrono::Utc::now().timestamp();
        for id in wake::take_seen() {
            self.bus.publish(SubscriptionActivity {
                subscription_id: id,
                kind: kind::RECEIVED.to_string(),
                run_id: None,
                status: None,
                summary: String::new(),
                event_count: 0,
                emitted_at_unix: now,
            });
        }
        for notice in wake::take_storm_notices() {
            let sub = match db.get_subscription(&notice.subscription_id).await {
                Ok(Some(sub)) => sub,
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!("subscription: lookup for a storm notice: {e}");
                    continue;
                }
            };
            let reason = sub.last_error.clone().unwrap_or_else(|| {
                format!(
                    "Paused itself: more than {} events in 10 minutes. Resume it when the source is quiet.",
                    notice.events_in_window.saturating_sub(1)
                )
            });
            announce_pause(self.host.as_ref(), db, config, &self.bus, &sub, &reason).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use cortex::agent::AgentEvent;
    use cortex::db::{IngestLimits, NewSubscription, NewSubscriptionEvent, SubscriptionEventRow};
    use cortex::subscriptions::status;
    use gateway::InboundTurn;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    /// `Database::open` reads `BORG_HOME`; point it at a fresh directory and
    /// hold the crate-wide env lock.
    struct HomeGuard {
        prev: Option<String>,
        _tmp: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl HomeGuard {
        fn isolated() -> Self {
            let lock = crate::test_env::env_lock();
            let tmp = tempfile::tempdir().expect("tempdir");
            let prev = std::env::var("BORG_HOME").ok();
            std::env::set_var("BORG_HOME", tmp.path());
            Self {
                prev,
                _tmp: tmp,
                _lock: lock,
            }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("BORG_HOME", v),
                None => std::env::remove_var("BORG_HOME"),
            }
        }
    }

    /// Records turns and sends; answers every turn with `reply`, or with an
    /// agent error when `reply` is `None`.
    struct RecordingHost {
        reply: Option<String>,
        turns: Mutex<Vec<InboundTurn>>,
        sent: Mutex<Vec<(String, String, String)>>,
    }

    impl RecordingHost {
        fn new(reply: Option<&str>) -> Arc<Self> {
            Arc::new(Self {
                reply: reply.map(str::to_string),
                turns: Mutex::new(Vec::new()),
                sent: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl RunHost for RecordingHost {
        async fn start_turn(
            &self,
            turn: InboundTurn,
        ) -> anyhow::Result<(mpsc::Receiver<AgentEvent>, CancellationToken)> {
            self.turns.lock().unwrap().push(turn);
            let (tx, rx) = mpsc::channel(4);
            let reply = self.reply.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let evt = match reply {
                    Some(text) => AgentEvent::TextDelta(text),
                    None => AgentEvent::Error("provider down".into()),
                };
                let _ = tx.send(evt).await;
            });
            Ok((rx, CancellationToken::new()))
        }

        fn is_busy(&self, _session_key: &str) -> bool {
            false
        }

        async fn deliver(
            &self,
            _config: &Config,
            channel: &str,
            target: &str,
            text: &str,
        ) -> anyhow::Result<()> {
            self.sent
                .lock()
                .unwrap()
                .push((channel.into(), target.into(), text.into()));
            Ok(())
        }
    }

    const LIMITS: IngestLimits = IngestLimits {
        per_minute: 1000,
        storm_threshold: 1000,
        max_pending: 1000,
    };

    async fn active_sub(db: &Database, delivery: Option<&str>) -> String {
        db.create_subscription(&NewSubscription {
            name: "ops",
            source: "channel",
            provider: "slack",
            auth_scheme: "none",
            secret_ref: None,
            filter: "{}",
            instruction: "Look into it.",
            delivery_channel: delivery,
            delivery_target: None,
            status: status::ACTIVE,
            rate_limit_per_hour: 60,
            timeout_secs: 30,
        })
        .await
        .unwrap()
        .id
    }

    async fn add_event(db: &Database, sub: &str, key: &str, origin: Option<&str>) {
        db.insert_subscription_event(
            &NewSubscriptionEvent {
                subscription_id: sub,
                kind: Some("message"),
                dedup_key: Some(key),
                summary: "needs repro: checkout fails",
                payload: r#"{"text":"needs repro"}"#,
                payload_truncated: false,
                origin,
                received_at: chrono::Utc::now().timestamp(),
            },
            LIMITS,
        )
        .await
        .unwrap();
        wake::poke();
    }

    struct Running {
        cancel: CancellationToken,
        handle: tokio::task::JoinHandle<Result<()>>,
        bus: SubscriptionBus,
    }

    fn start(host: Arc<dyn RunHost>, config: Config) -> Running {
        let (tx, rx) = watch::channel(config);
        let bus = SubscriptionBus::new();
        let dispatcher = Dispatcher {
            host,
            config_rx: rx,
            bus: bus.clone(),
        };
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let handle = tokio::spawn(async move {
            let _sender = tx;
            dispatcher.run(token, Arc::new(AtomicU64::new(0))).await
        });
        Running {
            cancel,
            handle,
            bus,
        }
    }

    async fn stop(running: Running) {
        running.cancel.cancel();
        running.handle.await.unwrap().unwrap();
    }

    /// Wait until no event of `sub` is waiting or running.
    async fn settled(db: &Database, sub: &str) -> Vec<SubscriptionEventRow> {
        for _ in 0..200 {
            let events = db.list_subscription_events(sub, 100).await.unwrap();
            if events
                .iter()
                .all(|e| e.status != "pending" && e.status != "running")
            {
                return events;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("events of {sub} never settled");
    }

    #[tokio::test]
    async fn a_queue_of_fifty_events_is_one_digest_run() {
        let _home = HomeGuard::isolated();
        let db = Database::open().await.unwrap();
        let sub = active_sub(&db, None).await;
        for i in 0..50 {
            add_event(&db, &sub, &format!("e{i}"), None).await;
        }
        let host = RecordingHost::new(Some("Found the cause."));
        let running = start(host.clone(), Config::default());
        let events = settled(&db, &sub).await;
        stop(running).await;

        assert_eq!(events.len(), 50);
        assert!(events.iter().all(|e| e.status == "succeeded"));
        let runs: std::collections::HashSet<_> = events.iter().map(|e| e.run_id.clone()).collect();
        assert_eq!(runs.len(), 1, "one run handled every event");
        let turns = host.turns.lock().unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].session_key, format!("subscription:{sub}"));
        assert!(
            turns[0].text.contains("50"),
            "the prompt says how many events: {}",
            turns[0].text
        );
        assert!(
            host.sent.lock().unwrap().is_empty(),
            "session-only delivery sends nothing"
        );
    }

    #[tokio::test]
    async fn a_reply_goes_to_the_thread_that_fired_it_and_silence_goes_nowhere() {
        let _home = HomeGuard::isolated();
        let db = Database::open().await.unwrap();
        let sub = active_sub(&db, Some("origin")).await;
        let origin = r#"{"channel":"slack","sender":"U1","chat":"C9","thread_id":"1.0","session_key":"U1:1.0"}"#;
        add_event(&db, &sub, "a", Some(origin)).await;
        let host = RecordingHost::new(Some("Repro steps attached."));
        let running = start(host.clone(), Config::default());
        settled(&db, &sub).await;
        stop(running).await;
        let sent = host.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "slack");
        assert!(
            sent[0].1.contains("\"C9\""),
            "replies in the channel: {}",
            sent[0].1
        );
        assert_eq!(sent[0].2, "[ops]\nRepro steps attached.");

        add_event(&db, &sub, "b", Some(origin)).await;
        let quiet = RecordingHost::new(Some("[SILENT]"));
        let running = start(quiet.clone(), Config::default());
        let events = settled(&db, &sub).await;
        stop(running).await;
        assert!(
            quiet.sent.lock().unwrap().is_empty(),
            "a silent run sends nothing"
        );
        assert!(events.iter().any(|e| e.status == "silent"));
    }

    #[tokio::test]
    async fn failed_runs_in_a_row_pause_the_subscription_and_say_so_once() {
        let _home = HomeGuard::isolated();
        let db = Database::open().await.unwrap();
        let sub = active_sub(&db, None).await;
        let mut config = Config::default();
        config.subscriptions.storm_failures = 2;
        let host = RecordingHost::new(None);
        let running = start(host.clone(), config);
        let mut activity = running.bus.subscribe();
        for key in ["a", "b"] {
            add_event(&db, &sub, key, None).await;
            settled(&db, &sub).await;
        }
        let refused = db
            .insert_subscription_event(
                &NewSubscriptionEvent {
                    subscription_id: &sub,
                    kind: None,
                    dedup_key: Some("c"),
                    summary: "c",
                    payload: "{}",
                    payload_truncated: false,
                    origin: None,
                    received_at: chrono::Utc::now().timestamp(),
                },
                LIMITS,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        stop(running).await;

        let row = db.get_subscription(&sub).await.unwrap().unwrap();
        assert_eq!(row.status, "storm");
        assert!(row.last_error.unwrap().contains("last 2 runs failed"));
        assert!(
            matches!(refused, cortex::db::InsertOutcome::NotAccepting),
            "a paused subscription takes no new events: {refused:?}"
        );
        assert_eq!(
            host.turns.lock().unwrap().len(),
            2,
            "no run after the pause"
        );
        let mut storms = 0;
        while let Ok(a) = activity.try_recv() {
            if a.kind == kind::STORM {
                storms += 1;
            }
        }
        assert_eq!(storms, 1, "the pause is announced once");
    }

    #[tokio::test]
    async fn events_of_a_run_cut_short_by_a_restart_run_again() {
        let _home = HomeGuard::isolated();
        let db = Database::open().await.unwrap();
        let sub = active_sub(&db, None).await;
        add_event(&db, &sub, "a", None).await;
        db.claim_next_subscription_event("run_before_restart", chrono::Utc::now().timestamp())
            .await
            .unwrap()
            .unwrap();
        let host = RecordingHost::new(Some("done"));
        let running = start(host.clone(), Config::default());
        let events = settled(&db, &sub).await;
        stop(running).await;
        assert_eq!(events[0].status, "succeeded");
        assert_ne!(events[0].run_id.as_deref(), Some("run_before_restart"));
    }
}
