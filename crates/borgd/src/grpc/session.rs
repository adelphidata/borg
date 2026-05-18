//! Session gRPC service.
//!
//! Wires the Session proto onto a [`SessionRegistry`] of live
//! [`SessionHost`]s. Construction is injection-
//! based: the daemon decides how to build a backend (real `Agent` or test
//! stub) and hands the registry + factory to this service.

use crate::session::{convert, SessionHost, SessionRegistry};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use protocols::session::{
    acp_client_message, acp_fs_request, acp_fs_response, acp_server_message,
    session_server::Session, AcpClientMessage, AcpFsCapabilities, AcpFsError, AcpFsRequest,
    AcpFsResponse, AcpReadTextFileRequest, AcpReadTextFileResponse, AcpServerMessage,
    AcpWriteTextFileRequest, AcpWriteTextFileResponse, AgentEvent, AppendAssistantMessageRequest,
    AutoSaveRequest, CancelRequest, CancelResponse, ClearRequest, CloseRequest, CloseResponse,
    CompactRequest, CompactResponse, Empty, ForkRequest, ForkResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, OpenRequest, OpenResponse,
    PromptResponse, RewindRequest, RewindResponse, SendRequest, SessionState, SessionStateRequest,
    SteerRequest, StreamRequest, UndoRequest, UndoResponse,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status as TStatus, Streaming};
use uuid::Uuid;

/// Factory: given an optional `resume_id`, build a fresh [`SessionHost`].
/// Boxed-future signature (instead of `async fn` in trait) keeps the trait
/// object-safe so the daemon can swap implementations at runtime.
pub trait SessionFactory: Send + Sync + 'static {
    /// Construct a new session host. May return an error if the underlying
    /// agent failed to initialize (bad config, missing API key, etc.).
    ///
    /// Async because constructing an `Agent` now opens the SQLite handle and
    /// hooks via the libsql async layer. Returned future borrows `self` so the
    /// implementor can stash references without cloning.
    fn open<'a>(
        &'a self,
        resume_id: Option<&'a str>,
        execution_context: Option<cortex::execution_context::ExecutionContext>,
    ) -> Pin<Box<dyn Future<Output = Result<SessionHost, anyhow::Error>> + Send + 'a>>;
}

/// Type alias for the boxed server-stream future returned by Send/Stream.
pub type EventStream = Pin<Box<dyn Stream<Item = Result<AgentEvent, TStatus>> + Send + 'static>>;
/// Type alias for the boxed ACP bidi server-stream.
pub type AcpEventStream =
    Pin<Box<dyn Stream<Item = Result<AcpServerMessage, TStatus>> + Send + 'static>>;

/// Upper bound on a single ACP fs round-trip. The bridge holds the agent's
/// async Mutex while it waits for the IDE — without a deadline a slow or
/// hung client stalls the whole session indefinitely.
const ACP_FS_CALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Read the gRPC `grpc-timeout` header off an inbound request and convert it
/// into an absolute wall-clock deadline. Returns `None` when the client did
/// not set a deadline OR the header is malformed (treat malformed as absent
/// rather than aborting the call — the watcher is a best-effort observability
/// improvement, not a security boundary). Logs at WARN when a header was
/// present but unparseable so future tickets can disambiguate "client sent
/// no deadline" from "we ignored garbage".
fn parse_grpc_deadline<T>(req: &Request<T>) -> Option<std::time::Instant> {
    let header = req.metadata().get("grpc-timeout")?;
    let raw = match header.to_str() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "grpc-timeout header is not valid UTF-8; ignoring");
            return None;
        }
    };
    match parse_grpc_timeout(raw) {
        Some(timeout) => Some(std::time::Instant::now() + timeout),
        None => {
            tracing::warn!(raw, "grpc-timeout header malformed; ignoring");
            None
        }
    }
}

/// Parse a gRPC timeout header value (`<num><unit>`, e.g. `120S`, `100m`).
/// Units per https://grpc.io/docs/guides/wire-format-protocol/: H, M, S, m,
/// u, n. Returns `None` for malformed input or for values that overflow `u64`
/// nanoseconds. Rejects non-ASCII input — `split_at` operates on byte
/// indices, so a multibyte trailing codepoint (e.g. `"120Ω"`) would otherwise
/// panic mid-codepoint on attacker-supplied metadata.
fn parse_grpc_timeout(s: &str) -> Option<std::time::Duration> {
    if !s.is_ascii() || s.len() < 2 {
        return None;
    }
    let (num_str, unit) = s.split_at(s.len() - 1);
    let n: u64 = num_str.parse().ok()?;
    let nanos = match unit {
        "H" => n.checked_mul(3_600_000_000_000)?,
        "M" => n.checked_mul(60_000_000_000)?,
        "S" => n.checked_mul(1_000_000_000)?,
        "m" => n.checked_mul(1_000_000)?,
        "u" => n.checked_mul(1_000)?,
        "n" => n,
        _ => return None,
    };
    Some(std::time::Duration::from_nanos(nanos))
}

type AcpFsResponseSender = oneshot::Sender<Result<AcpFsResponse, String>>;
type PendingAcpFsRequests = Arc<Mutex<std::collections::HashMap<String, AcpFsResponseSender>>>;

struct GrpcAcpTextFileBridge {
    fs_tx: mpsc::Sender<Result<AcpServerMessage, TStatus>>,
    pending: PendingAcpFsRequests,
    capabilities: AcpFsCapabilities,
}

impl GrpcAcpTextFileBridge {
    async fn request(&self, request: AcpFsRequest) -> Result<AcpFsResponse, String> {
        let (tx, rx) = oneshot::channel();
        let request_id = request.request_id.clone();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(request_id.clone(), tx);
        }
        if let Err(e) = self
            .fs_tx
            .send(Ok(AcpServerMessage {
                kind: Some(acp_server_message::Kind::FsRequest(request)),
            }))
            .await
        {
            let mut pending = self.pending.lock().await;
            pending.remove(&request_id);
            return Err(format!("failed to send ACP fs request to client: {e}"));
        }
        // Bound the wait so a hung IDE can't pin the agent's lock indefinitely.
        // On timeout we proactively evict our pending entry so the inbound
        // task doesn't accumulate orphaned senders for the lifetime of the
        // session (A8).
        match tokio::time::timeout(ACP_FS_CALLBACK_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&request_id);
                Err("ACP fs client disconnected before replying".to_string())
            }
            Err(_) => {
                self.pending.lock().await.remove(&request_id);
                Err(format!(
                    "ACP fs client did not reply within {}s",
                    ACP_FS_CALLBACK_TIMEOUT.as_secs()
                ))
            }
        }
    }
}

impl cortex::execution_context::TextFileBridge for GrpcAcpTextFileBridge {
    fn supports_read_text_file(&self) -> bool {
        self.capabilities.read_text_file
    }

    fn supports_write_text_file(&self) -> bool {
        self.capabilities.write_text_file
    }

    fn read_text_file(&self, path: &std::path::Path) -> anyhow::Result<String> {
        if !self.supports_read_text_file() {
            anyhow::bail!("ACP client did not advertise fs.read_text_file");
        }
        let request = AcpFsRequest {
            request_id: Uuid::new_v4().to_string(),
            kind: Some(acp_fs_request::Kind::ReadTextFile(AcpReadTextFileRequest {
                path: path.to_string_lossy().to_string(),
            })),
        };
        let response = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.request(request))
        })
        .map_err(anyhow::Error::msg)?;
        match response.kind {
            Some(acp_fs_response::Kind::ReadTextFile(AcpReadTextFileResponse { content })) => {
                Ok(content)
            }
            Some(acp_fs_response::Kind::Error(AcpFsError { message })) => anyhow::bail!(message),
            _ => anyhow::bail!("ACP fs client returned wrong response kind for read_text_file"),
        }
    }

    fn write_text_file(&self, path: &std::path::Path, content: &str) -> anyhow::Result<()> {
        if !self.supports_write_text_file() {
            anyhow::bail!("ACP client did not advertise fs.write_text_file");
        }
        let request = AcpFsRequest {
            request_id: Uuid::new_v4().to_string(),
            kind: Some(acp_fs_request::Kind::WriteTextFile(
                AcpWriteTextFileRequest {
                    path: path.to_string_lossy().to_string(),
                    content: content.to_string(),
                },
            )),
        };
        let response = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.request(request))
        })
        .map_err(anyhow::Error::msg)?;
        match response.kind {
            Some(acp_fs_response::Kind::WriteTextFile(AcpWriteTextFileResponse {})) => Ok(()),
            Some(acp_fs_response::Kind::Error(AcpFsError { message })) => anyhow::bail!(message),
            _ => anyhow::bail!("ACP fs client returned wrong response kind for write_text_file"),
        }
    }
}

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
    type AcpTurnStream = AcpEventStream;

    async fn open(&self, req: Request<OpenRequest>) -> Result<Response<OpenResponse>, TStatus> {
        let req = req.into_inner();
        let resume = (!req.resume_id.is_empty()).then_some(req.resume_id.as_str());
        let execution_context = if !req.workspace_cwd.is_empty()
            || !req.workspace_root.is_empty()
            || req.enforce_workspace_root
        {
            let workspace_cwd = std::path::PathBuf::from(req.workspace_cwd);
            let workspace_root = if req.workspace_root.is_empty() {
                workspace_cwd.clone()
            } else {
                std::path::PathBuf::from(req.workspace_root)
            };
            Some(
                cortex::execution_context::ExecutionContext::for_workspace(
                    workspace_cwd,
                    workspace_root,
                )
                .map_err(|e| {
                    TStatus::invalid_argument(format!("invalid workspace context: {e}"))
                })?,
            )
        } else {
            None
        };

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
            .open(resume, execution_context)
            .await
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
        // Capture the gRPC client's wall-clock deadline (if any) BEFORE consuming
        // the request. The deadline arrives in the `grpc-timeout` header
        // (https://grpc.io/docs/guides/wire-format-protocol/) which we parse
        // off `req.metadata()`.
        let deadline = parse_grpc_deadline(&req);
        let req = req.into_inner();
        let host = self.host_or_err(&req.session_id)?;

        // Subscribe BEFORE starting the turn so we can't miss the first event.
        // `start_turn` returns the baseline seq sampled atomically *after*
        // the prior forwarder finished draining — see `SessionHost::start_turn`
        // for why that matters. We then forward only events with seq strictly
        // above that baseline, which is what `Send` semantically promises
        // ("events for THIS turn").
        let mut rx = host.subscribe();
        let images = req
            .images
            .into_iter()
            .map(|i| crate::session::backend::InlineImage {
                mime_type: i.mime_type,
                data: i.data,
                filename: (!i.filename.is_empty()).then_some(i.filename),
            })
            .collect();
        let (_cancel, baseline_seq) = host.start_turn(req.text, images, None, deadline).await;

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

    async fn acp_turn(
        &self,
        req: Request<Streaming<AcpClientMessage>>,
    ) -> Result<Response<Self::AcpTurnStream>, TStatus> {
        let mut inbound = req.into_inner();
        let first = inbound
            .next()
            .await
            .ok_or_else(|| TStatus::invalid_argument("client closed before AcpStartTurn"))?
            .map_err(|e| TStatus::internal(format!("acp turn inbound error: {e}")))?;
        let start = match first.kind {
            Some(acp_client_message::Kind::StartTurn(start)) => start,
            _ => {
                return Err(TStatus::invalid_argument(
                    "first ACP turn message must be start_turn",
                ))
            }
        };

        let host = self.host_or_err(&start.session_id)?;
        let (tx, out_rx) = mpsc::channel::<Result<AcpServerMessage, TStatus>>(64);
        let pending: PendingAcpFsRequests = Arc::new(Mutex::new(std::collections::HashMap::new()));

        let bridge = if start
            .fs_capabilities
            .as_ref()
            .is_some_and(|caps| caps.read_text_file || caps.write_text_file)
        {
            Some(Arc::new(GrpcAcpTextFileBridge {
                fs_tx: tx.clone(),
                pending: pending.clone(),
                capabilities: start.fs_capabilities.unwrap_or(AcpFsCapabilities {
                    read_text_file: false,
                    write_text_file: false,
                }),
            })
                as Arc<dyn cortex::execution_context::TextFileBridge>)
        } else {
            None
        };

        let mut rx = host.subscribe();
        let images = start
            .images
            .into_iter()
            .map(|i| crate::session::backend::InlineImage {
                mime_type: i.mime_type,
                data: i.data,
                filename: (!i.filename.is_empty()).then_some(i.filename),
            })
            .collect();
        // ACP turns ride a streaming inbound and don't carry a single
        // request-level `grpc-timeout` header the way unary `Send` does — the
        // ACP client batches its own per-message timing. Pass `None` for the
        // turn deadline; ACP cancellation flows through `acp_client_message`
        // explicit cancel frames instead.
        let (_cancel, baseline_seq) = host.start_turn(start.text, images, bridge, None).await;

        let host_for_inbound = host.clone();
        let pending_for_inbound = pending.clone();
        tokio::spawn(async move {
            while let Some(msg) = inbound.next().await {
                match msg {
                    Ok(AcpClientMessage {
                        kind: Some(acp_client_message::Kind::FsResponse(response)),
                    }) => {
                        let tx = {
                            let mut pending = pending_for_inbound.lock().await;
                            pending.remove(&response.request_id)
                        };
                        if let Some(tx) = tx {
                            let _ = tx.send(Ok(response));
                        } else {
                            tracing::warn!(request_id = %response.request_id, "unknown ACP fs response");
                        }
                    }
                    Ok(AcpClientMessage {
                        kind: Some(acp_client_message::Kind::Cancel(cancel)),
                    }) => {
                        if cancel.session_id == host_for_inbound.id() {
                            host_for_inbound.cancel_turn().await;
                        }
                    }
                    Ok(AcpClientMessage {
                        kind: Some(acp_client_message::Kind::StartTurn(_)),
                    }) => {
                        tracing::warn!("received duplicate ACP start_turn on live stream");
                    }
                    Ok(AcpClientMessage { kind: None }) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "ACP inbound stream failed");
                        host_for_inbound.cancel_turn().await;
                        break;
                    }
                }
            }
            let pending = {
                let mut pending = pending_for_inbound.lock().await;
                std::mem::take(&mut *pending)
            };
            for (_id, tx) in pending {
                let _ = tx.send(Err("ACP fs stream closed before client replied".to_string()));
            }
        });

        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(evt) => {
                        if evt.event_seq <= baseline_seq {
                            continue;
                        }
                        let terminal = convert::is_terminal(&evt);
                        if tx
                            .send(Ok(AcpServerMessage {
                                kind: Some(acp_server_message::Kind::AgentEvent(evt)),
                            }))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        if terminal {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "session.acp_turn subscriber lagged");
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
        let Some(host) = self.registry.remove_host(&id) else {
            return Err(TStatus::not_found(format!("session `{id}` not found")));
        };
        host.close().await;
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

    async fn get_state(
        &self,
        req: Request<SessionStateRequest>,
    ) -> Result<Response<SessionState>, TStatus> {
        let req = req.into_inner();
        let host = self.host_or_err(&req.session_id)?;
        let snap = host
            .snapshot_state()
            .map_err(|e| TStatus::internal(format!("snapshot_state: {e}")))?;
        Ok(Response::new(SessionState {
            session_id: req.session_id,
            agent_session_id: snap.agent_session_id,
            title: snap.title,
            message_count: snap.message_count,
            user_message_count: snap.user_message_count,
            token_estimate: snap.token_estimate,
            llm_provider: snap.llm_provider,
            llm_model: snap.llm_model,
            collab_mode: snap.collab_mode,
            turn_in_flight: false,
        }))
    }

    async fn compact(
        &self,
        req: Request<CompactRequest>,
    ) -> Result<Response<CompactResponse>, TStatus> {
        let host = self.host_or_err(&req.into_inner().session_id)?;
        let (dropped, after) = host
            .compact()
            .await
            .map_err(|e| TStatus::internal(format!("compact: {e}")))?;
        Ok(Response::new(CompactResponse {
            dropped: dropped as u64,
            after_tokens: after as u64,
        }))
    }

    async fn clear_history(&self, req: Request<ClearRequest>) -> Result<Response<Empty>, TStatus> {
        let host = self.host_or_err(&req.into_inner().session_id)?;
        host.clear_history()
            .map_err(|e| TStatus::internal(format!("clear_history: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    async fn undo(&self, req: Request<UndoRequest>) -> Result<Response<UndoResponse>, TStatus> {
        let host = self.host_or_err(&req.into_inner().session_id)?;
        let removed = host
            .undo()
            .map_err(|e| TStatus::internal(format!("undo: {e}")))?;
        Ok(Response::new(UndoResponse {
            removed: removed as u64,
        }))
    }

    async fn rewind_to_nth_user_message(
        &self,
        req: Request<RewindRequest>,
    ) -> Result<Response<RewindResponse>, TStatus> {
        let req = req.into_inner();
        let host = self.host_or_err(&req.session_id)?;
        let removed = host
            .rewind_to_nth_user_message(req.n as usize)
            .map_err(|e| TStatus::internal(format!("rewind: {e}")))?;
        Ok(Response::new(RewindResponse {
            removed: removed as u64,
        }))
    }

    async fn auto_save(&self, req: Request<AutoSaveRequest>) -> Result<Response<Empty>, TStatus> {
        let host = self.host_or_err(&req.into_inner().session_id)?;
        host.auto_save()
            .map_err(|e| TStatus::internal(format!("auto_save: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    async fn new_session(
        &self,
        req: Request<NewSessionRequest>,
    ) -> Result<Response<NewSessionResponse>, TStatus> {
        let host = self.host_or_err(&req.into_inner().session_id)?;
        let id = host
            .new_agent_session()
            .await
            .map_err(|e| TStatus::internal(format!("new_session: {e}")))?;
        Ok(Response::new(NewSessionResponse {
            agent_session_id: id,
        }))
    }

    async fn load_session(
        &self,
        req: Request<LoadSessionRequest>,
    ) -> Result<Response<LoadSessionResponse>, TStatus> {
        let req = req.into_inner();
        let host = self.host_or_err(&req.session_id)?;
        let count = host
            .load_agent_session(&req.agent_session_id)
            .map_err(|e| TStatus::not_found(format!("load_session: {e}")))?;
        Ok(Response::new(LoadSessionResponse {
            agent_session_id: req.agent_session_id,
            message_count: count,
        }))
    }

    async fn steer(&self, req: Request<SteerRequest>) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        let host = self.host_or_err(&req.session_id)?;
        host.steer(&req.text)
            .map_err(|e| TStatus::failed_precondition(format!("steer: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    async fn append_assistant_message(
        &self,
        req: Request<AppendAssistantMessageRequest>,
    ) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        if req.content.trim().is_empty() {
            return Err(TStatus::invalid_argument("content must not be empty"));
        }
        // Heartbeats top out around a few hundred bytes; anything past 64KB
        // is either a runaway sender or something that should have been
        // chunked. Cheap upper bound — keeps a buggy caller from filling
        // a session's history with a single mirror call.
        const MAX_APPEND_BYTES: usize = 64 * 1024;
        if req.content.len() > MAX_APPEND_BYTES {
            return Err(TStatus::invalid_argument(format!(
                "content is {} bytes; cap is {MAX_APPEND_BYTES}",
                req.content.len()
            )));
        }
        let host = self.host_or_err(&req.session_id)?;
        host.append_assistant_message(&req.content)
            .await
            .map_err(|e| TStatus::internal(format!("append_assistant_message: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    async fn fork(&self, req: Request<ForkRequest>) -> Result<Response<ForkResponse>, TStatus> {
        let host = self.host_or_err(&req.into_inner().session_id)?;
        let outcome = host.fork().await.map_err(|e| {
            // "session has no messages" is a precondition failure, not internal.
            let msg = e.to_string();
            if msg.contains("no messages") {
                TStatus::failed_precondition(msg)
            } else {
                TStatus::internal(format!("fork: {msg}"))
            }
        })?;
        Ok(Response::new(ForkResponse {
            forked_agent_session_id: outcome.forked_agent_session_id,
            title: outcome.title,
            message_count: outcome.message_count,
        }))
    }
}

#[cfg(test)]
mod parse_grpc_timeout_tests {
    use super::parse_grpc_timeout;
    use std::time::Duration;

    #[test]
    fn parses_each_unit_per_grpc_wire_format() {
        // Real failure mode: a unit-table typo (e.g. swapping "S" and "m")
        // would silently produce a deadline 1000x off — the watcher would
        // either fire instantly on every Send or never fire at all. Cover
        // each unit explicitly rather than testing a few representative ones.
        let cases: &[(&str, Duration)] = &[
            ("1H", Duration::from_secs(3600)),
            ("2M", Duration::from_secs(120)),
            ("120S", Duration::from_secs(120)),
            ("100m", Duration::from_millis(100)),
            ("500u", Duration::from_micros(500)),
            ("750n", Duration::from_nanos(750)),
        ];
        for (raw, expected) in cases {
            assert_eq!(
                parse_grpc_timeout(raw),
                Some(*expected),
                "parse_grpc_timeout({raw:?}) mismatch"
            );
        }
    }

    #[test]
    fn rejects_malformed_or_unknown_unit() {
        // Treat malformed as absent (None) rather than aborting — see
        // parse_grpc_deadline doc. A regression that returned a default
        // duration here would silently apply a wrong deadline.
        for raw in &["", "S", "120", "120X", "abcS", "1.5S", "-1S"] {
            assert_eq!(
                parse_grpc_timeout(raw),
                None,
                "parse_grpc_timeout({raw:?}) should be None"
            );
        }
    }

    #[test]
    fn overflow_returns_none() {
        // u64 nanoseconds caps around ~584 years; an attacker-supplied huge
        // hour count must not panic or wrap.
        assert_eq!(parse_grpc_timeout("99999999H"), None);
    }

    #[test]
    fn non_ascii_input_does_not_panic_on_codepoint_boundary() {
        // Real failure mode: `split_at(s.len() - 1)` works on byte indices.
        // A multibyte trailing codepoint (e.g. `Ω` is 2 UTF-8 bytes) would
        // panic mid-codepoint without an `is_ascii()` guard. Untrusted gRPC
        // metadata can contain arbitrary UTF-8 — a per-request panic is a
        // remotely triggerable DoS surface.
        for raw in &["120Ω", "Ω120", "S\u{1F600}", "120\u{1F600}", "🚀"] {
            assert_eq!(
                parse_grpc_timeout(raw),
                None,
                "parse_grpc_timeout({raw:?}) must reject non-ASCII without panicking"
            );
        }
    }
}
