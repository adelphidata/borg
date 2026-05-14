//! gRPC service implementations.

pub mod admin;
pub mod capability;
pub mod session;
pub mod status;
pub mod telemetry;

use crate::cost_events::CostEventBus;
use crate::goal_bus::GoalEventBus;
use crate::heartbeat::HeartbeatBus;
use crate::heartbeat::HeartbeatTrigger;
use crate::session::{DaemonSessionExecutor, SessionRegistry};
use protocols::admin::admin_server::AdminServer;
use protocols::capability::capability_server::CapabilityServer;
use protocols::session::session_server::SessionServer;
use protocols::status::status_server::StatusServer;
use protocols::telemetry::telemetry_server::TelemetryServer;
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
    /// Daemon-wide telemetry streams.
    pub telemetry: TelemetryServer<telemetry::TelemetrySvc>,
}

/// Construct the service bundle. The session factory governs how new
/// sessions get their backend (real Agent in prod, stub in tests).
pub fn build_services(
    registry: SessionRegistry,
    factory: Arc<dyn session::SessionFactory>,
    capability_router: Arc<capability::CapabilityRouter>,
    cost_event_bus: CostEventBus,
) -> Services {
    Services {
        status: StatusServer::new(status::StatusSvc::new()),
        admin: AdminServer::new(admin::AdminSvc::new(registry.clone())),
        session: SessionServer::new(session::SessionSvc::new(registry, factory)),
        capability: CapabilityServer::new(capability::CapabilitySvc::new(capability_router)),
        telemetry: TelemetryServer::new(telemetry::TelemetrySvc::new(cost_event_bus)),
    }
}

/// Extras the daemon binary wires into the Admin service. All optional for
/// tests; production sets all four.
pub struct AdminExtras {
    /// Bus that `Admin.SubscribeHeartbeatEvents` reads from.
    pub heartbeat_bus: Option<HeartbeatBus>,
    /// Executor used by `Admin.SendApprovalGreeting`.
    pub executor: Option<Arc<DaemonSessionExecutor>>,
    /// Channel Admin RPCs signal to trigger an immediate heartbeat.
    pub heartbeat_trigger_tx: Option<tokio::sync::mpsc::Sender<HeartbeatTrigger>>,
    /// Broadcaster `Admin.Shutdown` fires.
    pub shutdown_tx: Option<tokio::sync::broadcast::Sender<()>>,
    /// Config watcher notifier for immediate post-setting reloads.
    pub config_notify_tx: Option<tokio::sync::mpsc::Sender<cortex::config::Config>>,
    /// Daemon-wide subprocess registry — Admin RPCs (`GetSubprocesses`,
    /// `SetSubprocessPinned`) and `Admin.Heal` use this when set.
    pub process_registry: Option<cortex::process_registry::ProcessRegistry>,
    /// Bus that `Admin.SubscribeGoalEvents` reads from and the goal RPC
    /// handlers publish into. Production wires this through the
    /// orchestrator so the agent loop and the RPC handlers share one bus.
    pub goal_bus: Option<GoalEventBus>,
}

/// Like [`build_services`] but additionally wires Admin extras (heartbeat
/// bus, approval-greeting executor, heartbeat-trigger + shutdown senders).
pub fn build_services_with_extras(
    registry: SessionRegistry,
    factory: Arc<dyn session::SessionFactory>,
    capability_router: Arc<capability::CapabilityRouter>,
    cost_event_bus: CostEventBus,
    extras: AdminExtras,
) -> Services {
    let mut admin_svc = admin::AdminSvc::new(registry.clone());
    if let Some(bus) = extras.heartbeat_bus {
        admin_svc = admin_svc.with_heartbeat_bus(bus);
    }
    if let Some(executor) = extras.executor {
        admin_svc = admin_svc.with_executor(executor);
    }
    if let Some(tx) = extras.heartbeat_trigger_tx {
        admin_svc = admin_svc.with_heartbeat_trigger_sender(tx);
    }
    if let Some(tx) = extras.shutdown_tx {
        admin_svc = admin_svc.with_shutdown_sender(tx);
    }
    if let Some(tx) = extras.config_notify_tx {
        admin_svc = admin_svc.with_config_notify_sender(tx);
    }
    if let Some(reg) = extras.process_registry {
        admin_svc = admin_svc.with_process_registry(reg);
    }
    if let Some(bus) = extras.goal_bus {
        admin_svc = admin_svc.with_goal_bus(bus);
    }
    Services {
        status: StatusServer::new(status::StatusSvc::new()),
        admin: AdminServer::new(admin_svc),
        session: SessionServer::new(session::SessionSvc::new(registry, factory)),
        capability: CapabilityServer::new(capability::CapabilitySvc::new(capability_router)),
        telemetry: TelemetryServer::new(telemetry::TelemetrySvc::new(cost_event_bus)),
    }
}
