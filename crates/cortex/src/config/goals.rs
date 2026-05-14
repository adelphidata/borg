//! Per-session-goal configuration.

use serde::{Deserialize, Serialize};

/// Tunables for the `/goal` subsystem.
///
/// Only fields backed by a live consumer in the runtime are surfaced as
/// settings. Continuation / budget-limit steering knobs are intentionally
/// not exposed yet — the steering-prompt injection sites land in a later
/// change, and shipping a dead knob silently lies to operators (CLAUDE.md
/// "Don't add features ... beyond what the task requires").
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GoalsConfig {
    /// Master switch. When `false`:
    /// * the three `*_goal` tools are excluded from the schema sent to the
    ///   LLM (saves ~1.5 KB / turn) and refuse at the dispatcher;
    /// * the `<goal_context>` system-prompt section emits nothing.
    pub enabled: bool,
    /// Default `token_budget` applied when `create_goal` is called without
    /// one. `None` = unbounded. The tool handler reads this on every
    /// `create_goal` invocation.
    pub default_token_budget: Option<i64>,
}

impl Default for GoalsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_token_budget: None,
        }
    }
}
