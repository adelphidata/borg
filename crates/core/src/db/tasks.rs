use super::models::{ClaimedTask, NewTask, ScheduledTaskRow, TaskRunRow, UpdateTask};
use super::Database;
use anyhow::Result;
use rusqlite::params;
use rusqlite::OptionalExtension;

/// Column list shared by all scheduled-task SELECT queries.
const TASK_COLUMNS: &str = "id, name, prompt, schedule_type, schedule_expr, timezone, status, next_run, created_at, max_retries, retry_count, retry_after, last_error, timeout_ms, delivery_channel, delivery_target, allowed_tools, task_type";

impl Database {
    /// Insert a new scheduled task into the database.
    pub fn create_task(&self, task: &NewTask<'_>) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let max_retries = task.max_retries.unwrap_or(3);
        let timeout_ms = task.timeout_ms.unwrap_or(300_000);
        self.conn.execute(
            "INSERT INTO scheduled_tasks (id, name, prompt, schedule_type, schedule_expr, timezone, status, next_run, created_at, max_retries, timeout_ms, delivery_channel, delivery_target, allowed_tools, task_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![task.id, task.name, task.prompt, task.schedule_type, task.schedule_expr, task.timezone, task.next_run, now, max_retries, timeout_ms, task.delivery_channel, task.delivery_target, task.allowed_tools, task.task_type],
        )?;
        Ok(())
    }

    /// List all scheduled tasks ordered by creation time descending.
    pub fn list_tasks(&self) -> Result<Vec<ScheduledTaskRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM scheduled_tasks ORDER BY created_at DESC"
        ))?;
        let rows = stmt
            .query_map([], Self::map_task_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Fetch active tasks whose next_run is at or before `now` and not pending retry.
    pub fn get_due_tasks(&self, now: i64) -> Result<Vec<ScheduledTaskRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM scheduled_tasks \
             WHERE status = 'active' AND next_run IS NOT NULL AND next_run <= ?1 \
             AND retry_after IS NULL \
             ORDER BY next_run ASC"
        ))?;
        let rows = stmt
            .query_map(params![now], Self::map_task_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Update a task's status. Returns true if the task was found.
    pub fn update_task_status(&self, id: &str, status: &str) -> Result<bool> {
        let updated = self.conn.execute(
            "UPDATE scheduled_tasks SET status = ?1 WHERE id = ?2",
            params![status, id],
        )?;
        Ok(updated > 0)
    }

    /// Set a task's next scheduled run time.
    pub fn update_task_next_run(&self, id: &str, next_run: Option<i64>) -> Result<()> {
        self.conn.execute(
            "UPDATE scheduled_tasks SET next_run = ?1 WHERE id = ?2",
            params![next_run, id],
        )?;
        Ok(())
    }

    /// Record a completed task execution in the task_runs table.
    pub fn record_task_run(
        &self,
        task_id: &str,
        started_at: i64,
        duration_ms: i64,
        result: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        let status = if error.is_some() {
            crate::tasks::RUN_STATUS_FAILED
        } else {
            crate::tasks::RUN_STATUS_SUCCESS
        };
        self.conn.execute(
            "INSERT INTO task_runs (task_id, started_at, duration_ms, result, error, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![task_id, started_at, duration_ms, result, error, status],
        )?;
        Ok(())
    }

    /// Fetch recent execution history for a task, newest first.
    pub fn task_run_history(&self, task_id: &str, limit: usize) -> Result<Vec<TaskRunRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, started_at, duration_ms, result, error, status
             FROM task_runs WHERE task_id = ?1 ORDER BY started_at DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![task_id, limit as i64], |row| {
                Ok(TaskRunRow {
                    id: row.get(0)?,
                    task_id: row.get(1)?,
                    started_at: row.get(2)?,
                    duration_ms: row.get(3)?,
                    result: row.get(4)?,
                    error: row.get(5)?,
                    status: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Look up a single task by its ID.
    pub fn get_task_by_id(&self, id: &str) -> Result<Option<ScheduledTaskRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM scheduled_tasks WHERE id = ?1"
        ))?;
        let mut rows = stmt.query_map(params![id], Self::map_task_row)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    fn map_task_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScheduledTaskRow> {
        Ok(ScheduledTaskRow {
            id: row.get(0)?,
            name: row.get(1)?,
            prompt: row.get(2)?,
            schedule_type: row.get(3)?,
            schedule_expr: row.get(4)?,
            timezone: row.get(5)?,
            status: row.get(6)?,
            next_run: row.get(7)?,
            created_at: row.get(8)?,
            max_retries: row.get(9)?,
            retry_count: row.get(10)?,
            retry_after: row.get(11)?,
            last_error: row.get(12)?,
            timeout_ms: row.get(13)?,
            delivery_channel: row.get(14)?,
            delivery_target: row.get(15)?,
            allowed_tools: row.get(16)?,
            task_type: row
                .get::<_, Option<String>>(17)?
                .unwrap_or_else(|| "prompt".to_string()),
        })
    }

    /// Mark a task for retry with backoff timing and error details.
    pub fn set_task_retry(
        &self,
        task_id: &str,
        retry_count: i32,
        last_error: &str,
        retry_after: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE scheduled_tasks SET retry_count = ?1, last_error = ?2, retry_after = ?3 WHERE id = ?4",
            params![retry_count, last_error, retry_after, task_id],
        )?;
        Ok(())
    }

    /// Reset retry state after a successful run.
    pub fn clear_task_retry(&self, task_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE scheduled_tasks SET retry_count = 0, retry_after = NULL, last_error = NULL WHERE id = ?1",
            params![task_id],
        )?;
        Ok(())
    }

    /// Fetch tasks whose retry_after time has elapsed.
    pub fn get_tasks_pending_retry(&self, now: i64) -> Result<Vec<ScheduledTaskRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM scheduled_tasks \
             WHERE status = 'active' AND retry_after IS NOT NULL AND retry_after <= ?1 \
             ORDER BY retry_after ASC"
        ))?;
        let rows = stmt
            .query_map(params![now], Self::map_task_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Atomically claim all due tasks: advances next_run and inserts a 'running' task_run
    /// row in a single IMMEDIATE transaction. Returns claimed tasks with their run IDs.
    pub fn claim_due_tasks(&self, now: i64) -> Result<Vec<ClaimedTask>> {
        // BEGIN IMMEDIATE acquires a reserved lock, preventing concurrent writers.
        // Rollback guard ensures we don't leave an open transaction on error.
        self.conn.execute_batch("BEGIN IMMEDIATE")?;

        let result = self.claim_due_tasks_inner(now);
        if result.is_err() {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
        result
    }

    fn claim_due_tasks_inner(&self, now: i64) -> Result<Vec<ClaimedTask>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM scheduled_tasks \
             WHERE status = 'active' AND next_run IS NOT NULL AND next_run <= ?1 \
             AND retry_after IS NULL \
             ORDER BY next_run ASC"
        ))?;
        let tasks: Vec<ScheduledTaskRow> = stmt
            .query_map(params![now], Self::map_task_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut claimed = Vec::with_capacity(tasks.len());
        for task in tasks {
            crate::tasks::advance_next_run_raw(&self.conn, &task)?;

            // Insert a 'running' task_run row
            self.conn.execute(
                "INSERT INTO task_runs (task_id, started_at, duration_ms, status)
                 VALUES (?1, ?2, 0, ?3)",
                params![task.id, now, crate::tasks::RUN_STATUS_RUNNING],
            )?;
            let run_id = self.conn.last_insert_rowid();
            claimed.push(ClaimedTask { task, run_id });
        }

        self.conn.execute_batch("COMMIT")?;
        Ok(claimed)
    }

    /// Insert a 'running' task_run row (used for retry path). Returns the run ID.
    pub fn start_task_run(&self, task_id: &str, started_at: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO task_runs (task_id, started_at, duration_ms, status)
             VALUES (?1, ?2, 0, ?3)",
            params![task_id, started_at, crate::tasks::RUN_STATUS_RUNNING],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Update a task_run row from 'running' to its final status.
    /// Returns Ok(true) if the row was updated, Ok(false) if no matching run was found.
    pub fn complete_task_run(
        &self,
        run_id: i64,
        duration_ms: i64,
        result: Option<&str>,
        error: Option<&str>,
    ) -> Result<bool> {
        let status = if error.is_some() {
            crate::tasks::RUN_STATUS_FAILED
        } else {
            crate::tasks::RUN_STATUS_SUCCESS
        };
        let updated = self.conn.execute(
            "UPDATE task_runs SET status = ?1, duration_ms = ?2, result = ?3, error = ?4
             WHERE id = ?5",
            params![status, duration_ms, result, error, run_id],
        )?;
        Ok(updated > 0)
    }

    /// Mark any 'running' task_runs as 'failed' (from a crashed daemon). Returns count.
    pub fn recover_stale_runs(&self, error_msg: &str) -> Result<u64> {
        let updated = self.conn.execute(
            "UPDATE task_runs SET status = ?1, error = ?2
             WHERE status = ?3",
            params![
                crate::tasks::RUN_STATUS_FAILED,
                error_msg,
                crate::tasks::RUN_STATUS_RUNNING
            ],
        )?;
        Ok(updated as u64)
    }

    // ── Daemon Lock ──

    /// Attempt to acquire the daemon lock. Returns Ok(true) if acquired.
    /// A lock is considered stale after 300s without heartbeat refresh.
    /// Uses IMMEDIATE transaction to prevent TOCTOU races.
    pub fn acquire_daemon_lock(&self, pid: u32, now: i64) -> Result<bool> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;

        let result = self.acquire_daemon_lock_inner(pid, now);
        match &result {
            Ok(_) => {
                self.conn.execute_batch("COMMIT")?;
            }
            Err(_) => {
                let _ = self.conn.execute_batch("ROLLBACK");
            }
        }
        result
    }

    fn acquire_daemon_lock_inner(&self, pid: u32, now: i64) -> Result<bool> {
        let existing: Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT pid, heartbeat_at FROM daemon_lock WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        match existing {
            None => {
                self.conn.execute(
                    "INSERT INTO daemon_lock (id, pid, started_at, heartbeat_at) VALUES (1, ?1, ?2, ?2)",
                    params![pid as i64, now],
                )?;
                Ok(true)
            }
            Some((existing_pid, heartbeat_at)) => {
                if existing_pid == pid as i64 {
                    // Same PID (daemon restart) — take over
                    self.conn.execute(
                        "UPDATE daemon_lock SET started_at = ?1, heartbeat_at = ?1 WHERE id = 1",
                        params![now],
                    )?;
                    Ok(true)
                } else if now - heartbeat_at > 300 {
                    // Stale lock — take over
                    self.conn.execute(
                        "UPDATE daemon_lock SET pid = ?1, started_at = ?2, heartbeat_at = ?2 WHERE id = 1",
                        params![pid as i64, now],
                    )?;
                    Ok(true)
                } else {
                    // Another live daemon holds the lock
                    Ok(false)
                }
            }
        }
    }

    /// Refresh the daemon lock heartbeat timestamp.
    /// Returns an error if the lock was stolen (0 rows updated).
    pub fn refresh_daemon_lock(&self, pid: u32, now: i64) -> Result<()> {
        let rows = self.conn.execute(
            "UPDATE daemon_lock SET heartbeat_at = ?1 WHERE id = 1 AND pid = ?2",
            params![now, pid as i64],
        )?;
        if rows == 0 {
            anyhow::bail!("daemon lock lost: no row matched pid {pid} (lock stolen or released)");
        }
        Ok(())
    }

    /// Returns true if a daemon holds a non-stale lock (heartbeat within 300s).
    pub fn is_daemon_lock_held(&self) -> bool {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .query_row(
                "SELECT heartbeat_at FROM daemon_lock WHERE id = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .ok()
            .map(|heartbeat_at| now - heartbeat_at <= 300)
            .unwrap_or(false)
    }

    /// Release the daemon lock on shutdown.
    pub fn release_daemon_lock(&self, pid: u32) -> Result<()> {
        self.conn.execute(
            "DELETE FROM daemon_lock WHERE id = 1 AND pid = ?1",
            params![pid as i64],
        )?;
        Ok(())
    }

    /// Delete a task and all its run history. Returns true if found.
    pub fn delete_task(&self, id: &str) -> Result<bool> {
        self.conn
            .execute("DELETE FROM task_runs WHERE task_id = ?1", params![id])?;
        let deleted = self
            .conn
            .execute("DELETE FROM scheduled_tasks WHERE id = ?1", params![id])?;
        Ok(deleted > 0)
    }

    /// Update a task's name, prompt, schedule, or timezone. Returns true if found.
    pub fn update_task(&self, id: &str, update: &UpdateTask<'_>) -> Result<bool> {
        let existing = match self.get_task_by_id(id)? {
            Some(row) => row,
            None => return Ok(false),
        };

        let name = update.name.unwrap_or(&existing.name);
        let prompt = update.prompt.unwrap_or(&existing.prompt);
        let schedule_type = update.schedule_type.unwrap_or(&existing.schedule_type);
        let schedule_expr = update.schedule_expr.unwrap_or(&existing.schedule_expr);
        let timezone = update.timezone.unwrap_or(&existing.timezone);

        let next_run = if update.schedule_type.is_some() || update.schedule_expr.is_some() {
            crate::tasks::calculate_next_run(schedule_type, schedule_expr)?
        } else {
            existing.next_run
        };

        self.conn.execute(
            "UPDATE scheduled_tasks SET name = ?1, prompt = ?2, schedule_type = ?3, schedule_expr = ?4, timezone = ?5, next_run = ?6 WHERE id = ?7",
            params![name, prompt, schedule_type, schedule_expr, timezone, next_run, id],
        )?;
        Ok(true)
    }

    /// Get the most recent run for a task, if any.
    pub fn last_task_run(&self, task_id: &str) -> Result<Option<TaskRunRow>> {
        let runs = self.task_run_history(task_id, 1)?;
        Ok(runs.into_iter().next())
    }
}
