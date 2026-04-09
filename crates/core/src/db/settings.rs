use anyhow::Result;
use rusqlite::{params, OptionalExtension};

use super::Database;

impl Database {
    // ── Settings CRUD ──

    /// Ensure every key in SETTING_REGISTRY has a row in the settings table.
    /// Missing keys are inserted with their compiled default value.
    /// Existing rows are never overwritten (`INSERT OR IGNORE` on PRIMARY KEY).
    /// Wrapped in a transaction for atomicity and first-run performance.
    pub fn ensure_all_settings(&self) -> Result<()> {
        use crate::config::Config;
        use crate::settings::SETTING_REGISTRY;

        let defaults = Config::default();
        let now = chrono::Utc::now().timestamp();

        self.conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<()> {
            let mut stmt = self.conn.prepare(
                "INSERT OR IGNORE INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)",
            )?;
            for &(key, extractor) in SETTING_REGISTRY.iter() {
                let value = extractor(&defaults);
                stmt.execute(params![key, value, now])?;
            }
            Ok(())
        })();

        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Returns SQLite's `data_version` counter, which increments whenever any
    /// connection (including other processes) modifies the database.
    /// This is an in-memory check — no disk I/O.
    ///
    /// Note: in WAL mode, `data_version` only reflects changes visible to this
    /// connection. A long-lived read transaction (snapshot) will not see bumps
    /// from other writers. The config watcher poll loop does not hold read
    /// transactions between polls, so this works correctly there.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA data_version", [], |row| row.get(0))?)
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM settings WHERE key = ?1")?;
        let value = stmt
            .query_row(params![key], |row| row.get::<_, String>(0))
            .optional()?;
        Ok(value)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = ?2, updated_at = ?3",
            params![key, value, now],
        )?;
        Ok(())
    }

    pub fn delete_setting(&self, key: &str) -> Result<bool> {
        let count = self
            .conn
            .execute("DELETE FROM settings WHERE key = ?1", params![key])?;
        Ok(count > 0)
    }

    pub fn list_settings(&self) -> Result<Vec<(String, String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT key, value, updated_at FROM settings ORDER BY key")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
