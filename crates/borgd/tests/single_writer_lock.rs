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
