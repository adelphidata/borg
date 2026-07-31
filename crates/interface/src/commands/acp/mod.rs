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
        let event = AgentEvent::ToolOutputDelta {
            tool_call_id: "c1".into(),
            name: "run_shell".into(),
            delta: "before <internal>secret-plan</internal> after".into(),
            is_stderr: false,
        };
        let update = event_to_update(&event, "sess-1").expect("update");
        assert_eq!(
            update["update"]["content"][0]["content"]["text"],
            "before  after"
        );
        assert_eq!(update["update"]["sessionUpdate"], "tool_call_update");
        assert_eq!(update["update"]["_meta"]["borg"]["stream"], "stdout");

        let stderr_event = AgentEvent::ToolOutputDelta {
            tool_call_id: "c1".into(),
            name: "run_shell".into(),
            delta: "<internal>x</internal>err".into(),
            is_stderr: true,
        };
        let stderr_update = event_to_update(&stderr_event, "sess-1").expect("update");
        assert_eq!(stderr_update["update"]["content"][0]["content"]["text"], "err");
        assert_eq!(stderr_update["update"]["_meta"]["borg"]["stream"], "stderr");
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
            lines[0]["params"]["update"]["_meta"]["borg"]["model"],
            "test-model",
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
        assert_eq!(prompt.text, "look at\n[main.rs](file:///ws/src/main.rs)");
        assert!(prompt.images.is_empty());
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
        assert_eq!(prompt.text, "[Project readme](file:///ws/README.md)");
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
}
