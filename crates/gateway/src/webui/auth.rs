use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use cortex::auth::tokens;

use crate::DbOpener;

/// Bearer-token gate for the browser's gRPC-Web mount.
///
/// Reuses the same `client_tokens` rows as the OpenAI-compat and MCP surfaces
/// (`borg token issue` mints one), so the browser is authenticated exactly like
/// every other network client. The unix socket needs no equivalent — its file
/// permissions are the boundary — which is why only this path is gated.
pub async fn require_client_token(
    State(db_open): State<DbOpener>,
    request: Request,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_owned);

    let Some(token) = token else {
        return unauthenticated("missing bearer token");
    };

    let Ok(db) = (db_open)().await else {
        return (StatusCode::SERVICE_UNAVAILABLE, "auth backend unavailable").into_response();
    };

    // TODO(scopes): `TokenRow.scopes` is not enforced anywhere yet. Once it is,
    // require a `webui` scope here — the Admin service behind this mount has
    // write RPCs that a read-only token should not reach.
    match tokens::verify(&db, &token).await {
        Ok(_) => next.run(request).await,
        Err(_) => unauthenticated("invalid or revoked client token"),
    }
}

/// 401 carrying `grpc-status: 16`, so gRPC-Web clients surface a real
/// UNAUTHENTICATED error instead of an opaque transport failure.
fn unauthenticated(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [("grpc-status", "16"), ("grpc-message", "unauthenticated")],
        message.to_owned(),
    )
        .into_response()
}
