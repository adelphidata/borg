//! Admin service — lifecycle + data-access RPCs.
//!
//! v1 foundation: Pair / Heal / Shutdown / settings / sessions return stubs.
//! Wired to real backends in subsequent tasks.

use borg_proto::admin::{
    admin_server::Admin, Empty, HealReport, PairRequest, PairResponse, SessionDetail, SessionList,
    SessionRef, SettingKey, SettingMutation, SettingsSnapshot,
};
use tonic::{Request, Response, Status as TStatus};

/// Admin service implementation.
pub struct AdminSvc;

impl AdminSvc {
    /// Construct a new Admin service.
    pub fn new() -> Self {
        Self
    }
}

impl Default for AdminSvc {
    fn default() -> Self {
        Self::new()
    }
}

#[tonic::async_trait]
impl Admin for AdminSvc {
    async fn pair(&self, _req: Request<PairRequest>) -> Result<Response<PairResponse>, TStatus> {
        Err(TStatus::unimplemented(
            "pairing lands in Task 02 (mTLS + PAKE)",
        ))
    }

    async fn poke(&self, _req: Request<Empty>) -> Result<Response<Empty>, TStatus> {
        // Wired to heartbeat scheduler's poke_tx in Task 4.
        Ok(Response::new(Empty {}))
    }

    async fn heal(&self, _req: Request<Empty>) -> Result<Response<HealReport>, TStatus> {
        Ok(Response::new(HealReport {
            pruned_logs: 0,
            pruned_activity: 0,
            evicted_embeddings: 0,
            healed_tasks: 0,
            warnings: vec![],
        }))
    }

    async fn shutdown(&self, _req: Request<Empty>) -> Result<Response<Empty>, TStatus> {
        // Real shutdown signaling lands when SessionHost is wired up.
        Ok(Response::new(Empty {}))
    }

    async fn get_settings(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<SettingsSnapshot>, TStatus> {
        Ok(Response::new(SettingsSnapshot { entries: vec![] }))
    }

    async fn set_setting(
        &self,
        _req: Request<SettingMutation>,
    ) -> Result<Response<Empty>, TStatus> {
        Err(TStatus::unimplemented(
            "set_setting wires to SettingsResolver in Task 6",
        ))
    }

    async fn unset_setting(&self, _req: Request<SettingKey>) -> Result<Response<Empty>, TStatus> {
        Err(TStatus::unimplemented(
            "unset_setting wires to SettingsResolver in Task 6",
        ))
    }

    async fn list_sessions(&self, _req: Request<Empty>) -> Result<Response<SessionList>, TStatus> {
        Ok(Response::new(SessionList { sessions: vec![] }))
    }

    async fn get_session(
        &self,
        _req: Request<SessionRef>,
    ) -> Result<Response<SessionDetail>, TStatus> {
        Err(TStatus::not_found("session not found"))
    }
}
