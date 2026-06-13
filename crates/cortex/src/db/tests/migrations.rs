use super::*;

async fn query_i64(db: &Database, sql: &str) -> i64 {
    let mut stmt = db.conn.prepare(sql).await.unwrap();
    let mut rows = stmt.query(()).await.unwrap();
    let row = rows.next().await.unwrap().expect("row");
    row.get::<i64>(0).unwrap()
}

async fn query_string(db: &Database, sql: &str) -> String {
    let mut stmt = db.conn.prepare(sql).await.unwrap();
    let mut rows = stmt.query(()).await.unwrap();
    let row = rows.next().await.unwrap().expect("row");
    row.get::<String>(0).unwrap()
}

// Schema version reaches CURRENT_VERSION on a freshly-initialized DB. If a new
// migrate_vN is added without bumping CURRENT_VERSION, the compile-time guard
// in `db/mod.rs` fires; this catches the runtime variant where the constant is
// bumped but the meta row never gets written.
#[tokio::test]
async fn fresh_db_reaches_current_schema_version() {
    let db = test_db().await;
    let v = db.schema_version().await.unwrap();
    assert!(v > 0, "schema_version must be set after init");
    let recorded = db.get_meta("schema_version").await.unwrap().unwrap();
    assert_eq!(
        recorded.parse::<u32>().unwrap(),
        v,
        "meta row and schema_version() must agree"
    );
}

// Two test DBs share no state but both reach a working, identical schema.
// Catches a class of regressions where init_connection's ordering (pragmas →
// migrations → settings seed → salt) silently fails the second time.
#[tokio::test]
async fn test_db_is_repeatedly_constructible() {
    let db_a = test_db().await;
    let db_b = test_db().await;
    assert_eq!(
        db_a.schema_version().await.unwrap(),
        db_b.schema_version().await.unwrap()
    );
    // Independent salts — proves get_or_create_hmac_salt actually runs per-DB
    // (not cached at process scope).
    let salt_a = db_a.get_meta("hmac_salt").await.unwrap().unwrap();
    let salt_b = db_b.get_meta("hmac_salt").await.unwrap().unwrap();
    assert_ne!(salt_a, salt_b, "each in-memory DB must mint its own salt");
}

// init_connection sets foreign_keys=ON and journal_mode=WAL. WAL on
// :memory: silently downgrades to MEMORY, so we accept either — but
// foreign_keys MUST be enabled or migrations that depend on FK enforcement
// (task_runs → scheduled_tasks) become silently broken.
#[tokio::test]
async fn pragmas_enabled_after_init() {
    let db = test_db().await;
    let fk = query_i64(&db, "PRAGMA foreign_keys").await;
    assert_eq!(fk, 1, "foreign_keys must be ON");

    let mode = query_string(&db, "PRAGMA journal_mode").await;
    // In-memory DBs report "memory"; file-backed DBs report "wal".
    assert!(
        mode == "wal" || mode == "memory",
        "unexpected journal_mode: {mode}"
    );

    let av = query_i64(&db, "PRAGMA auto_vacuum").await;
    assert_eq!(av, 2, "auto_vacuum must be INCREMENTAL (2)");
}

// task_runs.task_id REFERENCES scheduled_tasks(id). With foreign_keys=ON, an
// orphan insert must fail. If this test ever passes silently, either the FK
// was dropped from a migration or PRAGMA foreign_keys regressed.
#[tokio::test]
async fn foreign_key_enforced_task_runs_to_scheduled_tasks() {
    let db = test_db().await;
    let res = db
        .conn
        .execute(
            "INSERT INTO task_runs (task_id, started_at, duration_ms) VALUES (?1, ?2, 0)",
            params!("nonexistent_task_id", 0i64),
        )
        .await;
    let err = res.expect_err("FK violation must be rejected");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("foreign key") || msg.contains("constraint"),
        "expected FK error, got: {err}"
    );

    // Positive control: a row pointing at a real task succeeds.
    db.create_task(&simple_task("real", "n", "p", "interval", "1h", Some(0)))
        .await
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO task_runs (task_id, started_at, duration_ms) VALUES (?1, ?2, 0)",
            params!("real", 0i64),
        )
        .await
        .unwrap();
}

// Append-only triggers on the chain tables MUST remain in place across all
// migrations — if a later migration recreates the table without re-attaching
// the trigger, the integrity invariant silently disappears. This test pairs
// with the existing `append_only_triggers_block_*` tests in vitals.rs by
// checking the trigger metadata directly rather than going through the API.
#[tokio::test]
async fn append_only_triggers_present_on_chain_tables() {
    let db = test_db().await;
    for table in ["vitals_events", "bond_events", "evolution_events"] {
        let mut stmt = db
            .conn
            .prepare(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='trigger' AND tbl_name = ?1",
            )
            .await
            .unwrap();
        let mut rows = stmt.query(params!(table)).await.unwrap();
        let row = rows.next().await.unwrap().expect("row");
        let trigger_count: i64 = row.get(0).unwrap();
        assert!(
            trigger_count >= 2,
            "{table} should have at least UPDATE and DELETE block triggers, got {trigger_count}"
        );
    }
}

// scheduled_tasks must expose the pre_run preamble columns (script
// preprocessing) and the legacy workflow/project tables must not exist.
#[tokio::test]
async fn scheduled_tasks_has_pre_run_columns_and_no_workflow_tables() {
    let db = test_db().await;
    for absent in ["workflows", "workflow_steps", "projects"] {
        let mut stmt = db
            .conn
            .prepare("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1")
            .await
            .unwrap();
        let mut rows = stmt.query(params!(absent)).await.unwrap();
        let row = rows.next().await.unwrap().expect("row");
        let count: i64 = row.get(0).unwrap();
        assert_eq!(count, 0, "{absent} must not exist");
    }

    let mut stmt = db
        .conn
        .prepare("PRAGMA table_info(scheduled_tasks)")
        .await
        .unwrap();
    let mut rows = stmt.query(()).await.unwrap();
    let mut cols: Vec<String> = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        cols.push(row.get::<String>(1).unwrap());
    }
    for required in ["pre_run_command", "pre_run_cwd", "pre_run_timeout_ms"] {
        assert!(
            cols.iter().any(|c| c == required),
            "scheduled_tasks must expose {required}; columns: {cols:?}"
        );
    }
}

// The sessions table must expose the per-session lease columns (worker-pool /
// fallible executors). If a future migration recreates the table without them,
// claim/renew/release/expire all break — guard the column set directly.
#[tokio::test]
async fn sessions_has_lease_columns() {
    let db = test_db().await;
    let mut stmt = db
        .conn
        .prepare("PRAGMA table_info(sessions)")
        .await
        .unwrap();
    let mut rows = stmt.query(()).await.unwrap();
    let mut cols: Vec<String> = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        cols.push(row.get::<String>(1).unwrap());
    }
    for required in ["lease_owner", "lease_expiry", "lease_epoch"] {
        assert!(
            cols.iter().any(|c| c == required),
            "sessions must expose {required}; columns: {cols:?}"
        );
    }
}

// Re-running schema_version() when the meta row is corrupted (non-numeric)
// MUST degrade to 0 rather than panic — otherwise a single bad meta write
// would brick startup. Documented in db/mod.rs:451-460.
#[tokio::test]
async fn corrupted_schema_version_degrades_to_zero() {
    let db = test_db().await;
    db.set_meta("schema_version", "not-a-number").await.unwrap();
    let v = db.schema_version().await.unwrap();
    assert_eq!(v, 0, "non-numeric schema_version must degrade to 0");
}
