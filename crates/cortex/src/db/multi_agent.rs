use anyhow::Result;
use libsql::params;

use super::models::{AgentRoleRow, SubAgentRunRow};
use super::Database;
use crate::multi_agent::{SubAgentMetrics, SubAgentStatus};

/// Per-session snapshot ring cap (rows kept in `agent_tree_snapshots`).
pub const AGENT_SNAPSHOT_RING_CAP: usize = 10;

/// One persisted spawn-tree snapshot.
#[derive(Debug, Clone)]
pub struct AgentTreeSnapshotRow {
    /// Row id.
    pub id: i64,
    /// Owning session id.
    pub session_id: String,
    /// Caller-supplied turn identifier.
    pub turn_id: String,
    /// Short human-friendly label (used in the TUI history footer).
    pub label: String,
    /// Unix timestamp when the turn began.
    pub started_at: i64,
    /// Unix timestamp when the turn finished.
    pub finished_at: i64,
    /// JSON-encoded snapshot of the agents in this turn.
    pub snapshot_json: String,
}

impl Database {
    // ── Agent Roles ──

    #[allow(clippy::too_many_arguments)]
    pub async fn insert_role(
        &self,
        name: &str,
        description: &str,
        model: Option<&str>,
        provider: Option<&str>,
        temperature: Option<f32>,
        system_instructions: Option<&str>,
        tools_allowed: Option<&str>,
        max_iterations: Option<i64>,
        is_builtin: bool,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let temperature_f64 = temperature.map(|t| t as f64);
        self.conn.execute(
            "INSERT INTO agent_roles (name, description, model, provider, temperature, system_instructions, tools_allowed, max_iterations, is_builtin, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
            params!(name, description, model.map(std::string::ToString::to_string), provider.map(std::string::ToString::to_string), temperature_f64, system_instructions.map(std::string::ToString::to_string), tools_allowed.map(std::string::ToString::to_string), max_iterations, is_builtin as i32, now),
        ).await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update_role(
        &self,
        name: &str,
        description: Option<&str>,
        model: Option<&str>,
        provider: Option<&str>,
        temperature: Option<f32>,
        system_instructions: Option<&str>,
        tools_allowed: Option<&str>,
        max_iterations: Option<i64>,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let temperature_f64 = temperature.map(|t| t as f64);
        self.conn.execute(
            "UPDATE agent_roles SET description = COALESCE(?2, description), model = COALESCE(?3, model), provider = COALESCE(?4, provider), temperature = COALESCE(?5, temperature), system_instructions = COALESCE(?6, system_instructions), tools_allowed = COALESCE(?7, tools_allowed), max_iterations = COALESCE(?8, max_iterations), updated_at = ?1 WHERE name = ?9",
            params!(now, description.map(std::string::ToString::to_string), model.map(std::string::ToString::to_string), provider.map(std::string::ToString::to_string), temperature_f64, system_instructions.map(std::string::ToString::to_string), tools_allowed.map(std::string::ToString::to_string), max_iterations, name),
        ).await?;
        Ok(())
    }

    pub async fn delete_role(&self, name: &str) -> Result<bool> {
        let count = self
            .conn
            .execute("DELETE FROM agent_roles WHERE name = ?1", params!(name))
            .await?;
        Ok(count > 0)
    }

    pub async fn get_role(&self, name: &str) -> Result<Option<AgentRoleRow>> {
        let mut rows = self.conn.query(
            "SELECT name, description, model, provider, temperature, system_instructions, tools_allowed, max_iterations, is_builtin, created_at, updated_at FROM agent_roles WHERE name = ?1",
            params!(name),
        ).await?;
        match rows.next().await? {
            Some(row) => Ok(Some(Self::agent_role_row_from_row(&row)?)),
            None => Ok(None),
        }
    }

    pub async fn list_roles(&self) -> Result<Vec<AgentRoleRow>> {
        let mut rows = self.conn.query(
            "SELECT name, description, model, provider, temperature, system_instructions, tools_allowed, max_iterations, is_builtin, created_at, updated_at FROM agent_roles ORDER BY name",
            (),
        ).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(Self::agent_role_row_from_row(&row)?);
        }
        Ok(out)
    }

    fn agent_role_row_from_row(row: &libsql::Row) -> Result<AgentRoleRow> {
        Ok(AgentRoleRow {
            name: row.get::<String>(0)?,
            description: row.get::<String>(1)?,
            model: row.get::<Option<String>>(2)?,
            provider: row.get::<Option<String>>(3)?,
            temperature: row.get::<Option<f64>>(4)?.map(|v| v as f32),
            system_instructions: row.get::<Option<String>>(5)?,
            tools_allowed: row.get::<Option<String>>(6)?,
            max_iterations: row.get::<Option<i64>>(7)?,
            is_builtin: row.get::<i32>(8)? != 0,
            created_at: row.get::<i64>(9)?,
            updated_at: row.get::<i64>(10)?,
        })
    }

    // ── Sub-Agent Runs ──

    pub async fn insert_sub_agent_run(
        &self,
        id: &str,
        nickname: &str,
        role: &str,
        parent_session_id: &str,
        session_id: &str,
        depth: u32,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO sub_agent_runs (id, nickname, role, parent_session_id, session_id, depth, status, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending_init', ?7)",
            params!(id, nickname, role, parent_session_id, session_id, depth as i64, now),
        ).await?;
        Ok(())
    }

    /// Persist the `model` column once known. Called from `run_sub_agent`
    /// after the child config has resolved the effective model.
    pub async fn update_sub_agent_model(&self, id: &str, model: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE sub_agent_runs SET model = ?2 WHERE id = ?1",
                params!(id, model),
            )
            .await?;
        Ok(())
    }

    /// Flush accumulated metrics for an agent. Called periodically while
    /// running (debounced) and on terminal-state transitions so the live
    /// view and persisted row stay in sync.
    pub async fn update_sub_agent_metrics(
        &self,
        id: &str,
        metrics: &SubAgentMetrics,
    ) -> Result<()> {
        let files_json = metrics.files_touched_json();
        self.conn
            .execute(
                "UPDATE sub_agent_runs
                 SET input_tokens = ?2, output_tokens = ?3, reasoning_tokens = ?4,
                     cost_usd = ?5, tool_calls = ?6, files_touched = ?7
                 WHERE id = ?1",
                params!(
                    id,
                    metrics.input_tokens as i64,
                    metrics.output_tokens as i64,
                    metrics.reasoning_tokens as i64,
                    metrics.cost_usd,
                    metrics.tool_calls as i64,
                    files_json,
                ),
            )
            .await?;
        Ok(())
    }

    pub async fn update_sub_agent_status(&self, id: &str, status: &SubAgentStatus) -> Result<()> {
        let status_str = status.as_str();
        let result_text = match status {
            SubAgentStatus::Completed { result } => Some(result.clone()),
            _ => None,
        };
        let error_text = match status {
            SubAgentStatus::Errored { error } => Some(error.clone()),
            _ => None,
        };
        let completed_at = if status.is_terminal() {
            Some(chrono::Utc::now().timestamp())
        } else {
            None
        };
        self.conn.execute(
            "UPDATE sub_agent_runs SET status = ?2, result_text = ?3, error_text = ?4, completed_at = ?5 WHERE id = ?1",
            params!(id, status_str, result_text, error_text, completed_at),
        ).await?;
        Ok(())
    }

    /// Return every sub-agent in the spawn tree rooted at the given session
    /// — direct children PLUS transitive descendants (grandchildren etc.).
    ///
    /// A naïve `WHERE parent_session_id = ?1` only returns depth-1 rows
    /// because grandchildren carry their *parent agent's* session_id, not
    /// the root's. The dashboard and `borg agents tree` render the full
    /// tree, so the recursive CTE walks `session_id → parent_session_id`
    /// until the closure stabilizes. Ordering is `(depth, created_at)` so
    /// callers can render the tree directly without re-sorting.
    pub async fn list_sub_agent_runs(
        &self,
        parent_session_id: &str,
    ) -> Result<Vec<SubAgentRunRow>> {
        let mut rows = self
            .conn
            .query(
                "WITH RECURSIVE tree(id, nickname, role, parent_session_id, session_id, depth, status, result_text, error_text, created_at, completed_at, model, input_tokens, output_tokens, reasoning_tokens, cost_usd, tool_calls, files_touched) AS (
                    SELECT id, nickname, role, parent_session_id, session_id, depth, status, result_text, error_text, created_at, completed_at, model, input_tokens, output_tokens, reasoning_tokens, cost_usd, tool_calls, files_touched
                    FROM sub_agent_runs WHERE parent_session_id = ?1
                    UNION
                    SELECT s.id, s.nickname, s.role, s.parent_session_id, s.session_id, s.depth, s.status, s.result_text, s.error_text, s.created_at, s.completed_at, s.model, s.input_tokens, s.output_tokens, s.reasoning_tokens, s.cost_usd, s.tool_calls, s.files_touched
                    FROM sub_agent_runs s JOIN tree t ON s.parent_session_id = t.session_id
                 )
                 SELECT id, nickname, role, parent_session_id, session_id, depth, status, result_text, error_text, created_at, completed_at, model, input_tokens, output_tokens, reasoning_tokens, cost_usd, tool_calls, files_touched
                 FROM tree ORDER BY depth, created_at",
                params!(parent_session_id),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(Self::sub_agent_run_row_from_row(&row)?);
        }
        Ok(out)
    }

    pub async fn get_sub_agent_run(&self, id: &str) -> Result<Option<SubAgentRunRow>> {
        let mut rows = self.conn.query(
            "SELECT id, nickname, role, parent_session_id, session_id, depth, status, result_text, error_text, created_at, completed_at, model, input_tokens, output_tokens, reasoning_tokens, cost_usd, tool_calls, files_touched FROM sub_agent_runs WHERE id = ?1",
            params!(id),
        ).await?;
        match rows.next().await? {
            Some(row) => Ok(Some(Self::sub_agent_run_row_from_row(&row)?)),
            None => Ok(None),
        }
    }

    fn sub_agent_run_row_from_row(row: &libsql::Row) -> Result<SubAgentRunRow> {
        Ok(SubAgentRunRow {
            id: row.get::<String>(0)?,
            nickname: row.get::<String>(1)?,
            role: row.get::<String>(2)?,
            parent_session_id: row.get::<String>(3)?,
            session_id: row.get::<String>(4)?,
            depth: row.get::<i64>(5)? as u32,
            status: row.get::<String>(6)?,
            result_text: row.get::<Option<String>>(7)?,
            error_text: row.get::<Option<String>>(8)?,
            created_at: row.get::<i64>(9)?,
            completed_at: row.get::<Option<i64>>(10)?,
            model: row.get::<String>(11)?,
            input_tokens: row.get::<i64>(12)?,
            output_tokens: row.get::<i64>(13)?,
            reasoning_tokens: row.get::<i64>(14)?,
            cost_usd: row.get::<f64>(15)?,
            tool_calls: row.get::<i64>(16)?,
            files_touched: row.get::<String>(17)?,
        })
    }

    // ── Agent Tree Snapshots ──

    /// Insert a snapshot for `session_id`, then trim the per-session ring so
    /// only the most recent [`AGENT_SNAPSHOT_RING_CAP`] rows remain.
    pub async fn insert_agent_tree_snapshot(
        &self,
        session_id: &str,
        turn_id: &str,
        label: &str,
        started_at: i64,
        finished_at: i64,
        snapshot_json: &str,
    ) -> Result<i64> {
        self.conn
            .execute(
                "INSERT INTO agent_tree_snapshots
                 (session_id, turn_id, label, started_at, finished_at, snapshot_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params!(
                    session_id,
                    turn_id,
                    label,
                    started_at,
                    finished_at,
                    snapshot_json,
                ),
            )
            .await?;
        let id = self.conn.last_insert_rowid();

        // Enforce ring cap (delete everything past the newest N rows).
        self.conn
            .execute(
                "DELETE FROM agent_tree_snapshots
                 WHERE session_id = ?1
                   AND id NOT IN (
                       SELECT id FROM agent_tree_snapshots
                       WHERE session_id = ?1
                       ORDER BY id DESC LIMIT ?2
                   )",
                params!(session_id, AGENT_SNAPSHOT_RING_CAP as i64),
            )
            .await?;

        Ok(id)
    }

    /// List snapshots for a session, newest first (capped to ring size).
    pub async fn list_agent_tree_snapshots(
        &self,
        session_id: &str,
    ) -> Result<Vec<AgentTreeSnapshotRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, session_id, turn_id, label, started_at, finished_at, snapshot_json
                 FROM agent_tree_snapshots
                 WHERE session_id = ?1
                 ORDER BY id DESC
                 LIMIT ?2",
                params!(session_id, AGENT_SNAPSHOT_RING_CAP as i64),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(AgentTreeSnapshotRow {
                id: row.get::<i64>(0)?,
                session_id: row.get::<String>(1)?,
                turn_id: row.get::<String>(2)?,
                label: row.get::<String>(3)?,
                started_at: row.get::<i64>(4)?,
                finished_at: row.get::<i64>(5)?,
                snapshot_json: row.get::<String>(6)?,
            });
        }
        Ok(out)
    }

    /// Fetch a single snapshot by `(session_id, turn_id)`.
    pub async fn get_agent_tree_snapshot(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Option<AgentTreeSnapshotRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, session_id, turn_id, label, started_at, finished_at, snapshot_json
                 FROM agent_tree_snapshots
                 WHERE session_id = ?1 AND turn_id = ?2
                 ORDER BY id DESC LIMIT 1",
                params!(session_id, turn_id),
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(AgentTreeSnapshotRow {
                id: row.get::<i64>(0)?,
                session_id: row.get::<String>(1)?,
                turn_id: row.get::<String>(2)?,
                label: row.get::<String>(3)?,
                started_at: row.get::<i64>(4)?,
                finished_at: row.get::<i64>(5)?,
                snapshot_json: row.get::<String>(6)?,
            })),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::db::Database;

    /// Tokio mutex — safe to hold across `await`. Serializes
    /// process-wide `BORG_HOME` mutations so parallel tests don't
    /// trample each other's data-dir env var (the cortex DB open path
    /// resolves the path from that variable).
    static DB_ENV_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn insert_at(db: &Database, id: &str, parent_session: &str, session: &str, depth: u32) {
        db.insert_sub_agent_run(id, id, "role", parent_session, session, depth)
            .await
            .expect("insert");
    }

    /// Recursive CTE must surface grandchildren — the prior `WHERE
    /// parent_session_id = ?1` shape only returned depth-1 rows, which
    /// broke the tree renderer at level 2+. Both inclusion (descendants
    /// reachable from the root) AND exclusion (sibling subtree under a
    /// different root must not bleed in) are asserted.
    #[tokio::test]
    async fn list_sub_agent_runs_returns_full_transitive_descendants() {
        let _guard = DB_ENV_MUTEX.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_HOME", tmp.path());
        let db = Database::open().await.expect("open test db");
        // Tree under ROOT:  A → B → C ;  ROOT2 → X (must not appear)
        insert_at(&db, "A", "ROOT", "sess-A", 0).await;
        insert_at(&db, "B", "sess-A", "sess-B", 1).await;
        insert_at(&db, "C", "sess-B", "sess-C", 2).await;
        insert_at(&db, "X", "ROOT2", "sess-X", 0).await;

        let rows = db.list_sub_agent_runs("ROOT").await.unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"A"), "depth-1 root must be returned");
        assert!(
            ids.contains(&"B"),
            "depth-2 child must be returned via recursive CTE"
        );
        assert!(
            ids.contains(&"C"),
            "depth-3 grandchild must be returned via recursive CTE"
        );
        assert!(
            !ids.contains(&"X"),
            "agent rooted under ROOT2 must not bleed into ROOT's tree"
        );
        // Ordering: depth-first then created_at; verify grandchild
        // appears after its ancestors so callers can render directly.
        let depth_of = |id: &str| rows.iter().find(|r| r.id == id).unwrap().depth;
        assert!(depth_of("A") <= depth_of("B"));
        assert!(depth_of("B") <= depth_of("C"));

        std::env::remove_var("BORG_HOME");
    }
}
