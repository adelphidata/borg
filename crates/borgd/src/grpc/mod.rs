//! gRPC service implementations.

pub mod admin;
pub mod capability;
pub mod session;
pub mod status;

use crate::session::SessionRegistry;
use borg_proto::admin::admin_server::AdminServer;
use borg_proto::capability::capability_server::CapabilityServer;
use borg_proto::session::session_server::SessionServer;
use borg_proto::status::status_server::StatusServer;
use std::sync::Arc;

/// Bundle of all service servers exposed by borgd.
pub struct Services {
    /// Status read-only RPCs.
    pub status: StatusServer<status::StatusSvc>,
    /// Admin lifecycle + data-access RPCs.
    pub admin: AdminServer<admin::AdminSvc>,
    /// Session interaction (Open/Send/Stream/Cancel/Close/RespondToPrompt).
    pub session: SessionServer<session::SessionSvc>,
    /// Capability bidi-stream for client-offered capabilities.
    pub capability: CapabilityServer<capability::CapabilitySvc>,
}

/// Construct the service bundle. The session factory governs how new
/// sessions get their backend (real Agent in prod, stub in tests).
pub fn build_services(
    registry: SessionRegistry,
    factory: Arc<dyn session::SessionFactory>,
    capability_router: Arc<capability::CapabilityRouter>,
) -> Services {
    Services {
        status: StatusServer::new(status::StatusSvc::new()),
        admin: AdminServer::new(admin::AdminSvc::new(registry.clone())),
        session: SessionServer::new(session::SessionSvc::new(registry, factory)),
        capability: CapabilityServer::new(capability::CapabilitySvc::new(capability_router)),
    }
}
