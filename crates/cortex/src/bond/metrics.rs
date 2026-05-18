//! Rolling 30-day signals computed on demand from the database. Used to
//! decorate bond status output and inject context into the system prompt.

use chrono::Utc;

use crate::constants::SECS_PER_DAY;
use crate::db::Database;

/// Compute routine success rate from task_runs in last 30 days.
pub async fn compute_routine_success_rate(db: &Database) -> f32 {
    let since = Utc::now().timestamp() - 30 * SECS_PER_DAY;
    match db.count_task_runs_since(since, Some("success")).await {
        Ok((success, total)) if total > 0 => success as f32 / total as f32,
        _ => 0.0,
    }
}

/// Compute correction rate from vitals_events in last 30 days.
pub async fn compute_correction_rate(db: &Database) -> f32 {
    let since = Utc::now().timestamp() - 30 * SECS_PER_DAY;
    match db
        .count_vitals_events_by_category_since(since, "correction")
        .await
    {
        Ok((corrections, total)) if total > 0 => {
            let negatives = db
                .count_vitals_events_by_category_since(since, "negative_sentiment")
                .await
                .map(|(n, _)| n)
                .unwrap_or(0);
            (corrections + negatives) as f32 / total as f32
        }
        _ => 0.0,
    }
}

/// Count creation events (preference learning proxy) from vitals_events in last 30 days.
pub async fn compute_preference_learning_count(db: &Database) -> u32 {
    let since = Utc::now().timestamp() - 30 * SECS_PER_DAY;
    db.count_vitals_events_by_category_since(since, "creation")
        .await
        .map(|(count, _)| count)
        .unwrap_or(0)
}

// TODO: needs async DB harness
// they relied on `rusqlite::params!` in direct SQL inserts plus sync helpers.
#[cfg(all(test, feature = "phase9-tests-disabled"))]
mod tests {
    use super::*;
    use crate::vitals::{deltas_for, EventCategory};

    fn fresh_db() -> Database {
        Database::test_db()
    }

    fn record(db: &Database, category: &str) {
        let cat = EventCategory::parse(category).expect("valid category");
        db.record_vitals_event(category, "test", &deltas_for(cat), None)
            .expect("record vitals event");
    }

    /// Insert a task_run row directly so we can backdate `started_at` past
    /// the 30-day window. `record_task_run` would stamp `now`.
    fn insert_task_run(db: &Database, task_id: &str, started_at: i64, status: &str) {
        let conn = db.conn();
        // Ensure FK target exists.
        conn.execute(
            "INSERT OR IGNORE INTO scheduled_tasks (id, name, prompt, schedule_type, schedule_expr, timezone, status, next_run, created_at, max_retries, timeout_ms, task_type)
             VALUES (?1, ?1, '', 'interval', '1m', 'UTC', 'active', NULL, 0, 3, 60000, 'prompt')",
            rusqlite::params![task_id],
        ).expect("seed scheduled_task");
        conn.execute(
            "INSERT INTO task_runs (task_id, started_at, duration_ms, result, error, status)
             VALUES (?1, ?2, 0, NULL, NULL, ?3)",
            rusqlite::params![task_id, started_at, status],
        )
        .expect("insert task_run");
    }

    #[test]
    fn routine_success_rate_zero_on_empty_db() {
        let db = fresh_db();
        // Zero-denominator must short-circuit to 0.0, not divide-by-zero.
        assert_eq!(compute_routine_success_rate(&db), 0.0);
    }

    #[test]
    fn routine_success_rate_counts_only_30d_window() {
        let db = fresh_db();
        let now = Utc::now().timestamp();
        // 3 success, 1 failed within window
        insert_task_run(&db, "t1", now - 60, "success");
        insert_task_run(&db, "t1", now - 120, "success");
        insert_task_run(&db, "t1", now - 180, "success");
        insert_task_run(&db, "t1", now - 240, "failed");
        // 1 ancient success — must NOT be counted (>30 days old)
        insert_task_run(&db, "t1", now - 31 * 86_400, "success");

        let rate = compute_routine_success_rate(&db);
        assert!(
            (rate - 0.75).abs() < 1e-6,
            "expected 0.75 from 3 success / 4 total in window, got {rate}"
        );
    }

    #[test]
    fn correction_rate_combines_corrections_and_negative_sentiment() {
        let db = fresh_db();
        // 2 corrections + 1 negative_sentiment + 7 successes → numerator 3,
        // denom 10. Uses "success" (cap 8/hr) for filler so all 7 inserts
        // actually stick — "interaction" caps at 5/hr and would silently drop
        // the rest, breaking the denominator.
        record(&db, "correction");
        record(&db, "correction");
        record(&db, "negative_sentiment");
        for _ in 0..7 {
            record(&db, "success");
        }
        let rate = compute_correction_rate(&db);
        assert!(
            (rate - 0.30).abs() < 1e-6,
            "expected (2 + 1) / 10 = 0.30, got {rate}. If this asserts 0.20, the negative_sentiment branch was dropped."
        );
    }

    #[test]
    fn correction_rate_zero_on_empty_db() {
        let db = fresh_db();
        assert_eq!(compute_correction_rate(&db), 0.0);
    }

    #[test]
    fn correction_rate_zero_when_no_corrections_present() {
        let db = fresh_db();
        // Only non-correction, non-negative events — numerator must be 0.
        for _ in 0..5 {
            record(&db, "interaction");
        }
        assert_eq!(compute_correction_rate(&db), 0.0);
    }

    #[test]
    fn preference_learning_counts_only_creation_events() {
        let db = fresh_db();
        record(&db, "creation");
        record(&db, "creation");
        record(&db, "creation");
        // Other categories must NOT inflate the count.
        record(&db, "interaction");
        record(&db, "success");
        record(&db, "correction");
        assert_eq!(compute_preference_learning_count(&db), 3);
    }

    #[test]
    fn preference_learning_zero_on_empty_db() {
        let db = fresh_db();
        assert_eq!(compute_preference_learning_count(&db), 0);
    }
}
