#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! `borg --resume <id>` with an id that matches no session must fail: a
//! script that resumes a session by id has to see that it did not happen.
//! It printed the error and exited 0.
//!
//! Daemon auto-start goes to a stub that exits at once, as in
//! `cli_diagnostics_stderr.rs`, so no real `borgd` starts.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn resume_with_an_unknown_id_exits_non_zero() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let stub_daemon = tmp.path().join("stub-borgd");
    std::fs::write(&stub_daemon, "#!/bin/sh\nexit 1\n").expect("write stub");
    std::fs::set_permissions(&stub_daemon, std::fs::Permissions::from_mode(0o700))
        .expect("chmod stub");
    // A present database marks the home as onboarded, so the command goes
    // straight to the resume instead of the setup wizard.
    std::fs::write(tmp.path().join("borg.db"), b"").expect("write db");

    let output = Command::new(env!("CARGO_BIN_EXE_borg"))
        .args(["--resume", "nosuchid"])
        .env("BORG_HOME", tmp.path())
        .env("BORG_ACP_NO_AUTOSPAWN", "1")
        .env("BORG_DAEMON_BIN", &stub_daemon)
        .env("BORG_KEK_PASSPHRASE", "resume-not-found-test-passphrase")
        .output()
        .expect("run borg");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !tmp.path().join("borgd.sock").exists(),
        "a daemon socket appeared under the temp home: a real borgd was started"
    );
    assert!(
        stderr.contains("nosuchid"),
        "the error must name the id; stderr:\n{stderr}"
    );
    assert!(
        !output.status.success(),
        "`borg --resume nosuchid` exited {}; stderr:\n{stderr}",
        output.status
    );
}
