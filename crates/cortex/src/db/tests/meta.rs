use super::*;
use rusqlite::params;

#[test]
fn meta_get_set() {
    let db = test_db();
    assert!(db.get_meta("version").expect("get").is_none());

    db.set_meta("version", "1").expect("set");
    assert_eq!(db.get_meta("version").expect("get").as_deref(), Some("1"));

    db.set_meta("version", "2").expect("set again");
    assert_eq!(db.get_meta("version").expect("get").as_deref(), Some("2"));
}

#[test]
fn schema_version_tracking() {
    let db = test_db();
    let version = db.schema_version().expect("get version");
    assert_eq!(version, Database::CURRENT_VERSION);
}

#[test]
fn migrate_v21_creates_indexes() {
    let db = test_db();
    let index_exists = |name: &str| -> bool {
        db.conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name=?1",
                params![name],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
            == 1
    };
    assert!(index_exists("idx_task_runs_task"));
    assert!(index_exists("idx_tasks_due"));
    assert!(index_exists("idx_task_runs_status"));
    assert!(index_exists("idx_scripts_ephemeral"));
}

#[test]
fn auto_vacuum_is_set() {
    let db = test_db();
    let mode: i64 = db
        .conn
        .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
        .expect("auto_vacuum pragma");
    // 2 = INCREMENTAL
    assert_eq!(mode, 2, "auto_vacuum should be INCREMENTAL (2)");
}

#[test]
fn open_with_custom_timeout() {
    // Verify that a custom busy timeout is accepted
    let conn = Connection::open_in_memory().expect("open in-memory db");
    let db = Database::init_connection(conn, Database::GATEWAY_BUSY_TIMEOUT_MS)
        .expect("init with gateway timeout");
    // Verify the timeout was applied by reading it back
    let timeout: i64 = db
        .conn
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .expect("read busy_timeout pragma");
    assert_eq!(timeout, Database::GATEWAY_BUSY_TIMEOUT_MS as i64);
}

#[test]
fn default_open_uses_5s_timeout() {
    let conn = Connection::open_in_memory().expect("open in-memory db");
    let db = Database::init_connection(conn, 5000).expect("init with default timeout");
    let timeout: i64 = db
        .conn
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .expect("read busy_timeout pragma");
    assert_eq!(timeout, 5000);
}
