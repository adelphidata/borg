//! Hermetic stand-in for `codex app-server`, used only by integration tests.
//!
//! Speaks just enough of the JSON-RPC wire protocol to drive the client through
//! a happy-path turn: `initialize` → `initialized` (notif) → `thread/start` →
//! `turn/start` → streamed `item/*` notifications → `turn/completed`.
//!
//! The behavior is hard-coded — this is a test fixture, not a re-implementation.
//! `FAKE_CODEX_MODE` env-var modes:
//!
//! - `garbage` — emit a malformed line on `initialize` and keep stdin open, so
//!   the client's error-path can be exercised without subprocess exit.
//! - `exit_after_init` — respond to `initialize` normally, then exit. Used to
//!   drive the client's reader to EOF without going through `close()`.

use std::io::{BufRead, BufReader, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let reader = BufReader::new(stdin.lock());

    let mode = std::env::var("FAKE_CODEX_MODE").unwrap_or_default();

    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let id = msg.get("id").cloned();

        match method {
            "initialize" => {
                if mode == "garbage" {
                    let _ = writeln!(stdout, "this is not json {{{{");
                    let _ = stdout.flush();
                    continue;
                }
                send(
                    &mut stdout,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "userAgent": "fake-codex/0.0",
                            "codexHome": "/tmp/fake-codex",
                            "platformFamily": "test",
                            "platformOs": "test",
                        }
                    }),
                );
            }
            "initialized" => {
                // notification — no response
                if mode == "exit_after_init" {
                    // Drop stdout/stdin and exit so the client's reader sees
                    // EOF without the client having to issue close().
                    // Exercises the EOF-race short-circuit. We wait until
                    // after `initialized` so `client.initialize()` returns
                    // cleanly before the reader sees EOF.
                    return;
                }
            }
            "thread/start" => {
                send(
                    &mut stdout,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "threadId": "thread-fake-1" }
                    }),
                );
            }
            "turn/start" => {
                // Stream three notifications, then the turn/completed notification,
                // then the response. This shape (notifications before response) is
                // what the real codex sends; the client must not deadlock waiting
                // for a response while notifications back up.
                send(
                    &mut stdout,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "item/agentMessage",
                        "params": { "promptId": "p1", "itemId": "i1", "text": "hello" }
                    }),
                );
                send(
                    &mut stdout,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "item/commandExecution",
                        "params": { "promptId": "p1", "itemId": "i2", "cmd": "echo hi" }
                    }),
                );
                send(
                    &mut stdout,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "item/agentMessage",
                        "params": { "promptId": "p1", "itemId": "i3", "text": "done" }
                    }),
                );
                send(
                    &mut stdout,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "turn/completed",
                        "params": { "promptId": "p1" }
                    }),
                );
                send(
                    &mut stdout,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "promptId": "p1" }
                    }),
                );
            }
            "request/approval-probe" => {
                // Emit a server-initiated request so the client's server-request
                // queue can be exercised. Caller is expected to respond.
                send(
                    &mut stdout,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 9000,
                        "method": "exec/approve",
                        "params": { "cmd": "rm -rf /" }
                    }),
                );
                send(
                    &mut stdout,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {}
                    }),
                );
            }
            _ => {
                if let Some(id) = id {
                    send(
                        &mut stdout,
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32601, "message": format!("method not found: {method}") }
                        }),
                    );
                }
            }
        }
    }
}

fn send<W: Write>(stdout: &mut W, value: serde_json::Value) {
    let line = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string());
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}
