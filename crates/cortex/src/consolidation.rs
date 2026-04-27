//! Memory consolidation pipeline.
//!
//! Nightly and weekly scheduled tasks consolidate session data and long-term
//! memory entries. The tasks themselves are LLM-driven (seeded in V34 migration)
//! with tool access to `write_memory`, `read_memory`, and `memory_search`.
//!
//! This module provides constants and helper functions for the consolidation
//! system, including session-end flushing of short-term memory.

use anyhow::Result;

/// Fixed UUID for the nightly memory consolidation task (seeded in V34 migration).
pub const NIGHTLY_CONSOLIDATION_TASK_ID: &str = "00000000-0000-4000-8000-c005011d0001";

/// Fixed UUID for the weekly memory maintenance task (seeded in V34 migration).
pub const WEEKLY_CONSOLIDATION_TASK_ID: &str = "00000000-0000-4000-8000-c005011d0002";

/// Settings key for tracking the last successful nightly consolidation run.
pub const SETTING_LAST_NIGHTLY: &str = "consolidation.last_nightly";

/// Settings key for tracking the last successful weekly consolidation run.
pub const SETTING_LAST_WEEKLY: &str = "consolidation.last_weekly";

/// Flush short-term memory facts to a daily log entry in the DB.
///
/// Called on session end. Creates or appends to a `daily/{YYYY-MM-DD}` entry
/// in the `memory_entries` table. The nightly consolidation job processes
/// these daily entries into long-term topic entries.
pub fn flush_short_term_to_daily(facts_text: &str) -> Result<()> {
    if facts_text.trim().is_empty() {
        return Ok(());
    }
    let db = crate::db::Database::open()?;
    flush_short_term_to_daily_with_db(&db, facts_text)
}

/// Flush using the caller's existing DB handle when one is available,
/// otherwise open a fresh connection. Intended as the single entry point from
/// `Agent::flush_short_term_memory` so session-end flushes reuse the agent's
/// already-open database instead of racing a new connection against any
/// in-progress writes on the agent's handle.
pub fn flush_short_term_to_daily_with_optional_db(
    db: Option<&crate::db::Database>,
    facts_text: &str,
) -> Result<()> {
    if facts_text.trim().is_empty() {
        return Ok(());
    }
    match db {
        Some(db) => flush_short_term_to_daily_with_db(db, facts_text),
        None => flush_short_term_to_daily(facts_text),
    }
}

/// Same as [`flush_short_term_to_daily`] but uses a caller-provided database
/// handle. Split out primarily so tests can run against an in-memory DB.
pub fn flush_short_term_to_daily_with_db(db: &crate::db::Database, facts_text: &str) -> Result<()> {
    if facts_text.trim().is_empty() {
        return Ok(());
    }

    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let name = format!("daily/{date}");
    let header = format!(
        "\n## Session flush ({})\n",
        chrono::Local::now().format("%H:%M")
    );
    let content = format!("{header}{facts_text}");

    db.append_memory_entry("global", &name, &content)?;
    tracing::debug!("Flushed short-term memory to {name}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_empty_is_noop() {
        assert!(flush_short_term_to_daily("").is_ok());
        assert!(flush_short_term_to_daily("   ").is_ok());
    }

    #[test]
    fn constants_are_valid_uuids() {
        assert_eq!(NIGHTLY_CONSOLIDATION_TASK_ID.len(), 36);
        assert_eq!(WEEKLY_CONSOLIDATION_TASK_ID.len(), 36);
    }

    #[test]
    fn flush_writes_to_daily_entry() {
        // Use in-memory DB for this test
        let db = crate::db::Database::test_db();
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let name = format!("daily/{date}");

        db.append_memory_entry("global", &name, "- [Decision] test fact")
            .unwrap();

        let entry = db.get_memory_entry("global", &name).unwrap().unwrap();
        assert!(entry.content.contains("test fact"));
    }

    #[test]
    fn flush_with_db_creates_and_appends_daily_entry() {
        let db = crate::db::Database::test_db();
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let name = format!("daily/{date}");

        // First flush — creates the entry.
        flush_short_term_to_daily_with_db(&db, "- [Decision] chose rust").unwrap();
        let first = db.get_memory_entry("global", &name).unwrap().unwrap();
        assert!(first.content.contains("chose rust"));
        assert!(
            first.content.contains("Session flush"),
            "flush should be marked with a header"
        );

        // Second flush same day — must append, not overwrite (T8 regression).
        flush_short_term_to_daily_with_db(&db, "- [Correction] use snake_case").unwrap();
        let second = db.get_memory_entry("global", &name).unwrap().unwrap();
        assert!(
            second.content.contains("chose rust"),
            "first flush's fact must still be present after second flush"
        );
        assert!(
            second.content.contains("use snake_case"),
            "second flush's fact must be added"
        );
        assert!(
            second.content.len() > first.content.len(),
            "second flush must grow the entry (len {} vs {})",
            second.content.len(),
            first.content.len(),
        );
    }

    #[test]
    fn optional_db_routes_through_provided_handle() {
        // When the agent has an open DB handle, the flush must use it — not
        // open a fresh `Database::open()` that would race against the agent's
        // in-flight writes and (in tests) miss the in-memory DB entirely.
        let db = crate::db::Database::test_db();
        flush_short_term_to_daily_with_optional_db(Some(&db), "- [Decision] use Rust").unwrap();

        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let entry = db
            .get_memory_entry("global", &format!("daily/{date}"))
            .unwrap()
            .expect("entry must be written through the injected db handle");
        assert!(entry.content.contains("use Rust"));
    }

    #[test]
    fn optional_db_empty_facts_are_noop_with_or_without_db() {
        let db = crate::db::Database::test_db();
        assert!(flush_short_term_to_daily_with_optional_db(Some(&db), "").is_ok());
        assert!(flush_short_term_to_daily_with_optional_db(Some(&db), "   \n").is_ok());
        assert!(flush_short_term_to_daily_with_optional_db(None, "").is_ok());
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert!(
            db.get_memory_entry("global", &format!("daily/{date}"))
                .unwrap()
                .is_none(),
            "empty facts must not create a daily entry"
        );
    }

    #[test]
    fn flush_short_term_memory_integration_with_facts_as_text() {
        use crate::short_term_memory::{FactCategory, ShortTermMemory};

        let db = crate::db::Database::test_db();
        let mut stm = ShortTermMemory::new("sess-test".into(), 2000);
        stm.add_fact(FactCategory::Decision, "agreed on PG over MySQL".into(), 0);
        stm.add_fact(
            FactCategory::TaskOutcome,
            "migration script shipped".into(),
            1,
        );

        // This is the actual wiring F1 relies on: facts → text → DB daily entry.
        flush_short_term_to_daily_with_db(&db, &stm.facts_as_text()).unwrap();

        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let entry = db
            .get_memory_entry("global", &format!("daily/{date}"))
            .unwrap()
            .unwrap();
        assert!(entry.content.contains("PG over MySQL"));
        assert!(entry.content.contains("migration script shipped"));
    }
}
