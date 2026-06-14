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
