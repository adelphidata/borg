#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! A reader that stops early (`borg logs | head`, `borg settings | grep -q`)
//! closes the pipe while `borg` still has output to write. That must end the
//! output, not the command: `println!` panics on the broken pipe, and the
//! command exits 101 with a panic message instead of 0.
//!
//! Daemon auto-start goes to a stub that exits at once, as in
//! `cli_diagnostics_stderr.rs`, so no real `borgd` starts.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn a_reader_that_stops_early_does_not_crash_the_command() {
    for args in [
        &["logs", "--count", "1000"][..],
        &["logs", "--activity"][..],
        &["settings"][..],
    ] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stub_daemon = tmp.path().join("stub-borgd");
        std::fs::write(&stub_daemon, "#!/bin/sh\nexit 1\n").expect("write stub");
        std::fs::set_permissions(&stub_daemon, std::fs::Permissions::from_mode(0o700))
            .expect("chmod stub");
        write_history(tmp.path());

        let mut child = Command::new(env!("CARGO_BIN_EXE_borg"))
            .args(args)
            .env("BORG_HOME", tmp.path())
            .env("BORG_DAEMON_BIN", &stub_daemon)
            .env("BORG_KEK_PASSPHRASE", "broken-pipe-test-passphrase")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn borg");

        // Close the pipe before `borg` writes, the way `| true` or
        // `| grep -q` does once it has what it needs.
        drop(child.stdout.take());
        let mut stderr_pipe = child.stderr.take().expect("stderr");
        let stderr_reader = std::thread::spawn(move || {
            let mut stderr = String::new();
            stderr_pipe
                .read_to_string(&mut stderr)
                .expect("read stderr");
            stderr
        });

        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            match child.try_wait().expect("try_wait") {
                Some(status) => break status,
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("`borg {}` did not exit within 60s", args.join(" "));
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        };
        let stderr = stderr_reader.join().expect("stderr thread");

        assert!(
            !tmp.path().join("borgd.sock").exists(),
            "a daemon socket appeared under the temp home: a real borgd was started"
        );
        assert!(
            status.success() && !stderr.contains("panicked"),
            "`borg {}` exited {status} after the reader left; stderr:\n{stderr}",
            args.join(" ")
        );
    }
}

/// Conversation history under today's and tomorrow's file names: `borg`
/// picks the name after the daemon wait, which can cross midnight.
fn write_history(home: &std::path::Path) {
    let logs = home.join("logs");
    std::fs::create_dir_all(&logs).expect("logs dir");
    let entry = serde_json::json!({
        "timestamp": "2026-10-19T10:30:00+00:00",
        "type": "user_message",
        "content": "audit the dependencies",
    });
    let history = format!("{entry}\n").repeat(1000);
    let today = chrono::Local::now();
    for day in [today, today + chrono::Duration::days(1)] {
        std::fs::write(
            logs.join(format!("{}.jsonl", day.format("%Y-%m-%d"))),
            &history,
        )
        .expect("write history");
    }
}
