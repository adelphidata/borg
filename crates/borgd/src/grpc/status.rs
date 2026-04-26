//! Status service — read-only views of agent health.
//!
//! v1 foundation: returns placeholder values. Wired to real `VitalsHook`
//! state and the evolution chain in Task 4 of the redesign plan.

use borg_proto::status::{status_server::Status, Budget, Empty, Heartbeat, Posture, Vitals};
use tonic::{Request, Response, Status as TStatus};

/// Status service implementation.
pub struct StatusSvc;

impl StatusSvc {
    /// Construct a new Status service.
    pub fn new() -> Self {
        Self
    }
}

impl Default for StatusSvc {
    fn default() -> Self {
        Self::new()
    }
}

#[tonic::async_trait]
impl Status for StatusSvc {
    async fn get_vitals(&self, _req: Request<Empty>) -> Result<Response<Vitals>, TStatus> {
        Ok(Response::new(Vitals {
            stability: 0,
            focus: 0,
            sync: 0,
            growth: 0,
            happiness: 0,
            stage: "Base".to_string(),
            xp: 0,
        }))
    }

    async fn get_posture(&self, _req: Request<Empty>) -> Result<Response<Posture>, TStatus> {
        Ok(Response::new(Posture {
            posture: "Balanced".to_string(),
            xp_multiplier: 1.0,
        }))
    }

    async fn get_budget(&self, _req: Request<Empty>) -> Result<Response<Budget>, TStatus> {
        Ok(Response::new(Budget {
            daily_usd_cap: 0.0,
            daily_usd_spent: 0.0,
        }))
    }

    async fn get_heartbeat(&self, _req: Request<Empty>) -> Result<Response<Heartbeat>, TStatus> {
        Ok(Response::new(Heartbeat {
            next_in_seconds: 0,
            last_unix_ts: 0,
        }))
    }
}
