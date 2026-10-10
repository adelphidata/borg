//! Tokio runtime construction for the daemon.
//!
//! The agent loop — the whole `run_agent_loop` → `execute_tool_calls` →
//! `execute_tool` → per-tool handler poll chain — runs on a tokio **worker
//! thread**, not the main thread. Each `#[instrument]`ed async layer and each
//! large tool future (the browser handler, in particular, folds every action's
//! locals into one future) contributes to a single synchronous `poll` call
//! stack. Tokio's default worker stack is **2 MiB**, which the browser tool's
//! poll chain overflows — and a stack overflow trips the guard page and
//! `abort()`s the process. That abort is uncatchable: `catch_unwind`, the
//! subsystem supervisor, and per-task panic isolation cannot intercept it, so
//! the entire daemon dies and every gRPC stream is torn down (the client sees a
//! bare `h2 protocol error: error reading a body from connection`).
//!
//! The in-process TUI agent loop already ran on a 4 MiB worker stack
//! (`interface`'s `RUNTIME_STACK_SIZE`); when the loop moved into `borgd` the
//! daemon runtime was left on the 2 MiB default. [`WORKER_STACK_SIZE`] restores
//! parity so the daemon has the headroom the agent loop was designed for.

use tokio::runtime::{Builder, Runtime};

/// Worker-thread stack size for the daemon runtime.
///
/// Must stay **strictly above** tokio's 2 MiB default — the browser tool's
/// agent-loop poll chain overflows 2 MiB and `abort()`s the daemon. Kept in
/// step with `interface`'s `RUNTIME_STACK_SIZE`, the value under which the same
/// agent loop ran in-process without overflowing.
pub const WORKER_STACK_SIZE: usize = 4 * 1024 * 1024;

/// Build the daemon's multi-threaded tokio runtime with an enlarged worker
/// stack (see [`WORKER_STACK_SIZE`]) so the agent loop's deep poll chain cannot
/// overflow the default 2 MiB worker stack and abort the process.
pub fn build_runtime() -> std::io::Result<Runtime> {
    Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(WORKER_STACK_SIZE)
        .thread_name("borgd-worker")
        .build()
}

/// Soft open-file limit the daemon raises itself to on Unix.
///
/// A launchd- or shell-spawned process inherits a soft `RLIMIT_NOFILE` of
/// 256 on macOS (1024 on many Linux distributions). One long-lived daemon
/// holds a libsql handle, every open session's gRPC stream, each MCP server's
/// stdio pipes, browser CDP sockets, and channel websockets at once, so that
/// default is reachable in ordinary use and shows up as a confusing `EMFILE`
/// deep inside whichever subsystem opened the next descriptor.
#[cfg(unix)]
pub const NOFILE_SOFT_LIMIT: libc::rlim_t = 4096;

/// Raise the soft `RLIMIT_NOFILE` to [`NOFILE_SOFT_LIMIT`], capped at the
/// inherited hard limit. Never lowers an already higher soft limit. Returns
/// the soft limit in effect afterwards.
#[cfg(unix)]
pub fn raise_nofile_limit() -> std::io::Result<libc::rlim_t> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` only writes into the stack-owned struct we pass.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let target = limit.rlim_cur.max(limit.rlim_max.min(NOFILE_SOFT_LIMIT));
    if target == limit.rlim_cur {
        return Ok(target);
    }
    limit.rlim_cur = target;
    // SAFETY: the requested soft limit does not exceed the inherited hard
    // limit, and the struct is fully initialised by the `getrlimit` above.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(target)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn nofile() -> libc::rlimit {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: see `raise_nofile_limit`.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        limit
    }

    /// After the raise the daemon can open at least `NOFILE_SOFT_LIMIT`
    /// descriptors (or as many as the hard limit allows), and a second call
    /// is a no-op rather than a lowering.
    #[test]
    fn raise_nofile_limit_reaches_target_and_never_lowers() {
        let before = nofile();
        let after = raise_nofile_limit().expect("getrlimit/setrlimit");
        let expected = before.rlim_cur.max(before.rlim_max.min(NOFILE_SOFT_LIMIT));
        assert_eq!(after, expected);
        assert_eq!(nofile().rlim_cur, expected);
        assert!(after >= before.rlim_cur, "must never lower the soft limit");

        let again = raise_nofile_limit().expect("second call");
        assert_eq!(again, expected, "idempotent once at target");
        assert_eq!(nofile().rlim_max, before.rlim_max, "hard limit untouched");
    }

    /// Actual stack size of the calling thread, queried from the OS. Only the
    /// platforms borg ships on (macOS, Linux) are wired up; both expose the
    /// worker thread's real stack via pthread. Returns 0 if the query fails.
    fn current_thread_stack_size() -> usize {
        #[cfg(target_os = "macos")]
        // SAFETY: `pthread_get_stacksize_np` reads the stack size of a valid,
        // live pthread handle (`pthread_self`) and has no other effects.
        unsafe {
            libc::pthread_get_stacksize_np(libc::pthread_self())
        }
        #[cfg(target_os = "linux")]
        // SAFETY: `pthread_getattr_np` fills a zeroed `pthread_attr_t` for the
        // current thread; on success we read the stack size and always destroy
        // the attr. All handles are valid and confined to this call.
        unsafe {
            let mut attr: libc::pthread_attr_t = std::mem::zeroed();
            if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
                return 0;
            }
            let mut size: libc::size_t = 0;
            let rc = libc::pthread_attr_getstacksize(&attr, &mut size);
            libc::pthread_attr_destroy(&mut attr);
            if rc == 0 {
                size
            } else {
                0
            }
        }
    }

    /// Regression guard: `build_runtime` must actually apply an enlarged worker
    /// stack, not just define a large constant. We run on a *spawned* task —
    /// exactly where the agent loop executes — and read the worker thread's
    /// real OS stack size, asserting it exceeds tokio's 2 MiB default. That gap
    /// is what let the browser tool's poll chain overflow and `abort()` the
    /// daemon; if someone removes `.thread_stack_size(...)` (dropping workers
    /// back to the default) or shrinks it, this fails with a clean assertion
    /// rather than an uncatchable stack-overflow abort.
    #[test]
    fn build_runtime_applies_enlarged_worker_stack() {
        const TOKIO_DEFAULT_STACK: usize = 2 * 1024 * 1024;

        let rt = build_runtime().expect("build daemon runtime");
        let worker_stack = rt
            .block_on(async { tokio::spawn(async { current_thread_stack_size() }).await })
            .expect("stack-size probe task must complete");

        assert!(
            worker_stack >= WORKER_STACK_SIZE,
            "worker thread stack ({worker_stack}) must be at least the \
             configured WORKER_STACK_SIZE ({WORKER_STACK_SIZE}); build_runtime \
             is not applying .thread_stack_size()"
        );
        assert!(
            worker_stack > TOKIO_DEFAULT_STACK,
            "worker thread stack ({worker_stack}) must exceed tokio's 2 MiB \
             default — the agent loop's browser-tool poll chain overflows 2 MiB \
             and aborts the daemon"
        );
    }
}
