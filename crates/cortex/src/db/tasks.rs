use super::models::{
    ClaimedTask, FailedRunGroup, NewTask, ScheduledTaskRow, TaskRunRow, UpdateTask,
};
use super::Database;
use anyhow::Result;
use rusqlite::params;
use rusqlite::OptionalExtension;

/// Column list shared by all scheduled-task SELECT queries.
const TASK_COLUMNS: &str = "id, name, prompt, schedule_type, schedule_expr, timezone, status, next_run, created_at, max_retries, retry_count, retry_after, last_error, timeout_ms, delivery_channel, delivery_target, allowed_tools, task_type, pre_run_command, pre_run_cwd, pre_run_timeout_ms, bypass_quiet_hours_once, min_interval_secs, last_response_hash";

impl Database {
    /// Insert a new scheduled task into the database.
    pub fn create_task(&self, task: &NewTask<'_>) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let max_retries = task.max_retries.unwrap_or(3);
        let timeout_ms = task
            .timeout_ms
            .unwrap_or(crate::constants::SCHEDULED_TASK_DEFAULT_TIMEOUT_MS as i64);
        let pre_run_timeout_ms = task.pre_run_timeout_ms.unwrap_or(60_000);
        self.conn.execute(
            "INSERT INTO scheduled_tasks (id, name, prompt, schedule_type, schedule_expr, timezone, status, next_run, created_at, max_retries, timeout_ms, delivery_channel, delivery_target, allowed_tools, task_type, pre_run_command, pre_run_cwd, pre_run_timeout_ms, min_interval_secs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![task.id, task.name, task.prompt, task.schedule_type, task.schedule_expr, task.timezone, task.next_run, now, max_retries, timeout_ms, task.delivery_channel, task.delivery_target, task.allowed_tools, task.task_type, task.pre_run_command, task.pre_run_cwd, pre_run_timeout_ms, task.min_interval_secs],
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
            pre_run_command: row.get(18)?,
            pre_run_cwd: row.get(19)?,
            pre_run_timeout_ms: row.get::<_, Option<i64>>(20)?.unwrap_or(60_000),
            bypass_quiet_hours_once: row.get::<_, Option<i64>>(21)?.unwrap_or(0) != 0,
            min_interval_secs: row.get(22)?,
            last_response_hash: row.get(23)?,
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

            // Atomically consume any one-shot bypass flag so a concurrent
            // second poke (or any code reading the row mid-tick) can't lose
            // its override. The flag is already captured on `task`.
            if task.bypass_quiet_hours_once {
                self.conn.execute(
                    "UPDATE scheduled_tasks SET bypass_quiet_hours_once = 0 WHERE id = ?1",
                    params![task.id],
                )?;
            }

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

    /// Find scheduled tasks whose `next_run` has drifted more than
    /// `grace_secs` into the past without firing — a silent stall.
    ///
    /// Excludes tasks pending retry (those are handled by the retry loop)
    /// and one-shot tasks (they are allowed to sit until run). Returns the
    /// full task row so callers can reschedule using the existing helpers.
    pub fn find_stalled_tasks(&self, now: i64, grace_secs: i64) -> Result<Vec<ScheduledTaskRow>> {
        let cutoff = now - grace_secs;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM scheduled_tasks \
             WHERE status = 'active' \
               AND retry_after IS NULL \
               AND schedule_type != 'once' \
               AND next_run IS NOT NULL \
               AND next_run < ?1 \
             ORDER BY next_run ASC"
        ))?;
        let rows = stmt
            .query_map(params![cutoff], Self::map_task_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Scheduled tasks that currently carry a non-null `last_error`. These
    /// are the tasks the heartbeat / system-prompt should surface as
    /// "something failed last time it ran" — distinct from `find_stalled_tasks`
    /// which targets schedule drift, not execution failure.
    pub fn tasks_with_last_error(&self) -> Result<Vec<ScheduledTaskRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM scheduled_tasks \
             WHERE last_error IS NOT NULL AND last_error != '' \
             ORDER BY name ASC"
        ))?;
        let rows = stmt
            .query_map([], Self::map_task_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Count `task_runs` rows with `status = 'failed'` newer than `since_ts`.
    /// Used to ground heartbeat / system-prompt claims about recent failures
    /// in real DB state instead of LLM freelancing.
    pub fn count_failed_runs_since(&self, since_ts: i64) -> Result<i64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM task_runs WHERE status = ?1 AND started_at >= ?2",
            params![crate::tasks::RUN_STATUS_FAILED, since_ts],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Failed `task_runs` in `[since_ts, ∞)` grouped by their parent task,
    /// joined with `scheduled_tasks` so callers see human-readable names.
    /// Used by the heartbeat task_health augmenter to ground its claim that
    /// "N runs failed" in the actual task IDs the agent can investigate.
    /// Ordering: most failures first, ties broken by most recent failure.
    pub fn failed_runs_grouped_since(
        &self,
        since_ts: i64,
        limit: usize,
    ) -> Result<Vec<FailedRunGroup>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.name, COUNT(r.id) AS failures, \
                    MAX(r.started_at) AS latest_at, \
                    (SELECT r2.error FROM task_runs r2 \
                     WHERE r2.task_id = s.id \
                       AND r2.status = ?1 \
                       AND r2.started_at >= ?2 \
                     ORDER BY r2.started_at DESC LIMIT 1) AS latest_error \
             FROM task_runs r \
             JOIN scheduled_tasks s ON s.id = r.task_id \
             WHERE r.status = ?1 AND r.started_at >= ?2 \
             GROUP BY s.id, s.name \
             ORDER BY failures DESC, latest_at DESC \
             LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(
                params![crate::tasks::RUN_STATUS_FAILED, since_ts, limit as i64],
                |row| {
                    Ok(FailedRunGroup {
                        task_id: row.get(0)?,
                        task_name: row.get(1)?,
                        failures: row.get(2)?,
                        latest_at: row.get(3)?,
                        latest_error: row.get(4)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Count `task_runs` rows with `status = 'completed'` (success) newer than `since_ts`.
    pub fn count_successful_runs_since(&self, since_ts: i64) -> Result<i64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM task_runs WHERE status = ?1 AND started_at >= ?2",
            params![crate::tasks::RUN_STATUS_SUCCESS, since_ts],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Insert a `task_runs` row with `status = 'missed'` so the stall is
    /// visible in run history. `started_at` is the detection time, not the
    /// would-have-fired time (the latter is available on `scheduled_tasks.next_run`
    /// before it gets reset).
    pub fn record_missed_run(&self, task_id: &str, started_at: i64, note: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO task_runs (task_id, started_at, duration_ms, error, status)
             VALUES (?1, ?2, 0, ?3, ?4)",
            params![task_id, started_at, note, crate::tasks::RUN_STATUS_MISSED],
        )?;
        Ok(())
    }

    /// Count `task_runs` rows with `status = 'missed'` newer than `since_ts`.
    pub fn count_missed_runs_since(&self, since_ts: i64) -> Result<i64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM task_runs WHERE status = ?1 AND started_at >= ?2",
            params![crate::tasks::RUN_STATUS_MISSED, since_ts],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Mark 'running' task_runs as 'failed' when their `started_at` is
    /// older than the task's own `timeout_ms` (or `default_grace_secs`
    /// when the task has no explicit timeout). Used by the daemon loop
    /// to recover wedged executions without waiting for a restart.
    /// Returns the number of rows updated.
    pub fn recover_wedged_runs(&self, now: i64, default_grace_secs: i64) -> Result<u64> {
        let error_msg = format!(
            "self-healing: task_run wedged in 'running' past timeout (grace {default_grace_secs}s)"
        );
        let updated = self.conn.execute(
            "UPDATE task_runs \
             SET status = ?1, error = ?2 \
             WHERE id IN ( \
                 SELECT tr.id FROM task_runs tr \
                 LEFT JOIN scheduled_tasks st ON st.id = tr.task_id \
                 WHERE tr.status = ?3 \
                   AND tr.started_at < ?4 - COALESCE(st.timeout_ms / 1000, ?5) \
             )",
            params![
                crate::tasks::RUN_STATUS_FAILED,
                error_msg,
                crate::tasks::RUN_STATUS_RUNNING,
                now,
                default_grace_secs,
            ],
        )?;
        Ok(updated as u64)
    }

    /// Mark any 'running' task_runs as 'failed' (from a crashed daemon). Returns count.
    ///
    /// Also propagates `error_msg` to the parent `scheduled_tasks.last_error`
    /// for affected tasks — but only when `last_error` is currently NULL, so
    /// a more specific transient error from `set_task_retry`
    /// (e.g. "openrouter 429") is not clobbered by the generic crash message.
    /// Without this, daemon-crash victims show as failed in `task_runs` but
    /// invisible to anything reading `scheduled_tasks.last_error` (e.g. the
    /// `borg tasks` listing, `tasks_with_last_error()`).
    pub fn recover_stale_runs(&self, error_msg: &str) -> Result<u64> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE scheduled_tasks SET last_error = ?1
             WHERE last_error IS NULL
               AND id IN (SELECT DISTINCT task_id FROM task_runs WHERE status = ?2)",
            params![error_msg, crate::tasks::RUN_STATUS_RUNNING],
        )?;
        let updated = tx.execute(
            "UPDATE task_runs SET status = ?1, error = ?2
             WHERE status = ?3",
            params![
                crate::tasks::RUN_STATUS_FAILED,
                error_msg,
                crate::tasks::RUN_STATUS_RUNNING
            ],
        )?;
        tx.commit()?;
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
            crate::tasks::calculate_next_run(schedule_type, schedule_expr, timezone)?
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

    /// Set the one-shot `bypass_quiet_hours_once` flag on a task. Used by
    /// `Admin.Poke` / `TriggerHeartbeat(bypass=true)` to make the heartbeat
    /// row's next dispatch ignore quiet hours and the per-task min-interval
    /// floor. The flag is cleared atomically inside `claim_due_tasks` so a
    /// concurrent second poke cannot silently lose its override.
    pub fn set_task_bypass_quiet_hours_once(&self, id: &str, bypass: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE scheduled_tasks SET bypass_quiet_hours_once = ?1 WHERE id = ?2",
            params![bypass as i64, id],
        )?;
        Ok(())
    }

    /// Persist the hash of the most recently delivered response for a task.
    /// Used by the heartbeat dispatch path as belt-and-suspenders dedup
    /// behind the primary `[SILENT]` suppression. Pass `None` to clear.
    pub fn update_task_response_hash(&self, id: &str, hash: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE scheduled_tasks SET last_response_hash = ?1 WHERE id = ?2",
            params![hash, id],
        )?;
        Ok(())
    }
}
