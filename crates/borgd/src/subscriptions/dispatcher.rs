//! The dispatcher: claim waiting events and start runs, within the limits.
//!
//! One loop, woken by [`wake::poke`] or a 15 s tick (a row written by
//! another process, a run budget that opened up). Runs live in a `JoinSet`
//! owned by the loop, so a supervisor restart aborts them with it, and the
//! boot reset hands their events back to the queue: no run is ever left
//! orphaned and none runs twice at once.

use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use cortex::config::Config;
use cortex::db::Database;
use cortex::subscriptions::{event_status, new_id, session_key, wake, RUN_ID_PREFIX};
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

/// Claims of one event before the boot reset gives up on it. An event whose
/// run stops the daemon would otherwise restart it in a loop.
pub(crate) const MAX_ATTEMPTS: i64 = 3;

/// Pause after a wake before claiming, so a burst of pokes is one pass.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// What a finished run task hands back.
struct Joined {
    subscription_id: String,
    run_id: String,
    result: std::result::Result<Result<run::Ended>, String>,
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
    /// How long a subscription whose run was deferred is passed over.
    pub defer_backoff: Duration,
}

/// Production [`Dispatcher::defer_backoff`].
pub(crate) const DEFER_BACKOFF: Duration = TICK;

impl Dispatcher {
    /// Run until `cancel`. Bumps `liveness` every pass, at least once per
    /// [`TICK`], for the supervisor's stall watchdog.
    pub async fn run(&self, cancel: CancellationToken, liveness: Arc<AtomicU64>) -> Result<()> {
        let notify = wake::install();
        let db = Database::open().await?;
        let (requeued, abandoned) = db.reset_running_subscription_events(MAX_ATTEMPTS).await?;
        if requeued > 0 {
            tracing::info!("subscriptions: {requeued} events from runs cut short wait again");
        }
        if abandoned > 0 {
            tracing::warn!(
                "subscriptions: {abandoned} events closed as failed: Borg stopped each time they ran"
            );
        }
        let mut runs: JoinSet<Joined> = JoinSet::new();
        let mut cap_logged_at: i64 = 0;
        // Subscriptions whose last run was deferred, until when.
        let mut held_back: HashMap<String, Instant> = HashMap::new();
        // Subscriptions whose session the user was chatting in at the last
        // claim. Passed over without a claim while that turn lasts: a claim
        // and its release are two write transactions on every pass, and in a
        // burst the endpoint needs that write lock.
        let mut chatting: HashSet<String> = HashSet::new();
        // A settings change starts a pass now, so the gateway's endpoint and
        // tap hear it at once instead of on the next tick.
        let mut config_changes = self.config_rx.clone();
        loop {
            liveness.fetch_add(1, Ordering::Relaxed);
            let config = self.config_rx.borrow().clone();
            wake::set_live_settings(&config.subscriptions);
            self.drain_notices(&db, &config).await;
            match db.tapped_subscription_channels().await {
                Ok(channels) => wake::set_tapped_channels(channels),
                Err(e) => tracing::warn!("subscriptions: could not read tapped channels: {e}"),
            }
            // `borg pause` stops new turns; a claimed run would only find its
            // turn refused, so nothing is claimed while it holds.
            if config.subscriptions.enabled && !self.host.emergency_stopped().await {
                // A database error (the write lock held by a burst of
                // inserts) costs one pass, never the loop and the runs in
                // flight with it.
                held_back.retain(|_, until| *until > Instant::now());
                if let Err(e) = self
                    .start_runs(
                        &db,
                        &config,
                        &mut runs,
                        &mut cap_logged_at,
                        &held_back,
                        &mut chatting,
                    )
                    .await
                {
                    tracing::warn!("subscriptions: could not start runs this pass: {e:#}");
                }
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
                    match joined {
                        Ok(joined) => {
                            if matches!(joined.result, Ok(Ok(run::Ended::Deferred))) {
                                held_back.insert(
                                    joined.subscription_id.clone(),
                                    Instant::now() + self.defer_backoff,
                                );
                            }
                            self.reap(&db, &config, joined).await;
                        }
                        // Panics are caught inside the task, so this is an
                        // abort; its events wait for the boot reset.
                        Err(e) => tracing::warn!("subscriptions: a run task ended abnormally: {e}"),
                    }
                }
                _ = notify.notified() => tokio::time::sleep(DEBOUNCE).await,
                Ok(()) = config_changes.changed() => {}
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
        held_back: &HashMap<String, Instant>,
        chatting: &mut HashSet<String>,
    ) -> Result<()> {
        let max = config.subscriptions.max_concurrent.max(1);
        chatting.retain(|id| self.host.is_busy(&session_key(id)));
        // Subscriptions whose session the user is chatting in, and those
        // just deferred: passed over, so they hold neither a slot nor the
        // others up.
        let mut busy: Vec<String> = held_back.keys().chain(chatting.iter()).cloned().collect();
        while runs.len() < max {
            let now = chrono::Utc::now().timestamp();
            let today = db.subscription_runs_since(now - DAY_SECS).await?;
            // Past the daily cap only a test event runs: the user pressed
            // Test to see what a run does, and that answer cannot wait a day.
            let capped = today >= i64::from(config.subscriptions.max_runs_per_day);
            if capped && now - *cap_logged_at >= 3600 {
                tracing::warn!(
                    "subscriptions: {today} runs in the last day reached subscriptions.max_runs_per_day; events wait"
                );
                *cap_logged_at = now;
            }
            let run_id = new_id(RUN_ID_PREFIX);
            let Some(lead) = db
                .claim_next_subscription_event_except(&run_id, now, &busy, capped)
                .await?
            else {
                return Ok(());
            };
            if self.host.is_busy(&session_key(&lead.subscription_id)) {
                db.release_subscription_run(&run_id).await?;
                chatting.insert(lead.subscription_id.clone());
                busy.push(lead.subscription_id);
                continue;
            }
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
            Ok(Ok(_)) => return,
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
                announce_pause(&self.host, db, config, &self.bus, &sub, &reason).await;
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
            announce_pause(&self.host, db, config, &self.bus, &sub, &reason).await;
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

    /// Records turns and sends; answers every turn with `reply`, or with an
    /// agent error when `reply` is `None`.
    #[derive(Default)]
    struct RecordingHost {
        reply: Option<String>,
        /// How long each turn takes.
        turn_ms: u64,
        turns: Mutex<Vec<InboundTurn>>,
        sent: Mutex<Vec<(String, String, String)>>,
        /// Sessions the user is "chatting in".
        busy: Mutex<std::collections::HashSet<String>>,
        /// Hand back the executor's deferred shape (pre-cancelled, closed).
        defer: std::sync::atomic::AtomicBool,
        /// `borg pause` is on.
        stopped: std::sync::atomic::AtomicBool,
        deferrals: std::sync::atomic::AtomicUsize,
        in_flight: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl RecordingHost {
        fn new(reply: Option<&str>) -> Arc<Self> {
            Self::with(reply, |_| {})
        }

        fn with(reply: Option<&str>, tweak: impl FnOnce(&mut Self)) -> Arc<Self> {
            let mut host = Self {
                reply: reply.map(str::to_string),
                turn_ms: 50,
                ..Default::default()
            };
            tweak(&mut host);
            Arc::new(host)
        }

        fn turn_count(&self) -> usize {
            self.turns.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl RunHost for RecordingHost {
        async fn start_turn(
            &self,
            turn: InboundTurn,
        ) -> anyhow::Result<(mpsc::Receiver<AgentEvent>, CancellationToken)> {
            if self.defer.load(Ordering::SeqCst) {
                self.deferrals.fetch_add(1, Ordering::SeqCst);
                let cancelled = CancellationToken::new();
                cancelled.cancel();
                let (_, rx) = mpsc::channel(1);
                return Ok((rx, cancelled));
            }
            self.turns.lock().unwrap().push(turn);
            let (tx, rx) = mpsc::channel(4);
            let reply = self.reply.clone();
            let turn_ms = self.turn_ms;
            let (in_flight, peak) = (self.in_flight.clone(), self.peak.clone());
            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(turn_ms)).await;
                let evt = match reply {
                    Some(text) => AgentEvent::TextDelta(text),
                    None => AgentEvent::Error("provider down".into()),
                };
                in_flight.fetch_sub(1, Ordering::SeqCst);
                let _ = tx.send(evt).await;
            });
            Ok((rx, CancellationToken::new()))
        }

        fn is_busy(&self, session_key: &str) -> bool {
            self.busy.lock().unwrap().contains(session_key)
        }

        async fn emergency_stopped(&self) -> bool {
            self.stopped.load(Ordering::SeqCst)
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
            defer_backoff: Duration::from_millis(200),
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
    async fn a_queue_of_fifty_one_events_is_a_full_digest_and_the_rest() {
        let _home = crate::test_env::IsolatedHome::new();
        let db = Database::open().await.unwrap();
        let sub = active_sub(&db, None).await;
        for i in 0..51 {
            add_event(&db, &sub, &format!("e{i}"), None).await;
        }
        let host = RecordingHost::new(Some("Found the cause."));
        let running = start(host.clone(), Config::default());
        let events = settled(&db, &sub).await;
        stop(running).await;

        assert_eq!(events.len(), 51);
        assert!(events.iter().all(|e| e.status == "succeeded"));
        let mut per_run: std::collections::HashMap<_, usize> = std::collections::HashMap::new();
        for e in &events {
            *per_run.entry(e.run_id.clone()).or_default() += 1;
        }
        let mut sizes: Vec<_> = per_run.into_values().collect();
        sizes.sort_unstable();
        assert_eq!(
            sizes,
            vec![1, 50],
            "a run takes at most {DIGEST}",
            DIGEST = run::DIGEST_LIMIT
        );
        let turns = host.turns.lock().unwrap();
        assert_eq!(turns.len(), 2);
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
        let _home = crate::test_env::IsolatedHome::new();
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
        let _home = crate::test_env::IsolatedHome::new();
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
        // A row that was already waiting when the breaker tripped.
        db.conn()
            .execute(
                "INSERT INTO subscription_events (id, subscription_id, summary, payload, status, received_at) \
                 VALUES ('evt_waiting', ?1, 'late', '{}', 'pending', ?2)",
                libsql::params!(sub.clone(), chrono::Utc::now().timestamp()),
            )
            .await
            .unwrap();
        wake::poke();
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
        assert_eq!(host.turn_count(), 2, "no run after the pause");
        let waiting = db
            .get_subscription_event("evt_waiting")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(waiting.status, "pending", "it waits for a resume");
        let mut storms = 0;
        while let Ok(a) = activity.try_recv() {
            if a.kind == kind::STORM {
                storms += 1;
            }
        }
        assert_eq!(storms, 1, "the pause is announced once");
    }

    /// Poll `cond` for up to five seconds.
    async fn eventually(cond: impl Fn() -> bool) -> bool {
        for _ in 0..100 {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    async fn statuses(db: &Database, sub: &str) -> Vec<String> {
        db.list_subscription_events(sub, 100)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.status)
            .collect()
    }

    #[tokio::test]
    async fn a_deferred_turn_hands_its_events_back_instead_of_closing_them() {
        let _home = crate::test_env::IsolatedHome::new();
        let db = Database::open().await.unwrap();
        let sub = active_sub(&db, None).await;
        add_event(&db, &sub, "a", None).await;
        let host = RecordingHost::with(Some("done"), |h| h.defer = true.into());
        let running = start(host.clone(), Config::default());
        assert!(eventually(|| host.deferrals.load(Ordering::SeqCst) >= 1).await);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            statuses(&db, &sub).await,
            vec!["pending"],
            "not closed as silent"
        );
        let deferrals = host.deferrals.load(Ordering::SeqCst);
        assert!(
            deferrals <= 5,
            "a deferred subscription is held back, not claimed again at once: {deferrals}"
        );

        host.defer.store(false, Ordering::SeqCst);
        wake::poke();
        let events = settled(&db, &sub).await;
        stop(running).await;
        assert_eq!(
            events[0].status, "succeeded",
            "it runs once turns start again"
        );
    }

    #[tokio::test]
    async fn nothing_is_claimed_while_the_emergency_stop_holds() {
        let _home = crate::test_env::IsolatedHome::new();
        let db = Database::open().await.unwrap();
        let sub = active_sub(&db, None).await;
        add_event(&db, &sub, "a", None).await;
        let host = RecordingHost::with(Some("done"), |h| h.stopped = true.into());
        let running = start(host.clone(), Config::default());
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(host.turn_count(), 0);
        assert_eq!(statuses(&db, &sub).await, vec!["pending"]);
        stop(running).await;
    }

    #[tokio::test]
    async fn a_busy_session_does_not_hold_up_other_subscriptions() {
        let _home = crate::test_env::IsolatedHome::new();
        let db = Database::open().await.unwrap();
        let chatting = active_sub(&db, None).await;
        let quiet = active_sub(&db, None).await;
        // The busy one's event is older, so a plain claim would pick it first.
        add_event(&db, &chatting, "old", None).await;
        add_event(&db, &quiet, "new", None).await;
        let host = RecordingHost::with(Some("done"), |h| {
            h.busy
                .get_mut()
                .unwrap()
                .insert(format!("subscription:{chatting}"));
        });
        let mut config = Config::default();
        config.subscriptions.max_concurrent = 1;
        let running = start(host.clone(), config);
        settled(&db, &quiet).await;
        assert_eq!(statuses(&db, &chatting).await, vec!["pending"]);
        assert_eq!(host.turn_count(), 1);

        host.busy.lock().unwrap().clear();
        wake::poke();
        settled(&db, &chatting).await;
        stop(running).await;
        assert_eq!(host.turn_count(), 2, "it runs once the user is done");
    }

    #[tokio::test]
    async fn runs_never_exceed_max_concurrent() {
        let _home = crate::test_env::IsolatedHome::new();
        let db = Database::open().await.unwrap();
        let mut subs = Vec::new();
        for i in 0..4 {
            let sub = active_sub(&db, None).await;
            add_event(&db, &sub, &format!("e{i}"), None).await;
            subs.push(sub);
        }
        let host = RecordingHost::with(Some("done"), |h| h.turn_ms = 200);
        let mut config = Config::default();
        config.subscriptions.max_concurrent = 2;
        let running = start(host.clone(), config);
        for sub in &subs {
            settled(&db, sub).await;
        }
        stop(running).await;
        assert_eq!(host.turn_count(), 4);
        assert_eq!(host.peak.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn the_daily_cap_holds_events_back() {
        let _home = crate::test_env::IsolatedHome::new();
        let db = Database::open().await.unwrap();
        let a = active_sub(&db, None).await;
        let b = active_sub(&db, None).await;
        add_event(&db, &a, "a", None).await;
        add_event(&db, &b, "b", None).await;
        let host = RecordingHost::new(Some("done"));
        let mut config = Config::default();
        config.subscriptions.max_runs_per_day = 1;
        let running = start(host.clone(), config);
        assert!(eventually(|| host.turn_count() == 1).await);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(host.turn_count(), 1, "the second run waits for tomorrow");
        let mut all = statuses(&db, &a).await;
        all.extend(statuses(&db, &b).await);
        all.sort();
        assert_eq!(all, vec!["pending", "succeeded"]);

        // Test still answers past the cap, on the subscription whose real
        // event waits, and that event still waits.
        let waiting = if statuses(&db, &a).await == ["pending"] {
            &a
        } else {
            &b
        };
        let test = db
            .insert_subscription_test_event(waiting, "Test", "{}")
            .await
            .unwrap();
        wake::poke();
        assert!(
            eventually(|| host.turn_count() == 2).await,
            "a test runs past the daily cap"
        );
        let mut test_status = String::new();
        for _ in 0..100 {
            test_status = db
                .get_subscription_event(&test.id)
                .await
                .unwrap()
                .unwrap()
                .status;
            if test_status != "pending" && test_status != "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        stop(running).await;
        assert_eq!(test_status, "succeeded");
        let mut beside_the_test: Vec<String> = db
            .list_subscription_events(waiting, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.id != test.id)
            .map(|e| e.status)
            .collect();
        beside_the_test.sort();
        assert_eq!(
            beside_the_test,
            vec!["pending"],
            "the real event beside the test still waits for tomorrow"
        );
    }

    #[tokio::test]
    async fn events_of_a_run_cut_short_by_a_restart_run_again() {
        let _home = crate::test_env::IsolatedHome::new();
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
