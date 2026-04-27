//! Daemon-side orchestration: spawn gateway, heartbeat scheduler, scheduled-task
//! dispatcher, watchdog, lock refresh — all in one place.
//!
//! Called from [`crate::daemon::run`] (production) and from the cli's
//! `service::run_daemon` shim while Stage E migrates the legacy entry points.

use crate::heartbeat::{self, HeartbeatBus};
use crate::scheduler;
use crate::session::{DaemonSessionExecutor, SessionRegistry};
use crate::task_runner;
use anyhow::Result;
use borg_core::config::Config;
use borg_core::constants::{GATEWAY_MAX_CRASH_RESPAWNS, GATEWAY_RESPAWN_BASE_DELAY};
use borg_core::telemetry::BorgMetrics;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Spawn a gateway server task. Returns the join handle.
fn spawn_gateway(
    config: &Config,
    shutdown: CancellationToken,
    poke_tx: Option<mpsc::Sender<()>>,
) -> tokio::task::JoinHandle<()> {
    let gw_config = config.clone();
    tokio::spawn(async move {
        match borg_gateway::GatewayServer::new(
            gw_config,
            shutdown,
            borg_core::telemetry::BorgMetrics::noop(),
            poke_tx,
            None,
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

/// Result of [`start`] — handles the caller needs to query/wire the live tasks.
pub struct OrchestratorHandles {
    /// Bus subscribers read for `Admin.SubscribeHeartbeatEvents`.
    pub heartbeat_bus: HeartbeatBus,
    /// `Admin.Poke` sends `()` here to trigger an immediate heartbeat.
    pub poke_tx: mpsc::Sender<()>,
    /// Executor `Admin.SendApprovalGreeting` and the heartbeat scheduler share.
    pub executor: Arc<DaemonSessionExecutor>,
}

/// Start every long-running subsystem managed by `borgd`. Returns control
/// handles so the gRPC layer can wire `Admin.Poke` and
/// `SubscribeHeartbeatEvents`. Each task observes `shutdown` and drains on
/// cancellation.
pub fn start(
    registry: SessionRegistry,
    config: Config,
    shutdown: CancellationToken,
) -> OrchestratorHandles {
    let executor = Arc::new(DaemonSessionExecutor::new(
        registry,
        config.clone(),
        BorgMetrics::noop(),
    ));
    let (heartbeat_bus, poke_tx) =
        heartbeat::spawn(config.clone(), executor.clone(), shutdown.clone());

    // Gateway with respawn supervisor.
    let gw_poke_tx = poke_tx.clone();
    let gw_outer_shutdown = shutdown.clone();
    let gw_config = config.clone();
    tokio::spawn(async move {
        let mut crash_count: u32 = 0;
        loop {
            if gw_outer_shutdown.is_cancelled() {
                return;
            }
            let inner_shutdown = CancellationToken::new();
            let inner_for_join = inner_shutdown.clone();
            let outer_for_inner = gw_outer_shutdown.clone();

            // Bridge outer → inner so a global cancel halts the gateway.
            tokio::spawn(async move {
                outer_for_inner.cancelled().await;
                inner_for_join.cancel();
            });

            let handle = spawn_gateway(&gw_config, inner_shutdown.clone(), Some(gw_poke_tx.clone()));
            let result = handle.await;
            if gw_outer_shutdown.is_cancelled() {
                return;
            }
            crash_count += 1;
            tracing::warn!("Gateway exited unexpectedly: {result:?}; respawning ({crash_count})");
            if crash_count >= GATEWAY_MAX_CRASH_RESPAWNS {
                tracing::error!("Gateway crashed {crash_count} times, abandoning respawn");
                return;
            }
            let delay = GATEWAY_RESPAWN_BASE_DELAY * (1 << crash_count.min(4));
            tokio::time::sleep(delay).await;
        }
    });

    // iMessage monitor (macOS only, optional).
    #[cfg(target_os = "macos")]
    {
        let im_shutdown = shutdown.clone();
        let im_config = config.clone();
        tokio::spawn(async move {
            let imessage_dir = match Config::data_dir() {
                Ok(d) => d.join("channels/imessage"),
                Err(e) => {
                    tracing::warn!("iMessage: data_dir failed: {e}");
                    return;
                }
            };
            if !imessage_dir.join("channel.toml").exists() {
                return;
            }
            let probe = borg_gateway::imessage::probe::probe_imessage();
            match probe.status {
                borg_gateway::imessage::probe::ProbeStatus::Ok => {
                    if let Err(e) =
                        borg_gateway::imessage::start_imessage_monitor(im_config, im_shutdown).await
                    {
                        tracing::warn!("iMessage monitor failed: {e}");
                    }
                }
                borg_gateway::imessage::probe::ProbeStatus::NoDiskAccess => {
                    tracing::warn!("iMessage: Full Disk Access required. Skipping monitor.");
                }
                other => {
                    tracing::warn!("iMessage probe: {other}. Skipping monitor.");
                }
            }
        });
    }

    // Scheduled-task dispatcher.
    let semaphore = Arc::new(tokio::sync::Semaphore::new(config.tasks.max_concurrent));
    let scheduler_config = config;
    let scheduler_shutdown = shutdown;
    let scheduler_semaphore = semaphore;
    tokio::spawn(async move {
        let hooks = scheduler::DispatcherHooks {
            spawn_task: Arc::new(|task, run_id, retry, sem, cfg| {
                Box::pin(async move {
                    task_runner::spawn_task_execution(task, run_id, retry, sem, cfg).await
                })
            }),
            spawn_workflow_step: Arc::new(|wf, step, prior, all, sem, cfg| {
                Box::pin(async move {
                    task_runner::spawn_workflow_step(wf, step, prior, all, sem, cfg).await
                })
            }),
            deliver_celebration: Arc::new(|cfg, c| {
                Box::pin(async move { task_runner::deliver_celebration_to_channels(cfg, c).await })
            }),
        };
        if let Err(e) = scheduler::run(scheduler_config, hooks, scheduler_semaphore, scheduler_shutdown).await {
            tracing::error!("Scheduled-task dispatcher exited: {e}");
        }
    });

    OrchestratorHandles {
        heartbeat_bus,
        poke_tx,
        executor,
    }
}

/// Convenience: drive the orchestrator from a `CancellationToken` until it's
/// cancelled. Used by the cli's `service::run_daemon` shim. Caller is
/// responsible for any pre-flight setup (auto-spawning a borgd binary etc.).
pub async fn run_lifecycle(shutdown: CancellationToken) -> Result<()> {
    let config = Config::load_from_db()?;
    borg_core::activity_log::log_activity(
        &borg_core::db::Database::open()?,
        "info",
        "system",
        "Daemon orchestrator started (cli shim)",
    );
    let registry = SessionRegistry::new();
    let _handles = start(registry, config, shutdown.clone());
    shutdown.cancelled().await;
    Ok(())
}
