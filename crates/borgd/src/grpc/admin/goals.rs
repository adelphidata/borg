//! Goal RPC bodies for the admin service (`super::AdminSvc`).

use super::*;

/// Boxed goal-event stream; structurally identical to the
/// `Admin::SubscribeGoalEventsStream` associated type in the parent impl.
pub(super) type GoalEventsStream =
    Pin<Box<dyn Stream<Item = Result<PbGoalEvent, TStatus>> + Send + 'static>>;

pub(super) async fn get_goal(
    _svc: &AdminSvc,
    req: Request<GoalRef>,
) -> Result<Response<GoalDetail>, TStatus> {
    let session_id_req = req.into_inner().session_id;
    let detail = async {
        let db = Database::open().await?;
        let Some(session_id) = resolve_goal_session_id(&db, &session_id_req).await? else {
            return anyhow::Ok(GoalDetail::default());
        };
        Ok(db
            .get_goal(&session_id)
            .await?
            .map(|g| goal_to_detail(&g))
            .unwrap_or_default())
    }
    .await
    .map_err(|e| TStatus::internal(format!("get_goal failed: {e}")))?;
    Ok(Response::new(detail))
}

pub(super) async fn set_goal(
    svc: &AdminSvc,
    req: Request<GoalMutation>,
) -> Result<Response<GoalDetail>, TStatus> {
    let req = req.into_inner();
    let op = GoalMutationOp::try_from(req.op)
        .map_err(|_| TStatus::invalid_argument(format!("unknown op discriminator: {}", req.op)))?;
    if op == GoalMutationOp::Unspecified {
        return Err(TStatus::invalid_argument("op is required (UNSPECIFIED)"));
    }
    let session_id_req = req.session_id.clone();
    let objective = req.objective.clone();
    let status_raw = req.status.clone();
    let token_budget = if req.token_budget_present {
        Some(req.token_budget)
    } else {
        None
    };
    let bus = svc.goal_bus_handle();
    let outcome = async {
        let db = Database::open().await.map_err(SetGoalError::Internal)?;
        // Create/Replace autocreate a session when none exists so `/goal
        // <text>` works on a freshly-launched TUI without forcing the user
        // to send a message first. Status/budget mutations still require
        // an existing goal row, so they use the strict resolver below.
        let session_id = match op {
            GoalMutationOp::Create | GoalMutationOp::Replace => {
                resolve_or_create_goal_session_id(&db, &session_id_req)
                    .await
                    .map_err(SetGoalError::Internal)?
            }
            _ => resolve_goal_session_id(&db, &session_id_req)
                .await
                .map_err(SetGoalError::Internal)?
                .ok_or_else(|| {
                    SetGoalError::Precondition("no active session — start a turn first".into())
                })?,
        };
        match op {
            GoalMutationOp::Unspecified => unreachable!(),
            GoalMutationOp::Create => {
                let obj = cortex::goals::validate_objective(&objective)
                    .map_err(SetGoalError::Invalid)?;
                let budget = cortex::goals::validate_budget(token_budget)
                    .map_err(SetGoalError::Invalid)?;
                let inserted = db
                    .insert_goal(&session_id, &obj, cortex::goals::GoalStatus::Active, budget)
                    .await
                    .map_err(SetGoalError::Internal)?;
                let goal = inserted.ok_or_else(|| {
                    SetGoalError::AlreadyExists("goal already exists for this session".into())
                })?;
                Ok(SetGoalOutcome {
                    goal: Some(goal),
                    kind: cortex::goals::event_kind::CREATED,
                })
            }
            GoalMutationOp::Replace => {
                let obj = cortex::goals::validate_objective(&objective)
                    .map_err(SetGoalError::Invalid)?;
                let budget = cortex::goals::validate_budget(token_budget)
                    .map_err(SetGoalError::Invalid)?;
                let existed = db
                    .get_goal(&session_id)
                    .await
                    .map_err(SetGoalError::Internal)?
                    .is_some();
                let g = db
                    .replace_goal(&session_id, &obj, cortex::goals::GoalStatus::Active, budget)
                    .await
                    .map_err(SetGoalError::Internal)?;
                Ok(SetGoalOutcome {
                    goal: Some(g),
                    kind: if existed {
                        cortex::goals::event_kind::REPLACED
                    } else {
                        cortex::goals::event_kind::CREATED
                    },
                })
            }
            GoalMutationOp::SetStatus => {
                let new_status = cortex::goals::GoalStatus::from_str(status_raw.trim())
                    .ok_or_else(|| {
                        SetGoalError::Invalid(format!(
                            "status must be one of active|paused|complete (got {status_raw:?}); blocked and budget_limited are system-only"
                        ))
                    })?;
                if matches!(new_status, cortex::goals::GoalStatus::BudgetLimited) {
                    return Err(SetGoalError::Invalid(
                        "budget_limited is system-only; cannot be set via RPC".into(),
                    ));
                }
                if matches!(new_status, cortex::goals::GoalStatus::Blocked) {
                    return Err(SetGoalError::Invalid(
                        "blocked is agent-only; cannot be set via RPC".into(),
                    ));
                }
                let current = db
                    .get_goal(&session_id)
                    .await
                    .map_err(SetGoalError::Internal)?
                    .ok_or_else(|| {
                        SetGoalError::Precondition("no goal set for this session".into())
                    })?;
                use cortex::goals::GoalStatus as S;
                let allowed = matches!(
                    (current.status, new_status),
                    (S::Active, S::Paused)
                        | (S::Paused, S::Active)
                        | (S::Active, S::Complete)
                        | (S::Paused, S::Complete)
                        | (S::BudgetLimited, S::Complete)
                        // Blocked is set by the agent; the user resumes
                        // it back to Active or marks it Complete.
                        | (S::Blocked, S::Active)
                        | (S::Blocked, S::Complete)
                );
                if !allowed {
                    return Err(SetGoalError::Invalid(format!(
                        "illegal transition {} -> {}",
                        current.status.as_str(),
                        new_status.as_str()
                    )));
                }
                let updated = db
                    .update_goal(
                        &session_id,
                        cortex::db::GoalUpdate {
                            status: Some(new_status),
                            expected_goal_id: Some(current.goal_id),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(SetGoalError::Internal)?;
                let goal = updated.ok_or_else(|| {
                    SetGoalError::Precondition(
                        "goal changed concurrently during status update".into(),
                    )
                })?;
                let kind = if goal.status == cortex::goals::GoalStatus::Complete {
                    cortex::goals::event_kind::COMPLETED
                } else {
                    cortex::goals::event_kind::STATUS_CHANGED
                };
                Ok(SetGoalOutcome {
                    goal: Some(goal),
                    kind,
                })
            }
            GoalMutationOp::SetBudget => {
                let budget = cortex::goals::validate_budget(token_budget)
                    .map_err(SetGoalError::Invalid)?;
                let current = db
                    .get_goal(&session_id)
                    .await
                    .map_err(SetGoalError::Internal)?
                    .ok_or_else(|| {
                        SetGoalError::Precondition("no goal set for this session".into())
                    })?;
                let updated = db
                    .update_goal(
                        &session_id,
                        cortex::db::GoalUpdate {
                            token_budget: Some(budget),
                            expected_goal_id: Some(current.goal_id),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(SetGoalError::Internal)?;
                let goal = updated.ok_or_else(|| {
                    SetGoalError::Precondition(
                        "goal changed concurrently during budget update".into(),
                    )
                })?;
                Ok(SetGoalOutcome {
                    goal: Some(goal),
                    kind: cortex::goals::event_kind::BUDGET_CHANGED,
                })
            }
        }
    }
    .await
    .map_err(|e| match e {
        SetGoalError::Invalid(msg) => TStatus::invalid_argument(format!("set_goal: {msg}")),
        SetGoalError::Precondition(msg) => {
            TStatus::failed_precondition(format!("set_goal: {msg}"))
        }
        SetGoalError::AlreadyExists(msg) => TStatus::already_exists(format!("set_goal: {msg}")),
        SetGoalError::Internal(err) => TStatus::internal(format!("set_goal: {err}")),
    })?;

    let detail = outcome
        .goal
        .as_ref()
        .map(goal_to_detail)
        .unwrap_or_default();
    if let (Some(bus), Some(goal)) = (bus, outcome.goal.as_ref()) {
        bus.publish(crate::goal_bus::DaemonGoalEvent {
            session_id: goal.session_id.clone(),
            kind: outcome.kind.to_string(),
            goal: Some(goal.clone()),
            emitted_at_unix: now_unix(),
        });
    }
    Ok(Response::new(detail))
}

pub(super) async fn clear_goal(
    svc: &AdminSvc,
    req: Request<GoalRef>,
) -> Result<Response<Empty>, TStatus> {
    let session_id_req = req.into_inner().session_id;
    let bus = svc.goal_bus_handle();
    let removed = async {
        let db = Database::open().await?;
        let Some(session_id) = resolve_goal_session_id(&db, &session_id_req).await? else {
            return anyhow::Ok(None);
        };
        let did_remove = db.delete_goal(&session_id).await?;
        Ok(if did_remove { Some(session_id) } else { None })
    }
    .await
    .map_err(|e| TStatus::internal(format!("clear_goal failed: {e}")))?;
    if let (Some(bus), Some(session_id)) = (bus, removed) {
        bus.publish(crate::goal_bus::DaemonGoalEvent {
            session_id,
            kind: cortex::goals::event_kind::CLEARED.to_string(),
            goal: None,
            emitted_at_unix: now_unix(),
        });
    }
    Ok(Response::new(Empty {}))
}

pub(super) async fn pause_goal_on_interrupt(
    svc: &AdminSvc,
    req: Request<GoalRef>,
) -> Result<Response<GoalDetail>, TStatus> {
    let session_id_req = req.into_inner().session_id;
    let bus = svc.goal_bus_handle();
    let paused = async {
        let db = Database::open().await?;
        let Some(session_id) = resolve_goal_session_id(&db, &session_id_req).await? else {
            return anyhow::Ok(None);
        };
        db.pause_active_goal(&session_id).await
    }
    .await
    .map_err(|e: anyhow::Error| TStatus::internal(format!("pause_goal failed: {e}")))?;
    let detail = paused.as_ref().map(goal_to_detail).unwrap_or_default();
    if let (Some(bus), Some(goal)) = (bus, paused.as_ref()) {
        bus.publish(crate::goal_bus::DaemonGoalEvent {
            session_id: goal.session_id.clone(),
            kind: cortex::goals::event_kind::STATUS_CHANGED.to_string(),
            goal: Some(goal.clone()),
            emitted_at_unix: now_unix(),
        });
    }
    Ok(Response::new(detail))
}

pub(super) async fn subscribe_goal_events(
    svc: &AdminSvc,
    _req: Request<Empty>,
) -> Result<Response<GoalEventsStream>, TStatus> {
    let bus = svc
        .goal_bus_handle()
        .ok_or_else(|| TStatus::failed_precondition("goal event bus not wired"))?;
    let mut sub = bus.subscribe();
    let (tx, rx) = mpsc::channel::<Result<PbGoalEvent, TStatus>>(16);
    tokio::spawn(async move {
        loop {
            match sub.recv().await {
                Ok(evt) => {
                    let pb = PbGoalEvent {
                        session_id: evt.session_id,
                        kind: evt.kind,
                        goal: Some(evt.goal.as_ref().map(goal_to_detail).unwrap_or_default()),
                        emitted_at_unix: evt.emitted_at_unix,
                    };
                    if tx.send(Ok(pb)).await.is_err() {
                        return; // client gone
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("goal stream subscriber lagged by {n}; continuing");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
}
