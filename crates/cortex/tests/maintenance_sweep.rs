//! Integration tests: daily self-healing maintenance sweep end-to-end.
//!
//! Spec invariants (CLAUDE.md → "Self-Healing"):
//!   - Daily maintenance prunes stale embedding-cache rows, heals stalled
//!     scheduled tasks, and head-truncates oversized append-only log files.
//!   - The `maintenance.heal_action_budget` cap (default 1000) backstops
//!     runaway repair loops: once exhausted, remaining mutating steps
//!     short-circuit and `report.budget_exhausted = true`.
//!   - When `maintenance.enabled = false`, the sweep returns a `skipped`
//!     report with no DB mutations.
//!
//! These drive `cortex::maintenance::run_daily_maintenance` against a real
//! SQLite DB rooted under `BORG_DATA_DIR=<tempdir>` and assert on the
//! returned `MaintenanceReport` counters plus the post-sweep DB state.
//!
//! ── Mutation verification ──
//!
//! • Replace `prune_embedding_cache` to always return `Ok(0)`:
//!   `happy_path_drains_seeded_state` fails because `embeddings_pruned == 0`.
//! • Make `BudgetTracker::try_spend` always return `true`:
//!   `budget_exhausted_short_circuits_remaining_steps` fails because the
//!   sweep no longer flips `budget_exhausted` and continues consuming work.
//! • Remove the `if !config.maintenance.enabled { skipped = true; ... return }`
//!   early return: `disabled_returns_skipped_report_no_mutations` fails
//!   because the sweep runs and starts pruning rows.

#![allow(
    clippy::expect_used,
    clippy::field_reassign_with_default,
    clippy::unwrap_used,
    clippy::uninlined_format_args
)]

use std::fs;
use std::sync::Mutex;

use cortex::config::Config;
use cortex::constants::SECS_PER_DAY;
use cortex::db::{Database, NewTask};
use cortex::maintenance::run_daily_maintenance;

/// Tests in this binary share the process-global `BORG_DATA_DIR` env var
/// (which `Database::open` and `Config::data_dir`/`logs_dir` consult). Run
/// them serialized so each gets a clean tempdir without racing.
static SERIAL: Mutex<()> = Mutex::new(());

fn isolated_data_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: env mutation under multi-threaded test runners can race FFI
    // `getenv`. SERIAL above guarantees no other test in this binary reads
    // or writes BORG_DATA_DIR concurrently. Background threads in this
    // binary do not consult the env. See the same pattern in
    // `tests/memory_token_cap.rs`.
    unsafe {
        std::env::set_var("BORG_DATA_DIR", tmp.path());
    }
    tmp
}

/// Force-insert an embedding-cache row whose `last_accessed_at` is `age_secs`
/// in the past — older than the 30-day TTL the sweep prunes against.
fn seed_expired_embedding(db: &Database, hash: &str, age_secs: i64) {
    let now = chrono::Utc::now().timestamp();
    db.cache_embedding("openai", "text-embedding-3-small", hash, &[1u8, 2, 3, 4], 4)
        .expect("cache embedding");
    let stale = now - age_secs;
    db.conn()
        .execute(
            "UPDATE embedding_cache SET last_accessed_at = ?1 WHERE content_hash = ?2",
            rusqlite::params![stale, hash],
        )
        .expect("backdate last_accessed_at");
}

fn embedding_cache_row_count(db: &Database) -> i64 {
    db.conn()
        .query_row("SELECT COUNT(*) FROM embedding_cache", [], |r| r.get(0))
        .expect("count embedding_cache rows")
}

fn seed_stalled_cron_task(db: &Database, id: &str, overdue_secs: i64) {
    let now = chrono::Utc::now().timestamp();
    db.create_task(&NewTask {
        id,
        name: "test-stalled",
        prompt: "noop",
        // Cron expr: every minute — `heal_stalled_tasks` recomputes
        // `next_run` via `calculate_next_run`, which must succeed for the
        // healed counter to tick. A degenerate expr would route into the
        // "pause task" branch instead.
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
    .expect("create stalled task");
}

/// Write a `daemon.log` whose size exceeds the 5 MiB head-truncation cap.
/// `truncate_oversized_logs` must rewrite it down to the 1 MiB keep band.
fn seed_oversized_daemon_log(logs_dir: &std::path::Path) -> u64 {
    fs::create_dir_all(logs_dir).expect("create logs dir");
    // Slightly over the 5 MiB cap so the truncation step is guaranteed to fire.
    let size_bytes: u64 = 6 * 1024 * 1024;
    let path = logs_dir.join("daemon.log");
    let line = b"2026-01-01T00:00:00Z DEBUG noisy line that fills the log file\n";
    let mut written: u64 = 0;
    let mut f = fs::File::create(&path).expect("create daemon.log");
    use std::io::Write;
    while written < size_bytes {
        f.write_all(line).expect("write log");
        written += line.len() as u64;
    }
    f.sync_all().expect("sync");
    written
}

// ─────────────────────────────────────────────────────────────────────────────
// Test cases
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn happy_path_drains_seeded_state() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _tmp = isolated_data_dir();

    let db = Database::open().expect("open db at BORG_DATA_DIR");

    // Seed: 3 expired cache entries (older than 30-day TTL).
    let ttl_secs = 30 * SECS_PER_DAY;
    for i in 0..3 {
        seed_expired_embedding(&db, &format!("expired_{i}"), ttl_secs + 86_400);
    }
    // Seed: 1 stalled cron task (next_run 2h in the past — > 1h grace).
    seed_stalled_cron_task(&db, "stalled-task-001", 7_200);
    // Seed: oversized daemon.log under the test's logs_dir.
    let logs_dir = Config::logs_dir().expect("logs_dir");
    let initial_log_size = seed_oversized_daemon_log(&logs_dir);

    let config = Config::default();
    assert!(
        config.maintenance.enabled,
        "default config must have maintenance enabled — test relies on this"
    );

    let report = run_daily_maintenance(&db, &config).expect("sweep ok");

    assert!(!report.skipped, "sweep must run when enabled");
    assert!(
        report.embeddings_pruned >= 3,
        "all 3 expired cache rows should be pruned; got {}",
        report.embeddings_pruned
    );
    assert!(
        report.stalled_tasks_healed >= 1,
        "stalled cron task must be healed; got {}",
        report.stalled_tasks_healed
    );
    assert!(
        report.log_bytes_truncated > 0,
        "oversized daemon.log must be truncated; got {} bytes",
        report.log_bytes_truncated
    );

    // DB state: the seeded expired rows must be gone.
    let remaining_expired: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM embedding_cache WHERE content_hash LIKE 'expired_%'",
            [],
            |r| r.get(0),
        )
        .expect("count remaining expired rows");
    assert_eq!(
        remaining_expired, 0,
        "expired embedding_cache rows must be deleted by the sweep"
    );

    // FS state: daemon.log must shrink to ≤ the keep band (1 MiB) plus a
    // small slack for partial-line padding inside `truncate_oversized_logs`.
    let log_path = logs_dir.join("daemon.log");
    let after_size = fs::metadata(&log_path).expect("stat daemon.log").len();
    assert!(
        after_size < initial_log_size,
        "daemon.log must be smaller after sweep ({} < {})",
        after_size,
        initial_log_size
    );
    assert!(
        after_size <= 2 * 1024 * 1024,
        "daemon.log must shrink near the 1 MiB keep band; got {} bytes",
        after_size
    );
}

#[test]
fn budget_exhausted_short_circuits_remaining_steps() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _tmp = isolated_data_dir();

    let db = Database::open().expect("open db at BORG_DATA_DIR");

    // Seed enough work to overflow a tiny budget. 5 expired cache rows are
    // pruned in a single SQL statement that charges 5 actions at once —
    // budget=1 must trip on the first try_spend(5).
    let ttl_secs = 30 * SECS_PER_DAY;
    for i in 0..5 {
        seed_expired_embedding(&db, &format!("budget_{i}"), ttl_secs + 86_400);
    }
    // Also seed a stalled task — when the budget is blown, this step must
    // be skipped entirely (`stalled_tasks_healed` must stay 0).
    seed_stalled_cron_task(&db, "budget-stalled", 7_200);

    let mut config = Config::default();
    config.maintenance.heal_action_budget = 1;

    let report = run_daily_maintenance(&db, &config).expect("sweep ok");

    assert!(
        report.budget_exhausted,
        "budget=1 with multi-row seeded work must blow the budget; report: {:?}",
        report
    );
    assert_eq!(
        report.stalled_tasks_healed, 0,
        "stalled-task healing must short-circuit once the budget is blown; \
         got {} (production code reached the stalled-task step despite blown budget)",
        report.stalled_tasks_healed
    );
}

#[test]
fn disabled_returns_skipped_report_no_mutations() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _tmp = isolated_data_dir();

    let db = Database::open().expect("open db at BORG_DATA_DIR");

    // Seed an expired row that the sweep WOULD prune if it ran.
    let ttl_secs = 30 * SECS_PER_DAY;
    seed_expired_embedding(&db, "disabled_seed", ttl_secs + 86_400);
    let before = embedding_cache_row_count(&db);
    assert!(before >= 1, "fixture must persist");

    let mut config = Config::default();
    config.maintenance.enabled = false;

    let report = run_daily_maintenance(&db, &config).expect("disabled sweep returns Ok");

    assert!(
        report.skipped,
        "disabled config must produce a skipped report"
    );
    assert_eq!(
        report.embeddings_pruned, 0,
        "no embedding work when disabled"
    );
    assert_eq!(
        report.stalled_tasks_healed, 0,
        "no task healing when disabled"
    );
    assert_eq!(
        report.log_bytes_truncated, 0,
        "no log truncation when disabled"
    );

    // DB state: the seeded row must still be there — the disable flag must
    // be a real short-circuit, not a "skip the report counters but still
    // do the work" mistake.
    let after = embedding_cache_row_count(&db);
    assert_eq!(
        after, before,
        "disabled sweep must not mutate embedding_cache (before={}, after={})",
        before, after
    );
}
