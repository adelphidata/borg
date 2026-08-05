//! Process-group helpers for subprocesses we may need to abandon.
//!
//! Tokio's `kill_on_drop` only reaches the *direct* child. Anything that child
//! spawned — a shell's background jobs, git's credential helper or fsmonitor —
//! survives and is reparented to init. Spawning the child as its own process
//! group leader (PGID == PID) lets us signal the whole tree instead.

/// Spawn `cmd`'s child as the leader of a fresh process group so
/// [`kill_process_group`] can reach its descendants.
///
/// Establishes the PGID == PID invariant that every `killpg` in this crate
/// relies on: without it, the pid we later pass to `killpg` names some
/// *unrelated* group, so the signal is at best a no-op.
///
/// Tradeoff: a child in its own group is no longer in the terminal's
/// foreground group, so a terminal-generated SIGINT (Ctrl-C) no longer reaches
/// it. That is acceptable for the commands we apply this to — they are driven
/// by our own timeout/cancellation, not by the operator's terminal.
///
/// No-op off Unix, where process groups don't exist in this form.
pub(crate) fn spawn_in_own_group(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(not(unix))]
    let _ = cmd;
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
/// Requires the child to have been spawned via [`spawn_in_own_group`]; `None`
/// (the child already exited, so its pid is gone) is a no-op.
pub(crate) fn kill_process_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        signal_group(pid as i32, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
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

        // SAFETY: signal 0 only probes for existence; it never delivers.
        assert_eq!(
            unsafe { libc::kill(grandchild, 0) },
            0,
            "precondition: grandchild {grandchild} should be alive"
        );

        kill_process_group(pid);
        drop(child);

        // SIGKILL delivery is asynchronous; poll briefly rather than racing it.
        let mut gone = false;
        for _ in 0..100 {
            // SAFETY: existence probe only, as above.
            if unsafe { libc::kill(grandchild, 0) } != 0 {
                gone = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(gone, "grandchild {grandchild} survived the group kill");
    }
}
