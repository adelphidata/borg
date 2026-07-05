//! Sub-agent and subprocess RPC bodies for the admin service
//! (`super::AdminSvc`).

use super::*;

/// Boxed agent-tree-event stream; structurally identical to the
/// `Admin::SubscribeAgentTreeEventsStream` associated type in the parent impl.
pub(super) type AgentTreeEventsStream =
    Pin<Box<dyn Stream<Item = Result<PbAgentTreeEvent, TStatus>> + Send + 'static>>;

pub(super) async fn list_agents(
    _svc: &AdminSvc,
    req: Request<AgentListRequest>,
) -> Result<Response<PbAgentList>, TStatus> {
    let session_id = req.into_inner().session_id;
    let db = Database::open()
        .await
        .map_err(|e| TStatus::internal(format!("db open failed: {e}")))?;
    let resolved = resolve_agent_session_id(&db, &session_id)
        .await
        .map_err(|e| TStatus::internal(format!("session resolve failed: {e}")))?;
    let Some(resolved) = resolved else {
        return Ok(Response::new(PbAgentList::default()));
    };
    let rows = db
        .list_sub_agent_runs(&resolved)
        .await
        .map_err(|e| TStatus::internal(format!("list_sub_agent_runs: {e}")))?;
    let agents: Vec<PbAgentEntry> = rows.iter().map(sub_agent_row_to_pb).collect();
    let snapshots = db
        .list_agent_tree_snapshots(&resolved)
        .await
        .map_err(|e| TStatus::internal(format!("list_agent_tree_snapshots: {e}")))?;
    let history: Vec<PbAgentSnapshotRef> = snapshots.iter().map(snapshot_row_to_pb).collect();
    Ok(Response::new(PbAgentList { agents, history }))
}

pub(super) async fn kill_agent(
    _svc: &AdminSvc,
    _req: Request<AgentRef>,
) -> Result<Response<Empty>, TStatus> {
    Err(TStatus::unimplemented(
        "kill_agent: executor wiring pending — only ListAgents/snapshots are live in this slice",
    ))
}

pub(super) async fn kill_agent_subtree(
    _svc: &AdminSvc,
    _req: Request<AgentRef>,
) -> Result<Response<Empty>, TStatus> {
    Err(TStatus::unimplemented(
        "kill_agent_subtree: executor wiring pending",
    ))
}

pub(super) async fn pause_agent(
    _svc: &AdminSvc,
    _req: Request<AgentRef>,
) -> Result<Response<Empty>, TStatus> {
    Err(TStatus::unimplemented(
        "pause_agent: executor wiring pending",
    ))
}

pub(super) async fn resume_agent(
    _svc: &AdminSvc,
    _req: Request<AgentRef>,
) -> Result<Response<Empty>, TStatus> {
    Err(TStatus::unimplemented(
        "resume_agent: executor wiring pending",
    ))
}

pub(super) async fn subscribe_agent_tree_events(
    _svc: &AdminSvc,
    _req: Request<SessionRef>,
) -> Result<Response<AgentTreeEventsStream>, TStatus> {
    Err(TStatus::unimplemented(
        "subscribe_agent_tree_events: AgentTreeBus wiring pending — poll ListAgents instead",
    ))
}

pub(super) async fn list_agent_snapshots(
    _svc: &AdminSvc,
    req: Request<SessionRef>,
) -> Result<Response<AgentSnapshotList>, TStatus> {
    let session_id = req.into_inner().id;
    let db = Database::open()
        .await
        .map_err(|e| TStatus::internal(format!("db open failed: {e}")))?;
    let resolved = resolve_agent_session_id(&db, &session_id)
        .await
        .map_err(|e| TStatus::internal(format!("session resolve failed: {e}")))?;
    let Some(resolved) = resolved else {
        return Ok(Response::new(AgentSnapshotList::default()));
    };
    let rows = db
        .list_agent_tree_snapshots(&resolved)
        .await
        .map_err(|e| TStatus::internal(format!("list_agent_tree_snapshots: {e}")))?;
    Ok(Response::new(AgentSnapshotList {
        snapshots: rows.iter().map(snapshot_row_to_pb).collect(),
    }))
}

pub(super) async fn get_agent_snapshot(
    _svc: &AdminSvc,
    req: Request<AgentSnapshotRefRequest>,
) -> Result<Response<PbAgentList>, TStatus> {
    let inner = req.into_inner();
    if inner.turn_id.trim().is_empty() {
        return Err(TStatus::invalid_argument("turn_id is required"));
    }
    let db = Database::open()
        .await
        .map_err(|e| TStatus::internal(format!("db open failed: {e}")))?;
    let resolved = resolve_agent_session_id(&db, &inner.session_id)
        .await
        .map_err(|e| TStatus::internal(format!("session resolve failed: {e}")))?;
    let Some(resolved) = resolved else {
        return Err(TStatus::failed_precondition("no active session"));
    };
    let row = db
        .get_agent_tree_snapshot(&resolved, &inner.turn_id)
        .await
        .map_err(|e| TStatus::internal(format!("get_agent_tree_snapshot: {e}")))?
        .ok_or_else(|| TStatus::not_found("snapshot not found"))?;
    // Snapshot JSON is `Vec<PbAgentEntry>` serialized through serde-json
    // by the writer; tolerate both bare array and `{agents: [...]}` so
    // the format can grow without forcing a migration.
    let agents: Vec<PbAgentEntry> = parse_snapshot_payload(&row.snapshot_json)
        .map_err(|e| TStatus::internal(format!("snapshot decode failed: {e}")))?;
    Ok(Response::new(PbAgentList {
        agents,
        history: Vec::new(),
    }))
}

pub(super) async fn get_subprocesses(
    svc: &AdminSvc,
    _req: Request<Empty>,
) -> Result<Response<SubprocessSnapshot>, TStatus> {
    let reg = svc.process_registry_handle();
    let Some(reg) = reg else {
        return Ok(Response::new(SubprocessSnapshot::default()));
    };
    let snap = reg.snapshot();
    Ok(Response::new(SubprocessSnapshot {
        running: snap
            .running
            .into_iter()
            .map(process_entry_to_proto)
            .collect(),
        finished: snap
            .finished
            .into_iter()
            .map(process_entry_to_proto)
            .collect(),
    }))
}

pub(super) async fn set_subprocess_pinned(
    svc: &AdminSvc,
    req: Request<SetSubprocessPinnedRequest>,
) -> Result<Response<Empty>, TStatus> {
    let req = req.into_inner();
    let reg = svc
        .process_registry_handle()
        .ok_or_else(|| TStatus::failed_precondition("process registry not wired"))?;
    let result = if req.pinned {
        reg.pin(req.id)
    } else {
        reg.unpin(req.id)
    };
    result.map_err(|e| TStatus::not_found(e.to_string()))?;
    Ok(Response::new(Empty {}))
}
