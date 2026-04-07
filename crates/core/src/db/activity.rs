use anyhow::Result;
use rusqlite::params;

use super::models::ActivityEntry;
use super::Database;

impl Database {
    /// Insert an activity log entry with the current unix timestamp.
    pub fn log_activity(
        &self,
        level: &str,
        category: &str,
        message: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO activity_log (level, category, message, detail, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![level, category, message, detail, now],
        )?;
        Ok(())
    }

    /// Query activity log entries, ordered by created_at DESC.
    ///
    /// `min_level` filters by level hierarchy: error > warn > info > debug.
    /// If `None`, defaults to info+ (error, warn, info).
    /// `category` optionally filters to a single category.
    pub fn query_activity(
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

        let (sql, params_vec): (String, Vec<Box<dyn rusqlite::types::ToSql>>) =
            if let Some(cat) = category {
                let param_idx = levels.len() + 1;
                let count_idx = levels.len() + 2;
                (
                    format!(
                        "SELECT id, level, category, message, detail, created_at
                     FROM activity_log
                     WHERE {level_clause} AND category = ?{param_idx}
                     ORDER BY created_at DESC
                     LIMIT ?{count_idx}"
                    ),
                    {
                        let mut p: Vec<Box<dyn rusqlite::types::ToSql>> = levels
                            .iter()
                            .map(|l| Box::new(l.to_string()) as _)
                            .collect();
                        p.push(Box::new(cat.to_string()));
                        p.push(Box::new(count as i64));
                        p
                    },
                )
            } else {
                let count_idx = levels.len() + 1;
                (
                    format!(
                        "SELECT id, level, category, message, detail, created_at
                     FROM activity_log
                     WHERE {level_clause}
                     ORDER BY created_at DESC
                     LIMIT ?{count_idx}"
                    ),
                    {
                        let mut p: Vec<Box<dyn rusqlite::types::ToSql>> = levels
                            .iter()
                            .map(|l| Box::new(l.to_string()) as _)
                            .collect();
                        p.push(Box::new(count as i64));
                        p
                    },
                )
            };

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| &**p).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok(ActivityEntry {
                id: row.get(0)?,
                level: row.get(1)?,
                category: row.get(2)?,
                message: row.get(3)?,
                detail: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;

        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(entries)
    }

    /// Delete activity log entries older than the given cutoff timestamp.
    /// Returns the number of deleted rows.
    pub fn prune_activity_before(&self, cutoff_timestamp: i64) -> Result<usize> {
        let deleted = self.conn.execute(
            "DELETE FROM activity_log WHERE created_at < ?1",
            params![cutoff_timestamp],
        )?;
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn test_db() -> Database {
        Database::from_connection(Connection::open_in_memory().unwrap()).unwrap()
    }

    #[test]
    fn test_log_and_query_activity() {
        let db = test_db();
        db.log_activity(
            "info",
            "migrate",
            "Migration complete: 2 config change(s) applied",
            None,
        )
        .unwrap();
        db.log_activity("error", "migrate", "Migration failed: timeout", None)
            .unwrap();
        db.log_activity(
            "info",
            "gateway",
            "Gateway listening on 127.0.0.1:7842",
            None,
        )
        .unwrap();

        // Query all info+ entries
        let entries = db.query_activity(50, Some("info"), None).unwrap();
        assert_eq!(entries.len(), 3);

        // Query filtered by category
        let migrate_entries = db
            .query_activity(50, Some("info"), Some("migrate"))
            .unwrap();
        assert_eq!(migrate_entries.len(), 2);
        assert!(migrate_entries.iter().all(|e| e.category == "migrate"));

        // Query errors only — should get only the migrate error
        let errors = db.query_activity(50, Some("error"), None).unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message, "Migration failed: timeout");
    }

    #[test]
    fn test_log_activity_with_detail() {
        let db = test_db();
        db.log_activity(
            "info",
            "migrate",
            "Migration complete",
            Some("2 config change(s), 1 credential(s)"),
        )
        .unwrap();

        let entries = db.query_activity(1, Some("info"), Some("migrate")).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].detail.as_deref(),
            Some("2 config change(s), 1 credential(s)")
        );
    }

    #[test]
    fn test_prune_activity() {
        let db = test_db();
        db.log_activity("info", "migrate", "old entry", None)
            .unwrap();

        let now = chrono::Utc::now().timestamp();
        let pruned = db.prune_activity_before(now + 1).unwrap();
        assert_eq!(pruned, 1);

        let entries = db.query_activity(50, Some("info"), None).unwrap();
        assert!(entries.is_empty());
    }
}
