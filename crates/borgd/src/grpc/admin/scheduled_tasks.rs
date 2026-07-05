//! Scheduled-task RPC bodies for the admin service (`super::AdminSvc`).

use super::*;

pub(super) async fn list_scheduled_tasks(
    _svc: &AdminSvc,
    _req: Request<Empty>,
) -> Result<Response<ScheduledTaskList>, TStatus> {
    let rows = async {
        let db = Database::open().await?;
        db.list_tasks().await
    }
    .await
    .map_err(|e: anyhow::Error| TStatus::internal(format!("list_tasks failed: {e}")))?;
    Ok(Response::new(ScheduledTaskList {
        tasks: rows.into_iter().map(scheduled_task_row_to_proto).collect(),
    }))
}

pub(super) async fn create_scheduled_task(
    _svc: &AdminSvc,
    req: Request<ScheduledTaskCreate>,
) -> Result<Response<ScheduledTask>, TStatus> {
    let req = req.into_inner();
    let id = uuid::Uuid::new_v4().to_string();
    let id_for_lookup = id.clone();
    let inserted = async {
        use cortex::db::NewTask;
        let db = Database::open().await?;
        let task_type = if req.task_type.is_empty() {
            "prompt"
        } else {
            req.task_type.as_str()
        };
        let new_task = NewTask {
            id: &id,
            name: &req.name,
            prompt: &req.prompt,
            schedule_type: &req.schedule_type,
            schedule_expr: &req.schedule_expr,
            timezone: &req.timezone,
            next_run: opt_if_positive_i64(req.next_run_unix),
            max_retries: opt_if_non_negative_i32(req.max_retries),
            timeout_ms: opt_if_positive_i64(req.timeout_ms),
            delivery_channel: opt_if_not_empty(&req.delivery_channel),
            delivery_target: opt_if_not_empty(&req.delivery_target),
            allowed_tools: opt_if_not_empty(&req.allowed_tools),
            task_type,
            pre_run_command: None,
            pre_run_cwd: None,
            pre_run_timeout_ms: None,
            min_interval_secs: None,
        };
        db.create_task(&new_task).await?;
        anyhow::Ok(db.list_tasks().await?.into_iter().find(|t| t.id == id))
    }
    .await
    .map_err(map_invalid_arg("create_task"))?
    .ok_or_else(|| {
        TStatus::internal(format!(
            "create_task: row `{id_for_lookup}` not found after insert"
        ))
    })?;
    Ok(Response::new(scheduled_task_row_to_proto(inserted)))
}

pub(super) async fn delete_scheduled_task(
    _svc: &AdminSvc,
    req: Request<ScheduledTaskRef>,
) -> Result<Response<Empty>, TStatus> {
    let id = req.into_inner().id;
    let removed = async {
        let db = Database::open().await?;
        db.delete_task(&id).await
    }
    .await
    .map_err(|e: anyhow::Error| TStatus::internal(format!("delete_task failed: {e}")))?;
    if !removed {
        return Err(TStatus::not_found("scheduled task not found"));
    }
    Ok(Response::new(Empty {}))
}

pub(super) async fn update_scheduled_task(
    _svc: &AdminSvc,
    req: Request<ScheduledTaskUpdate>,
) -> Result<Response<ScheduledTask>, TStatus> {
    let req = req.into_inner();
    let id_for_lookup = req.id.clone();
    let updated = async {
        use cortex::db::UpdateTask;
        let db = Database::open().await?;
        // Empty string sentinel = "leave unchanged", matching the
        // ScheduledTaskUpdate proto semantics + Option<&str> on UpdateTask.
        let update = UpdateTask {
            name: opt_if_not_empty(&req.name),
            prompt: opt_if_not_empty(&req.prompt),
            schedule_type: opt_if_not_empty(&req.schedule_type),
            schedule_expr: opt_if_not_empty(&req.schedule_expr),
            timezone: opt_if_not_empty(&req.timezone),
        };
        let found = db.update_task(&req.id, &update).await?;
        if !found {
            return anyhow::Ok(None);
        }
        // Re-read the row so the caller observes the persisted state, not
        // a locally-merged copy that could drift if a column gets added.
        Ok(db.list_tasks().await?.into_iter().find(|t| t.id == req.id))
    }
    .await
    .map_err(map_invalid_arg("update_task"))?
    .ok_or_else(|| TStatus::not_found(format!("scheduled task `{id_for_lookup}` not found")))?;
    Ok(Response::new(scheduled_task_row_to_proto(updated)))
}

pub(super) async fn pause_scheduled_task(
    _svc: &AdminSvc,
    req: Request<ScheduledTaskRef>,
) -> Result<Response<Empty>, TStatus> {
    update_task_status(req.into_inner().id, "paused").await
}

pub(super) async fn resume_scheduled_task(
    _svc: &AdminSvc,
    req: Request<ScheduledTaskRef>,
) -> Result<Response<Empty>, TStatus> {
    update_task_status(req.into_inner().id, "active").await
}

pub(super) async fn run_scheduled_task_now(
    _svc: &AdminSvc,
    req: Request<ScheduledTaskRef>,
) -> Result<Response<Empty>, TStatus> {
    let id = req.into_inner().id;
    let id_for_err = id.clone();
    let found = async {
        let db = Database::open().await?;
        if db.get_task_by_id(&id).await?.is_none() {
            return anyhow::Ok(false);
        }
        let now = chrono::Utc::now().timestamp();
        db.update_task_next_run(&id, Some(now)).await?;
        db.clear_task_retry(&id).await?;
        Ok(true)
    }
    .await
    .map_err(|e| TStatus::internal(format!("run_scheduled_task_now failed: {e}")))?;
    if !found {
        return Err(TStatus::not_found(format!(
            "scheduled task `{id_for_err}` not found"
        )));
    }
    Ok(Response::new(Empty {}))
}

pub(super) async fn get_scheduled_task_runs(
    _svc: &AdminSvc,
    req: Request<GetScheduledTaskRunsRequest>,
) -> Result<Response<ScheduledTaskRunList>, TStatus> {
    let req = req.into_inner();
    let id = req.id;
    // 0 = caller didn't pin a limit; pick a sane default.
    let limit = if req.limit == 0 {
        10
    } else {
        req.limit as usize
    };
    let runs = async {
        let db = Database::open().await?;
        db.task_run_history(&id, limit).await
    }
    .await
    .map_err(|e: anyhow::Error| {
        TStatus::internal(format!("get_scheduled_task_runs failed: {e}"))
    })?;
    Ok(Response::new(ScheduledTaskRunList {
        runs: runs.into_iter().map(task_run_row_to_proto).collect(),
    }))
}

pub(super) async fn get_scheduled_task_detail(
    _svc: &AdminSvc,
    req: Request<ScheduledTaskRef>,
) -> Result<Response<ScheduledTaskDetail>, TStatus> {
    let id = req.into_inner().id;
    let id_for_err = id.clone();
    let detail = async {
        let db = Database::open().await?;
        let Some(row) = db.get_task_by_id(&id).await? else {
            return anyhow::Ok(None);
        };
        let last = db.last_task_run(&id).await?;
        let proto_task = scheduled_task_row_to_proto(row.clone());
        Ok(Some(ScheduledTaskDetail {
            task: Some(proto_task),
            max_retries: row.max_retries,
            retry_count: row.retry_count,
            retry_after_unix: row.retry_after.unwrap_or(0),
            last_error: row.last_error.unwrap_or_default(),
            timeout_ms: row.timeout_ms,
            has_last_run: last.is_some(),
            last_run: last.map(task_run_row_to_proto),
        }))
    }
    .await
    .map_err(|e| TStatus::internal(format!("get_scheduled_task_detail failed: {e}")))?;
    let detail = detail
        .ok_or_else(|| TStatus::not_found(format!("scheduled task `{id_for_err}` not found")))?;
    Ok(Response::new(detail))
}
