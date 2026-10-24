#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! ctrl-c or `kill` must end a `borg` command. The startup signal listener
//! replaces the default "terminate" action, so a listener that only records
//! the signal leaves the command running until it finishes on its own.
//!
//! The daemon socket is a listener that accepts and never answers, so
//! `borg ask` waits on it and the signal lands in that wait. Daemon auto-start
//! goes to a stub that exits at once, as in `cli_diagnostics_stderr.rs`, so no
//! real `borgd` starts.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn a_signal_ends_a_waiting_command() {
    for (signal, expected_code) in [(libc::SIGINT, 130), (libc::SIGTERM, 143)] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stub_daemon = tmp.path().join("stub-borgd");
        std::fs::write(&stub_daemon, "#!/bin/sh\nexit 1\n").expect("write stub");
        std::fs::set_permissions(&stub_daemon, std::fs::Permissions::from_mode(0o700))
            .expect("chmod stub");
        let listener = UnixListener::bind(tmp.path().join("borgd.sock")).expect("bind socket");
        let (connected_tx, connected_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for conn in listener.incoming() {
                held.push(conn);
                let _ = connected_tx.send(());
            }
        });

        let mut child = Command::new(env!("CARGO_BIN_EXE_borg"))
            .args(["ask", "hello"])
            .env("BORG_HOME", tmp.path())
            .env("BORG_ACP_NO_AUTOSPAWN", "1")
            .env("BORG_DAEMON_BIN", &stub_daemon)
            .env("BORG_KEK_PASSPHRASE", "signal-exit-test-passphrase")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn borg");

        // The signal listener installs before `borg` dials the daemon, so the
        // first connection means it is in place.
        connected_rx
            .recv_timeout(Duration::from_secs(60))
            .expect("`borg ask` never dialed the daemon socket");
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "`borg ask` exited before the signal; the test needs it waiting"
        );
        let pid = i32::try_from(child.id()).expect("pid fits i32");
        // SAFETY: `pid` is our own live child; `kill` has no memory effects.
        assert_eq!(unsafe { libc::kill(pid, signal) }, 0, "send signal");

        let deadline = Instant::now() + Duration::from_secs(3);
        let status = loop {
            match child.try_wait().expect("try_wait") {
                Some(status) => break status,
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("`borg ask` ignored signal {signal} for 3s");
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        };
        assert!(
            status.code() == Some(expected_code) || status.signal() == Some(signal),
            "signal {signal}: expected exit {expected_code}, got {status}"
        );
    }
}
