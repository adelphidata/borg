//! Rate guard integration tests.
//!
//! Tests multi-action workflows, hot reload, and gateway-specific limits.

#![allow(
    clippy::approx_constant,
    clippy::assertions_on_constants,
    clippy::const_is_empty,
    clippy::expect_used,
    clippy::field_reassign_with_default,
    clippy::identity_op,
    clippy::items_after_test_module,
    clippy::len_zero,
    clippy::manual_range_contains,
    clippy::needless_borrow,
    clippy::needless_collect,
    clippy::redundant_clone,
    clippy::redundant_closure_for_method_calls,
    clippy::uninlined_format_args,
    clippy::unnecessary_cast,
    clippy::unnecessary_map_or,
    clippy::unwrap_used,
    clippy::useless_format,
    clippy::useless_vec
)]

use borg_core::rate_guard::{ActionLimits, ActionType, RateDecision, SessionRateGuard};

// ── Test: multi-action workflow with independent counters ──

#[test]
fn multi_action_independent_counters() {
    let limits = ActionLimits {
        tool_calls_warn: 3,
        tool_calls_block: 5,
        shell_commands_warn: 2,
        shell_commands_block: 4,
        file_writes_warn: 10,
        file_writes_block: 20,
        memory_writes_warn: 10,
        memory_writes_block: 20,
        web_requests_warn: 10,
        web_requests_block: 20,
    };
    let mut guard = SessionRateGuard::new(limits);

    // Record tool calls up to warn threshold
    for _ in 0..3 {
        guard.record(ActionType::ToolCall);
    }
    // Next tool call should warn
    assert!(matches!(
        guard.record(ActionType::ToolCall),
        RateDecision::Warn(_)
    ));

    // Shell commands should still be at 0
    assert!(matches!(
        guard.record(ActionType::ShellCommand),
        RateDecision::Allow
    ));

    // File writes should still be at 0
    assert!(matches!(
        guard.record(ActionType::FileWrite),
        RateDecision::Allow
    ));
}

// ── Test: block threshold stops actions ──

#[test]
fn block_threshold_stops_actions() {
    let limits = ActionLimits {
        tool_calls_warn: 2,
        tool_calls_block: 4,
        shell_commands_warn: 100,
        shell_commands_block: 200,
        file_writes_warn: 100,
        file_writes_block: 200,
        memory_writes_warn: 100,
        memory_writes_block: 200,
        web_requests_warn: 100,
        web_requests_block: 200,
    };
    let mut guard = SessionRateGuard::new(limits);

    // Hit the block threshold
    for _ in 0..4 {
        guard.record(ActionType::ToolCall);
    }
    let decision = guard.record(ActionType::ToolCall);
    assert!(
        matches!(decision, RateDecision::Block(_)),
        "Should block after reaching threshold, got: {:?}",
        decision
    );
}

// ── Test: hot reload via update_limits ──

#[test]
fn hot_reload_update_limits() {
    let initial = ActionLimits {
        tool_calls_warn: 2,
        tool_calls_block: 3,
        shell_commands_warn: 100,
        shell_commands_block: 200,
        file_writes_warn: 100,
        file_writes_block: 200,
        memory_writes_warn: 100,
        memory_writes_block: 200,
        web_requests_warn: 100,
        web_requests_block: 200,
    };
    let mut guard = SessionRateGuard::new(initial);

    // Record 2 tool calls (at warn threshold)
    guard.record(ActionType::ToolCall);
    guard.record(ActionType::ToolCall);

    // Increase limits — should allow more
    let relaxed = ActionLimits {
        tool_calls_warn: 10,
        tool_calls_block: 20,
        shell_commands_warn: 100,
        shell_commands_block: 200,
        file_writes_warn: 100,
        file_writes_block: 200,
        memory_writes_warn: 100,
        memory_writes_block: 200,
        web_requests_warn: 100,
        web_requests_block: 200,
    };
    guard.update_limits(relaxed);

    // Should be allowed again since counter (2) < new warn (10)
    assert!(matches!(
        guard.record(ActionType::ToolCall),
        RateDecision::Allow
    ));
}

// ── Test: gateway defaults are more restrictive ──

#[test]
fn gateway_defaults_restrictive() {
    let standard = ActionLimits::default();
    let gateway = ActionLimits::gateway_defaults();

    // Gateway should have lower or equal limits
    assert!(
        gateway.tool_calls_block <= standard.tool_calls_block,
        "Gateway tool_calls_block should be <= standard"
    );
    assert!(
        gateway.shell_commands_block <= standard.shell_commands_block,
        "Gateway shell_commands_block should be <= standard"
    );
}

// ── Test: all action types work ──

#[test]
fn all_action_types_recordable() {
    let mut guard = SessionRateGuard::new(ActionLimits::default());

    let types = [
        ActionType::ToolCall,
        ActionType::ShellCommand,
        ActionType::FileWrite,
        ActionType::MemoryWrite,
        ActionType::WebRequest,
    ];

    for action in &types {
        let decision = guard.record(*action);
        assert!(
            matches!(decision, RateDecision::Allow),
            "First action of each type should be allowed"
        );
    }
}

// ── Test: warn message contains action type info ──

#[test]
fn warn_message_descriptive() {
    let limits = ActionLimits {
        tool_calls_warn: 1,
        tool_calls_block: 10,
        shell_commands_warn: 100,
        shell_commands_block: 200,
        file_writes_warn: 100,
        file_writes_block: 200,
        memory_writes_warn: 100,
        memory_writes_block: 200,
        web_requests_warn: 100,
        web_requests_block: 200,
    };
    let mut guard = SessionRateGuard::new(limits);

    guard.record(ActionType::ToolCall);
    let decision = guard.record(ActionType::ToolCall);
    assert!(
        matches!(&decision, RateDecision::Warn(msg) if !msg.is_empty()),
        "Should produce non-empty Warn, got: {:?}",
        decision
    );
}
