#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! A task id that matches nothing is an error: `borg tasks …` prints it on
//! stderr and exits non-zero, so a script that deletes or pins a task can tell
//! a typo from success. No real `borgd` starts: auto-start goes to a stub that
//! exits at once, and `BORG_ACP_NO_AUTOSPAWN` stops `connect_or_spawn`.

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

#[test]
fn an_unknown_task_id_fails_the_command() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let stub = tmp.path().join("stub-borgd");
    std::fs::write(&stub, "#!/bin/sh\nexit 1\n").expect("write stub");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).expect("chmod");

    let out = Command::new(env!("CARGO_BIN_EXE_borg"))
        .args(["tasks", "model", "nosuchtask", "some-model"])
        .env("BORG_HOME", tmp.path())
        .env("BORG_DAEMON_BIN", &stub)
        .env("BORG_ACP_NO_AUTOSPAWN", "1")
        .env("BORG_KEK_PASSPHRASE", "task-not-found-test-passphrase")
        .stdin(Stdio::null())
        .output()
        .expect("run borg");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "exited {} for an unknown task", out.status);
    assert!(
        stderr.contains("Task not found: nosuchtask"),
        "stderr does not name the id:\n{stderr}"
    );
    assert!(
        !tmp.path().join("borgd.pid").exists(),
        "a daemon pid file appeared under the temp home: a real borgd was started"
    );
}
