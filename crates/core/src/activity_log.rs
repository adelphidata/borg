//! Structured activity log — captures key system events for `/logs` and `borg logs --activity`.
//!
//! Events are stored in SQLite via `Database::log_activity()` with level/category filtering.
//! `ActivityHook` records agent lifecycle events; convenience functions allow other subsystems
//! (daemon, gateway) to log events directly.

use crate::db::Database;
use crate::hooks::{Hook, HookAction, HookContext, HookData, HookPoint};

/// Lifecycle hook that records agent events to the activity log.
/// Wraps Database in a Mutex because Hook requires Send + Sync
/// but rusqlite::Connection is !Sync.
pub struct ActivityHook {
    db: std::sync::Mutex<Database>,
}

impl ActivityHook {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            db: std::sync::Mutex::new(Database::open()?),
        })
    }
}

impl Hook for ActivityHook {
    fn name(&self) -> &str {
        "activity"
    }

    fn points(&self) -> &[HookPoint] {
        &[HookPoint::SessionStart, HookPoint::OnError]
    }

    fn execute(&self, ctx: &HookContext) -> HookAction {
        let Ok(db) = self.db.lock() else {
            return HookAction::Continue;
        };
        match &ctx.data {
            HookData::SessionStart { .. } => {
                let _ = db.log_activity("info", "session", "New session started", None);
            }
            HookData::Error { message } => {
                let _ = db.log_activity("error", "agent", &format!("Agent error: {message}"), None);
            }
            _ => {}
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
pub fn log_activity(db: &Database, level: &str, category: &str, msg: &str) {
    if let Err(e) = db.log_activity(level, category, msg, None) {
        tracing::debug!("activity_log: failed to record: {e}");
    }
}

/// Log an activity event with detail, swallowing errors with tracing::debug.
pub fn log_activity_detail(db: &Database, level: &str, category: &str, msg: &str, detail: &str) {
    if let Err(e) = db.log_activity(level, category, msg, Some(detail)) {
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

    #[test]
    fn test_log_activity_migrate_roundtrip() {
        let db =
            Database::from_connection(rusqlite::Connection::open_in_memory().unwrap()).unwrap();

        // Use the convenience function (same as TUI migration handler)
        log_activity(
            &db,
            "info",
            "migrate",
            "Migration complete: 2 config change(s) applied",
        );
        log_activity(
            &db,
            "error",
            "migrate",
            "Migration failed: connection refused",
        );

        let entries = db
            .query_activity(50, Some("info"), Some("migrate"))
            .unwrap();
        assert_eq!(entries.len(), 2);

        let formatted = format_activity_entry(&entries[0]);
        assert!(formatted.contains("migrate"));
        assert!(formatted.contains("Migration"));
    }

    #[test]
    fn test_level_priority() {
        assert!(level_priority("error") < level_priority("warn"));
        assert!(level_priority("warn") < level_priority("info"));
        assert!(level_priority("info") < level_priority("debug"));
        assert!(level_priority("debug") < level_priority("unknown"));
    }
}
