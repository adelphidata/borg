//! WAL checkpoint and VACUUM helpers used by the daily maintenance sweep.
//!
//! `journal_mode=WAL` is set on every connection (see `init_connection`
//! in `mod.rs`). What's missing is periodic *operation* on it: the `-wal`
//! file grows unbounded between explicit checkpoints. This module owns
//! that call.
//!
//! The maintenance sweep runs `wal_checkpoint(PASSIVE)` every time
//! (cheap when the WAL is small). Full `VACUUM` is gated behind both an
//! interval (`maintenance.vacuum_interval_days`) and a fragmentation
//! signal (>15% of pages on the freelist or >64 MB freelist) — VACUUM
//! locks the DB and rewrites the file, so we don't run it on every
//! sweep.

use anyhow::{Context, Result};

use super::Database;

/// The only checkpoint mode this process may run against `~/.borg/borg.db`.
///
/// `TRUNCATE` (and `RESTART`) take the exclusive checkpoint lock and reset the
/// WAL. `~/.borg/borg.db` is shared: `borgd` holds a long-lived writer while
/// every `borg` CLI invocation — from any worktree — opens its own handle and
/// may run a sweep. A transient opener resetting the WAL under the live writer
/// is a documented route to btree-page damage concentrated on the hottest
/// tables, which reads as disk failure and is not.
///
/// `PASSIVE` never resets or truncates the WAL and never takes that lock, so it
/// cannot lose the race; it simply copies whatever frames it can and returns.
/// The WAL stays bounded by [`crate::db::JOURNAL_SIZE_LIMIT_PRAGMA`] instead.
///
/// `TRUNCATE` is correct only on a sole-opener, quiescent connection (offline
/// maintenance). We do not try to detect that — PASSIVE is the safe default,
/// and the cost of being wrong in the other direction is only disk slack.
const WAL_CHECKPOINT_PRAGMA: &str = "PRAGMA wal_checkpoint(PASSIVE)";

/// Meta key storing unix-seconds of the last full VACUUM.
pub(crate) const META_LAST_VACUUM_AT: &str = "maintenance.last_vacuum_at";

/// Fragmentation ratio above which a full VACUUM is worth running. SQLite's
/// docs use 10–25% as a guideline; 15% sits in the middle and matches the
/// `freelist_count > 64 MB` byte-count guard.
const VACUUM_FRAGMENTATION_RATIO: f64 = 0.15;

/// Absolute byte threshold for triggering VACUUM regardless of ratio. A
/// large DB with a small percentage of free pages can still benefit from
/// VACUUM if the absolute waste is large.
const VACUUM_FREELIST_BYTES: i64 = 64 * 1024 * 1024;

/// Result of one WAL/VACUUM pass. Surfaced in `MaintenanceReport`.
#[derive(Debug, Default, Clone, Copy)]
pub struct WalVacuumOutcome {
    /// Pages reclaimed by the WAL checkpoint, or `-1` if the checkpoint
    /// itself failed. Negative is the explicit "didn't run" signal so
    /// downstream observers can distinguish "ran but reclaimed nothing"
    /// from "errored before reading the result."
    pub wal_checkpoint_pages_reclaimed: i64,
    /// True when the gating logic decided this sweep should run a full
    /// VACUUM.
    pub vacuum_ran: bool,
    /// Bytes the DB file shrank after VACUUM.
    ///
    /// - `0` → VACUUM ran but the file size was unchanged (rare; the DB
    ///   was already compact).
    /// - `>0` → VACUUM ran and reclaimed this many bytes.
    /// - `-1` → VACUUM ran but post-measure failed; metric unavailable.
    /// - When `vacuum_ran=false`, this field is `0` and meaningless.
    pub vacuum_bytes_reclaimed: i64,
}

/// Run one [`WAL_CHECKPOINT_PRAGMA`] and return the pages it checkpointed.
///
/// `wal_checkpoint` answers with `(busy, log_pages, checkpointed_pages)`; the
/// third column is the count we report. The row is read (and the cursor
/// dropped) here so no caller has to remember that a pragma returning a result
/// set must be drained before the next statement takes a lock.
async fn checkpoint(db: &Database) -> Result<i64> {
    let mut rows = db.conn().query(WAL_CHECKPOINT_PRAGMA, ()).await?;
    match rows.next().await? {
        Some(row) => Ok(row.get::<i64>(2)?),
        None => Ok(0),
    }
}

/// Read `(freelist_count, page_count, page_size)` for fragmentation gating.
async fn fragmentation_stats(db: &Database) -> Result<(i64, i64, i64)> {
    async fn read_pragma(db: &Database, sql: &str) -> Result<i64> {
        let mut rows = db
            .conn()
            .query(sql, ())
            .await
            .with_context(|| sql.to_string())?;
        match rows.next().await? {
            Some(row) => Ok(row.get::<i64>(0)?),
            None => Ok(0),
        }
    }
    let freelist = read_pragma(db, "PRAGMA freelist_count")
        .await
        .context("PRAGMA freelist_count")?;
    let page_count = read_pragma(db, "PRAGMA page_count")
        .await
        .context("PRAGMA page_count")?;
    let page_size = read_pragma(db, "PRAGMA page_size")
        .await
        .context("PRAGMA page_size")?;
    Ok((freelist, page_count, page_size))
}

/// True when both the interval and a fragmentation signal say it's time
/// to run a full VACUUM. Pure function — testable in isolation.
pub(crate) fn should_vacuum(
    now: i64,
    last_vacuum_at: Option<i64>,
    interval_days: u32,
    freelist: i64,
    page_count: i64,
    page_size: i64,
) -> bool {
    let interval_secs = (interval_days as i64) * 86_400;
    let interval_elapsed = match last_vacuum_at {
        Some(prev) => now.saturating_sub(prev) >= interval_secs,
        None => true,
    };
    if !interval_elapsed {
        return false;
    }
    let ratio = if page_count > 0 {
        freelist as f64 / page_count as f64
    } else {
        0.0
    };
    let bytes_free = freelist.saturating_mul(page_size);
    ratio > VACUUM_FRAGMENTATION_RATIO || bytes_free > VACUUM_FREELIST_BYTES
}

/// Run the WAL checkpoint and (conditionally) full VACUUM. Errors in
/// either step log + continue — they never propagate up. The maintenance
/// sweep treats this as best-effort housekeeping; one failed checkpoint
/// must not abort the rest of the sweep.
pub async fn run_wal_and_vacuum(db: &Database, now: i64, interval_days: u32) -> WalVacuumOutcome {
    let mut outcome = WalVacuumOutcome::default();

    // 1. Always: WAL checkpoint (PASSIVE — see WAL_CHECKPOINT_PRAGMA).
    //    Returns three integers: busy (0/1), log_pages, checkpointed_pages.
    //    We surface checkpointed_pages as the reclaimed count. A PASSIVE
    //    checkpoint that loses the race to a live writer reports busy=1 and
    //    checkpoints fewer pages — that is the intended graceful outcome,
    //    not an error, so it is not distinguished here.
    match checkpoint(db).await {
        Ok(pages) => outcome.wal_checkpoint_pages_reclaimed = pages,
        Err(e) => {
            tracing::warn!("maintenance: {WAL_CHECKPOINT_PRAGMA} failed: {e}");
            outcome.wal_checkpoint_pages_reclaimed = -1;
        }
    }

    // 2. Conditional: full VACUUM. Read fragmentation stats — failure here
    //    means we can't decide, so skip VACUUM rather than guess.
    let stats = match fragmentation_stats(db).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("maintenance: fragmentation_stats failed: {e}");
            return outcome;
        }
    };
    let (freelist, page_count, page_size) = stats;

    let last_vacuum_at = match db.get_meta(META_LAST_VACUUM_AT).await {
        Ok(v) => v.and_then(|s| s.parse::<i64>().ok()),
        Err(_) => None,
    };

    if !should_vacuum(
        now,
        last_vacuum_at,
        interval_days,
        freelist,
        page_count,
        page_size,
    ) {
        return outcome;
    }

    let bytes_before = page_count.saturating_mul(page_size);
    tracing::info!(
        "maintenance: running VACUUM (freelist={freelist}, page_count={page_count}, ~{} MB)",
        bytes_before / (1024 * 1024)
    );
    // We deliberately keep plain `VACUUM` rather than `VACUUM INTO '<tmp>'`
    // + atomic rename. The rename path would need to close-and-reopen the
    // live connection — libsql's `Connection` is shared by reference
    // through `&Database` here, and `run_wal_and_vacuum` is called as
    // `&self` with no way to mutate the connection handle in place.
    // Renaming the file under a held fd would orphan all subsequent writes
    // on this connection to the unlinked inode while fresh `Database::open`
    // calls would see the compacted file — silent divergence. Plain
    // `VACUUM` reclaims space in place on the live connection; the
    // `VACUUM INTO` swap is deferred until the `Database` lifecycle grows
    // a path-aware `reopen` method.
    if let Err(e) = db.conn().execute_batch("VACUUM;").await {
        tracing::warn!("maintenance: VACUUM failed: {e}");
        return outcome;
    }
    outcome.vacuum_ran = true;

    // VACUUM rewrites every page in the database, and on a WAL connection
    // every one of those pages goes through the WAL. Checkpointing only
    // *before* VACUUM therefore does nothing about the slack VACUUM itself
    // creates: without this second pass the sweep can report bytes reclaimed
    // while the `-wal` file has grown to the size of the whole database.
    // Best-effort, same as the first pass — this is housekeeping. Uses the
    // same query-and-drain form as the pre-VACUUM pass: `wal_checkpoint`
    // returns a row, and leaving that cursor un-finalized is its own class of
    // bug. Two forms for one pragma is how they drift apart.
    if let Err(e) = checkpoint(db).await {
        tracing::warn!("maintenance: post-VACUUM {WAL_CHECKPOINT_PRAGMA} failed: {e}");
    }

    // Re-measure to compute reclaimed bytes. If this read fails, we still
    // mark vacuum_ran=true (the operation succeeded) but use `-1` to
    // distinguish "unmeasurable" from "ran but reclaimed nothing".
    match fragmentation_stats(db).await {
        Ok((_, page_count_after, page_size_after)) => {
            let bytes_after = page_count_after.saturating_mul(page_size_after);
            outcome.vacuum_bytes_reclaimed = bytes_before.saturating_sub(bytes_after);
        }
        Err(_) => outcome.vacuum_bytes_reclaimed = -1,
    }

    if let Err(e) = db.set_meta(META_LAST_VACUUM_AT, &now.to_string()).await {
        tracing::warn!("maintenance: persisting last_vacuum_at failed: {e}");
    }

    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_vacuum_decision_table() {
        const D: i64 = 86_400;
        // (name, now, last_vacuum, interval_days, freelist, page_count,
        //  page_size, expected)
        #[allow(clippy::type_complexity)]
        let cases: &[(&str, i64, Option<i64>, u32, i64, i64, i64, bool)] = &[
            // 1 day since last vacuum, interval 7d — must skip even with high
            // fragmentation, because the interval gate dominates.
            (
                "interval_not_elapsed",
                7 * D,
                Some(7 * D - D),
                7,
                5_000,
                10_000,
                4096,
                false,
            ),
            // Interval elapsed but only 5% fragmentation and small freelist.
            (
                "fragmentation_low",
                30 * D,
                Some(0),
                7,
                50,
                1_000,
                4096,
                false,
            ),
            // 8d since last, 20% freelist ratio — runs.
            (
                "interval_and_ratio_exceeded",
                30 * D,
                Some(30 * D - 8 * D),
                7,
                200,
                1_000,
                4096,
                true,
            ),
            // 8d elapsed, only 1% ratio, but absolute freelist > 64 MB:
            // 20_000 free pages * 4096 B = ~78 MB — runs on the byte threshold.
            (
                "freelist_exceeds_byte_threshold",
                30 * D,
                Some(30 * D - 8 * D),
                7,
                20_000,
                2_000_000,
                4096,
                true,
            ),
            // First-ever sweep (no prior vacuum) with high fragmentation — runs.
            (
                "no_prior_high_fragmentation",
                30 * D,
                None,
                7,
                200,
                1_000,
                4096,
                true,
            ),
            // No prior, but the DB is clean — interval elapsed yet nothing to do.
            ("no_prior_clean_db", 30 * D, None, 7, 0, 1_000, 4096, false),
        ];
        for (name, now, last, interval, freelist, page_count, page_size, expected) in cases {
            assert_eq!(
                should_vacuum(*now, *last, *interval, *freelist, *page_count, *page_size),
                *expected,
                "should_vacuum mismatch for case {name}"
            );
        }
    }
}
