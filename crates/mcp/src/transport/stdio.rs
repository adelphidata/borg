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
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// Fallback per-line byte cap when [`StdioConfig::max_line_bytes`] is 0 (unset).
/// Mirrors the HTTP transport's 1 MiB body ceiling; a real per-server value is
/// threaded in from `mcp.max_response_bytes` at construction. A single
/// newline-delimited JSON-RPC frame larger than this is treated as a hostile /
/// broken peer and the transport is torn down rather than buffered unbounded.
pub const DEFAULT_MAX_LINE_BYTES: usize = 1024 * 1024;

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
    /// Hard cap on a single newline-delimited line from the child's stdout /
    /// stderr, enforced *as bytes arrive* so a child streaming megabytes with
    /// no newline can't grow the read buffer without bound and OOM the process.
    /// 0 means "unset" and falls back to [`DEFAULT_MAX_LINE_BYTES`].
    pub max_line_bytes: usize,
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

    // A single line larger than this is a hostile/broken peer; both the stdout
    // reader and the stderr pump enforce it so neither can OOM the process.
    let max_line_bytes = if config.max_line_bytes == 0 {
        DEFAULT_MAX_LINE_BYTES
    } else {
        config.max_line_bytes
    };

    // Pump stderr on a background task into the sink. Aborted on drop. Bounded
    // by the same per-line cap: a giant unterminated stderr line is the same
    // OOM vector as stdout, and stderr only feeds the log sink so we simply
    // stop pumping (rather than tearing down the transport) once tripped.
    let server_name = config.server_name.clone();
    let stderr_pump = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr_stream);
        let mut buf: Vec<u8> = Vec::new();
        loop {
            match read_capped_line(&mut reader, &mut buf, max_line_bytes).await {
                Ok(true) => stderr(
                    &server_name,
                    &String::from_utf8_lossy(trim_trailing_cr(&buf)),
                ),
                Ok(false) => break,
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
        line_buf: Vec::new(),
        max_line_bytes,
        stderr_pump: Some(stderr_pump),
        server_name: config.server_name,
        watchdog_pid,
    })
}

/// Read one newline-delimited line from `reader` into `buf` (cleared first),
/// enforcing a hard byte cap as bytes arrive. Unlike [`AsyncBufReadExt::read_line`]
/// / `read_until`, which grow their target without limit, this errors the moment
/// a single pre-newline line would exceed `cap`, so a peer streaming megabytes
/// with no newline can't OOM the process. Returns `Ok(true)` when a line was
/// read (`buf` holds it, sans trailing `\n`), `Ok(false)` on clean EOF with no
/// buffered bytes, and an `InvalidData` error when the cap is exceeded.
async fn read_capped_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    cap: usize,
) -> std::io::Result<bool> {
    buf.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            // EOF: deliver a partial (unterminated) final line if we have one.
            return Ok(!buf.is_empty());
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(idx) => {
                if buf.len() + idx > cap {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "line exceeds max_line_bytes",
                    ));
                }
                buf.extend_from_slice(&available[..idx]);
                reader.consume(idx + 1); // drop the newline
                return Ok(true);
            }
            None => {
                if buf.len() + available.len() > cap {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "line exceeds max_line_bytes",
                    ));
                }
                let n = available.len();
                buf.extend_from_slice(available);
                reader.consume(n);
            }
        }
    }
}

/// Strip any trailing `\r` bytes (CRLF framing) without allocating.
fn trim_trailing_cr(buf: &[u8]) -> &[u8] {
    let end = buf.iter().rposition(|&b| b != b'\r').map_or(0, |i| i + 1);
    &buf[..end]
}

/// Connected stdio transport.
pub struct StdioTransport {
    child: Arc<Mutex<Option<Child>>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    reader: BufReader<tokio::process::ChildStdout>,
    line_buf: Vec<u8>,
    /// Per-line byte cap for stdout, resolved from config at spawn.
    max_line_bytes: usize,
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
            match read_capped_line(&mut self.reader, &mut self.line_buf, self.max_line_bytes).await
            {
                Ok(true) => {}
                Ok(false) => return Ok(None),
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    // Oversized single line: a hostile/broken peer. Tearing the
                    // transport down (TransportClosed) triggers the existing
                    // child-reaping path rather than buffering unbounded.
                    return Err(McpError::TransportClosed(format!(
                        "{}: stdout line exceeds max_line_bytes ({} bytes)",
                        self.server_name, self.max_line_bytes
                    )));
                }
                Err(e) => {
                    return Err(McpError::TransportClosed(format!(
                        "{}: stdout read failed: {e}",
                        self.server_name
                    )))
                }
            }
            let trimmed = trim_trailing_cr(&self.line_buf);
            if trimmed.is_empty() {
                // Empty keepalive line — loop instead of recursing.
                // Hostile peer sending infinite empty lines would otherwise
                // blow the stack via boxed-future allocation.
                continue;
            }
            let msg = decode_line(trimmed)?;
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
            // Grace before we SIGKILL `child`. For a direct spawn `child` is the
            // server, and 250ms after EOF is plenty for a well-behaved server.
            // For a watchdogged spawn `child` is the WATCHDOG, which itself
            // needs time to notice our SIGTERM (≤100ms poll), SIGTERM the
            // server group, wait out its own ~300ms grace, then SIGKILL — so a
            // 250ms window here would SIGKILL the watchdog mid-reap and strand
            // the very server it exists to clean up. Give it comfortably more
            // than that whole budget.
            let graceful_wait = if self.watchdog_pid.is_some() {
                std::time::Duration::from_millis(800)
            } else {
                std::time::Duration::from_millis(250)
            };
            match tokio::time::timeout(graceful_wait, child.wait()).await {
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
        // The watchdog has now been reaped, freeing its pid. Clear it so the
        // Drop path can't re-`SIGTERM` a recycled pid (PID reuse would land the
        // signal on an unrelated process during heavy spawn churn).
        self.watchdog_pid = None;
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
                    // Reap so the SIGTERM'd server doesn't linger as a zombie; it may already be gone.
                    let _ = c.wait().await;
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn capped_line_reads_valid_then_rejects_oversized_unterminated() {
        // A well-formed line, then 200 bytes with no newline. Cap = 16 means
        // the first line is delivered and the second read trips the cap — the
        // real "valid message then hostile long line" shape.
        let mut data = b"valid\n".to_vec();
        data.extend(std::iter::repeat_n(b'x', 200));
        let mut reader = BufReader::new(&data[..]);
        let mut buf = Vec::new();

        assert!(read_capped_line(&mut reader, &mut buf, 16).await.unwrap());
        assert_eq!(buf, b"valid");

        let err = read_capped_line(&mut reader, &mut buf, 16)
            .await
            .expect_err("oversized unterminated line must be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn capped_line_trips_across_chunk_boundaries() {
        // 40 bytes, no newline, fed 4 bytes per fill_buf so the cap must be
        // enforced across many reads rather than in a single buffer.
        let data = [b'x'; 40];
        let mut reader = BufReader::with_capacity(4, &data[..]);
        let mut buf = Vec::new();

        let err = read_capped_line(&mut reader, &mut buf, 16)
            .await
            .expect_err("cross-chunk overflow must trip the cap");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn capped_line_passes_large_terminated_line_under_cap() {
        // 1000-byte terminated line, tiny read buffer, cap well above it: the
        // bound must not reject legitimate large-but-terminated frames.
        let mut data = vec![b'a'; 1000];
        data.push(b'\n');
        let mut reader = BufReader::with_capacity(8, &data[..]);
        let mut buf = Vec::new();

        assert!(read_capped_line(&mut reader, &mut buf, 4096).await.unwrap());
        assert_eq!(buf.len(), 1000);
    }

    #[tokio::test]
    async fn capped_line_eof_with_empty_buffer_returns_false() {
        let mut reader = BufReader::new(&b""[..]);
        let mut buf = Vec::new();
        assert!(!read_capped_line(&mut reader, &mut buf, 16).await.unwrap());
    }

    #[tokio::test]
    async fn capped_line_delivers_unterminated_final_under_cap() {
        // A final line with no trailing newline, under the cap, is delivered.
        let mut reader = BufReader::new(&b"tail-no-newline"[..]);
        let mut buf = Vec::new();
        assert!(read_capped_line(&mut reader, &mut buf, 64).await.unwrap());
        assert_eq!(buf, b"tail-no-newline");
    }

    #[test]
    fn trim_trailing_cr_strips_crlf_but_keeps_content() {
        assert_eq!(trim_trailing_cr(b"hello\r"), b"hello");
        assert_eq!(trim_trailing_cr(b"hello"), b"hello");
        assert_eq!(trim_trailing_cr(b"hello\r\r"), b"hello");
        assert_eq!(trim_trailing_cr(b"\r"), b"");
        assert_eq!(trim_trailing_cr(b""), b"");
    }
}
