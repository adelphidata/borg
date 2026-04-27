//! Smoke test for `Admin.ExportSession`.
//!
//! Real failure modes this guards against:
//!   1. Daemon-side handler doesn't load messages (would emit a session
//!      header with no body, both content checks fail).
//!   2. The proto `format` string is unhandled (the handler returns
//!      InvalidArgument, the test sees an Err and fails).
//!   3. The bytes round-trip drops content (would reduce length to zero).
//!
//! Mirrors the harness pattern in `admin_rpc_smoke.rs` (process-global
//! `BORG_DATA_DIR`, serialized via mutex).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use borg_core::db::Database;
use borg_proto::admin::admin_client::AdminClient;
use borg_proto::admin::admin_server::AdminServer;
use borg_proto::admin::ExportSessionRequest;
use borgd::daemon::bind_uds;
use borgd::grpc::admin::AdminSvc;
use borgd::session::SessionRegistry;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

static SERIAL: Mutex<()> = Mutex::new(());

async fn channel(socket: &Path) -> Channel {
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

#[tokio::test]
async fn export_session_round_trips_messages() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: tests in this binary serialize through SERIAL above.
    unsafe {
        std::env::set_var("BORG_DATA_DIR", tmp.path());
    }

    // Seed a session with one user + one assistant message via direct DB
    // write (the canonical writer the daemon's reader will read back).
    let session_id = "export-smoke-1";
    {
        let db = Database::open().expect("seed db");
        let now = chrono::Utc::now().timestamp();
        db.upsert_session(session_id, now, now, 0, "test-model", "Smoke Session")
            .expect("upsert");
        db.insert_message(
            session_id,
            "user",
            Some("UNIQUE-USER-NEEDLE"),
            None,
            None,
            None,
            None,
        )
        .expect("user msg");
        db.insert_message(
            session_id,
            "assistant",
            Some("UNIQUE-ASSISTANT-NEEDLE"),
            None,
            None,
            None,
            None,
        )
        .expect("assistant msg");
    }

    let socket = tmp.path().join("borgd.sock");
    let listener = bind_uds(&socket).expect("bind_uds");
    let stream = UnixListenerStream::new(listener);
    let registry = SessionRegistry::new();
    let admin = AdminSvc::new(registry);

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(AdminServer::new(admin))
            .serve_with_incoming_shutdown(stream, async {
                let _ = shutdown_rx.await;
            })
            .await;
    });
    let started = std::time::Instant::now();
    while UnixStream::connect(&socket).await.is_err() {
        if started.elapsed() > Duration::from_secs(2) {
            panic!("daemon UDS never became connectable");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let chan = channel(&socket).await;
    let mut admin = AdminClient::new(chan);

    // The cli's `borg export` defaults to JSON. Exercise that path so the
    // assertion below targets the real default — a regression that
    // unhandled "json" would fail here, not in a follow-up test.
    let resp = admin
        .export_session(ExportSessionRequest {
            session_id: session_id.into(),
            format: "json".into(),
        })
        .await
        .expect("export rpc")
        .into_inner();

    assert!(
        !resp.content.is_empty(),
        "export must return non-empty bytes"
    );
    assert!(
        !resp.suggested_filename.is_empty(),
        "suggested_filename must be populated for cli's --output flag"
    );
    let text = std::str::from_utf8(&resp.content).expect("export bytes are utf-8");
    assert!(
        text.contains("UNIQUE-USER-NEEDLE"),
        "user message must round-trip into export; got: {text}"
    );
    assert!(
        text.contains("UNIQUE-ASSISTANT-NEEDLE"),
        "assistant message must round-trip into export; got: {text}"
    );

    // Unknown format rejected with InvalidArgument — guards against the
    // daemon swallowing a bad format silently and returning empty bytes.
    let err = admin
        .export_session(ExportSessionRequest {
            session_id: session_id.into(),
            format: "yaml".into(),
        })
        .await
        .expect_err("yaml must be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    drop(shutdown_tx);
}
