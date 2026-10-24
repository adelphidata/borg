//! Constants and helpers for the seeded `task_type="heartbeat"` row.
//!
//! Heartbeat is a single seeded row in `scheduled_tasks`; this module owns
//! the row's fixed identifier and the cortex-side helpers around it.

pub use crate::task_ids::HEARTBEAT_TASK_ID;

/// Reserved `delivery_channel` value that means "fan out to all
/// gateway-bound channels". No real channel may be named `broadcast`, so the
/// column carries the fan-out case without a second column to keep in sync.
/// Rejected by the user-callable `schedule.create` so user tasks can't fan
/// out.
pub const BROADCAST_DELIVERY: &str = "broadcast";

/// Sentinel `delivery_channel` value meaning "reply wherever this task was
/// created from". It is resolved to a concrete channel at create time; a row
/// still carrying it at run time means that resolution failed, so the result
/// has nowhere to go.
pub const ORIGIN_DELIVERY: &str = "origin";

/// Returns true if the task ID matches the seeded heartbeat row.
pub fn is_heartbeat_task_id(id: &str) -> bool {
    id == HEARTBEAT_TASK_ID
}

/// The setting that sets the heartbeat cadence.
pub const INTERVAL_KEY: &str = "heartbeat.interval";

/// The optional cron expression. When it is set, it replaces the interval.
pub const CRON_KEY: &str = "heartbeat.cron";

/// True for the settings that decide when the heartbeat row fires.
pub fn is_schedule_key(key: &str) -> bool {
    key == INTERVAL_KEY || key == CRON_KEY
}

/// Reject a value of `key` that the heartbeat row cannot run on: an interval
/// that does not parse or is below the minimum, or a cron expression that
/// does not parse. An empty cron is valid; it means "use the interval".
pub fn validate_schedule_setting(key: &str, value: &str) -> anyhow::Result<()> {
    let value = value.trim();
    if key == CRON_KEY {
        if !value.is_empty() {
            crate::tasks::calculate_next_run(crate::tasks::SCHEDULE_TYPE_CRON, value, "local")
                .map_err(|e| {
                    anyhow::anyhow!("heartbeat.cron is not a valid cron expression: {e}")
                })?;
        }
        return Ok(());
    }
    let min = crate::constants::MIN_HEARTBEAT_INTERVAL_SECS;
    match crate::tasks::parse_interval(value) {
        Some(d) if d.as_secs() >= min => Ok(()),
        Some(_) => anyhow::bail!("heartbeat.interval must be at least {min}s"),
        None => anyhow::bail!("heartbeat.interval must look like 30m, 2h, 1d or 90s"),
    }
}

/// Put the seeded heartbeat row on the stored `heartbeat.cron`, or on
/// `heartbeat.interval` when no cron is set. The row is what the dispatcher
/// fires and the UI cannot edit it, so these settings are its only owner.
///
/// Returns false when there is nothing to move: the row is gone, or it
/// already runs on that schedule. A stored value that fails
/// [`validate_schedule_setting`] is an error and leaves the row as it is.
pub async fn sync_schedule(db: &crate::db::Database) -> anyhow::Result<bool> {
    use crate::tasks::{SCHEDULE_TYPE_CRON, SCHEDULE_TYPE_INTERVAL};
    let cron = db.get_setting(CRON_KEY).await?.unwrap_or_default();
    let (kind, key, expr) = if cron.trim().is_empty() {
        let interval = db
            .get_setting(INTERVAL_KEY)
            .await?
            .unwrap_or_else(|| crate::constants::DEFAULT_HEARTBEAT_INTERVAL.to_string());
        (SCHEDULE_TYPE_INTERVAL, INTERVAL_KEY, interval)
    } else {
        (SCHEDULE_TYPE_CRON, CRON_KEY, cron)
    };
    validate_schedule_setting(key, &expr)?;
    let expr = expr.trim();
    let Some(row) = db.get_task_by_id(HEARTBEAT_TASK_ID).await? else {
        return Ok(false);
    };
    if row.schedule_type == kind && row.schedule_expr == expr {
        return Ok(false);
    }
    let update = crate::db::UpdateTask {
        name: None,
        prompt: None,
        schedule_type: Some(kind),
        schedule_expr: Some(expr),
        timezone: None,
    };
    db.update_task(HEARTBEAT_TASK_ID, &update).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_task_id_is_a_valid_uuid_shape() {
        assert_eq!(HEARTBEAT_TASK_ID.len(), 36);
        assert_eq!(HEARTBEAT_TASK_ID.matches('-').count(), 4);
    }

    #[test]
    fn is_heartbeat_task_id_only_matches_the_seeded_uuid() {
        assert!(is_heartbeat_task_id(HEARTBEAT_TASK_ID));
        assert!(!is_heartbeat_task_id("not-the-heartbeat-row"));
        assert!(!is_heartbeat_task_id(""));
    }
}
