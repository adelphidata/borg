//! gRPC service implementations.
//!
//! v1 foundation: Status and Admin RPCs implemented as smoke endpoints.
//! Session and Capability services land in follow-up tasks (4 and 5 in the
//! plan) once the agent loop is lifted in.

pub mod admin;
pub mod status;

use borg_proto::admin::admin_server::AdminServer;
use borg_proto::status::status_server::StatusServer;

/// Bundle of all service servers exposed by borgd.
pub struct Services {
    /// Status read-only RPCs.
    pub status: StatusServer<status::StatusSvc>,
    /// Admin lifecycle + data-access RPCs.
    pub admin: AdminServer<admin::AdminSvc>,
}

/// Construct the service bundle with default state.
pub fn build_services() -> Services {
    Services {
        status: StatusServer::new(status::StatusSvc::new()),
        admin: AdminServer::new(admin::AdminSvc::new()),
    }
}
