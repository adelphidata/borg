//! End-to-end Signal SSE pipeline test: a stubbed signal-cli daemon feeds one
//! inbound event to a running `GatewayServer`, a stub `SessionExecutor` echoes
//! the turn text, and the reply must come back to the daemon as a `send` RPC.
//!
//! Hermetic: `BORG_HOME` points at a tempdir, the signal-cli daemon is a
//! loopback axum stub, the agent is an echo stub, and every other channel's
//! credentials are cleared. No real network, no real `~/.borg`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use cortex::agent::AgentEvent;
use cortex::config::Config;
use futures::StreamExt as _;
use gateway::session_executor::{InboundTurn, SessionExecutor};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const OWN_ACCOUNT: &str = "+15550001111";
const SENDER: &str = "+15557778888";

struct EchoExec;

#[async_trait::async_trait]
impl SessionExecutor for EchoExec {
    async fn start_turn(
        &self,
        turn: InboundTurn,
    ) -> anyhow::Result<(mpsc::Receiver<AgentEvent>, CancellationToken)> {
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move {
            let _ = tx
                .send(AgentEvent::TextDelta(format!("echo:{}", turn.text)))
                .await;
            let _ = tx.send(AgentEvent::TurnComplete).await;
        });
        Ok((rx, CancellationToken::new()))
    }
    async fn cancel(&self, _session_key: &str) -> bool {
        false
    }
    async fn cancel_all(&self) -> usize {
        0
    }
    async fn active_turns(&self) -> usize {
        0
    }
}

/// Messages the stub daemon received via `send` RPCs.
type SentMessages = Arc<Mutex<Vec<String>>>;

async fn rpc_handler(
    State(sent): State<SentMessages>,
    Json(req): Json<serde_json::Value>,
) -> impl IntoResponse {
    let id = req.get("id").cloned().unwrap_or(serde_json::json!(1));
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let result = match method {
        "version" => serde_json::json!({ "version": "0.13.0" }),
        "send" => {
            if let Some(msg) = req
                .get("params")
                .and_then(|p| p.get("message"))
                .and_then(|m| m.as_str())
            {
                sent.lock().unwrap().push(msg.to_string());
            }
            serde_json::json!({ "timestamp": 1 })
        }
        _ => serde_json::json!({}),
    };
    Json(serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// One inbound text message, then the stream stays open like a live daemon.
async fn events_handler() -> impl IntoResponse {
    let envelope = serde_json::json!({
        "envelope": {
            "source": SENDER,
            "timestamp": 1_723_000_000_000_i64,
            "dataMessage": {
                "timestamp": 1_723_000_000_000_i64,
                "message": "hi"
            }
        },
        "account": OWN_ACCOUNT
    });
    let frame = format!("data: {envelope}\n\n");
    let stream = futures::stream::once(async move {
        Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(frame))
    })
    .chain(futures::stream::pending());
    (
        [("content-type", "text/event-stream")],
        axum::body::Body::from_stream(stream),
    )
}

#[tokio::test]
async fn signal_sse_event_round_trips_agent_echo_to_send_rpc() {
    // Hermetic env: temp BORG_HOME, only Signal configured.
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("BORG_HOME", tmp.path());
    for key in [
        "TELEGRAM_BOT_TOKEN",
        "SLACK_BOT_TOKEN",
        "SLACK_SIGNING_SECRET",
        "TWILIO_ACCOUNT_SID",
        "TWILIO_AUTH_TOKEN",
        "DISCORD_BOT_TOKEN",
        "TEAMS_APP_ID",
        "TEAMS_APP_SECRET",
        "GOOGLE_CHAT_SERVICE_ACCOUNT",
        "GOOGLE_CHAT_TOKEN",
    ] {
        std::env::remove_var(key);
    }
    std::env::set_var("SIGNAL_ACCOUNT", OWN_ACCOUNT);

    // Stub signal-cli daemon.
    let sent: SentMessages = Arc::new(Mutex::new(Vec::new()));
    let stub = Router::new()
        .route("/api/v1/rpc", post(rpc_handler))
        .route("/api/v1/events", get(events_handler))
        .with_state(sent.clone());
    let stub_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stub_port = stub_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(stub_listener, stub).await;
    });

    // Free port for the gateway itself.
    let port_probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_port = port_probe.local_addr().unwrap().port();
    drop(port_probe);

    let mut config = Config::default();
    config.gateway.host = "127.0.0.1".to_string();
    config.gateway.port = gateway_port;
    config.gateway.dm_policy = cortex::pairing::DmPolicy::Open;
    config.gateway.signal_cli_host = Some("127.0.0.1".to_string());
    config.gateway.signal_cli_port = Some(stub_port);

    let shutdown = CancellationToken::new();
    let provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder().build();
    let metrics = {
        use opentelemetry::metrics::MeterProvider as _;
        cortex::telemetry::BorgMetrics::from_meter(&provider.meter("signal-sse-test"))
    };
    let server = gateway::GatewayServer::new(
        config,
        shutdown.clone(),
        metrics,
        None,
        Some(Arc::new(EchoExec)),
        None,
    )
    .unwrap();
    let run = tokio::spawn(server.run());

    // The inbound "hi" must reach the echo agent and its reply must be sent
    // back through the daemon's `send` RPC. This is the SSE callback's
    // deliver-non-empty-responses contract.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let echoed = loop {
        {
            let msgs = sent.lock().unwrap();
            if let Some(m) = msgs.iter().find(|m| m.contains("echo:")) {
                break m.clone();
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "gateway never delivered the agent reply to the signal daemon; sends so far: {:?}",
            sent.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        echoed.contains("hi"),
        "the reply must echo the inbound text, got: {echoed}"
    );

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), run).await;
}
