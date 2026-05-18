use anyhow::Result;
use libsql::params;

use super::models::ActivityEntry;
use super::Database;

impl Database {
    /// Insert an activity log entry with the current unix timestamp.
    pub async fn log_activity(
        &self,
        level: &str,
        category: &str,
        message: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "INSERT INTO activity_log (level, category, message, detail, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params!(
                    level,
                    category,
                    message,
                    detail.map(std::string::ToString::to_string),
                    now
                ),
            )
            .await?;
        Ok(())
    }

    /// Query activity log entries, ordered by created_at DESC.
    ///
    /// `min_level` filters by level hierarchy: error > warn > info > debug.
    /// If `None`, defaults to info+ (error, warn, info).
    /// `category` optionally filters to a single category.
    pub async fn query_activity(
        &self,
        count: usize,
        min_level: Option<&str>,
        category: Option<&str>,
    ) -> Result<Vec<ActivityEntry>> {
        let levels = match min_level {
            Some("error") => vec!["error"],
            Some("warn") => vec!["error", "warn"],
            Some("info") | None => vec!["error", "warn", "info"],
            Some("debug") | Some("all") => vec!["error", "warn", "info", "debug"],
            Some(_) => vec!["error", "warn", "info"],
        };

        let placeholders: Vec<String> = (1..=levels.len()).map(|i| format!("?{i}")).collect();
        let level_clause = format!("level IN ({})", placeholders.join(", "));

        let mut values: Vec<libsql::Value> = levels
            .iter()
            .map(|l| libsql::Value::Text((*l).to_string()))
            .collect();
        let sql = if let Some(cat) = category {
            let param_idx = levels.len() + 1;
            let count_idx = levels.len() + 2;
            values.push(libsql::Value::Text(cat.to_string()));
            values.push(libsql::Value::Integer(count as i64));
            format!(
                "SELECT id, level, category, message, detail, created_at
                 FROM activity_log
                 WHERE {level_clause} AND category = ?{param_idx}
                 ORDER BY created_at DESC
                 LIMIT ?{count_idx}"
            )
        } else {
            let count_idx = levels.len() + 1;
            values.push(libsql::Value::Integer(count as i64));
            format!(
                "SELECT id, level, category, message, detail, created_at
                 FROM activity_log
                 WHERE {level_clause}
                 ORDER BY created_at DESC
                 LIMIT ?{count_idx}"
            )
        };

        let stmt = self.conn.prepare(&sql).await?;
        let mut rows = stmt.query(values).await?;
        let mut entries = Vec::new();
        while let Some(row) = rows.next().await? {
            entries.push(ActivityEntry {
                id: row.get::<i64>(0)?,
                level: row.get::<String>(1)?,
                category: row.get::<String>(2)?,
                message: row.get::<String>(3)?,
                detail: row.get::<Option<String>>(4)?,
                created_at: row.get::<i64>(5)?,
            });
        }
        Ok(entries)
    }

    /// Delete activity log entries older than the given cutoff timestamp.
    /// Returns the number of deleted rows.
    pub async fn prune_activity_before(&self, cutoff_timestamp: i64) -> Result<usize> {
        let deleted = self
            .conn
            .execute(
                "DELETE FROM activity_log WHERE created_at < ?1",
                params!(cutoff_timestamp),
            )
            .await?;
        Ok(deleted as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> Database {
        Database::test_db().await
    }

    #[tokio::test]
    async fn test_log_and_query_activity() {
        let db = test_db().await;
        db.log_activity(
            "info",
            "migrate",
            "Migration complete: 2 config change(s) applied",
            None,
        )
        .await
        .unwrap();
        db.log_activity("error", "migrate", "Migration failed: timeout", None)
            .await
            .unwrap();
        db.log_activity(
            "info",
            "gateway",
            "Gateway listening on 127.0.0.1:7842",
            None,
        )
        .await
        .unwrap();

        let entries = db.query_activity(50, Some("info"), None).await.unwrap();
        assert_eq!(entries.len(), 3);

        let migrate_entries = db
            .query_activity(50, Some("info"), Some("migrate"))
            .await
            .unwrap();
        assert_eq!(migrate_entries.len(), 2);
        assert!(migrate_entries.iter().all(|e| e.category == "migrate"));

        let errors = db.query_activity(50, Some("error"), None).await.unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message, "Migration failed: timeout");
    }

    #[tokio::test]
    async fn test_log_activity_with_detail() {
        let db = test_db().await;
        db.log_activity(
            "info",
            "migrate",
            "Migration complete",
            Some("2 config change(s), 1 credential(s)"),
        )
        .await
        .unwrap();

        let entries = db
            .query_activity(1, Some("info"), Some("migrate"))
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].detail.as_deref(),
            Some("2 config change(s), 1 credential(s)")
        );
    }

    #[tokio::test]
    async fn test_prune_activity() {
        let db = test_db().await;
        db.log_activity("info", "migrate", "old entry", None)
            .await
            .unwrap();

        let now = chrono::Utc::now().timestamp();
        let pruned = db.prune_activity_before(now + 1).await.unwrap();
        assert_eq!(pruned, 1);

        let entries = db.query_activity(50, Some("info"), None).await.unwrap();
        assert!(entries.is_empty());
    }
}
