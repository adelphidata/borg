//! Daemon-side self-healing driver.
//!
//! Concentrates the four pockets of maintenance logic that previously
//! lived scattered across `scheduler.rs` and `heartbeat_augmenters.rs`:
//!
//! 1. **Boot/drift/stall recovery** — moved out of the dispatcher loop body
//!    into [`sweep`]. Each is a small free fn taking only the `&Database`
//!    it needs (no fat context).
//! 2. **Heal-action budget** — the `maintenance.heal_action_budget` cap is
//!    no longer re-parsed at every consumer. [`Maintenance`] reads it once
//!    at construction and exposes a typed [`ActionBudget`] handle.
//! 3. **Augmenter cooldowns** — meta-table strings for
//!    `heartbeat_augmenter_last_fired_*` are encapsulated in
//!    [`CooldownTable`], so augmenters consume a typed handle instead of
//!    reading meta strings ad-hoc.
//! 4. **Daily sweep delegate** — borgd does not duplicate the cortex daily
//!    sweep; it calls [`cortex::maintenance::run_daily_maintenance`] and
//!    threads the resulting `MaintenanceReport`'s `budget_exhausted` flag
//!    through [`ActionBudget::record_sweep_end`] so the streak counter
//!    correctly escalates after two consecutive exhausts.
//!
//! Per `RUST-IDIOMS.md`: `Maintenance` owns *state*, not collaborators.
//! Each step takes only the `&Database` (or `&mut ActionBudget`) it
//! actually uses — no `MaintenanceContext` god-struct passed everywhere.
//! Single concrete impl, so no `trait Maintainer`.

pub(crate) mod budget;
pub(crate) mod clock;
pub(crate) mod cooldowns;
pub(crate) mod sweep;

use cortex::config::Config;
use cortex::db::Database;
use cortex::maintenance::MaintenanceReport;

pub(crate) use budget::{ActionBudget, MonthlyBudgetTracker};
pub(crate) use clock::Clock;
pub(crate) use cooldowns::CooldownTable;

/// Owns the daemon-side maintenance state: heal-action budget streak,
/// last-vacuum timestamp, and the cooldown table.
///
/// Configuration values (`maintenance.heal_action_budget`,
/// `maintenance.vacuum_interval_days`) are captured at construction and
/// not re-read by consumers — that's the whole point of having a typed
/// handle.
pub(crate) struct Maintenance {
    budget: ActionBudget,
    /// Unix-seconds timestamp of the most-recent successful VACUUM, or
    /// `None` if we haven't observed one yet. Used by tests asserting
    /// the cadence; production code reads `MaintenanceReport.vacuum_ran`.
    last_vacuum_at: Option<i64>,
    /// Captured from `maintenance.vacuum_interval_days` at construction.
    vacuum_interval_days: u32,
}

impl Maintenance {
    /// Construct a new driver from `maintenance.*` config keys.
    ///
    /// `heal_action_budget` and `vacuum_interval_days` are captured
    /// here and never re-read — consumers who need either get the
    /// typed handle or the cached value.
    pub(crate) fn new(config: &Config) -> Self {
        Self {
            budget: ActionBudget::new(config.maintenance.heal_action_budget),
            last_vacuum_at: None,
            vacuum_interval_days: config.maintenance.vacuum_interval_days,
        }
    }

    /// Heal-action budget handle. The augmenter / sweep step that needs
    /// to consume actions takes `&mut ActionBudget` from here — narrow
    /// ref over fat context.
    pub(crate) fn budget_handle(&mut self) -> &mut ActionBudget {
        &mut self.budget
    }

    /// Read-only view of the streak counter — primarily for tests and
    /// observability.
    pub(crate) fn exhausted_streak(&self) -> u32 {
        self.budget.exhausted_streak()
    }

    /// Most-recent observed VACUUM timestamp.
    pub(crate) fn last_vacuum_at(&self) -> Option<i64> {
        self.last_vacuum_at
    }

    /// Captured at construction.
    pub(crate) fn vacuum_interval_days(&self) -> u32 {
        self.vacuum_interval_days
    }

    /// Boot-time recovery — run once when the daemon starts. Marks
    /// previously-in-flight task runs as failed and advances any
    /// long-overdue tasks past the laptop-asleep replay window.
    pub(crate) fn recover_at_boot(&self, db: &Database) -> sweep::BootRecovery {
        sweep::boot_recovery(db)
    }

    /// Drift recovery — re-run the stale-task advance after a wall-clock
    /// jump (host wake-from-sleep). Cheaper than [`Self::recover_at_boot`]
    /// since the in-flight `task_runs` were already cleaned at boot.
    pub(crate) fn recover_after_drift(&self, db: &Database) -> sweep::DriftRecovery {
        sweep::drift_recovery(db)
    }

    /// Periodic 5-min stalled-task scan.
    pub(crate) fn scan_stalled(
        &self,
        db: &Database,
        now: i64,
        grace_secs: i64,
    ) -> sweep::StalledScan {
        sweep::stalled_scan(db, now, grace_secs)
    }

    /// Cooldown registry handle. Augmenters consume this through
    /// `for_augmenter(id, secs)`.
    pub(crate) fn cooldown_table<'a>(&self, db: &'a Database) -> CooldownTable<'a> {
        CooldownTable::new(db)
    }

    /// Run the daily self-healing sweep. Delegates to
    /// [`cortex::maintenance::run_daily_maintenance`] for the heavy
    /// lifting, then threads the report's `budget_exhausted` flag
    /// through the streak counter so a second consecutive exhaust
    /// promotes the existing `"Maintenance:heal action budget"`
    /// issue-state row to a persistent warning.
    pub(crate) fn run_daily_sweep<C: Clock>(
        &mut self,
        db: &Database,
        config: &Config,
        clock: &C,
    ) -> anyhow::Result<MaintenanceReport> {
        self.budget.begin_sweep();
        let report = cortex::maintenance::run_daily_maintenance(db, config)?;
        // Cortex tracked spend internally — mirror its outcome onto our
        // typed handle so successive sweep boundaries observe the right
        // streak. The cortex sweep already records the budget-exhausted
        // issue-state row; our streak counter is observability for the
        // daemon-loop side (tests, future heartbeat augmenter).
        if report.budget_exhausted {
            // Force the handle into the exhausted state so its
            // `is_exhausted()` matches the report.
            let _ = self.budget.try_consume(self.budget.limit());
        }
        self.budget.record_sweep_end(report.budget_exhausted);
        if report.vacuum_ran {
            self.last_vacuum_at = Some(clock.now_secs());
        }
        Ok(report)
    }

    /// Construct a [`MonthlyBudgetTracker`] for the heartbeat
    /// `budget_threshold` augmenter. Borrows `db` for the lifetime of
    /// the tracker.
    pub(crate) fn monthly_budget_tracker<'a>(&self, db: &'a Database) -> MonthlyBudgetTracker<'a> {
        MonthlyBudgetTracker::new(db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex::config::Config;
    use rusqlite::Connection;

    use crate::maintenance::clock::FakeClock;

    fn test_db() -> Database {
        // File-backed DB so `journal_mode=WAL` actually engages — the
        // checkpoint cadence test asserts that real (non-sentinel) page
        // counts come back, which `:memory:` cannot provide.
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let conn = Connection::open(tmp.path()).expect("open file db");
        // Leak the temp handle for the test's duration — Database owns
        // the connection but not the file path; dropping `tmp` here would
        // unlink the file out from under the writer.
        std::mem::forget(tmp);
        Database::from_connection(conn).expect("init test db")
    }

    fn budget_test_config(limit: u32) -> Config {
        let mut cfg = Config::default();
        cfg.maintenance.heal_action_budget = limit;
        cfg.maintenance.vacuum_interval_days = 7;
        // Disable bits the daily sweep would otherwise touch on a fresh
        // tempdir — we're testing the budget/streak plumbing, not the
        // remediation steps themselves.
        cfg.maintenance.embedding_repair_enabled = false;
        cfg
    }

    /// Force the cortex daily sweep to exhaust its budget by setting the
    /// limit to 1 and seeding a row that the sweep counts as a heal action.
    fn seed_budget_exhausting_state(db: &Database) {
        // `prune_activity_before` is the first counted step in the cortex
        // sweep — even one row deleted spends a budget action, and a
        // limit of 1 then exhausts immediately.
        db.log_activity("info", "test", "old", None).expect("seed");
        // Backdate the row so it falls before the activity_retention cutoff.
        db.conn()
            .execute("UPDATE activity_log SET created_at = 0", [])
            .expect("backdate");
    }

    #[test]
    fn run_daily_sweep_starts_with_zero_streak() {
        let cfg = budget_test_config(1000);
        let m = Maintenance::new(&cfg);
        assert_eq!(m.exhausted_streak(), 0);
        assert!(m.last_vacuum_at().is_none());
    }

    #[test]
    fn streak_increments_on_consecutive_exhausts() {
        // Drive the sweep twice with a tiny budget that always exhausts.
        // After two runs the streak must be 2 — that's what triggers the
        // "Maintenance:heal action budget" persistent warning per the
        // CLAUDE.md self-healing invariant.
        let db = test_db();
        let cfg = budget_test_config(1);
        let mut m = Maintenance::new(&cfg);
        let clock = FakeClock::new(1_000_000);

        seed_budget_exhausting_state(&db);
        let r1 = m.run_daily_sweep(&db, &cfg, &clock).expect("sweep 1");
        assert!(r1.budget_exhausted, "first sweep must exhaust budget=1");
        assert_eq!(m.exhausted_streak(), 1);

        seed_budget_exhausting_state(&db);
        let r2 = m.run_daily_sweep(&db, &cfg, &clock).expect("sweep 2");
        assert!(r2.budget_exhausted, "second sweep must exhaust");
        assert_eq!(m.exhausted_streak(), 2);
    }

    #[test]
    fn streak_resets_when_a_clean_sweep_intervenes() {
        let db = test_db();
        let cfg_tight = budget_test_config(1);
        let cfg_loose = budget_test_config(10_000);
        let mut m = Maintenance::new(&cfg_tight);
        let clock = FakeClock::new(1_000_000);

        seed_budget_exhausting_state(&db);
        m.run_daily_sweep(&db, &cfg_tight, &clock)
            .expect("tight sweep exhausts");
        assert_eq!(m.exhausted_streak(), 1);

        // Loosen the budget mid-test by reconstructing — simulates a
        // user raising the cap before the next daily run. The streak
        // is the runtime state, not a config; the next clean sweep
        // must reset it. (Reuse the same Maintenance instance — the
        // streak is what we're testing.)
        let r2 = m
            .run_daily_sweep(&db, &cfg_loose, &clock)
            .expect("loose sweep");
        assert!(
            !r2.budget_exhausted,
            "loose budget must complete cleanly: {r2:?}"
        );
        assert_eq!(m.exhausted_streak(), 0, "clean sweep resets streak");
    }

    #[test]
    fn persistent_warning_surfaces_after_two_consecutive_exhausts() {
        // Matches CLAUDE.md self-healing invariant: budget exhaustion is
        // a persistent warning only after the SECOND consecutive sweep.
        // The cortex sweep records `"Maintenance:heal action budget"` to
        // `maintenance_issue_state`; on the second observation,
        // `consecutive_count >= 2` qualifies the row for
        // `list_persistent_open_issues`, and the report surfaces it.
        let db = test_db();
        let cfg = budget_test_config(1);
        let mut m = Maintenance::new(&cfg);
        let clock = FakeClock::new(1_000_000);

        seed_budget_exhausting_state(&db);
        let r1 = m.run_daily_sweep(&db, &cfg, &clock).expect("sweep 1");
        assert!(
            !r1.persistent_warnings
                .iter()
                .any(|w| w == "Maintenance:heal action budget"),
            "first exhaust must NOT yet be a persistent warning: {:?}",
            r1.persistent_warnings
        );

        seed_budget_exhausting_state(&db);
        let r2 = m.run_daily_sweep(&db, &cfg, &clock).expect("sweep 2");
        assert!(
            r2.persistent_warnings
                .iter()
                .any(|w| w == "Maintenance:heal action budget"),
            "second consecutive exhaust must surface the persistent warning: {:?}",
            r2.persistent_warnings
        );
    }

    #[test]
    fn wal_checkpoint_runs_every_sweep() {
        // The cortex sweep always invokes `wal_checkpoint(TRUNCATE)`;
        // `wal_checkpoint_pages_reclaimed >= 0` means the call ran (a
        // failed checkpoint is sentinel `-1`). Pinning this here means
        // any future change that gates the checkpoint behind a
        // condition trips this regression test.
        let db = test_db();
        let cfg = budget_test_config(1000);
        let mut m = Maintenance::new(&cfg);
        let clock = FakeClock::new(1_000_000);

        let r = m.run_daily_sweep(&db, &cfg, &clock).expect("sweep");
        assert!(
            r.wal_checkpoint_pages_reclaimed >= 0,
            "WAL checkpoint must run every sweep, got sentinel {}",
            r.wal_checkpoint_pages_reclaimed
        );
    }

    #[test]
    fn vacuum_only_runs_when_interval_elapsed_and_fragmentation_high() {
        // Cortex gates VACUUM behind both an interval (>= vacuum_interval_days)
        // and fragmentation > 15%. A fresh in-memory DB has no fragmentation,
        // so even with `last_vacuum_at = 0` (forever ago) VACUUM must NOT run.
        // This guards against a future "always VACUUM" regression that would
        // ship every sweep with a heavy DB rewrite.
        let db = test_db();
        let cfg = budget_test_config(1000);
        let mut m = Maintenance::new(&cfg);
        let clock = FakeClock::new(1_000_000);

        let r = m.run_daily_sweep(&db, &cfg, &clock).expect("sweep");
        assert!(
            !r.vacuum_ran,
            "fresh DB has no fragmentation — VACUUM must NOT fire even when interval has elapsed"
        );
        assert!(
            m.last_vacuum_at().is_none(),
            "no VACUUM observed, last_vacuum_at must remain None"
        );
    }
}
