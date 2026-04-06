//! Workflow step execution engine.
//!
//! Each step runs as an isolated agent turn with focused context.
//! Prior step outputs are injected as context summaries.

use crate::db::{WorkflowRow, WorkflowStepRow};

/// Maximum characters of prior step output to include in context.
const MAX_PRIOR_OUTPUT_CHARS: usize = 500;

/// Build the system prompt context for a workflow step.
///
/// Includes the workflow goal, completed step summaries, current step details,
/// remaining step titles, and retry context if applicable.
pub fn build_step_context(
    workflow: &WorkflowRow,
    step: &WorkflowStepRow,
    prior_steps: &[WorkflowStepRow],
    total_steps: usize,
) -> String {
    let mut ctx = String::with_capacity(2048);

    // Header
    ctx.push_str(&format!(
        "# Workflow: {}\nYou are executing step {} of {}.\n\n",
        workflow.title,
        step.step_index + 1,
        total_steps,
    ));

    // Goal
    ctx.push_str(&format!("## Goal\n{}\n\n", workflow.goal));

    // Completed steps
    if !prior_steps.is_empty() {
        ctx.push_str("## Completed Steps\n");
        for ps in prior_steps {
            let output_summary = ps
                .output
                .as_deref()
                .map(|o| truncate_output(o, MAX_PRIOR_OUTPUT_CHARS))
                .unwrap_or_default();
            ctx.push_str(&format!(
                "### Step {}: {} ✓\n{}\n\n",
                ps.step_index + 1,
                ps.title,
                output_summary,
            ));
        }
    }

    // Current step
    ctx.push_str(&format!(
        "## Current Step: {}\n{}\n\n",
        step.title, step.instructions,
    ));

    // Remaining steps (after current)
    let remaining_start = step.step_index as usize + 1;
    if remaining_start < total_steps {
        ctx.push_str("## Remaining Steps\n");
        // We don't have the remaining step details here, so just indicate count
        for i in remaining_start..total_steps {
            ctx.push_str(&format!("- Step {}\n", i + 1));
        }
        ctx.push('\n');
    }

    // Retry context
    if step.retry_count > 0 {
        ctx.push_str("## Previous Attempt Failed\n");
        if let Some(err) = &step.error {
            ctx.push_str(&format!("Error: {err}\n"));
        }
        ctx.push_str(&format!(
            "This is retry {}/{}. Try a different approach.\n\n",
            step.retry_count, step.max_retries,
        ));
    }

    ctx.push_str("Provide a clear summary of what you accomplished when done.");

    ctx
}

/// Build the step context with full remaining step titles available.
pub fn build_step_context_with_remaining(
    workflow: &WorkflowRow,
    step: &WorkflowStepRow,
    prior_steps: &[WorkflowStepRow],
    all_steps: &[WorkflowStepRow],
) -> String {
    let total = all_steps.len();
    let mut ctx = String::with_capacity(2048);

    // Header
    ctx.push_str(&format!(
        "# Workflow: {}\nYou are executing step {} of {}.\n\n",
        workflow.title,
        step.step_index + 1,
        total,
    ));

    // Goal
    ctx.push_str(&format!("## Goal\n{}\n\n", workflow.goal));

    // Completed steps
    if !prior_steps.is_empty() {
        ctx.push_str("## Completed Steps\n");
        for ps in prior_steps {
            let output_summary = ps
                .output
                .as_deref()
                .map(|o| truncate_output(o, MAX_PRIOR_OUTPUT_CHARS))
                .unwrap_or_default();
            ctx.push_str(&format!(
                "### Step {}: {} ✓\n{}\n\n",
                ps.step_index + 1,
                ps.title,
                output_summary,
            ));
        }
    }

    // Current step
    ctx.push_str(&format!(
        "## Current Step: {}\n{}\n\n",
        step.title, step.instructions,
    ));

    // Remaining steps with titles
    let remaining: Vec<_> = all_steps
        .iter()
        .filter(|s| s.step_index > step.step_index && s.status != "completed")
        .collect();
    if !remaining.is_empty() {
        ctx.push_str("## Remaining Steps\n");
        for s in remaining {
            ctx.push_str(&format!("- Step {}: {}\n", s.step_index + 1, s.title));
        }
        ctx.push('\n');
    }

    // Retry context
    if step.retry_count > 0 {
        ctx.push_str("## Previous Attempt Failed\n");
        if let Some(err) = &step.error {
            ctx.push_str(&format!("Error: {err}\n"));
        }
        ctx.push_str(&format!(
            "This is retry {}/{}. Try a different approach.\n\n",
            step.retry_count, step.max_retries,
        ));
    }

    ctx.push_str("Provide a clear summary of what you accomplished when done.");

    ctx
}

/// Truncate output to a maximum character count, appending "..." if truncated.
fn truncate_output(output: &str, max_chars: usize) -> String {
    let char_count = output.chars().count();
    if char_count <= max_chars {
        output.to_string()
    } else {
        let truncated: String = output.chars().take(max_chars).collect();
        format!("{truncated}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_workflow() -> WorkflowRow {
        WorkflowRow {
            id: "wf-1".to_string(),
            title: "Deploy feature".to_string(),
            goal: "Build, test, and deploy the auth feature".to_string(),
            status: "running".to_string(),
            current_step: 0,
            created_at: 1000,
            updated_at: 1000,
            completed_at: None,
            error: None,
            session_id: None,
            project_id: None,
            delivery_channel: None,
            delivery_target: None,
        }
    }

    fn make_step(index: i64, title: &str, status: &str) -> WorkflowStepRow {
        WorkflowStepRow {
            id: index + 1,
            workflow_id: "wf-1".to_string(),
            step_index: index,
            title: title.to_string(),
            instructions: format!("Do {title}"),
            status: status.to_string(),
            output: None,
            error: None,
            started_at: None,
            completed_at: None,
            max_retries: 3,
            retry_count: 0,
            timeout_ms: 300000,
        }
    }

    #[test]
    fn test_context_template_basic() {
        let wf = make_workflow();
        let step = make_step(0, "Run tests", "running");
        let ctx = build_step_context(&wf, &step, &[], 3);

        assert!(ctx.contains("# Workflow: Deploy feature"));
        assert!(ctx.contains("step 1 of 3"));
        assert!(ctx.contains("## Goal"));
        assert!(ctx.contains("## Current Step: Run tests"));
        assert!(ctx.contains("Do Run tests"));
        assert!(ctx.contains("## Remaining Steps"));
        assert!(!ctx.contains("## Completed Steps"));
    }

    #[test]
    fn test_context_template_with_prior_outputs() {
        let wf = make_workflow();
        let mut prior = make_step(0, "Run tests", "completed");
        prior.output = Some("All 42 tests passed".to_string());

        let step = make_step(1, "Build release", "running");

        let ctx = build_step_context(&wf, &step, &[prior], 3);

        assert!(ctx.contains("## Completed Steps"));
        assert!(ctx.contains("Step 1: Run tests ✓"));
        assert!(ctx.contains("All 42 tests passed"));
        assert!(ctx.contains("## Current Step: Build release"));
    }

    #[test]
    fn test_context_template_truncates_long_output() {
        let wf = make_workflow();
        let mut prior = make_step(0, "Research", "completed");
        prior.output = Some("x".repeat(1000));

        let step = make_step(1, "Summarize", "running");
        let ctx = build_step_context(&wf, &step, &[prior], 2);

        // Should contain truncated output with "..."
        assert!(ctx.contains(&"x".repeat(500)));
        assert!(ctx.contains("..."));
    }

    #[test]
    fn test_context_template_retry_shows_error() {
        let wf = make_workflow();
        let mut step = make_step(0, "Deploy", "running");
        step.retry_count = 2;
        step.max_retries = 3;
        step.error = Some("Connection timeout".to_string());

        let ctx = build_step_context(&wf, &step, &[], 1);

        assert!(ctx.contains("## Previous Attempt Failed"));
        assert!(ctx.contains("Connection timeout"));
        assert!(ctx.contains("retry 2/3"));
        assert!(ctx.contains("Try a different approach"));
    }

    #[test]
    fn test_context_template_no_remaining_on_last_step() {
        let wf = make_workflow();
        let step = make_step(2, "Final step", "running");

        let ctx = build_step_context(&wf, &step, &[], 3);

        assert!(!ctx.contains("## Remaining Steps"));
    }

    #[test]
    fn test_context_with_remaining_titles() {
        let wf = make_workflow();
        let step = make_step(0, "Step A", "running");
        let all = vec![
            make_step(0, "Step A", "running"),
            make_step(1, "Step B", "pending"),
            make_step(2, "Step C", "pending"),
        ];

        let ctx = build_step_context_with_remaining(&wf, &step, &[], &all);

        assert!(ctx.contains("Step 2: Step B"));
        assert!(ctx.contains("Step 3: Step C"));
    }

    #[test]
    fn test_truncate_output_short() {
        assert_eq!(truncate_output("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_output_exact() {
        assert_eq!(truncate_output("12345", 5), "12345");
    }

    #[test]
    fn test_truncate_output_long() {
        let result = truncate_output("1234567890", 5);
        assert_eq!(result, "12345...");
    }
}
