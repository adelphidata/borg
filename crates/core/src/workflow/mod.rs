//! Workflow engine — durable multi-step task orchestration.
//!
//! Provides ordered step execution with per-step isolation, retry, and crash recovery.
//! Designed to help weaker/open-source models manage long-running tasks that stronger
//! models handle implicitly.

pub mod engine;
pub mod tier;

#[cfg(test)]
mod tests;

use crate::config::Config;
use tier::model_needs_workflows;

/// Workflow status values stored in the database.
pub mod status {
    /// Workflow has been created but not yet started.
    pub const PENDING: &str = "pending";
    /// Workflow is actively executing steps.
    pub const RUNNING: &str = "running";
    /// All steps completed successfully.
    pub const COMPLETED: &str = "completed";
    /// A step failed after exhausting retries, workflow aborted.
    pub const FAILED: &str = "failed";
    /// Workflow was cancelled by the user or system.
    pub const CANCELLED: &str = "cancelled";
}

/// Step status values stored in the database.
pub mod step_status {
    /// Step has not yet started.
    pub const PENDING: &str = "pending";
    /// Step is currently executing.
    pub const RUNNING: &str = "running";
    /// Step completed successfully.
    pub const COMPLETED: &str = "completed";
    /// Step failed (may be retried or terminal).
    pub const FAILED: &str = "failed";
    /// Step was skipped (workflow cancelled before reaching it).
    pub const SKIPPED: &str = "skipped";
}

/// Check whether workflow orchestration should be active for the current config.
///
/// Resolution: `"on"` → true, `"off"` → false, `"auto"` → model heuristic.
/// All Claude models are excluded in auto mode; workflows target non-Claude models.
pub fn workflows_active(config: &Config) -> bool {
    match config.workflow.enabled.as_str() {
        "on" => true,
        "off" => false,
        _ => model_needs_workflows(&config.llm.model),
    }
}
