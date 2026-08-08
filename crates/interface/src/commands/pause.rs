//! `borg pause` / `borg resume` — the global emergency stop.
//!
//! Writes `security.emergency_paused` through the validated settings path
//! (DB-backed config), so the running daemon picks it up without a restart:
//! every gate reads the flag straight from the DB — new turns are rejected
//! immediately, the scheduler dispatcher skips from its next tick, and
//! heartbeat injection stops at its next poll. Pause context (reason +
//! engaged-at timestamp) lives in `meta` rows so the settings table gains
//! exactly one key for this feature.

use anyhow::Result;

/// Engage the emergency stop. Records the reason/timestamp *before*
/// flipping the flag so any turn rejected in the race window already sees
/// the full context.
pub(crate) async fn run_pause(reason: Option<String>) -> Result<()> {
    let db = cortex::db::Database::open().await?;
    let now = chrono::Utc::now().timestamp();
    db.set_meta(
        cortex::constants::META_EMERGENCY_PAUSE_SINCE,
        &now.to_string(),
    )
    .await?;
    match reason.as_deref() {
        Some(r) if !r.trim().is_empty() => {
            db.set_meta(cortex::constants::META_EMERGENCY_PAUSE_REASON, r.trim())
                .await?
        }
        // A pause without a reason must not inherit a stale one.
        _ => {
            db.delete_meta(cortex::constants::META_EMERGENCY_PAUSE_REASON)
                .await?
        }
    }

    let resolver = cortex::settings::SettingsResolver::new(db);
    resolver
        .set(cortex::emergency_pause::SETTING_KEY, "true")
        .await?;

    println!("Borg is PAUSED. New work is halted; in-flight turns finish normally.");
    if let Some(r) = reason.as_deref().filter(|r| !r.trim().is_empty()) {
        println!("Reason: {}", r.trim());
    }
    println!("New turns are rejected immediately; the task scheduler stops dispatching");
    println!("within one tick. Resume with: borg resume");
    Ok(())
}

/// Release the emergency stop and clear the recorded pause context.
pub(crate) async fn run_resume() -> Result<()> {
    let db = cortex::db::Database::open().await?;

    let resolver = cortex::settings::SettingsResolver::new(db);
    resolver
        .set(cortex::emergency_pause::SETTING_KEY, "false")
        .await?;

    // Resolver consumed the handle; reopen for the meta cleanup.
    let db = cortex::db::Database::open().await?;
    db.delete_meta(cortex::constants::META_EMERGENCY_PAUSE_REASON)
        .await?;
    db.delete_meta(cortex::constants::META_EMERGENCY_PAUSE_SINCE)
        .await?;

    println!("Borg resumed. Scheduled dispatch, heartbeats, and new turns are enabled again;");
    println!("runs deferred during the pause execute at their next opportunity.");
    Ok(())
}
