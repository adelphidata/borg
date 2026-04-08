use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use borg_core::agent::{Agent, AgentEvent};
use borg_core::config::Config;
use borg_heartbeat::scheduler::{HeartbeatEvent, HeartbeatResult, HeartbeatScheduler, SkipReason};

const LAUNCHD_LABEL: &str = "com.borg.daemon";

const LAUNCHD_PLIST_TEMPLATE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.borg.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>{{BINARY_PATH}}</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{{LOG_DIR}}/daemon.log</string>
    <key>StandardErrorPath</key>
    <string>{{LOG_DIR}}/daemon.err</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>HOME</key>
        <string>{{HOME}}</string>
    </dict>
</dict>
</plist>"#;

const SYSTEMD_UNIT_TEMPLATE: &str = r#"[Unit]
Description=Borg AI Assistant Daemon
After=network.target

[Service]
Type=simple
ExecStart={{BINARY_PATH}} daemon
Restart=on-failure
RestartSec=5
Environment=HOME={{HOME}}

[Install]
WantedBy=default.target
"#;

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

    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));

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
    const MAX_LOCK_REFRESH_FAILURES: u32 = 3;

    // Watchdog: detect main loop deadlocks
    let watchdog_ts = Arc::new(AtomicI64::new(chrono::Utc::now().timestamp()));
    {
        let ts = watchdog_ts.clone();
        let wd_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut wd_interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tokio::select! {
                    biased;
                    _ = wd_shutdown.cancelled() => return,
                    _ = wd_interval.tick() => {}
                }
                let last = ts.load(Ordering::Relaxed);
                let now = chrono::Utc::now().timestamp();
                if now - last > 180 {
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
                    if gw_crash_count >= 5 {
                        tracing::error!("Gateway crashed {gw_crash_count} times in rapid succession, stopping respawn");
                        borg_core::activity_log::log_activity(&db, "error", "system", "Gateway respawn abandoned after 5 crashes");
                        continue;
                    }
                }

                // Delay to let the port be released (longer backoff on crashes)
                let delay_ms = if is_restart { 250 } else { 250 * (1 << gw_crash_count.min(4)) };
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;

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

        // Detect sleep/wake drift: if wall-clock elapsed >> 60s, we likely resumed from sleep
        let elapsed = last_tick_wall.elapsed();
        last_tick_wall = std::time::Instant::now();
        if elapsed.as_secs() > 120 {
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
                    "Failed to refresh daemon lock ({lock_refresh_failures}/{MAX_LOCK_REFRESH_FAILURES}): {e}"
                );
                if lock_refresh_failures >= MAX_LOCK_REFRESH_FAILURES {
                    tracing::error!(
                        "Daemon lock lost after {MAX_LOCK_REFRESH_FAILURES} consecutive failures, exiting"
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
        let memory = borg_core::memory::load_memory_context(4000).unwrap_or_default();
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
    let memory = borg_core::memory::load_memory_context(4000).unwrap_or_default();
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
    let result = match channel {
        "telegram" => send_telegram(config, &parsed, &msg).await,
        "slack" => send_slack(config, &parsed, &msg).await,
        "discord" => send_discord(config, &parsed, &msg).await,
        _ => {
            tracing::warn!("Unknown delivery channel: {channel}");
            return;
        }
    };
    if let Err(e) = result {
        tracing::warn!("Failed to deliver task result via {channel}: {e}");
    }
}

async fn send_telegram(config: &Config, target: &DeliveryTarget, msg: &str) -> anyhow::Result<()> {
    let token = config
        .resolve_credential_or_env("TELEGRAM_BOT_TOKEN")
        .ok_or_else(|| anyhow::anyhow!("missing TELEGRAM_BOT_TOKEN"))?;
    let chat_id: i64 = target
        .sender
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid chat_id: {}", target.sender))?;
    // Telegram forum topics use message_thread_id (i32); our string thread_id
    // carries that value when the task was spawned from a forum topic.
    let message_thread_id: Option<i64> = target.thread_id.as_deref().and_then(|s| s.parse().ok());
    let client = borg_gateway::telegram::api::TelegramClient::new(&token)?;
    client
        .send_message(chat_id, msg, None, None, message_thread_id)
        .await
}

async fn send_slack(config: &Config, target: &DeliveryTarget, msg: &str) -> anyhow::Result<()> {
    let token = config
        .resolve_credential_or_env("SLACK_BOT_TOKEN")
        .ok_or_else(|| anyhow::anyhow!("missing SLACK_BOT_TOKEN"))?;
    let client = borg_gateway::slack::api::SlackClient::new(&token)?;
    client
        .post_message(&target.sender, msg, target.thread_id.as_deref())
        .await
        .map(|_| ())
}

async fn send_discord(config: &Config, target: &DeliveryTarget, msg: &str) -> anyhow::Result<()> {
    let token = config
        .resolve_credential_or_env("DISCORD_BOT_TOKEN")
        .ok_or_else(|| anyhow::anyhow!("missing DISCORD_BOT_TOKEN"))?;
    let client = borg_gateway::discord::api::DiscordClient::new(&token)?;
    // Discord threads are first-class channels with their own channel_id; if
    // the task captured a thread_id, post directly into the thread channel.
    let channel_id = target.thread_id.as_deref().unwrap_or(&target.sender);
    client.send_message(channel_id, msg).await
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

/// Shared heartbeat turn: creates a temporary agent, sends the heartbeat message
/// (with HEARTBEAT.md checklist if present), deduplicates, and returns a structured result.
pub async fn execute_heartbeat_turn(config: &Config) -> HeartbeatResult {
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
                    "warn",
                    "heartbeat",
                    &format!("Failed: {error}"),
                );
            }
            return HeartbeatResult::Failed { error };
        }
    };

    let checklist = borg_core::memory::load_heartbeat_checklist();
    let mut user_msg = "*heartbeat tick*".to_string();
    if let Some(ref cl) = checklist {
        user_msg.push_str("\n\n# Heartbeat Checklist\n");
        user_msg.push_str(cl);
    }

    // Proactive nudges: append LLM directives for heartbeat-only conditions
    // (e.g. no messaging channels configured). Rate-limited via the `meta`
    // table so each nudge fires at most once per its declared cooldown.
    // See `crates/cli/src/heartbeat_augmenters.rs` for how to add one.
    let augmenter_db = borg_core::db::Database::open().ok();
    let nudges = crate::heartbeat_augmenters::collect(&config, augmenter_db.as_ref());
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
    drop(augmenter_db);

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
                "warn",
                "heartbeat",
                &format!("Failed: {error}"),
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
                "debug",
                "heartbeat",
                "Heartbeat fired but response carried no actionable info",
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
                "debug",
                "heartbeat",
                "Duplicate response suppressed",
            );
        }
        return HeartbeatResult::Skipped {
            reason: SkipReason::DuplicateResponse,
        };
    }

    if let Ok(adb) = borg_core::db::Database::open() {
        borg_core::activity_log::log_activity(&adb, "info", "heartbeat", "Heartbeat fired");
    }

    HeartbeatResult::Ran {
        message: trimmed,
        duration_ms,
    }
}

/// Run a heartbeat turn in the daemon and deliver to configured channels.
async fn daemon_heartbeat_turn(config: Config) {
    let result = execute_heartbeat_turn(&config).await;
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
        match execute_heartbeat_turn(&route.config).await {
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

    match channel_name {
        "telegram" => {
            let token = config
                .resolve_credential_or_env("TELEGRAM_BOT_TOKEN")
                .ok_or_else(|| anyhow::anyhow!("TELEGRAM_BOT_TOKEN not configured"))?;
            let client = borg_gateway::telegram::api::TelegramClient::new(&token)?;
            let chat_id: i64 = sender_id
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid Telegram chat_id: {sender_id}"))?;
            client.send_message(chat_id, text, None, None, None).await?;
            tracing::info!("Heartbeat delivered to telegram:{sender_id}");
            if let Ok(adb) = borg_core::db::Database::open() {
                borg_core::activity_log::log_activity(
                    &adb,
                    "info",
                    "heartbeat",
                    &format!("Heartbeat delivered to telegram:{sender_id}"),
                );
            }
        }
        "slack" => {
            let token = config
                .resolve_credential_or_env("SLACK_BOT_TOKEN")
                .ok_or_else(|| anyhow::anyhow!("SLACK_BOT_TOKEN not configured"))?;
            let client = borg_gateway::slack::api::SlackClient::new(&token)?;
            client.post_message(sender_id, text, None).await?;
            tracing::info!("Heartbeat delivered to slack:{sender_id}");
            if let Ok(adb) = borg_core::db::Database::open() {
                borg_core::activity_log::log_activity(
                    &adb,
                    "info",
                    "heartbeat",
                    &format!("Heartbeat delivered to slack:{sender_id}"),
                );
            }
        }
        other => {
            tracing::debug!("Heartbeat: channel '{other}' not supported for native delivery");
        }
    }

    Ok(())
}

/// Ensure the daemon service is installed and running.
pub fn ensure_service_running() -> Result<()> {
    ensure_service_installed()?;

    let is_running = if cfg!(target_os = "macos") {
        std::process::Command::new("launchctl")
            .args(["list", LAUNCHD_LABEL])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    } else if cfg!(target_os = "linux") {
        std::process::Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", "borg.service"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    } else {
        return Ok(());
    };

    if !is_running {
        if cfg!(target_os = "macos") {
            if let Ok(plist) = launchd_plist_path() {
                match std::process::Command::new("launchctl")
                    .args(["load", &plist.to_string_lossy()])
                    .output()
                {
                    Ok(output) if !output.status.success() => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        tracing::warn!("launchctl load failed: {stderr}");
                    }
                    Err(e) => tracing::warn!("Failed to run launchctl load: {e}"),
                    _ => {}
                }
            }
        } else if cfg!(target_os = "linux") {
            match std::process::Command::new("systemctl")
                .args(["--user", "start", "borg.service"])
                .output()
            {
                Ok(output) if !output.status.success() => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::warn!("systemctl start failed: {stderr}");
                }
                Err(e) => tracing::warn!("Failed to run systemctl start: {e}"),
                _ => {}
            }
        }
    }

    Ok(())
}

/// Ensure the daemon service is installed, installing silently if needed.
pub fn ensure_service_installed() -> Result<()> {
    let already_installed = if cfg!(target_os = "macos") {
        launchd_plist_path().map(|p| p.exists()).unwrap_or(false)
    } else if cfg!(target_os = "linux") {
        systemd_unit_path().map(|p| p.exists()).unwrap_or(false)
    } else {
        return Ok(());
    };

    if !already_installed {
        install_service_inner()?;
    } else if service_binary_path_stale() {
        tracing::info!("Service binary path is stale, reinstalling service file");
        install_service_inner()?;
    }
    Ok(())
}

/// Check if the installed service file points to a different binary than the current one.
fn service_binary_path_stale() -> bool {
    // Canonicalize to resolve symlinks — prevents false positives when plist uses a
    // symlink path but current_exe() returns the resolved target.
    let current = match std::env::current_exe().and_then(|p| p.canonicalize()) {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(_) => return false,
    };

    let service_content = if cfg!(target_os = "macos") {
        launchd_plist_path()
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
    } else if cfg!(target_os = "linux") {
        systemd_unit_path()
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
    } else {
        None
    };

    match service_content {
        Some(content) => !content.contains(&current),
        None => false,
    }
}

fn install_service_inner() -> Result<()> {
    let binary_path = find_binary_path()?;
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    let log_dir = Config::logs_dir()?;
    std::fs::create_dir_all(&log_dir)?;

    if cfg!(target_os = "macos") {
        install_launchd(&binary_path, &home, &log_dir)
    } else if cfg!(target_os = "linux") {
        install_systemd(&binary_path, &home)
    } else {
        anyhow::bail!("Service installation is only supported on macOS and Linux")
    }
}

/// Uninstall the daemon service.
pub fn uninstall_service() -> Result<()> {
    if cfg!(target_os = "macos") {
        uninstall_launchd()
    } else if cfg!(target_os = "linux") {
        uninstall_systemd()
    } else {
        anyhow::bail!("Service management is only supported on macOS and Linux")
    }
}

/// Stop the daemon service without uninstalling it.
pub fn stop_service() -> Result<()> {
    if cfg!(target_os = "macos") {
        stop_launchd()
    } else if cfg!(target_os = "linux") {
        stop_systemd()
    } else {
        anyhow::bail!("Service management is only supported on macOS and Linux")
    }
}

/// Restart the daemon service.
pub fn restart_service() -> Result<()> {
    if cfg!(target_os = "macos") {
        restart_launchd()
    } else if cfg!(target_os = "linux") {
        restart_systemd()
    } else {
        anyhow::bail!("Service management is only supported on macOS and Linux")
    }
}

/// Show the daemon service status.
pub fn service_status() -> Result<()> {
    if cfg!(target_os = "macos") {
        status_launchd()
    } else if cfg!(target_os = "linux") {
        status_systemd()
    } else {
        anyhow::bail!("Service management is only supported on macOS and Linux")
    }
}

// ── macOS launchd ──

fn launchd_plist_path() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist")))
}

fn install_launchd(
    binary_path: &str,
    home: &std::path::Path,
    log_dir: &std::path::Path,
) -> Result<()> {
    let plist_path = launchd_plist_path()?;
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let content = LAUNCHD_PLIST_TEMPLATE
        .replace("{{BINARY_PATH}}", binary_path)
        .replace("{{LOG_DIR}}", &log_dir.to_string_lossy())
        .replace("{{HOME}}", &home.to_string_lossy());

    std::fs::write(&plist_path, &content)
        .with_context(|| format!("Failed to write plist to {}", plist_path.display()))?;

    let output = std::process::Command::new("launchctl")
        .args(["load", &plist_path.to_string_lossy()])
        .output()
        .context("Failed to run launchctl load")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("launchctl load returned non-zero: {stderr}");
    }

    Ok(())
}

fn uninstall_launchd() -> Result<()> {
    let plist_path = launchd_plist_path()?;

    if plist_path.exists() {
        let _ = std::process::Command::new("launchctl")
            .args(["unload", &plist_path.to_string_lossy()])
            .status();
        std::fs::remove_file(&plist_path)?;
    } else {
        println!("Borg not installed.");
    }
    Ok(())
}

fn status_launchd() -> Result<()> {
    let output = std::process::Command::new("launchctl")
        .args(["list", LAUNCHD_LABEL])
        .output()
        .context("Failed to run launchctl list")?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        println!("Service status: running");
        println!("{stdout}");
    } else {
        println!("Service status: not running");
    }
    Ok(())
}

fn stop_launchd() -> Result<()> {
    ensure_service_installed()?;
    let plist_path = launchd_plist_path()?;
    let status = std::process::Command::new("launchctl")
        .args(["unload", &plist_path.to_string_lossy()])
        .status()
        .context("Failed to run launchctl unload")?;
    if status.success() {
        println!("Service stopped.");
    } else {
        println!("Failed to stop service (it may not be running).");
    }
    Ok(())
}

fn restart_launchd() -> Result<()> {
    ensure_service_installed()?;
    let plist_path = launchd_plist_path()?;
    let _ = std::process::Command::new("launchctl")
        .args(["unload", &plist_path.to_string_lossy()])
        .status();
    let status = std::process::Command::new("launchctl")
        .args(["load", &plist_path.to_string_lossy()])
        .status()
        .context("Failed to run launchctl load")?;
    if status.success() {
        println!("Service restarted.");
    } else {
        println!("Failed to restart service.");
    }
    Ok(())
}

// ── Linux systemd ──

fn systemd_unit_path() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    Ok(home
        .join(".config")
        .join("systemd")
        .join("user")
        .join("borg.service"))
}

fn install_systemd(binary_path: &str, home: &std::path::Path) -> Result<()> {
    let unit_path = systemd_unit_path()?;
    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let content = SYSTEMD_UNIT_TEMPLATE
        .replace("{{BINARY_PATH}}", binary_path)
        .replace("{{HOME}}", &home.to_string_lossy());

    std::fs::write(&unit_path, &content)
        .with_context(|| format!("Failed to write unit to {}", unit_path.display()))?;

    match std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output()
    {
        Ok(output) if !output.status.success() => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!("systemctl daemon-reload failed: {stderr}");
        }
        Err(e) => tracing::warn!("Failed to run systemctl daemon-reload: {e}"),
        _ => {}
    }

    let output = std::process::Command::new("systemctl")
        .args(["--user", "enable", "--now", "borg.service"])
        .output()
        .context("Failed to enable service")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("systemctl enable --now failed: {stderr}");
    }

    Ok(())
}

fn uninstall_systemd() -> Result<()> {
    let unit_path = systemd_unit_path()?;

    let _ = std::process::Command::new("systemctl")
        .args(["--user", "disable", "--now", "borg.service"])
        .status();

    if unit_path.exists() {
        std::fs::remove_file(&unit_path)?;
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        println!("Borg decommissioned.");
    } else {
        println!("Borg not installed.");
    }
    Ok(())
}

fn status_systemd() -> Result<()> {
    let output = std::process::Command::new("systemctl")
        .args(["--user", "status", "borg.service"])
        .output()
        .context("Failed to run systemctl status")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("{stdout}");
    Ok(())
}

fn stop_systemd() -> Result<()> {
    ensure_service_installed()?;
    let status = std::process::Command::new("systemctl")
        .args(["--user", "stop", "borg.service"])
        .status()
        .context("Failed to run systemctl stop")?;
    if status.success() {
        println!("Service stopped.");
    } else {
        println!("Failed to stop service (it may not be running).");
    }
    Ok(())
}

fn restart_systemd() -> Result<()> {
    ensure_service_installed()?;
    let status = std::process::Command::new("systemctl")
        .args(["--user", "restart", "borg.service"])
        .status()
        .context("Failed to run systemctl restart")?;
    if status.success() {
        println!("Service restarted.");
    } else {
        println!("Failed to restart service.");
    }
    Ok(())
}

fn find_binary_path() -> Result<String> {
    // Prefer current executable to avoid finding a different `borg` binary (e.g., BorgBackup)
    if let Ok(exe) = std::env::current_exe() {
        return Ok(exe.to_string_lossy().to_string());
    }

    // Fall back to PATH lookup
    if let Ok(path) = which::which("borg") {
        return Ok(path.to_string_lossy().to_string());
    }

    anyhow::bail!("Could not determine binary path")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launchd_template_substitution() {
        let content = LAUNCHD_PLIST_TEMPLATE
            .replace("{{BINARY_PATH}}", "/usr/local/bin/borg")
            .replace("{{LOG_DIR}}", "/tmp/logs")
            .replace("{{HOME}}", "/Users/test");

        assert!(content.contains("<string>/usr/local/bin/borg</string>"));
        assert!(content.contains("<string>/tmp/logs/daemon.log</string>"));
        assert!(content.contains("<string>/Users/test</string>"));
        assert!(!content.contains("{{"));
    }

    #[test]
    fn systemd_template_substitution() {
        let content = SYSTEMD_UNIT_TEMPLATE
            .replace("{{BINARY_PATH}}", "/usr/local/bin/borg")
            .replace("{{HOME}}", "/home/test");

        assert!(content.contains("ExecStart=/usr/local/bin/borg daemon"));
        assert!(content.contains("Environment=HOME=/home/test"));
        assert!(!content.contains("{{"));
    }

    #[test]
    fn launchd_plist_is_valid_xml_structure() {
        let content = LAUNCHD_PLIST_TEMPLATE
            .replace("{{BINARY_PATH}}", "/bin/borg")
            .replace("{{LOG_DIR}}", "/tmp")
            .replace("{{HOME}}", "/home");

        assert!(content.starts_with("<?xml"));
        assert!(content.contains("<plist version=\"1.0\">"));
        assert!(content.contains("</plist>"));
        assert!(content.contains("<key>Label</key>"));
        assert!(content.contains(&format!("<string>{LAUNCHD_LABEL}</string>")));
    }

    #[test]
    fn systemd_unit_has_required_sections() {
        assert!(SYSTEMD_UNIT_TEMPLATE.contains("[Unit]"));
        assert!(SYSTEMD_UNIT_TEMPLATE.contains("[Service]"));
        assert!(SYSTEMD_UNIT_TEMPLATE.contains("[Install]"));
        assert!(SYSTEMD_UNIT_TEMPLATE.contains("WantedBy=default.target"));
        assert!(SYSTEMD_UNIT_TEMPLATE.contains("Restart=on-failure"));
    }

    #[test]
    fn launchd_label_constant() {
        assert_eq!(LAUNCHD_LABEL, "com.borg.daemon");
    }

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
    fn stale_binary_path_detected_for_launchd() {
        // Test that a plist with a different binary path is detected as stale
        let content = LAUNCHD_PLIST_TEMPLATE
            .replace("{{BINARY_PATH}}", "/old/path/to/borg")
            .replace("{{LOG_DIR}}", "/tmp/logs")
            .replace("{{HOME}}", "/Users/test");

        // The content should NOT contain the current exe
        let current_exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        assert!(
            !content.contains(&current_exe),
            "plist with old path should not contain current exe"
        );

        // Verify it does contain the old path
        assert!(content.contains("/old/path/to/borg"));
    }

    #[test]
    fn stale_binary_path_not_detected_when_matching() {
        let current_exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "/usr/local/bin/borg".to_string());

        let content = LAUNCHD_PLIST_TEMPLATE
            .replace("{{BINARY_PATH}}", &current_exe)
            .replace("{{LOG_DIR}}", "/tmp/logs")
            .replace("{{HOME}}", "/Users/test");

        assert!(
            content.contains(&current_exe),
            "plist with current path should contain current exe"
        );
    }
}
