//! Heartbeat broadcast smoke test.
//!
//! Exercises the Admin service stream directly without a transport layer. The
//! contract we care about here is the daemon-side event forwarding: a
//! published heartbeat event must reach a subscriber with the right source and
//! skip metadata intact.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use borgd::grpc::admin::AdminSvc;
use borgd::heartbeat::{DaemonHeartbeatEvent, HeartbeatBus, HeartbeatSource};
use borgd::session::SessionRegistry;
use futures::StreamExt;
use protocols::admin::admin_server::Admin;
use protocols::admin::Empty as AdminEmpty;
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
