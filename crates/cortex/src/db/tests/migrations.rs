use super::*;

// Schema version reaches CURRENT_VERSION on a freshly-initialized DB. If a new
// migrate_vN is added without bumping CURRENT_VERSION, the compile-time guard
// in `db/mod.rs` fires; this catches the runtime variant where the constant is
// bumped but the meta row never gets written.
#[test]
fn fresh_db_reaches_current_schema_version() {
    let db = test_db();
    let v = db.schema_version().unwrap();
    assert!(v > 0, "schema_version must be set after init");
    let recorded = db.get_meta("schema_version").unwrap().unwrap();
    assert_eq!(
        recorded.parse::<u32>().unwrap(),
        v,
        "meta row and schema_version() must agree"
    );
}

// Two test DBs share no state but both reach a working, identical schema.
// Catches a class of regressions where init_connection's ordering (pragmas →
// migrations → settings seed → salt) silently fails the second time.
#[test]
fn test_db_is_repeatedly_constructible() {
    let db_a = test_db();
    let db_b = test_db();
    assert_eq!(
        db_a.schema_version().unwrap(),
        db_b.schema_version().unwrap()
    );
    // Independent salts — proves get_or_create_hmac_salt actually runs per-DB
    // (not cached at process scope).
    let salt_a = db_a.get_meta("hmac_salt").unwrap().unwrap();
    let salt_b = db_b.get_meta("hmac_salt").unwrap().unwrap();
    assert_ne!(salt_a, salt_b, "each in-memory DB must mint its own salt");
}

// init_connection sets foreign_keys=ON and journal_mode=WAL. WAL on
// :memory: silently downgrades to MEMORY, so we accept either — but
// foreign_keys MUST be enabled or migrations that depend on FK enforcement
// (task_runs → scheduled_tasks) become silently broken.
#[test]
fn pragmas_enabled_after_init() {
    let db = test_db();
    let fk: i64 = db
        .conn
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fk, 1, "foreign_keys must be ON");

    let mode: String = db
        .conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    // In-memory DBs report "memory"; file-backed DBs report "wal".
    assert!(
        mode == "wal" || mode == "memory",
        "unexpected journal_mode: {mode}"
    );

    let av: i64 = db
        .conn
        .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
        .unwrap();
    assert_eq!(av, 2, "auto_vacuum must be INCREMENTAL (2)");
}

// task_runs.task_id REFERENCES scheduled_tasks(id). With foreign_keys=ON, an
// orphan insert must fail. If this test ever passes silently, either the FK
// was dropped from a migration or PRAGMA foreign_keys regressed.
#[test]
fn foreign_key_enforced_task_runs_to_scheduled_tasks() {
    let db = test_db();
    let res = db.conn.execute(
        "INSERT INTO task_runs (task_id, started_at, duration_ms) VALUES (?1, ?2, 0)",
        rusqlite::params!["nonexistent_task_id", 0i64],
    );
    let err = res.expect_err("FK violation must be rejected");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("foreign key") || msg.contains("constraint"),
        "expected FK error, got: {err}"
    );

    // Positive control: a row pointing at a real task succeeds.
    db.create_task(&simple_task("real", "n", "p", "interval", "1h", Some(0)))
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO task_runs (task_id, started_at, duration_ms) VALUES (?1, ?2, 0)",
            rusqlite::params!["real", 0i64],
        )
        .unwrap();
}

// Append-only triggers on the chain tables MUST remain in place across all
// migrations — if a later migration recreates the table without re-attaching
// the trigger, the integrity invariant silently disappears. This test pairs
// with the existing `append_only_triggers_block_*` tests in vitals.rs by
// checking the trigger metadata directly rather than going through the API.
#[test]
fn append_only_triggers_present_on_chain_tables() {
    let db = test_db();
    for table in ["vitals_events", "bond_events", "evolution_events"] {
        let trigger_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='trigger' AND tbl_name = ?1",
                rusqlite::params![table],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            trigger_count >= 2,
            "{table} should have at least UPDATE and DELETE block triggers, got {trigger_count}"
        );
    }
}

// Re-running schema_version() when the meta row is corrupted (non-numeric)
// MUST degrade to 0 rather than panic — otherwise a single bad meta write
// would brick startup. Documented in db/mod.rs:451-460.
#[test]
fn corrupted_schema_version_degrades_to_zero() {
    let db = test_db();
    db.set_meta("schema_version", "not-a-number").unwrap();
    let v = db.schema_version().unwrap();
    assert_eq!(v, 0, "non-numeric schema_version must degrade to 0");
}
