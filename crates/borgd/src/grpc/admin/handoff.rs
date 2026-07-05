//! Session-handoff RPC bodies for the admin service (`super::AdminSvc`).

use super::*;

pub(super) async fn request_handoff(
    _svc: &AdminSvc,
    req: Request<HandoffRequest>,
) -> Result<Response<Empty>, TStatus> {
    let req = req.into_inner();
    async {
        let db = Database::open().await?;
        // Empty session_id means "the most recently active session" —
        // same convention `/goal` uses so operators don't have to look
        // up an id to operate on their current session.
        let session_id = match resolve_goal_session_id(&db, &req.session_id).await? {
            Some(id) => id,
            None => anyhow::bail!("no session to hand off — start one first"),
        };
        db.request_handoff(&session_id, &req.target_platform).await
    }
    .await
    .map_err(|e: anyhow::Error| {
        TStatus::failed_precondition(format!("request_handoff failed: {e}"))
    })?;
    Ok(Response::new(Empty {}))
}

pub(super) async fn claim_handoff(
    _svc: &AdminSvc,
    req: Request<HandoffClaimRequest>,
) -> Result<Response<HandoffClaimResponse>, TStatus> {
    let req = req.into_inner();
    let claimed = async {
        let db = Database::open().await?;
        db.claim_handoff_for_platform(&req.platform).await
    }
    .await
    .map_err(|e: anyhow::Error| TStatus::internal(format!("claim_handoff failed: {e}")))?;
    Ok(Response::new(HandoffClaimResponse {
        session_id: claimed.clone().unwrap_or_default(),
        claimed: claimed.is_some(),
    }))
}

pub(super) async fn complete_handoff(
    _svc: &AdminSvc,
    req: Request<SessionRef>,
) -> Result<Response<Empty>, TStatus> {
    let id = req.into_inner().id;
    async {
        let db = Database::open().await?;
        db.complete_handoff(&id).await
    }
    .await
    .map_err(|e: anyhow::Error| {
        TStatus::failed_precondition(format!("complete_handoff failed: {e}"))
    })?;
    Ok(Response::new(Empty {}))
}

pub(super) async fn fail_handoff(
    _svc: &AdminSvc,
    req: Request<HandoffFailRequest>,
) -> Result<Response<Empty>, TStatus> {
    let req = req.into_inner();
    async {
        let db = Database::open().await?;
        db.fail_handoff(&req.session_id, &req.reason).await
    }
    .await
    .map_err(|e: anyhow::Error| {
        TStatus::failed_precondition(format!("fail_handoff failed: {e}"))
    })?;
    Ok(Response::new(Empty {}))
}

pub(super) async fn get_handoff_status(
    _svc: &AdminSvc,
    req: Request<SessionRef>,
) -> Result<Response<HandoffStatusReply>, TStatus> {
    let id = req.into_inner().id;
    let status = async {
        let db = Database::open().await?;
        let session_id = resolve_goal_session_id(&db, &id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no session to inspect"))?;
        db.handoff_status(&session_id).await
    }
    .await
    .map_err(|e: anyhow::Error| TStatus::internal(format!("get_handoff_status failed: {e}")))?;
    Ok(Response::new(HandoffStatusReply {
        state: status
            .state
            .map(|s| s.as_str().to_string())
            .unwrap_or_default(),
        target_platform: status.target.unwrap_or_default(),
        error: status.error.unwrap_or_default(),
        requested_at_unix: status.requested_at.unwrap_or(0),
    }))
}
