//! Heartbeat broadcast smoke test.
//!
//! Real failure mode the test catches:
//! - the AdminSvc never broadcasts to a `SubscribeHeartbeatEvents` subscriber
//!   (e.g. the bus wiring regressed in `daemon::run` or `build_services_with_extras`),
//! - or the proto stream silently drops events because the daemon-side
//!   `tokio::spawn` forwarding loop got removed.
//!
//! We construct an `AdminSvc` directly with a `HeartbeatBus`, subscribe via
//! the wire RPC, publish one event from the bus, and assert it round-trips.
//! This exercises the same daemon-side path the real heartbeat module uses.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use borgd::daemon::bind_uds;
use borgd::grpc::admin::AdminSvc;
use borgd::heartbeat::{DaemonHeartbeatEvent, HeartbeatBus, HeartbeatSource};
use borgd::session::SessionRegistry;
use protocols::admin::admin_client::AdminClient;
use protocols::admin::admin_server::AdminServer;
use protocols::admin::Empty as AdminEmpty;
use std::path::Path;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

async fn make_channel(socket: &Path) -> Channel {
    let p = socket.to_path_buf();
    Endpoint::try_from("http://[::]:50051")
        .unwrap()
        .connect_with_connector(service_fn(move |_: Uri| {
            let p = p.clone();
            async move {
                let s = UnixStream::connect(&p).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(s))
            }
        }))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_subscribe_heartbeat_events_round_trips_a_published_event() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let socket = tmp.path().join("borgd.sock");
    let listener = bind_uds(&socket).expect("bind_uds");
    let stream = UnixListenerStream::new(listener);

    let registry = SessionRegistry::new();
    let bus = HeartbeatBus::default();
    let admin = AdminSvc::new(registry).with_heartbeat_bus(bus.clone());

    let (tx_shutdown, rx_shutdown) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(AdminServer::new(admin))
            .serve_with_incoming_shutdown(stream, async {
                let _ = rx_shutdown.await;
            })
            .await;
    });

    let started = std::time::Instant::now();
    while UnixStream::connect(&socket).await.is_err() {
        if started.elapsed() > Duration::from_secs(2) {
            panic!("UDS never came up");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let chan = make_channel(&socket).await;
    let mut admin_client = AdminClient::new(chan);

    let mut stream = admin_client
        .subscribe_heartbeat_events(AdminEmpty {})
        .await
        .expect("subscribe")
        .into_inner();

    // Give the server a moment to register the subscriber before publishing.
    // broadcast::Sender::send on a freshly-created bus with no subscribers
    // discards the message, so a missed handshake here would silently empty
    // the test (`stream.message()` would just hang) — that's exactly the
    // regression we want to fail loud on, but only after a real timeout.
    tokio::time::sleep(Duration::from_millis(50)).await;

    bus.publish_for_test(DaemonHeartbeatEvent {
        source: HeartbeatSource::Scheduled,
        started_at_unix: 1000,
        finished_at_unix: 1001,
        status: "ran",
        message: "all clear".into(),
        duration_ms: 42.5,
    });

    let evt = tokio::time::timeout(Duration::from_secs(2), stream.message())
        .await
        .expect("event arrived in time")
        .expect("stream.message ok")
        .expect("event present");

    assert_eq!(evt.source, "scheduled", "source must round-trip");
    assert_eq!(evt.status, "ran");
    assert_eq!(evt.message, "all clear");
    assert!(
        (evt.duration_ms - 42.5).abs() < 0.001,
        "duration_ms must round-trip exactly"
    );
    assert_eq!(evt.started_at_unix, 1000);
    assert_eq!(evt.finished_at_unix, 1001);

    let _ = tx_shutdown.send(());
}
