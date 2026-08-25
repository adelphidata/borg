//! Fire-and-forget gateway activity logging.
//!
//! Several gateway paths record an operator-visible activity-log line as a
//! best-effort side effect: open the DB with the gateway busy-timeout, write
//! one `"gateway"`-category row, and move on. The DB open is allowed to fail
//! silently — this *is* the logging path, so there is nowhere meaningful to
//! report a failed log write, and it must never abort the request it annotates.
//!
//! Centralizing the pattern keeps the busy-timeout, category string, and
//! error-swallowing decision in one place instead of copy-pasted at every site.

use cortex::db::Database;

/// Best-effort append of a `"gateway"`-category activity-log entry.
///
/// `level` is the activity level (`"info"`, `"warn"`, …). Silently does nothing
/// if the database can't be opened within the gateway busy-timeout.
pub async fn log_gateway_activity(level: &str, message: &str) {
    if let Ok(db) = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS).await {
        cortex::activity_log::log_activity(&db, level, "gateway", message).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of this module is that a call actually lands one
    /// `"gateway"`-category row in the activity log — verify the row exists
    /// with the level and message that were passed in.
    #[tokio::test]
    async fn log_gateway_activity_writes_gateway_category_row() {
        let _guard = crate::test_env::borg_home_guard();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_HOME", tmp.path());

        log_gateway_activity("warn", "webhook signature rejected").await;

        let db = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS)
            .await
            .unwrap();
        let entries = db
            .query_activity(10, Some("warn"), Some("gateway"))
            .await
            .unwrap();
        std::env::remove_var("BORG_HOME");

        assert_eq!(
            entries.len(),
            1,
            "expected exactly one gateway activity row"
        );
        assert_eq!(entries[0].message, "webhook signature rejected");
        assert_eq!(entries[0].level, "warn");
        assert_eq!(entries[0].category, "gateway");
    }
}
