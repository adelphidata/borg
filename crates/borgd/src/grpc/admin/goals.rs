//! Goal RPC bodies for the admin service (`super::AdminSvc`).

use super::*;

/// Boxed goal-event stream; used as the `Admin::SubscribeGoalEventsStream`
/// associated type in the parent impl.
pub(super) type GoalEventsStream =
    Pin<Box<dyn Stream<Item = Result<PbGoalEvent, TStatus>> + Send + 'static>>;

pub(super) async fn get_goal(req: Request<GoalRef>) -> Result<Response<GoalDetail>, TStatus> {
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
    .map_err(map_internal("get_goal"))?;
    Ok(Response::new(detail))
}

pub(super) async fn mutate_goal_gates(
    req: Request<protocols::admin::GoalGateMutation>,
) -> Result<Response<protocols::admin::GoalGateList>, TStatus> {
    use protocols::admin::goal_gate_mutation::Op;

    let req = req.into_inner();
    let op = Op::try_from(req.op).unwrap_or(Op::Unspecified);
    let reply = async {
        let db = Database::open().await?;
        let Some(session_id) = resolve_goal_session_id(&db, &req.session_id).await? else {
            return anyhow::Ok(protocols::admin::GoalGateList {
                gates: Vec::new(),
                report: "no session yet — set a goal first".to_string(),
            });
        };
        let report = match op {
            Op::Add => {
                let command = req.command.trim();
                if command.is_empty() {
                    return anyhow::Ok(protocols::admin::GoalGateList {
                        gates: gates_to_proto(&db, &session_id).await?,
                        report: "gate command must not be empty".to_string(),
                    });
                }
                let gate = db.add_goal_gate(&session_id, command).await?;
                format!("gate {} added: {}", gate.ordinal, gate.command)
            }
            Op::Remove => {
                if db.remove_goal_gate(&session_id, req.ordinal).await? {
                    format!("gate {} removed", req.ordinal)
                } else {
                    format!("no gate {}", req.ordinal)
                }
            }
            Op::Clear => {
                let n = db.clear_goal_gates(&session_id).await?;
                format!("{n} gate(s) removed")
            }
            Op::List | Op::Unspecified => String::new(),
        };
        anyhow::Ok(protocols::admin::GoalGateList {
            gates: gates_to_proto(&db, &session_id).await?,
            report,
        })
    }
    .await
    .map_err(map_internal("mutate_goal_gates"))?;
    Ok(Response::new(reply))
}

/// Current gates for `session_id`, projected onto the wire type.
async fn gates_to_proto(
    db: &Database,
    session_id: &str,
) -> anyhow::Result<Vec<protocols::admin::GoalGate>> {
    Ok(db
        .list_goal_gates(session_id)
        .await?
        .into_iter()
        .map(|g| protocols::admin::GoalGate {
            ordinal: g.ordinal,
            command: g.command,
            attempts: g.attempts,
            last_exit_code_present: g.last_exit_code.is_some(),
            last_exit_code: g.last_exit_code.unwrap_or(0),
        })
        .collect())
}

pub(super) async fn mutate_session_heartbeat(
    req: Request<protocols::admin::SessionHeartbeatMutation>,
) -> Result<Response<protocols::admin::SessionHeartbeatDetail>, TStatus> {
    use protocols::admin::session_heartbeat_mutation::Op;

    let req = req.into_inner();
    let op = Op::try_from(req.op).unwrap_or(Op::Unspecified);
    let detail = async {
        let db = Database::open().await?;
        let Some(session_id) = resolve_goal_session_id(&db, &req.session_id).await? else {
            return anyhow::Ok(protocols::admin::SessionHeartbeatDetail {
                report: "no session yet".to_string(),
                ..Default::default()
            });
        };
        let report = match op {
            Op::Set => {
                let prompt = req.prompt.trim();
                if prompt.is_empty() {
                    return anyhow::Ok(
                        heartbeat_detail(&db, &session_id, "heartbeat prompt must not be empty")
                            .await?,
                    );
                }
                // The detail under the report shows the interval and the
                // next fire, in the units the user typed.
                db.set_session_heartbeat(&session_id, prompt, req.interval_secs)
                    .await?;
                "set".to_string()
            }
            Op::Pause => match db.set_session_heartbeat_paused(&session_id, true).await? {
                Some(_) => "paused".to_string(),
                None => "no heartbeat set".to_string(),
            },
            Op::Resume => match db.set_session_heartbeat_paused(&session_id, false).await? {
                Some(_) => "resumed".to_string(),
                None => "no heartbeat set".to_string(),
            },
            Op::Clear => {
                if db.clear_session_heartbeat(&session_id).await? {
                    "cleared".to_string()
                } else {
                    "no heartbeat set".to_string()
                }
            }
            Op::Status | Op::Unspecified => String::new(),
        };
        anyhow::Ok(heartbeat_detail(&db, &session_id, &report).await?)
    }
    .await
    .map_err(map_internal("mutate_session_heartbeat"))?;
    Ok(Response::new(detail))
}

/// Current heartbeat state for `session_id`, projected onto the wire type.
async fn heartbeat_detail(
    db: &Database,
    session_id: &str,
    report: &str,
) -> anyhow::Result<protocols::admin::SessionHeartbeatDetail> {
    let hb = db.get_session_heartbeat(session_id).await?;
    Ok(match hb {
        Some(hb) => protocols::admin::SessionHeartbeatDetail {
            present: true,
            session_id: hb.session_id.clone(),
            prompt: hb.prompt.clone(),
            interval_secs: hb.interval_secs,
            paused: hb.paused,
            seconds_until_due: hb.seconds_until_due(now_unix()),
            report: report.to_string(),
        },
        None => protocols::admin::SessionHeartbeatDetail {
            session_id: session_id.to_string(),
            report: report.to_string(),
            ..Default::default()
        },
    })
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
        // The ceiling is a DB-backed setting, so it is read per mutation
        // rather than captured at daemon start — `borg settings set` must take
        // effect on the next `/goal`, not the next restart.
        let max_budget = cortex::config::Config::load_from_db()
            .await
            .map_err(SetGoalError::Internal)?
            .goals
            .max_token_budget;
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
                apply_goal_create(&db, &session_id, &objective, token_budget, max_budget).await
            }
            GoalMutationOp::Replace => {
                apply_goal_replace(&db, &session_id, &objective, token_budget, max_budget).await
            }
            GoalMutationOp::SetStatus => apply_goal_status(&db, &session_id, &status_raw).await,
            GoalMutationOp::SetBudget => {
                apply_goal_budget(&db, &session_id, token_budget, max_budget).await
            }
        }
    }
    .await
    .map_err(|e| match e {
        SetGoalError::Invalid(msg) => TStatus::invalid_argument(format!("set_goal: {msg}")),
        SetGoalError::Precondition(msg) => TStatus::failed_precondition(format!("set_goal: {msg}")),
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

/// `SetGoal` op=Create: validate and insert a new goal row.
async fn apply_goal_create(
    db: &Database,
    session_id: &str,
    objective: &str,
    token_budget: Option<i64>,
    max_budget: Option<i64>,
) -> Result<SetGoalOutcome, SetGoalError> {
    let obj = cortex::goals::validate_objective(objective).map_err(SetGoalError::Invalid)?;
    let budget =
        cortex::goals::validate_budget(token_budget, max_budget).map_err(SetGoalError::Invalid)?;
    let inserted = db
        .insert_goal(session_id, &obj, cortex::goals::GoalStatus::Active, budget)
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

/// `SetGoal` op=Replace: validate and upsert, reporting Created vs Replaced.
async fn apply_goal_replace(
    db: &Database,
    session_id: &str,
    objective: &str,
    token_budget: Option<i64>,
    max_budget: Option<i64>,
) -> Result<SetGoalOutcome, SetGoalError> {
    let obj = cortex::goals::validate_objective(objective).map_err(SetGoalError::Invalid)?;
    let budget =
        cortex::goals::validate_budget(token_budget, max_budget).map_err(SetGoalError::Invalid)?;
    let existed = db
        .get_goal(session_id)
        .await
        .map_err(SetGoalError::Internal)?
        .is_some();
    let g = db
        .replace_goal(session_id, &obj, cortex::goals::GoalStatus::Active, budget)
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

/// `SetGoal` op=SetStatus: enforce the legal transition table and apply.
async fn apply_goal_status(
    db: &Database,
    session_id: &str,
    status_raw: &str,
) -> Result<SetGoalOutcome, SetGoalError> {
    let new_status = cortex::goals::GoalStatus::from_str(status_raw.trim()).ok_or_else(|| {
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
        .get_goal(session_id)
        .await
        .map_err(SetGoalError::Internal)?
        .ok_or_else(|| SetGoalError::Precondition("no goal set for this session".into()))?;
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
            session_id,
            cortex::db::GoalUpdate {
                status: Some(new_status),
                expected_goal_id: Some(current.goal_id),
                ..Default::default()
            },
        )
        .await
        .map_err(SetGoalError::Internal)?;
    let goal = updated.ok_or_else(|| {
        SetGoalError::Precondition("goal changed concurrently during status update".into())
    })?;
    // Returning to Active grants a fresh retry budget. A gate that
    // exhausted its budget tells the operator to `/goal resume`;
    // without this the first run after resuming re-reaches the cap
    // and re-pauses immediately, making that advice a lie.
    if goal.status == cortex::goals::GoalStatus::Active {
        if let Err(e) = db.reset_goal_gate_attempts(session_id).await {
            tracing::warn!("resetting goal gate attempts on resume failed: {e}");
        }
    }
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

/// `SetGoal` op=SetBudget: apply the new cap and re-evaluate the
/// budget-limit invariant.
async fn apply_goal_budget(
    db: &Database,
    session_id: &str,
    token_budget: Option<i64>,
    max_budget: Option<i64>,
) -> Result<SetGoalOutcome, SetGoalError> {
    let budget =
        cortex::goals::validate_budget(token_budget, max_budget).map_err(SetGoalError::Invalid)?;
    let current = db
        .get_goal(session_id)
        .await
        .map_err(SetGoalError::Internal)?
        .ok_or_else(|| SetGoalError::Precondition("no goal set for this session".into()))?;
    // Re-evaluate the budget-limit invariant (limited iff the new
    // budget is finite and already met) so a budget change resumes
    // or limits the goal instead of silently leaving it stuck. Only
    // the Active<->BudgetLimited pair is auto-flipped; Paused,
    // Blocked, and Complete are user/agent states we never override
    // here. Raising the cap on a BudgetLimited goal is the sole way
    // to resume it — SetStatus has no BudgetLimited->Active edge.
    use cortex::goals::GoalStatus as S;
    let over_budget = budget.is_some_and(|b| current.tokens_used >= b);
    let new_status = match current.status {
        S::Active if over_budget => Some(S::BudgetLimited),
        S::BudgetLimited if !over_budget => Some(S::Active),
        _ => None,
    };
    let updated = db
        .update_goal(
            session_id,
            cortex::db::GoalUpdate {
                token_budget: Some(budget),
                status: new_status,
                expected_goal_id: Some(current.goal_id),
                ..Default::default()
            },
        )
        .await
        .map_err(SetGoalError::Internal)?;
    let goal = updated.ok_or_else(|| {
        SetGoalError::Precondition("goal changed concurrently during budget update".into())
    })?;
    Ok(SetGoalOutcome {
        goal: Some(goal),
        kind: cortex::goals::event_kind::BUDGET_CHANGED,
    })
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
    .map_err(map_internal("clear_goal"))?;
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
    .map_err(map_internal("pause_goal"))?;
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
