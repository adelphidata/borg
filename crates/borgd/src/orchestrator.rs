//! Daemon-side orchestration: spawn gateway, heartbeat scheduler, scheduled-task
//! dispatcher, watchdog, lock refresh — all in one place.

use crate::goal_bus::GoalEventBus;
use crate::heartbeat::HeartbeatTrigger;
use crate::heartbeat::{self, HeartbeatBus};
use crate::scheduler;
use crate::session::{DaemonSessionExecutor, SessionRegistry};
use crate::supervisor::{spawn_supervised, DbHandle, SupervisorConfig, DEFAULT_RESTART_WINDOW};
use crate::task_runner;
use anyhow::Result;
use cortex::config::Config;
use cortex::constants::{GATEWAY_MAX_CRASH_RESPAWNS, GATEWAY_RESPAWN_BASE_DELAY};
use cortex::llm::CostEventSink;
use cortex::telemetry::BorgMetrics;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

/// Spawn a gateway server task. Returns the join handle.
fn spawn_gateway(
    config: &Config,
    shutdown: CancellationToken,
    poke_tx: Option<mpsc::Sender<()>>,
    executor: Arc<DaemonSessionExecutor>,
) -> tokio::task::JoinHandle<()> {
    let gw_config = config.clone();
    tokio::spawn(async move {
        match gateway::GatewayServer::new(
            gw_config,
            shutdown,
            cortex::telemetry::BorgMetrics::noop(),
            poke_tx,
            Some(executor),
        ) {
            Ok(gateway) => {
                if let Err(e) = gateway.run().await {
                    let msg = e.to_string();
                    if msg.contains("address already in use") || msg.contains("AddrInUse") {
                        tracing::warn!("Gateway: {e}");
                    } else {
                        tracing::error!("Gateway server error: {e}");
                    }
                }
            }
            Err(e) => tracing::error!("Failed to create gateway server: {e}"),
        }
    })
}

/// Persist the "gateway is unrecoverable" sentinel after the respawn
/// supervisor abandons the gateway. The heartbeat
/// `gateway_unrecoverable` augmenter and the maintenance recovery step
/// both read this. Best-effort — if the DB can't be opened we still
/// shut down the supervisor (the alternative is a noisy crash that
/// hides the underlying gateway failure).
fn mark_gateway_unrecoverable() {
    // The callback runs inside an async supervisor task, but the
    // `on_degraded` callback type is sync — spawn the actual DB writes
    // as a fire-and-forget task on the current runtime.
    tokio::spawn(async {
        let db = match cortex::db::Database::open().await {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!("mark_gateway_unrecoverable: failed to open db: {e}");
                return;
            }
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        write_gateway_unrecoverable_meta(&db, now).await;
    });
}

/// Inner helper for [`mark_gateway_unrecoverable`] — pulled out so unit
/// tests can exercise the actual write order against an in-memory DB
/// without going through `Database::open()`.
///
/// Writes the timestamp BEFORE the flag — the augmenter's 1h grace
/// check reads the timestamp to know when the abandonment happened. If
/// the flag landed first and the process died before the timestamp
/// write, the next heartbeat would see `flag="1"` with no `_at` and
/// would race the daily 02:00 self-healing pass this layered design
/// exists to avoid.
async fn write_gateway_unrecoverable_meta(db: &cortex::db::Database, now: i64) {
    use crate::heartbeat_augmenters::{GATEWAY_UNRECOVERABLE_AT_KEY, GATEWAY_UNRECOVERABLE_KEY};
    if let Err(e) = db
        .set_meta(GATEWAY_UNRECOVERABLE_AT_KEY, &now.to_string())
        .await
    {
        tracing::warn!("mark_gateway_unrecoverable: failed to set timestamp: {e}");
    }
    if let Err(e) = db.set_meta(GATEWAY_UNRECOVERABLE_KEY, "1").await {
        tracing::warn!("mark_gateway_unrecoverable: failed to set flag: {e}");
    }
}

/// Result of [`start`] — handles the caller needs to query/wire the live tasks.
pub struct OrchestratorHandles {
    /// Bus subscribers read for `Admin.SubscribeHeartbeatEvents`.
    pub heartbeat_bus: HeartbeatBus,
    /// Admin RPCs send heartbeat requests here.
    pub heartbeat_trigger_tx: mpsc::Sender<HeartbeatTrigger>,
    /// Legacy poke endpoint sends `()` here to trigger an immediate heartbeat.
    pub poke_tx: mpsc::Sender<()>,
    /// Executor `Admin.SendApprovalGreeting` and the heartbeat scheduler share.
    pub executor: Arc<DaemonSessionExecutor>,
    /// Bus subscribers read for `Admin.SubscribeGoalEvents`. The same bus
    /// is plumbed into every agent backend so accounting transitions and
    /// the four goal RPC handlers all publish to one place.
    pub goal_bus: GoalEventBus,
}

/// Start every long-running subsystem managed by `borgd`. Returns control
/// handles so the gRPC layer can wire `Admin.Poke` and
/// `SubscribeHeartbeatEvents`. Each task observes `shutdown` and drains on
/// cancellation.
pub async fn start(
    registry: SessionRegistry,
    config: Config,
    shutdown: CancellationToken,
    config_rx: watch::Receiver<Config>,
    cost_event_sink: Arc<dyn CostEventSink>,
    goal_bus: GoalEventBus,
) -> OrchestratorHandles {
    let executor = Arc::new(DaemonSessionExecutor::new(
        registry,
        config.clone(),
        BorgMetrics::noop(),
        Some(config_rx.clone()),
        Some(cost_event_sink.clone()),
        Some(goal_bus.clone()),
    ));
    let (heartbeat_bus, heartbeat_trigger_tx, poke_tx) = heartbeat::spawn(
        config.clone(),
        executor.clone(),
        shutdown.clone(),
        config_rx,
    );

    // Open one shared `Database` handle for supervisor activity-log
    // writes. Supervisor uses `std::sync::Mutex` because rusqlite is
    // !Sync; we accept the mutex contention for the rare error-path
    // writes (panic, restart, degraded — none of which are hot).
    //
    // Failure here means the daemon DB is unavailable — every other
    // subsystem will fail too. Panicking loud at orchestrator start
    // is better than degrading silently. Production callers
    // (run_lifecycle, daemon::run) have already validated DB access.
    let supervisor_db: DbHandle = match cortex::db::Database::open().await {
        Ok(db) => Arc::new(db),
        Err(e) => panic!(
            "orchestrator: cortex::db::Database::open() failed: {e}; daemon cannot supervise without DB"
        ),
    };

    // Gateway with respawn supervisor. Uses the same numeric config the
    // pre-supervisor ad-hoc loop did so behavior is unchanged: max 5
    // crashes in a 10-minute window, exp backoff capped by base × 2^4.
    let gw_cfg = SupervisorConfig {
        name: "gateway",
        max_restarts: GATEWAY_MAX_CRASH_RESPAWNS,
        restart_window: DEFAULT_RESTART_WINDOW,
        base_backoff: GATEWAY_RESPAWN_BASE_DELAY,
        max_backoff: GATEWAY_RESPAWN_BASE_DELAY * 16,
        on_degraded: Some(Box::new(mark_gateway_unrecoverable)),
    };
    let gw_poke_tx = poke_tx.clone();
    let gw_outer_shutdown = shutdown.clone();
    let gw_config = config.clone();
    let gw_executor = executor.clone();
    spawn_supervised(
        gw_cfg,
        gw_outer_shutdown.clone(),
        supervisor_db.clone(),
        move || {
            let gw_config = gw_config.clone();
            let gw_poke_tx = gw_poke_tx.clone();
            let gw_executor = gw_executor.clone();
            // Per-iteration child token — cancels when the outer
            // shutdown does AND when this iteration's future drops, so
            // we don't leak a long-lived bridge task across respawns.
            let inner_shutdown = gw_outer_shutdown.child_token();
            async move {
                let handle =
                    spawn_gateway(&gw_config, inner_shutdown, Some(gw_poke_tx), gw_executor);
                // Treat any inner error or panic as a supervisor-level
                // restart; convert into a Result so the supervisor's
                // restart logic kicks in.
                match handle.await {
                    Ok(()) => Ok::<(), anyhow::Error>(()),
                    Err(e) if e.is_cancelled() => Ok(()),
                    Err(e) => Err(anyhow::anyhow!("gateway task join error: {e}")),
                }
            }
        },
    );

    // iMessage monitor (macOS only, optional).
    #[cfg(target_os = "macos")]
    {
        let im_shutdown = shutdown.clone();
        let im_config = config.clone();
        let im_db = supervisor_db.clone();
        spawn_supervised(
            SupervisorConfig::new("imessage-monitor"),
            im_shutdown.clone(),
            im_db,
            move || {
                let im_config = im_config.clone();
                // Forward the daemon's outer shutdown into the monitor
                // via a per-iteration child token. `child_token()`
                // cancels when its parent does AND when the iteration's
                // future drops, so on supervisor restart we don't leak
                // a long-lived bridge task.
                let im_cancel = im_shutdown.child_token();
                async move {
                    let imessage_dir = match Config::data_dir() {
                        Ok(d) => d.join("channels/imessage"),
                        Err(e) => {
                            tracing::warn!("iMessage: data_dir failed: {e}");
                            return Ok::<(), anyhow::Error>(());
                        }
                    };
                    if !imessage_dir.join("channel.toml").exists() {
                        return Ok(());
                    }
                    let probe = gateway::imessage::probe::probe_imessage().await;
                    match probe.status {
                        gateway::imessage::probe::ProbeStatus::Ok => {
                            if let Err(e) =
                                gateway::imessage::start_imessage_monitor(im_config, im_cancel)
                                    .await
                            {
                                return Err(anyhow::anyhow!("iMessage monitor failed: {e}"));
                            }
                            Ok(())
                        }
                        gateway::imessage::probe::ProbeStatus::NoDiskAccess => {
                            tracing::warn!(
                                "iMessage: Full Disk Access required. Skipping monitor."
                            );
                            Ok(())
                        }
                        other => {
                            tracing::warn!("iMessage probe: {other}. Skipping monitor.");
                            Ok(())
                        }
                    }
                }
            },
        );
    }

    // Scheduled-task dispatcher.
    let semaphore = Arc::new(tokio::sync::Semaphore::new(config.tasks.max_concurrent));
    let scheduler_config = config;
    let scheduler_shutdown = shutdown;
    let scheduler_semaphore = semaphore;
    let task_cost_event_sink = cost_event_sink;
    let scheduler_db = supervisor_db;
    // The executor lets heartbeat ticks run the full agent loop (tools + the
    // goal/Ralph loop) for remediation; all other scheduled tasks ignore it.
    let scheduler_executor = executor.clone();
    spawn_supervised(
        SupervisorConfig::new("scheduler-dispatch"),
        scheduler_shutdown.clone(),
        scheduler_db,
        move || {
            let scheduler_config = scheduler_config.clone();
            let scheduler_semaphore = scheduler_semaphore.clone();
            let scheduler_shutdown = scheduler_shutdown.clone();
            let task_cost_event_sink = task_cost_event_sink.clone();
            let scheduler_executor = scheduler_executor.clone();
            async move {
                let cost_for_hook = task_cost_event_sink.clone();
                let exec_for_hook = scheduler_executor.clone();
                let hooks = scheduler::DispatcherHooks {
                    spawn_task: Arc::new(move |task, run_id, retry, sem, cfg| {
                        let cost_event_sink = cost_for_hook.clone();
                        let executor = exec_for_hook.clone();
                        Box::pin(async move {
                            task_runner::spawn_task_execution(
                                task,
                                run_id,
                                retry,
                                sem,
                                cfg,
                                Some(cost_event_sink),
                                Some(executor),
                            )
                            .await
                        })
                    }),
                    deliver_celebration: Arc::new(|cfg, c| {
                        Box::pin(async move {
                            task_runner::deliver_celebration_to_channels(cfg, c).await
                        })
                    }),
                };
                scheduler::run(
                    scheduler_config,
                    hooks,
                    scheduler_semaphore,
                    scheduler_shutdown,
                )
                .await
                .map_err(|e| anyhow::anyhow!("scheduler dispatcher exited: {e}"))
            }
        },
    );

    OrchestratorHandles {
        heartbeat_bus,
        heartbeat_trigger_tx,
        poke_tx,
        executor,
        goal_bus,
    }
}

/// Convenience: drive the orchestrator from a `CancellationToken` until it's
/// cancelled. Used by the cli's `service::run_daemon` shim. Caller is
/// responsible for any pre-flight setup (auto-spawning a borgd binary etc.).
pub async fn run_lifecycle(shutdown: CancellationToken) -> Result<()> {
    let config = Config::load_from_db().await?;
    cortex::activity_log::log_activity(
        &cortex::db::Database::open().await?,
        "info",
        "system",
        "Daemon orchestrator started (cli shim)",
    )
    .await;
    let registry = SessionRegistry::new();
    let (_config_tx, config_rx) = watch::channel(config.clone());
    let cost_event_sink: Arc<dyn CostEventSink> = Arc::new(crate::cost_events::CostEventBus::new(
        &config,
        BorgMetrics::noop(),
    ));
    let _handles = start(
        registry,
        config,
        shutdown.clone(),
        config_rx,
        cost_event_sink,
        GoalEventBus::new(),
    )
    .await;
    shutdown.cancelled().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heartbeat_augmenters::{
        check_gateway_unrecoverable, GATEWAY_UNRECOVERABLE_AT_KEY, GATEWAY_UNRECOVERABLE_KEY,
    };

    async fn in_memory_db() -> cortex::db::Database {
        cortex::db::Database::test_db().await
    }

    /// Round-trip guard: the orchestrator's meta writes must be the exact
    /// keys the augmenter reads. Catches future renames in either module
    /// — without this test, a typo in `GATEWAY_UNRECOVERABLE_KEY` here
    /// would silently disable the augmenter and only surface in
    /// production after a real gateway crash.
    #[tokio::test]
    async fn write_gateway_unrecoverable_meta_round_trips_to_augmenter() {
        let db = in_memory_db().await;
        // Stamp the abandonment two hours in the past so we're past the
        // 1h grace and the augmenter actually fires.
        let two_hours_ago = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64)
            - 2 * 60 * 60;
        write_gateway_unrecoverable_meta(&db, two_hours_ago).await;

        assert_eq!(
            db.get_meta(GATEWAY_UNRECOVERABLE_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some("1"),
            "flag must be persisted under the augmenter's key"
        );
        let cfg = Config::default();
        assert!(
            check_gateway_unrecoverable(&cfg, Some(&db)).await.is_some(),
            "augmenter must observe the orchestrator's write and fire"
        );
    }

    /// If the timestamp write succeeded but the process died before the
    /// flag write, the augmenter must stay silent — there's nothing to
    /// nudge about. Guards the reverse half of the write-order
    /// invariant.
    #[tokio::test]
    async fn timestamp_alone_without_flag_does_not_fire() {
        let db = in_memory_db().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        db.set_meta(GATEWAY_UNRECOVERABLE_AT_KEY, &(now - 7200).to_string())
            .await
            .unwrap();
        let cfg = Config::default();
        assert!(
            check_gateway_unrecoverable(&cfg, Some(&db)).await.is_none(),
            "augmenter must require the flag, not just the timestamp"
        );
    }

    /// End-to-end: a supervised subsystem with the gateway's
    /// degrade-callback wiring must, after exceeding `max_restarts`,
    /// land the two meta keys the augmenter reads. This closes the
    /// chain "supervisor escalates" → "on_degraded fires" →
    /// "write_gateway_unrecoverable_meta lands the keys" →
    /// "augmenter observes them" — none of which is covered by the
    /// individual round-trip / supervisor-escalation tests in
    /// isolation. The test substitutes an in-memory DB for production's
    /// `Database::open()` so we don't need the daemon's real data dir,
    /// but exercises the same code path otherwise.
    #[tokio::test]
    async fn gateway_degrade_marks_unrecoverable_meta() {
        // Shared Arc so the on_degraded closure can write into the same
        // in-memory DB the assertions read from. `Arc<Database>` matches
        // the production `DbHandle` shape — the supervisor needs it for
        // its own activity-log writes.
        let db = Arc::new(in_memory_db().await);
        let cancel = CancellationToken::new();
        let cb_db = db.clone();

        // Track that the callback actually ran — otherwise an assertion
        // that the meta keys are absent could equally mean "callback
        // didn't fire" or "callback ran but wrote the wrong keys", and
        // we want the failure mode unambiguous.
        let callback_fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let callback_fired_inner = callback_fired.clone();

        let cfg = SupervisorConfig {
            name: "gateway",
            max_restarts: 2,
            restart_window: Duration::from_secs(60),
            base_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(5),
            on_degraded: Some(Box::new(move || {
                callback_fired_inner.store(true, std::sync::atomic::Ordering::SeqCst);
                // Mirror production's mark_gateway_unrecoverable: a
                // sync callback that spawns the async writes onto the
                // current runtime. We capture the test DB instead of
                // calling Database::open().
                let cb_db = cb_db.clone();
                tokio::spawn(async move {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0)
                        - 2 * 60 * 60; // 2h ago — past the augmenter's 1h grace
                    write_gateway_unrecoverable_meta(&cb_db, now).await;
                });
            })),
        };

        let handle = spawn_supervised(cfg, cancel.clone(), db.clone(), move || async move {
            Err::<(), _>(anyhow::anyhow!("gateway always crashes"))
        });

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("supervisor stuck")
            .expect("supervisor task panicked");

        // Callback runs synchronously inside the supervisor task before
        // spawning the meta writes, so the flag must be set by the time
        // the supervisor handle resolves. Assert this first — if the
        // callback never fired, the meta-key checks below would also
        // fail but with a misleading message.
        assert!(
            callback_fired.load(std::sync::atomic::Ordering::SeqCst),
            "on_degraded callback must fire when supervisor escalates"
        );

        // The on_degraded callback spawns a tokio task to do the writes;
        // wait on the augmenter's actual signal (both meta keys land
        // AND the 1h grace has elapsed). Bounded by an explicit
        // timeout so failures read as "callback writes never landed"
        // rather than "polled N times and gave up".
        let app_cfg = Config::default();
        let observed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if check_gateway_unrecoverable(&app_cfg, Some(&db))
                    .await
                    .is_some()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            observed.is_ok(),
            "augmenter must observe the supervisor-driven meta writes and fire within 2s"
        );

        // Pin both meta keys explicitly so a future augmenter change
        // that started accepting one key without the other can't mask
        // a regression here.
        assert_eq!(
            db.get_meta(GATEWAY_UNRECOVERABLE_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some("1"),
            "GATEWAY_UNRECOVERABLE_KEY must be set to \"1\""
        );
        assert!(
            db.get_meta(GATEWAY_UNRECOVERABLE_AT_KEY)
                .await
                .unwrap()
                .is_some(),
            "GATEWAY_UNRECOVERABLE_AT_KEY must be populated"
        );
    }

    /// Flag without a timestamp (simulating mid-write death) must NOT
    /// instantly fire — the augmenter falls back to `now`, so the 1h
    /// grace re-arms cleanly.
    #[tokio::test]
    async fn flag_without_timestamp_does_not_fire_immediately() {
        let db = in_memory_db().await;
        db.set_meta(GATEWAY_UNRECOVERABLE_KEY, "1").await.unwrap();
        let cfg = Config::default();
        assert!(
            check_gateway_unrecoverable(&cfg, Some(&db)).await.is_none(),
            "missing timestamp must not be treated as `0` (would race the daily heal)"
        );
    }
}
