//! Borg's own work as an event source: goals that finish or get stuck,
//! scheduled tasks that fail or succeed, heartbeat turns that fail.
//!
//! Each event goes through the same fan-out as a channel message, with the
//! session it was born in, so an event caused by a subscription's own run is
//! never fed back to a subscription: that is the loop guard.

use anyhow::Result;
use cortex::config::Config;
use cortex::db::{Database, ScheduledTaskRow, TaskRunRow};
use cortex::goals::event_kind;
use cortex::subscriptions::ingest::{fan_out, FanoutEvent};
use cortex::subscriptions::Source;
use serde_json::{json, Value};
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;

use crate::goal_bus::{DaemonGoalEvent, GoalEventBus};
use crate::heartbeat::{DaemonHeartbeatEvent, HeartbeatBus};

/// The preset every internal event belongs to.
const PROVIDER: &str = "borg";

/// The payload for a goal event worth waking a subscription for.
pub(crate) fn goal_payload(evt: &DaemonGoalEvent) -> Option<Value> {
    let what = match evt.kind.as_str() {
        event_kind::COMPLETED => "completed",
        event_kind::BLOCKED => "blocked",
        event_kind::BUDGET_LIMITED => "budget_limited",
        _ => return None,
    };
    let goal = evt.goal.as_ref()?;
    let objective: String = goal.objective.chars().take(160).collect();
    let summary = match what {
        "completed" => format!("Goal completed: {objective}"),
        "blocked" => format!("Goal is stuck: {objective}"),
        _ => format!("Goal ran out of budget: {objective}"),
    };
    Some(json!({
        "id": format!("goal:{}:{}:{what}", evt.session_id, goal.goal_id),
        "kind": format!("goal.{what}"),
        "summary": summary,
        "session_id": evt.session_id,
        "objective": goal.objective,
        "tokens_used": goal.tokens_used,
    }))
}

/// The payload for a failed heartbeat turn. Skips and successes are the
/// heartbeat's normal rhythm, not events.
pub(crate) fn heartbeat_payload(evt: &DaemonHeartbeatEvent) -> Option<Value> {
    if evt.status != "failed" {
        return None;
    }
    let message: String = evt.message.chars().take(300).collect();
    Some(json!({
        "id": format!("heartbeat:{}:{}", evt.source.as_wire(), evt.started_at_unix),
        "kind": "heartbeat.failed",
        "summary": format!("Heartbeat check-in failed: {message}"),
        "error": evt.message,
    }))
}

/// The payload for a finished run of a user's scheduled task. Borg's own
/// system tasks (heartbeat, maintenance, memory consolidation) run all the
/// time and are left out.
pub(crate) fn task_payload(task: &ScheduledTaskRow, run: &TaskRunRow) -> Option<Value> {
    if matches!(task.task_type.as_str(), "heartbeat" | "maintenance")
        || cortex::memory::consolidation::is_consolidation_task_id(&task.id)
    {
        return None;
    }
    let failed = run.error.is_some() || run.status == "failed";
    let (what, summary) = if failed {
        let error = run.error.as_deref().unwrap_or("unknown error");
        let error: String = error.chars().take(200).collect();
        (
            "failed",
            format!("Scheduled task \"{}\" failed: {error}", task.name),
        )
    } else {
        (
            "succeeded",
            format!("Scheduled task \"{}\" finished", task.name),
        )
    };
    Some(json!({
        "id": format!("task:{}:{}", task.id, run.id),
        "kind": format!("task.{what}"),
        "summary": summary,
        "task_id": task.id,
        "task_name": task.name,
        "error": run.error,
        "result": run.result.as_deref().map(|r| r.chars().take(2000).collect::<String>()),
        "duration_ms": run.duration_ms,
    }))
}

async fn store(db: &Database, config: &Config, payload: &Value, born_in: Option<&str>) {
    let ev = FanoutEvent {
        source: Source::Internal,
        provider: PROVIDER,
        payload,
        origin: None,
        born_in_session: born_in,
    };
    if let Err(e) = fan_out(db, &ev, &config.subscriptions).await {
        tracing::warn!("subscriptions: could not store a Borg event: {e:#}");
    }
}

/// Called by the task runner after a run settles.
pub(crate) async fn after_task_run(task: &ScheduledTaskRow, run_id: i64, config: &Config) {
    if !config.subscriptions.enabled {
        return;
    }
    let db = match Database::open().await {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!("subscriptions: open db after a task run: {e}");
            return;
        }
    };
    let run = match db.task_run_history(&task.id, 5).await {
        Ok(runs) => runs.into_iter().find(|r| r.id == run_id),
        Err(e) => {
            tracing::warn!("subscriptions: read task run {run_id}: {e}");
            return;
        }
    };
    if let Some(payload) = run.as_ref().and_then(|r| task_payload(task, r)) {
        store(&db, config, &payload, None).await;
    }
}

/// Listen to the goal and heartbeat buses until `cancel`.
pub(crate) async fn run(
    goal_bus: GoalEventBus,
    heartbeat_bus: HeartbeatBus,
    config_rx: watch::Receiver<Config>,
    cancel: CancellationToken,
) -> Result<()> {
    let mut goals = goal_bus.subscribe();
    let mut beats = heartbeat_bus.subscribe();
    let db = Database::open().await?;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            evt = goals.recv() => match evt {
                Ok(evt) => {
                    if let Some(payload) = goal_payload(&evt) {
                        let config = config_rx.borrow().clone();
                        store(&db, &config, &payload, Some(&evt.session_id)).await;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("subscriptions: missed {n} goal events");
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            evt = beats.recv() => match evt {
                Ok(evt) => {
                    if let Some(payload) = heartbeat_payload(&evt) {
                        let config = config_rx.borrow().clone();
                        store(&db, &config, &payload, None).await;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("subscriptions: missed {n} heartbeat events");
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex::goals::{Goal, GoalStatus};

    fn goal_event(kind: &str) -> DaemonGoalEvent {
        DaemonGoalEvent {
            session_id: "s1".into(),
            kind: kind.into(),
            goal: Some(Goal {
                session_id: "s1".into(),
                goal_id: "g1".into(),
                objective: "ship the release".into(),
                status: GoalStatus::Active,
                token_budget: None,
                tokens_used: 10,
                time_used_seconds: 0,
                created_at: 0,
                updated_at: 0,
            }),
            emitted_at_unix: 0,
        }
    }

    #[test]
    fn only_goal_outcomes_become_events() {
        let blocked = goal_payload(&goal_event(event_kind::BLOCKED)).unwrap();
        assert_eq!(blocked["kind"], "goal.blocked");
        assert_eq!(blocked["id"], "goal:s1:g1:blocked");
        assert_eq!(blocked["summary"], "Goal is stuck: ship the release");
        for quiet in [
            event_kind::CREATED,
            event_kind::STATUS_CHANGED,
            event_kind::CLEARED,
        ] {
            assert!(goal_payload(&goal_event(quiet)).is_none(), "{quiet}");
        }
        let preset = cortex::subscriptions::presets::get(PROVIDER).unwrap();
        for kind in [
            event_kind::COMPLETED,
            event_kind::BLOCKED,
            event_kind::BUDGET_LIMITED,
        ] {
            let p = goal_payload(&goal_event(kind)).unwrap();
            let k = p["kind"].as_str().unwrap();
            assert!(preset.kinds.contains(&k), "{k} is not offered in the form");
        }
    }

    fn task(task_type: &str, id: &str) -> ScheduledTaskRow {
        ScheduledTaskRow {
            id: id.to_string(),
            name: "Nightly report".to_string(),
            prompt: "do the thing".to_string(),
            schedule_type: "cron".to_string(),
            schedule_expr: "0 3 * * *".to_string(),
            timezone: "local".to_string(),
            status: "active".to_string(),
            next_run: None,
            created_at: 0,
            max_retries: 3,
            retry_count: 0,
            retry_after: None,
            last_error: None,
            timeout_ms: 300_000,
            delivery_channel: None,
            delivery_target: None,
            allowed_tools: None,
            task_type: task_type.to_string(),
            pre_run_command: None,
            pre_run_cwd: None,
            pre_run_timeout_ms: 60_000,
            bypass_quiet_hours_once: false,
            min_interval_secs: None,
            last_response_hash: None,
            model: None,
            model_provider: None,
            monitor_mode: false,
            last_pre_run_hash: None,
            continuity: false,
            run_now_requested: false,
            failure_delivery_channel: None,
            failure_delivery_target: None,
        }
    }

    fn task_run(error: Option<&str>) -> TaskRunRow {
        TaskRunRow {
            id: 981,
            task_id: "42".into(),
            started_at: 0,
            duration_ms: 5,
            result: None,
            error: error.map(str::to_string),
            status: if error.is_some() { "failed" } else { "success" }.into(),
        }
    }

    #[test]
    fn user_task_runs_become_events_and_system_tasks_do_not() {
        let failed = task_payload(&task("prompt", "42"), &task_run(Some("exit status 1"))).unwrap();
        assert_eq!(failed["kind"], "task.failed");
        assert_eq!(failed["id"], "task:42:981");
        assert_eq!(
            failed["summary"],
            "Scheduled task \"Nightly report\" failed: exit status 1"
        );
        let ok = task_payload(&task("command", "42"), &task_run(None)).unwrap();
        assert_eq!(ok["kind"], "task.succeeded");
        assert!(task_payload(&task("heartbeat", "hb"), &task_run(Some("x"))).is_none());
        assert!(task_payload(&task("maintenance", "m"), &task_run(None)).is_none());
    }
}
