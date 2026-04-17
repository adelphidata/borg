//! End-to-end hook tests: on-disk `hooks.json` → loader → `HookRegistry` → dispatch → side effect.
//!
//! These tests exercise the full path from config file to the agent-facing
//! `HookRegistry` API so regressions between layers surface here. They do not
//! stand up a full `Agent` (mocking the LLM is out of scope) but they do
//! replicate the exact call sequence used by `repl.rs` / `tui/mod.rs` at
//! startup (load_from_file → register → dispatch).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use borg_core::hooks::{HookAction, HookContext, HookData, HookPoint, HookRegistry, ScriptHook};

fn write_hooks(dir: &Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("hooks.json");
    std::fs::write(&path, body).expect("write hooks.json");
    path
}

fn register_all(cfg_path: &Path) -> HookRegistry {
    let mut reg = HookRegistry::new();
    for hook in ScriptHook::load_from_file(cfg_path) {
        reg.register(Box::new(hook));
    }
    reg
}

fn tool_call_ctx(point: HookPoint, tool: &str) -> HookContext {
    HookContext {
        point,
        session_id: "s_e2e".to_string(),
        turn_count: 1,
        data: HookData::ToolCall {
            name: tool.to_string(),
            args: "{}".to_string(),
        },
    }
}

fn tool_result_ctx(point: HookPoint, tool: &str) -> HookContext {
    HookContext {
        point,
        session_id: "s_e2e".to_string(),
        turn_count: 1,
        data: HookData::ToolResult {
            name: tool.to_string(),
            result: "ok".to_string(),
            is_error: false,
        },
    }
}

/// Config-driven hook runs a side effect that the test can observe.
#[test]
fn e2e_post_tool_use_fires() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let marker = tmp.path().join("fired.txt");
    let body = format!(
        r#"{{"hooks":{{"PostToolUse":[{{"matcher":"run_shell","hooks":[{{"type":"command","command":"touch '{}'"}}]}}]}}}}"#,
        marker.display()
    );
    let cfg = write_hooks(tmp.path(), &body);
    let reg = register_all(&cfg);
    assert_eq!(reg.hook_count(), 1);

    let action = reg.dispatch(&tool_result_ctx(HookPoint::AfterToolCall, "run_shell"));
    assert!(matches!(action, HookAction::Continue));
    assert!(marker.exists(), "hook side effect must be observable");
}

/// PreToolUse hook exiting non-zero must map to Skip (the agent-loop's tool-skip path).
#[test]
fn e2e_pre_tool_use_blocks_tool() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let body = r#"{"hooks":{"PreToolUse":[{"matcher":"run_shell","hooks":[{"type":"command","command":"exit 1"}]}]}}"#;
    let cfg = write_hooks(tmp.path(), body);
    let reg = register_all(&cfg);

    let action = reg.dispatch(&tool_call_ctx(HookPoint::BeforeToolCall, "run_shell"));
    assert!(
        matches!(action, HookAction::Skip),
        "PreToolUse non-zero exit must produce Skip"
    );
}

/// PreToolUse hook only blocks tools that match its `matcher`.
#[test]
fn e2e_pre_tool_use_non_matching_tool_continues() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let body = r#"{"hooks":{"PreToolUse":[{"matcher":"run_shell","hooks":[{"type":"command","command":"exit 1"}]}]}}"#;
    let cfg = write_hooks(tmp.path(), body);
    let reg = register_all(&cfg);

    // A different tool name — matcher excludes, hook does not fire.
    let action = reg.dispatch(&tool_call_ctx(HookPoint::BeforeToolCall, "read_file"));
    assert!(matches!(action, HookAction::Continue));
}

/// The core guarantee: a broken user hook MUST NOT produce a failure that would
/// propagate into the agent loop. Loader returns `Continue`, the agent carries on.
#[test]
fn e2e_broken_hook_does_not_abort_turn() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let body = r#"{"hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"/definitely/nonexistent/binary arg"}]}]}}"#;
    let cfg = write_hooks(tmp.path(), body);
    let reg = register_all(&cfg);

    let action = reg.dispatch(&tool_result_ctx(HookPoint::AfterToolCall, "run_shell"));
    // sh is present so spawn succeeds; command not found → exit 127; PostToolUse
    // treats non-zero as Continue. Net effect: the agent loop sees a benign Continue.
    assert!(matches!(action, HookAction::Continue));
}

/// Malformed config file must not crash the loader — register zero hooks, keep going.
#[test]
fn e2e_malformed_config_loads_zero_hooks() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let cfg = write_hooks(tmp.path(), "this is not json {{{");
    let reg = register_all(&cfg);
    assert_eq!(reg.hook_count(), 0);

    // Registry still dispatches fine with zero hooks.
    let action = reg.dispatch(&tool_result_ctx(HookPoint::AfterToolCall, "read_file"));
    assert!(matches!(action, HookAction::Continue));
}

/// Hung hook subprocess must be killed within its timeout so the agent never hangs.
#[test]
fn e2e_hung_hook_is_killed_by_timeout() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let body = r#"{"hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"sleep 30","timeout":1}]}]}}"#;
    let cfg = write_hooks(tmp.path(), body);
    let reg = register_all(&cfg);

    let start = Instant::now();
    let action = reg.dispatch(&tool_result_ctx(HookPoint::AfterToolCall, "run_shell"));
    let elapsed = start.elapsed();
    assert!(matches!(action, HookAction::Continue));
    assert!(
        elapsed < Duration::from_secs(3),
        "hook should be reaped well under its timeout budget, elapsed={elapsed:?}"
    );
}

/// Multiple hooks at the same event are all dispatched.
#[test]
fn e2e_multiple_hooks_all_fire() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let marker_a = tmp.path().join("a.txt");
    let marker_b = tmp.path().join("b.txt");
    let body = format!(
        r#"{{"hooks":{{"PostToolUse":[
            {{"hooks":[{{"type":"command","command":"touch '{}'"}}]}},
            {{"hooks":[{{"type":"command","command":"touch '{}'"}}]}}
        ]}}}}"#,
        marker_a.display(),
        marker_b.display()
    );
    let cfg = write_hooks(tmp.path(), &body);
    let reg = register_all(&cfg);
    assert_eq!(reg.hook_count(), 2);

    let _ = reg.dispatch(&tool_result_ctx(HookPoint::AfterToolCall, "run_shell"));
    assert!(marker_a.exists() && marker_b.exists());
}

/// Skip from an earlier PreToolUse hook short-circuits later ones in the registry
/// — mirrors the existing `HookRegistry::dispatch` semantics the agent loop relies on.
#[test]
fn e2e_skip_short_circuits_later_hooks() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let marker = tmp.path().join("should_not_exist.txt");
    let body = format!(
        r#"{{"hooks":{{"PreToolUse":[
            {{"hooks":[{{"type":"command","command":"exit 1"}}]}},
            {{"hooks":[{{"type":"command","command":"touch '{}'"}}]}}
        ]}}}}"#,
        marker.display()
    );
    let cfg = write_hooks(tmp.path(), &body);
    let reg = register_all(&cfg);
    assert_eq!(reg.hook_count(), 2);

    let action = reg.dispatch(&tool_call_ctx(HookPoint::BeforeToolCall, "run_shell"));
    assert!(matches!(action, HookAction::Skip));
    assert!(
        !marker.exists(),
        "second hook MUST NOT run after first returns Skip"
    );
}
