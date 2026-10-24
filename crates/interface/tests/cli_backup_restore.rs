#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! `borg backup restore` into a home that does not exist yet — the move to a
//! new machine. Restore refuses a home that already holds state, and `borg`
//! used to create `borg.db` there during startup (and auto-start a daemon on
//! it) before the restore ran, so it always refused.
//!
//! No daemon starts: auto-start goes to a stub that exits at once, and
//! `BORG_ACP_NO_AUTOSPAWN` stops `connect_or_spawn` from starting one.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn borg(home: &Path, stub: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_borg"))
        .args(args)
        .env("BORG_HOME", home)
        .env("BORG_DAEMON_BIN", stub)
        .env("BORG_ACP_NO_AUTOSPAWN", "1")
        .env("BORG_KEK_PASSPHRASE", "restore-test-passphrase")
        .stdin(Stdio::null())
        .output()
        .expect("run borg")
}

#[test]
fn restore_into_a_fresh_home_succeeds() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let stub = tmp.path().join("stub-borgd");
    std::fs::write(&stub, "#!/bin/sh\nexit 1\n").expect("write stub");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).expect("chmod");

    let source = tmp.path().join("source");
    std::fs::create_dir(&source).expect("source home");
    let seeded = Command::new("sqlite3")
        .arg(source.join("borg.db"))
        .arg(
            "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL, \
             updated_at INTEGER NOT NULL DEFAULT 0); \
             INSERT INTO settings(key, value) VALUES ('security.kek_backend', 'file'), \
             ('user.name', 'Restore Probe');",
        )
        .status()
        .expect("run sqlite3");
    assert!(seeded.success());
    let created = borg(&source, &stub, &["backup", "create"]);
    assert!(
        created.status.success(),
        "backup create: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let tarball = std::fs::read_dir(source.join("backups"))
        .expect("backups dir")
        .map(|e| e.expect("entry").path())
        .find(|p| p.to_string_lossy().ends_with(".tar.zst"))
        .expect("a backup tarball");

    let target = tmp.path().join("target");
    let restored = borg(
        &target,
        &stub,
        &["backup", "restore", tarball.to_str().unwrap()],
    );
    assert!(
        restored.status.success(),
        "restore into a fresh home must succeed: {}",
        String::from_utf8_lossy(&restored.stderr)
    );
    let name = Command::new("sqlite3")
        .arg(target.join("borg.db"))
        .arg("SELECT value FROM settings WHERE key = 'user.name'")
        .output()
        .expect("read restored db");
    assert_eq!(
        String::from_utf8_lossy(&name.stdout).trim(),
        "Restore Probe"
    );
}
