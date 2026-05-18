//! Constants and helpers for the seeded `task_type="heartbeat"` row.
//!
//! Heartbeat is a single seeded row in `scheduled_tasks`; this module owns
//! the row's fixed identifier and the cortex-side helpers around it.

pub use crate::task_ids::HEARTBEAT_TASK_ID;

/// Sentinel `delivery_channel` value that means "fan out to all
/// gateway-bound channels" — preserves the legacy multi-channel
/// heartbeat broadcast semantics without adding a new column. Rejected
/// by the user-callable `schedule.create` so user tasks can't fan out.
pub const BROADCAST_DELIVERY: &str = "broadcast";

/// Returns true if the task ID matches the seeded heartbeat row.
pub fn is_heartbeat_task_id(id: &str) -> bool {
    id == HEARTBEAT_TASK_ID
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
