#![allow(clippy::expect_used, clippy::unwrap_used)]

//! End-to-end coverage for `borg mcp test <id>`, the operator's primary
//! one-shot validator for a configured MCP server. Spawns the real
//! `borg` binary against a fresh `BORG_DATA_DIR` and drives the three
//! failure modes the CLI ships with:
//!
//! 1. Global kill switch off (`mcp.enabled = false`) — must refuse with a
//!    clear, non-zero exit; success here would mislead operators into
//!    thinking the agent will pick the server up next turn.
//! 2. Unknown server id — must surface a "no MCP server named …" hint
//!    so a typo doesn't silently look "passing".
//! 3. Configured stdio server whose subprocess does NOT speak MCP — the
//!    handshake must time out and the CLI must exit non-zero. A
//!    regression that lost the handshake timeout would let a busted
//!    server hang the operator's terminal indefinitely.
//!
//! All three live in one `#[test]` because they share a single
//! `BORG_DATA_DIR` — re-initializing the DB between cases would
//! duplicate the per-spawn setup cost (~hundreds of ms) without adding
//! coverage.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

fn borg_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_borg"))
}

/// Spawn `borg <args...>` against the given data dir. Returns
/// (status, stdout, stderr). 30s wallclock cap so a wedged child fails
/// the test instead of hanging CI.
fn run_borg(
    data_dir: &std::path::Path,
    args: &[&str],
) -> (std::process::ExitStatus, String, String) {
    let mut child = Command::new(borg_exe())
        .args(args)
        .env("BORG_DATA_DIR", data_dir)
        // Headless Linux runners take the `FileKekStore` branch when
        // `kek_backend = auto` and the OS keychain is absent. Without a
        // passphrase the very first DB op fails with a KEK-init error
        // and the test misreads "KEK failure" as "kill switch off".
        // Setting a stable passphrase keeps macOS keychain + Linux file
        // backends interchangeable from the test's perspective.
        .env("BORG_KEK_PASSPHRASE", "mcp-cli-test-passphrase")
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

    // Wait with a hard cap. `wait_timeout` isn't in std, so poll.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = child.wait_with_output().expect("wait_with_output");
                return (
                    status,
                    String::from_utf8_lossy(&out.stdout).into_owned(),
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                );
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("`borg {args:?}` did not exit within 30s");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }
}

#[test]
fn mcp_test_cli_surfaces_kill_switch_missing_server_and_handshake_failure() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path();

    // ── Case 1: kill switch off → CLI refuses with the spec error ─────
    // A fresh DB defaults to `mcp.enabled = false`. The CLI must refuse
    // *before* it ever tries to connect; success here would suggest the
    // agent will pick the server up next turn, which it won't.
    let (status, _stdout, stderr) = run_borg(data_dir, &["mcp", "test", "anything"]);
    assert!(
        !status.success(),
        "kill-switch-off case must exit non-zero, got success. stderr:\n{stderr}",
    );
    assert!(
        stderr.contains("MCP is globally disabled"),
        "expected kill-switch hint in stderr, got:\n{stderr}",
    );

    // ── Case 2: enable MCP, ask for an unknown server → spec hint ─────
    let (set_status, _set_out, set_err) =
        run_borg(data_dir, &["settings", "set", "mcp.enabled", "true"]);
    assert!(
        set_status.success(),
        "`settings set mcp.enabled true` failed: stderr:\n{set_err}",
    );

    let (status, _stdout, stderr) = run_borg(data_dir, &["mcp", "test", "no_such_server"]);
    assert!(
        !status.success(),
        "unknown-server case must exit non-zero, got success. stderr:\n{stderr}",
    );
    assert!(
        stderr.contains("no MCP server named") || stderr.contains("no_such_server"),
        "expected missing-server hint in stderr, got:\n{stderr}",
    );

    // ── Case 3: configured stdio server that does not speak MCP ───────
    // `/bin/cat` reads stdin forever without ever emitting a JSON-RPC
    // frame, so the handshake must time out. We force a tight 500ms
    // handshake to keep the test under a few seconds. The CLI must:
    //   - exit non-zero, and
    //   - surface either "handshake timed out" or "handshake failed",
    //     not a silent "connected" banner.
    let (add_status, _add_out, add_err) = run_borg(
        data_dir,
        &[
            "mcp",
            "add",
            "wedged",
            "--stdio",
            "/bin/cat",
            "--connect-timeout-ms",
            "500",
            "--call-timeout-ms",
            "500",
        ],
    );
    assert!(
        add_status.success(),
        "`mcp add wedged --stdio /bin/cat` failed: stderr:\n{add_err}",
    );

    let (status, stdout, stderr) = run_borg(data_dir, &["mcp", "test", "wedged"]);
    assert!(
        !status.success(),
        "handshake-fail case must exit non-zero, got success. stdout:\n{stdout}\nstderr:\n{stderr}",
    );
    let combined = format!("{stderr}{stdout}");
    // `/bin/cat` reads stdin without ever responding, so the 500ms
    // connect timeout is the deterministic failure mode. Accept either
    // the timeout-specific phrasing or the generic "handshake failed"
    // wrapper, but nothing looser — a loose `contains("handshake")` would
    // greenlight a future banner like "skipping handshake because…".
    let handshake_signal =
        combined.contains("handshake timed out") || combined.contains("handshake failed");
    assert!(
        handshake_signal,
        "expected handshake/transport error in CLI output, got:\nstdout:{stdout}\nstderr:{stderr}",
    );
    // Negative: a "connected:" banner from a wedged server is a spec
    // violation — the banner only prints after `tools/list` succeeds.
    assert!(
        !stdout.contains("connected:"),
        "wedged-server case must NOT print the success banner, got stdout:\n{stdout}",
    );
}
