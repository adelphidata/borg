//! Persistent interactive shell sessions for `run_shell`.
//!
//! The default `run_shell` path spawns a child, streams it to completion, and
//! returns. That can't drive a REPL or any program that reads stdin across
//! multiple turns. This registry keeps a spawned child alive between tool
//! calls so the model can `start` a long-running process, `write_stdin` to it,
//! `poll` for new output, and `kill` it.
//!
//! ## Lifetime & ownership
//!
//! Sessions outlive any single [`crate::execution_context::ExecutionContext`]
//! (a fresh one is built per tool call), so the registry is a process-global
//! installed once by the daemon — exactly the pattern in
//! [`crate::process_registry`]. In CLI/REPL/test contexts the global stays
//! uninstalled and [`global`] returns `None`; the `run_shell` handler then
//! reports that persistent sessions require the daemon and the model falls
//! back to one-shot commands.
//!
//! ## Draining
//!
//! Each session spawns one detached drain task that reads stdout+stderr into a
//! shared buffer until both close. A plain `tokio::spawn` is correct here (not
//! `spawn_supervised`): the task is bounded by the child's lifetime — if it
//! dies the session is simply dead and the next `poll`/`write_stdin` surfaces
//! that. The existing `process_registry` auto-reap + the child's
//! `kill_on_drop` are the safety net against leaks.
//!
//! That buffer is bounded ([`SessionOutput`]): a session the model never polls
//! — or one whose child floods output faster than it is read — would otherwise
//! accumulate until the session is killed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin};
use tokio::sync::Mutex as AsyncMutex;

use crate::process_registry::ProcessGuard;
#[cfg(unix)]
use crate::util::process_tree::signal_group;

/// Grace period between SIGTERM and SIGKILL when terminating a session, giving
/// the process tree a chance to run its own cleanup before the hard kill.
const KILL_GRACE: Duration = Duration::from_millis(300);

/// Most sessions one daemon keeps alive at once. Each is a live shell plus a
/// drain task and an output buffer; a model that starts sessions and never
/// kills them must hit a clear error, not grow the process without bound. An
/// exited session is removed by the `poll` that reports its exit, so a normal
/// workflow never reaches this.
pub const MAX_SESSIONS: usize = 16;

/// Most sessions one owner (conversation) keeps alive at once. The registry
/// is process-global, so without this one chatty conversation could fill
/// every slot of [`MAX_SESSIONS`] and lock every other channel out of the
/// session lane. Ownerless sessions share one bucket. Checked alongside the
/// global cap; whichever is hit first refuses the start.
pub const MAX_SESSIONS_PER_OWNER: usize = 8;

/// Process-global session registry. The daemon installs one at startup so the
/// `run_shell` handler can reach it across turns without threading a handle
/// through every signature.
static GLOBAL: OnceLock<ShellSessionRegistry> = OnceLock::new();

/// Install the process-global session registry. Returns `true` on first
/// install, `false` if one was already present (single-installer invariant,
/// matching [`crate::process_registry::install_global`]).
pub fn install_global(reg: ShellSessionRegistry) -> bool {
    GLOBAL.set(reg).is_ok()
}

/// Fetch a clone of the process-global session registry, if installed.
pub fn global() -> Option<ShellSessionRegistry> {
    GLOBAL.get().cloned()
}

/// Bounded rolling buffer holding a session's output that `poll` has not yet
/// returned. The one-shot `run_shell` path caps its collection the same way;
/// without a cap here a long-lived session (a chatty REPL, a build loop, a
/// `yes`) grows a `String` until the session is killed.
///
/// The newest bytes are the ones kept: for an interactive session the current
/// prompt and the latest error are what the model needs. Dropped bytes are
/// counted and surfaced by `poll` as an explicit marker rather than silently
/// disappearing.
struct SessionOutput {
    text: String,
    /// Bytes dropped from the front since the last `poll`.
    dropped: usize,
    cap: usize,
}

impl SessionOutput {
    /// `cap` is the retained byte budget. `0` is treated as `1` (never
    /// unbounded); callers resolve the configured default first.
    fn new(cap: usize) -> Self {
        Self {
            text: String::new(),
            dropped: 0,
            cap: cap.max(1),
        }
    }

    fn push(&mut self, chunk: &str) {
        self.text.push_str(chunk);
        // Compact lazily: let the buffer reach twice the cap before trimming
        // back down to it, so a flooding child pays one memmove per `cap` bytes
        // written instead of one per 4 KiB chunk. This is a memory-residency
        // optimization only — `take` trims to the exact cap, so what a `poll`
        // returns never exceeds the configured budget.
        if self.text.len() <= self.cap.saturating_mul(2) {
            return;
        }
        self.trim_to_cap();
    }

    /// Drop the oldest bytes until at most `cap` remain, counting what went.
    fn trim_to_cap(&mut self) {
        if self.text.len() <= self.cap {
            return;
        }
        let mut cut = self.text.len() - self.cap;
        // Trim forward to a char boundary so the retained text stays valid
        // UTF-8 even when the cut lands mid-codepoint. Walking forward (not
        // back) keeps the result at or under the cap.
        while cut < self.text.len() && !self.text.is_char_boundary(cut) {
            cut += 1;
        }
        self.text.drain(..cut);
        self.dropped += cut;
    }

    /// Take everything retained, resetting the buffer so already-returned
    /// output is not held (or re-emitted) afterwards. Trims first so a single
    /// `poll` never returns more than the configured cap, matching the hard
    /// bound the one-shot `run_shell` path applies.
    fn take(&mut self) -> String {
        self.trim_to_cap();
        let dropped = std::mem::take(&mut self.dropped);
        let text = std::mem::take(&mut self.text);
        if dropped == 0 {
            text
        } else {
            format!("... {dropped} bytes omitted ...\n{text}")
        }
    }
}

/// What one [`ShellSessionRegistry::poll`] observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Polled {
    /// New output since the last poll, plus any `cwd:` or exit note.
    pub output: String,
    /// Whether `output` must go through env-assignment masking (a prior
    /// `write_stdin` read a blocked env/credential file). Read under the same
    /// lock as the output, so the final poll of an exited session — which
    /// removes the session — still carries the flag.
    pub env_mask: bool,
}

/// One live interactive session.
struct Session {
    /// The child handle, kept for `kill`. `kill_on_drop` is set at spawn so a
    /// dropped session also terminates the process.
    child: Child,
    /// The child's PID, captured at registration. On Unix the child leads its
    /// own process group (PGID == PID), so `kill` signals the whole group.
    /// `None` if the child had already exited before we could read it.
    pid: Option<u32>,
    /// Child stdin, behind an async mutex so `write_stdin` can `.await` the
    /// write without holding the registry's std mutex across an await point.
    /// `None` once stdin has been taken/closed.
    stdin: Arc<AsyncMutex<Option<ChildStdin>>>,
    /// Unread stdout+stderr. Appended by the drain task; drained by `poll`.
    output: Arc<Mutex<SessionOutput>>,
    /// Subprocess-registry guard. Marked `killed` on `kill`; its `Drop` marks
    /// the entry `Killed` if the session is torn down without an explicit kill.
    guard: Option<ProcessGuard>,
    /// Detached drain task. Aborted on `kill`.
    drain: tokio::task::JoinHandle<()>,
    /// Working directory last reported for this session, sampled at
    /// registration and refreshed on every `poll`. Lets `poll` append a
    /// `cwd: <path>` note when a `cd` moved the session, so the model stops
    /// defensively prefixing every command with `cd X &&`. `None` when the
    /// platform can't report it — then no note is ever emitted.
    last_cwd: Option<std::path::PathBuf>,
    /// Set once a `write_stdin` was detected reading a blocked env/credential
    /// file. Sticky for the session's lifetime: output arrives asynchronously,
    /// so per-command attribution is impossible — from that point every poll's
    /// output must go through env-assignment masking in the shell handler.
    env_mask: bool,
    /// The conversation session that started this shell session, from
    /// `ExecutionContext::session_id`. Every later action must present the
    /// same id: the registry is process-global, so without this check any
    /// channel/sender routed through the daemon could drive (inject stdin
    /// into, read output from, kill) a shell another conversation started.
    /// `None` means the starter had no session id; then only callers that
    /// also have none may act on it.
    owner: Option<String>,
}

/// Registry of live interactive shell sessions. Cheap to clone — state is
/// shared through `Arc<Mutex<_>>`.
#[derive(Clone, Default)]
pub struct ShellSessionRegistry {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, Session>,
    next_id: u64,
}

impl ShellSessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::warn!("shell_session: mutex poisoned; recovering");
                poisoned.into_inner()
            }
        }
    }

    /// Register a freshly-spawned child as a session and start draining its
    /// output. Takes ownership of the child's stdio pipes. Returns the
    /// generated session id (`sh-<n>`). The caller is responsible for having
    /// spawned `child` with piped stdin/stdout/stderr and `kill_on_drop(true)`.
    ///
    /// On Unix the caller MUST also spawn with `spawn_in_own_group` so the child
    /// leads its own group (PGID == PID): [`Self::kill`] signals the whole group
    /// via `killpg`, which is only safe — and only reaches descendants — when
    /// that holds. Without it, `kill` would signal an unrelated group, so it
    /// instead falls back to signaling just the direct child.
    ///
    /// `max_output_bytes` caps the unread output retained between polls; the
    /// caller resolves it from `security.max_shell_output_bytes` so sessions and
    /// one-shot commands share one budget.
    ///
    /// `owner` is the starting conversation's session id; all later actions on
    /// the returned id must present the same value (see [`Session::owner`]).
    ///
    /// Errors when [`MAX_SESSIONS`] are already live, or `owner` already has
    /// [`MAX_SESSIONS_PER_OWNER`]; the child is then dropped (and killed by
    /// `kill_on_drop`) rather than registered.
    pub fn register(
        &self,
        mut child: Child,
        guard: Option<ProcessGuard>,
        max_output_bytes: usize,
        owner: Option<String>,
    ) -> Result<String> {
        let mut inner = self.lock();
        if inner.sessions.len() >= MAX_SESSIONS {
            return Err(anyhow!(
                "too many shell sessions are open ({MAX_SESSIONS}); kill one with \
                 action=kill (or poll an exited one) before starting another"
            ));
        }
        let owned = inner.sessions.values().filter(|s| s.owner == owner).count();
        if owned >= MAX_SESSIONS_PER_OWNER {
            return Err(anyhow!(
                "this conversation already has {MAX_SESSIONS_PER_OWNER} shell sessions open; \
                 kill one with action=kill (or poll an exited one) before starting another"
            ));
        }
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = child.stdin.take();
        let output = Arc::new(Mutex::new(SessionOutput::new(max_output_bytes)));
        let drain = spawn_drain(stdout, stderr, output.clone());

        inner.next_id = inner.next_id.wrapping_add(1).max(1);
        // The counter keeps ids readable and collision-free; the random
        // suffix keeps them unguessable, so a foreign conversation cannot
        // enumerate `sh-1`, `sh-2`, … and probe sessions it does not own.
        // Defense in depth alongside the owner check — not a substitute.
        let suffix: u32 = rand::random();
        let id = format!("sh-{}-{suffix:08x}", inner.next_id);
        // The counter makes a collision unreachable, but assert the invariant
        // rather than silently `insert`-overwriting (and thus killing) a live
        // session.
        debug_assert!(
            !inner.sessions.contains_key(&id),
            "shell session id {id} collided with a live session"
        );
        inner.sessions.insert(
            id.clone(),
            Session {
                child,
                pid,
                stdin: Arc::new(AsyncMutex::new(stdin)),
                output,
                guard,
                drain,
                // Baseline for the `poll` cwd-change note. Best-effort: an
                // unreadable cwd just means the note is never produced.
                last_cwd: pid.and_then(crate::util::process_tree::process_cwd),
                env_mask: false,
                owner,
            },
        );
        Ok(id)
    }

    /// Whether a session with `id` is currently registered.
    pub fn contains(&self, id: &str) -> bool {
        self.lock().sessions.contains_key(id)
    }

    /// Whether `caller` may act on `session`. Both sides must match exactly:
    /// an owned session rejects every other (or absent) caller id, and an
    /// ownerless session rejects callers that do have one. Fail closed.
    fn caller_owns(session: &Session, caller: Option<&str>) -> bool {
        session.owner.as_deref() == caller
    }

    /// Look up `id` for `caller`, mapping both "unknown id" and "not the
    /// owner" to the same error so a foreign caller cannot use the message
    /// as an existence oracle for other conversations' sessions.
    fn get_owned<'a>(
        inner: &'a mut Inner,
        id: &str,
        caller: Option<&str>,
    ) -> Result<&'a mut Session> {
        inner
            .sessions
            .get_mut(id)
            .filter(|s| Self::caller_owns(s, caller))
            .ok_or_else(|| anyhow!("no such shell session: {id}"))
    }

    /// Write `data` to the session's stdin and flush. Errors if the session is
    /// unknown, owned by a different conversation, or its stdin has closed.
    pub async fn write_stdin(&self, id: &str, data: &str, caller: Option<&str>) -> Result<()> {
        let stdin = {
            let mut inner = self.lock();
            Self::get_owned(&mut inner, id, caller)?.stdin.clone()
        };
        let mut guard = stdin.lock().await;
        match guard.as_mut() {
            Some(si) => {
                si.write_all(data.as_bytes()).await?;
                si.flush().await?;
                Ok(())
            }
            None => Err(anyhow!("shell session {id} stdin is closed")),
        }
    }

    /// Return output accumulated since the last `poll` for this session,
    /// draining it so nothing is re-emitted or retained afterwards. When the
    /// session's working directory changed since the last observation (a `cd`
    /// ran), a `cwd: <path>` note is appended so the model knows where the
    /// shell now lives. Errors if the session is unknown.
    ///
    /// Once the child has exited *and* its output is fully drained, the poll
    /// that observes it returns the last output plus an
    /// `[session <id> exited with code N]` line, finalizes the subprocess
    /// guard, and removes the session — a later poll reports it as unknown
    /// rather than "no new output" forever.
    pub fn poll(&self, id: &str, caller: Option<&str>) -> Result<Polled> {
        let mut inner = self.lock();
        let (mut out, env_mask, exit) = {
            let session = Self::get_owned(&mut inner, id, caller)?;
            // Settle the exit BEFORE taking the buffer: the drain task appends
            // its last bytes and then finishes, so checking it afterwards could
            // see "finished" for bytes that landed after the take — and the
            // removal below would drop them.
            let exit = Self::settled_exit(session, id);
            let mut out = {
                let mut buf = session
                    .output
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                buf.take()
            };
            // Only note an *observed change*: a first-ever successful read
            // (prior baseline unknown) seeds silently, and an unreadable cwd
            // (dead child, unsupported platform) never fabricates a note.
            if let Some(now) = session.pid.and_then(crate::util::process_tree::process_cwd) {
                if session.last_cwd.as_ref().is_some_and(|prev| prev != &now) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&format!("cwd: {}", now.display()));
                }
                session.last_cwd = Some(now);
            }
            (out, session.env_mask, exit)
        };
        let Some(status) = exit else {
            return Ok(Polled {
                output: out,
                env_mask,
            });
        };
        let mut session = inner
            .sessions
            .remove(id)
            .ok_or_else(|| anyhow!("no such shell session: {id}"))?;
        drop(inner);
        if let Some(g) = session.guard.take() {
            g.complete(status.code());
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&exit_note(id, status));
        Ok(Polled {
            output: out,
            env_mask,
        })
    }

    /// The child's exit status once it has exited *and* the drain task has
    /// delivered everything it wrote — reporting the exit before the drain
    /// finishes would cut off the final lines. `None` while either is pending.
    /// A `try_wait` error is logged and treated as still running: the next
    /// poll asks again.
    fn settled_exit(session: &mut Session, id: &str) -> Option<std::process::ExitStatus> {
        let status = match session.child.try_wait() {
            Ok(status) => status?,
            Err(e) => {
                tracing::warn!(
                    session = %id,
                    error = %e,
                    "shell_session: could not query child exit status; will retry on next poll"
                );
                return None;
            }
        };
        session.drain.is_finished().then_some(status)
    }

    /// Mark this session as having read a blocked env/credential file via
    /// `write_stdin`; every subsequent [`Self::poll`] reports
    /// [`Polled::env_mask`] so the caller masks env assignments. Sticky by design:
    /// session output is drained asynchronously, so the file's contents can
    /// surface on any later poll. Unknown ids are ignored — the corresponding
    /// `write_stdin` fails on the same missing session anyway.
    pub fn mark_env_mask(&self, id: &str, caller: Option<&str>) {
        if let Some(session) = self
            .lock()
            .sessions
            .get_mut(id)
            .filter(|s| Self::caller_owns(s, caller))
        {
            session.env_mask = true;
        }
    }

    /// Terminate a session: abort its drain task, kill the child's whole
    /// process group, reap it, and finalize the subprocess-registry guard.
    /// Errors if the session is unknown.
    pub async fn kill(&self, id: &str, caller: Option<&str>) -> Result<()> {
        let session = {
            let mut inner = self.lock();
            // Check ownership BEFORE removing: a plain `remove` would let a
            // foreign caller tear the session out of the registry even when
            // the kill itself is then refused.
            match inner.sessions.get(id) {
                Some(s) if Self::caller_owns(s, caller) => inner.sessions.remove(id),
                _ => None,
            }
        };
        let mut session = session.ok_or_else(|| anyhow!("no such shell session: {id}"))?;
        session.drain.abort();
        terminate_child(&mut session.child, session.pid).await;
        if let Some(g) = session.guard.take() {
            g.killed();
        }
        Ok(())
    }
}

/// The line appended to the last poll of an exited session.
fn exit_note(id: &str, status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("[session {id} exited with code {code}]"),
        None => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt as _;
                if let Some(signal) = status.signal() {
                    return format!("[session {id} exited by signal {signal}]");
                }
            }
            format!("[session {id} exited]")
        }
    }
}

/// Terminate a session child and reap it so no zombie or descendant leaks.
///
/// On Unix the child leads its own process group (it is made a session
/// leader at spawn), so we signal the entire group: SIGTERM, a short grace period for
/// the tree to clean up, then SIGKILL if anything is still alive. Without the
/// group signal, a bare child kill would orphan whatever the shell spawned.
/// This is best-effort against cooperative descendants: a child that calls
/// `setsid()`/`setpgid()` to leave the group escapes the signal.
async fn terminate_child(child: &mut Child, pid: Option<u32>) {
    #[cfg(not(unix))]
    let _ = pid;
    #[cfg(unix)]
    if let Some(pid) = pid {
        let pgid = pid as i32;
        signal_group(pgid, libc::SIGTERM);
        if tokio::time::timeout(KILL_GRACE, child.wait())
            .await
            .is_err()
        {
            // Still running after the grace period — hard-kill the group, and
            // also SIGKILL the direct child by PID. The direct kill is the
            // backstop if the child wasn't a group leader (then the group
            // signal was a no-op): it guarantees the `wait` below can't block
            // forever even when the group-leader contract was not met.
            signal_group(pgid, libc::SIGKILL);
            let _ = child.start_kill();
        }
        // Reap the direct child regardless of which signal ended it.
        let _ = child.wait().await;
        return;
    }
    // Non-Unix, or PID unavailable: best-effort direct kill + reap.
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// Spawn the detached task that reads both pipes into the shared buffer until
/// both reach EOF. Raw byte reads (not line reads) so partial output — e.g. a
/// REPL prompt with no trailing newline — still surfaces on the next `poll`.
fn spawn_drain(
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    output: Arc<Mutex<SessionOutput>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut stdout = stdout;
        let mut stderr = stderr;
        let mut sbuf = [0u8; 4096];
        let mut ebuf = [0u8; 4096];
        let mut sdone = stdout.is_none();
        let mut edone = stderr.is_none();
        // Per-stream incremental decoders: a multibyte char split across two
        // pipe reads must not land in the buffer as two U+FFFD. Flushed at
        // each stream's EOF so a truncated trailing sequence still surfaces.
        let mut sdec = crate::util::utf8_stream::Utf8StreamDecoder::new();
        let mut edec = crate::util::utf8_stream::Utf8StreamDecoder::new();
        while !sdone || !edone {
            tokio::select! {
                r = async {
                    match stdout.as_mut() {
                        Some(p) => p.read(&mut sbuf).await,
                        None => std::future::pending().await,
                    }
                }, if !sdone => {
                    match r {
                        Ok(0) => {
                            sdone = true;
                            append(&output, &sdec.flush());
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "shell_session: stdout read failed; treating the stream as closed");
                            sdone = true;
                            append(&output, &sdec.flush());
                        }
                        Ok(n) => append(&output, &sdec.decode(&sbuf[..n])),
                    }
                }
                r = async {
                    match stderr.as_mut() {
                        Some(p) => p.read(&mut ebuf).await,
                        None => std::future::pending().await,
                    }
                }, if !edone => {
                    match r {
                        Ok(0) => {
                            edone = true;
                            append(&output, &edec.flush());
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "shell_session: stderr read failed; treating the stream as closed");
                            edone = true;
                            append(&output, &edec.flush());
                        }
                        Ok(n) => append(&output, &edec.decode(&ebuf[..n])),
                    }
                }
            }
        }
    })
}

fn append(output: &Arc<Mutex<SessionOutput>>, chunk: &str) {
    if chunk.is_empty() {
        return;
    }
    let mut buf = output
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    buf.push(chunk);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::process::Command;

    /// Output cap for tests that aren't exercising the cap itself: large enough
    /// that no fixture trips it.
    const TEST_CAP: usize = 64 * 1024;

    /// Spawn `cat` (echoes stdin to stdout line-by-line) as a session.
    /// Mirrors the production spawn contract documented on [`register`]:
    /// `spawn_in_own_group` so `kill`'s group signal reaches the child.
    fn spawn_cat(reg: &ShellSessionRegistry) -> String {
        spawn_cat_as(reg, None)
    }

    /// [`spawn_cat`] registered under `owner`.
    fn spawn_cat_as(reg: &ShellSessionRegistry, owner: Option<&str>) -> String {
        let mut cmd = Command::new("cat");
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        crate::util::process_tree::spawn_in_own_group(&mut cmd);
        let child = cmd.spawn().expect("spawn cat");
        reg.register(child, None, TEST_CAP, owner.map(str::to_string))
            .expect("register")
    }

    /// Poll until `needle` shows up or we give up — drain is async, so the
    /// echo may not have landed by the first poll. Accumulates across polls
    /// because each `poll` only returns *new* bytes.
    async fn poll_until(reg: &ShellSessionRegistry, id: &str, needle: &str) -> String {
        poll_until_as(reg, id, needle, None).await
    }

    /// [`poll_until`] with an explicit caller session id.
    async fn poll_until_as(
        reg: &ShellSessionRegistry,
        id: &str,
        needle: &str,
        caller: Option<&str>,
    ) -> String {
        let mut acc = String::new();
        for _ in 0..50 {
            acc.push_str(&reg.poll(id, caller).expect("poll").output);
            if acc.contains(needle) {
                return acc;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        acc
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn poll_reports_the_exit_code_with_the_last_output_and_removes_the_session() {
        // Before: an exited session answered every poll with "no new output"
        // and lived until an explicit kill. The model could not tell "quiet"
        // from "gone", and never learned the exit code.
        let reg = ShellSessionRegistry::new();
        let proc_reg = crate::process_registry::ProcessRegistry::new();
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("echo hi; exit 3")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        crate::util::process_tree::spawn_in_own_group(&mut cmd);
        let child = cmd.spawn().expect("spawn sh");
        let pid = child.id().expect("pid");
        let guard = proc_reg.register(
            pid,
            crate::process_registry::ProcessKind::RunShell,
            "echo hi; exit 3",
            None,
        );
        let guard_id = guard.id();
        let id = reg
            .register(child, Some(guard), TEST_CAP, None)
            .expect("register");

        let out = poll_until(&reg, &id, "exited with code 3").await;
        assert!(
            out.contains("hi\n"),
            "the final output must not be cut off: {out:?}"
        );
        assert!(
            out.contains(&format!("[session {id} exited with code 3]")),
            "the exit must be reported with its code: {out:?}"
        );
        assert!(
            !reg.contains(&id),
            "an exited session must be removed once its exit was reported"
        );
        assert!(
            reg.poll(&id, None).is_err(),
            "a later poll must say the session is gone, not 'no new output'"
        );
        let finished = proc_reg.snapshot().finished;
        let entry = finished
            .iter()
            .find(|e| e.id == guard_id)
            .expect("the subprocess-registry entry must be finalized");
        assert_eq!(entry.status, crate::process_registry::ProcessStatus::Exited);
        assert_eq!(entry.exit_code, Some(3));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_live_session_is_not_reported_as_exited() {
        // The exit note must come from a real exit, never from a quiet child.
        let reg = ShellSessionRegistry::new();
        let id = spawn_cat(&reg);
        for _ in 0..5 {
            let out = reg.poll(&id, None).expect("poll").output;
            assert!(
                !out.contains("exited"),
                "live session reported as exited: {out:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(reg.contains(&id));
        reg.kill(&id, None).await.expect("kill");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_session_cap_refuses_the_next_start_with_a_clear_error() {
        // Spread the sessions over owners so only the global cap is in play.
        let reg = ShellSessionRegistry::new();
        let owner_of = |i: usize| format!("owner-{}", i % 4);
        let ids: Vec<String> = (0..MAX_SESSIONS)
            .map(|i| spawn_cat_as(&reg, Some(&owner_of(i))))
            .collect();

        let mut cmd = Command::new("cat");
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = cmd.spawn().expect("spawn cat");
        let err = reg
            .register(child, None, TEST_CAP, Some("owner-fresh".into()))
            .expect_err("the cap must refuse one more session");
        let msg = err.to_string();
        assert!(msg.contains(&MAX_SESSIONS.to_string()), "{msg}");
        assert!(
            msg.contains("action=kill"),
            "the error names the next action: {msg}"
        );

        // Killing one frees a slot.
        reg.kill(&ids[0], Some(&owner_of(0))).await.expect("kill");
        let freed = spawn_cat_as(&reg, Some("owner-fresh"));
        assert!(reg.contains(&freed));
        for (i, id) in ids.iter().enumerate().skip(1) {
            reg.kill(id, Some(&owner_of(i))).await.expect("kill");
        }
        reg.kill(&freed, Some("owner-fresh")).await.expect("kill");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_per_owner_cap_refuses_one_conversation_without_locking_out_others() {
        // The registry is process-global: one conversation must not be able
        // to take every slot and lock every other channel out of sessions.
        let reg = ShellSessionRegistry::new();
        let mine: Vec<String> = (0..MAX_SESSIONS_PER_OWNER)
            .map(|_| spawn_cat_as(&reg, Some("owner-a")))
            .collect();

        let mut cmd = Command::new("cat");
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = cmd.spawn().expect("spawn cat");
        let msg = reg
            .register(child, None, TEST_CAP, Some("owner-a".into()))
            .expect_err("the per-owner cap must refuse one more session")
            .to_string();
        assert!(msg.contains(&MAX_SESSIONS_PER_OWNER.to_string()), "{msg}");
        assert!(msg.contains("action=kill"), "{msg}");

        // Another conversation is unaffected by owner-a's sessions.
        let theirs = spawn_cat_as(&reg, Some("owner-b"));
        assert!(reg.contains(&theirs));

        // Killing one of owner-a's frees a slot for owner-a.
        reg.kill(&mine[0], Some("owner-a")).await.expect("kill");
        let freed = spawn_cat_as(&reg, Some("owner-a"));
        assert!(reg.contains(&freed));

        for id in mine.iter().skip(1) {
            reg.kill(id, Some("owner-a")).await.expect("kill");
        }
        reg.kill(&freed, Some("owner-a")).await.expect("kill");
        reg.kill(&theirs, Some("owner-b")).await.expect("kill");
    }

    /// Spawn `sh -c <script>` as a session with `cap` bytes of retained output.
    fn spawn_sh(reg: &ShellSessionRegistry, script: &str, cap: usize) -> String {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        crate::util::process_tree::spawn_in_own_group(&mut cmd);
        let child = cmd.spawn().expect("spawn sh");
        reg.register(child, None, cap, None).expect("register")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_exit_poll_carries_every_byte_written_before_exit() {
        // The drain appends its last bytes and then finishes. A poll that took
        // the buffer first and checked the drain second could see "finished"
        // for bytes that landed in between — and the removal dropped them.
        // Flood output right before the exit so that window is as wide as the
        // real thing gets; a cut-off tail shows up as a missing last line.
        let reg = ShellSessionRegistry::new();
        let id = spawn_sh(&reg, "seq 1 20000; exit 3", 1024 * 1024);
        let out = poll_until(&reg, &id, "exited with code 3").await;
        assert!(
            out.contains("\n19999\n20000\n"),
            "the tail of the output was cut off before the exit note: {:?}",
            &out[out.len().saturating_sub(200)..]
        );
        assert!(!out.contains("omitted"), "nothing may be dropped: {out:?}");
        assert!(!reg.contains(&id));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_exit_poll_still_reports_the_env_mask_flag() {
        // `poll` removes an exited session, so a flag looked up by id after the
        // poll is gone with it — the last chunk of `cat .env; exit` would go
        // out unmasked. The flag must travel with the output instead.
        let reg = ShellSessionRegistry::new();
        let id = spawn_sh(&reg, "echo KEY=value", TEST_CAP);
        reg.mark_env_mask(&id, None);
        let mut saw_exit = false;
        for _ in 0..50 {
            let polled = reg.poll(&id, None).expect("poll");
            assert!(
                polled.env_mask,
                "every poll after the mark carries the flag"
            );
            if polled.output.contains("exited with code 0") {
                saw_exit = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(saw_exit, "the session never reported its exit");
        assert!(!reg.contains(&id));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdin_roundtrips_through_poll() {
        // Real pipe round-trip: write to a live `cat`, read the echo back.
        let reg = ShellSessionRegistry::new();
        let id = spawn_cat(&reg);
        assert!(reg.contains(&id), "session should be registered");

        reg.write_stdin(&id, "hello\n", None).await.expect("write");
        let out = poll_until(&reg, &id, "hello").await;
        assert!(
            out.contains("hello"),
            "echoed stdin should surface: {out:?}"
        );

        reg.kill(&id, None).await.expect("kill");
        assert!(!reg.contains(&id), "killed session is gone");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn poll_returns_only_new_output_each_call() {
        // `poll` advances the read offset, so a second poll with no new input
        // returns empty — the model isn't re-shown bytes it already saw.
        let reg = ShellSessionRegistry::new();
        let id = spawn_cat(&reg);

        reg.write_stdin(&id, "first\n", None).await.expect("write");
        let _ = poll_until(&reg, &id, "first").await;
        // Nothing new written; next poll must be empty.
        let second = reg.poll(&id, None).expect("poll").output;
        assert_eq!(second, "", "second poll with no new output must be empty");

        reg.write_stdin(&id, "second\n", None).await.expect("write");
        let out = poll_until(&reg, &id, "second").await;
        assert!(out.contains("second"));
        assert!(
            !out.contains("first"),
            "poll must not re-emit old output: {out:?}"
        );

        reg.kill(&id, None).await.expect("kill");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cwd_persists_across_commands_in_a_session() {
        // A persistent session is one long-lived shell process, so a `cd` in one
        // command must affect the next — the session must NOT snap back to its
        // spawn-time cwd. This pins the invariant that makes per-command cwd
        // re-injection (and the resulting clobber bug) impossible here.
        let reg = ShellSessionRegistry::new();
        let child = Command::new("sh")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir("/") // spawn-time cwd is `/`
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sh");
        let id = reg.register(child, None, TEST_CAP, None).expect("register");

        // Move somewhere distinct from the spawn-time cwd, then ask where we are
        // on a *separate* command. A fresh-process-per-command shell would
        // report `/`; a true persistent session reports `/tmp`.
        reg.write_stdin(&id, "cd /tmp\n", None)
            .await
            .expect("write cd");
        reg.write_stdin(&id, "pwd\n", None)
            .await
            .expect("write pwd");
        let out = poll_until(&reg, &id, "/tmp").await;
        assert!(
            out.contains("/tmp"),
            "cd must persist to the next command in the same session: {out:?}"
        );

        reg.kill(&id, None).await.expect("kill");
    }

    #[test]
    fn session_output_retention_is_bounded_and_keeps_newest() {
        // A session that floods output must not grow without limit: retention
        // stays within the lazy-compaction ceiling (2x cap) no matter how much
        // is written, and what survives is the most recent output.
        let cap = 1024;
        let mut out = SessionOutput::new(cap);
        for i in 0..1000 {
            out.push(&format!("line {i}\n"));
            assert!(
                out.text.len() <= cap * 2,
                "retention {} exceeded 2x cap after {i} pushes",
                out.text.len()
            );
        }
        let taken = out.take();
        assert!(
            taken.contains("line 999"),
            "newest output must survive: {taken:?}"
        );
        assert!(
            !taken.contains("line 0\n"),
            "oldest output should have been dropped"
        );
    }

    #[test]
    fn dropped_session_output_is_reported_not_silently_truncated() {
        let cap = 64;
        let mut out = SessionOutput::new(cap);
        let total = 4096;
        out.push(&"x".repeat(total));
        let retained = out.text.len();
        let dropped = out.dropped;
        assert_eq!(
            dropped + retained,
            total,
            "every byte is either retained or counted as dropped"
        );

        let taken = out.take();
        assert!(
            taken.starts_with(&format!("... {dropped} bytes omitted ...\n")),
            "elision must be surfaced with an exact count: {:?}",
            &taken[..taken.len().min(64)]
        );
    }

    #[test]
    fn session_output_under_cap_round_trips_exactly() {
        // The common case — a session that stays under budget — must be byte-
        // for-byte intact with no marker.
        let mut out = SessionOutput::new(1024);
        out.push("hello ");
        out.push("world\n");
        assert_eq!(out.take(), "hello world\n");
    }

    #[test]
    fn session_output_take_drains_so_output_is_not_re_emitted() {
        let mut out = SessionOutput::new(1024);
        out.push("first\n");
        assert_eq!(out.take(), "first\n");
        assert_eq!(out.take(), "", "drained buffer must not re-emit");
        out.push("second\n");
        assert_eq!(out.take(), "second\n");
    }

    #[test]
    fn session_output_trim_preserves_utf8_boundaries() {
        // A cut that lands mid-codepoint must advance FORWARD to the next
        // boundary. Walking backward would keep more than `cap`; not walking at
        // all would panic in `String::drain`. Sizes are chosen so the cut is
        // never already aligned, making the boundary walk observable.
        const CAP: usize = 10;
        let mut out = SessionOutput::new(CAP);
        // 8 x 4-byte codepoints = 32 bytes pushed, none of them cap-aligned.
        for _ in 0..8 {
            out.push("😀");
        }
        let taken = out.take();
        // 24 dropped + 8 retained = the 32 bytes pushed: nothing is unaccounted
        // for, and the retained tail is exactly two whole codepoints.
        assert_eq!(taken, "... 24 bytes omitted ...\n😀😀");
    }

    #[test]
    fn poll_never_returns_more_than_the_cap() {
        // `push` compacts lazily at 2x cap for amortization, but that must not
        // leak into what a caller receives: the operator sets
        // `security.max_shell_output_bytes` to bound context cost, and the
        // one-shot path treats it as a hard limit.
        const CAP: usize = 1024;
        let mut out = SessionOutput::new(CAP);
        for _ in 0..64 {
            out.push(&"x".repeat(30));
        }
        let taken = out.take();
        let marker_end = taken.find('\n').expect("omission marker present") + 1;
        assert_eq!(
            taken.len() - marker_end,
            CAP,
            "retained payload must be exactly the cap, not 2x it"
        );
    }

    /// A `cd` inside a live session must surface a `cwd: <path>` note on the
    /// next poll — that's what lets the model drop defensive `cd X &&`
    /// prefixes — and a session that never changes directory must never get
    /// one. Platform-gated to where `process_cwd` is implemented.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn poll_notes_cwd_only_when_it_changed() {
        let reg = ShellSessionRegistry::new();
        let child = Command::new("sh")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir("/")
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sh");
        let id = reg.register(child, None, TEST_CAP, None).expect("register");

        // No directory change yet: output-producing polls carry no cwd note.
        reg.write_stdin(&id, "echo before\n", None)
            .await
            .expect("write");
        let out = poll_until(&reg, &id, "before").await;
        assert!(
            !out.contains("cwd: "),
            "no cwd note while the directory is unchanged: {out:?}"
        );

        // The target must be symlink-free: the kernel reports the resolved
        // path (`/tmp` on macOS is a symlink to `/private/tmp`).
        let target = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp dir");
        reg.write_stdin(&id, &format!("cd {}\necho moved\n", target.display()), None)
            .await
            .expect("write cd");
        let out = poll_until(&reg, &id, "cwd: ").await;
        assert!(
            out.contains(&format!("cwd: {}", target.display())),
            "poll after a cd must note the new cwd: {out:?}"
        );

        // Once noted, an unchanged cwd produces no further notes.
        reg.write_stdin(&id, "echo after\n", None)
            .await
            .expect("write");
        let out = poll_until(&reg, &id, "after").await;
        assert!(
            !out.contains("cwd: "),
            "cwd note must appear only on change, not on every poll: {out:?}"
        );

        reg.kill(&id, None).await.expect("kill");
    }

    /// A multibyte character split across two separate pipe reads must reach
    /// the session buffer intact — per-chunk lossy decoding would turn the
    /// split 😀 into two U+FFFD.
    #[cfg(unix)]
    #[tokio::test]
    async fn split_multibyte_output_survives_chunked_drain() {
        // 😀 is F0 9F 98 80. `write_stdin` is &str-typed, so the split is
        // produced inside the session: two `printf` commands emit the first
        // two bytes and the last two bytes far enough apart that the drain
        // task reads them as separate chunks.
        let reg = ShellSessionRegistry::new();
        let child = Command::new("sh")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sh");
        let id = reg.register(child, None, TEST_CAP, None).expect("register");
        // First half of the emoji, no newline — lands as its own drain chunk.
        reg.write_stdin(&id, "printf '\\360\\237'\n", None)
            .await
            .expect("write first half");
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Second half completes the codepoint on a later read.
        reg.write_stdin(&id, "printf '\\230\\200\\n'\n", None)
            .await
            .expect("write second half");

        let out = poll_until(&reg, &id, "😀").await;
        assert!(
            out.contains('😀'),
            "split emoji must reassemble across drain chunks: {out:?}"
        );
        assert!(
            !out.contains('\u{FFFD}'),
            "no replacement chars for a cleanly split codepoint: {out:?}"
        );

        reg.kill(&id, None).await.expect("kill");
    }

    #[tokio::test]
    async fn operations_on_unknown_session_error() {
        let reg = ShellSessionRegistry::new();
        assert!(!reg.contains("sh-404"));
        assert!(reg.poll("sh-404", None).is_err());
        assert!(reg.write_stdin("sh-404", "x", None).await.is_err());
        assert!(reg.kill("sh-404", None).await.is_err());
    }

    /// The registry is process-global, so ownership is the only boundary
    /// between conversations: a caller with a different (or absent) session id
    /// must not write to, read from, or kill a session it did not start — and
    /// the refusal must be indistinguishable from "no such session" so it
    /// cannot be used as an existence oracle.
    #[cfg(unix)]
    #[tokio::test]
    async fn foreign_caller_cannot_drive_an_owned_session() {
        let reg = ShellSessionRegistry::new();
        let mut cmd = Command::new("cat");
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        crate::util::process_tree::spawn_in_own_group(&mut cmd);
        let child = cmd.spawn().expect("spawn cat");
        let id = reg
            .register(child, None, TEST_CAP, Some("owner-a".into()))
            .expect("register");

        for caller in [Some("owner-b"), None] {
            let write = reg.write_stdin(&id, "stolen\n", caller).await;
            let poll = reg.poll(&id, caller);
            let kill = reg.kill(&id, caller).await;
            for (verb, msg) in [
                (
                    "write_stdin",
                    write.expect_err("write must be refused").to_string(),
                ),
                ("poll", poll.expect_err("poll must be refused").to_string()),
                ("kill", kill.expect_err("kill must be refused").to_string()),
            ] {
                assert_eq!(
                    msg,
                    format!("no such shell session: {id}"),
                    "{verb} refusal for caller {caller:?} must not reveal the session exists"
                );
            }
        }
        // The refused kill must not have torn the session down.
        assert!(
            reg.contains(&id),
            "foreign kill must not remove the session"
        );

        // The owner still has full access.
        reg.write_stdin(&id, "mine\n", Some("owner-a"))
            .await
            .expect("owner write");
        let out = poll_until_as(&reg, &id, "mine", Some("owner-a")).await;
        assert!(out.contains("mine"), "owner poll must still work: {out:?}");
        reg.kill(&id, Some("owner-a")).await.expect("owner kill");
    }

    /// An ownerless session (started outside a conversation) is not driveable
    /// by a caller that HAS a session id — ownership matches exactly in both
    /// directions.
    #[cfg(unix)]
    #[tokio::test]
    async fn owned_caller_cannot_drive_an_ownerless_session() {
        let reg = ShellSessionRegistry::new();
        let id = spawn_cat(&reg); // registered with owner=None
        assert!(reg.write_stdin(&id, "x\n", Some("owner-a")).await.is_err());
        assert!(reg.poll(&id, Some("owner-a")).is_err());
        assert!(reg.kill(&id, Some("owner-a")).await.is_err());
        reg.kill(&id, None).await.expect("ownerless kill");
    }

    /// Session ids must not be guessable: the counter alone (`sh-1`, `sh-2`)
    /// let a foreign conversation enumerate live sessions. Each id must carry
    /// a random suffix.
    #[cfg(unix)]
    #[tokio::test]
    async fn session_ids_carry_a_random_suffix() {
        let reg = ShellSessionRegistry::new();
        let id = spawn_cat(&reg);
        let parts: Vec<&str> = id.splitn(3, '-').collect();
        assert_eq!(parts[0], "sh", "id keeps the sh- prefix: {id}");
        assert!(
            parts.len() == 3
                && parts[2].len() == 8
                && parts[2].chars().all(|c| c.is_ascii_hexdigit()),
            "id must end in an 8-hex-digit random suffix: {id}"
        );
        reg.kill(&id, None).await.expect("kill");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_terminates_descendant_process_group() {
        // A session whose command backgrounds a long-lived grandchild. Killing
        // the session must take down the whole process group, not just the
        // direct child — otherwise the `sleep` leaks.
        let reg = ShellSessionRegistry::new();
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 300 & echo $!; wait")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Mirror the production spawn: the child must lead its own group.
        crate::util::process_tree::spawn_in_own_group(&mut cmd);
        let child = cmd.spawn().expect("spawn sh");
        let id = reg.register(child, None, TEST_CAP, None).expect("register");

        // The shell echoes the backgrounded grandchild's PID.
        let out = poll_until(&reg, &id, "\n").await;
        let gpid: i32 = out
            .lines()
            .next()
            .and_then(|l| l.trim().parse().ok())
            .unwrap_or_else(|| panic!("expected a grandchild pid, got {out:?}"));
        assert!(
            crate::util::process_tree::pid_is_alive(gpid),
            "descendant should be alive before kill"
        );

        reg.kill(&id, None).await.expect("kill");

        // The descendant must be reaped shortly after the session is killed.
        assert!(
            crate::util::process_tree::wait_for_pid_exit(gpid).await,
            "descendant pid {gpid} survived session kill — process group leaked"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_ids_are_unique_and_monotonic() {
        let reg = ShellSessionRegistry::new();
        let a = spawn_cat(&reg);
        let b = spawn_cat(&reg);
        assert_ne!(a, b, "each session gets a distinct id");
        reg.kill(&a, None).await.expect("kill a");
        reg.kill(&b, None).await.expect("kill b");
    }
}
