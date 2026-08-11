//! Fixed UUIDs for built-in seeded `scheduled_tasks` rows.
//!
//! All seeded task UUIDs live here so audits and renames are local to one
//! file. Each owning module re-exports its own ID under the original public
//! path so existing imports keep working.

/// Monthly security audit task (seeded by `seed_default_tasks`).
pub const SECURITY_AUDIT_TASK_ID: &str = "00000000-0000-4000-8000-5ec041700001";

/// Daily standup summary task (seeded by `seed_default_tasks`).
pub const DAILY_SUMMARY_TASK_ID: &str = "00000000-0000-4000-8000-da1175000002";

/// Nightly memory consolidation task (seeded by `seed_consolidation_tasks`).
pub const NIGHTLY_CONSOLIDATION_TASK_ID: &str = "00000000-0000-4000-8000-c005011d0001";

/// Weekly memory maintenance task (seeded by `seed_consolidation_tasks`).
pub const WEEKLY_CONSOLIDATION_TASK_ID: &str = "00000000-0000-4000-8000-c005011d0002";

/// Daily self-healing maintenance sweep (seeded by `seed_maintenance_task`).
pub const MAINTENANCE_TASK_ID: &str = "00000000-0000-4000-8000-ada1ca1e0001";

/// Heartbeat row (seeded by `seed_heartbeat_task`).
pub const HEARTBEAT_TASK_ID: &str = "00000000-0000-4000-8000-5ec041700002";

/// Every seeded row, in one place. Anything that asks "is this the system's
/// task or the user's?" reads this list, so a newly seeded row cannot be
/// system-owned in one check and user-owned in another.
pub const SYSTEM_TASK_IDS: &[&str] = &[
    SECURITY_AUDIT_TASK_ID,
    DAILY_SUMMARY_TASK_ID,
    NIGHTLY_CONSOLIDATION_TASK_ID,
    WEEKLY_CONSOLIDATION_TASK_ID,
    MAINTENANCE_TASK_ID,
    HEARTBEAT_TASK_ID,
];

/// True when `task_id` names a seeded system row.
///
/// System rows are re-seeded on migration and are configured through their
/// own `*.enabled` settings, so they are exempt from user-facing advice
/// ("this task has no delivery channel") and from destructive tool actions.
pub fn is_system_task_id(task_id: &str) -> bool {
    SYSTEM_TASK_IDS.contains(&task_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_table_covers_every_row_the_migrations_seed() {
        // Read the answer from the database rather than from a second copy of
        // the list: a `seed_*_task` added without touching `SYSTEM_TASK_IDS`
        // fails here, which is the failure worth catching. A seeded row this
        // predicate misses is treated as the user's — deletable through the
        // `schedule` tool, and nagged about by doctor.
        let db = crate::db::Database::test_db().await;
        let seeded = db.list_tasks().await.expect("list seeded tasks");
        assert!(
            !seeded.is_empty(),
            "migrations must seed the system rows, or this test proves nothing"
        );
        for task in &seeded {
            assert!(
                is_system_task_id(&task.id),
                "seeded task '{}' ({}) is missing from SYSTEM_TASK_IDS",
                task.name,
                task.id
            );
        }
        // And an unrelated id must not be mistaken for one, which would make a
        // user's own task undeletable.
        assert!(!is_system_task_id("00000000-0000-4000-8000-000000000000"));
        assert!(!is_system_task_id(""));
    }

    #[test]
    fn the_seeded_ids_are_all_distinct() {
        // A copy-paste collision would make two seeded rows share a primary
        // key, so the second seed silently overwrites the first.
        let mut seen = std::collections::HashSet::new();
        for id in SYSTEM_TASK_IDS {
            assert!(seen.insert(*id), "duplicate seeded task id {id}");
        }
    }
}
