//! Process-group helpers for subprocesses we may need to abandon.
//!
//! Tokio's `kill_on_drop` only reaches the *direct* child. Anything that child
//! spawned — a shell's background jobs, git's credential helper or fsmonitor —
//! survives and is reparented to init. Spawning the child as its own process
//! group leader (PGID == PID) lets us signal the whole tree instead.

/// Spawn `cmd`'s child as the leader of a fresh process group so
/// [`kill_process_group`] can reach its descendants.
///
/// Establishes the PGID == PID invariant every `killpg` here relies on; without
/// it the pid we later signal names some *unrelated* group.
///
/// Tradeoff: a child in its own group is no longer in the terminal's
/// foreground group, so a terminal-generated SIGINT (Ctrl-C) no longer reaches
/// it. That is acceptable for the commands we apply this to — they are driven
/// by our own timeout/cancellation, not by the operator's terminal.
pub(crate) fn spawn_in_own_group(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(not(unix))]
    let _ = cmd;
}

/// [`spawn_in_own_group`] for blocking `std::process` spawns (hooks run on a
/// dedicated thread, not the tokio runtime).
pub(crate) fn spawn_in_own_group_std(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

/// Wait for a [`spawn_in_own_group_std`]-spawned child until `budget` elapses;
/// on expiry, kill its whole process group and report `TimedOut`.
///
/// Ordering is load-bearing: the group kill must land *before* `child.kill()`
/// / `child.wait()`, because the wait reaps the pid and a recycled pgid would
/// then be signalled. Callers must drain the child's pipes on their own
/// threads *while* this polls — draining after exit deadlocks any child whose
/// output exceeds the pipe buffer.
///
/// Polls with exponential backoff (0.5 ms doubling to 25 ms) so short-lived
/// commands don't pay a fixed sleep floor on hot paths.
pub(crate) fn wait_or_kill_group(
    child: &mut std::process::Child,
    budget: std::time::Duration,
) -> std::io::Result<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + budget;
    let mut poll = std::time::Duration::from_micros(500);
    loop {
        match child.try_wait()? {
            Some(status) => return Ok(status),
            None => {
                if std::time::Instant::now() >= deadline {
                    kill_process_group(Some(child.id()));
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("command timed out after {budget:?}"),
                    ));
                }
                std::thread::sleep(poll);
                poll = (poll * 2).min(std::time::Duration::from_millis(25));
            }
        }
    }
}

/// Send `sig` to every process in `pgid`.
///
/// Best-effort: `ESRCH` (the group is already gone) and every other failure are
/// ignored — the caller's `wait` is what actually reaps the direct child.
#[cfg(unix)]
pub(crate) fn signal_group(pgid: i32, sig: i32) {
    // SAFETY: FFI call to `killpg`. `pgid` is our own child's process group —
    // the child leads its group via `spawn_in_own_group` at spawn time — so we
    // never signal an unrelated group. A non-zero return is ignored.
    unsafe {
        libc::killpg(pgid, sig);
    }
}

/// SIGKILL the process group led by `pid`, for the abandon path where there is
/// no one left to wait on the tree (a timed-out command whose future we drop).
///
/// Requires the child to have been spawned via [`spawn_in_own_group`].
pub(crate) fn kill_process_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        #[cfg(unix)]
        signal_group(pid as i32, libc::SIGKILL);
        #[cfg(not(unix))]
        let _ = pid;
    }
}

/// Whether `pid` still exists. Signal 0 performs the permission and existence
/// checks without delivering anything.
///
/// Note a killed-but-unreaped child is a zombie and still "alive" here, so
/// prefer probing a *grandchild* when asserting that a group kill worked.
#[cfg(all(test, unix))]
pub(crate) fn pid_is_alive(pid: i32) -> bool {
    // SAFETY: FFI call to `kill` with signal 0 — an existence probe that never
    // delivers a signal and cannot affect the target.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Poll until `pid` disappears, up to ~2s. SIGKILL delivery is asynchronous, so
/// tests must not race it with a bare probe.
#[cfg(all(test, unix))]
pub(crate) async fn wait_for_pid_exit(pid: i32) -> bool {
    for _ in 0..100 {
        if !pid_is_alive(pid) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The whole point of the module: a child that backgrounds a grandchild and
    /// is then abandoned must not leave that grandchild running. `kill_on_drop`
    /// alone only reaps the direct child, so without `spawn_in_own_group` the
    /// grandchild survives.
    #[tokio::test]
    async fn kill_process_group_reaps_grandchildren() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 30 & echo $!; wait")
            .stdout(std::process::Stdio::piped());
        spawn_in_own_group(&mut cmd);
        let mut child = cmd.kill_on_drop(true).spawn().expect("spawn sh");
        let pid = child.id();

        // Read the grandchild's pid off stdout before abandoning the tree.
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut buf = Vec::new();
        let grandchild: i32 = {
            use tokio::io::AsyncReadExt;
            let mut byte = [0u8; 1];
            while stdout.read_exact(&mut byte).await.is_ok() {
                if byte[0] == b'\n' {
                    break;
                }
                buf.push(byte[0]);
            }
            String::from_utf8_lossy(&buf).trim().parse().expect("pid")
        };

        assert!(
            pid_is_alive(grandchild),
            "precondition: grandchild {grandchild} should be alive"
        );

        kill_process_group(pid);
        drop(child);

        assert!(
            wait_for_pid_exit(grandchild).await,
            "grandchild {grandchild} survived the group kill"
        );
    }
}
