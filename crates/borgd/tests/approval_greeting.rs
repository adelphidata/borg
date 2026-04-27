//! `Admin.SendApprovalGreeting` RPC contract test.
//!
//! Real failure modes the test catches:
//! - the RPC isn't wired (returns `Unimplemented`),
//! - the executor isn't installed via `with_executor` (returns
//!   `FailedPrecondition`),
//! - empty channel/sender_id are silently accepted instead of rejected
//!   (a regression that would cause confused log output if a buggy caller
//!   forgets to populate the message).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::await_holding_lock)]

use borgd::daemon::bind_uds;
use borgd::grpc::admin::AdminSvc;
use borgd::session::{DaemonSessionExecutor, SessionRegistry};
use cortex::config::Config;
use cortex::telemetry::BorgMetrics;
use protocols::admin::admin_client::AdminClient;
use protocols::admin::admin_server::AdminServer;
use protocols::admin::ApprovalGreetingRequest;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

// Process-global; `Database::open()` resolves BORG_DATA_DIR every call.
static SERIAL: Mutex<()> = Mutex::new(());

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
async fn send_approval_greeting_rejects_empty_args_and_requires_executor() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: tests in this binary serialize via SERIAL.
    unsafe {
        std::env::set_var("BORG_DATA_DIR", tmp.path());
    }

    let socket = tmp.path().join("borgd.sock");
    let listener = bind_uds(&socket).expect("bind_uds");
    let stream = UnixListenerStream::new(listener);

    let registry = SessionRegistry::new();

    // First: AdminSvc with NO executor wired — RPC must surface a clean
    // FailedPrecondition rather than panicking or returning Ok and dropping
    // the request silently.
    let admin_no_exec = AdminSvc::new(registry.clone());

    let (tx_shutdown, rx_shutdown) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(AdminServer::new(admin_no_exec))
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
    let mut client = AdminClient::new(chan);

    // Empty channel → InvalidArgument.
    let err = client
        .send_approval_greeting(ApprovalGreetingRequest {
            channel: String::new(),
            sender_id: "+15551234".into(),
        })
        .await
        .expect_err("empty channel must be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    // Without executor wired → FailedPrecondition.
    let err = client
        .send_approval_greeting(ApprovalGreetingRequest {
            channel: "imessage".into(),
            sender_id: "+15551234".into(),
        })
        .await
        .expect_err("missing executor must be rejected");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    let _ = tx_shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_approval_greeting_accepts_request_when_executor_wired() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let tmp = tempfile::tempdir().expect("tempdir");
    unsafe {
        std::env::set_var("BORG_DATA_DIR", tmp.path());
    }
    // Materialize DB so Config::load_from_db() inside the spawned greeting
    // task has something to read; the agent will then fail to construct
    // (no LLM provider) — that's logged + ignored, which is the design.
    let _ = cortex::db::Database::open().expect("seed db");

    let socket = tmp.path().join("borgd.sock");
    let listener = bind_uds(&socket).expect("bind_uds");
    let stream = UnixListenerStream::new(listener);

    let registry = SessionRegistry::new();
    let executor = Arc::new(DaemonSessionExecutor::new(
        registry.clone(),
        Config::default(),
        BorgMetrics::noop(),
    ));
    let admin = AdminSvc::new(registry).with_executor(executor);

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
    let mut client = AdminClient::new(chan);

    // Wired executor: RPC must return Ok (greeting itself runs in a
    // detached task and is allowed to fail later).
    client
        .send_approval_greeting(ApprovalGreetingRequest {
            channel: "imessage".into(),
            sender_id: "+15551234".into(),
        })
        .await
        .expect("RPC must return Ok with executor wired");

    let _ = tx_shutdown.send(());
}
