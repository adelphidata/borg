use anyhow::Result;
use turso::params;

use super::models::{AgentRoleRow, SubAgentRunRow};
use super::Database;
use crate::multi_agent::SubAgentStatus;

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
            params!(name, description, model.map(|s| s.to_string()), provider.map(|s| s.to_string()), temperature_f64, system_instructions.map(|s| s.to_string()), tools_allowed.map(|s| s.to_string()), max_iterations, is_builtin as i32, now),
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
            params!(now, description.map(|s| s.to_string()), model.map(|s| s.to_string()), provider.map(|s| s.to_string()), temperature_f64, system_instructions.map(|s| s.to_string()), tools_allowed.map(|s| s.to_string()), max_iterations, name),
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
            Some(row) => Ok(Some(Self::agent_role_row_from_turso(&row)?)),
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
            out.push(Self::agent_role_row_from_turso(&row)?);
        }
        Ok(out)
    }

    fn agent_role_row_from_turso(row: &turso::Row) -> Result<AgentRoleRow> {
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

    pub async fn update_sub_agent_status(
        &self,
        id: &str,
        status: &SubAgentStatus,
    ) -> Result<()> {
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

    pub async fn list_sub_agent_runs(
        &self,
        parent_session_id: &str,
    ) -> Result<Vec<SubAgentRunRow>> {
        let mut rows = self.conn.query(
            "SELECT id, nickname, role, parent_session_id, session_id, depth, status, result_text, error_text, created_at, completed_at FROM sub_agent_runs WHERE parent_session_id = ?1 ORDER BY created_at",
            params!(parent_session_id),
        ).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(Self::sub_agent_run_row_from_turso(&row)?);
        }
        Ok(out)
    }

    pub async fn get_sub_agent_run(&self, id: &str) -> Result<Option<SubAgentRunRow>> {
        let mut rows = self.conn.query(
            "SELECT id, nickname, role, parent_session_id, session_id, depth, status, result_text, error_text, created_at, completed_at FROM sub_agent_runs WHERE id = ?1",
            params!(id),
        ).await?;
        match rows.next().await? {
            Some(row) => Ok(Some(Self::sub_agent_run_row_from_turso(&row)?)),
            None => Ok(None),
        }
    }

    fn sub_agent_run_row_from_turso(row: &turso::Row) -> Result<SubAgentRunRow> {
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
        })
    }
}
