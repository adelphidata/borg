//! Session gRPC service.
//!
//! Wires the Session proto onto a [`SessionRegistry`] of live
//! [`SessionHost`](crate::session::SessionHost)s. Construction is injection-
//! based: the daemon decides how to build a backend (real `Agent` or test
//! stub) and hands the registry + factory to this service.

use crate::session::{convert, SessionHost, SessionRegistry};
use async_trait::async_trait;
use borg_proto::session::{
    session_server::Session, AgentEvent, CancelRequest, CancelResponse, CloseRequest,
    CloseResponse, Empty, OpenRequest, OpenResponse, PromptResponse, SendRequest, StreamRequest,
};
use futures_util::Stream;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status as TStatus};

/// Factory: given an optional `resume_id`, build a fresh [`SessionHost`].
/// Boxed-future signature (instead of `async fn` in trait) keeps the trait
/// object-safe so the daemon can swap implementations at runtime.
pub trait SessionFactory: Send + Sync + 'static {
    /// Construct a new session host. May return an error if the underlying
    /// agent failed to initialize (bad config, missing API key, etc.).
    fn open(&self, resume_id: Option<&str>) -> Result<SessionHost, anyhow::Error>;
}

/// Type alias for the boxed server-stream future returned by Send/Stream.
pub type EventStream = Pin<Box<dyn Stream<Item = Result<AgentEvent, TStatus>> + Send + 'static>>;

/// Session service implementation.
pub struct SessionSvc {
    registry: SessionRegistry,
    factory: Arc<dyn SessionFactory>,
}

impl SessionSvc {
    /// Construct a new Session service against `registry` using `factory` for
    /// new session creation.
    pub fn new(registry: SessionRegistry, factory: Arc<dyn SessionFactory>) -> Self {
        Self { registry, factory }
    }

    #[allow(clippy::result_large_err)]
    fn host_or_err(&self, id: &str) -> Result<SessionHost, TStatus> {
        self.registry
            .get(id)
            .ok_or_else(|| TStatus::not_found(format!("session `{id}` not found")))
    }
}

#[async_trait]
impl Session for SessionSvc {
    type SendStream = EventStream;
    type StreamStream = EventStream;

    async fn open(&self, req: Request<OpenRequest>) -> Result<Response<OpenResponse>, TStatus> {
        let req = req.into_inner();
        let resume = (!req.resume_id.is_empty()).then_some(req.resume_id.as_str());

        // If resuming and the host is still live, just hand back its cursor.
        if let Some(id) = resume {
            if let Some(host) = self.registry.get(id) {
                return Ok(Response::new(OpenResponse {
                    session_id: host.id().to_string(),
                    last_event_seq: host.last_event_seq(),
                }));
            }
        }

        let host = self
            .factory
            .open(resume)
            .map_err(|e| TStatus::internal(format!("session open failed: {e}")))?;
        let session_id = host.id().to_string();
        let last_event_seq = host.last_event_seq();
        self.registry.insert(host);
        Ok(Response::new(OpenResponse {
            session_id,
            last_event_seq,
        }))
    }

    async fn send(&self, req: Request<SendRequest>) -> Result<Response<Self::SendStream>, TStatus> {
        let req = req.into_inner();
        let host = self.host_or_err(&req.session_id)?;

        // Subscribe BEFORE starting the turn so we can't miss the first event.
        // We also remember the highest seq already buffered; the per-turn
        // stream only forwards events strictly above that mark, which is
        // what `Send` semantically promises ("events for THIS turn").
        let mut rx = host.subscribe();
        let baseline_seq = host.last_event_seq();
        host.start_turn(req.text).await;

        let (tx, out_rx) = mpsc::channel::<Result<AgentEvent, TStatus>>(64);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(evt) => {
                        if evt.event_seq <= baseline_seq {
                            continue;
                        }
                        let terminal = convert::is_terminal(&evt);
                        if tx.send(Ok(evt)).await.is_err() {
                            break;
                        }
                        if terminal {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        // Should be rare given 256-deep broadcast — log and
                        // continue. Lossy delivery here is preferable to
                        // bringing down the stream.
                        tracing::warn!(skipped = n, "session.send subscriber lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(out_rx))))
    }

    async fn stream(
        &self,
        req: Request<StreamRequest>,
    ) -> Result<Response<Self::StreamStream>, TStatus> {
        let req = req.into_inner();
        let host = self.host_or_err(&req.session_id)?;
        let since = req.since_event_seq;

        // Subscribe FIRST so events emitted between the replay snapshot and
        // the subscribe call aren't lost — we de-dupe on the consumer side.
        let mut rx = host.subscribe();
        let backlog = host.replay_since(since);
        let last_replayed = backlog.last().map(|e| e.event_seq).unwrap_or(since);

        let (tx, out_rx) = mpsc::channel::<Result<AgentEvent, TStatus>>(64);
        tokio::spawn(async move {
            for evt in backlog {
                if tx.send(Ok(evt)).await.is_err() {
                    return;
                }
            }
            let mut high_water = last_replayed;
            loop {
                match rx.recv().await {
                    Ok(evt) => {
                        if evt.event_seq <= high_water {
                            continue;
                        }
                        high_water = evt.event_seq;
                        if tx.send(Ok(evt)).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "session.stream subscriber lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(out_rx))))
    }

    async fn cancel(
        &self,
        req: Request<CancelRequest>,
    ) -> Result<Response<CancelResponse>, TStatus> {
        let host = self.host_or_err(&req.into_inner().session_id)?;
        host.cancel_turn().await;
        Ok(Response::new(CancelResponse {}))
    }

    async fn close(&self, req: Request<CloseRequest>) -> Result<Response<CloseResponse>, TStatus> {
        let id = req.into_inner().session_id;
        if !self.registry.remove(&id) {
            return Err(TStatus::not_found(format!("session `{id}` not found")));
        }
        Ok(Response::new(CloseResponse {}))
    }

    async fn respond_to_prompt(
        &self,
        req: Request<PromptResponse>,
    ) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        let host = self.host_or_err(&req.session_id)?;
        host.respond_to_prompt(&req.prompt_id, &req.value)
            .map_err(|e| TStatus::failed_precondition(e.to_string()))?;
        Ok(Response::new(Empty {}))
    }
}
