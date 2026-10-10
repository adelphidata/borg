//! Bounded output draining for child processes whose descendants may outlive
//! them.
//!
//! A shell command like `server & echo started` exits immediately, but the
//! backgrounded `server` inherits the shell's stdout/stderr pipes and holds
//! them open for as long as it runs. Waiting for pipe EOF before reaping the
//! leader therefore blocks until the *descendant* exits — in practice until
//! the tool's timeout fires and throws away the output the leader did
//! produce. [`wait_with_bounded_drain`] reaps the leader as soon as it exits
//! and keeps reading for a short grace period only, so a lingering descendant
//! costs at most [`OUTPUT_DRAIN_AFTER_EXIT`] rather than the whole timeout.

use std::future::Future;
use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use tokio::process::Child;

/// How long output is still read after the process leader has exited.
///
/// Local pipes flush in well under a second; anything still open past this
/// point belongs to a descendant that will keep the pipe open indefinitely.
pub const OUTPUT_DRAIN_AFTER_EXIT: Duration = Duration::from_millis(2_000);

/// Run `read_loop` (which must read the child's stdout/stderr pipes to EOF)
/// concurrently with reaping `child`, and return the child's exit status.
///
/// If the pipes close first, the child is reaped normally. If the child exits
/// first, `read_loop` is given [`OUTPUT_DRAIN_AFTER_EXIT`] more time; when
/// that elapses the loop is dropped — closing this side of the pipes — and the
/// status already collected is returned. `read_loop` must own no state the
/// caller needs after it is dropped mid-way other than through shared
/// borrows (buffers filled so far stay valid).
pub async fn wait_with_bounded_drain<F>(child: &mut Child, read_loop: F) -> io::Result<ExitStatus>
where
    F: Future<Output = ()>,
{
    tokio::pin!(read_loop);
    let exited = tokio::select! {
        _ = &mut read_loop => None,
        status = child.wait() => Some(status),
    };
    match exited {
        None => child.wait().await,
        Some(status) => {
            if tokio::time::timeout(OUTPUT_DRAIN_AFTER_EXIT, &mut read_loop)
                .await
                .is_err()
            {
                tracing::debug!(
                    grace_ms = OUTPUT_DRAIN_AFTER_EXIT.as_millis() as u64,
                    "process exited but a descendant still holds its output pipes; \
                     stopped draining"
                );
            }
            status
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Instant;
    use tokio::io::AsyncReadExt;
    use tokio::process::Command;

    fn spawn_sh(script: &str) -> Child {
        Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh")
    }

    /// A backgrounded descendant that inherits the pipes must not hold the
    /// wait past the grace period, and the leader's own output and status
    /// must survive.
    #[tokio::test]
    async fn descendant_holding_pipes_only_costs_the_grace_period() {
        let mut child = spawn_sh("sleep 30 & echo started; exit 7");
        let mut stdout = child.stdout.take().expect("stdout");
        let mut out = Vec::new();
        let started = Instant::now();
        let status = wait_with_bounded_drain(&mut child, async {
            // A read error here only means the pipe is gone; the buffer
            // keeps what arrived.
            let _ = stdout.read_to_end(&mut out).await;
        })
        .await
        .expect("wait");
        let elapsed = started.elapsed();
        assert_eq!(status.code(), Some(7));
        assert_eq!(String::from_utf8_lossy(&out).trim(), "started");
        assert!(
            elapsed >= OUTPUT_DRAIN_AFTER_EXIT,
            "must give output the full grace period, returned after {elapsed:?}"
        );
        assert!(
            elapsed < OUTPUT_DRAIN_AFTER_EXIT + Duration::from_secs(5),
            "descendant held the wait for {elapsed:?}"
        );
    }

    /// Output written right before exit, and before the pipes close, is
    /// collected in full rather than cut off at the exit instant.
    #[tokio::test]
    async fn output_flushed_after_exit_is_still_collected() {
        // The subshell holds stdout briefly after the leader exits, then
        // writes and closes — well inside the grace period.
        let mut child = spawn_sh("(sleep 0.3; echo late) & echo early");
        let mut stdout = child.stdout.take().expect("stdout");
        let mut out = Vec::new();
        let started = Instant::now();
        let status = wait_with_bounded_drain(&mut child, async {
            let _ = stdout.read_to_end(&mut out).await;
        })
        .await
        .expect("wait");
        assert!(status.success());
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("early") && text.contains("late"),
            "got {text:?}"
        );
        assert!(
            started.elapsed() < OUTPUT_DRAIN_AFTER_EXIT,
            "pipes closed early, so the wait must not run out the grace period"
        );
    }

    /// The common case: pipes close with the process; no grace wait is paid.
    #[tokio::test]
    async fn clean_exit_returns_without_grace_delay() {
        let mut child = spawn_sh("echo done");
        let mut stdout = child.stdout.take().expect("stdout");
        let mut out = Vec::new();
        let started = Instant::now();
        let status = wait_with_bounded_drain(&mut child, async {
            let _ = stdout.read_to_end(&mut out).await;
        })
        .await
        .expect("wait");
        assert!(status.success());
        assert_eq!(String::from_utf8_lossy(&out).trim(), "done");
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
