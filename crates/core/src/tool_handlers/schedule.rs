use anyhow::Result;

use crate::config::Config;
use crate::db::Database;
use crate::tasks;

use super::{optional_i64_param, optional_str_param, optional_u64_param, require_str_param};

/// Open the database and run a callback, formatting the open error if it fails.
fn with_db<F>(f: F) -> Result<String>
where
    F: FnOnce(&Database) -> Result<String>,
{
    match Database::open() {
        Ok(db) => f(&db),
        Err(e) => Ok(format!("Error opening database: {e}")),
    }
}

/// Unified schedule handler: dispatches to manage_tasks or manage_cron based on `type` field.
pub fn handle_schedule(args: &serde_json::Value, config: &Config) -> Result<String> {
    let job_type = optional_str_param(args, "type").unwrap_or("");
    let action = optional_str_param(args, "action").unwrap_or("");

    // For create, type is required
    if action == "create" && job_type.is_empty() {
        return Ok("Error: 'type' is required for create. Use 'prompt' for AI tasks or 'command' for shell cron jobs.".to_string());
    }

    // Remap "id" to "task_id"/"job_id" for backward compat with existing helpers
    let mut remapped = args.clone();
    if let Some(id) = args.get("id").cloned() {
        if job_type == "command" {
            remapped["job_id"] = id.clone();
        }
        remapped["task_id"] = id;
    }

    match job_type {
        "command" => handle_manage_cron(&remapped, config),
        "prompt" => handle_manage_tasks(&remapped, config),
        "" => {
            // For non-create actions, try to infer type from the task in DB
            if let Some(id) = remapped
                .get("id")
                .or(remapped.get("task_id"))
                .and_then(|v| v.as_str())
            {
                if let Ok(db) = Database::open() {
                    if let Ok(Some(task)) = db.get_task_by_id(id) {
                        return if task.task_type == "command" {
                            handle_manage_cron(&remapped, config)
                        } else {
                            handle_manage_tasks(&remapped, config)
                        };
                    }
                }
            }
            // Default: list both types, or delegate to tasks for other actions
            if action == "list" {
                let tasks_result = handle_manage_tasks(&remapped, config)?;
                let cron_result = handle_manage_cron(&remapped, config)?;
                Ok(format!(
                    "## Prompt Tasks\n{tasks_result}\n\n## Cron Jobs\n{cron_result}"
                ))
            } else {
                handle_manage_tasks(&remapped, config)
            }
        }
        other => Ok(format!("Unknown type: {other}. Use 'prompt' or 'command'.")),
    }
}

pub fn handle_manage_tasks(args: &serde_json::Value, _config: &Config) -> Result<String> {
    let action = require_str_param(args, "action")?;
    match action {
        "create" => manage_tasks_create(args),
        "list" => manage_tasks_list(),
        "get" => manage_tasks_get(args),
        "update" => manage_tasks_update(args),
        "pause" => {
            let task_id = require_str_param(args, "task_id")?;
            update_task_status(task_id, tasks::TASK_STATUS_PAUSED, "paused")
        }
        "resume" => {
            let task_id = require_str_param(args, "task_id")?;
            update_task_status(task_id, tasks::TASK_STATUS_ACTIVE, "resumed")
        }
        "cancel" => {
            let task_id = require_str_param(args, "task_id")?;
            update_task_status(task_id, tasks::TASK_STATUS_CANCELLED, "cancelled")
        }
        "delete" => {
            let task_id = require_str_param(args, "task_id")?;
            with_db(|db| match db.delete_task(task_id) {
                Ok(true) => Ok(format!("Task {task_id} deleted.")),
                Ok(false) => Ok(format!("Task {task_id} not found.")),
                Err(e) => Ok(format!("Error: {e}")),
            })
        }
        "runs" => manage_tasks_runs(args),
        "run_now" => manage_tasks_run_now(args),
        other => Ok(format!(
            "Unknown action: {other}. Use: create, list, get, update, pause, resume, cancel, delete, runs, run_now."
        )),
    }
}

/// Resolve an "origin" sentinel on `delivery_channel` to the concrete
/// `(channel, target)` of the current gateway conversation.
///
/// When the agent is running a gateway-triggered turn, [`crate::gateway_context::current`]
/// returns the originating channel/sender/thread. If `delivery_channel ==
/// "origin"`, we rewrite it to the real channel name and encode the sender
/// (and thread, if present) into `delivery_target` as JSON so the delivery
/// step can thread replies correctly. If no origin context is available the
/// sentinel is left as-is and `deliver_task_result` will report an error.
fn resolve_origin_delivery(
    delivery_channel: Option<String>,
    delivery_target: Option<String>,
) -> (Option<String>, Option<String>) {
    let Some(ch) = delivery_channel else {
        return (None, delivery_target);
    };
    if ch != "origin" {
        return (Some(ch), delivery_target);
    }
    let Some(origin) = crate::gateway_context::current() else {
        tracing::warn!(
            "schedule: delivery_channel='origin' requested outside a gateway turn; \
             leaving unresolved"
        );
        return (Some("origin".to_string()), delivery_target);
    };
    // Encode sender + thread as JSON so the delivery step can thread replies.
    let target_json = serde_json::json!({
        "sender": origin.sender_id,
        "thread_id": origin.thread_id,
    })
    .to_string();
    (Some(origin.channel_name), Some(target_json))
}

fn manage_tasks_create(args: &serde_json::Value) -> Result<String> {
    let task_name = require_str_param(args, "name")?;
    let prompt = require_str_param(args, "prompt")?;
    let schedule_type = optional_str_param(args, "schedule_type").unwrap_or("interval");
    let schedule_expr = require_str_param(args, "schedule_expr")?;
    let timezone = optional_str_param(args, "timezone").unwrap_or("local");
    if let Err(e) = tasks::validate_schedule(schedule_type, schedule_expr) {
        return Ok(format!("Error: Invalid schedule: {e}"));
    }
    let next_run = match tasks::calculate_next_run(schedule_type, schedule_expr) {
        Ok(nr) => nr,
        Err(e) => return Ok(format!("Error: Invalid schedule: {e}")),
    };
    let id = uuid::Uuid::new_v4().to_string();
    let (delivery_channel, delivery_target) = resolve_origin_delivery(
        optional_str_param(args, "delivery_channel").map(str::to_string),
        optional_str_param(args, "delivery_target").map(str::to_string),
    );
    with_db(|db| {
        match db.create_task(&crate::db::NewTask {
            id: &id,
            name: task_name,
            prompt,
            schedule_type,
            schedule_expr,
            timezone,
            next_run,
            max_retries: optional_i64_param(args, "max_retries").map(|v| v as i32),
            timeout_ms: optional_i64_param(args, "timeout_ms"),
            delivery_channel: delivery_channel.as_deref(),
            delivery_target: delivery_target.as_deref(),
            allowed_tools: optional_str_param(args, "allowed_tools"),
            task_type: "prompt",
        }) {
            Ok(()) => Ok(format!(
                "Scheduled task created: {task_name} (id: {})",
                &id[..8]
            )),
            Err(e) => Ok(format!("Error creating task: {e}")),
        }
    })
}

fn manage_tasks_list() -> Result<String> {
    with_db(|db| match db.list_tasks() {
        Ok(tl) if tl.is_empty() => Ok("No scheduled tasks.".to_string()),
        Ok(tl) => Ok(tl
            .iter()
            .map(tasks::format_task)
            .collect::<Vec<_>>()
            .join("\n\n")),
        Err(e) => Ok(format!("Error listing tasks: {e}")),
    })
}

fn manage_tasks_get(args: &serde_json::Value) -> Result<String> {
    let task_id = require_str_param(args, "task_id")?;
    with_db(|db| match db.get_task_by_id(task_id) {
        Ok(Some(task)) => {
            let mut output = tasks::format_task(&task);
            if let Ok(Some(run)) = db.last_task_run(task_id) {
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
        Ok(None) => Ok(format!("Task {task_id} not found.")),
        Err(e) => Ok(format!("Error: {e}")),
    })
}

fn manage_tasks_update(args: &serde_json::Value) -> Result<String> {
    let task_id = require_str_param(args, "task_id")?;
    let update = crate::db::UpdateTask {
        name: optional_str_param(args, "name"),
        prompt: optional_str_param(args, "prompt"),
        schedule_type: optional_str_param(args, "schedule_type"),
        schedule_expr: optional_str_param(args, "schedule_expr"),
        timezone: optional_str_param(args, "timezone"),
    };
    if let Some(st) = update.schedule_type {
        let expr = update.schedule_expr.unwrap_or("");
        if let Err(e) = tasks::validate_schedule(st, expr) {
            return Ok(format!("Error: Invalid schedule: {e}"));
        }
    } else if let Some(expr) = update.schedule_expr {
        let validation = with_db(|db| match db.get_task_by_id(task_id) {
            Ok(Some(existing)) => {
                if let Err(e) = tasks::validate_schedule(&existing.schedule_type, expr) {
                    return Ok(format!("Error: Invalid schedule: {e}"));
                }
                Ok(String::new())
            }
            Ok(None) => Ok(format!("Task {task_id} not found.")),
            Err(e) => Ok(format!("Error: {e}")),
        })?;
        if !validation.is_empty() {
            return Ok(validation);
        }
    }
    with_db(|db| match db.update_task(task_id, &update) {
        Ok(true) => Ok(format!("Task {task_id} updated.")),
        Ok(false) => Ok(format!("Task {task_id} not found.")),
        Err(e) => Ok(format!("Error: {e}")),
    })
}

fn manage_tasks_runs(args: &serde_json::Value) -> Result<String> {
    let task_id = require_str_param(args, "task_id")?;
    let limit = optional_u64_param(args, "limit", 5) as usize;
    with_db(|db| match db.task_run_history(task_id, limit) {
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
    })
}

fn manage_tasks_run_now(args: &serde_json::Value) -> Result<String> {
    let task_id = require_str_param(args, "task_id")?;
    with_db(|db| match db.get_task_by_id(task_id) {
        Ok(Some(_)) => {
            let now = chrono::Utc::now().timestamp();
            if let Err(e) = db.update_task_next_run(task_id, Some(now)) {
                return Ok(format!("Error: {e}"));
            }
            let _ = db.clear_task_retry(task_id);
            Ok(format!("Task {task_id} queued for immediate execution."))
        }
        Ok(None) => Ok(format!("Task {task_id} not found.")),
        Err(e) => Ok(format!("Error: {e}")),
    })
}

// ── Cron job management ──

pub fn handle_manage_cron(args: &serde_json::Value, _config: &Config) -> Result<String> {
    let action = require_str_param(args, "action")?;
    match action {
        "create" => manage_cron_create(args),
        "list" => manage_cron_list(),
        "get" => {
            let job_id = require_str_param(args, "job_id")?;
            with_db(|db| match db.get_task_by_id(job_id) {
                Ok(Some(task)) if task.task_type == "command" => Ok(tasks::format_task(&task)),
                Ok(Some(_)) => Ok(format!(
                    "Job {job_id} is not a cron job (it's a prompt task)."
                )),
                Ok(None) => Ok(format!("Cron job {job_id} not found.")),
                Err(e) => Ok(format!("Error: {e}")),
            })
        }
        "delete" => {
            let job_id = require_str_param(args, "job_id")?;
            with_db(|db| match db.get_task_by_id(job_id) {
                Ok(Some(task)) if task.task_type == "command" => match db.delete_task(job_id) {
                    Ok(true) => Ok(format!("Cron job {job_id} deleted.")),
                    Ok(false) => Ok(format!("Cron job {job_id} not found.")),
                    Err(e) => Ok(format!("Error: {e}")),
                },
                Ok(Some(_)) => Ok(format!(
                    "Job {job_id} is a prompt task, not a cron job. Use manage_tasks to manage it."
                )),
                Ok(None) => Ok(format!("Cron job {job_id} not found.")),
                Err(e) => Ok(format!("Error: {e}")),
            })
        }
        "pause" => {
            let job_id = require_str_param(args, "job_id")?;
            with_db(|db| match db.get_task_by_id(job_id) {
                Ok(Some(task)) if task.task_type == "command" => {
                    update_task_status(job_id, tasks::TASK_STATUS_PAUSED, "paused")
                }
                Ok(Some(_)) => Ok(format!(
                    "Job {job_id} is a prompt task, not a cron job. Use manage_tasks to manage it."
                )),
                Ok(None) => Ok(format!("Cron job {job_id} not found.")),
                Err(e) => Ok(format!("Error: {e}")),
            })
        }
        "resume" => {
            let job_id = require_str_param(args, "job_id")?;
            with_db(|db| match db.get_task_by_id(job_id) {
                Ok(Some(task)) if task.task_type == "command" => {
                    update_task_status(job_id, tasks::TASK_STATUS_ACTIVE, "resumed")
                }
                Ok(Some(_)) => Ok(format!(
                    "Job {job_id} is a prompt task, not a cron job. Use manage_tasks to manage it."
                )),
                Ok(None) => Ok(format!("Cron job {job_id} not found.")),
                Err(e) => Ok(format!("Error: {e}")),
            })
        }
        "runs" => {
            let job_id = require_str_param(args, "job_id")?;
            let limit = optional_u64_param(args, "limit", 5) as usize;
            with_db(|db| match db.task_run_history(job_id, limit) {
                Ok(runs) if runs.is_empty() => {
                    Ok(format!("No runs recorded for cron job {job_id}."))
                }
                Ok(runs) => {
                    let mut out = format!("Last {} runs for cron job {}:\n", runs.len(), job_id);
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
            })
        }
        "run_now" => {
            let job_id = require_str_param(args, "job_id")?;
            with_db(|db| match db.get_task_by_id(job_id) {
                Ok(Some(task)) if task.task_type == "command" => {
                    let now = chrono::Utc::now().timestamp();
                    if let Err(e) = db.update_task_next_run(job_id, Some(now)) {
                        return Ok(format!("Error: {e}"));
                    }
                    let _ = db.clear_task_retry(job_id);
                    Ok(format!("Cron job {job_id} queued for immediate execution."))
                }
                Ok(Some(_)) => Ok(format!(
                    "Job {job_id} is a prompt task, not a cron job. Use manage_tasks to manage it."
                )),
                Ok(None) => Ok(format!("Cron job {job_id} not found.")),
                Err(e) => Ok(format!("Error: {e}")),
            })
        }
        other => Ok(format!(
            "Unknown action: {other}. Use: create, list, get, delete, pause, resume, runs, run_now."
        )),
    }
}

fn manage_cron_create(args: &serde_json::Value) -> Result<String> {
    let schedule = require_str_param(args, "schedule")?;
    let command = require_str_param(args, "command")?;
    let name = args["name"]
        .as_str()
        .map(std::string::ToString::to_string)
        .unwrap_or_else(|| {
            let short: String = command.chars().take(30).collect();
            format!("cron: {short}")
        });

    let cron_7 = tasks::convert_5_to_7_field(schedule);
    if let Err(e) = tasks::validate_schedule("cron", &cron_7) {
        return Ok(format!("Error: Invalid schedule: {e}"));
    }

    let next_run = match tasks::calculate_next_run("cron", &cron_7) {
        Ok(nr) => nr,
        Err(e) => return Ok(format!("Error: Invalid schedule: {e}")),
    };

    let id = uuid::Uuid::new_v4().to_string();
    let (delivery_channel, delivery_target) = resolve_origin_delivery(
        optional_str_param(args, "delivery_channel").map(str::to_string),
        optional_str_param(args, "delivery_target").map(str::to_string),
    );
    with_db(|db| {
        match db.create_task(&crate::db::NewTask {
            id: &id,
            name: &name,
            prompt: command,
            schedule_type: "cron",
            schedule_expr: &cron_7,
            timezone: "local",
            next_run,
            max_retries: Some(0),
            timeout_ms: optional_i64_param(args, "timeout_ms"),
            delivery_channel: delivery_channel.as_deref(),
            delivery_target: delivery_target.as_deref(),
            allowed_tools: None,
            task_type: "command",
        }) {
            Ok(()) => Ok(format!(
                "Cron job created: {name} (id: {})\n  Schedule: {schedule}\n  Command: {command}",
                &id[..8]
            )),
            Err(e) => Ok(format!("Error creating cron job: {e}")),
        }
    })
}

fn manage_cron_list() -> Result<String> {
    with_db(|db| match db.list_tasks() {
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
    })
}

pub fn update_task_status(task_id: &str, status: &str, verb: &str) -> Result<String> {
    with_db(|db| match db.update_task_status(task_id, status) {
        Ok(true) => Ok(format!("Task {task_id} {verb}.")),
        Ok(false) => Ok(format!("Task {task_id} not found.")),
        Err(e) => Ok(format!("Error: {e}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn handle_manage_tasks_unknown_action() {
        let result = handle_manage_tasks(&json!({"action": "nope"}), &Config::default()).unwrap();
        assert!(result.contains("Unknown action"));
    }

    #[test]
    fn handle_manage_tasks_missing_action() {
        let result = handle_manage_tasks(&json!({}), &Config::default());
        assert!(result.is_err());
    }

    #[test]
    fn handle_manage_tasks_create_invalid_schedule() {
        let args = json!({
            "action": "create",
            "name": "test",
            "prompt": "do stuff",
            "schedule_type": "cron",
            "schedule_expr": "not a cron"
        });
        let result = handle_manage_tasks(&args, &Config::default()).unwrap();
        assert!(
            result.contains("Invalid schedule") || result.contains("Error"),
            "expected schedule error in: {result}"
        );
    }

    #[test]
    fn handle_manage_tasks_create_missing_name() {
        let args = json!({
            "action": "create",
            "prompt": "do stuff",
            "schedule_expr": "30m"
        });
        let result = handle_manage_tasks(&args, &Config::default());
        assert!(result.is_err());
    }
}
