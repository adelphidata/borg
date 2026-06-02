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

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin};
use tokio::sync::Mutex as AsyncMutex;

use crate::process_registry::ProcessGuard;

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
    /// Accumulated stdout+stderr. Appended by the drain task; sliced by `poll`.
    output: Arc<Mutex<String>>,
    /// Byte offset already returned by a previous `poll`. Only ever advances to
    /// a prior `output` length, which is always a UTF-8 char boundary.
    polled_offset: usize,
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
    /// Construct an empty registry.
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
    pub fn register(&self, mut child: Child, guard: Option<ProcessGuard>) -> String {
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = child.stdin.take();
        let output = Arc::new(Mutex::new(String::new()));
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
                polled_offset: 0,
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

    /// Return output accumulated since the last `poll` for this session and
    /// advance the read offset. Errors if the session is unknown.
    pub fn poll(&self, id: &str) -> Result<String> {
        let mut inner = self.lock();
        let session = inner
            .sessions
            .get_mut(id)
            .ok_or_else(|| anyhow!("no such shell session: {id}"))?;
        let (new, new_len) = {
            let buf = session
                .output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (buf[session.polled_offset..].to_string(), buf.len())
        };
        session.polled_offset = new_len;
        Ok(new)
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

/// Send `sig` to every process in `pgid`. Best-effort: `ESRCH` (group already
/// gone) and other failures are ignored — the caller's `wait` reaps the child.
#[cfg(unix)]
fn signal_group(pgid: i32, sig: i32) {
    // SAFETY: FFI call to `killpg`. `pgid` is our own child's process group
    // (the child leads its group via `process_group(0)` at spawn), so we never
    // signal an unrelated group. A non-zero return (e.g. ESRCH) is ignored.
    unsafe {
        libc::killpg(pgid, sig);
    }
}

/// Spawn the detached task that reads both pipes into the shared buffer until
/// both reach EOF. Raw byte reads (not line reads) so partial output — e.g. a
/// REPL prompt with no trailing newline — still surfaces on the next `poll`.
fn spawn_drain(
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    output: Arc<Mutex<String>>,
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

fn append(output: &Arc<Mutex<String>>, bytes: &[u8]) {
    let chunk = String::from_utf8_lossy(bytes);
    let mut buf = output
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    buf.push_str(&chunk);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::process::Command;

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
        reg.register(child, None)
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
        let id = reg.register(child, None);

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
        let id = reg.register(child, None);

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
