//! Heartbeat broadcast smoke test.
//!
//! Exercises the Admin service stream directly without a transport layer. The
//! contract we care about here is the daemon-side event forwarding: a
//! published heartbeat event must reach a subscriber with the right source and
//! skip metadata intact.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use async_trait::async_trait;
use borgd::grpc::admin::AdminSvc;
use borgd::heartbeat::{
    self, DaemonHeartbeatEvent, HeartbeatBus, HeartbeatSource, HeartbeatTrigger,
};
use borgd::session::convert::PROMPT_REQUEST_USER_INPUT;
use borgd::session::SessionRegistry;
use cortex::agent::AgentEvent;
use cortex::config::Config;
use futures::StreamExt;
use gateway::{InboundTurn, SessionExecutor};
use protocols::admin::admin_server::Admin;
use protocols::admin::Empty as AdminEmpty;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use tonic::Request;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_subscribe_heartbeat_events_round_trips_a_published_event() {
    let registry = SessionRegistry::new();
    let bus = HeartbeatBus::default();
    let admin = AdminSvc::new(registry).with_heartbeat_bus(bus.clone());

    let resp = Admin::subscribe_heartbeat_events(&admin, Request::new(AdminEmpty {}))
        .await
        .expect("subscribe");
    let mut stream = resp.into_inner();

    bus.publish_for_test(DaemonHeartbeatEvent {
        source: HeartbeatSource::SessionStart,
        started_at_unix: 1000,
        finished_at_unix: 1001,
        status: "skipped",
        message: "quiet hours".into(),
        skip_reason: Some("quiet_hours"),
        duration_ms: 42.5,
    });

    let evt = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("event arrived in time")
        .expect("stream not closed")
        .expect("stream item ok");

    assert_eq!(evt.source, "session_start", "source must round-trip");
    assert_eq!(evt.status, "skipped");
    assert_eq!(evt.message, "quiet hours");
    assert_eq!(evt.skip_reason, "quiet_hours");
    assert!(
        (evt.duration_ms - 42.5).abs() < 0.001,
        "duration_ms must round-trip exactly"
    );
    assert_eq!(evt.started_at_unix, 1000);
    assert_eq!(evt.finished_at_unix, 1001);
}

/// `SessionExecutor` stub whose `start_turn` returns a hand-built channel
/// the test controls. Holds the sender end so the receiver never closes —
/// simulates the "agent parked waiting for user input" hang that motivated
/// the heartbeat-turn timeout.
struct StubExecutor {
    /// Optional event to deliver immediately on start_turn (e.g. a
    /// `prompt_request:user_input` SystemNotice). When `None`, the channel
    /// stays empty and the heartbeat must rely on the wall-clock timeout.
    inject: Mutex<Option<AgentEvent>>,
    /// Held senders so the channel never closes for the duration of the test.
    held_senders: Mutex<Vec<mpsc::Sender<AgentEvent>>>,
}

#[async_trait]
impl SessionExecutor for StubExecutor {
    async fn start_turn(
        &self,
        _turn: InboundTurn,
    ) -> anyhow::Result<(mpsc::Receiver<AgentEvent>, CancellationToken)> {
        let (tx, rx) = mpsc::channel::<AgentEvent>(8);
        if let Some(evt) = self.inject.lock().await.take() {
            tx.send(evt).await.expect("send injected event");
        }
        self.held_senders.lock().await.push(tx);
        Ok((rx, CancellationToken::new()))
    }

    async fn cancel(&self, _session_key: &str) -> bool {
        true
    }

    async fn cancel_all(&self) -> usize {
        0
    }
}

/// Regression guard for `issues/empty-state-heartbeat-poke-yields-zero-*.md`.
///
/// Pre-fix: when the heartbeat agent emitted a `UserInputRequest`, the wire
/// proto event was silently dropped by `convert::from_proto`, the executor
/// bridge produced no `CoreEvent`, and `execute_heartbeat_turn` waited on a
/// never-closing channel until the `request_user_input` tool's 5-minute
/// oneshot fired — well past the smorgasborg eval's 90s budget. The bus
/// never saw a terminal event.
///
/// Post-fix: `from_proto` lowers `UserInputRequest` to a SystemNotice with
/// `prompt_request:user_input`, and `execute_heartbeat_turn` short-circuits
/// on it, returning `Failed`. `run_and_broadcast` then publishes a
/// terminal `failed` event to the bus inside the timeout window.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_subscribe_heartbeat_events_emits_terminal_event_when_turn_hangs_on_user_input() {
    let registry = SessionRegistry::new();
    let bus = HeartbeatBus::default();
    let admin = AdminSvc::new(registry).with_heartbeat_bus(bus.clone());

    let resp = Admin::subscribe_heartbeat_events(&admin, Request::new(AdminEmpty {}))
        .await
        .expect("subscribe");
    let mut stream = resp.into_inner();

    let executor = Arc::new(StubExecutor {
        inject: Mutex::new(Some(AgentEvent::SystemNotice {
            category: PROMPT_REQUEST_USER_INPUT.into(),
            text: "1 questions".into(),
        })),
        held_senders: Mutex::new(Vec::new()),
    });

    // Disable quiet-hours and zero-info paths that would also produce a
    // terminal event — we want the assertion to specifically cover the
    // prompt-request short-circuit.
    let mut config = Config::default();
    config.heartbeat.quiet_hours_start = None;
    config.heartbeat.quiet_hours_end = None;
    config.heartbeat.turn_timeout_secs = 5;

    let trigger = HeartbeatTrigger {
        source: HeartbeatSource::Scheduled,
        bypass_quiet_hours: true,
    };

    // Run on a separate task so the assertion below can race the publish.
    let executor_for_run = executor.clone();
    let bus_for_run = bus.clone();
    let config_for_run = config.clone();
    tokio::spawn(async move {
        heartbeat::run_and_broadcast(
            executor_for_run.as_ref(),
            &config_for_run,
            trigger,
            &bus_for_run,
        )
        .await;
    });

    let evt = tokio::time::timeout(Duration::from_secs(8), stream.next())
        .await
        .expect("terminal event must arrive well under turn_timeout_secs slack")
        .expect("stream not closed")
        .expect("stream item ok");

    assert_eq!(
        evt.status, "failed",
        "interactive prompt during heartbeat must surface as failed"
    );
    assert_eq!(evt.source, "scheduled");
    assert!(
        evt.message.contains("interactive input"),
        "message should explain the failure mode, got {:?}",
        evt.message
    );
}

/// Companion guard: even with no events at all on the channel, the bus
/// must still emit a terminal event within the configured timeout window.
/// Covers the case where the LLM provider hangs entirely (no UserInputRequest,
/// no TextDelta, no TurnComplete).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_subscribe_heartbeat_events_emits_terminal_event_on_hang_timeout() {
    let registry = SessionRegistry::new();
    let bus = HeartbeatBus::default();
    let admin = AdminSvc::new(registry).with_heartbeat_bus(bus.clone());

    let resp = Admin::subscribe_heartbeat_events(&admin, Request::new(AdminEmpty {}))
        .await
        .expect("subscribe");
    let mut stream = resp.into_inner();

    let executor = Arc::new(StubExecutor {
        inject: Mutex::new(None),
        held_senders: Mutex::new(Vec::new()),
    });

    let mut config = Config::default();
    config.heartbeat.quiet_hours_start = None;
    config.heartbeat.quiet_hours_end = None;
    // Tight timeout so the test is fast.
    config.heartbeat.turn_timeout_secs = 1;

    let trigger = HeartbeatTrigger {
        source: HeartbeatSource::Scheduled,
        bypass_quiet_hours: true,
    };

    let executor_for_run = executor.clone();
    let bus_for_run = bus.clone();
    let config_for_run = config.clone();
    tokio::spawn(async move {
        heartbeat::run_and_broadcast(
            executor_for_run.as_ref(),
            &config_for_run,
            trigger,
            &bus_for_run,
        )
        .await;
    });

    let evt = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("terminal event must arrive within timeout + slack")
        .expect("stream not closed")
        .expect("stream item ok");

    assert_eq!(
        evt.status, "failed",
        "wall-clock hang must surface as failed, not silent"
    );
    assert!(
        evt.message.contains("timeout"),
        "message should mention timeout, got {:?}",
        evt.message
    );
}
