//! Scheduled-task and workflow-step execution. Lifted from
//! `crates/cli/src/service/mod.rs` so the daemon binary can run jobs without
//! shelling out to the cli's in-process helpers.

use cortex::config::Config;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Spawn a single scheduled-task execution. Mirrors the cli's
/// `service::spawn_task_execution` — keeps the execution body identical so
/// behavior doesn't drift between the legacy in-cli path and the new
/// in-borgd path.
pub async fn spawn_task_execution(
    task: cortex::db::ScheduledTaskRow,
    run_id: i64,
    is_retry: bool,
    semaphore: Arc<Semaphore>,
    config: Config,
) {
    // Acquire the permit *inside* the spawned task, not on the caller's
    // dispatcher loop. If we awaited here while all permits were claimed, the
    // scheduler dispatcher would stall and the watchdog would kill the daemon.
    let task_name = task.name.clone();
    let task_id = task.id.clone();
    let task_prompt = match task.id.as_str() {
        cortex::daily_summary::DAILY_SUMMARY_TASK_ID => {
            match cortex::daily_summary::build_daily_summary_prompt() {
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
    let task_type = task.task_type;

    tokio::spawn(async move {
        let _permit = match semaphore.acquire_owned().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Task '{task_name}': failed to acquire semaphore: {e}");
                return;
            }
        };
        match cortex::db::Database::open() {
            Ok(adb) => {
                let attempt = if is_retry {
                    format!(" (retry {retry_count})")
                } else {
                    String::new()
                };
                cortex::activity_log::log_activity(
                    &adb,
                    "info",
                    "task",
                    &format!("Task '{task_name}' started{attempt}"),
                );
            }
            Err(e) => tracing::warn!("Task '{task_name}': activity log DB open failed: {e}"),
        }
        let started_at_ms = chrono::Utc::now().timestamp_millis();

        let exec_ctx = TaskExecContext {
            task_name: task_name.clone(),
            task_id,
            prompt_or_command: task_prompt,
            timeout: task_timeout,
            run_id,
            started_at_ms,
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
            match cortex::db::Database::open() {
                Ok(db) => {
                    let duration_ms = chrono::Utc::now().timestamp_millis() - started_at_ms;
                    if let Err(e) = db.complete_task_run(
                        run_id,
                        duration_ms,
                        None,
                        Some(&format!("panic: {panic_msg}")),
                    ) {
                        tracing::warn!("Task '{task_name}': failed to record panic: {e}");
                    }
                }
                Err(e) => tracing::warn!("Task '{task_name}': panic DB open failed: {e}"),
            }
        }
    });
}

/// Spawn a workflow step.
pub async fn spawn_workflow_step(
    workflow: cortex::db::WorkflowRow,
    step: cortex::db::WorkflowStepRow,
    prior_steps: Vec<cortex::db::WorkflowStepRow>,
    all_steps: Vec<cortex::db::WorkflowStepRow>,
    semaphore: Arc<Semaphore>,
    config: Config,
) {
    let step_id = step.id;
    let step_title = step.title.clone();
    let workflow_id = workflow.id.clone();
    let workflow_title = workflow.title.clone();
    let step_timeout = std::time::Duration::from_millis(step.timeout_ms as u64);

    tokio::spawn(async move {
        // Acquire inside the spawned task; see note in spawn_task_execution.
        let _permit = match semaphore.acquire_owned().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Workflow step '{step_title}': failed to acquire semaphore: {e}");
                return;
            }
        };

        let db = match cortex::db::Database::open() {
            Ok(db) => db,
            Err(e) => {
                tracing::error!("Failed to open DB for workflow step: {e}");
                return;
            }
        };

        let context = cortex::workflow::engine::build_step_context_with_remaining(
            &workflow,
            &step,
            &prior_steps,
            &all_steps,
        );

        let identity = cortex::identity::load_identity().unwrap_or_default();
        let memory = cortex::memory::load_memory_context_db(4000).unwrap_or_default();
        let time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z");

        let system = format!("{identity}\n\n# Current Time\n{time}\n\n{memory}\n\n{context}");

        let messages = vec![
            cortex::types::Message::system(system),
            cortex::types::Message::user(&step.instructions),
        ];

        let llm = match cortex::llm::LlmClient::new(&config) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("Failed to create LLM client for workflow step '{step_title}': {e}");
                if let Err(e2) = db.fail_workflow_step(step_id, &format!("{e}")) {
                    tracing::warn!("Workflow step '{step_title}': failed to mark failure: {e2}");
                }
                return;
            }
        };

        let result = tokio::time::timeout(step_timeout, llm.chat(&messages, None)).await;

        match result {
            Ok(Ok(response)) => {
                let result_text = response.text_content().unwrap_or("").to_string();
                if let Err(e) = db.complete_workflow_step(step_id, &result_text) {
                    tracing::warn!("Failed to complete workflow step: {e}");
                }
                if let Ok(Some(wf)) = db.get_workflow(&workflow_id) {
                    if wf.status == cortex::workflow::status::COMPLETED {
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
                tracing::warn!("Workflow step failed: {workflow_title} — {error_msg}");
                if let Err(e2) = db.fail_workflow_step(step_id, &error_msg) {
                    tracing::warn!(
                        "Workflow step '{workflow_title}': failed to mark failure: {e2}"
                    );
                }
            }
            Err(_) => {
                let error_msg = format!("Step timed out after {}ms", step_timeout.as_millis());
                tracing::warn!("Workflow step timed out: {workflow_title}");
                if let Err(e2) = db.fail_workflow_step(step_id, &error_msg) {
                    tracing::warn!(
                        "Workflow step '{workflow_title}': failed to mark timeout: {e2}"
                    );
                }
            }
        }
    });
}

fn build_workflow_summary(db: &cortex::db::Database, workflow_id: &str) -> String {
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

struct TaskExecContext {
    task_name: String,
    task_id: String,
    prompt_or_command: String,
    timeout: std::time::Duration,
    run_id: i64,
    started_at_ms: i64,
    retry_count: i32,
    max_retries: i32,
    schedule_type: String,
    delivery_channel: Option<String>,
    delivery_target: Option<String>,
    config: Config,
}

async fn execute_maintenance_task(ctx: &TaskExecContext) {
    let TaskExecContext {
        task_name,
        task_id,
        run_id,
        started_at_ms,
        config,
        ..
    } = ctx;

    let config_clone = config.clone();
    let result = tokio::task::spawn_blocking(move || {
        let db = cortex::db::Database::open()?;
        cortex::maintenance::run_daily_maintenance(&db, &config_clone)
    })
    .await;

    let duration_ms = chrono::Utc::now().timestamp_millis() - *started_at_ms;
    let (summary, error_msg) = match result {
        Ok(Ok(report)) => {
            let summary = report.activity_line();
            tracing::info!("Maintenance task '{task_name}': {summary}");
            (Some(summary), None)
        }
        Ok(Err(e)) => (None, Some(format!("maintenance sweep failed: {e}"))),
        Err(e) => (None, Some(format!("maintenance task panicked: {e}"))),
    };

    match cortex::db::Database::open() {
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
                    tracing::warn!("maintenance: failed to clear task retry for {task_id}: {e}");
                }
            }
        }
        Err(e) => tracing::warn!("maintenance: DB open failed for finalize: {e}"),
    }
}

async fn execute_command_task(ctx: &TaskExecContext) {
    let TaskExecContext {
        task_name,
        task_id,
        prompt_or_command: command,
        timeout,
        run_id,
        started_at_ms,
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
            let duration_ms = chrono::Utc::now().timestamp_millis() - *started_at_ms;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let combined = if stderr.is_empty() {
                stdout.to_string()
            } else {
                format!("{stdout}\n[stderr]\n{stderr}")
            };

            if output.status.success() {
                match cortex::db::Database::open() {
                    Ok(db) => {
                        if let Err(e) =
                            db.complete_task_run(*run_id, duration_ms, Some(&combined), None)
                        {
                            tracing::warn!("Task '{task_name}': failed to finalize run: {e}");
                        }
                        if let Err(e) = db.clear_task_retry(task_id) {
                            tracing::warn!("Task '{task_name}': failed to clear retry: {e}");
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Task '{task_name}': DB open failed at success path: {e}")
                    }
                }
                if let (Some(ch), Some(tgt)) = (delivery_channel, delivery_target) {
                    deliver_task_result(ch, tgt, task_name, &combined, config).await;
                }
            } else {
                let exit_code = output.status.code().unwrap_or(-1);
                let error_msg = format!("exit code {exit_code}");
                match cortex::db::Database::open() {
                    Ok(db) => {
                        if let Err(e) = db.complete_task_run(
                            *run_id,
                            duration_ms,
                            Some(&combined),
                            Some(&error_msg),
                        ) {
                            tracing::warn!(
                                "Task '{task_name}': failed to record exit-code failure: {e}"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Task '{task_name}': DB open failed at failure path: {e}")
                    }
                }
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
            let duration_ms = chrono::Utc::now().timestamp_millis() - *started_at_ms;
            handle_task_failure(
                task_id,
                task_name,
                *run_id,
                duration_ms,
                *retry_count,
                *max_retries,
                schedule_type,
                delivery_channel,
                delivery_target,
                config,
                &format!("Failed to spawn command: {e}"),
            )
            .await;
        }
        Err(_) => {
            let duration_ms = chrono::Utc::now().timestamp_millis() - *started_at_ms;
            handle_task_failure(
                task_id,
                task_name,
                *run_id,
                duration_ms,
                *retry_count,
                *max_retries,
                schedule_type,
                delivery_channel,
                delivery_target,
                config,
                "Cron job timed out",
            )
            .await;
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
        started_at_ms,
        retry_count,
        max_retries,
        schedule_type,
        delivery_channel,
        delivery_target,
        config,
    } = ctx;
    let identity = cortex::identity::load_identity().unwrap_or_default();
    let memory = cortex::memory::load_memory_context_db(4000).unwrap_or_default();
    let time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z");

    let system = format!(
        "{identity}\n\n# Current Time\n{time}\n\n{memory}\n\n\
         # Scheduled Task\nYou are executing a scheduled task: \"{task_name}\"\n\
         Respond with the task result. Be concise."
    );

    let messages = vec![
        cortex::types::Message::system(system),
        cortex::types::Message::user(task_prompt),
    ];

    let llm = match cortex::llm::LlmClient::new(config) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("Failed to create LLM client for task '{task_name}': {e}");
            match cortex::db::Database::open() {
                Ok(db) => {
                    if let Err(e2) = db.complete_task_run(*run_id, 0, None, Some(&format!("{e}"))) {
                        tracing::warn!(
                            "Task '{task_name}': failed to record LLM-init failure: {e2}"
                        );
                    }
                }
                Err(e2) => tracing::warn!("Task '{task_name}': DB open failed at LLM-init: {e2}"),
            }
            return;
        }
    };
    let result = tokio::time::timeout(*timeout, llm.chat(&messages, None)).await;

    match result {
        Ok(Ok(response)) => {
            let duration_ms = chrono::Utc::now().timestamp_millis() - *started_at_ms;
            let result_text = response.text_content().unwrap_or("");
            match cortex::db::Database::open() {
                Ok(db) => {
                    if let Err(e) =
                        db.complete_task_run(*run_id, duration_ms, Some(result_text), None)
                    {
                        tracing::warn!(
                            "Task '{task_name}': failed to finalize prompt-task run: {e}"
                        );
                    }
                    if let Err(e) = db.clear_task_retry(task_id) {
                        tracing::warn!("Task '{task_name}': failed to clear retry: {e}");
                    }
                }
                Err(e) => {
                    tracing::warn!("Task '{task_name}': DB open failed at prompt-success: {e}")
                }
            }
            if let (Some(ch), Some(tgt)) = (delivery_channel, delivery_target) {
                deliver_task_result(ch, tgt, task_name, result_text, config).await;
            }
        }
        Ok(Err(e)) => {
            let duration_ms = chrono::Utc::now().timestamp_millis() - *started_at_ms;
            handle_task_failure(
                task_id,
                task_name,
                *run_id,
                duration_ms,
                *retry_count,
                *max_retries,
                schedule_type,
                delivery_channel,
                delivery_target,
                config,
                &format!("{e}"),
            )
            .await;
        }
        Err(_) => {
            let duration_ms = chrono::Utc::now().timestamp_millis() - *started_at_ms;
            handle_task_failure(
                task_id,
                task_name,
                *run_id,
                duration_ms,
                *retry_count,
                *max_retries,
                schedule_type,
                delivery_channel,
                delivery_target,
                config,
                "Task timed out",
            )
            .await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_task_failure(
    task_id: &str,
    task_name: &str,
    run_id: i64,
    duration_ms: i64,
    retry_count: i32,
    max_retries: i32,
    schedule_type: &str,
    delivery_channel: &Option<String>,
    delivery_target: &Option<String>,
    config: &Config,
    error: &str,
) {
    match cortex::db::Database::open() {
        Ok(db) => {
            if let Err(e) = db.complete_task_run(run_id, duration_ms, None, Some(error)) {
                tracing::warn!("Task '{task_name}': failed to record failure: {e}");
            }
            if cortex::tasks::is_transient_error(error) && retry_count < max_retries {
                let next_attempt = retry_count + 1;
                let delay = cortex::tasks::retry_delay_secs(next_attempt);
                let retry_at = chrono::Utc::now().timestamp() + delay;
                if let Err(e) = db.set_task_retry(task_id, next_attempt, error, retry_at) {
                    tracing::warn!("Task '{task_name}': failed to schedule retry: {e}");
                }
                tracing::warn!(
                    "Task '{task_name}' failed (transient), retry {next_attempt}/{max_retries} in {delay}s: {error}"
                );
                return;
            }
            if let Err(e) = db.clear_task_retry(task_id) {
                tracing::warn!("Task '{task_name}': failed to clear retry: {e}");
            }
            if schedule_type == "once" {
                if let Err(e) = db.update_task_status(task_id, "completed") {
                    tracing::warn!("Task '{task_name}': failed to mark completed: {e}");
                }
            }
        }
        Err(e) => tracing::warn!("Task '{task_name}': DB open failed at handle_task_failure: {e}"),
    }
    tracing::warn!("Task '{task_name}' failed: {error}");

    if let (Some(ch), Some(tgt)) = (delivery_channel, delivery_target) {
        let msg = format!("Error: {error}");
        deliver_task_result(ch, tgt, &format!("{task_name} [FAILED]"), &msg, config).await;
    }
}

struct DeliveryTarget {
    sender: String,
    #[allow(dead_code)]
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
        tracing::warn!("Task '{task_name}' has unresolved 'origin' delivery — dropping");
        return;
    }
    let parsed = parse_delivery_target(target);
    let msg = format!("[Task: {task_name}]\n{text}");
    if let Err(e) = crate::channel_delivery::send(config, channel, &parsed.sender, &msg).await {
        tracing::warn!("Failed to deliver task result via {channel}: {e}");
    }
}

/// Deliver an evolution / milestone celebration to all configured heartbeat
/// channels. Returns `true` if at least one delivery succeeded (or no channels
/// configured — in which case there's nothing to do).
pub async fn deliver_celebration_to_channels(
    config: Config,
    celebration: cortex::db::PendingCelebration,
) -> bool {
    let kind = match celebration.celebration_type.as_str() {
        "evolution" => match serde_json::from_str::<cortex::evolution::CelebrationPayload>(
            &celebration.payload_json,
        ) {
            Ok(p) => cortex::evolution::CelebrationKind::Evolution(p),
            Err(e) => {
                tracing::warn!("Bad evolution celebration payload: {e}");
                return false;
            }
        },
        "milestone" => match serde_json::from_str::<cortex::evolution::MilestonePayload>(
            &celebration.payload_json,
        ) {
            Ok(p) => cortex::evolution::CelebrationKind::Milestone(p),
            Err(e) => {
                tracing::warn!("Bad milestone celebration payload: {e}");
                return false;
            }
        },
        other => {
            tracing::warn!("Unknown celebration_type {other}");
            return false;
        }
    };

    let message = cortex::evolution::format_celebration(&kind);
    let mut any_delivered = false;

    for channel_name in &config.heartbeat.channels {
        if let Ok(db) = cortex::db::Database::open() {
            if let Ok(approved) = db.list_approved_senders(Some(channel_name)) {
                let recipients: Vec<String> = match config.heartbeat.recipients.get(channel_name) {
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
                for sender_id in &recipients {
                    if let Err(e) =
                        crate::channel_delivery::send(&config, channel_name, sender_id, &message)
                            .await
                    {
                        tracing::warn!(
                            "Celebration delivery to {channel_name}:{sender_id} failed: {e}"
                        );
                    } else {
                        any_delivered = true;
                    }
                }
            }
        }
    }

    if config.heartbeat.channels.is_empty() {
        return true;
    }
    any_delivered
}
