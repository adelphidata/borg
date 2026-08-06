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

        // Spawn the real server in its own process group so we can signal the
        // whole tree (the server may fork its own children). stdio is inherited
        // — these fds are the parent's pipe ends, so JSON-RPC flows straight
        // through us without a copy.
        let mut child = match Command::new(&cmd).args(&args).process_group(0).spawn() {
            Ok(c) => c,
            Err(_) => return 127,
        };
        let pgid = child.id() as i32; // group leader pid == pgid

        // 100ms trades a little orphan-reap latency for ~10 wakeups/sec on a
        // thread that lives as long as every stdio MCP server.
        let poll = Duration::from_millis(100);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.code().unwrap_or(0),
                Ok(None) => {}
                Err(_) => {
                    reap_group(pgid, &mut child);
                    return 0;
                }
            }
            if TERMINATE.load(Ordering::SeqCst) || parent_gone(parent_pid) {
                reap_group(pgid, &mut child);
                return 0;
            }
            std::thread::sleep(poll);
        }
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
    /// SIGKILL and reap the direct child so it isn't left a zombie.
    fn reap_group(pgid: i32, child: &mut Child) {
        // 15 x 20ms = 300ms of grace — enough for a node/python MCP server to
        // flush stdio and unwind, short enough that daemon shutdown doesn't stall.
        const SIGTERM_GRACE_POLLS: u32 = 15;
        const SIGTERM_GRACE_POLL: Duration = Duration::from_millis(20);
        unsafe { libc::killpg(pgid, libc::SIGTERM) };
        for _ in 0..SIGTERM_GRACE_POLLS {
            if matches!(child.try_wait(), Ok(Some(_))) {
                let _ = child.wait();
                return;
            }
            std::thread::sleep(SIGTERM_GRACE_POLL);
        }
        unsafe { libc::killpg(pgid, libc::SIGKILL) };
        let _ = child.wait();
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
    }
}
