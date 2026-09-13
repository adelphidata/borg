//! Process-group helpers for subprocesses we may need to abandon.
//!
//! Tokio's `kill_on_drop` only reaches the *direct* child. Anything that child
//! spawned — a shell's background jobs, git's credential helper or fsmonitor —
//! survives and is reparented to init. Spawning the child as its own session
//! leader (PGID == PID) lets us signal the whole tree instead.

/// Spawn `cmd`'s child as the leader of a fresh session so
/// [`kill_process_group`] can reach its descendants.
///
/// Establishes the PGID == PID invariant every `killpg` here relies on; without
/// it the pid we later signal names some *unrelated* group.
///
/// Uses `setsid` rather than a plain `process_group(0)` so the child also loses
/// our controlling terminal — see [`sandbox::process_group::detach_from_tty`]
/// for why a background process group hangs on `/dev/tty`.
///
/// Tradeoff: a child outside our group is no longer in the terminal's
/// foreground group, so a terminal-generated SIGINT (Ctrl-C) no longer reaches
/// it. That is acceptable for the commands we apply this to — they are driven
/// by our own timeout/cancellation, not by the operator's terminal.
pub(crate) fn spawn_in_own_group(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    // SAFETY: `detach_from_tty` only calls `setsid`/`setpgid`, both
    // async-signal-safe, as `pre_exec` requires.
    unsafe {
        cmd.pre_exec(sandbox::process_group::detach_from_tty);
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

/// [`spawn_in_own_group`] for blocking `std::process` spawns (hooks run on a
/// dedicated thread, not the tokio runtime).
pub(crate) fn spawn_in_own_group_std(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: as above — `detach_from_tty` is async-signal-safe.
        unsafe {
            cmd.pre_exec(sandbox::process_group::detach_from_tty);
        }
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
                // Capped backoff: short-lived commands still reap in a few ms,
                // while a long-running one costs at most 40 wakeups/sec.
                poll = (poll * 2).min(std::time::Duration::from_millis(25));
            }
        }
    }
}

/// Send `sig` to every process in `pgid`.
///
/// Best-effort: `ESRCH` (the group is already gone) is ignored — the caller's
/// `wait` is what actually reaps the direct child.
///
/// Delegates to [`sandbox::process_group::signal_group_or_members`], which
/// retries per-member when macOS refuses the group call with `EPERM`.
/// Signalling the group and dropping the result silently left every descendant
/// alive on Borg's primary platform.
#[cfg(unix)]
pub(crate) fn signal_group(pgid: i32, sig: i32) {
    sandbox::process_group::signal_group_or_members(pgid, sig);
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

/// Current working directory of a live process, if the platform can report it.
///
/// Used by persistent shell sessions to notice when a `cd` moved the session
/// somewhere new. Best-effort by design: `None` (unsupported platform, dead
/// pid, or a failed kernel query) simply means "unknown" and callers must
/// treat it as "no observable change", never as an error.
#[cfg(target_os = "linux")]
pub(crate) fn process_cwd(pid: u32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// Current working directory of a live process, if the platform can report it.
///
/// macOS: `proc_pidinfo(PROC_PIDVNODEPATHINFO)`, which works without extra
/// entitlements for our own (same-uid) children.
#[cfg(target_os = "macos")]
pub(crate) fn process_cwd(pid: u32) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;

    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    // SAFETY: FFI call writing at most `size` bytes into `info`, which is a
    // plain-old-data struct we own; a non-positive return means the kernel
    // wrote nothing usable and we bail before reading it.
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    if written <= 0 {
        return None;
    }
    // `vip_path` is declared as `[[c_char; 32]; 32]` (a libc workaround for
    // `[c_char; MAXPATHLEN]`) — one contiguous NUL-terminated 1024-byte path.
    // SAFETY: reinterpreting the initialized, contiguous char array as bytes.
    let bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(info.pvi_cdir.vip_path.as_ptr().cast::<u8>(), 32 * 32)
    };
    let len = bytes.iter().position(|&b| b == 0)?;
    if len == 0 {
        return None;
    }
    Some(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(
        &bytes[..len],
    )))
}

/// Current working directory of a live process — unsupported on this
/// platform, so always `None` ("unknown", not an error).
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn process_cwd(_pid: u32) -> Option<std::path::PathBuf> {
    None
}

/// Whether `pid` still exists. Signal 0 performs the permission and existence
/// checks without delivering anything.
///
/// Note a killed-but-unreaped child is a zombie and still "alive" here, so
/// prefer probing a *grandchild* when asserting that a group kill worked.
/// Callers that need "still doing work" rather than "still has a pid" want
/// [`pid_is_running`].
#[cfg(all(test, unix))]
pub(crate) fn pid_is_alive(pid: i32) -> bool {
    // SAFETY: FFI call to `kill` with signal 0 — an existence probe that never
    // delivers a signal and cannot affect the target.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Whether `pid` names a process that is still *running* — exists and is not
/// a zombie.
///
/// `kill(pid, 0)` alone says yes for an exited-but-unreaped child: the kernel
/// keeps the pid until the parent waits. A registry or lock that trusts that
/// answer holds a dead process's slot forever (a task-runner lock is never
/// released; a shell entry is never swept). `EPERM` means the process exists
/// but is not ours to signal — counted as running, since a probe must never
/// fabricate a death. Zombie detection is best-effort: a platform or kernel
/// that cannot report process state falls back to the existence answer.
pub(crate) fn pid_is_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // A pid above `i32::MAX` would wrap negative and probe a process
        // *group*; no kernel hands out such a pid, so it names nothing.
        let Ok(pid_t) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: pure FFI; signal 0 delivers nothing and has no memory
        // preconditions.
        let rc = unsafe { libc::kill(pid_t, 0) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return matches!(err.raw_os_error(), Some(libc::EPERM));
        }
        !pid_is_zombie(pid).unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        // Borg targets unix for daemon workflows; keep non-unix builds
        // compiling without a liveness answer that could wrongly free a slot.
        true
    }
}

/// `Some(true)` when the kernel reports `pid` as a zombie; `None` when the
/// state cannot be read (gone between the probe and here, or no permission).
#[cfg(target_os = "linux")]
fn pid_is_zombie(pid: u32) -> Option<bool> {
    // `/proc/<pid>/stat`: the `comm` field can contain spaces and
    // parentheses, so the state letter is the first token after the final
    // `)`. `Z` is zombie; `X`/`x` is dead-and-going, same for our purposes.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    let state = after_comm.split_whitespace().next()?;
    Some(matches!(state, "Z" | "X" | "x"))
}

/// macOS keeps a zombie visible to `kill` but drops it from `proc_find`, so
/// `proc_pidinfo` answers `ESRCH` for a pid that `kill(pid, 0)` just
/// accepted. That disagreement *is* the zombie signal; there is no
/// `p_stat` to read because the query never reaches the struct. (`sysctl
/// KERN_PROC_PID` would report `SZOMB`, but libc does not ship the
/// `kinfo_proc` layout for this target.) Any other failure is "unknown".
#[cfg(target_os = "macos")]
fn pid_is_zombie(pid: u32) -> Option<bool> {
    use std::mem::{size_of, MaybeUninit};
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: pure FFI. `info` is a correctly-sized, zeroed `proc_bsdinfo`
    // buffer; `proc_pidinfo` writes at most `size` bytes into it and returns
    // the number written (== size on success, 0/-1 on failure).
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written == size {
        // SAFETY: `proc_pidinfo` filled the whole struct (checked above).
        let info = unsafe { info.assume_init() };
        return Some(info.pbi_status == libc::SZOMB);
    }
    if written > 0 {
        // Partial fill: no error was raised, so `errno` is stale and must
        // not be read — a leftover `ESRCH` would call a live process dead.
        return None;
    }
    let err = std::io::Error::last_os_error();
    (err.raw_os_error() == Some(libc::ESRCH)).then_some(true)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn pid_is_zombie(_pid: u32) -> Option<bool> {
    None
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

    /// Spec: an exited child the parent has not reaped is *not running*.
    /// `kill(pid, 0)` still succeeds for it — asserted here so the case is
    /// provably a zombie and not a pid that vanished — and a probe that
    /// stopped at that answer would hold the task lock / registry slot for
    /// as long as the corpse sat unreaped.
    #[test]
    fn an_unreaped_child_is_not_running() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        // `true` exits at once; we hold off `wait` so it lingers as a zombie.
        while pid_is_running(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "zombie child still reported as running after 5s"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            pid_is_alive(pid as i32),
            "the pid must still exist (unreaped) for this test to be about zombies"
        );
        assert!(pid_is_running(std::process::id()), "we are running");
        let _ = child.wait();

        // Positive control on a *child*: a probe that called every child
        // dead (a stale-errno read, say) would pass the zombie half above.
        let mut live = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        assert!(
            pid_is_running(live.id()),
            "a running child must be reported as running"
        );
        let _ = live.kill();
        let _ = live.wait();
    }

    /// Spec: a spawned child must land in a *new session*, not merely a new
    /// process group of ours.
    ///
    /// A child in a background process group of our terminal session takes
    /// `SIGTTIN`/`SIGTTOU` and stops the moment it touches `/dev/tty` — which
    /// an operator's shell startup file routinely does (`stty` guards). The
    /// command then never exits and we kill it at the timeout. Leaving the
    /// session is what removes the controlling terminal and the stop.
    ///
    /// Asserted on the session id rather than by driving a real pty so the test
    /// is meaningful under a test runner that has no controlling terminal.
    #[tokio::test]
    async fn a_spawned_child_leaves_our_terminal_session() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 30")
            .stdout(std::process::Stdio::null());
        spawn_in_own_group(&mut cmd);
        let mut child = cmd.kill_on_drop(true).spawn().expect("spawn sh");
        let pid = child.id().expect("child pid") as i32;

        // SAFETY: FFI calls taking no pointers.
        let (child_sid, child_pgid) = unsafe { (libc::getsid(pid), libc::getpgid(pid)) };

        child.kill().await.expect("kill child");

        // Asserted against the child's own pid, not merely `!=` our session: a
        // dead child reports -1, which would satisfy an inequality vacuously.
        assert_eq!(
            child_sid, pid,
            "child must lead a new session, not sit in a background group of ours"
        );
        assert_eq!(
            child_pgid, pid,
            "the PGID == PID invariant every killpg here relies on must survive"
        );
    }

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
