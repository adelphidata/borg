//! End-to-end tests for `mcp::transport::sse` against a real local HTTP
//! server (axum fixture). Mirrors the design of `stdio_subprocess.rs`: drive
//! the production `open()` and `Transport` impls against something that
//! actually speaks the wire format, so regressions in the SSE event-stream
//! parser, the `endpoint` handshake, or the POST-with-pinned-DNS path fail
//! loudly.
//!
//! Why local + real: the `SseTransport` reads through `eventsource-stream`,
//! resolves DNS twice (once for the SSE GET, again for the server-advertised
//! POST URL), and depends on `reqwest` for the wire. A ScriptedTransport
//! would skip all of that and reduce the coverage to a JSON-RPC test we
//! already have in `session::tests`.
//!
//! These tests bind to `127.0.0.1:0` and run with `mcp.allow_local_network`
//! semantically true (we pass `HttpPolicy { allow_local_network: true, .. }`
//! directly to `open()`).

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
    Json, Router,
};
use mcp::transport::http::HttpPolicy;
use mcp::transport::sse::{open, SseConfig};
use mcp::transport::Transport;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// One in-test server instance. The constructor binds to a random local port
/// and returns the URL the SSE GET lives on; the operator-style script
/// pushes events via the `tx` channel to simulate server-initiated traffic.
struct SseFixture {
    base_url: String,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    posted: Arc<tokio::sync::Mutex<Vec<Value>>>,
}

#[derive(Clone)]
struct FixtureState {
    /// Wrapped in `Option` so we can move the receiver out the first time
    /// the SSE handler is hit; tests open one connection per fixture so a
    /// second hit is a bug, not a feature.
    events: Arc<tokio::sync::Mutex<Option<mpsc::Receiver<Event>>>>,
    posted: Arc<tokio::sync::Mutex<Vec<Value>>>,
    posts_seen: Arc<AtomicUsize>,
    /// HTTP status to return from the `POST /messages` handler. Tests flip
    /// this to 500/4xx to exercise the negative paths.
    post_status: Arc<std::sync::atomic::AtomicU16>,
}

async fn sse_handler(State(state): State<FixtureState>) -> impl axum::response::IntoResponse {
    // Drain the test-side channel by *taking* the receiver — there's one
    // request per fixture so this is fine. The prefix event is the
    // `endpoint` advertisement; without it the client refuses to proceed.
    let receiver = {
        let mut guard = state.events.lock().await;
        guard
            .take()
            .expect("sse_handler called twice for one fixture")
    };
    let (out_tx, out_rx) = mpsc::channel::<Result<Event, std::convert::Infallible>>(64);
    out_tx
        .send(Ok(Event::default().event("endpoint").data("/messages")))
        .await
        .expect("queue endpoint event");
    tokio::spawn(async move {
        let mut rx = receiver;
        while let Some(ev) = rx.recv().await {
            if out_tx.send(Ok(ev)).await.is_err() {
                break;
            }
        }
    });
    Sse::new(ReceiverStream::new(out_rx)).keep_alive(KeepAlive::default())
}

async fn messages_handler(
    State(state): State<FixtureState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> StatusCode {
    state.posts_seen.fetch_add(1, Ordering::SeqCst);
    state.posted.lock().await.push(body);
    let _ = headers; // headers captured but not asserted here
    StatusCode::from_u16(state.post_status.load(Ordering::SeqCst)).unwrap_or(StatusCode::ACCEPTED)
}

impl SseFixture {
    /// Spawn an axum SSE+POST server on a random port. Returns the fixture
    /// plus the `tx` channel the caller can push events onto.
    async fn spawn() -> (Self, mpsc::Sender<Event>) {
        let (tx, rx) = mpsc::channel::<Event>(64);
        let posted = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let state = FixtureState {
            events: Arc::new(tokio::sync::Mutex::new(Some(rx))),
            posted: posted.clone(),
            posts_seen: Arc::new(AtomicUsize::new(0)),
            post_status: Arc::new(std::sync::atomic::AtomicU16::new(
                StatusCode::ACCEPTED.as_u16(),
            )),
        };

        let app = Router::new()
            .route("/sse", get(sse_handler))
            .route("/messages", post(messages_handler))
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr: SocketAddr = listener.local_addr().expect("local_addr");
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("serve");
        });

        let base_url = format!("http://{addr}/sse");
        let fixture = SseFixture {
            base_url,
            _shutdown: shutdown_tx,
            posted,
        };
        (fixture, tx)
    }

    fn config(&self) -> SseConfig {
        SseConfig {
            server_name: "sse-fixture".into(),
            url: self.base_url.clone(),
            headers: HashMap::new(),
            timeout: Duration::from_secs(2),
            policy: HttpPolicy {
                // 127.0.0.1 — must permit local network or the URL guard
                // will refuse it. This is exactly how the production code
                // is configured by an operator who flips `mcp.allow_local_network`.
                allow_plaintext_http: true,
                allow_local_network: true,
            },
        }
    }
}

fn jsonrpc_message_event(payload: Value) -> Event {
    Event::default().event("message").data(payload.to_string())
}

#[tokio::test]
async fn open_completes_when_server_sends_endpoint_event() {
    // Spec: SSE `open()` must wait for the server's `endpoint` event and
    // then return a usable transport. Without that event there's no POST
    // URL and the transport cannot send. This is the positive baseline that
    // every other SSE test depends on — if it breaks, no SSE server can
    // connect.
    let (fixture, _events_tx) = SseFixture::spawn().await;
    let cfg = fixture.config();
    let transport = open(cfg, Duration::from_secs(2))
        .await
        .expect("open should succeed once endpoint event arrives");
    assert_eq!(transport.server_name(), "sse-fixture");
}

#[tokio::test]
async fn open_times_out_when_endpoint_event_missing() {
    // Spec: if the server never advertises a POST URL within
    // `connect_timeout`, `open()` must time out rather than block forever.
    // We simulate by serving an empty SSE stream that never emits
    // `endpoint`. Failure mode: a borg operator points at an SSE-shaped URL
    // that isn't an MCP server and watches the CLI hang.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    // Hand-rolled mini-server that holds the SSE GET open with keep-alive
    // comments but never sends an `endpoint` event.
    let app = Router::new().route(
        "/sse",
        get(|| async {
            let (tx, rx) = mpsc::channel::<Result<Event, std::convert::Infallible>>(8);
            tokio::spawn(async move {
                loop {
                    if tx
                        .send(Ok(Event::default().comment("keep-alive")))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            });
            Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .expect("serve");
    });

    let cfg = SseConfig {
        server_name: "no-endpoint".into(),
        url: format!("http://{addr}/sse"),
        headers: HashMap::new(),
        timeout: Duration::from_millis(500),
        policy: HttpPolicy {
            allow_plaintext_http: true,
            allow_local_network: true,
        },
    };
    let res = open(cfg, Duration::from_millis(300)).await;
    let err = match res {
        Ok(_) => panic!("open must time out without endpoint event"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(
        msg.to_ascii_lowercase().contains("timeout")
            || msg.to_ascii_lowercase().contains("timed out"),
        "expected a timeout-flavored error; got: {msg}",
    );
    drop(shutdown_tx);
}

#[tokio::test]
async fn open_refuses_url_pointing_at_metadata_service_even_with_local_network_allowed() {
    // Spec: even when `allow_local_network=true`, hostnames in the
    // hardcoded forbidden list (cloud metadata IPs / FQDNs) are still
    // refused. Without this, an operator who flips the local-network knob
    // for legitimate intranet servers would simultaneously unlock SSRF to
    // the cloud metadata endpoint.
    //
    // We don't actually connect — `check_url_syntactic` runs first inside
    // `open()` and fails before any network IO. Using a metadata hostname
    // that the URL guard hardcodes (not a DNS lookup) keeps the test
    // hermetic.
    let cfg = SseConfig {
        server_name: "metadata".into(),
        url: "http://metadata.google.internal/computeMetadata/v1/sse".into(),
        headers: HashMap::new(),
        timeout: Duration::from_secs(1),
        policy: HttpPolicy {
            allow_plaintext_http: true,
            allow_local_network: true,
        },
    };
    let res = open(cfg, Duration::from_secs(1)).await;
    let err = match res {
        Ok(_) => panic!("metadata FQDNs must be refused regardless of local-network flag"),
        Err(e) => e,
    };
    let msg = format!("{err}").to_ascii_lowercase();
    assert!(
        msg.contains("local") || msg.contains("refus") || msg.contains("forbid"),
        "expected a URL-refusal error; got: {msg}",
    );
}

#[tokio::test]
async fn send_round_trips_through_post_and_recv_drains_pumped_events() {
    // Spec: `send()` POSTs to the server's advertised endpoint and `recv()`
    // drains messages the SSE pump has read off the long-lived stream.
    // Together these are the JSON-RPC round-trip the supervisor runs
    // millions of times per session — if this breaks, no streamable-HTTP /
    // SSE server is usable.
    let (fixture, events_tx) = SseFixture::spawn().await;
    let mut transport = open(fixture.config(), Duration::from_secs(2))
        .await
        .expect("open");

    // 1. Server pushes a response.
    events_tx
        .send(jsonrpc_message_event(json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {"ok": true},
        })))
        .await
        .expect("push event");

    // 2. Client sends a request (we don't actually correlate ids here — we're
    //    exercising the wire path, not the SessionActor).
    transport
        .send(json!({"jsonrpc": "2.0", "id": 42, "method": "ping"}))
        .await
        .expect("send");

    // 3. The pump should have read the response event. Wait for it; the
    //    pump is async and the event could arrive before or after the POST.
    let msg = tokio::time::timeout(Duration::from_secs(2), transport.recv())
        .await
        .expect("recv timed out")
        .expect("recv error")
        .expect("eof before frame");
    assert_eq!(msg.id, Some(42));
    assert_eq!(msg.result.unwrap()["ok"], true);

    // 4. The fixture should have captured the POST body.
    let posted = fixture.posted.lock().await;
    assert_eq!(posted.len(), 1, "expected exactly one POST, got {posted:?}");
    assert_eq!(posted[0]["method"], "ping");
    assert_eq!(posted[0]["id"], 42);
}

#[tokio::test]
async fn pump_ignores_malformed_frame_and_continues_recv() {
    // Spec: a malformed JSON-RPC frame pushed over the SSE stream is logged
    // and ignored, NOT propagated as a fatal error. The pump continues so a
    // single garbled frame from a flaky server can't tear down a healthy
    // session. We assert by pushing a malformed event, then a clean
    // response, and expecting `recv()` to surface only the clean one.
    let (fixture, events_tx) = SseFixture::spawn().await;
    let mut transport = open(fixture.config(), Duration::from_secs(2))
        .await
        .expect("open");

    events_tx
        .send(Event::default().event("message").data("{not json"))
        .await
        .expect("push garbage");
    events_tx
        .send(jsonrpc_message_event(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"recovered": true},
        })))
        .await
        .expect("push valid");

    let msg = tokio::time::timeout(Duration::from_secs(2), transport.recv())
        .await
        .expect("recv timed out")
        .expect("recv error")
        .expect("eof before frame");
    assert_eq!(msg.id, Some(1));
    assert_eq!(msg.result.unwrap()["recovered"], true);
}
