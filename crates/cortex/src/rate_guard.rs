use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::constants;

/// Categories of actions subject to per-session rate limiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionType {
    /// Any tool invocation.
    ToolCall,
    /// Shell command execution.
    ShellCommand,
    /// File write operation.
    FileWrite,
    /// Memory file write.
    MemoryWrite,
    /// Outbound HTTP request.
    WebRequest,
}

/// Outcome of a rate limit check.
#[derive(Debug, Clone, PartialEq)]
pub enum RateDecision {
    /// Action is within limits.
    Allow,
    /// Action is allowed but approaching the limit.
    Warn(String),
    /// Action is blocked — limit exceeded.
    Block(String),
}

/// Per-session warn and block thresholds for each action type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionLimits {
    /// Warn threshold for tool call count.
    #[serde(default = "default_tool_calls_warn")]
    pub tool_calls_warn: u32,
    /// Block threshold for tool call count.
    #[serde(default = "default_tool_calls_block")]
    pub tool_calls_block: u32,
    /// Warn threshold for shell command count.
    #[serde(default = "default_shell_commands_warn")]
    pub shell_commands_warn: u32,
    /// Block threshold for shell command count.
    #[serde(default = "default_shell_commands_block")]
    pub shell_commands_block: u32,
    /// Warn threshold for file write count.
    #[serde(default = "default_file_writes_warn")]
    pub file_writes_warn: u32,
    /// Block threshold for file write count.
    #[serde(default = "default_file_writes_block")]
    pub file_writes_block: u32,
    /// Warn threshold for memory write count.
    #[serde(default = "default_memory_writes_warn")]
    pub memory_writes_warn: u32,
    /// Block threshold for memory write count.
    #[serde(default = "default_memory_writes_block")]
    pub memory_writes_block: u32,
    /// Warn threshold for web request count.
    #[serde(default = "default_web_requests_warn")]
    pub web_requests_warn: u32,
    /// Block threshold for web request count.
    #[serde(default = "default_web_requests_block")]
    pub web_requests_block: u32,
}

macro_rules! serde_default {
    ($name:ident, $value:expr) => {
        fn $name() -> u32 {
            $value
        }
    };
}

serde_default!(default_tool_calls_warn, constants::RATE_TOOL_CALLS_WARN);
serde_default!(default_tool_calls_block, constants::RATE_TOOL_CALLS_BLOCK);
serde_default!(
    default_shell_commands_warn,
    constants::RATE_SHELL_COMMANDS_WARN
);
serde_default!(
    default_shell_commands_block,
    constants::RATE_SHELL_COMMANDS_BLOCK
);
serde_default!(default_file_writes_warn, constants::RATE_FILE_WRITES_WARN);
serde_default!(default_file_writes_block, constants::RATE_FILE_WRITES_BLOCK);
serde_default!(
    default_memory_writes_warn,
    constants::RATE_MEMORY_WRITES_WARN
);
serde_default!(
    default_memory_writes_block,
    constants::RATE_MEMORY_WRITES_BLOCK
);
serde_default!(default_web_requests_warn, constants::RATE_WEB_REQUESTS_WARN);
serde_default!(
    default_web_requests_block,
    constants::RATE_WEB_REQUESTS_BLOCK
);

impl Default for ActionLimits {
    fn default() -> Self {
        Self {
            tool_calls_warn: default_tool_calls_warn(),
            tool_calls_block: default_tool_calls_block(),
            shell_commands_warn: default_shell_commands_warn(),
            shell_commands_block: default_shell_commands_block(),
            file_writes_warn: default_file_writes_warn(),
            file_writes_block: default_file_writes_block(),
            memory_writes_warn: default_memory_writes_warn(),
            memory_writes_block: default_memory_writes_block(),
            web_requests_warn: default_web_requests_warn(),
            web_requests_block: default_web_requests_block(),
        }
    }
}

impl ActionLimits {
    /// Stricter defaults for gateway sessions (external senders).
    pub fn gateway_defaults() -> Self {
        Self {
            tool_calls_warn: constants::GW_RATE_TOOL_CALLS_WARN,
            tool_calls_block: constants::GW_RATE_TOOL_CALLS_BLOCK,
            shell_commands_warn: constants::GW_RATE_SHELL_COMMANDS_WARN,
            shell_commands_block: constants::GW_RATE_SHELL_COMMANDS_BLOCK,
            file_writes_warn: constants::GW_RATE_FILE_WRITES_WARN,
            file_writes_block: constants::GW_RATE_FILE_WRITES_BLOCK,
            memory_writes_warn: constants::GW_RATE_MEMORY_WRITES_WARN,
            memory_writes_block: constants::GW_RATE_MEMORY_WRITES_BLOCK,
            web_requests_warn: constants::GW_RATE_WEB_REQUESTS_WARN,
            web_requests_block: constants::GW_RATE_WEB_REQUESTS_BLOCK,
        }
    }

    /// Log warnings for misconfigured threshold pairs (warn >= block).
    pub fn validate_thresholds(&self) {
        let pairs: &[(&str, u32, u32)] = &[
            ("tool_calls", self.tool_calls_warn, self.tool_calls_block),
            (
                "shell_commands",
                self.shell_commands_warn,
                self.shell_commands_block,
            ),
            ("file_writes", self.file_writes_warn, self.file_writes_block),
            (
                "memory_writes",
                self.memory_writes_warn,
                self.memory_writes_block,
            ),
            (
                "web_requests",
                self.web_requests_warn,
                self.web_requests_block,
            ),
        ];
        for (name, warn, block) in pairs {
            if warn >= block {
                tracing::warn!(
                    "action_limits.{name}_warn ({warn}) >= {name}_block ({block}); warn threshold will never fire"
                );
            }
        }
    }

    fn limits_for(&self, action: ActionType) -> (u32, u32) {
        match action {
            ActionType::ToolCall => (self.tool_calls_warn, self.tool_calls_block),
            ActionType::ShellCommand => (self.shell_commands_warn, self.shell_commands_block),
            ActionType::FileWrite => (self.file_writes_warn, self.file_writes_block),
            ActionType::MemoryWrite => (self.memory_writes_warn, self.memory_writes_block),
            ActionType::WebRequest => (self.web_requests_warn, self.web_requests_block),
        }
    }

    /// Raise limits to at least the evolution-stage defaults from
    /// `docs/evolution.md#action-limits-by-stage`. Uses `max` so explicit user
    /// configuration (set higher) is always preserved — stage progression
    /// monotonically *unlocks* headroom, never tightens it.
    pub fn apply_stage(&mut self, stage: EvolutionStage) {
        let (tool_calls, shell_commands, file_writes) = match stage {
            EvolutionStage::Base => ((50, 100), (20, 50), (15, 30)),
            EvolutionStage::Evolved => ((75, 150), (30, 75), (25, 50)),
            EvolutionStage::Final => ((100, 200), (50, 100), (40, 80)),
        };
        self.tool_calls_warn = self.tool_calls_warn.max(tool_calls.0);
        self.tool_calls_block = self.tool_calls_block.max(tool_calls.1);
        self.shell_commands_warn = self.shell_commands_warn.max(shell_commands.0);
        self.shell_commands_block = self.shell_commands_block.max(shell_commands.1);
        self.file_writes_warn = self.file_writes_warn.max(file_writes.0);
        self.file_writes_block = self.file_writes_block.max(file_writes.1);
    }

    /// Return `self` with stage overrides applied — chainable form of `apply_stage`.
    pub fn with_stage(mut self, stage: EvolutionStage) -> Self {
        self.apply_stage(stage);
        self
    }
}

/// Evolution stage passed into [`ActionLimits::apply_stage`]. Local mirror to
/// keep `rate_guard` free of a dependency on the `evolution` module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvolutionStage {
    /// Pre-evolution (Lvl.0–99 at Stage 1).
    Base,
    /// Post first evolution.
    Evolved,
    /// Final form.
    Final,
}

/// Per-session rate limiter that tracks action counts against configurable thresholds.
pub struct SessionRateGuard {
    /// Running count of each action type in the current session.
    counters: HashMap<ActionType, u32>,
    /// Configured warn and block thresholds.
    limits: ActionLimits,
}

impl SessionRateGuard {
    /// Create a new rate guard with the given thresholds.
    pub fn new(limits: ActionLimits) -> Self {
        Self {
            counters: HashMap::new(),
            limits,
        }
    }

    /// Update the action limits in place (used by config hot reload).
    pub fn update_limits(&mut self, new_limits: ActionLimits) {
        self.limits = new_limits;
    }

    /// Record an action and return the rate decision.
    pub fn record(&mut self, action: ActionType) -> RateDecision {
        let count = self.counters.entry(action).or_insert(0);
        *count = count.saturating_add(1);
        let current = *count;

        let (warn_threshold, block_threshold) = self.limits.limits_for(action);

        if current >= block_threshold {
            RateDecision::Block(format!(
                "Rate limit exceeded: {action:?} count ({current}) reached block threshold ({block_threshold}). \
                 Session action limit reached — please start a new session to continue."
            ))
        } else if current >= warn_threshold {
            RateDecision::Warn(format!(
                "Rate warning: {action:?} count ({current}) reached warn threshold ({warn_threshold})"
            ))
        } else {
            RateDecision::Allow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allow_under_limit() {
        let mut guard = SessionRateGuard::new(ActionLimits::default());
        let decision = guard.record(ActionType::ToolCall);
        assert_eq!(decision, RateDecision::Allow);
    }

    #[test]
    fn test_warn_at_threshold() {
        let limits = ActionLimits {
            tool_calls_warn: 3,
            tool_calls_block: 5,
            ..Default::default()
        };
        let mut guard = SessionRateGuard::new(limits);
        for _ in 0..2 {
            assert_eq!(guard.record(ActionType::ToolCall), RateDecision::Allow);
        }
        match guard.record(ActionType::ToolCall) {
            RateDecision::Warn(_) => {}
            other => unreachable!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn test_block_at_limit() {
        let limits = ActionLimits {
            tool_calls_warn: 2,
            tool_calls_block: 3,
            ..Default::default()
        };
        let mut guard = SessionRateGuard::new(limits);
        guard.record(ActionType::ToolCall);
        guard.record(ActionType::ToolCall);
        match guard.record(ActionType::ToolCall) {
            RateDecision::Block(_) => {}
            other => unreachable!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn test_independent_counters() {
        let limits = ActionLimits {
            tool_calls_warn: 2,
            shell_commands_warn: 2,
            ..Default::default()
        };
        let mut guard = SessionRateGuard::new(limits);
        guard.record(ActionType::ToolCall);
        assert_eq!(guard.record(ActionType::ShellCommand), RateDecision::Allow);
    }

    #[test]
    fn test_default_limits() {
        let limits = ActionLimits::default();
        assert_eq!(limits.tool_calls_warn, 200);
        assert_eq!(limits.tool_calls_block, 500);
        assert_eq!(limits.shell_commands_warn, 100);
        assert_eq!(limits.shell_commands_block, 250);
        assert_eq!(limits.file_writes_warn, 50);
        assert_eq!(limits.file_writes_block, 150);
        assert_eq!(limits.memory_writes_warn, 20);
        assert_eq!(limits.memory_writes_block, 50);
        assert_eq!(limits.web_requests_warn, 50);
        assert_eq!(limits.web_requests_block, 150);
    }

    #[test]
    fn test_custom_limits() {
        let limits = ActionLimits {
            tool_calls_warn: 10,
            tool_calls_block: 20,
            ..Default::default()
        };
        let mut guard = SessionRateGuard::new(limits);
        for _ in 0..9 {
            assert_eq!(guard.record(ActionType::ToolCall), RateDecision::Allow);
        }
        match guard.record(ActionType::ToolCall) {
            RateDecision::Warn(_) => {}
            other => unreachable!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn test_multiple_action_types() {
        let limits = ActionLimits {
            file_writes_warn: 3,
            file_writes_block: 5,
            memory_writes_warn: 3,
            memory_writes_block: 5,
            ..Default::default()
        };
        let mut guard = SessionRateGuard::new(limits);

        assert_eq!(guard.record(ActionType::FileWrite), RateDecision::Allow);
        assert_eq!(guard.record(ActionType::MemoryWrite), RateDecision::Allow);
        assert_eq!(guard.record(ActionType::FileWrite), RateDecision::Allow);

        // 3rd FileWrite should hit warn threshold
        match guard.record(ActionType::FileWrite) {
            RateDecision::Warn(_) => {}
            other => unreachable!("expected Warn for FileWrite, got {other:?}"),
        }

        // MemoryWrite should still be at 1 (under warn of 3)
        assert_eq!(guard.record(ActionType::MemoryWrite), RateDecision::Allow);
    }

    #[test]
    fn test_update_limits_changes_thresholds() {
        let limits = ActionLimits {
            tool_calls_warn: 100,
            tool_calls_block: 200,
            ..Default::default()
        };
        let mut guard = SessionRateGuard::new(limits);

        // Record 3 tool calls — under old limits
        for _ in 0..3 {
            assert_eq!(guard.record(ActionType::ToolCall), RateDecision::Allow);
        }

        // Now lower the warn threshold to 3 (already at 3, so next should warn)
        let new_limits = ActionLimits {
            tool_calls_warn: 3,
            tool_calls_block: 5,
            ..Default::default()
        };
        guard.update_limits(new_limits);

        match guard.record(ActionType::ToolCall) {
            RateDecision::Warn(_) => {}
            other => unreachable!("expected Warn after limit update, got {other:?}"),
        }
    }

    #[test]
    fn test_web_request_limits() {
        let limits = ActionLimits {
            web_requests_warn: 2,
            web_requests_block: 4,
            ..Default::default()
        };
        let mut guard = SessionRateGuard::new(limits);
        assert_eq!(guard.record(ActionType::WebRequest), RateDecision::Allow);
        match guard.record(ActionType::WebRequest) {
            RateDecision::Warn(_) => {}
            other => unreachable!("expected Warn, got {other:?}"),
        }
        // 3rd is still warn
        match guard.record(ActionType::WebRequest) {
            RateDecision::Warn(_) => {}
            other => unreachable!("expected Warn, got {other:?}"),
        }
        // 4th hits block
        match guard.record(ActionType::WebRequest) {
            RateDecision::Block(_) => {}
            other => unreachable!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn test_block_message_contains_info() {
        let limits = ActionLimits {
            tool_calls_warn: 1,
            tool_calls_block: 2,
            ..Default::default()
        };
        let mut guard = SessionRateGuard::new(limits);
        guard.record(ActionType::ToolCall);
        match guard.record(ActionType::ToolCall) {
            RateDecision::Block(msg) => {
                assert!(msg.contains("ToolCall"));
                assert!(msg.contains("2")); // count
            }
            other => unreachable!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn test_limits_for_all_action_types() {
        let limits = ActionLimits::default();
        assert_eq!(limits.limits_for(ActionType::ToolCall), (200, 500));
        assert_eq!(limits.limits_for(ActionType::ShellCommand), (100, 250));
        assert_eq!(limits.limits_for(ActionType::FileWrite), (50, 150));
        assert_eq!(limits.limits_for(ActionType::MemoryWrite), (20, 50));
        assert_eq!(limits.limits_for(ActionType::WebRequest), (50, 150));
    }

    #[test]
    fn test_gateway_defaults() {
        let limits = ActionLimits::gateway_defaults();
        assert_eq!(limits.tool_calls_warn, 30);
        assert_eq!(limits.tool_calls_block, 50);
        assert_eq!(limits.shell_commands_warn, 10);
        assert_eq!(limits.shell_commands_block, 20);
        assert_eq!(limits.file_writes_warn, 10);
        assert_eq!(limits.file_writes_block, 20);
        assert_eq!(limits.memory_writes_warn, 5);
        assert_eq!(limits.memory_writes_block, 10);
        assert_eq!(limits.web_requests_warn, 10);
        assert_eq!(limits.web_requests_block, 25);
    }

    #[test]
    fn test_action_limits_deserialize() {
        let toml_str = r#"
tool_calls_warn = 5
tool_calls_block = 10
"#;
        let limits: ActionLimits = toml::from_str(toml_str).unwrap();
        assert_eq!(limits.tool_calls_warn, 5);
        assert_eq!(limits.tool_calls_block, 10);
        // Others should be defaults
        assert_eq!(limits.shell_commands_warn, 100);
    }

    #[test]
    fn test_validate_thresholds_valid_does_not_panic() {
        let limits = ActionLimits::default();
        // Should not panic — all warn < block
        limits.validate_thresholds();
    }

    #[test]
    fn test_validate_thresholds_inverted_does_not_panic() {
        let limits = ActionLimits {
            tool_calls_warn: 100,
            tool_calls_block: 50, // warn >= block
            ..Default::default()
        };
        // Should not panic — only logs a warning
        limits.validate_thresholds();
    }

    #[test]
    fn test_validate_thresholds_equal_does_not_panic() {
        let limits = ActionLimits {
            shell_commands_warn: 20,
            shell_commands_block: 20, // warn == block
            ..Default::default()
        };
        limits.validate_thresholds();
    }

    #[test]
    fn test_saturating_add_at_max() {
        let limits = ActionLimits {
            tool_calls_warn: u32::MAX - 1,
            tool_calls_block: u32::MAX,
            ..Default::default()
        };
        let mut guard = SessionRateGuard::new(limits);
        // Fill up to u32::MAX - 2 to get Allow
        guard.counters.insert(ActionType::ToolCall, u32::MAX - 2);
        // Next should warn at MAX-1
        match guard.record(ActionType::ToolCall) {
            RateDecision::Warn(_) => {}
            other => unreachable!("expected Warn, got {other:?}"),
        }
        // Next should block at MAX
        match guard.record(ActionType::ToolCall) {
            RateDecision::Block(_) => {}
            other => unreachable!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn apply_stage_base_is_noop_on_defaults() {
        // Defaults (200/500 tool calls etc) are already above Stage 1 caps (50/100).
        let mut limits = ActionLimits::default();
        let before = limits.clone();
        limits.apply_stage(EvolutionStage::Base);
        assert_eq!(limits.tool_calls_warn, before.tool_calls_warn);
        assert_eq!(limits.tool_calls_block, before.tool_calls_block);
    }

    #[test]
    fn apply_stage_lifts_tight_configured_caps() {
        let mut limits = ActionLimits {
            tool_calls_warn: 10,
            tool_calls_block: 20,
            shell_commands_warn: 5,
            shell_commands_block: 15,
            file_writes_warn: 5,
            file_writes_block: 10,
            ..Default::default()
        };
        limits.apply_stage(EvolutionStage::Evolved);
        assert_eq!(limits.tool_calls_warn, 75);
        assert_eq!(limits.tool_calls_block, 150);
        assert_eq!(limits.shell_commands_warn, 30);
        assert_eq!(limits.shell_commands_block, 75);
        assert_eq!(limits.file_writes_warn, 25);
        assert_eq!(limits.file_writes_block, 50);
    }

    #[test]
    fn apply_stage_preserves_higher_user_config() {
        let mut limits = ActionLimits {
            tool_calls_warn: 1000,
            tool_calls_block: 2000,
            ..Default::default()
        };
        limits.apply_stage(EvolutionStage::Final);
        assert_eq!(limits.tool_calls_warn, 1000);
        assert_eq!(limits.tool_calls_block, 2000);
    }

    #[test]
    fn with_stage_chains() {
        let limits = ActionLimits {
            tool_calls_warn: 10,
            tool_calls_block: 20,
            ..Default::default()
        }
        .with_stage(EvolutionStage::Final);
        assert_eq!(limits.tool_calls_warn, 100);
        assert_eq!(limits.tool_calls_block, 200);
    }

    #[test]
    fn apply_stage_final_unlocks_more_than_evolved() {
        let mut a = ActionLimits {
            tool_calls_warn: 0,
            tool_calls_block: 0,
            shell_commands_warn: 0,
            shell_commands_block: 0,
            file_writes_warn: 0,
            file_writes_block: 0,
            ..Default::default()
        };
        let mut b = a.clone();
        a.apply_stage(EvolutionStage::Evolved);
        b.apply_stage(EvolutionStage::Final);
        assert!(b.tool_calls_block > a.tool_calls_block);
        assert!(b.shell_commands_block > a.shell_commands_block);
        assert!(b.file_writes_block > a.file_writes_block);
    }

    #[test]
    fn stage_does_not_affect_memory_or_web_limits() {
        let mut limits = ActionLimits {
            memory_writes_warn: 7,
            memory_writes_block: 11,
            web_requests_warn: 13,
            web_requests_block: 17,
            ..Default::default()
        };
        limits.apply_stage(EvolutionStage::Final);
        assert_eq!(limits.memory_writes_warn, 7);
        assert_eq!(limits.memory_writes_block, 11);
        assert_eq!(limits.web_requests_warn, 13);
        assert_eq!(limits.web_requests_block, 17);
    }

    #[test]
    fn test_all_action_types_independent() {
        let limits = ActionLimits {
            tool_calls_warn: 1,
            tool_calls_block: 2,
            shell_commands_warn: 1,
            shell_commands_block: 2,
            file_writes_warn: 1,
            file_writes_block: 2,
            memory_writes_warn: 1,
            memory_writes_block: 2,
            web_requests_warn: 1,
            web_requests_block: 2,
        };
        let mut guard = SessionRateGuard::new(limits);

        // Record one of each — all should warn at 1
        for action in [
            ActionType::ToolCall,
            ActionType::ShellCommand,
            ActionType::FileWrite,
            ActionType::MemoryWrite,
            ActionType::WebRequest,
        ] {
            match guard.record(action) {
                RateDecision::Warn(_) => {}
                other => unreachable!("expected Warn for {action:?}, got {other:?}"),
            }
        }
    }
}
