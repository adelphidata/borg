use anyhow::Result;
use libsql::params;

use super::models::{
    NewWasmPlugin, PluginRow, WasmPluginRow, WasmToolDefinitionRow, WasmToolRuntimeRow,
};
use super::Database;

impl Database {
    // ── Plugins ──

    /// Insert or replace a plugin record in the customizations table.
    pub async fn insert_plugin(
        &self,
        id: &str,
        name: &str,
        kind: &str,
        category: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT OR REPLACE INTO customizations (id, name, kind, category, status, version, installed_at)
             VALUES (?1, ?2, ?3, ?4, 'installed', '1.0.0', ?5)",
            params!(id, name, kind, category, now),
        ).await?;
        Ok(())
    }

    /// Remove a plugin record. Returns true if it existed.
    pub async fn delete_plugin(&self, id: &str) -> Result<bool> {
        let deleted = self
            .conn
            .execute("DELETE FROM customizations WHERE id = ?1", params!(id))
            .await?;
        Ok(deleted > 0)
    }

    /// List all installed plugins ordered by category and name.
    pub async fn list_plugins(&self) -> Result<Vec<PluginRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, name, kind, category, status, version, installed_at, verified_at
             FROM customizations ORDER BY category, name",
                (),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(PluginRow {
                id: row.get::<String>(0)?,
                name: row.get::<String>(1)?,
                kind: row.get::<String>(2)?,
                category: row.get::<String>(3)?,
                status: row.get::<String>(4)?,
                version: row.get::<String>(5)?,
                installed_at: row.get::<i64>(6)?,
                verified_at: row.get::<Option<i64>>(7)?,
            });
        }
        Ok(out)
    }

    /// Mark a plugin as verified with the current timestamp.
    pub async fn set_plugin_verified(&self, id: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "UPDATE customizations SET verified_at = ?1 WHERE id = ?2",
                params!(now, id),
            )
            .await?;
        Ok(())
    }

    /// Store a credential reference for a plugin.
    pub async fn insert_credential(
        &self,
        plugin_id: &str,
        credential_key: &str,
        storage_type: &str,
        keychain_service: Option<&str>,
        env_var: Option<&str>,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO customization_credentials
             (customization_id, credential_key, storage_type, keychain_service, env_var)
             VALUES (?1, ?2, ?3, ?4, ?5)",
                params!(
                    plugin_id,
                    credential_key,
                    storage_type,
                    keychain_service.map(std::string::ToString::to_string),
                    env_var.map(std::string::ToString::to_string),
                ),
            )
            .await?;
        Ok(())
    }

    /// Delete all credentials associated with a plugin. Returns count deleted.
    pub async fn delete_credentials_for(&self, plugin_id: &str) -> Result<u64> {
        let count = self
            .conn
            .execute(
                "DELETE FROM customization_credentials WHERE customization_id = ?1",
                params!(plugin_id),
            )
            .await?;
        Ok(count)
    }

    // ── File hashes (integrity) ──

    /// Store a SHA-256 hash for a plugin's installed file.
    pub async fn insert_file_hash(
        &self,
        plugin_id: &str,
        file_path: &str,
        sha256: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "INSERT OR REPLACE INTO file_hashes (customization_id, file_path, sha256, created_at)
             VALUES (?1, ?2, ?3, ?4)",
                params!(plugin_id, file_path, sha256, now),
            )
            .await?;
        Ok(())
    }

    /// Get all stored file hashes for a plugin as (path, sha256) pairs.
    pub async fn get_file_hashes(&self, plugin_id: &str) -> Result<Vec<(String, String)>> {
        let mut rows = self
            .conn
            .query(
                "SELECT file_path, sha256 FROM file_hashes WHERE customization_id = ?1",
                params!(plugin_id),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push((row.get::<String>(0)?, row.get::<String>(1)?));
        }
        Ok(out)
    }

    /// Delete all file hashes for a plugin. Returns count deleted.
    pub async fn delete_file_hashes(&self, plugin_id: &str) -> Result<u64> {
        let count = self
            .conn
            .execute(
                "DELETE FROM file_hashes WHERE customization_id = ?1",
                params!(plugin_id),
            )
            .await?;
        Ok(count)
    }

    /// Look up which plugin installed a given tool.
    pub async fn get_tool_plugin_id(&self, tool_name: &str) -> Result<Option<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT customization_id FROM installed_tools WHERE name = ?1",
                params!(tool_name),
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(row.get::<Option<String>>(0)?),
            None => Ok(None),
        }
    }

    /// Look up which plugin installed a given channel.
    pub async fn get_channel_plugin_id(&self, channel_name: &str) -> Result<Option<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT customization_id FROM installed_channels WHERE name = ?1",
                params!(channel_name),
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(row.get::<Option<String>>(0)?),
            None => Ok(None),
        }
    }

    /// Record a tool installed by a plugin.
    pub async fn insert_installed_tool(
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
            params!(name, description, runtime, plugin_id, now),
        ).await?;
        Ok(())
    }

    /// Delete installed channels associated with a plugin. Returns count deleted.
    pub async fn delete_installed_channels_for(&self, plugin_id: &str) -> Result<u64> {
        let count = self
            .conn
            .execute(
                "DELETE FROM installed_channels WHERE customization_id = ?1",
                params!(plugin_id),
            )
            .await?;
        Ok(count)
    }

    /// Delete installed tools associated with a plugin. Returns count deleted.
    pub async fn delete_installed_tools_for(&self, plugin_id: &str) -> Result<u64> {
        let count = self
            .conn
            .execute(
                "DELETE FROM installed_tools WHERE customization_id = ?1",
                params!(plugin_id),
            )
            .await?;
        Ok(count)
    }

    /// Record a channel installed by a plugin.
    pub async fn insert_installed_channel(
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
            params!(name, description, runtime, plugin_id, webhook_path, now),
        ).await?;
        Ok(())
    }

    // ── WASM plugins ──

    /// Persist verified WASM plugin metadata and its tool definitions.
    pub async fn insert_wasm_plugin(
        &self,
        plugin: &NewWasmPlugin<'_>,
        tools: &[(&str, &str, &str)],
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute_batch("BEGIN IMMEDIATE").await?;
        let result: anyhow::Result<()> = async {
            self.conn.execute(
                "INSERT OR REPLACE INTO wasm_plugins
             (plugin_id, manifest_json, manifest_sha256, wasm_sha256, publisher_key, signature,
              local_hmac, manifest_path, wasm_path, status, quarantine_reason, installed_at, verified_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'verified', NULL, ?10, ?10)",
                params!(
                    plugin.plugin_id,
                    plugin.manifest_json,
                    plugin.manifest_sha256,
                    plugin.wasm_sha256,
                    plugin.publisher_key,
                    plugin.signature,
                    plugin.local_hmac,
                    plugin.manifest_path,
                    plugin.wasm_path,
                    now,
                ),
            ).await?;
            self.conn.execute(
                "DELETE FROM wasm_plugin_tools WHERE plugin_id = ?1",
                params!(plugin.plugin_id),
            )
            .await?;
            for (name, description, schema) in tools {
                self.conn.execute(
                    "INSERT INTO wasm_plugin_tools
                 (plugin_id, name, description, input_schema_json, installed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                    params!(plugin.plugin_id, *name, *description, *schema, now),
                )
                .await?;
            }
            Ok(())
        }.await;
        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT").await?;
                Ok(())
            }
            Err(e) => {
                if let Err(re) = self.conn.execute_batch("ROLLBACK").await {
                    tracing::warn!(
                        "plugins: rollback after insert_wasm_plugin failure failed: {re}"
                    );
                }
                Err(e)
            }
        }
    }

    /// Return a WASM plugin row by id.
    pub async fn get_wasm_plugin(&self, plugin_id: &str) -> Result<Option<WasmPluginRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT plugin_id, manifest_json, manifest_sha256, wasm_sha256, publisher_key,
                    signature, local_hmac, manifest_path, wasm_path, status, quarantine_reason,
                    installed_at, verified_at
             FROM wasm_plugins WHERE plugin_id = ?1",
                params!(plugin_id),
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(WasmPluginRow {
                plugin_id: row.get::<String>(0)?,
                manifest_json: row.get::<String>(1)?,
                manifest_sha256: row.get::<String>(2)?,
                wasm_sha256: row.get::<String>(3)?,
                publisher_key: row.get::<String>(4)?,
                signature: row.get::<String>(5)?,
                local_hmac: row.get::<String>(6)?,
                manifest_path: row.get::<String>(7)?,
                wasm_path: row.get::<String>(8)?,
                status: row.get::<String>(9)?,
                quarantine_reason: row.get::<Option<String>>(10)?,
                installed_at: row.get::<i64>(11)?,
                verified_at: row.get::<Option<i64>>(12)?,
            })),
            None => Ok(None),
        }
    }

    /// List all WASM plugin rows ordered by id, regardless of status.
    pub async fn list_wasm_plugins(&self) -> Result<Vec<WasmPluginRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT plugin_id, manifest_json, manifest_sha256, wasm_sha256, publisher_key,
                    signature, local_hmac, manifest_path, wasm_path, status, quarantine_reason,
                    installed_at, verified_at
             FROM wasm_plugins ORDER BY plugin_id",
                (),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(WasmPluginRow {
                plugin_id: row.get::<String>(0)?,
                manifest_json: row.get::<String>(1)?,
                manifest_sha256: row.get::<String>(2)?,
                wasm_sha256: row.get::<String>(3)?,
                publisher_key: row.get::<String>(4)?,
                signature: row.get::<String>(5)?,
                local_hmac: row.get::<String>(6)?,
                manifest_path: row.get::<String>(7)?,
                wasm_path: row.get::<String>(8)?,
                status: row.get::<String>(9)?,
                quarantine_reason: row.get::<Option<String>>(10)?,
                installed_at: row.get::<i64>(11)?,
                verified_at: row.get::<Option<i64>>(12)?,
            });
        }
        Ok(out)
    }

    /// Count tools registered for a plugin.
    pub async fn count_wasm_plugin_tools(&self, plugin_id: &str) -> Result<u32> {
        let mut rows = self
            .conn
            .query(
                "SELECT COUNT(*) FROM wasm_plugin_tools WHERE plugin_id = ?1",
                params!(plugin_id),
            )
            .await?;
        let count: i64 = match rows.next().await? {
            Some(row) => row.get::<i64>(0)?,
            None => 0,
        };
        Ok(count.max(0) as u32)
    }

    /// Mark a WASM plugin quarantined so its tools are no longer advertised or executable.
    pub async fn quarantine_wasm_plugin(&self, plugin_id: &str, reason: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE wasm_plugins
             SET status = 'quarantined', quarantine_reason = ?1
             WHERE plugin_id = ?2",
                params!(reason, plugin_id),
            )
            .await?;
        Ok(())
    }

    /// Clear a quarantine so a previously-failed plugin can be re-verified
    /// from a fresh on-disk copy. Idempotent: calling on a non-quarantined
    /// row updates nothing.
    pub async fn restore_wasm_plugin(&self, plugin_id: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "UPDATE wasm_plugins
             SET status = 'verified', quarantine_reason = NULL, verified_at = ?1
             WHERE plugin_id = ?2 AND status = 'quarantined'",
                params!(now, plugin_id),
            )
            .await?;
        Ok(())
    }

    /// Persist the new local HMAC after an auto-reseal. Idempotent — only
    /// rewrites the seal column, leaving status and quarantine fields alone.
    /// Used by the v1→v2 migration path in `cortex::wasm_plugins`.
    pub async fn update_wasm_plugin_local_hmac(
        &self,
        plugin_id: &str,
        local_hmac: &str,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE wasm_plugins SET local_hmac = ?1 WHERE plugin_id = ?2",
                params!(local_hmac, plugin_id),
            )
            .await?;
        Ok(())
    }

    /// Mark a verified plugin disabled. Disabled plugins keep their install
    /// row and on-disk files but their tools are hidden from the LLM and
    /// dispatch refuses to execute them. Quarantined rows are not changed —
    /// quarantine outranks operator-disable.
    pub async fn disable_wasm_plugin(&self, plugin_id: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE wasm_plugins
             SET status = 'disabled'
             WHERE plugin_id = ?1 AND status = 'verified'",
                params!(plugin_id),
            )
            .await?;
        Ok(())
    }

    /// Move a disabled plugin back to verified. Quarantined rows must go
    /// through `restore_wasm_plugin` instead — re-enable does not silently
    /// clear a quarantine.
    pub async fn enable_wasm_plugin(&self, plugin_id: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "UPDATE wasm_plugins
             SET status = 'verified', verified_at = ?1
             WHERE plugin_id = ?2 AND status = 'disabled'",
                params!(now, plugin_id),
            )
            .await?;
        Ok(())
    }

    /// Remove all DB state for a WASM plugin. Returns true if a row existed.
    /// The on-disk install directory is NOT touched — the caller (CLI / TUI)
    /// owns filesystem cleanup so this method stays usable from contexts that
    /// don't know the `data_dir`.
    pub async fn delete_wasm_plugin(&self, plugin_id: &str) -> Result<bool> {
        let affected = self
            .conn
            .execute(
                "DELETE FROM wasm_plugins WHERE plugin_id = ?1",
                params!(plugin_id),
            )
            .await?;
        // `wasm_plugin_tools` is cleaned by the FK ON DELETE CASCADE.
        Ok(affected > 0)
    }

    /// List verified WASM tool definitions to advertise to the LLM.
    pub async fn list_verified_wasm_tool_definitions(&self) -> Result<Vec<WasmToolDefinitionRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT t.name, t.description, t.input_schema_json
             FROM wasm_plugin_tools t
             JOIN wasm_plugins p ON p.plugin_id = t.plugin_id
             WHERE p.status = 'verified'
             ORDER BY t.name",
                (),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(WasmToolDefinitionRow {
                name: row.get::<String>(0)?,
                description: row.get::<String>(1)?,
                input_schema_json: row.get::<String>(2)?,
            });
        }
        Ok(out)
    }

    /// Resolve a WASM tool name to its owning plugin_id regardless of status.
    /// Used by the kill-switch filter so a disabled-by-publisher row is still
    /// reachable for the publisher-key check.
    pub async fn plugin_id_for_tool(&self, tool_name: &str) -> Result<Option<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT plugin_id FROM wasm_plugin_tools WHERE name = ?1 LIMIT 1",
                params!(tool_name),
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(row.get::<String>(0)?)),
            None => Ok(None),
        }
    }

    /// Look up runtime metadata for a verified WASM tool.
    pub async fn get_verified_wasm_tool_runtime(
        &self,
        name: &str,
    ) -> Result<Option<WasmToolRuntimeRow>> {
        let mut rows = self.conn.query(
            "SELECT p.plugin_id, t.name, p.manifest_json, p.manifest_path, p.wasm_path, p.local_hmac
             FROM wasm_plugin_tools t
             JOIN wasm_plugins p ON p.plugin_id = t.plugin_id
             WHERE t.name = ?1 AND p.status = 'verified'
             LIMIT 1",
            params!(name),
        ).await?;
        match rows.next().await? {
            Some(row) => Ok(Some(WasmToolRuntimeRow {
                plugin_id: row.get::<String>(0)?,
                tool_name: row.get::<String>(1)?,
                manifest_json: row.get::<String>(2)?,
                manifest_path: row.get::<String>(3)?,
                wasm_path: row.get::<String>(4)?,
                local_hmac: row.get::<String>(5)?,
            })),
            None => Ok(None),
        }
    }
}
