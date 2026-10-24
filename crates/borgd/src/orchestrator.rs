//! Daemon-side orchestration: spawn gateway, heartbeat scheduler, scheduled-task
//! dispatcher, watchdog, lock refresh — all in one place.

use crate::goal_bus::GoalEventBus;
use crate::heartbeat::HeartbeatTrigger;
use crate::heartbeat::{self, HeartbeatBus};
use crate::scheduler;
use crate::session::{DaemonSessionExecutor, SessionRegistry};
use crate::supervisor::{spawn_supervised, DbHandle, SupervisorConfig};
use crate::task_runner;
use anyhow::Result;
use cortex::config::Config;
use cortex::constants::{GATEWAY_MAX_CRASH_RESPAWNS, GATEWAY_RESPAWN_BASE_DELAY};
use cortex::llm::CostEventSink;
use cortex::telemetry::BorgMetrics;
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

/// Spawn a gateway server task. Returns the join handle.
fn spawn_gateway(
    config: &Config,
    shutdown: CancellationToken,
    poke_tx: Option<mpsc::Sender<()>>,
    executor: Arc<DaemonSessionExecutor>,
    grpc_web: gateway::webui::GrpcWebSlot,
) -> tokio::task::JoinHandle<()> {
    let gw_config = config.clone();
    tokio::spawn(async move {
        match gateway::GatewayServer::new(
            gw_config,
            shutdown,
            cortex::telemetry::BorgMetrics::noop(),
            poke_tx,
            Some(executor),
            Some(grpc_web),
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

/// Run gateways one after another until the daemon shuts down.
///
/// `/internal/restart` stops a gateway by cancelling only that gateway's
/// token, and the gateway then returns cleanly. The supervisor reads a clean
/// return as "finished for good", so the restart happens here: a gateway that
/// stops while `outer` is still live is followed by a new one (`spawn` gets
/// `true` for each of those). Each gateway gets a child token of `outer`, and
/// `spawn` resolves when its gateway task ends. A panic or join error goes
/// back to the supervisor as a crash.
async fn run_gateway_incarnations<S, Fut>(outer: CancellationToken, mut spawn: S) -> Result<()>
where
    S: FnMut(CancellationToken, bool) -> Fut,
    Fut: std::future::Future<Output = std::result::Result<(), tokio::task::JoinError>>,
{
    let mut restart = false;
    loop {
        let token = outer.child_token();
        match spawn(token.clone(), restart).await {
            Ok(()) => {}
            Err(e) if e.is_cancelled() => {}
            Err(e) => return Err(anyhow::anyhow!("gateway task join error: {e}")),
        }
        // Only a stop someone asked for (its own token cancelled) starts a
        // new gateway. A gateway that ended by itself — a failed bind, a
        // construction error — goes back to the supervisor, whose backoff
        // and crash cap apply; looping here would retry it with no pause.
        if outer.is_cancelled() || !token.is_cancelled() {
            return Ok(());
        }
        tracing::info!("gateway stopped on request; starting it again");
        restart = true;
    }
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
    /// Bus subscribers read for `Admin.SubscribeSubscriptionEvents`.
    pub subscription_bus: crate::subscriptions::SubscriptionBus,
}

/// Start every long-running subsystem managed by `borgd`. Returns control
/// handles so the gRPC layer can wire `Admin.Poke` and
/// `SubscribeHeartbeatEvents`. Each task observes `shutdown` and drains on
/// cancellation.
#[allow(clippy::too_many_arguments)]
pub async fn start(
    registry: SessionRegistry,
    config: Config,
    shutdown: CancellationToken,
    config_rx: watch::Receiver<Config>,
    cost_event_sink: Arc<dyn CostEventSink>,
    goal_bus: GoalEventBus,
    grpc_web: gateway::webui::GrpcWebSlot,
    shutdown_process: scheduler::ProcessShutdown,
) -> OrchestratorHandles {
    // Cloned before the executor takes ownership; the session-heartbeat poller
    // needs the same live-session map to decide what is hosted and idle.
    let heartbeat_registry = registry.clone();
    let executor = Arc::new(DaemonSessionExecutor::new(
        registry,
        config.clone(),
        BorgMetrics::noop(),
        Some(config_rx.clone()),
        Some(cost_event_sink.clone()),
        Some(goal_bus.clone()),
    ));
    // Open one shared `Database` handle for supervisor activity-log
    // writes. Supervisor uses `std::sync::Mutex` because rusqlite is
    // !Sync; we accept the mutex contention for the rare error-path
    // writes (panic, restart, degraded — none of which are hot).
    //
    // Opened before any subsystem starts: every supervised spawn below
    // needs it, including the heartbeat runner.
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

    let subscription_config_rx = config_rx.clone();
    let (heartbeat_bus, heartbeat_trigger_tx, poke_tx) = heartbeat::spawn(
        config.clone(),
        executor.clone(),
        shutdown.clone(),
        config_rx,
        supervisor_db.clone(),
    );

    // Subscriptions: the dispatcher that runs waiting events, and Borg's own
    // goals and heartbeat as a source. Both supervised; the dispatcher's runs
    // live inside its incarnation, so a restart never leaves one orphaned.
    let subscription_bus = crate::subscriptions::SubscriptionBus::new();
    {
        let dispatch_liveness = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let liveness_for_loop = dispatch_liveness.clone();
        let executor = executor.clone();
        let bus = subscription_bus.clone();
        let rx = subscription_config_rx.clone();
        let cancel = shutdown.clone();
        spawn_supervised(
            SupervisorConfig::new("subscriptions-dispatch").with_liveness(dispatch_liveness),
            shutdown.clone(),
            supervisor_db.clone(),
            move || {
                crate::subscriptions::run_dispatcher(
                    executor.clone(),
                    rx.clone(),
                    bus.clone(),
                    cancel.child_token(),
                    liveness_for_loop.clone(),
                )
            },
        );
    }
    {
        let goals = goal_bus.clone();
        let beats = heartbeat_bus.clone();
        let rx = subscription_config_rx;
        let cancel = shutdown.clone();
        spawn_supervised(
            SupervisorConfig::new("subscriptions-internal"),
            shutdown.clone(),
            supervisor_db.clone(),
            move || {
                let goals = goals.clone();
                let beats = beats.clone();
                let rx = rx.clone();
                let cancel = cancel.child_token();
                async move { crate::subscriptions::internal::run(goals, beats, rx, cancel).await }
            },
        );
    }

    // Hourly budget reset. Supervised for the same reason as everything
    // else here: a swallowed panic would freeze budgets and leave the agent
    // paused past its reset with no signal anywhere.
    {
        let tick_shutdown = shutdown.clone();
        spawn_supervised(
            SupervisorConfig::new("budget-tick"),
            shutdown.clone(),
            supervisor_db.clone(),
            move || crate::budget_tick::run(tick_shutdown.clone()),
        );
    }

    // Gateway with respawn supervisor. Uses the same numeric config the
    // pre-supervisor ad-hoc loop did so behavior is unchanged: max 5
    // crashes in a 10-minute window, exp backoff capped by base × 2^4.
    let gw_cfg = SupervisorConfig {
        max_restarts: GATEWAY_MAX_CRASH_RESPAWNS,
        base_backoff: GATEWAY_RESPAWN_BASE_DELAY,
        max_backoff: GATEWAY_RESPAWN_BASE_DELAY * 16,
        on_degraded: Some(Box::new(mark_gateway_unrecoverable)),
        ..SupervisorConfig::new("gateway")
    };
    let gw_poke_tx = poke_tx.clone();
    let gw_outer_shutdown = shutdown.clone();
    let gw_config = config.clone();
    let gw_executor = executor.clone();
    let gw_grpc_web_outer = grpc_web.clone();
    spawn_supervised(
        gw_cfg,
        gw_outer_shutdown.clone(),
        supervisor_db.clone(),
        move || {
            let gw_config = gw_config.clone();
            let gw_poke_tx = gw_poke_tx.clone();
            let gw_executor = gw_executor.clone();
            // The slot survives gateway restarts: borgd installs the router
            // once, and every respawned gateway serves the same one.
            let gw_grpc_web = gw_grpc_web_outer.clone();
            run_gateway_incarnations(gw_outer_shutdown.clone(), move |token, restart| {
                let fallback = gw_config.clone();
                let poke_tx = gw_poke_tx.clone();
                let executor = gw_executor.clone();
                let grpc_web = gw_grpc_web.clone();
                async move {
                    // A restart exists to pick up changed settings.
                    let config = if restart {
                        Config::load_from_db().await.unwrap_or_else(|e| {
                            tracing::warn!("gateway restart: config reload failed: {e}");
                            fallback
                        })
                    } else {
                        fallback
                    };
                    spawn_gateway(&config, token, Some(poke_tx), executor, grpc_web).await
                }
            })
        },
    );

    // Session-heartbeat poller: fires each session's recurring prompt back into
    // its own conversation when that session is idle and due. Supervised like
    // every other long-running subsystem — a bare `tokio::spawn` here would
    // silently stop firing after one panic.
    {
        let hb_shutdown = shutdown.clone();
        let hb_registry = heartbeat_registry;
        // Reuses the supervisor's handle rather than calling `Database::open()`
        // per tick — that runs the migration check every time and contends the
        // writer lock for the daemon's lifetime.
        let hb_db = supervisor_db.clone();
        spawn_supervised(
            SupervisorConfig::new("session-heartbeat"),
            hb_shutdown.clone(),
            supervisor_db.clone(),
            move || {
                let hb_registry = hb_registry.clone();
                let hb_db = hb_db.clone();
                let cancel = hb_shutdown.child_token();
                async move { crate::session_heartbeat::run(hb_registry, hb_db, cancel).await }
            },
        );
    }

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
                            // Awaits the monitor itself, not a spawn. The
                            // spawning variant returns immediately, which the
                            // supervisor reads as a clean exit — it would stop
                            // supervising at boot and leave a detached,
                            // unwatched monitor whose panic goes unlogged.
                            gateway::imessage::run_imessage_monitor(im_config, im_cancel)
                                .await
                                .map_err(|e| anyhow::anyhow!("iMessage monitor failed: {e}"))
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
    let scheduler_shutdown_process = shutdown_process;
    // Stall detection: the dispatcher bumps this once per loop iteration and
    // the supervisor arms a watchdog against each incarnation. Replaces the
    // dispatcher's own watchdog, which called `std::process::exit(1)` — a
    // wedged dispatcher now restarts instead of taking the daemon down.
    let scheduler_liveness = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let liveness_for_loop = scheduler_liveness.clone();
    spawn_supervised(
        SupervisorConfig::new("scheduler-dispatch").with_liveness(scheduler_liveness),
        scheduler_shutdown.clone(),
        scheduler_db,
        move || {
            let scheduler_config = scheduler_config.clone();
            let scheduler_semaphore = scheduler_semaphore.clone();
            let scheduler_shutdown = scheduler_shutdown.clone();
            let liveness_for_loop = liveness_for_loop.clone();
            let task_cost_event_sink = task_cost_event_sink.clone();
            let scheduler_executor = scheduler_executor.clone();
            let shutdown_process = scheduler_shutdown_process.clone();
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
                    shutdown_process,
                };
                scheduler::run(
                    scheduler_config,
                    hooks,
                    scheduler_semaphore,
                    scheduler_shutdown,
                    liveness_for_loop,
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
        subscription_bus,
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
    // The cli shim has no process-level server to stop: cancelling the
    // lifecycle token is the whole shutdown.
    let shutdown_for_process = shutdown.clone();
    let _handles = start(
        registry,
        config,
        shutdown.clone(),
        config_rx,
        cost_event_sink,
        GoalEventBus::new(),
        gateway::webui::GrpcWebSlot::default(),
        Arc::new(move || shutdown_for_process.cancel()),
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
    use std::time::Duration;

    /// REGRESSION: `/internal/restart` stopped the gateway and nothing
    /// started it again. The gateway returns cleanly once its token is
    /// cancelled, the supervisor treats a clean return as final, and so the
    /// port stayed closed until the daemon itself restarted.
    #[tokio::test]
    async fn a_requested_gateway_stop_starts_a_new_gateway() {
        let outer = CancellationToken::new();
        let mut spawns = Vec::new();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run_gateway_incarnations(outer.clone(), |token, restart| {
                spawns.push(restart);
                // The first gateway is restarted (its own token is cancelled,
                // as `/internal/restart` does); the second sees the daemon
                // shut down.
                if spawns.len() == 1 {
                    token.cancel();
                } else {
                    outer.cancel();
                }
                async move { tokio::spawn(async move { token.cancelled().await }).await }
            }),
        )
        .await
        .expect("gateway loop must end once the daemon shuts down");

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            spawns,
            vec![false, true],
            "a restart must start a second gateway on reloaded config"
        );
    }

    /// A gateway that ends by itself (a failed bind returns `Ok` after
    /// logging) is not a requested restart: the loop hands it back to the
    /// supervisor instead of binding again at once, forever.
    #[tokio::test]
    async fn a_gateway_that_ends_by_itself_is_not_restarted() {
        let outer = CancellationToken::new();
        let mut spawns = 0;
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run_gateway_incarnations(outer.clone(), |_token, _restart| {
                spawns += 1;
                async move { tokio::spawn(async {}).await }
            }),
        )
        .await
        .expect("a gateway that ended by itself must not loop");

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(spawns, 1, "only one gateway may start");
    }

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
            liveness: None,
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
