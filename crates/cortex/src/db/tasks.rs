use super::models::{
    ClaimedTask, FailedRunGroup, NewTask, ScheduledTaskRow, TaskRunRow, UpdateTask,
};
use super::Database;
use anyhow::Result;
use libsql::params;

/// Column list shared by all scheduled-task SELECT queries.
const TASK_COLUMNS: &str = "id, name, prompt, schedule_type, schedule_expr, timezone, status, next_run, created_at, max_retries, retry_count, retry_after, last_error, timeout_ms, delivery_channel, delivery_target, allowed_tools, task_type, pre_run_command, pre_run_cwd, pre_run_timeout_ms, bypass_quiet_hours_once, min_interval_secs, last_response_hash";

/// Returns true if `pid` is alive on this host. `kill(pid, 0)` only probes —
/// 0 means alive; ESRCH means dead; any other errno (e.g. EPERM) is treated
/// as alive (conservative: never starve a live daemon).
///
/// Assumes a single PID namespace (borgd is a single-host agent, DB lives at
/// `~/.borg/borg.db`). Inside a sandboxed PID-namespace container the probe
/// would see only namespace-local processes, which is still correct because
/// the recorded PID was also written from that namespace.
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: signal 0 never delivers a signal; the call only validates that
    // the kernel can find a process with this PID.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    errno != libc::ESRCH
}

fn map_task_row(row: &libsql::Row) -> Result<ScheduledTaskRow> {
    Ok(ScheduledTaskRow {
        id: row.get::<String>(0)?,
        name: row.get::<String>(1)?,
        prompt: row.get::<String>(2)?,
        schedule_type: row.get::<String>(3)?,
        schedule_expr: row.get::<String>(4)?,
        timezone: row.get::<String>(5)?,
        status: row.get::<String>(6)?,
        next_run: row.get::<Option<i64>>(7)?,
        created_at: row.get::<i64>(8)?,
        max_retries: row.get::<i64>(9)? as i32,
        retry_count: row.get::<i64>(10)? as i32,
        retry_after: row.get::<Option<i64>>(11)?,
        last_error: row.get::<Option<String>>(12)?,
        timeout_ms: row.get::<i64>(13)?,
        delivery_channel: row.get::<Option<String>>(14)?,
        delivery_target: row.get::<Option<String>>(15)?,
        allowed_tools: row.get::<Option<String>>(16)?,
        task_type: row
            .get::<Option<String>>(17)?
            .unwrap_or_else(|| "prompt".to_string()),
        pre_run_command: row.get::<Option<String>>(18)?,
        pre_run_cwd: row.get::<Option<String>>(19)?,
        pre_run_timeout_ms: row
            .get::<Option<i64>>(20)?
            .unwrap_or(crate::constants::PRE_RUN_DEFAULT_TIMEOUT_MS as i64),
        bypass_quiet_hours_once: row.get::<Option<i64>>(21)?.unwrap_or(0) != 0,
        min_interval_secs: row.get::<Option<i64>>(22)?,
        last_response_hash: row.get::<Option<String>>(23)?,
    })
}

impl Database {
    /// Insert a new scheduled task into the database.
    pub async fn create_task(&self, task: &NewTask<'_>) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let max_retries = task.max_retries.unwrap_or(3);
        let timeout_ms = task
            .timeout_ms
            .unwrap_or(crate::constants::SCHEDULED_TASK_DEFAULT_TIMEOUT_MS as i64);
        let pre_run_timeout_ms = task
            .pre_run_timeout_ms
            .unwrap_or(crate::constants::PRE_RUN_DEFAULT_TIMEOUT_MS as i64);
        self.conn.execute(
            "INSERT INTO scheduled_tasks (id, name, prompt, schedule_type, schedule_expr, timezone, status, next_run, created_at, max_retries, timeout_ms, delivery_channel, delivery_target, allowed_tools, task_type, pre_run_command, pre_run_cwd, pre_run_timeout_ms, min_interval_secs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params!(task.id, task.name, task.prompt, task.schedule_type, task.schedule_expr, task.timezone, task.next_run, now, max_retries, timeout_ms, task.delivery_channel, task.delivery_target, task.allowed_tools, task.task_type, task.pre_run_command, task.pre_run_cwd, pre_run_timeout_ms, task.min_interval_secs),
        ).await?;
        Ok(())
    }

    /// List all scheduled tasks ordered by creation time descending.
    pub async fn list_tasks(&self) -> Result<Vec<ScheduledTaskRow>> {
        let sql = format!("SELECT {TASK_COLUMNS} FROM scheduled_tasks ORDER BY created_at DESC");
        let mut rows = self.conn.query(&sql, ()).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(map_task_row(&row)?);
        }
        Ok(out)
    }

    /// Fetch active tasks whose next_run is at or before `now` and not pending retry.
    pub async fn get_due_tasks(&self, now: i64) -> Result<Vec<ScheduledTaskRow>> {
        let sql = format!(
            "SELECT {TASK_COLUMNS} FROM scheduled_tasks \
             WHERE status = 'active' AND next_run IS NOT NULL AND next_run <= ?1 \
             AND retry_after IS NULL \
             ORDER BY next_run ASC"
        );
        let mut rows = self.conn.query(&sql, params!(now)).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(map_task_row(&row)?);
        }
        Ok(out)
    }

    /// Update a task's status. Returns true if the task was found.
    pub async fn update_task_status(&self, id: &str, status: &str) -> Result<bool> {
        let updated = self
            .conn
            .execute(
                "UPDATE scheduled_tasks SET status = ?1 WHERE id = ?2",
                params!(status, id),
            )
            .await?;
        Ok(updated > 0)
    }

    /// Set a task's next scheduled run time.
    pub async fn update_task_next_run(&self, id: &str, next_run: Option<i64>) -> Result<()> {
        self.conn
            .execute(
                "UPDATE scheduled_tasks SET next_run = ?1 WHERE id = ?2",
                params!(next_run, id),
            )
            .await?;
        Ok(())
    }

    /// Record a completed task execution in the task_runs table.
    pub async fn record_task_run(
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
        self.conn
            .execute(
                "INSERT INTO task_runs (task_id, started_at, duration_ms, result, error, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params!(task_id, started_at, duration_ms, result, error, status),
            )
            .await?;
        Ok(())
    }

    /// Fetch recent execution history for a task, newest first.
    pub async fn task_run_history(&self, task_id: &str, limit: usize) -> Result<Vec<TaskRunRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, task_id, started_at, duration_ms, result, error, status
             FROM task_runs WHERE task_id = ?1 ORDER BY started_at DESC LIMIT ?2",
                params!(task_id, limit as i64),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(TaskRunRow {
                id: row.get::<i64>(0)?,
                task_id: row.get::<String>(1)?,
                started_at: row.get::<i64>(2)?,
                duration_ms: row.get::<i64>(3)?,
                result: row.get::<Option<String>>(4)?,
                error: row.get::<Option<String>>(5)?,
                status: row.get::<String>(6)?,
            });
        }
        Ok(out)
    }

    /// Look up a single task by its ID.
    pub async fn get_task_by_id(&self, id: &str) -> Result<Option<ScheduledTaskRow>> {
        let sql = format!("SELECT {TASK_COLUMNS} FROM scheduled_tasks WHERE id = ?1");
        let mut rows = self.conn.query(&sql, params!(id)).await?;
        match rows.next().await? {
            Some(row) => Ok(Some(map_task_row(&row)?)),
            None => Ok(None),
        }
    }

    /// Mark a task for retry with backoff timing and error details.
    pub async fn set_task_retry(
        &self,
        task_id: &str,
        retry_count: i32,
        last_error: &str,
        retry_after: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE scheduled_tasks SET retry_count = ?1, last_error = ?2, retry_after = ?3 WHERE id = ?4",
            params!(retry_count, last_error, retry_after, task_id),
        ).await?;
        Ok(())
    }

    /// Reset retry state after a successful run.
    pub async fn clear_task_retry(&self, task_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE scheduled_tasks SET retry_count = 0, retry_after = NULL, last_error = NULL WHERE id = ?1",
            params!(task_id),
        ).await?;
        Ok(())
    }

    /// Fetch tasks whose retry_after time has elapsed.
    pub async fn get_tasks_pending_retry(&self, now: i64) -> Result<Vec<ScheduledTaskRow>> {
        let sql = format!(
            "SELECT {TASK_COLUMNS} FROM scheduled_tasks \
             WHERE status = 'active' AND retry_after IS NOT NULL AND retry_after <= ?1 \
             ORDER BY retry_after ASC"
        );
        let mut rows = self.conn.query(&sql, params!(now)).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(map_task_row(&row)?);
        }
        Ok(out)
    }

    /// Atomically claim all due tasks: advances next_run and inserts a 'running' task_run
    /// row in a single IMMEDIATE transaction. Returns claimed tasks with their run IDs.
    pub async fn claim_due_tasks(&self, now: i64) -> Result<Vec<ClaimedTask>> {
        self.conn.execute_batch("BEGIN IMMEDIATE").await?;
        let result = self.claim_due_tasks_inner(now).await;
        if result.is_err() {
            if let Err(re) = self.conn.execute_batch("ROLLBACK").await {
                tracing::warn!("tasks: rollback after claim_due_tasks failure failed: {re}");
            }
        }
        result
    }

    async fn claim_due_tasks_inner(&self, now: i64) -> Result<Vec<ClaimedTask>> {
        let sql = format!(
            "SELECT {TASK_COLUMNS} FROM scheduled_tasks \
             WHERE status = 'active' AND next_run IS NOT NULL AND next_run <= ?1 \
             AND retry_after IS NULL \
             ORDER BY next_run ASC"
        );
        let mut rows = self.conn.query(&sql, params!(now)).await?;
        let mut tasks = Vec::new();
        while let Some(row) = rows.next().await? {
            tasks.push(map_task_row(&row)?);
        }
        drop(rows);

        let mut claimed = Vec::with_capacity(tasks.len());
        for task in tasks {
            self.advance_next_run_in_tx(&task).await?;

            // Atomically consume any one-shot bypass flag so a concurrent
            // second poke (or any code reading the row mid-tick) can't lose
            // its override. The flag is already captured on `task`.
            if task.bypass_quiet_hours_once {
                self.conn
                    .execute(
                        "UPDATE scheduled_tasks SET bypass_quiet_hours_once = 0 WHERE id = ?1",
                        params!(task.id.clone()),
                    )
                    .await?;
            }

            self.conn
                .execute(
                    "INSERT INTO task_runs (task_id, started_at, duration_ms, status)
                 VALUES (?1, ?2, 0, ?3)",
                    params!(task.id.clone(), now, crate::tasks::RUN_STATUS_RUNNING),
                )
                .await?;
            let run_id = self.conn.last_insert_rowid();
            claimed.push(ClaimedTask { task, run_id });
        }

        self.conn.execute_batch("COMMIT").await?;
        Ok(claimed)
    }

    /// Advance a task's next_run inside an already-open transaction. Inlined
    /// from `crate::tasks::advance_next_run_raw` so the claim path stays
    /// self-contained on the async connection.
    async fn advance_next_run_in_tx(&self, task: &ScheduledTaskRow) -> Result<()> {
        match task.schedule_type.as_str() {
            crate::tasks::SCHEDULE_TYPE_ONCE => {
                self.conn
                    .execute(
                        "UPDATE scheduled_tasks SET status = ?1, next_run = NULL WHERE id = ?2",
                        params!(crate::tasks::TASK_STATUS_COMPLETED, task.id.clone()),
                    )
                    .await?;
            }
            crate::tasks::SCHEDULE_TYPE_CRON | crate::tasks::SCHEDULE_TYPE_INTERVAL => {
                let next = crate::tasks::calculate_next_run(
                    &task.schedule_type,
                    &task.schedule_expr,
                    &task.timezone,
                )
                .unwrap_or(None);
                self.conn
                    .execute(
                        "UPDATE scheduled_tasks SET next_run = ?1 WHERE id = ?2",
                        params!(next, task.id.clone()),
                    )
                    .await?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Insert a 'running' task_run row (used for retry path). Returns the run ID.
    pub async fn start_task_run(&self, task_id: &str, started_at: i64) -> Result<i64> {
        self.conn
            .execute(
                "INSERT INTO task_runs (task_id, started_at, duration_ms, status)
             VALUES (?1, ?2, 0, ?3)",
                params!(task_id, started_at, crate::tasks::RUN_STATUS_RUNNING),
            )
            .await?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Update a task_run row from 'running' to its final status.
    /// Returns Ok(true) if the row was updated, Ok(false) if no matching run was found.
    pub async fn complete_task_run(
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
        let updated = self
            .conn
            .execute(
                "UPDATE task_runs SET status = ?1, duration_ms = ?2, result = ?3, error = ?4
             WHERE id = ?5",
                params!(status, duration_ms, result, error, run_id),
            )
            .await?;
        Ok(updated > 0)
    }

    /// Find scheduled tasks whose `next_run` has drifted more than
    /// `grace_secs` into the past without firing — a silent stall.
    ///
    /// Excludes tasks pending retry (those are handled by the retry loop)
    /// and one-shot tasks (they are allowed to sit until run). Returns the
    /// full task row so callers can reschedule using the existing helpers.
    pub async fn find_stalled_tasks(
        &self,
        now: i64,
        grace_secs: i64,
    ) -> Result<Vec<ScheduledTaskRow>> {
        let cutoff = now - grace_secs;
        let sql = format!(
            "SELECT {TASK_COLUMNS} FROM scheduled_tasks \
             WHERE status = 'active' \
               AND retry_after IS NULL \
               AND schedule_type != 'once' \
               AND next_run IS NOT NULL \
               AND next_run < ?1 \
             ORDER BY next_run ASC"
        );
        let mut rows = self.conn.query(&sql, params!(cutoff)).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(map_task_row(&row)?);
        }
        Ok(out)
    }

    /// Scheduled tasks that currently carry a non-null `last_error`. These
    /// are the tasks the heartbeat / system-prompt should surface as
    /// "something failed last time it ran" — distinct from `find_stalled_tasks`
    /// which targets schedule drift, not execution failure.
    pub async fn tasks_with_last_error(&self) -> Result<Vec<ScheduledTaskRow>> {
        let sql = format!(
            "SELECT {TASK_COLUMNS} FROM scheduled_tasks \
             WHERE last_error IS NOT NULL AND last_error != '' \
             ORDER BY name ASC"
        );
        let mut rows = self.conn.query(&sql, ()).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(map_task_row(&row)?);
        }
        Ok(out)
    }

    /// Count `task_runs` rows with `status = 'failed'` newer than `since_ts`.
    /// Used to ground heartbeat / system-prompt claims about recent failures
    /// in real DB state instead of LLM freelancing.
    pub async fn count_failed_runs_since(&self, since_ts: i64) -> Result<i64> {
        let mut rows = self
            .conn
            .query(
                "SELECT COUNT(*) FROM task_runs WHERE status = ?1 AND started_at >= ?2",
                params!(crate::tasks::RUN_STATUS_FAILED, since_ts),
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(row.get::<i64>(0)?),
            None => Ok(0),
        }
    }

    /// Failed `task_runs` in `[since_ts, ∞)` grouped by their parent task,
    /// joined with `scheduled_tasks` so callers see human-readable names.
    /// Used by the heartbeat task_health augmenter to ground its claim that
    /// "N runs failed" in the actual task IDs the agent can investigate.
    /// Ordering: most failures first, ties broken by most recent failure.
    pub async fn failed_runs_grouped_since(
        &self,
        since_ts: i64,
        limit: usize,
    ) -> Result<Vec<FailedRunGroup>> {
        let mut rows = self
            .conn
            .query(
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
                params!(crate::tasks::RUN_STATUS_FAILED, since_ts, limit as i64),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(FailedRunGroup {
                task_id: row.get::<String>(0)?,
                task_name: row.get::<String>(1)?,
                failures: row.get::<i64>(2)?,
                latest_at: row.get::<i64>(3)?,
                latest_error: row.get::<Option<String>>(4)?,
            });
        }
        Ok(out)
    }

    /// Count `task_runs` rows with `status = 'completed'` (success) newer than `since_ts`.
    pub async fn count_successful_runs_since(&self, since_ts: i64) -> Result<i64> {
        let mut rows = self
            .conn
            .query(
                "SELECT COUNT(*) FROM task_runs WHERE status = ?1 AND started_at >= ?2",
                params!(crate::tasks::RUN_STATUS_SUCCESS, since_ts),
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(row.get::<i64>(0)?),
            None => Ok(0),
        }
    }

    /// Insert a `task_runs` row with `status = 'missed'` so the stall is
    /// visible in run history. `started_at` is the detection time, not the
    /// would-have-fired time (the latter is available on `scheduled_tasks.next_run`
    /// before it gets reset).
    pub async fn record_missed_run(
        &self,
        task_id: &str,
        started_at: i64,
        note: &str,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO task_runs (task_id, started_at, duration_ms, error, status)
             VALUES (?1, ?2, 0, ?3, ?4)",
                params!(task_id, started_at, note, crate::tasks::RUN_STATUS_MISSED),
            )
            .await?;
        Ok(())
    }

    /// Count `task_runs` rows with `status = 'missed'` newer than `since_ts`.
    pub async fn count_missed_runs_since(&self, since_ts: i64) -> Result<i64> {
        let mut rows = self
            .conn
            .query(
                "SELECT COUNT(*) FROM task_runs WHERE status = ?1 AND started_at >= ?2",
                params!(crate::tasks::RUN_STATUS_MISSED, since_ts),
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(row.get::<i64>(0)?),
            None => Ok(0),
        }
    }

    /// Mark 'running' task_runs as 'failed' when their `started_at` is
    /// older than the task's own `timeout_ms` (or `default_grace_secs`
    /// when the task has no explicit timeout). Used by the daemon loop
    /// to recover wedged executions without waiting for a restart.
    /// Returns the number of rows updated.
    ///
    /// `live_run_ids` is the liveness escape hatch: candidates in that set
    /// are still executing in this process (see
    /// [`crate::tasks::LiveRunGuard`]), so they are deferred with a warning
    /// instead of being falsified into `failed` — a slow LLM call is not a
    /// wedge, and its future will finalize the row itself. Genuinely dead
    /// runs (in the DB, absent from the live set) behave exactly as before.
    pub async fn recover_wedged_runs(
        &self,
        now: i64,
        default_grace_secs: i64,
        live_run_ids: &std::collections::HashSet<i64>,
    ) -> Result<u64> {
        let mut rows = self
            .conn
            .query(
                "SELECT tr.id FROM task_runs tr \
                 LEFT JOIN scheduled_tasks st ON st.id = tr.task_id \
                 WHERE tr.status = ?1 \
                   AND tr.started_at < ?2 - COALESCE(st.timeout_ms / 1000, ?3)",
                params!(crate::tasks::RUN_STATUS_RUNNING, now, default_grace_secs),
            )
            .await?;
        let mut wedged: Vec<i64> = Vec::new();
        while let Some(row) = rows.next().await? {
            wedged.push(row.get::<i64>(0)?);
        }
        if wedged.is_empty() {
            return Ok(0);
        }

        let error_msg = format!(
            "self-healing: task_run wedged in 'running' past timeout (grace {default_grace_secs}s)"
        );
        let mut updated = 0u64;
        for run_id in wedged {
            if live_run_ids.contains(&run_id) {
                tracing::warn!(
                    run_id,
                    "wedged-run sweep: run is past its timeout but still \
                     executing in-process — deferring recovery"
                );
                continue;
            }
            // Re-check status in the UPDATE: the run may have finished
            // between the candidate SELECT and now, and a completed row
            // must not be clobbered back to failed.
            updated += self
                .conn
                .execute(
                    "UPDATE task_runs SET status = ?1, error = ?2 \
                     WHERE id = ?3 AND status = ?4",
                    params!(
                        crate::tasks::RUN_STATUS_FAILED,
                        error_msg.as_str(),
                        run_id,
                        crate::tasks::RUN_STATUS_RUNNING,
                    ),
                )
                .await?;
        }
        Ok(updated)
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
    pub async fn recover_stale_runs(&self, error_msg: &str) -> Result<u64> {
        self.conn.execute_batch("BEGIN IMMEDIATE").await?;
        let result: anyhow::Result<u64> = async {
            self.conn
                .execute(
                    "UPDATE scheduled_tasks SET last_error = ?1
             WHERE last_error IS NULL
               AND id IN (SELECT DISTINCT task_id FROM task_runs WHERE status = ?2)",
                    params!(error_msg, crate::tasks::RUN_STATUS_RUNNING),
                )
                .await?;
            let updated = self
                .conn
                .execute(
                    "UPDATE task_runs SET status = ?1, error = ?2
             WHERE status = ?3",
                    params!(
                        crate::tasks::RUN_STATUS_FAILED,
                        error_msg,
                        crate::tasks::RUN_STATUS_RUNNING
                    ),
                )
                .await?;
            Ok(updated)
        }
        .await;
        match result {
            Ok(updated) => {
                self.conn.execute_batch("COMMIT").await?;
                Ok(updated)
            }
            Err(e) => {
                if let Err(re) = self.conn.execute_batch("ROLLBACK").await {
                    tracing::warn!("tasks: rollback after recover_stale_runs failure failed: {re}");
                }
                Err(e)
            }
        }
    }

    // ── Daemon Lock ──

    /// Attempt to acquire the daemon lock. Returns Ok(true) if acquired.
    /// A lock is considered stale after 300s without heartbeat refresh.
    /// Uses IMMEDIATE transaction to prevent TOCTOU races.
    pub async fn acquire_daemon_lock(&self, pid: u32, now: i64) -> Result<bool> {
        self.conn.execute_batch("BEGIN IMMEDIATE").await?;
        let result = self.acquire_daemon_lock_inner(pid, now).await;
        match &result {
            Ok(_) => {
                self.conn.execute_batch("COMMIT").await?;
            }
            Err(_) => {
                if let Err(re) = self.conn.execute_batch("ROLLBACK").await {
                    tracing::warn!(
                        "tasks: rollback after acquire_daemon_lock failure failed: {re}"
                    );
                }
            }
        }
        result
    }

    async fn acquire_daemon_lock_inner(&self, pid: u32, now: i64) -> Result<bool> {
        let existing: Option<(i64, i64)> = {
            let mut rows = self
                .conn
                .query("SELECT pid, heartbeat_at FROM daemon_lock WHERE id = 1", ())
                .await?;
            match rows.next().await? {
                Some(row) => Some((row.get::<i64>(0)?, row.get::<i64>(1)?)),
                None => None,
            }
        };

        match existing {
            None => {
                self.conn.execute(
                    "INSERT INTO daemon_lock (id, pid, started_at, heartbeat_at) VALUES (1, ?1, ?2, ?2)",
                    params!(pid as i64, now),
                ).await?;
                Ok(true)
            }
            Some((existing_pid, heartbeat_at)) => {
                if existing_pid == pid as i64 {
                    // Same PID (daemon restart) — take over
                    self.conn
                        .execute(
                            "UPDATE daemon_lock SET started_at = ?1, heartbeat_at = ?1 WHERE id = 1",
                            params!(now),
                        )
                        .await?;
                    Ok(true)
                } else if !pid_alive(existing_pid as u32) {
                    // Prior daemon died (SIGKILL / crash) — PID gone from this
                    // host. Couples the kernel-managed PID flock to the DB
                    // lock: a SIGKILLed borgd would otherwise block restart
                    // for up to 300s waiting on heartbeat staleness.
                    tracing::info!(
                        stale_pid = existing_pid,
                        new_pid = pid,
                        "daemon_lock: previous borgd PID is gone, taking over immediately",
                    );
                    self.conn.execute(
                        "UPDATE daemon_lock SET pid = ?1, started_at = ?2, heartbeat_at = ?2 WHERE id = 1",
                        params!(pid as i64, now),
                    ).await?;
                    Ok(true)
                } else if now - heartbeat_at > crate::constants::DAEMON_LOCK_STALE_SECS {
                    // Stale lock fallback (PID recycled to unrelated live
                    // process, or DB shared across hosts) — take over
                    self.conn.execute(
                        "UPDATE daemon_lock SET pid = ?1, started_at = ?2, heartbeat_at = ?2 WHERE id = 1",
                        params!(pid as i64, now),
                    ).await?;
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
    pub async fn refresh_daemon_lock(&self, pid: u32, now: i64) -> Result<()> {
        let rows = self
            .conn
            .execute(
                "UPDATE daemon_lock SET heartbeat_at = ?1 WHERE id = 1 AND pid = ?2",
                params!(now, pid as i64),
            )
            .await?;
        if rows == 0 {
            anyhow::bail!("daemon lock lost: no row matched pid {pid} (lock stolen or released)");
        }
        Ok(())
    }

    /// Returns true if a daemon holds a non-stale lock (heartbeat within 300s).
    pub async fn is_daemon_lock_held(&self) -> bool {
        let now = chrono::Utc::now().timestamp();
        let mut rows = match self
            .conn
            .query("SELECT heartbeat_at FROM daemon_lock WHERE id = 1", ())
            .await
        {
            Ok(r) => r,
            Err(_) => return false,
        };
        match rows.next().await {
            Ok(Some(row)) => row
                .get::<i64>(0)
                .map(|heartbeat_at| now - heartbeat_at <= crate::constants::DAEMON_LOCK_STALE_SECS)
                .unwrap_or(false),
            _ => false,
        }
    }

    /// Release the daemon lock on shutdown.
    pub async fn release_daemon_lock(&self, pid: u32) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM daemon_lock WHERE id = 1 AND pid = ?1",
                params!(pid as i64),
            )
            .await?;
        Ok(())
    }

    /// Read-only snapshot of the singleton daemon-lock row as `(pid,
    /// heartbeat_at)`, or `None` when no daemon has ever taken the lock.
    /// Used by `borg doctor` to detect an orphaned lock without taking it.
    pub async fn daemon_lock_snapshot(&self) -> Result<Option<(u32, i64)>> {
        let mut rows = self
            .conn
            .query("SELECT pid, heartbeat_at FROM daemon_lock WHERE id = 1", ())
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some((row.get::<i64>(0)? as u32, row.get::<i64>(1)?))),
            None => Ok(None),
        }
    }

    /// Orphan age of the daemon lock: `Some(heartbeat_age_secs)` when the
    /// lock is held by a PID that is **not alive on this host** AND its
    /// heartbeat is older than `grace_secs`; `None` when the lock is healthy
    /// (live PID, fresh heartbeat) or absent. This is the read-only detection
    /// half of the orphan-lock repair — a running daemon is never reported.
    pub async fn daemon_lock_orphan_age(&self, now: i64, grace_secs: i64) -> Result<Option<i64>> {
        let Some((pid, heartbeat_at)) = self.daemon_lock_snapshot().await? else {
            return Ok(None);
        };
        let age = now - heartbeat_at;
        if pid_alive(pid) || age <= grace_secs {
            return Ok(None);
        }
        Ok(Some(age))
    }

    /// Clear an orphaned daemon-lock row — but **only** when it is genuinely
    /// orphaned by the same rule as [`daemon_lock_orphan_age`] (dead PID +
    /// stale heartbeat). Returns `true` when a row was removed. A live daemon
    /// (running PID or fresh heartbeat) is never cleared, so this is safe to
    /// run while a daemon is up. Backs the `locks.clear_orphan` remediation.
    pub async fn clear_stale_daemon_lock(&self, now: i64, grace_secs: i64) -> Result<bool> {
        let Some((pid, heartbeat_at)) = self.daemon_lock_snapshot().await? else {
            return Ok(false);
        };
        if pid_alive(pid) || now - heartbeat_at <= grace_secs {
            return Ok(false);
        }
        let removed = self
            .conn
            .execute(
                "DELETE FROM daemon_lock WHERE id = 1 AND pid = ?1",
                params!(pid as i64),
            )
            .await?;
        Ok(removed > 0)
    }

    /// Delete a task and all its run history. Returns true if found.
    pub async fn delete_task(&self, id: &str) -> Result<bool> {
        self.conn
            .execute("DELETE FROM task_runs WHERE task_id = ?1", params!(id))
            .await?;
        let deleted = self
            .conn
            .execute("DELETE FROM scheduled_tasks WHERE id = ?1", params!(id))
            .await?;
        Ok(deleted > 0)
    }

    /// Update a task's name, prompt, schedule, or timezone. Returns true if found.
    pub async fn update_task(&self, id: &str, update: &UpdateTask<'_>) -> Result<bool> {
        let existing = match self.get_task_by_id(id).await? {
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
            params!(name, prompt, schedule_type, schedule_expr, timezone, next_run, id),
        ).await?;
        Ok(true)
    }

    /// Get the most recent run for a task, if any.
    pub async fn last_task_run(&self, task_id: &str) -> Result<Option<TaskRunRow>> {
        let runs = self.task_run_history(task_id, 1).await?;
        Ok(runs.into_iter().next())
    }

    /// Set the one-shot `bypass_quiet_hours_once` flag on a task. Used by
    /// `Admin.Poke` / `TriggerHeartbeat(bypass=true)` to make the heartbeat
    /// row's next dispatch ignore quiet hours and the per-task min-interval
    /// floor. The flag is cleared atomically inside `claim_due_tasks` so a
    /// concurrent second poke cannot silently lose its override.
    pub async fn set_task_bypass_quiet_hours_once(&self, id: &str, bypass: bool) -> Result<()> {
        self.conn
            .execute(
                "UPDATE scheduled_tasks SET bypass_quiet_hours_once = ?1 WHERE id = ?2",
                params!(bypass as i64, id),
            )
            .await?;
        Ok(())
    }

    /// Persist the hash of the most recently delivered response for a task.
    /// Used by the heartbeat dispatch path as belt-and-suspenders dedup
    /// behind the primary `[SILENT]` suppression. Pass `None` to clear.
    pub async fn update_task_response_hash(&self, id: &str, hash: Option<&str>) -> Result<()> {
        self.conn
            .execute(
                "UPDATE scheduled_tasks SET last_response_hash = ?1 WHERE id = ?2",
                params!(hash, id),
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> Database {
        Database::test_db().await
    }

    /// Spawn a child, wait for it to exit, and return its PID. The kernel
    /// reaps on `wait()`, so the returned PID is reliably dead at the moment
    /// of return. Used to exercise the dead-PID takeover path without flaky
    /// "guess a free PID" heuristics.
    ///
    /// Theoretical PID-recycle race: on a saturated CI host the kernel could
    /// hand this PID to a brand-new process between `wait()` and the probe.
    /// Window is microseconds and we've not observed it; if it ever flakes,
    /// switch to a sentinel value or a held-but-unreaped child.
    fn reaped_child_pid() -> u32 {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn /usr/bin/true");
        let pid = child.id();
        let _ = child.wait().expect("wait on /usr/bin/true");
        pid
    }

    #[test]
    fn pid_alive_true_for_self() {
        // Real failure mode: a broken probe that returns false for live PIDs
        // would let any new daemon steal the lock from a running one.
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn pid_alive_false_for_reaped_child() {
        // Real failure mode: a probe that can't detect death keeps SIGKILL
        // recovery stuck on the 300s heartbeat fallback (the bug this fix
        // exists for).
        assert!(!pid_alive(reaped_child_pid()));
    }

    #[test]
    fn pid_alive_false_for_zero() {
        // PID 0 isn't a process — must never be treated as alive (would
        // otherwise let a zero-PID stale row block restart forever).
        assert!(!pid_alive(0));
    }

    #[tokio::test]
    async fn dead_pid_takeover_skips_heartbeat_wait() {
        // Couples the PID flock and DB daemon_lock: when the prior borgd is
        // SIGKILLed, its PID is gone and the new borgd must take over
        // immediately rather than waiting up to 300s for heartbeat staleness.
        let db = test_db().await;
        let dead_pid = reaped_child_pid();
        let now = 1_000;

        assert!(
            db.acquire_daemon_lock(dead_pid, now).await.unwrap(),
            "initial acquire as the soon-to-be-dead PID must succeed",
        );

        // Only 10s later — well inside the 300s heartbeat staleness window
        // — but the recorded PID is already dead. Liveness probe must
        // short-circuit the heartbeat fallback.
        let new_pid = std::process::id();
        assert_ne!(new_pid, dead_pid);
        assert!(
            db.acquire_daemon_lock(new_pid, now + 10).await.unwrap(),
            "new daemon must take over immediately when prior PID is dead, \
             not wait the 300s heartbeat window",
        );
    }

    #[tokio::test]
    async fn live_foreign_pid_within_heartbeat_blocks_acquire() {
        // Symmetric guard: a *live* foreign PID inside the heartbeat window
        // must still block. Without this, the liveness branch could be
        // mistakenly applied to live processes and starve the real daemon.
        let db = test_db().await;
        let live_pid = std::process::id();
        let now = 1_000;

        assert!(db.acquire_daemon_lock(live_pid, now).await.unwrap());

        // A different PID that we know is alive (this test process itself,
        // re-used as the "incumbent"): the new caller pretends to be a
        // sibling pid that the kernel will also report as alive. We pick
        // PID 1 (init/launchd) which is guaranteed-alive on any Unix host.
        let incumbent = 1u32;
        // Overwrite the row to record an alive foreign PID.
        db.conn
            .execute(
                "UPDATE daemon_lock SET pid = ?1 WHERE id = 1",
                params!(incumbent as i64),
            )
            .await
            .unwrap();

        // 10s later, still fresh heartbeat, incumbent is alive → must reject.
        assert!(
            !db.acquire_daemon_lock(live_pid + 1, now + 10)
                .await
                .unwrap(),
            "live incumbent PID inside heartbeat window must block takeover",
        );
    }

    #[tokio::test]
    async fn heartbeat_stale_takeover_still_works() {
        // Defense-in-depth fallback: even if the PID number happens to be
        // recycled to a live unrelated process, the 300s heartbeat staleness
        // path must still allow takeover.
        let db = test_db().await;
        let incumbent = 1u32; // launchd / init — reliably alive.
        let now = 1_000;

        assert!(db.acquire_daemon_lock(incumbent, now).await.unwrap());

        // 400s later: heartbeat is stale even though PID 1 is alive.
        assert!(
            db.acquire_daemon_lock(std::process::id(), now + 400)
                .await
                .unwrap(),
            "stale heartbeat must permit takeover even when recorded PID is alive",
        );
    }

    #[tokio::test]
    async fn orphan_age_and_clear_remove_dead_stale_lock() {
        // The crash-recovery case `borg doctor --repair` exists for: a daemon
        // SIGKILLed mid-session leaves a lock row whose PID is dead. Both the
        // detection (`daemon_lock_orphan_age`) and the repair
        // (`clear_stale_daemon_lock`) must fire once the heartbeat is stale.
        let db = test_db().await;
        let dead_pid = reaped_child_pid();
        let lock_time = 1_000;
        assert!(db.acquire_daemon_lock(dead_pid, lock_time).await.unwrap());

        // 400s later: dead PID + heartbeat past the 300s grace → orphaned.
        let now = lock_time + 400;
        let grace = 300;
        assert_eq!(
            db.daemon_lock_orphan_age(now, grace).await.unwrap(),
            Some(400),
            "dead PID past grace must report its idle age",
        );
        assert!(
            db.clear_stale_daemon_lock(now, grace).await.unwrap(),
            "stale orphan lock must be removed",
        );
        assert_eq!(
            db.daemon_lock_snapshot().await.unwrap(),
            None,
            "row must be gone after clear",
        );
        assert!(
            !db.clear_stale_daemon_lock(now, grace).await.unwrap(),
            "second clear is a no-op (nothing to remove)",
        );
    }

    #[tokio::test]
    async fn orphan_age_spares_dead_pid_within_grace() {
        // A daemon that died seconds ago is still inside the heartbeat grace
        // window — clearing it would race a restart that's about to take over.
        // Detection must stay silent and repair must not remove the row.
        let db = test_db().await;
        let dead_pid = reaped_child_pid();
        let lock_time = 1_000;
        assert!(db.acquire_daemon_lock(dead_pid, lock_time).await.unwrap());

        let now = lock_time + 10; // 10s < 300s grace
        let grace = 300;
        assert_eq!(db.daemon_lock_orphan_age(now, grace).await.unwrap(), None);
        assert!(!db.clear_stale_daemon_lock(now, grace).await.unwrap());
        assert!(
            db.daemon_lock_snapshot().await.unwrap().is_some(),
            "fresh-heartbeat lock must be preserved",
        );
    }

    #[tokio::test]
    async fn orphan_age_spares_live_pid_even_when_stale() {
        // A live daemon whose heartbeat thread merely stalled (e.g. a long GC
        // pause) must never be treated as orphaned — its PID is alive. PID 1
        // (launchd/init) is reliably alive on any Unix host.
        let db = test_db().await;
        let live_pid = 1u32;
        let lock_time = 1_000;
        assert!(db.acquire_daemon_lock(live_pid, lock_time).await.unwrap());

        let now = lock_time + 10_000; // heartbeat very stale
        let grace = 300;
        assert_eq!(
            db.daemon_lock_orphan_age(now, grace).await.unwrap(),
            None,
            "live PID must never be reported orphaned regardless of staleness",
        );
        assert!(!db.clear_stale_daemon_lock(now, grace).await.unwrap());
    }

    #[tokio::test]
    async fn orphan_age_none_when_no_lock() {
        let db = test_db().await;
        assert_eq!(db.daemon_lock_orphan_age(1_000, 300).await.unwrap(), None);
        assert!(!db.clear_stale_daemon_lock(1_000, 300).await.unwrap());
    }
}
