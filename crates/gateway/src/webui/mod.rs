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

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;

    fn test_router() -> Router {
        let db_open: DbOpener = std::sync::Arc::new(|| {
            Box::pin(async { anyhow::bail!("no database in this test") })
        });
        router(GrpcWebSlot::default(), db_open)
    }

    /// The composed router must serve BOTH browser surfaces: the SPA under
    /// `/ui` and the gRPC-Web mount under `/grpc`. Dropping either half
    /// leaves the web UI with no app or no backend.
    #[tokio::test]
    async fn router_mounts_spa_and_grpc_surfaces() {
        let resp = test_router()
            .oneshot(Request::builder().uri("/ui").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::PERMANENT_REDIRECT,
            "/ui must redirect to /ui/"
        );
        assert_eq!(
            resp.headers()
                .get(axum::http::header::LOCATION)
                .expect("redirect carries a Location header"),
            "/ui/"
        );

        // The /grpc mount must exist (any non-404 answer proves routing;
        // the exact status is the token gate's concern, tested in auth).
        let resp = test_router()
            .oneshot(
                Request::builder()
                    .uri("/grpc/borg.v1.Session/Open")
                    .method("POST")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "/grpc mount missing from the composed router"
        );
    }
}
