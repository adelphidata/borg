//! Per-tick advisory lock for the scheduler dispatcher.
//!
//! Sibling of [`crate::pidlock`], but with different semantics:
//!
//! - **Non-blocking.** A collision returns `Ok(None)` so the dispatcher can
//!   skip the tick and try again on the next interval. Blocking here would
//!   turn a defensive lock into a stall vector.
//! - **Per-tick lifetime.** The lock is acquired around the claim+dispatch
//!   phase of each iteration and dropped before the loop awaits the next
//!   `interval.tick()`. The pidlock, by contrast, is held for the whole
//!   daemon lifetime.
//!
//! What this guards: out-of-process tick callers (a future manual CLI tick,
//! maintenance hooks) and a re-entrant in-process iteration if a tick stalls
//! past `DAEMON_LOOP_INTERVAL` and the next `interval.tick()` fires
//! immediately. SQLite's `BEGIN IMMEDIATE` already serializes concurrent
//! `claim_due_tasks` writers — this lock is the explicit, named gate that
//! makes "no two ticks run concurrently" a verifiable property rather than
//! an emergent one.

use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::path::Path;

/// Holds an OS-level exclusive `flock` for the lifetime of the value.
/// Dropping closes the file handle, which causes the kernel to release the
/// lock.
#[derive(Debug)]
pub struct TickLock {
    /// File handle holding the flock. Dropping releases the lock.
    /// (Underscored — the field is never read; its only job is to live.)
    _file: File,
}

impl TickLock {
    /// Try to acquire the tick lock at `path` without blocking.
    ///
    /// - `Ok(Some(_))` — lock acquired; caller may enter the critical section.
    /// - `Ok(None)` — another holder owns the lock; caller must skip this tick.
    /// - `Err(_)` — filesystem error opening the lock file (rare; surface to logs).
    pub fn try_acquire(path: &Path) -> Result<Option<Self>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("opening tick lock file {}", path.display()))?;

        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { _file: file })),
            // fs2 surfaces lock contention as `ErrorKind::WouldBlock` on
            // both Unix (EWOULDBLOCK from flock) and Windows. That's the
            // expected skip-this-tick path. Anything else is a real
            // filesystem failure and should propagate so the caller can
            // warn-log it.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(anyhow::Error::from(e)
                .context(format!("locking tick lock file {}", path.display()))),
        }
    }
}

impl Drop for TickLock {
    fn drop(&mut self) {
        // Intentionally do NOT unlink the lock file. The flock is the source
        // of truth — the kernel releases it when the file handle closes here.
        // A persistent file at the path is harmless: the next acquirer simply
        // re-opens and re-locks. Unlinking would race against a concurrent
        // acquire on a different inode (same path, different file) and break
        // the mutual-exclusion invariant.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn acquire_succeeds_when_lock_free() {
        // Real failure mode: open() silently fails (e.g. parent dir missing,
        // permissions wrong) and the dispatcher would interpret a None return
        // as "collision, skip" and never claim any task.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("scheduler.tick.lock");

        let result = TickLock::try_acquire(&path).expect("acquire on free path");
        assert!(
            result.is_some(),
            "fresh path must yield Ok(Some), got Ok(None)"
        );
    }

    #[test]
    fn second_acquire_returns_none_while_first_held() {
        // Real failure mode: someone swaps `try_lock_exclusive` for a
        // blocking `lock_exclusive` and the dispatcher self-deadlocks on the
        // next iteration. Asserting Ok(None) (not blocking, not Err) pins
        // both the non-blocking semantic and the skip-on-collision contract.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("scheduler.tick.lock");

        let _first = TickLock::try_acquire(&path)
            .expect("first acquire")
            .expect("first must succeed");

        let second = TickLock::try_acquire(&path).expect("second call must not error");
        assert!(
            second.is_none(),
            "second acquire while first held must return Ok(None)"
        );
    }

    #[test]
    fn drop_releases_lock_so_next_acquire_succeeds() {
        // Real failure mode: the `_file` field gets renamed away or moved
        // out, the flock outlives the TickLock value, and the dispatcher
        // can never re-acquire the lock on the next iteration. Without this
        // test, that regression would only show up as "scheduler stops
        // claiming after the first tick" in production.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("scheduler.tick.lock");

        {
            let _lock = TickLock::try_acquire(&path)
                .expect("first")
                .expect("first must succeed");
        } // drop here releases the kernel flock

        let again = TickLock::try_acquire(&path)
            .expect("re-acquire after drop")
            .expect("re-acquire must succeed once first lock is dropped");
        drop(again);
    }

    #[test]
    fn lock_file_persists_across_acquires() {
        // Real failure mode: a Drop impl that unlinks the file would race
        // with a concurrent acquire (different inode → both holders think
        // they own the lock). Pin "Drop must not unlink" by asserting the
        // file still exists after the lock is released.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("scheduler.tick.lock");

        {
            let _lock = TickLock::try_acquire(&path)
                .expect("acquire")
                .expect("must succeed");
        }

        assert!(
            path.exists(),
            "lock file must persist after Drop so next acquire re-locks the same inode"
        );
    }
}
