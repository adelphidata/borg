//! Comprehensive tests for the workflow engine.

use crate::db::Database;
use crate::db::NewWorkflowStep;
use crate::workflow::{status, step_status};

fn test_db() -> Database {
    Database::test_db()
}

fn sample_steps(n: usize) -> Vec<NewWorkflowStep> {
    (0..n)
        .map(|i| NewWorkflowStep {
            title: format!("Step {}", i + 1),
            instructions: format!("Execute step {} instructions", i + 1),
            max_retries: 3,
            timeout_ms: 300000,
        })
        .collect()
}

mod cancel;
mod claim;
mod completion;
mod crud;
mod lifecycle;
mod project;
mod recovery;
mod retry;
mod runnable;
mod tier;
