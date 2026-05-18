use serde::{Deserialize, Serialize};

/// Agent-loop safety guards: stuck-loop exponential backoff and
/// hallucinated-tool refusal.
///
/// Both guards reset at the top of every agent-loop iteration. The
/// `identical_call_max` cap stays the terminal safety net — backoff
/// just buys an operator-interrupt window before it fires. The
/// unknown-tool counter is per-tool-name within a turn so a typo on
/// one name doesn't burn budget for an unrelated hallucination.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoopGuardConfig {
    /// Maximum consecutive identical `(name, args)` tool calls before the
    /// agent skips the next one with a synthetic error tool_result. The
    /// `(identical_call_max - 1)`th call is the last one that runs — after
    /// pre-backoff sleep.
    pub identical_call_max: u32,
    /// Initial backoff sleep applied before the 2nd identical call. Each
    /// subsequent identical call doubles the base delay, clamped at
    /// `backoff_max_ms`, then multiplied by uniform ±25% jitter.
    pub backoff_initial_ms: u64,
    /// Hard ceiling on the un-jittered base delay. Post-jitter delays may
    /// exceed this by up to 25%.
    pub backoff_max_ms: u64,
    /// Maximum times a single hallucinated tool name can be observed in a
    /// turn before subsequent calls return a terminal refusal instead of
    /// the soft "Unknown tool: X" error. The default of 2 allows one
    /// typo-recovery attempt then stops the retry cycle.
    pub unknown_tool_max_per_name: u32,
}

impl Default for LoopGuardConfig {
    fn default() -> Self {
        Self {
            identical_call_max: 3,
            backoff_initial_ms: 500,
            backoff_max_ms: 5000,
            unknown_tool_max_per_name: 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_guard_defaults_match_historical_constants() {
        let cfg = LoopGuardConfig::default();
        assert_eq!(
            cfg.identical_call_max, 3,
            "default identical-call cap must match the pre-config \
             MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS constant so behavior \
             is unchanged for installs that don't override it"
        );
        assert!(
            cfg.backoff_initial_ms > 0,
            "initial backoff must be positive"
        );
        assert!(
            cfg.backoff_max_ms >= cfg.backoff_initial_ms,
            "max backoff must be >= initial backoff so the clamp is meaningful"
        );
        assert!(
            cfg.unknown_tool_max_per_name >= 1,
            "must allow at least one attempt — zero would refuse every \
             tool call before the first dispatch could ever run"
        );
    }

    #[test]
    fn loop_guard_round_trips_through_toml() {
        let toml_str = r#"
            identical_call_max = 5
            backoff_initial_ms = 100
            backoff_max_ms = 800
            unknown_tool_max_per_name = 1
        "#;
        let cfg: LoopGuardConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(cfg.identical_call_max, 5);
        assert_eq!(cfg.backoff_initial_ms, 100);
        assert_eq!(cfg.backoff_max_ms, 800);
        assert_eq!(cfg.unknown_tool_max_per_name, 1);
    }
}
