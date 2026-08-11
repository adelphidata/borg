//! Integration tests for `Admin.BrowserManage` — the gRPC backing both
//! the TUI `/browser` slash command and the `borg browser` CLI.
//!
//! These run hermetically against a borgd UDS bound on a tempdir; no live
//! Chrome is required. The "connect to a real CDP" path is exercised by
//! standing up a minimal HTTP listener that answers `/json/version` —
//! that's all `is_cdp_ready` needs to flip `connected = true`.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::await_holding_lock,
    clippy::redundant_closure_for_method_calls
)]

use borgd::daemon::bind_uds;
use borgd::grpc::admin::AdminSvc;
use borgd::session::SessionRegistry;
use cortex::db::Database;
use protocols::admin::admin_client::AdminClient;
use protocols::admin::admin_server::AdminServer;
use protocols::admin::BrowserManageRequest;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

static SERIAL: Mutex<()> = Mutex::new(());

struct Harness {
    _tmp: tempfile::TempDir,
    socket: PathBuf,
    _shutdown: oneshot::Sender<()>,
}

async fn start_harness() -> Harness {
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: BORG_HOME is read every `Database::open()`. We serialize
    // tests within this binary through SERIAL so they don't see each other's
    // tempdirs. cargo runs separate `--test <name>` binaries in distinct
    // processes, so cross-binary env leakage is not a concern.
    unsafe {
        std::env::set_var("BORG_HOME", tmp.path());
    }
    let _ = Database::open().await.expect("seed DB open");

    let socket = tmp.path().join("borgd.sock");
    let listener = bind_uds(&socket).expect("bind_uds");
    let stream = UnixListenerStream::new(listener);
    let registry = SessionRegistry::new();
    let admin = AdminSvc::new(registry);

    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(AdminServer::new(admin))
            .serve_with_incoming_shutdown(stream, async {
                let _ = rx.await;
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
    Harness {
        _tmp: tmp,
        socket,
        _shutdown: tx,
    }
}

async fn channel(socket: &Path) -> Channel {
    let p = socket.to_path_buf();
    Endpoint::try_from(protocols::TONIC_UDS_PLACEHOLDER_URL)
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

/// Tiny synchronous HTTP responder that answers a single `/json/version`
/// request. Mirrors the contract `cortex::browser::cdp_attach::is_cdp_ready`
/// probes. Returns (port, JoinHandle).
fn spawn_fake_cdp() -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake cdp");
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        // Serve up to 5 requests then exit — the daemon's status probe may
        // hit `/json/version` once per status call, and disconnect re-probes.
        let mut served = 0;
        while served < 5 {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let body = br#"{"Browser":"Chrome/121.0.0.0","Protocol-Version":"1.3"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.write_all(body);
            served += 1;
        }
    });
    (port, handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_manage_status_connect_disconnect_lifecycle() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let h = start_harness().await;
    let mut client = AdminClient::new(channel(&h.socket).await);

    // 1. status before any setting → not connected.
    let resp = client
        .browser_manage(BrowserManageRequest {
            action: "status".into(),
            url: String::new(),
        })
        .await
        .expect("status rpc")
        .into_inner();
    assert!(
        !resp.connected,
        "fresh daemon should report disconnected; got connected with url={:?}",
        resp.url
    );
    assert!(
        resp.url.is_empty(),
        "fresh daemon should not echo a URL; got {:?}",
        resp.url
    );

    // 2. connect to an unreachable port → setting remains unset.
    // Bind+drop an ephemeral port to guarantee nothing listens there.
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = dead.local_addr().unwrap().port();
    drop(dead);

    let resp = client
        .browser_manage(BrowserManageRequest {
            action: "connect".into(),
            url: format!("http://127.0.0.1:{dead_port}"),
        })
        .await
        .expect("connect rpc (unreachable)")
        .into_inner();
    assert!(
        !resp.connected,
        "connect to dead port should not mark connected; messages={:?}",
        resp.messages
    );
    assert!(
        resp.messages.iter().any(|m| m.contains("not reachable")),
        "expected 'not reachable' diagnostic; got messages={:?}",
        resp.messages
    );

    // Verify the setting was NOT persisted by the failed attempt.
    let resp = client
        .browser_manage(BrowserManageRequest {
            action: "status".into(),
            url: String::new(),
        })
        .await
        .expect("status after failed connect")
        .into_inner();
    assert!(
        !resp.connected && resp.url.is_empty(),
        "failed connect must not persist a URL; got connected={} url={:?}",
        resp.connected,
        resp.url
    );

    // 3. connect against a fake CDP that answers /json/version.
    let (good_port, fake_handle) = spawn_fake_cdp();
    let good_url = format!("http://127.0.0.1:{good_port}");

    let resp = client
        .browser_manage(BrowserManageRequest {
            action: "connect".into(),
            url: good_url.clone(),
        })
        .await
        .expect("connect rpc (live)")
        .into_inner();
    assert!(
        resp.connected,
        "connect to fake CDP should succeed; messages={:?}",
        resp.messages
    );
    assert_eq!(
        resp.url, good_url,
        "response should echo the normalized URL"
    );

    // 4. disconnect → setting cleared, subsequent status reflects it.
    let resp = client
        .browser_manage(BrowserManageRequest {
            action: "disconnect".into(),
            url: String::new(),
        })
        .await
        .expect("disconnect rpc")
        .into_inner();
    assert!(!resp.connected);
    assert!(
        resp.url.is_empty(),
        "post-disconnect URL must be empty; got {:?}",
        resp.url
    );
    assert!(
        resp.messages.iter().any(|m| m.contains("disconnected")),
        "expected 'disconnected' message; got {:?}",
        resp.messages
    );

    let resp = client
        .browser_manage(BrowserManageRequest {
            action: "status".into(),
            url: String::new(),
        })
        .await
        .expect("status after disconnect")
        .into_inner();
    assert!(!resp.connected);
    assert!(resp.url.is_empty());

    // Best-effort: drain the fake server thread.
    drop(fake_handle);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_manage_rejects_bad_action_and_scheme() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let h = start_harness().await;
    let mut client = AdminClient::new(channel(&h.socket).await);

    // Bad action → invalid_argument.
    let err = client
        .browser_manage(BrowserManageRequest {
            action: "go-go-gadget".into(),
            url: String::new(),
        })
        .await
        .expect_err("expected invalid_argument for unknown action");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains("unknown browser action"));

    // Bad URL scheme on connect → response surfaces an error message, setting unchanged.
    let resp = client
        .browser_manage(BrowserManageRequest {
            action: "connect".into(),
            url: "ftp://localhost:9222".into(),
        })
        .await
        .expect("connect with bad scheme should not fail the RPC")
        .into_inner();
    assert!(!resp.connected);
    assert!(
        resp.messages
            .iter()
            .any(|m| m.contains("Unsupported browser url scheme")),
        "expected scheme-rejection message; got {:?}",
        resp.messages
    );
}
