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
    let mut cmd = Command::new(&config.command);
    cmd.args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        // Critical: wipe the inherited environment, then layer back only the
        // allowlist + operator-declared env. This is the guard that keeps
        // `ANTHROPIC_API_KEY` out of MCP subprocesses.
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

// Real subprocess round-trip + env-allowlist guard test land in
// `crates/mcp/tests/stdio_subprocess.rs` (Slice 4) with a dedicated fixture
// binary. The pure-logic tests for env hygiene live in `super::tests` on
// `transport/mod.rs`.
