//! Cross-platform (Unix) watchdog that reaps orphaned stdio MCP servers.
//!
//! Problem: a stdio MCP server is a direct child of the borg process (`borgd`,
//! or `borg` for operator `borg mcp …` tooling). Tokio's `kill_on_drop` reaps
//! it on a clean shutdown or a normal transport drop, but NOT when the parent
//! is `SIGKILL`ed or crashes — no destructors run, so the server (and its own
//! descendants, e.g. `npx` → `node`) is reparented to init and keeps running.
//! Repeated ungraceful restarts then pile up N orphans racing the same
//! upstream session.
//!
//! Fix: interpose a tiny watchdog process between the parent and the real MCP
//! server. [`wrapped_command`] rewrites the spawn to
//! `current_exe __borg_mcp_watchdog <parent_pid> -- <cmd> <args…>`; that
//! re-exec runs [`dispatch`] before any normal startup, which:
//!   - spawns the real command as its own child in a fresh process group,
//!     passing stdin/stdout/stderr straight through (the MCP JSON-RPC bytes
//!     flow parent ↔ server untouched — the watchdog never reads them),
//!   - polls the original parent's liveness and, the moment it is gone
//!     (reparented away / no longer exists), `SIGTERM`-then-`SIGKILL`s the
//!     server's whole process group,
//!   - on `SIGTERM`/`SIGINT` to itself (a clean teardown request from the
//!     parent) tears the server group down the same way.
//!
//! Windows has no equally cheap primitive here, so [`dispatch`] is a no-op and
//! [`wrapped_command`] returns `None`; the stdio transport then falls back to a
//! direct spawn plus `kill_on_drop` (unchanged pre-watchdog behavior).

use std::sync::atomic::{AtomicBool, Ordering};

/// `argv[1]` marker identifying a re-exec of this binary as the MCP watchdog.
pub const WATCHDOG_MARKER: &str = "__borg_mcp_watchdog";

/// Set true once [`dispatch`] confirms this binary has the watchdog trampoline
/// wired (i.e. it called `dispatch` at startup and it wasn't a watchdog
/// re-exec). Only such a binary can safely be re-exec'd as a watchdog, so
/// [`wrapped_command`] wraps ONLY when this is set. Test harnesses and other
/// binaries that never call `dispatch` therefore spawn MCP servers directly —
/// re-execing them with watchdog argv would run the wrong `main`.
static WATCHDOG_HOST: AtomicBool = AtomicBool::new(false);

/// If this process was re-exec'd as a watchdog, run the supervise loop and
/// return `Some(exit_code)`; the caller MUST `std::process::exit` with it
/// before doing any normal startup. Returns `None` for a normal invocation
/// (and records that this binary hosts the trampoline).
///
/// Call this as the FIRST thing in `main()` (before clap parsing) so the
/// watchdog argv never reaches the real CLI, mirroring the Windows
/// sandbox-spawn trampoline.
pub fn dispatch() -> Option<i32> {
    let mut args = std::env::args();
    let _exe = args.next();
    if args.next().as_deref() != Some(WATCHDOG_MARKER) {
        // Normal startup of a trampoline-equipped binary — safe to wrap.
        WATCHDOG_HOST.store(true, Ordering::SeqCst);
        return None;
    }
    #[cfg(unix)]
    {
        let parent_pid: i32 = args.next().and_then(|s| s.parse().ok())?;
        if args.next().as_deref() != Some("--") {
            return Some(2);
        }
        let cmd = args.next()?;
        let rest: Vec<String> = args.collect();
        Some(unix::run(parent_pid, cmd, rest))
    }
    #[cfg(not(unix))]
    {
        // Unreachable: non-unix spawns never wrap. Exit cleanly if it happens.
        Some(0)
    }
}

/// Rewrite `(command, args)` so the real process runs under a watchdog re-exec
/// of the current binary. Returns `None` when wrapping is unavailable — non-unix,
/// `current_exe` unresolvable, or this binary did not wire the trampoline (see
/// [`WATCHDOG_HOST`]) — and the caller then spawns directly.
///
/// The returned command is the current executable; the returned args are
/// `[MARKER, <this pid>, "--", command, args…]`.
#[cfg(unix)]
pub fn wrapped_command(command: &str, args: &[String]) -> Option<(String, Vec<String>)> {
    if !WATCHDOG_HOST.load(Ordering::SeqCst) {
        return None;
    }
    build_wrapped(command, args)
}

/// Non-unix stub: wrapping is not supported, so callers spawn directly.
#[cfg(not(unix))]
pub fn wrapped_command(_command: &str, _args: &[String]) -> Option<(String, Vec<String>)> {
    None
}

/// Build the wrapped argv (no host gating) — the mechanical part of
/// [`wrapped_command`], separated so it can be unit-tested directly.
#[cfg(unix)]
fn build_wrapped(command: &str, args: &[String]) -> Option<(String, Vec<String>)> {
    let exe = std::env::current_exe()
        .ok()?
        .into_os_string()
        .into_string()
        .ok()?;
    let mut wrapped = Vec::with_capacity(args.len() + 4);
    wrapped.push(WATCHDOG_MARKER.to_string());
    wrapped.push(std::process::id().to_string());
    wrapped.push("--".to_string());
    wrapped.push(command.to_string());
    wrapped.extend(args.iter().cloned());
    Some((exe, wrapped))
}

#[cfg(all(test, unix))]
mod wrap_tests {
    use super::*;

    #[test]
    fn build_wrapped_has_marker_pid_sep_then_command() {
        let (exe, args) = build_wrapped("mcp-server-foo", &["--port".into(), "8080".into()])
            .expect("current_exe");
        assert!(!exe.is_empty());
        assert_eq!(args[0], WATCHDOG_MARKER);
        assert_eq!(args[1], std::process::id().to_string());
        assert_eq!(args[2], "--");
        assert_eq!(args[3], "mcp-server-foo");
        assert_eq!(&args[4..], &["--port".to_string(), "8080".to_string()]);
    }

    #[test]
    fn wrapped_command_returns_none_until_host_wired() {
        // Test harness never calls `dispatch`, so wrapping must stay disabled —
        // otherwise a test that spawns an MCP server would re-exec the test
        // binary with watchdog argv.
        assert!(wrapped_command("cmd", &[]).is_none());
    }
}

#[cfg(unix)]
mod unix {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// Set by the SIGTERM/SIGINT handler; the poll loop notices and reaps.
    static TERMINATE: AtomicBool = AtomicBool::new(false);

    extern "C" fn on_signal(_sig: libc::c_int) {
        // async-signal-safe: a single atomic store.
        TERMINATE.store(true, Ordering::SeqCst);
    }

    /// Supervise the real MCP server. Returns the process exit code to use.
    ///
    /// `parent_pid` is the borg process that spawned this watchdog. When it
    /// dies (reparented away, or no longer exists) the server group is reaped.
    pub fn run(parent_pid: i32, cmd: String, args: Vec<String>) -> i32 {
        // SIGTERM/SIGINT → request teardown. `kill_on_drop`'s uncatchable
        // SIGKILL is deliberately NOT used by the parent for watchdog children
        // precisely so this catchable path can reap the server group.
        unsafe {
            // Cast through a pointer: a direct fn-item-to-integer cast is
            // lint-worthy and not guaranteed to produce the fn's address.
            libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        }

        let child = match spawn_server(&cmd, &args) {
            Ok(c) => c,
            Err(e) => {
                // 127 alone tells an operator nothing, and `pre_exec` failing is
                // a second way to land here. This process has no tracing
                // subscriber, but its stderr is the parent's pipe end, which the
                // parent routes to the server's stderr log.
                eprintln!("borg mcp watchdog: failed to spawn '{cmd}': {e}");
                return 127;
            }
        };
        supervise(child, || {
            TERMINATE.load(Ordering::SeqCst) || parent_gone(parent_pid)
        })
    }

    /// Spawn the real server in its own session so we can signal the whole
    /// tree (the server may fork its own children) and so a launcher shell's
    /// startup files cannot stop it on background terminal I/O. stdio is
    /// inherited — these fds are the parent's pipe ends, so JSON-RPC flows
    /// straight through us without a copy.
    fn spawn_server(cmd: &str, args: &[String]) -> std::io::Result<Child> {
        let mut command = Command::new(cmd);
        command.args(args);
        // SAFETY: `detach_from_tty` only calls `setsid`/`setpgid` and
        // `close_nonstdio_fds` only `fcntl`/`getrlimit`/`close_range`, all
        // async-signal-safe, as `pre_exec` requires.
        unsafe {
            command.pre_exec(|| {
                crate::process_group::detach_from_tty()?;
                // The server is untrusted third-party code: it gets stdio and
                // nothing else this process happens to hold open.
                crate::process_group::close_nonstdio_fds()
            });
        }
        command.spawn()
    }

    /// Poll `child` (a process-group leader) until it exits or `should_stop`
    /// fires, then tear its whole group down. Returns the exit code to use:
    /// the server's own code on a natural exit, 0 on a requested teardown.
    ///
    /// The group is signalled on BOTH paths, before `child` is reaped. A
    /// leader exiting does not end its group: a server that forked helpers
    /// and then exited (say, because `close` dropped its stdin) leaves them
    /// running in it.
    fn supervise(mut child: Child, mut should_stop: impl FnMut() -> bool) -> i32 {
        let pgid = child.id() as i32; // group leader pid == pgid

        // 100ms trades a little orphan-reap latency for ~10 wakeups/sec on a
        // thread that lives as long as every stdio MCP server.
        let poll = Duration::from_millis(100);
        let requested = loop {
            match leader_exited(pgid) {
                Ok(true) => break false,
                Ok(false) => {}
                Err(e) => {
                    eprintln!("borg mcp watchdog: waitid on server {pgid} failed: {e}");
                    break true;
                }
            }
            if should_stop() {
                break true;
            }
            std::thread::sleep(poll);
        };
        reap_group(pgid);
        // Only now is the leader reaped, which frees its pid and so the pgid.
        match child.wait() {
            Ok(status) if !requested => status.code().unwrap_or(0),
            Ok(_) => 0,
            Err(e) => {
                eprintln!("borg mcp watchdog: reaping server {pgid} failed: {e}");
                0
            }
        }
    }

    /// True once `pid` has exited, WITHOUT reaping it (`WNOWAIT`). The zombie
    /// leader keeps its pid, and with it the process-group id, reserved, so a
    /// later `killpg(pgid, …)` cannot land on a recycled, unrelated group.
    fn leader_exited(pid: i32) -> std::io::Result<bool> {
        // SAFETY: an all-zero `siginfo_t` is a valid out-param. With `WNOHANG`
        // the kernel leaves `si_pid` at 0 when the child has not exited.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: FFI call on our own child's pid with a valid out-pointer.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc == -1 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: reads the pid field of the `siginfo_t` filled in above.
        Ok(unsafe { info.si_pid() } != 0)
    }

    /// False only once no process reports `pgid` any more. `EPERM` means
    /// members exist that we may not signal, so the group still counts.
    ///
    /// The leader is deliberately left an unreaped zombie here (see
    /// [`leader_exited`]). On Linux a zombie still counts as a group member,
    /// so `killpg(pgid, 0)` succeeds and this stays true until the caller
    /// reaps it. Only a kernel that skips zombies in the probe reports
    /// `ESRCH` while the leader is held.
    fn group_alive(pgid: i32) -> bool {
        // SAFETY: signal 0 only probes; `pgid` is our child's positive group.
        if unsafe { libc::killpg(pgid, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    /// True once the original parent is gone. Primary signal: our parent pid
    /// changed (the kernel reparents an orphan to init/launchd). Backup:
    /// `kill(pid, 0)` reports `ESRCH`.
    fn parent_gone(parent_pid: i32) -> bool {
        if unsafe { libc::getppid() } != parent_pid {
            return true;
        }
        if unsafe { libc::kill(parent_pid, 0) } == -1 {
            return std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        }
        false
    }

    /// SIGTERM the server's process group, allow a brief grace to exit, then
    /// SIGKILL whatever is left. The leader must still be unreaped (see
    /// [`leader_exited`]) so `pgid` cannot have been recycled; the caller
    /// reaps it afterwards.
    ///
    /// The SIGKILL pass runs even when the leader dies inside the grace: a
    /// descendant that ignores SIGTERM is exactly what it exists for.
    fn reap_group(pgid: i32) {
        // 15 x 20ms = 300ms of grace — enough for a node/python MCP server to
        // flush stdio and unwind, short enough that daemon shutdown doesn't stall.
        const SIGTERM_GRACE_POLLS: u32 = 15;
        const SIGTERM_GRACE_POLL: Duration = Duration::from_millis(20);
        // Both signals go through the shared helper: macOS can refuse the
        // group call with EPERM, and dropping that result left the server's
        // descendants running after daemon shutdown.
        crate::process_group::signal_group_or_members(pgid, libc::SIGTERM);
        // The early exit fires only where a zombie-only group probes as gone.
        // On Linux the held zombie leader keeps [`group_alive`] true, so the
        // full grace always runs and SIGKILL follows — at most 300ms of extra
        // wait at shutdown, and SIGKILL to an exited group is harmless.
        for _ in 0..SIGTERM_GRACE_POLLS {
            if !group_alive(pgid) {
                return;
            }
            std::thread::sleep(SIGTERM_GRACE_POLL);
        }
        crate::process_group::signal_group_or_members(pgid, libc::SIGKILL);
        // The leader too, directly. It is unreaped, so its pid cannot be
        // reused; if it ever left the group, the caller's blocking `wait`
        // would otherwise hang on it. ESRCH just means it is already a zombie
        // (some kernels refuse to signal one); anything else is reported.
        // SAFETY: FFI call on our own unreaped child's pid.
        if unsafe { libc::kill(pgid, libc::SIGKILL) } == -1 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                eprintln!("borg mcp watchdog: SIGKILL to server {pgid} failed: {err}");
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::time::Instant;

        /// A watchdog whose parent is already gone must reap its server and
        /// return promptly, not linger for the server's natural lifetime.
        /// We point `parent_pid` at a pid that isn't our parent, so
        /// `parent_gone` trips on the first poll; the `sleep 30` server is
        /// then killed and `run` returns in well under a second.
        #[test]
        fn reaps_server_when_parent_already_gone() {
            // Our real parent is the test runner; pick a pid that is provably
            // not it. pid 1 (init/launchd) is never this process's parent.
            let fake_parent = 1;
            assert_ne!(unsafe { libc::getppid() }, fake_parent);

            let start = Instant::now();
            let code = run(fake_parent, "sleep".to_string(), vec!["30".to_string()]);
            let elapsed = start.elapsed();

            assert_eq!(code, 0);
            assert!(
                elapsed < Duration::from_secs(3),
                "watchdog should reap immediately, took {elapsed:?}"
            );
        }

        /// A server that exits on its own propagates its exit code and the
        /// loop returns without waiting on parent death.
        #[test]
        fn returns_server_exit_code_on_natural_exit() {
            // Parent is our real parent (alive), so only natural server exit
            // ends the loop. `false` exits 1 immediately.
            let real_parent = unsafe { libc::getppid() };
            let code = run(real_parent, "false".to_string(), vec![]);
            assert_eq!(code, 1);
        }

        fn pid_exists(pid: i32) -> bool {
            // SAFETY: signal 0 only probes.
            let rc = unsafe { libc::kill(pid, 0) };
            rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        }

        /// SIGKILLs a grandchild the test spawned if it is still alive on drop,
        /// so a failing assertion cannot leave a stray `sleep 60` behind.
        struct KillOnDrop(i32);
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                if self.0 > 0 && pid_exists(self.0) {
                    // SAFETY: FFI call on the pid this test spawned.
                    unsafe { libc::kill(self.0, libc::SIGKILL) };
                }
            }
        }

        /// The pid the server script wrote to `path`, once it is there.
        fn read_pid(path: &std::path::Path) -> Option<i32> {
            std::fs::read_to_string(path).ok()?.trim().parse().ok()
        }

        /// The grandchild disappears within a few seconds (its reparented
        /// zombie is reaped by init, not by us, so allow some slack).
        fn assert_gone_soon(pid: i32, case: &str) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while pid_exists(pid) {
                assert!(
                    Instant::now() < deadline,
                    "{case}: grandchild {pid} that ignores SIGTERM outlived the watchdog"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        /// Spec: the server's whole process group dies with the server, even
        /// when its helpers ignore SIGTERM, on every way the leader can exit:
        ///   - on its own (it forked a helper and exited, as a server does
        ///     when `close` drops its stdin) — the group must still be reaped
        ///     and the server's own exit code kept;
        ///   - on the teardown SIGTERM, inside the grace period — the SIGKILL
        ///     pass must still run for the helper that ignored it.
        #[test]
        fn server_group_is_reaped_however_the_leader_exits() {
            // (case, script, teardown requested once the pidfile exists,
            //  expected exit code). `$1` names where the helper's pid goes.
            // The helper ignores TERM before its pid is published, so a
            // teardown can never beat the `trap`.
            let cases: &[(&str, &str, bool, i32)] = &[
                (
                    "leader exits on its own",
                    r#"trap "" TERM; sleep 60 & echo $! > "$1"; exit 3"#,
                    false,
                    3,
                ),
                (
                    "leader dies on the teardown SIGTERM",
                    r#"sh -c 'trap "" TERM; echo $$ > "$1"; exec sleep 60' helper "$1" & wait"#,
                    true,
                    0,
                ),
            ];
            for &(case, script, teardown, expected_code) in cases {
                let dir = tempfile::tempdir().expect("tempdir");
                let pidfile = dir.path().join("helper.pid");
                let args = [
                    "-c".to_string(),
                    script.to_string(),
                    "sh".to_string(),
                    pidfile.display().to_string(),
                ];
                let child = spawn_server("/bin/sh", &args).expect("spawn sh");
                // Hard stop so a broken script fails the test instead of
                // hanging it.
                let give_up = Instant::now() + Duration::from_secs(10);
                let code = supervise(child, || {
                    (teardown && read_pid(&pidfile).is_some()) || Instant::now() > give_up
                });
                let helper = read_pid(&pidfile).expect("helper pid was never written");
                let _guard = KillOnDrop(helper);

                assert_eq!(code, expected_code, "{case}: exit code");
                assert_gone_soon(helper, case);
            }
        }
    }
}
