//! Single-instance lock for borgd via `flock` on `$BORG_HOME/borgd.pid`.
//!
//! The hard guarantee that only one daemon writes to `borg.db`. We acquire an
//! exclusive non-blocking lock on the PID file at startup; if another live
//! daemon holds it, we exit with a clear error. A stale PID file (process is
//! gone) is silently overwritten — `flock` on the stale file succeeds because
//! the kernel released it when the prior process died.

use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

/// Holds the OS-level flock for the lifetime of the value. Dropping the value
/// closes the file handle, which causes the kernel to release the lock.
#[derive(Debug)]
pub struct PidLock {
    /// File handle holding the flock. Kept alive for the lock's lifetime and
    /// unlocked explicitly on drop.
    file: File,
}

impl PidLock {
    /// Acquire the lock at `path`. Writes the current process's PID into the
    /// file on success. Returns an error whose message names `path` when
    /// another live daemon already holds it.
    pub fn acquire(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("opening PID file {}", path.display()))?;

        file.try_lock_exclusive().map_err(|e| {
            anyhow!(
                "another borgd appears to be running (PID file {} is locked: {})",
                path.display(),
                e
            )
        })?;

        // We hold the lock — safe to truncate and rewrite the PID.
        let pid = std::process::id();
        let mut writer = file.try_clone().context("cloning PID file handle")?;
        writer.set_len(0).context("truncating PID file")?;
        writeln!(writer, "{pid}").context("writing PID")?;

        Ok(Self { file })
    }
}

impl Drop for PidLock {
    fn drop(&mut self) {
        // Release the flock explicitly rather than relying on the close.
        // Closing only drops the lock once *every* descriptor referring to
        // this open file description is gone — and a child forked by another
        // thread between our `open` and its `exec` briefly owns one. An
        // explicit unlock releases the description's lock immediately, so a
        // restart can never see a phantom "already running".
        if let Err(e) = fs2::FileExt::unlock(&self.file) {
            tracing::warn!(error = %e, "releasing borgd PID file lock");
        }

        // Intentionally do NOT unlink the PID file. The flock is the source of
        // truth — the kernel releases it when the file handle closes (here),
        // and a stale file at the path is harmless because the next acquirer
        // will simply truncate and re-lock. Unlinking would race against a
        // concurrent restart (e.g. systemd Restart=on-failure): if a new
        // daemon B opens-and-locks the file between this Drop running and
        // unlinking, we'd delete B's PID file and a third daemon C could
        // start a brand-new file on a different inode → two live daemons,
        // both writing to borg.db.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn second_acquire_in_same_process_fails_with_path_in_message() {
        // Real failure mode: if flock isn't acquired exclusively, two borgd
        // processes could both open the SQLite DB writable and corrupt state.
        // This test exercises the actual flock path and asserts the error
        // message names the file so operators can find the conflict.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("borgd.pid");

        let _first = PidLock::acquire(&path).expect("first acquire");
        let err = PidLock::acquire(&path).expect_err("second acquire must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("borgd.pid"),
            "error must name the lock file path; got: {msg}"
        );
    }

    #[test]
    fn drop_releases_lock_so_next_acquire_succeeds() {
        // Real failure mode: if Drop forgets to release or the file isn't
        // unlinked, restarting the daemon would falsely report a conflict.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("borgd.pid");

        {
            let _lock = PidLock::acquire(&path).expect("first");
        } // drop here

        // The file may have been removed by Drop; either way, a fresh acquire
        // should succeed.
        PidLock::acquire(&path).expect("second acquire after drop");
    }

    #[test]
    fn drop_releases_lock_even_when_a_duplicate_descriptor_survives() {
        // Real failure mode: borgd spawns children, and a fork by another
        // thread duplicates every open descriptor for the moment before exec.
        // The kernel keeps a flock alive until *all* descriptors on the open
        // file description close, so relying on the close alone would leave a
        // phantom lock behind and make the next `borgd` start refuse to boot.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("borgd.pid");

        let duplicate = {
            let lock = PidLock::acquire(&path).expect("first");
            // Stands in for the descriptor a forked child holds; it outlives
            // the PidLock exactly the way that child's copy can.
            lock.file.try_clone().expect("duplicate descriptor")
        };

        PidLock::acquire(&path)
            .expect("acquire must succeed with a duplicate descriptor still open");
        drop(duplicate);
    }

    #[test]
    fn pid_file_contents_are_current_process_id() {
        // Real failure mode: if we don't truncate before writing, a stale PID
        // from a prior crashed daemon would remain and confuse stale-detection
        // logic.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("borgd.pid");

        let _lock = PidLock::acquire(&path).expect("acquire");
        let contents = std::fs::read_to_string(&path).expect("read pid file");
        let pid: u32 = contents.trim().parse().expect("pid file must be numeric");
        assert_eq!(pid, std::process::id());
    }
}
