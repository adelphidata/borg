//! Stdio transport: child process speaking newline-delimited JSON-RPC.
//!
//! Security posture:
//!
//! - **Strict env allowlist.** The child sees only entries in
//!   [`super::ENV_ALLOWLIST`] plus the explicit per-server `env` block from
//!   config. Borg's API keys never reach the child unless the operator wired
//!   them through `${SECRET:…}` placeholders for that specific server.
//! - **Stderr to log file.** stderr is read on a background task and routed
//!   through the caller-supplied [`StderrSink`]. It is *never* returned to
//!   the LLM — that would be a credential-exfiltration vector.
//! - **No shell.** The command and args are passed directly to `Command`;
//!   no `sh -c` interpolation.
//! - **Exfil-shape refusal.** Every spawn funnels through here, so this is the
//!   universal enforcement point for [`crate::security::validate_stdio_command`]:
//!   a shell interpreter wrapping network egress is refused before exec,
//!   covering the daemon and operator tooling (`borg mcp …`) alike.

use super::{is_env_allowed, Transport};
use crate::errors::{McpError, Result};
use crate::protocol::{decode_line, Message};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// Configuration for spawning a stdio MCP server.
#[derive(Debug, Clone)]
pub struct StdioConfig {
    /// Server name (used to tag stderr lines in the log).
    pub server_name: String,
    /// Executable path or name (`which` resolution applies normally).
    pub command: String,
    /// CLI arguments.
    pub args: Vec<String>,
    /// Additional env vars (already resolved from `SecretRef`).
    pub env: HashMap<String, String>,
}

/// Callback invoked on every line of the child's stderr.
///
/// The cortex adapter wires this to `~/.borg/logs/mcp-stderr.log` with the
/// server name prefixed. Tests can use a `Vec<String>` collector.
pub type StderrSink = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// Spawn the child and return a connected transport.
pub async fn spawn(config: StdioConfig, stderr: StderrSink) -> Result<StdioTransport> {
    // Universal exfil-shape backstop: refuse a shell interpreter wrapping
    // network egress before exec. cortex's start_server emits the specific
    // audit row and returns earlier on the daemon path; this catch covers any
    // caller (operator `borg mcp …` tooling spawns straight through here),
    // so no path can bypass the gate.
    if let Some(reason) = crate::security::validate_stdio_command(&config.command, &config.args) {
        return Err(McpError::Connect(format!(
            "refused exfiltration-shaped stdio command '{}': {reason}",
            config.command
        )));
    }

    // Interpose the watchdog reaper (Unix) so an ungraceful death of THIS
    // process — `SIGKILL`, crash — doesn't orphan the server. The watchdog
    // re-execs the current binary, spawns the real command, and reaps it on
    // parent death. On non-unix (or if `current_exe` is unresolvable) this is
    // `None` and we spawn directly, keeping `kill_on_drop` as the sole reaper.
    let (spawn_command, spawn_args, watchdogged) =
        match crate::watchdog::wrapped_command(&config.command, &config.args) {
            Some((c, a)) => (c, a, true),
            None => (config.command.clone(), config.args.clone(), false),
        };

    let mut cmd = Command::new(&spawn_command);
    cmd.args(&spawn_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // When watchdogged, the watchdog owns reaping the server group; a
        // `kill_on_drop` SIGKILL to the watchdog is uncatchable and would
        // strand the server, so we disable it and reap via SIGTERM in
        // `close`/`Drop`. Direct spawns keep `kill_on_drop` as before.
        .kill_on_drop(!watchdogged)
        // Critical: wipe the inherited environment, then layer back only the
        // allowlist + operator-declared env. This is the guard that keeps
        // `ANTHROPIC_API_KEY` out of MCP subprocesses. The watchdog re-exec
        // inherits this filtered env and passes it to the real server.
        .env_clear();

    for (key, value) in std::env::vars() {
        if is_env_allowed(&key) {
            cmd.env(key, value);
        }
    }
    for (key, value) in &config.env {
        cmd.env(key, value);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| McpError::Connect(format!("failed to spawn '{}': {e}", config.command)))?;
    // When watchdogged, `child` is the watchdog process; its pid is the target
    // for the catchable SIGTERM teardown path.
    let watchdog_pid: Option<i32> = if watchdogged {
        child.id().map(|id| id as i32)
    } else {
        None
    };

    let stdin = child.stdin.take().ok_or_else(|| {
        McpError::Connect(format!("child '{}' produced no stdin", config.command))
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        McpError::Connect(format!("child '{}' produced no stdout", config.command))
    })?;
    let stderr_stream = child.stderr.take().ok_or_else(|| {
        McpError::Connect(format!("child '{}' produced no stderr", config.command))
    })?;

    // Pump stderr on a background task into the sink. Aborted on drop.
    let server_name = config.server_name.clone();
    let stderr_pump = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr_stream).lines();
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => stderr(&server_name, &line),
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(server = %server_name, error = %e, "mcp stderr read failed");
                    break;
                }
            }
        }
    });

    let reader = BufReader::new(stdout);

    Ok(StdioTransport {
        child: Arc::new(Mutex::new(Some(child))),
        stdin: Arc::new(Mutex::new(Some(stdin))),
        reader,
        line_buf: String::new(),
        stderr_pump: Some(stderr_pump),
        server_name: config.server_name,
        watchdog_pid,
    })
}

/// Connected stdio transport.
pub struct StdioTransport {
    child: Arc<Mutex<Option<Child>>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    reader: BufReader<tokio::process::ChildStdout>,
    line_buf: String,
    stderr_pump: Option<JoinHandle<()>>,
    server_name: String,
    /// When spawned under the Unix watchdog, the watchdog process's pid. A
    /// catchable `SIGTERM` here asks it to reap the real server group. `None`
    /// for direct spawns (non-unix), which rely on `kill_on_drop`.
    watchdog_pid: Option<i32>,
}

impl StdioTransport {
    /// Server name, for diagnostics.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn send(&mut self, msg: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&msg)?;
        bytes.push(b'\n');
        let mut guard = self.stdin.lock().await;
        let stdin = guard.as_mut().ok_or_else(|| {
            McpError::TransportClosed(format!("{}: stdin closed", self.server_name))
        })?;
        stdin.write_all(&bytes).await.map_err(|e| {
            McpError::TransportClosed(format!("{}: stdin write failed: {e}", self.server_name))
        })?;
        stdin.flush().await.map_err(|e| {
            McpError::TransportClosed(format!("{}: stdin flush failed: {e}", self.server_name))
        })?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<Message>> {
        loop {
            self.line_buf.clear();
            let n = self
                .reader
                .read_line(&mut self.line_buf)
                .await
                .map_err(|e| {
                    McpError::TransportClosed(format!(
                        "{}: stdout read failed: {e}",
                        self.server_name
                    ))
                })?;
            if n == 0 {
                return Ok(None);
            }
            let trimmed = self.line_buf.trim_end_matches(['\n', '\r']);
            if trimmed.is_empty() {
                // Empty keepalive line — loop instead of recursing.
                // Hostile peer sending infinite empty lines would otherwise
                // blow the stack via boxed-future allocation.
                continue;
            }
            let msg = decode_line(trimmed.as_bytes())?;
            return Ok(Some(msg));
        }
    }

    async fn close(&mut self) -> Result<()> {
        // Drop stdin to signal EOF, then await the child briefly so that
        // close()'s contract — "graceful shutdown" — is honored rather than
        // returning while the child is still running.
        {
            let mut guard = self.stdin.lock().await;
            drop(guard.take());
        }
        if let Some(handle) = self.stderr_pump.take() {
            handle.abort();
        }
        // Ask the watchdog to reap the real server group (catchable SIGTERM);
        // the `child.wait()` below then reaps the watchdog process itself.
        #[cfg(unix)]
        if let Some(pid) = self.watchdog_pid {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        if let Some(mut child) = self.child.lock().await.take() {
            // Give the child 250ms to exit on its own after EOF; well-behaved
            // MCP servers do. Then kill + wait so we don't return with a
            // zombie.
            const GRACEFUL_WAIT: std::time::Duration = std::time::Duration::from_millis(250);
            match tokio::time::timeout(GRACEFUL_WAIT, child.wait()).await {
                Ok(Ok(_status)) => {}
                Ok(Err(e)) => {
                    tracing::debug!(server = %self.server_name, error = %e, "mcp child wait failed");
                }
                Err(_) => {
                    if let Err(e) = child.start_kill() {
                        tracing::debug!(server = %self.server_name, error = %e, "mcp child kill failed");
                    }
                    // Reap the zombie. kill_on_drop also covers this, but
                    // doing it explicitly keeps the contract honest.
                    if let Err(e) = child.wait().await {
                        tracing::debug!(server = %self.server_name, error = %e, "mcp child reap failed");
                    }
                }
            }
        }
        Ok(())
    }
}

/// Reap a watchdogged server on a plain drop (no `close`). Watchdogged spawns
/// disable `kill_on_drop`, so without this the watchdog — and the server it
/// supervises — would leak. SIGTERM (catchable) tells the watchdog to tear the
/// server group down; if a runtime is present we also reap the watchdog zombie.
/// A no-op for direct (non-watchdog) spawns, which `kill_on_drop` already
/// covers.
#[cfg(unix)]
impl Drop for StdioTransport {
    fn drop(&mut self) {
        let Some(pid) = self.watchdog_pid else {
            return;
        };
        // Harmless if `close` already reaped it (SIGTERM to a dead pid → ESRCH).
        unsafe { libc::kill(pid, libc::SIGTERM) };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let child = self.child.clone();
            handle.spawn(async move {
                if let Some(mut c) = child.lock().await.take() {
                    let _ = c.wait().await;
                }
            });
        }
    }
}
