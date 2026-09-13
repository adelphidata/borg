//! Real-subprocess tests for `mcp::transport::stdio`.
//!
//! The pure predicate `is_env_allowed` is unit-tested in `transport::mod`.
//! These cases drive the actual `spawn()` path so a regression in the
//! `env_clear()` + allowlist filter — the only thing standing between
//! `ANTHROPIC_API_KEY` and an untrusted MCP child — fails loudly.
//!
//! Unix-only: the fixtures use `/bin/sh`. The stdio transport itself is
//! cross-platform, but exercising it without a portable shell is more rope
//! than value.

#![cfg(unix)]
// The serialization `env_lock` is held across `.await` *by design* — env
// mutation is process-global and the spawn must observe the set vars.
#![allow(clippy::await_holding_lock, clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mcp::transport::stdio::{spawn, StderrSink, StdioConfig};
use mcp::transport::Transport;

/// Serializes env-mutation across cases. `std::env::set_var` is
/// process-global; without the lock, the credential-leak test could race
/// the cleanup in another case and observe a transient absence.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Set an env var for the guard's lifetime; restore prior value (or unset)
/// on drop. Mirrors the pattern used in `cortex::config::mcp` tests.
struct EnvGuard {
    key: String,
    prev: Option<String>,
}
impl EnvGuard {
    fn set(key: &str, value: &str) -> Self {
        let prev = std::env::var(key).ok();
        // SAFETY: env mutation guarded by `env_lock`.
        unsafe { std::env::set_var(key, value) };
        Self {
            key: key.into(),
            prev,
        }
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: env mutation guarded by `env_lock`.
        unsafe {
            match &self.prev {
                Some(v) => std::env::set_var(&self.key, v),
                None => std::env::remove_var(&self.key),
            }
        }
    }
}

fn cfg(command: &str, args: &[&str], env: HashMap<String, String>) -> StdioConfig {
    StdioConfig {
        server_name: "fixture".into(),
        command: command.into(),
        args: args.iter().map(|s| (*s).to_string()).collect(),
        env,
        max_line_bytes: 0,
    }
}

fn collecting_sink() -> (StderrSink, Arc<Mutex<Vec<String>>>) {
    let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let cloned = Arc::clone(&lines);
    let sink: StderrSink = Arc::new(move |_server: &str, line: &str| {
        cloned.lock().unwrap().push(line.to_string());
    });
    (sink, lines)
}

/// Spawn a `/bin/sh` child that emits a single JSON-RPC notification whose
/// `params` are the values of the named env keys as seen by the child.
/// Missing keys serialize as the literal string `"<missing>"`. Asserting on
/// `<missing>` proves the variable was *not* inherited; asserting on the
/// real value proves it *was*.
///
/// Using the real `spawn()` is what makes this test load-bearing: any
/// regression in `env_clear()` ordering or the allowlist filter shows up as
/// an unexpected value here.
async fn observe_env_via_spawn(
    keys: &[&str],
    explicit: HashMap<String, String>,
) -> HashMap<String, String> {
    // Build a shell snippet that prints one JSON object whose `params`
    // map each key to its value (or `<missing>`). We escape via base64
    // — sidesteps the entire "value contains quote/backslash/newline"
    // quoting minefield without depending on `jq`.
    //
    // Wire: `printf` + a here-doc-free chain so portable across bash/dash.
    // The child writes one line and exits; the stdio transport reads it
    // via `recv()` and decodes into a `Message`.
    let mut script = String::from(r#"printf '{"jsonrpc":"2.0","method":"env","params":{"#);
    for (i, k) in keys.iter().enumerate() {
        if i > 0 {
            script.push(',');
        }
        // `${VAR:-<missing>}` expands to the value or to `<missing>`. We
        // base64-encode the value so any byte (quotes, backslash) survives.
        // `printf %s` then emits a JSON-escape-free string.
        script.push_str(&format!(
            r#""{k}":"'"$(printf "%s" "${{{k}:-<missing>}}" | base64 | tr -d "\\n")"'""#
        ));
    }
    script.push_str(r#"}}\n'"#);

    let stdio = cfg("/bin/sh", &["-c", &script], explicit);
    let (sink, _) = collecting_sink();
    let mut transport = spawn(stdio, sink).await.expect("spawn sh");

    let msg = tokio::time::timeout(Duration::from_secs(2), transport.recv())
        .await
        .expect("recv timed out")
        .expect("recv error")
        .expect("eof before frame");

    let _ = transport.close().await;

    let mut out = HashMap::new();
    let params = msg.params.expect("child emitted no params block");
    let obj = params.as_object().expect("params not an object");
    for (k, v) in obj {
        let b64 = v.as_str().expect("value not a string").to_string();
        // Decode base64 by hand using std — avoids a dev-dep on base64.
        let decoded = b64_decode(&b64).expect("invalid base64 from child");
        out.insert(
            k.clone(),
            String::from_utf8(decoded).expect("non-utf8 env value"),
        );
    }
    out
}

/// Minimal RFC 4648 base64 decode. The child uses GNU/BSD `base64`'s
/// standard alphabet with `=` padding; we tolerate trailing whitespace.
fn b64_decode(s: &str) -> Result<Vec<u8>, &'static str> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let bytes = s.as_bytes();
    let mut chunk = [0u32; 4];
    let mut i = 0;
    while i < bytes.len() {
        let mut pad = 0u32;
        for slot in &mut chunk {
            if i >= bytes.len() {
                return Err("truncated base64");
            }
            let c = bytes[i];
            i += 1;
            *slot = match c {
                b'A'..=b'Z' => u32::from(c - b'A'),
                b'a'..=b'z' => u32::from(c - b'a') + 26,
                b'0'..=b'9' => u32::from(c - b'0') + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' => {
                    pad += 1;
                    0
                }
                _ => return Err("invalid base64 char"),
            };
        }
        let v = (chunk[0] << 18) | (chunk[1] << 12) | (chunk[2] << 6) | chunk[3];
        out.push((v >> 16) as u8);
        if pad < 2 {
            out.push((v >> 8) as u8);
        }
        if pad < 1 {
            out.push(v as u8);
        }
    }
    Ok(out)
}

#[tokio::test]
async fn env_clear_strips_credentials_at_spawn() {
    // Spec: credentials in the daemon's process env MUST NOT leak to an
    // MCP subprocess. The allowlist is the only thing standing between
    // `ANTHROPIC_API_KEY` (and friends) and code shipped by an untrusted
    // third-party server. Failure mode: a future edit broadens
    // `ENV_ALLOWLIST` or removes the `env_clear()` call, silently
    // re-exposing every cached credential to every MCP child.
    let _lock = env_lock();
    let _k1 = EnvGuard::set("BORG_TEST_LEAK_ANTHROPIC_API_KEY", "anthropic-secret");
    let _k2 = EnvGuard::set("BORG_TEST_LEAK_AWS_SECRET_ACCESS_KEY", "aws-secret");
    let _k3 = EnvGuard::set("BORG_TEST_LEAK_BORG_KEK_PASSPHRASE", "kek-secret");
    let _k4 = EnvGuard::set("BORG_TEST_LEAK_GITHUB_TOKEN", "github-secret");

    let observed = observe_env_via_spawn(
        &[
            "BORG_TEST_LEAK_ANTHROPIC_API_KEY",
            "BORG_TEST_LEAK_AWS_SECRET_ACCESS_KEY",
            "BORG_TEST_LEAK_BORG_KEK_PASSPHRASE",
            "BORG_TEST_LEAK_GITHUB_TOKEN",
            "PATH",
        ],
        HashMap::new(),
    )
    .await;

    for leaked in [
        "BORG_TEST_LEAK_ANTHROPIC_API_KEY",
        "BORG_TEST_LEAK_AWS_SECRET_ACCESS_KEY",
        "BORG_TEST_LEAK_BORG_KEK_PASSPHRASE",
        "BORG_TEST_LEAK_GITHUB_TOKEN",
    ] {
        assert_eq!(
            observed.get(leaked).map(String::as_str),
            Some("<missing>"),
            "credential {leaked} reached child (observed: {observed:?})"
        );
    }
    // PATH is in the allowlist — must reach the child or no binary lookup works.
    let path = observed.get("PATH").expect("PATH key absent");
    assert_ne!(
        path, "<missing>",
        "PATH must be allowlisted into the child; observed: {observed:?}"
    );
}

#[tokio::test]
async fn explicit_env_reaches_child_after_clear() {
    // Spec: the operator-declared `env` block (post-secret-resolution) is
    // the supported way for credentials to reach a stdio MCP server. After
    // `env_clear`, the explicit map must be applied so a configured
    // `GITHUB_PERSONAL_ACCESS_TOKEN=ghp_…` actually lands in the child.
    // Failure mode: a code reorder applies `env_clear()` *after* the
    // explicit env loop, silently dropping every operator-configured
    // credential.
    let _lock = env_lock();
    let mut explicit = HashMap::new();
    explicit.insert(
        "BORG_TEST_EXPLICIT_TOKEN".to_string(),
        "abc 123 with spaces \"quoted\"".to_string(),
    );

    let observed = observe_env_via_spawn(&["BORG_TEST_EXPLICIT_TOKEN"], explicit).await;
    assert_eq!(
        observed.get("BORG_TEST_EXPLICIT_TOKEN").map(String::as_str),
        Some("abc 123 with spaces \"quoted\""),
        "explicit env entry did not reach child (observed: {observed:?})",
    );
}

#[tokio::test]
async fn stderr_is_routed_to_sink_not_stdout() {
    // Spec: a child's stderr (typically panics, stack traces, debug prints)
    // is routed to the operator's log via the stderr sink and MUST NOT be
    // surfaced to the LLM via `recv()`. Surfacing stderr would be a
    // credential-exfiltration vector: hostile servers can scribble tokens
    // they discover into stderr and have them re-rendered into the
    // conversation. We assert (a) stderr lands in the sink, and (b) the
    // transport's `recv()` does not return a Message containing the stderr
    // text.
    let _lock = env_lock();

    let (sink, lines) = collecting_sink();
    let stdio = cfg(
        "/bin/sh",
        &["-c", "echo unique-stderr-token-1 1>&2; sleep 0.05"],
        HashMap::new(),
    );
    let mut t = spawn(stdio, sink).await.expect("spawn sh");

    // Give the stderr pump time to read + sink the line, then EOF.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let recv_attempt = tokio::time::timeout(Duration::from_millis(150), t.recv()).await;
    match recv_attempt {
        // The shell wrote nothing to stdout, so EOF is expected: Ok(None).
        Ok(Ok(None)) => {}
        // No frame within the window is also acceptable — the key is that
        // no Message carrying the stderr token reaches the LLM path.
        Err(_) => {}
        Ok(Ok(Some(m))) => panic!(
            "stderr token must not surface via recv as a Message: {:?}",
            m.method
        ),
        Ok(Err(e)) => {
            // Decode error is also fine (stderr should not be valid JSON);
            // what matters is that no Message frame carries the token.
            assert!(
                !format!("{e}").contains("unique-stderr-token-1"),
                "stderr token leaked into recv error path: {e}"
            );
        }
    }

    let _ = t.close().await;

    let captured = lines.lock().unwrap().clone();
    assert!(
        captured.iter().any(|l| l.contains("unique-stderr-token-1")),
        "stderr sink missed the line; captured: {captured:?}"
    );
}

/// Spec: dropping the transport ends the stderr reader even when a stranger
/// still holds the pipe's write end. The pump otherwise lives until EOF, and
/// EOF needs *every* writer gone — a descendant that left the server's
/// process group (`setsid`) survives the watchdog's group kill and keeps the
/// reader task and its fd alive for as long as it likes. The escaped child
/// here writes a line *after* the drop; a pump that was still running would
/// deliver it to the sink.
#[tokio::test]
async fn dropping_the_transport_stops_the_stderr_pump_despite_escaped_writers() {
    let _lock = env_lock();
    let (sink, lines) = collecting_sink();
    // The server (`cat`) blocks on stdin until torn down. Its grandchild
    // escapes to a fresh session, waits past the drop, then writes to the
    // inherited stderr and exits.
    let escaped = r#"perl -e 'use POSIX; POSIX::setsid(); select(undef,undef,undef,0.4); print STDERR "late-stderr-line\n"' & exec cat"#;
    let stdio = cfg("/bin/sh", &["-c", escaped], HashMap::new());
    let t = spawn(stdio, sink).await.expect("spawn sh");
    // Let the pump settle on its first read before we pull the rug.
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(t);
    tokio::time::sleep(Duration::from_millis(900)).await;

    let captured = lines.lock().unwrap().clone();
    assert!(
        !captured.iter().any(|l| l.contains("late-stderr-line")),
        "the stderr pump outlived the transport: {captured:?}"
    );
}

#[tokio::test]
async fn unknown_command_surfaces_spawn_error() {
    // Spec: a missing/typo'd command must fail at `spawn()` time with a
    // clean error rather than returning a transport whose first `recv()`
    // hangs indefinitely. Operators staring at `borg mcp test` deserve a
    // diagnostic, not a frozen CLI.
    let _lock = env_lock();
    let (sink, _) = collecting_sink();
    let stdio = cfg(
        "/nonexistent/path/to/borg-mcp-fixture-binary-xyz",
        &[],
        HashMap::new(),
    );
    let res = spawn(stdio, sink).await;
    let err = res.err().expect("spawn must fail for nonexistent command");
    let msg = format!("{err}");
    assert!(
        msg.contains("failed to spawn") && msg.contains("borg-mcp-fixture-binary-xyz"),
        "error must name the operation and the binary; got: {msg}"
    );
}

#[test]
fn b64_decode_round_trip() {
    // Sanity: the hand-rolled base64 decoder used to read child output
    // matches `base64 | tr -d '\n'` on a corpus that includes the bytes
    // (quote, backslash, newline, NUL) most likely to upset shell quoting.
    // If this test passes but a downstream env-observation test fails,
    // the bug isn't in the decoder.
    let cases: &[(&str, &str)] = &[
        ("hello", "aGVsbG8="),
        ("a\"b\\c\nd", "YSJiXGMKZA=="),
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
    ];
    for (plain, b64) in cases {
        let decoded = b64_decode(b64).expect("decode");
        assert_eq!(decoded, plain.as_bytes(), "decode({b64}) != {plain:?}");
    }
}
