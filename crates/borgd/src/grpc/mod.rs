//! gRPC service implementations.

pub mod admin;
pub mod capability;
pub mod session;
pub mod status;

use crate::heartbeat::HeartbeatBus;
use crate::session::{DaemonSessionExecutor, SessionRegistry};
use protocols::admin::admin_server::AdminServer;
use protocols::capability::capability_server::CapabilityServer;
use protocols::session::session_server::SessionServer;
use protocols::status::status_server::StatusServer;
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

/// Extras the daemon binary wires into the Admin service. All optional for
/// tests; production sets all four.
pub struct AdminExtras {
    /// Bus that `Admin.SubscribeHeartbeatEvents` reads from.
    pub heartbeat_bus: Option<HeartbeatBus>,
    /// Executor used by `Admin.SendApprovalGreeting`.
    pub executor: Option<Arc<DaemonSessionExecutor>>,
    /// Channel `Admin.Poke` signals to trigger an immediate heartbeat.
    pub poke_tx: Option<tokio::sync::mpsc::Sender<()>>,
    /// Broadcaster `Admin.Shutdown` fires.
    pub shutdown_tx: Option<tokio::sync::broadcast::Sender<()>>,
}

/// Like [`build_services`] but additionally wires Admin extras (heartbeat
/// bus, approval-greeting executor, poke + shutdown senders).
pub fn build_services_with_extras(
    registry: SessionRegistry,
    factory: Arc<dyn session::SessionFactory>,
    capability_router: Arc<capability::CapabilityRouter>,
    extras: AdminExtras,
) -> Services {
    let mut admin_svc = admin::AdminSvc::new(registry.clone());
    if let Some(bus) = extras.heartbeat_bus {
        admin_svc = admin_svc.with_heartbeat_bus(bus);
    }
    if let Some(executor) = extras.executor {
        admin_svc = admin_svc.with_executor(executor);
    }
    if let Some(tx) = extras.poke_tx {
        admin_svc = admin_svc.with_poke_sender(tx);
    }
    if let Some(tx) = extras.shutdown_tx {
        admin_svc = admin_svc.with_shutdown_sender(tx);
    }
    Services {
        status: StatusServer::new(status::StatusSvc::new()),
        admin: AdminServer::new(admin_svc),
        session: SessionServer::new(session::SessionSvc::new(registry, factory)),
        capability: CapabilityServer::new(capability::CapabilitySvc::new(capability_router)),
    }
}
