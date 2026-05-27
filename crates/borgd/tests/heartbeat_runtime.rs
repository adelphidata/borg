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
/// the test controls. Two modes:
///
/// - `close_after: false` — holds the sender end so the receiver never
///   closes, simulating the "agent parked waiting for user input" hang
///   that motivated the heartbeat-turn timeout.
/// - `close_after: true` — drops the sender after delivering the script,
///   simulating a clean turn-complete (the executor's bridge would close
///   the channel after a `TurnComplete` event).
struct StubExecutor {
    /// Events delivered (in order) on every `start_turn`. Drained on first
    /// call so a single executor only fires one heartbeat turn.
    script: Mutex<Vec<AgentEvent>>,
    /// When true, drops the sender after sending the script — the heartbeat
    /// helper sees a clean channel close (`Completed`). When false, the
    /// sender is held in `held_senders` so the channel stays open until the
    /// wall-clock timeout fires.
    close_after: bool,
    /// Senders held open across the test lifetime — only populated when
    /// `close_after == false`.
    held_senders: Mutex<Vec<mpsc::Sender<AgentEvent>>>,
    /// Count of `start_turn` invocations. The
    /// `no_llm_provider` short-circuit asserts this stays at zero — the
    /// guard MUST skip upstream of the executor, not after it has already
    /// spun up an agent backend whose `LlmClient::new` would then bail.
    start_turn_calls: std::sync::atomic::AtomicUsize,
}

impl StubExecutor {
    fn hanging(events: Vec<AgentEvent>) -> Self {
        Self {
            script: Mutex::new(events),
            close_after: false,
            held_senders: Mutex::new(Vec::new()),
            start_turn_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn completing(events: Vec<AgentEvent>) -> Self {
        Self {
            script: Mutex::new(events),
            close_after: true,
            held_senders: Mutex::new(Vec::new()),
            start_turn_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn start_turn_count(&self) -> usize {
        self.start_turn_calls
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl SessionExecutor for StubExecutor {
    async fn start_turn(
        &self,
        _turn: InboundTurn,
    ) -> anyhow::Result<(mpsc::Receiver<AgentEvent>, CancellationToken)> {
        self.start_turn_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (tx, rx) = mpsc::channel::<AgentEvent>(8);
        let events = std::mem::take(&mut *self.script.lock().await);
        for evt in events {
            tx.send(evt).await.expect("send scripted event");
        }
        if !self.close_after {
            self.held_senders.lock().await.push(tx);
        }
        Ok((rx, CancellationToken::new()))
    }

    async fn cancel(&self, _session_key: &str) -> bool {
        true
    }

    async fn cancel_all(&self) -> usize {
        0
    }
}

/// Regression guard: when the heartbeat agent emits a `UserInputRequest`,
/// the bus MUST see a terminal event within budget.
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

    let executor = Arc::new(StubExecutor::hanging(vec![AgentEvent::SystemNotice {
        category: PROMPT_REQUEST_USER_INPUT.into(),
        text: "1 questions".into(),
    }]));

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

    let executor = Arc::new(StubExecutor::hanging(Vec::new()));

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

/// Positive integration guard for the heartbeat happy path. The prompt-request
/// short-circuit and wall-clock timeout fixes added on top of
/// `run_and_broadcast` must not regress the case where the agent emits real
/// content and closes the channel cleanly: that turn must publish a terminal
/// `ran` event carrying the assistant's text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_subscribe_heartbeat_events_emits_ran_when_agent_completes_with_text() {
    let registry = SessionRegistry::new();
    let bus = HeartbeatBus::default();
    let admin = AdminSvc::new(registry).with_heartbeat_bus(bus.clone());

    let resp = Admin::subscribe_heartbeat_events(&admin, Request::new(AdminEmpty {}))
        .await
        .expect("subscribe");
    let mut stream = resp.into_inner();

    // Mix in a non-prompt SystemNotice (e.g. posture transition) before the
    // text content to also pin that the prompt short-circuit doesn't
    // over-fire on unrelated notices.
    let executor = Arc::new(StubExecutor::completing(vec![
        AgentEvent::SystemNotice {
            category: "posture".into(),
            text: "Boosted".into(),
        },
        AgentEvent::TextDelta("3 unread emails from the team".into()),
    ]));

    let mut config = Config::default();
    config.heartbeat.quiet_hours_start = None;
    config.heartbeat.quiet_hours_end = None;
    config.heartbeat.turn_timeout_secs = 5;

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

    let evt = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .expect("terminal event must arrive promptly on clean completion")
        .expect("stream not closed")
        .expect("stream item ok");

    assert_eq!(
        evt.status, "ran",
        "clean completion with non-trivial text must emit ran"
    );
    assert_eq!(evt.source, "scheduled");
    assert_eq!(
        evt.message, "3 unread emails from the team",
        "ran event must carry the assistant's accumulated TextDelta payload"
    );
    assert_eq!(
        evt.skip_reason, "",
        "ran events have no skip_reason on the wire"
    );
}

/// Negative-positive integration guard: a heartbeat that returns a
/// zero-info ack ("ok") must publish `skipped` with `empty_response`, not
/// `ran`. Pre-fix the channel could hang indefinitely on a UserInputRequest;
/// post-fix it returns Completed and the existing zero-info filter applies.
/// This locks the boundary between Failed (prompt/timeout) and Skipped
/// (zero-info) so a future refactor can't collapse them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_subscribe_heartbeat_events_emits_skipped_when_agent_returns_zero_info() {
    let registry = SessionRegistry::new();
    let bus = HeartbeatBus::default();
    let admin = AdminSvc::new(registry).with_heartbeat_bus(bus.clone());

    let resp = Admin::subscribe_heartbeat_events(&admin, Request::new(AdminEmpty {}))
        .await
        .expect("subscribe");
    let mut stream = resp.into_inner();

    let executor = Arc::new(StubExecutor::completing(vec![AgentEvent::TextDelta(
        "ok".into(),
    )]));

    let mut config = Config::default();
    config.heartbeat.quiet_hours_start = None;
    config.heartbeat.quiet_hours_end = None;
    config.heartbeat.turn_timeout_secs = 5;

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

    let evt = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .expect("terminal event must arrive promptly on clean completion")
        .expect("stream not closed")
        .expect("stream item ok");

    assert_eq!(
        evt.status, "skipped",
        "zero-info ack must publish skipped, not ran"
    );
    assert_eq!(
        evt.skip_reason, "empty_response",
        "skip_reason must identify the zero-info filter"
    );
}

/// Regression guard for the onboarding-race UX bug: when
/// `Config.llm.provider` is unset, the heartbeat MUST publish
/// `skipped` with `no_llm_provider` *before* invoking the
/// executor.
///
/// Pre-fix: borgd let `executor.start_turn` run, the agent backend
/// constructed an `LlmClient`, `resolve_provider()` bailed, the agent
/// emitted an `Error` event, and `run_and_broadcast` published `failed`
/// — surfacing as `[heartbeat error: No LLM provider selected …]` in
/// the TUI right when the user finished onboarding. The exact wrong
/// first impression.
///
/// Post-fix: `execute_heartbeat_turn` checks `config.llm.provider`
/// upfront and returns `Skipped(NoLlmProvider)` so the bus emits
/// `skipped` instead. The TUI silently logs `no_llm_provider` (see
/// `proto_heartbeat_result_maps_no_llm_provider_via_wire_field`).
///
/// Two invariants pinned by this test:
///   1. The wire event is `status="skipped"` + `skip_reason="no_llm_provider"`
///      — NOT `failed`, otherwise the TUI re-surfaces the alarming toast.
///   2. `executor.start_turn` is NEVER called — the guard must be
///      upstream of the agent backend. Letting the executor run before
///      bailing would still construct an agent backend per
///      `DaemonSessionExecutor::ensure_host`, defeating the cost-savings
///      half of this fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_subscribe_heartbeat_events_skips_no_llm_provider_without_invoking_executor() {
    let registry = SessionRegistry::new();
    let bus = HeartbeatBus::default();
    let admin = AdminSvc::new(registry).with_heartbeat_bus(bus.clone());

    let resp = Admin::subscribe_heartbeat_events(&admin, Request::new(AdminEmpty {}))
        .await
        .expect("subscribe");
    let mut stream = resp.into_inner();

    // The executor is scripted to emit real text — if the guard ever
    // regresses and lets start_turn run, the bus would publish "ran"
    // with this payload instead of "skipped". The status assertion below
    // catches that regression directly.
    let executor = Arc::new(StubExecutor::completing(vec![AgentEvent::TextDelta(
        "this content must not appear on the wire".into(),
    )]));

    let mut config = Config::default();
    // The fix's input: provider explicitly unset, simulating the
    // onboarding-race window where the ConfigWatcher hasn't yet refreshed
    // the snapshot.
    config.llm.provider = None;
    config.heartbeat.quiet_hours_start = None;
    config.heartbeat.quiet_hours_end = None;
    config.heartbeat.turn_timeout_secs = 5;

    let trigger = HeartbeatTrigger {
        source: HeartbeatSource::SessionStart,
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

    let evt = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .expect("terminal skipped event must arrive promptly — the guard short-circuits")
        .expect("stream not closed")
        .expect("stream item ok");

    assert_eq!(
        evt.status, "skipped",
        "no LLM provider must surface as skipped, not failed — failed re-surfaces \
         the onboarding-race toast that motivated this guard"
    );
    assert_eq!(
        evt.skip_reason, "no_llm_provider",
        "skip_reason must identify the no-provider path so the TUI can route it \
         to the silent-log arm alongside quiet hours"
    );
    assert_eq!(
        executor.start_turn_count(),
        0,
        "the guard MUST fire before executor.start_turn — letting the executor \
         run would still construct an agent backend and waste the work the \
         skip-reason is meant to avoid"
    );

    // Pin the empty-provider treatment too: a whitespace-only provider is
    // not a real configuration and must take the same path. Without this
    // case, a `" "` value in the DB would slip past the guard and re-hit
    // the original failure.
    let executor_blank = Arc::new(StubExecutor::completing(vec![AgentEvent::TextDelta(
        "must not appear".into(),
    )]));
    config.llm.provider = Some("   ".into());

    let resp_blank = Admin::subscribe_heartbeat_events(&admin, Request::new(AdminEmpty {}))
        .await
        .expect("subscribe");
    let mut stream_blank = resp_blank.into_inner();

    let executor_blank_for_run = executor_blank.clone();
    let bus_for_blank = bus.clone();
    let config_for_blank = config.clone();
    tokio::spawn(async move {
        heartbeat::run_and_broadcast(
            executor_blank_for_run.as_ref(),
            &config_for_blank,
            HeartbeatTrigger {
                source: HeartbeatSource::SessionStart,
                bypass_quiet_hours: true,
            },
            &bus_for_blank,
        )
        .await;
    });

    let evt_blank = tokio::time::timeout(Duration::from_secs(3), stream_blank.next())
        .await
        .expect("blank-provider variant must also produce a terminal event promptly")
        .expect("stream not closed")
        .expect("stream item ok");

    assert_eq!(evt_blank.status, "skipped");
    assert_eq!(evt_blank.skip_reason, "no_llm_provider");
    assert_eq!(
        executor_blank.start_turn_count(),
        0,
        "whitespace-only provider must take the same short-circuit path as None"
    );
}
