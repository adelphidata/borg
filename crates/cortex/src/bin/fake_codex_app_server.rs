//! Hermetic stand-in for `codex app-server`, used only by integration tests.
//!
//! Speaks just enough of the JSON-RPC wire protocol to drive the client through
//! a happy-path turn: `initialize` → `initialized` (notif) → `thread/start` →
//! `turn/start` → streamed `item/*` notifications → `turn/completed`.
//!
//! The behavior is hard-coded — this is a test fixture, not a re-implementation.
//! When invoked with the env var `FAKE_CODEX_MODE=garbage`, it emits a malformed
//! line so the client's error-path can be exercised.

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
