//! Two clients on one session must observe identical event streams.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::type_complexity)]

use async_trait::async_trait;
use borg_core::agent::AgentEvent as CoreEvent;
use borg_proto::session::agent_event::Kind;
use borg_proto::session::session_client::SessionClient;
use borg_proto::session::session_server::SessionServer;
use borg_proto::session::{OpenRequest, SendRequest, StreamRequest};
use borgd::daemon::bind_uds;
use borgd::grpc::session::{SessionFactory, SessionSvc};
use borgd::session::backend::SessionBackend;
use borgd::session::{SessionHost, SessionRegistry};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::UnixListenerStream;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

struct DelayedDeltas {
    id: String,
    deltas: Vec<&'static str>,
    pause: Duration,
}

#[async_trait]
impl SessionBackend for DelayedDeltas {
    async fn run_turn(
        &self,
        _text: String,
        event_tx: mpsc::Sender<CoreEvent>,
        _cancel: CancellationToken,
    ) {
        for d in &self.deltas {
            tokio::time::sleep(self.pause).await;
            let _ = event_tx.send(CoreEvent::TextDelta((*d).into())).await;
        }
        let _ = event_tx.send(CoreEvent::TurnComplete).await;
    }

    fn session_id(&self) -> &str {
        &self.id
    }
}

struct OneShotFactory(Mutex<Option<Arc<dyn SessionBackend>>>);

impl SessionFactory for OneShotFactory {
    fn open(&self, _resume: Option<&str>) -> Result<SessionHost, anyhow::Error> {
        let b = self
            .0
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| anyhow::anyhow!("factory exhausted"))?;
        Ok(SessionHost::new(b))
    }
}

async fn spawn_service(
    tmp: &std::path::Path,
    backend: Arc<dyn SessionBackend>,
) -> (PathBuf, oneshot::Sender<()>) {
    let socket = tmp.join("borgd.sock");
    let listener = bind_uds(&socket).expect("bind_uds");
    let stream = UnixListenerStream::new(listener);
    let registry = SessionRegistry::new();
    let factory = Arc::new(OneShotFactory(Mutex::new(Some(backend))));
    let svc = SessionSvc::new(registry, factory);
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(SessionServer::new(svc))
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
    (socket, tx)
}

async fn channel(socket: PathBuf) -> Channel {
    Endpoint::try_from("http://[::]:50051")
        .unwrap()
        .connect_with_connector(service_fn(move |_: Uri| {
            let p = socket.clone();
            async move {
                let s = UnixStream::connect(&p).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(s))
            }
        }))
        .await
        .unwrap()
}

#[tokio::test]
async fn two_clients_subscribed_to_same_session_see_identical_event_streams() {
    // Real failure mode: a unicast (non-broadcast) channel inside SessionHost
    // would mean only the first subscriber receives events. This test opens
    // one session, subscribes a second client via Stream(since=0), drives a
    // turn through the first client, and asserts both see the same seqs.
    let tmp = tempfile::tempdir().expect("tempdir");
    let backend = Arc::new(DelayedDeltas {
        id: "shared".into(),
        deltas: vec!["one ", "two ", "three"],
        pause: Duration::from_millis(20),
    });
    let (socket, _shutdown) = spawn_service(tmp.path(), backend).await;

    let mut client_a = SessionClient::new(channel(socket.clone()).await);
    let opened = client_a
        .open(OpenRequest {
            resume_id: String::new(),
        })
        .await
        .expect("open")
        .into_inner();

    // Subscribe a second client BEFORE the turn starts.
    let mut client_b = SessionClient::new(channel(socket).await);
    let mut stream_b = client_b
        .stream(StreamRequest {
            session_id: opened.session_id.clone(),
            since_event_seq: 0,
        })
        .await
        .expect("stream")
        .into_inner();

    // Drive the turn through client A.
    let mut stream_a = client_a
        .send(SendRequest {
            session_id: opened.session_id.clone(),
            text: "go".into(),
        })
        .await
        .expect("send")
        .into_inner();

    // Collect from both and compare.
    let collect_a = tokio::spawn(async move {
        let mut out = Vec::new();
        while let Some(item) = stream_a.next().await {
            let evt = item.expect("evt");
            let term = matches!(evt.kind, Some(Kind::TurnComplete(_)));
            out.push(evt.event_seq);
            if term {
                break;
            }
        }
        out
    });
    let collect_b = tokio::spawn(async move {
        let mut out = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_millis(800);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(100), stream_b.next()).await {
                Ok(Some(Ok(evt))) => {
                    let term = matches!(evt.kind, Some(Kind::TurnComplete(_)));
                    out.push(evt.event_seq);
                    if term {
                        break;
                    }
                }
                _ => break,
            }
        }
        out
    });
    let seqs_a = collect_a.await.expect("join a");
    let seqs_b = collect_b.await.expect("join b");
    assert!(!seqs_a.is_empty(), "client A must see events");
    assert_eq!(seqs_a, seqs_b, "both clients must see the same seq stream");
}
