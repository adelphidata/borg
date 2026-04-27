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

/// Lifecycle status of a workflow.
///
/// Stored as a lowercase string in the `workflows.status` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowStatus {
    /// Workflow has been created but not yet started.
    Pending,
    /// Workflow is actively executing steps.
    Running,
    /// All steps completed successfully.
    Completed,
    /// A step failed after exhausting retries, workflow aborted.
    Failed,
    /// Workflow was cancelled by the user or system.
    Cancelled,
}

impl WorkflowStatus {
    /// SQLite/JSON string form of this status.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl std::fmt::Display for WorkflowStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lifecycle status of a single workflow step.
///
/// Stored as a lowercase string in the `workflow_steps.status` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowStepStatus {
    /// Step has not yet started.
    Pending,
    /// Step is currently executing.
    Running,
    /// Step completed successfully.
    Completed,
    /// Step failed (may be retried or terminal).
    Failed,
    /// Step was skipped (workflow cancelled before reaching it).
    Skipped,
}

impl WorkflowStepStatus {
    /// SQLite/JSON string form of this status.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

impl std::fmt::Display for WorkflowStepStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Workflow status values stored in the database.
///
/// These constants mirror the variants of [`WorkflowStatus`] so existing call
/// sites that pass a `&str` into parameterized SQL continue to work. New code
/// should prefer the enum for type safety.
pub mod status {
    use super::WorkflowStatus;

    /// Workflow has been created but not yet started.
    pub const PENDING: &str = WorkflowStatus::Pending.as_str();
    /// Workflow is actively executing steps.
    pub const RUNNING: &str = WorkflowStatus::Running.as_str();
    /// All steps completed successfully.
    pub const COMPLETED: &str = WorkflowStatus::Completed.as_str();
    /// A step failed after exhausting retries, workflow aborted.
    pub const FAILED: &str = WorkflowStatus::Failed.as_str();
    /// Workflow was cancelled by the user or system.
    pub const CANCELLED: &str = WorkflowStatus::Cancelled.as_str();
}

/// Step status values stored in the database.
///
/// These constants mirror the variants of [`WorkflowStepStatus`]. New code
/// should prefer the enum for type safety.
pub mod step_status {
    use super::WorkflowStepStatus;

    /// Step has not yet started.
    pub const PENDING: &str = WorkflowStepStatus::Pending.as_str();
    /// Step is currently executing.
    pub const RUNNING: &str = WorkflowStepStatus::Running.as_str();
    /// Step completed successfully.
    pub const COMPLETED: &str = WorkflowStepStatus::Completed.as_str();
    /// Step failed (may be retried or terminal).
    pub const FAILED: &str = WorkflowStepStatus::Failed.as_str();
    /// Step was skipped (workflow cancelled before reaching it).
    pub const SKIPPED: &str = WorkflowStepStatus::Skipped.as_str();
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
