//! Test harness for ACP end-to-end tests.
//!
//! Two components:
//!
//! - [`BorgdFixture`] — boots the real `borgd` gRPC services in-process on a
//!   per-test Unix Domain Socket. The session factory is
//!   [`borgd::session::scripted::ScriptedFactory`], so every session runs
//!   under a [`ScriptedBackend`] whose per-turn behavior the test controls.
//!   The fixture also seeds `BORG_DATA_DIR` with `onboarded = "true"` so the
//!   ACP transport's `require_onboarding` check passes.
//!
//! - [`AcpClient`] — spawns `borg acp` as a real subprocess pinned to the
//!   fixture's UDS via `BORG_DAEMON_UDS` (with `BORG_ACP_NO_AUTOSPAWN=1` so
//!   the subprocess can't accidentally race-spawn a real daemon). A
//!   background task multiplexes the subprocess's stdout into three queues:
//!   responses (correlated by id), notifications, and FS callback requests
//!   that get answered against a test-injected [`FsMap`].
//!
//! ## Tempdir / serialization model
//!
//! `BORG_DATA_DIR` is a process-wide env var; setting it from one test
//! affects every concurrent test in the same binary. Following the same
//! pattern as `crates/borgd/tests/ingress_e2e_common.rs`, a single
//! `OnceLock<TempDir>` provides one shared data dir for the whole binary,
//! and a `tokio::sync::Mutex<()>` serialises tests so the shared SQLite
//! file is touched by only one fixture at a time. Per-test isolation
//! (workspace, UDS path) lives under per-fixture tempdirs.

#![allow(dead_code)]

use anyhow::{anyhow, Context, Result};
use borgd::cost_events::CostEventBus;
use borgd::grpc::capability::CapabilityRouter;
use borgd::grpc::{build_services, Services};
use borgd::session::scripted::{ScriptedBackend, ScriptedFactory};
use borgd::session::SessionRegistry;
use cortex::config::Config as CoreConfig;
use cortex::telemetry::BorgMetrics;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex, OnceLock, PoisonError};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
use tokio_stream::wrappers::UnixListenerStream;

// ── Shared data dir + serial mutex ───────────────────────────────────────

fn shared_data_dir() -> &'static Path {
    static SHARED: OnceLock<TempDir> = OnceLock::new();
    let dir = SHARED.get_or_init(|| {
        let td = tempfile::tempdir().expect("tempdir for BORG_DATA_DIR");
        std::env::set_var("BORG_DATA_DIR", td.path());
        // Initialise the DB once so subsequent fixtures can write to it
        // without racing migrations. Onboarding flag set so the ACP
        // transport's `require_onboarding` gate passes.
        // OnceLock::get_or_init is sync; the DB API is async post-turso.
        // `block_in_place` releases the worker thread so the inner block_on
        // can drive the migrations to completion without deadlocking the
        // multi-thread runtime this fixture runs on.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let db = cortex::db::Database::open()
                    .await
                    .expect("open shared test DB");
                db.set_setting("onboarded", "true")
                    .await
                    .expect("seed onboarded=true");
            })
        });
        td
    });
    dir.path()
}

fn serial_lock() -> Arc<AsyncMutex<()>> {
    static M: OnceLock<Arc<AsyncMutex<()>>> = OnceLock::new();
    M.get_or_init(|| Arc::new(AsyncMutex::new(()))).clone()
}

// ── BorgdFixture ─────────────────────────────────────────────────────────

/// One booted in-process daemon, ready for `borg acp` to talk to.
pub struct BorgdFixture {
    /// Per-test workspace dir; passed as `cwd` in `session/new`.
    pub workspace: TempDir,
    /// Per-test tempdir holding the UDS file. Kept alive for the fixture's
    /// lifetime so the socket file isn't deleted out from under the daemon.
    _socket_dir: TempDir,
    /// Resolved UDS path the subprocess connects to via `BORG_DAEMON_UDS`.
    pub uds_path: PathBuf,
    factory: Arc<ScriptedFactory>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    server_task: Option<tokio::task::JoinHandle<()>>,
    _serial_guard: tokio::sync::OwnedMutexGuard<()>,
}

impl BorgdFixture {
    /// Boot a fixture with one pre-registered scripted backend. The backend
    /// will be handed out on the first `Session.Open` (i.e. ACP `session/new`).
    pub async fn with_backend(backend: Arc<ScriptedBackend>) -> Self {
        Self::with_factory(ScriptedFactory::single(backend)).await
    }

    /// Boot a fixture from a pre-built [`ScriptedFactory`]. Use this for
    /// multi-session tests where the test wants to enqueue multiple
    /// backends.
    pub async fn with_factory(factory: Arc<ScriptedFactory>) -> Self {
        let serial_guard = serial_lock().lock_owned().await;
        let _ = shared_data_dir(); // initialise + seed onboarded=true
        let socket_dir = tempfile::tempdir().expect("uds tempdir");
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let uds_path = socket_dir.path().join("borgd.sock");
        let registry = SessionRegistry::new();
        let capability_router = Arc::new(CapabilityRouter::new());
        let core_config = CoreConfig::load_from_db().await.unwrap_or_default();
        let cost_event_bus = CostEventBus::new(&core_config, BorgMetrics::noop());
        let services = build_services(registry, factory.clone(), capability_router, cost_event_bus);

        let listener = borgd::daemon::bind_uds(&uds_path).expect("bind uds");
        let stream = UnixListenerStream::new(listener);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let server_task = tokio::spawn(spawn_server(services, stream, shutdown_rx));

        // Wait until the socket is connectable so the subprocess doesn't
        // race the listener.
        wait_for_socket(&uds_path, Duration::from_secs(2))
            .await
            .expect("borgd UDS came up");

        Self {
            workspace,
            _socket_dir: socket_dir,
            uds_path,
            factory,
            shutdown_tx: Some(shutdown_tx),
            server_task: Some(server_task),
            _serial_guard: serial_guard,
        }
    }

    /// Path to the per-test workspace tempdir. Use this as the `cwd` in
    /// `session/new` requests.
    pub fn workspace_path(&self) -> &Path {
        self.workspace.path()
    }

    /// Read access to the underlying scripted factory — primarily so tests
    /// can inspect the `ExecutionContext` the daemon passed to `open()`.
    pub fn factory(&self) -> &ScriptedFactory {
        &self.factory
    }

    /// Forcefully tear the daemon down. Sends the shutdown signal AND aborts
    /// the server task — the gRPC connection drops abruptly, simulating a
    /// real `borgd` crash. Used by lifecycle tests that pin "daemon-failed"
    /// recovery semantics. Returns once the listener task has been joined
    /// (or its abort observed).
    pub async fn shutdown_now(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.server_task.take() {
            handle.abort();
            let _ = handle.await;
        }
    }
}

impl Drop for BorgdFixture {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        // Detach the server task — the shutdown signal stops the listener;
        // the runtime will reap the task. We don't await here because Drop
        // can run from a sync context, and the test process is on its way
        // out anyway.
        if let Some(handle) = self.server_task.take() {
            handle.abort();
        }
    }
}

async fn spawn_server(
    services: Services,
    stream: UnixListenerStream,
    shutdown_rx: oneshot::Receiver<()>,
) {
    let result = tonic::transport::Server::builder()
        .add_service(services.status)
        .add_service(services.admin)
        .add_service(services.session)
        .add_service(services.capability)
        .add_service(services.telemetry)
        .serve_with_incoming_shutdown(stream, async move {
            let _ = shutdown_rx.await;
        })
        .await;
    if let Err(e) = result {
        // Test-only diagnostic; never reaches production code.
        eprintln!("test borgd server exited with error: {e:#}");
    }
}

async fn wait_for_socket(path: &Path, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if path.exists() && UnixStream::connect(path).await.is_ok() {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            return Err(anyhow!(
                "test borgd UDS at {} never became connectable",
                path.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

// ── FsMap ────────────────────────────────────────────────────────────────

/// Tiny in-memory filesystem the AcpClient uses to answer `fs/read_text_file`
/// and `fs/write_text_file` callbacks from the daemon. Reads pop from
/// `entries`; writes append to `writes`. Optionally a path can be primed to
/// return an explicit error from `read_errors` / `write_errors` so tests
/// exercise the no-silent-fallback rule.
#[derive(Default, Clone)]
pub struct FsMap {
    inner: Arc<StdMutex<FsMapInner>>,
}

#[derive(Default)]
struct FsMapInner {
    entries: HashMap<String, String>,
    read_errors: HashMap<String, String>,
    write_errors: HashMap<String, String>,
    writes: Vec<(String, String)>,
    reads_observed: Vec<String>,
}

impl FsMap {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn insert<K: Into<String>, V: Into<String>>(&self, path: K, content: V) -> &Self {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .insert(path.into(), content.into());
        self
    }
    pub fn fail_read<K: Into<String>, M: Into<String>>(&self, path: K, message: M) -> &Self {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .read_errors
            .insert(path.into(), message.into());
        self
    }
    pub fn fail_write<K: Into<String>, M: Into<String>>(&self, path: K, message: M) -> &Self {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write_errors
            .insert(path.into(), message.into());
        self
    }
    pub fn observed_reads(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reads_observed
            .clone()
    }
    pub fn observed_writes(&self) -> Vec<(String, String)> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .writes
            .clone()
    }
    fn handle_read(&self, path: &str) -> std::result::Result<String, String> {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.reads_observed.push(path.to_string());
        if let Some(err) = g.read_errors.get(path) {
            return Err(err.clone());
        }
        match g.entries.get(path) {
            Some(content) => Ok(content.clone()),
            None => Err(format!("FsMap: no entry for {path}")),
        }
    }
    fn handle_write(&self, path: &str, content: &str) -> std::result::Result<(), String> {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.writes.push((path.to_string(), content.to_string()));
        if let Some(err) = g.write_errors.get(path) {
            return Err(err.clone());
        }
        g.entries.insert(path.to_string(), content.to_string());
        Ok(())
    }
}

// ── AcpClient ────────────────────────────────────────────────────────────

/// Notification observed on the subprocess's stdout (i.e. a JSON-RPC message
/// with a `method` field and no `id`).
#[derive(Debug, Clone)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

/// Snapshot of the events streamed during one `session/prompt` turn.
#[derive(Debug, Clone, Default)]
pub struct PromptOutcome {
    pub stop_reason: Option<String>,
    pub error: Option<Value>,
    pub updates: Vec<Value>,
    pub session_info_updates: Vec<Value>,
}

impl PromptOutcome {
    pub fn updates_of_kind(&self, kind: &str) -> Vec<&Value> {
        self.updates
            .iter()
            .filter(|u| u["update"]["kind"] == json!(kind))
            .collect()
    }
    pub fn agent_message_text(&self) -> String {
        self.updates_of_kind("agent_message_chunk")
            .into_iter()
            .map(|u| u["update"]["text"].as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>()
            .join("")
    }
}

/// Spawned `borg acp` subprocess + multiplexer.
pub struct AcpClient {
    child: Option<Child>,
    /// Outgoing raw byte frames. The writer task appends each frame to
    /// stdin verbatim — no JSON re-encoding — so malformed-input tests can
    /// inject byte sequences that wouldn't survive a roundtrip through
    /// `serde_json`. Helpers like [`AcpClient::request`] serialize a JSON
    /// envelope and feed the frame through this same channel.
    stdin_tx: mpsc::Sender<Vec<u8>>,
    /// Pending request id → completion.
    pending: Arc<AsyncMutex<HashMap<i64, oneshot::Sender<Value>>>>,
    /// Notifications observed on stdout (tail-capped log).
    notifications: Arc<AsyncMutex<Vec<Notification>>>,
    /// Captured raw stdout lines (for stdout-cleanliness assertions).
    raw_stdout_lines: Arc<AsyncMutex<Vec<String>>>,
    /// Captured stderr lines (must remain empty in golden-path tests).
    raw_stderr_lines: Arc<AsyncMutex<Vec<String>>>,
    next_id: i64,
    /// FS map currently servicing daemon-initiated `fs/*` requests. Tests
    /// can reach in via [`AcpClient::fs_map`] to pre-load buffers.
    fs_map: FsMap,
    /// Subprocess's PID for diagnostics on failure.
    pid: u32,
}

impl AcpClient {
    /// Spawn `borg acp` against the given fixture and start the multiplexer.
    /// Inherits no env from the parent except the bare minimum needed
    /// (PATH so the subprocess can find dynamic libs; HOME so user-level
    /// configs that the subprocess may consult exist).
    pub async fn spawn(fixture: &BorgdFixture) -> Result<Self> {
        let exe = env!("CARGO_BIN_EXE_borg");
        let mut cmd = Command::new(exe);
        cmd.arg("acp")
            .env_clear()
            .env(
                "PATH",
                std::env::var_os("PATH").unwrap_or_else(|| std::ffi::OsString::from("/usr/bin")),
            )
            .env(
                "HOME",
                std::env::var_os("HOME").unwrap_or_else(|| std::ffi::OsString::from("/tmp")),
            )
            .env("BORG_DAEMON_UDS", &fixture.uds_path)
            .env("BORG_DATA_DIR", shared_data_dir())
            .env("BORG_ACP_NO_AUTOSPAWN", "1")
            .env(
                "RUST_LOG",
                std::env::var("BORG_ACP_TEST_LOG").unwrap_or_else(|_| "off".into()),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().context("spawn `borg acp`")?;
        let pid = child.id().unwrap_or(0);

        let stdin = child.stdin.take().context("missing stdin")?;
        let stdout = child.stdout.take().context("missing stdout")?;
        let stderr = child.stderr.take().context("missing stderr")?;

        let pending: Arc<AsyncMutex<HashMap<i64, oneshot::Sender<Value>>>> =
            Arc::new(AsyncMutex::new(HashMap::new()));
        let notifications = Arc::new(AsyncMutex::new(Vec::<Notification>::new()));
        let raw_stdout_lines = Arc::new(AsyncMutex::new(Vec::<String>::new()));
        let raw_stderr_lines = Arc::new(AsyncMutex::new(Vec::<String>::new()));
        let fs_map = FsMap::new();

        // ── stdin writer task ──
        let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(64);
        tokio::spawn({
            let mut stdin = stdin;
            async move {
                while let Some(bytes) = stdin_rx.recv().await {
                    if stdin.write_all(&bytes).await.is_err() {
                        break;
                    }
                    if stdin.flush().await.is_err() {
                        break;
                    }
                }
            }
        });

        // ── stdout reader / dispatcher task ──
        tokio::spawn({
            let pending = pending.clone();
            let notifications = notifications.clone();
            let raw_stdout_lines = raw_stdout_lines.clone();
            let stdin_tx = stdin_tx.clone();
            let fs_map = fs_map.clone();
            async move {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break,
                        Ok(_) => {
                            let trimmed = line.trim_end_matches('\n').to_string();
                            raw_stdout_lines.lock().await.push(trimmed.clone());
                            let value: Value = match serde_json::from_str(&trimmed) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            dispatch_stdout_message(
                                value,
                                &pending,
                                &notifications,
                                &stdin_tx,
                                &fs_map,
                            )
                            .await;
                        }
                        Err(_) => break,
                    }
                }
            }
        });

        // ── stderr reader task ──
        tokio::spawn({
            let raw_stderr_lines = raw_stderr_lines.clone();
            async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break,
                        Ok(_) => {
                            let trimmed = line.trim_end_matches('\n').to_string();
                            raw_stderr_lines.lock().await.push(trimmed);
                        }
                        Err(_) => break,
                    }
                }
            }
        });

        Ok(Self {
            child: Some(child),
            stdin_tx,
            pending,
            notifications,
            raw_stdout_lines,
            raw_stderr_lines,
            next_id: 1,
            fs_map,
            pid,
        })
    }

    pub fn fs_map(&self) -> &FsMap {
        &self.fs_map
    }

    /// Clone the channel half feeding the subprocess's stdin. Lets a
    /// concurrent task push raw bytes (e.g. a cancel notification) while
    /// the test holds `&mut self` for an in-flight `run_prompt` call.
    pub fn stdin_sender(&self) -> mpsc::Sender<Vec<u8>> {
        self.stdin_tx.clone()
    }

    pub async fn raw_stdout_lines(&self) -> Vec<String> {
        self.raw_stdout_lines.lock().await.clone()
    }

    pub async fn raw_stderr_lines(&self) -> Vec<String> {
        self.raw_stderr_lines.lock().await.clone()
    }

    /// Inject raw bytes verbatim onto the subprocess's stdin. Used by the
    /// malformed-JSON negative test cases. Caller is responsible for adding
    /// any trailing `\n` needed for line-delimited framing.
    pub async fn inject_raw_bytes(&self, bytes: &[u8]) -> Result<()> {
        self.stdin_tx
            .send(bytes.to_vec())
            .await
            .map_err(|_| anyhow!("subprocess stdin closed"))
    }

    fn allocate_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Send a JSON-RPC request and await the matching response. Returns the
    /// full response envelope (caller asserts on `result` / `error`).
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.allocate_id();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut bytes = serde_json::to_vec(&msg).context("serializing request")?;
        bytes.push(b'\n');
        self.stdin_tx
            .send(bytes)
            .await
            .map_err(|_| anyhow!("subprocess stdin closed"))?;
        // 5s was tight under heavy CI/parallel-build load — `initialize`
        // includes borgd's first DB open + skill catalog scan, which can
        // exceed 5s when the build is still hot. 30s is well above any
        // real handshake while still catching genuine deadlocks.
        let envelope = tokio::time::timeout(Duration::from_secs(30), rx)
            .await
            .map_err(|_| anyhow!("timeout waiting for response to {method} (id={id})"))??;
        Ok(envelope)
    }

    /// Send a JSON-RPC notification (no `id`, no response expected).
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut bytes = serde_json::to_vec(&msg).context("serializing notification")?;
        bytes.push(b'\n');
        self.stdin_tx
            .send(bytes)
            .await
            .map_err(|_| anyhow!("subprocess stdin closed"))
    }

    /// Send a `session/prompt` request and capture every notification that
    /// arrives until the response. Returns the consolidated [`PromptOutcome`].
    pub async fn run_prompt(&mut self, session_id: &str, text: &str) -> Result<PromptOutcome> {
        let nots_before = self.notifications.lock().await.len();
        let envelope = self
            .request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": text}],
                }),
            )
            .await?;
        let nots = self.notifications.lock().await;
        let captured: Vec<&Notification> = nots[nots_before..].iter().collect();
        let mut outcome = PromptOutcome::default();
        for n in captured {
            match n.method.as_str() {
                "session/update" => outcome.updates.push(n.params.clone()),
                "session_info_update" => outcome.session_info_updates.push(n.params.clone()),
                _ => {}
            }
        }
        if let Some(result) = envelope.get("result") {
            outcome.stop_reason = result["stopReason"].as_str().map(str::to_string);
        } else if let Some(err) = envelope.get("error") {
            outcome.error = Some(err.clone());
        }
        Ok(outcome)
    }

    /// Block until the subprocess exits, returning its exit status.
    pub async fn wait(&mut self) -> Result<std::process::ExitStatus> {
        let mut child = self
            .child
            .take()
            .ok_or_else(|| anyhow!("AcpClient already waited"))?;
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await??;
        Ok(status)
    }

    /// Diagnostic helper: PID of the spawned subprocess. Useful when a test
    /// fails and the captured stderr is empty.
    pub fn pid(&self) -> u32 {
        self.pid
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
    }
}

// Dispatch one parsed JSON value from the subprocess's stdout into the right
// queue. Splits responses (id + result/error), notifications (method without
// id), and FS callback requests (method WITH id, where the daemon is asking
// the IDE to read/write a buffer).
async fn dispatch_stdout_message(
    value: Value,
    pending: &Arc<AsyncMutex<HashMap<i64, oneshot::Sender<Value>>>>,
    notifications: &Arc<AsyncMutex<Vec<Notification>>>,
    stdin_tx: &mpsc::Sender<Vec<u8>>,
    fs_map: &FsMap,
) {
    if value.get("method").is_some() && value.get("id").is_some() {
        // FS callback request from daemon → IDE.
        let id = value["id"].clone();
        let method = value["method"].as_str().unwrap_or("").to_string();
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        let response = handle_fs_callback(&method, &params, fs_map);
        let envelope = match response {
            Ok(result) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
            }),
            Err(message) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32000, "message": message},
            }),
        };
        if let Ok(mut bytes) = serde_json::to_vec(&envelope) {
            bytes.push(b'\n');
            let _ = stdin_tx.send(bytes).await;
        }
    } else if let Some(method) = value.get("method").and_then(Value::as_str) {
        notifications.lock().await.push(Notification {
            method: method.to_string(),
            params: value.get("params").cloned().unwrap_or(Value::Null),
        });
    } else if let Some(id_val) = value.get("id") {
        if let Some(id) = id_val.as_i64() {
            let mut g = pending.lock().await;
            if let Some(tx) = g.remove(&id) {
                let _ = tx.send(value);
            }
        }
    }
}

fn handle_fs_callback(
    method: &str,
    params: &Value,
    fs_map: &FsMap,
) -> std::result::Result<Value, String> {
    match method {
        "fs/read_text_file" => {
            let path = params["path"]
                .as_str()
                .ok_or_else(|| "missing path".to_string())?;
            fs_map
                .handle_read(path)
                .map(|content| json!({"content": content}))
        }
        "fs/write_text_file" => {
            let path = params["path"]
                .as_str()
                .ok_or_else(|| "missing path".to_string())?;
            let content = params["content"].as_str().unwrap_or("");
            // The subprocess parses the envelope as `Option<Value>` for
            // `result`; serde maps JSON `null` → `None`, which then
            // collides with the "no result and no error" branch and
            // produces "invalid ACP callback response envelope". Return
            // an empty object so `Some(Value::Object({}))` survives.
            fs_map.handle_write(path, content).map(|_| json!({}))
        }
        other => Err(format!("unsupported callback method: {other}")),
    }
}

// ── Convenience JSON builders ────────────────────────────────────────────

/// Build the standard `initialize` params payload, advertising both fs
/// capabilities (the common ACP IDE shape).
pub fn initialize_params_with_fs() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": {"readTextFile": true, "writeTextFile": true}
        }
    })
}

/// `initialize` params with no fs capabilities advertised.
pub fn initialize_params_no_fs() -> Value {
    json!({"protocolVersion": 1})
}

/// Default scripted-turn convenience — emits a single text delta + TurnComplete.
pub fn simple_text_turn(text: impl Into<String>) -> Vec<cortex::agent::AgentEvent> {
    vec![
        cortex::agent::AgentEvent::TextDelta(text.into()),
        cortex::agent::AgentEvent::TurnComplete,
    ]
}
