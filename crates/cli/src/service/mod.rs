use anyhow::Result;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use borg_core::agent::{Agent, AgentEvent};
use borg_core::config::Config;
use borg_core::constants::{
    DAEMON_LOCK_MAX_REFRESH_FAILURES, DAEMON_LOOP_INTERVAL, GATEWAY_MAX_CRASH_RESPAWNS,
    GATEWAY_RESPAWN_BASE_DELAY, SLEEP_DRIFT_THRESHOLD, STALLED_TASK_GRACE_SECS,
    STALLED_TASK_SCAN_INTERVAL, WATCHDOG_STALL_THRESHOLD, WATCHDOG_TICK_INTERVAL,
};
use borg_heartbeat::scheduler::{HeartbeatEvent, HeartbeatResult, HeartbeatScheduler, SkipReason};

mod install;

pub use install::{
    ensure_service_running, kill_other_borg_processes, restart_service, service_status,
    stop_service, uninstall_service,
};

/// Spawn the gateway server as a tokio task. Returns the join handle.
fn spawn_daemon_gateway(
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

/// Run the daemon loop: executes scheduled tasks and heartbeat without a TUI.
pub async fn run_daemon(shutdown: CancellationToken) -> Result<()> {
    let config = Config::load_from_db()?;

    println!("Borg daemon starting...");

    // Open database for task scheduling
    let db = borg_core::db::Database::open()?;

    // Acquire singleton daemon lock
    let daemon_pid = std::process::id();
    let now = chrono::Utc::now().timestamp();
    if !db.acquire_daemon_lock(daemon_pid, now)? {
        anyhow::bail!(
            "Another daemon instance is already running. Only one daemon can run at a time."
        );
    }

    // Prune activity log entries older than 30 days
    match db.prune_activity_before(chrono::Utc::now().timestamp() - 30 * 86400) {
        Ok(count) if count > 0 => {
            tracing::debug!("Pruned {count} old activity log entries");
        }
        Err(e) => tracing::debug!("Failed to prune activity log: {e}"),
        _ => {}
    }

    // Recover any stale 'running' task_runs from a previous crashed daemon
    match db.recover_stale_runs("Daemon crashed during execution") {
        Ok(count) if count > 0 => {
            tracing::warn!("Recovered {count} stale task run(s) from previous daemon crash");
        }
        Err(e) => tracing::warn!("Failed to recover stale runs: {e}"),
        _ => {}
    }

    // Recover stale workflow steps left running after a crash
    match db.recover_stale_workflow_steps() {
        Ok(count) if count > 0 => {
            tracing::warn!("Recovered {count} stale workflow step(s) from previous daemon crash");
        }
        Err(e) => tracing::warn!("Failed to recover stale workflow steps: {e}"),
        _ => {}
    }

    borg_core::activity_log::log_activity(&db, "info", "system", "Daemon started");

    // Validate that LLM client can be constructed
    let _ = borg_core::llm::LlmClient::new(&config)?;

    let max_concurrent = config.tasks.max_concurrent;
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_concurrent));

    // Set up heartbeat scheduler (always on — heartbeat is a core feature)
    let (hb_tx, mut hb_rx) = mpsc::channel::<HeartbeatEvent>(32);
    let (poke_tx, poke_rx) = mpsc::channel::<()>(8);

    let tz = config.user_timezone();
    let scheduler = HeartbeatScheduler::new(config.heartbeat.clone(), tz, poke_rx);
    let hb_cancel = shutdown.clone();
    let hb_handle = tokio::spawn(async move {
        scheduler.run(hb_tx, hb_cancel).await;
    });
    println!("Heartbeat scheduler started.");

    // Start gateway server (with poke channel for /internal/poke endpoint).
    // The gateway gets its own CancellationToken so /internal/restart can stop
    // just the gateway without killing the whole daemon.
    let gw_shutdown = CancellationToken::new();
    let gw_poke_tx = poke_tx.clone();
    let gw_handle = spawn_daemon_gateway(&config, gw_shutdown.clone(), Some(gw_poke_tx));
    let mut gw_shutdown = gw_shutdown;
    println!("Gateway server started.");

    // Start native iMessage monitor if channel is installed (macOS only)
    #[cfg(target_os = "macos")]
    let im_handle: Option<tokio::task::JoinHandle<()>> = {
        let imessage_dir = Config::data_dir()?.join("channels/imessage");
        if imessage_dir.join("channel.toml").exists() {
            let probe = borg_gateway::imessage::probe::probe_imessage();
            match probe.status {
                borg_gateway::imessage::probe::ProbeStatus::Ok => {
                    let im_config = config.clone();
                    let im_shutdown = shutdown.clone();
                    Some(tokio::spawn(async move {
                        match borg_gateway::imessage::start_imessage_monitor(im_config, im_shutdown)
                            .await
                        {
                            Ok(_handle) => tracing::info!("iMessage monitor started"),
                            Err(e) => tracing::warn!("iMessage monitor failed: {e}"),
                        }
                    }))
                }
                borg_gateway::imessage::probe::ProbeStatus::NoDiskAccess => {
                    tracing::warn!("iMessage: Full Disk Access required (System Settings > Privacy & Security). Skipping monitor.");
                    None
                }
                other => {
                    tracing::warn!("iMessage probe: {other}. Skipping monitor.");
                    None
                }
            }
        } else {
            None
        }
    };
    #[cfg(not(target_os = "macos"))]
    let im_handle: Option<tokio::task::JoinHandle<()>> = None;

    println!("Daemon running. Press Ctrl+C to stop.");

    // Missed job catch-up: skip tasks overdue by more than 7 days
    skip_stale_tasks(&db);

    let mut interval = tokio::time::interval(DAEMON_LOOP_INTERVAL);

    // Pin subsystem handles for monitoring in select! loop
    tokio::pin!(hb_handle);
    let mut gw_handle = Box::pin(gw_handle);

    // Wrap optional iMessage handle into a future that pends forever if absent
    let im_fut = async {
        match im_handle {
            Some(handle) => handle.await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(im_fut);

    let mut lock_refresh_failures: u32 = 0;

    // Watchdog: detect main loop deadlocks
    let watchdog_ts = Arc::new(AtomicI64::new(chrono::Utc::now().timestamp()));
    {
        let ts = watchdog_ts.clone();
        let wd_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut wd_interval = tokio::time::interval(WATCHDOG_TICK_INTERVAL);
            loop {
                tokio::select! {
                    biased;
                    _ = wd_shutdown.cancelled() => return,
                    _ = wd_interval.tick() => {}
                }
                let last = ts.load(Ordering::Relaxed);
                let now = chrono::Utc::now().timestamp();
                if now - last > WATCHDOG_STALL_THRESHOLD.as_secs() as i64 {
                    tracing::error!(
                        "Daemon watchdog: main loop stale for {}s, exiting",
                        now - last
                    );
                    std::process::exit(1);
                }
            }
        });
    }

    // Track wall-clock time for drift detection after sleep/wake
    let mut last_tick_wall = std::time::Instant::now();
    let mut gw_crash_count: u32 = 0;

    // Self-healing: scan for scheduled tasks whose next_run silently drifted
    // into the past. Runs on a slower cadence than the main loop to keep the
    // DB pressure low.
    let mut last_stalled_scan = std::time::Instant::now();

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                println!("Daemon shutting down gracefully...");
                gw_shutdown.cancel(); // stop the gateway too
                for _ in 0..max_concurrent {
                    let _ = semaphore.acquire().await;
                }
                let _ = db.release_daemon_lock(daemon_pid);
                println!("All tasks drained. Goodbye.");
                return Ok(());
            }
            result = &mut hb_handle => {
                tracing::error!("Heartbeat scheduler exited unexpectedly: {result:?}");
                borg_core::activity_log::log_activity(&db, "error", "system", "Heartbeat scheduler crashed, daemon restarting");
                shutdown.cancel();
                continue; // graceful shutdown branch will handle drain
            }
            result = &mut gw_handle => {
                // Gateway exited — either a restart was requested or it crashed.
                // Either way, reload config and respawn instead of killing the daemon.
                let is_restart = gw_shutdown.is_cancelled();
                if is_restart {
                    gw_crash_count = 0;
                    tracing::info!("Gateway restart requested, respawning with fresh config");
                    borg_core::activity_log::log_activity(&db, "info", "system", "Gateway restarting (requested)");
                } else {
                    gw_crash_count += 1;
                    tracing::error!("Gateway server exited unexpectedly: {result:?}");
                    borg_core::activity_log::log_activity(&db, "error", "system", "Gateway crashed, respawning");
                    if gw_crash_count >= GATEWAY_MAX_CRASH_RESPAWNS {
                        tracing::error!("Gateway crashed {gw_crash_count} times in rapid succession, stopping respawn");
                        borg_core::activity_log::log_activity(&db, "error", "system", "Gateway respawn abandoned after repeated crashes");
                        continue;
                    }
                }

                // Delay to let the port be released (longer backoff on crashes)
                let base = GATEWAY_RESPAWN_BASE_DELAY;
                let delay = if is_restart { base } else { base * (1 << gw_crash_count.min(4)) };
                tokio::time::sleep(delay).await;

                let new_shutdown = CancellationToken::new();
                gw_handle = Box::pin(spawn_daemon_gateway(
                    &Config::load_from_db().unwrap_or_else(|_| config.clone()),
                    new_shutdown.clone(),
                    Some(poke_tx.clone()),
                ));
                gw_shutdown = new_shutdown;
                continue;
            }
            result = &mut im_fut => {
                tracing::error!("iMessage monitor exited unexpectedly: {result:?}");
                borg_core::activity_log::log_activity(&db, "error", "system", "iMessage monitor crashed, daemon restarting");
                shutdown.cancel();
                continue;
            }
            Some(HeartbeatEvent::Fire) = hb_rx.recv() => {
                let hb_config = config.clone();
                tokio::spawn(async move {
                    daemon_heartbeat_turn(hb_config).await;
                });
                continue;
            }
            _ = interval.tick() => {}
        }

        // Update watchdog timestamp
        watchdog_ts.store(chrono::Utc::now().timestamp(), Ordering::Relaxed);

        // Detect sleep/wake drift: if wall-clock elapsed far exceeds the loop interval, we likely resumed from sleep.
        let elapsed = last_tick_wall.elapsed();
        last_tick_wall = std::time::Instant::now();
        if elapsed > SLEEP_DRIFT_THRESHOLD {
            tracing::info!(
                "Daemon resumed after {}s pause (likely sleep/wake), re-checking stale tasks",
                elapsed.as_secs()
            );
            skip_stale_tasks(&db);
        }

        let now = chrono::Utc::now().timestamp();

        // Refresh daemon lock heartbeat — exit after consecutive failures (lock stolen)
        match db.refresh_daemon_lock(daemon_pid, now) {
            Ok(()) => {
                lock_refresh_failures = 0;
            }
            Err(e) => {
                lock_refresh_failures += 1;
                tracing::warn!(
                    "Failed to refresh daemon lock ({lock_refresh_failures}/{DAEMON_LOCK_MAX_REFRESH_FAILURES}): {e}"
                );
                if lock_refresh_failures >= DAEMON_LOCK_MAX_REFRESH_FAILURES {
                    tracing::error!(
                        "Daemon lock lost after {DAEMON_LOCK_MAX_REFRESH_FAILURES} consecutive failures, exiting"
                    );
                    borg_core::activity_log::log_activity(
                        &db,
                        "error",
                        "system",
                        "Daemon lock lost, restarting",
                    );
                    shutdown.cancel();
                    continue;
                }
            }
        }

        // Self-healing scan for stalled scheduled tasks. Runs on a slower
        // cadence than the main tick to keep DB pressure low. A task is
        // "stalled" if status=active, retry_after is null, and next_run
        // drifted more than STALLED_TASK_GRACE_SECS into the past.
        if last_stalled_scan.elapsed() >= STALLED_TASK_SCAN_INTERVAL {
            last_stalled_scan = std::time::Instant::now();
            match borg_core::tasks::heal_stalled_tasks(&db, now, STALLED_TASK_GRACE_SECS) {
                Ok(report) if report.detected > 0 => {
                    tracing::warn!(
                        detected = report.detected,
                        reset = report.reset,
                        "self-healing: reset next_run for stalled scheduled tasks"
                    );
                    borg_core::activity_log::log_activity(
                        &db,
                        "warn",
                        "system",
                        &format!(
                            "Self-healing reset {} stalled scheduled task(s) (detected {})",
                            report.reset, report.detected
                        ),
                    );
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("Stalled-task scan failed: {e}"),
            }
        }

        // Process due tasks (atomic claim: advances next_run + creates running task_run in one transaction)
        match db.claim_due_tasks(now) {
            Ok(claimed) => {
                for ct in claimed {
                    spawn_task_execution(
                        &ct.task,
                        ct.run_id,
                        false,
                        semaphore.clone(),
                        config.clone(),
                    )
                    .await;
                }
            }
            Err(e) => tracing::warn!("Failed to claim due tasks: {e}"),
        }

        // Process tasks pending retry
        match db.get_tasks_pending_retry(now) {
            Ok(retries) => {
                for task in retries {
                    let run_id = match db.start_task_run(&task.id, now) {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::warn!("Failed to start task run for '{}': {e}", task.name);
                            continue;
                        }
                    };
                    spawn_task_execution(&task, run_id, true, semaphore.clone(), config.clone())
                        .await;
                }
            }
            Err(e) => tracing::warn!("Failed to check retry tasks: {e}"),
        }

        // Process workflow steps (one step per workflow per tick)
        match db.get_runnable_workflows() {
            Ok(workflows) => {
                for wf in workflows {
                    if let Ok(Some(step)) = db.claim_next_workflow_step(&wf.id) {
                        let prior = db.get_completed_workflow_steps(&wf.id).unwrap_or_default();
                        let all_steps = db.get_workflow_steps(&wf.id).unwrap_or_default();
                        spawn_workflow_step(
                            wf,
                            step,
                            prior,
                            all_steps,
                            semaphore.clone(),
                            config.clone(),
                        )
                        .await;
                    }
                }
            }
            Err(e) => tracing::warn!("Failed to check runnable workflows: {e}"),
        }

        // Deliver pending evolution celebrations to configured channels.
        // Claim atomically to prevent duplicate delivery across ticks.
        match db.claim_pending_celebrations() {
            Ok(celebrations) => {
                for c in celebrations {
                    let celebration_id = c.id;
                    let config_clone = config.clone();
                    tokio::spawn(async move {
                        let delivered = deliver_celebration_to_channels(&config_clone, &c).await;
                        match borg_core::db::Database::open() {
                            Ok(cdb) => {
                                if delivered {
                                    if let Err(e) = cdb.mark_celebration_delivered(celebration_id) {
                                        tracing::warn!(
                                            "Failed to mark celebration {celebration_id} delivered: {e}"
                                        );
                                    }
                                } else {
                                    // Release back to pending for retry on next tick
                                    if let Err(e) = cdb.unclaim_celebration(celebration_id) {
                                        tracing::warn!(
                                            "Failed to unclaim celebration {celebration_id}: {e}"
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failed to open DB after celebration delivery: {e}");
                            }
                        }
                    });
                }
            }
            Err(e) => tracing::warn!("Failed to check pending celebrations: {e}"),
        }
    }
}

/// Spawn a workflow step execution in a background task.
async fn spawn_workflow_step(
    workflow: borg_core::db::WorkflowRow,
    step: borg_core::db::WorkflowStepRow,
    prior_steps: Vec<borg_core::db::WorkflowStepRow>,
    all_steps: Vec<borg_core::db::WorkflowStepRow>,
    semaphore: std::sync::Arc<tokio::sync::Semaphore>,
    config: Config,
) {
    let permit = semaphore.acquire_owned().await;
    let step_id = step.id;
    let step_title = step.title.clone();
    let workflow_id = workflow.id.clone();
    let workflow_title = workflow.title.clone();
    let step_timeout = std::time::Duration::from_millis(step.timeout_ms as u64);

    tokio::spawn(async move {
        let _permit = permit;
        tracing::info!(
            "Executing workflow step: {} / {} (wf: {})",
            step.step_index + 1,
            all_steps.len(),
            workflow_title,
        );

        // Open DB once for all state transitions in this step execution
        let db = match borg_core::db::Database::open() {
            Ok(db) => db,
            Err(e) => {
                tracing::error!("Failed to open DB for workflow step: {e}");
                return;
            }
        };

        // Build the step context
        let context = borg_core::workflow::engine::build_step_context_with_remaining(
            &workflow,
            &step,
            &prior_steps,
            &all_steps,
        );

        // Execute via LLM (same pattern as execute_prompt_task)
        let identity = borg_core::identity::load_identity().unwrap_or_default();
        let memory = borg_core::memory::load_memory_context_db(4000).unwrap_or_default();
        let time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z");

        let system = format!("{identity}\n\n# Current Time\n{time}\n\n{memory}\n\n{context}");

        let messages = vec![
            borg_core::types::Message::system(system),
            borg_core::types::Message::user(&step.instructions),
        ];

        let llm = match borg_core::llm::LlmClient::new(&config) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(
                    "Failed to create LLM client for workflow step '{}': {e}",
                    step_title
                );
                let _ = db.fail_workflow_step(step_id, &format!("{e}"));
                return;
            }
        };

        let result = tokio::time::timeout(step_timeout, llm.chat(&messages, None)).await;

        let step_label = format!("{}/{}", step.step_index + 1, all_steps.len());

        match result {
            Ok(Ok(response)) => {
                let result_text = response.text_content().unwrap_or("").to_string();
                tracing::info!(
                    "Workflow step completed: {step_label} in '{workflow_title}' — {}",
                    &result_text[..result_text.len().min(100)],
                );
                if let Err(e) = db.complete_workflow_step(step_id, &result_text) {
                    tracing::warn!("Failed to complete workflow step: {e}");
                }
                borg_core::activity_log::log_activity(
                    &db,
                    "info",
                    "workflow",
                    &format!("Step {step_label} completed in workflow '{workflow_title}'"),
                );

                // Check if workflow is now complete
                if let Ok(Some(wf)) = db.get_workflow(&workflow_id) {
                    if wf.status == borg_core::workflow::status::COMPLETED {
                        tracing::info!("Workflow '{workflow_title}' completed!");
                        borg_core::activity_log::log_activity(
                            &db,
                            "info",
                            "workflow",
                            &format!("Workflow '{workflow_title}' completed"),
                        );
                        // Deliver final result if configured
                        if let (Some(ch), Some(tgt)) = (wf.delivery_channel, wf.delivery_target) {
                            let summary = build_workflow_summary(&db, &wf.id);
                            deliver_task_result(&ch, &tgt, &workflow_title, &summary, &config)
                                .await;
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                let error_msg = format!("{e}");
                tracing::warn!(
                    "Workflow step failed: {step_label} in '{workflow_title}': {error_msg}",
                );
                let exhausted = db.fail_workflow_step(step_id, &error_msg).unwrap_or(false);
                if exhausted {
                    tracing::warn!("Workflow '{workflow_title}' failed: {error_msg}");
                    borg_core::activity_log::log_activity(
                        &db,
                        "error",
                        "workflow",
                        &format!("Workflow '{workflow_title}' failed: {error_msg}"),
                    );
                }
            }
            Err(_) => {
                let error_msg = format!("Step timed out after {}ms", step_timeout.as_millis());
                tracing::warn!("Workflow step timed out: {step_label} in '{workflow_title}'",);
                let exhausted = db.fail_workflow_step(step_id, &error_msg).unwrap_or(false);
                if exhausted {
                    borg_core::activity_log::log_activity(
                        &db,
                        "error",
                        "workflow",
                        &format!("Workflow '{workflow_title}' failed: {error_msg}"),
                    );
                }
            }
        }
    });
}

/// Build a summary of all completed workflow step outputs for delivery.
fn build_workflow_summary(db: &borg_core::db::Database, workflow_id: &str) -> String {
    let steps = db
        .get_completed_workflow_steps(workflow_id)
        .unwrap_or_default();
    let mut summary = String::new();
    for s in &steps {
        summary.push_str(&format!("**Step {}: {}**\n", s.step_index + 1, s.title));
        if let Some(ref output) = s.output {
            summary.push_str(output);
            summary.push_str("\n\n");
        }
    }
    summary
}

/// Skip tasks overdue by more than 7 days, advancing them to their next run.
fn skip_stale_tasks(db: &borg_core::db::Database) {
    let now = chrono::Utc::now().timestamp();
    if let Ok(stale_tasks) = db.get_due_tasks(now) {
        for task in &stale_tasks {
            let age = now - task.next_run.unwrap_or(0);
            if age > 7 * 86400 {
                tracing::warn!(
                    "Skipping stale task '{}' ({}d overdue), advancing to next run",
                    task.name,
                    age / 86400
                );
                let _ = borg_core::tasks::advance_next_run(task, db);
            }
        }
    }
}

async fn spawn_task_execution(
    task: &borg_core::db::ScheduledTaskRow,
    run_id: i64,
    is_retry: bool,
    semaphore: std::sync::Arc<tokio::sync::Semaphore>,
    config: Config,
) {
    let permit = semaphore.acquire_owned().await;
    let task_name = task.name.clone();
    let task_id = task.id.clone();
    let task_prompt = match task.id.as_str() {
        borg_core::daily_summary::DAILY_SUMMARY_TASK_ID => {
            match borg_core::daily_summary::build_daily_summary_prompt() {
                Ok(enriched) => enriched,
                Err(e) => {
                    tracing::warn!("Daily summary data gathering failed: {e}");
                    task.prompt.clone()
                }
            }
        }
        _ => task.prompt.clone(),
    };
    let task_timeout = std::time::Duration::from_millis(task.timeout_ms as u64);
    let max_retries = task.max_retries;
    let retry_count = task.retry_count;
    let delivery_channel = task.delivery_channel.clone();
    let delivery_target = task.delivery_target.clone();
    let schedule_type = task.schedule_type.clone();
    let task_type = task.task_type.clone();

    tokio::spawn(async move {
        let _permit = permit;
        let attempt_label = if is_retry {
            format!(" (retry {retry_count})")
        } else {
            String::new()
        };
        tracing::info!("Executing scheduled task: {task_name} ({task_id}){attempt_label}");
        if let Ok(adb) = borg_core::db::Database::open() {
            borg_core::activity_log::log_activity(
                &adb,
                "info",
                "task",
                &format!("Task '{task_name}' started{attempt_label}"),
            );
        }
        let started_at = chrono::Utc::now().timestamp();

        let exec_ctx = TaskExecContext {
            task_name: task_name.clone(),
            task_id,
            prompt_or_command: task_prompt,
            timeout: task_timeout,
            run_id,
            started_at,
            retry_count,
            max_retries,
            schedule_type,
            delivery_channel,
            delivery_target,
            config,
        };

        let result = std::panic::AssertUnwindSafe(async {
            if task_type == "command" {
                execute_command_task(&exec_ctx).await;
            } else if task_type == "maintenance" {
                execute_maintenance_task(&exec_ctx).await;
            } else {
                execute_prompt_task(&exec_ctx).await;
            }
        });

        if let Err(panic_err) = futures::FutureExt::catch_unwind(result).await {
            let panic_msg = panic_err
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic_err.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic");
            tracing::error!("Task '{task_name}' panicked: {panic_msg}");
            if let Ok(db) = borg_core::db::Database::open() {
                let duration_ms = (chrono::Utc::now().timestamp() - started_at) * 1000;
                let _ = db.complete_task_run(
                    run_id,
                    duration_ms,
                    None,
                    Some(&format!("panic: {panic_msg}")),
                );
                borg_core::activity_log::log_activity(
                    &db,
                    "error",
                    "task",
                    &format!("Task '{task_name}' panicked: {panic_msg}"),
                );
            }
        }
    });
}

struct TaskExecContext {
    task_name: String,
    task_id: String,
    prompt_or_command: String,
    timeout: std::time::Duration,
    run_id: i64,
    started_at: i64,
    retry_count: i32,
    max_retries: i32,
    schedule_type: String,
    delivery_channel: Option<String>,
    delivery_target: Option<String>,
    config: Config,
}

/// Run the daily self-healing maintenance sweep. No LLM call — executes
/// `run_daily_maintenance` directly and records the result against the
/// `task_runs` row opened by `claim_due_tasks`.
async fn execute_maintenance_task(ctx: &TaskExecContext) {
    let TaskExecContext {
        task_name,
        task_id,
        run_id,
        started_at,
        config,
        ..
    } = ctx;

    // `spawn_blocking` because the maintenance sweep does synchronous
    // sqlite work and filesystem walks. Keeps the async runtime free to
    // handle heartbeat and gateway events.
    let config_clone = config.clone();
    let result = tokio::task::spawn_blocking(move || {
        let db = borg_core::db::Database::open()?;
        borg_core::maintenance::run_daily_maintenance(&db, &config_clone)
    })
    .await;

    let duration_ms = (chrono::Utc::now().timestamp() - *started_at) * 1000;
    let (status, summary, error_msg) = match result {
        Ok(Ok(report)) => {
            let summary = report.activity_line();
            tracing::info!("Maintenance task '{task_name}' completed: {summary}");
            ("info", Some(summary), None)
        }
        Ok(Err(e)) => {
            let msg = format!("maintenance sweep failed: {e}");
            tracing::warn!("Maintenance task '{task_name}' failed: {e}");
            ("error", None, Some(msg))
        }
        Err(join_err) => {
            let msg = format!("maintenance task panicked: {join_err}");
            tracing::error!("{msg}");
            ("error", None, Some(msg))
        }
    };

    match borg_core::db::Database::open() {
        Ok(db) => {
            if let Err(e) = db.complete_task_run(
                *run_id,
                duration_ms,
                summary.as_deref(),
                error_msg.as_deref(),
            ) {
                tracing::warn!("maintenance: failed to finalize task_runs row {run_id}: {e}");
            }
            if error_msg.is_none() {
                if let Err(e) = db.clear_task_retry(task_id) {
                    tracing::warn!("maintenance: failed to clear retry for {task_id}: {e}");
                }
            }
            let line = summary.as_deref().or(error_msg.as_deref()).unwrap_or("");
            borg_core::activity_log::log_activity(&db, status, "task", line);
        }
        Err(e) => {
            tracing::warn!("maintenance: could not reopen db to record task_runs completion: {e}");
        }
    }
}

async fn execute_command_task(ctx: &TaskExecContext) {
    let TaskExecContext {
        task_name,
        task_id,
        prompt_or_command: command,
        timeout,
        run_id,
        started_at,
        retry_count,
        max_retries,
        schedule_type,
        delivery_channel,
        delivery_target,
        config,
    } = ctx;
    let result = tokio::time::timeout(
        *timeout,
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command.as_str())
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let duration_ms = (chrono::Utc::now().timestamp() - *started_at) * 1000;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let combined = if stderr.is_empty() {
                stdout.to_string()
            } else {
                format!("{stdout}\n[stderr]\n{stderr}")
            };

            if output.status.success() {
                if let Ok(db) = borg_core::db::Database::open() {
                    let _ = db.complete_task_run(*run_id, duration_ms, Some(&combined), None);
                    let _ = db.clear_task_retry(task_id);
                    borg_core::activity_log::log_activity(
                        &db,
                        "info",
                        "task",
                        &format!("Task '{task_name}' completed"),
                    );
                }
                tracing::info!(
                    "Cron job '{}' completed (exit 0): {}",
                    task_name,
                    &combined[..combined.len().min(100)]
                );
                if let (Some(ch), Some(tgt)) = (delivery_channel, delivery_target) {
                    deliver_task_result(ch, tgt, task_name, &combined, config).await;
                }
            } else {
                let exit_code = output.status.code().unwrap_or(-1);
                let error_msg = format!("exit code {exit_code}");
                if let Ok(db) = borg_core::db::Database::open() {
                    let _ = db.complete_task_run(
                        *run_id,
                        duration_ms,
                        Some(&combined),
                        Some(&error_msg),
                    );
                }
                tracing::warn!("Cron job '{task_name}' failed ({error_msg})");
                if let Ok(db) = borg_core::db::Database::open() {
                    borg_core::activity_log::log_activity(
                        &db,
                        "warn",
                        "task",
                        &format!("Task '{task_name}' failed: {error_msg}"),
                    );
                }
                // Non-zero exit is not retried (it's a user script error, not transient)
                if let (Some(ch), Some(tgt)) = (delivery_channel, delivery_target) {
                    deliver_task_result(
                        ch,
                        tgt,
                        &format!("{task_name} [FAILED]"),
                        &format!("Error: {error_msg}\n{combined}"),
                        config,
                    )
                    .await;
                }
            }
        }
        Ok(Err(e)) => {
            let duration_ms = (chrono::Utc::now().timestamp() - *started_at) * 1000;
            let fail_ctx = TaskFailureContext {
                task_id,
                task_name,
                run_id: *run_id,
                duration_ms,
                retry_count: *retry_count,
                max_retries: *max_retries,
                schedule_type,
                delivery_channel,
                delivery_target,
                config,
            };
            handle_task_failure(&fail_ctx, &format!("Failed to spawn command: {e}")).await;
        }
        Err(_) => {
            let duration_ms = (chrono::Utc::now().timestamp() - *started_at) * 1000;
            let fail_ctx = TaskFailureContext {
                task_id,
                task_name,
                run_id: *run_id,
                duration_ms,
                retry_count: *retry_count,
                max_retries: *max_retries,
                schedule_type,
                delivery_channel,
                delivery_target,
                config,
            };
            handle_task_failure(&fail_ctx, "Cron job timed out").await;
        }
    }
}

async fn execute_prompt_task(ctx: &TaskExecContext) {
    let TaskExecContext {
        task_name,
        task_id,
        prompt_or_command: task_prompt,
        timeout,
        run_id,
        started_at,
        retry_count,
        max_retries,
        schedule_type,
        delivery_channel,
        delivery_target,
        config,
    } = ctx;
    let identity = borg_core::identity::load_identity().unwrap_or_default();
    let memory = borg_core::memory::load_memory_context_db(4000).unwrap_or_default();
    let time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z");

    let system = format!(
        "{identity}\n\n# Current Time\n{time}\n\n{memory}\n\n\
         # Scheduled Task\nYou are executing a scheduled task: \"{task_name}\"\n\
         Respond with the task result. Be concise."
    );

    let messages = vec![
        borg_core::types::Message::system(system),
        borg_core::types::Message::user(task_prompt),
    ];

    let llm = match borg_core::llm::LlmClient::new(config) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("Failed to create LLM client for task '{task_name}': {e}");
            if let Ok(db) = borg_core::db::Database::open() {
                let _ = db.complete_task_run(*run_id, 0, None, Some(&format!("{e}")));
            }
            return;
        }
    };
    let result = tokio::time::timeout(*timeout, llm.chat(&messages, None)).await;

    match result {
        Ok(Ok(response)) => {
            let duration_ms = (chrono::Utc::now().timestamp() - *started_at) * 1000;
            let result_text = response.text_content().unwrap_or("");
            if let Ok(db) = borg_core::db::Database::open() {
                if let Err(e) = db.complete_task_run(*run_id, duration_ms, Some(result_text), None)
                {
                    tracing::warn!("Failed to complete task run for '{task_name}': {e}");
                }
                if let Err(e) = db.clear_task_retry(task_id) {
                    tracing::warn!("Failed to clear retry state for '{task_name}': {e}");
                }
                borg_core::activity_log::log_activity(
                    &db,
                    "info",
                    "task",
                    &format!("Task '{task_name}' completed"),
                );
            }
            tracing::info!(
                "Task '{}' completed: {}",
                task_name,
                &result_text[..result_text.len().min(100)]
            );
            if let (Some(ch), Some(tgt)) = (delivery_channel, delivery_target) {
                deliver_task_result(ch, tgt, task_name, result_text, config).await;
            }
        }
        Ok(Err(e)) => {
            let duration_ms = (chrono::Utc::now().timestamp() - *started_at) * 1000;
            let fail_ctx = TaskFailureContext {
                task_id,
                task_name,
                run_id: *run_id,
                duration_ms,
                retry_count: *retry_count,
                max_retries: *max_retries,
                schedule_type,
                delivery_channel,
                delivery_target,
                config,
            };
            handle_task_failure(&fail_ctx, &format!("{e}")).await;
        }
        Err(_) => {
            let duration_ms = (chrono::Utc::now().timestamp() - *started_at) * 1000;
            let fail_ctx = TaskFailureContext {
                task_id,
                task_name,
                run_id: *run_id,
                duration_ms,
                retry_count: *retry_count,
                max_retries: *max_retries,
                schedule_type,
                delivery_channel,
                delivery_target,
                config,
            };
            handle_task_failure(&fail_ctx, "Task timed out").await;
        }
    }
}

struct TaskFailureContext<'a> {
    task_id: &'a str,
    task_name: &'a str,
    run_id: i64,
    duration_ms: i64,
    retry_count: i32,
    max_retries: i32,
    schedule_type: &'a str,
    delivery_channel: &'a Option<String>,
    delivery_target: &'a Option<String>,
    config: &'a Config,
}

async fn handle_task_failure(ctx: &TaskFailureContext<'_>, error: &str) {
    let TaskFailureContext {
        task_id,
        task_name,
        run_id,
        duration_ms,
        retry_count,
        max_retries,
        schedule_type,
        delivery_channel,
        delivery_target,
        config,
    } = ctx;

    // Open DB for failure handling (Connection is not Send, so we can't share across spawn)
    if let Ok(db) = borg_core::db::Database::open() {
        // Always mark the run as failed
        if let Err(e) = db.complete_task_run(*run_id, *duration_ms, None, Some(error)) {
            tracing::warn!("Failed to complete task run for '{task_name}': {e}");
        }

        if borg_core::tasks::is_transient_error(error) && *retry_count < *max_retries {
            let next_attempt = *retry_count + 1;
            let delay = borg_core::tasks::retry_delay_secs(next_attempt);
            let retry_at = chrono::Utc::now().timestamp() + delay;
            if let Err(e) = db.set_task_retry(task_id, next_attempt, error, retry_at) {
                tracing::warn!("Failed to set retry state for '{task_name}': {e}");
            }
            tracing::warn!(
                "Task '{task_name}' failed (transient), retry {next_attempt}/{max_retries} in {delay}s: {error}"
            );
            return;
        }

        if let Err(e) = db.clear_task_retry(task_id) {
            tracing::warn!("Failed to clear retry state for '{task_name}': {e}");
        }
        if *schedule_type == "once" {
            if let Err(e) = db.update_task_status(task_id, "completed") {
                tracing::warn!("Failed to mark task '{task_name}' completed: {e}");
            }
        }
    }

    tracing::warn!("Task '{task_name}' failed: {error}");
    if let Ok(adb) = borg_core::db::Database::open() {
        borg_core::activity_log::log_activity_detail(
            &adb,
            "warn",
            "task",
            &format!("Task '{task_name}' failed"),
            error,
        );
    }

    if let (Some(ch), Some(tgt)) = (delivery_channel, delivery_target) {
        let msg = format!("Error: {error}");
        deliver_task_result(ch, tgt, &format!("{task_name} [FAILED]"), &msg, config).await;
    }
}

/// A parsed task delivery target.
///
/// Legacy tasks store the target as a raw string (a Telegram chat_id, Slack
/// channel/user ID, etc.). Tasks created via `delivery_channel = "origin"`
/// store a JSON object with an explicit sender and optional thread so replies
/// can be routed back into the same thread they were spawned from.
struct DeliveryTarget {
    sender: String,
    thread_id: Option<String>,
}

fn parse_delivery_target(target: &str) -> DeliveryTarget {
    if target.starts_with('{') {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(target) {
            let sender = parsed
                .get("sender")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let thread_id = parsed
                .get("thread_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            return DeliveryTarget { sender, thread_id };
        }
    }
    DeliveryTarget {
        sender: target.to_string(),
        thread_id: None,
    }
}

/// Send a message to a specific sender on a channel, creating the appropriate client.
///
/// Supports Telegram, Slack, and Discord. For Telegram, `thread_id` maps to
/// `message_thread_id` (forum topics). For Discord, `thread_id` is used as the
/// channel_id (threads are first-class channels). For Slack, `thread_id` is passed
/// as `thread_ts`.
async fn send_to_channel(
    config: &Config,
    channel: &str,
    sender_id: &str,
    text: &str,
    thread_id: Option<&str>,
) -> anyhow::Result<()> {
    match channel {
        "telegram" => {
            let token = config
                .resolve_credential_or_env("TELEGRAM_BOT_TOKEN")
                .ok_or_else(|| anyhow::anyhow!("TELEGRAM_BOT_TOKEN not configured"))?;
            let client = borg_gateway::telegram::api::TelegramClient::new(&token)?;
            let chat_id: i64 = sender_id
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid Telegram chat_id: {sender_id}"))?;
            let message_thread_id: Option<i64> = thread_id.and_then(|s| s.parse().ok());
            client
                .send_message(chat_id, text, None, None, message_thread_id)
                .await
        }
        "slack" => {
            let token = config
                .resolve_credential_or_env("SLACK_BOT_TOKEN")
                .ok_or_else(|| anyhow::anyhow!("SLACK_BOT_TOKEN not configured"))?;
            let client = borg_gateway::slack::api::SlackClient::new(&token)?;
            client
                .post_message(sender_id, text, thread_id)
                .await
                .map(|_| ())
        }
        "discord" => {
            let token = config
                .resolve_credential_or_env("DISCORD_BOT_TOKEN")
                .ok_or_else(|| anyhow::anyhow!("DISCORD_BOT_TOKEN not configured"))?;
            let client = borg_gateway::discord::api::DiscordClient::new(&token)?;
            let channel_id = thread_id.unwrap_or(sender_id);
            client.send_message(channel_id, text).await
        }
        other => anyhow::bail!("Channel '{other}' not supported for delivery"),
    }
}

async fn deliver_task_result(
    channel: &str,
    target: &str,
    task_name: &str,
    text: &str,
    config: &Config,
) {
    if channel == "origin" {
        tracing::warn!(
            "Task '{task_name}' has unresolved 'origin' delivery — task was likely \
             created outside a gateway turn; dropping delivery"
        );
        return;
    }
    let parsed = parse_delivery_target(target);
    let msg = format!("[Task: {task_name}]\n{text}");
    if let Err(e) = send_to_channel(
        config,
        channel,
        &parsed.sender,
        &msg,
        parsed.thread_id.as_deref(),
    )
    .await
    {
        tracing::warn!("Failed to deliver task result via {channel}: {e}");
    }
}

/// Returns true if the response hash is a duplicate within the dedup time window.
pub fn is_duplicate_heartbeat(
    hash: u64,
    now_secs: u64,
    prev_hash: u64,
    prev_time: u64,
    dedup_window_secs: u64,
) -> bool {
    prev_hash == hash && prev_time > 0 && (now_secs.saturating_sub(prev_time)) < dedup_window_secs
}

/// Returns true if a heartbeat response carries no meaningful information and
/// should be suppressed before delivery. Inspired by openclaw's heartbeat
/// transcript pruning — short acknowledgments like "ok", "no updates", "all
/// good" pollute downstream channels without conveying anything the user
/// couldn't already assume from the absence of a message.
pub fn is_zero_info_heartbeat(response: &str) -> bool {
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
    // Normalize: lowercase, strip trailing punctuation, collapse whitespace.
    let normalized: String = response
        .trim()
        .trim_end_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    if normalized.is_empty() {
        return true;
    }
    // Only treat as zero-info when the message is short enough to plausibly
    // be a pure acknowledgment. Longer replies may contain real content even
    // if they start with "ok,".
    if normalized.len() > 40 {
        return false;
    }
    TRIVIAL_PATTERNS.iter().any(|pat| {
        normalized == *pat || normalized == format!("{pat}.") || normalized == format!("{pat}!")
    })
}

enum HeartbeatActivity<'a> {
    Fired { duration_ms: u64 },
    EmptyResponse { duration_ms: u64 },
    DuplicateSuppressed,
    Failed(&'a str),
}

impl HeartbeatActivity<'_> {
    fn message(&self) -> String {
        match self {
            Self::Fired { duration_ms } => format!("Heartbeat tick: fired ({duration_ms}ms)"),
            Self::EmptyResponse { duration_ms } => {
                format!("Heartbeat tick: empty response ({duration_ms}ms)")
            }
            Self::DuplicateSuppressed => "Heartbeat tick: duplicate response suppressed".into(),
            Self::Failed(error) => format!("Heartbeat tick: failed ({error})"),
        }
    }
}

/// Why a heartbeat turn is running. Drives prompt flavor: scheduled ticks use
/// the terse checklist; session-start greetings use a warmer, activity-aware
/// greeting. Scheduler poke and interval Fire events both count as Scheduled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatSource {
    /// Scheduler fire — interval, cron, or `/poke`.
    Scheduled,
    /// TUI opened post-onboarding — proactive greeting with recent-activity
    /// context and time-of-day awareness.
    SessionStart,
}

/// Shared heartbeat turn: creates a temporary agent, sends the heartbeat message
/// (with HEARTBEAT.md checklist if present), deduplicates, and returns a structured result.
pub async fn execute_heartbeat_turn(config: &Config, source: HeartbeatSource) -> HeartbeatResult {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};

    static LAST_HASH: AtomicU64 = AtomicU64::new(0);
    static LAST_HASH_TIME: AtomicU64 = AtomicU64::new(0);

    let started = std::time::Instant::now();

    let metrics = borg_core::telemetry::BorgMetrics::noop();
    // Resolve adaptive cache TTL for heartbeat sessions (longer TTL since these run on schedule).
    let mut config = config.clone();
    config.llm.cache.ttl = config.llm.cache.ttl.resolve(true);
    let mut agent = match Agent::new(config.clone(), metrics) {
        Ok(a) => a,
        Err(e) => {
            let error = format!("agent creation: {e}");
            tracing::warn!("Heartbeat: failed to create agent: {e}");
            if let Ok(adb) = borg_core::db::Database::open() {
                borg_core::activity_log::log_activity(
                    &adb,
                    "info",
                    "heartbeat",
                    &HeartbeatActivity::Failed(&error).message(),
                );
            }
            return HeartbeatResult::Failed { error };
        }
    };

    let user_msg = match source {
        HeartbeatSource::Scheduled => build_scheduled_heartbeat_prompt(&config),
        HeartbeatSource::SessionStart => build_session_start_prompt(&config),
    };

    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(256);
    let cancel = CancellationToken::new();

    if let Err(e) = agent
        .send_message_with_cancel(&user_msg, event_tx, cancel)
        .await
    {
        let error = format!("agent error: {e}");
        tracing::warn!("Heartbeat: {error}");
        if let Ok(adb) = borg_core::db::Database::open() {
            borg_core::activity_log::log_activity(
                &adb,
                "info",
                "heartbeat",
                &HeartbeatActivity::Failed(&error).message(),
            );
        }
        return HeartbeatResult::Failed { error };
    }

    let mut response = String::new();
    while let Some(event) = event_rx.recv().await {
        if let AgentEvent::TextDelta(delta) = event {
            response.push_str(&delta);
        }
    }

    let duration_ms = started.elapsed().as_millis() as u64;
    let trimmed = response.trim().to_string();

    if trimmed.is_empty() || is_zero_info_heartbeat(&trimmed) {
        tracing::debug!(
            "Heartbeat: empty or zero-info response after {duration_ms}ms ({} chars)",
            trimmed.len()
        );
        if let Ok(adb) = borg_core::db::Database::open() {
            borg_core::activity_log::log_activity(
                &adb,
                "info",
                "heartbeat",
                &HeartbeatActivity::EmptyResponse { duration_ms }.message(),
            );
        }
        return HeartbeatResult::Skipped {
            reason: SkipReason::EmptyResponse,
        };
    }

    // Dedup: skip if identical to last heartbeat response within time window.
    // Note: DefaultHasher output is not stable across Rust versions — fine for
    // in-process dedup but should not be persisted. The two atomic swaps below
    // are not jointly atomic, but the worst case is a single false positive/negative
    // which is acceptable for dedup.
    let mut hasher = DefaultHasher::new();
    trimmed.hash(&mut hasher);
    let hash = hasher.finish();
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let prev_hash = LAST_HASH.swap(hash, Ordering::Relaxed);
    let prev_time = LAST_HASH_TIME.swap(now_secs, Ordering::Relaxed);

    // Dedup window: 2x the configured interval (default 1h for 30m interval)
    let dedup_window = borg_core::tasks::parse_interval(&config.heartbeat.interval)
        .unwrap_or(std::time::Duration::from_secs(1800))
        .as_secs()
        .saturating_mul(2);

    if is_duplicate_heartbeat(hash, now_secs, prev_hash, prev_time, dedup_window) {
        tracing::debug!("Heartbeat: duplicate response within {dedup_window}s window, suppressing");
        if let Ok(adb) = borg_core::db::Database::open() {
            borg_core::activity_log::log_activity(
                &adb,
                "info",
                "heartbeat",
                &HeartbeatActivity::DuplicateSuppressed.message(),
            );
        }
        return HeartbeatResult::Skipped {
            reason: SkipReason::DuplicateResponse,
        };
    }

    if let Ok(adb) = borg_core::db::Database::open() {
        borg_core::activity_log::log_activity(
            &adb,
            "info",
            "heartbeat",
            &HeartbeatActivity::Fired { duration_ms }.message(),
        );
        // Persist last-fired timestamp so the session-start greeting throttle
        // is shared across all heartbeat sources and survives process restarts.
        if let Err(e) = adb.set_setting("heartbeat.last_fired_at", &now_secs.to_string()) {
            tracing::warn!("Heartbeat: failed to persist last_fired_at: {e}");
        }
    }

    HeartbeatResult::Ran {
        message: trimmed,
        duration_ms,
    }
}

/// Assemble the scheduled/poke heartbeat prompt: `*heartbeat tick*` plus the
/// optional `HEARTBEAT.md` checklist and any rate-limited proactive nudges.
fn build_scheduled_heartbeat_prompt(config: &Config) -> String {
    let mut user_msg = "*heartbeat tick*".to_string();
    if let Some(cl) = borg_core::memory::load_heartbeat_checklist() {
        user_msg.push_str("\n\n# Heartbeat Checklist\n");
        user_msg.push_str(&cl);
    }

    // Proactive nudges: append LLM directives for heartbeat-only conditions
    // (e.g. no messaging channels configured). Rate-limited via the `meta`
    // table so each nudge fires at most once per its declared cooldown.
    // See `crates/cli/src/heartbeat_augmenters.rs` for how to add one.
    let augmenter_db = borg_core::db::Database::open().ok();
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

/// Assemble the session-start greeting prompt. Includes the recent-activity
/// digest from `gather_daily_context`, time-of-day context, and instructions
/// that nudge the agent to greet the user like a coworker who remembers the
/// last thread. Ignores quiet hours but surfaces the hour so the agent can
/// react to unusual times ("burning the midnight oil?").
fn build_session_start_prompt(config: &Config) -> String {
    use chrono::{TimeZone, Timelike, Utc};

    let tz = config.user_timezone();
    let now_utc = Utc::now();
    let now_local = tz.from_utc_datetime(&now_utc.naive_utc());

    let hour = now_local.hour();
    let weekday = now_local.format("%A").to_string();
    let is_quiet = in_configured_quiet_hours(config, hour);

    // Recent activity + gap-since-last-session — both pulled from the same
    // database so a single failure (e.g. db locked) degrades gracefully to a
    // bare greeting rather than aborting the turn.
    let (recent_activity, hours_since_last) = match borg_core::db::Database::open() {
        Ok(db) => {
            let ctx = borg_core::daily_summary::gather_daily_context(&db).unwrap_or_default();
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
    if is_quiet {
        msg.push_str(" note=unusual_hours");
    }
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
        "The user just opened the TUI. Greet them briefly and warmly — like a coworker who \
         remembers what you were both working on. If recent_activity names a concrete thread, \
         reference it specifically and offer to continue. If the hour is unusual for typical \
         working time, acknowledge it playfully (not forced). If there's no recent activity, \
         keep it short — greeting plus an open invitation. One or two sentences. No \
         <proactive_nudges>, no checklists.\n",
    );
    msg.push_str("</instructions>");
    msg
}

/// Whether the given local hour falls inside the configured quiet-hours
/// window. Used only as a *signal* to the session-start prompt so the agent
/// can phrase the greeting appropriately — does NOT suppress the greeting.
fn in_configured_quiet_hours(config: &Config, hour: u32) -> bool {
    fn parse_hour(s: &str) -> Option<u32> {
        s.split(':').next()?.parse().ok()
    }
    let start = config
        .heartbeat
        .quiet_hours_start
        .as_deref()
        .and_then(parse_hour);
    let end = config
        .heartbeat
        .quiet_hours_end
        .as_deref()
        .and_then(parse_hour);
    match (start, end) {
        (Some(s), Some(e)) if s < e => hour >= s && hour < e,
        (Some(s), Some(e)) => hour >= s || hour < e, // wraps midnight
        _ => false,
    }
}

/// Run a heartbeat turn in the daemon and deliver to configured channels.
async fn daemon_heartbeat_turn(config: Config) {
    let result = execute_heartbeat_turn(&config, HeartbeatSource::Scheduled).await;
    match &result {
        HeartbeatResult::Ran {
            message,
            duration_ms,
        } => {
            tracing::info!(
                "Heartbeat ran ({duration_ms}ms): {}",
                &message[..message.len().min(100)]
            );
            deliver_heartbeat_to_channels(&config, message).await;
        }
        HeartbeatResult::Skipped { reason } => {
            tracing::info!("Heartbeat skipped: {reason}");
        }
        HeartbeatResult::Failed { error } => {
            tracing::warn!("Heartbeat failed: {error}");
        }
    }
}

/// Send a heartbeat message to all configured channels.
///
/// The `base_text` is the result of the shared heartbeat turn run with the
/// root `[llm]` provider. For channels where a gateway binding matches, this
/// function runs an *additional* per-channel heartbeat turn using the binding's
/// overridden provider/model/identity so users can route different channels to
/// different models. Channels with no matching binding reuse `base_text` —
/// the common case stays a single agent turn.
async fn deliver_heartbeat_to_channels(config: &Config, base_text: &str) {
    for channel_name in &config.heartbeat.channels {
        if let Err(e) = deliver_to_channel(config, channel_name, base_text).await {
            tracing::warn!("Heartbeat delivery to {channel_name} failed: {e}");
        }
    }
}

/// Resolve the list of sender IDs to deliver a heartbeat to for a channel.
///
/// Precedence:
/// 1. If `heartbeat.recipients[channel]` is set and contains `"*"`, broadcast
///    to every approved sender for the channel.
/// 2. If `heartbeat.recipients[channel]` is set to explicit IDs, deliver only
///    to those (intersected with approved senders for safety).
/// 3. Otherwise fall back to the first approved sender (legacy behavior).
fn resolve_heartbeat_recipients(config: &Config, channel_name: &str) -> Result<Vec<String>> {
    let db = borg_core::db::Database::open()?;
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

/// Deliver a heartbeat message to a single channel using native clients.
///
/// Resolves the gateway binding for `(channel_name, sender_id)`; if the
/// binding overrides the LLM config, re-runs the heartbeat turn with that
/// config so the delivered message reflects the binding's provider/model.
async fn deliver_to_channel(config: &Config, channel_name: &str, base_text: &str) -> Result<()> {
    let recipients = resolve_heartbeat_recipients(config, channel_name)?;
    if recipients.is_empty() {
        tracing::debug!(
            "Heartbeat: no recipients resolved for {channel_name}, skipping (configure \
             [heartbeat.recipients.{channel_name}] or approve a sender)"
        );
        return Ok(());
    }

    for sender_id in &recipients {
        if let Err(e) = deliver_to_sender(config, channel_name, sender_id, base_text).await {
            tracing::warn!("Heartbeat delivery to {channel_name}:{sender_id} failed: {e}");
        }
    }

    Ok(())
}

/// Deliver a heartbeat to a specific sender on a specific channel.
async fn deliver_to_sender(
    config: &Config,
    channel_name: &str,
    sender_id: &str,
    base_text: &str,
) -> Result<()> {
    // Resolve gateway binding for this (channel, sender). If a binding matches,
    // re-run the heartbeat turn with the binding's overridden config so the
    // delivered text reflects the per-channel provider/model/identity. If no
    // binding matches, reuse the shared base_text (fast path).
    let route = borg_gateway::routing::resolve_route(config, channel_name, sender_id, None);
    let text_owned: String;
    let text: &str = if route.binding_id == "default" {
        base_text
    } else {
        tracing::info!(
            "Heartbeat: running per-channel turn for {channel_name}:{sender_id} with binding {}",
            route.binding_id
        );
        match execute_heartbeat_turn(&route.config, HeartbeatSource::Scheduled).await {
            HeartbeatResult::Ran { message, .. } => {
                text_owned = message;
                &text_owned
            }
            HeartbeatResult::Skipped { reason } => {
                tracing::debug!(
                    "Heartbeat: per-channel turn for {channel_name}:{sender_id} skipped: \
                     {reason}, falling back to base text"
                );
                base_text
            }
            HeartbeatResult::Failed { error } => {
                tracing::warn!(
                    "Heartbeat: per-channel turn for {channel_name}:{sender_id} failed: \
                     {error}, falling back to base text"
                );
                base_text
            }
        }
    };

    match send_to_channel(config, channel_name, sender_id, text, None).await {
        Ok(()) => {
            tracing::info!("Heartbeat delivered to {channel_name}:{sender_id}");
            if let Ok(adb) = borg_core::db::Database::open() {
                borg_core::activity_log::log_activity(
                    &adb,
                    "info",
                    "heartbeat",
                    &format!("Heartbeat delivered to {channel_name}:{sender_id}"),
                );
            }
        }
        Err(e) if e.to_string().contains("not supported for delivery") => {
            tracing::debug!(
                "Heartbeat: channel '{channel_name}' not supported for native delivery"
            );
        }
        Err(e) => return Err(e),
    }

    Ok(())
}

/// Send a plain text message to a specific sender on a specific channel.
async fn deliver_message_to_sender(
    config: &Config,
    channel_name: &str,
    sender_id: &str,
    text: &str,
) -> Result<()> {
    send_to_channel(config, channel_name, sender_id, text, None).await
}

/// Deliver an evolution or milestone celebration message to all configured
/// heartbeat channels. Returns `true` if at least one delivery succeeded.
async fn deliver_celebration_to_channels(
    config: &Config,
    celebration: &borg_core::db::PendingCelebration,
) -> bool {
    let kind = match celebration.celebration_type.as_str() {
        "evolution" => match serde_json::from_str::<borg_core::evolution::CelebrationPayload>(
            &celebration.payload_json,
        ) {
            Ok(p) => borg_core::evolution::CelebrationKind::Evolution(p),
            Err(e) => {
                tracing::warn!("Failed to deserialize evolution celebration payload: {e}");
                return false;
            }
        },
        "milestone" => match serde_json::from_str::<borg_core::evolution::MilestonePayload>(
            &celebration.payload_json,
        ) {
            Ok(p) => borg_core::evolution::CelebrationKind::Milestone(p),
            Err(e) => {
                tracing::warn!("Failed to deserialize milestone celebration payload: {e}");
                return false;
            }
        },
        other => {
            tracing::warn!("Unknown celebration_type {other}");
            return false;
        }
    };

    let activity_source = match kind {
        borg_core::evolution::CelebrationKind::Evolution(_) => "evolution",
        borg_core::evolution::CelebrationKind::Milestone(_) => "milestone",
    };
    let message = borg_core::evolution::format_celebration(&kind);
    let mut any_delivered = false;

    for channel_name in &config.heartbeat.channels {
        match resolve_heartbeat_recipients(config, channel_name) {
            Ok(recipients) => {
                for sender_id in &recipients {
                    if let Err(e) =
                        deliver_message_to_sender(config, channel_name, sender_id, &message).await
                    {
                        tracing::warn!(
                            "Celebration delivery ({activity_source}) to {channel_name}:{sender_id} failed: {e}"
                        );
                    } else {
                        any_delivered = true;
                        tracing::info!(
                            "Celebration ({activity_source}) delivered to {channel_name}:{sender_id}"
                        );
                        if let Ok(adb) = borg_core::db::Database::open() {
                            borg_core::activity_log::log_activity(
                                &adb,
                                "info",
                                activity_source,
                                &format!(
                                    "Celebration ({activity_source}) delivered to {channel_name}:{sender_id}"
                                ),
                            );
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to resolve recipients for {channel_name}: {e}");
            }
        }
    }

    // If no heartbeat channels configured, consider it delivered (nothing to send)
    if config.heartbeat.channels.is_empty() {
        return true;
    }

    any_delivered
}

/// Run a quick agent turn to generate a personalized greeting for a newly
/// approved sender, then deliver it to their channel. Fire-and-forget:
/// falls back to the static approval message on any failure.
pub async fn send_approval_greeting(config: &Config, channel_name: &str, sender_id: &str) {
    let agent_name = config
        .user
        .agent_name
        .as_deref()
        .unwrap_or("Borg")
        .to_string();

    match generate_and_deliver_greeting(config, channel_name, sender_id, &agent_name).await {
        Ok(()) => {
            tracing::info!(
                channel = channel_name,
                sender = sender_id,
                "Approval greeting delivered via LLM"
            );
            match borg_core::db::Database::open() {
                Ok(adb) => {
                    borg_core::activity_log::log_activity(
                        &adb,
                        "info",
                        "pairing",
                        &format!("Approval greeting delivered to {channel_name}:{sender_id}"),
                    );
                }
                Err(e) => {
                    tracing::warn!("Failed to open DB for activity log: {e}");
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                channel = channel_name,
                sender = sender_id,
                "LLM greeting failed ({e}), falling back to static message"
            );
            borg_core::pairing::send_approval_notification(
                config,
                channel_name,
                sender_id,
                &agent_name,
            )
            .await;
        }
    }
}

async fn generate_and_deliver_greeting(
    config: &Config,
    channel_name: &str,
    sender_id: &str,
    agent_name: &str,
) -> Result<()> {
    let metrics = borg_core::telemetry::BorgMetrics::noop();
    let mut config = config.clone();
    config.llm.cache.ttl = config.llm.cache.ttl.resolve(false);
    // Use Plan mode so the agent cannot execute mutating tools (shell, patch, etc.)
    config.conversation.collaboration_mode = borg_core::config::CollaborationMode::Plan;
    let mut agent = Agent::new(config.clone(), metrics)?;

    let channel_display = borg_core::pairing::channel_display_name(channel_name);
    let prompt = format!(
        "A new user has just been approved to message you on {channel_display}. \
         Generate a brief, warm greeting to introduce yourself as {agent_name}. \
         Keep it to 2-3 short sentences. Do NOT use any tools — just reply with the greeting text."
    );

    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(256);
    let cancel = CancellationToken::new();

    agent
        .send_message_with_cancel(&prompt, event_tx, cancel)
        .await?;

    let mut response = String::new();
    while let Some(event) = event_rx.recv().await {
        if let AgentEvent::TextDelta(delta) = event {
            response.push_str(&delta);
        }
    }

    let greeting = response.trim().to_string();
    if greeting.is_empty() {
        anyhow::bail!("LLM returned empty greeting");
    }

    // Truncate to a reasonable length for a greeting message
    let greeting = if greeting.len() > 500 {
        format!("{}…", &greeting[..500])
    } else {
        greeting
    };

    deliver_message_to_sender(&config, channel_name, sender_id, &greeting).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_suppresses_within_window() {
        // Same hash within dedup window should be suppressed
        assert!(is_duplicate_heartbeat(
            12345, // hash
            100,   // now_secs
            12345, // prev_hash (same)
            90,    // prev_time (10s ago)
            3600,  // window (1h)
        ));
    }

    #[test]
    fn dedup_allows_after_window() {
        // Same hash but outside dedup window should not be suppressed
        assert!(!is_duplicate_heartbeat(
            12345, // hash
            5000,  // now_secs
            12345, // prev_hash (same)
            100,   // prev_time (4900s ago)
            3600,  // window (1h)
        ));
    }

    #[test]
    fn dedup_allows_different_hash() {
        // Different hash within window should not be suppressed
        assert!(!is_duplicate_heartbeat(
            12345, // hash
            100,   // now_secs
            99999, // prev_hash (different)
            90,    // prev_time (10s ago)
            3600,  // window (1h)
        ));
    }

    #[test]
    fn dedup_allows_first_response() {
        // First response ever (prev_time = 0) should not be suppressed
        assert!(!is_duplicate_heartbeat(
            12345, // hash
            100,   // now_secs
            12345, // prev_hash (same — from AtomicU64::new(0) which could match)
            0,     // prev_time (never set)
            3600,  // window
        ));
    }

    #[test]
    fn zero_info_recognizes_trivial_acks() {
        for ack in &[
            "OK",
            "ok.",
            "ok!",
            "Nothing new",
            "all good",
            "No updates.",
            "Standing by",
            "   nothing to report   ",
        ] {
            assert!(
                is_zero_info_heartbeat(ack),
                "expected '{ack}' to be zero-info"
            );
        }
    }

    #[test]
    fn zero_info_ignores_meaningful_responses() {
        for msg in &[
            "You have 3 new emails from Alice",
            "Build failed on main — see CI",
            "Reminder: standup in 10 minutes",
            // Longer content that happens to start with an ack word is NOT zero-info.
            "ok, I finished reviewing the PR and left 4 comments on auth.rs",
        ] {
            assert!(
                !is_zero_info_heartbeat(msg),
                "expected '{msg}' to NOT be zero-info"
            );
        }
    }

    #[test]
    fn zero_info_handles_empty_and_whitespace() {
        assert!(is_zero_info_heartbeat(""));
        assert!(is_zero_info_heartbeat("   "));
        assert!(is_zero_info_heartbeat("\n\n\t"));
    }

    #[test]
    fn heartbeat_activity_fired_format() {
        assert_eq!(
            HeartbeatActivity::Fired { duration_ms: 123 }.message(),
            "Heartbeat tick: fired (123ms)"
        );
    }

    #[test]
    fn heartbeat_activity_empty_response_format() {
        assert_eq!(
            HeartbeatActivity::EmptyResponse { duration_ms: 456 }.message(),
            "Heartbeat tick: empty response (456ms)"
        );
    }

    #[test]
    fn heartbeat_activity_duplicate_format() {
        assert_eq!(
            HeartbeatActivity::DuplicateSuppressed.message(),
            "Heartbeat tick: duplicate response suppressed"
        );
    }

    #[test]
    fn heartbeat_activity_failed_format() {
        assert_eq!(
            HeartbeatActivity::Failed("agent creation: Connection refused").message(),
            "Heartbeat tick: failed (agent creation: Connection refused)"
        );
        assert_eq!(
            HeartbeatActivity::Failed("agent error: stream closed").message(),
            "Heartbeat tick: failed (agent error: stream closed)"
        );
    }

    #[test]
    fn heartbeat_activity_messages_share_prefix() {
        // Every outcome must start with "Heartbeat tick: " so /logs stays greppable
        // and visually groups a single row per tick.
        let samples = [
            HeartbeatActivity::Fired { duration_ms: 0 }.message(),
            HeartbeatActivity::EmptyResponse { duration_ms: 0 }.message(),
            HeartbeatActivity::DuplicateSuppressed.message(),
            HeartbeatActivity::Failed("x").message(),
        ];
        for msg in &samples {
            assert!(
                msg.starts_with("Heartbeat tick: "),
                "outcome message missing prefix: {msg}"
            );
        }
    }

    #[tokio::test]
    async fn deliver_message_unsupported_channel() {
        let config = Config::default();
        let result = deliver_message_to_sender(&config, "foobar", "123", "hello").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("not supported"),
            "expected 'not supported' in error, got: {err}"
        );
    }

    #[tokio::test]
    async fn deliver_message_invalid_telegram_chat_id() {
        use borg_core::config::media::CredentialValue;
        let mut config = Config::default();
        // Insert credential directly to avoid env var race conditions with other tests
        config.credentials.insert(
            "TELEGRAM_BOT_TOKEN".to_string(),
            CredentialValue::EnvVar("BORG_TEST_DUMMY_TG_TOKEN".to_string()),
        );
        unsafe { std::env::set_var("BORG_TEST_DUMMY_TG_TOKEN", "dummy") };
        let result = deliver_message_to_sender(&config, "telegram", "not_a_number", "hi").await;
        unsafe { std::env::remove_var("BORG_TEST_DUMMY_TG_TOKEN") };
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Invalid Telegram chat_id"),
            "expected chat_id parse error, got: {err}"
        );
    }

    #[tokio::test]
    async fn deliver_message_missing_telegram_token() {
        let config = Config::default();
        // Temporarily clear env var in case it's set in the test environment
        let saved = std::env::var("TELEGRAM_BOT_TOKEN").ok();
        unsafe { std::env::remove_var("TELEGRAM_BOT_TOKEN") };
        let result = deliver_message_to_sender(&config, "telegram", "12345", "hi").await;
        if let Some(v) = saved {
            unsafe { std::env::set_var("TELEGRAM_BOT_TOKEN", v) };
        }
        // If the OS keychain has a real token (dev machine), the call will reach the API
        // and fail with a different error. Either way, it should be an error.
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("TELEGRAM_BOT_TOKEN not configured") || err.contains("sendMessage failed"),
            "expected missing token or API error, got: {err}"
        );
    }

    #[tokio::test]
    async fn deliver_message_missing_slack_token() {
        let config = Config::default();
        let saved = std::env::var("SLACK_BOT_TOKEN").ok();
        unsafe { std::env::remove_var("SLACK_BOT_TOKEN") };
        let result = deliver_message_to_sender(&config, "slack", "C12345", "hi").await;
        if let Some(v) = saved {
            unsafe { std::env::set_var("SLACK_BOT_TOKEN", v) };
        }
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("SLACK_BOT_TOKEN not configured"),
            "expected missing token error, got: {err}"
        );
    }

    #[tokio::test]
    async fn greeting_fallback_on_agent_failure() {
        // With default config (no LLM provider), Agent::new will fail,
        // triggering the fallback path. This should not panic.
        let config = Config::default();
        send_approval_greeting(&config, "unsupported_channel", "12345").await;
        // If we get here without panic, the fallback path worked.
    }

    #[tokio::test]
    async fn deliver_celebration_handles_evolution_and_milestone_types() {
        // With no heartbeat channels configured, both celebration types should
        // deserialize cleanly and report success without panicking. Unknown
        // types and malformed JSON must fail closed (return false).
        let config = Config::default();

        let evo = borg_core::evolution::CelebrationPayload {
            from_stage: "base".into(),
            to_stage: "evolved".into(),
            evolution_name: Some("Pipeline Warden".into()),
            evolution_description: Some("A vigilant guardian".into()),
            dominant_archetype: Some("guardian".into()),
            bond_score: 30,
            stability: 50,
            focus: 50,
            sync_stat: 50,
            growth: 50,
            happiness: 50,
        };
        let evo_row = borg_core::db::PendingCelebration {
            id: 1,
            celebration_type: "evolution".into(),
            payload_json: serde_json::to_string(&evo).unwrap(),
            created_at: 0,
        };
        assert!(deliver_celebration_to_channels(&config, &evo_row).await);

        let milestone = borg_core::evolution::MilestonePayload {
            milestone_id: "level_10_base".into(),
            title: "Lvl.10".into(),
            level: 10,
            stage: "base".into(),
            archetype: Some("ops".into()),
        };
        let milestone_row = borg_core::db::PendingCelebration {
            id: 2,
            celebration_type: "milestone".into(),
            payload_json: serde_json::to_string(&milestone).unwrap(),
            created_at: 0,
        };
        assert!(deliver_celebration_to_channels(&config, &milestone_row).await);

        // Unknown type fails closed.
        let bogus = borg_core::db::PendingCelebration {
            id: 3,
            celebration_type: "mystery".into(),
            payload_json: "{}".into(),
            created_at: 0,
        };
        assert!(!deliver_celebration_to_channels(&config, &bogus).await);

        // Malformed milestone JSON fails closed rather than crashing.
        let malformed = borg_core::db::PendingCelebration {
            id: 4,
            celebration_type: "milestone".into(),
            payload_json: "{not-json}".into(),
            created_at: 0,
        };
        assert!(!deliver_celebration_to_channels(&config, &malformed).await);
    }

    #[test]
    fn format_milestone_renders_nonempty_with_title() {
        let kind = borg_core::evolution::CelebrationKind::Milestone(
            borg_core::evolution::MilestonePayload {
                milestone_id: "first_evolution".into(),
                title: "First Evolution".into(),
                level: 0,
                stage: "evolved".into(),
                archetype: None,
            },
        );
        let out = borg_core::evolution::format_celebration(&kind);
        assert!(!out.is_empty());
        assert!(out.contains("First Evolution"), "got: {out}");
    }
}
