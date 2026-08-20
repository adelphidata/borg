//! End-to-end tests for `mcp::transport::http` (streamable-HTTP) against a
//! real axum fixture. Mirrors `sse_transport.rs`: drive `open()` + the
//! `Transport` impl over the real wire so DNS pinning, redirect refusal,
//! and the `Mcp-Session-Id` resumption header all stay covered.
//!
//! The streamable-HTTP transport is now the most common MCP wire format
//! (notion.com, the python-MCP server library default, etc.) — if the
//! session-id round-trip silently broke, every reconnect would land on a
//! cold server-side session and `tools/list_changed` notifications would
//! stop being honored.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::post,
    Router,
};
use mcp::errors::McpError;
use mcp::transport::http::{open, HttpConfig, HttpPolicy, MCP_SESSION_ID_HEADER};
use mcp::transport::Transport;
use serde_json::{json, Value};
use tokio::sync::Mutex;

/// Per-request server state. Captures the headers (so the test can assert
/// the client echoed `Mcp-Session-Id`) and controls the response body /
/// status / session-id-to-emit on a per-call basis.
/// One scripted reply: `(status, body, session-id to emit)`.
type ScriptedReply = (u16, Value, Option<String>);

#[derive(Clone, Default)]
struct ServerLog {
    /// Headers from every POST so far.
    requests: Arc<Mutex<Vec<HeaderMap>>>,
    /// Bodies from every POST so far.
    bodies: Arc<Mutex<Vec<Value>>>,
    /// Next response body to emit. The test loads this before each round
    /// trip; if empty, the handler 200s with `{}`.
    next_response: Arc<Mutex<Option<Value>>>,
    /// Session id to attach to the *response*. Picked up by the client and
    /// echoed on subsequent requests.
    next_session_id: Arc<Mutex<Option<String>>>,
    /// Force the next response to a non-2xx status. Useful for the 404
    /// (session-not-found) regression test.
    next_status: Arc<std::sync::atomic::AtomicU16>,
    /// `Set-Cookie` value to attach to the *response*. Models a load
    /// balancer's sticky-session / affinity cookie; the client jar should
    /// store it and echo it on subsequent requests.
    next_set_cookie: Arc<Mutex<Option<String>>>,
    /// Scripted `(status, body, session-id)` replies, consumed in order and
    /// taking precedence over the single-shot fields above. Needed for
    /// exchanges that span several round trips — a 404 answered by a
    /// re-handshake is four POSTs, not one.
    script: Arc<Mutex<std::collections::VecDeque<ScriptedReply>>>,
}

impl ServerLog {
    fn new() -> Self {
        let s = Self::default();
        s.next_status
            .store(200, std::sync::atomic::Ordering::SeqCst);
        s
    }

    async fn expect_response(&self, body: Value) {
        *self.next_response.lock().await = Some(body);
    }

    async fn emit_session_id(&self, sid: &str) {
        *self.next_session_id.lock().await = Some(sid.to_string());
    }

    async fn script_reply(&self, status: u16, body: Value, session_id: Option<&str>) {
        self.script
            .lock()
            .await
            .push_back((status, body, session_id.map(str::to_string)));
    }

    async fn emit_set_cookie(&self, cookie: &str) {
        *self.next_set_cookie.lock().await = Some(cookie.to_string());
    }

    async fn next_request_headers(&self) -> HeaderMap {
        let guard = self.requests.lock().await;
        guard.last().cloned().expect("no request seen yet")
    }
}

async fn rpc_handler(
    State(state): State<ServerLog>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    state.requests.lock().await.push(headers);
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state.bodies.lock().await.push(parsed);

    let scripted = state.script.lock().await.pop_front();
    let status = match &scripted {
        Some((status, _, _)) => *status,
        None => state
            .next_status
            .swap(200, std::sync::atomic::Ordering::SeqCst),
    };
    let mut resp_headers = HeaderMap::new();
    if let Some(sid) = scripted
        .as_ref()
        .and_then(|(_, _, sid)| sid.clone())
        .or(state.next_session_id.lock().await.take())
    {
        resp_headers.insert(
            MCP_SESSION_ID_HEADER,
            HeaderValue::from_str(&sid).expect("bad session id header"),
        );
    }
    resp_headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Some(cookie) = state.next_set_cookie.lock().await.take() {
        resp_headers.insert(
            axum::http::header::SET_COOKIE,
            HeaderValue::from_str(&cookie).expect("bad set-cookie header"),
        );
    }

    let body_value = match scripted {
        Some((_, body, _)) => body,
        None => state
            .next_response
            .lock()
            .await
            .take()
            .unwrap_or_else(|| json!({})),
    };
    let body_str = body_value.to_string();

    let mut resp = (
        StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        body_str,
    )
        .into_response();
    for (k, v) in &resp_headers {
        resp.headers_mut().insert(k.clone(), v.clone());
    }
    resp
}

async fn spawn_fixture() -> (String, ServerLog, tokio::sync::oneshot::Sender<()>) {
    let state = ServerLog::new();
    let app = Router::new()
        .route("/mcp", post(rpc_handler))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .expect("serve");
    });
    (format!("http://{addr}/mcp"), state, shutdown_tx)
}

fn config(url: String) -> HttpConfig {
    HttpConfig {
        server_name: "fixture".into(),
        url,
        headers: HashMap::new(),
        timeout: Duration::from_secs(2),
        policy: HttpPolicy {
            allow_plaintext_http: true,
            allow_local_network: true,
        },
        client_identity: None,
        ca_cert_path: None,
        max_response_bytes: 0,
    }
}

#[tokio::test]
async fn session_id_is_echoed_on_subsequent_requests() {
    // Spec: the streamable-HTTP transport must capture `Mcp-Session-Id`
    // from the first response that emits it, then echo it on every
    // subsequent request. Without this, a server-side restart loses every
    // pre-existing session — `tools/list_changed` notifications would no
    // longer be honored across reconnects. This is the load-bearing test
    // for the resumption layer documented in `docs/mcp.md`.
    let (url, server, _shutdown) = spawn_fixture().await;
    let mut transport = open(config(url)).await.expect("open");

    server.emit_session_id("session-A").await;
    server
        .expect_response(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        .await;
    transport
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .await
        .expect("send 1");
    let _ = transport.recv().await.expect("recv 1");

    // First request should NOT have carried the header (we hadn't seen the
    // server's emission yet).
    let first = server.requests.lock().await[0].clone();
    assert!(
        first.get(MCP_SESSION_ID_HEADER).is_none(),
        "first request must not carry a session id",
    );

    // Second send: transport must now echo "session-A".
    server
        .expect_response(json!({"jsonrpc": "2.0", "id": 2, "result": {"ok": true}}))
        .await;
    transport
        .send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
        .await
        .expect("send 2");
    let _ = transport.recv().await.expect("recv 2");

    let second = server.next_request_headers().await;
    assert_eq!(
        second
            .get(MCP_SESSION_ID_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some("session-A"),
        "second request must echo Mcp-Session-Id captured from first response",
    );
}

#[tokio::test]
async fn affinity_cookie_is_stored_and_echoed_on_subsequent_requests() {
    // Spec: a streamable-HTTP MCP server behind a load balancer may set a
    // sticky-session / affinity cookie (e.g. Cloudflare `__cf*`) on its first
    // response. The transport's client must persist that cookie and resend it
    // on every subsequent request so the whole session lands on one backend.
    // Without the per-client cookie jar, only `Mcp-Session-Id` round-trips and
    // affinity is lost mid-session. This is the regression guard for that jar.
    let (url, server, _shutdown) = spawn_fixture().await;
    let mut transport = open(config(url)).await.expect("open");

    server.emit_set_cookie("affinity=backend-7; Path=/").await;
    server
        .expect_response(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}))
        .await;
    transport
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .await
        .expect("send 1");
    let _ = transport.recv().await.expect("recv 1");

    // First request predates the server's Set-Cookie, so it must carry none.
    let first = server.requests.lock().await[0].clone();
    assert!(
        first.get(axum::http::header::COOKIE).is_none(),
        "first request must not carry a cookie",
    );

    // Second send: the jar must now echo the affinity cookie back.
    server
        .expect_response(json!({"jsonrpc": "2.0", "id": 2, "result": {"ok": true}}))
        .await;
    transport
        .send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
        .await
        .expect("send 2");
    let _ = transport.recv().await.expect("recv 2");

    let second = server.next_request_headers().await;
    let cookie = second
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        cookie.contains("affinity=backend-7"),
        "second request must echo the affinity cookie, got: {cookie:?}",
    );
}

#[tokio::test]
async fn a_404_reinitializes_the_session_and_retries_the_request() {
    // Spec: a 404 means the server forgot this session — it restarted, or the
    // session TTL lapsed while we were idle. The remedy is to initialize
    // again, transparently, and carry on. Tearing the transport down instead
    // makes every server restart cost the operator a daemon restart.
    let (url, server, _shutdown) = spawn_fixture().await;
    let mut transport = open(config(url)).await.expect("open");

    server.emit_session_id("session-X").await;
    server
        .expect_response(json!({"jsonrpc": "2.0", "id": 1, "result": {}}))
        .await;
    transport
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .await
        .expect("send 1");
    let _ = transport.recv().await.expect("recv 1");
    assert_eq!(transport.session_id(), Some("session-X"));

    // 404 on the next call, then a healthy re-handshake, then the retry.
    server.script_reply(404, json!({}), None).await;
    server
        .script_reply(
            200,
            json!({"jsonrpc": "2.0", "id": u64::MAX, "result": {"protocolVersion": "2025-06-18"}}),
            Some("session-Y"),
        )
        .await;
    server.script_reply(200, json!({}), None).await;
    server
        .script_reply(
            200,
            json!({"jsonrpc": "2.0", "id": 2, "result": {"tools": []}}),
            None,
        )
        .await;

    transport
        .send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
        .await
        .expect("a 404 must be recovered, not surfaced");
    let msg = transport
        .recv()
        .await
        .expect("recv after recovery")
        .expect("the retried request's response must reach the caller");
    assert_eq!(msg.id, Some(2), "the caller must get its own response back");
    assert_eq!(
        transport.session_id(),
        Some("session-Y"),
        "the transport must adopt the id from the fresh session",
    );

    let bodies = server.bodies.lock().await.clone();
    let methods: Vec<&str> = bodies
        .iter()
        .filter_map(|b| b.get("method").and_then(|m| m.as_str()))
        .collect();
    assert_eq!(
        methods,
        vec![
            "initialize",
            "tools/list",
            "initialize",
            "notifications/initialized",
            "tools/list"
        ],
        "recovery must be a full handshake followed by exactly one retry",
    );
}

#[tokio::test]
async fn a_second_404_after_reinitializing_is_terminal() {
    // The bound that keeps recovery from spinning: a server that 404s the
    // session it just handed us is broken, and the caller must hear about it.
    let (url, server, _shutdown) = spawn_fixture().await;
    let mut transport = open(config(url)).await.expect("open");

    server.emit_session_id("session-X").await;
    server
        .expect_response(json!({"jsonrpc": "2.0", "id": 1, "result": {}}))
        .await;
    transport
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .await
        .expect("send 1");
    let _ = transport.recv().await.expect("recv 1");

    server.script_reply(404, json!({}), None).await;
    server
        .script_reply(
            200,
            json!({"jsonrpc": "2.0", "id": u64::MAX, "result": {}}),
            Some("session-Y"),
        )
        .await;
    server.script_reply(200, json!({}), None).await;
    server.script_reply(404, json!({}), None).await;

    let err = transport
        .send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
        .await
        .expect_err("a server that 404s its own fresh session must fail the call");
    // The variant matters, not just the wording: the session supervisor keys
    // its teardown on `TransportClosed`. Leaking `SessionExpired` would leave
    // a broken server marked alive, and every later call would repeat this
    // whole handshake and fail — the wedged-transport loop the teardown
    // exists to prevent.
    assert!(
        matches!(err, McpError::TransportClosed(_)),
        "recovery failure must be terminal for the supervisor; got: {err:?}",
    );
}

#[tokio::test]
async fn a_404_with_no_session_of_ours_names_the_endpoint() {
    // Without a session id in play, a 404 is the URL being wrong. Re-running
    // the handshake against a path that does not exist would only repeat the
    // same 404, so this must fail fast and say why.
    let (url, server, _shutdown) = spawn_fixture().await;
    let mut transport = open(config(url)).await.expect("open");

    server
        .next_status
        .store(404, std::sync::atomic::Ordering::SeqCst);
    let err = transport
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .await
        .expect_err("a 404 with no session must fail");
    assert!(
        format!("{err}").contains("endpoint"),
        "the error must point at the endpoint URL; got: {err}",
    );
}

#[tokio::test]
async fn the_negotiated_protocol_version_is_echoed_on_later_requests() {
    // Spec: every post-handshake request identifies the revision it was
    // agreed under. Sending a "latest known" constant instead makes the
    // header and the handshake body disagree, and a conforming server then
    // judges the request against rules the client never agreed to.
    let (url, server, _shutdown) = spawn_fixture().await;
    let mut transport = open(config(url)).await.expect("open");

    assert!(
        transport.protocol_version().is_none(),
        "nothing is negotiated before the handshake",
    );
    transport.set_protocol_version("2025-11-25");

    server
        .expect_response(json!({"jsonrpc": "2.0", "id": 1, "result": {}}))
        .await;
    transport
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .await
        .expect("send");

    let headers = server.next_request_headers().await;
    let sent = headers
        .get("MCP-Protocol-Version")
        .expect("the negotiated version must be sent on every later request")
        .to_str()
        .expect("header must be ascii");
    assert_eq!(
        sent, "2025-11-25",
        "the header must carry the negotiated value, not a latest-known constant",
    );
}

#[tokio::test]
async fn server_5xx_is_classified_as_retryable_transient() {
    // Spec: a 5xx during the streamable-HTTP handshake / read-only `tools/list`
    // is transient — it must surface as a `TransientHttp` error the startup path
    // can retry (by recreating the transport), not as a terminal `TransportClosed`
    // that permanently fails the server until a daemon restart. The status must
    // still be carried for logs.
    let (url, server, _shutdown) = spawn_fixture().await;
    let mut transport = open(config(url)).await.expect("open");

    server
        .next_status
        .store(503, std::sync::atomic::Ordering::SeqCst);
    let err = transport
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .await
        .expect_err("send must fail on 5xx");
    assert!(
        matches!(&err, McpError::TransientHttp { status: 503, .. }),
        "5xx must classify as TransientHttp; got: {err:?}",
    );
    assert!(
        err.is_retryable_startup(),
        "5xx during startup must be retryable",
    );
    assert!(format!("{err}").contains("503"), "status must reach logs");
}

#[tokio::test]
async fn rate_limit_429_is_classified_as_retryable_transient() {
    // Spec: a 429 during startup is transient (server warming up / momentary
    // throttle) and retryable, same as 5xx.
    let (url, server, _shutdown) = spawn_fixture().await;
    let mut transport = open(config(url)).await.expect("open");

    server
        .next_status
        .store(429, std::sync::atomic::Ordering::SeqCst);
    let err = transport
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .await
        .expect_err("send must fail on 429");
    assert!(
        matches!(&err, McpError::TransientHttp { status: 429, .. }) && err.is_retryable_startup(),
        "429 must classify as retryable TransientHttp; got: {err:?}",
    );
}

#[tokio::test]
async fn client_4xx_is_not_retryable() {
    // Spec: a deterministic 4xx (other than auth/404-session) is the server's
    // last word — it must stay a terminal `TransportClosed` so the startup path
    // fails fast instead of spinning a doomed retry loop.
    let (url, server, _shutdown) = spawn_fixture().await;
    let mut transport = open(config(url)).await.expect("open");

    server
        .next_status
        .store(400, std::sync::atomic::Ordering::SeqCst);
    let err = transport
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .await
        .expect_err("send must fail on 400");
    assert!(
        matches!(&err, McpError::TransportClosed(_)) && !err.is_retryable_startup(),
        "400 must be terminal (TransportClosed, not retryable); got: {err:?}",
    );
}

#[tokio::test]
async fn unauthorized_status_surfaces_as_auth_error_for_supervisor_halt() {
    // Spec: 401/403 must classify as `Auth` so the supervisor stops
    // retrying (per `RpcError::looks_like_auth` doc — credential-spamming
    // retry loops can lock out a token). Without the distinction we'd
    // burn through a server's auth-lockout budget on a misconfigured PAT.
    let (url, server, _shutdown) = spawn_fixture().await;
    let mut transport = open(config(url)).await.expect("open");

    server
        .next_status
        .store(401, std::sync::atomic::Ordering::SeqCst);
    let err = transport
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .await
        .expect_err("send must fail on 401");
    let msg = format!("{err}").to_ascii_lowercase();
    assert!(
        msg.contains("401") || msg.contains("auth"),
        "error must indicate auth failure; got: {msg}",
    );
}

#[tokio::test]
async fn session_id_refreshes_when_server_rotates_it() {
    // Spec: a server may rotate `Mcp-Session-Id` at any time (e.g. token
    // refresh). The transport must adopt the most recent value rather than
    // pin to the first one it saw — otherwise a rotation breaks the
    // session permanently. This is a quieter spec than the 404 path but
    // matters for long-lived sessions.
    let (url, server, _shutdown) = spawn_fixture().await;
    let mut transport = open(config(url)).await.expect("open");

    server.emit_session_id("session-old").await;
    server
        .expect_response(json!({"jsonrpc": "2.0", "id": 1, "result": {}}))
        .await;
    transport
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .await
        .expect("send 1");
    let _ = transport.recv().await.expect("recv 1");
    assert_eq!(transport.session_id(), Some("session-old"));

    server.emit_session_id("session-new").await;
    server
        .expect_response(json!({"jsonrpc": "2.0", "id": 2, "result": {}}))
        .await;
    transport
        .send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
        .await
        .expect("send 2");
    let _ = transport.recv().await.expect("recv 2");
    assert_eq!(transport.session_id(), Some("session-new"));
}

#[tokio::test]
async fn error_status_with_endless_body_fails_fast_without_buffering() {
    // Spec: non-2xx bodies are display-only color for the error message, so
    // the transport must read at most a small capped prefix. Before the cap,
    // the whole error body was buffered via `resp.text()` — a hostile server
    // answering `initialize` with a 400 plus an endless chunked body could
    // hold the transport (and its memory) hostage until the client timeout.
    let app = Router::new().route(
        "/mcp",
        post(|| async {
            let stream = futures_util::stream::repeat_with(|| {
                Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(&[b'x'; 8192]))
            });
            (
                StatusCode::BAD_REQUEST,
                axum::body::Body::from_stream(stream),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    // A generous client timeout: an uncapped read of the endless body only
    // returns once this expires, so finishing well before it proves the cap.
    let mut cfg = config(format!("http://{addr}/mcp"));
    cfg.timeout = Duration::from_secs(30);
    let mut transport = open(cfg).await.expect("open");

    let started = std::time::Instant::now();
    let err = transport
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .await
        .expect_err("HTTP 400 must surface as an error");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "error body read did not stop at the cap (took {:?})",
        started.elapsed()
    );
    let msg = err.to_string();
    assert!(msg.contains("400"), "unexpected error: {msg}");
    assert!(
        msg.len() < 1024,
        "error message must stay bounded, got {} bytes",
        msg.len()
    );
}
