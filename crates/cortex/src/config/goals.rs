//! Per-session-goal configuration.

use serde::{Deserialize, Serialize};

/// Tunables for the `/goal` subsystem.
///
/// Only fields backed by a live consumer in the runtime are surfaced as
/// settings. Continuation / budget-limit steering knobs are intentionally
/// not exposed yet — the steering-prompt injection sites land in a later
/// change, and shipping a dead knob would silently lie to operators.
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
    /// Ceiling on any goal's `token_budget`. `None` = no ceiling.
    ///
    /// A budget is what stops a Ralph loop from running until the credit runs
    /// out, so a model that asks for an enormous one has effectively disabled
    /// it. `create_goal` rejects a request above this, and a goal created with
    /// no budget at all inherits this value rather than becoming unbounded.
    pub max_token_budget: Option<i64>,
    /// Per-gate wall-clock budget, in milliseconds. A gate that overruns counts
    /// as failed — a test suite that hangs is not evidence the goal is done.
    #[serde(default = "default_gate_timeout_ms")]
    pub gate_timeout_ms: u64,
    /// Consecutive failures a single gate may accumulate before the goal is
    /// paused. Without a cap, a gate the agent cannot satisfy would loop until
    /// the token budget ran out; pausing surfaces it to the operator instead.
    #[serde(default = "default_gate_max_attempts")]
    pub gate_max_attempts: i64,
}

/// Default for [`GoalsConfig::gate_timeout_ms`] — five minutes, enough for a
/// real test suite or build without letting a wedged command hold the loop.
fn default_gate_timeout_ms() -> u64 {
    300_000
}

/// Default for [`GoalsConfig::gate_max_attempts`].
///
/// Counts failures the agent made *no workspace progress* against (see
/// [`crate::db::Database::record_goal_gate_run`]), not failing turns — so this
/// is "five consecutive turns that changed nothing", which is a genuinely stuck
/// loop rather than a normal edit-compile-test cycle.
fn default_gate_max_attempts() -> i64 {
    5
}

impl Default for GoalsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_token_budget: None,
            max_token_budget: None,
            gate_timeout_ms: default_gate_timeout_ms(),
            gate_max_attempts: default_gate_max_attempts(),
        }
    }
}
