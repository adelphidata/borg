use anyhow::Result;
use libsql::params;

use super::Database;

impl Database {
    // ── Settings CRUD ──

    /// Ensure every key in SETTING_REGISTRY has a row in the settings table.
    /// Missing keys are inserted with their compiled default value.
    /// Existing rows are never overwritten (`INSERT OR IGNORE` on PRIMARY KEY).
    /// Wrapped in a transaction for atomicity and first-run performance.
    pub async fn ensure_all_settings(&self) -> Result<()> {
        use crate::config::Config;
        use crate::settings::SETTING_REGISTRY;

        let defaults = Config::default();
        let now = chrono::Utc::now().timestamp();

        self.conn.execute_batch("BEGIN IMMEDIATE").await?;
        let result = async {
            let stmt = self
                .conn
                .prepare(
                    "INSERT OR IGNORE INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)",
                )
                .await?;
            for &(key, extractor) in SETTING_REGISTRY.iter() {
                let value = extractor(&defaults);
                stmt.execute(params!(key, value, now)).await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT").await?;
                Ok(())
            }
            Err(e) => {
                if let Err(rb) = self.conn.execute_batch("ROLLBACK").await {
                    tracing::warn!("settings rollback failed: {rb} (original: {e})");
                }
                Err(e)
            }
        }
    }

    /// Returns SQLite's `PRAGMA data_version` — a connection-local counter
    /// that bumps whenever another connection commits a write to the
    /// database. Used by the config watcher to detect cross-process settings
    /// changes without reloading the full table every poll.
    ///
    /// `data_version` is per-database, not per-table — any write to `borg.db`
    /// from another connection will shift the counter, which is fine for the
    /// watcher (a wasted poll is cheaper than missing a real settings change).
    pub async fn data_version(&self) -> Result<i64> {
        let mut rows = self.conn.query("PRAGMA data_version", ()).await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("PRAGMA data_version returned no rows"))?;
        Ok(row.get::<i64>(0)?)
    }

    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let mut rows = self
            .conn
            .query("SELECT value FROM settings WHERE key = ?1", params!(key))
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(row.get::<String>(0)?)),
            None => Ok(None),
        }
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET value = ?2, updated_at = ?3",
                params!(key, value, now),
            )
            .await?;
        Ok(())
    }

    pub async fn delete_setting(&self, key: &str) -> Result<bool> {
        let count = self
            .conn
            .execute("DELETE FROM settings WHERE key = ?1", params!(key))
            .await?;
        Ok(count > 0)
    }

    pub async fn list_settings(&self) -> Result<Vec<(String, String, i64)>> {
        let stmt = self
            .conn
            .prepare("SELECT key, value, updated_at FROM settings ORDER BY key")
            .await?;
        let mut rows = stmt.query(()).await?;
        let mut out: Vec<(String, String, i64)> = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push((
                row.get::<String>(0)?,
                row.get::<String>(1)?,
                row.get::<i64>(2)?,
            ));
        }
        Ok(out)
    }
}
