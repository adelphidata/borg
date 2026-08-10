use anyhow::Result;
use libsql::params;

use super::Database;

impl Database {
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
                // libsql's `Statement` holds binding/execution state between
                // calls — without `reset()`, only the first iteration's
                // INSERT actually fires (the rest are silently no-op'd).
                // See: every subsequent settings row would otherwise be
                // missing on first daemon start.
                stmt.reset();
                stmt.execute(params!(key, value, now)).await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        self.finish_immediate_tx(result, "settings ensure_all")
            .await
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_setting_returns_none_not_error() {
        // Spec: a never-set key returns Ok(None). A regression that
        // returned Err would force every caller to wrap in `or_else`
        // and obscure real errors.
        let db = Database::test_db().await;
        let v = db
            .get_setting("no.such.key.exists")
            .await
            .expect("query ok");
        assert_eq!(v, None);
    }

    #[tokio::test]
    async fn set_get_round_trips_value() {
        let db = Database::test_db().await;
        db.set_setting("test.key.alpha", "alpha-value")
            .await
            .unwrap();
        let v = db.get_setting("test.key.alpha").await.unwrap();
        assert_eq!(v.as_deref(), Some("alpha-value"));
    }

    #[tokio::test]
    async fn set_overwrites_existing_value_via_upsert() {
        // Real-world failure: a refactor swapping ON CONFLICT for plain
        // INSERT would silently make set_setting fail-on-update and the
        // operator's `borg set` would have no effect on existing keys.
        let db = Database::test_db().await;
        db.set_setting("test.key.beta", "first").await.unwrap();
        db.set_setting("test.key.beta", "second").await.unwrap();
        assert_eq!(
            db.get_setting("test.key.beta").await.unwrap().as_deref(),
            Some("second")
        );
    }

    #[tokio::test]
    async fn set_setting_advances_updated_at_on_overwrite() {
        // The updated_at column is used by the watcher to detect changes.
        // A regression that left updated_at frozen on overwrite would
        // break live-reload.
        let db = Database::test_db().await;
        db.set_setting("test.key.ts", "v1").await.unwrap();
        let first = db
            .list_settings()
            .await
            .unwrap()
            .into_iter()
            .find(|(k, _, _)| k == "test.key.ts")
            .expect("seeded")
            .2;
        // Sleep at least one second so the second timestamp must differ —
        // updated_at is recorded with second granularity.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        db.set_setting("test.key.ts", "v2").await.unwrap();
        let second = db
            .list_settings()
            .await
            .unwrap()
            .into_iter()
            .find(|(k, _, _)| k == "test.key.ts")
            .expect("seeded")
            .2;
        assert!(
            second > first,
            "updated_at must advance: first={first} second={second}"
        );
    }

    #[tokio::test]
    async fn delete_setting_returns_true_when_present_false_when_absent() {
        // Idempotent-delete contract. The `borg unset <key>` flow relies
        // on the bool to distinguish "actually removed something" from
        // "key was already absent" for user feedback.
        let db = Database::test_db().await;
        db.set_setting("test.key.gamma", "x").await.unwrap();
        assert!(db.delete_setting("test.key.gamma").await.unwrap());
        // Second delete: nothing to remove.
        assert!(!db.delete_setting("test.key.gamma").await.unwrap());
        // And the key really is gone.
        assert_eq!(db.get_setting("test.key.gamma").await.unwrap(), None);
    }

    #[tokio::test]
    async fn list_settings_returns_rows_sorted_by_key() {
        // The ORDER BY contract is load-bearing for the `borg settings`
        // CLI output, which formats the rows verbatim. A regression to
        // un-sorted output would scramble the user-facing display.
        let db = Database::test_db().await;
        db.set_setting("zzz.last.key", "z").await.unwrap();
        db.set_setting("aaa.first.key", "a").await.unwrap();
        db.set_setting("mmm.middle.key", "m").await.unwrap();
        let rows = db.list_settings().await.unwrap();
        let observed_keys: Vec<&String> = rows.iter().map(|(k, _, _)| k).collect();
        let mut sorted = observed_keys.clone();
        sorted.sort();
        assert_eq!(
            observed_keys, sorted,
            "list_settings must return rows ordered by key"
        );
    }

    #[tokio::test]
    async fn ensure_all_settings_seeds_defaults_without_overwriting_existing() {
        // Spec: re-seeding is INSERT OR IGNORE. A user-set value must
        // survive a daemon restart that calls ensure_all_settings again.
        // (A swap to plain INSERT — or any "overwrite to default" tweak —
        // would silently reset every operator-customized value.)
        let db = Database::test_db().await;
        // Pick a real registered key so the seeded default is well-defined.
        use crate::settings::SETTING_REGISTRY;
        let key = SETTING_REGISTRY
            .first()
            .expect("registry has at least one entry")
            .0;
        db.set_setting(key, "user-customized-value").await.unwrap();
        db.ensure_all_settings().await.unwrap();
        assert_eq!(
            db.get_setting(key).await.unwrap().as_deref(),
            Some("user-customized-value"),
            "ensure_all_settings must not overwrite a user-set value"
        );
    }

    #[tokio::test]
    async fn ensure_all_settings_populates_every_registered_key() {
        // Each entry in the registry must end up as a row after the
        // first ensure_all_settings() call. A missing key here means a
        // setting was added without the corresponding default extractor
        // — silently undefined behavior at runtime.
        use crate::settings::SETTING_REGISTRY;
        let db = Database::test_db().await;
        db.ensure_all_settings().await.unwrap();
        let mut missing: Vec<&str> = Vec::new();
        for &(key, _) in SETTING_REGISTRY.iter() {
            if db.get_setting(key).await.unwrap().is_none() {
                missing.push(key);
            }
        }
        assert!(
            missing.is_empty(),
            "ensure_all_settings did not seed: {missing:?}"
        );
    }

    #[tokio::test]
    async fn data_version_is_stable_between_reads_on_same_connection() {
        // Spec: data_version is a *connection-local* counter that tracks
        // *other* connections' commits. On the same connection, repeated
        // reads with no writes from other handles MUST return the same
        // value — otherwise the watcher poll would always think "changed"
        // and reload the entire settings table every tick.
        let db = Database::test_db().await;
        let v0 = db.data_version().await.unwrap();
        let v1 = db.data_version().await.unwrap();
        assert_eq!(
            v0, v1,
            "data_version must be stable across reads on same connection: {v0} vs {v1}"
        );
    }
}
