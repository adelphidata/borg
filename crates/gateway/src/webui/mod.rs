//! The browser surface: the embedded `apps/web` SPA plus the gRPC-Web mount it
//! talks to.
//!
//! A browser can reach borgd through neither of the daemon's own transports —
//! it cannot open a unix socket, and it cannot present a client certificate for
//! the mTLS TCP port. So the gateway, which already runs in-process inside
//! borgd, re-exposes the same service structs over gRPC-Web and serves the SPA
//! that calls them. No proxy process and no second copy of the services.

mod assets;
mod auth;
mod grpc_web;

pub use grpc_web::GrpcWebSlot;

use axum::Router;

use crate::DbOpener;

/// Static SPA plus the token-gated `/grpc` mount.
pub fn router(slot: GrpcWebSlot, db_open: DbOpener) -> Router {
    assets::router().merge(grpc_web::router(slot, db_open))
}
