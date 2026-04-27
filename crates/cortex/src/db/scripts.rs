use anyhow::Result;
use rusqlite::{params, OptionalExtension};

use super::models::{NewScript, ScriptRow};
use super::Database;

impl Database {
    // ── Scripts CRUD ──

    pub fn create_script(&self, s: &NewScript) -> Result<()> {
        self.conn.execute(
            "INSERT INTO scripts (id, name, description, runtime, entrypoint, sandbox_profile,
             network_access, fs_read, fs_write, ephemeral, hmac, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                s.id,
                s.name,
                s.description,
                s.runtime,
                s.entrypoint,
                s.sandbox_profile,
                s.network_access as i32,
                s.fs_read,
                s.fs_write,
                s.ephemeral as i32,
                s.hmac,
                s.created_at,
                s.updated_at,
            ],
        )?;
        Ok(())
    }

    const SCRIPTS_SELECT: &'static str =
        "SELECT id, name, description, runtime, entrypoint, sandbox_profile,
                network_access, fs_read, fs_write, ephemeral, hmac,
                created_at, updated_at, last_run_at, run_count
         FROM scripts";

    fn script_row_from_sql(row: &rusqlite::Row) -> rusqlite::Result<ScriptRow> {
        Ok(ScriptRow {
            id: row.get(0)?,
            name: row.get(1)?,
            description: row.get(2)?,
            runtime: row.get(3)?,
            entrypoint: row.get(4)?,
            sandbox_profile: row.get(5)?,
            network_access: row.get::<_, i32>(6)? != 0,
            fs_read: row.get(7)?,
            fs_write: row.get(8)?,
            ephemeral: row.get::<_, i32>(9)? != 0,
            hmac: row.get(10)?,
            created_at: row.get(11)?,
            updated_at: row.get(12)?,
            last_run_at: row.get(13)?,
            run_count: row.get(14)?,
        })
    }

    pub fn get_script_by_name(&self, name: &str) -> Result<Option<ScriptRow>> {
        let sql = format!("{} WHERE name = ?1", Self::SCRIPTS_SELECT);
        let mut stmt = self.conn.prepare(&sql)?;
        let result = stmt
            .query_row(params![name], Self::script_row_from_sql)
            .optional()?;
        Ok(result)
    }

    pub fn list_scripts(&self) -> Result<Vec<ScriptRow>> {
        let sql = format!("{} ORDER BY name", Self::SCRIPTS_SELECT);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], Self::script_row_from_sql)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn update_script_hmac(&self, id: &str, hmac: &str, updated_at: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE scripts SET hmac = ?1, updated_at = ?2 WHERE id = ?3",
            params![hmac, updated_at, id],
        )?;
        Ok(())
    }

    pub fn record_script_run(&self, id: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "UPDATE scripts SET run_count = run_count + 1, last_run_at = ?1 WHERE id = ?2",
            params![now, id],
        )?;
        Ok(())
    }

    pub fn delete_script(&self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM scripts WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn delete_ephemeral_scripts_older_than(&self, cutoff: i64) -> Result<u64> {
        let count = self.conn.execute(
            "DELETE FROM scripts WHERE ephemeral = 1 AND created_at < ?1",
            params![cutoff],
        )?;
        Ok(count as u64)
    }
}
