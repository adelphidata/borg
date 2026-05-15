use anyhow::Result;
use turso::params;

use super::Database;

impl Database {
    // ── Meta key-value ──

    pub async fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let mut rows = self
            .conn
            .query("SELECT value FROM meta WHERE key = ?1", params!(key))
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(row.get::<String>(0)?)),
            None => Ok(None),
        }
    }

    pub async fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = ?2",
                params!(key, value),
            )
            .await?;
        Ok(())
    }

    pub async fn delete_meta(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM meta WHERE key = ?1", params!(key))
            .await?;
        Ok(())
    }
}
