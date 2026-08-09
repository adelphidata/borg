use std::sync::{Arc, OnceLock};

use axum::extract::{Request, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use tower::ServiceExt as _;

use crate::DbOpener;

/// Late-bound holder for borgd's gRPC-Web router.
///
/// The gateway starts before borgd finishes constructing its tonic services
/// (see the ordering in `borgd::daemon`), so the mount cannot be handed over at
/// construction time. borgd creates a slot, passes it to the gateway, and fills
/// it once the services exist; until then `/grpc` answers 503.
///
/// The gateway only ever sees an opaque `axum::Router`, so it keeps no
/// dependency on borgd's types.
#[derive(Clone, Default)]
pub struct GrpcWebSlot(Arc<OnceLock<Router>>);

impl GrpcWebSlot {
    /// Installs the router. Later calls are ignored — the first wins.
    pub fn install(&self, router: Router) {
        let _ = self.0.set(router);
    }
}

/// Mounts gRPC-Web at `/grpc/*`, gated on a client token.
pub fn router(slot: GrpcWebSlot, db_open: DbOpener) -> Router {
    Router::new()
        .route("/grpc/{*rest}", any(dispatch))
        .layer(axum::middleware::from_fn_with_state(
            db_open,
            super::auth::require_client_token,
        ))
        .with_state(slot)
}

async fn dispatch(State(slot): State<GrpcWebSlot>, mut request: Request) -> Response {
    let Some(inner) = slot.0.get() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "daemon services are still starting",
        )
            .into_response();
    };

    // tonic routes on `/<package>.<Service>/<Method>`, so the mount prefix has
    // to come off before the request reaches it.
    let path_and_query = request
        .uri()
        .path_and_query()
        .map(axum::http::uri::PathAndQuery::as_str)
        .unwrap_or("/");
    let stripped = path_and_query
        .strip_prefix("/grpc")
        .unwrap_or(path_and_query);
    let Ok(uri) = stripped.parse::<Uri>() else {
        return (StatusCode::BAD_REQUEST, "malformed grpc path").into_response();
    };
    *request.uri_mut() = uri;

    match inner.clone().oneshot(request).await {
        Ok(response) => response.into_response(),
        Err(infallible) => match infallible {},
    }
}
