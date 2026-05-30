//! Hourly budget reset tick.
//!
//! Spawned at daemon startup. Every hour (and once at boot) calls
//! [`cortex::budgets::BudgetEngine::reset_if_due`] to advance any budget
//! whose period has elapsed and write a `budget_reset` event. Failures log
//! a `tracing::warn!` and the loop continues — a budget tick error must
//! never kill the daemon.

use std::time::Duration;

use cortex::budgets::BudgetEngine;
use cortex::db::Database;
use tokio::sync::broadcast;
use tokio::time::{interval, MissedTickBehavior};

/// Hourly cadence. Daily resets only need hour-grain precision — the
/// user-visible effect ("agent unpauses") tolerates up to an hour of latency.
const TICK_INTERVAL_SECS: u64 = 3600;

/// Spawn the reset loop. Returns immediately; the task runs until the
/// shared shutdown signal fires.
pub fn spawn(mut shutdown: broadcast::Receiver<()>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(TICK_INTERVAL_SECS));
        // If the daemon is paused (e.g. machine sleep) we don't want a
        // burst of catch-up resets — one per wake is enough.
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Err(e) = run_once().await {
                        tracing::warn!(error = %e, "budget reset tick failed");
                    }
                }
                _ = shutdown.recv() => {
                    tracing::debug!("budget tick shutting down");
                    return;
                }
            }
        }
    })
}

async fn run_once() -> anyhow::Result<()> {
    let db = Database::open().await?;
    let now = chrono::Utc::now().timestamp();
    let count = BudgetEngine::reset_if_due(&db, now).await?;
    if count > 0 {
        tracing::info!(reset_count = count, "advanced expired budget periods");
    }
    Ok(())
}
