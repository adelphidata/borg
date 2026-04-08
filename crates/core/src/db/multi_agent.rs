use anyhow::Result;
use rusqlite::params;

use super::models::{AgentRoleRow, SubAgentRunRow};
use super::Database;
use crate::multi_agent::SubAgentStatus;

use rusqlite::OptionalExtension;

impl Database {
    // ── Agent Roles ──

    #[allow(clippy::too_many_arguments)]
    pub fn insert_role(
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
        self.conn.execute(
            "INSERT INTO agent_roles (name, description, model, provider, temperature, system_instructions, tools_allowed, max_iterations, is_builtin, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
            params![name, description, model, provider, temperature, system_instructions, tools_allowed, max_iterations, is_builtin as i32, now],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_role(
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
        self.conn.execute(
            "UPDATE agent_roles SET description = COALESCE(?2, description), model = COALESCE(?3, model), provider = COALESCE(?4, provider), temperature = COALESCE(?5, temperature), system_instructions = COALESCE(?6, system_instructions), tools_allowed = COALESCE(?7, tools_allowed), max_iterations = COALESCE(?8, max_iterations), updated_at = ?1 WHERE name = ?9",
            params![now, description, model, provider, temperature, system_instructions, tools_allowed, max_iterations, name],
        )?;
        Ok(())
    }

    pub fn delete_role(&self, name: &str) -> Result<bool> {
        let count = self
            .conn
            .execute("DELETE FROM agent_roles WHERE name = ?1", params![name])?;
        Ok(count > 0)
    }

    pub fn get_role(&self, name: &str) -> Result<Option<AgentRoleRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, description, model, provider, temperature, system_instructions, tools_allowed, max_iterations, is_builtin, created_at, updated_at FROM agent_roles WHERE name = ?1",
        )?;
        let row = stmt
            .query_row(params![name], |row| {
                Ok(AgentRoleRow {
                    name: row.get(0)?,
                    description: row.get(1)?,
                    model: row.get(2)?,
                    provider: row.get(3)?,
                    temperature: row.get(4)?,
                    system_instructions: row.get(5)?,
                    tools_allowed: row.get(6)?,
                    max_iterations: row.get(7)?,
                    is_builtin: row.get::<_, i32>(8)? != 0,
                    created_at: row.get(9)?,
                    updated_at: row.get(10)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    pub fn list_roles(&self) -> Result<Vec<AgentRoleRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, description, model, provider, temperature, system_instructions, tools_allowed, max_iterations, is_builtin, created_at, updated_at FROM agent_roles ORDER BY name",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(AgentRoleRow {
                    name: row.get(0)?,
                    description: row.get(1)?,
                    model: row.get(2)?,
                    provider: row.get(3)?,
                    temperature: row.get(4)?,
                    system_instructions: row.get(5)?,
                    tools_allowed: row.get(6)?,
                    max_iterations: row.get(7)?,
                    is_builtin: row.get::<_, i32>(8)? != 0,
                    created_at: row.get(9)?,
                    updated_at: row.get(10)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ── Sub-Agent Runs ──

    pub fn insert_sub_agent_run(
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
            params![id, nickname, role, parent_session_id, session_id, depth, now],
        )?;
        Ok(())
    }

    pub fn update_sub_agent_status(&self, id: &str, status: &SubAgentStatus) -> Result<()> {
        let status_str = status.as_str();
        let result_text = match status {
            SubAgentStatus::Completed { result } => Some(result.as_str()),
            _ => None,
        };
        let error_text = match status {
            SubAgentStatus::Errored { error } => Some(error.as_str()),
            _ => None,
        };
        let completed_at = if status.is_terminal() {
            Some(chrono::Utc::now().timestamp())
        } else {
            None
        };
        self.conn.execute(
            "UPDATE sub_agent_runs SET status = ?2, result_text = ?3, error_text = ?4, completed_at = ?5 WHERE id = ?1",
            params![id, status_str, result_text, error_text, completed_at],
        )?;
        Ok(())
    }

    pub fn list_sub_agent_runs(&self, parent_session_id: &str) -> Result<Vec<SubAgentRunRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, nickname, role, parent_session_id, session_id, depth, status, result_text, error_text, created_at, completed_at FROM sub_agent_runs WHERE parent_session_id = ?1 ORDER BY created_at",
        )?;
        let rows = stmt
            .query_map(params![parent_session_id], |row| {
                Ok(SubAgentRunRow {
                    id: row.get(0)?,
                    nickname: row.get(1)?,
                    role: row.get(2)?,
                    parent_session_id: row.get(3)?,
                    session_id: row.get(4)?,
                    depth: row.get(5)?,
                    status: row.get(6)?,
                    result_text: row.get(7)?,
                    error_text: row.get(8)?,
                    created_at: row.get(9)?,
                    completed_at: row.get(10)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get_sub_agent_run(&self, id: &str) -> Result<Option<SubAgentRunRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, nickname, role, parent_session_id, session_id, depth, status, result_text, error_text, created_at, completed_at FROM sub_agent_runs WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![id], |row| {
                Ok(SubAgentRunRow {
                    id: row.get(0)?,
                    nickname: row.get(1)?,
                    role: row.get(2)?,
                    parent_session_id: row.get(3)?,
                    session_id: row.get(4)?,
                    depth: row.get(5)?,
                    status: row.get(6)?,
                    result_text: row.get(7)?,
                    error_text: row.get(8)?,
                    created_at: row.get(9)?,
                    completed_at: row.get(10)?,
                })
            })
            .optional()?;
        Ok(row)
    }
}
