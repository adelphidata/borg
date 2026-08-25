//! Signalling a child's process group, with a per-member fallback.
//!
//! Borg's whole process-tree cleanup story — hook timeouts, git timeouts,
//! persistent shell sessions, and the stdio MCP watchdog — rests on `killpg`.
//! On macOS `killpg` can return `EPERM` even for a group we lead, and the
//! failure is silent: the call returns, the caller's `wait` reaps only the
//! direct child, and every descendant survives. macOS is Borg's primary
//! platform, so that is a live leak rather than a theoretical one.
//!
//! This module is the one place that handles it. On `EPERM` it enumerates the
//! group's processes, re-checks each one's process group, and signals the
//! members individually.
//!
//! Lives in `sandbox` — the crate that owns process containment — so the
//! script runner, the stdio MCP watchdog, and `cortex`'s shell/git/hook paths
//! all share one implementation rather than each growing their own. `mcp`
//! re-exports it for its existing callers.

#[cfg(unix)]
mod imp {
    /// Send `sig` to every process in `pgid`, falling back to per-member
    /// signalling when the group call is refused.
    ///
    /// Best-effort by contract, like the `killpg` it replaces: `ESRCH` (the
    /// group is already gone) and any failure of the fallback are ignored. The
    /// caller's `wait` is still what reaps the direct child.
    ///
    /// A non-positive `pgid` is rejected outright. `killpg(0, …)` signals the
    /// *caller's own* group, and a negative value is not a group at all — either
    /// would turn a cleanup into self-termination.
    pub fn signal_group_or_members(pgid: i32, sig: i32) {
        if pgid <= 0 {
            tracing::warn!(pgid, "refusing to signal a non-positive process group");
            return;
        }

        // SAFETY: FFI call to `killpg`. `pgid` is positive (checked above) and
        // belongs to a child group the caller created via `spawn_in_own_group`,
        // so we never signal an unrelated group.
        let failed = unsafe { libc::killpg(pgid, sig) } == -1;
        if !failed {
            return;
        }

        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            // The group is already gone. Nothing to clean up.
            return;
        }
        if err.raw_os_error() != Some(libc::EPERM) {
            tracing::warn!(pgid, sig, error = %err, "killpg failed");
            return;
        }

        let members = group_members(pgid);
        tracing::debug!(
            pgid,
            sig,
            members = members.len(),
            "killpg returned EPERM; signalling group members individually"
        );
        for pid in members {
            // SAFETY: FFI call to `kill`. `pid` was just confirmed to report
            // `pgid` as its process group, so this cannot reach a process
            // outside the group we were asked to signal.
            if unsafe { libc::kill(pid, sig) } == -1 {
                let err = std::io::Error::last_os_error();
                // The member exiting between the enumeration and the signal is
                // the ordinary case, not a fault. Anything else is a member we
                // failed to reap, and this fallback exists precisely because a
                // silently-dropped signal result hid a process leak before.
                if err.raw_os_error() != Some(libc::ESRCH) {
                    tracing::debug!(pid, sig, error = %err, "could not signal group member");
                }
            }
        }
    }

    /// PIDs that currently report `pgid` as their process group.
    ///
    /// The `getpgid` re-check is the safety property, not an optimisation: PIDs
    /// are recycled, so a stale list could otherwise carry a signal to an
    /// unrelated process that happened to inherit the number.
    fn group_members(pgid: i32) -> Vec<i32> {
        all_pids()
            .into_iter()
            .filter(|&pid| {
                // Never signal ourselves even if we somehow share the group.
                // SAFETY: FFI call to `getpgid`; a failure returns -1, which
                // cannot equal a positive `pgid`.
                pid != std::process::id() as i32 && unsafe { libc::getpgid(pid) } == pgid
            })
            .collect()
    }

    /// Every PID visible to this process.
    #[cfg(target_os = "macos")]
    fn all_pids() -> Vec<i32> {
        // `proc_listallpids(NULL, 0)` reports how many slots are needed. Ask
        // for extra headroom because processes can appear between the two
        // calls; the second call's return value is what bounds the slice.
        const HEADROOM: i32 = 64;
        // SAFETY: FFI call. A null buffer with zero size is the documented way
        // to query the required capacity.
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count <= 0 {
            return Vec::new();
        }

        let capacity = count + HEADROOM;
        let mut pids = vec![0i32; capacity as usize];
        let bytes = capacity * std::mem::size_of::<i32>() as i32;
        // SAFETY: FFI call. `pids` has `capacity` i32 slots and `bytes` is the
        // matching byte length, so the kernel cannot write past the end.
        let written = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if written <= 0 {
            return Vec::new();
        }
        pids.truncate(written as usize);
        pids
    }

    /// Every PID visible to this process, read from `/proc`.
    #[cfg(not(target_os = "macos"))]
    fn all_pids() -> Vec<i32> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        use std::sync::{Arc, Mutex};

        /// Run `f` with a subscriber that captures everything at DEBUG and
        /// above, returning the rendered log text. The signalling contract is
        /// best-effort by design, so *what it logs* is the observable spec
        /// for its error paths.
        fn capture_logs(f: impl FnOnce()) -> String {
            #[derive(Clone)]
            struct Buf(Arc<Mutex<Vec<u8>>>);
            impl std::io::Write for Buf {
                fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                    self.0
                        .lock()
                        .expect("log buffer poisoned")
                        .extend_from_slice(data);
                    Ok(data.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
                type Writer = Buf;
                fn make_writer(&'a self) -> Buf {
                    self.clone()
                }
            }
            let buf = Buf(Arc::new(Mutex::new(Vec::new())));
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(buf.clone())
                .finish();
            tracing::subscriber::with_default(subscriber, f);
            let bytes = buf.0.lock().expect("log buffer poisoned");
            String::from_utf8_lossy(&bytes).into_owned()
        }

        /// A child leading its own process group, for signalling tests.
        fn spawn_group_leader() -> std::process::Child {
            Command::new("sleep")
                .arg("30")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .expect("spawn sleep")
        }

        /// Spec: a `killpg` failure that is neither ESRCH (group already
        /// gone) nor EPERM (fall back per-member) must be *warned about and
        /// stopped at* — not silently swallowed (the leak this module exists
        /// to close) and not misrouted into the EPERM member walk.
        ///
        /// An out-of-range signal number gives a deterministic EINVAL.
        #[test]
        fn an_unexpected_killpg_failure_is_warned_not_swallowed() {
            let mut child = spawn_group_leader();
            let pgid = child.id() as i32;
            // Not a valid signal on any unix; killpg fails with EINVAL.
            const BAD_SIGNAL: i32 = 999;

            let logs = capture_logs(|| signal_group_or_members(pgid, BAD_SIGNAL));
            assert!(
                logs.contains("killpg failed"),
                "an EINVAL from killpg must be surfaced as a warning: {logs:?}"
            );
            assert!(
                !logs.contains("signalling group members individually"),
                "EINVAL must not be treated as the EPERM fallback case: {logs:?}"
            );

            signal_group_or_members(pgid, libc::SIGKILL);
            child.wait().expect("wait");
        }

        /// Spec: a successful group kill needs no fallback and no complaint —
        /// the happy path is silent, and the child is actually dead.
        #[test]
        fn a_successful_group_kill_is_silent() {
            let mut child = spawn_group_leader();
            let pgid = child.id() as i32;

            let logs = capture_logs(|| signal_group_or_members(pgid, libc::SIGKILL));
            assert!(
                !logs.contains("killpg failed") && !logs.contains("EPERM"),
                "a successful killpg must not log a failure: {logs:?}"
            );

            let status = child.wait().expect("wait");
            assert!(!status.success(), "the child must have been killed");
        }

        /// Spec: the member-walk fallback is only as good as its process
        /// enumeration. The table must span old (launchd, pid 1), current
        /// (this test runner) and brand-new (a just-spawned child) processes;
        /// a truncated or padded list would let group members escape the
        /// EPERM fallback or signal garbage PIDs.
        #[cfg(target_os = "macos")]
        #[test]
        fn all_pids_spans_the_whole_process_table() {
            let mut child = spawn_group_leader();
            let child_pid = child.id() as i32;

            let pids = all_pids();
            for (pid, what) in [
                (1, "launchd"),
                (std::process::id() as i32, "the test runner"),
                (child_pid, "a freshly spawned child"),
            ] {
                assert!(
                    pids.contains(&pid),
                    "{what} (pid {pid}) missing from the {} listed pids",
                    pids.len()
                );
            }

            signal_group_or_members(child_pid, libc::SIGKILL);
            child.wait().expect("wait");
        }

        /// Spec: signalling group 0 would hit the *caller's* own process group
        /// and kill borg itself. A cleanup helper must refuse it outright.
        #[test]
        fn a_non_positive_group_is_never_signalled() {
            // Reaching `killpg` here would terminate the test runner, so
            // surviving the call is the assertion.
            signal_group_or_members(0, libc::SIGKILL);
            signal_group_or_members(-1, libc::SIGKILL);
        }

        /// Spec: the fallback must reach a real group member. Enumerating and
        /// re-checking `getpgid` is what makes the EPERM path work, so exercise
        /// that machinery directly against a live child.
        #[test]
        fn group_members_finds_a_live_child_and_excludes_us() {
            let mut child = Command::new("sleep")
                .arg("30")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .expect("spawn sleep");
            let pgid = child.id() as i32;

            // The child leads its own group, so it must show up as a member.
            let members = group_members(pgid);
            assert!(
                members.contains(&pgid),
                "the group leader must be found: {members:?}"
            );
            assert!(
                !members.contains(&(std::process::id() as i32)),
                "the signalling process must never be in its own kill list"
            );

            signal_group_or_members(pgid, libc::SIGKILL);
            let status = child.wait().expect("wait");
            assert!(!status.success(), "the child must have been killed");

            // Once the group is gone the member list is empty, so a repeat
            // call is a no-op rather than a signal to a recycled PID.
            assert!(group_members(pgid).is_empty());
        }
    }
}

#[cfg(unix)]
pub use imp::signal_group_or_members;

/// No-op on platforms without process groups. Windows containment is a job
/// object, handled at the spawn site rather than by signalling.
#[cfg(not(unix))]
pub fn signal_group_or_members(_pgid: i32, _sig: i32) {}
