//! Structured activity log — captures key system events for `/logs` and `borg logs --activity`.
//!
//! Events are stored in SQLite via `Database::log_activity()` with level/category filtering.
//! `ActivityHook` records agent lifecycle events; convenience functions allow other subsystems
//! (daemon, gateway) to log events directly.

use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;

use crate::db::Database;
use crate::hooks::{Hook, HookAction, HookContext, HookData, HookPoint};

/// Lifecycle hook that records agent events to the activity log.
/// `tokio::Mutex` lets the hook hold the DB lock across an `.await` —
/// turso's connection is `Send + Sync` and cheaply cloneable, so we
/// only serialize the single-write-per-event critical section.
pub struct ActivityHook {
    db: AsyncMutex<Database>,
}

impl ActivityHook {
    pub async fn new() -> anyhow::Result<Self> {
        Ok(Self {
            db: AsyncMutex::new(Database::open().await?),
        })
    }
}

#[async_trait]
impl Hook for ActivityHook {
    fn name(&self) -> &str {
        "activity"
    }

    fn points(&self) -> &[HookPoint] {
        &[HookPoint::SessionStart]
    }

    async fn execute(&self, ctx: &HookContext) -> HookAction {
        let db = self.db.lock().await;
        if let HookData::SessionStart { .. } = &ctx.data {
            if let Err(e) = db
                .log_activity("info", "session", "New session started", None)
                .await
            {
                tracing::debug!("activity_log: session_start record failed: {e}");
            }
        }
        HookAction::Continue
    }
}

impl std::fmt::Debug for ActivityHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivityHook").finish()
    }
}

/// Log an activity event, swallowing errors with tracing::debug.
pub async fn log_activity(db: &Database, level: &str, category: &str, msg: &str) {
    if let Err(e) = db.log_activity(level, category, msg, None).await {
        tracing::debug!("activity_log: failed to record: {e}");
    }
}

/// Log an activity event with detail, swallowing errors with tracing::debug.
pub async fn log_activity_detail(
    db: &Database,
    level: &str,
    category: &str,
    msg: &str,
    detail: &str,
) {
    if let Err(e) = db.log_activity(level, category, msg, Some(detail)).await {
        tracing::debug!("activity_log: failed to record: {e}");
    }
}

/// Format an activity entry for display in TUI or CLI.
pub fn format_activity_entry(entry: &crate::db::ActivityEntry) -> String {
    let dt = chrono::DateTime::from_timestamp(entry.created_at, 0)
        .map(|d| d.with_timezone(&chrono::Local))
        .map(|d| d.format("%H:%M:%S %Z").to_string())
        .unwrap_or_else(|| "??:??:?? ???".to_string());
    let base = format!(
        "[{dt}] {:<5} {:<9} {}",
        entry.level.to_uppercase(),
        entry.category,
        entry.message
    );
    if let Some(detail) = &entry.detail {
        if !detail.is_empty() {
            return format!("{base}\n             {detail}");
        }
    }
    base
}

/// Map level string to numeric priority for ordering.
pub fn level_priority(level: &str) -> u8 {
    match level {
        "error" => 0,
        "warn" => 1,
        "info" => 2,
        "debug" => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_activity_entry() {
        let entry = crate::db::ActivityEntry {
            id: 1,
            level: "info".to_string(),
            category: "session".to_string(),
            message: "New session started".to_string(),
            detail: None,
            created_at: 1700000000, // 2023-11-14
        };
        let formatted = format_activity_entry(&entry);
        assert!(formatted.contains("INFO"));
        assert!(formatted.contains("session"));
        assert!(formatted.contains("New session started"));
    }

    #[test]
    fn test_format_activity_entry_with_detail() {
        let entry = crate::db::ActivityEntry {
            id: 1,
            level: "error".to_string(),
            category: "agent".to_string(),
            message: "Task failed".to_string(),
            detail: Some("timeout after 30s".to_string()),
            created_at: 1700000000,
        };
        let formatted = format_activity_entry(&entry);
        assert!(formatted.contains("ERROR"));
        assert!(formatted.contains("Task failed"));
        assert!(formatted.contains("timeout after 30s"));
    }

    // PHASE 9 TODO: roundtrip test disabled pending async DB harness.
    // Original asserted log_activity → query_activity behavior; the helpers
    // are now async and Database::from_connection needs a turso connection.

    #[test]
    fn test_level_priority() {
        assert!(level_priority("error") < level_priority("warn"));
        assert!(level_priority("warn") < level_priority("info"));
        assert!(level_priority("info") < level_priority("debug"));
        assert!(level_priority("debug") < level_priority("unknown"));
    }
}
