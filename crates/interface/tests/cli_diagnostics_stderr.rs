#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Diagnostics from a non-TUI `borg` command must go to stderr. Stdout is
//! the data channel (`borg ask --json`, `borg settings get`), and a tracing
//! line there corrupts whatever is parsing it.
//!
//! The warning is provoked deterministically: every non-service command
//! auto-starts the daemon, and `BORG_DAEMON_BIN` points at a stub that
//! records its invocation and exits at once, so the launcher's "UDS never
//! became reachable" `warn!` fires and no real `borgd` is ever started. A
//! missing path would not do: the launcher falls back to the sibling
//! `target/debug/borgd`, and a real daemon would outlive the test.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

fn borg_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_borg"))
}

#[cfg(unix)]
#[test]
fn non_tui_diagnostics_go_to_stderr_not_stdout() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let invoked_marker = tmp.path().join("stub-invoked");
    let stub_daemon = tmp.path().join("stub-borgd");
    std::fs::write(
        &stub_daemon,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", invoked_marker.display()),
    )
    .expect("write stub");
    std::fs::set_permissions(&stub_daemon, std::fs::Permissions::from_mode(0o700))
        .expect("chmod stub");

    let mut child = Command::new(borg_exe())
        .args(["settings", "get", "mcp.enabled"])
        .env("BORG_HOME", tmp.path())
        .env("BORG_DAEMON_BIN", &stub_daemon)
        .env("BORG_KEK_PASSPHRASE", "diagnostics-test-passphrase")
        .env("RUST_LOG", "warn")
        .env(
            "PATH",
            std::env::var_os("PATH").unwrap_or_else(|| std::ffi::OsString::from("/usr/bin")),
        )
        .env(
            "HOME",
            std::env::var_os("HOME").unwrap_or_else(|| std::ffi::OsString::from("/tmp")),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn borg");

    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let output = loop {
        match child.try_wait() {
            Ok(Some(_)) => break child.wait_with_output().expect("wait_with_output"),
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("`borg settings get` did not exit within 60s");
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => panic!("try_wait failed: {e}"),
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        invoked_marker.exists(),
        "the stub daemon was not what the launcher spawned; stderr:\n{stderr}"
    );
    assert!(
        !tmp.path().join("borgd.sock").exists(),
        "a daemon socket appeared under the temp home: a real borgd was started"
    );

    let marker = "Auto-start borgd:";
    assert!(
        stderr.contains(marker) && stderr.contains("never became reachable"),
        "expected the launcher warning on stderr; stderr:\n{stderr}\nstdout:\n{stdout}"
    );
    assert!(
        !stdout.contains(marker) && !stdout.contains("WARN"),
        "diagnostics leaked onto stdout:\n{stdout}"
    );
}
