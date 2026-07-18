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

#[cfg(test)]
mod tests {
    use super::*;

    /// The daemon worker stack must stay strictly above tokio's 2 MiB default.
    /// This is the exact gap that let the browser tool's poll chain overflow
    /// and `abort()` the whole daemon. If someone drops the size back to (or
    /// below) the default, this fails before the crash can ship again.
    #[test]
    fn worker_stack_exceeds_tokio_default() {
        const TOKIO_DEFAULT_STACK: usize = 2 * 1024 * 1024;
        assert!(
            WORKER_STACK_SIZE > TOKIO_DEFAULT_STACK,
            "daemon worker stack ({WORKER_STACK_SIZE}) must exceed tokio's 2 MiB \
             default; the agent loop's browser-tool poll chain overflows 2 MiB \
             and aborts the daemon"
        );
    }

    /// Behavioral regression guard: a spawned task (i.e. one running on a
    /// worker thread, exactly like the agent loop) must be able to consume a
    /// call stack deeper than tokio's 2 MiB default without overflowing.
    ///
    /// The recursion below consumes at least ~2.6 MiB of stack — comfortably
    /// past the 2 MiB default (which would `abort()` the test binary here) yet
    /// well under the configured 4 MiB. If `build_runtime` ever regresses to
    /// the default worker stack, this task overflows and the process aborts,
    /// turning the silent daemon-killing crash into a loud test failure.
    #[test]
    fn spawned_task_survives_deep_stack_use() {
        /// ~1 KiB of live stack per frame, black-boxed so the optimizer can
        /// neither elide the buffer nor turn the recursion into a loop.
        #[inline(never)]
        fn consume_stack(depth: usize) -> u8 {
            let mut buf = [0u8; 1024];
            std::hint::black_box(&mut buf[..]);
            buf[depth % buf.len()] = depth as u8;
            if depth == 0 {
                buf[0]
            } else {
                buf[0].wrapping_add(consume_stack(depth - 1))
            }
        }

        // 2600 frames × ≥1 KiB ⇒ ≥2.6 MiB: past the 2 MiB default, under 4 MiB.
        const DEPTH: usize = 2600;

        let rt = build_runtime().expect("build daemon runtime");
        let out = rt.block_on(async {
            // `spawn` runs the work on a worker thread with WORKER_STACK_SIZE,
            // mirroring where the agent loop actually executes — `block_on`'s
            // own thread carries the larger OS main-thread stack and would mask
            // an undersized worker stack.
            tokio::spawn(async { consume_stack(DEPTH) })
                .await
                .expect("deep-stack task must complete without overflowing")
        });
        std::hint::black_box(out);
    }
}
