use rusqlite::Connection;
use std::path::Path;
use tempfile::tempdir;

use borg_core::db::Database;
use borg_core::db_key;

/// Set the SQLCipher key on a connection. Uses query_row since PRAGMA returns a result.
fn set_key(conn: &Connection, key: &[u8]) {
    let key_pragma = db_key::format_sqlcipher_key(key);
    conn.query_row(&format!("PRAGMA key = \"{key_pragma}\""), [], |_| Ok(()))
        .expect("set key");
}

/// Helper: create an encrypted DB at the given path, write some data, close it.
fn create_encrypted_db(path: &Path, key: &[u8]) {
    let conn = Connection::open(path).expect("open new DB");
    set_key(&conn, key);
    conn.execute_batch(
        "CREATE TABLE test_data (id INTEGER PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO test_data (value) VALUES ('hello');
         INSERT INTO test_data (value) VALUES ('world');",
    )
    .expect("create test data");
}

#[test]
fn encrypted_db_round_trip() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let key = db_key::generate_random_key_for_test();

    create_encrypted_db(&db_path, &key);

    // Reopen with same key and verify data
    let conn = Connection::open(&db_path).expect("reopen DB");
    set_key(&conn, &key);
    let count: i64 = conn
        .query_row("SELECT count(*) FROM test_data", [], |row| row.get(0))
        .expect("count rows");
    assert_eq!(count, 2);

    let value: String = conn
        .query_row("SELECT value FROM test_data WHERE id = 1", [], |row| {
            row.get(0)
        })
        .expect("read value");
    assert_eq!(value, "hello");
}

#[test]
fn wrong_key_fails() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let key = db_key::generate_random_key_for_test();

    create_encrypted_db(&db_path, &key);

    // Open with a different key
    let wrong_key = db_key::generate_random_key_for_test();
    assert_ne!(key, wrong_key);

    let conn = Connection::open(&db_path).expect("open DB");
    set_key(&conn, &wrong_key);

    // Reading should fail — wrong key makes the DB unreadable
    let result = conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()));
    assert!(result.is_err(), "should fail with wrong key");
}

#[test]
fn no_key_fails() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let key = db_key::generate_random_key_for_test();

    create_encrypted_db(&db_path, &key);

    // Open without setting PRAGMA key
    let conn = Connection::open(&db_path).expect("open DB");

    // Reading should fail — DB is encrypted and no key was provided
    let result = conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()));
    assert!(result.is_err(), "should fail without key");
}

#[test]
fn in_memory_db_works_without_encryption() {
    // from_connection uses unencrypted path — must not break
    let conn = Connection::open_in_memory().expect("open in-memory db");
    let db = Database::from_connection(conn).expect("init db");
    db.conn()
        .execute_batch("CREATE TABLE smoke (id INTEGER PRIMARY KEY);")
        .expect("DDL on in-memory DB");
    let count: i64 = db
        .conn()
        .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
        .expect("query in-memory DB");
    assert!(count > 0);
}

#[test]
fn key_generation_produces_256_bits() {
    let key = db_key::generate_random_key_for_test();
    assert_eq!(key.len(), 32, "key should be 256 bits (32 bytes)");
}

#[test]
fn key_generation_is_unique() {
    let k1 = db_key::generate_random_key_for_test();
    let k2 = db_key::generate_random_key_for_test();
    assert_ne!(k1, k2, "two generated keys should differ");
}

#[test]
fn sqlcipher_key_format() {
    let key = vec![0xab, 0xcd, 0xef, 0x01];
    let formatted = db_key::format_sqlcipher_key(&key);
    assert_eq!(formatted, "x'abcdef01'");
}
