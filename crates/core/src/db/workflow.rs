//! Database operations for workflow persistence.

use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension};

use super::models::{NewWorkflowStep, ProjectRow, WorkflowRow, WorkflowStepRow};
use super::Database;

use crate::workflow::{status, step_status};

/// Maximum number of steps allowed per workflow.
const MAX_WORKFLOW_STEPS: usize = 50;

impl Database {
    /// Create a workflow with its steps in a single atomic transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn create_workflow(
        &self,
        id: &str,
        title: &str,
        goal: &str,
        steps: &[NewWorkflowStep],
        session_id: Option<&str>,
        project_id: Option<&str>,
        delivery_channel: Option<&str>,
        delivery_target: Option<&str>,
    ) -> Result<()> {
        if steps.is_empty() {
            anyhow::bail!("Workflow must have at least one step");
        }
        if steps.len() > MAX_WORKFLOW_STEPS {
            anyhow::bail!(
                "Workflow exceeds maximum of {MAX_WORKFLOW_STEPS} steps (got {})",
                steps.len()
            );
        }

        let now = chrono::Utc::now().timestamp();

        let tx = self
            .conn
            .unchecked_transaction()
            .context("Failed to begin workflow transaction")?;

        tx.execute(
            "INSERT INTO workflows (id, title, goal, status, current_step, created_at, updated_at, session_id, project_id, delivery_channel, delivery_target)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                title,
                goal,
                status::RUNNING,
                now,
                session_id,
                project_id,
                delivery_channel,
                delivery_target,
            ],
        )?;

        for (i, step) in steps.iter().enumerate() {
            tx.execute(
                "INSERT INTO workflow_steps (workflow_id, step_index, title, instructions, status, max_retries, timeout_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id,
                    i as i64,
                    step.title,
                    step.instructions,
                    step_status::PENDING,
                    step.max_retries,
                    step.timeout_ms,
                ],
            )?;
        }

        tx.commit().context("Failed to commit workflow creation")?;
        Ok(())
    }

    /// Fetch a single workflow by ID.
    pub fn get_workflow(&self, id: &str) -> Result<Option<WorkflowRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, goal, status, current_step, created_at, updated_at,
                    completed_at, error, session_id, project_id, delivery_channel, delivery_target
             FROM workflows WHERE id = ?1",
        )?;

        let row = stmt
            .query_row(params![id], |row| {
                Ok(WorkflowRow {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    goal: row.get(2)?,
                    status: row.get(3)?,
                    current_step: row.get(4)?,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
                    completed_at: row.get(7)?,
                    error: row.get(8)?,
                    session_id: row.get(9)?,
                    project_id: row.get(10)?,
                    delivery_channel: row.get(11)?,
                    delivery_target: row.get(12)?,
                })
            })
            .optional()?;

        Ok(row)
    }

    /// Fetch all steps for a workflow, ordered by step_index.
    pub fn get_workflow_steps(&self, workflow_id: &str) -> Result<Vec<WorkflowStepRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, workflow_id, step_index, title, instructions, status,
                    output, error, started_at, completed_at, max_retries, retry_count, timeout_ms
             FROM workflow_steps WHERE workflow_id = ?1 ORDER BY step_index",
        )?;

        let rows = stmt
            .query_map(params![workflow_id], |row| {
                Ok(WorkflowStepRow {
                    id: row.get(0)?,
                    workflow_id: row.get(1)?,
                    step_index: row.get(2)?,
                    title: row.get(3)?,
                    instructions: row.get(4)?,
                    status: row.get(5)?,
                    output: row.get(6)?,
                    error: row.get(7)?,
                    started_at: row.get(8)?,
                    completed_at: row.get(9)?,
                    max_retries: row.get(10)?,
                    retry_count: row.get(11)?,
                    timeout_ms: row.get(12)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    /// List workflows, optionally filtered by status.
    pub fn list_workflows(&self, status_filter: Option<&str>) -> Result<Vec<WorkflowRow>> {
        let (sql, filter_params): (String, Vec<Box<dyn rusqlite::types::ToSql>>) =
            match status_filter {
                Some(s) => (
                    "SELECT id, title, goal, status, current_step, created_at, updated_at,
                        completed_at, error, session_id, project_id, delivery_channel, delivery_target
                 FROM workflows WHERE status = ?1 ORDER BY created_at DESC"
                        .to_string(),
                    vec![Box::new(s.to_string())],
                ),
                None => (
                    "SELECT id, title, goal, status, current_step, created_at, updated_at,
                        completed_at, error, session_id, project_id, delivery_channel, delivery_target
                 FROM workflows ORDER BY created_at DESC"
                        .to_string(),
                    vec![],
                ),
            };

        let mut stmt = self.conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            filter_params.iter().map(AsRef::as_ref).collect();

        let rows = stmt
            .query_map(params_refs.as_slice(), |row| {
                Ok(WorkflowRow {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    goal: row.get(2)?,
                    status: row.get(3)?,
                    current_step: row.get(4)?,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
                    completed_at: row.get(7)?,
                    error: row.get(8)?,
                    session_id: row.get(9)?,
                    project_id: row.get(10)?,
                    delivery_channel: row.get(11)?,
                    delivery_target: row.get(12)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    /// Atomically claim the next pending step in a workflow for execution.
    /// Uses BEGIN IMMEDIATE to prevent concurrent writers from double-claiming.
    pub fn claim_next_workflow_step(&self, workflow_id: &str) -> Result<Option<WorkflowStepRow>> {
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .context("Failed to begin step claim transaction")?;

        let result = self.claim_next_workflow_step_inner(workflow_id);
        if result.is_err() {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
        result
    }

    fn claim_next_workflow_step_inner(&self, workflow_id: &str) -> Result<Option<WorkflowStepRow>> {
        let now = chrono::Utc::now().timestamp();

        // Find the first pending step by index order
        let step_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM workflow_steps
                 WHERE workflow_id = ?1 AND status = ?2
                 ORDER BY step_index LIMIT 1",
                params![workflow_id, step_status::PENDING],
                |row| row.get(0),
            )
            .optional()?;

        let step_id = match step_id {
            Some(id) => id,
            None => {
                self.conn.execute_batch("COMMIT")?;
                return Ok(None);
            }
        };

        // Mark it as running
        self.conn.execute(
            "UPDATE workflow_steps SET status = ?1, started_at = ?2 WHERE id = ?3",
            params![step_status::RUNNING, now, step_id],
        )?;

        // Update workflow timestamp
        self.conn.execute(
            "UPDATE workflows SET updated_at = ?1 WHERE id = ?2",
            params![now, workflow_id],
        )?;

        self.conn.execute_batch("COMMIT")?;

        // Re-read the claimed step (outside transaction)
        let step = self.read_workflow_step(step_id)?;
        Ok(Some(step))
    }

    /// Mark a step as completed and advance the workflow's current_step.
    /// If this was the last step, marks the workflow as completed.
    /// No-op if the step is no longer in 'running' status (e.g., cancelled).
    pub fn complete_workflow_step(&self, step_id: i64, output: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();

        let tx = self
            .conn
            .unchecked_transaction()
            .context("Failed to begin step completion transaction")?;

        // Only update if step is still running (guard against cancel race)
        let rows_affected = tx.execute(
            "UPDATE workflow_steps SET status = ?1, output = ?2, completed_at = ?3
             WHERE id = ?4 AND status = ?5",
            params![
                step_status::COMPLETED,
                output,
                now,
                step_id,
                step_status::RUNNING
            ],
        )?;

        if rows_affected == 0 {
            // Step was cancelled or already completed — no-op
            return Ok(());
        }

        // Get the step's workflow_id and step_index
        let (workflow_id, step_index): (String, i64) = tx.query_row(
            "SELECT workflow_id, step_index FROM workflow_steps WHERE id = ?1",
            params![step_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        // Check if there are more steps
        let total_steps: i64 = tx.query_row(
            "SELECT COUNT(*) FROM workflow_steps WHERE workflow_id = ?1",
            params![workflow_id],
            |row| row.get(0),
        )?;

        let new_current = step_index + 1;
        if new_current >= total_steps {
            // All steps done — mark workflow completed
            tx.execute(
                "UPDATE workflows SET status = ?1, current_step = ?2, updated_at = ?3, completed_at = ?3 WHERE id = ?4",
                params![status::COMPLETED, new_current, now, workflow_id],
            )?;
        } else {
            // Advance to next step
            tx.execute(
                "UPDATE workflows SET current_step = ?1, updated_at = ?2 WHERE id = ?3",
                params![new_current, now, workflow_id],
            )?;
        }

        tx.commit().context("Failed to commit step completion")?;

        Ok(())
    }

    /// Mark a step as failed and increment its retry count.
    /// If retries are exhausted, marks the workflow as failed.
    /// No-op if the step is no longer in 'running' status (e.g., cancelled).
    pub fn fail_workflow_step(&self, step_id: i64, error: &str) -> Result<bool> {
        let now = chrono::Utc::now().timestamp();

        let tx = self
            .conn
            .unchecked_transaction()
            .context("Failed to begin step failure transaction")?;

        // Get current step info — check it's still running
        let step_info: Option<(String, i32, i32, String)> = tx
            .query_row(
                "SELECT workflow_id, retry_count, max_retries, status FROM workflow_steps WHERE id = ?1",
                params![step_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;

        let (workflow_id, retry_count, max_retries, current_status) = match step_info {
            Some(info) => info,
            None => return Ok(false),
        };

        // Guard against cancel race — only fail running steps
        if current_status != step_status::RUNNING {
            return Ok(false);
        }

        let new_retry_count = retry_count + 1;
        let retries_exhausted = new_retry_count >= max_retries;

        if retries_exhausted {
            // Terminal failure — mark step and workflow as failed
            tx.execute(
                "UPDATE workflow_steps SET status = ?1, error = ?2, retry_count = ?3, completed_at = ?4 WHERE id = ?5",
                params![step_status::FAILED, error, new_retry_count, now, step_id],
            )?;
            tx.execute(
                "UPDATE workflows SET status = ?1, error = ?2, updated_at = ?3 WHERE id = ?4",
                params![status::FAILED, error, now, workflow_id],
            )?;
            // Skip remaining pending steps
            tx.execute(
                "UPDATE workflow_steps SET status = ?1 WHERE workflow_id = ?2 AND status = ?3",
                params![step_status::SKIPPED, workflow_id, step_status::PENDING],
            )?;
        } else {
            // Retryable — reset to pending for next daemon tick
            tx.execute(
                "UPDATE workflow_steps SET status = ?1, error = ?2, retry_count = ?3, started_at = NULL WHERE id = ?4",
                params![step_status::PENDING, error, new_retry_count, step_id],
            )?;
            tx.execute(
                "UPDATE workflows SET updated_at = ?1 WHERE id = ?2",
                params![now, workflow_id],
            )?;
        }

        tx.commit().context("Failed to commit step failure")?;

        Ok(retries_exhausted)
    }

    /// Get workflows that are running and have at least one pending step ready for execution.
    pub fn get_runnable_workflows(&self) -> Result<Vec<WorkflowRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT w.id, w.title, w.goal, w.status, w.current_step,
                    w.created_at, w.updated_at, w.completed_at, w.error,
                    w.session_id, w.project_id, w.delivery_channel, w.delivery_target
             FROM workflows w
             INNER JOIN workflow_steps ws ON ws.workflow_id = w.id
             WHERE w.status = ?1
               AND ws.status = ?2
               AND NOT EXISTS (
                   SELECT 1 FROM workflow_steps ws2
                   WHERE ws2.workflow_id = w.id AND ws2.status = ?3
               )
             ORDER BY w.created_at",
        )?;

        let rows = stmt
            .query_map(
                params![status::RUNNING, step_status::PENDING, step_status::RUNNING],
                |row| {
                    Ok(WorkflowRow {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        goal: row.get(2)?,
                        status: row.get(3)?,
                        current_step: row.get(4)?,
                        created_at: row.get(5)?,
                        updated_at: row.get(6)?,
                        completed_at: row.get(7)?,
                        error: row.get(8)?,
                        session_id: row.get(9)?,
                        project_id: row.get(10)?,
                        delivery_channel: row.get(11)?,
                        delivery_target: row.get(12)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    /// Cancel a workflow. Sets status to cancelled and skips all pending steps.
    /// Returns false if the workflow was already completed or cancelled.
    pub fn cancel_workflow(&self, id: &str) -> Result<bool> {
        let now = chrono::Utc::now().timestamp();

        let tx = self
            .conn
            .unchecked_transaction()
            .context("Failed to begin cancel transaction")?;

        // Check current status
        let current_status: Option<String> = tx
            .query_row(
                "SELECT status FROM workflows WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;

        match current_status.as_deref() {
            Some(status::COMPLETED) | Some(status::CANCELLED) | None => {
                return Ok(false);
            }
            _ => {}
        }

        tx.execute(
            "UPDATE workflows SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status::CANCELLED, now, id],
        )?;

        tx.execute(
            "UPDATE workflow_steps SET status = ?1 WHERE workflow_id = ?2 AND status IN (?3, ?4)",
            params![
                step_status::SKIPPED,
                id,
                step_status::PENDING,
                step_status::RUNNING
            ],
        )?;

        tx.commit().context("Failed to commit cancellation")?;

        Ok(true)
    }

    /// Recover stale workflow steps left in 'running' state after a crash.
    /// Only recovers steps that have been running longer than 5 minutes (grace period
    /// to avoid resetting legitimately executing steps during restart).
    /// Returns the number of recovered steps.
    pub fn recover_stale_workflow_steps(&self) -> Result<usize> {
        let stale_threshold = chrono::Utc::now().timestamp() - 300; // 5 min grace
        let count = self.conn.execute(
            "UPDATE workflow_steps SET status = ?1, started_at = NULL
             WHERE status = ?2 AND (started_at IS NULL OR started_at < ?3)",
            params![step_status::PENDING, step_status::RUNNING, stale_threshold],
        )?;
        Ok(count)
    }

    /// Get all completed steps for a workflow (for context injection into subsequent steps).
    pub fn get_completed_workflow_steps(&self, workflow_id: &str) -> Result<Vec<WorkflowStepRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, workflow_id, step_index, title, instructions, status,
                    output, error, started_at, completed_at, max_retries, retry_count, timeout_ms
             FROM workflow_steps
             WHERE workflow_id = ?1 AND status = ?2
             ORDER BY step_index",
        )?;

        let rows = stmt
            .query_map(params![workflow_id, step_status::COMPLETED], |row| {
                Ok(WorkflowStepRow {
                    id: row.get(0)?,
                    workflow_id: row.get(1)?,
                    step_index: row.get(2)?,
                    title: row.get(3)?,
                    instructions: row.get(4)?,
                    status: row.get(5)?,
                    output: row.get(6)?,
                    error: row.get(7)?,
                    started_at: row.get(8)?,
                    completed_at: row.get(9)?,
                    max_retries: row.get(10)?,
                    retry_count: row.get(11)?,
                    timeout_ms: row.get(12)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    /// Delete a workflow and all its steps (CASCADE).
    pub fn delete_workflow(&self, id: &str) -> Result<bool> {
        let count = self
            .conn
            .execute("DELETE FROM workflows WHERE id = ?1", params![id])?;
        Ok(count > 0)
    }

    /// Helper: read a single workflow step by ID.
    fn read_workflow_step(&self, step_id: i64) -> Result<WorkflowStepRow> {
        let mut stmt = self.conn.prepare(
            "SELECT id, workflow_id, step_index, title, instructions, status,
                    output, error, started_at, completed_at, max_retries, retry_count, timeout_ms
             FROM workflow_steps WHERE id = ?1",
        )?;

        let step = stmt.query_row(params![step_id], |row| {
            Ok(WorkflowStepRow {
                id: row.get(0)?,
                workflow_id: row.get(1)?,
                step_index: row.get(2)?,
                title: row.get(3)?,
                instructions: row.get(4)?,
                status: row.get(5)?,
                output: row.get(6)?,
                error: row.get(7)?,
                started_at: row.get(8)?,
                completed_at: row.get(9)?,
                max_retries: row.get(10)?,
                retry_count: row.get(11)?,
                timeout_ms: row.get(12)?,
            })
        })?;

        Ok(step)
    }

    // ── Workflow query methods ──

    /// List workflows belonging to a specific session.
    pub fn list_workflows_by_session(&self, session_id: &str) -> Result<Vec<WorkflowRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, goal, status, current_step, created_at, updated_at,
                    completed_at, error, session_id, project_id, delivery_channel, delivery_target
             FROM workflows WHERE session_id = ?1 ORDER BY created_at DESC",
        )?;

        let rows = stmt
            .query_map(params![session_id], Self::map_workflow_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    /// List workflows belonging to a specific project.
    pub fn list_workflows_by_project(&self, project_id: &str) -> Result<Vec<WorkflowRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, goal, status, current_step, created_at, updated_at,
                    completed_at, error, session_id, project_id, delivery_channel, delivery_target
             FROM workflows WHERE project_id = ?1 ORDER BY created_at DESC",
        )?;

        let rows = stmt
            .query_map(params![project_id], Self::map_workflow_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    /// Row mapper for WorkflowRow (shared across queries).
    fn map_workflow_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkflowRow> {
        Ok(WorkflowRow {
            id: row.get(0)?,
            title: row.get(1)?,
            goal: row.get(2)?,
            status: row.get(3)?,
            current_step: row.get(4)?,
            created_at: row.get(5)?,
            updated_at: row.get(6)?,
            completed_at: row.get(7)?,
            error: row.get(8)?,
            session_id: row.get(9)?,
            project_id: row.get(10)?,
            delivery_channel: row.get(11)?,
            delivery_target: row.get(12)?,
        })
    }

    // ── Project operations ──

    /// Create a new project.
    pub fn create_project(&self, id: &str, name: &str, description: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO projects (id, name, description, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'active', ?4, ?4)",
            params![id, name, description, now],
        )?;
        Ok(())
    }

    /// Fetch a project by ID.
    pub fn get_project(&self, id: &str) -> Result<Option<ProjectRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, description, status, created_at, updated_at
             FROM projects WHERE id = ?1",
        )?;

        let row = stmt
            .query_row(params![id], |row| {
                Ok(ProjectRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    description: row.get(2)?,
                    status: row.get(3)?,
                    created_at: row.get(4)?,
                    updated_at: row.get(5)?,
                })
            })
            .optional()?;

        Ok(row)
    }

    /// List projects, optionally filtered by status.
    pub fn list_projects(&self, status_filter: Option<&str>) -> Result<Vec<ProjectRow>> {
        let (sql, filter_params): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = match status_filter
        {
            Some(s) => (
                "SELECT id, name, description, status, created_at, updated_at
                     FROM projects WHERE status = ?1 ORDER BY created_at DESC",
                vec![Box::new(s.to_string())],
            ),
            None => (
                "SELECT id, name, description, status, created_at, updated_at
                     FROM projects ORDER BY created_at DESC",
                vec![],
            ),
        };

        let mut stmt = self.conn.prepare(sql)?;
        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            filter_params.iter().map(AsRef::as_ref).collect();

        let rows = stmt
            .query_map(params_refs.as_slice(), |row| {
                Ok(ProjectRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    description: row.get(2)?,
                    status: row.get(3)?,
                    created_at: row.get(4)?,
                    updated_at: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    /// Archive a project (set status to 'archived').
    pub fn archive_project(&self, id: &str) -> Result<bool> {
        let now = chrono::Utc::now().timestamp();
        let count = self.conn.execute(
            "UPDATE projects SET status = 'archived', updated_at = ?1 WHERE id = ?2 AND status = 'active'",
            params![now, id],
        )?;
        Ok(count > 0)
    }

    /// Delete a project. Does not cascade to workflows (they keep a dangling project_id).
    pub fn delete_project(&self, id: &str) -> Result<bool> {
        let count = self
            .conn
            .execute("DELETE FROM projects WHERE id = ?1", params![id])?;
        Ok(count > 0)
    }
}
