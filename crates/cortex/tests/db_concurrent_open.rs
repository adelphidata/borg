//! Regression: multiple `Database::open_at()` callers racing against the same
//! fresh DB file used to crash with `table settings already exists` because
//! the schema-version check happened *before* `BEGIN IMMEDIATE`. The first
//! writer committed the schema; the second got the writer lock next, still
//! believed `schema_version = 0`, and re-ran the full `CREATE TABLE` batch.
//!
//! Borgd reproduces this on every cold start because `daemon.rs`,
//! `budget_tick`, `config_watcher`, and the PKI bootstrap all open their own
//! `Database` handle concurrently.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::{Arc, Barrier};
use std::thread;

use cortex::db::Database;
use tempfile::TempDir;

#[test]
fn concurrent_open_on_fresh_db_does_not_double_migrate() {
    const N: usize = 8;

    let tmp = TempDir::new().expect("tempdir");
    let path = tmp.path().join("borg.db");

    let barrier = Arc::new(Barrier::new(N));
    let mut handles = Vec::with_capacity(N);

    for _ in 0..N {
        let path = path.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || -> anyhow::Result<u32> {
            barrier.wait();
            let db = Database::open_at(&path)?;
            db.schema_version()
        }));
    }

    for h in handles {
        let version = h
            .join()
            .expect("thread panicked")
            .expect("Database::open_at must succeed under concurrent first-time init");
        assert!(
            version >= 1,
            "schema_version after open should be at the latest version, got {version}"
        );
    }
}
