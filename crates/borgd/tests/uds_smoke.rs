// Integration tests are a separate compilation unit and don't pick up the
// crate-level `cfg(test)` allow for expect/unwrap. Allowed here for the same
// reason: failed setup means the test environment itself is broken.
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! End-to-end smoke test: spin a real borgd against a temp BORG_HOME,
//! connect over UDS via tonic, call Status RPCs, and assert observable
//! behaviour. Exercises pidlock + UDS bind + tonic server + the actual
//! `StatusSvc`/`AdminSvc` impls (not test fixtures).
//!
//! No mocking — failures here mean the daemon's transport or service wiring
//! is broken.

use borg_proto::admin::admin_client::AdminClient;
use borg_proto::admin::admin_server::AdminServer;
use borg_proto::admin::Empty as AdminEmpty;
use borg_proto::status::status_client::StatusClient;
use borg_proto::status::status_server::StatusServer;
use borg_proto::status::Empty as StatusEmpty;
use borgd::daemon::bind_uds;
use borgd::grpc::{admin::AdminSvc, status::StatusSvc};
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

/// Spawn the real Status+Admin services on a UDS inside `tmp` using the
/// production `bind_uds` helper. Returns the socket path and a shutdown
/// sender.
async fn spawn_real_services(tmp: &std::path::Path) -> (PathBuf, tokio::sync::oneshot::Sender<()>) {
    let socket = tmp.join("borgd.sock");
    let listener = bind_uds(&socket).expect("bind_uds");
    let stream = UnixListenerStream::new(listener);

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(StatusServer::new(StatusSvc::new()))
            .add_service(AdminServer::new(AdminSvc::new()))
            .serve_with_incoming_shutdown(stream, async {
                let _ = rx.await;
            })
            .await;
    });

    // tonic doesn't expose a "ready" signal for serve_with_incoming, so
    // poll-until-connect.
    let started = std::time::Instant::now();
    loop {
        if UnixStream::connect(&socket).await.is_ok() {
            break;
        }
        if started.elapsed() > Duration::from_secs(2) {
            panic!("daemon UDS never became connectable");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (socket, tx)
}

async fn uds_channel(socket: PathBuf) -> Channel {
    Endpoint::try_from("http://[::]:50051")
        .expect("endpoint")
        .connect_with_connector(service_fn(move |_: Uri| {
            let socket = socket.clone();
            async move {
                let stream = UnixStream::connect(&socket).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
        .expect("connect")
}

#[tokio::test]
async fn status_rpcs_reach_real_service_impl_over_uds() {
    // Real failure mode: tonic transport / UDS / codec wiring breaks, OR the
    // service registration in grpc::build_services drifts. Exercises the
    // production StatusSvc, not a test fixture.
    let tmp = tempfile::tempdir().expect("tempdir");
    let (socket, _shutdown) = spawn_real_services(tmp.path()).await;

    let mut client = StatusClient::new(uds_channel(socket).await);
    let vitals = client
        .get_vitals(StatusEmpty {})
        .await
        .expect("get_vitals")
        .into_inner();
    let posture = client
        .get_posture(StatusEmpty {})
        .await
        .expect("get_posture")
        .into_inner();

    // StatusSvc currently returns the v1 placeholder shape. These assertions
    // pin the contract: stage starts at "Base", posture at "Balanced" with a
    // 1.0 multiplier (no rate adjustment until security-posture lands). When
    // Task 4 wires real state, these tests must be updated alongside the impl.
    assert_eq!(vitals.stage, "Base");
    assert_eq!(posture.posture, "Balanced");
    assert!(
        (posture.xp_multiplier - 1.0).abs() < f64::EPSILON,
        "neutral multiplier; got {}",
        posture.xp_multiplier
    );
}

#[tokio::test]
async fn admin_unimplemented_rpcs_return_unimplemented_status() {
    // Real failure mode: someone replaces the unimplemented stubs with `Ok`
    // before the real backend is wired, silently giving callers wrong data.
    // A future Task 6 will replace this assertion with real-behaviour tests.
    let tmp = tempfile::tempdir().expect("tempdir");
    let (socket, _shutdown) = spawn_real_services(tmp.path()).await;

    let mut client = AdminClient::new(uds_channel(socket).await);
    let err = client
        .set_setting(borg_proto::admin::SettingMutation {
            key: "x".into(),
            value: "y".into(),
        })
        .await
        .expect_err("must fail until Task 6 wires SettingsResolver");
    assert_eq!(err.code(), tonic::Code::Unimplemented);

    // Heal *is* a stub Ok in v1 (returns an empty report). Asserting the
    // Ok-shape verifies the codec round-trip on a non-trivial message type.
    let report = client
        .heal(AdminEmpty {})
        .await
        .expect("heal stub returns Ok")
        .into_inner();
    assert_eq!(report.warnings.len(), 0);
}
