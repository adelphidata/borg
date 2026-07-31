//! ACP (Agent Client Protocol) stdio bridge: lets IDEs drive Borg over
//! newline-delimited JSON-RPC on stdin/stdout.
//!
//! Split by concern:
//! - [`transport`] — JSON-RPC envelopes, frame writers, dispatch loop.
//! - [`handlers`] — per-method request handling and param parsing.
//! - [`translate`] — pure agent-event/history → `session/update` mapping.
//!
//! This module keeps the public entrypoint (`run_acp_stdio`), process-wide
//! logging/panic setup, the `AcpDaemon` abstraction over borgd, and the
//! shared session/state/error types.

mod handlers;
mod translate;
mod transport;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use cortex::agent::AgentEvent;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::daemon_client::{connect_or_spawn, AcpFsClient, DaemonClient};

use transport::run_transport;

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
        // `process::exit` skips destructors, so flush the tracing worker
        // first — the tail of acp.log is exactly the lines explaining why
        // the daemon connection failed.
        flush_acp_logs();
        std::process::exit(exit_code);
    }
    Ok(())
}

/// Drop the non-blocking writer guard (if we own one) so buffered log lines
/// reach disk. Safe to call at most once, right before a hard exit.
fn flush_acp_logs() {
    if let Some(slot) = ACP_LOG_GUARD.get() {
        drop(
            slot.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        );
    }
}

/// Holds the non-blocking writer guard for the lifetime of the process so
/// the background flush thread doesn't drop log lines on shutdown. A
/// `static OnceLock` is preferred over `Box::leak` because we can detect
/// double-init (which can happen if a cortex code path lazy-installed a
/// subscriber before us) and degrade gracefully instead of panicking.
static ACP_LOG_GUARD: std::sync::OnceLock<
    StdMutex<Option<tracing_appender::non_blocking::WorkerGuard>>,
> = std::sync::OnceLock::new();

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
    let _ = ACP_LOG_GUARD.set(StdMutex::new(Some(guard)));
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
    /// agent as failed.
    daemon_failed: Arc<std::sync::atomic::AtomicBool>,
    /// Wakes the inbound loop so it stops reading stdin after a daemon
    /// disconnect — the error frame for the in-flight request is already
    /// sent; further requests would only produce more failures.
    shutdown: Arc<tokio::sync::Notify>,
}

#[derive(Clone, Debug, Default)]
struct ClientCapabilities {
    read_text_file: bool,
    write_text_file: bool,
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

    fn prompt_in_flight(session_id: &str) -> Self {
        Self {
            code: -32000,
            message: "Server busy",
            data: Some(json!({
                "kind": "prompt_in_flight",
                "sessionId": session_id,
                "detail": "another session/prompt is already running on this session; cancel it first or wait for it to complete",
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

#[cfg(test)]
mod tests {
    use super::handlers::*;
    use super::translate::*;
    use super::transport::*;
    use super::*;
    use anyhow::anyhow;
    use base64::Engine;
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
        /// When set, send_prompt drives a full fs-callback roundtrip through
        /// the provided [`AcpFsClient`] (read then write) instead of playing
        /// canned events — exercising the transport's outbound-request /
        /// inbound-response plumbing that canned events never touch.
        fs_roundtrip: bool,
        /// With `cancel_gate`: drop the event channel after the gate fires
        /// instead of emitting `TurnComplete` — models a daemon that winds
        /// the turn down (channel close, no terminal event) in response to
        /// cancel, exercising the Disconnected+cancel race branch.
        cancel_gate_drops: bool,
        /// `onboarding_complete` returns Err — models a daemon that's up
        /// but can't answer the onboarding probe.
        onboarding_err: bool,
        /// `cancel` returns Err — models a daemon-side cancel failure,
        /// which must be logged and treated as a no-op.
        cancel_fails: bool,
        /// When present, `open_session` blocks acquiring a permit — lets a
        /// test pile up in-flight requests to hit the dispatcher cap.
        open_gate: Option<Arc<tokio::sync::Semaphore>>,
    }

    #[async_trait]
    impl AcpDaemon for FakeDaemon {
        async fn onboarding_complete(&self) -> Result<bool> {
            if self.onboarding_err {
                return Err(anyhow!("daemon unreachable during onboarding probe"));
            }
            Ok(self.onboarded)
        }

        async fn open_session(
            &self,
            resume_id: Option<&str>,
            _workspace_cwd: Option<&Path>,
            _workspace_root: Option<&Path>,
            _enforce_workspace_root: bool,
        ) -> Result<(String, u64)> {
            if let Some(gate) = self.open_gate.as_ref() {
                let _permit = gate.acquire().await;
            }
            Ok((resume_id.unwrap_or(&self.open_session_id).to_string(), 0))
        }

        async fn send_prompt<F: AcpFsClient>(
            &self,
            session_id: &str,
            prompt: PromptInput,
            fs_client: F,
        ) -> Result<mpsc::Receiver<AgentEvent>> {
            self.prompts.lock().await.push(prompt);
            let (tx, rx) = mpsc::channel(16);
            if self.fs_roundtrip {
                tokio::spawn(async move {
                    match fs_client.read_text_file("buffer.txt").await {
                        Ok(content) => {
                            let _ = tx
                                .send(AgentEvent::TextDelta(format!("FS:{content}")))
                                .await;
                        }
                        Err(e) => {
                            let _ = tx.send(AgentEvent::Error(format!("fs read: {e}"))).await;
                            return;
                        }
                    }
                    if let Err(e) = fs_client.write_text_file("out.txt", "data").await {
                        let _ = tx.send(AgentEvent::Error(format!("fs write: {e}"))).await;
                        return;
                    }
                    let _ = tx.send(AgentEvent::TurnComplete).await;
                });
                return Ok(rx);
            }
            let events = self
                .events
                .lock()
                .await
                .remove(session_id)
                .unwrap_or_default();
            if let Some(gate) = self.cancel_gate.clone() {
                let drop_after = self.cancel_gate_drops;
                tokio::spawn(async move {
                    for event in events {
                        let _ = tx.send(event).await;
                    }
                    gate.notified().await;
                    if !drop_after {
                        let _ = tx.send(AgentEvent::TurnComplete).await;
                    }
                    // drop_after: channel closes with no terminal event.
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
            if self.cancel_fails {
                return Err(anyhow!("daemon cancel RPC failed"));
            }
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
            fs_roundtrip: false,
            cancel_gate_drops: false,
            onboarding_err: false,
            cancel_fails: false,
            open_gate: None,
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
        // Regression guard: persisted history must replay as ACP-spec
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
            .filter(|l| {
                l["method"] == "session/update"
                    && l["params"]["update"]["sessionUpdate"] != "session_info_update"
            })
            .collect();
        assert_eq!(updates.len(), 2, "system role must be dropped: {lines:?}");
        assert_eq!(
            updates[0]["params"]["update"]["sessionUpdate"],
            "user_message_chunk"
        );
        assert_eq!(updates[0]["params"]["update"]["content"]["type"], "text");
        assert_eq!(updates[0]["params"]["update"]["content"]["text"], "hi");
        assert_eq!(
            updates[1]["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        assert_eq!(
            updates[1]["params"]["update"]["content"]["text"], "hello  world",
            "internal tags must be stripped on replay",
        );
        assert!(
            !lines
                .iter()
                .any(|l| l["params"]["update"]["sessionUpdate"] == "history_message"),
            "history_message is non-spec and must not appear",
        );
        let info = lines
            .iter()
            .find(|l| l["params"]["update"]["sessionUpdate"] == "session_info_update")
            .expect("session_info_update variant missing");
        assert_eq!(info["method"], "session/update");
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
        assert_eq!(
            lines[2]["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        assert_eq!(
            lines[2]["params"]["update"]["content"]["text"],
            "ok  visible"
        );
        // Tool kickoff must be a spec-shaped tool_call: required `title`,
        // `kind` (spec field name — NOT `toolKind`), a valid status enum
        // value, and rawInput parsed from the args JSON so IDEs can render
        // structured params. `read_file` categorises as `read`.
        let kickoff = &lines[3]["params"]["update"];
        assert_eq!(kickoff["sessionUpdate"], "tool_call");
        assert_eq!(kickoff["toolCallId"], "call_1");
        assert_eq!(kickoff["title"], "read_file");
        assert_eq!(kickoff["kind"], "read");
        assert_eq!(kickoff["status"], "in_progress");
        assert_eq!(kickoff["rawInput"]["path"], "/tmp/x");
        // Output delta + result are tool_call_update frames carrying a
        // ToolCallContent array.
        let delta = &lines[4]["params"]["update"];
        assert_eq!(delta["sessionUpdate"], "tool_call_update");
        assert_eq!(delta["status"], "in_progress");
        assert_eq!(delta["content"][0]["type"], "content");
        assert_eq!(delta["content"][0]["content"]["text"], "partial");
        let result_update = &lines[5]["params"]["update"];
        assert_eq!(result_update["status"], "completed");
        assert_eq!(result_update["content"][0]["content"]["text"], "done");
        // Plan entries are spec-shaped {content, priority, status}.
        let plan = &lines[6]["params"]["update"];
        assert_eq!(plan["sessionUpdate"], "plan");
        assert_eq!(plan["entries"][0]["content"], "Ship ACP");
        assert_eq!(plan["entries"][0]["priority"], "medium");
        assert_eq!(plan["entries"][0]["status"], "in_progress");
        assert!(
            plan["entries"][0].get("id").is_none() && plan["entries"][0].get("title").is_none(),
            "plan entries must use spec field `content`, not id/title: {plan}"
        );
        assert_eq!(
            lines.last().expect("final line")["result"]["stopReason"],
            "end_turn"
        );
    }

    /// ACP toolKind mapping. Drives the
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

    /// `<internal>` stripping is a translator-only concern. Other event consumers (TUI,
    /// daemon log) keep raw text. Regression guard: if upstream cortex
    /// starts stripping, this still passes; if the ACP translator stops
    /// stripping on `ToolOutputDelta`, this fails.
    #[test]
    fn tool_output_delta_strips_internal_tags_on_wire() {
        let mut streams = ToolCallStreams::default();
        let event = AgentEvent::ToolOutputDelta {
            tool_call_id: "c1".into(),
            name: "run_shell".into(),
            delta: "before <internal>secret-plan</internal> after".into(),
            is_stderr: false,
        };
        let update = streams.update_for(&event, "sess-1").expect("update");
        assert_eq!(
            update["update"]["content"][0]["content"]["text"],
            "before  after"
        );
        assert_eq!(update["update"]["sessionUpdate"], "tool_call_update");
        assert_eq!(update["update"]["_meta"]["borg"]["stream"], "stdout");

        let stderr_event = AgentEvent::ToolOutputDelta {
            tool_call_id: "c2".into(),
            name: "run_shell".into(),
            delta: "<internal>x</internal>err".into(),
            is_stderr: true,
        };
        let stderr_update = streams.update_for(&stderr_event, "sess-1").expect("update");
        assert_eq!(
            stderr_update["update"]["content"][0]["content"]["text"],
            "err"
        );
        assert_eq!(stderr_update["update"]["_meta"]["borg"]["stream"], "stderr");
    }

    /// ACP `tool_call_update.content` REPLACES the client's content
    /// collection ("Replace the content collection." in the schema), so
    /// each streamed frame must carry the full output so far. A per-delta
    /// single chunk regresses spec clients to showing only the newest
    /// fragment of a streaming shell command.
    #[test]
    fn tool_output_deltas_accumulate_into_replace_shaped_content() {
        let mut streams = ToolCallStreams::default();
        let delta = |text: &str| AgentEvent::ToolOutputDelta {
            tool_call_id: "c1".into(),
            name: "run_shell".into(),
            delta: text.into(),
            is_stderr: false,
        };
        let first = streams.update_for(&delta("file-a\n"), "s").expect("update");
        assert_eq!(first["update"]["content"][0]["content"]["text"], "file-a\n");
        let second = streams.update_for(&delta("file-b\n"), "s").expect("update");
        assert_eq!(
            second["update"]["content"][0]["content"]["text"], "file-a\nfile-b\n",
            "replace-semantics frames must carry the full accumulated output"
        );
        // `<internal>` tags split across delta boundaries are stripped once
        // the accumulation reassembles them.
        let mut split = ToolCallStreams::default();
        split.update_for(
            &AgentEvent::ToolOutputDelta {
                tool_call_id: "c2".into(),
                name: "run_shell".into(),
                delta: "keep <inter".into(),
                is_stderr: false,
            },
            "s",
        );
        let rejoined = split
            .update_for(
                &AgentEvent::ToolOutputDelta {
                    tool_call_id: "c2".into(),
                    name: "run_shell".into(),
                    delta: "nal>secret</internal> this".into(),
                    is_stderr: false,
                },
                "s",
            )
            .expect("update");
        let text = rejoined["update"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(
            !text.contains("secret"),
            "tag split across deltas must still be stripped: {text:?}"
        );
    }

    /// A PTY reader that outlives its tool timeout keeps emitting deltas
    /// after `ToolResult`. Forwarding them would flip the completed card
    /// back to `in_progress` and wipe its final content (replace
    /// semantics), leaving a spinner running forever in the IDE.
    #[test]
    fn late_tool_output_delta_after_result_is_dropped() {
        let mut streams = ToolCallStreams::default();
        let result = AgentEvent::ToolResult {
            tool_call_id: "c1".into(),
            name: "run_shell".into(),
            result: "final".into(),
        };
        let completed = streams.update_for(&result, "s").expect("update");
        assert_eq!(completed["update"]["status"], "completed");
        let late = AgentEvent::ToolOutputDelta {
            tool_call_id: "c1".into(),
            name: "run_shell".into(),
            delta: "stray pty output".into(),
            is_stderr: true,
        };
        assert!(
            streams.update_for(&late, "s").is_none(),
            "a delta for a completed call must not re-open it"
        );
        // Other calls are unaffected.
        let other = AgentEvent::ToolOutputDelta {
            tool_call_id: "c2".into(),
            name: "run_shell".into(),
            delta: "live".into(),
            is_stderr: false,
        };
        assert!(streams.update_for(&other, "s").is_some());
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
        // The agent advertises
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
        assert_eq!(
            result["agentInfo"]["name"],
            json!("borg"),
            "agentInfo identifies the agent to the client: {result}"
        );
        assert!(
            result["agentInfo"]["version"].is_string(),
            "agentInfo.version must be present: {result}"
        );
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
        // it considers the session ready. Session metadata rides inside the
        // spec `session/update` notification as a `session_info_update`
        // variant — a bare top-level `session_info_update` method is
        // non-spec and real clients would drop it.
        assert_eq!(lines[0]["method"], "session/update");
        assert_eq!(
            lines[0]["params"]["update"]["sessionUpdate"],
            "session_info_update"
        );
        assert_eq!(lines[0]["params"]["sessionId"], "sess-1");
        assert_eq!(lines[0]["params"]["update"]["title"], "ACP Session");
        assert_eq!(
            lines[0]["params"]["update"]["_meta"]["borg"]["model"], "test-model",
            "Borg extras (model) must ride in _meta, the spec extension point"
        );
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

    /// ACP baseline: agents MUST accept `resource_link` prompt blocks
    /// regardless of advertised capabilities — Zed sends them for `@file`
    /// mentions. A serde reject here breaks every mention-carrying prompt.
    #[test]
    fn resource_link_blocks_render_as_markdown_links_in_text() {
        let prompt = parse_prompt_blocks(&[
            PromptBlock::Text {
                text: "look at".into(),
            },
            PromptBlock::ResourceLink {
                uri: "file:///ws/src/main.rs".into(),
                name: "main.rs".into(),
                title: None,
            },
        ])
        .expect("resource_link must be accepted");
        // `file:` URIs are decoded to plain absolute paths: the daemon's
        // path resolution has no `file://` handling, so the raw URI would
        // resolve to `<cwd>/file:/ws/...` and fail every tool call the
        // model copies it into.
        assert_eq!(prompt.text, "look at\n[main.rs](/ws/src/main.rs)");
        assert!(prompt.images.is_empty());
    }

    /// Zed percent-encodes `file:` URIs, so a path with a space arrives as
    /// `%20`. The daemon's file tools look up literal bytes — an undecoded
    /// `%20` never matches the on-disk name, and unlike the bare-scheme
    /// case the model can't recover without guessing.
    #[test]
    fn resource_link_file_uris_are_percent_decoded() {
        let prompt = parse_prompt_blocks(&[
            PromptBlock::ResourceLink {
                uri: "file:///ws/my%20file.rs".into(),
                name: "my file.rs".into(),
                title: None,
            },
            PromptBlock::ResourceLink {
                uri: "https://example.com/a%20b".into(),
                name: "docs".into(),
                title: None,
            },
        ])
        .expect("prompt");
        assert_eq!(
            prompt.text, "[my file.rs](/ws/my file.rs)\n[docs](https://example.com/a%20b)",
            "file URIs decode to plain paths; non-file schemes pass through untouched"
        );
    }

    /// `title` (display label) wins over `name` when present.
    #[test]
    fn resource_link_prefers_title_over_name() {
        let prompt = parse_prompt_blocks(&[PromptBlock::ResourceLink {
            uri: "file:///ws/README.md".into(),
            name: "README.md".into(),
            title: Some("Project readme".into()),
        }])
        .expect("prompt");
        assert_eq!(prompt.text, "[Project readme](/ws/README.md)");
    }

    /// Audio and embedded-resource blocks are gated behind prompt
    /// capabilities Borg does not advertise. A compliant client never sends
    /// them; a non-compliant one must get a clean invalid_params (-32602)
    /// naming the missing capability — not a serde "unknown variant" blob.
    #[test]
    fn audio_and_resource_blocks_reject_with_capability_error() {
        let audio_err =
            parse_prompt_blocks(&[PromptBlock::Audio {}]).expect_err("audio must reject");
        let mapped = AcpRequestError::from_anyhow(&audio_err);
        assert_eq!(mapped.code, -32602);
        assert!(
            audio_err.to_string().contains("audio"),
            "error must name the capability: {audio_err}"
        );

        let resource_err = parse_prompt_blocks(&[PromptBlock::Resource {}])
            .expect_err("embedded resource must reject");
        let mapped = AcpRequestError::from_anyhow(&resource_err);
        assert_eq!(mapped.code, -32602);
        assert!(
            resource_err.to_string().contains("embeddedContext"),
            "error must name the capability: {resource_err}"
        );
    }

    /// Tool args stream from the daemon as a JSON string; `rawInput` must be
    /// the parsed object so IDEs can render structured parameters. Garbled
    /// args (truncated stream) must degrade to the raw string, not drop the
    /// whole tool_call frame.
    #[test]
    fn tool_call_raw_input_parses_json_and_falls_back_to_string() {
        let event = AgentEvent::ToolExecuting {
            tool_call_id: "c1".into(),
            name: "run_shell".into(),
            args: r#"{"command":"ls"}"#.into(),
        };
        let update = event_to_update(&event, "s").expect("update");
        assert_eq!(update["update"]["rawInput"]["command"], "ls");

        let garbled = AgentEvent::ToolExecuting {
            tool_call_id: "c2".into(),
            name: "run_shell".into(),
            args: r#"{"command": trunc"#.into(),
        };
        let update = event_to_update(&garbled, "s").expect("update");
        assert_eq!(
            update["update"]["rawInput"],
            json!(r#"{"command": trunc"#),
            "unparseable args must fall back to the raw string"
        );
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

    /// Regression: `AgentEvent::ThinkingDelta` (Claude-CLI extended thinking,
    /// OpenAI-style reasoning streams) used to fall through `event_to_update`
    /// and never reach the IDE. ACP surfaces reasoning as
    /// `agent_thought_chunk`. `<internal>` stripping must apply to thoughts
    /// the same way it does to `TextDelta` so a reasoning stream can't
    /// smuggle hidden scratchpad.
    #[test]
    fn thinking_delta_emits_agent_thought_chunk() {
        let event =
            AgentEvent::ThinkingDelta("let me think<internal>scratch</internal> about this".into());
        let update =
            event_to_update(&event, "sess-1").expect("ThinkingDelta must produce a session/update");
        assert_eq!(update["sessionId"], json!("sess-1"));
        assert_eq!(
            update["update"]["sessionUpdate"],
            json!("agent_thought_chunk")
        );
        assert_eq!(
            update["update"]["content"]["text"],
            json!("let me think about this"),
            "internal tags must be stripped from agent_thought_chunk too: {update}"
        );
    }

    /// Regression: a second `session/prompt` arriving while the first is
    /// still streaming used to overwrite `prompt_in_flight = true` and reset
    /// `cancel_requested = false`, racing the original prompt's cancel
    /// handling. The transport must reject with a structured busy error
    /// (-32000 / kind=`prompt_in_flight`) instead so the IDE can surface it.
    ///
    /// Tests `handle_request` directly with a session pre-marked in-flight.
    /// The end-to-end timing race in `acp_e2e/lifecycle.rs` (cancel mid-turn)
    /// covers the streaming path; here we pin the dispatcher's reject
    /// behavior in isolation so the daemon is never invoked.
    #[tokio::test]
    async fn concurrent_prompt_on_same_session_rejects_with_busy_error() {
        use std::sync::atomic::AtomicU64;
        let daemon = fake_daemon();
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().to_path_buf();
        let state = AcpState {
            daemon: daemon.clone(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            client_capabilities: Arc::new(StdMutex::new(ClientCapabilities::default())),
            onboarding_confirmed: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            daemon_failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shutdown: Arc::new(tokio::sync::Notify::new()),
        };
        // Pre-seed the session with prompt_in_flight = true to simulate a
        // first turn that's still streaming.
        state.sessions.lock().await.insert(
            "sess-1".into(),
            SessionRecord {
                workspace_cwd: workspace.clone(),
                workspace_root: workspace,
                runtime: SessionRuntime {
                    prompt_in_flight: true,
                    cancel_requested: false,
                },
            },
        );
        let (writer, _output) = CaptureWriter::new();
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(writer)));
        let callback_client = JsonRpcCallbackClient {
            writer: writer.clone(),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            capabilities: state.client_capabilities.clone(),
        };
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(3)),
            method: "session/prompt".to_string(),
            params: Some(json!({
                "sessionId": "sess-1",
                "prompt": [{"type": "text", "text": "second"}],
            })),
        };
        let result = handle_request(state.clone(), req, writer, callback_client).await;
        let err = result.expect_err("second concurrent prompt must reject");
        let mapped = AcpRequestError::from_anyhow(&err);
        assert_eq!(
            mapped.code, -32000,
            "second prompt must surface server-busy code -32000; got {mapped:?}"
        );
        let data = mapped.data.expect("busy error must carry data");
        assert_eq!(
            data["kind"],
            json!("prompt_in_flight"),
            "busy error must carry kind=prompt_in_flight for routing: {data}"
        );
        assert_eq!(
            data["sessionId"],
            json!("sess-1"),
            "busy error must echo sessionId so the IDE can correlate: {data}"
        );
        // Daemon must NOT have been invoked — busy reject happens before
        // send_prompt, otherwise the cancel-flag race we're guarding
        // against would still be reachable.
        assert!(
            daemon.prompts.lock().await.is_empty(),
            "rejected prompt must never reach daemon.send_prompt"
        );
        // Flag must remain set (the first prompt is still in flight). A
        // regression that reset it would let the next prompt slip through.
        let sessions = state.sessions.lock().await;
        assert!(
            sessions
                .get("sess-1")
                .expect("session present")
                .runtime
                .prompt_in_flight,
            "rejection must not flip prompt_in_flight off (still owned by the first prompt)"
        );
    }

    /// Regression: when `daemon.send_prompt` itself fails (e.g. transient
    /// gRPC error opening the bidi stream), the prompt handler used to
    /// propagate the error without resetting `prompt_in_flight`. Combined
    /// with the new busy-reject above, that would lock the session
    /// permanently. Pin that the flag is restored on the daemon-failure
    /// path so the next prompt can proceed.
    #[tokio::test]
    async fn prompt_handler_resets_in_flight_on_daemon_send_failure() {
        use std::sync::atomic::AtomicU64;
        // A purpose-built failing daemon: `FakeDaemon`'s `send_prompt` only
        // errors on a missing session entry, which the dispatcher catches
        // earlier (BEFORE setting the in-flight flag). We need the failure
        // to come from `send_prompt` itself so the cleanup path runs.
        #[derive(Clone)]
        struct FailingDaemon;
        #[async_trait]
        impl AcpDaemon for FailingDaemon {
            async fn onboarding_complete(&self) -> Result<bool> {
                Ok(true)
            }
            async fn open_session(
                &self,
                _resume_id: Option<&str>,
                _workspace_cwd: Option<&Path>,
                _workspace_root: Option<&Path>,
                _enforce_workspace_root: bool,
            ) -> Result<(String, u64)> {
                Ok(("sess-1".to_string(), 0))
            }
            async fn send_prompt<F: AcpFsClient>(
                &self,
                _session_id: &str,
                _prompt: PromptInput,
                _fs_client: F,
            ) -> Result<mpsc::Receiver<AgentEvent>> {
                Err(anyhow!("simulated daemon RPC failure"))
            }
            async fn cancel(&self, _session_id: &str) -> Result<()> {
                Ok(())
            }
            async fn state(&self, _session_id: &str) -> Result<SessionMetadata> {
                Ok(SessionMetadata {
                    session_id: "sess-1".into(),
                    title: "t".into(),
                    model: "m".into(),
                    workspace_cwd: None,
                    workspace_root: None,
                })
            }
            async fn replay_history(&self, _session_id: &str) -> Result<Vec<HistoryMessage>> {
                Ok(Vec::new())
            }
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().to_path_buf();
        let state = AcpState {
            daemon: FailingDaemon,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            client_capabilities: Arc::new(StdMutex::new(ClientCapabilities::default())),
            onboarding_confirmed: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            daemon_failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shutdown: Arc::new(tokio::sync::Notify::new()),
        };
        state.sessions.lock().await.insert(
            "sess-1".into(),
            SessionRecord {
                workspace_cwd: workspace.clone(),
                workspace_root: workspace,
                runtime: SessionRuntime::default(),
            },
        );
        let (writer, _output) = CaptureWriter::new();
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(writer)));
        let callback_client = JsonRpcCallbackClient {
            writer: writer.clone(),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            capabilities: state.client_capabilities.clone(),
        };
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(2)),
            method: "session/prompt".to_string(),
            params: Some(json!({
                "sessionId": "sess-1",
                "prompt": [{"type": "text", "text": "go"}],
            })),
        };
        let result = handle_request(state.clone(), req, writer, callback_client).await;
        assert!(result.is_err(), "send_prompt failure must propagate");
        let sessions = state.sessions.lock().await;
        assert!(
            !sessions
                .get("sess-1")
                .expect("session present")
                .runtime
                .prompt_in_flight,
            "prompt_in_flight MUST reset on daemon failure or the session is permanently locked"
        );
    }

    /// Regression: text blocks accumulated unbounded; a misbehaving IDE
    /// could materialise an arbitrary-large prompt before the daemon ever
    /// saw it (DoS surface). `parse_prompt_blocks` must short-circuit with
    /// invalid_params once accumulated bytes exceed `MAX_PROMPT_BYTES`.
    #[test]
    fn oversize_prompt_text_rejected_as_invalid_params() {
        let huge = "x".repeat(MAX_PROMPT_BYTES + 1);
        let err = parse_prompt_blocks(&[PromptBlock::Text { text: huge }])
            .expect_err("oversized text must reject");
        let mapped = AcpRequestError::from_anyhow(&err);
        assert_eq!(mapped.code, -32602, "must surface as invalid_params");
        assert!(
            err.to_string().contains("prompt too large"),
            "error must explain the cap to the caller; got: {err}"
        );
    }

    /// Regression for the image branch of the byte cap: base64 decode used
    /// to run unconditionally, so an attacker could stream a multi-GB image
    /// block and OOM the transport long before the daemon weighed in.
    #[test]
    fn oversize_prompt_image_rejected_as_invalid_params() {
        // Build an image block whose decoded payload (3 MiB of zero bytes)
        // exceeds MAX_PROMPT_BYTES. Base64 ratio is 4/3, so the encoded
        // string is ~4 MiB — also above the early-reject threshold.
        let bytes = vec![0u8; (MAX_PROMPT_BYTES * 3) / 2];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let err = parse_prompt_blocks(&[PromptBlock::Image {
            mime_type: "image/png".into(),
            data: encoded,
            filename: Some("huge.png".into()),
        }])
        .expect_err("oversized image must reject");
        let mapped = AcpRequestError::from_anyhow(&err);
        assert_eq!(mapped.code, -32602, "must surface as invalid_params");
        assert!(
            err.to_string().contains("prompt too large"),
            "error must explain the cap to the caller; got: {err}"
        );
    }

    /// Regression: ACP spec defines `session/cancel` as a notification, but a
    /// tolerant client can send it as a request (with `id`). The handler
    /// previously returned `Ok(())` without writing any response, so a
    /// request-shaped cancel made the IDE wait forever. Must respond with
    /// `result: null` keyed to the request id.
    #[tokio::test]
    async fn session_cancel_with_id_returns_null_result() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_string_lossy();
        let input = format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/new\",\"params\":{{\"cwd\":\"{}\"}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"sess-1\"}}}}\n"
            ),
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
        let cancel_resp = lines
            .iter()
            .find(|l| l["id"] == json!(7))
            .unwrap_or_else(|| panic!("no response for cancel-as-request id=7: {lines:?}"));
        assert!(
            cancel_resp.get("error").is_none(),
            "cancel-as-request must succeed, not error: {cancel_resp}"
        );
        assert_eq!(
            cancel_resp["result"],
            Value::Null,
            "ACP spec: cancel response is empty/null when one is sent"
        );
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
        // Regression: previously a `bail!` from validate_workspace_cwd
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
        // Regression: onboarding gate failures must surface as a
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
        // Regression: previously the cancel test seeded an
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
        // Regression: if the daemon's event channel closes before
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

    /// A grab-bag of malformed inbound frames: non-object JSON, wrong-typed
    /// `method`, blank/whitespace lines, and a notification whose handler
    /// errors (bad params). The transport must answer each parse failure
    /// with -32700, stay silent for the errored notification, and keep
    /// serving valid requests afterwards.
    #[tokio::test]
    async fn transport_survives_garbage_frames_and_errored_notifications() {
        let input = concat!(
            "[]\n",
            "42\n",
            "{\"jsonrpc\":\"2.0\",\"method\":42}\n",
            "\n",
            "   \n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"session/cancel\",\"params\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"initialize\",\"params\":{}}\n",
        )
        .as_bytes()
        .to_vec();
        let (writer, output) = CaptureWriter::new();
        run_transport(
            fake_daemon(),
            BufReader::new(std::io::Cursor::new(input)),
            writer,
        )
        .await
        .expect("transport");
        let lines = parse_lines(output.lock().unwrap().clone());
        let parse_errors = lines
            .iter()
            .filter(|l| l["error"]["code"] == json!(-32700))
            .count();
        // `42` and `{"method":42}` are unparseable in every reading.
        assert_eq!(
            parse_errors, 2,
            "malformed frames (42, method:42) must yield -32700: {lines:?}"
        );
        // `[]` is a JSON-RPC batch, which ACP does not use. Without an
        // explicit check, serde would accept the sequence as an all-default
        // response envelope and silently drop it — spec says -32600.
        let invalid = lines
            .iter()
            .find(|l| l["error"]["code"] == json!(-32600))
            .expect("batch array must yield -32600 Invalid Request");
        assert_eq!(invalid["id"], Value::Null, "batch reject carries null id");
        let init = lines
            .iter()
            .find(|l| l["id"] == json!(9))
            .expect("initialize after garbage must be served");
        assert!(init["result"].is_object(), "got: {init}");
        // The errored cancel notification (bad params) must produce NO
        // frame — JSON-RPC forbids replying to notifications, even errors.
        assert_eq!(
            lines.len(),
            4,
            "2 parse errors + 1 batch reject + 1 init result and nothing else: {lines:?}"
        );
    }

    /// Response envelopes whose id matches no pending callback are dropped
    /// silently (both numeric and string ids) — a late or duplicated IDE
    /// reply must not wedge or spam the transport.
    #[tokio::test]
    async fn response_with_unknown_id_is_silently_ignored() {
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":\"abc\",\"result\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        )
        .as_bytes()
        .to_vec();
        let (writer, output) = CaptureWriter::new();
        run_transport(
            fake_daemon(),
            BufReader::new(std::io::Cursor::new(input)),
            writer,
        )
        .await
        .expect("transport");
        let lines = parse_lines(output.lock().unwrap().clone());
        assert_eq!(
            lines.len(),
            1,
            "stray responses must produce no output: {lines:?}"
        );
        assert_eq!(lines[0]["id"], json!(1));
    }

    /// A prompt to a session id that was never opened must reject with the
    /// structured unknown_session error, and never reach the daemon.
    #[tokio::test]
    async fn prompt_for_unknown_session_rejects_with_unknown_session_kind() {
        let daemon = fake_daemon();
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/prompt\",",
            "\"params\":{\"sessionId\":\"ghost\",\"prompt\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n",
        )
        .as_bytes()
        .to_vec();
        let (writer, output) = CaptureWriter::new();
        run_transport(
            daemon.clone(),
            BufReader::new(std::io::Cursor::new(input)),
            writer,
        )
        .await
        .expect("transport");
        let lines = parse_lines(output.lock().unwrap().clone());
        let frame = lines.iter().find(|l| l["id"] == json!(1)).expect("frame");
        assert_eq!(frame["error"]["code"], json!(-32602));
        assert_eq!(frame["error"]["data"]["kind"], json!("unknown_session"));
        assert_eq!(frame["error"]["data"]["sessionId"], json!("ghost"));
        assert!(
            daemon.prompts.lock().await.is_empty(),
            "unknown-session prompt must never reach the daemon"
        );
    }

    /// Missing required params (session/new without cwd) must map to a
    /// -32602 with the serde detail, not a hang or an internal error.
    #[tokio::test]
    async fn session_new_missing_cwd_param_yields_invalid_params() {
        let input = br#"{"jsonrpc":"2.0","id":4,"method":"session/new","params":{}}
"#;
        let (writer, output) = CaptureWriter::new();
        run_transport(fake_daemon(), BufReader::new(&input[..]), writer)
            .await
            .expect("transport");
        let lines = parse_lines(output.lock().unwrap().clone());
        let frame = lines.iter().find(|l| l["id"] == json!(4)).expect("frame");
        assert_eq!(frame["error"]["code"], json!(-32602));
        assert!(
            frame["error"]["data"]
                .as_str()
                .is_some_and(|s| s.contains("invalid params")),
            "detail must explain the parse failure: {frame}"
        );
    }

    /// Cancel race: the client cancels, and the daemon reacts by closing the
    /// event channel *without* a terminal event. That must surface as
    /// stopReason=cancelled — NOT as daemon_disconnected — and must not
    /// poison the process exit code.
    #[tokio::test]
    async fn cancel_then_channel_close_reports_cancelled_not_disconnect() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_string_lossy();
        let mut daemon = fake_daemon();
        daemon.cancel_gate = Some(Arc::new(tokio::sync::Notify::new()));
        daemon.cancel_gate_drops = true;
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
        let exit = run_transport(
            daemon,
            BufReader::new(std::io::Cursor::new(input.into_bytes())),
            writer,
        )
        .await
        .expect("transport");
        assert_eq!(exit, 0, "cancel race must not flag daemon failure");
        let lines = parse_lines(output.lock().unwrap().clone());
        let frame = lines.iter().find(|l| l["id"] == json!(2)).expect("frame");
        assert_eq!(
            frame["result"]["stopReason"],
            json!("cancelled"),
            "channel-close after cancel is a cancellation, not a crash: {frame}"
        );
    }

    /// A daemon-side cancel RPC failure is logged and treated as a no-op —
    /// it must not surface an error frame to the IDE.
    #[tokio::test]
    async fn daemon_cancel_failure_is_swallowed_as_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_string_lossy();
        let mut daemon = fake_daemon();
        daemon.cancel_fails = true;
        let input = format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/new\",\"params\":{{\"cwd\":\"{}\"}}}}\n",
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
        assert!(
            !lines.iter().any(|l| l["error"].is_object()),
            "cancel failure must not produce an error frame: {lines:?}"
        );
        assert_eq!(
            daemon.cancels.lock().await.as_slice(),
            &["sess-1".to_string()],
            "daemon.cancel must still have been attempted"
        );
    }

    /// The onboarding probe itself erroring (daemon up but RPC failing) maps
    /// to -32603 with a probe-specific detail — distinct from the -32001
    /// "not onboarded" gate.
    #[tokio::test]
    async fn onboarding_probe_failure_maps_to_internal_error() {
        let mut daemon = fake_daemon();
        daemon.onboarding_err = true;
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
        let frame = lines.iter().find(|l| l["id"] == json!(1)).expect("frame");
        assert_eq!(frame["error"]["code"], json!(-32603));
        assert!(
            frame["error"]["data"]
                .as_str()
                .is_some_and(|s| s.contains("onboarding probe failed")),
            "probe failure needs its own detail: {frame}"
        );
    }

    /// The dispatcher caps concurrently in-flight requests. Requests past
    /// the cap get a structured busy error; notifications past the cap are
    /// dropped. Releasing the blockage drains the queued work normally.
    #[tokio::test]
    async fn inflight_cap_rejects_excess_requests_with_busy_error() {
        use tokio::io::AsyncWriteExt;
        let mut daemon = fake_daemon();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        daemon.open_gate = Some(gate.clone());
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_string_lossy().to_string();
        let (mut input, input_reader) = tokio::io::duplex(1 << 20);
        let (writer, output) = CaptureWriter::new();
        let transport = tokio::spawn(run_transport(daemon, BufReader::new(input_reader), writer));
        // 64 session/new requests block on the open gate and fill every
        // dispatcher slot; the 65th must bounce with too_many_inflight.
        let mut bytes = String::new();
        for id in 1..=65 {
            bytes.push_str(&format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"session/new\",\"params\":{{\"cwd\":\"{cwd}\"}}}}\n"
            ));
        }
        // A notification arriving at the cap is dropped (logged, no frame).
        bytes.push_str("{\"jsonrpc\":\"2.0\",\"method\":\"session/cancel\",\"params\":{\"sessionId\":\"sess-1\"}}\n");
        input.write_all(bytes.as_bytes()).await.expect("write");

        let busy = wait_for_predicate_frame(&output, |f| {
            f["error"]["data"]["kind"] == json!("too_many_inflight")
        })
        .await;
        assert_eq!(busy["id"], json!(65), "the 65th request takes the bounce");
        assert_eq!(busy["error"]["code"], json!(-32000));

        // Unblock the daemon; all 64 queued opens must now complete.
        gate.add_permits(1000);
        drop(input);
        let exit = tokio::time::timeout(std::time::Duration::from_secs(10), transport)
            .await
            .expect("transport exits")
            .expect("join")
            .expect("run_transport");
        assert_eq!(exit, 0);
        let lines = parse_lines(output.lock().unwrap().clone());
        let ok_results = lines
            .iter()
            .filter(|l| l["result"]["sessionId"] == json!("sess-1"))
            .count();
        assert_eq!(ok_results, 64, "all capped-but-admitted requests drain");
    }

    /// Poll the captured output until any frame satisfies the predicate.
    async fn wait_for_predicate_frame(
        output: &Arc<StdMutex<Vec<u8>>>,
        pred: impl Fn(&Value) -> bool,
    ) -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let frames = parse_lines(output.lock().unwrap().clone());
            if let Some(frame) = frames.iter().find(|f| pred(f)) {
                return frame.clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for frame; got: {frames:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Encoded image data larger than twice the remaining budget must be
    /// rejected BEFORE base64 decode spends memory/CPU on it.
    #[test]
    fn oversized_encoded_image_early_rejects_before_decode() {
        // Deliberately NOT valid base64 — the early reject must fire on
        // encoded length alone, before any decode attempt (which would
        // error differently).
        let encoded = "!".repeat(MAX_PROMPT_BYTES * 2 + 1);
        let err = parse_prompt_blocks(&[PromptBlock::Image {
            mime_type: "image/png".into(),
            data: encoded,
            filename: None,
        }])
        .expect_err("must early-reject");
        assert!(
            err.to_string().contains("prompt too large"),
            "early reject must be the size error, not a decode error: {err}"
        );
    }

    /// resource_link rendering counts toward the prompt byte cap.
    #[test]
    fn resource_link_over_budget_rejects_as_too_large() {
        let err = parse_prompt_blocks(&[PromptBlock::ResourceLink {
            uri: "x".repeat(MAX_PROMPT_BYTES + 1),
            name: "big".into(),
            title: None,
        }])
        .expect_err("must reject");
        let mapped = AcpRequestError::from_anyhow(&err);
        assert_eq!(mapped.code, -32602);
        assert!(err.to_string().contains("prompt too large"), "got: {err}");
    }

    /// Absolute-but-unresolvable cwd (nonexistent dir) maps to invalid
    /// params with a "cannot resolve" detail.
    #[test]
    fn validate_workspace_cwd_unresolvable_path_errors() {
        let err = validate_workspace_cwd("/nonexistent/borg/acp/test/path")
            .expect_err("nonexistent path must fail");
        let mapped = AcpRequestError::from_anyhow(&err);
        assert_eq!(mapped.code, -32602);
        assert!(
            err.to_string().contains("cannot resolve"),
            "detail must say the path was unresolvable: {err}"
        );
    }

    /// A writer failure mid-stream (IDE hung up) must fail the turn AND
    /// reset prompt_in_flight — the historical bug left the session locked.
    #[tokio::test]
    async fn stream_write_failure_fails_turn_and_resets_in_flight() {
        use std::sync::atomic::AtomicU64;

        /// Writer that fails every write — models the IDE side of the pipe
        /// going away while a turn is streaming.
        #[derive(Clone)]
        struct BrokenWriter;
        impl AsyncWrite for BrokenWriter {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                _buf: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "ide hung up",
                )))
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

        let daemon = fake_daemon();
        daemon.events.lock().await.insert(
            "sess-1".into(),
            vec![
                AgentEvent::TextDelta("streamed".into()),
                AgentEvent::TurnComplete,
            ],
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().to_path_buf();
        let state = AcpState {
            daemon,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            client_capabilities: Arc::new(StdMutex::new(ClientCapabilities::default())),
            onboarding_confirmed: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            daemon_failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shutdown: Arc::new(tokio::sync::Notify::new()),
        };
        state.sessions.lock().await.insert(
            "sess-1".into(),
            SessionRecord {
                workspace_cwd: workspace.clone(),
                workspace_root: workspace,
                runtime: SessionRuntime::default(),
            },
        );
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(BrokenWriter)));
        let callback_client = JsonRpcCallbackClient {
            writer: writer.clone(),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            capabilities: state.client_capabilities.clone(),
        };
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(2)),
            method: "session/prompt".to_string(),
            params: Some(json!({
                "sessionId": "sess-1",
                "prompt": [{"type": "text", "text": "go"}],
            })),
        };
        let result = handle_request(state.clone(), req, writer, callback_client).await;
        let err = result.expect_err("broken pipe must fail the turn");
        assert!(
            err.to_string().contains("session/update write failed"),
            "failure must name the write: {err}"
        );
        let sessions = state.sessions.lock().await;
        assert!(
            !sessions
                .get("sess-1")
                .expect("session present")
                .runtime
                .prompt_in_flight,
            "write failure MUST reset prompt_in_flight or the session locks forever"
        );
    }

    /// Poll the captured output until a frame with the given method appears.
    async fn wait_for_outbound_frame(output: &Arc<StdMutex<Vec<u8>>>, method: &str) -> Value {
        wait_for_predicate_frame(output, |f| f["method"] == json!(method)).await
    }

    /// Full fs-callback roundtrip through the live transport: the daemon
    /// side asks for a buffer read + write, the transport emits spec-shaped
    /// `fs/*` requests (which MUST carry `sessionId` — spec-required, and
    /// spec-strict IDEs reject the call without it), and inbound response
    /// envelopes are routed back to the awaiting caller by id.
    #[tokio::test]
    async fn fs_callbacks_roundtrip_with_session_id_over_transport() {
        use tokio::io::AsyncWriteExt;
        let mut daemon = fake_daemon();
        daemon.fs_roundtrip = true;
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_string_lossy().to_string();
        let (mut input, input_reader) = tokio::io::duplex(4096);
        let (writer, output) = CaptureWriter::new();
        let transport = tokio::spawn(run_transport(daemon, BufReader::new(input_reader), writer));

        input
            .write_all(
                format!(
                    concat!(
                        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/new\",\"params\":{{\"cwd\":\"{}\"}}}}\n",
                        "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"sess-1\",\"prompt\":[{{\"type\":\"text\",\"text\":\"go\"}}]}}}}\n",
                    ),
                    cwd
                )
                .as_bytes(),
            )
            .await
            .expect("write requests");

        let read_req = wait_for_outbound_frame(&output, "fs/read_text_file").await;
        assert_eq!(
            read_req["params"]["sessionId"],
            json!("sess-1"),
            "fs/read_text_file MUST carry the sessionId: {read_req}"
        );
        assert_eq!(read_req["params"]["path"], json!("buffer.txt"));
        let read_id = read_req["id"].clone();
        input
            .write_all(
                format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{read_id},\"result\":{{\"content\":\"BUF\"}}}}\n"
                )
                .as_bytes(),
            )
            .await
            .expect("write read response");

        let write_req = wait_for_outbound_frame(&output, "fs/write_text_file").await;
        assert_eq!(
            write_req["params"]["sessionId"],
            json!("sess-1"),
            "fs/write_text_file MUST carry the sessionId: {write_req}"
        );
        assert_eq!(write_req["params"]["path"], json!("out.txt"));
        assert_eq!(write_req["params"]["content"], json!("data"));
        let write_id = write_req["id"].clone();
        input
            .write_all(
                format!("{{\"jsonrpc\":\"2.0\",\"id\":{write_id},\"result\":{{}}}}\n").as_bytes(),
            )
            .await
            .expect("write write response");

        // EOF ends the transport once the turn resolves.
        drop(input);
        let exit = tokio::time::timeout(std::time::Duration::from_secs(5), transport)
            .await
            .expect("transport must exit after EOF")
            .expect("join")
            .expect("run_transport");
        assert_eq!(exit, 0);

        let frames = parse_lines(output.lock().unwrap().clone());
        // Match responses only — outbound agent→client fs requests use their
        // own id counter and can collide numerically with client request ids
        // (JSON-RPC id spaces are per-direction).
        let prompt_resp = frames
            .iter()
            .find(|f| f["id"] == json!(2) && f.get("method").is_none())
            .expect("prompt response");
        assert_eq!(prompt_resp["result"]["stopReason"], json!("end_turn"));
        assert!(
            frames.iter().any(|f| {
                f["params"]["update"]["sessionUpdate"] == json!("agent_message_chunk")
                    && f["params"]["update"]["content"]["text"] == json!("FS:BUF")
            }),
            "the buffer content returned by the IDE must reach the turn: {frames:?}"
        );
    }

    /// An fs response carrying neither `result` nor `error` is a protocol
    /// violation; the awaiting caller must fail with the envelope error
    /// rather than hanging on a half-answered request.
    #[tokio::test]
    async fn fs_callback_response_without_result_or_error_fails_cleanly() {
        use tokio::io::AsyncWriteExt;
        let mut daemon = fake_daemon();
        daemon.fs_roundtrip = true;
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_string_lossy().to_string();
        let (mut input, input_reader) = tokio::io::duplex(4096);
        let (writer, output) = CaptureWriter::new();
        let transport = tokio::spawn(run_transport(daemon, BufReader::new(input_reader), writer));
        input
            .write_all(
                format!(
                    concat!(
                        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/new\",\"params\":{{\"cwd\":\"{}\"}}}}\n",
                        "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"sess-1\",\"prompt\":[{{\"type\":\"text\",\"text\":\"go\"}}]}}}}\n",
                    ),
                    cwd
                )
                .as_bytes(),
            )
            .await
            .expect("write requests");
        let read_req = wait_for_outbound_frame(&output, "fs/read_text_file").await;
        let read_id = read_req["id"].clone();
        input
            .write_all(format!("{{\"jsonrpc\":\"2.0\",\"id\":{read_id}}}\n").as_bytes())
            .await
            .expect("write half-empty response");
        drop(input);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), transport)
            .await
            .expect("transport must exit")
            .expect("join");
        let frames = parse_lines(output.lock().unwrap().clone());
        let prompt_resp = frames
            .iter()
            .find(|f| f["id"] == json!(2) && f.get("method").is_none())
            .expect("prompt response");
        assert!(
            prompt_resp["error"]["data"]
                .as_str()
                .is_some_and(|s| s.contains("invalid ACP callback response envelope")),
            "half-empty envelope must fail with the envelope error: {prompt_resp}"
        );
    }

    /// The IDE answering an fs request with an error envelope must fail the
    /// awaiting caller with that message — never silently fall back or hang.
    #[tokio::test]
    async fn fs_callback_error_envelope_fails_the_turn_with_client_message() {
        use tokio::io::AsyncWriteExt;
        let mut daemon = fake_daemon();
        daemon.fs_roundtrip = true;
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_string_lossy().to_string();
        let (mut input, input_reader) = tokio::io::duplex(4096);
        let (writer, output) = CaptureWriter::new();
        let transport = tokio::spawn(run_transport(daemon, BufReader::new(input_reader), writer));

        input
            .write_all(
                format!(
                    concat!(
                        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/new\",\"params\":{{\"cwd\":\"{}\"}}}}\n",
                        "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"sess-1\",\"prompt\":[{{\"type\":\"text\",\"text\":\"go\"}}]}}}}\n",
                    ),
                    cwd
                )
                .as_bytes(),
            )
            .await
            .expect("write requests");

        let read_req = wait_for_outbound_frame(&output, "fs/read_text_file").await;
        let read_id = read_req["id"].clone();
        input
            .write_all(
                format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{read_id},\"error\":{{\"code\":-32000,\"message\":\"file not open in editor\"}}}}\n"
                )
                .as_bytes(),
            )
            .await
            .expect("write error response");

        drop(input);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), transport)
            .await
            .expect("transport must exit")
            .expect("join");

        let frames = parse_lines(output.lock().unwrap().clone());
        let prompt_resp = frames
            .iter()
            .find(|f| f["id"] == json!(2) && f.get("method").is_none())
            .expect("prompt response");
        assert_eq!(
            prompt_resp["error"]["code"],
            json!(-32603),
            "fs failure must fail the turn: {prompt_resp}"
        );
        assert!(
            prompt_resp["error"]["data"]
                .as_str()
                .is_some_and(|s| s.contains("file not open in editor")),
            "the IDE's error message must round-trip to the turn failure: {prompt_resp}"
        );
    }

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
        // Every source file of the ACP module — a new submodule must be
        // added here or the guard silently stops covering it.
        let code = [
            include_str!("mod.rs"),
            include_str!("transport.rs"),
            include_str!("handlers.rs"),
            include_str!("translate.rs"),
        ]
        .iter()
        .map(|src| strip_tests_and_comments(src))
        .collect::<Vec<_>>()
        .join("\n");
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
                "crates/interface/src/commands/acp/ contains forbidden output \
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

    /// Writer that fails every write — models the IDE side of the pipe
    /// going away mid-turn.
    #[derive(Clone)]
    struct FailingWriter;
    impl AsyncWrite for FailingWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "ide hung up",
            )))
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

    fn callback_client_over(writer: SharedWriter) -> JsonRpcCallbackClient {
        JsonRpcCallbackClient {
            writer,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            capabilities: Arc::new(StdMutex::new(ClientCapabilities::default())),
        }
    }

    /// A write failure on an outbound fs callback must error the call AND
    /// reap the pending-response entry — no reply will ever arrive for a
    /// request the client never received, so a leftover entry leaks per
    /// failed callback for the life of the process.
    #[tokio::test]
    async fn fs_callback_write_failure_does_not_leak_pending_entry() {
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(FailingWriter)));
        let client = callback_client_over(writer);
        let pending = client.pending.clone();
        let fs = SessionFsClient::new(client, "sess-1".into());
        let err = fs
            .read_text_file("f.txt")
            .await
            .expect_err("broken pipe must fail the callback");
        assert!(
            err.to_string().contains("ide hung up"),
            "error must surface the io failure: {err}"
        );
        assert!(
            pending.lock().await.is_empty(),
            "failed write must remove its pending entry"
        );
    }

    /// An IDE that accepts the fs request but never answers must not wedge
    /// the turn forever: the callback times out (virtual clock — paused
    /// runtime auto-advances) and reaps its pending entry.
    #[tokio::test(start_paused = true)]
    async fn fs_callback_times_out_when_ide_never_answers() {
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(tokio::io::sink())));
        let client = callback_client_over(writer);
        let pending = client.pending.clone();
        let fs = SessionFsClient::new(client, "sess-1".into());
        let err = fs
            .read_text_file("f.txt")
            .await
            .expect_err("unanswered callback must time out");
        assert!(
            err.to_string().contains("did not answer"),
            "error must name the timeout: {err}"
        );
        assert!(
            pending.lock().await.is_empty(),
            "timed-out callback must remove its pending entry"
        );
    }

    /// `session/load` for a session that is already tracked (e.g. an IDE
    /// re-loading while a prompt streams) must NOT reset the runtime flags:
    /// clobbering `prompt_in_flight` would bypass the per-session busy gate
    /// and drop a pending cancel.
    #[tokio::test]
    async fn session_load_preserves_in_flight_runtime_flags() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().to_path_buf();
        let daemon = fake_daemon();
        let state = AcpState {
            daemon,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            client_capabilities: Arc::new(StdMutex::new(ClientCapabilities::default())),
            onboarding_confirmed: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            daemon_failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shutdown: Arc::new(tokio::sync::Notify::new()),
        };
        state.sessions.lock().await.insert(
            "sess-1".into(),
            SessionRecord {
                workspace_cwd: workspace.clone(),
                workspace_root: workspace.clone(),
                runtime: SessionRuntime {
                    prompt_in_flight: true,
                    cancel_requested: true,
                },
            },
        );
        let (writer_impl, _output) = CaptureWriter::new();
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(writer_impl)));
        let callback_client = callback_client_over(writer.clone());
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(5)),
            method: "session/load".to_string(),
            params: Some(json!({
                "sessionId": "sess-1",
                "cwd": workspace.to_string_lossy(),
            })),
        };
        handle_request(state.clone(), req, writer, callback_client)
            .await
            .expect("session/load must succeed");
        let sessions = state.sessions.lock().await;
        let runtime = &sessions.get("sess-1").expect("session tracked").runtime;
        assert!(
            runtime.prompt_in_flight,
            "session/load must not clear prompt_in_flight"
        );
        assert!(
            runtime.cancel_requested,
            "session/load must not clear cancel_requested"
        );
    }
}
