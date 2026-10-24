#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Sync command handlers run inside `#[tokio::main]`. One that drives its
//! daemon call with `Handle::current().block_on` panics with "Cannot start a
//! runtime from within a runtime" before it sends anything, so the command
//! is dead on every invocation.
//!
//! No daemon starts: the launcher's auto-start goes to a stub that exits at
//! once, and `BORG_ACP_NO_AUTOSPAWN` stops `connect_or_spawn` from starting
//! the `borgd` built next to `borg` (which ignores `BORG_DAEMON_BIN`, and on a
//! default KEK backend would write its own root CA key over the real one in
//! the OS keychain). Each command fails to connect — the point is that it
//! gets that far, with an error instead of a panic.

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

#[test]
fn daemon_backed_sync_commands_do_not_panic_on_a_nested_runtime() {
    for args in [&["export", "abcd1234"][..], &["auth", "list"][..]] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stub = tmp.path().join("stub-borgd");
        std::fs::write(&stub, "#!/bin/sh\nexit 1\n").expect("write stub");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).expect("chmod");

        let out = Command::new(env!("CARGO_BIN_EXE_borg"))
            .args(args)
            .env("BORG_HOME", tmp.path())
            .env("BORG_DAEMON_BIN", &stub)
            .env("BORG_ACP_NO_AUTOSPAWN", "1")
            .env("BORG_KEK_PASSPHRASE", "nested-runtime-test-passphrase")
            .stdin(Stdio::null())
            .output()
            .expect("run borg");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !tmp.path().join("borgd.pid").exists(),
            "`borg {}` started a real borgd under the temp home",
            args.join(" ")
        );
        assert!(
            !stderr.contains("panicked"),
            "`borg {}` panicked:\n{stderr}",
            args.join(" ")
        );
    }
}

/// `connect_or_spawn` (export, auth, tasks, sessions…) must start the daemon
/// `BORG_DAEMON_BIN` names, like the launcher does. It used to start the
/// `borgd` next to `borg` regardless.
///
/// The home is pinned to the file KEK backend first, so even a regression
/// only starts a scratch daemon that cannot touch the OS keychain; the test
/// stops it and fails.
#[test]
fn auto_start_runs_the_daemon_borg_daemon_bin_names() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let marker = tmp.path().join("stub-invoked");
    let stub = tmp.path().join("stub-borgd");
    std::fs::write(
        &stub,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .expect("write stub");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    let seeded = Command::new("sqlite3")
        .arg(tmp.path().join("borg.db"))
        .arg(
            "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL, \
             updated_at INTEGER NOT NULL DEFAULT 0); \
             INSERT INTO settings(key, value) VALUES ('security.kek_backend', 'file');",
        )
        .status()
        .expect("run sqlite3");
    assert!(seeded.success(), "seed the file KEK backend");

    let out = Command::new(env!("CARGO_BIN_EXE_borg"))
        .args(["export", "abcd1234"])
        .env("BORG_HOME", tmp.path())
        .env("BORG_DAEMON_BIN", &stub)
        .env("BORG_KEK_PASSPHRASE", "auto-start-test-passphrase")
        .stdin(Stdio::null())
        .output()
        .expect("run borg");

    let stray = std::fs::read_to_string(tmp.path().join("borgd.pid")).ok();
    if let Some(pid) = stray.as_deref().map(str::trim) {
        let _ = Command::new("kill").arg(pid).status();
    }
    assert!(
        stray.is_none(),
        "a real borgd started instead of the stub; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        marker.exists(),
        "the stub named by BORG_DAEMON_BIN never ran"
    );
}
