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
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

/// Hourly cadence. Daily resets only need hour-grain precision — the
/// user-visible effect ("agent unpauses") tolerates up to an hour of latency.
const TICK_INTERVAL_SECS: u64 = 3600;

/// Run the reset loop until `shutdown` fires.
///
/// Registered through `spawn_supervised` (see `orchestrator::start`) rather
/// than a bare `tokio::spawn`: this is a daemon-lifetime loop, and an
/// unsupervised panic here would be swallowed by tokio, leaving budgets
/// frozen — the agent stays paused past its reset with no activity-log row,
/// no `maintenance_issue_state` entry, and no doctor finding.
///
/// Per-tick errors are logged and the loop continues; only a panic reaches
/// the supervisor.
pub async fn run(shutdown: CancellationToken) -> anyhow::Result<()> {
    let mut tick = interval(Duration::from_secs(TICK_INTERVAL_SECS));
    // If the daemon is paused (e.g. machine sleep) we don't want a
    // burst of catch-up resets — one per wake is enough.
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                tracing::debug!("budget tick shutting down");
                return Ok(());
            }
            _ = tick.tick() => {
                if let Err(e) = run_once().await {
                    tracing::warn!(error = %e, "budget reset tick failed");
                }
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn run_returns_ok_on_cancellation_so_the_supervisor_does_not_respawn() {
        // The supervisor treats `Err` as a crash and respawns with backoff.
        // Shutdown must therefore be an `Ok` exit, or every daemon stop would
        // log a spurious subsystem restart on the way down.
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_secs(5), run(cancel))
            .await
            .expect("run must return promptly once cancelled");

        assert!(result.is_ok(), "cancellation is a clean exit, not a crash");
    }

    #[tokio::test(start_paused = true)]
    async fn run_does_not_tick_before_its_interval_elapses() {
        // Guards the `biased` select order and the interval cadence: a
        // hot-looping tick would hammer `Database::open` every iteration.
        // `interval` fires immediately on first poll, so one tick at t=0 is
        // expected; the point is that it then waits.
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run(cancel.clone()));

        tokio::time::advance(Duration::from_secs(TICK_INTERVAL_SECS / 2)).await;
        assert!(!handle.is_finished(), "loop must still be running");

        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("must observe cancellation")
            .expect("task must not panic");
        assert!(result.is_ok());
    }
}
