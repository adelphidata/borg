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
    /// On Unix the caller MUST also spawn with `process_group(0)` so the child
    /// leads its own group (PGID == PID): [`Self::kill`] signals the whole group
    /// via `killpg`, which is only safe — and only reaches descendants — when
    /// that holds. Without it, `kill` would signal an unrelated group, so it
    /// instead falls back to signaling just the direct child.
    ///
    /// `max_output_bytes` caps the unread output retained between polls; the
    /// caller resolves it from `security.max_shell_output_bytes` so sessions and
    /// one-shot commands share one budget.
    pub fn register(
        &self,
        mut child: Child,
        guard: Option<ProcessGuard>,
        max_output_bytes: usize,
    ) -> String {
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = child.stdin.take();
        let output = Arc::new(Mutex::new(SessionOutput::new(max_output_bytes)));
        let drain = spawn_drain(stdout, stderr, output.clone());

        let mut inner = self.lock();
        inner.next_id = inner.next_id.wrapping_add(1).max(1);
        let id = format!("sh-{}", inner.next_id);
        // Ids are monotonic (u64); a collision would mean a wrap past a still-live
        // session — practically unreachable, but assert the invariant rather than
        // silently `insert`-overwriting (and thus killing) a live session.
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
            },
        );
        id
    }

    /// Whether a session with `id` is currently registered.
    pub fn contains(&self, id: &str) -> bool {
        self.lock().sessions.contains_key(id)
    }

    /// Write `data` to the session's stdin and flush. Errors if the session is
    /// unknown or its stdin has already closed.
    pub async fn write_stdin(&self, id: &str, data: &str) -> Result<()> {
        let stdin = {
            let inner = self.lock();
            inner
                .sessions
                .get(id)
                .map(|s| s.stdin.clone())
                .ok_or_else(|| anyhow!("no such shell session: {id}"))?
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
    /// draining it so nothing is re-emitted or retained afterwards. Errors if
    /// the session is unknown.
    pub fn poll(&self, id: &str) -> Result<String> {
        let inner = self.lock();
        let session = inner
            .sessions
            .get(id)
            .ok_or_else(|| anyhow!("no such shell session: {id}"))?;
        let mut buf = session
            .output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(buf.take())
    }

    /// Terminate a session: abort its drain task, kill the child's whole
    /// process group, reap it, and finalize the subprocess-registry guard.
    /// Errors if the session is unknown.
    pub async fn kill(&self, id: &str) -> Result<()> {
        let session = {
            let mut inner = self.lock();
            inner.sessions.remove(id)
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

/// Terminate a session child and reap it so no zombie or descendant leaks.
///
/// On Unix the child leads its own process group (set via `process_group(0)`
/// at spawn), so we signal the entire group: SIGTERM, a short grace period for
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
            // forever even when the `process_group(0)` contract was not met.
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
        while !sdone || !edone {
            tokio::select! {
                r = async {
                    match stdout.as_mut() {
                        Some(p) => p.read(&mut sbuf).await,
                        None => std::future::pending().await,
                    }
                }, if !sdone => {
                    match r {
                        Ok(0) | Err(_) => sdone = true,
                        Ok(n) => append(&output, &sbuf[..n]),
                    }
                }
                r = async {
                    match stderr.as_mut() {
                        Some(p) => p.read(&mut ebuf).await,
                        None => std::future::pending().await,
                    }
                }, if !edone => {
                    match r {
                        Ok(0) | Err(_) => edone = true,
                        Ok(n) => append(&output, &ebuf[..n]),
                    }
                }
            }
        }
    })
}

fn append(output: &Arc<Mutex<SessionOutput>>, bytes: &[u8]) {
    let chunk = String::from_utf8_lossy(bytes);
    let mut buf = output
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    buf.push(&chunk);
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
    /// `process_group(0)` so `kill`'s group signal reaches the child.
    fn spawn_cat(reg: &ShellSessionRegistry) -> String {
        let child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0)
            .spawn()
            .expect("spawn cat");
        reg.register(child, None, TEST_CAP)
    }

    /// Poll until `needle` shows up or we give up — drain is async, so the
    /// echo may not have landed by the first poll. Accumulates across polls
    /// because each `poll` only returns *new* bytes.
    async fn poll_until(reg: &ShellSessionRegistry, id: &str, needle: &str) -> String {
        let mut acc = String::new();
        for _ in 0..50 {
            acc.push_str(&reg.poll(id).expect("poll"));
            if acc.contains(needle) {
                return acc;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        acc
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdin_roundtrips_through_poll() {
        // Real pipe round-trip: write to a live `cat`, read the echo back.
        let reg = ShellSessionRegistry::new();
        let id = spawn_cat(&reg);
        assert!(reg.contains(&id), "session should be registered");

        reg.write_stdin(&id, "hello\n").await.expect("write");
        let out = poll_until(&reg, &id, "hello").await;
        assert!(
            out.contains("hello"),
            "echoed stdin should surface: {out:?}"
        );

        reg.kill(&id).await.expect("kill");
        assert!(!reg.contains(&id), "killed session is gone");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn poll_returns_only_new_output_each_call() {
        // `poll` advances the read offset, so a second poll with no new input
        // returns empty — the model isn't re-shown bytes it already saw.
        let reg = ShellSessionRegistry::new();
        let id = spawn_cat(&reg);

        reg.write_stdin(&id, "first\n").await.expect("write");
        let _ = poll_until(&reg, &id, "first").await;
        // Nothing new written; next poll must be empty.
        let second = reg.poll(&id).expect("poll");
        assert_eq!(second, "", "second poll with no new output must be empty");

        reg.write_stdin(&id, "second\n").await.expect("write");
        let out = poll_until(&reg, &id, "second").await;
        assert!(out.contains("second"));
        assert!(
            !out.contains("first"),
            "poll must not re-emit old output: {out:?}"
        );

        reg.kill(&id).await.expect("kill");
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
        let id = reg.register(child, None, TEST_CAP);

        // Move somewhere distinct from the spawn-time cwd, then ask where we are
        // on a *separate* command. A fresh-process-per-command shell would
        // report `/`; a true persistent session reports `/tmp`.
        reg.write_stdin(&id, "cd /tmp\n").await.expect("write cd");
        reg.write_stdin(&id, "pwd\n").await.expect("write pwd");
        let out = poll_until(&reg, &id, "/tmp").await;
        assert!(
            out.contains("/tmp"),
            "cd must persist to the next command in the same session: {out:?}"
        );

        reg.kill(&id).await.expect("kill");
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

    #[tokio::test]
    async fn operations_on_unknown_session_error() {
        let reg = ShellSessionRegistry::new();
        assert!(!reg.contains("sh-404"));
        assert!(reg.poll("sh-404").is_err());
        assert!(reg.write_stdin("sh-404", "x").await.is_err());
        assert!(reg.kill("sh-404").await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_terminates_descendant_process_group() {
        // A session whose command backgrounds a long-lived grandchild. Killing
        // the session must take down the whole process group, not just the
        // direct child — otherwise the `sleep` leaks.
        let reg = ShellSessionRegistry::new();
        let child = Command::new("sh")
            .arg("-c")
            .arg("sleep 300 & echo $!; wait")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0) // mirror the production spawn: child leads its group
            .spawn()
            .expect("spawn sh");
        let id = reg.register(child, None, TEST_CAP);

        // The shell echoes the backgrounded grandchild's PID.
        let out = poll_until(&reg, &id, "\n").await;
        let gpid: i32 = out
            .lines()
            .next()
            .and_then(|l| l.trim().parse().ok())
            .unwrap_or_else(|| panic!("expected a grandchild pid, got {out:?}"));
        // SAFETY: signal 0 only probes for existence; no signal is delivered.
        assert_eq!(
            unsafe { libc::kill(gpid, 0) },
            0,
            "descendant should be alive before kill"
        );

        reg.kill(&id).await.expect("kill");

        // The descendant must be reaped shortly after the session is killed.
        let mut dead = false;
        for _ in 0..100 {
            if unsafe { libc::kill(gpid, 0) } != 0 {
                dead = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            dead,
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
        reg.kill(&a).await.expect("kill a");
        reg.kill(&b).await.expect("kill b");
    }
}
