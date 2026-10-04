//! The browser-facing subset of the daemon's gRPC services.
//!
//! `gateway::webui` mounts this router at `/grpc` and gates it on a client
//! token. The services are the same structs already serving the unix socket and
//! the mTLS TCP port — cloning them costs nothing and keeps one implementation
//! behind all three transports.

use axum::Router;
use tonic::service::Routes;
use tonic_web::GrpcWebLayer;
use tower::Layer as _;

use crate::grpc::Services;

/// Wraps the UI-reachable services for gRPC-Web and returns them as an axum
/// router.
///
/// Two services are deliberately left off, so this is a subset of the UDS/TCP
/// routers rather than a violation of their "register everywhere" invariant:
///
/// * `Capability` is a bidirectional stream. gRPC-Web cannot carry one, and it
///   is an agent-to-agent channel rather than a UI surface.
/// * `Pki` bootstraps device pairing before a client has credentials. A browser
///   reaching this mount has already presented a token, so pairing over it
///   would be circular — pairing stays on the socket transports.
pub fn build_grpc_web_router(services: &Services) -> Router {
    // Wrapping each service (rather than layering the finished router) keeps
    // `NamedService` intact, which is what `Routes` uses to build the paths.
    let web = GrpcWebLayer::new();

    Routes::new(web.layer(services.status.clone()))
        .add_service(web.layer(services.admin.clone()))
        .add_service(web.layer(services.session.clone()))
        .add_service(web.layer(services.telemetry.clone()))
        .add_service(web.layer(services.voice.clone()))
        .into_axum_router()
}
