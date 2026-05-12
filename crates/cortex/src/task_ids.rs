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
