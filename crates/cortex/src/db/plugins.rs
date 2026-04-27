use anyhow::Result;
use rusqlite::params;

use super::models::PluginRow;
use super::Database;

impl Database {
    // ── Plugins ──

    /// Insert or replace a plugin record in the customizations table.
    pub fn insert_plugin(&self, id: &str, name: &str, kind: &str, category: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT OR REPLACE INTO customizations (id, name, kind, category, status, version, installed_at)
             VALUES (?1, ?2, ?3, ?4, 'installed', '1.0.0', ?5)",
            params![id, name, kind, category, now],
        )?;
        Ok(())
    }

    /// Remove a plugin record. Returns true if it existed.
    pub fn delete_plugin(&self, id: &str) -> Result<bool> {
        let deleted = self
            .conn
            .execute("DELETE FROM customizations WHERE id = ?1", params![id])?;
        Ok(deleted > 0)
    }

    /// List all installed plugins ordered by category and name.
    pub fn list_plugins(&self) -> Result<Vec<PluginRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, kind, category, status, version, installed_at, verified_at
             FROM customizations ORDER BY category, name",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(PluginRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    kind: row.get(2)?,
                    category: row.get(3)?,
                    status: row.get(4)?,
                    version: row.get(5)?,
                    installed_at: row.get(6)?,
                    verified_at: row.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Mark a plugin as verified with the current timestamp.
    pub fn set_plugin_verified(&self, id: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "UPDATE customizations SET verified_at = ?1 WHERE id = ?2",
            params![now, id],
        )?;
        Ok(())
    }

    /// Store a credential reference for a plugin.
    pub fn insert_credential(
        &self,
        plugin_id: &str,
        credential_key: &str,
        storage_type: &str,
        keychain_service: Option<&str>,
        env_var: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO customization_credentials
             (customization_id, credential_key, storage_type, keychain_service, env_var)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                plugin_id,
                credential_key,
                storage_type,
                keychain_service,
                env_var
            ],
        )?;
        Ok(())
    }

    /// Delete all credentials associated with a plugin. Returns count deleted.
    pub fn delete_credentials_for(&self, plugin_id: &str) -> Result<usize> {
        let count = self.conn.execute(
            "DELETE FROM customization_credentials WHERE customization_id = ?1",
            params![plugin_id],
        )?;
        Ok(count)
    }

    // ── File hashes (integrity) ──

    /// Store a SHA-256 hash for a plugin's installed file.
    pub fn insert_file_hash(&self, plugin_id: &str, file_path: &str, sha256: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT OR REPLACE INTO file_hashes (customization_id, file_path, sha256, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![plugin_id, file_path, sha256, now],
        )?;
        Ok(())
    }

    /// Get all stored file hashes for a plugin as (path, sha256) pairs.
    pub fn get_file_hashes(&self, plugin_id: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT file_path, sha256 FROM file_hashes WHERE customization_id = ?1")?;
        let rows = stmt
            .query_map(params![plugin_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Delete all file hashes for a plugin. Returns count deleted.
    pub fn delete_file_hashes(&self, plugin_id: &str) -> Result<usize> {
        let count = self.conn.execute(
            "DELETE FROM file_hashes WHERE customization_id = ?1",
            params![plugin_id],
        )?;
        Ok(count)
    }

    /// Look up which plugin installed a given tool.
    pub fn get_tool_plugin_id(&self, tool_name: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT customization_id FROM installed_tools WHERE name = ?1")?;
        let mut rows = stmt.query_map(params![tool_name], |row| row.get::<_, Option<String>>(0))?;
        match rows.next() {
            Some(Ok(val)) => Ok(val),
            _ => Ok(None),
        }
    }

    /// Look up which plugin installed a given channel.
    pub fn get_channel_plugin_id(&self, channel_name: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT customization_id FROM installed_channels WHERE name = ?1")?;
        let mut rows =
            stmt.query_map(params![channel_name], |row| row.get::<_, Option<String>>(0))?;
        match rows.next() {
            Some(Ok(val)) => Ok(val),
            _ => Ok(None),
        }
    }

    /// Record a tool installed by a plugin.
    pub fn insert_installed_tool(
        &self,
        name: &str,
        description: &str,
        runtime: &str,
        plugin_id: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT OR REPLACE INTO installed_tools (name, description, runtime, source, customization_id, installed_at)
             VALUES (?1, ?2, ?3, 'plugin', ?4, ?5)",
            params![name, description, runtime, plugin_id, now],
        )?;
        Ok(())
    }

    /// Delete installed channels associated with a plugin. Returns count deleted.
    pub fn delete_installed_channels_for(&self, plugin_id: &str) -> Result<usize> {
        let count = self.conn.execute(
            "DELETE FROM installed_channels WHERE customization_id = ?1",
            params![plugin_id],
        )?;
        Ok(count)
    }

    /// Delete installed tools associated with a plugin. Returns count deleted.
    pub fn delete_installed_tools_for(&self, plugin_id: &str) -> Result<usize> {
        let count = self.conn.execute(
            "DELETE FROM installed_tools WHERE customization_id = ?1",
            params![plugin_id],
        )?;
        Ok(count)
    }

    /// Record a channel installed by a plugin.
    pub fn insert_installed_channel(
        &self,
        name: &str,
        description: &str,
        runtime: &str,
        plugin_id: &str,
        webhook_path: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT OR REPLACE INTO installed_channels (name, description, runtime, source, customization_id, webhook_path, installed_at)
             VALUES (?1, ?2, ?3, 'plugin', ?4, ?5, ?6)",
            params![name, description, runtime, plugin_id, webhook_path, now],
        )?;
        Ok(())
    }
}
