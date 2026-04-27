//! HTTP transport: `POST /mcp` with bearer auth.

use std::sync::Arc;

use axum::{
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};

/// Maximum size of an inbound JSON-RPC body. Same 1 MiB cap as the
/// generic-webhook surface — defends against unbounded allocation
/// before auth runs, since axum's `Json` extractor materializes the
/// whole body before invoking the handler.
const MAX_BODY_BYTES: usize = 1024 * 1024;
use cortex::auth::tokens;
use cortex::db::Database;

use super::server::McpServer;

/// Shared state for the MCP HTTP route.
#[derive(Clone)]
pub struct McpHttpState {
    /// Open a fresh DB for token verification per request.
    pub db_open: Arc<dyn Fn() -> anyhow::Result<Database> + Send + Sync>,
    /// MCP server instance handling JSON-RPC dispatch.
    pub server: Arc<McpServer>,
}

/// Build the HTTP router. Mount at root: provides `POST /mcp`.
pub fn http_router(state: McpHttpState) -> Router {
    Router::new()
        .route("/mcp", post(mcp_post))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

async fn mcp_post(
    State(state): State<McpHttpState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(resp) = authenticate(&headers, &state) {
        return resp;
    }

    match state.server.handle(&body).await {
        Some(resp) => Json(resp).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

#[allow(clippy::result_large_err)]
fn authenticate(headers: &HeaderMap, state: &McpHttpState) -> Result<(), Response> {
    let db = (state.db_open)().map_err(|_| auth_error("auth backend unavailable"))?;
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| auth_error("missing Authorization header"))?;
    let token = auth
        .strip_prefix("Bearer ")
        .ok_or_else(|| auth_error("Authorization header must use Bearer scheme"))?;
    tokens::verify(&db, token).map_err(|_| auth_error("invalid client token"))?;
    Ok(())
}

fn auth_error(msg: &str) -> Response {
    let body = serde_json::json!({"error": {"message": msg}});
    (StatusCode::UNAUTHORIZED, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::super::server::{McpToolDescriptor, ReadOnlyToolHandler};
    use super::*;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Arc;
    use tower::ServiceExt;

    struct StubTool;

    #[async_trait]
    impl ReadOnlyToolHandler for StubTool {
        fn descriptor(&self) -> McpToolDescriptor {
            McpToolDescriptor {
                name: "memory_search".into(),
                description: "stub".into(),
                input_schema: serde_json::json!({"type":"object"}),
            }
        }
        async fn call(
            &self,
            _arguments: serde_json::Value,
        ) -> Result<serde_json::Value, super::super::server::McpError> {
            Ok(serde_json::json!({"hits":[]}))
        }
    }

    fn make_state() -> (
        McpHttpState,
        cortex::auth::tokens::IssuedToken,
        tempfile::TempDir,
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("test.db");
        let path_for_factory = path.clone();
        let db_open: Arc<dyn Fn() -> anyhow::Result<Database> + Send + Sync> =
            Arc::new(move || {
                let conn = rusqlite::Connection::open(&path_for_factory)?;
                Database::from_connection(conn)
            });
        let setup_db = (db_open)().expect("open setup db");
        let issued = cortex::auth::tokens::issue(&setup_db, "mcp-tok", "mcp").expect("issue token");
        let tools: Vec<Box<dyn ReadOnlyToolHandler>> = vec![Box::new(StubTool)];
        let server = Arc::new(McpServer::new(tools, "0.1.0-test"));
        (McpHttpState { db_open, server }, issued, tmp)
    }

    fn req_with(token: Option<&str>, body: serde_json::Value) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json");
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::from(body.to_string())).expect("build")
    }

    async fn resp_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .expect("read body");
        serde_json::from_slice(&bytes).expect("json")
    }

    #[tokio::test]
    async fn http_rejects_unauthenticated() {
        // MCP HTTP transport is just as sensitive as OpenAI-compat —
        // any unauthenticated MCP `tools/call` would let third
        // parties query Borg's memory store unrestricted.
        let (state, _issued, _tmp) = make_state();
        let resp = http_router(state)
            .oneshot(req_with(
                None,
                serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn http_invalid_token_rejected() {
        let (state, _issued, _tmp) = make_state();
        let resp = http_router(state)
            .oneshot(req_with(
                Some("forged-token"),
                serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn http_authenticated_tools_list_round_trip() {
        // End-to-end happy path: a valid token gets the registered
        // tools back over the wire. Catches breaks in HTTP→server
        // wiring (axum extract, JSON ser/de, dispatcher routing).
        let (state, issued, _tmp) = make_state();
        let resp = http_router(state)
            .oneshot(req_with(
                Some(&issued.plaintext),
                serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp_json(resp).await;
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 1);
        let names: Vec<String> = body["result"]["tools"]
            .as_array()
            .expect("array")
            .iter()
            .map(|t| t["name"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(names, vec!["memory_search".to_string()]);
    }

    #[tokio::test]
    async fn http_notification_returns_204_no_content() {
        // Notifications get no JSON-RPC body but the HTTP response
        // still needs status — 204 is the right signal that the
        // server processed the message without producing a reply.
        let (state, issued, _tmp) = make_state();
        let resp = http_router(state)
            .oneshot(req_with(
                Some(&issued.plaintext),
                serde_json::json!({
                    "jsonrpc":"2.0",
                    "method":"notifications/cancelled",
                    "params":{}
                }),
            ))
            .await
            .expect("call");
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }
}
