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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt as _;

    fn gated_router(db_open: DbOpener) -> Router {
        Router::new()
            .route("/probe", get(|| async { "inner reached" }))
            .layer(axum::middleware::from_fn_with_state(
                db_open,
                require_client_token,
            ))
    }

    fn failing_db() -> DbOpener {
        std::sync::Arc::new(|| Box::pin(async { anyhow::bail!("no database in this test") }))
    }

    /// The gate is the browser surface's only authentication. A request
    /// without a bearer token must be refused as gRPC-Web UNAUTHENTICATED
    /// (401 + grpc-status 16) and must never reach the inner service.
    #[tokio::test]
    async fn missing_token_is_grpc_web_unauthenticated() {
        let resp = gated_router(failing_db())
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            resp.headers()
                .get("grpc-status")
                .expect("grpc-web clients need a grpc-status trailer-header"),
            "16"
        );
        assert_eq!(
            resp.headers().get("grpc-message").unwrap(),
            "unauthenticated"
        );
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(
            body.contains("missing bearer token"),
            "refusal must say what was missing: {body}"
        );
        assert!(
            !body.contains("inner reached"),
            "the inner service must not run for an unauthenticated request"
        );
    }

    /// When the auth backend cannot be opened the gate must fail closed
    /// with 503 — not wave the request through, not claim the token is bad.
    #[tokio::test]
    async fn db_unavailable_fails_closed_with_503() {
        let resp = gated_router(failing_db())
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("authorization", "Bearer some-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        assert!(
            !String::from_utf8_lossy(&body).contains("inner reached"),
            "the inner service must not run when auth cannot be checked"
        );
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
