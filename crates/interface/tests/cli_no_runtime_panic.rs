#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Sync command handlers run inside `#[tokio::main]`. One that drives its
//! daemon call with `Handle::current().block_on` panics with "Cannot start a
//! runtime from within a runtime" before it sends anything, so the command
//! is dead on every invocation.
//!
//! Daemon auto-start goes to a stub that exits at once, so each command fails
//! to connect — the point is that it gets that far, with an error instead of
//! a panic.

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

#[test]
fn daemon_backed_sync_commands_do_not_panic_on_a_nested_runtime() {
    for args in [&["export", "abcd1234"][..]] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stub = tmp.path().join("stub-borgd");
        std::fs::write(&stub, "#!/bin/sh\nexit 1\n").expect("write stub");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).expect("chmod");

        let out = Command::new(env!("CARGO_BIN_EXE_borg"))
            .args(args)
            .env("BORG_HOME", tmp.path())
            .env("BORG_DAEMON_BIN", &stub)
            .env("BORG_KEK_PASSPHRASE", "nested-runtime-test-passphrase")
            .stdin(Stdio::null())
            .output()
            .expect("run borg");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("panicked"),
            "`borg {}` panicked:\n{stderr}",
            args.join(" ")
        );
    }
}
