//! Two `PidLock::acquire` calls against the same path can never both succeed.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use borgd::pidlock::PidLock;

#[test]
fn second_acquire_in_same_process_fails_fast_with_path_in_message() {
    // Real failure mode: if flock isn't acquired exclusively, two borgd
    // processes could both open the SQLite DB writable and corrupt state.
    // The error must mention the lock file path so operators can find it.
    let dir = tempfile::tempdir().expect("tempdir");
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
fn stale_pidfile_recovery() {
    // Real failure mode: a daemon that crashed without releasing its pidfile
    // would leave a stale PID on disk. A new daemon must still acquire the
    // lock (because the kernel released the flock when the prior fd closed)
    // and overwrite the pidfile with its own PID — otherwise operators
    // looking at borgd.pid would see a ghost PID and miss the live daemon.
    //
    // We pre-write a non-existent PID into the file, acquire the lock, and
    // assert the file now names the current process. PID 999_999 is chosen
    // so it almost certainly does not exist; we additionally assert via
    // `kill(pid, 0) == ESRCH` that the OS agrees.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("borgd.pid");

    let stale_pid: i32 = 999_999;
    // Sanity-check: the chosen PID must be unused on this host. If a real
    // process happens to hold it, skip the kill check rather than fabricating
    // success — we still exercise the recovery path.
    #[cfg(unix)]
    unsafe {
        let rc = libc::kill(stale_pid, 0);
        if rc == 0 {
            // Live process; the test still proves recovery because the
            // pidfile is informational, but log the unusual condition.
            eprintln!("warning: PID {stale_pid} is live on this host");
        } else {
            let err = std::io::Error::last_os_error();
            assert_eq!(
                err.raw_os_error(),
                Some(libc::ESRCH),
                "expected ESRCH for unused pid {stale_pid}, got {err}"
            );
        }
    }

    std::fs::write(&path, format!("{stale_pid}\n")).expect("seed stale pidfile");

    let _lock = PidLock::acquire(&path).expect("acquire over stale pidfile");

    let after = std::fs::read_to_string(&path).expect("read pidfile");
    let written: u32 = after.trim().parse().expect("pidfile must be numeric");
    assert_eq!(
        written,
        std::process::id(),
        "stale pidfile must be overwritten with the current PID, got {written}"
    );
}

#[test]
fn dropping_first_lock_lets_second_acquire_succeed() {
    // Real failure mode: a Drop that forgot to release the flock would mean
    // restarting the daemon falsely reports a conflict. Since the kernel
    // releases the flock when the file handle drops (no unlink needed), this
    // test demonstrates the intended lifecycle.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("borgd.pid");
    {
        let _lock = PidLock::acquire(&path).expect("first");
    }
    PidLock::acquire(&path).expect("second acquire after drop");
}
