//! Admin heartbeat-trigger RPC smoke tests.
//!
//! Verifies the daemon-side service contract directly: `TriggerHeartbeat`
//! preserves the requested source, and `Poke` still maps to a scheduled
//! bypass-quiet-hours trigger.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use borgd::grpc::admin::AdminSvc;
use borgd::heartbeat::{HeartbeatSource, HeartbeatTrigger};
use borgd::session::SessionRegistry;
use protocols::admin::admin_server::Admin;
use protocols::admin::{Empty as AdminEmpty, TriggerHeartbeatRequest};
use tokio::sync::mpsc;
use tonic::Request;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_trigger_heartbeat_and_poke_enqueue_expected_requests() {
    let registry = SessionRegistry::new();
    let (tx, mut rx) = mpsc::channel::<HeartbeatTrigger>(8);
    let admin = AdminSvc::new(registry).with_heartbeat_trigger_sender(tx);

    Admin::trigger_heartbeat(
        &admin,
        Request::new(TriggerHeartbeatRequest {
            source: "session_start".into(),
            bypass_quiet_hours: true,
        }),
    )
    .await
    .expect("trigger_heartbeat");
    let trigger = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("trigger arrived")
        .expect("channel open");
    assert_eq!(trigger.source, HeartbeatSource::SessionStart);
    assert!(trigger.bypass_quiet_hours);

    Admin::poke(&admin, Request::new(AdminEmpty {}))
        .await
        .expect("poke");
    let poke = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("poke arrived")
        .expect("channel open");
    assert_eq!(poke.source, HeartbeatSource::Scheduled);
    assert!(poke.bypass_quiet_hours, "poke must bypass quiet hours");
}
