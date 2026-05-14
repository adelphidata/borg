//! Async JSON-RPC client for the `codex app-server` subprocess.
//!
//! Lifecycle: `spawn` boots the child process and a reader task. The caller
//! drives request/response pairs via [`CodexAppServerClient::request`] and pops
//! streamed events via [`CodexAppServerClient::take_notification`] /
//! [`CodexAppServerClient::take_server_request`]. `close` terminates the child
//! cooperatively; `Drop` is a backstop that kills if `close` was skipped.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{debug, warn};

use super::protocol::{
    encode_error_response, encode_notification, encode_request, encode_response, parse_frame,
    InboundFrame, RequestId, RpcError, METHOD_INITIALIZE, METHOD_INITIALIZED,
};

/// Minimum codex CLI version we target. Bumped in lockstep with the wire-protocol
/// fields parsed here.
pub const MIN_CODEX_VERSION: (u32, u32, u32) = (0, 125, 0);

/// Default timeout for the `initialize` handshake.
pub const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);

/// Buffer size for the notification queue. Codex bursts items during a turn;
/// 1024 absorbs a long turn comfortably without back-pressuring the reader.
const NOTIFICATION_BUFFER: usize = 1024;

/// Buffer size for server-initiated requests (approval prompts). Small — codex
/// rarely emits more than one at a time.
const SERVER_REQUEST_BUFFER: usize = 64;

/// Cap on retained stderr lines. Keeps memory bounded while preserving enough
/// tail for diagnostic surfaces.
const STDERR_TAIL_CAP: usize = 500;

/// Lock a `std::sync::Mutex`, recovering from poisoning by taking the inner value.
///
/// Poisoning would only occur if a previous holder panicked while holding the
/// lock — we'd rather continue with a possibly-corrupt state than abort the
/// agent, and the panic itself has already been reported via the panic handler.
#[inline]
fn lock_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Options for spawning the codex `app-server` subprocess.
///
/// **Stage 5 follow-up:** the config layer that populates this struct from
/// operator settings MUST validate `env` and `extra_args` against an allowlist
/// (`CODEX_HOME`, `RUST_LOG`, codex-specific OAuth keys). Forbid `LD_PRELOAD`,
/// `DYLD_*`, and `PATH` overrides — a misconfigured channel binding shouldn't
/// be able to inject library shims into the child. Tracking this on the struct
/// itself rather than in a separate validator type so the constraint is
/// visible at every call site.
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    /// Path or name of the codex CLI binary (default `"codex"`).
    pub codex_bin: PathBuf,
    /// Optional `CODEX_HOME` override. When `None`, codex uses its default
    /// (`~/.codex`).
    pub codex_home: Option<PathBuf>,
    /// Extra arguments passed after `app-server` (advanced; usually empty).
    pub extra_args: Vec<String>,
    /// Env vars to overlay on top of the inherited environment.
    pub env: Vec<(String, String)>,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            codex_bin: PathBuf::from("codex"),
            codex_home: None,
            extra_args: Vec::new(),
            env: Vec::new(),
        }
    }
}

/// Errors returned by client operations.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The peer returned a JSON-RPC error object.
    #[error("codex app-server error {code}: {message}")]
    Rpc {
        /// JSON-RPC error code.
        code: i64,
        /// Human-readable message.
        message: String,
        /// Optional structured payload.
        data: Option<serde_json::Value>,
    },
    /// Request didn't complete within the configured timeout.
    #[error("codex app-server method {method:?} timed out after {timeout_secs}s")]
    Timeout {
        /// Method that timed out.
        method: String,
        /// Timeout duration in seconds.
        timeout_secs: f64,
    },
    /// Subprocess exited or stdin closed before the response arrived.
    #[error("codex app-server is no longer running: {0}")]
    Disconnected(String),
    /// Lower-level IO or protocol error.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<RpcError> for ClientError {
    fn from(e: RpcError) -> Self {
        ClientError::Rpc {
            code: e.code,
            message: e.message,
            data: e.data,
        }
    }
}

/// One-shot reply channel for an in-flight request.
type ReplyTx = oneshot::Sender<Result<serde_json::Value, RpcError>>;

/// Shared map of in-flight requests keyed by their JSON-RPC id.
///
/// Invariant: only locked from `read_stdout_loop` (reader task) and the
/// `request()` path. Both lock briefly without holding the guard across any
/// `.await`. Do not introduce a third lock site without auditing for deadlock.
type PendingMap = Arc<Mutex<HashMap<RequestId, ReplyTx>>>;

/// Async JSON-RPC client over the codex `app-server` subprocess.
pub struct CodexAppServerClient {
    next_id: AtomicU64,
    /// `None` after `close()` drops the writer half so the child sees EOF.
    stdin: Arc<tokio::sync::Mutex<Option<ChildStdin>>>,
    pending: PendingMap,
    /// Set to `true` once `read_stdout_loop` has exited and drained `pending`.
    /// New requests inserted after this flag is set are orphaned (the reader
    /// won't drain them), so `request()` post-checks the flag and removes its
    /// own entry to return `Disconnected` instead of letting the caller hang
    /// on its `oneshot::Receiver` until the timeout fires.
    reader_done: Arc<AtomicBool>,
    notifications_rx: tokio::sync::Mutex<mpsc::Receiver<Notification>>,
    server_requests_rx: tokio::sync::Mutex<mpsc::Receiver<ServerRequest>>,
    stderr_tail: Arc<Mutex<Vec<String>>>,
    child: Arc<tokio::sync::Mutex<Option<Child>>>,
    reader_handle: Mutex<Option<JoinHandle<()>>>,
    stderr_handle: Mutex<Option<JoinHandle<()>>>,
    initialized: Mutex<bool>,
    closed: Mutex<bool>,
}

/// A streamed notification from the server.
#[derive(Debug, Clone)]
pub struct Notification {
    /// JSON-RPC method, e.g. `item/agentMessage` or `turn/completed`.
    pub method: String,
    /// Method-specific params payload.
    pub params: serde_json::Value,
}

/// A server-initiated JSON-RPC request (e.g. approval prompt).
#[derive(Debug, Clone)]
pub struct ServerRequest {
    /// Request id; echo back in the response.
    pub id: RequestId,
    /// Method the server is invoking.
    pub method: String,
    /// Method-specific params.
    pub params: serde_json::Value,
}

impl CodexAppServerClient {
    /// Spawn `codex app-server` and start the reader tasks.
    ///
    /// Returns once the subprocess is launched; the `initialize` handshake is a
    /// separate step (see [`Self::initialize`]).
    pub async fn spawn(opts: SpawnOptions) -> Result<Self> {
        let mut cmd = Command::new(&opts.codex_bin);
        cmd.arg("app-server");
        for arg in &opts.extra_args {
            cmd.arg(arg);
        }
        if let Some(home) = &opts.codex_home {
            cmd.env("CODEX_HOME", home);
        }
        for (k, v) in &opts.env {
            cmd.env(k, v);
        }
        // Codex emits tracing to stderr; keep it quiet by default. Operators can
        // override by setting RUST_LOG before launching borg.
        if std::env::var_os("RUST_LOG").is_none() {
            cmd.env("RUST_LOG", "warn");
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // We manage shutdown explicitly via close(); kill_on_drop is the
            // backstop for panicking-path cleanup only.
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning {:?} app-server", opts.codex_bin))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("codex app-server stdin not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("codex app-server stdout not piped"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("codex app-server stderr not piped"))?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let stderr_tail = Arc::new(Mutex::new(Vec::<String>::with_capacity(64)));
        let reader_done = Arc::new(AtomicBool::new(false));
        let (notif_tx, notifications_rx) = mpsc::channel::<Notification>(NOTIFICATION_BUFFER);
        let (srv_tx, server_requests_rx) = mpsc::channel::<ServerRequest>(SERVER_REQUEST_BUFFER);

        let reader_pending = Arc::clone(&pending);
        let reader_stderr = Arc::clone(&stderr_tail);
        let reader_done_signal = Arc::clone(&reader_done);
        let reader_handle = tokio::spawn(read_stdout_loop(
            stdout,
            reader_pending,
            notif_tx,
            srv_tx,
            reader_stderr,
            reader_done_signal,
        ));

        let stderr_handle = tokio::spawn(read_stderr_loop(stderr, Arc::clone(&stderr_tail)));

        Ok(Self {
            next_id: AtomicU64::new(1),
            stdin: Arc::new(tokio::sync::Mutex::new(Some(stdin))),
            pending,
            reader_done,
            notifications_rx: tokio::sync::Mutex::new(notifications_rx),
            server_requests_rx: tokio::sync::Mutex::new(server_requests_rx),
            stderr_tail,
            child: Arc::new(tokio::sync::Mutex::new(Some(child))),
            reader_handle: Mutex::new(Some(reader_handle)),
            stderr_handle: Mutex::new(Some(stderr_handle)),
            initialized: Mutex::new(false),
            closed: Mutex::new(false),
        })
    }

    /// Send the `initialize` request and the matching `initialized` notification.
    ///
    /// Codex's app-server rejects re-initialization, so this is gated by a
    /// flag that is set **only on success** — a transient failure (timeout /
    /// disconnect) on the first attempt does not block a retry.
    pub async fn initialize(
        &self,
        client_name: &str,
        client_title: &str,
        client_version: &str,
    ) -> Result<serde_json::Value, ClientError> {
        if *lock_recover(&self.initialized) {
            return Err(ClientError::Other(anyhow!(
                "codex app-server already initialized"
            )));
        }
        let params = serde_json::json!({
            "clientInfo": {
                "name": client_name,
                "title": client_title,
                "version": client_version,
            },
            "capabilities": {},
        });
        let result = self
            .request(METHOD_INITIALIZE, params, INITIALIZE_TIMEOUT)
            .await?;
        // Per JSON-RPC, the matching notification fires *after* we consume the response.
        self.notify(METHOD_INITIALIZED, serde_json::json!({}))
            .await?;
        // Only flip the flag after both wire ops succeed. A transient failure
        // above keeps the client retryable.
        *lock_recover(&self.initialized) = true;
        Ok(result)
    }

    /// Send a JSON-RPC request and await the response.
    pub async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
        request_timeout: Duration,
    ) -> Result<serde_json::Value, ClientError> {
        let id = RequestId::from_u64(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        lock_recover(&self.pending).insert(id.clone(), tx);

        // EOF race-close: if the reader has already exited and drained `pending`
        // (or is about to), our just-inserted entry is orphaned. Post-check the
        // flag with Acquire ordering against the reader's Release store, and
        // take our own entry back so the caller surfaces `Disconnected` fast
        // instead of waiting out the full timeout. Reader's drain may already
        // have removed it — that's fine, our take() returns None and we still
        // return the error (rx will have been completed with an error in that
        // case, but we never await it).
        if self.reader_done.load(Ordering::Acquire) {
            lock_recover(&self.pending).remove(&id);
            return Err(ClientError::Disconnected(
                "reader task already exited".into(),
            ));
        }

        let wire = encode_request(&id, method, &params);
        if let Err(e) = self.write_line(&wire).await {
            lock_recover(&self.pending).remove(&id);
            return Err(ClientError::Disconnected(e.to_string()));
        }

        match timeout(request_timeout, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(rpc_err))) => Err(rpc_err.into()),
            Ok(Err(_)) => {
                lock_recover(&self.pending).remove(&id);
                Err(ClientError::Disconnected(
                    "reader task dropped before reply".into(),
                ))
            }
            Err(_) => {
                lock_recover(&self.pending).remove(&id);
                Err(ClientError::Timeout {
                    method: method.to_string(),
                    timeout_secs: request_timeout.as_secs_f64(),
                })
            }
        }
    }

    /// Send a JSON-RPC notification (no id, no response).
    pub async fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), ClientError> {
        let wire = encode_notification(method, &params);
        self.write_line(&wire)
            .await
            .map_err(|e| ClientError::Disconnected(e.to_string()))
    }

    /// Reply to a server-initiated request with a success payload.
    pub async fn respond(
        &self,
        id: &RequestId,
        result: serde_json::Value,
    ) -> Result<(), ClientError> {
        let wire = encode_response(id, &result);
        self.write_line(&wire)
            .await
            .map_err(|e| ClientError::Disconnected(e.to_string()))
    }

    /// Reply to a server-initiated request with a JSON-RPC error.
    pub async fn respond_error(
        &self,
        id: &RequestId,
        code: i64,
        message: &str,
    ) -> Result<(), ClientError> {
        let wire = encode_error_response(id, code, message);
        self.write_line(&wire)
            .await
            .map_err(|e| ClientError::Disconnected(e.to_string()))
    }

    /// Await the next streamed notification, or `None` if the reader has shut
    /// down (subprocess exited / stdout closed).
    pub async fn take_notification(&self) -> Option<Notification> {
        self.notifications_rx.lock().await.recv().await
    }

    /// Non-blocking pop of the next notification.
    pub fn try_take_notification(&self) -> Option<Notification> {
        self.notifications_rx
            .try_lock()
            .ok()
            .and_then(|mut rx| rx.try_recv().ok())
    }

    /// Await the next server-initiated request (approval prompts etc.).
    pub async fn take_server_request(&self) -> Option<ServerRequest> {
        self.server_requests_rx.lock().await.recv().await
    }

    /// Last N lines of subprocess stderr — diagnostic surface only.
    pub fn stderr_tail(&self, n: usize) -> Vec<String> {
        let tail = lock_recover(&self.stderr_tail);
        let start = tail.len().saturating_sub(n);
        tail[start..].to_vec()
    }

    /// Cooperative shutdown: drop stdin so the child sees EOF, wait for the
    /// child, then escalate to kill on `grace` timeout. Errors on the kill path
    /// are surfaced via `tracing::warn!` — a stuck child is operationally
    /// meaningful and we don't want to swallow it silently.
    pub async fn close(&self, grace: Duration) -> Result<()> {
        {
            let mut closed = lock_recover(&self.closed);
            if *closed {
                return Ok(());
            }
            *closed = true;
        }
        // Drop stdin to signal EOF. `shutdown().await` half-closes the writer,
        // but the `ChildStdin` fd persists until the value is dropped — codex
        // app-server only exits when the fd actually closes, so we `take()` the
        // Option to drop it.
        {
            let mut stdin = self.stdin.lock().await;
            if let Some(mut s) = stdin.take() {
                if let Err(e) = s.shutdown().await {
                    warn!(error = %e, "codex app-server: stdin shutdown error");
                }
                drop(s);
            }
        }
        let mut child_guard = self.child.lock().await;
        if let Some(mut child) = child_guard.take() {
            match timeout(grace, child.wait()).await {
                Ok(Ok(_status)) => {}
                Ok(Err(e)) => warn!(error = %e, "codex app-server: wait failed"),
                Err(_) => {
                    warn!(
                        grace_secs = grace.as_secs_f64(),
                        "codex app-server did not exit on stdin EOF; escalating to SIGKILL"
                    );
                    if let Err(e) = child.start_kill() {
                        warn!(error = %e, "codex app-server: start_kill failed");
                    }
                    if let Err(e) = child.wait().await {
                        warn!(error = %e, "codex app-server: post-kill wait failed");
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether the subprocess is still running.
    pub async fn is_alive(&self) -> bool {
        let mut guard = self.child.lock().await;
        match guard.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    async fn write_line(&self, line: &str) -> Result<()> {
        if *lock_recover(&self.closed) {
            bail!("codex app-server client is closed");
        }
        let mut stdin_guard = self.stdin.lock().await;
        let stdin = stdin_guard
            .as_mut()
            .ok_or_else(|| anyhow!("codex app-server stdin was dropped"))?;
        stdin
            .write_all(line.as_bytes())
            .await
            .context("write to codex stdin")?;
        stdin.write_all(b"\n").await.context("write newline")?;
        stdin.flush().await.context("flush codex stdin")?;
        Ok(())
    }
}

impl Drop for CodexAppServerClient {
    fn drop(&mut self) {
        // Best-effort: tokio's kill_on_drop handles the subprocess. We just need
        // to abort the long-lived reader tasks so they don't outlive the client.
        if let Some(handle) = lock_recover(&self.reader_handle).take() {
            handle.abort();
        }
        if let Some(handle) = lock_recover(&self.stderr_handle).take() {
            handle.abort();
        }
    }
}

/// Signal reader-exit and drain every waiter so no `request()` caller hangs
/// past the next poll.
///
/// Ordering matters: the `Release` store must happen **before** the drain so
/// that a concurrent `request()` whose insert is sequenced after our drain
/// observes the flag as `true` on its `Acquire` load and self-removes. See the
/// post-check in `request()` for the matching half.
fn finish_reader(reader_done: &AtomicBool, pending: &PendingMap, reason: &'static str) {
    reader_done.store(true, Ordering::Release);
    let mut waiters = lock_recover(pending);
    for (_, tx) in waiters.drain() {
        let _ = tx.send(Err(RpcError {
            code: -1,
            message: reason.to_string(),
            data: None,
        }));
    }
}

async fn read_stdout_loop(
    stdout: tokio::process::ChildStdout,
    pending: PendingMap,
    notif_tx: mpsc::Sender<Notification>,
    srv_tx: mpsc::Sender<ServerRequest>,
    stderr_tail: Arc<Mutex<Vec<String>>>,
    reader_done: Arc<AtomicBool>,
) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => match parse_frame(&line) {
                None => continue,
                Some(Err(e)) => {
                    let truncated: String = line.chars().take(200).collect();
                    warn!(error = %e, line = %truncated, "codex app-server: malformed stdout frame");
                    push_stderr_line(&stderr_tail, format!("<malformed stdout> {truncated}"));
                }
                Some(Ok(InboundFrame::Response { id, result })) => {
                    let waiter = lock_recover(&pending).remove(&id);
                    match waiter {
                        Some(tx) => {
                            let _ = tx.send(result);
                        }
                        None => {
                            warn!(?id, "codex app-server: response for unknown request id");
                        }
                    }
                }
                Some(Ok(InboundFrame::Notification { method, params })) => {
                    if method.is_empty() {
                        warn!("codex app-server: notification with empty method");
                        continue;
                    }
                    if notif_tx
                        .send(Notification { method, params })
                        .await
                        .is_err()
                    {
                        debug!("codex app-server: notification receiver dropped");
                        finish_reader(&reader_done, &pending, "notification receiver dropped");
                        return;
                    }
                }
                Some(Ok(InboundFrame::ServerRequest { id, method, params })) => {
                    if srv_tx
                        .send(ServerRequest { id, method, params })
                        .await
                        .is_err()
                    {
                        debug!("codex app-server: server-request receiver dropped");
                        finish_reader(&reader_done, &pending, "server-request receiver dropped");
                        return;
                    }
                }
            },
            Ok(None) => {
                // EOF: subprocess closed stdout.
                debug!("codex app-server: stdout EOF");
                finish_reader(&reader_done, &pending, "codex app-server stdout closed");
                return;
            }
            Err(e) => {
                warn!(error = %e, "codex app-server: stdout read error");
                finish_reader(&reader_done, &pending, "codex app-server stdout read error");
                return;
            }
        }
    }
}

async fn read_stderr_loop(stderr: tokio::process::ChildStderr, tail: Arc<Mutex<Vec<String>>>) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        push_stderr_line(&tail, line);
    }
}

fn push_stderr_line(tail: &Arc<Mutex<Vec<String>>>, line: String) {
    let mut guard = lock_recover(tail);
    guard.push(line);
    let overflow = guard.len().saturating_sub(STDERR_TAIL_CAP);
    if overflow > 0 {
        guard.drain(0..overflow);
    }
}

/// Parse `codex --version` output to `(major, minor, patch)`.
///
/// Returns `None` when the output doesn't carry a recognizable semver triple.
pub fn parse_codex_version(output: &str) -> Option<(u32, u32, u32)> {
    static RE: std::sync::OnceLock<Option<regex::Regex>> = std::sync::OnceLock::new();
    let re = RE
        .get_or_init(|| regex::Regex::new(r"(\d+)\.(\d+)\.(\d+)").ok())
        .as_ref()?;
    let caps = re.captures(output)?;
    Some((
        caps.get(1)?.as_str().parse().ok()?,
        caps.get(2)?.as_str().parse().ok()?,
        caps.get(3)?.as_str().parse().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_codex_version_extracts_triple() {
        assert_eq!(parse_codex_version("codex-cli 0.130.0"), Some((0, 130, 0)));
        assert_eq!(
            parse_codex_version("codex-cli 1.2.3-beta+meta\n"),
            Some((1, 2, 3))
        );
    }

    #[test]
    fn parse_codex_version_handles_garbage() {
        assert_eq!(parse_codex_version(""), None);
        assert_eq!(parse_codex_version("codex"), None);
    }
}
