use anyhow::Result;
use rusqlite::{params, OptionalExtension};

use super::Database;

/// Outcome of recording one observed user skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillAuditOutcome {
    /// This skill name has not been seen before — its hash is now recorded.
    FirstSeen,
    /// The stored hash matches the observed one.
    Unchanged,
    /// The stored hash differs from the observed one; the stored hash has
    /// been updated to the new value. `prev_sha256` is the hash we had
    /// before this observation.
    Modified { prev_sha256: String },
}

/// One row from the `skill_audit` table.
#[derive(Debug, Clone)]
pub struct SkillAuditRow {
    pub name: String,
    pub sha256: String,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
}

impl Database {
    /// Record that a user skill was observed with the given content hash.
    /// Returns an outcome describing whether the hash is new, unchanged,
    /// or diverged from a prior observation.
    ///
    /// Always updates `last_seen_at`. For a divergent hash, updates
    /// `sha256` as well so the next call with the same content reports
    /// `Unchanged`.
    pub fn record_skill_seen(&self, name: &str, sha256: &str) -> Result<SkillAuditOutcome> {
        let now = chrono::Utc::now().timestamp();
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT sha256 FROM skill_audit WHERE name = ?1",
                params![name],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        match existing {
            None => {
                self.conn.execute(
                    "INSERT INTO skill_audit (name, sha256, first_seen_at, last_seen_at)
                     VALUES (?1, ?2, ?3, ?3)",
                    params![name, sha256, now],
                )?;
                Ok(SkillAuditOutcome::FirstSeen)
            }
            Some(prev) if prev == sha256 => {
                self.conn.execute(
                    "UPDATE skill_audit SET last_seen_at = ?1 WHERE name = ?2",
                    params![now, name],
                )?;
                Ok(SkillAuditOutcome::Unchanged)
            }
            Some(prev) => {
                self.conn.execute(
                    "UPDATE skill_audit SET sha256 = ?1, last_seen_at = ?2 WHERE name = ?3",
                    params![sha256, now, name],
                )?;
                Ok(SkillAuditOutcome::Modified { prev_sha256: prev })
            }
        }
    }

    /// List every row in `skill_audit`, ordered by name.
    pub fn list_skill_audit(&self) -> Result<Vec<SkillAuditRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, sha256, first_seen_at, last_seen_at
             FROM skill_audit ORDER BY name",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(SkillAuditRow {
                    name: row.get(0)?,
                    sha256: row.get(1)?,
                    first_seen_at: row.get(2)?,
                    last_seen_at: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Remove a skill from the audit table (used when a skill is
    /// uninstalled so the next reinstall is treated as first-seen).
    pub fn forget_skill_audit(&self, name: &str) -> Result<bool> {
        let count = self
            .conn
            .execute("DELETE FROM skill_audit WHERE name = ?1", params![name])?;
        Ok(count > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::test_db()
    }

    #[test]
    fn record_skill_seen_first_time_inserts_row() {
        let db = db();
        let outcome = db.record_skill_seen("my-skill", "aaaa").unwrap();
        assert_eq!(outcome, SkillAuditOutcome::FirstSeen);

        let rows = db.list_skill_audit().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "my-skill");
        assert_eq!(rows[0].sha256, "aaaa");
        // first_seen_at == last_seen_at on initial insert
        assert_eq!(rows[0].first_seen_at, rows[0].last_seen_at);
    }

    #[test]
    fn record_skill_seen_unchanged_updates_last_seen_only() {
        let db = db();
        db.record_skill_seen("my-skill", "aaaa").unwrap();
        let first_ts = db.list_skill_audit().unwrap()[0].first_seen_at;

        // Sleep 1s so last_seen_at can advance (timestamps are second-resolution).
        std::thread::sleep(std::time::Duration::from_secs(1));

        let outcome = db.record_skill_seen("my-skill", "aaaa").unwrap();
        assert_eq!(outcome, SkillAuditOutcome::Unchanged);

        let row = &db.list_skill_audit().unwrap()[0];
        assert_eq!(row.sha256, "aaaa");
        assert_eq!(row.first_seen_at, first_ts, "first_seen_at must be stable");
        assert!(
            row.last_seen_at > first_ts,
            "last_seen_at must advance on re-observation (first={first_ts}, last={})",
            row.last_seen_at,
        );
    }

    #[test]
    fn record_skill_seen_modified_returns_previous_hash() {
        let db = db();
        db.record_skill_seen("my-skill", "aaaa").unwrap();
        let first_ts = db.list_skill_audit().unwrap()[0].first_seen_at;

        let outcome = db.record_skill_seen("my-skill", "bbbb").unwrap();
        assert_eq!(
            outcome,
            SkillAuditOutcome::Modified {
                prev_sha256: "aaaa".to_string()
            }
        );

        let row = &db.list_skill_audit().unwrap()[0];
        assert_eq!(row.sha256, "bbbb");
        assert_eq!(
            row.first_seen_at, first_ts,
            "first_seen_at must be preserved across modifications"
        );
    }

    #[test]
    fn forget_skill_audit_removes_row() {
        let db = db();
        db.record_skill_seen("my-skill", "aaaa").unwrap();
        assert!(db.forget_skill_audit("my-skill").unwrap());
        assert!(db.list_skill_audit().unwrap().is_empty());
        // second forget is a no-op
        assert!(!db.forget_skill_audit("my-skill").unwrap());
    }

    #[test]
    fn forget_does_not_reset_first_seen_on_reinsert() {
        // After `forget`, a re-observation should be treated as FirstSeen,
        // not as an unchanged continuation — this is the contract that
        // lets uninstall/reinstall cleanly reset audit state.
        let db = db();
        db.record_skill_seen("my-skill", "aaaa").unwrap();
        db.forget_skill_audit("my-skill").unwrap();
        let outcome = db.record_skill_seen("my-skill", "aaaa").unwrap();
        assert_eq!(outcome, SkillAuditOutcome::FirstSeen);
    }
}
