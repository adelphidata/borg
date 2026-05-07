use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex as StdMutex,
};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use base64::Engine;
use cortex::agent::AgentEvent;
use cortex::internal_tag_filter::strip_internal_tags;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::daemon_client::{connect_or_spawn, AcpFsClient, DaemonClient};

pub(crate) async fn run_acp_stdio() -> Result<()> {
    init_acp_logging()?;
    install_acp_panic_hook();
    let daemon = connect_or_spawn()
        .await
        .context("connect to borgd for ACP")?;
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    let exit_code = run_transport(AcpDaemonClient::new(daemon), stdin, stdout).await?;
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

/// Holds the non-blocking writer guard for the lifetime of the process so
/// the background flush thread doesn't drop log lines on shutdown. A
/// `static OnceLock` is preferred over `Box::leak` because we can detect
/// double-init (which can happen if a cortex code path lazy-installed a
/// subscriber before us) and degrade gracefully instead of panicking.
static ACP_LOG_GUARD: std::sync::OnceLock<tracing_appender::non_blocking::WorkerGuard> =
    std::sync::OnceLock::new();

fn init_acp_logging() -> Result<()> {
    let log_dir = cortex::config::Config::logs_dir().context("resolve logs dir")?;
    std::fs::create_dir_all(&log_dir).context("create logs dir")?;
    let log_path = log_dir.join("acp.log");
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(&log_path).context("open ACP log file")?;
    let (non_blocking, guard) = tracing_appender::non_blocking(file);
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn,hyper=error"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_ansi(false)
        .with_writer(non_blocking);
    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer);
    if let Err(set_err) = registry.try_init() {
        // A subscriber was already installed (e.g. by a cortex code path that
        // ran before us). Don't panic — write the conflict to acp.log
        // directly and let the existing subscriber handle subsequent events.
        let mut direct = opts.open(&log_path).context("reopen ACP log on conflict")?;
        let _ = std::io::Write::write_all(
            &mut direct,
            format!("[acp] tracing subscriber already initialized; falling back: {set_err}\n")
                .as_bytes(),
        );
        // Drop the unused guard explicitly: we couldn't attach the writer.
        drop(guard);
        return Ok(());
    }
    let _ = ACP_LOG_GUARD.set(guard);
    Ok(())
}

fn install_acp_panic_hook() {
    std::panic::set_hook(Box::new(|panic| {
        tracing::error!("acp panic: {panic}");
    }));
}

#[async_trait]
trait AcpDaemon: Clone + Send + Sync + 'static {
    async fn onboarding_complete(&self) -> Result<bool>;
    async fn open_session(
        &self,
        resume_id: Option<&str>,
        workspace_cwd: Option<&Path>,
        workspace_root: Option<&Path>,
        enforce_workspace_root: bool,
    ) -> Result<(String, u64)>;
    async fn send_prompt<F: AcpFsClient>(
        &self,
        session_id: &str,
        prompt: PromptInput,
        fs_client: F,
    ) -> Result<mpsc::Receiver<AgentEvent>>;
    async fn cancel(&self, session_id: &str) -> Result<()>;
    async fn state(&self, session_id: &str) -> Result<SessionMetadata>;
    async fn replay_history(&self, session_id: &str) -> Result<Vec<HistoryMessage>>;
}

#[derive(Clone)]
struct AcpDaemonClient {
    client: DaemonClient,
}

impl AcpDaemonClient {
    fn new(client: DaemonClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl AcpDaemon for AcpDaemonClient {
    async fn onboarding_complete(&self) -> Result<bool> {
        let mut client = self.client.clone();
        Ok(client.get_onboarding_state().await?.onboarded)
    }

    async fn open_session(
        &self,
        resume_id: Option<&str>,
        workspace_cwd: Option<&Path>,
        workspace_root: Option<&Path>,
        enforce_workspace_root: bool,
    ) -> Result<(String, u64)> {
        let mut client = self.client.clone();
        client
            .open_session_with_workspace(
                resume_id,
                workspace_cwd,
                workspace_root,
                enforce_workspace_root,
            )
            .await
    }

    async fn send_prompt<F: AcpFsClient>(
        &self,
        session_id: &str,
        prompt: PromptInput,
        fs_client: F,
    ) -> Result<mpsc::Receiver<AgentEvent>> {
        let mut client = self.client.clone();
        let images = prompt
            .images
            .into_iter()
            .map(|image| protocols::session::InlineImage {
                mime_type: image.mime_type,
                data: image.data,
                filename: image.filename.unwrap_or_default(),
            })
            .collect();
        client
            .send_acp_prompt(session_id, &prompt.text, images, fs_client)
            .await
    }

    async fn cancel(&self, session_id: &str) -> Result<()> {
        let mut client = self.client.clone();
        client.cancel(session_id).await
    }

    async fn state(&self, session_id: &str) -> Result<SessionMetadata> {
        let mut client = self.client.clone();
        let state = client.get_state(session_id).await?;
        Ok(SessionMetadata {
            session_id: state.agent_session_id,
            title: state.title,
            model: state.llm_model,
            workspace_cwd: None,
            workspace_root: None,
        })
    }

    async fn replay_history(&self, session_id: &str) -> Result<Vec<HistoryMessage>> {
        let mut client = self.client.clone();
        let detail = client.get_session(session_id).await?;
        Ok(detail
            .messages
            .into_iter()
            .map(|m| HistoryMessage {
                role: m.role,
                text: m.text,
            })
            .collect())
    }
}

#[derive(Clone, Debug, Default)]
struct SessionRuntime {
    prompt_in_flight: bool,
    cancel_requested: bool,
}

#[derive(Clone, Debug)]
struct SessionRecord {
    workspace_cwd: PathBuf,
    // Today ACP only exposes a single `cwd`, so root == cwd at session creation.
    // Kept as a distinct field for forward-compat with a future ACP root capability
    // (e.g. multi-root workspaces) where cwd may live under a broader root.
    workspace_root: PathBuf,
    runtime: SessionRuntime,
}

#[derive(Clone)]
struct AcpState<D: AcpDaemon> {
    daemon: D,
    sessions: Arc<Mutex<HashMap<String, SessionRecord>>>,
    client_capabilities: Arc<StdMutex<ClientCapabilities>>,
    /// Latch that flips to `true` after the first `onboarding_complete()`
    /// roundtrip returns `Ok(true)`. Lets later requests skip the gRPC
    /// probe — onboarding is monotonic for the lifetime of `borg acp`.
    onboarding_confirmed: Arc<std::sync::atomic::AtomicBool>,
    /// Set when a turn observes the daemon event channel closing without
    /// emitting TurnComplete/Error. Causes `run_acp_stdio` to exit non-zero
    /// after the in-flight error frame is flushed, so the IDE sees the
    /// agent as failed (per plan §"Daemon failures").
    daemon_failed: Arc<std::sync::atomic::AtomicBool>,
    /// Wakes the inbound loop so it stops reading stdin after a daemon
    /// disconnect — the error frame for the in-flight request is already
    /// sent; further requests would only produce more failures.
    shutdown: Arc<tokio::sync::Notify>,
}

/// How a `session/prompt` event-stream loop terminated.
enum StreamTermination {
    /// Saw `AgentEvent::TurnComplete`.
    Complete,
    /// Saw `AgentEvent::Error("cancelled")`.
    Cancelled,
    /// Saw `AgentEvent::Error(other)` — a daemon-reported failure mid-turn.
    Failed(String),
    /// Channel closed before any terminal event arrived. Signals daemon
    /// disconnect unless the client had requested cancel.
    Disconnected,
}

type SharedWriter = Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>;

#[derive(Clone, Debug, Default)]
struct ClientCapabilities {
    read_text_file: bool,
    write_text_file: bool,
}

type JsonRpcResponseSender = oneshot::Sender<Result<Value, String>>;
type PendingJsonRpcRequests = Arc<Mutex<HashMap<String, JsonRpcResponseSender>>>;

#[derive(Clone)]
struct JsonRpcCallbackClient {
    writer: SharedWriter,
    pending: PendingJsonRpcRequests,
    next_id: Arc<AtomicU64>,
    capabilities: Arc<StdMutex<ClientCapabilities>>,
}

impl JsonRpcCallbackClient {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id_num = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = Value::Number(serde_json::Number::from(id_num));
        let key = rpc_id_key(&id);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(key, tx);
        }
        write_json(
            &self.writer,
            &serde_json::to_value(JsonRpcOutboundRequest {
                jsonrpc: "2.0",
                id,
                method: method.to_string(),
                params,
            })?,
        )
        .await?;
        rx.await
            .map_err(|_| anyhow!("ACP callback response channel closed"))?
            .map_err(anyhow::Error::msg)
    }
}

#[async_trait]
impl AcpFsClient for JsonRpcCallbackClient {
    async fn read_text_file(&self, path: &str) -> Result<String> {
        let result = self
            .request("fs/read_text_file", json!({ "path": path }))
            .await?;
        let content = result
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("ACP fs/read_text_file missing string content"))?;
        Ok(content.to_string())
    }

    async fn write_text_file(&self, path: &str, content: &str) -> Result<()> {
        let _ = self
            .request(
                "fs/write_text_file",
                json!({ "path": path, "content": content }),
            )
            .await?;
        Ok(())
    }

    fn capabilities(&self) -> protocols::session::AcpFsCapabilities {
        let caps = self
            .capabilities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        protocols::session::AcpFsCapabilities {
            read_text_file: caps.read_text_file,
            write_text_file: caps.write_text_file,
        }
    }
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcResponseEnvelope {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<IncomingJsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct IncomingJsonRpcError {
    message: String,
}

#[derive(Debug, Serialize)]
struct JsonRpcOutboundRequest {
    jsonrpc: &'static str,
    id: Value,
    method: String,
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcErrorResponse<'a> {
    jsonrpc: &'static str,
    id: Value,
    error: JsonRpcError<'a>,
}

#[derive(Debug, Serialize)]
struct JsonRpcSuccessResponse {
    jsonrpc: &'static str,
    id: Value,
    result: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcNotification<'a> {
    jsonrpc: &'static str,
    method: &'a str,
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcError<'a> {
    code: i64,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct InitializeParams {
    #[serde(default)]
    #[serde(rename = "clientCapabilities")]
    client_capabilities: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct SessionNewParams {
    cwd: String,
    #[serde(default)]
    #[serde(rename = "mcpServers")]
    mcp_servers: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct SessionLoadParams {
    #[serde(rename = "sessionId")]
    session_id: String,
    cwd: String,
}

#[derive(Debug, Deserialize)]
struct SessionPromptParams {
    #[serde(rename = "sessionId")]
    session_id: String,
    prompt: Vec<PromptBlock>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PromptBlock {
    Text {
        text: String,
    },
    Image {
        #[serde(rename = "mimeType")]
        mime_type: String,
        data: String,
        #[serde(default)]
        filename: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct SessionCancelParams {
    #[serde(rename = "sessionId")]
    session_id: String,
}

#[derive(Debug, Clone)]
struct PromptInput {
    text: String,
    images: Vec<PromptImage>,
}

#[derive(Debug, Clone)]
struct PromptImage {
    mime_type: String,
    data: Vec<u8>,
    filename: Option<String>,
}

#[derive(Debug, Clone)]
struct SessionMetadata {
    session_id: String,
    title: String,
    model: String,
    workspace_cwd: Option<String>,
    workspace_root: Option<String>,
}

#[derive(Debug, Clone)]
struct HistoryMessage {
    role: String,
    text: String,
}

enum InboundTransportMessage {
    Request(JsonRpcRequest),
    Response(JsonRpcResponseEnvelope),
    ParseError(String),
}

/// Structured error returned by `handle_request`.
///
/// Carries a JSON-RPC code/message plus optional `data` so the dispatcher can
/// translate any handler failure into a spec-compliant error frame the IDE can
/// route on (instead of logging it and leaving the client to time out).
#[derive(Debug, Clone)]
struct AcpRequestError {
    code: i64,
    message: &'static str,
    data: Option<Value>,
}

impl AcpRequestError {
    fn invalid_params(detail: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: "Invalid params",
            data: Some(Value::String(detail.into())),
        }
    }

    fn onboarding_required() -> Self {
        Self {
            code: -32001,
            message: "Onboarding required",
            data: Some(json!({
                "kind": "onboarding_required",
                "detail": "Borg has not completed onboarding; run `borg` once interactively to finish setup."
            })),
        }
    }

    fn unknown_session(session_id: &str) -> Self {
        Self {
            code: -32602,
            message: "Invalid params",
            data: Some(json!({
                "kind": "unknown_session",
                "sessionId": session_id,
            })),
        }
    }

    fn daemon_disconnected() -> Self {
        Self {
            code: -32603,
            message: "Internal error",
            data: Some(json!({
                "kind": "daemon_disconnected",
                "detail": "borgd disconnected before the turn completed."
            })),
        }
    }

    fn internal(detail: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: "Internal error",
            data: Some(Value::String(detail.into())),
        }
    }

    fn from_anyhow(err: &anyhow::Error) -> Self {
        if let Some(known) = err.downcast_ref::<Self>() {
            known.clone()
        } else {
            Self::internal(format!("{err:#}"))
        }
    }
}

impl std::fmt::Display for AcpRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.data {
            Some(Value::String(s)) => write!(f, "{}: {s}", self.message),
            Some(other) => write!(f, "{}: {other}", self.message),
            None => f.write_str(self.message),
        }
    }
}

impl std::error::Error for AcpRequestError {}

async fn run_transport<D, R, W>(daemon: D, reader: R, writer: W) -> Result<i32>
where
    D: AcpDaemon,
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let writer: SharedWriter = Arc::new(Mutex::new(Box::new(writer)));
    let pending = Arc::new(Mutex::new(HashMap::<
        String,
        oneshot::Sender<Result<Value, String>>,
    >::new()));
    let callback_client = JsonRpcCallbackClient {
        writer: writer.clone(),
        pending: pending.clone(),
        next_id: Arc::new(AtomicU64::new(1)),
        capabilities: Arc::new(StdMutex::new(ClientCapabilities::default())),
    };
    let state = AcpState {
        daemon,
        sessions: Arc::new(Mutex::new(HashMap::new())),
        client_capabilities: callback_client.capabilities.clone(),
        onboarding_confirmed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        daemon_failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        shutdown: Arc::new(tokio::sync::Notify::new()),
    };
    let daemon_failed = state.daemon_failed.clone();
    let shutdown = state.shutdown.clone();
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundTransportMessage>(64);
    tokio::spawn(async move {
        let mut lines = reader.lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let value: Value = match serde_json::from_str(&line) {
                        Ok(value) => value,
                        Err(e) => {
                            if inbound_tx
                                .send(InboundTransportMessage::ParseError(e.to_string()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                            continue;
                        }
                    };
                    if value.get("method").is_some() {
                        match serde_json::from_value::<JsonRpcRequest>(value) {
                            Ok(req) => {
                                if inbound_tx
                                    .send(InboundTransportMessage::Request(req))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(e) => {
                                if inbound_tx
                                    .send(InboundTransportMessage::ParseError(e.to_string()))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    } else if inbound_tx
                        .send(
                            serde_json::from_value::<JsonRpcResponseEnvelope>(value)
                                .map(InboundTransportMessage::Response)
                                .unwrap_or_else(|e| {
                                    InboundTransportMessage::ParseError(e.to_string())
                                }),
                        )
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = inbound_tx
                        .send(InboundTransportMessage::ParseError(e.to_string()))
                        .await;
                    break;
                }
            }
        }
    });
    let mut request_tasks = tokio::task::JoinSet::new();
    /// Soft cap on concurrently-in-flight ACP requests so a misbehaving
    /// client can't pile up handlers and exhaust memory. Plenty for any
    /// real IDE workload — ACP turns are sequential per session.
    const MAX_INFLIGHT_REQUESTS: usize = 64;

    loop {
        let msg = tokio::select! {
            msg = inbound_rx.recv() => match msg {
                Some(msg) => msg,
                None => break,
            },
            _ = shutdown.notified() => break,
        };
        // Reap any completed task slots before deciding whether the
        // dispatcher is at capacity. Surface join errors so a panicked
        // handler doesn't get swallowed silently.
        while let Some(result) = request_tasks.try_join_next() {
            if let Err(e) = result {
                tracing::error!("ACP request task ended with error: {e}");
            }
        }
        match msg {
            InboundTransportMessage::Request(req) => {
                if req.jsonrpc != "2.0" {
                    send_error(
                        &writer,
                        req.id.unwrap_or(Value::Null),
                        -32600,
                        "Invalid Request",
                        Some(json!("jsonrpc must be 2.0")),
                    )
                    .await?;
                    continue;
                }
                if request_tasks.len() >= MAX_INFLIGHT_REQUESTS {
                    if let Some(id) = req.id.clone() {
                        send_error(
                            &writer,
                            id,
                            -32000,
                            "Server busy",
                            Some(json!({
                                "kind": "too_many_inflight",
                                "max": MAX_INFLIGHT_REQUESTS,
                            })),
                        )
                        .await?;
                    } else {
                        tracing::warn!("dropped ACP notification {} — at inflight cap", req.method);
                    }
                    continue;
                }
                let writer = writer.clone();
                let state = state.clone();
                let callback_client = callback_client.clone();
                let req_id = req.id.clone();
                let shutdown_signal = state.shutdown.clone();
                let daemon_failed_flag = state.daemon_failed.clone();
                request_tasks.spawn(async move {
                    if let Err(e) =
                        handle_request(state, req, writer.clone(), callback_client).await
                    {
                        let mapped = AcpRequestError::from_anyhow(&e);
                        match req_id {
                            Some(id) => {
                                if let Err(send_err) = send_kind_error(&writer, id, &mapped).await {
                                    tracing::error!("acp send_error failed: {send_err:#}");
                                }
                            }
                            None => {
                                // Notification — JSON-RPC says don't reply,
                                // even on error. Log and drop.
                                tracing::warn!("acp notification handler errored: {e:#}");
                            }
                        }
                        // After the error frame flushes, tear the inbound
                        // loop down so the IDE sees process exit non-zero
                        // instead of a hung agent.
                        if daemon_failed_flag.load(Ordering::SeqCst) {
                            shutdown_signal.notify_one();
                        }
                    }
                });
            }
            InboundTransportMessage::Response(response) => {
                if let Some(id) = response.id {
                    let key = rpc_id_key(&id);
                    let tx = {
                        let mut pending = pending.lock().await;
                        pending.remove(&key)
                    };
                    if let Some(tx) = tx {
                        let _ = tx.send(match (response.result, response.error) {
                            (Some(result), None) => Ok(result),
                            (_, Some(error)) => Err(error.message),
                            _ => Err("invalid ACP callback response envelope".to_string()),
                        });
                    }
                }
            }
            InboundTransportMessage::ParseError(e) => {
                send_error(
                    &writer,
                    Value::Null,
                    -32700,
                    "Parse error",
                    Some(json!(e.to_string())),
                )
                .await?;
            }
        }
    }

    while let Some(result) = request_tasks.join_next().await {
        if let Err(e) = result {
            tracing::error!("ACP request task join failed: {e}");
        }
    }

    Ok(if daemon_failed.load(Ordering::SeqCst) {
        1
    } else {
        0
    })
}

async fn handle_request<D: AcpDaemon>(
    state: AcpState<D>,
    req: JsonRpcRequest,
    writer: SharedWriter,
    callback_client: JsonRpcCallbackClient,
) -> Result<()> {
    match req.method.as_str() {
        "initialize" => {
            let params: InitializeParams = parse_params(req.params)?;
            {
                let mut caps = state
                    .client_capabilities
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *caps = parse_client_capabilities(params.client_capabilities.as_ref());
            }
            send_result(&writer, req.id, initialize_response()).await
        }
        "authenticate" => send_result(&writer, req.id, json!({"authenticated": true})).await,
        "session/new" => {
            require_onboarding(&state).await?;
            let params: SessionNewParams = parse_params(req.params)?;
            let cwd = validate_workspace_cwd(&params.cwd)?;
            // `mcpServers` is structurally accepted but not honored —
            // MCP-over-ACP bridging is explicitly out of scope for v1
            // (plan §Non-Goals).
            _ = params.mcp_servers;
            let (session_id, _) = state
                .daemon
                .open_session(None, Some(&cwd), Some(&cwd), true)
                .await
                .map_err(|e| anyhow!(AcpRequestError::internal(format!("{e:#}"))))?;
            {
                let mut sessions = state.sessions.lock().await;
                sessions.insert(
                    session_id.clone(),
                    SessionRecord {
                        workspace_cwd: cwd.clone(),
                        workspace_root: cwd.clone(),
                        runtime: SessionRuntime::default(),
                    },
                );
            }
            let mut info = state
                .daemon
                .state(&session_id)
                .await
                .map_err(|e| anyhow!(AcpRequestError::internal(format!("{e:#}"))))?;
            if let Some(record) = state.sessions.lock().await.get(&session_id).cloned() {
                info.workspace_cwd = Some(record.workspace_cwd.display().to_string());
                info.workspace_root = Some(record.workspace_root.display().to_string());
            }
            send_notification(&writer, "session_info_update", session_info_params(&info)).await?;
            send_result(&writer, req.id, json!({"sessionId": session_id})).await
        }
        "session/load" => {
            require_onboarding(&state).await?;
            let params: SessionLoadParams = parse_params(req.params)?;
            let cwd = validate_workspace_cwd(&params.cwd)?;
            let (session_id, _) = state
                .daemon
                .open_session(Some(&params.session_id), Some(&cwd), Some(&cwd), true)
                .await
                .map_err(|e| anyhow!(AcpRequestError::internal(format!("{e:#}"))))?;
            {
                let mut sessions = state.sessions.lock().await;
                sessions.insert(
                    session_id.clone(),
                    SessionRecord {
                        workspace_cwd: cwd.clone(),
                        workspace_root: cwd.clone(),
                        runtime: SessionRuntime::default(),
                    },
                );
            }
            let history = state
                .daemon
                .replay_history(&session_id)
                .await
                .map_err(|e| anyhow!(AcpRequestError::internal(format!("{e:#}"))))?;
            for message in history {
                if let Some(update) = history_replay_update(&session_id, &message) {
                    send_notification(&writer, "session/update", update).await?;
                }
            }
            let mut info = state
                .daemon
                .state(&session_id)
                .await
                .map_err(|e| anyhow!(AcpRequestError::internal(format!("{e:#}"))))?;
            if let Some(record) = state.sessions.lock().await.get(&session_id).cloned() {
                info.workspace_cwd = Some(record.workspace_cwd.display().to_string());
                info.workspace_root = Some(record.workspace_root.display().to_string());
            }
            send_notification(&writer, "session_info_update", session_info_params(&info)).await?;
            send_result(&writer, req.id, json!({"sessionId": info.session_id})).await
        }
        "session/prompt" => {
            require_onboarding(&state).await?;
            let params: SessionPromptParams = parse_params(req.params)?;
            // Mention/path expansion is deferred to the IDE in ACP sessions
            // (n6). Zed, Neovim's ACP client, and similar editors implement
            // their own `@mention` resolution before sending the prompt;
            // calling `cortex::mentions::expand_mentions` here would
            // (a) double-expand for clients that already resolved their
            // mentions and (b) read from the daemon's process cwd via
            // `std::env::current_dir()` rather than the session's
            // workspace_cwd. If a future ACP version standardizes a server-
            // resolved mention block, route that through the workspace
            // context instead — do NOT re-introduce process-cwd expansion.
            let prompt = parse_prompt_blocks(&params.prompt)?;
            {
                let mut sessions = state.sessions.lock().await;
                let session = sessions
                    .get_mut(&params.session_id)
                    .ok_or_else(|| anyhow!(AcpRequestError::unknown_session(&params.session_id)))?;
                session.runtime.prompt_in_flight = true;
                session.runtime.cancel_requested = false;
            }
            let mut events = state
                .daemon
                .send_prompt(&params.session_id, prompt, callback_client.clone())
                .await
                .map_err(|e| anyhow!(AcpRequestError::internal(format!("{e:#}"))))?;
            let mut stop_reason = "end_turn";
            let mut termination = StreamTermination::Disconnected;
            while let Some(event) = events.recv().await {
                match &event {
                    AgentEvent::TurnComplete => {
                        termination = StreamTermination::Complete;
                        break;
                    }
                    AgentEvent::Error(message) if message == "cancelled" => {
                        termination = StreamTermination::Cancelled;
                        break;
                    }
                    AgentEvent::Error(message) => {
                        termination = StreamTermination::Failed(message.clone());
                        break;
                    }
                    _ => {
                        if let Some(update) = event_to_update(&event, &params.session_id) {
                            send_notification(&writer, "session/update", update).await?;
                        }
                    }
                }
            }
            let cancel_observed = {
                let mut sessions = state.sessions.lock().await;
                let mut observed = false;
                if let Some(session) = sessions.get_mut(&params.session_id) {
                    session.runtime.prompt_in_flight = false;
                    observed = session.runtime.cancel_requested;
                }
                observed
            };
            match termination {
                StreamTermination::Complete => {}
                StreamTermination::Cancelled => stop_reason = "cancelled",
                StreamTermination::Failed(message) => {
                    return Err(anyhow!(AcpRequestError::internal(message)));
                }
                StreamTermination::Disconnected => {
                    if cancel_observed {
                        // Cancellation race: client cancelled while the
                        // daemon was already winding the turn down. Surface
                        // as cancelled rather than disconnect.
                        stop_reason = "cancelled";
                    } else {
                        state.daemon_failed.store(true, Ordering::SeqCst);
                        return Err(anyhow!(AcpRequestError::daemon_disconnected()));
                    }
                }
            }
            if cancel_observed {
                stop_reason = "cancelled";
            }
            send_result(&writer, req.id, json!({"stopReason": stop_reason})).await
        }
        "session/cancel" => {
            let params: SessionCancelParams = parse_params(req.params)?;
            let known = {
                let mut sessions = state.sessions.lock().await;
                if let Some(session) = sessions.get_mut(&params.session_id) {
                    // Mark cancel_requested unconditionally — a cancel
                    // arriving in the narrow window between `send_prompt`
                    // returning and the dispatcher checking the flag would
                    // otherwise be lost. The flag is reset when the next
                    // prompt sets `prompt_in_flight = true`, so a stray
                    // pre-cancel can't bleed into a later turn.
                    session.runtime.cancel_requested = true;
                    true
                } else {
                    false
                }
            };
            // Unknown session id is a no-op per ACP spec semantics — the IDE
            // may issue cancel after the session was forgotten.
            if known {
                if let Err(e) = state.daemon.cancel(&params.session_id).await {
                    tracing::warn!(
                        session = %params.session_id,
                        error = %e,
                        "daemon cancel failed; treating as no-op",
                    );
                }
            }
            Ok(())
        }
        _ => {
            if req.id.is_some() {
                send_error(
                    &writer,
                    req.id.unwrap_or(Value::Null),
                    -32601,
                    "Method not found",
                    None,
                )
                .await?;
            }
            Ok(())
        }
    }
}

async fn require_onboarding<D: AcpDaemon>(state: &AcpState<D>) -> Result<()> {
    // Onboarding state is monotonic: once `borg` has been onboarded it stays
    // onboarded for the lifetime of this process. Cache the first success
    // so we skip the gRPC roundtrip on every subsequent session/* call.
    if state.onboarding_confirmed.load(Ordering::SeqCst) {
        return Ok(());
    }
    match state.daemon.onboarding_complete().await {
        Ok(true) => {
            state.onboarding_confirmed.store(true, Ordering::SeqCst);
            Ok(())
        }
        Ok(false) => Err(anyhow!(AcpRequestError::onboarding_required())),
        Err(e) => Err(anyhow!(AcpRequestError::internal(format!(
            "onboarding probe failed: {e:#}"
        )))),
    }
}

fn validate_workspace_cwd(cwd: &str) -> Result<PathBuf> {
    let path = PathBuf::from(cwd);
    if !path.is_absolute() {
        return Err(anyhow!(AcpRequestError::invalid_params(
            "cwd must be an absolute path",
        )));
    }
    path.canonicalize().map_err(|e| {
        anyhow!(AcpRequestError::invalid_params(format!(
            "cannot resolve cwd {}: {e}",
            path.display()
        )))
    })
}

/// Convert ACP prompt blocks into the daemon's `(text, images)` shape.
///
/// **Interleaving is not preserved.** Text fragments are concatenated in
/// order with `\n` separators; images are pushed onto a separate list and
/// reach the daemon as multimodal attachments after the text. This matches
/// plan §"Prompt Content Handling" ("text blocks compose the user message
/// text"). If a future ACP client relies on tight image-text interleaving,
/// extend `AcpStartTurn` with an ordered `repeated PromptBlock` field and
/// thread that through `Message::user_multimodal`.
fn parse_prompt_blocks(blocks: &[PromptBlock]) -> Result<PromptInput> {
    let mut text_parts = Vec::new();
    let mut images = Vec::new();
    for block in blocks {
        match block {
            PromptBlock::Text { text } => text_parts.push(text.clone()),
            PromptBlock::Image {
                mime_type,
                data,
                filename,
            } => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|e| {
                        anyhow!(AcpRequestError::invalid_params(format!(
                            "decode image block {mime_type}: {e}"
                        )))
                    })?;
                images.push(PromptImage {
                    mime_type: mime_type.clone(),
                    data: bytes,
                    filename: filename.clone(),
                });
            }
        }
    }
    Ok(PromptInput {
        text: text_parts.join("\n"),
        images,
    })
}

fn parse_params<T: for<'de> Deserialize<'de>>(params: Option<Value>) -> Result<T> {
    serde_json::from_value(params.unwrap_or(Value::Null)).map_err(|e| {
        anyhow!(AcpRequestError::invalid_params(format!(
            "invalid params: {e}"
        )))
    })
}

/// Extract the client-advertised `fs.{readTextFile,writeTextFile}` flags from
/// the `clientCapabilities` field of `initialize`. Per ACP spec the flags
/// MUST be nested under `fs`; bare top-level booleans are ignored so a
/// malformed client doesn't get accidentally credited with capabilities it
/// didn't actually claim.
fn parse_client_capabilities(value: Option<&Value>) -> ClientCapabilities {
    let fs = value.and_then(|value| value.get("fs"));
    if let Some(value) = value {
        if fs.is_none() && value.is_object() {
            // The IDE sent clientCapabilities but no nested `fs` object.
            // Per spec we treat this as "no fs capabilities", which means
            // the daemon falls back to disk reads/writes. Surface in the
            // log so an integrator who expected unsaved-buffer support
            // can see why it isn't firing.
            tracing::debug!(
                payload = %value,
                "ACP clientCapabilities present but no `fs` object — falling back to disk for read/write"
            );
        }
    }
    ClientCapabilities {
        read_text_file: fs
            .and_then(|fs| fs.get("readTextFile"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        write_text_file: fs
            .and_then(|fs| fs.get("writeTextFile"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// Build the spec-shaped `initialize` response.
///
/// Per ACP §"Initialization", the agent advertises `agentCapabilities`
/// (loadSession + promptCapabilities + mcpCapabilities). It does NOT
/// re-advertise `fs.*`, which is the **client's** capability surface —
/// `parse_client_capabilities` reads those from the request and stores
/// them on `AcpState` for later use during turns.
fn initialize_response() -> Value {
    json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "loadSession": true,
            "promptCapabilities": {
                "image": true,
                "audio": false,
                "embeddedContext": false,
            },
            "mcpCapabilities": {
                "http": false,
                "sse": false,
            },
        },
        "authMethods": [],
    })
}

fn rpc_id_key(id: &Value) -> String {
    match id {
        Value::String(s) => format!("s:{s}"),
        Value::Number(n) => format!("n:{n}"),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

async fn send_result(writer: &SharedWriter, id: Option<Value>, result: Value) -> Result<()> {
    let response = JsonRpcSuccessResponse {
        jsonrpc: "2.0",
        id: id.unwrap_or(Value::Null),
        result,
    };
    write_json(writer, &serde_json::to_value(response)?).await
}

async fn send_error(
    writer: &SharedWriter,
    id: Value,
    code: i64,
    message: &'static str,
    data: Option<Value>,
) -> Result<()> {
    let response = JsonRpcErrorResponse {
        jsonrpc: "2.0",
        id,
        error: JsonRpcError {
            code,
            message,
            data,
        },
    };
    write_json(writer, &serde_json::to_value(response)?).await
}

async fn send_kind_error(writer: &SharedWriter, id: Value, err: &AcpRequestError) -> Result<()> {
    send_error(writer, id, err.code, err.message, err.data.clone()).await
}

async fn send_notification(writer: &SharedWriter, method: &str, params: Value) -> Result<()> {
    let notification = JsonRpcNotification {
        jsonrpc: "2.0",
        method,
        params,
    };
    write_json(writer, &serde_json::to_value(notification)?).await
}

async fn write_json(writer: &SharedWriter, value: &Value) -> Result<()> {
    let line = serde_json::to_vec(value)?;
    let mut writer = writer.lock().await;
    writer.write_all(&line).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

fn session_info_params(info: &SessionMetadata) -> Value {
    json!({
        "sessionId": info.session_id,
        "title": info.title,
        "model": info.model,
        "workspaceCwd": info.workspace_cwd,
        "workspaceRoot": info.workspace_root,
    })
}

/// Map a Borg tool name to the ACP `tool_call` category. ACP clients (Zed,
/// etc.) render tool calls based on this `kind` field; without it they fall
/// back to a generic icon and label. Plan §"Tool lifecycle" specifies the
/// `run_shell → execute`, fs → `other` baseline; we extend it to the full
/// ACP `kind` enum (`read`, `edit`, `search`, `fetch`, `execute`, `other`)
/// so that read-only inspection, mutation, and network access render
/// distinctly. Unknown tools degrade to `"other"` rather than erroring.
fn tool_kind_for(name: &str) -> &'static str {
    match name {
        "run_shell" => "execute",
        "read_file" | "read_memory" | "list_dir" | "list" => "read",
        "apply_patch" | "write_memory" => "edit",
        "memory_search" => "search",
        "web_fetch" | "web_search" | "browser" => "fetch",
        _ => "other",
    }
}

fn event_to_update(event: &AgentEvent, session_id: &str) -> Option<Value> {
    match event {
        AgentEvent::TextDelta(text) => Some(json!({
            "sessionId": session_id,
            "update": {
                "kind": "agent_message_chunk",
                "text": strip_internal_tags(text),
            }
        })),
        AgentEvent::ToolExecuting {
            tool_call_id,
            name,
            args,
        } => Some(json!({
            "sessionId": session_id,
            "update": {
                "kind": "tool_call",
                "toolCallId": tool_call_id,
                "name": name,
                "toolKind": tool_kind_for(name),
                "status": "running",
                "arguments": args,
            }
        })),
        AgentEvent::ToolOutputDelta {
            tool_call_id,
            name,
            delta,
            is_stderr,
        } => Some(json!({
            "sessionId": session_id,
            "update": {
                "kind": "tool_call_update",
                "toolCallId": tool_call_id,
                "name": name,
                "toolKind": tool_kind_for(name),
                "status": "running",
                "stream": if *is_stderr { "stderr" } else { "stdout" },
                "content": strip_internal_tags(delta),
            }
        })),
        AgentEvent::ToolResult {
            tool_call_id,
            name,
            result,
        } => Some(json!({
            "sessionId": session_id,
            "update": {
                "kind": "tool_call_update",
                "toolCallId": tool_call_id,
                "name": name,
                "toolKind": tool_kind_for(name),
                "status": "completed",
                "content": strip_internal_tags(result),
            }
        })),
        AgentEvent::PlanUpdated { steps } => Some(json!({
            "sessionId": session_id,
            "update": {
                "kind": "plan",
                "entries": steps.iter().enumerate().map(|(idx, step)| {
                    json!({
                        "id": idx.to_string(),
                        "title": step.title,
                        "status": plan_step_status_to_acp(&step.status),
                        "priority": "medium"
                    })
                }).collect::<Vec<_>>()
            }
        })),
        _ => None,
    }
}

/// Translate a persisted `HistoryMessage` into the ACP `session/update`
/// frame the IDE expects on `session/load`.
///
/// Per ACP spec the replay update kinds are `user_message_chunk` (role
/// "user") and `agent_message_chunk` (role "assistant"); other roles
/// (e.g. internal "system" or "tool") are dropped because there's no
/// spec-blessed kind for them. Tool-call replay is a known follow-up:
/// the daemon's `MessageView` proto only carries `{role, text}` today,
/// so persisted tool calls and their results aren't surfaced to the
/// translator. Plan §"Phase 7 Session load/replay" calls for replaying
/// tool calls/statuses; that requires extending `MessageView` (and the
/// underlying DB read in `borgd/src/grpc/admin.rs`) to carry structured
/// tool-event entries — out of scope for v1.
fn history_replay_update(session_id: &str, message: &HistoryMessage) -> Option<Value> {
    let kind = match message.role.as_str() {
        "user" => "user_message_chunk",
        "assistant" => "agent_message_chunk",
        _ => return None,
    };
    Some(json!({
        "sessionId": session_id,
        "update": {
            "kind": kind,
            "text": strip_internal_tags(&message.text),
        }
    }))
}

/// Map Borg's `PlanStepStatus` to ACP's plan-entry status string. Explicit
/// `match` (not `Debug`-stringification) so a future variant addition forces
/// a compile-error decision rather than silently emitting a non-spec value.
fn plan_step_status_to_acp(status: &cortex::types::PlanStepStatus) -> &'static str {
    use cortex::types::PlanStepStatus;
    match status {
        PlanStepStatus::Pending => "pending",
        PlanStepStatus::InProgress => "in_progress",
        PlanStepStatus::Completed => "completed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex::types::{PlanStep, PlanStepStatus};
    use tokio::io::AsyncWrite;
    use tokio::io::BufReader;

    #[derive(Clone, Default)]
    struct CaptureWriter {
        inner: Arc<StdMutex<Vec<u8>>>,
    }

    impl CaptureWriter {
        fn new() -> (Self, Arc<StdMutex<Vec<u8>>>) {
            let inner = Arc::new(StdMutex::new(Vec::new()));
            (
                Self {
                    inner: inner.clone(),
                },
                inner,
            )
        }
    }

    impl AsyncWrite for CaptureWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(buf);
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[derive(Clone)]
    struct FakeDaemon {
        onboarded: bool,
        open_session_id: String,
        history: Vec<HistoryMessage>,
        metadata: SessionMetadata,
        prompts: Arc<Mutex<Vec<PromptInput>>>,
        events: Arc<Mutex<HashMap<String, Vec<AgentEvent>>>>,
        cancels: Arc<Mutex<Vec<String>>>,
        /// When set, send_prompt streams these events first then awaits a
        /// `cancel()` call before emitting a final `TurnComplete`. Lets a
        /// test exercise the cancel flag wiring without stuffing the events
        /// list with a pre-baked `Error("cancelled")`.
        cancel_gate: Option<Arc<tokio::sync::Notify>>,
    }

    #[async_trait]
    impl AcpDaemon for FakeDaemon {
        async fn onboarding_complete(&self) -> Result<bool> {
            Ok(self.onboarded)
        }

        async fn open_session(
            &self,
            resume_id: Option<&str>,
            _workspace_cwd: Option<&Path>,
            _workspace_root: Option<&Path>,
            _enforce_workspace_root: bool,
        ) -> Result<(String, u64)> {
            Ok((resume_id.unwrap_or(&self.open_session_id).to_string(), 0))
        }

        async fn send_prompt<F: AcpFsClient>(
            &self,
            session_id: &str,
            prompt: PromptInput,
            _fs_client: F,
        ) -> Result<mpsc::Receiver<AgentEvent>> {
            self.prompts.lock().await.push(prompt);
            let (tx, rx) = mpsc::channel(16);
            let events = self
                .events
                .lock()
                .await
                .remove(session_id)
                .unwrap_or_default();
            if let Some(gate) = self.cancel_gate.clone() {
                tokio::spawn(async move {
                    for event in events {
                        let _ = tx.send(event).await;
                    }
                    gate.notified().await;
                    let _ = tx.send(AgentEvent::TurnComplete).await;
                });
            } else {
                for event in events {
                    tx.send(event).await.expect("send fake event");
                }
            }
            Ok(rx)
        }

        async fn cancel(&self, session_id: &str) -> Result<()> {
            self.cancels.lock().await.push(session_id.to_string());
            if let Some(gate) = self.cancel_gate.as_ref() {
                // notify_one stores a permit so the gate fires even if the
                // spawned send_prompt task hasn't reached `notified().await`
                // yet — `notify_waiters` drops the wake under that race.
                gate.notify_one();
            }
            Ok(())
        }

        async fn state(&self, _session_id: &str) -> Result<SessionMetadata> {
            Ok(self.metadata.clone())
        }

        async fn replay_history(&self, _session_id: &str) -> Result<Vec<HistoryMessage>> {
            Ok(self.history.clone())
        }
    }

    fn fake_daemon() -> FakeDaemon {
        FakeDaemon {
            onboarded: true,
            open_session_id: "sess-1".into(),
            history: Vec::new(),
            metadata: SessionMetadata {
                session_id: "sess-1".into(),
                title: "ACP Session".into(),
                model: "test-model".into(),
                workspace_cwd: None,
                workspace_root: None,
            },
            prompts: Arc::new(Mutex::new(Vec::new())),
            events: Arc::new(Mutex::new(HashMap::new())),
            cancels: Arc::new(Mutex::new(Vec::new())),
            cancel_gate: None,
        }
    }

    #[tokio::test]
    async fn initialize_returns_capabilities() {
        let input = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
"#;
        let (writer, output) = CaptureWriter::new();
        let code = run_transport(fake_daemon(), BufReader::new(&input[..]), writer)
            .await
            .expect("transport");
        assert_eq!(code, 0);
        let lines: Vec<Value> = String::from_utf8(output.lock().unwrap().clone())
            .expect("utf8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("json"))
            .collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["result"]["authMethods"], json!([]));
    }

    #[tokio::test]
    async fn malformed_json_yields_parse_error() {
        let input = b"{not json}\n";
        let (writer, output) = CaptureWriter::new();
        run_transport(fake_daemon(), BufReader::new(&input[..]), writer)
            .await
            .expect("transport");
        let bytes = output.lock().unwrap().clone();
        let line: Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).expect("json");
        assert_eq!(line["error"]["code"], -32700);
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let input = br#"{"jsonrpc":"2.0","id":9,"method":"bogus","params":{}}
"#;
        let (writer, output) = CaptureWriter::new();
        run_transport(fake_daemon(), BufReader::new(&input[..]), writer)
            .await
            .expect("transport");
        let bytes = output.lock().unwrap().clone();
        let line: Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).expect("json");
        assert_eq!(line["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn session_load_replays_history_with_spec_message_kinds() {
        // B3 regression: persisted history must replay as ACP-spec
        // `user_message_chunk` / `agent_message_chunk` updates — never the
        // legacy non-spec `history_message` kind. Internal tags must be
        // stripped on the way out (translator-only — upstream callers
        // still see raw text).
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_string_lossy();
        let mut daemon = fake_daemon();
        daemon.history = vec![
            HistoryMessage {
                role: "user".into(),
                text: "hi".into(),
            },
            HistoryMessage {
                role: "assistant".into(),
                text: "hello <internal>scratch</internal> world".into(),
            },
            HistoryMessage {
                role: "system".into(),
                text: "ignored".into(),
            },
        ];
        let input = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"sess-1\",\"cwd\":\"{}\"}}}}\n",
            cwd
        );
        let (writer, output) = CaptureWriter::new();
        run_transport(
            daemon,
            BufReader::new(std::io::Cursor::new(input.into_bytes())),
            writer,
        )
        .await
        .expect("transport");
        let lines = parse_lines(output.lock().unwrap().clone());
        let updates: Vec<&Value> = lines
            .iter()
            .filter(|l| l["method"] == "session/update")
            .collect();
        assert_eq!(updates.len(), 2, "system role must be dropped: {lines:?}");
        assert_eq!(updates[0]["params"]["update"]["kind"], "user_message_chunk");
        assert_eq!(updates[0]["params"]["update"]["text"], "hi");
        assert_eq!(
            updates[1]["params"]["update"]["kind"],
            "agent_message_chunk"
        );
        assert_eq!(
            updates[1]["params"]["update"]["text"], "hello  world",
            "internal tags must be stripped on replay",
        );
        assert!(
            !lines
                .iter()
                .any(|l| l["params"]["update"]["kind"] == "history_message"),
            "history_message is non-spec and must not appear",
        );
        let info = lines
            .iter()
            .find(|l| l["method"] == "session_info_update")
            .expect("session_info_update missing");
        assert_eq!(info["params"]["sessionId"], "sess-1");
        let result = lines
            .iter()
            .find(|l| l["id"] == json!(1))
            .expect("response for id=1");
        assert_eq!(result["result"]["sessionId"], "sess-1");
    }

    #[tokio::test]
    async fn prompt_translates_text_tool_plan_and_completion() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_string_lossy();
        let daemon = fake_daemon();
        daemon.events.lock().await.insert(
            "sess-1".into(),
            vec![
                AgentEvent::TextDelta("ok <internal>hide</internal> visible".into()),
                AgentEvent::ToolExecuting {
                    tool_call_id: "call_1".into(),
                    name: "read_file".into(),
                    args: "{\"path\":\"/tmp/x\"}".into(),
                },
                AgentEvent::ToolOutputDelta {
                    tool_call_id: "call_1".into(),
                    name: "read_file".into(),
                    delta: "partial".into(),
                    is_stderr: false,
                },
                AgentEvent::ToolResult {
                    tool_call_id: "call_1".into(),
                    name: "read_file".into(),
                    result: "done".into(),
                },
                AgentEvent::PlanUpdated {
                    steps: vec![PlanStep {
                        title: "Ship ACP".into(),
                        status: PlanStepStatus::InProgress,
                    }],
                },
                AgentEvent::TurnComplete,
            ],
        );
        let input = format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/new\",\"params\":{{\"cwd\":\"{}\"}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"sess-1\",\"prompt\":[{{\"type\":\"text\",\"text\":\"hello\"}}]}}}}\n"
            ),
            cwd
        );
        let (writer, output) = CaptureWriter::new();
        run_transport(
            daemon,
            BufReader::new(std::io::Cursor::new(input.into_bytes())),
            writer,
        )
        .await
        .expect("transport");
        let lines: Vec<Value> = String::from_utf8(output.lock().unwrap().clone())
            .expect("utf8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("json"))
            .collect();
        assert_eq!(lines[2]["method"], "session/update");
        assert_eq!(lines[2]["params"]["update"]["kind"], "agent_message_chunk");
        assert_eq!(lines[2]["params"]["update"]["text"], "ok  visible");
        assert_eq!(lines[3]["params"]["update"]["toolCallId"], "call_1");
        // Plan §"Tool lifecycle": tool updates must carry an ACP toolKind so
        // clients can render category-specific affordances. `read_file`
        // categorises as `read`.
        assert_eq!(lines[3]["params"]["update"]["toolKind"], "read");
        assert_eq!(lines[4]["params"]["update"]["toolKind"], "read");
        assert_eq!(lines[5]["params"]["update"]["toolKind"], "read");
        assert_eq!(
            lines.last().expect("final line")["result"]["stopReason"],
            "end_turn"
        );
    }

    /// Plan §"Tool lifecycle" — ACP toolKind mapping. Drives the
    /// IDE-side rendering of tool calls. A new tool added to the catalog
    /// will fall through to `other`, which is the safe default; promoting
    /// it (e.g. a future `git` tool → `execute`) is an explicit edit.
    #[test]
    fn tool_kind_for_maps_categories() {
        let cases: &[(&str, &str)] = &[
            ("run_shell", "execute"),
            ("read_file", "read"),
            ("list_dir", "read"),
            ("read_memory", "read"),
            ("apply_patch", "edit"),
            ("write_memory", "edit"),
            ("memory_search", "search"),
            ("web_fetch", "fetch"),
            ("web_search", "fetch"),
            ("browser", "fetch"),
            ("text_to_speech", "other"),
            ("a_tool_that_does_not_exist", "other"),
        ];
        for (name, expected) in cases {
            assert_eq!(
                tool_kind_for(name),
                *expected,
                "tool_kind_for({name}) should be {expected}"
            );
        }
    }

    /// Plan §"Logging and Stdio Safety" + §"Text streaming" — `<internal>`
    /// stripping is a translator-only concern. Other event consumers (TUI,
    /// daemon log) keep raw text. Regression guard: if upstream cortex
    /// starts stripping, this still passes; if the ACP translator stops
    /// stripping on `ToolOutputDelta`, this fails.
    #[test]
    fn tool_output_delta_strips_internal_tags_on_wire() {
        let event = AgentEvent::ToolOutputDelta {
            tool_call_id: "c1".into(),
            name: "run_shell".into(),
            delta: "before <internal>secret-plan</internal> after".into(),
            is_stderr: false,
        };
        let update = event_to_update(&event, "sess-1").expect("update");
        assert_eq!(update["update"]["content"], "before  after");
        assert_eq!(update["update"]["toolKind"], "execute");
        assert_eq!(update["update"]["stream"], "stdout");

        let stderr_event = AgentEvent::ToolOutputDelta {
            tool_call_id: "c1".into(),
            name: "run_shell".into(),
            delta: "<internal>x</internal>err".into(),
            is_stderr: true,
        };
        let stderr_update = event_to_update(&stderr_event, "sess-1").expect("update");
        assert_eq!(stderr_update["update"]["content"], "err");
        assert_eq!(stderr_update["update"]["stream"], "stderr");
    }

    #[tokio::test]
    async fn cancel_notification_marks_stop_reason_cancelled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_string_lossy();
        let daemon = fake_daemon();
        daemon
            .events
            .lock()
            .await
            .insert("sess-1".into(), vec![AgentEvent::Error("cancelled".into())]);
        let input = format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/new\",\"params\":{{\"cwd\":\"{}\"}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"sess-1\"}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"sess-1\",\"prompt\":[{{\"type\":\"text\",\"text\":\"stop\"}}]}}}}\n"
            ),
            cwd
        );
        let (writer, output) = CaptureWriter::new();
        run_transport(
            daemon.clone(),
            BufReader::new(std::io::Cursor::new(input.into_bytes())),
            writer,
        )
        .await
        .expect("transport");
        let lines: Vec<Value> = String::from_utf8(output.lock().unwrap().clone())
            .expect("utf8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("json"))
            .collect();
        assert_eq!(
            lines.last().expect("last")["result"]["stopReason"],
            "cancelled"
        );
        assert_eq!(
            daemon.cancels.lock().await.as_slice(),
            &["sess-1".to_string()]
        );
    }

    #[test]
    fn prompt_parser_rejects_non_absolute_cwd() {
        let err = validate_workspace_cwd("relative/path").expect_err("must fail");
        assert!(err.to_string().contains("absolute path"));
    }

    #[test]
    fn image_blocks_are_decoded() {
        let prompt = parse_prompt_blocks(&[
            PromptBlock::Text {
                text: "hello".into(),
            },
            PromptBlock::Image {
                mime_type: "image/png".into(),
                data: base64::engine::general_purpose::STANDARD.encode(b"png-bytes"),
                filename: Some("x.png".into()),
            },
        ])
        .expect("prompt");
        assert_eq!(prompt.text, "hello");
        assert_eq!(prompt.images[0].data, b"png-bytes");
    }

    fn parse_lines(bytes: Vec<u8>) -> Vec<Value> {
        String::from_utf8(bytes)
            .expect("utf8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("json"))
            .collect()
    }

    #[tokio::test]
    async fn initialize_advertises_spec_agent_capabilities() {
        // Plan §"Capability advertisement": the agent advertises
        // `agentCapabilities.{loadSession, promptCapabilities, mcpCapabilities}`.
        // It does NOT re-advertise client `fs.*` capabilities; those are read
        // from the request's `clientCapabilities` and stored on AcpState.
        let input = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientCapabilities":{"fs":{"readTextFile":true,"writeTextFile":true}}}}
"#;
        let (writer, output) = CaptureWriter::new();
        run_transport(fake_daemon(), BufReader::new(&input[..]), writer)
            .await
            .expect("transport");
        let lines = parse_lines(output.lock().unwrap().clone());
        let result = &lines[0]["result"];
        assert_eq!(result["protocolVersion"], json!(1));
        assert_eq!(result["agentCapabilities"]["loadSession"], json!(true));
        assert_eq!(
            result["agentCapabilities"]["promptCapabilities"]["image"],
            json!(true),
        );
        assert_eq!(
            result["agentCapabilities"]["promptCapabilities"]["audio"],
            json!(false),
        );
        assert_eq!(
            result["agentCapabilities"]["promptCapabilities"]["embeddedContext"],
            json!(false),
        );
        assert_eq!(
            result["agentCapabilities"]["mcpCapabilities"]["http"],
            json!(false),
        );
        assert_eq!(
            result["agentCapabilities"]["mcpCapabilities"]["sse"],
            json!(false),
        );
        assert_eq!(result["authMethods"], json!([]));
        assert!(
            result.get("clientCapabilities").is_none(),
            "agent must not echo clientCapabilities back: {result}"
        );
        assert!(
            result.get("capabilities").is_none(),
            "spec uses `agentCapabilities`, not `capabilities`: {result}"
        );
    }

    #[tokio::test]
    async fn authenticate_returns_success() {
        let input = br#"{"jsonrpc":"2.0","id":7,"method":"authenticate","params":{}}
"#;
        let (writer, output) = CaptureWriter::new();
        run_transport(fake_daemon(), BufReader::new(&input[..]), writer)
            .await
            .expect("transport");
        let lines = parse_lines(output.lock().unwrap().clone());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["id"], json!(7));
        assert_eq!(lines[0]["result"]["authenticated"], json!(true));
    }

    #[tokio::test]
    async fn non_2_0_jsonrpc_yields_invalid_request() {
        let input = br#"{"jsonrpc":"1.0","id":3,"method":"initialize","params":{}}
"#;
        let (writer, output) = CaptureWriter::new();
        run_transport(fake_daemon(), BufReader::new(&input[..]), writer)
            .await
            .expect("transport");
        let lines = parse_lines(output.lock().unwrap().clone());
        assert_eq!(lines[0]["error"]["code"], -32600);
        assert_eq!(lines[0]["id"], json!(3));
    }

    #[tokio::test]
    async fn unknown_method_notification_is_silently_dropped() {
        // No `id` field → notification. Spec says don't reply with
        // MethodNotFound for notifications.
        let input = br#"{"jsonrpc":"2.0","method":"bogus.notify","params":{}}
"#;
        let (writer, output) = CaptureWriter::new();
        run_transport(fake_daemon(), BufReader::new(&input[..]), writer)
            .await
            .expect("transport");
        let bytes = output.lock().unwrap().clone();
        assert!(
            bytes.is_empty(),
            "notification must not produce a response: {bytes:?}"
        );
    }

    #[tokio::test]
    async fn daemon_error_event_maps_to_internal_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_string_lossy();
        let daemon = fake_daemon();
        daemon
            .events
            .lock()
            .await
            .insert("sess-1".into(), vec![AgentEvent::Error("boom".into())]);
        let input = format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/new\",\"params\":{{\"cwd\":\"{}\"}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"sess-1\",\"prompt\":[{{\"type\":\"text\",\"text\":\"hi\"}}]}}}}\n"
            ),
            cwd
        );
        let (writer, output) = CaptureWriter::new();
        run_transport(
            daemon,
            BufReader::new(std::io::Cursor::new(input.into_bytes())),
            writer,
        )
        .await
        .expect("transport");
        let lines = parse_lines(output.lock().unwrap().clone());
        let prompt_response = lines
            .iter()
            .find(|line| line["id"] == json!(2))
            .expect("response for id=2");
        assert_eq!(prompt_response["error"]["code"], -32603);
        assert_eq!(prompt_response["error"]["data"], json!("boom"));
    }

    #[tokio::test]
    async fn session_new_emits_session_info_update_before_result() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_string_lossy();
        let input = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"session/new\",\"params\":{{\"cwd\":\"{}\"}}}}\n",
            cwd
        );
        let (writer, output) = CaptureWriter::new();
        run_transport(
            fake_daemon(),
            BufReader::new(std::io::Cursor::new(input.into_bytes())),
            writer,
        )
        .await
        .expect("transport");
        let lines = parse_lines(output.lock().unwrap().clone());
        // Notification must precede the result so the IDE has metadata before
        // it considers the session ready.
        assert_eq!(lines[0]["method"], "session_info_update");
        assert_eq!(lines[0]["params"]["sessionId"], "sess-1");
        assert_eq!(lines[1]["id"], json!(42));
        assert_eq!(lines[1]["result"]["sessionId"], "sess-1");
    }

    #[test]
    fn multiple_text_blocks_join_with_newline() {
        let prompt = parse_prompt_blocks(&[
            PromptBlock::Text {
                text: "first".into(),
            },
            PromptBlock::Text {
                text: "second".into(),
            },
        ])
        .expect("prompt");
        assert_eq!(prompt.text, "first\nsecond");
        assert!(prompt.images.is_empty());
    }

    #[test]
    fn invalid_base64_image_block_errors() {
        let err = parse_prompt_blocks(&[PromptBlock::Image {
            mime_type: "image/png".into(),
            data: "!!!not_b64!!!".into(),
            filename: None,
        }])
        .expect_err("invalid base64 must error");
        assert!(err.to_string().contains("decode image block"), "got: {err}");
    }

    #[tokio::test]
    async fn cancel_after_completion_does_not_error() {
        // session/cancel arriving after the turn already finished must be a
        // no-op, not an error frame: the cancel handler short-circuits when
        // no prompt is in flight, but still calls daemon.cancel for symmetry.
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_string_lossy();
        let daemon = fake_daemon();
        daemon
            .events
            .lock()
            .await
            .insert("sess-1".into(), vec![AgentEvent::TurnComplete]);
        let input = format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/new\",\"params\":{{\"cwd\":\"{}\"}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"sess-1\",\"prompt\":[{{\"type\":\"text\",\"text\":\"hi\"}}]}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"sess-1\"}}}}\n"
            ),
            cwd
        );
        let (writer, output) = CaptureWriter::new();
        run_transport(
            daemon.clone(),
            BufReader::new(std::io::Cursor::new(input.into_bytes())),
            writer,
        )
        .await
        .expect("transport");
        let lines = parse_lines(output.lock().unwrap().clone());
        let prompt_response = lines
            .iter()
            .find(|line| line["id"] == json!(2))
            .expect("response for id=2");
        assert_eq!(prompt_response["result"]["stopReason"], "end_turn");
        assert!(
            !lines.iter().any(|line| line["error"].is_object()),
            "no error frame expected: {lines:?}"
        );
    }

    #[test]
    fn parse_client_capabilities_requires_nested_fs_shape() {
        // Nested under `fs` (canonical ACP shape) — accepted.
        let nested = json!({"fs": {"readTextFile": true, "writeTextFile": false}});
        let caps = parse_client_capabilities(Some(&nested));
        assert!(caps.read_text_file);
        assert!(!caps.write_text_file);

        // Flat shape (top-level `readTextFile`) is NOT accepted — spec
        // requires nesting under `fs`. We don't want a malformed client to
        // get accidentally credited with caps it didn't claim.
        let flat = json!({"readTextFile": true, "writeTextFile": true});
        let caps = parse_client_capabilities(Some(&flat));
        assert!(!caps.read_text_file);
        assert!(!caps.write_text_file);

        // Absence of any capability info defaults to false rather than erroring.
        let empty = parse_client_capabilities(None);
        assert!(!empty.read_text_file);
        assert!(!empty.write_text_file);
    }

    #[tokio::test]
    async fn relative_cwd_returns_invalid_params() {
        // Regression for B1: previously a `bail!` from validate_workspace_cwd
        // bubbled up to the dispatcher's silent log path, leaving the IDE
        // hanging. Must surface as a JSON-RPC -32602 error frame keyed to
        // the request id.
        let input =
            br#"{"jsonrpc":"2.0","id":11,"method":"session/new","params":{"cwd":"relative/path"}}
"#;
        let (writer, output) = CaptureWriter::new();
        run_transport(fake_daemon(), BufReader::new(&input[..]), writer)
            .await
            .expect("transport");
        let lines = parse_lines(output.lock().unwrap().clone());
        let frame = lines
            .iter()
            .find(|line| line["id"] == json!(11))
            .expect("response for id=11");
        assert_eq!(frame["error"]["code"], json!(-32602));
        assert!(
            frame["error"]["data"]
                .as_str()
                .is_some_and(|s| s.contains("absolute")),
            "expected absolute-path detail, got: {frame}"
        );
    }

    #[tokio::test]
    async fn onboarding_required_returns_structured_error() {
        // Regression for B1+M5: onboarding gate failures must surface as a
        // dedicated error code with a kind hint so IDEs can route to a
        // user-facing onboarding prompt.
        let mut daemon = fake_daemon();
        daemon.onboarded = false;
        let input = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/new\",\"params\":{{\"cwd\":\"{}\"}}}}\n",
            tempfile::tempdir().unwrap().path().display()
        );
        let (writer, output) = CaptureWriter::new();
        run_transport(
            daemon,
            BufReader::new(std::io::Cursor::new(input.into_bytes())),
            writer,
        )
        .await
        .expect("transport");
        let lines = parse_lines(output.lock().unwrap().clone());
        let frame = lines
            .iter()
            .find(|line| line["id"] == json!(1))
            .expect("response for id=1");
        assert_eq!(frame["error"]["code"], json!(-32001));
        assert_eq!(frame["error"]["data"]["kind"], json!("onboarding_required"));
    }

    #[tokio::test]
    async fn cancel_during_active_turn_yields_cancelled_stop_reason() {
        // t1 regression: previously the cancel test seeded an
        // `Error("cancelled")` event directly, which never exercised the
        // `cancel_requested` flag. Here we stream a real text delta, send a
        // `session/cancel` notification while the turn is still alive, then
        // let the daemon emit `TurnComplete` — the dispatcher must read the
        // flag and report `cancelled` rather than `end_turn`.
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_string_lossy();
        let mut daemon = fake_daemon();
        daemon.cancel_gate = Some(Arc::new(tokio::sync::Notify::new()));
        daemon.events.lock().await.insert(
            "sess-1".into(),
            vec![AgentEvent::TextDelta("partial".into())],
        );
        let input = format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/new\",\"params\":{{\"cwd\":\"{}\"}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"sess-1\",\"prompt\":[{{\"type\":\"text\",\"text\":\"go\"}}]}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"sess-1\"}}}}\n"
            ),
            cwd
        );
        let (writer, output) = CaptureWriter::new();
        run_transport(
            daemon.clone(),
            BufReader::new(std::io::Cursor::new(input.into_bytes())),
            writer,
        )
        .await
        .expect("transport");
        let lines = parse_lines(output.lock().unwrap().clone());
        let prompt_response = lines
            .iter()
            .find(|line| line["id"] == json!(2))
            .expect("response for id=2");
        assert_eq!(prompt_response["result"]["stopReason"], json!("cancelled"));
        assert_eq!(
            daemon.cancels.lock().await.as_slice(),
            &["sess-1".to_string()],
            "daemon.cancel must be called for the active session"
        );
    }

    #[tokio::test]
    async fn daemon_disconnect_mid_turn_yields_error_and_nonzero_exit() {
        // Regression for M1: if the daemon's event channel closes before
        // emitting TurnComplete or Error, the IDE must see a -32603
        // daemon_disconnected frame and the process must exit non-zero.
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_string_lossy();
        let daemon = fake_daemon();
        // Send a partial turn (one text delta, then channel closes — no
        // TurnComplete, no Error).
        daemon
            .events
            .lock()
            .await
            .insert("sess-1".into(), vec![AgentEvent::TextDelta("hi".into())]);
        let input = format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/new\",\"params\":{{\"cwd\":\"{}\"}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"sess-1\",\"prompt\":[{{\"type\":\"text\",\"text\":\"hi\"}}]}}}}\n"
            ),
            cwd
        );
        let (writer, output) = CaptureWriter::new();
        let exit_code = run_transport(
            daemon,
            BufReader::new(std::io::Cursor::new(input.into_bytes())),
            writer,
        )
        .await
        .expect("transport");
        assert_eq!(exit_code, 1, "daemon disconnect must exit non-zero");
        let lines = parse_lines(output.lock().unwrap().clone());
        let prompt_response = lines
            .iter()
            .find(|line| line["id"] == json!(2))
            .expect("response for id=2");
        assert_eq!(prompt_response["error"]["code"], json!(-32603));
        assert_eq!(
            prompt_response["error"]["data"]["kind"],
            json!("daemon_disconnected"),
        );
    }

    // ───────────────────────────────────────────────────────────────────────
    // Source-level guard: the ACP transport's hard invariant is that
    // *nothing* writes to stdout/stderr except valid JSON-RPC frames going
    // through the `run_transport` writer. A stray `println!` / `eprintln!`
    // anywhere in this module corrupts the IDE's stdin parser and is the
    // ACP equivalent of the TUI mouse-mode regression: easy to reintroduce,
    // expensive to debug from the IDE side. This invariant cannot be
    // expressed by types (the language permits printing from any function);
    // a source-grep guard is the cheapest way to keep it locked down. Same
    // template as the TUI mouse guards in `crates/interface/src/tui/mod.rs`.
    //
    // Comments and `#[cfg(test)]` blocks are stripped before scanning so
    // documentation may reference the forbidden patterns and tests may
    // legitimately use `eprintln!` for diagnostic output that never runs in
    // production.
    fn strip_tests_and_comments(src: &str) -> String {
        let code = match src.find("#[cfg(test)]") {
            Some(idx) => &src[..idx],
            None => src,
        };
        code.lines()
            .map(|line| match line.find("//") {
                Some(idx) => &line[..idx],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn acp_module_emits_no_stdout_or_stderr_writes() {
        // Forbid the print/eprint family (which auto-acquire stdout/stderr
        // from any call site) and direct `std::io::{stdout,stderr}()` grabs.
        //
        // We deliberately do NOT forbid `tokio::io::stdout()` / `tokio::io::
        // stderr()` because `run_acp_stdio` legitimately calls
        // `tokio::io::stdout()` exactly once to hand the writer to
        // `run_transport`. Restricting that call site is enforced by the
        // count check below: more than one `tokio::io::stdout(` site means
        // someone added another writer path.
        const FORBIDDEN_MACROS_AND_STD_IO: &[&str] = &[
            "println!",
            "print!",
            "eprintln!",
            "eprint!",
            "std::io::stdout(",
            "std::io::stderr(",
        ];
        let src = include_str!("acp.rs");
        let code = strip_tests_and_comments(src);
        // Sentinel: the strip helper is naive (line-comments only, first
        // `#[cfg(test)]` block onward). If it ever became aggressive
        // enough to strip away the whole prod code section, every
        // forbidden-pattern check would vacuously pass. Pin a known
        // production identifier so a stripping bug fails the test
        // instead.
        assert!(
            code.contains("run_acp_stdio"),
            "strip_tests_and_comments removed prod code; the source-grep \
             guard would silently pass against an empty input. Investigate \
             the helper before trusting any subsequent assertion in this test."
        );
        for needle in FORBIDDEN_MACROS_AND_STD_IO {
            assert!(
                !code.contains(needle),
                "crates/interface/src/commands/acp.rs contains forbidden output \
                 pattern `{needle}` outside of `#[cfg(test)]`. ACP mode reserves \
                 stdout for JSON-RPC frames and stderr for IDE-captured logs; \
                 even one stray write corrupts the IDE's parser. Route \
                 diagnostics through `tracing::*` (which goes to acp.log) \
                 instead. See the comment block above this test for rationale."
            );
        }

        // The single allowed stdout grab lives in `run_acp_stdio`. Any
        // additional `tokio::io::stdout(` site means a second code path is
        // writing to the transport — also a regression.
        let stdout_count = code.matches("tokio::io::stdout(").count();
        assert_eq!(
            stdout_count, 1,
            "exactly one `tokio::io::stdout()` call site is allowed (the one \
             in `run_acp_stdio` that hands the writer to `run_transport`); \
             found {stdout_count}. A second site means another code path \
             is writing to stdout, which corrupts ACP framing."
        );
        let stderr_count = code.matches("tokio::io::stderr(").count();
        assert_eq!(
            stderr_count, 0,
            "no `tokio::io::stderr()` calls are allowed in ACP mode; the \
             panic hook + tracing layer route all diagnostic output to \
             acp.log. Found {stderr_count}."
        );
    }
}
