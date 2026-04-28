//! Rolling 30-day signals computed on demand from the database. Used to
//! decorate bond status output and inject context into the system prompt.

use chrono::Utc;

use crate::db::Database;

/// Compute routine success rate from task_runs in last 30 days.
pub fn compute_routine_success_rate(db: &Database) -> f32 {
    let since = Utc::now().timestamp() - 30 * 86400;
    match db.count_task_runs_since(since, Some("success")) {
        Ok((success, total)) if total > 0 => success as f32 / total as f32,
        _ => 0.0,
    }
}

/// Compute correction rate from vitals_events in last 30 days.
pub fn compute_correction_rate(db: &Database) -> f32 {
    let since = Utc::now().timestamp() - 30 * 86400;
    match db.count_vitals_events_by_category_since(since, "correction") {
        Ok((corrections, total)) if total > 0 => {
            let negatives = db
                .count_vitals_events_by_category_since(since, "negative_sentiment")
                .map(|(n, _)| n)
                .unwrap_or(0);
            (corrections + negatives) as f32 / total as f32
        }
        _ => 0.0,
    }
}

/// Count creation events (preference learning proxy) from vitals_events in last 30 days.
pub fn compute_preference_learning_count(db: &Database) -> u32 {
    let since = Utc::now().timestamp() - 30 * 86400;
    db.count_vitals_events_by_category_since(since, "creation")
        .map(|(count, _)| count)
        .unwrap_or(0)
}
