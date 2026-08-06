//! Live coverage for the `maintenance.heal_action_budget` backstop.
//!
//! The broader `maintenance_sweep.rs` suite is gated behind
//! `phase9-tests-disabled` (pre-async rusqlite style, pending port), so the
//! budget invariant — the thing that stops a runaway repair loop — had no
//! coverage that actually runs. These tests drive the real async
//! `run_daily_maintenance` and assert on the returned report.
//!
//! Spec invariant (CLAUDE.md → "Self-Healing"): mutating actions are capped by
//! `maintenance.heal_action_budget`; once exhausted, remaining mutating steps
//! short-circuit and `report.budget_exhausted` is true.

#![allow(clippy::expect_used, clippy::field_reassign_with_default)]

use std::sync::Mutex;

use cortex::config::Config;
use cortex::constants::SECS_PER_DAY;
use cortex::db::{Database, NewTask};
use cortex::maintenance::run_daily_maintenance;

/// These tests set the process-global `BORG_DATA_DIR`, so they must not race.
static SERIAL: Mutex<()> = Mutex::new(());

fn isolated_data_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: serialized by SERIAL; no other thread reads the env concurrently.
    unsafe {
        std::env::set_var("BORG_DATA_DIR", tmp.path());
    }
    tmp
}

/// Cache an embedding and backdate it past the eviction TTL.
async fn seed_expired_embedding(db: &Database, hash: &str, age_secs: i64) {
    let now = chrono::Utc::now().timestamp();
    db.cache_embedding("openai", "text-embedding-3-small", hash, &[1u8, 2, 3, 4], 4)
        .await
        .expect("cache embedding");
    db.conn()
        .execute(
            "UPDATE embedding_cache SET last_accessed_at = ?1 WHERE content_hash = ?2",
            libsql::params![now - age_secs, hash],
        )
        .await
        .expect("backdate last_accessed_at");
}

async fn seed_stalled_cron_task(db: &Database, id: &str, overdue_secs: i64) {
    let now = chrono::Utc::now().timestamp();
    db.create_task(&NewTask {
        id,
        name: "test-stalled",
        prompt: "noop",
        // Every minute: `heal_stalled_tasks` must be able to recompute
        // `next_run`, or it routes into the "pause task" branch instead.
        schedule_type: "cron",
        schedule_expr: "0 * * * * *",
        timezone: "UTC",
        next_run: Some(now - overdue_secs),
        max_retries: None,
        timeout_ms: None,
        delivery_channel: None,
        delivery_target: None,
        allowed_tools: None,
        task_type: "prompt",
        pre_run_command: None,
        pre_run_cwd: None,
        pre_run_timeout_ms: None,
        min_interval_secs: None,
    })
    .await
    .expect("create stalled task");
}

#[tokio::test]
async fn a_blown_budget_short_circuits_the_remaining_mutating_steps() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _tmp = isolated_data_dir();
    let db = Database::open().await.expect("open db at BORG_DATA_DIR");

    // Five expired cache rows are pruned by one statement that charges 5 at
    // once, so a budget of 1 is blown by that step. The gate is checked
    // *between* steps, so the pruning step itself still completes — the
    // contract is that everything after it does not.
    let ttl_secs = 30 * SECS_PER_DAY;
    for i in 0..5 {
        seed_expired_embedding(&db, &format!("budget_{i}"), ttl_secs + 86_400).await;
    }
    seed_stalled_cron_task(&db, "budget-stalled", 7_200).await;

    let mut config = Config::default();
    config.maintenance.heal_action_budget = 1;

    let report = run_daily_maintenance(&db, &config)
        .await
        .expect("sweep must not error");

    assert!(
        report.budget_exhausted,
        "pruning 5 rows against a budget of 1 must blow it; report: {report:?}"
    );
    assert_eq!(
        report.stalled_tasks_healed, 0,
        "a step after the budget blew must not run; report: {report:?}"
    );
    assert!(
        report.auto_fixed_issues.is_empty(),
        "the remediation loop must not run on a blown budget; report: {report:?}"
    );
}

#[tokio::test]
async fn a_generous_budget_is_not_blown_by_a_healthy_sweep() {
    // The other half: the per-key charge added to the remediation loop must
    // not make an ordinary sweep look runaway. Without this, tightening the
    // budget accounting could silently disable self-healing entirely.
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _tmp = isolated_data_dir();
    let db = Database::open().await.expect("open db at BORG_DATA_DIR");

    let config = Config::default();
    assert_eq!(
        config.maintenance.heal_action_budget, 1000,
        "documented default; the assertions below assume there is ample room"
    );

    let report = run_daily_maintenance(&db, &config)
        .await
        .expect("sweep must not error");

    assert!(
        !report.budget_exhausted,
        "a clean database must not exhaust the default budget; report: {report:?}"
    );
}
