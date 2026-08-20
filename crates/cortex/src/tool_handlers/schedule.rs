use anyhow::Result;

use crate::config::Config;
use crate::db::Database;
use crate::tasks;

use super::{
    optional_bool_param, optional_i64_param, optional_str_param, optional_u64_param,
    require_str_param, with_db,
};

/// One wording for "no such task", shared by every action. The model treats a
/// changed reply as a changed outcome, so the phrasing must not drift per call
/// site.
fn task_not_found(task_id: &str) -> String {
    format!("Task {task_id} not found.")
}

/// One wording for a rejected cron/interval spec, for the same reason.
fn invalid_schedule(e: impl std::fmt::Display) -> String {
    format!("Error: Invalid schedule: {e}")
}

/// Unified schedule handler: dispatches to manage_tasks or manage_cron based on `type` field.
pub async fn handle_schedule(args: &serde_json::Value, config: &Config) -> Result<String> {
    let job_type = optional_str_param(args, "type").unwrap_or("").to_string();
    let action = optional_str_param(args, "action").unwrap_or("").to_string();

    // For create, type is required
    if action == "create" && job_type.is_empty() {
        return Ok("Error: 'type' is required for create. Use 'prompt' for AI tasks or 'command' for shell cron jobs.".to_string());
    }

    // The `schedule` tool names its identifier `id`; the `manage_tasks` and
    // `manage_cron` tools that share these helpers name theirs `task_id` and
    // `job_id`. Remap so one helper serves both schemas — this is an adapter
    // between live tools, not a shim for an old name.
    let mut remapped = args.clone();
    if let Some(id) = args.get("id").cloned() {
        if job_type == "command" {
            remapped["job_id"] = id.clone();
        }
        remapped["task_id"] = id;
    }

    match job_type.as_str() {
        "command" => handle_manage_cron(&remapped, config).await,
        "prompt" => handle_manage_tasks(&remapped, config).await,
        "" => {
            // For non-create actions, try to infer type from the task in DB
            if let Some(id) = remapped
                .get("id")
                .or(remapped.get("task_id"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
            {
                if let Ok(db) = Database::open().await {
                    if let Ok(Some(task)) = db.get_task_by_id(&id).await {
                        return if task.task_type == "command" {
                            handle_manage_cron(&remapped, config).await
                        } else {
                            handle_manage_tasks(&remapped, config).await
                        };
                    }
                }
            }
            // Default: list both types, or delegate to tasks for other actions
            if action == "list" {
                let tasks_result = handle_manage_tasks(&remapped, config).await?;
                let cron_result = handle_manage_cron(&remapped, config).await?;
                Ok(format!(
                    "## Prompt Tasks\n{tasks_result}\n\n## Cron Jobs\n{cron_result}"
                ))
            } else {
                handle_manage_tasks(&remapped, config).await
            }
        }
        other => Ok(format!("Unknown type: {other}. Use 'prompt' or 'command'.")),
    }
}

pub async fn handle_manage_tasks(args: &serde_json::Value, config: &Config) -> Result<String> {
    let action = require_str_param(args, "action")?.to_string();
    match action.as_str() {
        "create" => manage_tasks_create(args, config).await,
        "list" => manage_tasks_list().await,
        "get" => manage_tasks_get(args).await,
        "update" => manage_tasks_update(args).await,
        "pause" => {
            let task_id = require_str_param(args, "task_id")?.to_string();
            if let Some(msg) = reject_system_task(&task_id, "pause") {
                return Ok(msg);
            }
            update_task_status(&task_id, tasks::TASK_STATUS_PAUSED, "paused").await
        }
        "resume" => {
            let task_id = require_str_param(args, "task_id")?.to_string();
            manage_tasks_resume(&task_id).await
        }
        "cancel" => {
            let task_id = require_str_param(args, "task_id")?.to_string();
            if let Some(msg) = reject_system_task(&task_id, "cancel") {
                return Ok(msg);
            }
            update_task_status(&task_id, tasks::TASK_STATUS_CANCELLED, "cancelled").await
        }
        "delete" => {
            let task_id = require_str_param(args, "task_id")?.to_string();
            if let Some(msg) = reject_system_task(&task_id, "delete") {
                return Ok(msg);
            }
            with_db(move |db| async move {
                match db.delete_task(&task_id).await {
                    Ok(true) => Ok(format!("Task {task_id} deleted.")),
                    Ok(false) => Ok(task_not_found(&task_id)),
                    Err(e) => Ok(format!("Error: {e}")),
                }
            })
            .await
        }
        "runs" => manage_tasks_runs(args).await,
        "run_now" => manage_tasks_run_now(args).await,
        other => Ok(format!(
            "Unknown action: {other}. Use: create, list, get, update, pause, resume, cancel, delete, runs, run_now."
        )),
    }
}

/// Refuse destructive actions on system-seeded task rows (heartbeat,
/// maintenance, consolidation, daily summary, security audit). These are
/// re-seeded idempotently on migration; deleting them would leave the
/// system without a heartbeat / maintenance loop until the next migration
/// run. Returns `Some(error_string)` if the action should be refused.
///
/// This is the agent-facing gate only. `borg tasks` does not route through
/// here, so the operator keeps control — the refusal names that route rather
/// than an `*.enabled` setting, which only some of these rows actually have.
fn reject_system_task(task_id: &str, action: &str) -> Option<String> {
    if crate::task_ids::is_system_task_id(task_id) {
        Some(format!(
            "Error: refusing to {action} system-seeded task {task_id}. \
             System tasks (heartbeat, maintenance, consolidation, daily summary, \
             security audit) are managed by Borg itself. The operator can still \
             run `borg tasks pause {task_id}` or `borg tasks run {task_id}` directly."
        ))
    } else {
        None
    }
}

/// Resolve an "origin" sentinel on `delivery_channel` to the concrete
/// `(channel, target)` of the current gateway conversation.
fn resolve_origin_delivery(
    delivery_channel: Option<String>,
    delivery_target: Option<String>,
) -> (Option<String>, Option<String>) {
    let Some(ch) = delivery_channel else {
        return (None, delivery_target);
    };
    if ch != crate::heartbeat_task::ORIGIN_DELIVERY {
        return (Some(ch), delivery_target);
    }
    let Some(origin) = crate::gateway_context::current() else {
        tracing::warn!(
            "schedule: delivery_channel='origin' requested outside a gateway turn; \
             leaving unresolved"
        );
        return (
            Some(crate::heartbeat_task::ORIGIN_DELIVERY.to_string()),
            delivery_target,
        );
    };
    let target_json = serde_json::json!({
        "sender": origin.sender_id,
        "thread_id": origin.thread_id,
        // Exact gateway session key (binding-aware) so delivery can mirror the
        // reply back into this precise transcript, not a reconstructed one.
        "session_key": origin.session_key,
    })
    .to_string();
    (Some(origin.channel_name), Some(target_json))
}

async fn manage_tasks_create(args: &serde_json::Value, config: &Config) -> Result<String> {
    let task_name = require_str_param(args, "name")?.to_string();
    let prompt = require_str_param(args, "prompt")?.to_string();
    let schedule_type = optional_str_param(args, "schedule_type")
        .unwrap_or("interval")
        .to_string();
    let schedule_expr = require_str_param(args, "schedule_expr")?.to_string();
    let timezone = optional_str_param(args, "timezone")
        .unwrap_or("local")
        .to_string();
    if let Err(e) = tasks::validate_schedule(&schedule_type, &schedule_expr) {
        return Ok(invalid_schedule(e));
    }
    let next_run = match tasks::calculate_next_run(&schedule_type, &schedule_expr, &timezone) {
        Ok(nr) => nr,
        Err(e) => return Ok(invalid_schedule(e)),
    };
    let id = uuid::Uuid::new_v4().to_string();
    if let Some(ch) = optional_str_param(args, "delivery_channel") {
        if ch == crate::heartbeat_task::BROADCAST_DELIVERY {
            return Ok(format!(
                "Error: delivery_channel='{ch}' is reserved for system tasks; user tasks must target a single channel."
            ));
        }
    }
    let (delivery_channel, delivery_target) = resolve_origin_delivery(
        optional_str_param(args, "delivery_channel").map(str::to_string),
        optional_str_param(args, "delivery_target").map(str::to_string),
    );
    let pre_run_command = optional_str_param(args, "pre_run_command").map(str::to_string);
    // A `pre_run_command` is a shell command too, and the runner gates it at
    // fire time. Refusing here as well means the model finds out now instead
    // of persisting a task that fails unattended hours later — the same
    // contract `manage_cron_create` has.
    if let Some(ref cmd) = pre_run_command {
        if let Err(denial) = tasks::check_scheduled_command(cmd, &config.policy) {
            tasks::audit_scheduled_denial(cmd, denial).await;
            return Ok(format!("Error: pre_run_command — {}", denial.message()));
        }
    }
    let pre_run_cwd = optional_str_param(args, "pre_run_cwd").map(str::to_string);
    let pre_run_timeout_ms = optional_i64_param(args, "pre_run_timeout_ms");
    if let Some(t) = pre_run_timeout_ms {
        if t <= 0 {
            return Ok(format!(
                "Error: pre_run_timeout_ms must be positive, got {t}."
            ));
        }
    }
    let max_retries = optional_i64_param(args, "max_retries").map(|v| v as i32);
    let timeout_ms = optional_i64_param(args, "timeout_ms");
    let allowed_tools = optional_str_param(args, "allowed_tools").map(str::to_string);
    // Monitor mode dedups on the pre_run output hash — without a
    // pre_run_command there is nothing to hash and every run would fire,
    // so refuse at create time instead of persisting a no-op flag.
    let continuity = optional_bool_param(args, "continuity", false);
    let monitor_mode = optional_bool_param(args, "monitor_mode", false);
    if monitor_mode && pre_run_command.is_none() {
        return Ok(
            "Error: monitor_mode requires a pre_run_command (its output hash decides whether the run fires)."
                .to_string(),
        );
    }
    with_db(move |db| async move {
        match db
            .create_task(&crate::db::NewTask {
                id: &id,
                name: &task_name,
                prompt: &prompt,
                schedule_type: &schedule_type,
                schedule_expr: &schedule_expr,
                timezone: &timezone,
                next_run,
                max_retries,
                timeout_ms,
                delivery_channel: delivery_channel.as_deref(),
                delivery_target: delivery_target.as_deref(),
                allowed_tools: allowed_tools.as_deref(),
                task_type: "prompt",
                pre_run_command: pre_run_command.as_deref(),
                pre_run_cwd: pre_run_cwd.as_deref(),
                pre_run_timeout_ms,
                min_interval_secs: None,
                continuity,
            })
            .await
        {
            Ok(()) => {
                if monitor_mode {
                    if let Err(e) = db.set_task_monitor_mode(&id, true).await {
                        // The row exists but the flag didn't land — surface it
                        // rather than reporting a monitor job that isn't one.
                        return Ok(format!(
                            "Error: task {id} created but enabling monitor_mode failed: {e}. \
                             Retry with action='update'."
                        ));
                    }
                }
                Ok(format!(
                    "Scheduled task created: {task_name} (id: {})",
                    &id[..8]
                ))
            }
            Err(e) => Ok(format!("Error creating task: {e}")),
        }
    })
    .await
}

async fn manage_tasks_list() -> Result<String> {
    with_db(|db| async move {
        match db.list_tasks().await {
            Ok(tl) if tl.is_empty() => Ok("No scheduled tasks.".to_string()),
            Ok(tl) => Ok(tl
                .iter()
                .map(tasks::format_task)
                .collect::<Vec<_>>()
                .join("\n\n")),
            Err(e) => Ok(format!("Error listing tasks: {e}")),
        }
    })
    .await
}

async fn manage_tasks_get(args: &serde_json::Value) -> Result<String> {
    let task_id = require_str_param(args, "task_id")?.to_string();
    with_db(move |db| async move {
        match db.get_task_by_id(&task_id).await {
            Ok(Some(task)) => {
                let mut output = tasks::format_task(&task);
                if let Ok(Some(run)) = db.last_task_run(&task_id).await {
                    let when = chrono::DateTime::from_timestamp(run.started_at, 0)
                        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
                        .unwrap_or_else(|| run.started_at.to_string());
                    output.push_str(&format!(
                        "\n    Last run: {} at {when} ({} ms)",
                        run.status, run.duration_ms
                    ));
                }
                Ok(output)
            }
            Ok(None) => Ok(task_not_found(&task_id)),
            Err(e) => Ok(format!("Error: {e}")),
        }
    })
    .await
}

async fn manage_tasks_update(args: &serde_json::Value) -> Result<String> {
    let task_id = require_str_param(args, "task_id")?.to_string();
    if let Some(msg) = reject_system_task(&task_id, "update") {
        return Ok(msg);
    }
    if let Some(ch) = optional_str_param(args, "delivery_channel") {
        if ch == crate::heartbeat_task::BROADCAST_DELIVERY {
            return Ok(format!(
                "Error: delivery_channel='{ch}' is reserved for system tasks; user tasks must target a single channel."
            ));
        }
    }
    let name = optional_str_param(args, "name").map(str::to_string);
    let prompt = optional_str_param(args, "prompt").map(str::to_string);
    let schedule_type = optional_str_param(args, "schedule_type").map(str::to_string);
    let schedule_expr = optional_str_param(args, "schedule_expr").map(str::to_string);
    let timezone = optional_str_param(args, "timezone").map(str::to_string);
    if let Some(st) = schedule_type.as_deref() {
        let expr = schedule_expr.as_deref().unwrap_or("");
        if let Err(e) = tasks::validate_schedule(st, expr) {
            return Ok(invalid_schedule(e));
        }
    } else if let Some(expr) = schedule_expr.as_deref() {
        let task_id_v = task_id.clone();
        let expr_v = expr.to_string();
        let validation = with_db(move |db| async move {
            match db.get_task_by_id(&task_id_v).await {
                Ok(Some(existing)) => {
                    if let Err(e) = tasks::validate_schedule(&existing.schedule_type, &expr_v) {
                        return Ok(invalid_schedule(e));
                    }
                    Ok(String::new())
                }
                Ok(None) => Ok(format!("Task {task_id_v} not found.")),
                Err(e) => Ok(format!("Error: {e}")),
            }
        })
        .await?;
        if !validation.is_empty() {
            return Ok(validation);
        }
    }
    let monitor_mode = args
        .get("monitor_mode")
        .and_then(serde_json::Value::as_bool);
    let continuity = args.get("continuity").and_then(serde_json::Value::as_bool);
    with_db(move |db| async move {
        // Enabling monitor_mode only makes sense on a task with a
        // pre_run_command — its output hash is what decides whether the run
        // fires. Check before mutating anything.
        if monitor_mode == Some(true) {
            match db.get_task_by_id(&task_id).await {
                Ok(Some(existing)) if existing.pre_run_command.is_none() => {
                    return Ok(format!(
                        "Error: monitor_mode requires a pre_run_command, and task {task_id} has none."
                    ));
                }
                Ok(Some(_)) => {}
                Ok(None) => return Ok(task_not_found(&task_id)),
                Err(e) => return Ok(format!("Error: {e}")),
            }
        }
        let update = crate::db::UpdateTask {
            name: name.as_deref(),
            prompt: prompt.as_deref(),
            schedule_type: schedule_type.as_deref(),
            schedule_expr: schedule_expr.as_deref(),
            timezone: timezone.as_deref(),
        };
        match db.update_task(&task_id, &update).await {
            Ok(true) => {
                if let Some(m) = monitor_mode {
                    if let Err(e) = db.set_task_monitor_mode(&task_id, m).await {
                        return Ok(format!(
                            "Error: task {task_id} updated but setting monitor_mode failed: {e}"
                        ));
                    }
                    // Toggling monitor mode invalidates the old baseline:
                    // clear it so the next monitored run re-baselines instead
                    // of dedup-ing against a hash from a different regime.
                    if let Err(e) = db.update_task_pre_run_hash(&task_id, None).await {
                        return Ok(format!(
                            "Error: task {task_id} updated but clearing the monitor baseline failed: {e}"
                        ));
                    }
                }
                if let Some(c) = continuity {
                    if let Err(e) = db.set_task_continuity(&task_id, c).await {
                        return Ok(format!(
                            "Error: task {task_id} updated but setting continuity failed: {e}"
                        ));
                    }
                }
                Ok(format!("Task {task_id} updated."))
            }
            Ok(false) => Ok(task_not_found(&task_id)),
            Err(e) => Ok(format!("Error: {e}")),
        }
    })
    .await
}

async fn manage_tasks_runs(args: &serde_json::Value) -> Result<String> {
    let task_id = require_str_param(args, "task_id")?.to_string();
    let limit = optional_u64_param(args, "limit", 5) as usize;
    with_db(move |db| async move {
        match db.task_run_history(&task_id, limit).await {
            Ok(runs) if runs.is_empty() => Ok("No runs recorded.".to_string()),
            Ok(runs) => {
                let mut out = String::new();
                for run in &runs {
                    let when = chrono::DateTime::from_timestamp(run.started_at, 0)
                        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
                        .unwrap_or_else(|| run.started_at.to_string());
                    let status = tasks::format_run_status(&run.status);
                    out.push_str(&format!("  {when} [{status}] {}ms", run.duration_ms));
                    if let Some(ref e) = run.error {
                        out.push_str(&format!("\n    Error: {}", &e[..e.len().min(200)]));
                    }
                    out.push('\n');
                }
                Ok(out)
            }
            Err(e) => Ok(format!("Error: {e}")),
        }
    })
    .await
}

/// Resume a paused task (delegates to [`tasks::resume_task`]).
async fn manage_tasks_resume(task_id: &str) -> Result<String> {
    let task_id = task_id.to_string();
    with_db(move |db| async move { Ok(resume_task_in(&db, &task_id).await) }).await
}

/// Map [`tasks::resume_task`] onto the tool's string reply.
pub(crate) async fn resume_task_in(db: &Database, task_id: &str) -> String {
    match tasks::resume_task(db, task_id).await {
        Ok(tasks::ResumeOutcome::Resumed) => format!("Task {task_id} resumed."),
        Ok(tasks::ResumeOutcome::AlreadyActive) => format!("Task {task_id} is already active."),
        Ok(tasks::ResumeOutcome::NotFound) => task_not_found(task_id),
        // `{e:#}` keeps the cause chain — "stays paused: rescheduling failed"
        // alone would hide *why* the stored schedule no longer parses.
        Err(e) => format!("Error: {e:#}"),
    }
}

async fn manage_tasks_run_now(args: &serde_json::Value) -> Result<String> {
    let task_id = require_str_param(args, "task_id")?.to_string();
    if let Some(msg) = reject_system_task(&task_id, "run_now") {
        return Ok(msg);
    }
    with_db(move |db| async move { Ok(run_task_now_in(&db, &task_id).await) }).await
}

/// Map [`tasks::run_task_now`] onto the tool's string reply.
pub(crate) async fn run_task_now_in(db: &Database, task_id: &str) -> String {
    match tasks::run_task_now(db, task_id).await {
        Ok(tasks::RunNowOutcome::Queued) => {
            format!("Task {task_id} queued for immediate execution.")
        }
        Ok(tasks::RunNowOutcome::Refused { status }) => format!(
            "Error: task {task_id} is {status} — the scheduler only claims active \
             tasks, so queueing it would silently do nothing. Resume it first."
        ),
        Ok(tasks::RunNowOutcome::NotFound) => task_not_found(task_id),
        Err(e) => format!("Error: {e:#}"),
    }
}

pub async fn handle_manage_cron(args: &serde_json::Value, config: &Config) -> Result<String> {
    let action = require_str_param(args, "action")?.to_string();
    match action.as_str() {
        "create" => manage_cron_create(args, config).await,
        "list" => manage_cron_list().await,
        "get" => {
            let job_id = require_str_param(args, "job_id")?.to_string();
            with_db(move |db| async move {
                match db.get_task_by_id(&job_id).await {
                    Ok(Some(task)) if task.task_type == "command" => Ok(tasks::format_task(&task)),
                    Ok(Some(_)) => Ok(format!(
                        "Job {job_id} is a prompt task, not a cron job. Use manage_tasks to manage it."
                    )),
                    Ok(None) => Ok(format!("Cron job {job_id} not found.")),
                    Err(e) => Ok(format!("Error: {e}")),
                }
            })
            .await
        }
        "delete" => {
            let job_id = require_str_param(args, "job_id")?.to_string();
            with_db(move |db| async move {
                match db.get_task_by_id(&job_id).await {
                    Ok(Some(task)) if task.task_type == "command" => {
                        match db.delete_task(&job_id).await {
                            Ok(true) => Ok(format!("Cron job {job_id} deleted.")),
                            Ok(false) => Ok(format!("Cron job {job_id} not found.")),
                            Err(e) => Ok(format!("Error: {e}")),
                        }
                    }
                    Ok(Some(_)) => Ok(format!(
                        "Job {job_id} is a prompt task, not a cron job. Use manage_tasks to manage it."
                    )),
                    Ok(None) => Ok(format!("Cron job {job_id} not found.")),
                    Err(e) => Ok(format!("Error: {e}")),
                }
            })
            .await
        }
        "pause" => {
            let job_id = require_str_param(args, "job_id")?.to_string();
            let job_id_for_check = job_id.clone();
            let exists = with_db(move |db| async move {
                match db.get_task_by_id(&job_id_for_check).await {
                    Ok(Some(task)) if task.task_type == "command" => Ok(String::new()),
                    Ok(Some(_)) => Ok(format!(
                        "Job {job_id_for_check} is a prompt task, not a cron job. Use manage_tasks to manage it."
                    )),
                    Ok(None) => Ok(format!("Cron job {job_id_for_check} not found.")),
                    Err(e) => Ok(format!("Error: {e}")),
                }
            })
            .await?;
            if !exists.is_empty() {
                return Ok(exists);
            }
            update_task_status(&job_id, tasks::TASK_STATUS_PAUSED, "paused").await
        }
        "resume" => {
            let job_id = require_str_param(args, "job_id")?.to_string();
            let job_id_for_check = job_id.clone();
            let exists = with_db(move |db| async move {
                match db.get_task_by_id(&job_id_for_check).await {
                    Ok(Some(task)) if task.task_type == "command" => Ok(String::new()),
                    Ok(Some(_)) => Ok(format!(
                        "Job {job_id_for_check} is a prompt task, not a cron job. Use manage_tasks to manage it."
                    )),
                    Ok(None) => Ok(format!("Cron job {job_id_for_check} not found.")),
                    Err(e) => Ok(format!("Error: {e}")),
                }
            })
            .await?;
            if !exists.is_empty() {
                return Ok(exists);
            }
            update_task_status(&job_id, tasks::TASK_STATUS_ACTIVE, "resumed").await
        }
        "runs" => {
            let job_id = require_str_param(args, "job_id")?.to_string();
            let limit = optional_u64_param(args, "limit", 5) as usize;
            with_db(move |db| async move {
                match db.task_run_history(&job_id, limit).await {
                    Ok(runs) if runs.is_empty() => {
                        Ok(format!("No runs recorded for cron job {job_id}."))
                    }
                    Ok(runs) => {
                        let mut out =
                            format!("Last {} runs for cron job {}:\n", runs.len(), job_id);
                        for run in &runs {
                            let when = chrono::DateTime::from_timestamp(run.started_at, 0)
                                .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
                                .unwrap_or_else(|| "?".to_string());
                            let status = tasks::format_run_status(&run.status);
                            let details = run
                                .error
                                .as_deref()
                                .or(run.result.as_deref())
                                .unwrap_or("")
                                .chars()
                                .take(80)
                                .collect::<String>();
                            out.push_str(&format!(
                                "  [{status}] {when} ({}ms) {details}\n",
                                run.duration_ms
                            ));
                        }
                        Ok(out)
                    }
                    Err(e) => Ok(format!("Error: {e}")),
                }
            })
            .await
        }
        "run_now" => {
            let job_id = require_str_param(args, "job_id")?.to_string();
            with_db(move |db| async move {
                match db.get_task_by_id(&job_id).await {
                    Ok(Some(task)) if task.task_type == "command" => {
                        let now = chrono::Utc::now().timestamp();
                        if let Err(e) = db.update_task_next_run(&job_id, Some(now)).await {
                            return Ok(format!("Error: {e}"));
                        }
                        if let Err(e) = db.clear_task_retry(&job_id).await {
                            tracing::warn!(
                                "schedule.cron.run_now: clear_task_retry({job_id}) failed: {e}"
                            );
                        }
                        Ok(format!("Cron job {job_id} queued for immediate execution."))
                    }
                    Ok(Some(_)) => Ok(format!(
                        "Job {job_id} is a prompt task, not a cron job. Use manage_tasks to manage it."
                    )),
                    Ok(None) => Ok(format!("Cron job {job_id} not found.")),
                    Err(e) => Ok(format!("Error: {e}")),
                }
            })
            .await
        }
        other => Ok(format!(
            "Unknown action: {other}. Use: create, list, get, delete, pause, resume, runs, run_now."
        )),
    }
}

async fn manage_cron_create(args: &serde_json::Value, config: &Config) -> Result<String> {
    let schedule = require_str_param(args, "schedule")?.to_string();
    let command = require_str_param(args, "command")?.to_string();
    let name = args["name"]
        .as_str()
        .map(std::string::ToString::to_string)
        .unwrap_or_else(|| {
            let short: String = command.chars().take(30).collect();
            format!("cron: {short}")
        });

    // Refuse at create time what the runner would refuse at fire time. The
    // runner remains the boundary (rows are mutable afterwards, and tasks
    // already on disk predate this check) — this exists so the model learns
    // immediately instead of persisting a task that fails unattended later.
    if let Err(denial) = tasks::check_scheduled_command(&command, &config.policy) {
        tasks::audit_scheduled_denial(&command, denial).await;
        return Ok(format!("Error: {}", denial.message()));
    }

    let cron_7 = tasks::convert_5_to_7_field(&schedule);
    if let Err(e) = tasks::validate_schedule("cron", &cron_7) {
        return Ok(invalid_schedule(e));
    }

    let next_run = match tasks::calculate_next_run("cron", &cron_7, "local") {
        Ok(nr) => nr,
        Err(e) => return Ok(invalid_schedule(e)),
    };

    let id = uuid::Uuid::new_v4().to_string();
    if optional_str_param(args, "pre_run_command").is_some() {
        return Ok(
            "Error: pre_run_command is not supported for type=command (shell-only) tasks. \
             It runs before the LLM, but command tasks have no LLM step."
                .to_string(),
        );
    }
    let (delivery_channel, delivery_target) = resolve_origin_delivery(
        optional_str_param(args, "delivery_channel").map(str::to_string),
        optional_str_param(args, "delivery_target").map(str::to_string),
    );
    let timeout_ms = optional_i64_param(args, "timeout_ms");
    with_db(move |db| async move {
        match db
            .create_task(&crate::db::NewTask {
                id: &id,
                name: &name,
                prompt: &command,
                schedule_type: "cron",
                schedule_expr: &cron_7,
                timezone: "local",
                next_run,
                max_retries: Some(0),
                timeout_ms,
                delivery_channel: delivery_channel.as_deref(),
                delivery_target: delivery_target.as_deref(),
                allowed_tools: None,
                task_type: "command",
                pre_run_command: None,
                pre_run_cwd: None,
                pre_run_timeout_ms: None,
                min_interval_secs: None,
                // A command job has no LLM turn, so there is no context for
                // continuity to carry.
                continuity: false,
            })
            .await
        {
            Ok(()) => Ok(format!(
                "Cron job created: {name} (id: {})\n  Schedule: {schedule}\n  Command: {command}",
                &id[..8]
            )),
            Err(e) => Ok(format!("Error creating cron job: {e}")),
        }
    })
    .await
}

async fn manage_cron_list() -> Result<String> {
    with_db(|db| async move {
        match db.list_tasks().await {
            Ok(tasks) => {
                let cron_jobs: Vec<_> = tasks.iter().filter(|t| t.task_type == "command").collect();
                if cron_jobs.is_empty() {
                    return Ok("No cron jobs configured.".to_string());
                }
                let mut out = format!("Cron jobs ({}):\n", cron_jobs.len());
                for job in &cron_jobs {
                    out.push_str(&tasks::format_task(job));
                    out.push('\n');
                }
                Ok(out)
            }
            Err(e) => Ok(format!("Error: {e}")),
        }
    })
    .await
}

pub async fn update_task_status(task_id: &str, status: &str, verb: &str) -> Result<String> {
    let task_id = task_id.to_string();
    let status = status.to_string();
    let verb = verb.to_string();
    with_db(move |db| async move {
        match db.update_task_status(&task_id, &status).await {
            Ok(true) => Ok(format!("Task {task_id} {verb}.")),
            Ok(false) => Ok(task_not_found(&task_id)),
            Err(e) => Ok(format!("Error: {e}")),
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refused command must be rejected *before* `with_db`, so this test
    /// needs no database — and the absence of a DB is itself the assertion
    /// that no row was created.
    #[tokio::test]
    async fn manage_cron_create_refuses_policy_denied_command() {
        let config = Config::default();
        for command in ["rm -rf /", "echo pwned > ~/.borg/hooks.json"] {
            let args = serde_json::json!({
                "schedule": "0 3 * * *",
                "command": command,
            });
            let out = manage_cron_create(&args, &config)
                .await
                .expect("handler returns Ok with an error string");
            assert!(
                out.starts_with("Error: Scheduled command refused"),
                "expected a refusal for {command:?}, got: {out}"
            );
        }
    }

    /// The `pre_run_command` on a prompt task is a shell command too. The
    /// runner gates it at fire time; refusing at create time means the model
    /// learns now rather than persisting a task that fails unattended later.
    #[tokio::test]
    async fn manage_tasks_create_refuses_a_denied_pre_run_command() {
        let config = Config::default();
        for cmd in ["rm -rf /", "echo x > ~/.borg/hooks.json"] {
            let args = serde_json::json!({
                "name": "brief",
                "prompt": "summarize",
                "schedule_type": "cron",
                "schedule_expr": "0 0 9 * * * *",
                "pre_run_command": cmd,
            });
            let out = manage_tasks_create(&args, &config)
                .await
                .expect("handler returns Ok with an error string");
            assert!(
                out.starts_with("Error: pre_run_command"),
                "expected a refusal for {cmd:?}, got: {out}"
            );
        }
    }

    /// Monitor mode dedups on the pre_run output hash; without a
    /// pre_run_command there is nothing to hash and the flag is a silent
    /// no-op. Refuse before `with_db` — no DB needed, and its absence is
    /// itself the assertion that no row was created.
    #[tokio::test]
    async fn manage_tasks_create_refuses_monitor_mode_without_pre_run() {
        let args = serde_json::json!({
            "name": "watch",
            "prompt": "report",
            "schedule_type": "interval",
            "schedule_expr": "5m",
            "monitor_mode": true,
        });
        let out = manage_tasks_create(&args, &Config::default())
            .await
            .expect("handler returns Ok with an error string");
        assert!(
            out.starts_with("Error: monitor_mode requires a pre_run_command"),
            "monitor_mode without a pre_run_command must be refused, got: {out}"
        );
    }

    #[tokio::test]
    async fn manage_cron_create_rejects_bad_schedule_for_allowed_command() {
        // Guards the ordering: an ordinary command must reach the schedule
        // validation rather than being swallowed by the policy gate.
        let args = serde_json::json!({
            "schedule": "not a cron expression",
            "command": "echo hello",
        });
        let out = manage_cron_create(&args, &Config::default())
            .await
            .expect("handler returns Ok with an error string");
        assert!(
            out.contains("Invalid schedule"),
            "allowed command must fall through to schedule validation, got: {out}"
        );
    }

    async fn seed_task(db: &Database, id: &str, next_run: Option<i64>) {
        seed_task_with(db, id, next_run, tasks::SCHEDULE_TYPE_INTERVAL, "1h").await;
    }

    async fn seed_task_with(
        db: &Database,
        id: &str,
        next_run: Option<i64>,
        schedule_type: &str,
        schedule_expr: &str,
    ) {
        db.create_task(&crate::db::NewTask {
            id,
            name: "t",
            prompt: "do the thing",
            schedule_type,
            schedule_expr,
            timezone: "",
            next_run,
            max_retries: None,
            timeout_ms: None,
            delivery_channel: None,
            delivery_target: None,
            allowed_tools: None,
            task_type: "prompt",
            pre_run_command: None,
            pre_run_cwd: None,
            pre_run_timeout_ms: None,
            min_interval_secs: None,
            continuity: false,
        })
        .await
        .expect("seed task");
    }

    /// Pause must be authoritative: `run_now` on a paused task previously set
    /// `next_run` and reported "queued", but the scheduler only claims active
    /// rows — nothing would ever fire. The refusal (and the untouched
    /// `next_run`) is the contract.
    #[tokio::test]
    async fn run_now_refuses_a_paused_task_and_leaves_next_run_alone() {
        let db = Database::test_db().await;
        let stale = chrono::Utc::now().timestamp() - 10_000;
        seed_task(&db, "task-paused", Some(stale)).await;
        db.update_task_status("task-paused", tasks::TASK_STATUS_PAUSED)
            .await
            .expect("pause");

        let out = run_task_now_in(&db, "task-paused").await;
        assert!(
            out.starts_with("Error:") && out.contains("paused"),
            "expected a pause refusal, got: {out}"
        );
        let task = db
            .get_task_by_id("task-paused")
            .await
            .expect("query")
            .expect("row");
        assert_eq!(
            task.next_run,
            Some(stale),
            "a refused run_now must not move next_run"
        );
    }

    /// Resuming a recurring task must reschedule it from now: the stored
    /// `next_run` went stale while paused, and firing immediately on resume
    /// was the half-paused failure mode.
    #[tokio::test]
    async fn resume_reschedules_a_recurring_task_instead_of_firing_immediately() {
        let db = Database::test_db().await;
        let stale = chrono::Utc::now().timestamp() - 10_000;
        seed_task(&db, "task-resume", Some(stale)).await;
        db.update_task_status("task-resume", tasks::TASK_STATUS_PAUSED)
            .await
            .expect("pause");

        let out = resume_task_in(&db, "task-resume").await;
        assert_eq!(out, "Task task-resume resumed.");
        let task = db
            .get_task_by_id("task-resume")
            .await
            .expect("query")
            .expect("row");
        assert_eq!(task.status, tasks::TASK_STATUS_ACTIVE);
        let next = task.next_run.expect("recurring task keeps a next_run");
        assert!(
            next > chrono::Utc::now().timestamp(),
            "resume must push next_run into the future, got {next} (stale was {stale})"
        );
    }

    /// A stored schedule that no longer parses must fail the resume loudly
    /// and leave the task paused. Silently writing `next_run = NULL` while
    /// flipping to active would report success for a task the scheduler can
    /// never claim again.
    #[tokio::test]
    async fn resume_of_a_bad_schedule_fails_loud_and_leaves_the_task_paused() {
        let db = Database::test_db().await;
        let stale = chrono::Utc::now().timestamp() - 10_000;
        seed_task_with(
            &db,
            "task-bad-cron",
            Some(stale),
            tasks::SCHEDULE_TYPE_CRON,
            "not a cron expr",
        )
        .await;
        db.update_task_status("task-bad-cron", tasks::TASK_STATUS_PAUSED)
            .await
            .expect("pause");

        let out = resume_task_in(&db, "task-bad-cron").await;
        assert!(
            out.starts_with("Error:") && out.contains("rescheduling failed"),
            "expected a loud reschedule failure, got: {out}"
        );
        let task = db
            .get_task_by_id("task-bad-cron")
            .await
            .expect("query")
            .expect("row");
        assert_eq!(
            task.status,
            tasks::TASK_STATUS_PAUSED,
            "a failed resume must not half-activate the task"
        );
        assert_eq!(
            task.next_run,
            Some(stale),
            "a failed resume must not touch next_run"
        );
    }

    /// Resuming a task that is already active must be a no-op: rescheduling
    /// it from now would silently push out an imminent fire.
    #[tokio::test]
    async fn resume_of_an_already_active_task_leaves_next_run_alone() {
        let db = Database::test_db().await;
        let imminent = chrono::Utc::now().timestamp() + 5;
        seed_task(&db, "task-active", Some(imminent)).await;

        let out = resume_task_in(&db, "task-active").await;
        assert_eq!(out, "Task task-active is already active.");
        let task = db
            .get_task_by_id("task-active")
            .await
            .expect("query")
            .expect("row");
        assert_eq!(
            task.next_run,
            Some(imminent),
            "resume of an active task must not delay its next fire"
        );
    }
}
